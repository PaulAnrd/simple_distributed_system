//! The agent runtime: what `clusterd agent run` actually executes. A head
//! runs a TLS listener for workers plus a local control socket for the
//! CLI; a worker dials the head and heartbeats on a timer, reconnecting
//! with backoff if the connection drops. Head and worker share every other
//! module (`tls`, `resources`, `nodes`) — this file is just the wiring.

use std::fs::{File, OpenOptions, TryLockError};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use uuid::Uuid;

use clusterd_proto::{message_codec, Message, PROTOCOL_VERSION};

use crate::config::{self, Config, Role};
use crate::error::Error;
use crate::ipc;
use crate::nodes::{HeartbeatSnapshot, NodeTable};
use crate::resources::ResourceSampler;
use crate::tls;

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A connection that survived at least this long is considered to have
/// been healthy, so a subsequent drop resets backoff instead of continuing
/// to climb from wherever it was before.
const HEALTHY_CONNECTION_THRESHOLD: Duration = Duration::from_secs(10);
/// Upper bound, on both sides, for TCP connect + TLS handshake + the
/// Hello/HelloAck exchange. On a LAN this takes milliseconds; the bound
/// stops a peer that connects and then says nothing from holding a task
/// and a file descriptor on the head indefinitely.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause after a failed `accept` before trying again; see `run_head`.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

type HeadConnection = Framed<tokio_rustls::server::TlsStream<TcpStream>, LengthDelimitedCodec>;
type WorkerConnection = Framed<tokio_rustls::client::TlsStream<TcpStream>, LengthDelimitedCodec>;

pub async fn run(data_dir: PathBuf) -> Result<(), Error> {
    let config = Config::load(&data_dir)?;
    // Held until this function returns, i.e. for the agent's lifetime.
    let _lock = lock_data_dir(&data_dir)?;
    tracing::info!(node_name = %config.node_name, role = ?config.role, "starting clusterd agent");

    match config.role {
        Role::Head => run_head(data_dir, config).await,
        Role::Worker => run_worker(data_dir, config).await,
    }
}

/// Takes an exclusive lock on the data directory for as long as the
/// returned file is kept. Two agents on one data directory would share a
/// node_id, and on a head the second one would delete the first one's
/// control socket while starting up. The operating system releases the
/// lock when the process exits, however it exits, so a crash never leaves
/// a stale lock behind.
fn lock_data_dir(data_dir: &Path) -> Result<File, Error> {
    let path = config::lock_path(data_dir);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|source| Error::LockDataDir {
            path: path.clone(),
            source,
        })?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(Error::AlreadyRunning {
            path: data_dir.to_path_buf(),
        }),
        Err(TryLockError::Error(source)) => Err(Error::LockDataDir { path, source }),
    }
}

async fn run_head(data_dir: PathBuf, config: Config) -> Result<(), Error> {
    let identity = tls::load_identity(
        &config::node_cert_path(&data_dir),
        &config::node_key_path(&data_dir),
    )?;
    let ca_roots = tls::load_ca_root_store(&config::ca_cert_path(&data_dir))?;
    let tls_config = tls::server_config(identity, ca_roots)?;

    // Bind both listeners before starting anything else, so that a start-up
    // problem (port already in use, control-socket path too long) stops the
    // agent with a clear error instead of leaving a half-working head.
    let listen_addr = config.listen_addr.expect("validated by Config::load");
    let listener = TcpListener::bind(listen_addr).await.map_err(Error::Io)?;
    let uds_listener = ipc::bind(&config::uds_socket_path(&data_dir))?;
    let acceptor = TlsAcceptor::from(tls_config);
    tracing::info!(listen_addr = %listener.local_addr().map_err(Error::Io)?, "control-plane listener started");

    let heartbeat_timeout = Duration::from_secs(config.heartbeat_timeout_secs);
    let node_table = Arc::new(Mutex::new(NodeTable::new(heartbeat_timeout)));

    // Head is a superset of worker (see architecture doc): it contributes
    // its own resources to the pool too, so it feeds its own table
    // directly rather than looping heartbeats through the network to itself.
    tokio::spawn(self_heartbeat_loop(
        node_table.clone(),
        config.node_id,
        config.node_name.clone(),
        Duration::from_secs(config.heartbeat_interval_secs),
        data_dir.clone(),
    ));
    tokio::spawn(ipc::serve(uds_listener, node_table.clone()));

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(connection) => connection,
            Err(e) => {
                // Accept errors are transient in practice — typically the
                // process running out of file descriptors under a burst of
                // connections. Returning here would take the whole head
                // down; pause so a persistent error doesn't spin, then keep
                // accepting.
                tracing::warn!(error = %e, "accepting a connection failed; retrying");
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let node_table = node_table.clone();
        tokio::spawn(async move {
            if let Err(e) =
                handle_worker_connection(stream, acceptor, node_table, heartbeat_timeout).await
            {
                tracing::warn!(%peer_addr, error = %e.chain(), "connection ended with error");
            }
        });
    }
}

async fn handle_worker_connection(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    node_table: Arc<Mutex<NodeTable>>,
    idle_timeout: Duration,
) -> Result<(), Error> {
    let (mut framed, node_id, node_name) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, accept_handshake(stream, acceptor))
            .await
            .map_err(|_| Error::HandshakeTimeout(HANDSHAKE_TIMEOUT))??;
    tracing::info!(%node_id, %node_name, "node connected");

    loop {
        // A connection silent for longer than the heartbeat timeout belongs
        // to a node that is already reported down, so close it. Otherwise a
        // peer that vanished without closing (power loss, cable pulled)
        // would hold this task and its socket forever: the head only reads,
        // so TCP never reports the loss.
        let frame = match tokio::time::timeout(idle_timeout, framed.next()).await {
            Err(_) => return Err(Error::IdleTimeout(idle_timeout)),
            Ok(None) => break,
            Ok(Some(frame)) => frame.map_err(Error::Io)?,
        };
        match Message::decode(&frame)? {
            Message::Heartbeat {
                node_id: hb_id,
                cpu_free_pct,
                mem_total_bytes,
                mem_free_bytes,
                disk_free_bytes,
                running_jobs,
                agent_version,
                timestamp,
            } => {
                if hb_id != node_id {
                    tracing::warn!(
                        connected_as = %node_id,
                        claimed = %hb_id,
                        "heartbeat node_id does not match this connection's identity, ignoring"
                    );
                    continue;
                }
                let snapshot = HeartbeatSnapshot {
                    cpu_free_pct,
                    mem_total_bytes,
                    mem_free_bytes,
                    disk_free_bytes,
                    running_jobs,
                    agent_version,
                    reported_at: timestamp,
                };
                node_table
                    .lock()
                    .expect("node table mutex poisoned")
                    .record_heartbeat(node_id, node_name.clone(), snapshot);
            }
            other => tracing::warn!(?other, "unexpected message after handshake, ignoring"),
        }
    }

    tracing::info!(%node_id, %node_name, "node disconnected");
    Ok(())
}

/// Head side of the handshake: TLS, then `Hello` in, `HelloAck` out.
/// Returns the connection with the identity the worker announced.
async fn accept_handshake(
    stream: TcpStream,
    acceptor: TlsAcceptor,
) -> Result<(HeadConnection, Uuid, String), Error> {
    let tls_stream = acceptor.accept(stream).await.map_err(Error::Io)?;
    let mut framed = Framed::new(tls_stream, message_codec());

    let frame = framed
        .next()
        .await
        .ok_or(Error::HandshakeIncomplete)?
        .map_err(Error::Io)?;
    let (node_id, node_name) = match Message::decode(&frame)? {
        Message::Hello {
            protocol_version,
            node_id,
            node_name,
            ..
        } => {
            if protocol_version != PROTOCOL_VERSION {
                let ack = Message::HelloAck {
                    protocol_version: PROTOCOL_VERSION,
                    accepted: false,
                    reason: Some(format!(
                        "protocol version mismatch: head={PROTOCOL_VERSION}, node={protocol_version}"
                    )),
                };
                // Best-effort: tell the peer why before we close on them.
                let _ = framed.send(Bytes::from(ack.encode()?)).await;
                return Err(Error::ProtocolMismatch {
                    local: PROTOCOL_VERSION,
                    remote: protocol_version,
                });
            }
            (node_id, node_name)
        }
        other => return Err(Error::UnexpectedMessage(Box::new(other))),
    };

    let ack = Message::HelloAck {
        protocol_version: PROTOCOL_VERSION,
        accepted: true,
        reason: None,
    };
    framed
        .send(Bytes::from(ack.encode()?))
        .await
        .map_err(Error::Io)?;
    Ok((framed, node_id, node_name))
}

async fn self_heartbeat_loop(
    node_table: Arc<Mutex<NodeTable>>,
    node_id: Uuid,
    node_name: String,
    interval: Duration,
    data_dir: PathBuf,
) {
    let mut sampler = ResourceSampler::new(&data_dir);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let sample = sampler.sample().await;
        let snapshot = HeartbeatSnapshot {
            cpu_free_pct: sample.cpu_free_pct,
            mem_total_bytes: sample.mem_total_bytes,
            mem_free_bytes: sample.mem_free_bytes,
            disk_free_bytes: sample.disk_free_bytes,
            running_jobs: vec![],
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            reported_at: SystemTime::now(),
        };
        node_table
            .lock()
            .expect("node table mutex poisoned")
            .record_heartbeat(node_id, node_name.clone(), snapshot);
    }
}

async fn run_worker(data_dir: PathBuf, config: Config) -> Result<(), Error> {
    let head_addr = config.head_addr.expect("validated by Config::load");
    let interval = Duration::from_secs(config.heartbeat_interval_secs);
    // One sampler for the whole process, not one per connection: its CPU
    // reading is a delta against the previous sample, so a fresh sampler
    // on every reconnect would make each first heartbeat meaningless.
    let mut sampler = ResourceSampler::new(&data_dir);
    let mut backoff = MIN_BACKOFF;

    loop {
        let attempt_start = Instant::now();
        if let Err(e) =
            connect_and_heartbeat(&data_dir, &config, head_addr, interval, &mut sampler).await
        {
            tracing::warn!(error = %e.chain(), "lost connection to head");
        }
        if attempt_start.elapsed() > HEALTHY_CONNECTION_THRESHOLD {
            backoff = MIN_BACKOFF;
        }
        tracing::info!(
            retry_in_secs = backoff.as_secs(),
            "retrying connection to head"
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Connects to the head, completes the handshake, then heartbeats until the
/// connection fails for any reason. Always returns via `Err` (or a short-
/// circuit `?` on a setup failure) — `run_worker` owns all retry/backoff
/// policy in one place rather than this function looping forever itself.
async fn connect_and_heartbeat(
    data_dir: &Path,
    config: &Config,
    head_addr: SocketAddr,
    interval: Duration,
    sampler: &mut ResourceSampler,
) -> Result<(), Error> {
    let mut framed = tokio::time::timeout(HANDSHAKE_TIMEOUT, connect(data_dir, config, head_addr))
        .await
        .map_err(|_| Error::HandshakeTimeout(HANDSHAKE_TIMEOUT))??;
    tracing::info!(%head_addr, "connected to head");

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let sample = sampler.sample().await;
                let heartbeat = Message::Heartbeat {
                    node_id: config.node_id,
                    cpu_free_pct: sample.cpu_free_pct,
                    mem_total_bytes: sample.mem_total_bytes,
                    mem_free_bytes: sample.mem_free_bytes,
                    disk_free_bytes: sample.disk_free_bytes,
                    running_jobs: vec![],
                    agent_version: env!("CARGO_PKG_VERSION").to_string(),
                    timestamp: SystemTime::now(),
                };
                framed.send(Bytes::from(heartbeat.encode()?)).await.map_err(Error::Io)?;
            }
            frame = framed.next() => {
                match frame {
                    None => return Err(Error::ConnectionClosed),
                    Some(Err(e)) => return Err(Error::Io(e)),
                    // Phase 1: head never sends anything after HelloAck; ignore
                    // whatever it was rather than treating an unknown message
                    // as fatal.
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Worker side of the handshake: TCP, TLS, then `Hello` out, `HelloAck` in.
/// Certificate files are reloaded on every attempt, so replacing them on
/// disk takes effect at the next reconnection.
async fn connect(
    data_dir: &Path,
    config: &Config,
    head_addr: SocketAddr,
) -> Result<WorkerConnection, Error> {
    let identity = tls::load_identity(
        &config::node_cert_path(data_dir),
        &config::node_key_path(data_dir),
    )?;
    let ca_roots = tls::load_ca_root_store(&config::ca_cert_path(data_dir))?;
    let tls_config = tls::client_config(identity, ca_roots)?;
    let connector = TlsConnector::from(tls_config);

    let tcp = TcpStream::connect(head_addr).await.map_err(Error::Io)?;
    let tls_stream = connector
        .connect(tls::node_server_name(), tcp)
        .await
        .map_err(Error::Io)?;
    let mut framed = Framed::new(tls_stream, message_codec());

    let hello = Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        node_id: config.node_id,
        node_name: config.node_name.clone(),
        cert_fingerprint: String::new(),
    };
    framed
        .send(Bytes::from(hello.encode()?))
        .await
        .map_err(Error::Io)?;

    let frame = framed
        .next()
        .await
        .ok_or(Error::HandshakeIncomplete)?
        .map_err(Error::Io)?;
    match Message::decode(&frame)? {
        Message::HelloAck { accepted: true, .. } => Ok(framed),
        Message::HelloAck {
            accepted: false,
            reason,
            ..
        } => Err(Error::HandshakeRejected(
            reason.unwrap_or_else(|| "no reason given".to_string()),
        )),
        other => Err(Error::UnexpectedMessage(Box::new(other))),
    }
}

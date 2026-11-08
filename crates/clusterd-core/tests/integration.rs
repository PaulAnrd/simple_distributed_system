//! End-to-end test of the real Phase 1 flow: generate a CA, issue certs,
//! write configs, run a head and a worker agent over real localhost TCP
//! with real TLS, and confirm `clusterd status`'s underlying query sees the
//! worker join and later flip to down after it stops heartbeating. This is
//! the automated equivalent of `bin/dev-smoke-test.sh` — that script is
//! still useful as a manual, human-readable demo, but this is what
//! actually guards against regressions in CI/`cargo test`.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use uuid::Uuid;

use clusterd_core::config::{self, Config, Role};
use clusterd_core::identity;
use clusterd_core::ipc;

const HEAD_ADDR: &str = "127.0.0.1:18700";

fn write_head_identity(data_dir: &Path, node_name: &str) -> identity::PemPair {
    let ca = identity::generate_ca().unwrap();
    std::fs::write(config::ca_cert_path(data_dir), &ca.cert_pem).unwrap();
    std::fs::write(config::ca_key_path(data_dir), &ca.key_pem).unwrap();

    let node_cert = identity::issue_node_cert(&ca, node_name).unwrap();
    std::fs::write(config::node_cert_path(data_dir), &node_cert.cert_pem).unwrap();
    std::fs::write(config::node_key_path(data_dir), &node_cert.key_pem).unwrap();
    ca
}

fn write_worker_identity(data_dir: &Path, ca: &identity::PemPair, node_name: &str) {
    let node_cert = identity::issue_node_cert(ca, node_name).unwrap();
    std::fs::write(config::ca_cert_path(data_dir), &ca.cert_pem).unwrap();
    std::fs::write(config::node_cert_path(data_dir), &node_cert.cert_pem).unwrap();
    std::fs::write(config::node_key_path(data_dir), &node_cert.key_pem).unwrap();
}

#[tokio::test]
async fn worker_joins_pool_and_flips_down_after_it_stops() {
    let head_dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
    let head_addr: SocketAddr = HEAD_ADDR.parse().unwrap();

    // Short heartbeat interval/timeout so the down-detection half of this
    // test doesn't need to wait out Phase 1's real-world 15s default.
    let heartbeat_interval_secs = 1;
    let heartbeat_timeout_secs = 2;

    let ca = write_head_identity(head_dir.path(), "head-1");
    Config {
        node_id: Uuid::now_v7(),
        node_name: "head-1".to_string(),
        role: Role::Head,
        listen_addr: Some(head_addr),
        head_addr: None,
        heartbeat_interval_secs,
        heartbeat_timeout_secs,
    }
    .save(head_dir.path())
    .unwrap();

    write_worker_identity(worker_dir.path(), &ca, "worker-1");
    Config {
        node_id: Uuid::now_v7(),
        node_name: "worker-1".to_string(),
        role: Role::Worker,
        listen_addr: None,
        head_addr: Some(head_addr),
        heartbeat_interval_secs,
        heartbeat_timeout_secs,
    }
    .save(worker_dir.path())
    .unwrap();

    let head_task = tokio::spawn(clusterd_core::agent::run(head_dir.path().to_path_buf()));
    // Give the head's TCP listener a moment to bind before the worker dials it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let worker_task = tokio::spawn(clusterd_core::agent::run(worker_dir.path().to_path_buf()));

    let uds_path = config::uds_socket_path(head_dir.path());

    // Poll status until the worker shows up (or time out the test) instead
    // of a single fixed sleep, since exact heartbeat timing can vary under
    // CI load.
    let statuses = wait_for(Duration::from_secs(5), || {
        let uds_path = uds_path.clone();
        async move {
            let statuses = ipc::query_status(&uds_path).await.ok()?;
            (statuses.len() == 2 && statuses.iter().all(|s| s.up)).then_some(statuses)
        }
    })
    .await
    .expect("both nodes should be reporting and up within 5s");

    let worker_status = statuses.iter().find(|s| s.node_name == "worker-1").unwrap();
    assert!(worker_status.mem_total_bytes > 0);
    assert_eq!(worker_status.running_jobs, 0);

    worker_task.abort();

    let statuses = wait_for(Duration::from_secs(5), || {
        let uds_path = uds_path.clone();
        async move {
            let statuses = ipc::query_status(&uds_path).await.ok()?;
            let worker = statuses.iter().find(|s| s.node_name == "worker-1")?;
            (!worker.up).then_some(statuses)
        }
    })
    .await
    .expect("worker should flip to down within 5s of stopping");

    let head_status = statuses.iter().find(|s| s.node_name == "head-1").unwrap();
    assert!(
        head_status.up,
        "head should still be reporting itself as up"
    );

    head_task.abort();
}

fn head_config(listen_addr: SocketAddr) -> Config {
    Config {
        node_id: Uuid::now_v7(),
        node_name: "head-1".to_string(),
        role: Role::Head,
        listen_addr: Some(listen_addr),
        head_addr: None,
        heartbeat_interval_secs: 1,
        heartbeat_timeout_secs: 2,
    }
}

/// Regression test: a second agent started on the same data directory used
/// to delete the running agent's control socket before failing on the
/// already-bound port, leaving `clusterd status` broken.
#[tokio::test]
async fn second_agent_on_same_data_dir_is_refused_and_first_keeps_serving() {
    let head_dir = tempfile::tempdir().unwrap();
    write_head_identity(head_dir.path(), "head-1");
    // Port 0: nothing dials this head, so any free port will do.
    head_config("127.0.0.1:0".parse().unwrap())
        .save(head_dir.path())
        .unwrap();

    let head_task = tokio::spawn(clusterd_core::agent::run(head_dir.path().to_path_buf()));
    let uds_path = config::uds_socket_path(head_dir.path());
    wait_for(Duration::from_secs(5), || {
        let uds_path = uds_path.clone();
        async move { ipc::query_status(&uds_path).await.ok() }
    })
    .await
    .expect("first agent should serve status within 5s");

    let second = tokio::time::timeout(
        Duration::from_secs(5),
        clusterd_core::agent::run(head_dir.path().to_path_buf()),
    )
    .await
    .expect("second agent should give up immediately, not keep running");
    assert!(
        matches!(second, Err(clusterd_core::Error::AlreadyRunning { .. })),
        "expected AlreadyRunning, got {second:?}"
    );

    assert!(
        ipc::query_status(&uds_path).await.is_ok(),
        "first agent's control socket must still answer"
    );
    head_task.abort();
}

/// Regression test: the head used to keep a connection open forever once
/// its peer went silent, leaking a task and a socket per vanished worker.
#[tokio::test]
async fn head_closes_a_connection_that_goes_silent() {
    use bytes::Bytes;
    use clusterd_core::tls;
    use clusterd_proto::{message_codec, Message, PROTOCOL_VERSION};
    use futures_util::{SinkExt, StreamExt};
    use tokio_util::codec::Framed;

    const SILENT_TEST_ADDR: &str = "127.0.0.1:18702";
    let head_dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
    let ca = write_head_identity(head_dir.path(), "head-1");
    head_config(SILENT_TEST_ADDR.parse().unwrap())
        .save(head_dir.path())
        .unwrap();
    write_worker_identity(worker_dir.path(), &ca, "worker-1");

    let head_task = tokio::spawn(clusterd_core::agent::run(head_dir.path().to_path_buf()));
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Hand-rolled worker: complete the handshake, then never heartbeat.
    let identity = tls::load_identity(
        &config::node_cert_path(worker_dir.path()),
        &config::node_key_path(worker_dir.path()),
    )
    .unwrap();
    let roots = tls::load_ca_root_store(&config::ca_cert_path(worker_dir.path())).unwrap();
    let connector = tokio_rustls::TlsConnector::from(tls::client_config(identity, roots).unwrap());
    let tcp = tokio::net::TcpStream::connect(SILENT_TEST_ADDR)
        .await
        .unwrap();
    let tls_stream = connector
        .connect(tls::node_server_name(), tcp)
        .await
        .unwrap();
    let mut framed = Framed::new(tls_stream, message_codec());

    let hello = Message::Hello {
        protocol_version: PROTOCOL_VERSION,
        node_id: Uuid::now_v7(),
        node_name: "silent".to_string(),
        cert_fingerprint: String::new(),
    };
    framed
        .send(Bytes::from(hello.encode().unwrap()))
        .await
        .unwrap();
    let ack = framed.next().await.unwrap().unwrap();
    assert!(matches!(
        Message::decode(&ack).unwrap(),
        Message::HelloAck { accepted: true, .. }
    ));

    // The head's heartbeat timeout is 2s; it should hang up soon after.
    let next = tokio::time::timeout(Duration::from_secs(5), framed.next())
        .await
        .expect("head should close a silent connection within 5s");
    assert!(
        !matches!(next, Some(Ok(_))),
        "expected the connection to be closed, got a frame"
    );
    head_task.abort();
}

/// Polls `f` every 100ms until it returns `Some`, or panics-via-`None` if
/// `timeout` elapses first. Small and local to this test file rather than
/// a shared crate helper.
async fn wait_for<F, Fut, T>(timeout: Duration, mut f: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(value) = f().await {
            return Some(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

//! Local control-socket protocol between the `clusterd` CLI and a running
//! agent. Deliberately separate from `clusterd-proto`'s `Message` type:
//! this never crosses a node boundary (see architecture doc — the CLI only
//! ever talks to the agent on the same machine over a Unix domain socket),
//! so it has no auth/versioning concerns and no reason to share a wire
//! format with the cross-node protocol.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::error::Error;
use crate::nodes::{NodeStatus, NodeTable};

const MAX_LOCAL_FRAME_LEN: usize = 4 * 1024 * 1024;

/// Longest path a Unix socket can be bound to: the size of
/// `sockaddr_un.sun_path` (108 bytes on Linux, 104 on macOS and the BSDs)
/// minus its terminating NUL.
#[cfg(target_os = "linux")]
pub const MAX_SOCKET_PATH_LEN: usize = 107;
#[cfg(not(target_os = "linux"))]
pub const MAX_SOCKET_PATH_LEN: usize = 103;

/// Pause after a failed `accept` before trying again; see `serve`.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

#[derive(Debug, Serialize, Deserialize)]
pub enum LocalRequest {
    Status,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum LocalResponse {
    Status(Vec<NodeStatus>),
    Error(String),
}

fn local_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_LOCAL_FRAME_LEN)
        .new_codec()
}

/// Fails with a clear message if `socket_path` is too long to bind a Unix
/// socket to. Checked by `clusterd init` as well as at agent start-up, so a
/// too-deep data directory is reported before anything is written to it.
pub fn check_socket_path(socket_path: &Path) -> Result<(), Error> {
    let len = socket_path.as_os_str().len();
    if len > MAX_SOCKET_PATH_LEN {
        return Err(Error::SocketPathTooLong {
            path: socket_path.to_path_buf(),
            len,
            max: MAX_SOCKET_PATH_LEN,
        });
    }
    Ok(())
}

/// Binds the local control socket. Called at start-up, before the agent
/// starts serving anything, so that a failure stops the agent with a clear
/// error instead of leaving a head running without `clusterd status`.
///
/// The caller must hold the data-directory lock (see `agent::run`): that
/// guarantees no other agent is using this socket, so a file already at the
/// path is a leftover from a crash and is safe to remove.
pub fn bind(socket_path: &Path) -> Result<UnixListener, Error> {
    check_socket_path(socket_path)?;
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path).map_err(|source| Error::BindControlSocket {
        path: socket_path.to_path_buf(),
        source,
    })?;
    tracing::info!(path = %socket_path.display(), "local control socket listening");
    Ok(listener)
}

/// Runs forever, accepting connections from `clusterd status` (or future
/// local subcommands) and answering from the live `node_table`. Only the
/// head runs this — workers have no local status to serve, since the CLI
/// experience is "SSH into the head and run commands there."
pub async fn serve(listener: UnixListener, node_table: Arc<Mutex<NodeTable>>) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _addr)) => stream,
            Err(e) => {
                // Transient (e.g. out of file descriptors): pause so a
                // persistent error doesn't spin, then keep serving.
                tracing::warn!(error = %e, "accepting a local control connection failed; retrying");
                tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                continue;
            }
        };
        let node_table = node_table.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, node_table).await {
                tracing::debug!(error = %e.chain(), "local control connection ended with error");
            }
        });
    }
}

async fn handle_connection(
    stream: UnixStream,
    node_table: Arc<Mutex<NodeTable>>,
) -> Result<(), Error> {
    let mut framed = Framed::new(stream, local_codec());
    let Some(frame) = framed.next().await else {
        return Ok(()); // peer disconnected without sending a request; nothing to do
    };
    let frame = frame?;

    let response = match bincode::deserialize::<LocalRequest>(&frame) {
        Ok(LocalRequest::Status) => {
            let statuses = node_table
                .lock()
                .expect("node table mutex poisoned")
                .snapshot();
            LocalResponse::Status(statuses)
        }
        Err(source) => {
            tracing::debug!(error = %source, "malformed local request");
            LocalResponse::Error(format!("malformed request: {source}"))
        }
    };

    let encoded = bincode::serialize(&response)
        .map_err(|e| Error::LocalIpc(format!("encoding response: {e}")))?;
    framed.send(Bytes::from(encoded)).await?;
    Ok(())
}

/// Client side, used by the `clusterd status` subcommand.
pub async fn query_status(socket_path: &Path) -> Result<Vec<NodeStatus>, Error> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|source| Error::UdsConnect {
            path: socket_path.to_path_buf(),
            source,
        })?;
    let mut framed = Framed::new(stream, local_codec());

    let request =
        bincode::serialize(&LocalRequest::Status).expect("LocalRequest always serializes");
    framed.send(Bytes::from(request)).await?;

    let frame = framed.next().await.ok_or(Error::HandshakeIncomplete)??;
    let response: LocalResponse = bincode::deserialize(&frame)
        .map_err(|e| Error::LocalIpc(format!("decoding response: {e}")))?;

    match response {
        LocalResponse::Status(statuses) => Ok(statuses),
        LocalResponse::Error(message) => Err(Error::LocalIpc(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    #[test]
    fn socket_path_at_the_limit_is_accepted() {
        let path = format!("/{}", "a".repeat(MAX_SOCKET_PATH_LEN - 1));
        assert!(check_socket_path(Path::new(&path)).is_ok());
    }

    #[test]
    fn socket_path_over_the_limit_is_rejected_with_its_length() {
        let path = format!("/{}", "a".repeat(MAX_SOCKET_PATH_LEN));
        match check_socket_path(Path::new(&path)) {
            Err(Error::SocketPathTooLong { len, max, .. }) => {
                assert_eq!(len, MAX_SOCKET_PATH_LEN + 1);
                assert_eq!(max, MAX_SOCKET_PATH_LEN);
            }
            other => panic!("expected SocketPathTooLong, got {other:?}"),
        }
    }

    #[test]
    fn bind_refuses_a_too_long_path_instead_of_failing_later() {
        let path = format!("/{}", "a".repeat(MAX_SOCKET_PATH_LEN));
        assert!(matches!(
            bind(Path::new(&path)),
            Err(Error::SocketPathTooLong { .. })
        ));
    }

    #[tokio::test]
    async fn bind_replaces_a_stale_socket_file_and_serves_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        std::fs::write(&path, b"left over from a crash").unwrap();

        let listener = bind(&path).unwrap();
        let table = Arc::new(Mutex::new(NodeTable::new(StdDuration::from_secs(15))));
        tokio::spawn(serve(listener, table));

        let statuses = query_status(&path).await.unwrap();
        assert!(statuses.is_empty());
    }
}

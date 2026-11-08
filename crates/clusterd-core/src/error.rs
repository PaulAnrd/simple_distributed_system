use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// The one error type for `clusterd-core`. Kept as a single flat enum rather
/// than per-module error types: at this crate's current size that's less
/// ceremony than wiring up `From` conversions between half a dozen tiny
/// error types, while still giving callers (the `clusterd` bin crate)
/// something they can match on instead of anyhow's opaque chains.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading config at {path}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing config at {path}")]
    ParseConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("writing config at {path}")]
    WriteConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serializing config")]
    SerializeConfig(#[source] toml::ser::Error),
    #[error("role = head requires listen_addr to be set")]
    HeadMissingListenAddr,
    #[error("role = worker requires head_addr to be set")]
    WorkerMissingHeadAddr,
    #[error("heartbeat_interval_secs must be at least 1")]
    InvalidHeartbeatInterval,
    #[error(
        "heartbeat_timeout_secs ({timeout}) must be greater than heartbeat_interval_secs ({interval})"
    )]
    HeartbeatTimeoutTooShort { interval: u64, timeout: u64 },

    #[error("generating certificate")]
    CertGen(#[source] rcgen::Error),
    #[error("reading {what} at {path}")]
    ReadPem {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no PEM-encoded certificate found in {path}")]
    NoCertInPem { path: PathBuf },
    #[error("no PEM-encoded private key found in {path}")]
    NoKeyInPem { path: PathBuf },
    #[error("writing private key to {path}")]
    WriteSecret {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("TLS configuration error")]
    Tls(#[from] rustls::Error),

    #[error("io error")]
    Io(#[from] std::io::Error),
    #[error("protocol codec error")]
    Codec(#[from] clusterd_proto::CodecError),

    #[error("connection closed before handshake completed")]
    HandshakeIncomplete,
    #[error("protocol version mismatch: local={local}, remote={remote}")]
    ProtocolMismatch { local: u16, remote: u16 },
    #[error("head rejected connection: {0}")]
    HandshakeRejected(String),
    #[error("unexpected message during handshake: {0:?}")]
    UnexpectedMessage(Box<clusterd_proto::Message>),
    #[error("handshake not completed within {0:?}")]
    HandshakeTimeout(Duration),
    #[error("no message received for {0:?}")]
    IdleTimeout(Duration),

    #[error("data directory {path} is missing {what}; run `clusterd init` first")]
    MissingIdentity { path: PathBuf, what: &'static str },
    #[error("another clusterd agent is already running with data directory {path}")]
    AlreadyRunning { path: PathBuf },
    #[error("locking data directory via {path}")]
    LockDataDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "control socket path {path} is {len} bytes long, but Unix sockets allow at most {max} \
         on this platform; use a shorter --data-dir"
    )]
    SocketPathTooLong {
        path: PathBuf,
        len: usize,
        max: usize,
    },
    #[error("binding local control socket at {path}")]
    BindControlSocket {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "connecting to control socket at {path} (is `clusterd agent run` running on this host?)"
    )]
    UdsConnect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("local control-socket IPC error: {0}")]
    LocalIpc(String),

    #[error("connection closed by peer")]
    ConnectionClosed,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Displays this error followed by every underlying cause, e.g.
    /// "io error: invalid peer certificate: UnknownIssuer". Use this when
    /// logging: the plain `Display` of a wrapping variant such as `Io` is
    /// just "io error", which hides the cause that explains the failure.
    pub fn chain(&self) -> Chain<'_> {
        Chain(self)
    }
}

/// See [`Error::chain`].
pub struct Chain<'a>(&'a (dyn std::error::Error + 'static));

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(cause) = source {
            write!(f, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_includes_every_cause() {
        let err = Error::ReadPem {
            what: "certificate",
            path: PathBuf::from("/data/node.crt"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        assert_eq!(
            err.chain().to_string(),
            "reading certificate at /data/node.crt: no such file"
        );
    }

    #[test]
    fn chain_shows_what_plain_display_hides() {
        let err = Error::Io(std::io::Error::other(
            "invalid peer certificate: UnknownIssuer",
        ));
        assert_eq!(err.to_string(), "io error");
        assert_eq!(
            err.chain().to_string(),
            "io error: invalid peer certificate: UnknownIssuer"
        );
    }

    #[test]
    fn chain_of_error_without_cause_is_its_display() {
        assert_eq!(
            Error::ConnectionClosed.chain().to_string(),
            "connection closed by peer"
        );
    }
}

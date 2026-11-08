use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Head,
    Worker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Stable identity generated once by `clusterd init` and reused across
    /// restarts — the wire protocol and `NodeTable` key everything off this
    /// rather than off node_name (a human might rename a node; node_id
    /// never changes without re-running `init`).
    pub node_id: Uuid,
    pub node_name: String,
    pub role: Role,
    /// Head only: address the control-plane TLS listener binds to.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub listen_addr: Option<SocketAddr>,
    /// Worker only: head's control-plane address to dial.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub head_addr: Option<SocketAddr>,
    #[serde(default = "default_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_heartbeat_timeout_secs")]
    pub heartbeat_timeout_secs: u64,
}

fn default_heartbeat_interval_secs() -> u64 {
    3
}

fn default_heartbeat_timeout_secs() -> u64 {
    15
}

impl Config {
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = config_path(data_dir);
        let text = std::fs::read_to_string(&path).map_err(|source| Error::ReadConfig {
            path: path.clone(),
            source,
        })?;
        let config: Config =
            toml::from_str(&text).map_err(|source| Error::ParseConfig { path, source })?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        self.validate()?;
        let path = config_path(data_dir);
        let text = toml::to_string_pretty(self).map_err(Error::SerializeConfig)?;
        std::fs::write(&path, text).map_err(|source| Error::WriteConfig { path, source })?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        match self.role {
            Role::Head if self.listen_addr.is_none() => return Err(Error::HeadMissingListenAddr),
            Role::Worker if self.head_addr.is_none() => return Err(Error::WorkerMissingHeadAddr),
            _ => {}
        }
        // A zero interval would make the heartbeat timer panic, and a timeout
        // that isn't longer than the interval would report a healthy node
        // down between two of its heartbeats.
        if self.heartbeat_interval_secs == 0 {
            return Err(Error::InvalidHeartbeatInterval);
        }
        if self.heartbeat_timeout_secs <= self.heartbeat_interval_secs {
            return Err(Error::HeartbeatTimeoutTooShort {
                interval: self.heartbeat_interval_secs,
                timeout: self.heartbeat_timeout_secs,
            });
        }
        Ok(())
    }
}

pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.toml")
}

pub fn ca_cert_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ca.crt")
}

pub fn ca_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("ca.key")
}

pub fn node_cert_path(data_dir: &Path) -> PathBuf {
    data_dir.join("node.crt")
}

pub fn node_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("node.key")
}

pub fn uds_socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("clusterd.sock")
}

/// Held locked by a running agent so that a second agent can't start on the
/// same data directory (see `agent::run`).
pub fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("clusterd.lock")
}

pub fn state_db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("state.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_config_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            node_id: Uuid::now_v7(),
            node_name: "head-1".to_string(),
            role: Role::Head,
            listen_addr: Some("0.0.0.0:7700".parse().unwrap()),
            head_addr: None,
            heartbeat_interval_secs: 3,
            heartbeat_timeout_secs: 15,
        };
        config.save(dir.path()).unwrap();
        let loaded = Config::load(dir.path()).unwrap();
        assert_eq!(loaded.node_name, "head-1");
        assert_eq!(loaded.role, Role::Head);
        assert_eq!(loaded.listen_addr, config.listen_addr);
    }

    #[test]
    fn head_without_listen_addr_is_rejected() {
        let config = Config {
            node_id: Uuid::now_v7(),
            node_name: "head-1".to_string(),
            role: Role::Head,
            listen_addr: None,
            head_addr: None,
            heartbeat_interval_secs: 3,
            heartbeat_timeout_secs: 15,
        };
        assert!(matches!(
            config.validate(),
            Err(Error::HeadMissingListenAddr)
        ));
    }

    #[test]
    fn worker_without_head_addr_is_rejected() {
        let config = Config {
            node_id: Uuid::now_v7(),
            node_name: "worker-1".to_string(),
            role: Role::Worker,
            listen_addr: None,
            head_addr: None,
            heartbeat_interval_secs: 3,
            heartbeat_timeout_secs: 15,
        };
        assert!(matches!(
            config.validate(),
            Err(Error::WorkerMissingHeadAddr)
        ));
    }

    fn worker_config(interval: u64, timeout: u64) -> Config {
        Config {
            node_id: Uuid::now_v7(),
            node_name: "worker-1".to_string(),
            role: Role::Worker,
            listen_addr: None,
            head_addr: Some("127.0.0.1:7700".parse().unwrap()),
            heartbeat_interval_secs: interval,
            heartbeat_timeout_secs: timeout,
        }
    }

    #[test]
    fn zero_heartbeat_interval_is_rejected() {
        assert!(matches!(
            worker_config(0, 15).validate(),
            Err(Error::InvalidHeartbeatInterval)
        ));
    }

    #[test]
    fn timeout_not_longer_than_interval_is_rejected() {
        assert!(matches!(
            worker_config(3, 3).validate(),
            Err(Error::HeartbeatTimeoutTooShort {
                interval: 3,
                timeout: 3
            })
        ));
        assert!(worker_config(3, 4).validate().is_ok());
    }

    #[test]
    fn invalid_timing_in_file_is_rejected_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let text = toml::to_string_pretty(&worker_config(0, 15)).unwrap();
        std::fs::write(config_path(dir.path()), text).unwrap();
        assert!(matches!(
            Config::load(dir.path()),
            Err(Error::InvalidHeartbeatInterval)
        ));
    }

    #[test]
    fn load_missing_file_gives_read_config_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Config::load(dir.path()),
            Err(Error::ReadConfig { .. })
        ));
    }
}

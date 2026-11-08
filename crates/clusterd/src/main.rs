use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::prelude::*;
use uuid::Uuid;

use clusterd_core::config::{self, Config, Role};
use clusterd_core::identity::{self, write_secret, PemPair};
use clusterd_core::ipc;

#[derive(Parser)]
#[command(name = "clusterd", version, about = "Minimal Linux cluster daemon")]
struct Cli {
    /// Directory holding this node's config, certs, and local state.
    #[arg(long, global = true, default_value = "./clusterd-data")]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bootstrap this machine's data directory: writes a config and, for a
    /// head, generates a fresh cluster CA plus the head's own certificate.
    Init(InitArgs),
    /// Certificate management (run against a head's data directory).
    Cert {
        #[command(subcommand)]
        command: CertCommand,
    },
    /// Agent process management.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Show live pool status: nodes, resources, up/down.
    Status,
}

#[derive(clap::Args)]
struct InitArgs {
    #[arg(long)]
    node_name: String,
    #[arg(long, value_enum)]
    role: RoleArg,
    /// Head only: address to listen on, e.g. 0.0.0.0:7700
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Worker only: head's address to dial, e.g. 192.168.1.10:7700
    #[arg(long)]
    head: Option<SocketAddr>,
}

#[derive(ValueEnum, Clone, Copy)]
enum RoleArg {
    Head,
    Worker,
}

#[derive(Subcommand)]
enum CertCommand {
    /// Issue a certificate for a new node, signed by this data directory's
    /// CA. Only valid against a head's data directory. Copy the resulting
    /// ca.crt/node.crt/node.key to the target machine to complete setup —
    /// `clusterd join` automates that copy starting in Phase 3.
    Issue {
        #[arg(long)]
        name: String,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Run the agent in the foreground. This is what a systemd unit execs
    /// once Phase 3 automates service installation; for now, run it
    /// directly (e.g. under `tmux`/`screen`, or let it run in the
    /// foreground of an SSH session).
    Run,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Command::Init(args) => cmd_init(&cli.data_dir, args),
        Command::Cert {
            command: CertCommand::Issue { name, out },
        } => cmd_cert_issue(&cli.data_dir, &name, &out),
        Command::Agent {
            command: AgentCommand::Run,
        } => clusterd_core::agent::run(cli.data_dir)
            .await
            .map_err(Into::into),
        Command::Status => cmd_status(&cli.data_dir).await,
    }
}

fn cmd_init(data_dir: &Path, args: InitArgs) -> anyhow::Result<()> {
    // Re-initialising replaces this node's identity (node_id, certificate)
    // and, on a head, the cluster CA — which silently cuts off every
    // worker. Never do that implicitly.
    let existing = [config::config_path(data_dir), config::ca_key_path(data_dir)];
    if let Some(found) = existing.iter().find(|p| p.exists()) {
        anyhow::bail!(
            "{} is already initialised ({} exists); re-initialising would replace this \
             node's identity{}. To really start over, delete the data directory first.",
            data_dir.display(),
            found.display(),
            if matches!(args.role, RoleArg::Head) {
                " and the cluster CA, cutting off every worker"
            } else {
                ""
            }
        );
    }
    if matches!(args.role, RoleArg::Head) {
        // Fail now, before writing anything, rather than when the agent
        // starts and can't bind its control socket.
        ipc::check_socket_path(&config::uds_socket_path(data_dir))?;
    }

    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data directory {}", data_dir.display()))?;

    let node_id = Uuid::now_v7();
    let config = match args.role {
        RoleArg::Head => {
            let listen_addr = args
                .listen
                .context("--listen is required for --role head, e.g. --listen 0.0.0.0:7700")?;
            Config {
                node_id,
                node_name: args.node_name.clone(),
                role: Role::Head,
                listen_addr: Some(listen_addr),
                head_addr: None,
                heartbeat_interval_secs: 3,
                heartbeat_timeout_secs: 15,
            }
        }
        RoleArg::Worker => {
            let head_addr = args
                .head
                .context("--head is required for --role worker, e.g. --head 192.168.1.10:7700")?;
            Config {
                node_id,
                node_name: args.node_name.clone(),
                role: Role::Worker,
                listen_addr: None,
                head_addr: Some(head_addr),
                heartbeat_interval_secs: 3,
                heartbeat_timeout_secs: 15,
            }
        }
    };
    config.save(data_dir)?;

    match config.role {
        Role::Head => {
            let ca = identity::generate_ca().context("generating cluster CA")?;
            write_secret(&config::ca_key_path(data_dir), &ca.key_pem)?;
            std::fs::write(config::ca_cert_path(data_dir), &ca.cert_pem)?;

            let node_cert = identity::issue_node_cert(&ca, &args.node_name)
                .context("issuing head's own node certificate")?;
            write_secret(&config::node_key_path(data_dir), &node_cert.key_pem)?;
            std::fs::write(config::node_cert_path(data_dir), &node_cert.cert_pem)?;

            println!(
                "Initialized head node '{}' in {}",
                args.node_name,
                data_dir.display()
            );
            println!("Listening on {}", config.listen_addr.unwrap());
            println!();
            println!("To add a worker, issue it a certificate from this data directory:");
            println!(
                "  clusterd --data-dir {} cert issue --name <worker-name> --out <local-output-dir>",
                data_dir.display()
            );
            println!("then copy that output directory's ca.crt/node.crt/node.key to the worker's data directory.");
        }
        Role::Worker => {
            println!(
                "Initialized worker config '{}' in {}",
                args.node_name,
                data_dir.display()
            );
            println!(
                "Now copy ca.crt, node.crt, node.key (from `clusterd cert issue` run on the head) into {}",
                data_dir.display()
            );
            println!(
                "Then run: clusterd --data-dir {} agent run",
                data_dir.display()
            );
        }
    }

    Ok(())
}

fn cmd_cert_issue(data_dir: &Path, name: &str, out: &Path) -> anyhow::Result<()> {
    let cert_pem = std::fs::read_to_string(config::ca_cert_path(data_dir)).with_context(|| {
        format!(
            "reading CA cert from {} (is this a head's data directory? run `clusterd init --role head` first)",
            data_dir.display()
        )
    })?;
    let key_pem = std::fs::read_to_string(config::ca_key_path(data_dir))
        .with_context(|| format!("reading CA key from {}", data_dir.display()))?;
    let ca = PemPair { cert_pem, key_pem };

    let node_cert = identity::issue_node_cert(&ca, name)
        .with_context(|| format!("issuing certificate for '{name}'"))?;

    std::fs::create_dir_all(out)
        .with_context(|| format!("creating output directory {}", out.display()))?;
    // Writing into the head's own data directory would copy ca.crt onto
    // itself — which truncates it to zero bytes — and replace the head's own
    // certificate and key with the new node's.
    if std::fs::canonicalize(out)? == std::fs::canonicalize(data_dir)? {
        anyhow::bail!(
            "--out {} is the head's own data directory; writing there would destroy the \
             cluster CA certificate and the head's own key. Choose another directory.",
            out.display()
        );
    }
    std::fs::copy(config::ca_cert_path(data_dir), out.join("ca.crt"))?;
    std::fs::write(out.join("node.crt"), &node_cert.cert_pem)?;
    write_secret(&out.join("node.key"), &node_cert.key_pem)?;

    println!("Issued certificate for '{name}' in {}", out.display());
    println!("Copy it to the target machine's data directory, e.g.:");
    println!(
        "  scp {}/ca.crt {}/node.crt {}/node.key user@host:/path/to/data-dir/",
        out.display(),
        out.display(),
        out.display()
    );

    Ok(())
}

async fn cmd_status(data_dir: &Path) -> anyhow::Result<()> {
    let socket_path = config::uds_socket_path(data_dir);
    let statuses = ipc::query_status(&socket_path)
        .await
        .context("querying local agent for status")?;

    if statuses.is_empty() {
        println!("No nodes reporting yet.");
        return Ok(());
    }

    println!(
        "{:<16} {:<6} {:>9} {:>10} {:>10} {:>10} {:>5} {:<10} {:>10}",
        "NAME",
        "UP",
        "CPU FREE",
        "MEM TOTAL",
        "MEM FREE",
        "DISK FREE",
        "JOBS",
        "VERSION",
        "LAST SEEN"
    );
    for s in statuses {
        println!(
            "{:<16} {:<6} {:>8.1}% {:>10} {:>10} {:>10} {:>5} {:<10} {:>9}",
            s.node_name,
            if s.up { "up" } else { "DOWN" },
            s.cpu_free_pct,
            human_bytes(s.mem_total_bytes),
            human_bytes(s.mem_free_bytes),
            human_bytes(s.disk_free_bytes),
            s.running_jobs,
            s.agent_version,
            format!("{}s ago", s.last_seen_secs_ago),
        );
    }
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_idx = 0;
    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }
    format!("{value:.1}{}", UNITS[unit_idx])
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    #[cfg(target_os = "linux")]
    {
        if let Ok(journald_layer) = tracing_journald::layer() {
            tracing_subscriber::registry()
                .with(filter)
                .with(journald_layer)
                .init();
            return;
        }
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head_args() -> InitArgs {
        InitArgs {
            node_name: "head-1".to_string(),
            role: RoleArg::Head,
            listen: Some("127.0.0.1:7700".parse().unwrap()),
            head: None,
        }
    }

    #[test]
    fn init_refuses_an_already_initialised_head() {
        let dir = tempfile::tempdir().unwrap();
        cmd_init(dir.path(), head_args()).unwrap();
        let ca_before = std::fs::read(config::ca_cert_path(dir.path())).unwrap();

        let err = cmd_init(dir.path(), head_args()).unwrap_err();
        assert!(err.to_string().contains("already initialised"));
        assert_eq!(
            std::fs::read(config::ca_cert_path(dir.path())).unwrap(),
            ca_before,
            "the cluster CA must be left untouched"
        );
    }

    #[test]
    fn init_refuses_a_head_whose_socket_path_would_be_too_long() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("d".repeat(ipc::MAX_SOCKET_PATH_LEN));
        let err = cmd_init(&deep, head_args()).unwrap_err();
        assert!(format!("{err:#}").contains("use a shorter --data-dir"));
        assert!(!deep.exists(), "nothing should be written");
    }

    #[test]
    fn cert_issue_refuses_the_head_data_directory_as_output() {
        // Regression test: this used to truncate ca.crt to zero bytes.
        let dir = tempfile::tempdir().unwrap();
        cmd_init(dir.path(), head_args()).unwrap();
        let ca_before = std::fs::read(config::ca_cert_path(dir.path())).unwrap();
        let key_before = std::fs::read(config::node_key_path(dir.path())).unwrap();

        // Same directory, spelled differently.
        let same = dir.path().join(".");
        assert!(cmd_cert_issue(dir.path(), "worker-1", &same).is_err());
        assert_eq!(
            std::fs::read(config::ca_cert_path(dir.path())).unwrap(),
            ca_before
        );
        assert_eq!(
            std::fs::read(config::node_key_path(dir.path())).unwrap(),
            key_before
        );
    }

    #[test]
    fn cert_issue_writes_the_three_files_elsewhere() {
        let head = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        cmd_init(head.path(), head_args()).unwrap();
        cmd_cert_issue(head.path(), "worker-1", out.path()).unwrap();
        for file in ["ca.crt", "node.crt", "node.key"] {
            assert!(out.path().join(file).exists(), "{file} missing");
        }
    }
}

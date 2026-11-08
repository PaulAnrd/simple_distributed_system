# clusterd architecture

## Why this exists

Bundle several under-powered Linux machines into one pooled system: submit
simulation jobs from one place and have them load-balanced onto whichever
node has free capacity, and pool disk space across nodes with parity-based
redundancy ("RAID 5" — tolerate losing a disk/node without losing data).
Zero changes to the simulation code. Administered entirely over SSH, and
must feel like one system, not a fleet of machines to manage individually.

This started as a prototype but is intended to grow into a production
system in a critical environment where failure carries real cost. That
doesn't change the phased approach — a small, well-tested foundation *is*
the stable way to start — but it does raise the bar within each phase:
thorough test coverage from Phase 1 onward, careful handling of
untrusted/network input, and avoiding privilege or assumptions that would
be hard to walk back later (see "Security posture" below). This is a solid
engineering foundation, not a security audit or certification — before this
carries real critical-environment weight, it needs a dedicated
security/reliability review beyond what's been done here.

We explicitly do **not** attempt transparent distributed shared memory or
CPU pooling for a single unmodified process across machines. That was the
original ask; after digging into the actual workload (CPU-bound *and* a
memory gap needing several machines' worth, in a managed runtime), the
honest technical answer was that no minimal/lightweight system can deliver
it — the real "make N machines look like one" technology is either
proprietary hypervisor-level tooling needing specialized RDMA hardware
(ScaleMP, TidalScale), or old research systems (Kerrighed, openMosix) that
are abandoned and were never fast. The goal was reframed to what's actually
tractable: job-level parallelism (many independent simulation runs,
load-balanced across nodes) plus pooled redundant storage. Both are
solvable with zero changes to the simulation code, which was the hard
constraint that made the original ask infeasible in the first place.

## Guiding principle

Delegate to proven tools wherever one already solves the problem correctly;
write from-scratch Rust only for the coordination logic specific to this
project.

| Problem | Delegate to | Our code's job |
|---|---|---|
| Redundant distributed storage (Phase 4) | SeaweedFS (`weed`) | generate its config, drive it via systemd, pick redundancy ratios |
| Process supervision / boot-start (Phase 3) | systemd | generate unit files, enable/start, poll status |
| Remote auth + first contact with a new node (Phase 3) | system `ssh`/`scp` | shell out; never reimplement SSH |
| Encryption + mutual auth for our own protocol (Phase 1) | rustls | tiny self-issued cluster CA, one leaf cert per node |
| CPU/RAM/disk telemetry (Phase 1) | `sysinfo` crate | sample every heartbeat, feed the scheduler |

## Status: what's built vs. roadmap

| Phase | Status | Delivers |
|---|---|---|
| 0 — workspace skeleton | **Done** | Cargo workspace, `clusterd-proto`/`clusterd-core`/`clusterd` |
| 1 — heartbeat/status | **Done** | Agent runtime, mTLS, live node table, `clusterd status`, `clusterd init`/`cert issue` |
| 2 — job submission & scheduling | Not started | `clusterd run`, placement, log streaming, job persistence |
| 3 — SSH `join` automation | Not started | One-command node onboarding, systemd unit install |
| 4 — SeaweedFS storage orchestration | Not started | Pooled redundant storage, FUSE mount |

Each phase is scoped to land as a complete, working increment rather than
one large partially-working system — distributed systems don't work as one
giant first commit.

## Topology

Every node — head included — runs the same `clusterd agent run` binary.
Role (`Head`/`Worker`) is a config toggle, not a different binary. Head is a
superset: it does everything a worker does (contributes its own CPU/RAM/disk
to the pool — see `agent::self_heartbeat_loop`), plus runs the scheduler
(Phase 2) and the CLI's local control socket.

```
crates/
  clusterd-proto/    wire Message enum + framing — the cross-node contract
  clusterd-core/      agent runtime, TLS/identity, node table, resource
                       sampling, local IPC — everything as a library
  clusterd/            clap CLI; thin wiring over clusterd-core
```

`clusterd-core` modules, Phase 1:

- `config` — TOML config (role, addresses, heartbeat timing), plus the data
  directory's file layout (`config.toml`, `ca.crt`/`ca.key`,
  `node.crt`/`node.key`, `clusterd.sock`).
- `identity` — CA and per-node certificate generation (`rcgen`).
- `tls` — builds rustls `ServerConfig`/`ClientConfig` for mutual TLS from
  loaded PEM material; installs the `ring` crypto provider.
- `nodes` — the live node table: heartbeat recording, up/down timeout
  logic. Pure and synchronous by design — no networking, so it's trivially
  unit-testable against a fabricated clock instead of real sleeps.
- `resources` — CPU/RAM/disk sampling via `sysinfo`.
- `agent` — the actual runtime: head's TLS listener + connection handling,
  worker's connect-and-heartbeat loop with reconnect/backoff.
- `ipc` — local Unix-domain-socket protocol between the CLI and a running
  agent (`clusterd status`). Deliberately separate from the cross-node wire
  protocol — this never leaves the machine, so it has no auth/versioning
  concerns of its own.

No database yet: Phase 1 has no state worth persisting beyond config and
certs (the node table is live telemetry that correctly rebuilds itself from
incoming heartbeats after a restart). `rusqlite` arrives with Phase 2 for
job history/queue state — adding it now would mean an unused dependency
with nothing to persist.

## Protocol

Custom TCP + TLS + bincode, not gRPC and not QUIC. gRPC/tonic pulls in a
large dependency tree (prost/tonic/hyper/h2/tower) built for many-service,
load-balanced-microservice problems this system doesn't have. QUIC/quinn's
value is surviving packet loss and connection migration on unstable
networks — this is a trusted LAN, so that complexity buys nothing here. See
`doc/protocol.md` for the full message reference.

Worker dials head (not the other way around), sends a `Hello`, and on
`HelloAck{accepted: true}` starts heartbeating on a timer.
Every node, including the head connecting to itself in spirit, is
identified by a stable `node_id` (UUID v7, generated once by `clusterd
init` and stored in `config.toml`) — not by name, which a human might
change, and not by TLS identity, since every node's certificate carries the
same fixed SAN (see `identity::NODE_SAN`) rather than a real hostname.

The CLI never talks over the network: `clusterd status` connects to the
local agent's Unix domain socket only, so running it after SSHing into the
head needs no network-auth setup of its own, and it answers from the head's
already-accumulated heartbeat data with no round trip.

## Data directory layout

Each node's `--data-dir` (default `./clusterd-data` for local dev; a real
deployment should point this at `/var/lib/clusterd` once Phase 3 adds
systemd packaging) holds:

```
config.toml       role, node_id, addresses, heartbeat timing
ca.crt            cluster CA certificate (head only generates ca.key too)
ca.key            cluster CA private key — 0600, head only
node.crt          this node's leaf certificate, signed by the cluster CA
node.key          this node's private key — 0600
clusterd.sock     local control socket (head only; created at runtime)
```

Worker onboarding in Phase 1 is manual: `clusterd cert issue` on the head
produces a `ca.crt`/`node.crt`/`node.key` triple to copy (`scp`) into a
worker's data directory after `clusterd init --role worker` there. Phase 3's
`clusterd join` automates that copy over SSH so it becomes one command run
from the head.

## Security posture (current, and what's deliberately deferred)

- Mutual TLS rooted in a small self-issued cluster CA — not a bare shared
  secret, not the system trust store. Both ends of every connection
  present and verify a certificate.
- Private key files are written with `0600` permissions from creation, not
  tightened after the fact.
- The agent runs unprivileged in Phase 1 — nothing it does needs root
  (`sysinfo` reads `/proc`/disk stats without special permissions). Root
  becomes necessary in Phase 3 (systemd unit installation) and Phase 4
  (FUSE mount management); when we get there, evaluate narrow Linux
  capabilities or a sudo-scoped helper rather than defaulting the whole
  agent to root.
- The frame decoder — the one piece of code parsing bytes an attacker or a
  corrupted connection fully controls — has adversarial and property-based
  fuzz tests (`clusterd-proto/src/lib.rs`'s `fuzz_tests` module) confirming
  it errors rather than panics on arbitrary input.
- Head node is an accepted single point of failure for now (scheduler in
  Phase 2, storage master in Phase 4 both live there). SeaweedFS supports
  multi-master Raft groups natively, so hardening this later is a bounded,
  well-understood upgrade rather than a redesign — reasonable to defer
  rather than build upfront.
- **This is not a security audit.** Before this carries real
  critical-environment weight, it needs a dedicated security/reliability
  review beyond what's been done here.

## Verification

- `cargo build --workspace` / `cargo test --workspace` / `cargo clippy
  --workspace --all-targets` / `cargo fmt --all -- --check` — all clean as
  of Phase 1.
- `bin/dev-smoke-test.sh` — manual, human-readable local demo: spins up one
  head and two workers as separate localhost processes, shows `clusterd
  status` reporting all three live, kills a worker, confirms it flips to
  `DOWN` after the heartbeat timeout.
- `crates/clusterd-core/tests/integration.rs` — the automated equivalent,
  run by `cargo test`: real TCP+TLS between a head and worker agent (not
  just in-process function calls), asserting the same join/timeout
  behavior the shell script demonstrates.
- `doc/testing-vms.md` — runbook for validating on real, separate Ubuntu
  machines rather than localhost processes.

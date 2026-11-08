# Wire protocol reference

Defined in `crates/clusterd-proto/src/message.rs`. One `Message` enum,
bincode-encoded, carried over a length-delimited frame
(`tokio_util::codec::LengthDelimitedCodec`, max 16 MiB/frame — see
`clusterd_proto::MAX_FRAME_LEN`) inside a TLS 1.3 connection authenticated
by the cluster's self-issued CA (see `doc/architecture.md`).

Transport: one persistent TCP+TLS connection per worker, **worker-initiated**
— the worker dials the head, not the reverse. This means only the worker
needs to know the head's address; the head never needs to track or dial out
to worker IPs, which also survives a worker's IP changing (e.g. DHCP
lease renewal) since it's the worker reconnecting, not the head reaching in.

## Handshake

1. Worker connects, completes the TLS handshake (mutual — both sides
   present a cert signed by the cluster CA), then sends `Hello` as the
   first frame.
2. Head replies `HelloAck`. If `protocol_version` doesn't match, `accepted`
   is `false`, `reason` explains why, and the head closes the connection
   right after sending it. `PROTOCOL_VERSION` (currently `1`) bumps on any
   wire-incompatible change to `Message`, so a version mismatch fails
   loudly and immediately instead of silently misparsing bytes on either
   end.
3. On `accepted: true`, the worker starts sending `Heartbeat` on a timer
   (`heartbeat_interval_secs` from config, default 3s).

## Messages (Phase 1)

### `Hello` (worker → head, first frame only)

| Field | Type | Notes |
|---|---|---|
| `protocol_version` | `u16` | Must match the head's `PROTOCOL_VERSION` |
| `node_id` | `Uuid` (v7) | Stable identity, generated once by `clusterd init`, stored in `config.toml` |
| `node_name` | `String` | Human-readable, can be renamed without changing `node_id` |
| `cert_fingerprint` | `String` | Currently unused by the head (TLS already verified the cert); reserved for future audit logging |

### `HelloAck` (head → worker, reply to `Hello`)

| Field | Type | Notes |
|---|---|---|
| `protocol_version` | `u16` | Head's own version, so a mismatched worker can log what it expected vs. got |
| `accepted` | `bool` | `false` only on version mismatch in Phase 1 |
| `reason` | `Option<String>` | Human-readable; not meant for programmatic branching |

### `Heartbeat` (worker → head, on a timer; head also feeds itself these synthetically)

| Field | Type | Notes |
|---|---|---|
| `node_id` | `Uuid` | Must match the `Hello` this connection registered with — a mismatch is logged and the heartbeat is dropped, not applied under the wrong identity |
| `cpu_free_pct` | `f32` | `0.0..=100.0`, see `resources::ResourceSampler` |
| `mem_total_bytes` / `mem_free_bytes` | `u64` | From `sysinfo` |
| `disk_free_bytes` | `u64` | Free space on the filesystem containing this node's data directory (Phase 1; Phase 4 will report the dedicated storage-volume path instead) |
| `running_jobs` | `Vec<Uuid>` | Always empty until Phase 2 |
| `agent_version` | `String` | `CARGO_PKG_VERSION` of the sending binary |
| `timestamp` | `SystemTime` | Wall-clock time the sample was taken; **not** used for timeout math (see below) |

## Up/down detection

The head's `NodeTable` (`clusterd-core/src/nodes.rs`) tracks the
`std::time::Instant` each node's heartbeat was *received*, not the
`SystemTime` it claims to have been sent — `Instant` is monotonic and
immune to clock skew/adjustment between machines, which wall-clock
timestamps are not. A node is `up` as long as a heartbeat arrived within
`heartbeat_timeout_secs` (default 15s) of "now"; past that, `clusterd
status` reports it `DOWN`. There's no separate "node left" message — a
dropped connection and a silently-stalled one look the same from the head's
perspective, and both are handled by the same timeout.

## Local CLI protocol (not cross-node — separate from the above)

`crates/clusterd-core/src/ipc.rs` defines `LocalRequest`/`LocalResponse`,
used only between the `clusterd` CLI and the agent running on the *same*
machine, over a Unix domain socket (`clusterd.sock` in the data directory).
Same length-delimited framing primitive, deliberately not the same enum or
even the same codec instance as the cross-node protocol — this traffic
never crosses a node boundary, so it has none of the versioning/auth
concerns `Message` has to carry.

| Request | Response | Notes |
|---|---|---|
| `Status` | `Status(Vec<NodeStatus>)` or `Error(String)` | Backs `clusterd status`; answered from the head's already-accumulated `NodeTable`, no network round trip |

## Planned additions (not yet implemented)

- **Phase 2**: `JobAssign`, `JobCancel`, `JobStarted`, `JobLogChunk`,
  `JobResult` — job dispatch and log streaming. New enum variants are
  additive under bincode's encoding, so this doesn't require breaking the
  Phase 1 wire format.
- **Phase 3**: `NodeJoin`/`NodeJoinAck` — currently node registration
  happens implicitly via `Hello` against a manually-provisioned cert; these
  formalize the SSH-driven join flow.

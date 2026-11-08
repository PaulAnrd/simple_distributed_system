use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use uuid::Uuid;

/// Bumped on any wire-incompatible change to [`Message`]. Checked during the
/// `Hello` handshake so a mismatched agent version fails loudly and
/// immediately instead of silently misparsing bytes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Upper bound on a single encoded message, enforced by both the frame codec
/// (see [`crate::codec::message_codec`]) and [`Message::decode`]. Without a
/// cap, a corrupt or hostile length prefix could trigger an unbounded
/// allocation before we ever get to parsing the payload.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Messages exchanged between a worker's agent and the head's agent over the
/// mTLS control connection. Phase 1 only ever sends the three handshake /
/// telemetry variants below; job dispatch and node-join messages are added
/// in later phases without needing to break this wire format, since new
/// enum variants are additive under bincode's encoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Message {
    /// First frame sent by whichever side initiated the connection (always
    /// the worker in Phase 1 — see `clusterd-core::agent`).
    Hello {
        protocol_version: u16,
        node_id: Uuid,
        node_name: String,
        cert_fingerprint: String,
    },
    /// Response to `Hello`. If `accepted` is false the connection is closed
    /// immediately after this frame; `reason` is meant for a human reading
    /// logs, not for programmatic branching.
    HelloAck {
        protocol_version: u16,
        accepted: bool,
        reason: Option<String>,
    },
    /// Sent by a worker on a fixed timer once the handshake completes; the
    /// head also feeds itself synthetic heartbeats of this shape for its own
    /// resources (see `clusterd-core::agent::self_heartbeat_loop`) so head
    /// and workers are reported identically by `clusterd status`.
    Heartbeat {
        node_id: Uuid,
        cpu_free_pct: f32,
        mem_total_bytes: u64,
        mem_free_bytes: u64,
        disk_free_bytes: u64,
        running_jobs: Vec<Uuid>,
        agent_version: String,
        timestamp: SystemTime,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("failed to encode message")]
    Encode(#[source] bincode::Error),
    #[error("failed to decode message")]
    Decode(#[source] bincode::Error),
    #[error("frame exceeds maximum length of {max} bytes (got {got})")]
    FrameTooLarge { max: usize, got: usize },
}

impl Message {
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        let bytes = bincode::serialize(self).map_err(CodecError::Encode)?;
        if bytes.len() > MAX_FRAME_LEN {
            return Err(CodecError::FrameTooLarge {
                max: MAX_FRAME_LEN,
                got: bytes.len(),
            });
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() > MAX_FRAME_LEN {
            return Err(CodecError::FrameTooLarge {
                max: MAX_FRAME_LEN,
                got: bytes.len(),
            });
        }
        bincode::deserialize(bytes).map_err(CodecError::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_heartbeat() -> Message {
        Message::Heartbeat {
            node_id: Uuid::now_v7(),
            cpu_free_pct: 42.5,
            mem_total_bytes: 16_000_000_000,
            mem_free_bytes: 4_000_000_000,
            disk_free_bytes: 500_000_000_000,
            running_jobs: vec![Uuid::now_v7(), Uuid::now_v7()],
            agent_version: "0.1.0".to_string(),
            timestamp: SystemTime::now(),
        }
    }

    #[test]
    fn round_trips_every_variant() {
        let hello = Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            node_id: Uuid::now_v7(),
            node_name: "node-a".to_string(),
            cert_fingerprint: "deadbeef".to_string(),
        };
        let ack = Message::HelloAck {
            protocol_version: PROTOCOL_VERSION,
            accepted: true,
            reason: None,
        };
        let heartbeat = sample_heartbeat();

        for msg in [hello, ack, heartbeat] {
            let encoded = msg.encode().expect("encode should succeed");
            let decoded = Message::decode(&encoded).expect("decode should succeed");
            assert_eq!(msg, decoded);
        }
    }

    #[test]
    fn decode_rejects_truncated_bytes() {
        let encoded = sample_heartbeat().encode().unwrap();
        // Chop the payload at every prefix length and confirm we get a
        // decode error, never a panic.
        for cut in 0..encoded.len() {
            let truncated = &encoded[..cut];
            assert!(Message::decode(truncated).is_err());
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        let garbage = vec![0xFFu8; 64];
        assert!(Message::decode(&garbage).is_err());
    }

    #[test]
    fn decode_rejects_empty_input() {
        assert!(Message::decode(&[]).is_err());
    }

    #[test]
    fn decode_rejects_oversized_claim() {
        // A buffer that's individually valid-looking but larger than
        // MAX_FRAME_LEN must be rejected before bincode ever touches it.
        let oversized = vec![0u8; MAX_FRAME_LEN + 1];
        match Message::decode(&oversized) {
            Err(CodecError::FrameTooLarge { .. }) => {}
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }
}

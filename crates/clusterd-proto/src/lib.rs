//! Wire protocol shared by every `clusterd` node: the [`Message`] enum sent
//! between agents, and the framing ([`codec::message_codec`]) used to carry
//! it over a TCP+TLS stream. This crate has no async runtime opinions of its
//! own beyond `tokio_util`'s codec trait — connection handling lives in
//! `clusterd-core`.

mod codec;
mod message;

pub use codec::message_codec;
pub use message::{CodecError, Message, MAX_FRAME_LEN, PROTOCOL_VERSION};

#[cfg(test)]
mod fuzz_tests {
    use crate::message::Message;
    use proptest::prelude::*;

    proptest! {
        /// However mangled the input, decode() must return an error, never
        /// panic. This is the one function in the whole system that parses
        /// bytes an attacker (or a corrupted connection) fully controls.
        #[test]
        fn decode_never_panics_on_arbitrary_bytes(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
            let _ = Message::decode(&bytes);
        }
    }
}

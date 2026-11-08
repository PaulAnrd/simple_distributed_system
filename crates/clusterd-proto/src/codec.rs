use tokio_util::codec::LengthDelimitedCodec;

use crate::message::MAX_FRAME_LEN;

/// The one place both sides of a connection configure frame length limits.
/// Head and worker must use an identically-configured codec or a message
/// that's valid to one side could be rejected (or mis-length-prefixed) by
/// the other.
pub fn message_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{CodecError, Message};
    use bytes::Bytes;
    use futures_util::{SinkExt, StreamExt};
    use tokio_util::codec::Framed;

    #[tokio::test]
    async fn framed_round_trip_over_duplex_stream() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut client = Framed::new(client, message_codec());
        let mut server = Framed::new(server, message_codec());

        let msg = Message::HelloAck {
            protocol_version: crate::message::PROTOCOL_VERSION,
            accepted: true,
            reason: None,
        };
        let encoded = msg.encode().unwrap();
        client.send(Bytes::from(encoded)).await.unwrap();

        let frame = server.next().await.unwrap().unwrap();
        let decoded = Message::decode(&frame).unwrap();
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn oversized_frame_prefix_is_rejected_not_panicked_on() {
        // Hand-craft a length-delimited frame whose 4-byte big-endian prefix
        // claims a size larger than MAX_FRAME_LEN, and confirm the codec
        // returns an error on read instead of trying to allocate it.
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let huge_len = (MAX_FRAME_LEN as u32) + 1;
        writer.write_all(&huge_len.to_be_bytes()).await.unwrap();

        let mut framed = Framed::new(reader, message_codec());
        let result = framed.next().await;
        assert!(matches!(result, Some(Err(_))));
    }

    #[test]
    fn codec_error_display_is_human_readable() {
        let err = CodecError::FrameTooLarge {
            max: MAX_FRAME_LEN,
            got: MAX_FRAME_LEN + 1,
        };
        let text = err.to_string();
        assert!(text.contains("exceeds maximum length"));
    }
}

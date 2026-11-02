//! Length-prefixed JSON frames used for control messages and HTTP headers.
//!
//! Wire format: a 4-byte big-endian length followed by that many bytes of UTF-8 JSON.
//! Lengths are checked against the caller's limit before any allocation.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::Message;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {size} bytes exceeds the {limit} byte limit")]
    TooLarge { size: usize, limit: usize },
    #[error("frame has zero length")]
    Empty,
    #[error("frame is not a valid protocol message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("stream error: {0}")]
    Io(#[from] std::io::Error),
}

pub async fn write_frame<W>(
    writer: &mut W,
    message: &Message,
    limit: usize,
) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(message)?;
    if payload.len() > limit {
        return Err(FrameError::TooLarge {
            size: payload.len(),
            limit,
        });
    }
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::TooLarge {
        size: payload.len(),
        limit,
    })?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    Ok(())
}

pub async fn read_frame<R>(reader: &mut R, limit: usize) -> Result<Message, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut length_bytes = [0u8; 4];
    reader.read_exact(&mut length_bytes).await?;
    let length = u32::from_be_bytes(length_bytes) as usize;
    if length == 0 {
        return Err(FrameError::Empty);
    }
    if length > limit {
        return Err(FrameError::TooLarge {
            size: length,
            limit,
        });
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{MAX_CONTROL_FRAME_BYTES, Ping};

    fn ping(nonce: u64) -> Message {
        Message::Ping(Ping { nonce })
    }

    #[tokio::test]
    async fn round_trips_a_message() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        write_frame(&mut client, &ping(7), MAX_CONTROL_FRAME_BYTES)
            .await
            .unwrap();
        let received = read_frame(&mut server, MAX_CONTROL_FRAME_BYTES)
            .await
            .unwrap();
        assert_eq!(received, ping(7));
    }

    #[tokio::test]
    async fn reassembles_frames_split_across_small_reads() {
        let (mut client, mut server) = tokio::io::duplex(3);
        let writer = tokio::spawn(async move {
            write_frame(&mut client, &ping(42), MAX_CONTROL_FRAME_BYTES)
                .await
                .unwrap();
        });
        let received = read_frame(&mut server, MAX_CONTROL_FRAME_BYTES)
            .await
            .unwrap();
        writer.await.unwrap();
        assert_eq!(received, ping(42));
    }

    #[tokio::test]
    async fn rejects_oversized_length_before_allocating() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        let error = read_frame(&mut server, MAX_CONTROL_FRAME_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(error, FrameError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn rejects_zero_length_frame() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&0u32.to_be_bytes()).await.unwrap();
        let error = read_frame(&mut server, MAX_CONTROL_FRAME_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(error, FrameError::Empty));
    }

    #[tokio::test]
    async fn rejects_truncated_payload() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&10u32.to_be_bytes()).await.unwrap();
        client.write_all(b"{\"type\"").await.unwrap();
        drop(client);
        let error = read_frame(&mut server, MAX_CONTROL_FRAME_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(error, FrameError::Io(_)));
    }

    #[tokio::test]
    async fn rejects_non_protocol_json() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let body = br#"{"type":"NOT_A_MESSAGE"}"#;
        client
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .unwrap();
        client.write_all(body).await.unwrap();
        let error = read_frame(&mut server, MAX_CONTROL_FRAME_BYTES)
            .await
            .unwrap_err();
        assert!(matches!(error, FrameError::Json(_)));
    }

    #[tokio::test]
    async fn refuses_to_write_oversized_message() {
        let (mut client, _server) = tokio::io::duplex(64);
        let message = Message::StreamError(crate::protocol::StreamError {
            request_id: None,
            code: "x".into(),
            message: "y".repeat(200),
        });
        let error = write_frame(&mut client, &message, 64).await.unwrap_err();
        assert!(matches!(error, FrameError::TooLarge { .. }));
    }
}

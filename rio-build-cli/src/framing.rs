//! Length-delimited proto frames over an `AsyncRead`/`AsyncWrite` fd
//! pair (ADR-024 "IPC").
//!
//! Wire shape: a 4-byte big-endian length prefix, then exactly that
//! many bytes of one encoded proto message. Stream mode, not
//! SEQPACKET: result frames carry drv-byte bursts ≥1MB, exceeding
//! datagram comfort and needing a length prefix anyway.
//!
//! Deliberately dependency-light — prost + tokio io only, no tonic:
//! this channel is a pipe between two local processes (coordinator ↔
//! eval parent), not an RPC surface. The eval parent (P3b) reuses the
//! same `rio.evaljob` messages and this exact framing.

use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Hard cap on one frame. A worker's largest legitimate frame is a
/// result batch of skeleton nodes + drv bodies; drv bodies are a few
/// KB each (3.4KB mean, fat p99 tail — ADR-024) and batches are
/// flushed long before this. Anything larger is a corrupt length
/// prefix, and reading it would allocate unboundedly.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Write one length-prefixed frame. Flushes — frames are control-flow
/// edges (work items, ack feedback), not bulk throughput; a buffered
/// unflushed WorkItem would deadlock an idle worker.
pub async fn write_frame<W, M>(w: &mut W, msg: &M) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    M: Message,
{
    let body = msg.encode_to_vec();
    let len = u32::try_from(body.len())
        .ok()
        .filter(|l| *l <= MAX_FRAME_LEN)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("frame too large: {} > {MAX_FRAME_LEN}", body.len()),
            )
        })?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

/// Read one frame. `Ok(None)` on clean EOF at a frame boundary (peer
/// closed); `InvalidData` on an oversized length prefix, a truncated
/// body, or a decode failure.
// r[impl bc.ipc.frame-cap]
pub async fn read_frame<R, M>(r: &mut R) -> std::io::Result<Option<M>>
where
    R: AsyncRead + Unpin,
    M: Message + Default,
{
    let mut len_buf = [0u8; 4];
    // Clean EOF is ZERO bytes at a frame boundary. `read_exact` folds
    // "0 bytes then EOF" and "1–3 bytes then EOF" into one
    // UnexpectedEof, so the first byte is read separately — a torn
    // length prefix is truncation mid-frame, not a close.
    if r.read(&mut len_buf[..1]).await? == 0 {
        return Ok(None);
    }
    r.read_exact(&mut len_buf[1..]).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stream truncated mid-length-prefix",
            )
        } else {
            e
        }
    })?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds cap {MAX_FRAME_LEN}"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stream truncated mid-frame",
            )
        } else {
            e
        }
    })?;
    M::decode(body.as_slice())
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::evaljob::{CoordinatorFrame, WorkItem, coordinator_frame};

    fn work(attr: &str) -> CoordinatorFrame {
        CoordinatorFrame {
            msg: Some(coordinator_frame::Msg::Work(WorkItem { attr: attr.into() })),
        }
    }

    // r[verify bc.ipc.frame-cap]
    #[tokio::test]
    async fn roundtrip_and_clean_eof() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &work("pkgs.hello")).await.unwrap();
        write_frame(&mut a, &work("pkgs.world")).await.unwrap();
        drop(a);
        let f1: CoordinatorFrame = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(f1, work("pkgs.hello"));
        let f2: CoordinatorFrame = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(f2, work("pkgs.world"));
        // EOF at a frame boundary is a clean close, not an error.
        assert!(
            read_frame::<_, CoordinatorFrame>(&mut b)
                .await
                .unwrap()
                .is_none()
        );
    }

    // r[verify bc.ipc.frame-cap]
    #[tokio::test]
    async fn oversized_length_prefix_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        tokio::io::AsyncWriteExt::write_all(&mut a, &u32::MAX.to_be_bytes())
            .await
            .unwrap();
        drop(a);
        let err = read_frame::<_, CoordinatorFrame>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    // r[verify bc.ipc.frame-cap]
    #[tokio::test]
    async fn truncated_length_prefix_is_invalid_data_not_eof() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // Two of the four length bytes, then close.
        tokio::io::AsyncWriteExt::write_all(&mut a, &[0u8, 0u8])
            .await
            .unwrap();
        drop(a);
        let err = read_frame::<_, CoordinatorFrame>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    // r[verify bc.ipc.frame-cap]
    #[tokio::test]
    async fn truncated_body_is_invalid_data_not_eof() {
        let (mut a, mut b) = tokio::io::duplex(64);
        // Claim 100 bytes, deliver 3, close.
        tokio::io::AsyncWriteExt::write_all(&mut a, &100u32.to_be_bytes())
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut a, b"abc")
            .await
            .unwrap();
        drop(a);
        let err = read_frame::<_, CoordinatorFrame>(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}

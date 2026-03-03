//! Streaming framed reader for Nix framed byte streams.

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

/// Maximum total size for framed stream reassembly (1 GiB).
pub const MAX_FRAMED_TOTAL: u64 = 1024 * 1024 * 1024;

/// Maximum single frame size (64 MiB).
pub const MAX_FRAME_SIZE: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Streaming framed reader
// ---------------------------------------------------------------------------

/// Internal state for [`FramedStreamReader`].
#[derive(Debug)]
enum FramedState {
    /// Reading the 8-byte frame length header.
    ReadingHeader,
    /// Reading frame data bytes.
    ReadingData,
    /// Terminal u64(0) sentinel reached — EOF.
    Done,
}

/// An `AsyncRead` adapter that reads a Nix framed byte stream, presenting
/// a contiguous byte stream to the caller.
///
/// The Nix framed protocol is: sequence of `u64(chunk_len) + chunk_data`
/// (no padding), terminated by `u64(0)`.
///
/// # Cancellation
///
/// Once dropped without consuming to EOF, the underlying reader is left at an
/// indeterminate position within the framed stream. The connection must be
/// abandoned — there is no way to resynchronize.
pub struct FramedStreamReader<R> {
    inner: R,
    state: FramedState,
    /// Buffer for reading the 8-byte frame length. Only meaningful in `ReadingHeader`.
    header_buf: [u8; 8],
    /// Bytes read into `header_buf` so far. Only meaningful in `ReadingHeader`.
    header_pos: usize,
    /// Bytes remaining in the current frame. Only meaningful in `ReadingData`.
    frame_remaining: u64,
    /// Total bytes delivered to the caller so far.
    total_read: u64,
    /// Maximum total bytes allowed (clamped to `MAX_FRAMED_TOTAL`).
    max_total: u64,
}

impl<R> FramedStreamReader<R> {
    /// Create a new `FramedStreamReader`.
    ///
    /// `max_total` is clamped to [`MAX_FRAMED_TOTAL`] for defense-in-depth.
    pub fn new(inner: R, max_total: u64) -> Self {
        Self {
            inner,
            state: FramedState::ReadingHeader,
            header_buf: [0u8; 8],
            header_pos: 0,
            frame_remaining: 0,
            total_read: 0,
            max_total: max_total.min(MAX_FRAMED_TOTAL),
        }
    }

    /// Total bytes delivered to the caller so far.
    #[cfg(test)]
    pub fn total_bytes_read(&self) -> u64 {
        self.total_read
    }

    /// Whether the terminal frame sentinel has been reached.
    #[cfg(test)]
    pub fn is_done(&self) -> bool {
        matches!(self.state, FramedState::Done)
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for FramedStreamReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        loop {
            match this.state {
                FramedState::Done => return Poll::Ready(Ok(())),

                FramedState::ReadingHeader => {
                    // Read up to 8 bytes of the frame length header.
                    // Handles partial reads across polls.
                    while this.header_pos < 8 {
                        let mut hdr_buf = ReadBuf::new(&mut this.header_buf[this.header_pos..]);
                        match Pin::new(&mut this.inner).poll_read(cx, &mut hdr_buf) {
                            Poll::Ready(Ok(())) => {
                                let n = hdr_buf.filled().len();
                                if n == 0 {
                                    return Poll::Ready(Err(std::io::Error::new(
                                        std::io::ErrorKind::UnexpectedEof,
                                        if this.header_pos > 0 {
                                            "connection closed mid-frame header"
                                        } else {
                                            "connection closed before frame sentinel"
                                        },
                                    )));
                                }
                                this.header_pos += n;
                            }
                            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                            Poll::Pending => return Poll::Pending,
                        }
                    }

                    // Parse frame length from completed header.
                    let frame_len = u64::from_le_bytes(this.header_buf);
                    this.header_pos = 0;

                    if frame_len == 0 {
                        this.state = FramedState::Done;
                        return Poll::Ready(Ok(()));
                    }

                    if frame_len > MAX_FRAME_SIZE {
                        return Poll::Ready(Err(std::io::Error::other(format!(
                            "framed stream frame size {frame_len} exceeds maximum \
                             {MAX_FRAME_SIZE}"
                        ))));
                    }

                    let new_total = this.total_read + frame_len;
                    if new_total > this.max_total {
                        return Poll::Ready(Err(std::io::Error::other(format!(
                            "framed stream total size {new_total} exceeds maximum {}",
                            this.max_total
                        ))));
                    }

                    this.frame_remaining = frame_len;
                    this.state = FramedState::ReadingData;
                    // Fall through to ReadingData.
                }

                FramedState::ReadingData => {
                    if this.frame_remaining == 0 {
                        this.state = FramedState::ReadingHeader;
                        continue;
                    }

                    if buf.remaining() == 0 {
                        return Poll::Ready(Ok(()));
                    }

                    // Read from inner, capped to frame_remaining and buf capacity.
                    let limit = (this.frame_remaining as usize).min(buf.remaining());
                    let n = {
                        let unfilled = buf.initialize_unfilled_to(limit);
                        let mut sub_buf = ReadBuf::new(&mut unfilled[..limit]);
                        match Pin::new(&mut this.inner).poll_read(cx, &mut sub_buf) {
                            Poll::Ready(Ok(())) => sub_buf.filled().len(),
                            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                            Poll::Pending => return Poll::Pending,
                        }
                    };

                    if n == 0 {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "connection closed mid-frame data",
                        )));
                    }

                    buf.advance(n);
                    this.frame_remaining -= n as u64;
                    this.total_read += n as u64;
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{write_framed_stream, write_u64};
    use super::*;
    use std::io::Cursor;

    // FramedStreamReader tests

    /// Helper: write framed stream, then read back via FramedStreamReader.
    async fn framed_reader_roundtrip(data: &[u8], chunk_size: usize) -> anyhow::Result<Vec<u8>> {
        let mut wire_buf = Vec::new();
        write_framed_stream(&mut wire_buf, data, chunk_size).await?;
        let reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        let mut result = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut tokio::io::BufReader::new(reader), &mut result)
            .await?;
        Ok(result)
    }

    #[tokio::test]
    async fn test_framed_reader_single_frame() -> anyhow::Result<()> {
        let data = b"hello framed world";
        let result = framed_reader_roundtrip(data, 1024).await?;
        assert_eq!(result, data);
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_reader_multi_frame() -> anyhow::Result<()> {
        let data = b"abcdefghijklmnopqrstuvwxyz";
        let result = framed_reader_roundtrip(data, 10).await?;
        assert_eq!(result, data);
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_reader_empty_stream() -> anyhow::Result<()> {
        // Just the u64(0) sentinel
        let result = framed_reader_roundtrip(b"", 64).await?;
        assert!(result.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_reader_chunk_size_1() -> anyhow::Result<()> {
        let data = b"test";
        let result = framed_reader_roundtrip(data, 1).await?;
        assert_eq!(result, data);
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_reader_frame_too_large() -> anyhow::Result<()> {
        // Construct a wire buffer with a frame larger than MAX_FRAME_SIZE
        let mut wire_buf = Vec::new();
        write_u64(&mut wire_buf, MAX_FRAME_SIZE + 1).await?;
        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        let mut result = Vec::new();
        let err = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut result)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("frame size"),
            "expected 'frame size' in error, got: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_reader_total_too_large() -> anyhow::Result<()> {
        // Create a stream that would exceed max_total (set low for testing)
        let mut wire_buf = Vec::new();
        // Frame 1: 10 bytes
        write_u64(&mut wire_buf, 10).await?;
        wire_buf.extend_from_slice(&[0u8; 10]);
        // Frame 2: 10 bytes — would make total 20, exceeding max_total=15
        write_u64(&mut wire_buf, 10).await?;
        wire_buf.extend_from_slice(&[0u8; 10]);
        write_u64(&mut wire_buf, 0).await?; // sentinel

        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), 15);
        let mut result = Vec::new();
        let err = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut result)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("total size"),
            "expected 'total size' in error, got: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_reader_small_buf_reads() -> anyhow::Result<()> {
        // Test that the reader works correctly with small read buffer sizes
        // by reading one byte at a time
        let data = b"hello";
        let mut wire_buf = Vec::new();
        write_framed_stream(&mut wire_buf, data, 3).await?;

        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        let mut result = Vec::new();
        let mut one_byte = [0u8; 1];
        loop {
            match tokio::io::AsyncReadExt::read(&mut reader, &mut one_byte).await {
                Ok(0) => break,
                Ok(n) => result.extend_from_slice(&one_byte[..n]),
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(result, data);
        assert!(reader.is_done());
        assert_eq!(reader.total_bytes_read(), data.len() as u64);
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_reader_eof_mid_header() {
        // Only 3 of 8 header bytes available
        let wire_buf = vec![1, 2, 3];
        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        let mut result = Vec::new();
        let err = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut result)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(
            err.to_string().contains("mid-frame header"),
            "expected 'mid-frame header', got: {err}"
        );
    }

    #[tokio::test]
    async fn test_framed_reader_eof_mid_data() -> anyhow::Result<()> {
        // Header says 10 bytes, but only 5 available
        let mut wire_buf = Vec::new();
        write_u64(&mut wire_buf, 10).await?;
        wire_buf.extend_from_slice(&[0u8; 5]); // only 5 of 10
        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        let mut result = Vec::new();
        let err = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut result)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(
            err.to_string().contains("mid-frame data"),
            "expected 'mid-frame data', got: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_reader_eof_before_sentinel() {
        // Empty input — no sentinel at all
        let wire_buf: Vec<u8> = Vec::new();
        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        let mut result = Vec::new();
        let err = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut result)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn test_framed_reader_accessors() -> anyhow::Result<()> {
        let data = b"test data here";
        let mut wire_buf = Vec::new();
        write_framed_stream(&mut wire_buf, data, 5).await?;

        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        assert!(!reader.is_done());
        assert_eq!(reader.total_bytes_read(), 0);

        let mut result = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut result).await?;

        assert!(reader.is_done());
        assert_eq!(reader.total_bytes_read(), data.len() as u64);
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_reader_max_total_clamped() {
        // Verify that max_total is clamped to MAX_FRAMED_TOTAL
        let reader = FramedStreamReader::new(Cursor::new(Vec::<u8>::new()), u64::MAX);
        assert_eq!(reader.max_total, MAX_FRAMED_TOTAL);
    }
}

//! Nix worker protocol wire format primitives.
//!
//! All integers are 64-bit unsigned, little-endian — including handshake magic bytes.
//! Strings are length-prefixed and padded to 8-byte boundaries.
//! Collections are count-prefixed.

use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

/// Padding alignment for the Nix wire format.
const PADDING: usize = 8;

/// Maximum allowed string length (64 MiB) to prevent OOM on malicious input.
pub const MAX_STRING_LEN: u64 = 64 * 1024 * 1024;

/// Maximum allowed collection count (1M items) to prevent OOM on malicious input.
pub const MAX_COLLECTION_COUNT: u64 = 1_048_576;

#[derive(Debug, Error)]
pub enum WireError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("string length {0} exceeds maximum {MAX_STRING_LEN}")]
    StringTooLong(u64),

    #[error("collection count {0} exceeds maximum {MAX_COLLECTION_COUNT}")]
    CollectionTooLarge(u64),

    #[error("invalid UTF-8 in string")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),

    #[error("framed stream frame size {0} exceeds maximum {MAX_FRAME_SIZE}")]
    FrameTooLarge(u64),

    #[error("framed stream total size {0} exceeds maximum {MAX_FRAMED_TOTAL}")]
    FramedStreamTooLarge(u64),
}

pub type Result<T> = std::result::Result<T, WireError>;

/// Compute how many zero-padding bytes are needed after `len` data bytes.
#[inline]
pub fn padding_len(len: usize) -> usize {
    let rem = len % PADDING;
    if rem == 0 { 0 } else { PADDING - rem }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Read a single byte.
///
/// **Note:** Nix wire protocol integers are ALWAYS u64, even for logically
/// u8 values. This function is for non-protocol byte reading (e.g., NAR
/// format tag bytes). Do not use for protocol integer fields.
pub async fn read_u8<R: AsyncRead + Unpin>(r: &mut R) -> Result<u8> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf).await?;
    Ok(buf[0])
}

/// Read a little-endian u64.
pub async fn read_u64<R: AsyncRead + Unpin>(r: &mut R) -> Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).await?;
    Ok(u64::from_le_bytes(buf))
}

/// Read a little-endian i64.
pub async fn read_i64<R: AsyncRead + Unpin>(r: &mut R) -> Result<i64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).await?;
    Ok(i64::from_le_bytes(buf))
}

/// Read a u64-encoded boolean (0 = false, nonzero = true).
pub async fn read_bool<R: AsyncRead + Unpin>(r: &mut R) -> Result<bool> {
    Ok(read_u64(r).await? != 0)
}

/// Read a length-prefixed, padded byte string.
pub async fn read_bytes<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let len = read_u64(r).await?;
    if len > MAX_STRING_LEN {
        return Err(WireError::StringTooLong(len));
    }
    let len = len as usize;

    if len == 0 {
        return Ok(Vec::new());
    }

    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;

    // Skip padding bytes
    let pad = padding_len(len);
    if pad > 0 {
        let mut pad_buf = [0u8; 8]; // max padding is 7
        r.read_exact(&mut pad_buf[..pad]).await?;
    }

    Ok(buf)
}

/// Read a length-prefixed, padded UTF-8 string.
pub async fn read_string<R: AsyncRead + Unpin>(r: &mut R) -> Result<String> {
    let bytes = read_bytes(r).await?;
    Ok(String::from_utf8(bytes)?)
}

/// Read a collection of UTF-8 strings (`u64(count)` followed by `count` strings).
pub async fn read_strings<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<String>> {
    let count = read_u64(r).await?;
    if count > MAX_COLLECTION_COUNT {
        return Err(WireError::CollectionTooLarge(count));
    }
    let count = count as usize;
    let mut result = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        result.push(read_string(r).await?);
    }
    Ok(result)
}

/// Read a collection of key-value string pairs.
pub async fn read_string_pairs<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<(String, String)>> {
    let count = read_u64(r).await?;
    if count > MAX_COLLECTION_COUNT {
        return Err(WireError::CollectionTooLarge(count));
    }
    let count = count as usize;
    let mut result = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let key = read_string(r).await?;
        let value = read_string(r).await?;
        result.push((key, value));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Write a single byte.
///
/// **Note:** Nix wire protocol integers are ALWAYS u64. This function is
/// for non-protocol byte writing only (e.g., NAR format).
pub async fn write_u8<W: AsyncWrite + Unpin>(w: &mut W, val: u8) -> Result<()> {
    w.write_all(&[val]).await?;
    Ok(())
}

/// Write a little-endian u64.
pub async fn write_u64<W: AsyncWrite + Unpin>(w: &mut W, val: u64) -> Result<()> {
    w.write_all(&val.to_le_bytes()).await?;
    Ok(())
}

/// Write a little-endian i64.
pub async fn write_i64<W: AsyncWrite + Unpin>(w: &mut W, val: i64) -> Result<()> {
    w.write_all(&val.to_le_bytes()).await?;
    Ok(())
}

/// Write a u64-encoded boolean.
pub async fn write_bool<W: AsyncWrite + Unpin>(w: &mut W, val: bool) -> Result<()> {
    write_u64(w, u64::from(val)).await
}

/// Write a length-prefixed, padded byte string.
pub async fn write_bytes<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    let len = data.len() as u64;
    if len > MAX_STRING_LEN {
        return Err(WireError::StringTooLong(len));
    }
    write_u64(w, len).await?;

    if !data.is_empty() {
        w.write_all(data).await?;

        let pad = padding_len(data.len());
        if pad > 0 {
            w.write_all(&[0u8; 8][..pad]).await?;
        }
    }

    Ok(())
}

/// Write a length-prefixed, padded UTF-8 string.
pub async fn write_string<W: AsyncWrite + Unpin>(w: &mut W, s: &str) -> Result<()> {
    write_bytes(w, s.as_bytes()).await
}

/// Empty string slice, for callers of [`write_strings`] that need to send an
/// empty collection without allocating (type inference aid).
pub const NO_STRINGS: &[&str] = &[];

/// Write a collection of UTF-8 strings.
pub async fn write_strings<W: AsyncWrite + Unpin, S: AsRef<str>>(
    w: &mut W,
    items: &[S],
) -> Result<()> {
    let count = items.len() as u64;
    if count > MAX_COLLECTION_COUNT {
        return Err(WireError::CollectionTooLarge(count));
    }
    write_u64(w, count).await?;
    for item in items {
        write_string(w, item.as_ref()).await?;
    }
    Ok(())
}

/// Maximum total size for framed stream reassembly (1 GiB).
pub const MAX_FRAMED_TOTAL: u64 = 1024 * 1024 * 1024;

/// Maximum single frame size (64 MiB).
pub const MAX_FRAME_SIZE: u64 = 64 * 1024 * 1024;

/// Write a collection of key-value string pairs.
pub async fn write_string_pairs<W: AsyncWrite + Unpin, K: AsRef<str>, V: AsRef<str>>(
    w: &mut W,
    pairs: &[(K, V)],
) -> Result<()> {
    let count = pairs.len() as u64;
    if count > MAX_COLLECTION_COUNT {
        return Err(WireError::CollectionTooLarge(count));
    }
    write_u64(w, count).await?;
    for (key, value) in pairs {
        write_string(w, key.as_ref()).await?;
        write_string(w, value.as_ref()).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Framed byte streams (used by wopAddMultipleToStore)
// ---------------------------------------------------------------------------

/// Read a framed byte stream: sequence of `u64(chunk_len) + chunk_data`
/// terminated by `u64(0)`.
///
/// **Important:** Unlike string encoding, chunk data is NOT padded to 8 bytes.
///
/// Enforces a maximum total size to prevent OOM on malicious input.
pub async fn read_framed_stream<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut result = Vec::new();

    loop {
        let frame_len = read_u64(r).await?;
        if frame_len == 0 {
            return Ok(result);
        }
        if frame_len > MAX_FRAME_SIZE {
            return Err(WireError::FrameTooLarge(frame_len));
        }
        let total = result.len() as u64 + frame_len;
        if total > MAX_FRAMED_TOTAL {
            return Err(WireError::FramedStreamTooLarge(total));
        }

        let frame_len = frame_len as usize;
        let start = result.len();
        result.resize(start + frame_len, 0);
        r.read_exact(&mut result[start..]).await?;
    }
}

/// Write data as a framed byte stream with a given chunk size.
///
/// Each frame: `u64(chunk_len) + chunk_data` (no padding).
/// Terminated by `u64(0)`.
pub async fn write_framed_stream<W: AsyncWrite + Unpin>(
    w: &mut W,
    data: &[u8],
    chunk_size: usize,
) -> Result<()> {
    for chunk in data.chunks(chunk_size) {
        write_u64(w, chunk.len() as u64).await?;
        w.write_all(chunk).await?;
    }
    // Sentinel
    write_u64(w, 0).await?;
    Ok(())
}

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
    pub fn total_bytes_read(&self) -> u64 {
        self.total_read
    }

    /// Whether the terminal frame sentinel has been reached.
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
    use super::*;
    use std::io::Cursor;

    /// Helper: write to buffer then read back.
    async fn roundtrip_bytes(data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_bytes(&mut buf, data).await.unwrap();
        let mut reader = Cursor::new(buf);
        read_bytes(&mut reader).await.unwrap()
    }

    async fn roundtrip_string(s: &str) -> String {
        let mut buf = Vec::new();
        write_string(&mut buf, s).await.unwrap();
        let mut reader = Cursor::new(buf);
        read_string(&mut reader).await.unwrap()
    }

    #[tokio::test]
    async fn test_u64_roundtrip() {
        for val in [0u64, 1, 42, u64::MAX, 0x6e697863] {
            let mut buf = Vec::new();
            write_u64(&mut buf, val).await.unwrap();
            assert_eq!(buf.len(), 8);
            let mut reader = Cursor::new(buf);
            assert_eq!(read_u64(&mut reader).await.unwrap(), val);
        }
    }

    #[tokio::test]
    async fn test_bool_roundtrip() {
        for val in [true, false] {
            let mut buf = Vec::new();
            write_bool(&mut buf, val).await.unwrap();
            let mut reader = Cursor::new(buf);
            assert_eq!(read_bool(&mut reader).await.unwrap(), val);
        }
    }

    #[tokio::test]
    async fn test_empty_string() {
        let result = roundtrip_bytes(b"").await;
        assert!(result.is_empty());

        // Verify wire format: just u64(0), nothing else
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"").await.unwrap();
        assert_eq!(buf.len(), 8); // just the length field
        assert_eq!(buf, vec![0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn test_string_padding() {
        // String of length 1: needs 7 bytes padding
        let result = roundtrip_bytes(b"x").await;
        assert_eq!(result, b"x");

        // Verify total wire size: 8 (len) + 1 (data) + 7 (pad) = 16
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"x").await.unwrap();
        assert_eq!(buf.len(), 16);

        // String of length 8: no padding needed
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"12345678").await.unwrap();
        assert_eq!(buf.len(), 16); // 8 (len) + 8 (data) + 0 (pad)

        // String of length 9: needs 7 bytes padding
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"123456789").await.unwrap();
        assert_eq!(buf.len(), 24); // 8 (len) + 9 (data) + 7 (pad)
    }

    #[tokio::test]
    async fn test_string_boundary_lengths() {
        for len in [0, 1, 7, 8, 9, 15, 16, 17, 100] {
            let data = vec![b'A'; len];
            let result = roundtrip_bytes(&data).await;
            assert_eq!(result, data, "roundtrip failed for len={len}");
        }
    }

    #[tokio::test]
    async fn test_utf8_string_roundtrip() {
        let cases = ["", "hello", "hello world", "/nix/store/abc-hello-2.12.1"];
        for s in cases {
            let result = roundtrip_string(s).await;
            assert_eq!(result, s);
        }
    }

    #[tokio::test]
    async fn test_strings_collection() {
        let items = vec![
            "hello".to_string(),
            "world".to_string(),
            "/nix/store/abc".to_string(),
        ];
        let mut buf = Vec::new();
        write_strings(&mut buf, &items).await.unwrap();
        let mut reader = Cursor::new(buf);
        let result = read_strings(&mut reader).await.unwrap();
        assert_eq!(result, items);
    }

    #[tokio::test]
    async fn test_empty_collection() {
        let items: Vec<String> = vec![];
        let mut buf = Vec::new();
        write_strings(&mut buf, &items).await.unwrap();
        let mut reader = Cursor::new(buf);
        let result = read_strings(&mut reader).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_string_too_long() {
        // Craft a buffer with a huge length field
        let mut buf = Vec::new();
        write_u64(&mut buf, MAX_STRING_LEN + 1).await.unwrap();
        let mut reader = Cursor::new(buf);
        let result = read_bytes(&mut reader).await;
        assert!(matches!(result, Err(WireError::StringTooLong(_))));
    }

    #[tokio::test]
    async fn test_padding_len() {
        assert_eq!(padding_len(0), 0);
        assert_eq!(padding_len(1), 7);
        assert_eq!(padding_len(7), 1);
        assert_eq!(padding_len(8), 0);
        assert_eq!(padding_len(9), 7);
        assert_eq!(padding_len(16), 0);
    }

    #[tokio::test]
    async fn test_collection_too_large() {
        let mut buf = Vec::new();
        write_u64(&mut buf, MAX_COLLECTION_COUNT + 1).await.unwrap();
        let mut reader = Cursor::new(buf);
        let result = read_strings(&mut reader).await;
        assert!(matches!(result, Err(WireError::CollectionTooLarge(_))));
    }

    #[tokio::test]
    async fn test_read_u64_truncated() {
        // Only 3 bytes available, need 8
        let mut reader = Cursor::new(vec![0, 1, 2]);
        let result = read_u64(&mut reader).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_u64_empty() {
        let mut reader = Cursor::new(vec![]);
        let result = read_u64(&mut reader).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_string_truncated_body() {
        // Length says 10 bytes, but only 5 available
        let mut buf = Vec::new();
        write_u64(&mut buf, 10).await.unwrap();
        buf.extend_from_slice(b"hello"); // only 5 of 10 bytes
        let mut reader = Cursor::new(buf);
        let result = read_string(&mut reader).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_string_missing_padding() {
        // 3-byte string needs 5 bytes padding, but we only provide the string
        let mut buf = Vec::new();
        write_u64(&mut buf, 3).await.unwrap();
        buf.extend_from_slice(b"abc"); // no padding
        let mut reader = Cursor::new(buf);
        let result = read_bytes(&mut reader).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_string_invalid_utf8() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 4).await.unwrap();
        buf.extend_from_slice(&[0xFF, 0xFE, 0xFD, 0xFC]); // invalid UTF-8
        buf.extend_from_slice(&[0, 0, 0, 0]); // padding
        let mut reader = Cursor::new(buf);
        let result = read_string(&mut reader).await;
        assert!(matches!(result, Err(WireError::InvalidUtf8(_))));
    }

    #[tokio::test]
    async fn test_string_pairs_too_large() {
        let mut buf = Vec::new();
        write_u64(&mut buf, MAX_COLLECTION_COUNT + 1).await.unwrap();
        let mut reader = Cursor::new(buf);
        let result = read_string_pairs(&mut reader).await;
        assert!(matches!(result, Err(WireError::CollectionTooLarge(_))));
    }

    #[tokio::test]
    async fn test_read_strings_truncated_elements() {
        // Says 3 elements, but only provides data for 1
        let mut buf = Vec::new();
        write_u64(&mut buf, 3).await.unwrap();
        write_string(&mut buf, "first").await.unwrap();
        // missing 2nd and 3rd elements
        let mut reader = Cursor::new(buf);
        let result = read_strings(&mut reader).await;
        assert!(result.is_err());
    }

    // Framed stream tests

    #[tokio::test]
    async fn test_framed_stream_empty() {
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, b"", 64).await.unwrap();

        // Should just be u64(0) sentinel
        assert_eq!(buf.len(), 8);

        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_framed_stream_single_chunk() {
        let data = b"hello framed world";
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, data, 1024).await.unwrap();

        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await.unwrap();
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_framed_stream_multiple_chunks() {
        let data = b"abcdefghijklmnopqrstuvwxyz";
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, data, 10).await.unwrap();

        // Should have 3 frames: 10 + 10 + 6 + sentinel
        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await.unwrap();
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_framed_stream_no_padding() {
        // Verify that framed stream data is NOT padded (unlike string encoding)
        let data = b"abc"; // 3 bytes, would need 5 bytes padding in string format
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, data, 1024).await.unwrap();

        // Expected: u64(3) + "abc" + u64(0) = 8 + 3 + 8 = 19 bytes
        // If it were padded like strings, it would be 8 + 3 + 5 + 8 = 24 bytes
        assert_eq!(buf.len(), 19, "framed stream should not pad chunk data");

        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await.unwrap();
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_framed_stream_chunk_size_1() {
        let data = b"test";
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, data, 1).await.unwrap();

        // 4 frames of 1 byte each + sentinel
        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await.unwrap();
        assert_eq!(result, data);
    }

    // FramedStreamReader tests

    /// Helper: write framed stream, then read back via FramedStreamReader.
    async fn framed_reader_roundtrip(data: &[u8], chunk_size: usize) -> Vec<u8> {
        let mut wire_buf = Vec::new();
        write_framed_stream(&mut wire_buf, data, chunk_size)
            .await
            .unwrap();
        let reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        let mut result = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut tokio::io::BufReader::new(reader), &mut result)
            .await
            .unwrap();
        result
    }

    #[tokio::test]
    async fn test_framed_reader_single_frame() {
        let data = b"hello framed world";
        let result = framed_reader_roundtrip(data, 1024).await;
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_framed_reader_multi_frame() {
        let data = b"abcdefghijklmnopqrstuvwxyz";
        let result = framed_reader_roundtrip(data, 10).await;
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_framed_reader_empty_stream() {
        // Just the u64(0) sentinel
        let result = framed_reader_roundtrip(b"", 64).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_framed_reader_chunk_size_1() {
        let data = b"test";
        let result = framed_reader_roundtrip(data, 1).await;
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_framed_reader_frame_too_large() {
        // Construct a wire buffer with a frame larger than MAX_FRAME_SIZE
        let mut wire_buf = Vec::new();
        write_u64(&mut wire_buf, MAX_FRAME_SIZE + 1).await.unwrap();
        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        let mut result = Vec::new();
        let err = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut result)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("frame size"),
            "expected 'frame size' in error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_framed_reader_total_too_large() {
        // Create a stream that would exceed max_total (set low for testing)
        let mut wire_buf = Vec::new();
        // Frame 1: 10 bytes
        write_u64(&mut wire_buf, 10).await.unwrap();
        wire_buf.extend_from_slice(&[0u8; 10]);
        // Frame 2: 10 bytes — would make total 20, exceeding max_total=15
        write_u64(&mut wire_buf, 10).await.unwrap();
        wire_buf.extend_from_slice(&[0u8; 10]);
        write_u64(&mut wire_buf, 0).await.unwrap(); // sentinel

        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), 15);
        let mut result = Vec::new();
        let err = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut result)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("total size"),
            "expected 'total size' in error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_framed_reader_small_buf_reads() {
        // Test that the reader works correctly with small read buffer sizes
        // by reading one byte at a time
        let data = b"hello";
        let mut wire_buf = Vec::new();
        write_framed_stream(&mut wire_buf, data, 3).await.unwrap();

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
    async fn test_framed_reader_eof_mid_data() {
        // Header says 10 bytes, but only 5 available
        let mut wire_buf = Vec::new();
        write_u64(&mut wire_buf, 10).await.unwrap();
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
    async fn test_framed_reader_accessors() {
        let data = b"test data here";
        let mut wire_buf = Vec::new();
        write_framed_stream(&mut wire_buf, data, 5).await.unwrap();

        let mut reader = FramedStreamReader::new(Cursor::new(wire_buf), MAX_FRAMED_TOTAL);
        assert!(!reader.is_done());
        assert_eq!(reader.total_bytes_read(), 0);

        let mut result = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut result)
            .await
            .unwrap();

        assert!(reader.is_done());
        assert_eq!(reader.total_bytes_read(), data.len() as u64);
    }

    #[tokio::test]
    async fn test_framed_reader_max_total_clamped() {
        // Verify that max_total is clamped to MAX_FRAMED_TOTAL
        let reader = FramedStreamReader::new(Cursor::new(Vec::<u8>::new()), u64::MAX);
        assert_eq!(reader.max_total, MAX_FRAMED_TOTAL);
    }

    // Property-based tests
    mod proptests {
        use super::*;
        use proptest::prelude::*;
        use std::io::Cursor;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]
            #[test]
            fn roundtrip_u64(val: u64) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_u64(&mut buf, val).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let result = read_u64(&mut reader).await.unwrap();
                    prop_assert_eq!(result, val);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_bytes(data: Vec<u8>) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_bytes(&mut buf, &data).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let result = read_bytes(&mut reader).await.unwrap();
                    prop_assert_eq!(result, data);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_bool(val: bool) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_bool(&mut buf, val).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let result = read_bool(&mut reader).await.unwrap();
                    prop_assert_eq!(result, val);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_string(s in "[a-zA-Z0-9 /._-]{0,200}") {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_string(&mut buf, &s).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let result = read_string(&mut reader).await.unwrap();
                    prop_assert_eq!(result, s);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_string_utf8(s in "\\PC{0,100}") {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_string(&mut buf, &s).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let result = read_string(&mut reader).await.unwrap();
                    prop_assert_eq!(result, s);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_strings(items in proptest::collection::vec("[a-zA-Z0-9/_-]{0,50}", 0..20)) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let items: Vec<String> = items.into_iter().collect();
                    let mut buf = Vec::new();
                    write_strings(&mut buf, &items).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let result = read_strings(&mut reader).await.unwrap();
                    prop_assert_eq!(result, items);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_string_pairs(
                pairs in proptest::collection::vec(
                    ("[a-zA-Z_]{1,20}", "[a-zA-Z0-9 ]{0,50}"),
                    0..10
                )
            ) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let pairs: Vec<(String, String)> = pairs.into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                    let mut buf = Vec::new();
                    write_string_pairs(&mut buf, &pairs).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let result = read_string_pairs(&mut reader).await.unwrap();
                    prop_assert_eq!(result, pairs);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_framed_stream(
                data in proptest::collection::vec(any::<u8>(), 0..500),
                chunk_size in 1usize..64,
            ) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_framed_stream(&mut buf, &data, chunk_size).await.unwrap();
                    let mut reader = Cursor::new(buf);
                    let result = read_framed_stream(&mut reader).await.unwrap();
                    prop_assert_eq!(result, data);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_framed_stream_reader(
                data in proptest::collection::vec(any::<u8>(), 0..500),
                chunk_size in 1usize..64,
            ) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_framed_stream(&mut buf, &data, chunk_size).await.unwrap();
                    let reader = FramedStreamReader::new(
                        Cursor::new(buf),
                        MAX_FRAMED_TOTAL,
                    );
                    let mut result = Vec::new();
                    tokio::io::AsyncReadExt::read_to_end(
                        &mut tokio::io::BufReader::new(reader),
                        &mut result,
                    )
                    .await
                    .unwrap();
                    prop_assert_eq!(result, data);
                    Ok(())
                })?;
            }
        }
    }
}

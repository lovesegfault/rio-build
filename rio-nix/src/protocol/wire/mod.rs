//! Nix worker protocol wire format primitives.
//!
//! All integers are 64-bit unsigned, little-endian — including handshake magic bytes.
//! Strings are length-prefixed and padded to 8-byte boundaries.
//! Collections are count-prefixed.
// r[impl gw.wire.all-ints-u64]
// r[impl gw.wire.string-encoding]

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

mod framed;
pub use framed::{FramedStreamReader, MAX_FRAME_SIZE, MAX_FRAMED_TOTAL};

/// Padding alignment for the Nix wire format.
const PADDING: usize = 8;

/// Maximum allowed string length (64 MiB) to prevent OOM on malicious input.
pub const MAX_STRING_LEN: u64 = 64 * 1024 * 1024;

// r[impl gw.wire.collection-max]
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

/// Read a little-endian u64.
pub async fn read_u64<R: AsyncRead + Unpin>(r: &mut R) -> Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).await?;
    Ok(u64::from_le_bytes(buf))
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

/// Write a little-endian u64.
pub async fn write_u64<W: AsyncWrite + Unpin>(w: &mut W, val: u64) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Helper: write to buffer then read back.
    async fn roundtrip_bytes(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut buf = Vec::new();
        write_bytes(&mut buf, data).await?;
        let mut reader = Cursor::new(buf);
        Ok(read_bytes(&mut reader).await?)
    }

    async fn roundtrip_string(s: &str) -> anyhow::Result<String> {
        let mut buf = Vec::new();
        write_string(&mut buf, s).await?;
        let mut reader = Cursor::new(buf);
        Ok(read_string(&mut reader).await?)
    }

    #[tokio::test]
    async fn test_u64_roundtrip() -> anyhow::Result<()> {
        for val in [0u64, 1, 42, u64::MAX, 0x6e697863] {
            let mut buf = Vec::new();
            write_u64(&mut buf, val).await?;
            assert_eq!(buf.len(), 8);
            let mut reader = Cursor::new(buf);
            assert_eq!(read_u64(&mut reader).await?, val);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_bool_roundtrip() -> anyhow::Result<()> {
        for val in [true, false] {
            let mut buf = Vec::new();
            write_bool(&mut buf, val).await?;
            let mut reader = Cursor::new(buf);
            assert_eq!(read_bool(&mut reader).await?, val);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_empty_string() -> anyhow::Result<()> {
        let result = roundtrip_bytes(b"").await?;
        assert!(result.is_empty());

        // Verify wire format: just u64(0), nothing else
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"").await?;
        assert_eq!(buf.len(), 8); // just the length field
        assert_eq!(buf, vec![0, 0, 0, 0, 0, 0, 0, 0]);
        Ok(())
    }

    #[tokio::test]
    async fn test_string_padding() -> anyhow::Result<()> {
        // String of length 1: needs 7 bytes padding
        let result = roundtrip_bytes(b"x").await?;
        assert_eq!(result, b"x");

        // Verify total wire size: 8 (len) + 1 (data) + 7 (pad) = 16
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"x").await?;
        assert_eq!(buf.len(), 16);

        // String of length 8: no padding needed
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"12345678").await?;
        assert_eq!(buf.len(), 16); // 8 (len) + 8 (data) + 0 (pad)

        // String of length 9: needs 7 bytes padding
        let mut buf = Vec::new();
        write_bytes(&mut buf, b"123456789").await?;
        assert_eq!(buf.len(), 24); // 8 (len) + 9 (data) + 7 (pad)
        Ok(())
    }

    #[tokio::test]
    async fn test_string_boundary_lengths() -> anyhow::Result<()> {
        for len in [0, 1, 7, 8, 9, 15, 16, 17, 100] {
            let data = vec![b'A'; len];
            let result = roundtrip_bytes(&data).await?;
            assert_eq!(result, data, "roundtrip failed for len={len}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_utf8_string_roundtrip() -> anyhow::Result<()> {
        let cases = ["", "hello", "hello world", "/nix/store/abc-hello-2.12.1"];
        for s in cases {
            let result = roundtrip_string(s).await?;
            assert_eq!(result, s);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_strings_collection() -> anyhow::Result<()> {
        let items = vec![
            "hello".to_string(),
            "world".to_string(),
            "/nix/store/abc".to_string(),
        ];
        let mut buf = Vec::new();
        write_strings(&mut buf, &items).await?;
        let mut reader = Cursor::new(buf);
        let result = read_strings(&mut reader).await?;
        assert_eq!(result, items);
        Ok(())
    }

    #[tokio::test]
    async fn test_empty_collection() -> anyhow::Result<()> {
        let items: Vec<String> = vec![];
        let mut buf = Vec::new();
        write_strings(&mut buf, &items).await?;
        let mut reader = Cursor::new(buf);
        let result = read_strings(&mut reader).await?;
        assert!(result.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_string_too_long() -> anyhow::Result<()> {
        // Craft a buffer with a huge length field
        let mut buf = Vec::new();
        write_u64(&mut buf, MAX_STRING_LEN + 1).await?;
        let mut reader = Cursor::new(buf);
        let result = read_bytes(&mut reader).await;
        assert!(matches!(result, Err(WireError::StringTooLong(_))));
        Ok(())
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
    async fn test_collection_too_large() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        write_u64(&mut buf, MAX_COLLECTION_COUNT + 1).await?;
        let mut reader = Cursor::new(buf);
        let result = read_strings(&mut reader).await;
        assert!(matches!(result, Err(WireError::CollectionTooLarge(_))));
        Ok(())
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
    async fn test_read_string_truncated_body() -> anyhow::Result<()> {
        // Length says 10 bytes, but only 5 available
        let mut buf = Vec::new();
        write_u64(&mut buf, 10).await?;
        buf.extend_from_slice(b"hello"); // only 5 of 10 bytes
        let mut reader = Cursor::new(buf);
        let result = read_string(&mut reader).await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_read_string_missing_padding() -> anyhow::Result<()> {
        // 3-byte string needs 5 bytes padding, but we only provide the string
        let mut buf = Vec::new();
        write_u64(&mut buf, 3).await?;
        buf.extend_from_slice(b"abc"); // no padding
        let mut reader = Cursor::new(buf);
        let result = read_bytes(&mut reader).await;
        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn test_read_string_invalid_utf8() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        write_u64(&mut buf, 4).await?;
        buf.extend_from_slice(&[0xFF, 0xFE, 0xFD, 0xFC]); // invalid UTF-8
        buf.extend_from_slice(&[0, 0, 0, 0]); // padding
        let mut reader = Cursor::new(buf);
        let result = read_string(&mut reader).await;
        assert!(matches!(result, Err(WireError::InvalidUtf8(_))));
        Ok(())
    }

    #[tokio::test]
    async fn test_string_pairs_too_large() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        write_u64(&mut buf, MAX_COLLECTION_COUNT + 1).await?;
        let mut reader = Cursor::new(buf);
        let result = read_string_pairs(&mut reader).await;
        assert!(matches!(result, Err(WireError::CollectionTooLarge(_))));
        Ok(())
    }

    #[tokio::test]
    async fn test_read_strings_truncated_elements() -> anyhow::Result<()> {
        // Says 3 elements, but only provides data for 1
        let mut buf = Vec::new();
        write_u64(&mut buf, 3).await?;
        write_string(&mut buf, "first").await?;
        // missing 2nd and 3rd elements
        let mut reader = Cursor::new(buf);
        let result = read_strings(&mut reader).await;
        assert!(result.is_err());
        Ok(())
    }

    // Framed stream tests

    #[tokio::test]
    async fn test_framed_stream_empty() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, b"", 64).await?;

        // Should just be u64(0) sentinel
        assert_eq!(buf.len(), 8);

        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await?;
        assert!(result.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_stream_single_chunk() -> anyhow::Result<()> {
        let data = b"hello framed world";
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, data, 1024).await?;

        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await?;
        assert_eq!(result, data);
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_stream_multiple_chunks() -> anyhow::Result<()> {
        let data = b"abcdefghijklmnopqrstuvwxyz";
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, data, 10).await?;

        // Should have 3 frames: 10 + 10 + 6 + sentinel
        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await?;
        assert_eq!(result, data);
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_stream_no_padding() -> anyhow::Result<()> {
        // Verify that framed stream data is NOT padded (unlike string encoding)
        let data = b"abc"; // 3 bytes, would need 5 bytes padding in string format
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, data, 1024).await?;

        // Expected: u64(3) + "abc" + u64(0) = 8 + 3 + 8 = 19 bytes
        // If it were padded like strings, it would be 8 + 3 + 5 + 8 = 24 bytes
        assert_eq!(buf.len(), 19, "framed stream should not pad chunk data");

        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await?;
        assert_eq!(result, data);
        Ok(())
    }

    #[tokio::test]
    async fn test_framed_stream_chunk_size_1() -> anyhow::Result<()> {
        let data = b"test";
        let mut buf = Vec::new();
        write_framed_stream(&mut buf, data, 1).await?;

        // 4 frames of 1 byte each + sentinel
        let mut reader = Cursor::new(buf);
        let result = read_framed_stream(&mut reader).await?;
        assert_eq!(result, data);
        Ok(())
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
                    write_u64(&mut buf, val).await?;
                    let mut reader = Cursor::new(buf);
                    let result = read_u64(&mut reader).await?;
                    prop_assert_eq!(result, val);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_bytes(data: Vec<u8>) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_bytes(&mut buf, &data).await?;
                    let mut reader = Cursor::new(buf);
                    let result = read_bytes(&mut reader).await?;
                    prop_assert_eq!(result, data);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_bool(val: bool) {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_bool(&mut buf, val).await?;
                    let mut reader = Cursor::new(buf);
                    let result = read_bool(&mut reader).await?;
                    prop_assert_eq!(result, val);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_string(s in "[a-zA-Z0-9 /._-]{0,200}") {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_string(&mut buf, &s).await?;
                    let mut reader = Cursor::new(buf);
                    let result = read_string(&mut reader).await?;
                    prop_assert_eq!(result, s);
                    Ok(())
                })?;
            }

            #[test]
            fn roundtrip_string_utf8(s in "\\PC{0,100}") {
                let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_string(&mut buf, &s).await?;
                    let mut reader = Cursor::new(buf);
                    let result = read_string(&mut reader).await?;
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
                    write_strings(&mut buf, &items).await?;
                    let mut reader = Cursor::new(buf);
                    let result = read_strings(&mut reader).await?;
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
                    let pairs: Vec<(String, String)> = pairs;
                    let mut buf = Vec::new();
                    write_string_pairs(&mut buf, &pairs).await?;
                    let mut reader = Cursor::new(buf);
                    let result = read_string_pairs(&mut reader).await?;
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
                    write_framed_stream(&mut buf, &data, chunk_size).await?;
                    let mut reader = Cursor::new(buf);
                    let result = read_framed_stream(&mut reader).await?;
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
                    write_framed_stream(&mut buf, &data, chunk_size).await?;
                    let reader = FramedStreamReader::new(
                        Cursor::new(buf),
                        MAX_FRAMED_TOTAL,
                    );
                    let mut result = Vec::new();
                    tokio::io::AsyncReadExt::read_to_end(
                        &mut tokio::io::BufReader::new(reader),
                        &mut result,
                    )
                    .await?;
                    prop_assert_eq!(result, data);
                    Ok(())
                })?;
            }
        }
    }
}

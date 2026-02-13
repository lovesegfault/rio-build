//! Nix worker protocol wire format primitives.
//!
//! All integers are 64-bit unsigned, little-endian (except handshake magic which is u32).
//! Strings are length-prefixed and padded to 8-byte boundaries.
//! Collections are count-prefixed.

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Padding alignment for the Nix wire format.
const PADDING: usize = 8;

/// Maximum allowed string length (64 MiB) to prevent OOM on malicious input.
const MAX_STRING_LEN: u64 = 64 * 1024 * 1024;

/// Maximum allowed collection count (1M items) to prevent OOM on malicious input.
const MAX_COLLECTION_COUNT: u64 = 1_048_576;

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

    #[error("unexpected end of input")]
    UnexpectedEof,
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

/// Read a little-endian u32 (only used for handshake magic bytes).
pub async fn read_u32<R: AsyncRead + Unpin>(r: &mut R) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).await?;
    Ok(u32::from_le_bytes(buf))
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

/// Write a little-endian u32 (only used for handshake magic bytes).
pub async fn write_u32<W: AsyncWrite + Unpin>(w: &mut W, val: u32) -> Result<()> {
    w.write_all(&val.to_le_bytes()).await?;
    Ok(())
}

/// Write a u64-encoded boolean.
pub async fn write_bool<W: AsyncWrite + Unpin>(w: &mut W, val: bool) -> Result<()> {
    write_u64(w, u64::from(val)).await
}

/// Write a length-prefixed, padded byte string.
pub async fn write_bytes<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    write_u64(w, data.len() as u64).await?;

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

/// Write a collection of UTF-8 strings.
pub async fn write_strings<W: AsyncWrite + Unpin>(w: &mut W, items: &[String]) -> Result<()> {
    write_u64(w, items.len() as u64).await?;
    for item in items {
        write_string(w, item).await?;
    }
    Ok(())
}

/// Write a collection of string slices.
pub async fn write_str_slice<W: AsyncWrite + Unpin>(w: &mut W, items: &[&str]) -> Result<()> {
    write_u64(w, items.len() as u64).await?;
    for item in items {
        write_string(w, item).await?;
    }
    Ok(())
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
    async fn test_u32_roundtrip() {
        for val in [0u32, 1, 0x6e697863, u32::MAX] {
            let mut buf = Vec::new();
            write_u32(&mut buf, val).await.unwrap();
            assert_eq!(buf.len(), 4);
            let mut reader = Cursor::new(buf);
            assert_eq!(read_u32(&mut reader).await.unwrap(), val);
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

    // Property-based tests
    mod proptests {
        use super::*;
        use proptest::prelude::*;
        use std::io::Cursor;

        proptest! {
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
        }
    }
}

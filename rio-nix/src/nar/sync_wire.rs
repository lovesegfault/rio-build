//! Synchronous `std::io` wire primitives for NAR.
//!
//! Sibling of [`crate::protocol::wire`] (the async tokio-based encoder used
//! by the daemon protocol). NAR runs inside `spawn_blocking`/filesystem
//! contexts so it uses blocking `Read`/`Write`; the byte layout is identical.
//!
//! The shared, IO-free core lives in `protocol::wire`: [`padding_len`] and
//! [`ZERO_PAD`](crate::protocol::wire::ZERO_PAD). Only the
//! `read_exact`/`write_all` adapter layer is duplicated here — Rust has no
//! ergonomic abstraction over sync vs async I/O, and these bodies are ~4
//! lines each.

use std::io::{self, Read, Write};

use crate::protocol::wire::{ZERO_PAD, padding_len};

use super::{MAX_NAME_LEN, MAX_TARGET_LEN, NarError, Result};

pub(super) fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

pub(super) fn write_u64(w: &mut impl Write, val: u64) -> io::Result<()> {
    w.write_all(&val.to_le_bytes())
}

/// Read `len` data bytes followed by validated zero-padding to the next
/// 8-byte boundary.
pub(super) fn read_padded_bytes(r: &mut impl Read, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    consume_padding(r, len)?;
    Ok(buf)
}

/// Consume the trailing pad bytes for a field of `len` data bytes and
/// reject if any are non-zero. Matches Nix C++ `readPadding` (serialise.cc).
pub(super) fn consume_padding(r: &mut impl Read, len: usize) -> Result<()> {
    let pad = padding_len(len);
    if pad > 0 {
        let mut pad_buf = [0u8; 8];
        r.read_exact(&mut pad_buf[..pad])?;
        if pad_buf[..pad].iter().any(|&b| b != 0) {
            return Err(NarError::NonZeroPadding(len));
        }
    }
    Ok(())
}

pub(super) fn read_bytes_bounded(r: &mut impl Read, max_len: u64) -> Result<Vec<u8>> {
    let len = read_u64(r)?;
    if len > max_len {
        return Err(NarError::ContentTooLarge(len));
    }
    read_padded_bytes(r, len as usize)
}

pub(super) fn read_name_bytes(r: &mut impl Read) -> Result<Vec<u8>> {
    let len = read_u64(r)?;
    if len > MAX_NAME_LEN {
        return Err(NarError::NameTooLong(len));
    }
    read_padded_bytes(r, len as usize)
}

pub(super) fn read_target_bytes(r: &mut impl Read) -> Result<Vec<u8>> {
    let len = read_u64(r)?;
    if len > MAX_TARGET_LEN {
        return Err(NarError::TargetTooLong(len));
    }
    read_padded_bytes(r, len as usize)
}

pub(super) fn read_string(r: &mut impl Read) -> Result<String> {
    let bytes = read_target_bytes(r)?;
    String::from_utf8(bytes).map_err(|e| NarError::InvalidUtf8 {
        context: "token",
        offset: e.utf8_error().valid_up_to(),
        source: e,
    })
}

pub(super) fn write_bytes(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write_u64(w, data.len() as u64)?;
    w.write_all(data)?;
    let pad = padding_len(data.len());
    if pad > 0 {
        w.write_all(&ZERO_PAD[..pad])?;
    }
    Ok(())
}

pub(super) fn write_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    write_bytes(w, s.as_bytes())
}

pub(super) fn expect_str(r: &mut impl Read, expected: &str) -> Result<()> {
    let got = read_string(r)?;
    if got != expected {
        return Err(NarError::UnexpectedToken {
            expected: expected.to_string(),
            got,
        });
    }
    Ok(())
}

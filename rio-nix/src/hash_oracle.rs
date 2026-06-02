//! Vendored line-by-line port of CppNix 2.34.7's non-SRI unprefixed
//! hash decoding, for differential testing ONLY.
//!
//! This is NOT production code: it exists so the proptest
//! `oracle_containment_4096` (in `hash.rs`) and the `fod_hash_decode`
//! fuzz target can execute the oracle's decode semantics against
//! [`crate::hash::NixHash::parse_nonsri_unprefixed`] on adversarial
//! inputs the differential VM corpus cannot reach (`nix-instantiate`
//! emits canonical base16 only). Every block cites the pinned source
//! file:line it ports; the deliberate quirks — the swallowed
//! `FormatError`, the base64 newline-skip / `=`-break / trailing-bit
//! discard, nixbase32's overflow-extends-output decode — are preserved
//! bit-for-bit, because the divergences they create are exactly what
//! `nix.divergence.fod-base64-strict` registers.
//!
//! Compiled only under `cfg(any(test, feature = "test-oracle"))` —
//! the same gate as the NAR test oracle.

use crate::hash::HashAlgo;

/// The oracle's rejection classes for `parseNonSRIUnprefixed`.
///
/// The oracle never surfaces a decode error from this entry point:
/// `parseLowLevel` (`hash.cc:145-166`) wraps `pair.decode(rest)` in a
/// `try` whose `catch (Error & e)` only calls `e.addTrace(...)` and
/// does NOT rethrow — `d` stays empty and falls through to the length
/// check, so every codec failure presents as `BadHashLength`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleReject {
    /// `baseFromSize` (`hash.cc:123-138`): the input length matches no
    /// encoding of this algorithm — "hash '%s' has wrong length for
    /// hash algorithm '%s'".
    WrongLength,
    /// `parseLowLevel` (`hash.cc:158-160`): the decoded byte count
    /// differs from the digest size — "invalid %s hash '%s', length %d
    /// != expected length %d". Includes every swallowed decode error.
    BadHashLength,
}

/// `base16::decode`, `base-n.cc:24-47`. Accepts `0-9 A-F a-f`; any
/// other character throws `FormatError` (swallowed upstream). The
/// oracle asserts an even input length — `baseFromSize` guarantees it
/// here (base16 length = 2 × digest size).
fn base16_decode(s: &str) -> Result<Vec<u8>, ()> {
    fn nibble(c: u8) -> Result<u8, ()> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(()),
        }
    }
    let b = s.as_bytes();
    debug_assert_eq!(b.len() % 2, 0);
    let mut res = Vec::with_capacity(b.len() / 2);
    for i in 0..b.len() / 2 {
        res.push(nibble(b[i * 2])? << 4 | nibble(b[i * 2 + 1])?);
    }
    Ok(res)
}

/// The Nix32 alphabet, `base-nix-32.hh:17` (no `e`, `o`, `u`, `t`).
const NIX32_CHARS: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// `BaseNix32::decode`, `base-nix-32.cc:42-71`. Iterates the input
/// REVERSED, packing 5 bits per character little-endian; when a
/// digit's high bits cross the current byte, the output is EXTENDED
/// (`res.resize(i + 2)`) — non-canonical trailing bits therefore grow
/// the result past the digest size and die at the length check rather
/// than at the codec.
fn nixbase32_decode(s: &str) -> Result<Vec<u8>, ()> {
    let bytes = s.as_bytes();
    let mut res: Vec<u8> = Vec::with_capacity((bytes.len() * 5).div_ceil(8));
    for n in 0..bytes.len() {
        let c = bytes[bytes.len() - n - 1];
        let digit = NIX32_CHARS.iter().position(|&x| x == c).ok_or(())? as u8;
        let b = n * 5;
        let i = b / 8;
        let j = b % 8;
        if res.len() < i + 1 {
            res.resize(i + 1, 0);
        }
        res[i] |= digit << j;
        let overflow = if j == 0 { 0 } else { digit >> (8 - j) };
        if overflow != 0 {
            if res.len() < i + 2 {
                res.resize(i + 2, 0);
            }
            res[i + 1] |= overflow;
        }
    }
    Ok(res)
}

/// The base64 alphabet, `base-n.cc:49-50`.
const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `base64::decode`, `base-n.cc:75-112`, quirks preserved:
/// - `if (c == '=') break;` — decoding STOPS at the first `=`
///   (`base-n.cc:94-95`); anything after it is ignored.
/// - `if (c == '\n') continue;` — newlines are SKIPPED
///   (`base-n.cc:96-97`).
/// - leftover bits below a full byte are silently DISCARDED
///   (`base-n.cc:103-107`) — non-zero trailing bits alias to the
///   canonical spelling's digest.
fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    let mut res = Vec::with_capacity((s.len() + 2) / 4 * 3);
    let mut d: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        if c == b'\n' {
            continue;
        }
        let digit = BASE64_CHARS.iter().position(|&x| x == c).ok_or(())? as u32;
        bits += 6;
        d = d << 6 | digit;
        if bits >= 8 {
            res.push((d >> (bits - 8) & 0xff) as u8);
            bits -= 8;
        }
    }
    Ok(res)
}

/// Which codec `baseFromSize` (`hash.cc:123-138`) selects for this
/// input length, if any. Encoded lengths: base16 = `2n`
/// (`base-n.hh:14`), Nix32 = `(8n - 1) / 5 + 1` (`base-nix-32.hh:37`),
/// base64 = `4 * ceil(n / 3)` (`base-n.hh:36`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleFormat {
    Base16,
    Nix32,
    Base64,
}

/// `baseFromSize`, `hash.cc:123-138`.
pub fn base_from_size(algo: HashAlgo, len: usize) -> Result<OracleFormat, OracleReject> {
    let n = algo.digest_len();
    if len == 2 * n {
        Ok(OracleFormat::Base16)
    } else if len == (8 * n - 1) / 5 + 1 {
        Ok(OracleFormat::Nix32)
    } else if len == n.div_ceil(3) * 4 {
        Ok(OracleFormat::Base64)
    } else {
        Err(OracleReject::WrongLength)
    }
}

/// `Hash::parseNonSRIUnprefixed` (`hash.cc:260-263`) =
/// `baseFromSize` + `parseLowLevel` (`hash.cc:145-166`), with the
/// swallowed-error quirk: a codec failure leaves the decoded buffer
/// EMPTY and presents as the length error.
pub fn parse_nonsri_unprefixed_oracle(algo: HashAlgo, s: &str) -> Result<Vec<u8>, OracleReject> {
    let format = base_from_size(algo, s.len())?;
    let decoded = match format {
        OracleFormat::Base16 => base16_decode(s),
        OracleFormat::Nix32 => nixbase32_decode(s),
        OracleFormat::Base64 => base64_decode(s),
    }
    // The swallowed FormatError (`hash.cc:151-155`): error → empty.
    .unwrap_or_default();
    if decoded.len() != algo.digest_len() {
        return Err(OracleReject::BadHashLength);
    }
    Ok(decoded)
}

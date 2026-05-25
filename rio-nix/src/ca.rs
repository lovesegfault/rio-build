//! Content-addressing primitives for floating-CA outputs.
//!
//! A floating content-addressed output is built into a *scratch* path
//! (its real path is unknown until after the build), so the bytes on
//! disk embed the scratch path's hash part wherever the build wrote
//! `$out`. Computing the real (content-derived) path therefore needs a
//! hash of the content **modulo** those self-references — and once the
//! real path is known, the content must be rewritten scratch→final.
//! This module provides the two streaming primitives for that:
//!
//! - [`HashModuloSink`]: hashes a byte stream with every occurrence of
//!   one designated 32-char hash part replaced by NUL bytes. This is
//!   the "hash modulo self-references" of CppNix's `HashModuloSink`
//!   (`src/libutil/references.cc`): the replacement is
//!   `std::string(modulus.size(), 0)` — i.e. **0x00 bytes, not ASCII
//!   `'0'`**. (CppNix's comment about also hashing the *positions* of
//!   self-references is dead code in current Nix — the matches vector
//!   is never populated — so the hash is purely
//!   content-with-self-hashes-zeroed. Pinned by the golden fixture
//!   tests in `tests/ca_golden.rs` against a real `nix` 2.34 build.)
//!
//! - [`RewritingSink`]: streaming same-length substring replacement
//!   (hash-part rewrites: sibling scratch→final before hashing, own
//!   scratch→final after the path is known), writing through to an
//!   inner writer.
//!
//! Both handle matches that straddle `write()` chunk boundaries by
//! holding back the last `pattern_len - 1` bytes until more data (or
//! [`finish`](RewritingSink::finish)) arrives, so callers may feed
//! arbitrarily-split chunks (e.g. straight from
//! [`nar::dump_path_streaming`](crate::nar::dump_path_streaming)).

use std::io::{self, Write};

use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::hash::{HashAlgo, NixHash};

/// Streaming same-length substring rewriter.
///
/// Replaces every occurrence of each `(from, to)` pair in the byte
/// stream and forwards the result to the inner writer. All pairs must
/// satisfy `from.len() == to.len()` (no length shifts — the use case
/// is 32-character store-path hash parts) and `from` must be
/// non-empty. Matches are non-overlapping and scanned left-to-right;
/// replaced bytes are not rescanned (matching CppNix
/// `rewriteStrings`).
///
/// Call [`finish`](Self::finish) (or [`flush`](Write::flush) followed
/// by dropping) to flush the held-back tail; `finish` returns the
/// inner writer and the total number of replacements made.
pub struct RewritingSink<W> {
    /// `(from, to)` pairs, all the same `from.len() == to.len()`.
    rewrites: Vec<(Vec<u8>, Vec<u8>)>,
    /// Longest `from` length — how many trailing bytes must be held
    /// back across writes (minus one) to catch straddling matches.
    max_from: usize,
    /// Held-back tail from the previous write (length < `max_from`).
    tail: Vec<u8>,
    /// Total replacements performed so far.
    replaced: u64,
    inner: W,
}

/// Error constructing a [`RewritingSink`] or [`HashModuloSink`].
#[derive(Debug, thiserror::Error)]
pub enum RewriteError {
    #[error("rewrite pair {index}: from is empty")]
    EmptyFrom { index: usize },

    #[error("rewrite pair {index}: from is {from} bytes but to is {to} bytes (must be equal)")]
    LengthMismatch {
        index: usize,
        from: usize,
        to: usize,
    },
}

impl<W: Write> RewritingSink<W> {
    /// Create a sink replacing each `from` with its `to`, writing the
    /// rewritten stream to `inner`.
    pub fn new(
        rewrites: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
        inner: W,
    ) -> Result<Self, RewriteError> {
        let rewrites: Vec<(Vec<u8>, Vec<u8>)> = rewrites.into_iter().collect();
        let mut max_from = 0;
        for (i, (from, to)) in rewrites.iter().enumerate() {
            if from.is_empty() {
                return Err(RewriteError::EmptyFrom { index: i });
            }
            if from.len() != to.len() {
                return Err(RewriteError::LengthMismatch {
                    index: i,
                    from: from.len(),
                    to: to.len(),
                });
            }
            max_from = max_from.max(from.len());
        }
        Ok(Self {
            rewrites,
            max_from,
            tail: Vec::new(),
            replaced: 0,
            inner,
        })
    }

    /// Number of replacements performed so far (excluding anything
    /// still hidden in the held-back tail).
    pub fn replacements(&self) -> u64 {
        self.replaced
    }

    /// Flush the held-back tail and return the inner writer plus the
    /// total replacement count.
    pub fn finish(mut self) -> io::Result<(W, u64)> {
        self.drain_tail()?;
        Ok((self.inner, self.replaced))
    }

    /// Write the held-back tail through (no further data is coming, so
    /// it can no longer be the start of a straddling match).
    fn drain_tail(&mut self) -> io::Result<()> {
        if !self.tail.is_empty() {
            let tail = std::mem::take(&mut self.tail);
            self.inner.write_all(&tail)?;
        }
        Ok(())
    }

    /// Rewrite `buf` in place (it already has the previous tail
    /// prepended), forward everything except the new tail, and stash
    /// the new tail.
    fn process(&mut self, mut buf: Vec<u8>) -> io::Result<()> {
        if self.rewrites.is_empty() {
            self.inner.write_all(&buf)?;
            return Ok(());
        }

        // Left-to-right scan with non-overlapping replacement. The
        // patterns are store-path hash parts (high-entropy nixbase32),
        // so a first-byte mismatch rejects almost every position —
        // the inner loop is effectively one byte-compare per position.
        let mut i = 0;
        while i < buf.len() {
            let mut matched_len = None;
            for (from, to) in &self.rewrites {
                if buf[i..].len() >= from.len() && &buf[i..i + from.len()] == from.as_slice() {
                    buf[i..i + to.len()].copy_from_slice(to);
                    matched_len = Some(to.len());
                    self.replaced += 1;
                    break;
                }
            }
            i += matched_len.unwrap_or(1);
        }

        // Hold back the last (max_from - 1) bytes: they could be the
        // start of a match whose remainder arrives in the next write.
        // Bytes already *replaced* above may land in the held-back
        // region and be scanned again next round; that is harmless for
        // hash-part rewrites because a replacement value is never one
        // of the `from` patterns (scratch hashes are replaced *by*
        // final hashes, never the other way around), so the rescan can
        // never match. The forwarded region is final either way.
        // (saturating_sub keeps the empty-rewrites invariant local even
        // though that case already returned above.)
        let keep = self.max_from.saturating_sub(1).min(buf.len());
        let forward = buf.len() - keep;
        self.inner.write_all(&buf[..forward])?;
        self.tail = buf[forward..].to_vec();
        Ok(())
    }
}

impl<W: Write> Write for RewritingSink<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // TODO(perf): this allocates a fresh working buffer (tail +
        // chunk) per write call and a fresh tail Vec per call — one
        // extra copy of the whole stream. A persistent reusable buffer
        // would remove both; not worth it until a profile shows CA
        // uploads spending time here.
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(data);
        self.process(buf)?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Deliberately does NOT drain the tail: a straddling match
        // could still complete on the next write. Use `finish()` to
        // terminate the stream.
        self.inner.flush()
    }
}

/// Incremental hasher over the supported [`HashAlgo`]s.
///
/// Internal helper for [`HashModuloSink`]; also usable on its own when
/// a caller needs a plain streaming hash with a runtime-selected
/// algorithm (e.g. re-hashing rewritten CA content for `narHash`).
pub enum HashWriter {
    Sha1(Sha1),
    Sha256(Sha256),
    Sha512(Sha512),
}

impl HashWriter {
    pub fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::SHA1 => Self::Sha1(Sha1::new()),
            HashAlgo::SHA256 => Self::Sha256(Sha256::new()),
            HashAlgo::SHA512 => Self::Sha512(Sha512::new()),
        }
    }

    pub fn finish(self) -> NixHash {
        let (algo, digest) = match self {
            Self::Sha1(h) => (HashAlgo::SHA1, h.finalize().to_vec()),
            Self::Sha256(h) => (HashAlgo::SHA256, h.finalize().to_vec()),
            Self::Sha512(h) => (HashAlgo::SHA512, h.finalize().to_vec()),
        };
        NixHash::new(algo, digest).expect("digest length matches algo by construction")
    }
}

impl Write for HashWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            Self::Sha1(h) => h.update(data),
            Self::Sha256(h) => h.update(data),
            Self::Sha512(h) => h.update(data),
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Hash a byte stream modulo one self-reference hash part.
///
/// Every occurrence of `modulus` (the 32-character nixbase32 hash part
/// of the output's *scratch* path) is replaced by `modulus.len()` NUL
/// bytes before hashing. The result is the content hash that
/// [`StorePath::make_fixed_output_with_self`](crate::store_path::StorePath::make_fixed_output_with_self)
/// turns into the final CA store path — and it is invariant under the
/// later scratch→final rewrite, because the final hash part occupies
/// exactly the same byte positions.
///
/// Feed it the NAR serialization (recursive/`nar` ingestion) or the
/// raw file contents (flat ingestion), then call
/// [`finish`](Self::finish).
pub struct HashModuloSink {
    inner: RewritingSink<HashWriter>,
}

impl HashModuloSink {
    /// `modulus` is the 32-char nixbase32 hash part to zero out;
    /// `algo` is the output's declared hash algorithm.
    ///
    /// # Panics
    ///
    /// Panics if `modulus` is empty. Callers always pass a store path's
    /// hash part (32 characters by construction); an empty modulus is a
    /// caller bug, not an input condition.
    pub fn new(algo: HashAlgo, modulus: &str) -> Self {
        let rewrites = vec![(modulus.as_bytes().to_vec(), vec![0u8; modulus.len()])];
        let inner = RewritingSink::new(rewrites, HashWriter::new(algo))
            .expect("modulus is non-empty and replacement has equal length");
        Self { inner }
    }

    /// Number of self-reference occurrences zeroed so far.
    pub fn replacements(&self) -> u64 {
        self.inner.replacements()
    }

    /// Finish the stream and return `(hash, occurrence_count)`.
    pub fn finish(self) -> (NixHash, u64) {
        let (hasher, replaced) = self
            .inner
            .finish()
            .expect("HashWriter::write_all is infallible");
        (hasher.finish(), replaced)
    }
}

impl Write for HashModuloSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.inner.write(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 32-char stand-ins for hash parts (the sinks don't care that
    /// they aren't real nixbase32, only the length matters).
    const A: &[u8] = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &[u8] = b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &[u8] = b"cccccccccccccccccccccccccccccccc";

    fn rewrite_all(pairs: &[(&[u8], &[u8])], data: &[u8], chunk: usize) -> (Vec<u8>, u64) {
        let mut sink = RewritingSink::new(
            pairs
                .iter()
                .map(|(f, t)| (f.to_vec(), t.to_vec()))
                .collect::<Vec<_>>(),
            Vec::new(),
        )
        .unwrap();
        for c in data.chunks(chunk.max(1)) {
            sink.write_all(c).unwrap();
        }
        let (out, n) = sink.finish().unwrap();
        (out, n)
    }

    #[test]
    fn no_occurrence_passes_through() {
        let data = b"hello world, nothing to see here".repeat(7);
        for chunk in [1, 3, 32, 1024] {
            let (out, n) = rewrite_all(&[(A, B)], &data, chunk);
            assert_eq!(out, data);
            assert_eq!(n, 0);
        }
    }

    #[test]
    fn single_and_multiple_occurrences() {
        let mut data = Vec::new();
        data.extend_from_slice(b"/nix/store/");
        data.extend_from_slice(A);
        data.extend_from_slice(b"-foo and also ");
        data.extend_from_slice(A);
        data.extend_from_slice(b" twice");
        let mut want = Vec::new();
        want.extend_from_slice(b"/nix/store/");
        want.extend_from_slice(B);
        want.extend_from_slice(b"-foo and also ");
        want.extend_from_slice(B);
        want.extend_from_slice(b" twice");
        for chunk in 1..=data.len() {
            let (out, n) = rewrite_all(&[(A, B)], &data, chunk);
            assert_eq!(out, want, "chunk size {chunk}");
            assert_eq!(n, 2, "chunk size {chunk}");
        }
    }

    #[test]
    fn adjacent_occurrences() {
        let mut data = Vec::new();
        data.extend_from_slice(A);
        data.extend_from_slice(A);
        let mut want = Vec::new();
        want.extend_from_slice(B);
        want.extend_from_slice(B);
        for chunk in 1..=data.len() {
            let (out, n) = rewrite_all(&[(A, B)], &data, chunk);
            assert_eq!(out, want, "chunk size {chunk}");
            assert_eq!(n, 2);
        }
    }

    #[test]
    fn occurrence_at_start_and_end() {
        let mut data = Vec::new();
        data.extend_from_slice(A);
        data.extend_from_slice(b"middle");
        data.extend_from_slice(A);
        for chunk in 1..=data.len() {
            let (out, n) = rewrite_all(&[(A, B)], &data, chunk);
            assert_eq!(&out[..32], B, "chunk size {chunk}");
            assert_eq!(&out[out.len() - 32..], B);
            assert_eq!(n, 2);
        }
    }

    #[test]
    fn multiple_pairs_and_shared_prefix() {
        // Two froms sharing a 31-byte prefix: the scan must pick the
        // right one at each site.
        let mut a_prime = A.to_vec();
        *a_prime.last_mut().unwrap() = b'z';
        let mut data = Vec::new();
        data.extend_from_slice(A);
        data.extend_from_slice(b" / ");
        data.extend_from_slice(&a_prime);
        let (out, n) = rewrite_all(&[(A, B), (&a_prime, C)], &data, 5);
        let mut want = Vec::new();
        want.extend_from_slice(B);
        want.extend_from_slice(b" / ");
        want.extend_from_slice(C);
        assert_eq!(out, want);
        assert_eq!(n, 2);
    }

    #[test]
    fn rejects_bad_pairs() {
        assert!(matches!(
            RewritingSink::new(vec![(Vec::new(), vec![1])], Vec::new()),
            Err(RewriteError::EmptyFrom { index: 0 })
        ));
        assert!(matches!(
            RewritingSink::new(vec![(vec![1, 2], vec![1])], Vec::new()),
            Err(RewriteError::LengthMismatch {
                index: 0,
                from: 2,
                to: 1
            })
        ));
    }

    #[test]
    fn hash_modulo_equals_hash_of_nul_substituted() {
        // Reference implementation: replace then hash in one shot.
        let modulus = std::str::from_utf8(A).unwrap();
        let mut data = Vec::new();
        data.extend_from_slice(b"self at /nix/store/");
        data.extend_from_slice(A);
        data.extend_from_slice(b"-thing\n");
        let mut expected_input = data.clone();
        let pos = 19;
        expected_input[pos..pos + 32].copy_from_slice(&[0u8; 32]);
        let expected = NixHash::compute(HashAlgo::SHA256, &expected_input);

        for chunk in 1..=data.len() {
            let mut sink = HashModuloSink::new(HashAlgo::SHA256, modulus);
            for c in data.chunks(chunk) {
                sink.write_all(c).unwrap();
            }
            let (got, n) = sink.finish();
            assert_eq!(got.digest(), expected.digest(), "chunk size {chunk}");
            assert_eq!(n, 1);
        }
    }

    #[test]
    fn hash_modulo_no_occurrence_is_plain_hash() {
        let data = b"no self references at all";
        let mut sink = HashModuloSink::new(HashAlgo::SHA256, std::str::from_utf8(A).unwrap());
        sink.write_all(data).unwrap();
        let (got, n) = sink.finish();
        assert_eq!(
            got.digest(),
            NixHash::compute(HashAlgo::SHA256, data).digest()
        );
        assert_eq!(n, 0);
    }

    #[test]
    fn hash_writer_algos_match_oneshot() {
        let data = b"algo parity check";
        for algo in [HashAlgo::SHA1, HashAlgo::SHA256, HashAlgo::SHA512] {
            let mut w = HashWriter::new(algo);
            w.write_all(data).unwrap();
            assert_eq!(w.finish().digest(), NixHash::compute(algo, data).digest());
        }
    }
}

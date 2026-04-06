# Remediation 02: Empty references — NAR content scanner

**Parent finding:** [§1.2 Worker empty references → GC data loss + signature + sandbox breakage](../phase4a.md#12-worker-empty-references--gc-data-loss--signature--sandbox-breakage)
**Consolidated findings:** `wkr-upload-refs-empty`, `store-gc-mark-walks-empty`, `store-sign-fingerprint-empty-refs`, `wkr-sandbox-bindmount-missing-transitive`, `wkr-upload-deriver-empty`
**Blast radius:** GC sweeps live paths past `grace_hours=2`; signatures permanently commit to `refs=""`; sandbox ENOENT on transitive deps.

---

## 0. Summary of changes

| # | Component | Change | Lines |
|---|---|---|---|
| 1 | `rio-nix` | New module `refscan` — streaming reference scanner | ~250 |
| 2 | `rio-worker/src/upload.rs` | Tee `RefScanSink` alongside `HashingChannelWriter`; fill `references` + `deriver` | ~80 |
| 3 | `rio-worker/src/executor/mod.rs` | Plumb `drv_path` + candidate set into `upload_all_outputs` | ~15 |
| 4 | `rio-store/src/grpc/mod.rs` | `warn!` on signing non-CA path with zero refs | ~5 |
| 5 | `rio-store/src/grpc/admin.rs` | GC safety gate: refuse sweep if >N% empty-ref narinfo | ~40 |
| 6 | `rio-store/src/grpc/admin.rs` | New admin RPC `ResignPaths` for re-sign sweep | ~120 |
| 7 | `migrations/010_refscan_backfill.sql` | Flag column for pre-fix paths | ~10 |
| 8 | `docs/src/components/worker.md` | Rewrite §upload ¶190 (remove "Phase deferral") + new `r[...]` markers | — |

---

## 1. NAR content scanner

### 1.1 Algorithm

Cribbed directly from Nix C++ `src/libstore/references.cc` (verified against local checkout at `references/nix/src/libstore/references.cc`). **Not** Aho-Corasick — it's a purpose-built skip-scan that exploits the restricted nixbase32 alphabet:

```
search(data, candidates, found):
    i ← 0
    while i + 32 ≤ data.len():
        # Check window data[i..i+32] backwards from the END
        for j in 31 downto 0:
            if data[i+j] is NOT a nixbase32 char:
                i ← i + j + 1          # ← Boyer-Moore-style skip
                continue outer loop
        # All 32 chars valid — candidate lookup
        ref ← data[i..i+32]
        if ref ∈ candidates:
            candidates.remove(ref)      # ← once found, stop searching for it
            found.insert(ref)
        i ← i + 1
```

**Why backwards?** The nixbase32 alphabet is `0123456789abcdfghijklmnpqrsvwxyz` — 32 of 256 possible byte values (~12.5%). In binary data, the *last* byte of any 32-byte window is very likely non-base32, so checking from the end lets us skip `j+1` bytes in a single comparison. For binary sections of a NAR (ELF headers, compressed blobs), this approaches `O(n/32)` comparisons. For text sections it degrades toward `O(n)` but still never worse than a naïve scan.

**Why not Aho-Corasick?** The §1.2 report suggested it, but the Nix approach is better here:
- No automaton to build (candidate set is just `HashSet<[u8; 32]>`).
- The alphabet filter rejects ~87.5% of windows with zero hash-map probes.
- `candidates.remove` means each hash is looked up at most a handful of times after its first match (early exit).
- `aho-corasick` is already in the dep tree (transitive via `regex`), but adding a direct dep just to use a more general algorithm on a problem with a natural specialization is waste.

### 1.2 Streaming: chunk-boundary handling

A reference can span two `write()` calls. Nix's `RefScanSink::operator()` keeps a rolling tail of the previous chunk's last `refLength` bytes:

```
on_chunk(data):
    # Search the tail+head seam
    seam ← tail ++ data[0..min(data.len(), 32)]
    search(seam, ...)
    # Search the full chunk
    search(data, ...)
    # Update tail: keep last 31 bytes for next seam
    rest ← 32 - min(data.len(), 32)
    if rest < tail.len():
        tail ← tail[tail.len()-rest..]
    tail ← tail ++ data[data.len()-min(data.len(),32)..]
```

The seam buffer is at most `2 × refLength = 64` bytes. The full-chunk search does redundant work on the first 31 bytes (already covered by the seam search), but that's negligible vs. the simplicity of not having to track "how much of the seam was already searched."

### 1.3 Module location & API

**New file:** `rio-nix/src/refscan.rs`

Lives in `rio-nix` (not `rio-worker`) because:
- It's a pure function of NAR bytes + the nixbase32 alphabet — no worker runtime dependencies.
- `rio-store`'s re-sign sweep job ([§4](#4-migration-re-sign-sweep)) also needs it to re-scan NARs pulled from the blob store.
- `rio-nix` already owns `store_path::nixbase32` (the alphabet constant lives there).

```rust
// rio-nix/src/refscan.rs

use std::collections::HashSet;
use crate::store_path::HASH_CHARS; // = 32

/// Lookup table: `IS_BASE32[b] == true` iff byte `b` is in the nixbase32
/// alphabet (`0123456789abcdfghijklmnpqrsvwxyz`). Initialized at compile
/// time — the hot path is a single array index, no branching.
const IS_BASE32: [bool; 256] = {
    let mut t = [false; 256];
    let alphabet = b"0123456789abcdfghijklmnpqrsvwxyz";
    let mut i = 0;
    while i < alphabet.len() {
        t[alphabet[i] as usize] = true;
        i += 1;
    }
    t
};

/// Streaming reference scanner. Feed NAR bytes via `Write`; `into_found()`
/// returns the subset of `candidates` whose 32-char hash prefix appeared
/// anywhere in the stream.
///
/// Chunk-boundary safe: a hash straddling two `write()` calls is detected
/// via a 31-byte rolling tail buffer.
///
/// Algorithm matches Nix's `RefScanSink` (src/libstore/references.cc) —
/// a Boyer-Moore-style skip-scan over the restricted nixbase32 alphabet.
pub struct RefScanSink {
    /// Hash prefixes not yet found. Moved to `found` on first match
    /// (so we stop probing the hash map for already-found refs).
    remaining: HashSet<[u8; HASH_CHARS]>,
    found: HashSet<[u8; HASH_CHARS]>,
    /// Last ≤31 bytes of the previous chunk, for seam searching.
    tail: Vec<u8>,
}

impl RefScanSink {
    /// Construct a scanner for the given candidate hash prefixes.
    ///
    /// `candidates` are the 32-byte nixbase32 **hash parts** of store paths
    /// (i.e., the `abc...xyz` in `/nix/store/abc...xyz-name`), as ASCII
    /// bytes. Callers typically build this from
    /// `StorePath::hash_part().as_bytes().try_into()`.
    pub fn new(candidates: impl IntoIterator<Item = [u8; HASH_CHARS]>) -> Self {
        Self {
            remaining: candidates.into_iter().collect(),
            found: HashSet::new(),
            tail: Vec::with_capacity(HASH_CHARS),
        }
    }

    /// Consume the sink and return the set of candidate hashes that
    /// appeared in the stream.
    pub fn into_found(self) -> HashSet<[u8; HASH_CHARS]> {
        self.found
    }

    /// Fast path: all candidates already found → subsequent writes are
    /// no-ops. Upload hot loop can check this to skip the scan entirely
    /// once the (typically small) candidate set is exhausted.
    #[inline]
    pub fn is_exhausted(&self) -> bool {
        self.remaining.is_empty()
    }

    /// Core search: Boyer-Moore-style skip-scan. Mutates `remaining`/`found`
    /// in place. Separate from `write()` so both seam + full-chunk paths
    /// can call it.
    fn search(&mut self, data: &[u8]) {
        if data.len() < HASH_CHARS || self.remaining.is_empty() {
            return;
        }
        let mut i = 0;
        'outer: while i + HASH_CHARS <= data.len() {
            // Check from the END of the window backwards. On first
            // non-base32 byte at offset j, no window starting in i..=i+j
            // can possibly be a valid hash — skip all of them.
            let mut j = HASH_CHARS - 1;
            loop {
                if !IS_BASE32[data[i + j] as usize] {
                    i += j + 1;
                    continue 'outer;
                }
                if j == 0 { break; }
                j -= 1;
            }
            // All 32 bytes are valid nixbase32. Probe the candidate set.
            // SAFETY: slice is exactly HASH_CHARS bytes (loop invariant).
            let window: [u8; HASH_CHARS] =
                data[i..i + HASH_CHARS].try_into().unwrap();
            if self.remaining.remove(&window) {
                self.found.insert(window);
            }
            i += 1;
        }
    }
}

impl std::io::Write for RefScanSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        // Early out: all refs found, nothing left to scan for.
        // (Still return Ok(data.len()) — the tee keeps writing through us.)
        if self.remaining.is_empty() {
            return Ok(data.len());
        }

        // Seam search: a hash may start in the tail and end in `data`.
        // tail is ≤31 bytes; we append up to 32 bytes from `data` so the
        // seam buffer is ≤63 bytes — always stack-cheap.
        let tail_len = data.len().min(HASH_CHARS);
        if !self.tail.is_empty() {
            let mut seam = std::mem::take(&mut self.tail);
            seam.extend_from_slice(&data[..tail_len]);
            self.search(&seam);
            // Restore tail from seam (pre-extension part), then the
            // update below overwrites it properly. We `take` then
            // re-assign rather than clone because `search` needs `&mut
            // self` and `&seam` simultaneously.
            self.tail = seam;
        }

        // Full-chunk search. Overlaps the seam's tail region by ≤31
        // bytes — harmless (hash lookups are idempotent).
        self.search(data);

        // Update tail: keep the last (HASH_CHARS - 1) bytes of `data`
        // (or all of `data` if it's shorter). Matches Nix's
        // `references.cc:49-52` tail-shift logic.
        let rest = HASH_CHARS - tail_len;
        if rest < self.tail.len() {
            self.tail.drain(..self.tail.len() - rest);
        }
        self.tail
            .extend_from_slice(&data[data.len() - tail_len..]);
        // Cap at HASH_CHARS - 1 (we never need more overlap than that).
        if self.tail.len() >= HASH_CHARS {
            self.tail.drain(..self.tail.len() - (HASH_CHARS - 1));
        }

        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
```

**Add to `rio-nix/src/lib.rs`:**

```rust
pub mod refscan;
```

### 1.4 Helper: build candidate set from store paths

Scanner wants `[u8; 32]` hash parts; callers have `String` / `StorePath`. Small helper to bridge:

```rust
// rio-nix/src/refscan.rs (continued)

use crate::store_path::StorePath;

/// Extract the 32-byte nixbase32 hash part from a full store path string
/// (`/nix/store/abc...-name`) for use as a `RefScanSink` candidate.
///
/// Returns `None` if the path doesn't parse — caller decides whether
/// that's fatal (it shouldn't be for paths that came from a parsed
/// `Derivation`, but may be for user-supplied input).
pub fn hash_part_of(path: &str) -> Option<[u8; HASH_CHARS]> {
    let sp = StorePath::parse(path).ok()?;
    // hash_part() returns a fresh String (nixbase32::encode). Always 32
    // ASCII bytes by construction — try_into can't fail, but we use .ok()
    // to keep this function total.
    sp.hash_part().as_bytes().try_into().ok()
}

/// A `(hash_part → full_path)` map. Scanner returns hash parts; caller
/// maps them back to full `/nix/store/...` paths for `PathInfo.references`.
pub struct CandidateSet {
    map: std::collections::HashMap<[u8; HASH_CHARS], String>,
}

impl CandidateSet {
    /// Build from an iterator of full store path strings. Paths that
    /// don't parse are silently skipped (they can't possibly match).
    pub fn from_paths<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let map = paths
            .into_iter()
            .filter_map(|p| {
                let p = p.as_ref();
                hash_part_of(p).map(|h| (h, p.to_string()))
            })
            .collect();
        Self { map }
    }

    /// Hash parts for feeding into `RefScanSink::new`.
    pub fn hashes(&self) -> impl Iterator<Item = [u8; HASH_CHARS]> + '_ {
        self.map.keys().copied()
    }

    /// Map found hash parts back to full store paths. Sorted for
    /// determinism (PathInfo.references order affects the narinfo
    /// signature fingerprint — MUST be stable).
    pub fn resolve(&self, found: &HashSet<[u8; HASH_CHARS]>) -> Vec<String> {
        let mut refs: Vec<String> = found
            .iter()
            .filter_map(|h| self.map.get(h).cloned())
            .collect();
        refs.sort();
        refs
    }
}
```

### 1.5 Unit tests

```rust
// rio-nix/src/refscan.rs — #[cfg(test)] mod tests

#[test]
fn finds_hash_in_plain_bytes() {
    // A valid 32-char nixbase32 hash (from a real store path).
    let hash = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    let mut sink = RefScanSink::new([hash]);
    sink.write_all(b"prefix /nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-hello-2.12 suffix")
        .unwrap();
    assert_eq!(sink.into_found(), HashSet::from([hash]));
}

#[test]
fn no_false_positive_for_valid_base32_not_in_candidates() {
    // 32 valid base32 chars, but NOT in the candidate set.
    let candidate = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    let decoy     = *b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; // all valid base32
    let mut sink = RefScanSink::new([candidate]);
    sink.write_all(&decoy).unwrap();
    assert!(sink.into_found().is_empty());
}

#[test]
fn no_false_positive_for_near_miss() {
    // 31 matching chars + 1 off. Must NOT match.
    let candidate = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    let mut sink = RefScanSink::new([candidate]);
    sink.write_all(b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3b").unwrap(); // last char differs
    assert!(sink.into_found().is_empty());
}

#[test]
fn boyer_moore_skip_over_binary() {
    // Binary garbage (0xFF is not base32) followed by a hash. The skip
    // should land us directly at the hash without scanning every byte.
    // (Correctness test only — perf is measured by the benchmark below.)
    let hash = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    let mut data = vec![0xFFu8; 10_000];
    data.extend_from_slice(&hash);
    let mut sink = RefScanSink::new([hash]);
    sink.write_all(&data).unwrap();
    assert_eq!(sink.into_found(), HashSet::from([hash]));
}

#[test]
fn finds_hash_straddling_chunk_boundary() {
    // THE seam test. Split a hash across two write() calls at every
    // possible offset — each split point must be detected.
    let hash = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    for split in 1..31 {
        let mut sink = RefScanSink::new([hash]);
        let mut chunk1 = b"leading garbage ".to_vec();
        chunk1.extend_from_slice(&hash[..split]);
        let mut chunk2 = hash[split..].to_vec();
        chunk2.extend_from_slice(b" trailing");
        sink.write_all(&chunk1).unwrap();
        sink.write_all(&chunk2).unwrap();
        assert_eq!(
            sink.into_found(),
            HashSet::from([hash]),
            "failed at split={split}"
        );
    }
}

#[test]
fn finds_hash_straddling_many_tiny_chunks() {
    // Pathological: every write() is 1 byte. Seam logic must still work.
    let hash = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    let mut sink = RefScanSink::new([hash]);
    for &b in b"xx".iter().chain(hash.iter()).chain(b"yy".iter()) {
        sink.write_all(&[b]).unwrap();
    }
    assert_eq!(sink.into_found(), HashSet::from([hash]));
}

#[test]
fn self_reference_detected() {
    // An output can contain its own path (shebangs, rpaths). The
    // candidate set includes the output itself — scanner must find it.
    let own_hash = *b"v5sv61sszx301i0x6xysaqzla09nksnd";
    let mut sink = RefScanSink::new([own_hash]);
    // Simulated shebang line pointing at self.
    sink.write_all(b"#!/nix/store/v5sv61sszx301i0x6xysaqzla09nksnd-foo/bin/sh\n")
        .unwrap();
    assert_eq!(sink.into_found(), HashSet::from([own_hash]));
}

#[test]
fn is_exhausted_short_circuits() {
    let hash = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    let mut sink = RefScanSink::new([hash]);
    assert!(!sink.is_exhausted());
    sink.write_all(&hash).unwrap();
    assert!(sink.is_exhausted());
    // Subsequent writes are no-ops (can't observe directly, but at
    // least verify no panic / no state change).
    sink.write_all(&[0u8; 1024]).unwrap();
    assert_eq!(sink.into_found(), HashSet::from([hash]));
}

#[test]
fn nar_roundtrip_scan() {
    // End-to-end: write a file containing a store path, dump it as a
    // NAR, feed the NAR through the scanner. This is the shape of the
    // real worker flow.
    use crate::nar;
    let dep_hash = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("output");
    std::fs::write(
        &out,
        b"RPATH=/nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-glibc-2.38/lib\n",
    )
    .unwrap();

    let mut sink = RefScanSink::new([dep_hash]);
    nar::dump_path_streaming(&out, &mut sink).unwrap();
    assert_eq!(sink.into_found(), HashSet::from([dep_hash]));
}

proptest! {
    /// Property: hash embedded at a random offset in random bytes is
    /// always found, regardless of chunking.
    #[test]
    fn prop_finds_hash_at_random_offset(
        prefix in proptest::collection::vec(any::<u8>(), 0..500),
        suffix in proptest::collection::vec(any::<u8>(), 0..500),
        chunk_size in 1usize..200,
    ) {
        let hash = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
        let mut data = prefix;
        data.extend_from_slice(&hash);
        data.extend_from_slice(&suffix);

        let mut sink = RefScanSink::new([hash]);
        for chunk in data.chunks(chunk_size) {
            sink.write_all(chunk).unwrap();
        }
        prop_assert!(sink.into_found().contains(&hash));
    }
}
```

**Fuzz target** (`rio-nix/fuzz/fuzz_targets/refscan.rs`):

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use rio_nix::refscan::RefScanSink;
use std::io::Write;

fuzz_target!(|input: &[u8]| {
    // Fixed candidate (fuzzer explores input space, not candidate space).
    let cand = *b"7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    let mut sink = RefScanSink::new([cand]);
    // Feed in small irregular chunks to exercise seam logic.
    for chunk in input.chunks(input.first().copied().unwrap_or(7) as usize % 60 + 1) {
        let _ = sink.write_all(chunk);
    }
    let _ = sink.into_found();
    // Oracle: result ⊆ {cand}. No crash, no hang.
});
```

Add to `rio-nix/fuzz/Cargo.toml` and to `fuzzTargets` in `flake.nix`.

---

## 2. Plumbing: worker upload path

### 2.1 New tee: `ScanningHashingWriter`

Extend the existing tee (`rio-worker/src/upload.rs:324-413` — `HashingChannelWriter`) to a three-way tee: hash + chunk-to-channel + ref-scan. Composition over inheritance — wrap, don't modify.

```rust
// rio-worker/src/upload.rs — new struct below HashingChannelWriter

use rio_nix::refscan::RefScanSink;

/// Three-way tee: every byte goes through SHA-256, the gRPC chunk
/// channel, AND the reference scanner. Single pass, no rewind.
///
/// Wraps `HashingChannelWriter` rather than duplicating its logic —
/// the scanner is an independent stage that happens to share the byte
/// stream. If the scanner exhausts (`is_exhausted()`) early, its
/// `write()` becomes a no-op; the hash+chunk path is unaffected.
struct ScanningHashingWriter {
    inner: HashingChannelWriter,
    scanner: RefScanSink,
}

impl ScanningHashingWriter {
    fn new(tx: mpsc::Sender<PutPathRequest>, scanner: RefScanSink) -> Self {
        Self {
            inner: HashingChannelWriter::new(tx),
            scanner,
        }
    }

    /// Finalize: send trailer, return (hash, size, found_references).
    fn finalize(self) -> ([u8; 32], u64, std::collections::HashSet<[u8; 32]>) {
        let (hash, size) = self.inner.finalize();
        (hash, size, self.scanner.into_found())
    }
}

impl Write for ScanningHashingWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        // Scanner first: its write() is infallible (always Ok(len)),
        // so it can't short-circuit the real write.
        let _ = self.scanner.write(data);
        self.inner.write(data)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
```

### 2.2 Signature changes: `do_upload_streaming` → `upload_output` → `upload_all_outputs`

**`do_upload_streaming`** (currently `upload.rs:204-301`):

```rust
// BEFORE: line 204
async fn do_upload_streaming(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    output_path: PathBuf,
    assignment_token: &str,
) -> Result<([u8; 32], u64), tonic::Status>

// AFTER:
async fn do_upload_streaming(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    output_path: PathBuf,
    assignment_token: &str,
    deriver: &str,                                   // NEW — drv_path string
    candidates: &rio_nix::refscan::CandidateSet,     // NEW — hash parts to scan for
) -> Result<([u8; 32], u64, Vec<String>), tonic::Status>
//                          ^^^^^^^^^^^ NEW — found references (full paths, sorted)
```

Inside the body, the `PathInfo` construction at **`upload.rs:218-229`** changes:

```rust
// BEFORE (lines 218-229):
let info = PathInfo {
    store_path: store_path.to_string(),
    nar_hash: Vec::new(),
    nar_size: 0,
    store_path_hash: Vec::new(),
    deriver: String::new(),       // ← line 223, bug
    references: Vec::new(),       // ← line 224, bug
    registration_time: 0,
    ultimate: false,
    signatures: Vec::new(),
    content_address: String::new(),
};

// AFTER:
// references are scanned DURING the stream and sent in the trailer-
// adjacent metadata — but PathInfo is the FIRST message. We have a
// chicken-and-egg problem.
//
// RESOLUTION: references stay in PathInfo (first message). We can't
// know them until the dump finishes. Two options:
//   (a) Buffer PathInfo send until after dump → breaks trailer-mode
//       protocol (metadata MUST be message 0).
//   (b) Send refs in a new trailer field → proto change, store-side
//       plumbing.
//   (c) Pre-scan: dump to a RefScanSink ONLY (no hash, no channel)
//       before the real dump. Two disk reads.
//
// We pick (c). Rationale:
//   - Proto/store changes (b) ripple into put_path.rs, ValidatedPathInfo,
//     metadata.rs, and the re-sign path. Scope creep for a P0 fix.
//   - The pre-scan is disk-read only (no hash, no network, no
//     compression). On NVMe this is ~GB/s. The scanner itself is
//     ~memcpy speed (Boyer-Moore skips ~31/32 of bytes in binary
//     sections). A 4 GiB output adds ~4s wall time.
//   - The retry loop ALREADY re-reads from disk on each attempt
//     (upload.rs:112-116). Adding one more read before the first
//     attempt is within the existing cost model.
//   - If perf becomes an issue, option (b) is the escape hatch.
//     TODO(phase4b): trailer-refs protocol extension.

// [Pre-scan moved OUTSIDE do_upload_streaming — see upload_output below.
//  References are computed once, passed into every retry attempt.]

let info = PathInfo {
    store_path: store_path.to_string(),
    nar_hash: Vec::new(),
    nar_size: 0,
    store_path_hash: Vec::new(),
    deriver: deriver.to_string(),               // ← FILLED
    references: references.clone(),             // ← FILLED (param, see below)
    registration_time: 0,
    ultimate: false,
    signatures: Vec::new(),
    content_address: String::new(),
};
```

**Actually simpler**: do the pre-scan in `upload_output` (outside the retry loop) so retries don't re-scan. Pass `references: Vec<String>` into `do_upload_streaming`:

```rust
// upload_output (upload.rs:118-195) — additions

#[instrument(skip_all, fields(store_path = %format!("/nix/store/{output_basename}")))]
async fn upload_output(
    store_client: &mut StoreServiceClient<Channel>,
    upper_dir: &Path,
    output_basename: &str,
    assignment_token: &str,
    deriver: &str,                                   // NEW
    candidates: &rio_nix::refscan::CandidateSet,     // NEW
) -> Result<UploadResult, UploadError> {
    let output_path = upper_dir.join("nix/store").join(output_basename);
    let store_path = format!("/nix/store/{output_basename}");

    // ... existing store_path validation ...

    // --- NEW: pre-scan for references --------------------------------
    // r[impl worker.upload.references-scanned]
    // Single disk read through RefScanSink. spawn_blocking because
    // dump_path_streaming is sync I/O.
    let references = {
        let output_path = output_path.clone();
        let scanner = RefScanSink::new(candidates.hashes());
        let candidates = candidates.clone(); // Arc it if CandidateSet is big
        tokio::task::spawn_blocking(move || {
            let mut sink = scanner;
            nar::dump_path_streaming(&output_path, &mut sink)
                .map(|_| candidates.resolve(&sink.into_found()))
        })
        .await
        .map_err(|e| UploadError::UploadExhausted {
            path: store_path.clone(),
            source: tonic::Status::internal(format!("ref-scan task panicked: {e}")),
        })?
        .map_err(|e| UploadError::UploadExhausted {
            path: store_path.clone(),
            source: nar_err_to_status(&output_path, e),
        })?
    };

    tracing::info!(
        store_path = %store_path,
        ref_count = references.len(),
        "scanned references"
    );
    metrics::histogram!("rio_worker_upload_references_count")
        .record(references.len() as f64);
    // -----------------------------------------------------------------

    tracing::info!(store_path = %store_path, "uploading output (streaming tee)");

    let mut last_error = None;
    for attempt in 0..MAX_UPLOAD_RETRIES {
        // ... existing backoff ...
        match do_upload_streaming(
            store_client,
            &store_path,
            output_path.clone(),
            assignment_token,
            deriver,           // NEW
            &references,       // NEW — already computed, no re-scan per retry
        )
        .await
        { /* ... unchanged ... */ }
    }
    // ...
}
```

And `do_upload_streaming`'s signature becomes:

```rust
async fn do_upload_streaming(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
    output_path: PathBuf,
    assignment_token: &str,
    deriver: &str,           // NEW
    references: &[String],   // NEW — already-scanned, just fill PathInfo
) -> Result<([u8; 32], u64), tonic::Status>
```

No `ScanningHashingWriter` needed after all — the pre-scan is a separate pass. Simpler. Drop §2.1 from the implementation PR (keep it in this doc as the considered-and-rejected alternative).

**`upload_all_outputs`** (`upload.rs:425-454`):

```rust
// BEFORE: line 425
pub async fn upload_all_outputs(
    store_client: &StoreServiceClient<Channel>,
    upper_dir: &Path,
    assignment_token: &str,
) -> Result<Vec<UploadResult>, UploadError>

// AFTER:
pub async fn upload_all_outputs(
    store_client: &StoreServiceClient<Channel>,
    upper_dir: &Path,
    assignment_token: &str,
    deriver: &str,                           // NEW — the .drv path
    ref_candidates: &[String],               // NEW — resolved_input_srcs ∪ output paths
) -> Result<Vec<UploadResult>, UploadError> {
    let outputs = scan_new_outputs(upper_dir)?;

    // Build the candidate set ONCE. Shared across all output uploads
    // (same input closure for all outputs of a derivation). Wrap in
    // Arc so buffer_unordered clones cheaply.
    let candidates = std::sync::Arc::new(
        rio_nix::refscan::CandidateSet::from_paths(ref_candidates.iter())
    );

    // ... existing buffer_unordered, with `deriver` + `candidates.clone()`
    //     passed into each upload_output call ...
}
```

### 2.3 Executor call site

**`rio-worker/src/executor/mod.rs:648-656`** — the `upload_all_outputs` call. The candidate set is `resolved_input_srcs ∪ drv.outputs()`. Both are already in scope:

- `resolved_input_srcs` — built at `mod.rs:327-354`, still live (it was cloned into `basic_drv` at :357 but the original `Vec<String>` is still around — verify with a compile; if moved, re-clone at :357).
- `drv.outputs()` — the `drv: Derivation` is still live (used at :669 for the output-name map).
- `drv_path: &String` — live throughout (it's `&assignment.drv_path`, see :239).

```rust
// executor/mod.rs:648 — AFTER
// Candidate set for reference scanning:
//   - resolved_input_srcs: every direct input (static + resolved inputDrv
//     outputs). Transitive inputs are NOT scanned for — Nix itself only
//     scans the direct input closure. If an output embeds a transitive
//     path that's not a direct input, that's a derivation bug (missing
//     explicit dep), and Nix would also miss it.
//   - drv.outputs(): self-references and cross-output references are
//     legal (e.g., -dev output referencing the lib output's rpath).
let mut ref_candidates: Vec<String> = resolved_input_srcs.iter().cloned().collect();
ref_candidates.extend(drv.outputs().iter().map(|o| o.path().to_string()));

match upload::upload_all_outputs(
    store_client,
    overlay_mount.upper_dir(),
    &assignment.assignment_token,
    drv_path,           // NEW — deriver
    &ref_candidates,    // NEW
)
.await
```

**Also add `references` to `UploadResult`** (`upload.rs:44-54`) so the executor can log per-output ref counts — useful for the `r[verify ...]` integration test assertion:

```rust
pub struct UploadResult {
    pub store_path: String,
    pub nar_hash: [u8; 32],
    pub nar_size: u64,
    pub references: Vec<String>,  // NEW — sorted, full paths
}
```

---

## 3. Store-side defense

### 3.1 Warn on signing empty-ref non-CA path

**`rio-store/src/grpc/mod.rs:204-224`** — `maybe_sign()`. `ValidatedPathInfo` has `content_address: Option<String>` (from `validated.rs:57`); `None` means non-CA.

```rust
// grpc/mod.rs — inside maybe_sign, AFTER `let Some(signer) = ...`

// Defensive: a non-CA path with zero references is almost certainly
// a worker that didn't scan (pre-fix) or a scanning bug. CA paths
// legitimately have empty refs (fetchurl, etc.). Don't block the
// upload — just make noise so it's visible in logs/alerts.
// r[impl store.signing.empty-refs-warn]
if info.content_address.is_none() && info.references.is_empty() {
    warn!(
        store_path = %info.store_path.as_str(),
        "signing non-CA path with empty references — GC will not protect deps; \
         check worker ref-scanner"
    );
    metrics::counter!("rio_store_sign_empty_refs_total").increment(1);
}
```

### 3.2 GC safety gate: refuse sweep on high empty-ref ratio

**`rio-store/src/grpc/admin.rs`** — inside `trigger_gc`, **after** the advisory lock is acquired (past :106) and **before** the mark phase starts. This is the last line of defense: if the store is full of empty-ref narinfo (pre-fix data) and someone triggers GC, we refuse rather than sweep everything.

```rust
// grpc/admin.rs — new helper + call in trigger_gc

/// Safety gate: if more than `threshold_pct`% of COMPLETE narinfo rows
/// older than `grace_hours` have empty references AND no content
/// address, refuse GC. Protects against running GC on pre-refscan data.
///
/// CA paths are excluded from the count (legitimately ref-free).
/// Paths inside the grace window are excluded (they're protected
/// anyway; their ref-state doesn't matter for this sweep).
// r[impl store.gc.empty-refs-gate]
async fn check_empty_refs_gate(
    pool: &PgPool,
    grace_hours: u32,
    threshold_pct: f64,
) -> Result<(), Status> {
    let row: (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            count(*) FILTER (
                WHERE n."references" = '{}'
                  AND (n.content_address IS NULL OR n.content_address = '')
            ) AS empty_ref_non_ca,
            count(*) AS total
        FROM narinfo n
        JOIN manifests m USING (store_path_hash)
        WHERE m.status = 'complete'
          AND n.created_at < now() - make_interval(hours => $1::int)
        "#,
    )
    .bind(grace_hours as i32)
    .fetch_one(pool)
    .await
    .map_err(|e| Status::internal(format!("empty-refs gate query failed: {e}")))?;

    let (empty, total) = row;
    if total == 0 {
        return Ok(()); // nothing to sweep anyway
    }
    let pct = (empty as f64 / total as f64) * 100.0;
    metrics::gauge!("rio_store_gc_empty_refs_pct").set(pct);

    if pct > threshold_pct {
        error!(
            empty_ref_non_ca = empty,
            total,
            pct,
            threshold_pct,
            "GC REFUSED: {pct:.1}% of sweep-eligible narinfo have empty refs — \
             store likely contains pre-refscan data. Run admin ResignPaths \
             backfill before enabling GC. Override with --force-gc-unsafe."
        );
        return Err(Status::failed_precondition(format!(
            "GC safety gate: {empty}/{total} ({pct:.1}%) of sweep-eligible paths \
             have empty references (threshold: {threshold_pct}%). \
             Run backfill first or pass force=true."
        )));
    }

    if empty > 0 {
        warn!(
            empty_ref_non_ca = empty,
            total, pct, "GC proceeding with some empty-ref paths (below threshold)"
        );
    }
    Ok(())
}
```

**Call site** in `trigger_gc`, after lock acquisition and before `compute_unreachable`:

```rust
// Default threshold 10%. Overridable via GcRequest.force (new proto field).
// 10% is intentionally low: in a healthy post-fix store, empty-ref
// non-CA paths should be ~0% (only genuinely ref-free outputs like
// static binaries). 10% gives headroom for legitimate cases without
// allowing a pre-fix store (where it'd be ~100%) through.
const GC_EMPTY_REFS_THRESHOLD_PCT: f64 = 10.0;

if !req.force {
    if let Err(e) = check_empty_refs_gate(&pool, grace_hours, GC_EMPTY_REFS_THRESHOLD_PCT).await {
        let _ = tx.send(Err(e)).await;
        return;
    }
}
```

**Proto change** (`rio-proto/proto/store.proto`):

```proto
message GcRequest {
  optional uint32 grace_period_hours = 1;
  bool dry_run = 2;
  repeated string extra_roots = 3;
  // Bypass the empty-refs safety gate. Use ONLY if you've verified
  // the empty-ref paths are genuinely ref-free (static binaries,
  // fetchurl outputs with missing CA field, etc.).
  bool force = 4;  // NEW
}
```

---

## 4. Migration: re-sign sweep

### 4.1 Identifying affected paths

Pre-fix paths have `references = '{}'` AND `(content_address IS NULL OR = '')`. But that's not a perfect discriminator — a genuinely ref-free output (pure static binary) also matches. We can't distinguish "never scanned" from "scanned, found nothing" after the fact.

**Solution:** a flag column set on all rows that existed before the scanner shipped.

**`migrations/010_refscan_backfill.sql`:**

```sql
-- Remediation 02: mark all narinfo rows that predate the reference
-- scanner. These need their NARs re-scanned and (potentially) re-signed.
--
-- NULL  = post-fix row, never needs backfill
-- false = pre-fix row, backfill not yet done
-- true  = pre-fix row, backfill done (refs re-scanned, sig verified)
--
-- The backfill job (ResignPaths admin RPC) flips false→true. Once
-- `SELECT count(*) WHERE refs_backfilled = false` is 0, this column
-- can be dropped in a follow-up migration.
ALTER TABLE narinfo ADD COLUMN refs_backfilled BOOLEAN DEFAULT NULL;

-- Mark every EXISTING row. DEFAULT NULL means new rows (post-fix
-- uploads) won't get the flag — they don't need backfill.
UPDATE narinfo SET refs_backfilled = false;

-- Index for the backfill job's `WHERE refs_backfilled = false` scan.
-- Partial index: only the false rows (shrinks to zero as backfill
-- proceeds, then drop with the column).
CREATE INDEX idx_narinfo_refs_backfill_pending
    ON narinfo (store_path_hash)
    WHERE refs_backfilled = false;
```

**SQL to identify affected paths** (for the admin RPC's batch loader):

```sql
-- Batch fetch: next N paths needing backfill. store_path_hash is the
-- PK; `> $1` cursor for pagination (no OFFSET — OFFSET is O(N) scan).
SELECT n.store_path_hash, n.store_path, n.nar_hash, n.nar_size,
       n."references", n.signatures, n.content_address
FROM narinfo n
JOIN manifests m USING (store_path_hash)
WHERE n.refs_backfilled = false
  AND m.status = 'complete'
  AND n.store_path_hash > $1       -- cursor
ORDER BY n.store_path_hash
LIMIT $2;
```

### 4.2 Re-sign admin RPC

**New RPC** in `StoreAdminService` (`rio-proto/proto/store.proto`):

```proto
service StoreAdminService {
  // ... existing TriggerGC, PinPath, UnpinPath ...

  // Re-scan references for paths uploaded before the ref-scanner fix.
  // Streams progress. For each path:
  //   1. Fetch NAR bytes from chunk store
  //   2. Scan for references (against the path's existing deriver's
  //      input closure — or, if deriver is also empty, against ALL
  //      store paths as candidates; slower but correct)
  //   3. If refs changed: update narinfo, re-compute fingerprint,
  //      re-sign, update signatures
  //   4. Set refs_backfilled = true
  //
  // Idempotent. Safe to cancel and resume (cursor-based pagination).
  // Does NOT delete old signatures — appends new ones (Nix accepts
  // any valid signature from a trusted key; old sig for empty refs
  // won't verify against new refs, but it's harmless noise).
  rpc ResignPaths(ResignPathsRequest) returns (stream ResignPathsProgress);
}

message ResignPathsRequest {
  // Batch size for the cursor scan. Default 100.
  optional uint32 batch_size = 1;
  // If true, only re-scan paths whose references are CURRENTLY empty.
  // Faster (skips paths that somehow got refs another way) but misses
  // paths where the scanner would find MORE refs than currently stored.
  // Default: false (scan all flagged paths).
  bool only_empty = 2;
  // Dry run: compute what WOULD change, don't write.
  bool dry_run = 3;
}

message ResignPathsProgress {
  uint64 scanned = 1;
  uint64 refs_changed = 2;
  uint64 resigned = 3;
  uint64 remaining = 4;       // count(*) WHERE refs_backfilled = false
  string current_path = 5;    // for operator tail
}
```

**Implementation sketch** (`rio-store/src/grpc/admin.rs`):

```rust
#[instrument(skip(self, request), fields(rpc = "ResignPaths"))]
async fn resign_paths(
    &self,
    request: Request<ResignPathsRequest>,
) -> Result<Response<Self::ResignPathsStream>, Status> {
    let req = request.into_inner();
    let batch_size = req.batch_size.unwrap_or(100).min(1000) as i64;
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let pool = self.pool.clone();
    let chunk_backend = self.chunk_backend.clone()
        .ok_or_else(|| Status::failed_precondition(
            "ResignPaths requires a chunk backend (NAR bytes must be fetchable)"
        ))?;
    let signer = self.signer.clone();

    tokio::spawn(async move {
        let mut cursor: Vec<u8> = Vec::new();
        let mut scanned = 0u64;
        let mut refs_changed = 0u64;
        let mut resigned = 0u64;

        loop {
            // Fetch next batch (SQL from §4.1).
            let batch = match fetch_backfill_batch(&pool, &cursor, batch_size).await {
                Ok(b) if b.is_empty() => break,
                Ok(b) => b,
                Err(e) => { let _ = tx.send(Err(e)).await; return; }
            };

            for row in &batch {
                // 1. Reassemble NAR from chunk store.
                //    (Reuse get_path's chunk-assembly logic — factor out
                //    `assemble_nar(pool, chunk_backend, store_path_hash) -> impl AsyncRead`.)
                // 2. Candidate set: if deriver is set, fetch its .drv,
                //    parse, use input_srcs ∪ outputs. If deriver is
                //    empty (also a pre-fix bug), fall back to "all
                //    paths in store" — slow (O(N) candidate set) but
                //    correct, and one-time.
                // 3. Stream NAR through RefScanSink.
                // 4. Compare new refs vs row.references.
                // 5. If changed AND NOT dry_run:
                //    - UPDATE narinfo SET "references" = $new WHERE store_path_hash = $h
                //    - If signer.is_some():
                //        new_fp = narinfo::fingerprint(path, nar_hash, nar_size, &new_refs)
                //        new_sig = signer.sign(&new_fp)
                //        UPDATE narinfo SET signatures = array_append(signatures, $new_sig)
                //                       WHERE store_path_hash = $h
                //    - UPDATE narinfo SET refs_backfilled = true WHERE store_path_hash = $h
                //    (All three in one transaction per path.)
                scanned += 1;
            }

            cursor = batch.last().unwrap().store_path_hash.clone();
            let remaining = count_backfill_pending(&pool).await.unwrap_or(0);
            let _ = tx.send(Ok(ResignPathsProgress {
                scanned, refs_changed, resigned, remaining,
                current_path: batch.last().unwrap().store_path.clone(),
            })).await;
        }
    });

    Ok(Response::new(ReceiverStream::new(rx)))
}
```

### 4.3 Why admin RPC, not a one-time script

- **Reuses existing infra.** The chunk-store NAR reassembly already exists (`get_path.rs`). The signer is already wired into `StoreServiceImpl`. A standalone script would need to re-plumb both (DB connection, chunk backend config, signing key path).
- **Streamed progress.** For a large store (100k+ paths), this takes hours. `kubectl port-forward` + `grpcurl` gives a live progress feed; a script would need its own progress reporting.
- **Resumable.** Cursor-based, idempotent. If the pod restarts mid-backfill, re-run the RPC and it picks up where `refs_backfilled = false` left off.
- **Same security boundary.** Admin RPCs are already mTLS-gated to the operator. A script with raw DB + signing key access is a wider attack surface.

---

## 5. Tests

### 5.1 Unit tests (rio-nix)

See [§1.5](#15-unit-tests) — 8 unit tests + 1 proptest + 1 fuzz target for `RefScanSink`.

### 5.2 Unit tests (rio-worker)

Extend the existing `MockStore` tests in `upload.rs` (`tests` mod at :458):

```rust
// r[verify worker.upload.references-scanned]
#[tokio::test]
async fn test_upload_output_scans_references() -> anyhow::Result<()> {
    let (store, mut client, _h) = spawn_mock_store_with_client().await?;

    // Output file contains a reference to a known dep.
    let dep_basename = test_store_basename("glibc-2.38");
    let dep_path = format!("/nix/store/{dep_basename}");
    let out_basename = test_store_basename("hello");
    let content = format!("RPATH={dep_path}/lib\n");
    let tmp = make_output_file(&out_basename, content.as_bytes())?;

    let candidates = rio_nix::refscan::CandidateSet::from_paths(
        [dep_path.clone(), format!("/nix/store/{out_basename}")]
    );
    let result = upload_output(
        &mut client, tmp.path(), &out_basename, "",
        "/nix/store/fake-deriver.drv", &candidates,
    ).await?;

    // Scanner found the dep.
    assert_eq!(result.references, vec![dep_path.clone()]);

    // MockStore recorded it in PathInfo.
    let puts = store.put_calls.read().unwrap();
    assert_eq!(puts[0].references, vec![dep_path]);
    assert_eq!(puts[0].deriver, "/nix/store/fake-deriver.drv");
    Ok(())
}

#[tokio::test]
async fn test_upload_output_self_reference() -> anyhow::Result<()> {
    // Output contains its own path (shebang/rpath). Must be detected.
    let (store, mut client, _h) = spawn_mock_store_with_client().await?;
    let out_basename = test_store_basename("self-ref");
    let out_path = format!("/nix/store/{out_basename}");
    let content = format!("#!/{out_path}/bin/sh\n");
    let tmp = make_output_file(&out_basename, content.as_bytes())?;

    let candidates = rio_nix::refscan::CandidateSet::from_paths([out_path.clone()]);
    let result = upload_output(
        &mut client, tmp.path(), &out_basename, "", "", &candidates,
    ).await?;

    assert_eq!(result.references, vec![out_path]);
    Ok(())
}

#[tokio::test]
async fn test_upload_output_no_false_positives() -> anyhow::Result<()> {
    // Output contains a 32-char base32 sequence NOT in candidates.
    let (_store, mut client, _h) = spawn_mock_store_with_client().await?;
    let out_basename = test_store_basename("clean");
    // Valid base32 but not a real path.
    let tmp = make_output_file(&out_basename, b"decoy: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n")?;

    let dep = format!("/nix/store/{}", test_store_basename("real-dep"));
    let candidates = rio_nix::refscan::CandidateSet::from_paths([dep]);
    let result = upload_output(
        &mut client, tmp.path(), &out_basename, "", "", &candidates,
    ).await?;

    assert!(result.references.is_empty());
    Ok(())
}
```

### 5.3 Integration test (VM)

New fragment in `nix/tests/vm/fragments/` — two-stage build where stage B depends on stage A:

```nix
# fragments/refscan.nix — worker-scanned references land in store PG
{
  testScript = ''
    # Stage A: builds a shared lib. Stage B: links against it.
    # After B's upload, query narinfo for B's output → references
    # MUST contain A's output path.

    # ... submit build of stage B (which pulls in stage A) ...
    machine.wait_until_succeeds(
        "psql -U rio -d rio -tAc "
        "\"SELECT array_length(\\\"references\\\", 1) FROM narinfo "
        "  WHERE store_path = '${stageBOut}'\" | grep -qE '^[1-9]'"
    )

    # Deriver is also populated.
    machine.succeed(
        "psql -U rio -d rio -tAc "
        "\"SELECT deriver FROM narinfo WHERE store_path = '${stageBOut}'\""
        " | grep -q '${stageBDrv}'"
    )

    # GC mark correctness: pin B, run GC, A survives.
    machine.succeed("grpcurl ... PinPath ${stageBOut}")
    # Wait past grace, trigger GC, assert A still in narinfo.
    # (Use grace_period_hours=0 + a sleep to avoid waiting 2h.)
    machine.succeed("grpcurl ... TriggerGC grace_period_hours=0")
    machine.succeed(
        "psql -U rio -d rio -tAc "
        "\"SELECT 1 FROM narinfo WHERE store_path = '${stageAOut}'\""
    )
  '';
}
```

### 5.4 Store-side tests

```rust
// rio-store/src/grpc/admin.rs tests

// r[verify store.gc.empty-refs-gate]
#[tokio::test]
async fn gc_refuses_on_high_empty_ref_ratio() {
    let db = TestDb::new(&crate::MIGRATOR).await;
    // Seed 10 paths, 9 with empty refs + no CA, all past grace.
    for i in 0..9 {
        seed_path(&db.pool, &test_store_path(&format!("empty-{i}")), &[], 48).await;
    }
    seed_path(&db.pool, &test_store_path("ok"), &["/nix/store/x-dep"], 48).await;

    let err = check_empty_refs_gate(&db.pool, 2, 10.0).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("90.0%"));
}

#[tokio::test]
async fn gc_gate_ignores_ca_paths() {
    // CA paths with empty refs don't count toward the threshold.
    // ... seed 10 CA paths with empty refs, 1 non-CA with refs ...
    // ... assert gate passes ...
}

#[tokio::test]
async fn gc_gate_ignores_grace_window() {
    // Recent paths (inside grace) don't count — their ref state is
    // irrelevant to THIS sweep.
    // ... seed 10 empty-ref paths at created_at=now, grace=2h ...
    // ... assert gate passes (zero eligible paths) ...
}
```

### 5.5 Tracey markers

Tracey markers: `r[worker.upload.references-scanned]`, `r[worker.upload.deriver-populated]` — see [`worker.md`](../../components/worker.md) (replaces the former "Phase deferral" paragraph). The first covers the pre-upload NAR reference scan (Boyer-Moore-style skip-scan over 32-char nixbase32 prefixes against `resolved_input_srcs ∪ drv.outputs()`, populating `PathInfo.references` as a separate disk-read-only pre-pass before the upload retry loop); the second covers `PathInfo.deriver` being set to `assignment.drv_path` plumbed through `upload_all_outputs`.

Tracey markers: `r[store.signing.empty-refs-warn]`, `r[store.gc.empty-refs-gate]` — see [`store.md`](../../components/store.md). The first covers the `warn!` log + `rio_store_sign_empty_refs_total` counter when signing a non-CA path with empty `references` (detection aid, not enforcement); the second covers the `TriggerGC` pre-mark gate that refuses GC with `FailedPrecondition` if >10% of sweep-eligible rows have empty `references` AND empty `content_address` (unless `force=true`).

| Rule ID | `r[impl]` site | `r[verify]` site |
|---|---|---|
| `worker.upload.references-scanned` | `rio-worker/src/upload.rs` (`upload_output` pre-scan block) | `upload.rs::tests::test_upload_output_scans_references` + VM `refscan.nix` |
| `worker.upload.deriver-populated` | `rio-worker/src/upload.rs` (`PathInfo.deriver = deriver`) | `upload.rs::tests::test_upload_output_scans_references` (checks `puts[0].deriver`) |
| `store.signing.empty-refs-warn` | `rio-store/src/grpc/mod.rs` (`maybe_sign`) | log-assertion test in `grpc/mod.rs` tests |
| `store.gc.empty-refs-gate` | `rio-store/src/grpc/admin.rs` (`check_empty_refs_gate`) | `admin.rs::tests::gc_refuses_on_high_empty_ref_ratio` |

---

## 6. Sequencing & deployment gate

### 6.1 PR breakdown

| PR | Contents | Gates on | Green check |
|---|---|---|---|
| **PR-A** | `rio-nix/src/refscan.rs` + unit tests + fuzz target | — | `cargo nextest run -p rio-nix` + 2min fuzz |
| **PR-B** | `upload.rs` plumbing + `executor/mod.rs` call site + `UploadResult.references` + MockStore tests | PR-A merged | `cargo nextest run -p rio-worker` |
| **PR-C** | `store/grpc/mod.rs` warn + `store/grpc/admin.rs` gate + proto `GcRequest.force` + tests | — (orthogonal) | `cargo nextest run -p rio-store` |
| **PR-D** | Migration 010 + `ResignPaths` RPC + proto | PR-A merged (needs `RefScanSink` in store) | `cargo nextest run -p rio-store` + migration test |
| **PR-E** | VM test fragment `refscan.nix` | PR-B + PR-C merged | `nix-build-remote -- .#checks.x86_64-linux.vm-refscan` |
| **PR-F** | Spec updates (`worker.md` :190 rewrite, new `r[...]` markers) | PR-B merged (else `tracey uncovered` fails) | `nix build .#checks.x86_64-linux.tracey-validate` |

PR-A/B and PR-C are independent — can land in either order. PR-C is the **defensive** half; ship it first if there's any risk of GC being triggered on current data before PR-B ships.

### 6.2 Deployment gate

**HARD GATE: no GC on production stores until the backfill is complete.**

Pre-fix stores have ~100% empty-ref narinfo (every rio-built path). The first `TriggerGC` past `grace_hours` sweeps everything not directly pinned. The §3.2 gate (PR-C) blocks this at the RPC boundary, but belt-and-suspenders:

**Operator checklist for enabling GC on a cluster:**

```
[ ] 1. PR-C deployed (rio-store has the gate). Verify:
       grpcurl ... TriggerGC dry_run=true
       → expect FailedPrecondition if store has pre-fix data.

[ ] 2. PR-B deployed (rio-worker scans refs). Verify NEW uploads have refs:
       psql: SELECT store_path, array_length("references",1)
             FROM narinfo ORDER BY created_at DESC LIMIT 5;
       → expect non-zero for recent non-CA paths.

[ ] 3. Migration 010 applied. Verify:
       psql: SELECT count(*) FROM narinfo WHERE refs_backfilled = false;
       → expect >0 (the backfill queue).

[ ] 4. Run ResignPaths. Monitor:
       grpcurl ... ResignPaths → stream progress until remaining=0.
       Alert on rio_store_sign_empty_refs_total rate — should flatline
       after backfill (new uploads scan correctly; old paths backfilled).

[ ] 5. Verify gate passes:
       grpcurl ... TriggerGC dry_run=true
       → expect success (or FailedPrecondition with <10%).

[ ] 6. Dry-run GC, sanity-check the sweep set:
       grpcurl ... TriggerGC dry_run=true grace_period_hours=2
       → eyeball the "would delete N paths" number. If N ≈ total store
       size, STOP — something is still wrong.

[ ] 7. Enable GC. First run with conservative grace_period_hours=168
       (1 week) to limit blast radius if anything was missed.
```

**Helm values gate:** add `gc.enabled: false` as the default in the store chart. Flipping to `true` is the explicit operator action after the checklist above. The periodic GC cronjob checks this flag before calling `TriggerGC`.

### 6.3 Rollback plan

If post-deploy GC sweeps something it shouldn't have:

1. **Immediately:** `gc.enabled: false` in Helm; rollout.
2. The `pending_s3_deletes` table (existing) is the sweep queue — S3 objects are not deleted until the drain job runs. **Pause the drain job.** Rows in `pending_s3_deletes` can be re-enqueued into `manifests` + `narinfo` from S3 (the chunks are still there).
3. The narinfo/manifest PG rows ARE gone (CASCADE on sweep). Recovery requires re-deriving from `pending_s3_deletes` + chunk store — build a one-shot recovery script if this happens. **The drain pause in step 2 is the critical action**; everything else is recoverable as long as S3 objects survive.

---

## 7. Open questions

- **Transitive refs in candidate set?** Current plan uses direct inputs only (`resolved_input_srcs`), matching Nix. The full input *closure* is already computed at `executor/mod.rs:379-388` for the synth DB — using it as the candidate set would catch derivations with missing explicit deps (at the cost of a larger HashSet). Defer: start with direct-only (Nix parity), add a metric for "output path found in closure but not in direct inputs" to gauge how common the broader scan would be useful.
- **Scanner in store for real-time?** The store could re-scan on `PutPath` as a belt-and-suspenders check against a buggy worker. Deferred — adds latency to every upload; the `maybe_sign` warn is sufficient detection.
- **Delete old invalid signatures?** `ResignPaths` appends the new sig; the old one (commits to `refs=""`) is now invalid but harmless. Leaving it avoids a `signatures = array_remove(...)` footgun (what if a path has TWO sigs from different keys?). Clean up in a follow-up migration once backfill is confirmed complete.

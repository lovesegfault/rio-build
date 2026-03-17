# Remediation 17: rio-nix wire-parser fixes (omnibus)

**Parent findings:** [§2.11](../phase4a.md#211-make_text-unsorted-refs), [§2.12](../phase4a.md#212-nar-parser-unbounded-recursion), [§5 FOD modular hash](../phase4a.md#fod-modular-hash-critical--low)
**Severity:** P1 (make_text, NAR depth, base32 padding) + P3 (proptest coverage) + LOW (FOD fingerprint)
**Findings:** `nix-make-text-unsorted-refs`, `nix-nar-unbounded-recursion`, `nix-nixbase32-decode-silent-overflow`, `nix-fod-hash-modulo-omits-outpath`, `nix-aterm-modulo-key-collision`, `nix-proptest-escapes`, `nix-hash-no-proptest`
**Commit shape:** ONE commit. All six are `rio-nix` crate-local, no cross-crate ripple, independently testable. The FOD fingerprint change breaks 3 unit-test vectors that must be updated in the same commit or the tree is red.
**Effort:** ~2 h implementation + `cargo nextest run -p rio-nix` + `cargo fuzz run nar_parsing -- -runs=100000`

---

## Ground truth vs. §2.11/§2.12 pseudo-code

§2.11's pseudo-code shows `h.update(r.as_bytes())`. The actual code builds a `String` and hashes the whole fingerprint at once (`Self::from_fingerprint`). Same bug, different shape — the diff below targets the real code.

§2.12's pseudo-code names the function `parse_entry`. The actual recursion is `parse_node` → `parse_directory` → `parse_node`. `parse_entry` doesn't exist. The depth counter threads through both.

§5's file reference `rio-nix/src/hash.rs:69` is stale — the code moved to `rio-nix/src/derivation/hash.rs:69` during the phase-3b crate split. Same line number by coincidence.

---

## 1. `make_text` — sort references before fingerprint

**File:** `rio-nix/src/store_path.rs:217-221`

### Bug

Nix's `makeTextPath` (libstore/store-api.cc) takes a `StorePathSet` (`std::set`, sorted). rio iterates `references: &[StorePath]` in caller order. The caller is `opcodes_write.rs:279,377` → `parse_reference_paths` → `Vec<StorePath>` preserving wire order. The wire protocol does **not** mandate sorted references.

If a client sends `["/nix/store/b...-ref", "/nix/store/a...-ref"]`, rio hashes `text:/nix/store/b...:/nix/store/a...:` while Nix hashes `text:/nix/store/a...:/nix/store/b...:` → different SHA-256 → different store path → client's `nix store verify` fails.

`narinfo.rs:280-281` already does this correctly with a `sort_unstable` + comment calling out the `BTreeSet` requirement. Copy that pattern.

### Diff

```diff
--- a/rio-nix/src/store_path.rs
+++ b/rio-nix/src/store_path.rs
@@ -214,10 +214,15 @@ impl StorePath {
         hash: &crate::hash::NixHash,
         references: &[StorePath],
     ) -> Result<Self, StorePathError> {
+        // Nix uses StorePathSet (a BTreeSet), so references are always
+        // iterated in sorted order. The wire protocol does not mandate
+        // sorted references from the client, so we must sort here.
+        // narinfo.rs fingerprint() does the same thing for the same reason.
+        let mut sorted: Vec<&str> = references.iter().map(|r| r.as_str()).collect();
+        sorted.sort_unstable();
+
         let mut type_str = "text".to_string();
-        for r in references {
+        for r in sorted {
             type_str.push(':');
-            type_str.push_str(r.as_str());
+            type_str.push_str(r);
         }
```

### Test

Replace `test_make_text_with_references` (store_path.rs:597) — it currently passes `[r1, r2]` in already-sorted order, which is precisely the input that hides this bug.

```rust
/// make_text must sort references before hashing. Nix uses a StorePathSet
/// (BTreeSet), so iteration order is always sorted; the wire protocol does
/// not mandate sorted order from the client.
#[test]
fn test_make_text_sorts_references() -> anyhow::Result<()> {
    let r_a = StorePath::parse("/nix/store/00000000000000000000000000000000-a-ref")?;
    let r_b = StorePath::parse("/nix/store/11111111111111111111111111111111-b-ref")?;
    let hash = crate::hash::NixHash::new(crate::hash::HashAlgo::SHA256, vec![0u8; 32])?;

    // Same reference set, two orderings. Must produce identical path.
    let p_sorted = StorePath::make_text("mytext", &hash, &[r_a.clone(), r_b.clone()])?;
    let p_rev    = StorePath::make_text("mytext", &hash, &[r_b, r_a])?;

    assert_eq!(p_sorted, p_rev, "make_text must be insensitive to caller ref ordering");
    assert_eq!(p_sorted.name(), "mytext");
    Ok(())
}
```

**Golden follow-up (separate commit, not blocking):** `rio-gateway/tests/golden/daemon.rs:485` exercises `builtins.toFile` with **zero** references. Add a golden variant with two refs in intentionally-reversed order and assert rio's computed path matches the real daemon's. This is the only test that proves conformance — the unit test above proves self-consistency only.

---

## 2. NAR parser — bounded recursion depth

**File:** `rio-nix/src/nar.rs:214,267,306`

### Bug

`parse_node` → `parse_directory` → `parse_node` recurses once per directory level with no depth counter. Each level is ~88 bytes of NAR framing on the wire:

```text
"entry" (16)  "(" (16)  "name" (16)  "a" (16)  "node" (16)
"(" (16)  "type" (16)  "directory" (24)           = 136 bytes payload-inclusive
```

But each level's stack frame is ~200-400 bytes (two `String`s + `Vec<NarEntry>` + `Option<String>` + locals). Default thread stack is 2 MiB; tokio worker threads default to 2 MiB. A ~100 KiB malicious NAR gets ~700 levels → stack overflow → SIGSEGV → worker crash, no error logged.

The `MAX_DIRECTORY_ENTRIES` check at line 272 bounds **width**, not depth. Sibling file `derivation/hash.rs:55` already does depth-limiting via `MAX_HASH_RECURSION_DEPTH`. Same pattern.

### Diff

```diff
--- a/rio-nix/src/nar.rs
+++ b/rio-nix/src/nar.rs
@@ -37,6 +37,13 @@ const MAX_NAME_LEN: u64 = 256;
 /// Maximum allowed symlink target length.
 const MAX_TARGET_LEN: u64 = 4096;

+/// Maximum NAR directory nesting depth. Nix's PATH_MAX is 4096; with the
+/// 1-char minimum entry name plus separator that caps legitimate nesting
+/// at ~2048, but no real store path approaches 256 components.
+/// Each level costs ~300 bytes of stack; 256 * 300 = 77 KiB, well within
+/// the 2 MiB thread stack.
+const MAX_NAR_DEPTH: usize = 256;
+
 /// Errors from NAR operations.
 #[derive(Debug, Error)]
 pub enum NarError {
@@ -64,6 +71,9 @@ pub enum NarError {
     #[error("directory entries not in sorted order: {prev:?} >= {cur:?}")]
     UnsortedEntries { prev: String, cur: String },

+    #[error("directory nesting depth {0} exceeds maximum {MAX_NAR_DEPTH}")]
+    NestingTooDeep(usize),
+
     #[error("not a single-file NAR")]
     NotSingleFile,
 }
@@ -203,7 +213,7 @@ pub fn parse(r: &mut impl Read) -> Result<NarNode> {
         return Err(NarError::InvalidMagic(magic));
     }

-    parse_node(r)
+    parse_node(r, 0)
 }

 /// Parse a single NAR node.
@@ -211,14 +221,17 @@ pub fn parse(r: &mut impl Read) -> Result<NarNode> {
 /// Each sub-parser is responsible for consuming its own closing ")".
 /// This is necessary because `parse_directory` reads tokens in a loop
 /// and must consume ")" to detect end-of-directory.
-fn parse_node(r: &mut impl Read) -> Result<NarNode> {
+fn parse_node(r: &mut impl Read, depth: usize) -> Result<NarNode> {
+    if depth > MAX_NAR_DEPTH {
+        return Err(NarError::NestingTooDeep(depth));
+    }
     expect_str(r, "(")?;
     expect_str(r, "type")?;

     let node_type = read_string(r)?;
     match node_type.as_str() {
         "regular" => parse_regular(r),
-        "directory" => parse_directory(r),
+        "directory" => parse_directory(r, depth),
         "symlink" => parse_symlink(r),
         _ => Err(NarError::UnknownNodeType(node_type)),
     }
@@ -264,7 +277,7 @@ fn parse_regular(r: &mut impl Read) -> Result<NarNode> {
 /// Maximum number of directory entries (DoS prevention for unbounded allocation).
 const MAX_DIRECTORY_ENTRIES: usize = 1_048_576;

-fn parse_directory(r: &mut impl Read) -> Result<NarNode> {
+fn parse_directory(r: &mut impl Read, depth: usize) -> Result<NarNode> {
     let mut entries = Vec::new();
     let mut prev_name: Option<String> = None;

@@ -303,7 +316,7 @@ fn parse_directory(r: &mut impl Read) -> Result<NarNode> {
                 prev_name = Some(name.clone());

                 expect_str(r, "node")?;
-                let node = parse_node(r)?;
+                let node = parse_node(r, depth + 1)?;
                 expect_str(r, ")")?; // close the entry's parens

                 entries.push(NarEntry { name, node });
```

Guard placed in `parse_node` (not `parse_directory`) so the check fires **before** reading the `(` / `type` / node-type tokens — fails fast, no partial allocation.

### Tests

**Unit test (nar.rs test module):** construct a 300-level deep NAR via `serialize`, assert `parse` returns `NestingTooDeep` rather than crashing.

```rust
/// A malicious NAR with ~300 levels of nested directories must be rejected
/// with NestingTooDeep, not crash with a stack overflow.
#[test]
fn reject_deeply_nested_nar() {
    // Build inside-out: each level wraps the previous in a one-entry dir.
    let mut node = NarNode::Regular { executable: false, contents: vec![] };
    for _ in 0..(MAX_NAR_DEPTH + 10) {
        node = NarNode::Directory {
            entries: vec![NarEntry { name: "a".to_string(), node }],
        };
    }

    // serialize() is iterative-per-level via the loop in serialize_node's
    // Directory arm but *also* recurses — if this blows the test stack,
    // switch to hand-rolling bytes with write_str. At 266 levels it's fine.
    let mut buf = Vec::new();
    serialize(&mut buf, &node).expect("serialize succeeds");

    let result = parse(&mut Cursor::new(&buf));
    assert!(
        matches!(result, Err(NarError::NestingTooDeep(d)) if d > MAX_NAR_DEPTH),
        "expected NestingTooDeep, got {result:?}"
    );
}

/// A NAR at exactly MAX_NAR_DEPTH must still parse.
#[test]
fn accept_nar_at_depth_limit() -> anyhow::Result<()> {
    let mut node = NarNode::Regular { executable: false, contents: b"leaf".to_vec() };
    for _ in 0..MAX_NAR_DEPTH {
        node = NarNode::Directory {
            entries: vec![NarEntry { name: "a".to_string(), node }],
        };
    }
    let mut buf = Vec::new();
    serialize(&mut buf, &node)?;
    let parsed = parse(&mut Cursor::new(&buf))?;
    assert_eq!(parsed, node);
    Ok(())
}
```

**Note on `serialize_node`:** it also recurses at line 368 (`serialize_node(w, &entry.node)?`). The proptest strategy at line 883 caps depth at 4, so proptest never trips this. `dump_path_streaming` (line 452) walks the filesystem and would also recurse unboundedly on a pathological tree, but that's host-filesystem-controlled, not attacker-controlled. **Leave `serialize_node` unbounded** — its inputs are rio-produced `NarNode` trees that went through `parse` first, so the depth invariant is enforced at the entry point. If a future caller constructs deep trees in-memory, that's a separate issue.

**Fuzz seed:** add `rio-nix/fuzz/corpus/nar_parsing/seed-deep-nesting.nar` — 50-level nesting (deep enough to give the fuzzer a structural template, shallow enough that the seed itself parses). Generate via:

```bash
nix develop -c cargo run --example gen-nar-seed -- --depth 50 \
    > rio-nix/fuzz/corpus/nar_parsing/seed-deep-nesting.nar
```

If `gen-nar-seed` doesn't exist yet, inline a `#[test] #[ignore]` that writes the file and run it once with `cargo nextest run --run-ignored all gen_deep_seed`.

---

## 3. nixbase32 — reject nonzero padding bits

**File:** `rio-nix/src/store_path.rs:394-399`

### Bug

The decode loop computes `byte_idx = bit_pos / 8`. For the last input char(s), `byte_idx` can equal `out_len` — those bits are silently dropped by the `if byte_idx < out_len` guard. Similarly at line 397, the high-bits spill can land past `out_len`.

Nix C++ (`libutil/hash.cc:parseHash32`) throws `"invalid base-32 hash"` when these padding bits are nonzero. rio's silent drop means multiple distinct base32 strings decode to the same bytes: the decode is **not injective**, which breaks any code that treats the base32 string as a canonical key (store path hash portions, narinfo signatures).

Concretely: a 20-byte output is 160 bits; 32 input chars = 160 bits exactly — no slack. But a 32-byte SHA-256 output is 256 bits; 52 chars = 260 bits — **4 slack bits** on the most-significant (first) char. Right now `"0<51 chars>"` and `"1<51 chars>"` decode to the same 32 bytes.

### Diff

Add the error variant, then check both overflow sites.

```diff
--- a/rio-nix/src/store_path.rs
+++ b/rio-nix/src/store_path.rs
@@ -49,6 +49,9 @@ pub enum StorePathError {

     #[error("invalid nixbase32 length: expected {expected}, got {got}")]
     InvalidBase32Length { expected: usize, got: usize },
+
+    #[error("invalid nixbase32 string: non-zero padding bits (non-canonical encoding)")]
+    InvalidBase32Padding,
 }
```

```diff
--- a/rio-nix/src/store_path.rs
+++ b/rio-nix/src/store_path.rs
@@ -391,12 +391,24 @@ pub mod nixbase32 {
             let byte_idx = bit_pos / 8;
             let bit_offset = bit_pos % 8;

-            if byte_idx < out_len {
-                out[byte_idx] |= digit << bit_offset;
-            }
-            if bit_offset > 3 && byte_idx + 1 < out_len {
-                out[byte_idx + 1] |= digit >> (8 - bit_offset);
-            }
+            // Low bits of this digit land in out[byte_idx].
+            let lo = digit << bit_offset;
+            if byte_idx < out_len {
+                out[byte_idx] |= lo;
+            } else if lo != 0 {
+                // These bits have nowhere to go — non-canonical encoding.
+                // Nix's parseHash32 throws here.
+                return Err(StorePathError::InvalidBase32Padding);
+            }
+
+            // High bits (if bit_offset > 3) spill into out[byte_idx + 1].
+            if bit_offset > 3 {
+                let hi = digit >> (8 - bit_offset);
+                if byte_idx + 1 < out_len {
+                    out[byte_idx + 1] |= hi;
+                } else if hi != 0 {
+                    return Err(StorePathError::InvalidBase32Padding);
+                }
+            }
         }

         Ok(out)
```

### Tests

```rust
/// nixbase32 encoding is NOT surjective onto the full input-char space when
/// output_len*8 is not a multiple of 5. For a 32-byte (SHA-256) decode,
/// 52 chars * 5 = 260 bits but only 256 fit — the top 4 bits of the
/// most-significant (first) char are padding and must be zero.
#[test]
fn test_nixbase32_rejects_nonzero_padding() {
    // 52 chars decode to 32 bytes. The first char's top 4 bits are overflow.
    // 'z' = 31 (0b11111) → all 4 overflow bits set.
    let bad = format!("z{}", "0".repeat(51));
    assert!(matches!(
        nixbase32::decode(&bad),
        Err(StorePathError::InvalidBase32Padding)
    ));

    // '0' = 0 → zero overflow bits. Must still decode cleanly.
    let good = "0".repeat(52);
    let decoded = nixbase32::decode(&good).expect("zero padding is canonical");
    assert_eq!(decoded, vec![0u8; 32]);

    // '1' = 1 (0b00001) → only bit 0 set, which lands in out[31] bit 7.
    // That bit IS in-bounds for 32 bytes (bit 255). No padding, must succeed.
    let one_in_lsb = format!("{}1", "0".repeat(51));
    assert!(nixbase32::decode(&one_in_lsb).is_ok());
}

#[test]
fn test_nixbase32_20byte_no_padding() {
    // 20 bytes = 160 bits = 32 chars * 5 exactly. No padding exists,
    // so every valid-alphabet input is canonical. Exhaustively verify
    // the first char can be anything.
    for c in "0123456789abcdfghijklmnpqrsvwxyz".chars() {
        let s = format!("{c}{}", "0".repeat(31));
        assert!(nixbase32::decode(&s).is_ok(), "rejected valid first char {c:?}");
    }
}
```

**Proptest (store_path.rs proptest module — or add one if missing):**

```rust
proptest! {
    /// encode is canonical: decode∘encode = id, and the encoding is the
    /// ONLY input that decodes to those bytes (for inputs where bits
    /// align — we test 20-byte store-path hashes where this holds trivially,
    /// and 32-byte SHA-256 hashes where padding rejection enforces it).
    #[test]
    fn nixbase32_roundtrip_20(data in proptest::array::uniform20(any::<u8>())) {
        let encoded = nixbase32::encode(&data);
        let decoded = nixbase32::decode(&encoded)?;
        prop_assert_eq!(decoded.as_slice(), &data[..]);
    }

    #[test]
    fn nixbase32_roundtrip_32(data in proptest::array::uniform32(any::<u8>())) {
        let encoded = nixbase32::encode(&data);
        let decoded = nixbase32::decode(&encoded)?;
        prop_assert_eq!(decoded.as_slice(), &data[..]);
    }

    /// Mutating a single char in a canonical encoding either fails to
    /// decode (bad char / bad padding) or decodes to DIFFERENT bytes.
    /// This is the injectivity property the padding check restores.
    #[test]
    fn nixbase32_decode_injective(
        data in proptest::array::uniform32(any::<u8>()),
        mutate_idx in 0usize..52,
        mutate_to in 0u8..32,
    ) {
        let encoded = nixbase32::encode(&data);
        let orig_char = encoded.as_bytes()[mutate_idx];
        let new_char = b"0123456789abcdfghijklmnpqrsvwxyz"[mutate_to as usize];
        prop_assume!(orig_char != new_char);

        let mut mutated = encoded.into_bytes();
        mutated[mutate_idx] = new_char;
        let mutated = String::from_utf8(mutated).unwrap();

        match nixbase32::decode(&mutated) {
            Err(_) => {} // padding rejection — good
            Ok(decoded) => prop_assert_ne!(decoded.as_slice(), &data[..],
                "two distinct inputs decoded to identical output: injectivity violated"),
        }
    }
}
```

---

## 4. NAR parser — delete dead `")" => Regular{empty}` arm

**File:** `rio-nix/src/nar.rs:242-248`

### Why it's dead

The NAR grammar in Nix C++ (`libstore/nar-archive.cc:parse`) is:

```
regular := "type" "regular" ["executable" ""] "contents" <bytes> ")"
```

The `contents` token is **mandatory** even for empty files — `nix-store --dump` on a zero-byte file emits `... contents <0-length bytes> )`. There is no `( type regular )` production. rio's own serializer agrees: `serialize_node` (line 356) unconditionally writes `"contents"`.

The `")"` arm at 242-248 accepts a NAR that no conformant Nix producer emits. That's harmless in isolation — it's a parser over-acceptance. The reason to **delete** it is that it masks a real class of bug: if a future refactor of `read_string` or `expect_str` drops a token (off-by-one in padding, premature return), the parser can skip past `"contents"`, land on the directory-entry's closing `")"` (from one level up), and return a phantom empty file instead of erroring. The proptest at line 908 would catch that roundtrip failure eventually, but only after the fuzzer constructs a directory-with-regular-child shape, and the error surfaces as a `prop_assert_eq` mismatch rather than a parse error at the actual desync point.

### Diff

```diff
--- a/rio-nix/src/nar.rs
+++ b/rio-nix/src/nar.rs
@@ -239,19 +239,12 @@ fn parse_regular(r: &mut impl Read) -> Result<NarNode> {
             let contents = read_bytes_bounded(r, MAX_CONTENT_SIZE)?;
             (false, contents)
         }
-        ")" => {
-            // Empty file with no "contents" field: ( type regular )
-            return Ok(NarNode::Regular {
-                executable: false,
-                contents: vec![],
-            });
-        }
         _ => {
             return Err(NarError::UnexpectedToken {
-                expected: "\"executable\" or \"contents\" or \")\"".to_string(),
+                expected: "\"executable\" or \"contents\"".to_string(),
                 got: token,
             });
         }
     };
```

### Test

```rust
/// The NAR grammar requires a "contents" token even for empty regular
/// files. `( type regular )` is not a valid production — reject it.
#[test]
fn reject_regular_without_contents() {
    // Hand-roll: nix-archive-1 ( type regular )
    let mut buf = Vec::new();
    write_str(&mut buf, NAR_MAGIC).unwrap();
    write_str(&mut buf, "(").unwrap();
    write_str(&mut buf, "type").unwrap();
    write_str(&mut buf, "regular").unwrap();
    write_str(&mut buf, ")").unwrap();

    let result = parse(&mut Cursor::new(&buf));
    assert!(
        matches!(result, Err(NarError::UnexpectedToken { .. })),
        "expected UnexpectedToken for contents-less regular, got {result:?}"
    );
}
```

No existing test asserts the positive case for this arm (grep for `Empty file with no` turns up only the comment). `roundtrip_regular_file` at line 630 uses `contents: b"hello world\n"`; the proptest strategy at 872 generates `vec(any::<u8>(), 0..256)` which can be empty but goes through `serialize` first, which always emits `contents`. **No test churn.**

---

## 5. FOD fingerprint — include `output.path()`

**File:** `rio-nix/src/derivation/hash.rs:16,69`

### Recap from §5

Already downgraded CRITICAL → LOW after verifying the hash is never persisted and never crosses the wire. Fixing anyway because (a) the docstring at line 16 claims parity with Nix and lies, (b) three tests assert wrong golden values and will bite the next person who touches this, (c) `nix-aterm-modulo-key-collision` is a symptom — two FODs with identical `(algo, hash)` but different output paths currently collide in the memoization cache. Fixing the fingerprint fixes the collision.

### Diff

```diff
--- a/rio-nix/src/derivation/hash.rs
+++ b/rio-nix/src/derivation/hash.rs
@@ -13,7 +13,7 @@ use super::{Derivation, DerivationError, MAX_HASH_RECURSION_DEPTH};
 /// Compute the modular derivation hash, matching Nix C++ `hashDerivationModulo`.
 ///
 /// Three cases:
-/// - **FOD** (fixed-output): `SHA-256("fixed:out:{hash_algo}:{hash}:")`
+/// - **FOD** (fixed-output): `SHA-256("fixed:out:{hash_algo}:{hash}:{output_path}")`
 /// - **Input-addressed**: replace `inputDrvs` keys with recursive modular hashes,
 ///   then `SHA-256(modified_aterm)`
 /// - **CA floating / impure**: same as input-addressed but output paths are masked
@@ -66,7 +66,13 @@ fn hash_derivation_modulo_inner<'c>(
         if drv.is_fixed_output() {
             // FOD base case: hash the fingerprint string (no recursion)
             let output = &drv.outputs()[0];
-            let fingerprint = format!("fixed:out:{}:{}:", output.hash_algo(), output.hash());
+            // Nix C++ derivations.cc:hashDerivationModulo — the output path
+            // IS part of the fingerprint. The trailing-colon-no-path shape
+            // was a copy-paste from store_path.rs make_store_path_hash
+            // where it IS correct (different function). See phase4a.md §5.
+            let fingerprint = format!(
+                "fixed:out:{}:{}:{}", output.hash_algo(), output.hash(), output.path()
+            );
             Ok(Sha256::digest(fingerprint.as_bytes()).into())
         } else {
```

### Test vector updates

Three tests hard-code the fingerprint bytes. All three use `/nix/store/xyz-fixed` or `/nix/store/xyz-rec` as the output path (see the `Derive([("out","/nix/store/xyz-fixed",...)]` fixtures at lines 117, 407).

| Test | Line | Old vector | New vector |
|---|---|---|---|
| `fod_hash_matches_fingerprint` | 141 | `b"fixed:out:sha256:e3b0...855:"` | `b"fixed:out:sha256:e3b0...855:/nix/store/xyz-fixed"` |
| `ia_with_fod_input_uses_modular_hash` | 187 | same | same as above |
| `fod_recursive_hash` | 418 | `b"fixed:out:r:sha256:e3b0...855:"` | `b"fixed:out:r:sha256:e3b0...855:/nix/store/xyz-rec"` |

```diff
--- a/rio-nix/src/derivation/hash.rs
+++ b/rio-nix/src/derivation/hash.rs
@@ -138,8 +138,8 @@ mod hash_derivation_modulo_tests {
         let hash = hash_derivation_modulo(&drv, "/nix/store/xyz-fixed.drv", &resolve, &mut cache)?;

-        // Expected: SHA-256("fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:")
+        // Expected: SHA-256("fixed:out:sha256:<hex>:/nix/store/xyz-fixed")
         let expected: [u8; 32] = Sha256::digest(
-            b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:",
+            b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:/nix/store/xyz-fixed",
         )
         .into();

@@ -184,8 +184,8 @@ mod hash_derivation_modulo_tests {
         // The FOD modular hash
         let fod_hash: [u8; 32] = Sha256::digest(
-            b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:",
+            b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:/nix/store/xyz-fixed",
         )
         .into();

@@ -415,8 +415,8 @@ mod hash_derivation_modulo_tests {
-        // Expected: SHA-256("fixed:out:r:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:")
+        // Expected: SHA-256("fixed:out:r:sha256:<hex>:/nix/store/xyz-rec")
         let expected: [u8; 32] = Sha256::digest(
-            b"fixed:out:r:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:",
+            b"fixed:out:r:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:/nix/store/xyz-rec",
         )
         .into();
```

**These are still rio-vs-rio tests.** §5 says "compute with `nix-store --query --hash` on a real FOD" for a true golden. That requires a live-daemon test fixture with a known FOD path — **defer to a follow-up `golden/` test**, the three vector updates above are enough to make the tree green and the docstring honest.

### Collision regression test

```rust
/// Two FODs with identical (algo, hash) but different output paths must
/// produce different modular hashes — nix-aterm-modulo-key-collision.
/// Before this fix they collided because the fingerprint omitted path.
#[test]
fn fod_different_paths_different_hashes() -> anyhow::Result<()> {
    let drv_a = Derivation::parse(
        r#"Derive([("out","/nix/store/aaa-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed-a"),("out","/nix/store/aaa-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#,
    )?;
    let drv_b = Derivation::parse(
        r#"Derive([("out","/nix/store/bbb-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed-b"),("out","/nix/store/bbb-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#,
    )?;

    let mut cache = HashMap::new();
    let resolve = |_: &str| -> Option<&Derivation> { None };
    let hash_a = hash_derivation_modulo(&drv_a, "/nix/store/aaa-fixed.drv", &resolve, &mut cache)?;
    let hash_b = hash_derivation_modulo(&drv_b, "/nix/store/bbb-fixed.drv", &resolve, &mut cache)?;

    assert_ne!(hash_a, hash_b, "FODs differing only in output path must not collide");
    Ok(())
}
```

---

## 6. Proptest coverage — ATerm escapes + NixHash roundtrips

### 6a. ATerm: widen escape strategy

**File:** `rio-nix/src/derivation/aterm.rs:742-743`

Current `args` strategy is `"[a-zA-Z0-9 ./-]{0,20}"` and `env` values are `"[a-zA-Z0-9 =/_.-]{0,30}"`. Neither generates `\n`, `\r`, `\t`, `\\`, or `"` — **the five characters that `write_aterm_string` (line 14-23) escapes**. The proptest never exercises the escape roundtrip. `parse_with_escaped_strings` (532) and `roundtrip_with_escapes` (574) are static fixtures that cover a handful of combinations.

The proptest runs 4096 cases; with a strategy that includes escape chars at ~5% density, it'll cover escape-at-boundary, consecutive-escapes, escape-adjacent-to-quote — the positions static tests miss.

```diff
--- a/rio-nix/src/derivation/aterm.rs
+++ b/rio-nix/src/derivation/aterm.rs
@@ -739,8 +739,21 @@ mod proptests {
                 proptest::collection::btree_set("/nix/store/[a-z0-9]{32}-[a-z]{1,8}", 0..3),
                 "(x86_64|aarch64)-linux",
                 "/nix/store/[a-z0-9]{32}-bash/bin/bash",
-                proptest::collection::vec("[a-zA-Z0-9 ./-]{0,20}", 0..4),
-                proptest::collection::btree_map("[a-zA-Z_]{1,10}", "[a-zA-Z0-9 =/_.-]{0,30}", 0..5),
+                // args: include the five ATerm-escaped chars (\\, ", \n, \r, \t)
+                // so the proptest actually exercises write_aterm_string's
+                // escape branches. Roughly 5/40 char density.
+                proptest::collection::vec(arb_aterm_string(20), 0..4),
+                proptest::collection::btree_map("[a-zA-Z_]{1,10}", arb_aterm_string(30), 0..5),
             )
```

With a helper above the `arb_derivation` strategy:

```rust
/// String strategy that includes all five ATerm-escaped characters.
/// Weighted so ~10-15% of generated chars need escaping.
fn arb_aterm_string(max_len: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            // 85%: safe ASCII
            30 => proptest::char::range('a', 'z'),
            10 => proptest::char::range('0', '9'),
            5  => Just(' '),
            5  => Just('/'),
            // 15%: escape-requiring chars
            2  => Just('\\'),
            2  => Just('"'),
            2  => Just('\n'),
            1  => Just('\r'),
            1  => Just('\t'),
        ],
        0..max_len,
    )
    .prop_map(|chars| chars.into_iter().collect())
}
```

### 6b. NixHash: proptest roundtrips

**File:** `rio-nix/src/hash.rs` — add a `mod proptests` at the bottom of the existing `mod tests` (line ~320).

`test_colon_format_roundtrip` (262) and `test_sri_format_roundtrip` (272) each test **one** value (`SHA256("hello")`). That's one point on a ~2²⁵⁶ domain.

```rust
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn arb_hash_algo() -> impl Strategy<Value = HashAlgo> {
        prop_oneof![
            Just(HashAlgo::SHA256),
            Just(HashAlgo::SHA512),
            Just(HashAlgo::SHA1),
        ]
    }

    fn arb_nixhash() -> impl Strategy<Value = NixHash> {
        arb_hash_algo().prop_flat_map(|algo| {
            proptest::collection::vec(any::<u8>(), algo.digest_len())
                .prop_map(move |digest| NixHash::new(algo, digest).expect("length matches algo"))
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        #[test]
        fn colon_roundtrip(h in arb_nixhash()) {
            let s = h.to_colon();
            let parsed = NixHash::parse_colon(&s)?;
            prop_assert_eq!(parsed, h);
        }

        #[test]
        fn sri_roundtrip(h in arb_nixhash()) {
            let s = h.to_sri();
            let parsed = NixHash::parse_sri(&s)?;
            prop_assert_eq!(parsed, h);
        }

        /// Any byte sequence of wrong length must be rejected by NixHash::new.
        #[test]
        fn new_rejects_wrong_length(
            algo in arb_hash_algo(),
            digest in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            let result = NixHash::new(algo, digest.clone());
            if digest.len() == algo.digest_len() {
                prop_assert!(result.is_ok());
            } else {
                prop_assert!(matches!(result, Err(HashError::WrongDigestLength { .. })));
            }
        }
    }
}
```

`proptest` is already a dev-dependency (used in `nar.rs`, `aterm.rs`). No `Cargo.toml` change.

---

## Validation sequence

```bash
# Unit + proptest — should be green after all six changes.
nix develop -c cargo nextest run -p rio-nix

# Clippy — the new error variants need exhaustive match audits.
# `StorePathError` is matched in narinfo.rs and store_path.rs tests;
# `NarError` is matched in worker upload.rs. Grep first:
rg 'match.*StorePathError|match.*NarError' --type rust

# Fuzz smoke — 100k runs on the NAR target with the new deep seed.
# Should NOT find new crashes (depth limit catches the deep case as
# a structured error, not a panic).
nix develop -c bash -c 'cd rio-nix/fuzz && cargo fuzz run nar_parsing -- -runs=100000'
```

**Watch for:** `NarError` is `#[derive(Error)]` with `#[from] io::Error`. Adding a variant is non-breaking for `?` ergonomics but any downstream `match` on `NarError` without a `_` arm goes red. Grep shows `rio-worker/src/upload.rs` and `rio-store/src/nar_reassembly.rs` as likely candidates — if they have exhaustive matches, add a catch-all or explicit arm for `NestingTooDeep`.

---

## Finding-index updates

After this lands, flip in `phase4a.md`:

| Finding | From | To |
|---|---|---|
| `nix-make-text-unsorted-refs` | OPEN | FIXED |
| `nix-nar-unbounded-recursion` | OPEN | FIXED |
| `nix-nixbase32-decode-silent-overflow` | OPEN | FIXED |
| `nix-fod-hash-modulo-omits-outpath` | OPEN | FIXED |
| `nix-aterm-modulo-key-collision` | OPEN | FIXED (symptom of above) |
| `nix-proptest-escapes` | OPEN | FIXED |
| `nix-hash-no-proptest` | OPEN | FIXED |

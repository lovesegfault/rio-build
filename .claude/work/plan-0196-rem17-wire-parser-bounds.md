# Plan 0196: Rem-17 — Wire parser bounds: NAR depth, nixbase32 padding, make_text sort, FOD fingerprint

## Design

**Omnibus of six rio-nix parser/encoding fixes.** All predate phase 4a; found by the remediation audit's fuzz+proptest sweep.

1. **`make_text` sorts references before fingerprint.** Nix uses `StorePathSet` (BTreeSet) which iterates sorted; the wire protocol does not mandate sorted order from the client. Unsorted refs produced a different store path than a real Nix daemon. `test_make_text_with_references` was passing already-sorted input, hiding the bug.

2. **NAR parser bounds recursion depth at 256.** `parse_node` → `parse_directory` → `parse_node` had no depth counter; a ~100 KiB malicious NAR could stack-overflow a worker thread. New `NarError::NestingTooDeep`, guard before any allocation. 50-level fuzz seed added.

3. **`nixbase32::decode` rejects nonzero padding bits.** For 32-byte SHA-256 decodes, 52 chars × 5 = 260 bits but only 256 fit; the top 4 bits of the first char were silently dropped, breaking injectivity. Nix C++ `parseHash32` throws here. New `InvalidBase32Padding` variant. Proptests: `roundtrip_20`, `roundtrip_32`, `decode_injective`.

4-6: FOD fingerprint hash-algo prefix; ATerm escape handling; derivation hash modular arithmetic edge cases (see commit body for detail).

Remediation doc: `docs/src/remediations/phase4a/17-wire-parser-fixes.md` (729 lines). Doc was created inside commit `a335485` (same mixed commit as rem-05's codecov fix).

## Files

```json files
[
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "make_text sorts references before fingerprint"},
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "depth counter 256; NestingTooDeep variant; guard before alloc"},
  {"path": "rio-nix/src/hash.rs", "action": "MODIFY", "note": "nixbase32 decode rejects nonzero padding; InvalidBase32Padding variant"},
  {"path": "rio-nix/src/derivation/aterm.rs", "action": "MODIFY", "note": "ATerm escape handling"},
  {"path": "rio-nix/src/derivation/hash.rs", "action": "MODIFY", "note": "FOD fingerprint hash-algo prefix; modular edge cases"},
  {"path": "rio-nix/fuzz/corpus/nar_parsing/seed-deep-nesting.nar", "action": "NEW", "note": "50-level nesting fuzz seed"}
]
```

## Tracey

No new markers. These are pre-spec parser bugs; fuzz corpus seed added for regression.

## Entry

- Depends on P0148: phase 3b complete
- Depends on P0009: phase 1b NAR reader (extends `parse_node`)
- Depends on P0008: phase 1b ATerm parser (extends escape handling)

## Exit

Merged as `a335485` (plan doc, mixed commit) + `8a7e26c` (fix). `.#ci` green including 2min fuzz on `nar_parsing` with the new seed.

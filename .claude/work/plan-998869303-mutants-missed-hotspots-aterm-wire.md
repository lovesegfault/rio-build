# Plan 998869303: cargo-mutants 58-MISSED triage — aterm×7 + wire×6 hot-spots

[P0301](plan-0301-cargo-mutants-ci.md) wired `cargo-mutants`; [P0368](plan-0368-cargo-mutants-baseline-failure.md) fixes the baseline so the mutation pass actually runs. Once the baseline is green, `mutants.out` shows ~58 MISSED mutants concentrated in three hot-spots:

| Hot-spot | MISSED count | File | Class |
|---|---|---|---|
| ATerm parser | 7 | [`rio-nix/src/derivation/aterm.rs`](../../rio-nix/src/derivation/aterm.rs) | off-by-one in `pos` advance, escape-char handling, list-vs-tuple delimiter |
| wire primitives | 6 | [`rio-nix/src/protocol/wire/mod.rs`](../../rio-nix/src/protocol/wire/mod.rs) + [`framed.rs`](../../rio-nix/src/protocol/wire/framed.rs) | padding calculation, u64-LE byte order, collection-max enforcement |
| HMAC expiry boundary | 1 | [`rio-common/src/hmac.rs:218`](../../rio-common/src/hmac.rs) | `>` vs `>=` at `now == expiry` |

The HMAC boundary is tracked separately at [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T39 (simple 15-line test, deps=[], lands immediately). This plan covers the aterm×7 + wire×6 plus a first-pass triage of the remaining ~44 scattered MISSED mutants to determine which are noise (equivalent mutations, dead code) vs gaps.

**CORRECTS coordinator speculation:** an earlier followup said the hmac.rs MISSED mutant was "constant-time comparison" — WRONG. Actual: `> vs >=` at the expiry-second boundary. No test exercises `now_unix == claims.expiry_unix`. Current code at [`:218`](../../rio-common/src/hmac.rs) uses `if now_unix > claims.expiry_unix` → at-the-second is VALID. A `>=` mutation would reject at-the-second tokens and no test catches it. P0311-T39 codifies at-the-second-is-valid. discovered_from=bughunt-mc147.

## Entry criteria

- [P0368](plan-0368-cargo-mutants-baseline-failure.md) merged (baseline passes, `mutants.out` has >0 outcomes)
- `just mutants` or `/nixbuild .#mutants` produces a fresh `mutants.out/missed.txt` listing the exact mutants

## Tasks

### T1 — `test(nix):` ATerm parser — target the 7 MISSED mutants

The fuzz target at [`rio-nix/fuzz/fuzz_targets/aterm.rs`](../../rio-nix/fuzz/fuzz_targets/) (check existence; may be named `derivation` or `aterm_parse`) exercises parse-crash-freedom but NOT semantic correctness — mutations that produce wrong-but-valid output are MISSED.

At dispatch, grep `mutants.out/missed.txt` for `aterm.rs:` lines. Expected classes:

| Mutation class | Example | Test gap |
|---|---|---|
| `pos += 1` → `pos += 0` | `:42` in `advance()` | Infinite loop or wrong-char consumed — roundtrip test catches (parse→serialize→byte-equal) |
| `'\\'` → `'/'` in escape handling | `:75` or nearby | Escaped backslash misparsed — fixture with `\\` in drv path |
| `'['` ↔ `'('` list/tuple delimiter | `:100`-range | Wrong ATerm constructor emitted — roundtrip diff |
| `','` → `';'` separator | `:120`-range | Multi-element lists merge — count assert |

NEW test module in [`rio-nix/src/derivation/aterm.rs`](../../rio-nix/src/derivation/aterm.rs) `#[cfg(test)]` (or extend existing):

```rust
#[cfg(test)]
mod mutants_gap {
    use super::*;

    // Seed fixtures: real .drv ATerm from the golden corpus (P0247).
    // Include at least one with:
    //  - escaped chars in a string (\\, \", \n)
    //  - multi-element input list (≥3)
    //  - nested tuple-in-list (inputDrvs)
    const FIXTURE_ESCAPES: &[u8] = include_bytes!("../../tests/fixtures/escapes.drv");
    const FIXTURE_MULTI_INPUT: &[u8] = include_bytes!("../../tests/fixtures/multi-input.drv");

    /// Parse → serialize → byte-equal. Catches pos-advance and
    /// delimiter mutations (wrong structure → wrong serialization).
    #[test]
    fn roundtrip_byte_equal_escapes() {
        let parsed = Derivation::from_aterm(FIXTURE_ESCAPES).unwrap();
        let reserialized = parsed.to_aterm();
        assert_eq!(reserialized.as_bytes(), FIXTURE_ESCAPES,
            "roundtrip diff: parser consumed wrong structure");
    }

    /// Escape-char handling: \\ → backslash, \" → quote, \n → newline.
    /// A mutation flipping the escape table would produce the wrong
    /// literal char, which roundtrip-byte-equal catches but this is
    /// the focused assertion.
    #[test]
    fn escape_chars_roundtrip() {
        let drv_text = r#"Derive([("out","/nix/store/aaa-out","","")],[],["/nix/store/bbb-src"],"x86_64-linux","/bin/sh",["-c","echo \"hello\\world\""],[("env_var","line1\nline2")])"#;
        let parsed = Derivation::from_aterm(drv_text.as_bytes()).unwrap();
        // builder arg should contain literal quote and backslash
        assert!(parsed.args[1].contains(r#""hello\world""#),
            "escape parsing broken: args={:?}", parsed.args);
        // env value should contain literal newline
        assert_eq!(parsed.env.get("env_var"), Some(&"line1\nline2".to_string()));
    }

    /// Multi-element count: inputSrcs with N entries must parse as N,
    /// not 1 (separator-swap mutation would merge them into one string).
    #[test]
    fn input_srcs_count_preserved() {
        let parsed = Derivation::from_aterm(FIXTURE_MULTI_INPUT).unwrap();
        assert_eq!(parsed.input_srcs.len(), 3, // adjust to fixture's real count
            "separator mutation merged elements: {:?}", parsed.input_srcs);
    }
}
```

Generate fixture `.drv` files via `nix-instantiate` on trivial expressions if golden corpus lacks them; commit under `rio-nix/tests/fixtures/` (add `seed-` prefix if also useful as fuzz seeds).

### T2 — `test(nix):` wire primitives — target the 6 MISSED mutants

The wire parsers at [`rio-nix/src/protocol/wire/mod.rs`](../../rio-nix/src/protocol/wire/mod.rs) have fuzz coverage (`wire_primitives` target) but MISSED mutants indicate the fuzz corpus doesn't cover specific byte patterns.

At dispatch, grep `mutants.out/missed.txt` for `wire/mod.rs:` and `wire/framed.rs:`. Expected classes:

| Mutation | Likely gap |
|---|---|
| padding `8 - len % 8` → `len % 8` | No test with `len % 8 ∈ {1..7}` — add fixtures at each residue |
| `u64::from_le_bytes` → `from_be_bytes` | Big-endian value would parse — add known-u64 roundtrip (e.g., `0x0102030405060708`) |
| `COLLECTION_MAX` comparison `>` → `>=` | Boundary value exactly-at-max not tested |
| `framed.rs` length-prefix `read_u64` → `read_u32` | Frame >4GB never tested (reasonable) — add `u64::MAX` frame-length error test |

Extend [`rio-nix/src/protocol/wire/mod.rs`](../../rio-nix/src/protocol/wire/mod.rs) tests (grep `#[test]` in the file):

```rust
/// Padding: for each residue 1..=7, the 8-byte alignment must pad
/// with (8-residue) zero bytes. A padding-math mutation flips this.
#[test]
fn string_padding_all_residues() {
    for len in 1..=7usize {
        let s = "x".repeat(len);
        let mut buf = Vec::new();
        write_string(&mut buf, &s).unwrap();
        // 8 (u64 length prefix) + len (payload) + (8-len%8)%8 (padding)
        let expected_len = 8 + len + (8 - len % 8) % 8;
        assert_eq!(buf.len(), expected_len,
            "len={len}: wrong padding; buf.len()={}", buf.len());
        // padding bytes MUST be zero
        for &b in &buf[8 + len..] {
            assert_eq!(b, 0, "non-zero padding byte at len={len}");
        }
    }
}

/// u64 LE byte order: a value with distinct bytes per position
/// detects endianness flip.
#[test]
fn u64_le_byte_order() {
    let mut buf = Vec::new();
    write_u64(&mut buf, 0x0807060504030201).unwrap();
    assert_eq!(buf, &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
    let mut cursor = std::io::Cursor::new(&buf[..]);
    assert_eq!(read_u64(&mut cursor).unwrap(), 0x0807060504030201);
}

/// COLLECTION_MAX boundary: exactly-at-max is valid, one-past is not.
#[test]
fn collection_max_boundary() {
    use super::COLLECTION_MAX;
    // at-max: should parse (construct a valid wire-encoded vec of
    // COLLECTION_MAX empty strings — may be large; use 0-len strings).
    let mut buf = Vec::new();
    write_u64(&mut buf, COLLECTION_MAX as u64).unwrap();
    for _ in 0..COLLECTION_MAX { write_string(&mut buf, "").unwrap(); }
    let mut cursor = std::io::Cursor::new(&buf[..]);
    assert!(read_string_vec(&mut cursor).is_ok(),
        "exactly COLLECTION_MAX should be accepted");

    // one-past: should error.
    let mut buf = Vec::new();
    write_u64(&mut buf, (COLLECTION_MAX + 1) as u64).unwrap();
    let mut cursor = std::io::Cursor::new(&buf[..]);
    assert!(read_string_vec(&mut cursor).is_err(),
        "COLLECTION_MAX + 1 should be rejected");
}
```

Adjust function names (`write_string`, `read_u64`, `read_string_vec`, `COLLECTION_MAX`) to match actual API at dispatch — grep the file for the real names.

### T3 — `test(common):` hmac.rs expiry boundary — forward-reference to P0311-T39

**This is tracked at [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T39.** No work here; this section exists so the mutants-triage is complete. P0311-T39 adds `expiry_boundary_at_the_second_is_valid` asserting `verify()` with `expiry_unix == now()` → `Ok`. Catches the `>` → `>=` mutation at [`hmac.rs:218`](../../rio-common/src/hmac.rs).

After P0311-T39 lands: re-run `just mutants` and confirm `hmac.rs:218` is CAUGHT not MISSED.

### T4 — `feat(tooling):` `.config/mutants.toml` — exempt equivalent mutants

After T1+T2 land and `just mutants` re-runs, some of the ~44 scattered MISSED will remain. Triage each:

| Category | Action |
|---|---|
| Equivalent mutation (e.g., `x * 1` → `x / 1`) | Add to `[[exclude]]` in `.config/mutants.toml` with a `# reason:` comment |
| Dead code / unreachable | File a trivial followup to delete the dead code (P0304 batch target) |
| Real gap, simple test | Add test in this plan's T5 catch-all |
| Real gap, complex setup | File a test-gap followup (P0311 batch target) |

Extend [`.config/mutants.toml`](../../.config/mutants.toml) with per-mutant exclusions:

```toml
# Equivalent mutants — mutation produces semantically identical code.
# Each line cites the source line + why it's equivalent.
[[exclude]]
file = "rio-nix/src/derivation/hash.rs"
line = 42
# `x.saturating_sub(0)` ↔ `x - 0` — both no-op. Cargo-mutants doesn't
# recognize saturating-arith identities.

# ... one per confirmed-equivalent ...
```

### T5 — `test:` catch-all — simple tests for scattered real-gap MISSED

For each scattered MISSED that's a real gap with a simple test (not worth a full followup): add the test to the owning crate's existing test module. Budget: ≤5 simple tests. Anything beyond that → `onibus state followup` → P0311 batch.

Target: after T1-T5, `just mutants` shows ≤10 MISSED (all documented equivalent mutants or complex-setup deferred with a P0311 T-ref).

## Exit criteria

- `/nbr .#ci` green
- T1: `cargo nextest run -p rio-nix mutants_gap` → ≥3 passed (roundtrip, escape, count)
- T2: `cargo nextest run -p rio-nix string_padding_all_residues u64_le_byte_order collection_max_boundary` → 3 passed
- T3: forward-ref only — `grep 'P0311.*T39\|hmac.rs:218' .claude/work/plan-998869303-*.md` → ≥1 hit (this doc references the tracking)
- `just mutants` (or `/nixbuild .#mutants`) shows ≤10 MISSED after T1-T5 (down from ~58)
- T4: `.config/mutants.toml` has ≥1 `[[exclude]]` block with `# reason:` for each equivalent mutant
- T5: for each remaining MISSED >10, a `onibus state followup` entry exists OR a `TODO(P0311)` in the source

## Tracey

References existing markers:
- `r[gw.wire.all-ints-u64]` — T2's `u64_le_byte_order` verifies the u64-LE encoding the marker describes at [`gateway.md:374`](../../docs/src/components/gateway.md)
- `r[gw.wire.string-encoding]` — T2's `string_padding_all_residues` verifies the 8-byte padding the marker describes at [`gateway.md:377`](../../docs/src/components/gateway.md)
- `r[gw.wire.collection-max]` — T2's `collection_max_boundary` verifies the enforcement the marker describes at [`gateway.md:380`](../../docs/src/components/gateway.md)

No new markers. ATerm parsing has no dedicated spec marker (it's a Nix-protocol implementation detail, not a rio contract); the roundtrip-byte-equal tests prove correctness against Nix's golden fixtures rather than against a rio spec.

## Files

```json files
[
  {"path": "rio-nix/src/derivation/aterm.rs", "action": "MODIFY", "note": "T1: +mutants_gap test mod — roundtrip, escape, count"},
  {"path": "rio-nix/tests/fixtures/escapes.drv", "action": "NEW", "note": "T1: golden .drv with \\\\ \\\" \\n escapes (nix-instantiate on trivial expr)"},
  {"path": "rio-nix/tests/fixtures/multi-input.drv", "action": "NEW", "note": "T1: golden .drv with ≥3 inputSrcs (count preservation fixture)"},
  {"path": "rio-nix/src/protocol/wire/mod.rs", "action": "MODIFY", "note": "T2: +string_padding_all_residues +u64_le_byte_order +collection_max_boundary tests"},
  {"path": ".config/mutants.toml", "action": "MODIFY", "note": "T4: +[[exclude]] per equivalent mutant with # reason: comments"}
]
```

```
rio-nix/
├── src/
│   ├── derivation/aterm.rs        # T1: mutants_gap tests
│   └── protocol/wire/mod.rs       # T2: padding/LE/max-boundary tests
└── tests/fixtures/
    ├── escapes.drv                # T1: NEW golden
    └── multi-input.drv            # T1: NEW golden
.config/mutants.toml               # T4: equivalent-mutant exclusions
```

## Dependencies

```json deps
{"deps": [368, 301], "soft_deps": [311, 247], "note": "discovered_from=bughunt-mc147 + coord mutants followup. P0368 fixes baseline (without it mutants.out shows 0 caught/0 missed — mutation phase never runs). P0301 wired the derivation + .config/mutants.toml. Soft-dep P0311-T39 (hmac boundary test — CORRECTS earlier coordinator speculation 'constant-time comparison' which was WRONG; actual mutation is > vs >= at hmac.rs:218 expiry-second boundary). T3 here is a forward-ref to P0311-T39, no work. Soft-dep P0247 (golden corpus — T1's fixtures may reuse corpus .drv files if escape chars present; grep corpus first). aterm.rs count=3 (low). wire/mod.rs count=4 (low). mutants.toml count=2. T1+T2 are additive test-fns; T4 is additive toml. No signature changes."}
```

**Depends on:** [P0368](plan-0368-cargo-mutants-baseline-failure.md) — baseline must pass or mutation phase never runs (0/0/0). [P0301](plan-0301-cargo-mutants-ci.md) — merged (`.#mutants` derivation + `.config/mutants.toml` exist).

**Conflicts with:** None. aterm.rs + wire/mod.rs are low-traffic. `.config/mutants.toml` also touched by P0368 T2-T3 (exit-code check, smoke test) but those edit the derivation buildPhase in flake.nix, not the toml scope config.

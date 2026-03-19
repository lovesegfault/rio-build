# Plan 0337: Consolidate DerivationNode fixture copies + NIXBASE32 → rio-test-support

Consolidator finding (mc-31 window). `rio_proto::types::DerivationNode` has **10 fields** — every test that constructs one inline lists all ten, even though only 2-3 differ from the default. [P0221](plan-0221-rio-bench-crate-hydra-doc.md) added the most recent full-struct copy at [`rio-bench/src/lib.rs:170-192`](../../rio-bench/src/lib.rs). The right pattern already exists at [`dag/tests.rs:8-12`](../../rio-scheduler/src/dag/tests.rs) and [`assignment.rs:431-434`](../../rio-scheduler/src/assignment.rs): spread-update over [`make_derivation_node()`](../../rio-test-support/src/fixtures.rs).

**The cost of inaction is concrete:** the next `DerivationNode` proto field addition is a **3× compile-break** (critical_path.rs:265, rio-bench/src/lib.rs:172, plus any drift since the consolidator scanned) instead of a **1× edit** (fixtures.rs:38). Proto field additions happen — `input_srcs_nar_size` was the most recent (see the [`DerivationNode`](../../rio-proto/proto/types.proto) message).

**Bonus find:** [`rio-bench/src/lib.rs:157`](../../rio-bench/src/lib.rs) defines `NIXBASE32` (the 32-char alphabet) + `rand_hash()` for per-iteration-unique store paths. [`rio-test-support/src/fixtures.rs:12`](../../rio-test-support/src/fixtures.rs) has `TEST_HASH = "aaaa...a"` — **fixed**, deterministic. These serve different needs: bench wants unique-per-call (criterion iterates, scheduler DAG dedupes on `drv_hash`, collisions skew latency low); unit tests want deterministic. Both belong in fixtures.rs.

## Entry criteria

- [P0221](plan-0221-rio-bench-crate-hydra-doc.md) merged — DONE. [`rio-bench/src/lib.rs`](../../rio-bench/src/lib.rs) exists.

## Tasks

### T1 — `feat(test-support):` add `NIXBASE32` + `rand_store_hash()` to fixtures

MODIFY [`rio-test-support/src/fixtures.rs`](../../rio-test-support/src/fixtures.rs) — insert after `TEST_HASH` at `:12`:

```rust
/// nixbase32 alphabet (0-9, a-z minus e/o/u/t). `StorePath::parse`
/// validates against exactly this set — a random ASCII-alphanumeric
/// string won't pass (the chars 'e','o','u','t' are rejected).
///
/// `pub` because rio-bench and property tests need it for generating
/// valid store paths on the fly.
pub const NIXBASE32: &[u8; 32] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// 32 random nixbase32 chars. A fresh valid store-path hash per call.
///
/// Use when tests need DISTINCT paths (criterion iterates;
/// scheduler DAG dedupes on `drv_hash`; collisions short-circuit the
/// merge path). Use [`TEST_HASH`] / [`test_store_path`] when
/// determinism matters (most unit tests).
pub fn rand_store_hash() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..32).map(|_| NIXBASE32[rng.random_range(0..32)] as char).collect()
}
```

`rio-test-support/Cargo.toml` already has `rand` (check at dispatch: `grep '^rand' rio-test-support/Cargo.toml` — if absent, add `rand = { workspace = true }`).

### T2 — `refactor(scheduler):` migrate critical_path.rs:264 to `..make_derivation_node()`

MODIFY [`rio-scheduler/src/critical_path.rs`](../../rio-scheduler/src/critical_path.rs) at `:264-277`:

Before (13 lines):
```rust
fn node(tag: &str) -> rio_proto::types::DerivationNode {
    rio_proto::types::DerivationNode {
        drv_path: test_drv_path(tag),
        drv_hash: tag.to_string(),
        pname: tag.to_string(),
        system: "x86_64-linux".into(),
        required_features: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        expected_output_paths: vec![],
        drv_content: Vec::new(),
        input_srcs_nar_size: 0,
    }
}
```

After (6 lines):
```rust
fn node(tag: &str) -> rio_proto::types::DerivationNode {
    rio_proto::types::DerivationNode {
        pname: tag.to_string(),  // overrides make_derivation_node's "test-pkg"
        ..rio_test_support::fixtures::make_derivation_node(tag, "x86_64-linux")
    }
}
```

The **only meaningful delta** from `make_derivation_node(tag, "x86_64-linux")` is `pname: tag` vs `pname: "test-pkg"`. Check at dispatch whether any critical_path test actually asserts on `pname` — if none do, the `pname` override can go too and this collapses to a single `make_derivation_node()` call (or better: delete `fn node()` entirely and use `make_derivation_node` at call sites).

Also collapse `fn edge()` at `:279-284` — it's byte-identical to [`make_edge()`](../../rio-test-support/src/fixtures.rs):

```rust
// Before: 6-line inline fn
// After:
use rio_test_support::fixtures::make_edge as edge;
```

### T3 — `refactor(bench):` migrate rio-bench synth_node to fixtures

MODIFY [`rio-bench/src/lib.rs`](../../rio-bench/src/lib.rs) at `:154-192`:

Delete `NIXBASE32` at `:157` and `rand_hash()` at `:163-168` (both move to T1). Replace `synth_node()` at `:170-192`:

```rust
fn synth_node(name: &str) -> DerivationNode {
    use rio_test_support::fixtures::{make_derivation_node, rand_store_hash};
    let hash = rand_store_hash();
    DerivationNode {
        // drv_hash must be globally unique within a bench run (across
        // criterion iterations) or MergeDag's dedup path fires and we
        // measure a cache hit, not a cold insert. make_derivation_node
        // uses the tag AS the hash — that's deterministic → collision.
        drv_hash: hash.clone(),
        drv_path: format!("/nix/store/{hash}-{name}.drv"),
        pname: name.into(),
        // expected_output_paths stays empty: the gateway would populate
        // this from the parsed .drv; we have none. The scheduler's
        // TOCTOU re-check skips empty, which is what we want.
        ..make_derivation_node(name, "x86_64-linux")
    }
}
```

**Preserve the two load-bearing comments** (drv_hash uniqueness, expected_output_paths empty). They explain WHY bench can't just call `make_derivation_node` directly.

`rio-bench` already has `rio-test-support = { workspace = true }` at [`Cargo.toml:12`](../../rio-bench/Cargo.toml). No Cargo changes.

### T4 — `refactor:` grep-sweep for remaining full-struct DerivationNode in test modules

The consolidator reported "3× copies" but T2+T3 covers 2. The third may have drifted (merged away, or consolidator double-counted). Grep at dispatch:

```bash
grep -rn 'DerivationNode {' rio-*/src rio-bench/src rio-*/tests | \
  grep -v 'rio-proto\|rio-test-support\|translate.rs' | \
  grep -v '\.\.make_derivation_node\|\.\.Default::default\|\.\.make_node'
```

Anything that survives this filter and is in a `#[cfg(test)]` block or a test crate → migrate the same way.

**Deliberately excluded:** [`translate.rs:358,391,610,632,637,992`](../../rio-gateway/src/translate.rs) — these are **production code** (the gateway's actual .drv→proto translation) and the test helpers at `:610-644,:992` already use `..Default::default()` (they only set 2 fields). Not consolidation targets.

## Exit criteria

- `/nbr .#ci` green
- `grep 'pub const NIXBASE32' rio-test-support/src/fixtures.rs` → 1 hit (T1)
- `grep 'pub fn rand_store_hash' rio-test-support/src/fixtures.rs` → 1 hit (T1)
- `grep 'NIXBASE32\|rand_hash' rio-bench/src/lib.rs` → 0 hits (T3: moved out)
- `grep -c '\.\.make_derivation_node\|\.\.rio_test_support::fixtures::make_derivation_node' rio-scheduler/src/critical_path.rs rio-bench/src/lib.rs` ≥ 2 (T2+T3: both sites use spread-update)
- **Proto-add simulation:** add a dummy field `repeated string _test_dummy = 99;` to `DerivationNode` in [`types.proto`](../../rio-proto/proto/types.proto), run `cargo build -p rio-scheduler -p rio-bench`. If BOTH compile cleanly → the 3×-break surface is closed. Then revert the proto change. (The struct-update `..` covers the new field.)
- T4 grep pipeline (see task) returns **only** lines in production code paths (`translate.rs` derivation_to_node, etc.). Zero test-module full-struct hits.

## Tracey

No new markers. This is test-fixture consolidation — no spec contract changes.

The migrated sites keep their existing tracey annotations: [`critical_path.rs`](../../rio-scheduler/src/critical_path.rs) tests carry `r[verify sched.critical-path.*]` markers that are unaffected (T2 changes the node constructor, not the test bodies).

## Files

```json files
[
  {"path": "rio-test-support/src/fixtures.rs", "action": "MODIFY", "note": "T1: +NIXBASE32 const +rand_store_hash() after :12"},
  {"path": "rio-test-support/Cargo.toml", "action": "MODIFY", "note": "T1: add rand dep if absent (check at dispatch)"},
  {"path": "rio-scheduler/src/critical_path.rs", "action": "MODIFY", "note": "T2: :264-284 — fn node()→..make_derivation_node, fn edge()→use make_edge"},
  {"path": "rio-bench/src/lib.rs", "action": "MODIFY", "note": "T3: :154-192 — delete NIXBASE32+rand_hash, synth_node uses fixtures"}
]
```

```
rio-test-support/
├── src/fixtures.rs          # T1: +NIXBASE32 +rand_store_hash
└── Cargo.toml               # T1: rand dep (conditional)
rio-scheduler/src/
└── critical_path.rs         # T2: :264-284 collapsed
rio-bench/src/
└── lib.rs                   # T3: :154-192 — fixtures import
```

## Dependencies

```json deps
{"deps": [221], "soft_deps": [], "note": "P0221 DONE — rio-bench/src/lib.rs exists. rio-bench already deps on rio-test-support (Cargo.toml:12, regular dep not dev-dep — fine, rio-bench is itself a bench crate). fixtures.rs:38 make_derivation_node already exists (unknown plan, check blame at dispatch if curious). critical_path.rs not in collisions top-30. rio-bench/src/lib.rs not in collisions top-30. Low-conflict refactor. discovered_from=consolidator mc-31 window (P0221 added the 3rd copy)."}
```

**Depends on:** [P0221](plan-0221-rio-bench-crate-hydra-doc.md) (DONE) — `rio-bench/src/lib.rs:170` `synth_node` exists.

**Conflicts with:** None. `rio-test-support/src/fixtures.rs` and `rio-scheduler/src/critical_path.rs` are not in `onibus collisions top 30`. `rio-bench/src/lib.rs` is P0221-only territory.

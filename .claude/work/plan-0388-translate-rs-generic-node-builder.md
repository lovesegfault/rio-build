# Plan 0388: translate.rs generic node-builder — collapse dual DerivationNode constructors

consol-mc190 convergent finding. [`translate.rs:346-391`](../../rio-gateway/src/translate.rs) `single_node_from_basic` and [`:395-429`](../../rio-gateway/src/translate.rs) `derivation_to_node` are near-identical 11-field `types::DerivationNode` builders. They share the same proto struct literal shape, same `node_common_fields` call, same `r[impl sched.ca.detect]` annotation (duplicated at `:385` and `:423`), and the same `is_fixed_output() || has_ca_floating_outputs()` disjunction. They differ only in which receiver they call the trait methods on (`basic_drv` vs `drv`) and in three comment blocks explaining *why* `drv_content`/`input_srcs_nar_size` are zeroed (fallback-simplicity vs populate-later).

[P0384](plan-0384-is-fixed-output-strict-plus-trait.md)'s `DerivationLike` trait at [`mod.rs:149`](../../rio-nix/src/derivation/mod.rs) makes these two functions unifiable: both `Derivation` and `BasicDerivation` implement the same accessor surface. The split into two functions dates from before the trait existed — when `is_fixed_output()` on `BasicDerivation` used a different (loose) predicate than `Derivation`'s (the very divergence P0384 fixed). With the trait landed, the two bodies are isomorphic.

**Divergence hazard (the motivating shape consol-mc190 flagged)**: the next proto field added to `DerivationNode` must be threaded into *both* builders. P0384 just patched this exact shape — `is_fixed_output` had diverged between the two paths. [P0250](plan-0250-ca-detect-plumb-is-ca.md) added `is_content_addressed` to both with identical comment blocks. The pattern repeats each time `types.proto:DerivationNode` grows a field. A single generic builder makes the divergence structurally impossible.

## Entry criteria

- [P0384](plan-0384-is-fixed-output-strict-plus-trait.md) merged (`DerivationLike` trait unifies the accessor surface — required for the `&impl DerivationLike` signature)
- [P0250](plan-0250-ca-detect-plumb-is-ca.md) merged (`is_content_addressed` field + dual `r[impl sched.ca.detect]` annotations exist at `:385`/`:423` — the duplication this plan collapses)

## Tasks

### T1 — `refactor(gateway):` extract generic build_node<D: DerivationLike> — collapse dual constructors

MODIFY [`rio-gateway/src/translate.rs`](../../rio-gateway/src/translate.rs). Replace the two builder bodies with one generic:

```rust
/// Build the proto `DerivationNode` for any `DerivationLike`. Both
/// `Derivation` (full BFS path) and `BasicDerivation` (single-node
/// fallback) route through here. drv_content/input_srcs_nar_size are
/// left zeroed — `filter_and_inline_drv()` and `populate_input_srcs_sizes()`
/// fill them AFTER FindMissingPaths/BFS batching (see call-site comments).
///
/// r[impl sched.ca.detect]
/// Both CA kinds: floating (hash_algo set, hash empty) and fixed-output
/// (hash also set). Cutoff applies to either — the output's nar_hash is
/// what gets compared, not the input addressing.
fn build_node<D: DerivationLike>(drv_path: &str, drv: &D) -> types::DerivationNode {
    let f = node_common_fields(
        drv.outputs()
            .iter()
            .map(|o| (o.name().to_string(), o.path().to_string())),
        drv.env(),
        drv.platform(),
    );
    types::DerivationNode {
        drv_path: drv_path.to_string(),
        drv_hash: drv_path.to_string(),
        pname: f.pname,
        system: f.system,
        required_features: f.required_features,
        output_names: f.output_names,
        is_fixed_output: drv.is_fixed_output(),
        expected_output_paths: f.expected_output_paths,
        drv_content: Vec::new(),
        input_srcs_nar_size: 0,
        is_content_addressed: drv.is_fixed_output() || drv.has_ca_floating_outputs(),
    }
}
```

Then:
- `single_node_from_basic(drv_path, basic_drv)` → `vec![build_node(drv_path, basic_drv)]` (one-liner; keep the fn and its `pub` signature for callers)
- `derivation_to_node(drv_path, drv)` → `build_node(&drv_path.to_string(), drv)` (one-liner wrapper OR inline the one caller — check at dispatch; `translate_dag` at `:~200` is the only caller)

The wrapper fns carry the *why*-comments (fallback-simplicity vs populate-later) that differed between the two original bodies. Those comments move to doc-comments on the wrapper fns, not inline in the struct literal (which is now shared).

**CARE — `&str` vs `&StorePath`**: `derivation_to_node` took `&StorePath`, `single_node_from_basic` took `&str`. The generic builder takes `&str` (both callers can `.to_string()` a `StorePath` or pass the string). Keep the wrapper signatures unchanged so callers don't churn.

**CARE — r[impl] marker singleton**: the `r[impl sched.ca.detect]` annotation moves from `:385`+`:423` (2 copies) to the one `build_node` doc-comment. `tracey query rule sched.ca.detect` should show one impl site post-refactor (three `r[verify]` sites at `:624/:656/:693` stay unchanged).

**Net-negative check**: both original bodies are ~35L each; `build_node` is ~25L + two 1-line wrappers. Expect `~-40L` net (matching the [P0318](plan-0318-riostackbuilder-tranche-2.md) NET-NEGATIVE builder precedent — "smaller than the N-constructor matrix it replaces").

### T2 — `test(gateway):` parametrize existing is_content_addressed tests over both DerivationLike impls

The three existing tests `test_single_node_is_content_addressed` (`:629`), `single_node_floating_ca_strict_fod_false` (`:656`), and `test_derivation_to_node_is_content_addressed` (`:697`) exercise the two paths separately. Post-T1 they test the SAME code. Keep all three (proves both impls route correctly through `build_node`) but add a comment explaining the shared-builder relationship:

```rust
// r[verify sched.ca.detect]
// Both Derivation and BasicDerivation route through build_node<D> —
// these tests prove the trait dispatch is correct for each impl (not
// that the builder logic differs; it doesn't). Regression guard for the
// pre-P0388 divergence shape: same proto field, two hand-rolled
// struct literals, drift on every DerivationNode field-add.
```

No new assertions — the existing tests already cover the matrix (IA/floating-CA/FOD × basic/full).

### T3 — `refactor(gateway):` fold node_common_fields into build_node

`node_common_fields` at [`:247-274`](../../rio-gateway/src/translate.rs) exists ONLY to share field extraction between the two builders. Post-T1 there's one builder — the intermediate `NodeCommonFields` struct and `node_common_fields` fn become a single-caller abstraction. Inline them into `build_node`:

```rust
fn build_node<D: DerivationLike>(drv_path: &str, drv: &D) -> types::DerivationNode {
    let (output_names, expected_output_paths): (Vec<_>, Vec<_>) = drv
        .outputs()
        .iter()
        .map(|o| (o.name().to_string(), o.path().to_string()))
        .unzip();
    let env = drv.env();
    types::DerivationNode {
        // ... (pname/system/required_features extraction inlined
        //      from node_common_fields body — the :256-272 logic)
    }
}
```

Delete `struct NodeCommonFields` at `:~238` and `fn node_common_fields` at `:247`. The pname→name fallback comment at `:256-262` moves inline. **Net: another ~-30L.**

T3 is **optional** — commit T1+T2 first (the load-bearing collapse), T3 is the cleanup. If `node_common_fields` has test coverage that exercises it independently, keep it and skip T3.

## Exit criteria

- `/nbr .#ci` green
- `grep -c 'r\[impl sched.ca.detect\]' rio-gateway/src/translate.rs` → 1 (was 2 — duplicate collapsed)
- `grep 'fn build_node\|build_node<' rio-gateway/src/translate.rs` → ≥1 hit (generic builder exists)
- `grep -c 'types::DerivationNode {' rio-gateway/src/translate.rs` → 1 (was 2 — single struct-literal site)
- `git diff --stat HEAD~3 -- rio-gateway/src/translate.rs` → net negative (consol-mc190's "next proto field threads both" claim is structurally closed: one literal = one thread point)
- `cargo nextest run -p rio-gateway translate` → all existing `is_content_addressed` + `is_fixed_output` tests pass unchanged
- `nix develop -c tracey query rule sched.ca.detect` → 1 impl site, ≥3 verify sites (same count as before — verify sites unchanged, impl sites collapsed)
- Optional T3: `grep 'NodeCommonFields\|node_common_fields' rio-gateway/src/translate.rs` → 0 (inlined)

## Tracey

References existing markers:
- `r[sched.ca.detect]` — T1 consolidates two `r[impl]` annotations into one (the marker at [`scheduler.md:256`](../../docs/src/components/scheduler.md) already describes "at gateway translate"; this plan doesn't change WHAT is detected, just collapses WHERE the annotation lives)
- `r[gw.dag.reconstruct]` — T1 touches code under this marker's scope (top-of-file annotation at [`translate.rs:6`](../../rio-gateway/src/translate.rs)); no change to the DAG-reconstruction semantics

No new markers. This is pure structural deduplication — the generic builder produces byte-identical `DerivationNode` values for the same inputs. No spec behavior changes.

## Files

```json files
[
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T1: +build_node<D: DerivationLike> generic builder; collapse single_node_from_basic + derivation_to_node bodies to one-liner wrappers; r[impl sched.ca.detect] 2→1. T2: shared-builder comment on existing tests. T3: delete NodeCommonFields + inline node_common_fields (optional cleanup)"}
]
```

```
rio-gateway/src/
└── translate.rs   # T1: +build_node<D: DerivationLike> (~25L), wrappers (2×1L), delete dual literals (~70L)
                   # T2: comment on existing r[verify] tests
                   # T3: inline node_common_fields, delete NodeCommonFields (~-30L)
```

## Dependencies

```json deps
{"deps": [384, 250], "soft_deps": [304], "note": "P0384 provides DerivationLike trait (required for &impl DerivationLike signature). P0250 added is_content_addressed field + dual r[impl] annotations at :385/:423 (the duplication being collapsed). discovered_from=consol-mc190. Soft-dep P0304: T144 in P0304 adds is_content_addressed() as DerivationLike trait default — if T144 lands first, build_node's `drv.is_fixed_output() || drv.has_ca_floating_outputs()` becomes `drv.is_content_addressed()`. Sequence-independent: both forms compile, T144-first is cleaner. translate.rs collision count=24 (HOT) — this plan's edits are net-subtractive and localized to :238-430; low semantic conflict (no signature changes to pub fns)."}
```

**Depends on:** [P0384](plan-0384-is-fixed-output-strict-plus-trait.md) (DONE — `DerivationLike` at [`mod.rs:149`](../../rio-nix/src/derivation/mod.rs)), [P0250](plan-0250-ca-detect-plumb-is-ca.md) (DONE — `is_content_addressed` field).

**Conflicts with:** [`translate.rs`](../../rio-gateway/src/translate.rs) collision count=24. This plan edits `:238-430` (net-subtractive). [P0304](plan-0304-trivial-batch-p0222-harness.md)-T144 touches [`mod.rs:149+`](../../rio-nix/src/derivation/mod.rs) (trait default add) — different file. No other UNIMPL plan touches the `:346-429` builder region.

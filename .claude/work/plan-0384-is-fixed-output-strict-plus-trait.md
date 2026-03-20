# Plan 0384: is_fixed_output strict-vs-loose consistency + DerivationLike trait

Two cadence agents converged on the same fault-line (bughunt-mc175 + consol-mc175). [`translate.rs:368`](../../rio-gateway/src/translate.rs) populates `DerivationNode.is_fixed_output` via the **loose** per-output predicate `basic_drv.outputs().iter().any(|o| o.is_fixed_output())` — true whenever any output has `hash_algo` set, regardless of whether `hash` is also set or the output is named `out`. [`translate.rs:407`](../../rio-gateway/src/translate.rs) populates the **same** proto field via the **strict** aggregate predicate `drv.is_fixed_output()` — true only for single-`out`-output derivations with both `hash_algo` AND `hash` set.

For a floating-CA derivation arriving through the single-node fallback (`single_node_from_basic`), `:368` evaluates TRUE (floating-CA has `hash_algo` set, `hash` empty — `DerivationOutput::is_fixed_output()` at [`mod.rs:130`](../../rio-nix/src/derivation/mod.rs) only checks `hash_algo`). The worker at [`executor/mod.rs:344`](../../rio-worker/src/executor/mod.rs) recomputes the strict form `drv.is_fixed_output()` → FALSE. The disagreement `warn!` at `:346-351` fires spuriously. This blocks deprecation of the `wkr-fod-flag-trust` assignment-field read (per [`21-p2-p3-rollup.md:64`](../../docs/src/remediations/phase4a/21-p2-p3-rollup.md) — "remove in a follow-up once this warn! has been silent in prod").

Intertwined with consol-mc175's structural finding: `Derivation` and `BasicDerivation` carry eight byte-identical methods ([`mod.rs:184-237`](../../rio-nix/src/derivation/mod.rs) vs [`mod.rs:288-336`](../../rio-nix/src/derivation/mod.rs)) — `outputs()`, `input_srcs()`, `platform()`, `builder()`, `args()`, `env()`, `is_fixed_output()`, `has_ca_floating_outputs()`. [P0250](plan-0250-ca-detect-plumb-is-ca.md) added the latter two to `BasicDerivation` by copying; its dispatch-note at `:11` flagged the `DerivationOutput::is_fixed_output()` misnomer but deferred any rename. A trait with `fn outputs() -> &[DerivationOutput]` + default predicate impls makes the duplication structurally impossible and forces `:368` to call the same codepath as `:407`.

## Entry criteria

- [P0250](plan-0250-ca-detect-plumb-is-ca.md) merged (added `is_fixed_output()` + `has_ca_floating_outputs()` to `impl BasicDerivation` — the duplication this plan collapses)

## Tasks

### T1 — `refactor(nix):` extract DerivationLike trait — outputs() + default predicates

Add `pub trait DerivationLike` to [`rio-nix/src/derivation/mod.rs`](../../rio-nix/src/derivation/mod.rs). Required methods mirror the existing accessors; `is_fixed_output()` and `has_ca_floating_outputs()` become default-impl'd (operating on `self.outputs()`):

```rust
/// Common accessor surface for [`Derivation`] and [`BasicDerivation`].
///
/// The two structs differ only in `input_drvs` (DAG edges — absent in
/// wire `BasicDerivation`). All output-predicate logic operates on
/// `outputs()` alone, so it lives here as default impls to avoid the
/// byte-duplication that [P0250](plan-0250-ca-detect-plumb-is-ca.md)
/// flagged-but-deferred.
pub trait DerivationLike {
    fn outputs(&self) -> &[DerivationOutput];
    fn input_srcs(&self) -> &std::collections::BTreeSet<String>;
    fn platform(&self) -> &str;
    fn builder(&self) -> &str;
    fn args(&self) -> &[String];
    fn env(&self) -> &std::collections::BTreeMap<String, String>;

    /// Strict FOD predicate: single output named `out` with both
    /// `hash_algo` AND `hash` set.
    ///
    /// Contrast [`DerivationOutput::is_fixed_output`] which is the
    /// loose per-output "has hash_algo" check (covers floating-CA too).
    /// Callers wanting "is this drv a FOD" MUST use this; callers
    /// wanting "is this output content-addressed in any way" use the
    /// per-output predicate.
    fn is_fixed_output(&self) -> bool {
        let outs = self.outputs();
        outs.len() == 1
            && outs[0].name() == "out"
            && !outs[0].hash_algo().is_empty()
            && !outs[0].hash().is_empty()
    }

    /// Any output is CA-floating (hash_algo set, hash empty).
    fn has_ca_floating_outputs(&self) -> bool {
        self.outputs()
            .iter()
            .any(|o| !o.hash_algo().is_empty() && o.hash().is_empty())
    }
}
```

Then:
- `impl DerivationLike for Derivation { ... }` — six required methods delegate to fields; delete the inherent `is_fixed_output` at `:222-227` and `has_ca_floating_outputs` at `:233-237` (trait defaults now own them)
- `impl DerivationLike for BasicDerivation { ... }` — same; delete inherent duplicates at `:321-326` and `:332-336`
- Re-export the trait from `rio-nix/src/lib.rs` (`pub use derivation::DerivationLike`)

**CARE — inherent-method priority:** Rust resolves inherent methods before trait methods. Deleting the inherent copies is REQUIRED, not optional — if both exist, existing call-sites continue to resolve to inherent, and the trait defaults are dead code. After deletion, every caller needs `use rio_nix::DerivationLike;` in scope (trait methods aren't callable without the trait imported).

**CARE — accessor-preservation choice:** Keeping inherent `outputs()`/`platform()`/etc alongside trait impls avoids churning call-sites (inherent wins, trait is a secondary surface for generic code). Alternative: move all to trait-only (single source of truth, more import churn). Prefer **keeping inherent accessors + deleting inherent predicates** — minimizes import-scope changes to just the two predicate call-sites.

**Callers needing `use`-import** (verify at dispatch via `cargo check`): [`hash.rs:66,82`](../../rio-nix/src/derivation/hash.rs) (`drv.is_fixed_output()`, `drv.has_ca_floating_outputs()`) — inside rio-nix so `use super::DerivationLike` or `use crate::derivation::DerivationLike`. [`translate.rs:384,407,422`](../../rio-gateway/src/translate.rs) — `use rio_nix::DerivationLike` (or narrow-scope `use rio_nix::derivation::DerivationLike`). [`executor/mod.rs:344`](../../rio-worker/src/executor/mod.rs) — `use rio_nix::DerivationLike`. Tests at `hash.rs:142,163-164,291-292,419` + `aterm.rs:480,512` also call the predicates.

### T2 — `fix(gateway):` translate.rs:368 — use strict basic_drv.is_fixed_output()

Change `:368` in `single_node_from_basic`:

```rust
// BEFORE: loose — matches floating-CA too (DerivationOutput::is_fixed_output
// only checks hash_algo, not hash)
is_fixed_output: basic_drv.outputs().iter().any(|o| o.is_fixed_output()),

// AFTER: strict — same predicate as :407 (drv.is_fixed_output()).
// Floating-CA via fallback: was TRUE here (loose), worker recomputes
// FALSE (strict) → spurious warn! at executor/mod.rs:346. Now consistent.
is_fixed_output: basic_drv.is_fixed_output(),
```

With T1 landed, `basic_drv.is_fixed_output()` resolves to the trait default (strict). The `:384` line already uses `basic_drv.is_fixed_output()` for `is_content_addressed` — that stays correct (floating-CA is caught by the `|| basic_drv.has_ca_floating_outputs()` disjunct).

### T3 — `test(gateway):` floating-CA via fallback — is_fixed_output=false, is_content_addressed=true

Add to the `#[cfg(test)] mod tests` block in [`translate.rs`](../../rio-gateway/src/translate.rs) (near `:632-679` where the existing `is_content_addressed` tests live):

```rust
// r[verify sched.ca.detect]
// Floating-CA via single-node fallback: is_fixed_output MUST be false
// (strict predicate — hash_algo set but hash empty doesn't qualify),
// is_content_addressed MUST be true (either-kind disjunct). Pre-fix,
// :368 used the loose per-output predicate → is_fixed_output was TRUE
// here, diverging from the full-DAG path at :407. Worker's strict
// recompute at executor/mod.rs:344 saw FALSE; warn! at :346 fired.
#[test]
fn single_node_floating_ca_strict_fod_false() {
    let basic = floating_ca_basic_drv();  // hash_algo="sha256", hash=""
    let nodes = single_node_from_basic("/nix/store/abc-floating.drv", &basic);
    assert_eq!(nodes.len(), 1);
    assert!(
        !nodes[0].is_fixed_output,
        "floating-CA via fallback: strict FOD predicate → false (hash is empty)"
    );
    assert!(
        nodes[0].is_content_addressed,
        "floating-CA via fallback: is_ca true via has_ca_floating_outputs()"
    );
}
```

Reuse or mirror the floating-CA fixture from `:632` (if that block builds a `Derivation`, add a `BasicDerivation` counterpart or use `drv.to_basic()` — the latter is `#[cfg(test)]` at [`mod.rs:162`](../../rio-nix/src/derivation/mod.rs) so fine from a test).

**Mutation check:** revert `:368` to `basic_drv.outputs().iter().any(|o| o.is_fixed_output())` → T3 MUST fail (is_fixed_output=true when it should be false). This is the regression proof that T2 is load-bearing.

## Exit criteria

- `cargo nextest run -p rio-nix` → pass (trait default impls match inherent-method semantics)
- `cargo nextest run -p rio-gateway single_node_floating_ca_strict_fod_false` → pass
- `grep -c 'pub fn is_fixed_output' rio-nix/src/derivation/mod.rs` → 1 (only `DerivationOutput::is_fixed_output` inherent remains; trait default is `fn is_fixed_output`, not `pub fn`)
- `grep 'outputs().iter().any(|o| o.is_fixed_output())' rio-gateway/src/translate.rs` → 0 hits (loose form removed)
- `grep 'basic_drv.is_fixed_output()' rio-gateway/src/translate.rs` → ≥2 hits (`:368` fix + existing `:384`)
- Mutation: `sed -i '368s/basic_drv.is_fixed_output()/basic_drv.outputs().iter().any(|o| o.is_fixed_output())/' rio-gateway/src/translate.rs && cargo nextest run -p rio-gateway single_node_floating_ca_strict_fod_false` → FAIL (then revert)
- `cargo clippy --all-targets -- --deny warnings` → pass (no unused-import warnings on the new `use DerivationLike` additions)

## Tracey

References existing markers:
- `r[sched.ca.detect]` — T2+T3 tighten the implementation; T3 adds a `verify` site specific to the fallback-path consistency. The marker text at [`scheduler.md:256-258`](../../docs/src/components/scheduler.md) already describes `is_fixed_output() || has_ca_floating_outputs()`; this plan makes both translate-paths evaluate that consistently.
- `r[worker.fod.verify-hash]` — indirect: the `:345` disagreement-warn! references `assignment.is_fixed_output`; this plan's fix makes the assignment-side value match worker-side recompute for floating-CA. No new `r[impl]`/`r[verify]` — the FOD hash-verify behavior is unchanged; only the proto-flag population path is tightened.

## Files

```json files
[
  {"path": "rio-nix/src/derivation/mod.rs", "action": "MODIFY", "note": "T1: +DerivationLike trait with default is_fixed_output/has_ca_floating_outputs; delete inherent predicate copies :222-237 (Derivation) + :321-336 (BasicDerivation); impl trait for both"},
  {"path": "rio-nix/src/lib.rs", "action": "MODIFY", "note": "T1: pub use derivation::DerivationLike (re-export)"},
  {"path": "rio-nix/src/derivation/hash.rs", "action": "MODIFY", "note": "T1: +use super::DerivationLike (trait-method resolution for :66,:82 + tests)"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T1: +use rio_nix::DerivationLike; T2: :368 loose→strict; T3: +single_node_floating_ca_strict_fod_false test (HOT count=23)"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "T1: +use rio_nix::DerivationLike (trait-method resolution for :344 drv.is_fixed_output())"}
]
```

```
rio-nix/src/
├── derivation/
│   ├── mod.rs        # T1: +trait DerivationLike, delete dup predicates
│   └── hash.rs       # T1: +use super::DerivationLike
└── lib.rs            # T1: re-export
rio-gateway/src/translate.rs  # T1: use-import; T2: :368 strict; T3: test
rio-worker/src/executor/mod.rs  # T1: use-import
```

## Dependencies

```json deps
{"deps": [250], "soft_deps": [], "note": "discovered_from=bughunt-mc175+consol-mc175 (2-agent convergence). P0250 (DONE) added is_fixed_output()+has_ca_floating_outputs() to impl BasicDerivation — copy-pasted from impl Derivation, per its dispatch-note at :9. P0250:11 flagged DerivationOutput::is_fixed_output() misnomer (semantically is_content_addressed) but deferred — this plan doesn't rename (still deferred; trait-default's doc-comment at T1 documents the distinction). The :368 loose-vs-:407 strict divergence predates P0250 but P0250 made both paths uniformly call predicate methods — just not the SAME predicate. HOT FILES: translate.rs count=23 (T1+T2+T3 all additive/single-line; existing P0295-T75 edits :547 different section; P0250 edits :379+413 adjacent to T2's :368 — P0250 DONE so no live conflict). executor/mod.rs count~15 (T1 is a single use-import at file top — additive)."}
```

**Depends on:** [P0250](plan-0250-ca-detect-plumb-is-ca.md) (DONE) — added the duplicate predicate methods this plan collapses.

**Conflicts with:** [`translate.rs`](../../rio-gateway/src/translate.rs) count=23 — T2 edits `:368`, T3 adds test near `:632`; P0295-T75 edits `:547` (different TODO-tag); all non-overlapping. [`derivation/mod.rs`](../../rio-nix/src/derivation/mod.rs) low-traffic — no UNIMPL plan touches it post-P0250. [`executor/mod.rs`](../../rio-worker/src/executor/mod.rs) — T1 is a single `use` line at top; [P0304-T59](plan-0304-trivial-batch-p0222-harness.md) edits `:442` caller — non-overlapping.

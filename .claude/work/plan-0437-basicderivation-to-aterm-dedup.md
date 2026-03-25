# Plan 437: BasicDerivation::to_aterm — ATerm dedup post-P0398

consol-mc242 sharpening of [P0304-T158/T174](plan-0304-trivial-batch-p0222-harness.md). Those tracked the `serialize_resolved` vs `to_aterm` ATerm-serialization duplication and proposed `Derivation::to_aterm_resolved(drop_input_drvs, extra_srcs)` as the consolidation target. [P0398](plan-0398-ca-resolve-drop-all-inputdrvs.md) has since landed and changed `serialize_resolved` semantics to **inputDrvs-ALWAYS-empty** (BasicDerivation slice-copy per Nix `tryResolve` at `derivations.cc:1204`). This makes the type-correct consolidation target `BasicDerivation::to_aterm()` — not a parameterized `Derivation` method.

`rio-nix` already has the pieces: [`BasicDerivation` struct](../../rio-nix/src/derivation/mod.rs) at `:291`, [`Derivation::to_basic()` converter](../../rio-nix/src/derivation/mod.rs) at `:216`, [`write_aterm_tail`](../../rio-nix/src/derivation/aterm.rs) shared tail-writer. Missing only: `BasicDerivation::to_aterm()`. The crate-private `write_aterm_string` copy at [`resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) (the escape-table dup T158 called out) goes away entirely.

**Supersedes P0304-T158 route-(a) and T174's `to_aterm_with` proposal.** Those proposed `Derivation::to_aterm_resolved` / `to_aterm_with(outputs_mask, input_drvs_transform, extra_input_srcs)` — but post-P0398 the resolved ATerm is semantically a `BasicDerivation` (no inputDrvs), so the method belongs on that type. Simpler signature, no parameterization.

## Entry criteria

- [P0398](plan-0398-ca-resolve-drop-all-inputdrvs.md) merged (`serialize_resolved` has `inputDrvs = []` unconditionally)
- [P0428](plan-0428-ca-resolve-ia-inputsrcs-expected-paths.md) merged (`resolve_ca_inputs` collects IA output paths — the `extra_srcs` that feed into `BasicDerivation`'s `input_srcs`)

## Tasks

### T1 — `feat(nix):` BasicDerivation::to_aterm — reuse write_aterm_tail, empty inputDrvs

MODIFY [`rio-nix/src/derivation/aterm.rs`](../../rio-nix/src/derivation/aterm.rs). Add `impl BasicDerivation { pub fn to_aterm(&self) -> String }` alongside the existing `Derivation::to_aterm` at `:288`:

```rust
impl BasicDerivation {
    /// Serialize to ATerm format. `inputDrvs` is always empty by
    /// construction (BasicDerivation = post-resolution / wire-slice).
    /// Mirrors Nix `BasicDerivation::unparse` at derivations.cc.
    pub fn to_aterm(&self) -> String {
        let mut out = String::with_capacity(2048);
        out.push_str("Derive([");
        // outputs-loop — identical to Derivation::to_aterm :293-309
        for (i, o) in self.outputs.iter().enumerate() {
            if i > 0 { out.push(','); }
            out.push('(');
            write_aterm_string(&mut out, &o.name);
            // ... path, hash_algo, hash ...
            out.push(')');
        }
        out.push_str("],[");
        // inputDrvs — ALWAYS EMPTY for BasicDerivation
        out.push_str("],");
        self.write_aterm_tail(&mut out);
        out
    }
}
```

`write_aterm_tail` currently takes `&mut String` + an optional inputSrcs override. `BasicDerivation` holds `input_srcs` directly — if `write_aterm_tail` needs the srcs as a parameter (check at dispatch whether it reads `self.input_srcs` or takes a slice), either (a) add `BasicDerivation::write_aterm_tail` that forwards, or (b) make the existing tail-writer take `input_srcs: &[String]` explicitly. Route-(b) cleaner — it already parameterizes the override.

### T2 — `feat(nix):` BasicDerivation::from_resolved constructor — extra_srcs merge

MODIFY [`rio-nix/src/derivation/mod.rs`](../../rio-nix/src/derivation/mod.rs). Add near `BasicDerivation::new` at `:306`:

```rust
impl BasicDerivation {
    /// Build a resolved BasicDerivation from a full Derivation +
    /// extra input_srcs (realized CA paths + IA expected outputs).
    /// The merge sorts-and-dedups the union, matching Nix tryResolve.
    pub fn from_resolved(
        drv: &Derivation,
        extra_srcs: impl IntoIterator<Item = String>,
    ) -> Self {
        let mut srcs: Vec<String> = drv.input_srcs.iter().cloned()
            .chain(extra_srcs)
            .collect();
        srcs.sort_unstable();
        srcs.dedup();
        let mut basic = drv.to_basic();
        basic.input_srcs = srcs;
        basic
    }
}
```

### T3 — `refactor(scheduler):` serialize_resolved → BasicDerivation::from_resolved().to_aterm()

MODIFY [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs). Replace `serialize_resolved` body (post-P0398, post-P0428 state):

```rust
pub(crate) fn serialize_resolved(
    drv: &Derivation,
    extra_srcs: Vec<String>,  // realized CA + IA expected (P0428)
) -> String {
    BasicDerivation::from_resolved(drv, extra_srcs).to_aterm()
}
```

Delete the crate-private `write_aterm_string` copy (the escape-table dup) and the ~100L manual ATerm construction. Keep the doc-comment referencing Nix `tryResolve` for archaeology.

### T4 — `test(nix):` BasicDerivation::to_aterm round-trip vs Derivation::to_aterm

MODIFY [`rio-nix/src/derivation/aterm.rs`](../../rio-nix/src/derivation/aterm.rs) `cfg(test)` mod. Add:

```rust
#[test]
fn basic_derivation_to_aterm_matches_empty_inputdrvs() {
    // A Derivation with input_drvs=[] should produce byte-identical
    // ATerm whether serialized via Derivation::to_aterm or
    // to_basic().to_aterm().
    let drv = Derivation { input_drvs: BTreeMap::new(), /* ... */ };
    assert_eq!(drv.to_aterm(), drv.to_basic().to_aterm());
}
```

### T5 — `test(scheduler):` serialize_resolved still matches pre-refactor golden

MODIFY [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) `cfg(test)`. The existing `serialize_resolved_drops_all_inputdrvs` (P0398) and IA-inputSrcs tests (P0428-T5) should pass unchanged — no new test needed, just verify they still pass. If [P0311-T62](plan-0311-test-gap-batch-cli-recovery-dash.md) (golden ATerm vs Nix `tryResolve`) landed, that's the byte-exact proof.

### T6 — `docs(plan):` P0304-T158/T174 — mark superseded by this plan

MODIFY [`.claude/work/plan-0304-trivial-batch-p0222-harness.md`](plan-0304-trivial-batch-p0222-harness.md). At T158 (`:2520`) and T174 (`:2668`), add a blockquote:

> **Superseded by [P0437](plan-0437-basicderivation-to-aterm-dedup.md).** Post-P0398, the resolved ATerm is semantically a `BasicDerivation`; `BasicDerivation::to_aterm()` is the type-correct home, not `Derivation::to_aterm_resolved`.

## Exit criteria

- `/nbr .#ci` green
- `grep 'fn write_aterm_string' rio-scheduler/src/ca/resolve.rs` → 0 hits (crate-private escape-table copy deleted)
- `grep 'BasicDerivation::to_aterm\|fn to_aterm' rio-nix/src/derivation/aterm.rs` → ≥2 hits (`Derivation::to_aterm` + `BasicDerivation::to_aterm`)
- `wc -l rio-scheduler/src/ca/resolve.rs` — net ~-110L from current
- Existing `serialize_resolved_drops_all_inputdrvs` test passes unchanged (behavior preserved)
- `basic_derivation_to_aterm_matches_empty_inputdrvs` passes

## Tracey

References existing markers:
- `r[sched.ca.resolve+2]` — T3 preserves the resolve semantics (implementation moves to rio-nix, behavior unchanged). The existing `r[impl sched.ca.resolve]` at `resolve.rs` stays — `serialize_resolved` still exists, just with a 3-line body.

No new markers. Pure refactor — moves ~110L from rio-scheduler to rio-nix without behavior change. The ATerm format itself is Nix-upstream-defined, not rio-spec-defined.

## Files

```json files
[
  {"path": "rio-nix/src/derivation/aterm.rs", "action": "MODIFY", "note": "T1: +BasicDerivation::to_aterm impl block (~20L). T4: +round-trip test. Adjusts write_aterm_tail signature if needed for route-(b)"},
  {"path": "rio-nix/src/derivation/mod.rs", "action": "MODIFY", "note": "T2: +BasicDerivation::from_resolved constructor near :306 (~15L)"},
  {"path": "rio-scheduler/src/ca/resolve.rs", "action": "MODIFY", "note": "T3: serialize_resolved body → 1-liner; delete crate-private write_aterm_string (~-110L net). T5: verify existing tests pass"},
  {"path": ".claude/work/plan-0304-trivial-batch-p0222-harness.md", "action": "MODIFY", "note": "T6: T158/T174 superseded-by blockquote"}
]
```

```
rio-nix/src/derivation/
├── aterm.rs              # T1: +BasicDerivation::to_aterm  T4: test
└── mod.rs                # T2: +from_resolved constructor
rio-scheduler/src/ca/
└── resolve.rs            # T3: serialize_resolved → 3L; -write_aterm_string
```

## Dependencies

```json deps
{"deps": [398, 428], "soft_deps": [304, 311], "note": "consol-mc242 finding (discovered_from=consolidator). Hard-dep P0398 (inputDrvs-always-empty makes BasicDerivation the right type). Hard-dep P0428 (IA-input collection feeds from_resolved's extra_srcs). Soft-dep P0304 (T6 edits T158/T174 — superseded-by blockquote, no behavior change). Soft-dep P0311-T62 (golden ATerm test — if landed, T5 is a free byte-exact verification). resolve.rs is moderate-traffic post-P0428; aterm.rs low-traffic. Sequence AFTER P0428 drains to avoid rebase on the extra_srcs plumbing."}
```

**Depends on:** [P0398](plan-0398-ca-resolve-drop-all-inputdrvs.md) — `inputDrvs = []` semantics. [P0428](plan-0428-ca-resolve-ia-inputsrcs-expected-paths.md) — IA-input collection.
**Conflicts with:** [`resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) — [P0311-T62](plan-0311-test-gap-batch-cli-recovery-dash.md) golden test touches `cfg(test)` mod; T3 touches fn body above it. Non-overlapping. [`aterm.rs`](../../rio-nix/src/derivation/aterm.rs) — low-traffic, no active plan touches it post-P0398.

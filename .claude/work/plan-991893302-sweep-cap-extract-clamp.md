# Plan 991893302: extract `sweep_cap()` with i32-clamp — closes bughunt + test-gap convergence

Two independent analyses converged on [`manifest.rs:228`](../../rio-controller/src/reconcilers/builderpool/manifest.rs):

**Bughunter (mc=70):** `FAILED_SWEEP_MIN.max(wp.spec.replicas.max as usize)` — `i32 → usize` cast is unclamped. `replicas.max` is `i32` with no CEL/schema floor (only ephemeral mode gates `max > 0` at [`builderpool.rs:74`](../../rio-crds/src/builderpool.rs); manifest mode has only `min <= max`). `-1_i32 as usize` wraps to `usize::MAX`; `sweep_cap` goes unbounded → an 8640-job backlog fires 8640 delete RPCs in one tick.

Contrast [`:325`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) (~97 lines later, same `replicas.max` field via `ceiling`): `ceiling.saturating_sub(active_total).max(0) as usize` — the `.max(0)` clamp pattern already exists in this file.

**P0515 reviewer (refiled — lost in coordinator truncate at mc=70):** the `sweep_cap` formula is inline; tests restate `std::cmp::max` semantics rather than calling the site. A `.max()` → `.min()` typo at `:228` passes every unit test but re-introduces the [P0511](plan-0511-manifest-failed-job-sweep.md) divergence. The VM test runs a small pool (`replicas.max ≤ 4`), never hits `replicas.max > FAILED_SWEEP_MIN` (20).

**One extraction closes both:** `fn sweep_cap(replicas_max: i32) -> usize { FAILED_SWEEP_MIN.max(replicas_max.max(0) as usize) }` — clamped, named, testable. Introduced at P0515 (`1a0a4c2`). This plan CONSUMES the test-gap followup; the extraction IS the test-gap fix. discovered_from=515. origin=bughunter+reviewer.

## Entry criteria

- [P0515](plan-0515-manifest-sweep-cap-divergence.md) merged (introduced the inline `sweep_cap` formula at `:228`)

## Tasks

### T1 — `fix(controller):` extract `sweep_cap()` with i32 clamp

Add near [`FAILED_SWEEP_MIN`](../../rio-controller/src/reconcilers/builderpool/manifest.rs) at `:131`:

```rust
/// Per-tick Failed-Job sweep cap. Tracks `replicas.max` so the sweep
/// converges under full crash-loop (net accumulation ≤ 0 per tick:
/// at most `replicas.max` Failed Jobs spawn, at most that many swept).
/// Floors at FAILED_SWEEP_MIN for small pools — even replicas.max=2
/// gets a 20/tick sweep so a short burst clears quickly.
///
/// The `.max(0)` clamp: `replicas.max` is i32 (k8s typed
/// `IntOrString` backing type); negative values have no CEL floor in
/// manifest mode. `-1_i32 as usize` wraps to `usize::MAX`.
// r[impl ctrl.pool.manifest-failed-sweep]
pub(super) fn sweep_cap(replicas_max: i32) -> usize {
    FAILED_SWEEP_MIN.max(replicas_max.max(0) as usize)
}
```

Replace [`:228`](../../rio-controller/src/reconcilers/builderpool/manifest.rs):

```rust
// Before:
let sweep_cap = FAILED_SWEEP_MIN.max(wp.spec.replicas.max as usize);
// After:
let cap = sweep_cap(wp.spec.replicas.max);
```

Rename the local binding `sweep_cap` → `cap` to avoid shadowing the function (used at `:247` comment and `:270` event message interpolation).

### T2 — `test(controller):` `sweep_cap` boundary table — clamp, floor, ceiling, typo-canary

In [`manifest_tests.rs`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs):

```rust
/// Boundary table for sweep_cap. The `.max(0)` clamp (row 3) guards
/// i32→usize wrap; the `replicas.max > FAILED_SWEEP_MIN` case (row 5)
/// is the ceiling-tracks-spawn-rate property that VM tests never hit
/// (small pools only). Row 4 is the P0511-divergence canary: if
/// .max() → .min() at the call site, `sweep_cap(4)` → 4 not 20.
// r[verify ctrl.pool.manifest-failed-sweep]
#[test]
fn sweep_cap_boundaries() {
    use super::super::manifest::{sweep_cap, FAILED_SWEEP_MIN};
    assert_eq!(FAILED_SWEEP_MIN, 20, "doc-const sync");
    // (replicas_max, expected, what-it-proves)
    let cases = [
        (0, 20, "zero → floor"),
        (-1, 20, "negative clamped (i32→usize wrap guard)"),
        (i32::MIN, 20, "MIN clamped"),
        (4, 20, "small pool → floor (P0511 canary: .min typo → 4)"),
        (20, 20, "equal to floor"),
        (100, 100, "ceiling tracks spawn rate — VM never hits this"),
        (i32::MAX, i32::MAX as usize, "MAX passes through"),
    ];
    for (replicas_max, want, why) in cases {
        assert_eq!(sweep_cap(replicas_max), want, "{why}");
    }
}
```

The `.max → .min` typo check is row 4: if someone writes `FAILED_SWEEP_MIN.min(...)`, `sweep_cap(4)` returns 4 not 20 → test fails with "small pool → floor". The wrap-guard is row 2: without `.max(0)`, `sweep_cap(-1)` returns `usize::MAX` not 20.

## Exit criteria

- `/nbr .#ci` green
- `grep 'fn sweep_cap' rio-controller/src/reconcilers/builderpool/manifest.rs` → 1 hit (extraction done)
- `grep 'FAILED_SWEEP_MIN.max(wp\|replicas.max as usize' rio-controller/src/reconcilers/builderpool/manifest.rs` → 0 hits (inline formula gone — only call to `sweep_cap(...)` remains)
- `cargo nextest run -p rio-controller sweep_cap_boundaries` → 1 passed; 7 cases assert
- Mutation check: `.max(0)` → (deleted) at the extraction → test FAILS at row 2 (`negative clamped`) with actual `18446744073709551615` (usize::MAX)
- Mutation check: `FAILED_SWEEP_MIN.max(` → `.min(` → test FAILS at row 4 (`small pool → floor`) with actual `4`
- `nix develop -c tracey query rule ctrl.pool.manifest-failed-sweep` shows the new `r[impl]` and `r[verify]` sites

## Tracey

References existing markers:
- `r[ctrl.pool.manifest-failed-sweep+2]` — T1 adds `r[impl]` on the extracted fn; T2 adds `r[verify]` on the boundary table. The spec at [`controller.md:156-157`](../../docs/src/components/controller.md) already says "bounded per-tick to `max(20, spec.replicas.max)`"; this plan makes the implementation match under negative `replicas.max` (spec implicitly assumes non-negative — no spec change).

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "T1: extract sweep_cap() at :131 with .max(0) clamp; delegate :228. HOT — P0516 touched :357, P0311-T501 touches :345"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs", "action": "MODIFY", "note": "T2: sweep_cap_boundaries 7-row table. P0311-T501 also touches this file (diff section: label-complement test)"}
]
```

```
rio-controller/src/reconcilers/builderpool/
├── manifest.rs          # T1: extract sweep_cap() near :131, delegate :228
└── tests/
    └── manifest_tests.rs  # T2: sweep_cap_boundaries table
```

## Dependencies

```json deps
{"deps": [515], "soft_deps": [511, 516], "note": "P0515 introduced the formula. P0511 defined FAILED_SWEEP_MIN semantics (soft — this plan doesn't change them). P0516 (soft) touched manifest.rs:357 — serialize if both in-flight."}
```

**Depends on:** [P0515](plan-0515-manifest-sweep-cap-divergence.md) — introduced the inline `sweep_cap` formula at `:228` (`1a0a4c2`).

**Conflicts with:** `manifest.rs` is HOT. [P0516](plan-0516-manifest-quota-deadlock.md) (DONE) touched `:235-241` (sweep-first rationale) and `:357` (warn+continue). [P991893304](plan-991893304-warn-continue-escalation.md) will touch `:357` again. [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T501 touches `:345` and `manifest_tests.rs`. This plan's edit is localized to `:131` + `:228` — minimal overlap, but serialize with P991893304 if both in-flight.

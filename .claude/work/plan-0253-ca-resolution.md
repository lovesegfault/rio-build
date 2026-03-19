# Plan 0253: CA resolution — resolve-time inputDrvs rewrite

**USER A10: T0, NOT cuttable.** Milestone demo ([P0254](plan-0254-ca-metrics-vm-demo.md)) MUST include a CA-depends-on-CA chain. This plan is on the minimum-viable cut.

When a CA derivation's inputs are themselves CA, the scheduler must rewrite `inputDrvs` placeholder paths to realized store paths before dispatch. The gateway's `{}` stub for `dependentRealisations` ([`opcodes_read.rs:586`](../../rio-gateway/src/handler/opcodes_read.rs)) is **correct** — Nix removed the field upstream ([ADR-018](../../docs/src/decisions/018-ca-resolution.md)). The `realisation_deps` junction table is populated by the scheduler at resolve time (not by the gateway from wire payloads) — each successful `(drv_hash, output_name) → output_path` lookup during resolution IS the dependency edge.

[P0247](plan-0247-spike-ca-wire-capture-schema-adr.md)'s spike informs the resolve shape (ADR-018 Appendix B maps Nix's `tryResolve` step-by-step).

## Entry criteria

- [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) merged (wire shape captured in ADR-018)
- [P0250](plan-0250-ca-detect-plumb-is-ca.md) merged (`is_ca` flag available)
- [P0249](plan-0249-migration-batch-014-015-016.md) merged (`realisation_deps` junction table exists)
- [P0251](plan-0251-ca-cutoff-compare.md) merged (completion.rs serial — resolution may hook near cutoff)
- [P0230](plan-0230-actor-rwlock-callsite.md) merged (4c last `dispatch.rs` touch)

## Tasks

### T1 — `feat(scheduler):` resolve module + realisation_deps INSERT side-effect

NEW [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs):

```rust
// r[impl sched.ca.resolve]
/// After all CA-input derivations complete, query realisations for their
/// outputs and rewrite inputDrvs placeholder paths to realized store paths.
/// Returns the "resolved" derivation ready for dispatch.
pub async fn resolve_ca_inputs(
    drv: &DerivationState,
    realisations: &RealisationStore,
    db: &SchedulerDb,               // ← for realisation_deps INSERT
) -> Result<ResolvedDerivation> {
    // Per ADR-018 Appendix B (tryResolve @ derivations.cc:1215-1239):
    // For each (inputDrv, outputNames):
    //   1. Compute input drv's hash modulo (drv_hash half of realisations PK)
    //   2. Query realisations for (drv_hash, output_name) → output_path
    //   3. Insert output_path into resolved.inputSrcs
    //   4. Record rewrite: DownstreamPlaceholder(input, output).render() → output_path
    //   5. SIDE EFFECT: INSERT INTO realisation_deps (realisation_id, dep_realisation_id)
    //      — this IS rio's derived-build-trace per ADR-018:45. Never crosses the wire.
    // Then: string-replace all placeholder renderings through env/args/builder
    // Finally: drop inputDrvs (resolved derivation is a BasicDerivation)
    todo!("shape per ADR-018 Appendix B")
}
```

Module decl in `rio-scheduler/src/lib.rs` or a `ca/mod.rs`.

### T2 — `feat(scheduler):` call resolve before dispatch

MODIFY [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) — before assigning to a worker, IF `state.is_ca` AND any input is itself CA:

```rust
let drv_to_send = if state.has_ca_inputs() {
    ca::resolve_ca_inputs(&state, &realisations, &db).await?
} else {
    state.to_dispatch_drv()
};
```

**At dispatch:** `git log -p --since='2 months' -- rio-scheduler/src/actor/dispatch.rs` — find P0230's RwLock callsite; this hook goes near the worker-assignment decision.

### T3 — `test(scheduler):` resolve with mock realisations

```rust
// r[verify sched.ca.resolve]
#[tokio::test]
async fn resolve_rewrites_ca_input_paths() {
    // CA drv with one CA inputDrv. Mock realisations store returns
    // the realized path. Assert resolved drv has the realized path
    // in inputSrcs, placeholder is gone from env. Assert realisation_deps
    // has exactly one row (the INSERT side-effect fired).
}
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule sched.ca.resolve` shows impl + verify
- `rg 'realisation_deps' rio-scheduler/src/ca/resolve.rs` → ≥1 hit (junction-table INSERT lives in the scheduler, not gateway)
- `rg 'dependentRealisations' rio-gateway/src/handler/opcodes_read.rs` → still present (the `{}` stub is CORRECT per ADR-018; do not remove)

## Tracey

References existing markers:
- `r[sched.ca.resolve]` — T1 implements, T3 verifies (seeded by P0245; text bumped by P0320 per ADR-018)

## Files

```json files
[
  {"path": "rio-scheduler/src/ca/resolve.rs", "action": "NEW", "note": "T1: resolve_ca_inputs() + realisation_deps INSERT side-effect"},
  {"path": "rio-scheduler/src/ca/mod.rs", "action": "NEW", "note": "T1: module decl"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T2: call resolve before worker assignment (near P0230's anchor)"}
]
```

```
rio-scheduler/src/
├── ca/
│   ├── mod.rs                    # T1: module
│   └── resolve.rs                # T1: resolve_ca_inputs() + realisation_deps INSERT
└── actor/dispatch.rs             # T2: resolve hook
```

## Dependencies

```json deps
{"deps": [247, 250, 249, 251, 230], "soft_deps": [], "note": "USER A10: T0 NOT cuttable — milestone demo includes CA-on-CA chain. dispatch.rs serial after 4c P0230. Gateway NOT touched — {} stub is correct per ADR-018. Resolve shape from ADR-018 Appendix B (P0247)."}
```

**Depends on:** [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) — ADR-018 resolve shape. [P0250](plan-0250-ca-detect-plumb-is-ca.md) — `is_ca`. [P0249](plan-0249-migration-batch-014-015-016.md) — junction table. [P0251](plan-0251-ca-cutoff-compare.md) — completion.rs serial. [P0230](plan-0230-actor-rwlock-callsite.md) — dispatch.rs serial.
**Conflicts with:** `dispatch.rs` serial after 4c P0230 (dep enforces). Scheduler-only — no gateway collision surface.

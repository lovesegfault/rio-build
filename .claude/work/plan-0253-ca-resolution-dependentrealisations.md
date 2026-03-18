# Plan 0253: CA resolution + dependentRealisations populate

**USER A10: T0, NOT cuttable.** Milestone demo ([P0254](plan-0254-ca-metrics-vm-demo.md)) MUST include a CA-depends-on-CA chain. This plan is on the minimum-viable cut.

When a CA derivation's inputs are themselves CA (CA-on-CA), the scheduler must rewrite `inputDrvs` placeholder paths to realized store paths before dispatch. [`opcodes_read.rs:418`](../../rio-gateway/src/handler/opcodes_read.rs) currently discards `dependentRealisations` on write; `:577`/`:586` return empty/hardcoded `{}`. This plan stops discarding, populates from the `realisation_deps` junction table (USER Q3), and adds the resolve step in dispatch.

[P0247](plan-0247-spike-ca-wire-capture-schema-adr.md)'s wire capture informs the parser shape (what JSON structure nix-daemon actually sends).

## Entry criteria

- [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) merged (wire shape captured in ADR-018)
- [P0250](plan-0250-ca-detect-plumb-is-ca.md) merged (`is_ca` flag available)
- [P0249](plan-0249-migration-batch-014-015-016.md) merged (`realisation_deps` junction table exists)
- [P0251](plan-0251-ca-cutoff-compare.md) merged (completion.rs serial — resolution may hook near cutoff)
- [P0230](plan-0230-actor-rwlock-callsite.md) merged (4c last `dispatch.rs` touch)

## Tasks

### T1 — `feat(gateway):` stop discarding dependentRealisations

MODIFY [`rio-gateway/src/handler/opcodes_read.rs`](../../rio-gateway/src/handler/opcodes_read.rs) at 3 sites:
- `:418` — on write (`wopRegisterDrvOutput`): parse `dependentRealisations` JSON per P0247's captured shape, INSERT into `realisation_deps` junction table
- `:577` — on read (`wopQueryRealisation`): SELECT from `realisation_deps`, populate response
- `:586` — replace hardcoded `{}` with real lookup

Parser shape comes from `docs/src/decisions/018-ca-resolution.md` appendix (P0247's wire capture).

### T2 — `feat(scheduler):` resolve module

NEW [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs):

```rust
// r[impl sched.ca.resolve]
/// After all CA-input derivations complete, query realisations for their
/// outputs and rewrite inputDrvs placeholder paths to realized store paths.
/// Returns the "resolved" derivation ready for dispatch.
pub async fn resolve_ca_inputs(
    drv: &DerivationState,
    realisations: &RealisationStore,
) -> Result<ResolvedDerivation> {
    // For each inputDrv that is CA:
    //   1. Look up its realisation (output hash → store path)
    //   2. Rewrite the placeholder path in inputDrvs
    // P0247's ATerm diff shows exactly what "rewrite" means.
    todo!("shape per ADR-018 appendix")
}
```

Module decl in `rio-scheduler/src/lib.rs` or a `ca/mod.rs`.

### T3 — `feat(scheduler):` call resolve before dispatch

MODIFY [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) — before assigning to a worker, IF `state.is_ca` AND any input is itself CA:

```rust
let drv_to_send = if state.has_ca_inputs() {
    ca::resolve_ca_inputs(&state, &realisations).await?
} else {
    state.to_dispatch_drv()
};
```

**At dispatch:** `git log -p --since='2 months' -- rio-scheduler/src/actor/dispatch.rs` — find P0230's RwLock callsite; this hook goes near the worker-assignment decision.

### T4 — `test(scheduler):` resolve with mock realisations

```rust
// r[verify sched.ca.resolve]
#[tokio::test]
async fn resolve_rewrites_ca_input_paths() {
    // CA drv with one CA inputDrv. Mock realisations store returns
    // the realized path. Assert resolved drv has the realized path
    // in inputDrvs, placeholder is gone.
}
```

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c tracey query rule sched.ca.resolve` shows impl + verify
- `rg 'dependentRealisations.*\{\}' rio-gateway/src/handler/opcodes_read.rs` → 0 (no more hardcoded empty)

## Tracey

References existing markers:
- `r[sched.ca.resolve]` — T2 implements, T4 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "T1: 3 sites — :418 stop discard, :577/:586 populate from realisation_deps"},
  {"path": "rio-scheduler/src/ca/resolve.rs", "action": "NEW", "note": "T2: resolve_ca_inputs()"},
  {"path": "rio-scheduler/src/ca/mod.rs", "action": "NEW", "note": "T2: module decl"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T3: call resolve before worker assignment (near P0230's anchor)"}
]
```

```
rio-gateway/src/handler/
└── opcodes_read.rs               # T1: 3 sites populated
rio-scheduler/src/
├── ca/
│   ├── mod.rs                    # T2: module
│   └── resolve.rs                # T2: resolve_ca_inputs()
└── actor/dispatch.rs             # T3: resolve hook
```

## Dependencies

```json deps
{"deps": [247, 250, 249, 251, 230], "soft_deps": [], "note": "USER A10: T0 NOT cuttable — milestone demo includes CA-on-CA chain. dispatch.rs serial after 4c P0230. opcodes_read.rs low collision. Parser shape from ADR-018 (P0247)."}
```

**Depends on:** [P0247](plan-0247-spike-ca-wire-capture-schema-adr.md) — wire shape. [P0250](plan-0250-ca-detect-plumb-is-ca.md) — `is_ca`. [P0249](plan-0249-migration-batch-014-015-016.md) — junction table. [P0251](plan-0251-ca-cutoff-compare.md) — completion.rs serial. [P0230](plan-0230-actor-rwlock-callsite.md) — dispatch.rs serial.
**Conflicts with:** `dispatch.rs` serial after 4c P0230 (dep enforces). `opcodes_read.rs` low.

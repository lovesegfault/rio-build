# Plan 0254: CA cutoff metrics + VM scenario (MILESTONE CLAUSE 1)

End-to-end demonstration that CA early cutoff works. Submit a CA chain, complete it once, resubmit identical — assert `rio_scheduler_ca_cutoff_saves_total > 0` AND the second submit completes in <5s (vs ~30s normal build time).

**USER A10 impact:** [P0253](plan-0253-ca-resolution-dependentrealisations.md) is T0 — this VM test MUST include a CA-depends-on-CA chain (not just a single CA drv). If the VM shows `ca_cutoff_saves_total = 0`, do NOT mark milestone clause 1 complete — investigate whether resolution is the blocker.

> **DISPATCH NOTE (v-p253, docs-875101):** P0253 landed 8× `TODO(P0254)` that this plan's original Tasks fence does NOT cover. The VM demo at T2 will FAIL the USER-A10 CA-on-CA assertion unless these are absorbed. **Scope expansion:** T5-T8 below are the wiring; T1-T4 remain as originally scoped.
>
> **TODOs absorbed** (from [`resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) + [`dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs)):
> | Site | TODO | Absorbed by |
> |---|---|---|
> | [`resolve.rs:87`](../../rio-scheduler/src/ca/resolve.rs) | `ca_modular_hash` proto field | T5 |
> | [`resolve.rs:354`](../../rio-scheduler/src/ca/resolve.rs) | wire `insert_realisation_deps` into `handle_success_completion` | T7 |
> | [`dispatch.rs:661`](../../rio-scheduler/src/actor/dispatch.rs) | fetch drv from store when `drv_content` empty (recovery) | T8 (document-defer) |
> | [`dispatch.rs:674-679`](../../rio-scheduler/src/actor/dispatch.rs) | gateway plumbs `ca_modular_hash` through proto | T5 |
> | [`dispatch.rs:700-706`](../../rio-scheduler/src/actor/dispatch.rs) | stash `resolved.lookups` on `DerivationState` for completion-time insert | T7 |
> | [`dispatch.rs:724,729,738-749`](../../rio-scheduler/src/actor/dispatch.rs) | `collect_ca_inputs` — uncomment push, use `child.ca_modular_hash` | T6 |
>
> **Also absorbed (rev-p253 row 9):** `maybe_resolve_ca` error handling at [`dispatch.rs:708-715`](../../rio-scheduler/src/actor/dispatch.rs) — ANY `ResolveError` (incl `Db` transient blip) swallows to warn + dispatches unresolved. For `RealisationMissing` that's fine (worker fails on placeholder, retries). For `Db` it wastes a full worker build cycle. T6's uncomment should also add a `ResolveError::Db(_) => defer+requeue` arm (or document why swallow-to-warn is acceptable — the comment at `:640-644` already rationalizes retry-with-backoff).

## Entry criteria

- [P0252](plan-0252-ca-cutoff-propagate-skipped.md) merged (Skipped variant + cascade)
- [P0253](plan-0253-ca-resolution-dependentrealisations.md) merged (CA-on-CA resolution — USER A10)

## Tasks

### T1 — `feat(scheduler):` cutoff-saves metrics

MODIFY [`rio-scheduler/src/lib.rs`](../../rio-scheduler/src/lib.rs) — register:
- `rio_scheduler_ca_cutoff_saves_total` — counter, derivations skipped via cutoff (already incremented in P0252 T3)
- `rio_scheduler_ca_cutoff_seconds_saved` — gauge, sum of `ema_duration_secs` of skipped drvs

### T2 — `test(vm):` ca-cutoff scenario

NEW [`nix/tests/scenarios/ca-cutoff.nix`](../../nix/tests/scenarios/ca-cutoff.nix). Pattern from `nix/tests/lib/derivations/chain.nix` + `__contentAddressed = true;`:

```nix
# r[verify sched.ca.cutoff-propagate]
# (col-0 BEFORE the `{` — NOT in testScript literal per tracey-adoption)
{ fixture, ... }: {
  name = "ca-cutoff";
  testScript = ''
    start_all()
    scheduler.wait_for_unit("rio-scheduler")

    # Build 1: CA chain A→B→C (all __contentAddressed=true), completes normally.
    # Takes ~30s (each node is a sleep 10).
    gateway.succeed("nix build -f ${caChain} --store ssh-ng://localhost")

    # Prometheus scrape: record baseline saves count.
    before = int(scheduler.succeed(
        "curl -s localhost:9090/metrics | grep '^rio_scheduler_ca_cutoff_saves_total ' | awk '{print $2}'"
    ).strip() or "0")

    # Build 2: resubmit identical chain. A completes, hash matches,
    # B+C should Skipped without running.
    import time
    t0 = time.monotonic()
    gateway.succeed("nix build -f ${caChain} --store ssh-ng://localhost --rebuild")
    elapsed = time.monotonic() - t0

    after = int(scheduler.succeed(
        "curl -s localhost:9090/metrics | grep '^rio_scheduler_ca_cutoff_saves_total ' | awk '{print $2}'"
    ).strip())

    # B and C both skipped → saves ≥ 2
    assert after - before >= 2, f"expected ≥2 cutoff saves, got {after - before}"
    # A rebuilds (~10s), B+C skip (instant) → total <15s, not 30s
    assert elapsed < 15, f"second build took {elapsed}s, cutoff not working"
  '';
}
```

Where `caChain` is a NEW derivation fixture: 3-node CA-on-CA chain with `__contentAddressed = true;` on each, `outputHashMode = "recursive";`.

### T3 — `test(vm):` register scenario

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix) — add `vm-ca-cutoff-standalone` (or whichever fixture is lightest).

### T4 — `docs:` observability + scheduler doc updates

MODIFY [`docs/src/observability.md`](../../docs/src/observability.md) — add `rio_scheduler_ca_cutoff_saves_total` / `rio_scheduler_ca_cutoff_seconds_saved` entries.
MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) — document the cascade + Skipped variant.
MODIFY [`docs/src/data-flows.md`](../../docs/src/data-flows.md) — close the `:58` deferral block (CA cutoff was deferred there).

### T5 — `feat(proto):` DerivationNode.ca_modular_hash field + gateway populate

MODIFY [`rio-proto/proto/dag.proto`](../../rio-proto/proto/dag.proto) — add to `DerivationNode`:

```proto
// For CA derivations: the modular derivation hash (hashDerivationModulo
// SHA-256). This is the `drv_hash` half of the realisations table
// composite PK. Populated by the gateway from its hashDerivationModulo
// cache (build.rs:1158 already computes it for builtOutputs). Empty
// (zero-length) for IA derivations.
bytes ca_modular_hash = <next-field-number>;
```

MODIFY [`rio-gateway/src/handler/translate.rs`](../../rio-gateway/src/handler/translate.rs) — where `DerivationNode` is constructed (around `:408` per `dispatch.rs:670-671` comment), set `ca_modular_hash` from the same `hashDerivationModulo` computation the gateway already does for `builtOutputs` at [`build.rs:1158`](../../rio-gateway/src/handler/build.rs). No new computation — reuse the cached value.

MODIFY [`rio-scheduler/src/state/derivation.rs`](../../rio-scheduler/src/state/derivation.rs) — add `ca_modular_hash: Option<[u8; 32]>` field, populate from proto at DAG merge.

### T6 — `feat(scheduler):` collect_ca_inputs — uncomment push, use modular hash

MODIFY [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) at `:726-752`. With T5's `ca_modular_hash` available on `DerivationState`, the commented-out push at `:744-748` goes live:

```rust
fn collect_ca_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::CaResolveInput> {
    let children = self.dag.get_children(drv_hash);
    let mut inputs = Vec::new();
    for child_hash in children {
        let Some(child) = self.dag.node(&child_hash) else { continue };
        if !child.is_ca { continue }
        let Some(modular_hash) = child.ca_modular_hash else {
            // IA-classified-as-CA edge case OR gateway didn't populate.
            // Skip — resolve will be incomplete, worker fails on
            // placeholder, retry-with-backoff handles it.
            continue;
        };
        inputs.push(crate::ca::CaResolveInput {
            drv_path: child.drv_path().to_string(),
            modular_hash,
            output_names: child.output_names.clone(),
        });
    }
    inputs
}
```

Delete the `#[allow(unused_mut)]` at `:729` and the `let _ = child;` at `:749` — no longer needed.

**ResolveError handling at `:708-715`:** Keep warn+dispatch-unresolved for `RealisationMissing` (worker fails on placeholder → retry-with-backoff gives the input's `wopRegisterDrvOutput` time to land — correct per `:640-644` comment). Consider adding `ResolveError::Db(_) => { defer+requeue }` if profiling shows wasted worker cycles; for now, document the rationale at `:708` ("Db blip → unresolved dispatch → worker-fail → retry converges; slot-wasteful but correct").

### T7 — `feat(scheduler):` stash lookups + insert_realisation_deps at completion

MODIFY [`rio-scheduler/src/state/derivation.rs`](../../rio-scheduler/src/state/derivation.rs) — add field:

```rust
/// Realisation lookups from dispatch-time resolve. Consumed by
/// handle_success_completion → insert_realisation_deps AFTER the
/// parent's own realisation lands (FK ordering per resolve.rs:346-352).
pub pending_realisation_deps: Vec<crate::ca::RealisationLookup>,
```

MODIFY [`dispatch.rs:700-706`](../../rio-scheduler/src/actor/dispatch.rs) — instead of dropping `resolved.lookups`, stash on state:

```rust
if let Some(state) = self.dag.node_mut(drv_hash) {
    state.pending_realisation_deps = resolved.lookups;
}
```

MODIFY [`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs) — in `handle_success_completion`, AFTER the realisation is registered (wherever `wopRegisterDrvOutput` equivalent fires — grep for `realisations` INSERT or the P0251 cutoff-compare hook's anchor), call `insert_realisation_deps`:

```rust
// r[impl sched.ca.resolve]
// FK ordering: parent's realisation row must exist before
// realisation_deps rows (they reference it). wopRegisterDrvOutput
// just landed the parent row; now insert the dep edges recorded
// at dispatch time.
if let Some(state) = self.dag.node(drv_hash)
    && !state.pending_realisation_deps.is_empty()
    && let Some(modular_hash) = state.ca_modular_hash
{
    let output_names: Vec<String> = result.built_outputs.iter()
        .map(|o| o.output_name.clone()).collect();
    if let Err(e) = crate::ca::insert_realisation_deps(
        self.db.pool(),
        &modular_hash,
        &output_names,
        &state.pending_realisation_deps,
    ).await {
        warn!(drv_hash = %drv_hash, error = %e,
              "insert_realisation_deps failed (best-effort)");
    }
}
```

Also: re-export `insert_realisation_deps` from [`ca/mod.rs:18-21`](../../rio-scheduler/src/ca/mod.rs) (rev-p253 row 7 — it's currently `pub async fn` but not in the re-export list).

### T8 — `docs(scheduler):` dispatch.rs drv_content-empty recovery — document-defer

MODIFY [`dispatch.rs:661-665`](../../rio-scheduler/src/actor/dispatch.rs) — the `TODO(P0254): fetch from store here when drv_content empty` is for recovered derivations (scheduler restart, DAG reloaded from PG, `drv_content` not persisted). Fetching the ATerm from the store at dispatch time adds an RPC. Two options:

- **(a) Defer further:** re-tag `TODO(P0NNN-recovery-resolve)` with a new plan number (not this one). CA-on-CA chains that survive a scheduler restart are an edge case of an edge case.
- **(b) Fetch:** `store_client.get_path(drv_path)` → bytes → resolve. Adds ~10-50ms dispatch latency for recovered CA-on-CA only.

**Pick (a).** Re-tag with a new plan number at dispatch (coordinator allocates) or keep `TODO(P0254)` and note "OUT OF SCOPE for milestone — recovered chains build unresolved, worker fails on placeholder, eventually succeeds once resolve becomes possible."

## Exit criteria

- `/nbr .#ci` green INCLUDING new `vm-ca-cutoff-*` test
- VM test asserts `ca_cutoff_saves_total ≥ 2` AND second-build elapsed < 15s
- **MILESTONE CLAUSE 1 ("CA early cutoff skips downstream") DEMONSTRATED**
- `grep 'TODO(P0254)' rio-scheduler/src/` → 0 hits (or only the T8 re-tag — all 8 original TODOs closed or re-scoped)
- `grep 'ca_modular_hash' rio-proto/proto/dag.proto rio-gateway/src/handler/translate.rs rio-scheduler/src/state/derivation.rs rio-scheduler/src/actor/dispatch.rs` → ≥4 hits (T5+T6 wired end-to-end)
- `grep 'pending_realisation_deps' rio-scheduler/src/state/derivation.rs rio-scheduler/src/actor/dispatch.rs rio-scheduler/src/actor/completion.rs` → ≥3 hits (T7 stash+consume)
- `grep 'insert_realisation_deps' rio-scheduler/src/ca/mod.rs` → ≥1 hit (T7: re-export)
- T2 VM test's FIRST build (before resubmit) — `ca_cutoff_saves_total == before` (i.e., FIRST build does NOT trigger false saves — regression guard for [P0397](plan-0397-ca-contentlookup-self-match-exclude.md))

## Tracey

References existing markers:
- `r[sched.ca.cutoff-propagate]` — T2 VM-verifies (col-0 header in `.nix`). Closes the cross-plan verify from [P0252](plan-0252-ca-cutoff-propagate-skipped.md).

## Files

```json files
[
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "T1: register cutoff-saves metrics"},
  {"path": "nix/tests/scenarios/ca-cutoff.nix", "action": "NEW", "note": "T2: CA-on-CA chain VM demo"},
  {"path": "nix/tests/lib/derivations/ca-chain.nix", "action": "NEW", "note": "T2: 3-node CA fixture"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T3: register vm-ca-cutoff scenario"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "T4: metric entries"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T4: cascade + Skipped docs"},
  {"path": "docs/src/data-flows.md", "action": "MODIFY", "note": "T4: close :58 deferral"},
  {"path": "rio-proto/proto/dag.proto", "action": "MODIFY", "note": "T5: +ca_modular_hash field on DerivationNode"},
  {"path": "rio-gateway/src/handler/translate.rs", "action": "MODIFY", "note": "T5: populate ca_modular_hash from hashDerivationModulo cache ~:408"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T5+T7: +ca_modular_hash + +pending_realisation_deps fields"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T6: collect_ca_inputs uncomment push :726-752; stash lookups :700; document ResolveError::Db swallow :708"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T7: insert_realisation_deps call after parent realisation registers"},
  {"path": "rio-scheduler/src/ca/mod.rs", "action": "MODIFY", "note": "T7: re-export insert_realisation_deps :18-21"}
]
```

```
rio-scheduler/src/
├── lib.rs                        # T1: metric registration
├── state/derivation.rs           # T5+T7: modular_hash + pending_deps fields
├── actor/
│   ├── dispatch.rs               # T6: collect_ca_inputs live + stash + error-doc
│   └── completion.rs             # T7: insert_realisation_deps call
└── ca/mod.rs                     # T7: re-export
rio-proto/proto/dag.proto         # T5: +ca_modular_hash
rio-gateway/src/handler/
└── translate.rs                  # T5: populate
nix/tests/
├── scenarios/ca-cutoff.nix       # T2: VM demo
├── lib/derivations/ca-chain.nix  # T2: fixture
└── default.nix                   # T3: register
docs/src/
├── observability.md              # T4
├── components/scheduler.md       # T4
└── data-flows.md                 # T4: close deferral
```

## Dependencies

```json deps
{"deps": [252, 253, 397], "soft_deps": [268, 398, 393], "note": "USER A10: deps P0253 — VM demo MUST include CA-on-CA chain. default.nix SOFT-conflict with P0268 (chaos) — coordinator serializes dispatch, not dag dep (4c A9 pattern). DISPATCH-NOTE ABSORBED (docs-875101): T5-T8 wire the 8× TODO(P0254) P0253 landed; without them collect_ca_inputs returns [] and resolve is a no-op → VM demo CA-on-CA chain WILL NOT resolve → placeholder paths → worker-fail. Hard-dep P0397 (self-match exclusion — without it FIRST build falsely sets ca_output_unchanged=true; T2's 'saves>=2' passes trivially-wrong). Soft-dep P0398 (inputDrvs always-empty — brings resolved-drv-hash in line with Nix; may affect VM Nix-client cache-hit). Soft-dep P0393 (CA ContentLookup timeout — same completion.rs hook, additive). T5 dag.proto = class-3-weak proto touch (field-only additive). T5 translate.rs count=24, T6 dispatch.rs count=22, T7 completion.rs count=26 — all HOT but T5-T7 are localized additive inserts. T5's ca_modular_hash reuses gateway's existing hashDerivationModulo cache (build.rs:1158) — zero new computation."}
```

**Depends on:** [P0252](plan-0252-ca-cutoff-propagate-skipped.md) — Skipped cascade. [P0253](plan-0253-ca-resolution.md) — CA-on-CA resolution (USER A10: T0, chain must work). [P0397](plan-0397-ca-contentlookup-self-match-exclude.md) — self-match exclusion (blocks false-positive first-build).
**Conflicts with:** `default.nix` SOFT-conflict with [P0268](plan-0268-chaos-harness-toxiproxy.md) — both add one line. Coordinator serializes dispatch. [`dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) — P0304-T34 adds emit_progress at `:459`, T6 here edits `:700-752` — non-overlapping. [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) — [P0397-T4](plan-0397-ca-contentlookup-self-match-exclude.md), [P0393](plan-0393-ca-contentlookup-serial-timeout.md)-T1/T2, P0304-T148/T157 all touch the CA-compare hook at `:289-337`; T7 here adds a NEW block AFTER that hook (post-realisation-register) — additive. [`dag.proto`](../../rio-proto/proto/dag.proto) class-3-weak (field-add, nextest suffices).

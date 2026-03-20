# Plan 428: CA resolve — IA-input inputSrcs from DAG expected_output_paths

rev-p398 correctness. [P0398](plan-0398-ca-resolve-drop-all-inputdrvs.md) ships `serialize_resolved` with `inputDrvs = []` per Nix's `BasicDerivation` slice-copy semantics. But it leaves four `TODO(P0254)` tags at [`resolve.rs:324,854,868,891`](../../rio-scheduler/src/ca/resolve.rs) (p398 worktree refs) pointing at the IA-input `inputSrcs` gap — and P0254 is already DONE (dag.jsonl confirms). P0254 was the CA-metrics-VM-demo plan; it absorbed the `ca_modular_hash` proto plumbing but did NOT plumb IA output paths (wasn't in its scope; P0398's T2 speculatively deferred to it).

**The gap is cheaper than P0398's T2 comment claims.** The comment at `:313-326` says "Rio's scheduler doesn't hold the transitive closure of parsed .drvs… cannot do this here without an extra store RPC per IA input." But [`DerivationState.expected_output_paths`](../../rio-scheduler/src/state/derivation.rs) at `:304` already holds exactly what's needed — each DAG node carries its own output paths (plumbed via `DerivationNode.expected_output_paths` proto field since phase-4a, consumed by [`assignment.rs:338`](../../rio-scheduler/src/assignment.rs) for prefetch). For an IA input, `expected_output_paths` IS the deterministic output-path set — no store RPC required.

**Also:** the doc-comment at [`resolve.rs:201-209`](../../rio-scheduler/src/ca/resolve.rs) self-contradicts the fast-path. `:207-209` says "If `ca_inputs` is empty, this is a no-op that returns the original `drv_content` unchanged (the parent doesn't need resolution at all)" — but after P0398-T1 drops ALL `inputDrvs`, a CA parent with ONLY IA inputs still needs its `inputDrvs` stripped to match Nix's resolved form. The fast-path at `:231-234` bypasses that. Either (a) the fast-path is wrong (should serialize with empty `inputDrvs` even with no CA inputs), or (b) the caller ([`dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) `maybe_resolve_ca`) only calls `resolve_ca_inputs` when the parent IS CA-and-has-CA-inputs, in which case the fast-path is dead code and the doc is a red herring. Verify at dispatch — if (b), delete the fast-path; if (a), fix it.

## Entry criteria

- [P0398](plan-0398-ca-resolve-drop-all-inputdrvs.md) merged (`serialize_resolved` has `inputDrvs = []`; four `TODO(P0254)` tags exist)
- [P0254](plan-0254-ca-metrics-vm-demo.md) merged (DONE — `ca_modular_hash` plumbing exists; proves proto-plumbing infra is live)

## Tasks

### T1 — `fix(scheduler):` resolve_ca_inputs — take &Dag, collect IA output paths from expected_output_paths

MODIFY [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs). The current signature at `:219` is `pub async fn resolve_ca_inputs(drv_content: &[u8], ca_inputs: &[CaResolveInput], pool: &PgPool)`. Add a `dag: &DerivationDag` parameter (or a narrower `&HashMap<String, DerivationState>` view if the caller can pass it):

```rust
// For each inputDrv NOT in ca_inputs (i.e., IA inputs), look up its
// output paths in the DAG. These are deterministic — the gateway
// computed them at submit time from the parsed .drv and plumbed
// them via DerivationNode.expected_output_paths. No store RPC needed.
//
// Nix's tryResolve iterates ALL inputDrvs (derivations.cc:1206-1234),
// adding each output path to inputSrcs. IA outputs are concrete; CA
// outputs come from realisations. Post-P0398 we do the CA half; this
// closes the IA half.
for (input_drv_path, output_names) in drv.input_drvs() {
    if ca_input_paths.contains(input_drv_path.as_str()) {
        continue; // CA — already handled above via realisation lookup
    }
    // IA input — look up its DerivationState in the DAG. The
    // .drv-path-to-DrvHash mapping is the same one collect_ca_inputs
    // at dispatch.rs:724 uses.
    let Some(input_state) = dag.get_by_drv_path(input_drv_path) else {
        // Parent references an IA input not in our DAG slice — the
        // submission didn't include this transitive dep. Unusual but
        // not an error: the worker's FUSE layer will on-demand-fetch.
        // Log at debug, skip (preserves pre-P428 behavior for
        // this edge).
        debug!(input_drv_path, "IA input not in DAG — skipping inputSrcs add");
        continue;
    };
    for output_name in output_names {
        // expected_output_paths is Vec<String> — pair it with the
        // parsed .drv's outputs map to filter by the names this parent
        // actually wants. Most IA derivations have a single "out" so
        // the filter is trivial.
        new_input_srcs.extend(
            input_state.expected_output_paths.iter().cloned()
            // If output_names is ["out"] and expected_output_paths has
            // one entry, this is a 1:1 pass. Multi-output derivations
            // (dev, doc, lib) need the outputs-index pairing — see the
            // DerivationState.output_names field for the sibling index.
        );
    }
}
```

**Pairing `output_names` with `expected_output_paths`:** `DerivationState` at [`derivation.rs:304`](../../rio-scheduler/src/state/derivation.rs) holds `expected_output_paths: Vec<String>` and (nearby) `output_names: Vec<String>` — they're index-paired. The parent's `inputDrvs` entry is `(path, {names})`; collect only the `expected_output_paths[i]` where `output_names[i]` is in that set.

### T2 — `fix(scheduler):` resolve_ca_inputs caller — pass &dag from dispatch.rs

MODIFY [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) at the `maybe_resolve_ca` call site (grep `resolve_ca_inputs` — P0253's wiring + P0254's uncommented push). The actor already holds `&self.dag` — thread it through:

```rust
let resolved = resolve_ca_inputs(
    &drv_content,
    &ca_inputs,
    &self.dag,        // NEW — IA-input lookup via expected_output_paths
    &self.pool,
).await?;
```

If `resolve_ca_inputs` lives in a module that shouldn't depend on `dag::DerivationDag`, pass a narrower closure or a pre-collected `HashMap<drv_path → Vec<output_path>>` built by the caller from the dag slice it already walks for `collect_ca_inputs`.

### T3 — `fix(scheduler):` resolve_ca_inputs fast-path — fix or delete per doc-comment audit

Audit the `:207-209` doc vs `:231-234` fast-path. Check `maybe_resolve_ca` at [`dispatch.rs:645`](../../rio-scheduler/src/actor/dispatch.rs) — does it guard on `ca_inputs.is_empty()` before calling? If YES: fast-path is dead code, delete `:228-235` and rewrite `:201-209` to say "caller guarantees `ca_inputs` is non-empty". If NO and the parent IS CA with only IA inputs: the fast-path needs to serialize with empty `inputDrvs` + IA-paths-in-inputSrcs — same serialization as the full path, just skipping the realisation lookups.

Most likely case (check at dispatch): `maybe_resolve_ca` only calls when `collect_ca_inputs` returned non-empty, so the fast-path is defensive-dead. Delete it.

### T4 — `fix(scheduler):` retag the four TODO(P0254) → delete (closed by this plan)

The TODOs at `:324,854,868,891` point at work this plan does. Delete `:324` (the prose TODO, replaced by T1's code). Delete `:854,:868` (test-comment TODOs — T5 below updates those tests to assert IA paths ARE present). Delete `:891` (golden-ATerm TODO — T6 below adds the fixture OR retags to `TODO(P0311-T62)` if no fixture lands).

### T5 — `test(scheduler):` serialize_resolved_drops_all_inputdrvs — assert IA paths in inputSrcs

MODIFY same file at `:851` (p398 ref). The test currently asserts IA output paths are NOT in inputSrcs (the TODO(P0254) gap). After T1, invert:

```rust
// r[verify sched.ca.resolve+2]
#[test]
fn serialize_resolved_includes_ia_output_paths_in_inputsrcs() {
    // Parent with one CA input + one IA input. After resolve:
    //   inputDrvs = [] (P0398)
    //   inputSrcs = {orig_srcs, realized_ca, IA's expected output path}
    // The IA path comes from the DAG's expected_output_paths, not a
    // store RPC — proves T1's DAG-lookup is wired.
    let ia_out = "/nix/store/ddd-ia-out";  // from a test DAG slice
    // ... build test DAG with the IA node's expected_output_paths=[ia_out] ...
    let resolved = resolve_ca_inputs(aterm, &ca_inputs, &test_dag, &db.pool).await?;
    let reparsed = Derivation::parse(str::from_utf8(&resolved.drv_content)?)?;
    assert!(reparsed.input_drvs().is_empty());
    let srcs: HashSet<_> = reparsed.input_srcs().iter().collect();
    assert!(srcs.contains("/nix/store/ccc-realized"));  // CA (unchanged)
    assert!(srcs.contains(ia_out));                     // IA (NEW — was absent)
}
```

Keep a separate test for the "IA input not in DAG" fallthrough case (T1's `debug!` + skip path) — asserts no panic, inputSrcs lacks the missing IA path, resolve still succeeds.

### T6 — `test(scheduler):` golden resolved-ATerm vs Nix tryResolve (or retag to P0311-T62)

If a golden fixture is feasible (capture Nix's `tryResolve` output for a known CA+IA-input derivation), add byte-compare test. The gap P0398-T5 noted ("ADR-018 Appendix B captures the transformation but not a byte-exact output") may close now that inputSrcs is complete — a byte-diff would only be `inputSrcs` ordering, which serialize_resolved already sorts.

If still no fixture at dispatch: retag `:891` `TODO(P0254)` → `TODO(P0311-T62)` (adjacent golden-fixture work, same test file).

## Exit criteria

- `/nbr .#ci` green
- `cargo nextest run -p rio-scheduler serialize_resolved_includes_ia_output_paths` → passes
- `grep 'TODO(P0254)' rio-scheduler/src/ca/resolve.rs` → 0 hits (all four retagged/deleted)
- `grep 'expected_output_paths' rio-scheduler/src/ca/resolve.rs` → ≥1 hit (T1's DAG lookup present)
- `grep 'dag.*resolve_ca_inputs\|resolve_ca_inputs.*dag\|&self.dag' rio-scheduler/src/actor/dispatch.rs` → ≥1 hit near maybe_resolve_ca call (T2's threading)
- `nix develop -c tracey query rule sched.ca.resolve` — T5's `r[verify]` site visible
- `:201-209` doc-comment NO LONGER says "no-op that returns original drv_content unchanged" (T3 — fast-path fixed or deleted + doc rewritten)

## Tracey

References existing markers:
- `r[sched.ca.resolve+2]` — T1 refines the existing `r[impl]` at [`resolve.rs`](../../rio-scheduler/src/ca/resolve.rs); T5 adds another `r[verify]` for the IA-path-in-inputSrcs invariant (partners with P0398-T4's inputDrvs-empty verify)

No new markers — the tryResolve semantics are already covered by `r[sched.ca.resolve+2]` text at [`scheduler.md:290`](../../docs/src/components/scheduler.md) ("rewrite inputDrvs placeholder paths to realized store paths before dispatch"); this plan completes the inputSrcs side of that rewrite per ADR-018.

## Files

```json files
[
  {"path": "rio-scheduler/src/ca/resolve.rs", "action": "MODIFY", "note": "T1: +dag param + IA-lookup loop at :310-327; T3: fast-path :228-235 fix-or-delete + :201-209 doc rewrite; T4: delete TODO(P0254) ×4 at :324/:854/:868/:891; T5: test :851 invert → IA path IN inputSrcs + IA-not-in-DAG fallthrough test; T6: golden-ATerm test OR TODO(P0311-T62) retag at :891"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T2: maybe_resolve_ca call site — pass &self.dag to resolve_ca_inputs"}
]
```

```
rio-scheduler/src/
├── ca/resolve.rs           # T1+T3+T4+T5+T6: IA-lookup + fast-path + test + TODOs
└── actor/dispatch.rs       # T2: &dag threading
```

## Dependencies

```json deps
{"deps": [398, 254], "soft_deps": [408, 413, 304, 311], "note": "discovered_from=398 (rev-p398 correctness row). Hard-dep P0398 (ships serialize_resolved inputDrvs=[] + the 4 TODO(P0254) tags this plan closes). Hard-dep P0254 (DONE — ca_modular_hash proto plumbing exists; proves the proto-plumbing infra is live for the DAG-lookup approach). Soft-dep P0408 (DONE — recovery-resolve fetches ATerm from store when drv_content empty; also at dispatch.rs call sites, doesn't change resolve_ca_inputs signature). Soft-dep P0413 (translate.rs populate_* walker dedup — touches gateway BFS plumbing that produces expected_output_paths; if P0413 lands first, verify the DAG still carries expected_output_paths unchanged). Soft-dep P0304-T158 (ATerm serializer dedup — if T158 replaces serialize_resolved, T1's IA-lookup applies to the surviving fn; semantic change is caller-side not serializer-side). Soft-dep P0311-T62/T63 (test-gap batch — T62 downstream_placeholder golden, T63 maybe_resolve_ca gate-paths; both touch resolve.rs+dispatch.rs cfg(test); additive). resolve.rs count≈5 (P0253-new + P0398 + P0408 + this + P0304-T158-T161); dispatch.rs count≈12."}
```

**Depends on:** [P0398](plan-0398-ca-resolve-drop-all-inputdrvs.md) — ships the `inputDrvs=[]` semantics + TODO tags. [P0254](plan-0254-ca-metrics-vm-demo.md) — DONE; proves proto plumbing infra.

**Conflicts with:** [P0304-T158-T161](plan-0304-trivial-batch-p0222-harness.md) (resolve.rs serializer dedup — T1's IA-lookup is in `resolve_ca_inputs` not the serializer, mostly non-overlapping). [P0311-T63](plan-0311-test-gap-batch-cli-recovery-dash.md) (dispatch.rs maybe_resolve_ca test coverage — same file, cfg(test) additive).

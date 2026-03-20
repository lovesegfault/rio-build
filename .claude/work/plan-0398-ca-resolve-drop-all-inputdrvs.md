# Plan 398: CA resolve — drop ALL inputDrvs (Nix tryResolve semantics)

rev-p253 correctness finding. [`serialize_resolved` at `resolve.rs:448-559`](../../rio-scheduler/src/ca/resolve.rs) diverges from Nix's `tryResolve` ([`derivations.cc:1206-1234`](https://github.com/NixOS/nix/blob/master/src/libstore/derivations.cc), mapped in [ADR-018 Appendix B](../../docs/src/decisions/018-ca-resolution.md)):

| Axis | Nix `tryResolve` | Rio `serialize_resolved` |
|---|---|---|
| `inputDrvs` handling | `BasicDerivation resolved{*this}` slice-copy — **ALL `inputDrvs` dropped** (IA + CA) | Only drops entries in `drop_input_drvs` (the CA set); IA entries retained |
| `inputSrcs` additions | Iterates **ALL** `inputDrvs` (IA too) — each resolved output path added | Only adds `extra_input_srcs` = realized CA paths |
| IA-input output paths | Added to `inputSrcs` from the IA .drv's output spec (deterministic, known at eval time) | Left implicit — IA paths are literal in env/args but NOT in `inputSrcs` |

The comment at [`:305-310`](../../rio-scheduler/src/ca/resolve.rs) explicitly acknowledges this: "Actually per ADR-018 Appendix B step 3, resolved.inputDrvs is ALWAYS empty (BasicDerivation slice-copy drops it)." — but the code doesn't follow. Test [`serialize_resolved_preserves_non_ca_inputs`](../../rio-scheduler/src/ca/resolve.rs) (around `:829`) **asserts the divergence** rather than the spec.

**Impact depends on worker consumption model:** if the worker's FUSE layer resolves all input paths via `inputSrcs` (explicit closure seed) vs on-demand via env/args path references, an IA dep NOT in `inputSrcs` may fail to materialize. Per [`worker.fuse.lazy-fetch`](../../docs/src/components/worker.md) the FUSE layer IS on-demand, so the impact is likely nil TODAY — but the divergence means (a) the resolved drv hash doesn't match what Nix would compute (P0254's CA-on-CA demo uses a Nix client that ALSO resolves — hash mismatch = cache miss), (b) any future `inputSrcs`-driven closure pre-fetch misses IA deps.

Cross-check [`derivations.cc:1206-1234`](https://github.com/NixOS/nix/blob/master/src/libstore/derivations.cc) at dispatch — ADR-018's `:55-64` and Appendix B `:201-215` both confirm the BasicDerivation slice.

## Entry criteria

- [P0253](plan-0253-ca-resolution.md) merged (`serialize_resolved` at [`resolve.rs:448-559`](../../rio-scheduler/src/ca/resolve.rs) exists)

## Tasks

### T1 — `fix(scheduler):` serialize_resolved — unconditionally empty inputDrvs

MODIFY [`rio-scheduler/src/ca/resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) at the `inputDrvs` serialization block (`~:490-510`). Replace the filtered loop with an unconditional empty list:

```rust
// inputDrvs — ALWAYS empty in a resolved derivation. Nix's
// `BasicDerivation resolved{*this}` slice-copy (derivations.cc:1204)
// drops inputDrvs entirely — it's a Derivation-only field, not
// present on BasicDerivation. ADR-018 Appendix B step 3.
out.push_str("[],");
```

Delete the `drop_input_drvs` parameter — it's no longer consulted.

### T2 — `fix(scheduler):` resolve_ca_inputs — add IA-input output paths to inputSrcs

MODIFY same file at the caller block around `:240-322`. Before serializing, iterate ALL `inputDrvs` (not just CA) and add each output's deterministic path to `new_input_srcs`:

```rust
// Collect ALL input output paths for inputSrcs — IA and CA alike.
// Nix's tryResolveInput iterates every inputDrv regardless of
// addressing mode (derivations.cc:1206-1234 loop); the CA/IA
// distinction only matters for PLACEHOLDER rewriting (CA paths
// are placeholders, IA paths are literal).
for (input_drv_path, output_names) in drv.input_drvs() {
    if ca_input_paths.contains(input_drv_path.as_str()) {
        // CA input — realized path comes from realisations table,
        // already in new_input_srcs from the lookup loop above.
        continue;
    }
    // IA input — output path is deterministic (in the .drv's
    // outputs spec). Fetch from the parsed Derivation's outputs.
    // These are the literal /nix/store/... paths the parent
    // derivation already references in env/args.
    //
    // LIMITATION: rio's scheduler doesn't hold the transitive
    // closure of parsed .drvs (only the DAG metadata + the
    // parent's drv_content). So we CANNOT resolve IA output
    // paths from the child's .drv here. Two options:
    // (a) omit IA paths from inputSrcs — worker's FUSE on-demand
    //     fetch handles it; resolved-drv hash diverges from Nix
    // (b) fetch child .drv from store, parse, extract output paths
    //     — adds dispatch latency + another store RPC
    //
    // Option (a) is the status quo. Option (b) is correct but
    // expensive. DEFER to P0254's modular-hash plumbing — when
    // the gateway plumbs ca_modular_hash, it can ALSO plumb the
    // IA output-path spec (one proto field per DerivationNode).
    //
    // For now: document the divergence + add a TODO. The worker's
    // FUSE layer IS on-demand so missing inputSrcs doesn't break
    // builds TODAY; it only breaks resolved-drv-hash compatibility.
    // TODO(P0254): plumb IA output paths via proto so inputSrcs
    //   is complete per Nix semantics.
}
```

The signature of `serialize_resolved` simplifies to `(drv, extra_input_srcs)` — no more `drop_input_drvs`.

### T3 — `fix(scheduler):` update comment at :305-310 — code now follows

MODIFY the comment block at [`:305-310`](../../rio-scheduler/src/ca/resolve.rs) — it currently says "Actually per ADR-018 Appendix B step 3, resolved.inputDrvs is ALWAYS empty" while the code disagrees. After T1, the code agrees; rewrite the comment to describe the IA-inputSrcs gap (T2's TODO) instead of the now-fixed inputDrvs issue.

### T4 — `test(scheduler):` REWRITE serialize_resolved_preserves_non_ca_inputs

MODIFY same file at `~:829`. The existing test ASSERTS the divergence (IA `inputDrvs` retained). After T1 it FAILS. Rewrite to assert the CORRECT behavior:

```rust
// r[verify sched.ca.resolve]
#[test]
fn serialize_resolved_drops_all_inputdrvs() {
    // Derivation with 1 CA input + 1 IA input.
    // After serialize_resolved: inputDrvs MUST be [] (both dropped).
    // inputSrcs MUST contain: original inputSrcs ∪ realized CA path.
    // (IA output path NOT yet in inputSrcs — see T2's TODO(P0254).)
    let resolved = serialize_resolved(&drv, std::iter::once(realized_ca_path));
    let reparsed = Derivation::parse(&resolved).unwrap();
    assert!(reparsed.input_drvs().is_empty(),
            "resolved drv MUST have empty inputDrvs (BasicDerivation slice)");
    assert!(reparsed.input_srcs().contains(&realized_ca_path));
}
```

Keep a second test proving the positive CA case still works (placeholder replacement + realized path in `inputSrcs`).

### T5 — `test(scheduler):` golden resolved-ATerm against Nix tryResolve

Optional but high-value. If a golden fixture exists (ADR-018 Appendix B may have captured one during P0247's spike), assert `serialize_resolved` output byte-matches Nix's `tryResolve` output for the same input. If no fixture: add `TODO(P0311-T62)` pointing at the downstream_placeholder golden-value test (adjacent golden-fixture work).

## Exit criteria

- `/nbr .#ci` green
- `cargo nextest run -p rio-scheduler serialize_resolved_drops_all_inputdrvs` → passes
- `grep 'drop_input_drvs' rio-scheduler/src/ca/resolve.rs` → 0 hits (parameter deleted)
- `grep '"\[\],"' rio-scheduler/src/ca/resolve.rs` → ≥1 hit in `serialize_resolved` body (unconditional empty inputDrvs)
- [`:305-310`](../../rio-scheduler/src/ca/resolve.rs) comment NO LONGER says "but code does not follow" — rewritten per T3
- `nix develop -c tracey query rule sched.ca.resolve` — T4's `r[verify]` site visible

## Tracey

References existing markers:
- `r[sched.ca.resolve+2]` — T1+T2 refine the existing `r[impl]` at [`resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) (P0253's annotation); T4 adds `r[verify]` for the inputDrvs-empty invariant

No new markers — the tryResolve semantics are already covered by `r[sched.ca.resolve+2]` text ("rewrite inputDrvs placeholder paths to realized store paths before dispatch"); this plan brings the implementation into compliance with the existing spec.

## Files

```json files
[
  {"path": "rio-scheduler/src/ca/resolve.rs", "action": "MODIFY", "note": "T1: serialize_resolved inputDrvs → unconditional []; T2: IA-inputSrcs TODO+comment; T3: :305-310 comment rewrite; T4: test :829 rewrite (asserts NEW behavior)"}
]
```

```
rio-scheduler/src/ca/
└── resolve.rs                    # T1-T4: all changes localized to one file
```

## Dependencies

```json deps
{"deps": [253], "soft_deps": [254, 304], "note": "discovered_from=253 (rev-p253 correctness row 1). Hard-dep P0253 (DONE — serialize_resolved + the :305-310 comment + test :829 exist). Soft-dep P0254 (UNIMPL — the IA-output-path plumbing this plan defers to; P0254's proto plumbing for ca_modular_hash can ALSO carry IA output specs, closing T2's gap; see P0254 dispatch-note). Soft-dep P0304-T158 (ATerm serializer dedup — if T158 lands first and replaces serialize_resolved with a rio-nix to_aterm_resolved variant, T1 here applies to THAT function's inputDrvs logic instead; the FIX is semantic (always-empty inputDrvs) not syntactic (which fn hand-rolls it). Coordinate at dispatch: grep for serialize_resolved vs to_aterm_resolved)."}
```

**Depends on:** [P0253](plan-0253-ca-resolution.md) — `serialize_resolved` + comment + test exist.

**Conflicts with:** [P0304-T158](plan-0304-trivial-batch-p0222-harness.md) deduplicates `serialize_resolved` against `rio-nix/src/derivation/aterm.rs` `to_aterm`. If T158 lands first, this plan's T1 targets the new `to_aterm_resolved` (or whatever T158 names it) — the semantic change (inputDrvs always `[]`) is the same, just applied to whichever serializer survives. [`resolve.rs`](../../rio-scheduler/src/ca/resolve.rs) count≈3 (P0253-new) — P0304-T158/T159/T160/T161 all touch it; additive/localized.

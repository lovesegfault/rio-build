# Plan 0057: Delete rio-spike crate (superseded by VM tests)

## Design

The Phase 1a FUSE+overlay+sandbox validation spike (`rio-spike/`, created in P0007) was a standalone binary that proved the concept: mount a FUSE filesystem exposing a synthetic Nix store database, overlay it under a sandboxed builder, run a real derivation. It served its purpose — the architecture works — and by phase-2a the production `rio-worker` crate does everything the spike did, with higher fidelity, validated by multi-VM NixOS tests (`nix/tests/phase{1a,1b,2a}.nix`).

Crucially, the spike code had diverged from `rio-worker` and was missing several VM-test bugfixes: the stacked-lower overlay (host `/nix/store` mounted *before* the FUSE mount in the lower chain), FUSE materialize-on-lookup semantics, and the `IndexReferrer`/`IndexReference` synthetic DB indexes. Keeping it around meant either letting it rot or doing double-maintenance for a binary nobody ran.

Also removed: the `spike-image` Docker target from `flake.nix` (57 lines) and three transitive-only workspace deps that only the spike pulled in (`hdrhistogram`, `ctrlc`, `walkdir`).

This is the first act of phase-2b's "Post-Phase-2a Cleanup" checklist: delete what's superseded before building new.

## Files

```json files
[
  {"path": "rio-spike/Cargo.toml", "action": "DELETE", "note": "spike crate manifest"},
  {"path": "rio-spike/src/main.rs", "action": "DELETE", "note": "spike entrypoint"},
  {"path": "rio-spike/src/benchmark.rs", "action": "DELETE", "note": "hdrhistogram timing harness"},
  {"path": "rio-spike/src/fuse_store.rs", "action": "DELETE", "note": "prototype FUSE filesystem"},
  {"path": "rio-spike/src/overlay.rs", "action": "DELETE", "note": "prototype overlay mount (missing stacked-lower fix)"},
  {"path": "rio-spike/src/synthetic_db.rs", "action": "DELETE", "note": "prototype synth-db (missing Index* indexes)"},
  {"path": "rio-spike/src/validate.rs", "action": "DELETE", "note": "spike self-check"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "remove workspace member + hdrhistogram/ctrlc/walkdir"},
  {"path": "flake.nix", "action": "MODIFY", "note": "remove spike-image Docker target (57 lines)"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive markers would be in `r[wk.fuse.*]` / `r[wk.overlay.*]` domain for the production implementations that superseded this crate.

## Entry

- Depends on **P0007** (FUSE-spike): deletes the crate P0007 created.
- Depends on **P0056** (phase-2a terminal): VM tests `nix/tests/phase{1a,1b,2a}.nix` that supersede the spike were added during 2a.

## Exit

Merged as `3ceb5da` (1 commit). `.#ci` green at merge (VM tests still pass with spike removed; workspace builds without the three transitive deps).

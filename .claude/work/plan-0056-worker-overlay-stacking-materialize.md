# Plan 0056: Worker runtime — overlay host-stacking, FUSE materialize-on-lookup, synth-DB DerivationOutputs

## Context

After P0055's deadlock fixes, the VM test's builds started but failed with a cascade of ENOENT/OutputRejected errors. Four distinct bugs, all in the "outputs produced and reported" pipeline:

**nix-daemon unreachable after bind-mount:** `pre_exec` bind-mounts the overlay at `/nix/store`. `exec()` then resolves `nix-daemon` at `/nix/store/{hash}-nix/bin/...` — but the overlay only contains FUSE-served rio-store paths. ENOENT. Fix: stack the host's `/nix/store` as the *first* overlay lower — `lowerdir=/nix/store:{fuse_mount}`. nix-daemon + glibc stay reachable.

**FUSE lookup returns synthetic attr, never materializes:** `lookup()` returned a synthetic dir attr with 1h TTL for top-level store paths (existence-only via `QueryPathInfo`), deferring NAR fetch to `getattr`/`open`/`readdir`. But the kernel caches the lookup attr and never calls `getattr`. First lookup *into* the directory (`lookup(busybox_ino, 'bin')`) hit an empty `cache_dir` → ENOENT. Builds failed with `OutputRejected` "builder failed to produce output path" — the builder (`busybox sh`) started but couldn't find `/nix/store/HASH-busybox/bin/busybox`.

**synth-DB missing `DerivationOutputs` table:** nix-daemon's `checkPathValidity()` calls `queryPartialDerivationOutputMap()` which reads `DerivationOutputs`. Without it, `initialOutputs[out].known = None` → `scratchPath = makeFallbackPath(drvPath)` (hash of `'rewrite:<drvPath>:name:out'` with all-zero content hash) instead of the real output path. Builder's `$out` is the REAL path → mismatch → `OutputRejected`.

**`builtOutputs` empty in response:** gateway sent `BuildResult` with empty `builtOutputs` after successful builds. Modern Nix clients rely on this to map derivations → realized output paths (`DrvOutput → Realisation`).

## Commits

- `3cb916e` — fix(rio-worker): stack host /nix/store as first overlay lower + skip .links lookup
- `830885e` — fix(rio-worker): materialize store paths in FUSE lookup instead of synthetic attr
- `a72a496` — fix(rio-worker): populate DerivationOutputs in synth DB + resolve inputDrv outputs
- `a29697b` — fix(rio-gateway): populate builtOutputs in BuildPathsWithResults response

## Files

```json files
[
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "lowerdir=/nix/store:{fuse_mount} — host store FIRST, FUSE layered on top"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "resolve inputDrv outputs: fetch each input .drv, extract output paths, add to BasicDerivation.inputSrcs AND compute_input_closure seeds"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "lookup(): call ensure_cached() for top-level paths — materialize full NAR tree BEFORE any child lookup; remove synthetic_dir_attr; map GetPath NotFound→ENOENT (was: EIO); skip .links and <34-char names"},
  {"path": "rio-worker/src/fuse/lookup.rs", "action": "MODIFY", "note": "materialize-on-lookup: real attrs from materialized path"},
  {"path": "rio-worker/src/synth_db.rs", "action": "MODIFY", "note": "populate DerivationOutputs table: (drv_path, output_name, output_path)"},
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "after successful build: compute hash_derivation_modulo per DerivedPath, populate builtOutputs via BuildResult::with_outputs_from_drv"},
  {"path": "nix/tests/phase2a.nix", "action": "MODIFY", "note": "worker VMs use writableStore=false (NixOS VM default makes /nix/store an overlay; stacking ours on it may break copy-up); all VMs 4 cores"},
  {"path": "nix/tests/phase2a-derivation.nix", "action": "MODIFY", "note": "collector derivation now actually exercises inputDrv outputs"}
]
```

## Design

**Host-store stacking:** `lowerdir=/nix/store:{fuse_mount}` — overlay resolves left-to-right. nix-daemon (and glibc, and every runtime dep) is at `/nix/store/{hash}-nix/` on the host. FUSE only serves rio-store paths. Outputs copy-up to upper as before. The `.links` directory (Nix's hardlink-optimise dir) lives in host store too — FUSE lookup skips non-store-basename names (< 34 chars or starting with `.`) → ENOENT directly.

**Materialize-on-lookup:** `lookup(ROOT, store_basename)` → `ensure_cached()` — fetches NAR, extracts to `cache_dir/{basename}`, inserts into cache index. Returns *real* attrs from `symlink_metadata(cache_dir/{basename})`. Subsequent `lookup(parent_ino, child_name)` finds real directory entries. The synthetic-attr-with-lazy-fetch approach doesn't work with kernel attr caching.

**inputDrv output resolution:** `drv.to_basic()` only copies static `input_srcs`. For derivations with `inputDrvs`, the *outputs* of those deps must be fetched (parse each input .drv, extract `outputs` map) and added to (a) `BasicDerivation.inputSrcs` — nix-daemon's sandbox only bind-mounts these, and (b) `compute_input_closure` seeds — else `ValidPaths` missing them → "dependency does not exist".

**builtOutputs:** for each requested `DerivedPath`, `hash_derivation_modulo` (P0024) with `drv_cache` as resolver, then `BuildResult::with_outputs_from_drv` populates the `DrvOutput → Realisation` map.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[worker.overlay.host-stack]`, `r[worker.fuse.materialize-on-lookup]`, `r[worker.synthdb.derivation-outputs]`, `r[gw.build.built-outputs]`.

## Outcome

Merged as `3cb916e..a29697b` (4 commits). At `a72a496`: "all 5 VM test builds now succeed (4 leaves + 1 root collector)." The VM test's final metric assertions passed at `132de90` (P0054). This is the **terminal plan of phase 2a** — the phase-2a milestone is complete.

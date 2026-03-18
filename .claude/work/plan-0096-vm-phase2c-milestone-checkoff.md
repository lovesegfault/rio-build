# Plan 0096: vm-phase2c milestone test + NixOS modules + docs checkoff

## Design

Phase closeout: the 5-VM milestone test, NixOS module extensions for size-class config, and the docs sweep that marked all `phase2c.md` tasks checked and synced component docs to implementation reality.

### vm-phase2c milestone test

5 VMs: control (PG+store+scheduler+gateway), 2 small workers, 1 large worker, client. Validates what's observable end-to-end:

1. **Size-class config load:** `cutoff_seconds` gauge proves figment read `/etc/rio/scheduler.toml` `[[size_classes]]` arrays
2. **Critical-path dispatch:** chain-a (3-deep) and solo (1-deep) both dispatch — wiring works (see "3 iterations" below for why order-assertion was dropped)
3. **Size-class routing:** pre-seeded 120s EMA for `bigthing` → `classify` picks `large` → `w-large` gets it, `w-small` doesn't (metric + log grep)
4. **`content_index` populated:** `PutPath` inserts rows (P0095 wired in binary)

Chunk dedup + binary cache HTTP were library-complete (P0086-P0088 unit tests pass) but `main.rs` wiring was deferred to phase3a per existing `TODO` at `rio-store/src/main.rs:159` — those VM assertions were deferred too.

**NixOS module changes:** `worker.nix` gained `sizeClass` option → `RIO_SIZE_CLASS` env (empty = wildcard). `scheduler.nix` gained `extraConfig` option → `/etc/rio/scheduler.toml` for TOML arrays (env vars can't express `[[size_classes]]`). `tickIntervalSecs=2` in test (6 ticks = 12s estimator refresh vs 60s). `phase2c-derivation.nix`: chain A→B→C + standalone solo, 1s sleep each for observable dispatch order.

### VM test fixes: 3 iterations

The docs-checkoff commit `1ada1f2` included three rounds of VM test fixes discovered during final validation:

**Iteration 1 — log grep target:** `grep 'worker acknowledged assignment'` (INFO) not `'assigned derivation to worker'` (`debug!`, filtered by default). The assertion was silently passing on the wrong thing.

**Iteration 2 — pname in env attrset:** `bigthing` derivation: `pname` MUST be in the `env` attrset (gateway reads `drv.env().get("pname")`, not the derivation name). Moved to `-A` attr in `phase2c-derivation.nix` instead of fragile inline `-E` quoting.

**Iteration 3 — unobservable assertions dropped:**
- Critical-path order assertion removed: `chain-a` and `solo` are both LEAVES with `priority = est_dur = 30` → tie. Critical-path gives higher priority to nodes with accumulated work (`chain-b=60`, `chain-c=90`), not to leaves at the start of long chains. Assert wiring-works (all dispatched) instead; P0090's unit tests prove priority math.
- Circuit breaker assertion removed: gateway's `wopEnsurePath` fails on store-down BEFORE `SubmitBuild` reaches the scheduler's merge where the breaker lives. Can't trigger via `nix-build` when store is fully down. P0085's unit tests cover it directly.

### Docs sweep

All `phase2c.md` tasks checked. ZERO `TODO(phase2c)` in `.rs`. ZERO `TODO[^(]` (untagged). Component docs synced:
- `scheduler.md`: Key Files list updated (`actor/` split, `queue.rs` `BinaryHeap`, `critical_path.rs`, `assignment.rs`, `estimator.rs`). Phase 2a simplification note replaced with phase2c state + phase3a deferrals (CRD, `CutoffRebalancer`).
- `store.md`: Key Files = actual files (`cas.rs`, `chunker.rs`, `manifest.rs`, `content_index.rs`, `realisations.rs`, `cache_server.rs`, `signing.rs`). Phase 2a `nar_blobs` deferral note replaced: `manifests` table is active schema, `ChunkBackend` `main.rs` wiring is phase3a.
- `gateway.md`: `wopRegisterDrvOutput`/`wopQueryRealisation` moved from Stubbed to Fully implemented. CA opcode note added.
- `observability.md`: removed `*(Phase 2c+)*` tags on now-implemented metrics. Added `cache_check_circuit_open_total`, `chunk_cache_hits/misses`. `class_load_fraction` re-tagged phase3a.
- `rio-store/src/main.rs`: stale "TODO-phase2c" comment wording updated (library-complete, `main.rs` wiring phase3a).

Phase 3a deferrals documented in `phase2c.md`: `rio-store` `main.rs` `ChunkBackend` construction, `WorkerPoolSet` CRD + adaptive `CutoffRebalancer`, closure-size-as-proxy estimator fallback, `ema_peak_cpu_cores` (needs mid-build polling).

## Files

```json files
[
  {"path": "nix/tests/phase2c.nix", "action": "NEW", "note": "5-VM test: control + 2 small workers + 1 large + client; 3 iterations of fixes"},
  {"path": "nix/tests/phase2c-derivation.nix", "action": "NEW", "note": "chain A->B->C + solo, pname in env attrset, -A attr not inline -E"},
  {"path": "nix/modules/scheduler.nix", "action": "MODIFY", "note": "extraConfig option -> /etc/rio/scheduler.toml for [[size_classes]]"},
  {"path": "nix/modules/worker.nix", "action": "MODIFY", "note": "sizeClass option -> RIO_SIZE_CLASS env"},
  {"path": "flake.nix", "action": "MODIFY", "note": "vm-phase2c check registration"},
  {"path": "docs/src/phases/phase2c.md", "action": "MODIFY", "note": "all tasks checked, phase3a deferrals documented"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "Key Files updated, phase2a simplification note replaced"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "Key Files = actual files, nar_blobs note replaced"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "CA opcodes Stubbed -> Fully implemented"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "Phase 2c+ tags removed, new metrics added"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "stale TODO wording updated (library-complete, main.rs wiring 3a)"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0085**: circuit breaker (VM test initially tried to validate it, iteration 3 dropped — unit-test-only surface).
- Depends on **P0090**: critical-path dispatch (VM test validates wiring).
- Depends on **P0093**: size-class routing (VM test validates `bigthing` → `w-large`; NixOS module `sizeClass` option).
- Depends on **P0095**: `content_index` populated (VM test checks rows exist after PutPath).

## Exit

Merged as `092c876..1ada1f2` (2 commits). Final gate: `ci-fast` PASS. All 5 VM tests green (`vm-phase1a`, `1b`, `2a`, `2b`, `2c`). 811 unit tests pass. Coverage 87.0% line (was 83.9% at phase2b end).

Phase 2c terminal plan. `phase-2c` tag on `1ada1f2`.

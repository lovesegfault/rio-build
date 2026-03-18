# Plan 0030: Gateway refactor — opcode→gRPC delegation + DAG reconstruction

## Context

Phase 1b's gateway (P0012/P0013) handled opcodes by calling a local `nix-daemon --stdio` subprocess. Phase 2a flips this: the gateway is now a pure protocol translator. Store-read opcodes (`wopIsValidPath`, `wopQueryPathInfo`, `wopNarFromPath`) become `StoreService` gRPC calls. Store-write opcodes (`wopAddToStoreNar`, `wopAddMultipleToStore`) become `PutPath`. Build opcodes (`wopBuildDerivation`, `wopBuildPathsWithResults`) become `SubmitBuild` + `WatchBuild` on the scheduler.

The tricky part is build opcodes. Nix sends `wopBuildPathsWithResults` with a list of `DerivedPath`s — not a DAG. The gateway must reconstruct the full dependency graph by walking `inputDrvs` recursively (parsing each `.drv` file with P0008's ATerm parser), then ship the whole DAG to the scheduler as a `SubmitBuildRequest`. That's `translate.rs::reconstruct_dag`.

**Shared commit with P0031:** `395e826` is a 5,831-insertion mega-commit that landed gateway AND worker together. This plan covers the `rio-gateway/` half.

## Commits

- `395e826` — feat(rio-build): implement gateway refactor and worker executor (gateway half)

## Files

```json files
[
  {"path": "rio-gateway/src/handler.rs", "action": "NEW", "note": "all 20 opcodes remapped: store reads/writes → StoreService, builds → SchedulerService via reconstruct_dag"},
  {"path": "rio-gateway/src/translate.rs", "action": "NEW", "note": "reconstruct_dag: walk inputDrvs recursively via ATerm parse, build DerivationNode list + edges; drv_cache for dedup"},
  {"path": "rio-gateway/src/session.rs", "action": "NEW", "note": "SSH channel session: track active builds per client; on disconnect, send CancelBuild"},
  {"path": "rio-gateway/src/server.rs", "action": "NEW", "note": "SSH server with StoreServiceClient + SchedulerServiceClient injected"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "connect gRPC clients, start SSH server"}
]
```

## Design

**Opcode routing table (20 opcodes):**
- `wopIsValidPath` (1), `wopQueryPathInfo` (26), `wopQueryPathFromHashPart` (29) → `StoreService.QueryPathInfo` / `FindMissingPaths`
- `wopNarFromPath` (38) → `StoreService.GetPath`, stream chunks as `STDERR_WRITE` (BUG — should be raw-after-LAST, fixed P0054)
- `wopAddToStoreNar` (39), `wopAddMultipleToStore` (44) → `StoreService.PutPath` (wire-format bug in 44, fixed P0054)
- `wopBuildDerivation` (36), `wopBuildPaths` (9), `wopBuildPathsWithResults` (46) → `reconstruct_dag` + `SchedulerService.SubmitBuild` + `WatchBuild`
- `wopQueryValidPaths` (31), `wopQueryMissing` (40), `wopQueryAllValidPaths` (23) → `FindMissingPaths` variants
- Stubs: `wopAddTempRoot` (11), `wopAddSignatures` (37), `wopRegisterDrvOutput` (42), `wopQueryRealisation` (43), `wopEnsurePath` (10)

**`reconstruct_dag`:** given a root `.drv` path, fetch its ATerm from the store (`GetPath`), parse with P0008's `Derivation::parse`, extract `inputDrvs`. For each inputDrv, recurse. Cache parsed derivations in a `HashMap<StorePath, Derivation>` to avoid re-fetching shared deps. Output: `Vec<DerivationNode>` (proto) + `Vec<Edge>`. Leaf nodes (no inputDrvs) get `system` from their ATerm.

**BuildEvent → STDERR translation:** scheduler's `WatchBuild` stream emits `BuildEvent`s (DerivationStarted, LogLine, DerivationCompleted, BuildCompleted). The gateway converts these to Nix's STDERR protocol: `STDERR_START_ACTIVITY`/`STDERR_STOP_ACTIVITY` for derivation lifecycle, `STDERR_RESULT` with log lines, `STDERR_LAST` + `BuildResult` at terminal. This is `process_build_events` in handler.rs.

**IFD detection:** a `wopBuildDerivation` (opcode 36) that arrives BEFORE any `wopBuildPathsWithResults` is probably import-from-derivation (Nix evaluating a .nix file that depends on a build output). The gateway sets `priority_class="interactive"` on that submit. Tracked via a `has_seen_build_paths_with_results` flag on the session.

**Client disconnect:** SSH channel close drops the `ChannelSession`. Its `Drop` iterates active build IDs and sends `CancelBuild` to the scheduler. Best-effort.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Gateway spec text in `docs/src/components/gateway.md` was later retro-tagged with `r[gw.translate.reconstruct-dag]`, `r[gw.session.cancel-on-disconnect]`, `r[gw.ifd.detect]` markers. Opcode markers inherited from P0012/P0013.

## Outcome

Merged as `395e826` (shared with P0031). 38 new tests (gateway + worker combined). Two protocol bugs in this implementation are latent until P0054: `wopAddMultipleToStore` reads the wire format wrong (missing `num_paths` prefix, nested framed instead of plain bytes), and `wopNarFromPath` sends `STDERR_WRITE` chunks instead of raw-after-`STDERR_LAST`. Both pass the byte-level tests because the tests were written to match the handler, not the Nix spec.

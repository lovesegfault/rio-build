# Plan 0050: Gateway STDERR discipline + reconstruct_dag fail-fast + unwrap safety

## Context

Three STDERR-protocol correctness fixes and one fail-fast behavioral change:

**`STDERR_ERROR` then `Ok(())`:** `handle_nar_from_path` sent `STDERR_ERROR` on invalid-path/not-found, then returned `Ok(())`. Session loop continues reading next opcode. But Nix client received `STDERR_ERROR` and is tearing down. Next wire read → EOF or garbage → confusing error cascade.

**`STDERR_ERROR` then `STDERR_LAST`:** `process_build_events` sent `STDERR_ERROR` on stream end, returned `Err`. `submit_and_process_build` caught the `Err`, returned `Ok(BuildResult::failure)`. Caller sent `STDERR_LAST` + `BuildResult`. Protocol violation — `STDERR_ERROR` followed by `STDERR_LAST`.

**`reconstruct_dag` silent degradation:** unresolvable `inputDrv` (store unreachable / `.drv` missing) pushed a stub leaf with `system:""`. No worker matches `system:""` → derivation stuck in `Ready` forever. User saw silent hang; root cause logged at `error!` but invisible.

This plan echoes P0018 (phase 1b): same category of STDERR-discipline mistakes.

## Commits

- `5d65955` — fix(rio-gateway): return Err after STDERR_ERROR in handle_nar_from_path
- `31ac293` — fix(rio-gateway): remove premature STDERR_ERROR in process_build_events
- `0118243` — refactor(workspace): replace production .unwrap() with safe patterns
- `9dd2ad8` — fix(rio-gateway): fail reconstruct_dag on unresolvable inputDrv instead of broken leaf stub
- `1e8f310` — fix(rio-scheduler): fall back to drv_hash when drv_hash_to_path returns None, log submit failure

(Discontinuous — commits 72, 73, 84, 97, 120.)

## Files

```json files
[
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "handle_nar_from_path: STDERR_ERROR then return Err; process_build_events: remove STDERR_ERROR, just return Err (error flows into BuildResult::failure); contains_key+remove.unwrap() → if-let-Some; wopBuildPathsWithResults submit failure: warn! + rio_gateway_errors_total{type=scheduler_submit}"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "reconstruct_dag: unresolvable inputDrv → hard fail with parent+child path in message (was: stub leaf with system=\"\"); unparseable path → hard fail (was: warn!+continue left dangling edge)"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "last_error.unwrap() → .expect() documenting invariant (MAX_UPLOAD_RETRIES>=1)"},
  {"path": "rio-gateway/tests/wire_opcodes.rs", "action": "MODIFY", "note": "test_nar_from_path_invalid_path_returns_error verifies session termination"}
]
```

## Design

**STDERR_ERROR contract:** "On errors, sends STDERR_ERROR to the client and returns Err, which terminates the session." Every other site in handler.rs followed this. `handle_nar_from_path` was the outlier.

**`process_build_events` fix:** for `wopBuildDerivation`/`wopBuildPathsWithResults`, scheduler disconnect is a *build failure*, not a *protocol error*. The error message flows into `BuildResult::failure` via `submit_and_process_build`, which handlers correctly send via `STDERR_LAST` + `BuildResult`. Removing the premature `STDERR_ERROR` lets that work. `handle_build_paths` (opcode 9) unaffected — its `Err` path fires for initial submit failures which don't send `STDERR_ERROR`, so its own `STDERR_ERROR` is still correct.

**reconstruct_dag fail-fast:** both failure cases now `bail!()` with a message including both parent and unresolvable-child paths. Caller's `BuildResult::failure` surfaces the actual problem to the Nix client. Replaced `test_reconstruct_dag_missing_drv_leaf_fallback` (tested removed behavior) with `test_reconstruct_dag_unresolvable_inputdrv_fails`.

**drv_hash fallback (`1e8f310`):** `drv_hash_to_path().unwrap_or_default()` emitted `BuildEvent{derivation_path=""}` when reverse-index missing. Gateway's `process_build_events` can't match empty paths → events dropped. Fall back to `drv_hash.to_string()` + `warn!`.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). STDERR discipline later spec'd as `r[gw.stderr.error-then-err]`, `r[gw.stderr.no-error-before-last]`.

## Outcome

Merged as `5d65955`, `31ac293`, `0118243`, `9dd2ad8`, `1e8f310` (5 commits, discontinuous). P0045's `close_stream_early` mock specifically regression-guards the `31ac293` fix. `handle_build_paths` (opcode 9) carefully verified to produce exactly one `STDERR_ERROR`.

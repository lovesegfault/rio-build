# Plan 0146: Dead code cleanup + test hygiene (helpers, Arc::clone style)

## Design

Coverage reports have a side effect: they surface dead code. Functions at 0% that NOTHING calls. This plan deleted them and consolidated test boilerplate that had accumulated across four validation rounds.

**Dead code deletions:**
- `rio-worker/src/cgroup.rs`: `cpu_usage_usec` — read once by a metric that was removed in phase 2c.
- `rio-worker/src/fuse/cache.rs`: unnecessary clone in evict loop (was cloning for a debug log that no longer existed).
- `rio-store/src/cas.rs`: `test_cache_len` helper — tests moved to measure via `Arc::strong_count` directly.
- `rio-nix`: `OutputNames` newtype (wrapped `Vec<String>`, added nothing); `NO_STRINGS` const used once, inlined; `StorePathHash` made private (only used within `store_path.rs`).
- `rio-controller`: unused `_active` param in `compute_desired`; `as_deref` for `metadata.name`; `then_some` instead of `if-then-Some-else-None`.
- `rio-gateway`: provably-dead `if let` flattened (pattern always matched after a refactor); consume-in-loop instead of collect-then-iterate; intermediate `Vec` dropped.
- `rio-scheduler`: unread `Debug` fields on internal structs (only printed, never read — but `Debug` derive still compiled them); consume `newly_ready` instead of `.iter().cloned()`; `from_recovery_row` takes ownership (was borrowing + cloning).

**Test hygiene:**
- `ChunkSession` wrapper with `Drop` — tests that created sessions now auto-clean. `expect_err` pattern instead of `match Err(_) => (), _ => panic!()`. `tokio-test` assertions where applicable.
- `spawn_grpc_server` helper — every gRPC test had 15 lines of `TcpListener::bind + tokio::spawn(Server::builder()...)`. Extracted once.
- `setup_actor_configured` — scheduler tests had 20 lines of `DagActor::new().with_*().with_*()` boilerplate. Parameterized helper.
- `sleep → pause` — `start_paused=true` + `tokio::time::advance` instead of real `sleep(50ms)`.
- ATerm literal dedup — same 200-char derivation ATerm string in 5 test files. Extracted to `test-support`.
- `test_store_path` helper — `mark.rs` tests had invented 3-char-hash paths that wouldn't pass `StorePath::parse`. Replaced with valid generated paths.
- `Arc::clone(&x)` style unification — some places had `x.clone()` where `x: Arc<T>` (confusing — is this deep or shallow?), others `Arc::clone(&x)`. Standardized on explicit `Arc::clone`.

## Files

```json files
[
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "delete dead cpu_usage_usec"},
  {"path": "rio-worker/src/executor/daemon/stderr_loop.rs", "action": "MODIFY", "note": "delete unnecessary clone"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "delete unnecessary clone in evict"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-worker/src/health.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "delete test_cache_len + Arc::clone"},
  {"path": "rio-store/src/cache_server.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-store/src/backend/chunk.rs", "action": "MODIFY", "note": "ChunkSession Drop test wrapper"},
  {"path": "rio-store/src/gc/mark.rs", "action": "MODIFY", "note": "test_store_path helper (was 3-char invalid hashes)"},
  {"path": "rio-store/src/gc/drain.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-store/tests/grpc/chunk_service.rs", "action": "MODIFY", "note": "ChunkSession + expect_err + Arc::clone"},
  {"path": "rio-store/tests/grpc/core.rs", "action": "MODIFY", "note": "test_store_path + expect_err"},
  {"path": "rio-store/tests/grpc/main.rs", "action": "MODIFY", "note": "spawn_grpc_server helper + Arc::clone"},
  {"path": "rio-nix/src/protocol/derived_path.rs", "action": "MODIFY", "note": "delete OutputNames newtype"},
  {"path": "rio-nix/src/protocol/handshake.rs", "action": "MODIFY", "note": "inline NO_STRINGS"},
  {"path": "rio-nix/src/protocol/wire/framed.rs", "action": "MODIFY", "note": "dead import"},
  {"path": "rio-nix/src/protocol/wire/mod.rs", "action": "MODIFY", "note": "visibility"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "privatize StorePathHash"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "remove _active param"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "as_deref + then_some"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "flatten provably-dead if-let"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "consume loop + drop intermediate Vec"},
  {"path": "rio-gateway/tests/golden_conformance.rs", "action": "MODIFY", "note": "ATerm literal dedup"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "MODIFY", "note": "ATerm literal dedup"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "drop unread Debug fields"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "consume newly_ready"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "style"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "from_recovery_row ownership"},
  {"path": "rio-scheduler/src/actor/tests/dispatch.rs", "action": "MODIFY", "note": "setup_actor_configured"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "spawn boilerplate extracted"},
  {"path": "rio-scheduler/src/actor/tests/misc.rs", "action": "MODIFY", "note": "use setup_actor_configured"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-scheduler/src/lease.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-scheduler/src/logs/flush.rs", "action": "MODIFY", "note": "dead import + Arc::clone"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "Arc::clone style"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "from_recovery_row ownership"}
]
```

## Tracey

No tracey markers landed. Pure cleanup.

## Entry

- Depends on P0145: coverage tests surface dead code (0% functions).

## Exit

Merged as `e8334a1..7621333` (12 commits). `.#ci` green at merge. Net -~300 lines.

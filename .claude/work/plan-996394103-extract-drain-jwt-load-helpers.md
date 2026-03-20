# Plan 996394103: Extract spawn_drain_task + load_and_wire_jwt from main.rs ×3

Consolidator (mc100) found `spawn_monitored("drain-on-sigterm", ...)` duplicated across three `main.rs` files: [`rio-scheduler/src/main.rs:450`](../../rio-scheduler/src/main.rs), [`rio-store/src/main.rs:242`](../../rio-store/src/main.rs), [`rio-gateway/src/main.rs:302`](../../rio-gateway/src/main.rs). ~20L each, same shape: `parent.cancelled() → reporter.set_not_serving() → sleep(grace) → child.cancel()`. [P0343](plan-0343-extract-spawn-health-plaintext-common.md) (DONE) extracted the ADJACENT `spawn_health_plaintext` from these SAME three files this window but left drain untouched. Net ~-40L.

Review of [P0349](plan-0349-wire-spawn-pubkey-reload-main-rs.md) (UNIMPL, in frontier) adds a second consolidation target in the same files: a 21-line `match cfg.jwt.key_path { Some(p) => load + Arc::new(RwLock) + spawn_pubkey_reload + tracing::info, None => ... }` block duplicated verbatim in `scheduler/main.rs:667-687` and `store/main.rs:487-507` (p349 worktree refs). The [`spawn_pubkey_reload`](../../rio-common/src/jwt_interceptor.rs) docstring at `:128` says "do not inline ×2" — it consolidated the reload closure but not the caller boilerplate.

**Worth it NOW:** P0349 is LIVE and touches scheduler+store main.rs. [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) (DONE) touched gateway main.rs. Third main.rs-touching plan in 5 merges (P0343/P0260/P0349). Scheduler main.rs collision=32, store=28. Extracting before more churn reduces future conflict surface.

## Entry criteria

- [P0343](plan-0343-extract-spawn-health-plaintext-common.md) merged (`rio-common/src/server.rs` exists with `spawn_health_plaintext`) — **DONE**
- [P0349](plan-0349-wire-spawn-pubkey-reload-main-rs.md) merged (the 21L jwt.key_path match block exists in scheduler+store main.rs — T2 extracts it)

## Tasks

### T1 — `refactor(common):` extract spawn_drain_task to server.rs

MODIFY [`rio-common/src/server.rs`](../../rio-common/src/server.rs) — alongside `spawn_health_plaintext`:

```rust
/// Spawn the drain-on-SIGTERM task: wait for parent token cancel,
/// flip health to NOT_SERVING, sleep grace_secs, then cancel the
/// serve_shutdown token. The serve_shutdown token is the ONE tonic
/// Server::serve_with_shutdown awaits — an INDEPENDENT token, not a
/// child of `parent`, so the server survives the drain window.
///
/// The `set_not_serving` closure handles the service-name divergence:
/// scheduler+store use `reporter.set_not_serving::<S>()` (named
/// service); gateway uses `reporter.set_service_status("", NotServing)`
/// (empty-string — K8s probe uses `service: ""`, see helm gateway.yaml
/// comment). Caller provides the closure.
// r[impl common.drain.not-serving-before-exit]
pub fn spawn_drain_task(
    parent: CancellationToken,
    serve_shutdown: CancellationToken,
    grace: std::time::Duration,
    set_not_serving: impl FnOnce() -> futures::future::BoxFuture<'static, ()>
        + Send + 'static,
) {
    spawn_monitored("drain-on-sigterm", async move {
        parent.cancelled().await;
        set_not_serving().await;
        tracing::info!(grace_secs = grace.as_secs(), "SIGTERM: health=NOT_SERVING, draining");
        tokio::time::sleep(grace).await;
        serve_shutdown.cancel();
    });
}
```

MODIFY three call sites:

- [`rio-scheduler/src/main.rs:444-468`](../../rio-scheduler/src/main.rs) — replace inline block with `spawn_drain_task(shutdown.clone(), serve_shutdown.clone(), Duration::from_secs(cfg.drain_grace_secs), move || Box::pin(async move { reporter.set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>().await }))`
- [`rio-store/src/main.rs:236-254`](../../rio-store/src/main.rs) — same pattern, `StoreServiceServer<StoreServiceImpl>`
- [`rio-gateway/src/main.rs:296-316`](../../rio-gateway/src/main.rs) — `move || Box::pin(async move { reporter.set_service_status("", NotServing).await })`

**Preserve the comments.** The scheduler's `:435-442` block ("INDEPENDENT token fired only by drain task...") is load-bearing — move it to `spawn_drain_task`'s docstring.

### T2 — `refactor(common):` extract load_and_wire_jwt to jwt_interceptor.rs

MODIFY [`rio-common/src/jwt_interceptor.rs`](../../rio-common/src/jwt_interceptor.rs) — new helper alongside `spawn_pubkey_reload`:

```rust
/// Load JWT pubkey from `key_path` (if Some), wrap in Arc<RwLock>,
/// spawn the SIGHUP reload task, return the shared pubkey slot.
///
/// Consolidates the 21-line `match cfg.jwt.key_path` block that
/// P0349 duplicated in scheduler/main.rs + store/main.rs.
/// `spawn_pubkey_reload` already existed; this wraps the caller
/// boilerplate around it.
pub fn load_and_wire_jwt(
    key_path: Option<&std::path::Path>,
    shutdown: CancellationToken,
) -> anyhow::Result<JwtPubkey> {
    match key_path {
        Some(p) => {
            let key = load_jwt_pubkey(p)?;
            let slot = Arc::new(RwLock::new(Some(key)));
            spawn_pubkey_reload(p.to_path_buf(), Arc::clone(&slot), shutdown);
            tracing::info!(path = %p.display(), "JWT pubkey loaded; SIGHUP-reloadable");
            Ok(slot)
        }
        None => {
            tracing::info!("JWT pubkey not configured; auth disabled");
            Ok(Arc::new(RwLock::new(None)))
        }
    }
}
```

MODIFY two call sites (post-P0349-merge — line refs are p349-worktree):

- `rio-scheduler/src/main.rs:667-687` → `let jwt_pubkey = rio_common::jwt_interceptor::load_and_wire_jwt(cfg.jwt.key_path.as_deref(), shutdown.clone())?;`
- `rio-store/src/main.rs:487-507` → same

**If P0349 hasn't merged yet:** the blocks don't exist; T2 becomes a pre-write of the helper that P0349 then CALLS instead of duplicating. Check at dispatch; if P0349 is UNIMPL, coordinate: P0349 lands first with the duplication, this T2 consolidates immediately after. OR: P0349 uses `load_and_wire_jwt` directly (this plan lands the helper first, P0349 calls it — requires sequencing this before P0349, adjusting deps).

### T3 — `test(common):` spawn_drain_task behavioral test

NEW test in [`rio-common/src/server.rs`](../../rio-common/src/server.rs) (or `rio-common/tests/`):

```rust
/// spawn_drain_task: parent cancel → set_not_serving called → sleep
/// → serve_shutdown cancelled. Order is load-bearing: NOT_SERVING
/// must flip BEFORE serve_shutdown fires (K8s probe sees NOT_SERVING
/// during drain, not ECONNREFUSED).
// r[verify common.drain.not-serving-before-exit]
#[tokio::test(start_paused = true)]
async fn drain_task_sets_not_serving_before_shutdown() {
    let parent = CancellationToken::new();
    let serve_shutdown = CancellationToken::new();
    let called = Arc::new(AtomicBool::new(false));
    let c = called.clone();

    spawn_drain_task(
        parent.clone(),
        serve_shutdown.clone(),
        Duration::from_secs(6),
        move || Box::pin(async move { c.store(true, Ordering::SeqCst); }),
    );

    assert!(!serve_shutdown.is_cancelled());
    parent.cancel();
    tokio::time::advance(Duration::from_millis(10)).await;
    tokio::task::yield_now().await;
    assert!(called.load(Ordering::SeqCst), "set_not_serving not called");
    assert!(!serve_shutdown.is_cancelled(), "shutdown fired before grace");
    tokio::time::advance(Duration::from_secs(7)).await;
    tokio::task::yield_now().await;
    assert!(serve_shutdown.is_cancelled(), "shutdown not fired after grace");
}
```

The scheduler already has `drain_sets_not_serving_before_child_cancel` at `:1012` — T3's test is the common-crate unit for the extracted helper. Keep the scheduler test (integration-ish, exercises the real reporter).

## Exit criteria

- `/nbr .#ci` green
- `grep -c 'spawn_drain_task' rio-common/src/server.rs rio-scheduler/src/main.rs rio-store/src/main.rs rio-gateway/src/main.rs` → ≥4 (T1: def + 3 call sites)
- `grep 'spawn_monitored("drain-on-sigterm"' rio-scheduler/src/main.rs rio-store/src/main.rs rio-gateway/src/main.rs` → 0 hits (T1: inline blocks gone; the call is now inside `spawn_drain_task`)
- `grep -c 'load_and_wire_jwt' rio-common/src/jwt_interceptor.rs rio-scheduler/src/main.rs rio-store/src/main.rs` → ≥3 (T2: def + 2 call sites; post-P0349-merge)
- `cargo nextest run -p rio-common drain_task_sets_not_serving_before_shutdown` → pass (T3)
- `cargo nextest run -p rio-scheduler drain_sets_not_serving_before_child_cancel` → still passes (T1 didn't break the existing integration test)
- Net LOC: `git diff --shortstat` shows deletions > insertions for the three main.rs files combined (consolidation goal — P0343 precedent was net negative)
- `nix develop -c tracey query rule common.drain.not-serving-before-exit` → shows ≥1 `impl` (moved from 3 sites to 1) + ≥2 `verify` (T3 + existing scheduler test)

## Tracey

References existing markers:
- `r[common.drain.not-serving-before-exit]` — T1 moves the `r[impl]` annotation from three main.rs sites to one `spawn_drain_task` site; T3 adds a `r[verify]` at the common-crate unit level. Existing `r[verify]` at scheduler's `drain_sets_not_serving_before_child_cancel` (`:1012`) stays — now 2 verify sites.

No new markers. `spawn_drain_task` + `load_and_wire_jwt` are consolidations of existing spec'd behavior; no new spec text.

## Files

```json files
[
  {"path": "rio-common/src/server.rs", "action": "MODIFY", "note": "T1: +spawn_drain_task alongside spawn_health_plaintext; T3: +drain_task_sets_not_serving_before_shutdown test"},
  {"path": "rio-common/src/jwt_interceptor.rs", "action": "MODIFY", "note": "T2: +load_and_wire_jwt helper wrapping load_jwt_pubkey+Arc<RwLock>+spawn_pubkey_reload"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T1: :444-468 inline drain block → spawn_drain_task call; T2: :667-687 jwt.key_path match → load_and_wire_jwt call (p349 ref)"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T1: :236-254 inline drain block → spawn_drain_task call; T2: :487-507 jwt.key_path match → load_and_wire_jwt call (p349 ref)"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "T1: :296-316 inline drain block → spawn_drain_task call (set_service_status empty-string variant)"}
]
```

```
rio-common/src/
├── server.rs                # T1: spawn_drain_task + T3: test
└── jwt_interceptor.rs       # T2: load_and_wire_jwt
rio-scheduler/src/main.rs    # T1+T2: call sites
rio-store/src/main.rs        # T1+T2: call sites
rio-gateway/src/main.rs      # T1: call site (empty-string variant)
```

## Dependencies

```json deps
{"deps": [343, 349], "soft_deps": [260], "note": "consolidator-mc100 finding (drain ×3) + rev-p349 finding (jwt.key_path ×2) folded together — same 3 files. P0343 (DONE) established rio-common/src/server.rs with spawn_health_plaintext; T1 adds spawn_drain_task as sibling. P0349 (UNIMPL, in frontier) wires spawn_pubkey_reload in scheduler+store main.rs — T2 extracts its caller boilerplate. SEQUENCE: P0349→this (T2 consolidates what P0349 wrote). If coordinator prefers: land T1 (drain) independently, defer T2 until P0349 merges — drain has no P0349 dep. Scheduler main.rs collision=32, store=28 — both HOT. P0349 edits ADJACENT-NOT-OVERLAPPING lines (adds jwt field + spawn_pubkey_reload call, does not modify :444-468 drain block). Only divergence is gateway's set_service_status('') vs set_not_serving::<S>() — handled by closure param. Net ~-40L drain + ~-30L jwt = -70L. 3rd main.rs-touching plan in 5 merges — worth it NOW per consolidator."}
```

**Depends on:** [P0343](plan-0343-extract-spawn-health-plaintext-common.md) (DONE) — `server.rs` exists. [P0349](plan-0349-wire-spawn-pubkey-reload-main-rs.md) — T2 extracts the block P0349 writes; sequence P0349→this.
**Soft-dep:** [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) (DONE) — gateway's drain block at `:296-316` exists post-P0260's main.rs touch.
**Conflicts with:** [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) count=32 (HOT) — T1 replaces `:444-468`, T2 replaces `:667-687`; P0349 writes `:667-687` → sequence resolves. [`rio-store/src/main.rs`](../../rio-store/src/main.rs) count=28 — same pattern. [`rio-gateway/src/main.rs`](../../rio-gateway/src/main.rs) — T1 only (no jwt.key_path block in gateway — gateway ISSUES JWTs, doesn't verify them).

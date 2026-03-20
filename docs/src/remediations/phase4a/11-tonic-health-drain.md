# Remediation 11: tonic health drain — set_not_serving on SIGTERM

**Parent:** [§2.5 tonic health: no set_not_serving on drain](../phase4a.md#25-tonic-health-no-set_not_serving-on-drain)
**Findings:** `tonic-no-set-not-serving-scheduler`, `tonic-no-set-not-serving-store`, `tonic-no-set-not-serving-gateway`, `tonic-actor-channelsend-wrong-code`
**Priority:** P1 (HIGH)
**tracey:** new spec marker `r[common.drain.not-serving-before-exit]` (see §Spec updates)

---

## Problem

All three long-lived servers pass `shutdown.cancelled_owned()` directly to `serve_with_shutdown`. On SIGTERM:

1. `rio_common::signal::shutdown_signal()` fires the token (`signal.rs:55`).
2. `serve_with_shutdown` sees the cancelled future → stops accepting, drains in-flight, returns.
3. `main()` falls through → process exits.

Between step 1 and step 3 (typically ~50-500ms for in-flight drain), the tonic-health reporter is **still reporting SERVING**. Kubernetes' kubelet probes at `periodSeconds: 5` — it has a ~5s window where the last probe it saw was SERVING, so the pod stays in the Endpoint slice. kube-proxy / the NLB keeps routing **new** connections to a process that is tearing down.

Observed failure mode: client opens a fresh gRPC stream → TCP accept succeeds (listener is still bound during `serve_with_shutdown`'s drain) → stream gets RST when the process exits → client sees `Unavailable: transport error` with zero diagnostic context.

### Per-component probe topology

The fix is **not** uniform across the three binaries because their readinessProbes differ:

| Component | readinessProbe (`infra/helm/rio-build/templates/*.yaml`) | Who observes `set_not_serving`? |
|---|---|---|
| **store** | `grpc: {port: 9102/9002, service: rio.store.StoreService}`, `periodSeconds: 5` | **K8s kubelet** → removes from Endpoints → Service stops routing |
| **gateway** | `grpc: {port: 9190}` (empty service name), `periodSeconds: 5` | **K8s kubelet** → removes from Endpoints → NLB stops routing to SSH port 2222 |
| **scheduler** | `tcpSocket: {port: 9001}`, `periodSeconds: 5` | **NOT K8s** — tcpSocket passes as long as the listener is bound. Only **BalancedChannel clients** (gateway, controller, worker) see the health flip via their 3s probe loop (`rio-proto/src/client/balance.rs:318`, `DEFAULT_PROBE_INTERVAL`). |

The scheduler's `tcpSocket` probe is deliberate: standby replicas must stay Ready so clients can discover them for health-probing, but they're NOT_SERVING on the named service so BalancedChannel routes around them. This means **for the scheduler, `set_not_serving()` does nothing for K8s routing** — it only helps clients using the balanced channel. The drain sleep for the scheduler should therefore budget for `DEFAULT_PROBE_INTERVAL` (3s), not the kubelet period.

For **store** and **gateway**, the sleep must cover: one full probe period (5s) + endpoint-controller propagation to kube-proxy (~1s). §2.5 suggests `periodSeconds + 1`.

### The ChannelSend wrong-code finding

`rio-scheduler/src/grpc/mod.rs:138`:

```rust
ActorError::ChannelSend => Status::internal("scheduler actor is unavailable"),
```

`ChannelSend` fires when `actor.tx.send(cmd).await` returns `Err` — the actor's mpsc receiver is dropped (`rio-scheduler/src/actor/handle.rs:156,166,187,200,209,224,245,264`). This happens in exactly two scenarios:

1. Actor task **panicked** — the `JoinHandle` completes, the receiver drops.
2. Actor task **exited cleanly** on its shutdown-token `select!` arm — same effect.

Scenario 2 is the relevant one here: during the new drain-sleep window, the actor sees `shutdown.cancelled()` and exits. A late RPC that sneaks in before `serve_with_shutdown` stops accepting will hit `ChannelSend` → currently `Status::internal` → **non-retriable** → client gives up. Should be `Status::unavailable` → client's retry interceptor tries the next replica (BalancedChannel has already removed us, so retry lands on the new leader).

The message already says "unavailable"; it's only the code that's wrong.

### The TriggerGC forwarding-task leak

`rio-scheduler/src/admin/gc.rs (post-P0383 split; was admin/mod.rs:562-587)`: the TriggerGC proxy spawns a `tokio::spawn` that forwards the store's progress stream to the client. This task has **no shutdown awareness**:

```rust
tokio::spawn(async move {
    let mut store_stream = store_stream;
    loop {
        match store_stream.message().await {
            Ok(Some(progress)) => { if tx.send(Ok(progress)).await.is_err() { break; } }
            Ok(None) => break,
            Err(e) => { let _ = tx.send(Err(e)).await; break; }
        }
    }
});
```

On shutdown: `serve_with_shutdown` returns → the client's stream is torn down → `tx.send().await` **eventually** returns `Err` (receiver dropped when tonic drops the response) → task exits. But this is *reactive*, not *proactive*: if the store-side GC is in its sweep phase (can take minutes on a large store) and hasn't sent a progress message, `store_stream.message().await` blocks indefinitely. The task holds the store gRPC channel alive. During `main()`'s final `lease_loop.await` (scheduler/main.rs:595-597), this task is still pending → process exit races it.

Not a correctness bug (store-side GC completes regardless; the scopeguard at `rio-store/src/grpc/admin.rs:159-161` releases the advisory lock on connection close). But it's a leak during the drain window and delays clean exit. Same fix as every other background task: `select!` on a cloned shutdown token.

---

## Fix

### Architecture: two-stage shutdown via child token

The root tension: `serve_with_shutdown` needs a future that resolves when it's time to stop accepting, but we want to do work **after** SIGTERM and **before** that future resolves. Passing `shutdown.cancelled_owned()` directly gives no gap.

**Pattern:** the gRPC server gets a **child** token. SIGTERM fires the **parent**. A dedicated drain task waits for parent cancellation → `set_not_serving()` → sleep → cancel child → `serve_with_shutdown` returns.

`tokio_util::sync::CancellationToken::child_token()` gives exactly this: cancelling the parent cascades to the child, but the child can *also* be cancelled independently. Here we invert: we **don't** pass the parent to `serve_with_shutdown`; we pass the child and cancel it manually after the drain sleep. All other background tasks keep using the parent (they should exit immediately on SIGTERM — no drain needed for a tick loop).

One subtlety: `child_token()` cascades parent→child. If someone (future refactor) accidentally calls `shutdown.cancel()` on the parent directly instead of via SIGTERM, the child fires immediately and we lose the drain window. That's acceptable — direct-cancel is a test-only pattern and tests don't need drain. The SIGTERM path is the one we're fixing.

### Config: `drain_grace_secs`

Per §2.5: "read it from config, don't hardcode." Add to each binary's `Config` struct + `CliArgs` + Helm env. Default `6` (= `periodSeconds: 5` + 1s propagation). The scheduler could use a smaller value (`DEFAULT_PROBE_INTERVAL` is 3s → 4s would suffice) but a uniform default simplifies the Helm chart — 6s out of a 30s `terminationGracePeriodSeconds` budget is cheap.

Setting `RIO_DRAIN_GRACE_SECS=0` disables the drain sleep (useful for unit tests + VM tests that delete pods with `--grace-period=0` and don't care about connection-reset cleanliness).

### 1. rio-scheduler/src/main.rs

**Current shutdown path** (main.rs:586-600):
```rust
    .serve_with_shutdown(listen_addr, shutdown.cancelled_owned())
    .await?;

// Wait for step_down() to complete. ...
if let Some(h) = lease_loop {
    let _ = h.await;
}
```

**Complication:** the scheduler has a **health-toggle loop** (main.rs:416-449) that polls `is_leader` at 1Hz and calls `set_serving`/`set_not_serving` edge-triggered. This loop's own `select!` on `health_shutdown.cancelled()` (line 427) breaks it on SIGTERM. **If the toggle loop exits before our drain task runs, we race** — but if it runs *after*, the loop might flip back to SERVING (if `is_leader` is still true, which it is until `step_down()` completes).

**Resolution:** the drain task runs the `set_not_serving()`. The toggle loop's `break` on line 429 exits *without* setting anything (it's just `break;`). Since `set_not_serving` is an async `RwLock` write + broadcast, last-writer-wins. The only unsafe ordering is: (a) toggle loop ticks and sees `is_leader==true` → calls `set_serving()`, then (b) drain task's `set_not_serving()` runs, then (c) toggle loop ticks *again* and re-sets SERVING. Step (c) can't happen because the toggle loop broke on its `health_shutdown.cancelled()` arm (same parent token). So: **safe as long as the drain task awaits the parent token, which it does.** Add a comment.

**Also:** the plaintext health server (main.rs:537-545, only when mTLS) currently uses `health_plain_shutdown.cancelled_owned()` — same parent token → it exits immediately on SIGTERM. It should **also** wait for the drain, otherwise the K8s probe (which hits the plaintext port when mTLS is on) gets connection-refused instead of NOT_SERVING. Give it the same child token as the main server.

```diff
--- a/rio-scheduler/src/main.rs
+++ b/rio-scheduler/src/main.rs
@@ Config struct, after hmac_key_path (line 53) @@
     hmac_key_path: Option<std::path::PathBuf>,
+    /// Seconds to wait after SIGTERM between set_not_serving()
+    /// and serve_with_shutdown returning. Gives the BalancedChannel
+    /// probe loop (DEFAULT_PROBE_INTERVAL=3s) time to observe
+    /// NOT_SERVING and reroute. 0 = no drain (tests). Default 6.
+    drain_grace_secs: u64,
 }

@@ Default impl, after hmac_key_path (line 71) @@
             hmac_key_path: None,
+            // periodSeconds: 5 (helm) + 1s propagation. Uniform across
+            // all three binaries even though scheduler's actual client
+            // probe is 3s — 6s out of 30s termGrace is cheap.
+            drain_grace_secs: 6,
         }

@@ CliArgs, after log_s3_prefix (line 115) @@
     log_s3_prefix: Option<String>,
+
+    /// Drain grace period in seconds (0 = disabled)
+    #[arg(long)]
+    #[serde(skip_serializing_if = "Option::is_none")]
+    drain_grace_secs: Option<u64>,
 }

@@ Replace line 392 (after health_reporter binding) @@
     let (health_reporter, health_service) = tonic_health::server::health_reporter();
+
+    // Two-stage shutdown: `shutdown` (parent) fires on SIGTERM and
+    // stops background loops immediately. `serve_shutdown` (child)
+    // fires AFTER the drain task has called set_not_serving + slept.
+    // child_token cascades parent→child, so a direct shutdown.cancel()
+    // (test-only) still works — it just skips the drain window.
+    let serve_shutdown = shutdown.child_token();
+    {
+        let reporter = health_reporter.clone();
+        let parent = shutdown.clone();
+        let child = serve_shutdown.clone();
+        let grace = std::time::Duration::from_secs(cfg.drain_grace_secs);
+        rio_common::task::spawn_monitored("drain-on-sigterm", async move {
+            parent.cancelled().await;
+            // The health-toggle loop below breaks on the SAME parent
+            // token and its break arm does NOT call set_serving — so
+            // it cannot un-flip us here. Last write wins.
+            // r[impl common.drain.not-serving-before-exit]
+            reporter
+                .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
+                .await;
+            tracing::info!(grace_secs = grace.as_secs(), "SIGTERM: health=NOT_SERVING, draining");
+            if !grace.is_zero() {
+                tokio::time::sleep(grace).await;
+            }
+            child.cancel();
+        });
+    }

@@ Replace line 540 (plaintext health serve_with_shutdown) @@
-                .serve_with_shutdown(health_addr, health_plain_shutdown.cancelled_owned())
+                // serve_shutdown (child), not the parent: the K8s probe
+                // hits THIS port when mTLS is on. If it exits on the
+                // parent, the probe gets ECONNREFUSED instead of
+                // NOT_SERVING — same bug, different symptom.
+                .serve_with_shutdown(health_addr, health_plain_shutdown.cancelled_owned())

# Actually — line 536 binds `health_plain_shutdown = shutdown.clone()`.
# Change that binding instead of the callsite:

@@ Replace line 536 @@
-        let health_plain_shutdown = shutdown.clone();
+        let health_plain_shutdown = serve_shutdown.clone();

@@ Replace line 586 (main serve_with_shutdown) @@
-        .serve_with_shutdown(listen_addr, shutdown.cancelled_owned())
+        .serve_with_shutdown(listen_addr, serve_shutdown.cancelled_owned())

@@ config_defaults_are_stable test (after line 622 health_addr assert) @@
         assert_eq!(d.health_addr.to_string(), "0.0.0.0:9101");
+        assert_eq!(d.drain_grace_secs, 6);
```

### 2. rio-store/src/main.rs

Simpler than scheduler — no toggle loop, no lease. The store's health reporter is set SERVING once at main.rs:348 and never touched again. Same child-token pattern. The plaintext health listener (main.rs:364-374, mTLS mode) needs the child token too.

```diff
--- a/rio-store/src/main.rs
+++ b/rio-store/src/main.rs
@@ Config struct, after cache_allow_unauthenticated (line 91) @@
     cache_allow_unauthenticated: bool,
+    /// Seconds to wait after SIGTERM between set_not_serving() and
+    /// exit. Gives kubelet readinessProbe (periodSeconds: 5) time to
+    /// observe NOT_SERVING + endpoint-controller to propagate. 0 =
+    /// no drain. Default 6 (= 5 + 1).
+    drain_grace_secs: u64,
 }

@@ Default impl, after cache_allow_unauthenticated (line 112) @@
             cache_allow_unauthenticated: false,
+            drain_grace_secs: 6,
         }

@@ CliArgs: add drain_grace_secs Option<u64> (same pattern as scheduler) @@

@@ After line 198 (health_reporter binding) @@
     let (health_reporter, health_service) = tonic_health::server::health_reporter();
+
+    // Two-stage shutdown. Parent stops background tasks (drain task,
+    // orphan scanner, cache HTTP) immediately. Child stops the gRPC
+    // server AFTER set_not_serving + sleep. See scheduler main.rs for
+    // the rationale.
+    let serve_shutdown = shutdown.child_token();
+    {
+        let reporter = health_reporter.clone();
+        let parent = shutdown.clone();
+        let child = serve_shutdown.clone();
+        let grace = std::time::Duration::from_secs(cfg.drain_grace_secs);
+        rio_common::task::spawn_monitored("drain-on-sigterm", async move {
+            parent.cancelled().await;
+            // r[impl common.drain.not-serving-before-exit]
+            reporter
+                .set_not_serving::<StoreServiceServer<StoreServiceImpl>>()
+                .await;
+            tracing::info!(grace_secs = grace.as_secs(), "SIGTERM: health=NOT_SERVING, draining");
+            if !grace.is_zero() {
+                tokio::time::sleep(grace).await;
+            }
+            child.cancel();
+        });
+    }

@@ Replace line 364 (inside mTLS branch) @@
-        let health_shutdown = shutdown.clone();
+        let health_shutdown = serve_shutdown.clone();

@@ Replace line 389 @@
-        .serve_with_shutdown(addr, shutdown.cancelled_owned())
+        .serve_with_shutdown(addr, serve_shutdown.cancelled_owned())

@@ config_defaults_are_stable test (after line 417 cache_allow_unauthenticated) @@
         assert!(!d.cache_allow_unauthenticated);
+        assert_eq!(d.drain_grace_secs, 6);
```

**NOT changed:** the cache-HTTP axum server (main.rs:323, `http_shutdown.cancelled_owned()`). It keeps the **parent** token — binary-cache `GET /nar/*` is stateless; a 404-during-teardown is harmless. Only the stateful gRPC paths (PutPath mid-stream) need the drain. Could be argued either way; keeping it on the parent means background tasks exit promptly.

### 3. rio-gateway/src/main.rs

The gateway is structurally different: the main protocol is SSH (russh `server.run()`), not tonic. The health server is a **separate** tonic instance spawned at main.rs:261-270. The SSH server races against `shutdown.cancelled()` in a `select!` at main.rs:287-292.

**K8s probe topology:** `grpc: {port: 9190}` with **no** `service:` field → empty-string check. The gateway's `set_serving` at main.rs:254-258 uses `HealthServer<HealthService>` as the generic — tonic-health registers under **both** the type-derived name and `""` on any `set_serving` call (verified by `health_serving_after_set` test at scheduler/main.rs:737-751). But `set_not_serving::<S>()` flips **only** the named service, not `""` (verified by `health_toggle_not_serving` test at scheduler/main.rs:854-861). **So calling `set_not_serving` with the same generic won't flip the empty-string check that kubelet probes.**

**Fix:** use `HealthReporter::set_service_status("", ServingStatus::NotServing)` directly. This is the lower-level API that `set_not_serving<S>` wraps; calling it with `""` flips the whole-server check.

The health server itself (main.rs:265, `health_shutdown.cancelled_owned()`) needs the child token — if it exits on the parent, kubelet's probe gets ECONNREFUSED, which it treats as a failure, which *works* but is noisier (kubelet logs `probe failed: connection refused` instead of a clean NOT_SERVING response).

The SSH accept loop at main.rs:287-292 races `server.run()` against the parent. In-flight SSH sessions are per-session `tokio::spawn` tasks (inside russh) — dropping the `run()` future stops accepting but doesn't kill sessions. They die at process exit. For proper SSH drain we'd need to track session handles and `join_all` them, which is out of scope for this remediation (and §2.5 doesn't ask for it). The drain sleep alone is enough to stop the NLB from routing *new* SSH connections; in-flight sessions get the remaining ~24s of `terminationGracePeriodSeconds` to finish naturally.

Change the `select!` to race against the **child** token so the SSH accept loop keeps running during the drain window (so a late session that was already NLB-routed before the endpoint propagation can still connect).

```diff
--- a/rio-gateway/src/main.rs
+++ b/rio-gateway/src/main.rs
@@ Config struct, after tls (line 54) @@
     tls: rio_common::tls::TlsConfig,
+    /// Seconds to wait after SIGTERM between health=NOT_SERVING and
+    /// stopping the SSH accept loop. Gives kubelet readinessProbe
+    /// (periodSeconds: 5) + NLB target deregistration time. 0 = no
+    /// drain. Default 6.
+    drain_grace_secs: u64,
 }

@@ Default impl, after tls (line 78) @@
             tls: rio_common::tls::TlsConfig::default(),
+            drain_grace_secs: 6,
         }

@@ CliArgs: add drain_grace_secs Option<u64> @@

@@ After line 249 (health_reporter binding), BEFORE set_serving @@
     let (health_reporter, health_service) = tonic_health::server::health_reporter();
+
+    // Two-stage shutdown. Parent fires on SIGTERM; child fires after
+    // the drain sleep. The health server AND the SSH accept loop both
+    // wait for the CHILD — new SSH connections that were already
+    // NLB-routed before endpoint propagation land on a live listener.
+    let serve_shutdown = shutdown.child_token();
+    {
+        let reporter = health_reporter.clone();
+        let parent = shutdown.clone();
+        let child = serve_shutdown.clone();
+        let grace = std::time::Duration::from_secs(cfg.drain_grace_secs);
+        rio_common::task::spawn_monitored("drain-on-sigterm", async move {
+            parent.cancelled().await;
+            // r[impl common.drain.not-serving-before-exit]
+            // Gateway's probe is empty-string (no `service:` field in
+            // helm gateway.yaml). set_not_serving::<S>() only flips
+            // the NAMED service — must use set_service_status("")
+            // directly. See scheduler/main.rs health_toggle_not_serving
+            // test for the proof that named-only is tonic-health's
+            // behavior.
+            reporter
+                .set_service_status("", tonic_health::ServingStatus::NotServing)
+                .await;
+            tracing::info!(grace_secs = grace.as_secs(), "SIGTERM: health=NOT_SERVING, draining");
+            if !grace.is_zero() {
+                tokio::time::sleep(grace).await;
+            }
+            child.cancel();
+        });
+    }
+
     // Generic param: we don't have a "GatewayService" proto. Use the

@@ Replace line 260 @@
-    let health_shutdown = shutdown.clone();
+    let health_shutdown = serve_shutdown.clone();

@@ Replace lines 287-292 (the select!) @@
-    tokio::select! {
-        r = server.run(host_key, cfg.listen_addr) => r?,
-        _ = shutdown.cancelled() => {
-            info!("gateway shut down cleanly");
-        }
-    }
+    // Race against serve_shutdown (child), not the parent: the SSH
+    // accept loop stays live during the drain window so late-routed
+    // connections (NLB propagation lag) still land on a listener.
+    tokio::select! {
+        r = server.run(host_key, cfg.listen_addr) => r?,
+        _ = serve_shutdown.cancelled() => {
+            info!("gateway shut down cleanly");
+        }
+    }

@@ config_defaults_are_stable test (after line 316 authorized_keys) @@
         assert!(d.authorized_keys.as_os_str().is_empty());
+        assert_eq!(d.drain_grace_secs, 6);
```

### 4. rio-scheduler/src/grpc/mod.rs:138 — ChannelSend → Unavailable

One-line change + one-line test fix.

```diff
--- a/rio-scheduler/src/grpc/mod.rs
+++ b/rio-scheduler/src/grpc/mod.rs
@@ -135,7 +135,12 @@ impl SchedulerGrpc {
             ActorError::Backpressure => {
                 Status::resource_exhausted("scheduler is overloaded, please retry later")
             }
-            ActorError::ChannelSend => Status::internal("scheduler actor is unavailable"),
+            // ChannelSend = actor's mpsc receiver dropped. Either the
+            // actor panicked OR it exited on its shutdown-token arm
+            // during drain. UNAVAILABLE (retriable) not INTERNAL —
+            // BalancedChannel clients retry on the next replica; with
+            // INTERNAL they'd surface the error to the user.
+            ActorError::ChannelSend => Status::unavailable("scheduler actor is unavailable"),
             ActorError::Database(e) => Status::internal(format!("database error: {e}")),
```

```diff
--- a/rio-scheduler/src/grpc/tests.rs
+++ b/rio-scheduler/src/grpc/tests.rs
@@ -612,7 +612,7 @@ fn test_actor_error_to_status_all_arms() {
             Code::ResourceExhausted,
             "overloaded",
         ),
-        (ActorError::ChannelSend, Code::Internal, "unavailable"),
+        (ActorError::ChannelSend, Code::Unavailable, "unavailable"),
         (
```

### 5. rio-scheduler/src/admin/gc.rs (post-P0383 split; was admin/mod.rs:562-587) — TriggerGC forward task shutdown

Add a `select!` arm. Requires threading a `CancellationToken` into `AdminServiceImpl`. The constructor at `AdminServiceImpl::new` (called at main.rs:469) already takes 7 args; adding an 8th is ugly but consistent with how every other long-lived struct gets its shutdown token.

```diff
--- a/rio-scheduler/src/admin/gc.rs
+++ b/rio-scheduler/src/admin/gc.rs
@@ struct AdminServiceImpl, after store_addr field @@
     store_addr: String,
+    /// For aborting long-running proxy tasks (TriggerGC forward).
+    /// Parent token, not serve_shutdown — the forward task should
+    /// exit IMMEDIATELY on SIGTERM (store-side GC continues; we
+    /// just stop forwarding progress to a client who's about to be
+    /// disconnected anyway).
+    shutdown: rio_common::signal::Token,

@@ AdminServiceImpl::new signature + body: add shutdown param @@

@@ Replace the tokio::spawn body at lines 562-587 @@
         let (tx, rx) = mpsc::channel::<Result<GcProgress, Status>>(8);
+        let shutdown = self.shutdown.clone();
         tokio::spawn(async move {
             let mut store_stream = store_stream;
             loop {
-                match store_stream.message().await {
+                // biased: check shutdown first. A store-side sweep
+                // can go minutes between progress messages; without
+                // this arm the task holds the store channel alive
+                // past main()'s lease_loop.await (main.rs:595).
+                let msg = tokio::select! {
+                    biased;
+                    _ = shutdown.cancelled() => {
+                        tracing::debug!("TriggerGC forward: shutdown, dropping store stream");
+                        break;
+                    }
+                    m = store_stream.message() => m,
+                };
+                match msg {
                     Ok(Some(progress)) => {
```

And at `rio-scheduler/src/main.rs:469`, add `shutdown.clone()` as the last argument to `AdminServiceImpl::new(...)`.

---

## Helm

No new env var **required** — default `drain_grace_secs: 6` matches the existing `periodSeconds: 5` in all three templates. But wire it through so operators can tune:

```diff
--- a/infra/helm/rio-build/templates/scheduler.yaml
+++ b/infra/helm/rio-build/templates/scheduler.yaml
@@ env block (after RIO_DATABASE_URL, ~line 86) @@
             - name: RIO_DATABASE_URL
               ...
+            - name: RIO_DRAIN_GRACE_SECS
+              value: "6"  # readinessProbe.periodSeconds (5) + 1
```

Same for `store.yaml` and `gateway.yaml`. **Keep the value in sync with `periodSeconds`** — if someone bumps the probe period to 10s, `RIO_DRAIN_GRACE_SECS` should be 11. A Helm `_helpers.tpl` that derives one from the other would be nicer but out of scope.

**`terminationGracePeriodSeconds`:** none of the three templates set it → K8s default 30s. Drain sleep (6s) + in-flight gRPC drain (~1s) + lease step_down PATCH (~1s) ≈ 8s. Fits comfortably. No change needed.

---

## Tests

### Unit: drain task flips health then cancels child

`rio-scheduler/src/main.rs` tests module already has `spawn_health_server()` helper (main.rs:650-664) and `health_toggle_not_serving` (main.rs:837-901) proving the `set_not_serving` mechanism. New test exercises the **sequencing**: parent cancel → NOT_SERVING observable → child cancelled.

```rust
/// r[verify common.drain.not-serving-before-exit]
/// Drain task sequencing: parent token cancel → health flips to
/// NOT_SERVING → child token cancels AFTER grace. A client checking
/// health between parent-cancel and child-cancel sees NOT_SERVING.
/// This is the window K8s kubelet needs to pull us from Endpoints.
#[tokio::test]
async fn drain_sets_not_serving_before_child_cancel() -> anyhow::Result<()> {
    use tonic_health::pb::{
        HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
    };
    use tokio_util::sync::CancellationToken;

    let (addr, reporter) = spawn_health_server().await;
    reporter
        .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
        .await;

    let parent = CancellationToken::new();
    let child = parent.child_token();
    // Short grace for test speed — the production default is 6s.
    let grace = std::time::Duration::from_millis(200);

    // Inline the drain task body (can't call main()).
    {
        let reporter = reporter.clone();
        let parent = parent.clone();
        let child = child.clone();
        tokio::spawn(async move {
            parent.cancelled().await;
            reporter
                .set_not_serving::<SchedulerServiceServer<SchedulerGrpc>>()
                .await;
            tokio::time::sleep(grace).await;
            child.cancel();
        });
    }

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = HealthClient::new(channel);
    let req = || HealthCheckRequest {
        service: "rio.scheduler.SchedulerService".into(),
    };

    // Pre-cancel: SERVING, child not cancelled.
    assert_eq!(
        ServingStatus::try_from(client.check(req()).await?.into_inner().status)?,
        ServingStatus::Serving
    );
    assert!(!child.is_cancelled());

    // Fire parent. The drain task is woken; set_not_serving is an
    // async RwLock write — yield to let it run.
    parent.cancel();
    tokio::task::yield_now().await;
    // A few more yields for the broadcast to propagate to the
    // health service's watch channel.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }

    // CRITICAL ASSERTION: during the grace window, health is
    // NOT_SERVING but child is NOT YET cancelled. This is the
    // window where kubelet probes NOT_SERVING → removes endpoint.
    assert_eq!(
        ServingStatus::try_from(client.check(req()).await?.into_inner().status)?,
        ServingStatus::NotServing,
        "health must flip BEFORE child cancels — this is the drain window"
    );
    assert!(
        !child.is_cancelled(),
        "child must NOT cancel until grace elapses — serve_with_shutdown \
         would return early and we'd exit while kubelet still thinks SERVING"
    );

    // After grace: child cancelled.
    tokio::time::timeout(grace * 3, child.cancelled())
        .await
        .expect("child should cancel within ~grace");

    Ok(())
}
```

### Unit: gateway empty-string flip

The gateway uses `set_service_status("", NotServing)` because its probe has no `service:` field. Prove this actually flips the empty-string check (the named-only behavior of `set_not_serving<S>` is already proven by `health_toggle_not_serving`; this is the complement).

```rust
/// Gateway's kubelet probe sends empty service name. set_not_serving<S>
/// does NOT flip "" (proven by health_toggle_not_serving). This test
/// proves set_service_status("", NotServing) DOES.
#[tokio::test]
async fn set_service_status_empty_string_flips_whole_server() -> anyhow::Result<()> {
    use tonic_health::pb::{
        HealthCheckRequest, health_check_response::ServingStatus, health_client::HealthClient,
    };

    let (addr, reporter) = spawn_health_server().await;
    // Register "" as SERVING (side effect of any set_serving call).
    reporter
        .set_serving::<SchedulerServiceServer<SchedulerGrpc>>()
        .await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
        .connect()
        .await?;
    let mut client = HealthClient::new(channel);
    let empty = || HealthCheckRequest { service: String::new() };

    assert_eq!(
        ServingStatus::try_from(client.check(empty()).await?.into_inner().status)?,
        ServingStatus::Serving
    );

    // The gateway's drain call.
    reporter
        .set_service_status("", tonic_health::ServingStatus::NotServing)
        .await;

    assert_eq!(
        ServingStatus::try_from(client.check(empty()).await?.into_inner().status)?,
        ServingStatus::NotServing,
        "gateway drain must flip the empty-string check — that's what \
         kubelet probes when helm gateway.yaml has no grpc.service field"
    );
    Ok(())
}
```

### Unit: ChannelSend → Unavailable (already covered, just flip expectation)

`rio-scheduler/src/grpc/tests.rs:615` — change `Code::Internal` to `Code::Unavailable`. The test already asserts the message contains `"unavailable"`; that stays.

### Unit: TriggerGC forward task exits on shutdown

```rust
/// TriggerGC forward task exits promptly on shutdown even when the
/// store stream is silent. Without the select! arm, store_stream.
/// message().await blocks indefinitely and the task outlives main().
#[tokio::test]
async fn trigger_gc_forward_exits_on_shutdown() {
    use tokio_util::sync::CancellationToken;
    // Mock store_stream that never yields: a mpsc channel where we
    // hold the sender but never send. .message().await on the
    // receiver-backed Streaming blocks until sender drops.
    let (_store_tx, store_rx) = tokio::sync::mpsc::channel::<Result<GcProgress, Status>>(1);
    let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<Result<GcProgress, Status>>(8);
    let shutdown = CancellationToken::new();

    // Inline the forward-task body (matches admin/gc.rs (post-P0383 split; was admin/mod.rs:562-587) post-fix).
    let task = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut store_stream = tokio_stream::wrappers::ReceiverStream::new(store_rx);
            use futures_util::StreamExt;
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    m = store_stream.next() => m,
                };
                match msg {
                    Some(Ok(p)) => { if client_tx.send(Ok(p)).await.is_err() { break; } }
                    Some(Err(e)) => { let _ = client_tx.send(Err(e)).await; break; }
                    None => break,
                }
            }
        })
    };

    // No messages sent — task is blocked on store_stream.next().
    // Without the shutdown arm this would hang the test.
    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("task should exit within 1s of shutdown")
        .expect("task should not panic");

    // Client stream sees EOF (forward task dropped client_tx).
    assert!(client_rx.recv().await.is_none());
}
```

### VM test: not in scope for this remediation

The existing `vm-le-stability-k3s` → `graceful-release` subtest (`nix/tests/scenarios/leader-election.nix`) already exercises SIGTERM → step_down → standby acquires. It doesn't assert "no connection-reset during drain" because that requires a client actively opening connections during the 6s window — fiddly under TCG timing variance. The unit test above is a tighter assertion of the **mechanism**; the VM test already covers the end-to-end "process exits cleanly on SIGTERM" path.

If we want VM-level coverage later: in `lifecycle.nix`, during a `kubectl delete pod --grace-period=30`, poll `grpcurl -plaintext :9102 grpc.health.v1.Health/Check` on the terminating pod and assert at least one `NOT_SERVING` response before the connection refuses. Defer to phase4b — the `grpc.service` field on the probe plus the empty-string gotcha makes this brittle.

---

## Spec updates

Tracey marker: `r[common.drain.not-serving-before-exit]` — see [`observability.md`](../../observability.md). On SIGTERM, each long-lived server MUST call `set_not_serving()` on its tonic-health reporter before `serve_with_shutdown` returns, and MUST sleep for at least `readinessProbe.periodSeconds + 1` seconds between the two. For the scheduler specifically (tcpSocket readinessProbe, not gRPC health), the drain sleep signals BalancedChannel clients via their `DEFAULT_PROBE_INTERVAL` loop.

---

## Checklist

- [ ] `rio-scheduler/src/main.rs`: `drain_grace_secs` config + child token + drain task + plaintext-health uses child + `serve_with_shutdown` uses child + default-stable test
- [ ] `rio-store/src/main.rs`: same (config + child token + drain task + plaintext-health uses child + serve uses child + test)
- [ ] `rio-gateway/src/main.rs`: same, but `set_service_status("")` not `set_not_serving<S>`; SSH select! races child
- [ ] `rio-scheduler/src/grpc/mod.rs:138`: `Status::internal` → `Status::unavailable`
- [ ] `rio-scheduler/src/grpc/tests.rs:615`: `Code::Internal` → `Code::Unavailable`
- [ ] `rio-scheduler/src/admin/mod.rs`: thread `shutdown: Token` into `AdminServiceImpl`; `select!` in TriggerGC forward task at admin/gc.rs (post-P0383 split)
- [ ] `rio-scheduler/src/main.rs:469`: pass `shutdown.clone()` to `AdminServiceImpl::new`
- [ ] Unit: `drain_sets_not_serving_before_child_cancel` (scheduler/main.rs tests)
- [ ] Unit: `set_service_status_empty_string_flips_whole_server` (scheduler/main.rs tests — lives here because that's where `spawn_health_server` is)
- [ ] Unit: `trigger_gc_forward_exits_on_shutdown` (scheduler/admin/tests.rs or admin/tests/gc_tests.rs post-P0386)
- [ ] Helm: `RIO_DRAIN_GRACE_SECS` env in scheduler.yaml, store.yaml, gateway.yaml
- [ ] Spec: `r[common.drain.not-serving-before-exit]` in observability.md or architecture.md
- [ ] `nix develop -c cargo nextest run -p rio-scheduler -p rio-store -p rio-gateway`
- [ ] `nix develop -c cargo clippy --all-targets -- --deny warnings`

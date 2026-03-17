# §1.4 — Eager connect without retry (gateway + worker)

**Parent:** [`phase4a.md` §1.4](../phase4a.md#14-eager-connect-without-retry-gateway--worker)
**Findings:** `tonic-gateway-eager-connect-no-retry`, `tonic-worker-eager-connect-no-retry`
**Status:** OPEN · P0 · simplest P0 in the batch

---

## 1. Reference pattern

The controller already fixed this. `rio-controller/src/main.rs:192-232`, motivated by the 2026-03-16 coverage-full failures (k3s flannel CNI race → scheduler Service has no endpoints at t≈185s → bare `?` → CrashLoopBackOff → 180s test budget gone).

```rust
// rio-controller/src/main.rs:192-232 (verbatim, reference only — do NOT edit)
let (admin, sched_client, _balance_guard) = loop {
    let result: anyhow::Result<_> = match &cfg.scheduler_balance_host {
        None => {
            info!(addr = %cfg.scheduler_addr, "connecting to scheduler (single-channel)");
            async {
                let admin = rio_proto::client::connect_admin(&cfg.scheduler_addr).await?;
                let sched = rio_proto::client::connect_scheduler(&cfg.scheduler_addr).await?;
                Ok((admin, sched, None))
            }
            .await
        }
        Some(host) => {
            info!(
                %host, port = cfg.scheduler_balance_port,
                "connecting to scheduler (health-aware balanced)"
            );
            async {
                let (admin, bc) = rio_proto::client::balance::connect_admin_balanced(
                    host.clone(),
                    cfg.scheduler_balance_port,
                )
                .await?;
                // Reuse the same balanced Channel for the
                // SchedulerServiceClient --- ONE probe loop,
                // BOTH clients route only to the leader.
                let sched = rio_proto::SchedulerServiceClient::new(bc.channel())
                    .max_decoding_message_size(rio_proto::max_message_size())
                    .max_encoding_message_size(rio_proto::max_message_size());
                Ok((admin, sched, Some(bc)))
            }
            .await
        }
    };
    match result {
        Ok(triple) => break triple,
        Err(e) => {
            warn!(error = %e, "scheduler connect failed; retrying in 2s (pod stays not-Ready)");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
};
```

Key properties to preserve:

| Property | Why it matters |
|---|---|
| `loop { ... break ... }` returns the tuple directly | No `Option` unwrapping, no mutable outer binding |
| Both single-channel and balanced arms are inside the loop | Balanced `BalancedChannel::new` DNS-resolves the headless Service — fails the same way when no endpoints exist |
| All connects for one binary are in **one** loop body | Partial success (store OK, scheduler refused) doesn't leak a Channel — next iteration reconnects everything |
| Fixed 2s sleep, no exponential | Startup races resolve in seconds. Exponential backoff is for flapping deps; this is a one-shot cold-start gate. KISS. |
| Health server spawn is **after** the loop (`:243`) | Pod stays `0/1 Running` (not-Ready) until deps are reachable — Deployment's Available condition gates correctly |

**What the controller pattern does NOT have** (and we won't add): shutdown-token cancellation inside the loop. Controller has no `shutdown_signal()` at this point in `main()`. A SIGTERM during the retry loop kills the process uncleanly, which is fine — nothing has been set up yet, no coverage profraw to flush, no in-flight builds. Keep this symmetry.

---

## 2. Gateway fix

**Sites:** `rio-gateway/src/main.rs:202` (store), `:217` (scheduler single), `:230` (scheduler balanced)
**Health server:** currently `:249-270`, already AFTER the connects — **no move needed**, just wrap the connects.

The gateway is the cleanest case: three bare `.await?` sites, health server already in the right place, `shutdown` token already exists at `:196` but we won't use it (see §1 note).

### Diff

```diff
--- a/rio-gateway/src/main.rs
+++ b/rio-gateway/src/main.rs
@@ -198,36 +198,56 @@
     rio_common::observability::init_metrics(cfg.metrics_addr)?;
     rio_gateway::describe_metrics();

-    info!(addr = %cfg.store_addr, "connecting to store service");
-    let store_client = rio_proto::client::connect_store(&cfg.store_addr).await?;
-
-    // Scheduler connection. Two modes:
-    // - Balanced (K8s): DNS-resolve headless Service, health-probe
-    //   each pod IP, route to the leader. The BalancedChannel guard
-    //   must live for process lifetime — dropping it stops the probe
-    //   loop. We box it into an Option and just hold it.
-    // - Single (non-K8s): plain connect to ClusterIP Service or a
-    //   fixed addr. VM tests and local dev use this.
-    //
-    // The branch is on Option, not on "am I in K8s." Explicit
-    // config (env unset → None), no magic detection.
-    let (scheduler_client, _balance_guard) = match cfg.scheduler_balance_host {
-        None => {
-            info!(addr = %cfg.scheduler_addr, "connecting to scheduler service (single-channel)");
-            let c = rio_proto::client::connect_scheduler(&cfg.scheduler_addr).await?;
-            (c, None)
-        }
-        Some(host) => {
-            info!(
-                %host,
-                port = cfg.scheduler_balance_port,
-                "connecting to scheduler service (health-aware balanced)"
-            );
-            let (c, bc) = rio_proto::client::balance::connect_scheduler_balanced(
-                host,
-                cfg.scheduler_balance_port,
-            )
-            .await?;
-            (c, Some(bc))
-        }
-    };
+    // Retry-until-connected. Cold-start race: all rio-* pods start
+    // in parallel (helm install, node drain+reschedule); this process
+    // can reach here before rio-store / rio-scheduler Services have
+    // endpoints. connect_* uses eager .connect().await → refused →
+    // Err. Bare `?` meant process-exit → CrashLoopBackOff → kubelet's
+    // 10s/20s/40s backoff delays recovery past dep readiness.
+    // Retry internally: pod stays not-Ready (health server below
+    // hasn't spawned yet). Same pattern as rio-controller/src/main.rs:192.
+    //
+    // Both connects in ONE loop body: partial success (store OK,
+    // scheduler refused) reconnects store on next iteration rather
+    // than leaking a half-configured state.
+    //
+    // Scheduler has two modes:
+    // - Balanced (K8s): DNS-resolve headless Service, health-probe
+    //   each pod IP, route to the leader. The BalancedChannel guard
+    //   must live for process lifetime — dropping it stops the probe
+    //   loop. Held in _balance_guard.
+    // - Single (non-K8s): plain connect. VM tests and local dev.
+    let (store_client, scheduler_client, _balance_guard) = loop {
+        let result: anyhow::Result<_> = async {
+            info!(addr = %cfg.store_addr, "connecting to store service");
+            let store = rio_proto::client::connect_store(&cfg.store_addr).await?;
+
+            let (sched, guard) = match &cfg.scheduler_balance_host {
+                None => {
+                    info!(addr = %cfg.scheduler_addr, "connecting to scheduler (single-channel)");
+                    let c = rio_proto::client::connect_scheduler(&cfg.scheduler_addr).await?;
+                    (c, None)
+                }
+                Some(host) => {
+                    info!(
+                        %host, port = cfg.scheduler_balance_port,
+                        "connecting to scheduler (health-aware balanced)"
+                    );
+                    let (c, bc) = rio_proto::client::balance::connect_scheduler_balanced(
+                        host.clone(),
+                        cfg.scheduler_balance_port,
+                    )
+                    .await?;
+                    (c, Some(bc))
+                }
+            };
+            Ok((store, sched, guard))
+        }
+        .await;
+        match result {
+            Ok(triple) => break triple,
+            Err(e) => {
+                warn!(error = %e, "upstream connect failed; retrying in 2s (pod stays not-Ready)");
+                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
+            }
+        }
+    };
```

**Gotcha — balanced arm move semantics:** current code at `:220` takes `Some(host)` by value. Inside a retry loop that's a use-after-move on iteration 2. Match on `&cfg.scheduler_balance_host` and `host.clone()` into the balanced connect (exactly as the controller does at `:193` / `:210`).

**Gotcha — comment at `:238-243` is now stale:** "store/scheduler connect are `.await?` (fail-fast) so by the time we're here, both are up" — still true in effect (loop exited ⇒ both up) but the "fail-fast" wording is wrong. Rewrite:

```diff
-    // SERVING gate: both gRPC connects above succeeded. That's the right
-    // signal — a gateway that can't reach the scheduler would accept SSH
-    // connections and then hang every wopBuild* opcode. Better to fail
-    // readiness so K8s doesn't route to this pod until it's actually
-    // usable. store/scheduler connect are `.await?` (fail-fast) so by
-    // the time we're here, both are up.
+    // SERVING gate: retry loop above exited ⇒ both store + scheduler
+    // are reachable. A gateway that can't reach the scheduler would
+    // accept SSH and then hang every wopBuild* opcode — fail readiness
+    // instead so K8s doesn't route here.
```

---

## 3. Worker fix

**Sites:** `rio-worker/src/main.rs:157` (store), `:161` (scheduler single), `:170` (scheduler balanced)
**Health server:** `:147` — **BEFORE** the connects. This is the interesting one.

### Health server: keep it where it is (DO NOT move)

§1.4 offers two options. **Keep the health server at `:147`, start `ready=false`, flip after connect.** Justification:

1. **The `ready` flag already starts `false` and already gates on something strictly stronger than "connected."** Line `:146`: `AtomicBool::new(false)`. Line `:311`: flipped `true` only after the **first heartbeat comes back accepted** — which requires connect + register + one full RPC round-trip. A retry loop that delays reaching `:311` doesn't change the readiness contract at all. `/readyz` is correct today and stays correct.

2. **`/healthz` liveness semantics are already right for a retry loop.** The doc comment at `rio-worker/src/health.rs:76-80`: *"liveness is 'is the process OK' not 'is the process USEFUL.' A worker that can't reach the scheduler is still alive; restarting won't fix the network."* A worker looping on connect-retry **is** alive and **should** pass liveness. Moving the health spawn after the loop would make `/healthz` fail during retry → kubelet restarts the pod → exactly the CrashLoopBackOff we're trying to avoid, just triggered by the liveness probe instead of process exit.

3. **The comment at `:141-145` already documents this intent.** *"Spawned BEFORE gRPC connect so liveness passes as soon as the process is up (connect may take seconds if scheduler DNS is slow to resolve)."* The author anticipated slow connects; they just didn't anticipate connect *failing* (vs. blocking). The retry loop turns failures into slowness — the original intent now actually holds.

4. **Contrast with gateway/controller:** those use `tonic_health` / always-200 `/healthz` with no separate readiness flag. For them, "move health after the loop" is the only lever. The worker's split liveness/readiness design is strictly more expressive — use it.

**Net:** `kubectl get pods` shows `0/1 Running` (not `CrashLoopBackOff`, not `0/1 Ready` timeout-pending) during the retry window. Operator can `kubectl logs` and see the `warn!` retry lines. Best observability of the three binaries.

### Diff

```diff
--- a/rio-worker/src/main.rs
+++ b/rio-worker/src/main.rs
@@ -146,31 +146,51 @@
     let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
     rio_worker::health::spawn_health_server(cfg.health_addr, Arc::clone(&ready));

-    // Connect to gRPC services. Scheduler has two modes:
-    // - Balanced (K8s, multi-replica scheduler): DNS-resolve the
-    //   headless Service, health-probe pod IPs, route to leader.
-    //   Heartbeat (separate unary RPC) routes through the same
-    //   balanced channel — when leadership flips, the probe task
-    //   sees NOT_SERVING on the old leader within one tick (~3s)
-    //   and removes it; next heartbeat goes to the new leader.
-    // - Single (non-K8s): plain connect. VM tests use this.
-    let store_client = rio_proto::client::connect_store(&cfg.store_addr).await?;
-    let (mut scheduler_client, _balance_guard) = match cfg.scheduler_balance_host {
-        None => {
-            info!(addr = %cfg.scheduler_addr, "scheduler: single-channel mode");
-            let c = rio_proto::client::connect_worker(&cfg.scheduler_addr).await?;
-            (c, None)
-        }
-        Some(host) => {
-            info!(%host, port = cfg.scheduler_balance_port, "scheduler: health-aware balanced mode");
-            let (c, bc) = rio_proto::client::balance::connect_worker_balanced(
-                host,
-                cfg.scheduler_balance_port,
-            )
-            .await?;
-            (c, Some(bc))
-        }
-    };
+    // Retry-until-connected. Cold-start race (see rio-controller/src/
+    // main.rs:168 for the full story): store/scheduler Services may
+    // have no endpoints yet. Bare `?` → process exit → kubelet sees
+    // exit-after-liveness-passed → restart → same race → flapping.
+    // Retry internally: /healthz stays 200 (process IS alive, restart
+    // won't help), /readyz stays 503 (ready flag won't flip until
+    // first heartbeat accepted, far past this loop).
+    //
+    // Scheduler has two modes:
+    // - Balanced (K8s, multi-replica): DNS-resolve headless Service,
+    //   health-probe pod IPs, route to leader. Heartbeat routes
+    //   through the same balanced channel — leadership flip detected
+    //   within one probe tick (~3s).
+    // - Single (non-K8s): plain connect. VM tests use this.
+    let (store_client, mut scheduler_client, _balance_guard) = loop {
+        let result: anyhow::Result<_> = async {
+            let store = rio_proto::client::connect_store(&cfg.store_addr).await?;
+            let (sched, guard) = match &cfg.scheduler_balance_host {
+                None => {
+                    info!(addr = %cfg.scheduler_addr, "scheduler: single-channel mode");
+                    let c = rio_proto::client::connect_worker(&cfg.scheduler_addr).await?;
+                    (c, None)
+                }
+                Some(host) => {
+                    info!(
+                        %host, port = cfg.scheduler_balance_port,
+                        "scheduler: health-aware balanced mode"
+                    );
+                    let (c, bc) = rio_proto::client::balance::connect_worker_balanced(
+                        host.clone(),
+                        cfg.scheduler_balance_port,
+                    )
+                    .await?;
+                    (c, Some(bc))
+                }
+            };
+            Ok((store, sched, guard))
+        }
+        .await;
+        match result {
+            Ok(triple) => break triple,
+            Err(e) => {
+                warn!(error = %e, "upstream connect failed; retrying in 2s (liveness stays 200, readiness stays 503)");
+                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
+            }
+        }
+    };
```

Same move-semantics gotcha: `match cfg.scheduler_balance_host` → `match &cfg.scheduler_balance_host` + `host.clone()`.

---

## 4. Shared helper? → **No.** Keep three inline loops.

§1.4 suggests `rio-common::connect_with_retry()`. After reading the actual code: **don't extract.**

### What the helper would need to abstract

The three loop bodies are not "connect to an endpoint." They are:

| Binary | Body |
|---|---|
| controller | `match balance_host { None => (connect_admin, connect_scheduler), Some => (connect_admin_balanced, reuse bc.channel() for sched) }` → 3-tuple |
| gateway | `connect_store` + `match balance_host { None => connect_scheduler, Some => connect_scheduler_balanced }` → 3-tuple |
| worker | `connect_store` + `match balance_host { None => connect_worker, Some => connect_worker_balanced }` → 3-tuple |

Three different client types, three different balanced-channel reuse strategies, three different tuple shapes. The only common structure is `loop { match body().await { Ok => break, Err => warn+sleep } }`. That's **7 lines**.

### Why extraction makes it worse

1. **The generic helper is harder to read than the duplication.** A `retry<F, T>(f: F) -> T where F: FnMut() -> impl Future<Output = Result<T>>` helper obscures the one interesting question at each site: *what exactly is being retried together?* The inline loop makes "store + scheduler are one atomic unit" visible at the call site.

2. **Layering problem.** `rio-common` has no `rio-proto` dependency (`rio-proto/Cargo.toml:36` — `rio-common` appears only as a *dev*-dep of rio-proto). A typed `connect_with_retry` helper needs tonic's `Channel` type → either `rio-common` grows a `tonic` dep for 7 lines, or the helper goes in `rio-proto::client` and becomes a generic `async fn retry<T>(op: impl FnMut() -> ...) -> T` that has nothing proto-specific about it. Neither home is right.

3. **The `warn!` message is the most useful part and it's per-binary.** `"pod stays not-Ready"` (controller, gateway) vs `"liveness stays 200, readiness stays 503"` (worker) — these tell the operator what `kubectl get pods` will show. A shared helper would generic-ify this into `"connect failed, retrying"` which is strictly less useful.

4. **N=3 and stable.** Controller, gateway, worker. There is no fourth gRPC-client binary on the roadmap. Rule of three is met but the bodies diverge enough that the abstraction doesn't pay.

**If a fourth copy ever appears,** the right extraction is a tiny `rio_common::retry::until_ok(|| async { ... }, Duration::from_secs(2))` that takes an `FnMut` — not a connect-specific helper. Defer until then.

---

## 5. Tests

### 5a. Unit test: retry-then-succeed

The loop itself is 7 lines with no branches worth unit-testing — the interesting behaviour is "does `connect_*` return `Err` fast on a closed port" (already covered by the `CONNECT_TIMEOUT` at `rio-proto/src/client/mod.rs:82`) and "does it return `Ok` when the port opens." But the §1.4 spec helper (if we were extracting one) deserves a test proving the transition:

**Location:** `rio-proto/src/client/mod.rs` (behind `#[cfg(test)]`), since that's where `connect_channel` lives.

```rust
#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::time::Duration;

    /// Retry loop pattern: connect to a closed port, assert it fails
    /// fast (not hang), bind the port, assert next attempt succeeds.
    /// This is the contract the main.rs retry loops depend on:
    /// closed port = fast Err, not 10s CONNECT_TIMEOUT hang.
    #[tokio::test]
    async fn connect_closed_port_fails_fast_then_succeeds() {
        // Reserve a port, then close the listener — port is now free
        // but nothing's listening. connect() should get ECONNREFUSED
        // in <100ms (kernel fast-path, no SYN retry).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        // First attempt: refused, fast.
        let t0 = std::time::Instant::now();
        let err = connect_channel(&addr.to_string()).await.unwrap_err();
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "closed port should fail fast (ECONNREFUSED), got {:?} after {:?}",
            err, t0.elapsed()
        );

        // Bind a real gRPC server on that port. tonic's Server::builder
        // with no services still completes the HTTP/2 handshake —
        // connect_channel only needs the transport to come up.
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .serve_with_incoming(
                    tokio_stream::wrappers::TcpListenerStream::new(listener)
                )
                .await
        });

        // Simulate the retry loop: poll until Ok. Bounded at 10 tries
        // (= 20s with the real 2s sleep; here 50ms so the test is fast).
        let mut ch = None;
        for _ in 0..10 {
            match connect_channel(&addr.to_string()).await {
                Ok(c) => { ch = Some(c); break; }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        assert!(ch.is_some(), "connect never succeeded after port opened");

        server.abort();
    }
}
```

**Do NOT use `#[tokio::test(start_paused = true)]`** — real TCP sockets + auto-advancing mock clock fires `CONNECT_TIMEOUT` spuriously while the kernel is doing real work.

### 5b. Integration: cold-start VM scenario

New subtest in `nix/tests/scenarios/lifecycle.nix` fragments map — `cold-start-ordering`:

```python
with subtest("cold-start-ordering: gateway+worker survive store+scheduler unavailable"):
    # Stop the upstreams. Gateway and worker retry-loop; they
    # do NOT exit. Before this fix: both CrashLoopBackOff.
    server.succeed("systemctl stop rio-store rio-scheduler")

    # Restart downstream. With bare `?` this would exit in <1s
    # (ECONNREFUSED is fast). With retry loop it stays up.
    server.succeed("systemctl restart rio-gateway rio-worker")

    # Positive: the retry warn! appears. This is the proof
    # the loop is running (not that the process happened to
    # start slow enough to miss the window).
    server.wait_until_succeeds(
        "journalctl -u rio-gateway --since='10 seconds ago' "
        "| grep -q 'upstream connect failed; retrying'",
        timeout=30,
    )
    server.wait_until_succeeds(
        "journalctl -u rio-worker --since='10 seconds ago' "
        "| grep -q 'upstream connect failed; retrying'",
        timeout=30,
    )

    # Negative: process is still running after 10s of refused
    # connects. `systemctl is-active` = `active` means the main
    # PID hasn't exited. (With bare `?`: `failed` or `activating`
    # on a restart loop.)
    import time; time.sleep(10)
    server.succeed("systemctl is-active rio-gateway")
    server.succeed("systemctl is-active rio-worker")

    # Worker-specific: /healthz is 200 (liveness passes during
    # retry), /readyz is 503 (not ready — no heartbeat yet).
    # This is the §3 justification made executable.
    server.succeed("curl -fsS http://127.0.0.1:${workerHealthPort}/healthz")
    server.fail("curl -fsS http://127.0.0.1:${workerHealthPort}/readyz")

    # Bring upstreams back. Retry loop exits on next tick (≤2s).
    server.succeed("systemctl start rio-store rio-scheduler")
    server.wait_until_succeeds(
        "journalctl -u rio-gateway --since='30 seconds ago' "
        "| grep -q 'rio-gateway ready'",
        timeout=60,
    )
    # Worker readiness flips after first heartbeat accepted.
    server.wait_until_succeeds(
        "curl -fsS http://127.0.0.1:${workerHealthPort}/readyz",
        timeout=60,
    )
```

**Fixture:** standalone (`fixtures/standalone.nix`), not k3s. This is a process-ordering test; k3s adds 4min bootstrap + the flannel race as a confound. Wire into `default.nix` as its own `mkTest { subtests = [ "cold-start-ordering" ]; }` — runs in ~2min, parallelizes with everything else.

---

## 6. Which existing VM test would have caught this

**None directly — that's why it shipped.** But two near-misses:

1. **`vm-lifecycle-ctrlrestart-k3s` (`controller-restart` subtest, `lifecycle.nix:644`).** This restarts the *controller* pod mid-build. The controller's retry loop is exercised here — that's why the controller got the fix (comment at `main.rs:176` cites the 2026-03-16 coverage-full run where this subtest failed 2/2). If there were a symmetric `gateway-restart-during-scheduler-down` subtest, it would have caught the gateway. **There isn't one because the gateway has no CRD-watch state to lose on restart, so nobody wrote a restart test for it.** The bug hid in the "why would you ever restart the gateway" blind spot.

2. **`vm-lifecycle-recovery-k3s` (`recovery` subtest, `lifecycle.nix:955`).** Kills the scheduler leader mid-build. The gateway and worker **already have** balanced-channel connections at this point — the balanced channel's probe loop handles leader flip transparently. The retry loop under test here is a different one (`drain_stream` reconnect, not startup connect). **If this test did a full `systemctl restart rio-gateway` after killing the leader**, it would hit the startup-connect race. It doesn't — it only restarts the scheduler.

**The structural gap:** every existing restart test restarts the component *under test* while its *dependencies* stay up. No test restarts a component while its dependencies are *down*. The new `cold-start-ordering` subtest (§5b) fills that hole for gateway+worker; the controller is already covered by the coverage-full k3s-boot race accidentally.

---

## Implementation checklist

- [ ] `rio-gateway/src/main.rs` — wrap `:201-233` in retry loop (§2 diff)
- [ ] `rio-gateway/src/main.rs` — fix stale comment at `:238-243`
- [ ] `rio-worker/src/main.rs` — wrap `:157-173` in retry loop (§3 diff), health server stays at `:147`
- [ ] `rio-proto/src/client/mod.rs` — add `connect_closed_port_fails_fast_then_succeeds` test (§5a)
- [ ] `nix/tests/scenarios/lifecycle.nix` — add `cold-start-ordering` fragment (§5b)
- [ ] `nix/tests/default.nix` — wire `vm-lifecycle-coldstart-standalone` test
- [ ] `nix develop -c cargo nextest run` — green
- [ ] `nix develop -c cargo clippy --all-targets -- --deny warnings` — green
- [ ] `nix-build-remote --no-nom --dev -- -L .#vm-lifecycle-coldstart-standalone` — green

# Remediation 14: Span propagation + observability grab-bag

**Parent findings:** [§2.8](../phase4a.md#28-span-propagation-component-field-lost), [§3 observability table](../phase4a.md#observability)
**Findings:** `obs-spawn-monitored-no-span-propagation`, `obs-verify-required-fields-missing`, `obs-fallback-reads-unregistered`, `obs-watch-reconnects-unregistered`, `obs-recovery-histogram-success-only`, plus one gauge-drift finding not in the §3 table
**Blast radius:** P1 for span propagation (log-aggregation filter key broken on all background tasks); P2 for the rest (silent metric gaps)
**Spec rule touched:** `r[obs.log.required-fields]` — gets its first `r[verify]` annotation
**Effort:** ~2 h implementation, unit-test only (no VM cycle)

---

## Ground truth corrections to §3 table

The §3 observability table says `rio_store_fallback_reads_total` in `store`.
**Wrong on both counts.** The metric is `rio_worker_fuse_fallback_reads_total`
at `rio-worker/src/fuse/ops.rs:377`. `grep -r rio_store_fallback` finds only the
§3 table itself. The fix location is `rio-worker/src/lib.rs:describe_metrics()`,
not `rio-store`.

---

## 1. `spawn_monitored`: propagate the current span

### Bug

`rio-common/src/task.rs:21` — bare `tokio::spawn`:

```rust
// task.rs:17-36 (current)
pub fn spawn_monitored<F>(name: &'static str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {              // ← no span captured
        let result = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
        if let Err(panic) = result {
            let msg = panic_message(&panic);
            tracing::error!(task = name, panic = %msg, "spawned task panicked");
            std::panic::resume_unwind(panic);
        }
    })
}
```

Each binary's `main()` establishes a root span with `component = "scheduler"`
(or `gateway`/`store`/`worker`/`controller`). `observability.md:279` lists
`component` as a spec-required field — it's the primary filter key in Loki /
CloudWatch.

`tokio::spawn` does not carry the current span across the task boundary; the
spawned future runs in `Span::none()`. Every caller of `spawn_monitored` —
the heartbeat loop, the GC sweeper, the lease-renew task, the reconnect retry
tasks, the controller health server (`main.rs:382`) — logs with no `component`
field.

`tracey query rule obs.log.required-fields` currently shows **zero** `r[verify]`
sites. The spec says "MUST include component"; nothing proves it.

### Fix

Capture `Span::current()` before the spawn point and instrument the outer
wrapper (not `fut` — the panic-handler `error!` also needs the span):

```diff
--- a/rio-common/src/task.rs
+++ b/rio-common/src/task.rs
@@ -4,6 +4,7 @@ use std::future::Future;

 use futures_util::FutureExt as _;
 use tokio::task::JoinHandle;
+use tracing::Instrument as _;

 /// Spawn a task that logs any panic at `error!` level before unwinding.
 ///
@@ -14,24 +15,35 @@ use tokio::task::JoinHandle;
 /// `spawn_monitored` catches the panic, logs it with the task name and panic
 /// message, and then resumes unwinding so the `JoinHandle` still reports the
 /// panic to any awaiter.
+///
+/// The current tracing span is propagated across the task boundary. This is
+/// why the root span's `component` field appears in logs emitted from
+/// background tasks — see `r[obs.log.required-fields]`. `tokio::spawn` does
+/// NOT do this on its own; the span must be captured here, before the spawn
+/// point, because `Span::current()` inside the spawned future would return
+/// `Span::none()`.
 pub fn spawn_monitored<F>(name: &'static str, fut: F) -> JoinHandle<()>
 where
     F: Future<Output = ()> + Send + 'static,
 {
-    tokio::spawn(async move {
-        // AssertUnwindSafe: we're logging and re-panicking, not recovering.
-        // Any shared state is the caller's responsibility.
-        let result = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
-        if let Err(panic) = result {
-            let msg = panic_message(&panic);
-            tracing::error!(
-                task = name,
-                panic = %msg,
-                "spawned task panicked"
-            );
-            // Resume unwinding so JoinHandle::is_finished() and .await report it.
-            std::panic::resume_unwind(panic);
+    // Capture BEFORE tokio::spawn — inside the async block, Span::current()
+    // is the executor's ambient span (none), not the caller's.
+    let span = tracing::Span::current();
+    tokio::spawn(
+        async move {
+            // AssertUnwindSafe: we're logging and re-panicking, not recovering.
+            // Any shared state is the caller's responsibility.
+            let result = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
+            if let Err(panic) = result {
+                let msg = panic_message(&panic);
+                tracing::error!(task = name, panic = %msg, "spawned task panicked");
+                // Resume unwinding so JoinHandle::is_finished() and .await report it.
+                std::panic::resume_unwind(panic);
+            }
         }
-    })
+        // Instrument the WRAPPER, not `fut` — the panic error! above also
+        // needs the component field.
+        .instrument(span),
+    )
 }
```

No behavior change for callers already at `Span::none()` (instrumenting with
the none-span is a no-op). No performance cost beyond one span-clone per spawn
(these are long-lived background tasks, not per-request).

### Test: `r[verify obs.log.required-fields]`

The spec-required test. Captures JSON output to a buffer, enters a span with
`component = "test"`, spawns via `spawn_monitored`, asserts the emitted JSON
line carries the span field.

`tracing_subscriber::fmt::TestWriter` writes to test stdout — useless for
programmatic assertion. Use a hand-rolled `MakeWriter` into
`Arc<Mutex<Vec<u8>>>`:

```diff
--- a/rio-common/src/task.rs
+++ b/rio-common/src/task.rs
@@ -49,8 +49,37 @@ fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {

 #[cfg(test)]
 mod tests {
+    use std::sync::{Arc, Mutex};
+
     use super::*;

+    /// `MakeWriter` into a shared buffer. `tracing_subscriber::fmt::TestWriter`
+    /// goes to stdout — no good for asserting on the bytes.
+    #[derive(Clone, Default)]
+    struct BufWriter(Arc<Mutex<Vec<u8>>>);
+
+    impl std::io::Write for BufWriter {
+        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
+            self.0.lock().unwrap().extend_from_slice(buf);
+            Ok(buf.len())
+        }
+        fn flush(&mut self) -> std::io::Result<()> {
+            Ok(())
+        }
+    }
+
+    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
+        type Writer = Self;
+        fn make_writer(&'a self) -> Self::Writer {
+            self.clone()
+        }
+    }
+
+    fn take_lines(buf: &BufWriter) -> Vec<String> {
+        let bytes = std::mem::take(&mut *buf.0.lock().unwrap());
+        String::from_utf8(bytes).unwrap().lines().map(String::from).collect()
+    }
+
     #[tokio::test]
     async fn test_spawn_monitored_normal_completion() -> anyhow::Result<()> {
         let handle = spawn_monitored("test-normal", async {});
@@ -70,4 +99,58 @@ mod tests {
         let err = result.unwrap_err();
         assert!(err.is_panic(), "error should be a panic");
     }
+
+    /// Span fields from the caller's context appear on logs emitted inside
+    /// the spawned task. This is how `component` (set on the root span in
+    /// each binary's main()) ends up on every log line — including those
+    /// from the heartbeat loop, GC sweeper, lease renewer, etc.
+    ///
+    /// Guard local, not `set_global_default`: the test binary shares a
+    /// global slot across all tests in this crate; setting it here would
+    /// either race or panic on the second test to hit it.
+    // r[verify obs.log.required-fields]
+    #[tokio::test]
+    async fn test_spawn_monitored_propagates_span_component() {
+        use tracing_subscriber::{fmt, layer::SubscriberExt as _};
+
+        let buf = BufWriter::default();
+        let subscriber = tracing_subscriber::registry().with(
+            fmt::layer()
+                .json()
+                // flatten span fields into the top-level JSON object —
+                // matches rio-common's production layout (observability.rs)
+                // and makes the assertion a straight key lookup.
+                .flatten_event(true)
+                .with_current_span(true)
+                .with_span_list(true)
+                .with_writer(buf.clone()),
+        );
+        let _guard = tracing::subscriber::set_default(subscriber);
+
+        // Enter the span BEFORE spawn_monitored — this is what each binary's
+        // main() does with its root span.
+        let root = tracing::info_span!("root", component = "test-component");
+        let handle = root.in_scope(|| {
+            spawn_monitored("propagation-test", async {
+                tracing::info!("hello from spawned task");
+            })
+        });
+        handle.await.unwrap();
+
+        let lines = take_lines(&buf);
+        // Exactly one INFO line — the "hello" above. No span-open/close
+        // events (the fmt layer doesn't emit those by default).
+        let line = lines
+            .iter()
+            .find(|l| l.contains("hello from spawned task"))
+            .unwrap_or_else(|| panic!("no log line found; captured: {lines:?}"));
+
+        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
+        // With with_span_list(true), span fields live under "spans":[{...}].
+        // Look there — the exact nesting is a tracing-subscriber detail
+        // we don't want to over-fit to. substring on the raw line would
+        // pass for `"message":"component"` too; walk the JSON.
+        let haystack = parsed.to_string();
+        assert!(haystack.contains("test-component"), "parsed: {parsed}");
+    }
 }
```

**Subscriber scope subtlety.** The test awaits a `JoinHandle`; the spawned task
runs on a worker thread, not the thread holding `_guard`.
`tracing::subscriber::set_default` returns a `DefaultGuard` that is
thread-local — **but** `#[tokio::test]` defaults to the current-thread
runtime, so the spawned task runs on the same OS thread that holds the guard.
If this test is ever moved to `flavor = "multi_thread"`, switch to
`tracing::subscriber::with_default(subscriber, || async { ... }.block_on())`
or `set_global_default` with a `#[serial]` gate.

**Negative control (optional but worth it).** Add a paired test that spawns
**without** `spawn_monitored` — bare `tokio::spawn` — and asserts the
`component` field is absent. Proves the test actually detects the bug it's
guarding against. Otherwise a future refactor that breaks `.instrument(span)`
but coincidentally leaves `component` in the line (e.g., via a different span)
passes silently.

---

## 2. Missing `describe_counter!` registrations

Two counters are incremented without a matching `describe_counter!`. The
metrics-rs exporter registers on first increment — the metric appears in
`/metrics` once something happens, but:

- No `# HELP` line → Prometheus marks it `untyped`; Grafana tooltips empty.
- Until the first increment, the metric is invisible. For rare events
  (FUSE fallback reads when passthrough is working correctly, watch reconnects
  when the scheduler is stable), "absent" is indistinguishable from "zero".
  Absence alerts can't tell the difference either.

### 2a. `rio_worker_fuse_fallback_reads_total`

Incremented at `rio-worker/src/fuse/ops.rs:377`. Not in `describe_metrics()`.

```diff
--- a/rio-worker/src/lib.rs
+++ b/rio-worker/src/lib.rs
@@ -100,6 +100,14 @@ pub fn describe_metrics() {
     describe_counter!(
         "rio_worker_fuse_fetch_bytes_total",
         "Bytes fetched from store via FUSE misses (nar_data.len())"
     );
+    describe_counter!(
+        "rio_worker_fuse_fallback_reads_total",
+        "Userspace read() callbacks served. When passthrough is ON (default), \
+         the kernel handles reads directly and this counter stays near zero — \
+         nonzero means open_backing() failed for some file. When passthrough \
+         is OFF (RIO_FUSE_PASSTHROUGH=false), every read comes through here. \
+         Sustained nonzero rate with passthrough ON = investigate open_backing."
+    );
     describe_gauge!(
         "rio_worker_cpu_fraction",
```

The description encodes the interpretation from `ops.rs:371-376` — near-zero
vs nonzero is the signal.

### 2b. `rio_controller_build_watch_reconnects_total`

Incremented at `rio-controller/src/reconcilers/build.rs:824` (file deleted in P0294). Not in
`describe_metrics()` (which ends at `:371` with `build_watch_spawns_total`).

```diff
--- a/rio-controller/src/main.rs
+++ b/rio-controller/src/main.rs
@@ -366,6 +366,13 @@ fn describe_metrics() {
     describe_counter!(
         "rio_controller_build_watch_spawns_total",
         "drain_stream tasks spawned (initial + reconnect). \
          Should be ~1 per Build lifetime; high rate = reconnect churn (scheduler instability)."
     );
+    describe_counter!(
+        "rio_controller_build_watch_reconnects_total",
+        "BuildEvent stream reconnect attempts after stream drop. Distinct from \
+         spawns_total: spawns counts initial watch + every reconnect-that-reached-\
+         WatchBuild; reconnects_total counts every attempt including those that \
+         fail before reaching the RPC. High rate = scheduler instability or \
+         network partition; alert on rate > 0 sustained over 5m."
+    );
 }
```

Insert after `spawns_total` (line 370) — keeps the two watch-related counters
adjacent.

---

## 3. `rio_scheduler_recovery_duration_seconds`: failed recoveries invisible

`rio-scheduler/src/actor/recovery.rs` — histogram recorded only on the success
path:

```rust
// recovery.rs:321-331 (success path, inside recover_from_pg)
let elapsed = start.elapsed();
info!(/* ... */ "state recovery complete");
metrics::histogram!("rio_scheduler_recovery_duration_seconds")
    .record(elapsed.as_secs_f64());
metrics::counter!("rio_scheduler_recovery_total", "outcome" => "success").increment(1);
Ok(())

// recovery.rs:369-380 (error path, inside handle_leader_acquired)
Err(e) => {
    error!(error = %e, "state recovery FAILED — continuing with empty DAG ...");
    metrics::counter!("rio_scheduler_recovery_total", "outcome" => "failure")
        .increment(1);
    // ← histogram NOT recorded
```

A failed recovery — typically PG timeout mid-load — is exactly the case where
the duration is most interesting (how long did we wait before giving up?). The
histogram has no `outcome` label, so even if it were recorded on both paths,
fast successes would wash out slow failures.

### Fix: move timing to the caller, add `outcome` label

The task brief suggested scopeguard. That would work but (a) `scopeguard` isn't
in rio-scheduler's deps, and (b) it leaves the label problem unsolved. Hoisting
to `handle_leader_acquired` solves both — the match already knows the outcome:

```diff
--- a/rio-scheduler/src/actor/recovery.rs
+++ b/rio-scheduler/src/actor/recovery.rs
@@ -54,7 +54,6 @@ impl DagActor {
     /// ...
     /// (critical-path sort, ready-queue seed, build-counts).
     pub(super) async fn recover_from_pg(&mut self) -> Result<(), ActorError> {
-        let start = Instant::now();
         info!("starting state recovery from PG");

         // --- Clear in-mem state ---
@@ -318,17 +317,12 @@ impl DagActor {
             self.check_build_completion(build_id).await;
         }

-        let elapsed = start.elapsed();
         info!(
             builds = self.builds.len(),
             derivations = self.dag.iter_nodes().count(),
             ready_queue = self.ready_queue.len(),
-            elapsed_ms = elapsed.as_millis(),
             "state recovery complete"
         );
-        metrics::histogram!("rio_scheduler_recovery_duration_seconds")
-            .record(elapsed.as_secs_f64());
-        metrics::counter!("rio_scheduler_recovery_total", "outcome" => "success").increment(1);

         Ok(())
     }
@@ -343,7 +337,22 @@ impl DagActor {
     /// inconsistent). MergeDag from a standby-period SubmitBuild
     /// would queue in the mpsc channel and get processed after.
     pub(super) async fn handle_leader_acquired(&mut self) {
-        match self.recover_from_pg().await {
+        let start = Instant::now();
+        let result = self.recover_from_pg().await;
+
+        // Record BEFORE the match — both arms need it, and the error
+        // arm's partial-state clear (.dag = new(), etc.) doesn't touch
+        // `start`. Label by outcome: a 30s failure (PG timeout) and a
+        // 30s success (huge DAG) are very different signals; without the
+        // label, one washes out the other.
+        let outcome = if result.is_ok() { "success" } else { "failure" };
+        let elapsed = start.elapsed();
+        metrics::histogram!("rio_scheduler_recovery_duration_seconds", "outcome" => outcome)
+            .record(elapsed.as_secs_f64());
+        metrics::counter!("rio_scheduler_recovery_total", "outcome" => outcome).increment(1);
+        info!(elapsed_ms = elapsed.as_millis(), outcome, "recovery timing");
+
+        match result {
             Ok(()) => {
                 // Stale-pin cleanup: crash-between-pin-and-unpin
@@ -376,9 +385,7 @@ impl DagActor {
                     error = %e,
                     "state recovery FAILED — continuing with empty DAG \
                      (Phase 3a behavior: in-flight builds lost)"
                 );
-                metrics::counter!("rio_scheduler_recovery_total", "outcome" => "failure")
-                    .increment(1);
                 // Explicitly re-clear: recovery may have partially
                 // populated before failing.
```

The `elapsed_ms` field moves from the success-only `info!` to a separate
timing line emitted on both paths. The success `info!` still logs build/drv
counts (that's what it's for — the sizes, not the timing).

**Dashboard note.** Adding the `outcome` label breaks existing PromQL that
queries `rio_scheduler_recovery_duration_seconds` without a label selector —
the pre-fix series had no labels, post-fix has two (`outcome=success`,
`outcome=failure`). Grep Helm chart templates / Grafana dashboards for the
metric name; unlabeled queries need `sum without (outcome) (...)` or an
explicit `{outcome="success"}`.

---

## 4. `rio_scheduler_workers_active`: drifts on actor shutdown

Not in the §3 table — found while auditing the other gauge sites.

`rio-scheduler/src/actor/mod.rs:320`:

```rust
// mod.rs:311-322 (shutdown arm)
_ = self.shutdown.cancelled() => {
    info!(workers = self.workers.len(), "actor shutting down, dropping worker streams");
    // Drop all stream_tx → ... → serve_with_shutdown unblocks.
    self.workers.clear();   // ← gauge not zeroed
    break;
}
```

The gauge is maintained via inc/dec at `worker.rs:52` (connect), `:76`
(disconnect), `:384` (heartbeat-completes-registration). `workers.clear()`
empties the map without going through `handle_worker_disconnected`, so the
gauge stays at its last value for the window between this `clear()` and
process exit.

**Blast radius: low.** After `break` the actor loop exits, and in practice the
process follows shortly — the gauge is stale for milliseconds before
`/metrics` goes away. Not an alert-worthy bug on its own.

**But it's an inconsistency.** `handle_tick` at `worker.rs:610-629` already
does ground-truth `.set()` for every *other* gauge:

```rust
// worker.rs:610-629 (handle_tick — existing)
// Update metrics. All gauges are set from ground-truth state on each
// Tick — this is self-healing against any counting bugs elsewhere.
metrics::gauge!("rio_scheduler_derivations_queued").set(self.ready_queue.len() as f64);
metrics::gauge!("rio_scheduler_builds_active").set(/* filter+count */);
metrics::gauge!("rio_scheduler_derivations_running").set(/* filter+count */);
```

`workers_active` is the only gauge NOT in this block. The comment promises
self-healing against counting bugs — `workers_active` is the one gauge that
can actually drift (three inc/dec sites with a `was_registered` guard on each;
any guard logic bug → permanent skew).

### Fix: add to the ground-truth block

```diff
--- a/rio-scheduler/src/actor/worker.rs
+++ b/rio-scheduler/src/actor/worker.rs
@@ -608,10 +608,18 @@ impl DagActor {
         }

         // Update metrics. All gauges are set from ground-truth state on each
-        // Tick — this is self-healing against any counting bugs elsewhere.
+        // Tick — this is self-healing against any counting bugs elsewhere.
+        // The inc/dec calls at connect/disconnect/heartbeat (worker.rs:52/
+        // :76/:384) stay — they give sub-tick responsiveness. This block
+        // corrects any drift every tick.
         metrics::gauge!("rio_scheduler_derivations_queued").set(self.ready_queue.len() as f64);
+        metrics::gauge!("rio_scheduler_workers_active").set(
+            self.workers
+                .values()
+                .filter(|w| w.is_registered())
+                .count() as f64,
+        );
         metrics::gauge!("rio_scheduler_builds_active").set(
```

Keep the inc/dec sites — they give correct readings *between* ticks (10s
apart). The `.set()` in `handle_tick` corrects any accumulated drift. Same
pattern as the other three gauges.

This also covers any *future* code path that mutates `self.workers` without
going through `handle_worker_disconnected` — e.g., Remediation 01's
recovery-reap or a future leader-demotion handler.

The task brief's alternative — `gauge.set(0.0)` before `workers.clear()` at
`mod.rs:320` — fixes the one known drift site but not the pattern. Skip it.

---

## 5. Per-component "metric exists at zero" unit tests

**Problem this catches.** metrics-rs registers on first `increment()`/`record()`,
not at `describe_*!()`. A metric that is described but never touched appears
in `/metrics` as a `# HELP`/`# TYPE` pair with no sample line — which is fine.
A metric that is *neither* described *nor* touched (the bug in §2a/§2b above)
doesn't appear at all. A scrape-and-assert test after `describe_metrics()`
catches the second class at unit-test time.

**Approach.** `PrometheusBuilder::build_recorder()` returns a recorder +
`PrometheusHandle` without installing globally or starting an HTTP listener.
Wrap `describe_metrics()` in `metrics::with_local_recorder`, then
`handle.render()` and grep for each spec'd name.

New test file per component, under `tests/metrics_registered.rs` (integration
test — `describe_metrics()` is `pub`). Each component has its own spec'd
metric set (`observability.md` tables), so each gets its own assertion list.

```rust
// rio-worker/tests/metrics_registered.rs (new)
//! After `describe_metrics()`, every spec'd metric name appears in the
//! Prometheus scrape — as a `# HELP` line at minimum. Catches the
//! "incremented but never described" class (§3 obs-*-unregistered).
//!
//! This does NOT catch "described but never incremented in any code path"
//! — that's dead-metric detection, a grep job not a unit test.

use metrics_exporter_prometheus::PrometheusBuilder;

/// Metric names from observability.md's Worker Metrics table.
/// Keep in sync; the tracey rule `r[obs.metric.worker]` on
/// `describe_metrics()` is the spec link, this is the enforcement.
const WORKER_METRICS: &[&str] = &[
    "rio_worker_builds_total",
    "rio_worker_builds_active",
    "rio_worker_uploads_total",
    "rio_worker_build_duration_seconds",
    "rio_worker_fuse_cache_size_bytes",
    "rio_worker_fuse_cache_hits_total",
    "rio_worker_fuse_cache_misses_total",
    "rio_worker_fuse_fetch_duration_seconds",
    "rio_worker_fuse_fallback_reads_total",   // ← the one §2a adds
    "rio_worker_overlay_teardown_failures_total",
    "rio_worker_prefetch_total",
    "rio_worker_upload_bytes_total",
    "rio_worker_fuse_fetch_bytes_total",
    "rio_worker_cpu_fraction",
    "rio_worker_memory_fraction",
];

// r[verify obs.metric.worker]
#[test]
fn all_spec_metrics_appear_in_scrape() {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();

    // with_local_recorder: describe_*! macros route to `recorder` for
    // the duration of the closure only. No global install → no
    // cross-test contamination, no #[serial] needed.
    metrics::with_local_recorder(&recorder, || {
        rio_worker::describe_metrics();
    });

    let scrape = handle.render();
    let missing: Vec<_> = WORKER_METRICS
        .iter()
        .filter(|name| !scrape.contains(&format!("# HELP {name} ")))
        .collect();

    assert!(
        missing.is_empty(),
        "spec'd metrics missing from scrape after describe_metrics(): {missing:?}\n\
         \n\
         scrape:\n{scrape}"
    );
}
```

**Why `# HELP {name} ` and not just `{name}`.** A histogram named
`rio_worker_build_duration_seconds` generates
`rio_worker_build_duration_seconds_bucket`, `_sum`, `_count` sample lines —
substring-matching the bare name would pass even if the HELP line were
missing. The trailing space rules out prefix collisions (`foo` matching
`foo_bar`).

**Dependency note.** `metrics-exporter-prometheus` is currently only in
`rio-common/Cargo.toml` (line 15). Each component crate needs it in
`[dev-dependencies]` for this test — workspace dep already exists, just add
the reference:

```toml
# rio-worker/Cargo.toml, rio-scheduler/Cargo.toml, etc.
[dev-dependencies]
metrics-exporter-prometheus = { workspace = true }
```

**Replicate across components.** Same shape for `rio-scheduler`, `rio-store`,
`rio-gateway` (their `describe_metrics()` are all `pub fn` in `lib.rs`). The
controller's `describe_metrics()` is a private `fn` in `main.rs:341` — either
hoist it to `lib.rs` (controller doesn't have a `lib.rs` currently, but
trivial to add) or make it `pub(crate)` and test from inside the crate. Prefer
the hoist: consistency with the other four, and then `r[impl obs.metric.controller]`
can live next to the others.

---

## Checklist

- [ ] `task.rs`: `Span::current()` + `.instrument()` on the wrapper, doc comment re: why
- [ ] `task.rs` test: `BufWriter` `MakeWriter`, span with `component`, assert JSON carries it — `r[verify obs.log.required-fields]`
- [ ] (optional) negative-control test: bare `tokio::spawn` → component absent
- [ ] `rio-worker/src/lib.rs`: `describe_counter!("rio_worker_fuse_fallback_reads_total", ...)`
- [ ] `rio-controller/src/main.rs`: `describe_counter!("rio_controller_build_watch_reconnects_total", ...)`
- [ ] `recovery.rs`: move `start` + histogram + counter to `handle_leader_acquired`, add `outcome` label
- [ ] grep Helm / Grafana for unlabeled `rio_scheduler_recovery_duration_seconds` queries
- [ ] `worker.rs:handle_tick`: ground-truth `.set()` for `workers_active`
- [ ] `rio-*/tests/metrics_registered.rs` × 5 components, `r[verify obs.metric.*]` on each
- [ ] `[dev-dependencies] metrics-exporter-prometheus` in 4 component Cargo.toml (rio-common has it already)
- [ ] hoist controller `describe_metrics()` to a `lib.rs` (or `pub(crate)` + in-crate test)
- [ ] update §3 `obs-fallback-reads-unregistered` row: metric name + crate are both wrong
- [ ] `tracey query rule obs.log.required-fields` → confirm `verify` site appears
- [ ] `nix develop -c cargo nextest run -p rio-common -p rio-worker -p rio-scheduler`

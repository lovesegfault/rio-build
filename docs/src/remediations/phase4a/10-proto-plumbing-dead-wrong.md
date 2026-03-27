# Remediation 10: Proto plumbing dead/wrong

**Parent finding:** [§2.4](../phase4a.md#24-proto-plumbing-deadwrong)
**Findings:** `proto-heartbeat-resources-zero-producer`, `proto-heartbeat-response-generation-unread`, `proto-build-options-subfields-ignored`
**Blast radius:** P1 — autoscaler blind, split-brain fence one-sided, per-tenant limits silently ignored
**Effort:** ~3 h implementation + one VM cycle to verify

---

## Ground truth

§2.4 says `rio-worker/src/heartbeat.rs`. There is no such file. The producer is
`rio-worker/src/runtime.rs:94`:

```rust
resources: Some(ResourceUsage::default()),
```

The scheduler side (`state/worker.rs:79-82`, `admin/mod.rs:459-468`) is correctly
plumbed — `ListWorkers` serves whatever the worker sends. The phase4-plan-research
memory was wrong on the layer; §2.4's correction is right.

---

## 1. `ResourceUsage` producer — cgroup v2, not `/proc`

### Decision: cgroup v2

§2.4 suggests `/proc/stat` + `/proc/meminfo`. **Use cgroup v2 instead** — the
machinery already exists and `/proc` is wrong here:

- `/proc/stat` is **host-wide**. On a shared node, it reports other pods' CPU.
- `/proc/meminfo MemTotal` is the host's RAM, not the pod's `memory.max` limit.
  The scheduler wants "how close is this worker to OOM-kill" — that's
  `memory.current / memory.max`, not `MemAvailable / MemTotal`.
- `utilization_reporter_loop` (`cgroup.rs:572-618`) **already samples** the
  delegated-root cgroup's `cpu.stat` + `memory.current` + `memory.max` every 15s
  for Prometheus gauges. Reusing it means one sampling site, not two racing
  delta-CPU readers.

### Refactor: extract a shared sampler

`utilization_reporter_loop` currently computes fractions and sets gauges inline.
Split it: a `CgroupSampler` struct that holds the delta-CPU state (`last_usage_usec`,
`last_instant`) and a `sample()` method; the Prometheus loop and the heartbeat
loop both read the same `Arc<RwLock<ResourceSnapshot>>` that the sampler writes.

Heartbeat interval (`main.rs:289`, `HEARTBEAT_INTERVAL`) is 10s; Prometheus poll
is 15s. Run the sampler at 10s (fast enough for both), have both consumers read
the shared snapshot. No double-read of `cpu.stat`, no independent delta windows.

```diff
--- a/rio-worker/src/cgroup.rs
+++ b/rio-worker/src/cgroup.rs
@@ -547,6 +547,61 @@ fn mem_fraction(current: u64, max: Option<u64>) -> f64 {
     }
 }

+/// Snapshot of the worker's whole-tree resource usage, published by
+/// [`utilization_reporter_loop`] and read by the heartbeat loop.
+///
+/// `cpu_fraction` is cores-equivalent (1.0 = one core fully busy; >1.0
+/// on multi-core). `memory_total_bytes` is the cgroup `memory.max`
+/// limit — `None` means unbounded ("max"), in which case the proto
+/// `memory_total_bytes` is sent as 0 (the scheduler treats 0 as
+/// "unknown ceiling"; it won't compute a fraction from 0).
+///
+/// Disk fields: statvfs on `overlay_base_dir`. This is where per-build
+/// overlay upper dirs accumulate — the relevant quota for "can this
+/// worker accept another build." Not the FUSE cache dir (that's LRU-
+/// bounded separately; see `r[builder.fuse.cache-lru]`).
+#[derive(Debug, Clone, Copy, Default)]
+pub struct ResourceSnapshot {
+    pub cpu_fraction: f64,
+    pub memory_current_bytes: u64,
+    pub memory_max_bytes: Option<u64>,
+    pub disk_used_bytes: u64,
+    pub disk_total_bytes: u64,
+}
+
+impl ResourceSnapshot {
+    pub fn to_proto(&self) -> rio_proto::types::ResourceUsage {
+        rio_proto::types::ResourceUsage {
+            cpu_fraction: self.cpu_fraction,
+            memory_used_bytes: self.memory_current_bytes,
+            memory_total_bytes: self.memory_max_bytes.unwrap_or(0),
+            disk_used_bytes: self.disk_used_bytes,
+            disk_total_bytes: self.disk_total_bytes,
+        }
+    }
+}
+
+/// Shared handle: heartbeat loop reads, sampler writes. `parking_lot::RwLock`
+/// (already a dep via `metrics`) — no async needed, reads are non-blocking
+/// and the critical section is a `Copy` struct store.
+pub type ResourceSnapshotHandle = std::sync::Arc<parking_lot::RwLock<ResourceSnapshot>>;
+
+fn sample_disk(overlay_base: &Path) -> (u64, u64) {
+    // statvfs on the overlay base dir. `f_blocks * f_frsize` = total;
+    // `(f_blocks - f_bavail) * f_frsize` = used. `f_bavail` (not
+    // `f_bfree`) because bfree includes root-reserved blocks — the
+    // worker runs unprivileged, so bavail is the real ceiling.
+    //
+    // ENOENT (overlay_base_dir not yet created on a fresh worker) →
+    // (0, 0). The scheduler reads both-zero as "unknown" and doesn't
+    // penalize placement.
+    match nix::sys::statvfs::statvfs(overlay_base) {
+        Ok(s) => {
+            let frsize = s.fragment_size();
+            let total = s.blocks().saturating_mul(frsize);
+            let used = s.blocks().saturating_sub(s.blocks_available()).saturating_mul(frsize);
+            (used, total)
+        }
+        Err(_) => (0, 0),
+    }
+}
+
 /// Background task that polls the worker's parent cgroup for CPU and
 /// memory utilization, emitting `rio_worker_cpu_fraction` and
 /// `rio_worker_memory_fraction` gauges every 15s.
@@ -569,8 +624,12 @@ fn mem_fraction(current: u64, max: Option<u64>) -> f64 {
 /// cgroup was removed out from under us), the gauge simply stops
 /// updating; no crash.
 // r[impl obs.metric.worker-util]
-pub async fn utilization_reporter_loop(root: PathBuf) {
-    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
+pub async fn utilization_reporter_loop(
+    root: PathBuf,
+    overlay_base: PathBuf,
+    snapshot: ResourceSnapshotHandle,
+) {
+    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
     let cpu_stat_path = root.join("cpu.stat");
     let mem_current_path = root.join("memory.current");
     let mem_max_path = root.join("memory.max");
@@ -598,11 +657,21 @@ pub async fn utilization_reporter_loop(root: PathBuf) {
             }
         });

+        let (disk_used, disk_total) = sample_disk(&overlay_base);
+
+        let cpu_frac;
         // CPU: only set if we have BOTH a previous sample and a new reading.
         if let (Some(last_usage), Some(now_usage)) = (last_usage_usec, now_usage) {
             let cpu_delta = now_usage.saturating_sub(last_usage);
             let wall_delta = now_instant.duration_since(last_instant).as_micros() as u64;
-            metrics::gauge!("rio_worker_cpu_fraction").set(cpu_fraction(cpu_delta, wall_delta));
+            cpu_frac = cpu_fraction(cpu_delta, wall_delta);
+            metrics::gauge!("rio_worker_cpu_fraction").set(cpu_frac);
+        } else {
+            cpu_frac = 0.0;
         }
         if let Some(nu) = now_usage {
             last_usage_usec = Some(nu);
@@ -614,6 +683,15 @@ pub async fn utilization_reporter_loop(root: PathBuf) {
         if let Some(current) = mem_current {
             metrics::gauge!("rio_worker_memory_fraction").set(mem_fraction(current, mem_max));
         }
+
+        // Publish snapshot. Heartbeat reads this; it's always one poll
+        // behind reality (up to 10s stale), which is fine for placement.
+        *snapshot.write() = ResourceSnapshot {
+            cpu_fraction: cpu_frac,
+            memory_current_bytes: mem_current.unwrap_or(0),
+            memory_max_bytes: mem_max,
+            disk_used_bytes: disk_used,
+            disk_total_bytes: disk_total,
+        };
     }
 }
```

```diff
--- a/rio-worker/src/main.rs
+++ b/rio-worker/src/main.rs
@@ -133,9 +133,15 @@
     // Background utilization reporter: polls parent cgroup cpu.stat +
-    // memory.current/max every 15s → rio_worker_{cpu,memory}_fraction gauges.
-    // Fire-and-forget — runs for the worker's lifetime.
+    // memory.current/max every 10s → Prometheus gauges AND the shared
+    // snapshot the heartbeat loop reads for ResourceUsage.
+    let resource_snapshot: rio_worker::cgroup::ResourceSnapshotHandle = Default::default();
     rio_common::task::spawn_monitored(
         "cgroup-utilization-reporter",
-        rio_worker::cgroup::utilization_reporter_loop(cgroup_parent.clone()),
+        rio_worker::cgroup::utilization_reporter_loop(
+            cgroup_parent.clone(),
+            cfg.overlay_base_dir.clone(),
+            std::sync::Arc::clone(&resource_snapshot),
+        ),
     );
```

```diff
--- a/rio-worker/src/runtime.rs
+++ b/rio-worker/src/runtime.rs
@@ -61,6 +61,7 @@ pub async fn build_heartbeat_request(
     size_class: &str,
     running: &RwLock<HashSet<String>>,
     bloom: Option<&BloomHandle>,
+    resources: &crate::cgroup::ResourceSnapshotHandle,
 ) -> HeartbeatRequest {
     let current: Vec<String> = running.read().await.iter().cloned().collect();

@@ -91,7 +92,9 @@ pub async fn build_heartbeat_request(
     HeartbeatRequest {
         worker_id: worker_id.to_string(),
         running_builds: current,
-        resources: Some(ResourceUsage::default()),
+        // Snapshot is Copy; the read lock is held for one pointer-sized
+        // load. First heartbeat (before first 10s poll) sends zeros —
+        // same as today, converges after one poll interval.
+        resources: Some(resources.read().to_proto()),
         local_paths,
```

Pass `resource_snapshot` to `build_heartbeat_request` at `main.rs:293-302`
(one more arg + one more `Arc::clone` before `spawn_monitored`).

**Why not a dedicated sampler struct?** `utilization_reporter_loop` is already
the single authoritative sampling site. Adding a second `CgroupSampler` that
independently computes delta-CPU would mean two tasks reading `cpu.stat` at
different cadences, producing slightly different `cpu_fraction` values in
Prometheus vs ListWorkers. The shared-snapshot approach keeps them identical.

---

## 2. Generation fence — the spec already decided

### This is not a "pick one" — the spec is normative

`docs/src/components/scheduler.md:499` `r[sched.lease.generation-fence]`:

> Workers see the new generation in `HeartbeatResponse` and reject any
> `WorkAssignment` carrying an older generation.

`rio-scheduler/src/lease/mod.rs:32-35` (module doc) repeats this, and
`scheduler.md:497` relies on it as the split-brain bound:

> workers reject stale-generation assignments after seeing the new generation
> in `HeartbeatResponse`. Worst case: a derivation is dispatched twice

The "informational only" option would invalidate `r[sched.lease.generation-fence]`
and leave the split-brain window unbounded on the worker side (scheduler-side
`is_leader` guard is still there, but that's polled — a deposed leader
dispatches for one more poll tick). **Implement the fence.**

### Implementation

Store the latest generation in an `Arc<AtomicU64>` shared between the heartbeat
loop and the assignment handler. Reject strictly-less (`<`, not `<=`) —
generation is constant during a leader's tenure, so `assignment.generation ==
stored` is the steady state.

```diff
--- a/rio-worker/src/main.rs
+++ b/rio-worker/src/main.rs
@@ -285,6 +285,13 @@
     let heartbeat_running = Arc::clone(&running_builds);
     let heartbeat_ready = Arc::clone(&ready);
+    // Latest generation observed in an accepted HeartbeatResponse. Starts
+    // at 0 — the scheduler's generation starts at 1 (lease/mod.rs:162 for
+    // non-K8s, lease.rs:293 increments from 1 on first acquire), so 0
+    // never rejects a real assignment. Relaxed ordering: this is a fence
+    // against a DIFFERENT process's stale writes, not a within-process
+    // happens-before. The value itself is the signal.
+    let latest_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
+    let heartbeat_gen = Arc::clone(&latest_generation);
     let mut heartbeat_client = scheduler_client.clone();
     let heartbeat_handle = rio_common::task::spawn_monitored("heartbeat-loop", async move {
@@ -304,11 +311,23 @@
             match heartbeat_client.heartbeat(request).await {
                 Ok(response) => {
                     let resp = response.into_inner();
                     if resp.accepted {
                         // READY. Set unconditionally — it's idempotent
                         // (already-true → true is a no-op at the atomic
                         // level) and cheaper than a load-then-store.
                         heartbeat_ready.store(true, std::sync::atomic::Ordering::Relaxed);
+                        // r[impl sched.lease.generation-fence]
+                        // fetch_max, not store: if the heartbeat raced a
+                        // leader transition and we got a response from the
+                        // OLD leader (accepted=true but stale generation)
+                        // after already seeing the new leader's gen via a
+                        // previous heartbeat, `store` would REGRESS the
+                        // fence. fetch_max is monotone. Stale-leader
+                        // heartbeats with accepted=true are possible during
+                        // the 15s Lease TTL window — r[sched.lease.k8s-lease]
+                        // split-brain paragraph.
+                        heartbeat_gen.fetch_max(
+                            resp.generation,
+                            std::sync::atomic::Ordering::Relaxed,
+                        );
                     } else {
```

```diff
--- a/rio-worker/src/main.rs
+++ b/rio-worker/src/main.rs
@@ -456,6 +456,27 @@
                         Some(scheduler_message::Msg::Assignment(assignment)) => {
-                            info!(drv_path = %assignment.drv_path, "received work assignment");
+                            // r[impl sched.lease.generation-fence]
+                            // Generation fence: reject assignments from a
+                            // deposed leader. `<` not `<=` — equal is the
+                            // steady state (generation is constant per
+                            // leader tenure). The deposed leader's
+                            // BuildExecution stream stays open until its
+                            // process exits; this is the ONLY worker-side
+                            // defense against split-brain double-dispatch.
+                            let latest = latest_generation.load(std::sync::atomic::Ordering::Relaxed);
+                            if assignment.generation < latest {
+                                info!(
+                                    drv_path = %assignment.drv_path,
+                                    assignment_gen = assignment.generation,
+                                    latest_gen = latest,
+                                    "rejecting stale-generation assignment (deposed leader)"
+                                );
+                                metrics::counter!("rio_worker_stale_assignments_rejected_total")
+                                    .increment(1);
+                                // No ACK. The deposed leader is about to
+                                // die; its actor state is going away. Not
+                                // ACKing means the derivation stays
+                                // Assigned in the deposed actor — harmless.
+                                // The NEW leader re-dispatches from PG.
+                                continue;
+                            }
+                            info!(drv_path = %assignment.drv_path, gen = assignment.generation, "received work assignment");
```

### Why `fetch_max` and not `store`

Heartbeat is on a `tonic::Channel` to the scheduler's ClusterIP (or balanced
channel — check `main.rs` connect logic). During the 15s Lease TTL split-brain
window (`scheduler.md:497`), both leaders answer heartbeats with
`accepted=true`. If two heartbeat responses interleave as new-leader-then-old-leader,
`store(old_gen)` would regress the fence and let through exactly the assignment
we're trying to block. `fetch_max` makes the fence monotone regardless of
response ordering.

### Edge case: `latest_generation == 0` on first assignment

Worker cold-start sequence: connect BuildExecution stream → register → heartbeat
loop starts (independent task). An assignment can arrive before the first
heartbeat response. At that point `latest_generation == 0`, `assignment.generation >= 1`,
so `0 < 1` is false — **not rejected**. Correct: we have no evidence of
staleness yet.

---

## 3. `BuildOptions` subfields — `wopSetOptions`, not daemon CLI args

### Decision on `keep_going`: reserve+delete, it's semantically wrong here

`keep_going` is **already implemented scheduler-side** — `r[impl sched.build.keep-going]`
at `rio-scheduler/src/actor/build.rs:3`, with the DAG-level semantics at
`build.rs:302-320` and `completion.rs:704`. It's a per-**build** flag (continue
dispatching sibling DAG nodes after one fails), not per-derivation.

`actor/build.rs:567` already acknowledges this:

```rust
keep_going: false, // per-derivation, not per-build
```

The scheduler always sends `false`. The worker never reads it. A per-derivation
`keep_going` doesn't make sense for `wopBuildDerivation` (we pass one drv to
the daemon; there's nothing to "keep going" to). §2.4's description
("multi-output build should continue building other outputs after one fails")
is confusing Nix's `wopBuildPaths --keep-going` semantics with our one-drv-at-a-time
model. **Reserve the field.**

### `max_silent_time` + `build_cores`: plumb through `client_set_options`

§2.4 says pass `--option max-silent-time` as a daemon CLI arg. **Don't** — the
daemon is already spawned by the time we know the per-assignment value, and
`spawn_daemon_in_namespace` is correctly assignment-agnostic.

The right place is `rio-nix/src/protocol/client.rs:275` `client_set_options`,
which sends `wopSetOptions` after the handshake. Currently hardcodes
`maxSilentTime = 0` (line 290) and `buildCores = 0` (line 299). Add parameters:

```diff
--- a/rio-nix/src/protocol/client.rs
+++ b/rio-nix/src/protocol/client.rs
@@ -271,10 +271,18 @@
 /// Send `wopSetOptions` to the local daemon.
 ///
-/// Sends minimal default options. The local daemon needs this before
-/// it will accept build requests.
+/// `max_silent_time`: seconds without build output before the daemon
+/// kills the builder. 0 = unbounded (daemon default). Maps to Nix's
+/// `--max-silent-time`.
+///
+/// `build_cores`: value of `NIX_BUILD_CORES` in the builder's env.
+/// 0 = "use all cores" (daemon substitutes `nproc`). Maps to Nix's
+/// `--cores`. The daemon sets this env var; we don't set it directly
+/// because the builder runs in the daemon's sandbox namespace, not ours.
 pub async fn client_set_options<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
     reader: &mut R,
     writer: &mut W,
+    max_silent_time: u64,
+    build_cores: u64,
 ) -> Result<(), WireError> {
     wire::write_u64(writer, WorkerOp::SetOptions as u64).await?;

@@ -287,7 +295,7 @@
     // maxBuildJobs
     wire::write_u64(writer, 1).await?;
     // maxSilentTime
-    wire::write_u64(writer, 0).await?;
+    wire::write_u64(writer, max_silent_time).await?;
     // obsolete_useBuildHook (always 1)
     wire::write_u64(writer, 1).await?;
@@ -297,7 +305,7 @@
     wire::write_u64(writer, 0).await?;
     wire::write_u64(writer, 0).await?;
     // buildCores
-    wire::write_u64(writer, 0).await?;
+    wire::write_u64(writer, build_cores).await?;
     // useSubstitutes
```

Plumb through `stderr_loop.rs:67` → `run_daemon_build` signature → call site
in `executor/mod.rs`. Extract next to the existing `build_timeout` extraction:

```diff
--- a/rio-worker/src/executor/mod.rs
+++ b/rio-worker/src/executor/mod.rs
@@ -425,11 +425,23 @@
-    let timeout = assignment
-        .build_options
-        .as_ref()
-        .and_then(|opts| {
-            if opts.build_timeout > 0 {
-                Some(Duration::from_secs(opts.build_timeout))
-            } else {
-                None
-            }
-        })
-        .unwrap_or(env.daemon_timeout);
+    // Extract BuildOptions. The scheduler computes these per-derivation
+    // from the intersecting builds' options (actor/build.rs:555-568):
+    // min-nonzero for timeouts, max for cores. `None` (no BuildOptions
+    // on the assignment) → daemon defaults: unbounded silence, nproc cores.
+    let opts = assignment.build_options.as_ref();
+    let timeout = opts
+        .and_then(|o| (o.build_timeout > 0).then(|| Duration::from_secs(o.build_timeout)))
+        .unwrap_or(env.daemon_timeout);
+    // 0 → 0 on the wire = "unbounded" / "all cores" to the daemon. The
+    // scheduler's min_nonzero / max already handle the 0-means-unset
+    // semantics; we pass through verbatim.
+    let max_silent_time = opts.map(|o| o.max_silent_time).unwrap_or(0);
+    let build_cores = opts.map(|o| o.build_cores).unwrap_or(0);
```

Pass `max_silent_time` and `build_cores` to `run_daemon_build` (new params) →
`client_set_options` call at `stderr_loop.rs:67`.

### Why not `NIX_BUILD_CORES` env on the daemon process?

The daemon doesn't forward its own env to the builder — it constructs the
builder's env from `wopSetOptions.buildCores` + the derivation's
`impureEnvVars`. Setting `NIX_BUILD_CORES` on the daemon process would be
ignored. `wopSetOptions` is the correct channel.

---

## 4. Cleanup — `ProgressUpdate`, `Substituted`, `CachedFailure`, `keep_going`

### 4a. `WorkerMessage.progress` (field 4) — reserve

Worker never sends it (grep: zero `Msg::Progress` constructions in `rio-worker/`).
Scheduler explicitly drops it (`grpc/mod.rs:750-764`) with a `TODO(phase5)` for
live preemption/migration. `grpc/tests.rs:1066,1211` send `ProgressUpdate::default()`
just to assert no-op — pure dead-message coverage.

**Keep the `ProgressUpdate` message definition** (phase5 will re-add the field);
reserve only the oneof variant:

```diff
--- a/rio-proto/proto/types.proto
+++ b/rio-proto/proto/types.proto
@@ -142,12 +142,17 @@
 message WorkerMessage {
   oneof msg {
     WorkAssignmentAck ack = 1;              // Worker confirms receipt of assignment
     BuildLogBatch log_batch = 2;            // Batched log lines (not per-line)
     CompletionReport completion = 3;        // Build result
-    ProgressUpdate progress = 4;            // Resource usage, build phase
     WorkerRegister register = 5;            // First message: identifies the worker
   }
+  // Field 4 (ProgressUpdate progress) — never sent worker-side; scheduler
+  // dropped it. TODO(phase5): re-add for live preemption/migration
+  // (mid-build ResourceUsage to detect "about to OOM, migrate"). Needs
+  // worker-side checkpoint support first. The ProgressUpdate message
+  // below is kept for that.
+  reserved 4;
 }
```

Delete the scheduler's `Msg::Progress(_)` arm (`grpc/mod.rs:750-764`) and the
two no-op tests (`grpc/tests.rs:1066,1187-1211`). `prost` won't generate the
variant; the match becomes exhaustive without it.

### 4b. `BuildOptions.keep_going` (field 4) — reserve

Per §3: semantically wrong at this layer, scheduler always sends `false`, worker
never reads it.

```diff
--- a/rio-proto/proto/types.proto
+++ b/rio-proto/proto/types.proto
@@ -296,8 +296,12 @@
 message BuildOptions {
   uint64 max_silent_time = 1;             // Max seconds without output before timeout
   uint64 build_timeout = 2;              // Max total build time in seconds
   uint64 build_cores = 3;               // Number of cores to use (0 = all)
-  bool keep_going = 4;                   // Continue building other derivations on failure
+  // Field 4 (keep_going) — wrong layer. keep_going is per-BUILD (DAG-level:
+  // "continue dispatching sibling derivations after one fails"), handled
+  // scheduler-side by r[sched.build.keep-going]. Per-DERIVATION keep_going
+  // is meaningless for wopBuildDerivation (one drv → one daemon call).
+  reserved 4;
+  reserved "keep_going";
 }
```

Delete `actor/build.rs:567` (the `keep_going: false` line) — struct init shrinks.

### 4c. `Substituted` / `CachedFailure` — wire the translation, don't reserve

These are live on the consumer side (`completion.rs:141,156` — `Substituted` →
success path, `CachedFailure` → permanent-failure path). The gap is the
**translation** at `executor/mod.rs:736-744`:

```rust
let status = match build_result.status {
    rio_nix::protocol::build::BuildStatus::PermanentFailure => {
        BuildResultStatus::PermanentFailure
    }
    rio_nix::protocol::build::BuildStatus::TransientFailure => {
        BuildResultStatus::TransientFailure
    }
    _ => BuildResultStatus::PermanentFailure,
};
```

The `_` arm silently collapses `TimedOut`, `CachedFailure`, `LogLimitExceeded`,
`OutputRejected`, `DependencyFailed`, `NotDeterministic`, `MiscFailure`,
`InputRejected`, `NoSubstituters` all to `PermanentFailure`. Some of those have
direct proto analogues (`LogLimitExceeded`, `OutputRejected`, `DependencyFailed`).

**Are `Substituted` / `CachedFailure` reachable?** In the current setup, no:

- `client_set_options` sends `useSubstitutes: false` (line 301). The daemon
  won't substitute → never returns `Substituted`.
- Each build gets a fresh daemon with a synthetic SQLite DB (`synth_db::generate_db`
  at `mod.rs:409`). No failure cache → never returns `CachedFailure`.

But the `_` arm is still wrong because it **hides** statuses that ARE reachable:
`TimedOut` (daemon's own `max-silent-time` fires), `OutputRejected` (FOD hash
mismatch), `NotDeterministic` (repeat builds). Replace with an exhaustive match:

```diff
--- a/rio-worker/src/executor/mod.rs
+++ b/rio-worker/src/executor/mod.rs
@@ -736,9 +736,38 @@
-        let status = match build_result.status {
-            rio_nix::protocol::build::BuildStatus::PermanentFailure => {
-                BuildResultStatus::PermanentFailure
-            }
-            rio_nix::protocol::build::BuildStatus::TransientFailure => {
-                BuildResultStatus::TransientFailure
-            }
-            _ => BuildResultStatus::PermanentFailure,
-        };
+        use rio_nix::protocol::build::BuildStatus as Nix;
+        let status = match build_result.status {
+            // Direct mappings.
+            Nix::PermanentFailure => BuildResultStatus::PermanentFailure,
+            Nix::TransientFailure => BuildResultStatus::TransientFailure,
+            Nix::OutputRejected => BuildResultStatus::OutputRejected,
+            Nix::LogLimitExceeded => BuildResultStatus::LogLimitExceeded,
+            Nix::DependencyFailed => BuildResultStatus::DependencyFailed,
+            Nix::CachedFailure => BuildResultStatus::CachedFailure,
+            // TimedOut: daemon's own max-silent-time fired. Map to
+            // TransientFailure — a silent stall is usually a flaky
+            // network fetch or a hung subprocess, retryable.
+            Nix::TimedOut => BuildResultStatus::TransientFailure,
+            // NotDeterministic: --check found differing outputs. We
+            // don't (yet) run repeat builds, but if we did this is
+            // permanent (the derivation is broken, not the infra).
+            Nix::NotDeterministic => BuildResultStatus::PermanentFailure,
+            // InputRejected: daemon rejected an input path. Our
+            // synthetic DB should make this impossible; if it
+            // happens it's a worker bug, not the user's derivation.
+            Nix::InputRejected => BuildResultStatus::InfrastructureFailure,
+            // MiscFailure: Nix's catch-all. No better bucket.
+            Nix::MiscFailure => BuildResultStatus::PermanentFailure,
+            // Unreachable in this branch (is_success() returned false
+            // above — see mod.rs:is_success check). Exhaustive match
+            // so adding a new BuildStatus variant forces a revisit.
+            Nix::Built | Nix::Substituted | Nix::AlreadyValid
+            | Nix::ResolvesToAlreadyValid => {
+                // Defensive: log and report as built. If this fires,
+                // is_success() and the wire enum diverged.
+                tracing::error!(status = ?build_result.status, "success status in failure branch — is_success() out of sync");
+                BuildResultStatus::Built
+            }
+            // NoSubstituters: we set useSubstitutes=false, so the
+            // daemon won't even try. Unreachable; infra-fail if seen.
+            Nix::NoSubstituters => BuildResultStatus::InfrastructureFailure,
+        };
```

`Substituted` stays in the proto enum unchanged — the scheduler's handling is
correct for a future where workers CAN substitute (phase5+).

---

## 5. Tests

### 5a. `ListWorkers` shows nonzero resources (VM)

Add to whichever VM fragment already asserts on `ListWorkers` (check
`nix/tests/vm/`). The worker needs at least one heartbeat after the 10s poll
interval:

```python
# After a build has run (so cpu.stat has nonzero usage_usec delta):
worker_info = json.loads(
    control.succeed("rio-cli admin list-workers --json")
)[0]
res = worker_info["resources"]
# cpu_fraction: the sampler's first tick is 10s after startup; by the
# time we're here a build ran, so delta-CPU is definitely nonzero.
assert res["cpu_fraction"] > 0.0, f"cpu_fraction still zero: {res}"
# memory_used_bytes: worker process + at least one nix-daemon that ran.
# Even idle, the worker itself is >10 MB RSS.
assert res["memory_used_bytes"] > 10 * 1024 * 1024, f"memory too low: {res}"
# disk_total_bytes: statvfs on overlay_base_dir. Zero only if the
# directory doesn't exist, which would have failed the build earlier.
assert res["disk_total_bytes"] > 0, f"disk_total zero: {res}"
```

### 5b. Stale-generation rejection (unit)

`rio-worker` has no `MockScheduler` that can drive the event loop (§2.7 covers
why the scheduler's MockScheduler lies). Extract a free function for the
fence check so it's unit-testable without a stream:

```rust
// rio-worker/src/runtime.rs
/// Generation fence: should this assignment be rejected as stale?
/// Separate from the handler for testability — the event loop
/// in main.rs is hard to drive in unit tests (no mock SchedulerMessage stream).
pub fn is_stale_assignment(assignment_gen: u64, latest_observed: u64) -> bool {
    assignment_gen < latest_observed
}

#[cfg(test)]
mod fence_tests {
    // r[verify sched.lease.generation-fence]
    #[test]
    fn fence_rejects_strictly_less() {
        assert!(super::is_stale_assignment(1, 2));  // deposed leader
    }
    #[test]
    fn fence_accepts_equal() {
        assert!(!super::is_stale_assignment(2, 2)); // steady state
    }
    #[test]
    fn fence_accepts_greater() {
        // Assignment from a generation we haven't heartbeat-observed
        // yet. Possible: heartbeat interval is 10s, assignment can
        // arrive first after failover. Accept — we have no evidence
        // of staleness, and rejecting would stall post-failover dispatch.
        assert!(!super::is_stale_assignment(3, 2));
    }
    #[test]
    fn fence_cold_start_accepts() {
        // latest_observed starts at 0 (before first heartbeat).
        // Scheduler generation is always >= 1.
        assert!(!super::is_stale_assignment(1, 0));
    }
}
```

The `main.rs` handler calls `is_stale_assignment(assignment.generation,
latest_generation.load(Relaxed))` and the unit test covers the logic. For
the `fetch_max` monotonicity, a separate test:

```rust
#[test]
fn heartbeat_gen_monotone_under_interleaving() {
    let gen = AtomicU64::new(0);
    gen.fetch_max(3, Relaxed); // new leader heartbeat arrives first
    gen.fetch_max(1, Relaxed); // stale leader heartbeat arrives second
    assert_eq!(gen.load(Relaxed), 3); // didn't regress
}
```

### 5c. `max_silent_time` / `build_cores` on the wire (unit)

The existing `test_client_set_options_roundtrip` (`client.rs:659`) asserts the
wire layout. Extend it to take nonzero params and assert those bytes:

```rust
#[tokio::test]
async fn client_set_options_plumbs_values() -> anyhow::Result<()> {
    let mut buf = Vec::new();
    let mut sink = tokio::io::sink(); // discard the STDERR_LAST readback
    // (actually: use duplex + a fake STDERR_LAST writer, same as existing test)

    client_set_options(&mut reader, &mut buf, /*max_silent*/ 3600, /*cores*/ 4).await?;

    // Byte offsets: opcode(8) + keepFailed(8) + keepGoing(8) + tryFallback(8)
    //   + verbosity(8) + maxBuildJobs(8) = 48; maxSilentTime at [48..56].
    assert_eq!(u64::from_le_bytes(buf[48..56].try_into()?), 3600);
    // + useBuildHook(8) + verboseBuild(8) + logType(8) + printBuildTrace(8) = 80+8=88
    // buildCores at [88..96].
    assert_eq!(u64::from_le_bytes(buf[88..96].try_into()?), 4);
    Ok(())
}
```

(Compute exact offsets from the existing roundtrip test's layout — the above
is illustrative.)

### 5d. `fetch_max` observability

Add `rio_worker_stale_assignments_rejected_total` to `observability.md`.
The VM test for 2-replica failover (`scheduler.md:515` TODO(phase4b)) is the
right place to assert this counter increments; it's deferred. For phase4a,
the unit test (5b) + the metric's existence is sufficient.

---

## Checklist

- [ ] `cgroup.rs`: add `ResourceSnapshot` + `ResourceSnapshotHandle` + `sample_disk`;
      extend `utilization_reporter_loop` signature, write snapshot each tick
- [ ] `main.rs:136-139`: construct snapshot handle, pass to reporter loop +
      heartbeat loop
- [ ] `runtime.rs:94`: `resources.read().to_proto()` instead of `default()`
- [ ] `main.rs:285`: `latest_generation: Arc<AtomicU64>`, clone into heartbeat loop
- [ ] `main.rs:307`: `heartbeat_gen.fetch_max(resp.generation, Relaxed)` on accepted
- [ ] `main.rs:456`: fence check before permit-acquire; `continue` on stale
- [ ] `runtime.rs`: extract `is_stale_assignment` + unit tests
- [ ] `client.rs:275`: add `max_silent_time` + `build_cores` params, wire at 290/299
- [ ] `stderr_loop.rs:67`: pass params through from `run_daemon_build`
- [ ] `executor/mod.rs:425`: extract all three BuildOptions fields; pass to `run_daemon_build`
- [ ] `executor/mod.rs:736`: exhaustive `BuildStatus` → `BuildResultStatus` match
- [ ] `types.proto:147`: reserve `WorkerMessage` field 4; keep `ProgressUpdate` message
- [ ] `types.proto:300`: reserve `BuildOptions` field 4 (`keep_going`)
- [ ] `grpc/mod.rs:750-764`: delete `Msg::Progress(_)` arm
- [ ] `grpc/tests.rs:1066,1187-1211`: delete ProgressUpdate no-op tests
- [ ] `actor/build.rs:567`: delete `keep_going: false` line
- [ ] `client.rs` test: assert `max_silent_time`/`build_cores` bytes on wire
- [ ] VM test: `ListWorkers` resources nonzero after a build
- [ ] `observability.md`: add `rio_worker_stale_assignments_rejected_total`
- [ ] `tracey query rule sched.lease.generation-fence` shows worker-side `r[impl]`

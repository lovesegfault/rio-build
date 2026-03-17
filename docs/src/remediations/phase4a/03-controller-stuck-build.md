# Remediation 03: Controller stuck-Build (stream-drop-before-first-event)

**Source:** §1.3 of `docs/src/remediations/phase4a.md`
**Severity:** CRITICAL (P0) — stuck Build CRs require manual `kubectl edit` to unstick
**Findings:** 7 (one causal cluster)
**Files:** `rio-controller/src/reconcilers/build.rs`, `rio-controller/src/crds/build.rs`

---

## Causal chain (read this first)

All seven findings share one root: **a fire-and-forget `tokio::spawn` patches
nothing on a CR whose reconciler already returned `Action::await_change()`.**
With no `.owns()` watch and no periodic requeue, a spawned task that exits
without mutating the CR leaves it stuck forever.

```
SubmitBuild stream drops before event 0
 └─ drain_stream: build_id.is_empty() → bare `return`  ← #1
      └─ no status patch → no watch event → apply() never runs again  ← STUCK

spawn_reconnect_watch exhausts MAX_RECONNECT
 └─ patches phase=Unknown with ..Default::default()  ← #2, #3
      └─ Unknown is non-terminal → apply() spawns reconnect again  ← #5 (31s loop)

drain_stream WatchBuild Err
 └─ `stream` not replaced, next iter reads dead stream → Ok(None) → +1 attempt  ← #4
      └─ effective budget = MAX_RECONNECT / 2  ← #6
```

The doc comment at `build.rs:858-860` already **says** "apply()'s idempotence
gate will retry on next reconcile" — but nothing triggers that reconcile. The
comment is a lie; fix #1 makes it true.

---

## Fix #1 — `build.rs:861-864`: clear sentinel before return

**Finding:** `ctrl-drain-stream-early-exit-no-patch`

### Current code

```rust
// build.rs:852-864
// Reconnect: fresh scheduler client + WatchBuild.
// Need build_id — if we never got the first event
// (status.build_id empty), we can't reconnect.
// That's the "SubmitBuild stream dropped before
// first event" case — rare (scheduler crashed
// between MergeDag and first BuildStarted). Just
// exit; apply()'s idempotence gate will retry on
// next reconcile (build_id is still "submitted"
// sentinel → resubmit).
if status.build_id.is_empty() {
    warn!(build = %name, "no build_id yet; cannot reconnect, exiting watch");
    return;
}
```

The comment is wrong: `apply()` already returned `Action::await_change()`. No
`.owns()` watch, no periodic requeue → **nothing** triggers the next reconcile.
The CR sits at `phase=Pending, build_id="submitted"` forever.

Note that the `orphaned_sentinel` gate at `build.rs:141` **would** handle this
correctly — `!is_real_uuid && !ctx.watching.contains_key(&watch_key)` is true
the instant the scopeguard fires — but `apply()` is never called to evaluate it.

### Fix

Use `Patch::Merge` on the status subresource to clear **only** `build_id`. This
touches one field (no SSA stomp risk) and the resulting status change triggers
a reconcile via the controller's own watch on Build. The
`orphaned_sentinel` path at `build.rs:143-155` then falls through to resubmit.

```diff
--- a/rio-controller/src/reconcilers/build.rs
+++ b/rio-controller/src/reconcilers/build.rs
@@ -852,13 +852,29 @@ async fn drain_stream(
         // Reconnect: fresh scheduler client + WatchBuild.
         // Need build_id — if we never got the first event
         // (status.build_id empty), we can't reconnect.
         // That's the "SubmitBuild stream dropped before
-        // first event" case — rare (scheduler crashed
-        // between MergeDag and first BuildStarted). Just
-        // exit; apply()'s idempotence gate will retry on
-        // next reconcile (build_id is still "submitted"
-        // sentinel → resubmit).
+        // first event" case — scheduler crashed between
+        // MergeDag and first BuildStarted (SIGTERM rollout,
+        // panic on submit path, network partition).
+        //
+        // Clear the sentinel so the orphaned_sentinel gate in
+        // apply() (build.rs:141-155) fires on the reconcile
+        // that this patch triggers. MERGE patch: touches only
+        // build_id, no SSA stomp on progress/conditions.
+        // Best-effort — if the CR was deleted mid-watch, the
+        // 404 is fine (the scopeguard removes watching anyway).
         if status.build_id.is_empty() {
             warn!(build = %name, "no build_id yet; cannot reconnect, exiting watch");
+            let clear = serde_json::json!({"status": {"buildId": ""}});
+            if let Err(e) = api
+                .patch_status(&name, &PatchParams::default(), &Patch::Merge(&clear))
+                .await
+            {
+                warn!(build = %name, error = %e,
+                      "failed to clear sentinel on early-exit; Build may be stuck \
+                       until controller restart");
+            }
             return;
         }
```

**Why `Patch::Merge` not `patch_status()`:** `patch_status()` sends the full
`BuildStatus` via SSA-with-force. At this point `status` is the task-local
accumulator initialized at `build.rs:715-720` with `phase: "Pending"`,
`progress: ""`, `total_derivations: 0` — sending it would stomp any progress
that a *previous* watch task wrote (relevant when `drain_stream` is called from
`spawn_reconnect_watch` with a reconnect stream that immediately EOFs — see fix
#5's interaction note below). Merge patch on a single JSON key is surgical.

**Why `buildId: ""` not `buildId: "submitted"`:** the `orphaned_sentinel` gate
at `build.rs:141` checks `!is_real_uuid` which is `build_id != "submitted"`.
But the outer gate at `build.rs:121` checks `!status.build_id.is_empty()`.
Empty build_id **skips** the inner gates entirely and falls through to the
"first apply" path at `build.rs:195` — which is exactly what we want (full
resubmit including the sentinel-patch-before-submit sequence). Simpler than
relying on `orphaned_sentinel` logic.

---

## Fix #2 — `build.rs:419-428`: don't wipe status on reconnect-exhaust

**Finding:** `ctrl-reconnect-exhaust-wipes-status`

### Current code (`spawn_reconnect_watch`, exhaustion arm)

```rust
// build.rs:419-428
let _ = patch_status(
    &api,
    &name,
    BuildStatus {
        build_id: build_id.clone(),
        phase: "Unknown".into(),
        last_sequence: since_seq as i64,
        ..Default::default()   // ← wipes progress, started_at, conditions, *_derivations
    },
)
.await;
```

`patch_status()` is SSA with `.force()` (`build.rs:1074`). SSA replaces the
managed-fields set with what you send. `..Default::default()` sends:
- `progress: ""` — was e.g. `"47/100"`
- `total_derivations: 0` — was e.g. `100`
- `completed_derivations: 0` — was e.g. `47`
- `cached_derivations: 0` — was e.g. `12`
- `started_at: None` — was e.g. `2026-03-17T03:14:00Z`
- `conditions: []` — drops `Building` condition

User sees `kubectl get build` go from `47/100` → `(blank)`.

### Decision: Merge patch (not fetch-then-patch)

Two options were considered:

1. **Fetch current status, preserve, patch full struct.** Adds an extra
   round-trip, and has a TOCTOU race: another process (or the now-dead
   `drain_stream` if its final patch was still in-flight) could land a patch
   between GET and PATCH. Also: this path runs when the **scheduler** is
   unreachable; the apiserver is likely still up, but why add a fragile
   dependency on it?

2. **Merge patch setting only `phase` + `last_sequence`.** Touches exactly the
   fields we care about. No round-trip. No TOCTOU. Matches the
   `workerpool/mod.rs:373` scale-to-zero pattern. Works *together* with fix #3
   (serde skip) — SSA fields managed by this manager are preserved because we
   don't send them; fields we **do** send are exactly `phase`/`lastSequence`.

**Pick option 2.** Merge patch is strictly simpler and does not lose
information.

```diff
--- a/rio-controller/src/reconcilers/build.rs
+++ b/rio-controller/src/reconcilers/build.rs
@@ -407,32 +407,32 @@ fn spawn_reconnect_watch(
                 Err(e) => {
                     // Exhausted. Scheduler down past failover
                     // window, or build not found (recovery didn't
                     // reconstruct it — PG cleared, or from a very
                     // old pre-recovery scheduler). Patch phase=
                     // Unknown so operator sees "watch lost" not
-                    // just "stuck at last state" (same as
-                    // drain_stream's exhaustion path). Next
-                    // controller restart retries.
+                    // just "stuck at last state". MERGE patch:
+                    // touches only phase+last_sequence — progress/
+                    // conditions/started_at/*_derivations are
+                    // preserved (operator can still see "got to
+                    // 47/100 before watch broke").
                     warn!(build = %name, %build_id, error = %e,
                           attempts = MAX_RECONNECT,
                           "WatchBuild reconnect exhausted; status will stop updating");
-                    let _ = patch_status(
-                        &api,
-                        &name,
-                        BuildStatus {
-                            build_id: build_id.clone(),
-                            phase: "Unknown".into(),
-                            last_sequence: since_seq as i64,
-                            ..Default::default()
-                        },
-                    )
-                    .await;
+                    let patch = serde_json::json!({"status": {
+                        "phase": "Unknown",
+                        "lastSequence": since_seq as i64,
+                    }});
+                    let _ = api
+                        .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
+                        .await;
                     // Manual cleanup — drain_stream never ran so
                     // its guard didn't fire.
                     watching.remove(&watch_key);
                     return;
                 }
```

### Matching change in `drain_stream`'s own exhaustion path

`build.rs:837-838` has the **same** problem but is subtly less broken:
`drain_stream` accumulates `status` across events, so by the time it exhausts
it has the real `progress`/`conditions`/etc. **But** on the reconnect-spawn
path (`spawn_reconnect_watch` → `drain_stream`), `status` is re-initialized at
`build.rs:715-720` with `..Default::default()` — if the reconnected stream EOFs
immediately (scheduler came back, then went down again before any event), we're
back to wiping.

The safe fix: same merge-patch treatment.

```diff
--- a/rio-controller/src/reconcilers/build.rs
+++ b/rio-controller/src/reconcilers/build.rs
@@ -832,12 +832,17 @@ async fn drain_stream(
-            // Patch phase=Unknown so operator sees "watch
-            // lost" not just "stuck at last state". Terminal-
-            // ish (not Succeeded/Failed/Cancelled) so the
-            // idempotence gate WOULD reconnect on controller
-            // restart. Best-effort; ignore PATCH error.
-            status.phase = "Unknown".into();
-            let _ = patch_status(&api, &name, status).await;
+            // Patch phase=Unknown so operator sees "watch lost"
+            // not "stuck at last state". MERGE patch (not the SSA
+            // patch_status helper): the accumulated `status` may
+            // be the fresh-init default if this drain_stream was
+            // spawned via reconnect and the stream EOF'd before
+            // the first event — sending it would wipe progress.
+            let patch = serde_json::json!({"status": {
+                "phase": "Unknown",
+                "lastSequence": status.last_sequence,
+            }});
+            let _ = api
+                .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
+                .await;
             return;
```

---

## Fix #3 — `crds/build.rs`: skip-serialize-if-zero on count fields

**Finding:** `kube-status-ssa-wipes-cached-count`

### Problem

Every `patch_status()` call sends the full `BuildStatus` struct via SSA-force.
`i32` fields with `#[serde(default)]` but **no** `skip_serializing_if` always
serialize, even at zero. When `drain_stream` reconnects via
`spawn_reconnect_watch`, it re-initializes `status` at `build.rs:715-720` with
`..Default::default()` → `cached_derivations: 0`. The first `Progress` event
after reconnect patches `completed_derivations` and `total_derivations` (via
`apply_event`, `build.rs:947-951`) but **not** `cached_derivations` (only
`Started` sets it, `build.rs:929`). If `since_seq > Started.seq`, `Started` is
never replayed → `cached_derivations: 0` is sent → SSA stomps the real value.

### Affected fields

| Field | Line | Set by | Stomp risk |
|---|---|---|---|
| `total_derivations: i32` | 97 | `Started` + `Progress` | Low (Progress re-sets it) |
| `completed_derivations: i32` | 99 | `Progress` | Low (Progress re-sets it) |
| `cached_derivations: i32` | 101 | `Started` **only** | **HIGH** — reconnect past Started never refreshes it |
| `last_sequence: i64` | 133 | every event | None — always correct |
| `progress: String` | 92 | `Started` + `Progress` | Low — but `""` would stomp |
| `phase: String` | 80 | every transition | None — always set deliberately |

The `String` fields (`phase`, `progress`, `build_id`) are less dangerous
because they're always populated before any patch, but `progress` **can** be
`""` on reconnect-before-Progress — add `skip_serializing_if = "String::is_empty"`
for defense in depth.

### Fix

No `is_zero` helper exists in the workspace. Add one in `crds/build.rs` (not
`rio-common` — it's serde-specific and only used by CRDs).

```diff
--- a/rio-controller/src/crds/build.rs
+++ b/rio-controller/src/crds/build.rs
@@ -67,6 +67,17 @@ pub struct BuildSpec {
     pub tenant: Option<String>,
 }

+/// Serde helper: skip i32 fields at zero under SSA. Without this, a
+/// `patch_status()` after reconnect sends `cached_derivations: 0` (the
+/// fresh-init default) and SSA-force stomps the real value. Applies to
+/// count fields that are only set by specific events (Started) which a
+/// since_seq reconnect may skip.
+///
+/// Free function (not a closure) because `skip_serializing_if` takes a path.
+fn is_zero_i32(v: &i32) -> bool {
+    *v == 0
+}
+
 #[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
 #[serde(rename_all = "camelCase")]
 pub struct BuildStatus {
@@ -88,17 +99,22 @@ pub struct BuildStatus {
     /// "31/47" — completed/total derivations. Derived from
     /// scheduler's BuildEvent stream. String for the printer
     /// column (which only does simple jsonPath, no formatting).
-    #[serde(default)]
+    ///
+    /// skip_serializing_if: the reconnect-fresh-init case sends ""
+    /// otherwise — SSA would wipe the operator's "47/100" view.
+    #[serde(default, skip_serializing_if = "String::is_empty")]
     pub progress: String,

     /// Derivation counts. Individual fields so dashboards can
     /// `sum(.status.totalDerivations) across builds` etc.
-    #[serde(default)]
+    ///
+    /// skip_serializing_if on all three: see `is_zero_i32`. SSA
+    /// preserves the previously-patched value when omitted.
+    #[serde(default, skip_serializing_if = "is_zero_i32")]
     pub total_derivations: i32,
-    #[serde(default)]
+    #[serde(default, skip_serializing_if = "is_zero_i32")]
     pub completed_derivations: i32,
-    #[serde(default)]
+    #[serde(default, skip_serializing_if = "is_zero_i32")]
     pub cached_derivations: i32,
```

**NOT** applying to `last_sequence`: `0` is a meaningful value ("replay all",
per `build.rs:131`). Skipping it would prevent a deliberate reset-to-0, and
`drain_stream` always has the correct `last_sequence` anyway.

**NOT** applying to `phase`/`build_id`: empty string is never accidentally
sent — `phase` is always set before any patch (`"Pending"` at minimum), and
`build_id` is either the sentinel, empty-by-design (fix #1), or a real UUID.

---

## Fix #4 — `build.rs:882-898`: don't read dead stream after WatchBuild Err

**Finding:** `tonic-controller-watchbuild-retry-burns-on-dead-stream`

### Current code

```rust
// build.rs:869-898 (drain_stream reconnect body)
match sched
    .watch_build(types::WatchBuildRequest {
        build_id: status.build_id.clone(),
        since_sequence: status.last_sequence as u64,
    })
    .await
{
    Ok(resp) => {
        info!(build = %name, "reconnected WatchBuild");
        stream = resp.into_inner();
        // Loop continues: next iteration reads from the new stream.
    }
    Err(wb_err) => {
        // [ comment about failover / recovery race ]
        warn!(build = %name, error = %wb_err,
              "WatchBuild failed; retrying (failover/recovery race or build unknown)");
        // ← stream NOT replaced; next iter reads from the DEAD stream
    }
}
// loop iteration ends; back to top: stream.message().await on dead stream
```

The dead stream yields `Ok(None)` on the next `stream.message().await`. That's
a non-terminal EOF → `reconnect_attempts += 1` → backoff → WatchBuild again.
**Every WatchBuild error burns two attempts.** With `MAX_RECONNECT=5`, that's
effectively ~2.5 retries. Worse: the backoff sequence goes 1, 2, 4, 8, 16
seconds — but every *other* iteration is the dead-stream-read, which has zero
useful wait time. The effective backoff between WatchBuild calls becomes
`(1+2)=3s, (4+8)=12s, exhaust` — two real attempts, ~15s total, far short of
the ~31s the comment at `build.rs:821` claims.

### Fix

When WatchBuild errors, **skip** going back to `stream.message()`. Restructure
the loop so the reconnect body is a nested `loop` that only exits with a fresh
stream (or `return`s on exhaust). This is the cleanest option — `continue` from
the `Err` arm goes to the *outer* loop's top (still the dead stream), and
`stream: Option<Streaming<...>>` + top-of-loop match adds a None arm to every
iteration even in the steady state.

```diff
--- a/rio-controller/src/reconcilers/build.rs
+++ b/rio-controller/src/reconcilers/build.rs
@@ -819,12 +819,11 @@ async fn drain_stream(
         };

-        // ---- Reconnect via WatchBuild with last seq ----
-        // Backoff 1s/2s/4s/8s/16s; after MAX_RECONNECT fails, give
-        // up — status stale until next controller restart.
-        reconnect_attempts += 1;
-        metrics::counter!("rio_controller_build_watch_reconnects_total").increment(1);
-        if reconnect_attempts > MAX_RECONNECT {
+        // ---- Reconnect loop: WatchBuild until we get a fresh stream ----
+        // Nested loop so WatchBuild Err retries WITHOUT going back to
+        // stream.message() on the dead stream (which would Ok(None) →
+        // burn a second attempt per failure). Each iteration here
+        // backs off then calls WatchBuild; breaks with a fresh stream.
+        stream = loop {
+            reconnect_attempts += 1;
+            metrics::counter!("rio_controller_build_watch_reconnects_total").increment(1);
+            if reconnect_attempts > MAX_RECONNECT {
                 warn!(
                     build = %name,
                     error = %reconnect_reason,
@@ -839,7 +838,7 @@ async fn drain_stream(
                     .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                     .await;
                 return;
-        }
+            }
@@ -851,8 +850,8 @@ async fn drain_stream(
             tokio::time::sleep(backoff).await;

-        // Reconnect: fresh scheduler client + WatchBuild.
-        // Need build_id — if we never got the first event
+            // Need build_id — if we never got the first event
             // (status.build_id empty), we can't reconnect.
             // [fix #1 body here — clear sentinel + return]
@@ -875,28 +874,25 @@ async fn drain_stream(
                 .await
             {
                 Ok(resp) => {
                     info!(build = %name, "reconnected WatchBuild");
-                stream = resp.into_inner();
-                // Loop continues: next iteration reads from the
-                // new stream.
+                    break resp.into_inner();
                 }
                 Err(wb_err) => {
-                // WatchBuild failed. (a) Failover window: probe
-                // loop hasn't updated endpoint set yet — stale
-                // leader IP or no-endpoints. (b) Recovery RACE: a
-                // still-recovering leader → build_events map empty
-                // → BuildNotFound. TRANSIENT; retry after next
-                // backoff. ~15-30s until recover_from_pg completes.
-                // (c) Recovery GAP: recover_from_pg ran but didn't
-                // reconstruct this build (PG was cleared, build_id
-                // orphaned). Permanent. Can't distinguish ---
-                // retry and let MAX_RECONNECT bound the damage.
-                // The dead stream errors again next iteration
-                // → another backoff → another WatchBuild.
-                warn!(build = %name, error = %wb_err,
-                      "WatchBuild failed; retrying (failover/recovery race or build unknown)");
+                    // (a) Failover: probe loop hasn't updated endpoint
+                    // set yet. (b) Recovery RACE: still-recovering
+                    // leader → NotFound (transient, ~15-30s).
+                    // (c) Recovery GAP: build_id orphaned (permanent).
+                    // Can't distinguish (b)/(c) — retry and let
+                    // MAX_RECONNECT bound the damage. The nested loop
+                    // retries WatchBuild directly (no dead-stream
+                    // read); each pass = one real attempt.
+                    reconnect_reason = wb_err.to_string();
+                    warn!(build = %name, error = %wb_err,
+                          "WatchBuild failed; retrying");
                 }
             }
-        }
+        };
+        // Outer loop resumes: reads from the fresh stream.
     }
 }
```

Key changes:
- Reconnect body becomes `stream = loop { ... break resp.into_inner(); }`.
- `Err` arm updates `reconnect_reason` and falls through to the next inner
  iteration (which increments attempts + backs off).
- `reconnect_attempts` is only incremented per WatchBuild call, not per
  dead-stream read. Metric `rio_controller_build_watch_reconnects_total` now
  counts **actual** WatchBuild attempts.

**Interaction with fix #1:** the build_id-empty check + sentinel-clear stays
inside the inner loop (runs once on the first pass — `build_id` doesn't change
across WatchBuild failures).

---

## Fix #5 — Infinite-Unknown-cycle: stop respawning on `phase=Unknown`

**Finding:** `ctrl-unknown-phase-infinite-cycle`

### The loop

```
apply() runs
  → status.build_id is real UUID, phase="Unknown", non-terminal
  → ctx.watching doesn't contain watch_key (previous task exited, scopeguard fired)
  → spawn_reconnect_watch(...)  ← insert watching, spawn

spawn_reconnect_watch runs
  → WatchBuild fails (scheduler still unreachable — nothing changed)
  → backoff 1+2+4+8+16 = 31s
  → MAX_RECONNECT exhausted
  → patch_status(phase="Unknown")  ← triggers reconcile
  → watching.remove(&watch_key)
  → return

[reconcile triggered by patch]
apply() runs → GOTO top
```

Period ≈ 31s. Runs forever. Burns a reconcile worker slot. Generates one PATCH
per cycle (apiserver noise). Emits `MAX_RECONNECT` WARN logs per cycle
(log noise).

### Decision: treat `Unknown` as reconnect-terminal in `apply()`

Three options considered:

| Option | Pros | Cons |
|---|---|---|
| A. Make `Unknown` terminal in `is_terminal_phase()` | One-line fix | Semantically wrong — Unknown is **not** terminal (build may complete on its own). The test at `build.rs:1113` explicitly asserts `!is_terminal_phase("Unknown")` *because* controller restart should retry. Changing this breaks that contract. |
| B. Add `reconnect_cycles` counter field to `BuildStatus` | Bounded retry across restarts; operator-visible | CRD schema change (migration); one more field to reason about; doesn't fix the fundamental "nothing changed, why retry" smell |
| C. `apply()` refuses to spawn reconnect when `phase == "Unknown"` | No schema change; controller restart *does* still retry (apply() runs fresh, but `watching` is empty so... wait) | See below |

**Pick C with a twist.** The nuance: option C *as stated* doesn't work on
controller restart either — `apply()` sees `phase == "Unknown"` and refuses to
spawn. But that's actually **fine**: if the previous controller instance
exhausted, a fresh controller exhausting again is the likely outcome.

The real unstick path is: **operator (or admission webhook) patches phase back
to `Building` or `Pending`**. Then `apply()` respawns. This is a *deliberate*
human-in-the-loop gate — "the system gave up, a human confirmed it's worth
trying again".

To make this ergonomic, the `Unknown` patch (fix #2) should also set a
`Condition` explaining *why* and *what to do*:

```diff
--- a/rio-controller/src/reconcilers/build.rs
+++ b/rio-controller/src/reconcilers/build.rs
@@ -156,8 +156,24 @@ async fn apply(b: Arc<Build>, ctx: &Ctx) -> Result<Action> {
             // Fall through to the "first apply" path below — we'll
             // resubmit. SubmitBuild generates a FRESH build_id ...
             info!(build = %name, "orphaned 'submitted' sentinel with no watch; resubmitting");
-        } else if is_real_uuid && !is_terminal {
+        } else if is_real_uuid && !is_terminal && status.phase != "Unknown" {
             // Dedup: if drain_stream already running for this Build,
             // ... [existing reconnect-spawn logic]
+        } else if is_real_uuid && status.phase == "Unknown" {
+            // Reconnect previously EXHAUSTED. Respawning would
+            // re-exhaust (nothing changed) → patch Unknown again →
+            // trigger this reconcile → 31s infinite loop.
+            //
+            // Human-in-the-loop gate: operator patches phase back
+            // to "Building" (or deletes + recreates the CR) to
+            // explicitly retry. Controller restart does NOT auto-
+            // retry (if it exhausted once, it'll exhaust again
+            // absent intervention). The WatchLost condition (set
+            // by the exhaustion patch) has the `kubectl` command.
+            //
+            // requeue(5min): slow periodic reconcile keeps the
+            // `Unknown` phase visible in apply logs — operator can
+            // grep for it. NOT await_change: that would mean zero
+            // telemetry until someone pokes the CR.
+            debug!(build = %name, "phase=Unknown (reconnect exhausted); awaiting operator intervention");
+            return Ok(Action::requeue(Duration::from_secs(300)));
         } else {
             // Sentinel with watch still alive, OR terminal.
```

And the exhaustion patch (both `spawn_reconnect_watch` and `drain_stream`)
pushes a `WatchLost` condition:

```diff
 // In both exhaustion arms (spawn_reconnect_watch:419 and drain_stream:837):
 let patch = serde_json::json!({"status": {
     "phase": "Unknown",
     "lastSequence": since_seq as i64,
+    "conditions": [{
+        "type": "WatchLost",
+        "status": "True",
+        "reason": "ReconnectExhausted",
+        "message": format!(
+            "WatchBuild reconnect exhausted after {MAX_RECONNECT} attempts. \
+             Scheduler unreachable or build unknown. To retry: \
+             `kubectl patch build {name} -n {ns} --subresource=status \
+             --type=merge -p '{{\"status\":{{\"phase\":\"Building\"}}}}'`"
+        ),
+        "lastTransitionTime": k8s_openapi::jiff::Timestamp::now().to_string(),
+    }],
 }});
```

**Careful:** merge-patch on `conditions` (an array) has JSON merge patch
semantics — it **replaces** the whole array, not appends. This is *acceptable*
here because (a) at reconnect-exhaust time the only condition that matters is
`WatchLost`, and (b) the alternative is a JSON Patch with `add` on an index,
which requires knowing the current array length (another GET). If preserving
prior conditions matters, use `set_condition` (`build.rs:~1020`) on a
GET-then-merge — but per fix #2's reasoning, the extra round-trip isn't worth it.

### Test impact

`build.rs:1113` currently asserts `!is_terminal_phase("Unknown")`. That
assertion **stays** — `is_terminal_phase()` is unchanged. Add a new test:

```rust
/// Unknown is non-terminal (scheduler may complete the build) but
/// apply() must NOT respawn reconnect for it — the previous spawn
/// just exhausted, respawning is a 31s infinite loop.
#[test]
fn unknown_phase_is_reconnect_terminal_not_build_terminal() {
    // is_terminal_phase is about BUILD state — Unknown != build done.
    assert!(!is_terminal_phase("Unknown"));
    // But the reconnect gate in apply() must treat it as
    // don't-spawn. This is asserted by the kube_mock integration
    // test (see Tests section) — here we just document the contract.
}
```

---

## Fix #6 — `MAX_RECONNECT` budget: raise to 8, no separate NotFound budget

**Finding:** `ctrl-no-periodic-requeue` (retry budget too tight for the
recovery window it's meant to cover)

### Current state

Two separate `const MAX_RECONNECT: u32 = 5;`:
- `build.rs:361` (`spawn_reconnect_watch`)
- `build.rs:724` (`drain_stream`)

With fix #4 applied, backoff sequence for 5 attempts is `1+2+4+8+16 = 31s`.

### The race window this budget covers

Per the comment at `build.rs:885-888`: "still-recovering leader → build_events
map empty → BuildNotFound. TRANSIENT; retry after next backoff. ~15-30s until
recover_from_pg completes."

`lifecycle.nix:1174-1189` (`build-crd-reconnect` subtest) papers over this
race **externally** by waiting for `rio_scheduler_derivations_running ≥ 1` on
the new leader before checking Build status. The comment there says: "This is
STRUCTURAL, not a timeout bump — moves the controller's retry budget out of
the race window." That's fine for the **test**, but the **production** budget
should cover the window without the test's help.

The window is: lease TTL expire (~15s) + standby acquire tick (~5s) +
`recover_from_pg` (~1-5s depending on PG row count) + probe loop endpoint
update (~5s) = **~25-30s worst case**. 31s is on the edge. Under load (slow
PG, many rows) it can miss.

### Decision: raise to 8, single budget

- **8 attempts** with capped backoff `1+2+4+8+16+16+16+16 = 79s`. Comfortably
  covers the recovery window with 2× headroom.
- **No separate NotFound budget.** The comment at `build.rs:891` correctly
  notes: can't distinguish "recovery RACE (transient)" from "recovery GAP
  (permanent)". A separate NotFound budget would need to distinguish them,
  which we can't. One budget, sized for the transient case.
- **Hoist to module-level const** so the two call sites share it (and the
  test at `build.rs:1098-1116` can reference it).

```diff
--- a/rio-controller/src/reconcilers/build.rs
+++ b/rio-controller/src/reconcilers/build.rs
@@ -63,6 +63,20 @@ const MAX_DRV_NAR_SIZE: u64 = 256 * 1024;
 /// Builds queue behind this one in the controller's work queue).
 const DRV_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

+/// WatchBuild reconnect attempts before giving up and patching
+/// phase=Unknown. Shared by `spawn_reconnect_watch` (controller-restart
+/// path) and `drain_stream` (mid-stream scheduler-death path).
+///
+/// Budget must cover: lease TTL expire (~15s) + standby acquire tick
+/// (~5s) + `recover_from_pg` (~1-5s) + balanced-client probe-loop
+/// endpoint update (~5s) ≈ 30s worst case. With fix #4's one-attempt-
+/// per-WatchBuild (not two), 8 attempts at capped backoff
+/// 1+2+4+8+16+16+16+16 = 79s. 2× headroom over the window.
+///
+/// Not a separate NotFound budget: can't distinguish transient
+/// recovery-race from permanent recovery-gap (build.rs:891).
+const MAX_RECONNECT: u32 = 8;
+
 #[tracing::instrument(

@@ -361 +375 @@ fn spawn_reconnect_watch(
-        const MAX_RECONNECT: u32 = 5;

@@ -724 +737 @@ async fn drain_stream(
-    const MAX_RECONNECT: u32 = 5;
```

(Both local `const` declarations deleted; uses inherit the module-level const.)

---

## Tests

### T1 — Unit: serde skip on zero count fields (fix #3)

**File:** `rio-controller/src/crds/build.rs` (tests module)

```rust
/// SSA-force sends every serialized field. Zero-valued count fields
/// must be OMITTED so reconnect's fresh-init BuildStatus doesn't stomp
/// the real counts. Regression: §1.3 kube-status-ssa-wipes-cached-count.
#[test]
fn build_status_skips_zero_counts() {
    let s = BuildStatus {
        phase: "Building".into(),
        build_id: "abc".into(),
        // All counts at default (0) — must NOT appear in JSON.
        ..Default::default()
    };
    let json = serde_json::to_string(&s).unwrap();
    assert!(!json.contains("totalDerivations"), "zero total_derivations should be skipped: {json}");
    assert!(!json.contains("completedDerivations"), "zero completed_derivations should be skipped: {json}");
    assert!(!json.contains("cachedDerivations"), "zero cached_derivations should be skipped: {json}");
    assert!(!json.contains("progress"), "empty progress should be skipped: {json}");
    // phase/build_id ARE sent (non-empty).
    assert!(json.contains(r#""phase":"Building""#));
    // last_sequence IS sent even at 0 (0 = "replay all", meaningful).
    assert!(json.contains(r#""lastSequence":0"#));
}

/// Non-zero counts ARE serialized. Paired with the zero-skip test
/// above — proves the predicate, not just "field never emitted".
#[test]
fn build_status_sends_nonzero_counts() {
    let s = BuildStatus {
        total_derivations: 100,
        completed_derivations: 47,
        cached_derivations: 12,
        progress: "47/100".into(),
        ..Default::default()
    };
    let json = serde_json::to_string(&s).unwrap();
    assert!(json.contains(r#""totalDerivations":100"#));
    assert!(json.contains(r#""completedDerivations":47"#));
    assert!(json.contains(r#""cachedDerivations":12"#));
    assert!(json.contains(r#""progress":"47/100""#));
}
```

### T2 — Integration (kube_mock + MockScheduler): stream-drop-before-first-event resubmits

**File:** new `rio-controller/src/reconcilers/build_tests.rs` (or extend
`tests` module in `build.rs` — but this needs `MockScheduler`, which is an
integration-style fixture; prefer a separate file gated behind `#[cfg(test)]`).

**Precondition:** `MockScheduler` (`rio-test-support/src/grpc.rs:372`) already
supports `scripted_events: Some(vec![])` — empty scripted events → tx drops
immediately after stream open → `stream.message()` yields `Ok(None)` with no
events. Exactly the bug trigger.

```rust
/// Stream drops before event 0 → drain_stream clears sentinel →
/// next reconcile resubmits. Regression: §1.3 core bug.
///
/// The kube_mock must see TWO PATCHes on /builds/<name>/status:
///   1. sentinel (build_id="submitted", phase="Pending")  — from apply()
///   2. clear (build_id="")                                — from drain_stream fix #1
/// Then a THIRD PATCH (sentinel again) from the second apply() pass.
///
/// We assert the sequence, not the final state, because the mock
/// apiserver doesn't merge — it records what was SENT.
#[tokio::test(flavor = "multi_thread")]
async fn stream_drop_before_first_event_clears_sentinel_and_resubmits() {
    // MockScheduler: scripted_events = Some(vec![]) → immediate EOF.
    let sched = MockScheduler::new();
    sched.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![]), // ← zero events, then drop
        ..Default::default()
    });
    let (sched_handle, sched_addr) = spawn_mock_scheduler(sched.clone()).await;

    // ApiServerVerifier scenario list. The finalizer dance adds a
    // GET + PATCH (finalizer add) before apply() runs — same as
    // workerpool tests. Then:
    let scenarios = vec![
        // ... finalizer add GET+PATCH (copy from workerpool tests) ...
        Scenario::capture_body(
            http::Method::PATCH, "/builds/test-drop/status",
            /* assert body contains "buildId":"submitted" */),
        // ... store fetch for .drv content (or stub fetch_and_build_node) ...
        // drain_stream's clear patch — the FIX:
        Scenario::capture_body(
            http::Method::PATCH, "/builds/test-drop/status",
            /* assert body contains "buildId":"" */),
        // Second reconcile (triggered by the clear patch). GET shows
        // build_id="" → first-apply path → sentinel PATCH #2:
        Scenario::ok(http::Method::GET, "/builds/test-drop",
            /* body with status.buildId = "" */),
        Scenario::capture_body(
            http::Method::PATCH, "/builds/test-drop/status",
            /* assert body contains "buildId":"submitted" AGAIN */),
    ];

    // Run the reconciler with a timeout. 5s: sentinel patch is sync,
    // drain_stream's first stream.message() is immediate (mock tx
    // already dropped), clear patch is immediate. No backoff in the
    // build_id-empty path.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        run_reconcile_until_scenarios_exhausted(scenarios, sched_addr),
    ).await;

    assert!(result.is_ok(), "reconcile stuck — fix #1 not applied?");

    // MockScheduler saw exactly 2 SubmitBuild calls (initial + resubmit).
    let calls = sched.submit_calls.read().unwrap();
    assert_eq!(calls.len(), 2,
        "expected 2 SubmitBuild calls (original + resubmit after clear), got {}",
        calls.len());

    sched_handle.abort();
}
```

**Note:** `Scenario::capture_body` and `run_reconcile_until_scenarios_exhausted`
are not in the current `kube_mock` — they're small additions (`capture_body`
stores the request body in an `Arc<Mutex<Vec<String>>>` for later assertion;
`run_reconcile_until_scenarios_exhausted` drives the reconcile loop until the
scenario list is consumed). ~50 lines in `rio-test-support::kube_mock`.

### T3 — Integration (kube_mock + MockScheduler): status fields survive reconnect exhaustion

**Covers:** fix #2 (merge patch) + fix #3 (serde skip working together)

```rust
/// Reconnect-exhaust patches ONLY phase — progress/counts/started_at
/// preserved. Regression: §1.3 ctrl-reconnect-exhaust-wipes-status.
#[tokio::test(flavor = "multi_thread")]
async fn reconnect_exhaust_preserves_progress() {
    // MockScheduler: WatchBuild always fails (watch_fail_count = u32::MAX).
    // spawn_reconnect_watch will exhaust MAX_RECONNECT and merge-patch
    // phase=Unknown.
    let sched = MockScheduler::new();
    sched.set_outcome(MockSchedulerOutcome {
        watch_fail_count: Arc::new(AtomicU32::new(u32::MAX)),
        ..Default::default()
    });
    let (_handle, sched_addr) = spawn_mock_scheduler(sched).await;

    // kube_mock scenario: the Build CR GET response has a populated
    // status (real UUID, phase=Building, progress="47/100",
    // cached_derivations=12, started_at set). This simulates "controller
    // restarted mid-build, previous controller got to 47/100 before dying".
    let build_get_body = serde_json::json!({
        "apiVersion": "rio.build/v1alpha1", "kind": "Build",
        "metadata": {"name": "t", "namespace": "ns", "uid": "uid-1",
                     "finalizers": ["rio.build/build-cleanup"]},
        "spec": {"derivation": "/nix/store/aaa-foo.drv"},
        "status": {
            "buildId": "real-uuid-00000000-1111-2222-3333-444444444444",
            "phase": "Building",
            "progress": "47/100",
            "totalDerivations": 100,
            "completedDerivations": 47,
            "cachedDerivations": 12,
            "lastSequence": 47,
            "startedAt": "2026-03-17T03:00:00Z",
        }
    });

    // Scenario: GET (finalizer sees it) → apply() runs →
    // spawn_reconnect_watch → WatchBuild fails ×8 → exhaustion PATCH.
    // Capture that PATCH body.
    let exhaustion_patch = Arc::new(Mutex::new(String::new()));
    let scenarios = vec![
        Scenario::ok(http::Method::GET, "/builds/t", build_get_body.to_string()),
        Scenario::capture_into(
            http::Method::PATCH, "/builds/t/status",
            Arc::clone(&exhaustion_patch)),
    ];

    // NOTE: this test sleeps through MAX_RECONNECT backoffs (79s with
    // the raised budget). For unit-test speed, either:
    //  (a) tokio::time::pause() + manual advance — but see
    //      lang-gotchas.md re start_paused + real TCP
    //  (b) make MAX_RECONNECT test-configurable (feature flag or
    //      env var override in Ctx)
    // Option (b) is cleanest. Add `max_reconnect: u32` to Ctx with
    // default = MAX_RECONNECT; tests override to 2.
    run_with_test_ctx(scenarios, sched_addr, /* max_reconnect */ 2).await;

    let body = exhaustion_patch.lock().unwrap();
    // The merge patch sent ONLY phase + lastSequence + conditions.
    assert!(body.contains(r#""phase":"Unknown""#));
    assert!(body.contains(r#""lastSequence":47"#)); // preserved since_seq
    // It did NOT send zero-valued count fields (fix #3 + merge patch).
    assert!(!body.contains("totalDerivations"));
    assert!(!body.contains("completedDerivations"));
    assert!(!body.contains("cachedDerivations"));
    assert!(!body.contains(r#""progress":"""#));  // no empty-string stomp
    // And definitely didn't send started_at (Option::is_none skip already works).
    assert!(!body.contains("startedAt"));
}
```

### T4 — Integration: WatchBuild Err burns exactly one attempt (fix #4)

```rust
/// WatchBuild Err → ONE attempt consumed, not two. Regression: §1.3
/// tonic-controller-watchbuild-retry-burns-on-dead-stream.
///
/// Metric-based: rio_controller_build_watch_reconnects_total increments
/// exactly N for N WatchBuild calls, where N-1 fail and the Nth succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn watchbuild_err_burns_one_attempt_not_two() {
    // MockScheduler: initial SubmitBuild stream errors after 1 event
    // (so we have a real build_id). Then WatchBuild fails TWICE,
    // succeeds on the third call.
    let sched = MockScheduler::new();
    sched.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![mk_started_event()]),
        error_after_n: Some((1, tonic::Code::Unavailable)),  // stream errs after Started
        watch_fail_count: Arc::new(AtomicU32::new(2)),       // 2 fails, then ok
        watch_scripted_events: Some(vec![mk_completed_event()]),
        ..Default::default()
    });

    // Run drain_stream. Expect:
    //  - 1 reconnect for the initial Err (error_after_n)
    //  - 2 reconnects for WatchBuild fails
    //  - success on 3rd WatchBuild
    //  = 3 total increments of rio_controller_build_watch_reconnects_total
    // BEFORE fix #4: each WatchBuild Err went back to the dead stream,
    // burning 2 per failure → 1 + 2*2 = 5 increments (and exhaustion
    // at the old MAX_RECONNECT=5, so it never even reaches success).

    // ... (setup and run) ...

    let reconnects = read_metric("rio_controller_build_watch_reconnects_total");
    assert_eq!(reconnects, 3.0,
        "expected 3 reconnect attempts (1 stream-err + 2 WatchBuild-err); \
         {reconnects} suggests dead-stream double-burn (fix #4 regression)");
}
```

### T5 — VM assertion: `build-crd-reconnect` catches status wipe + stuck-sentinel

**File:** `nix/tests/scenarios/lifecycle.nix`, `build-crd-reconnect` fragment
(lines 1113-1247).

The existing subtest already kills the scheduler mid-build and asserts
`phase` reaches `Succeeded`. **Add** two assertions that would catch the
§1.3 bugs:

```diff
--- a/nix/tests/scenarios/lifecycle.nix
+++ b/nix/tests/scenarios/lifecycle.nix
@@ -1160,6 +1160,18 @@
           # still be the "submitted" sentinel and WatchBuild can't work.
           k3s_server.wait_until_succeeds(
               "k3s kubectl -n ${ns} get build test-reconnect "
               "-o jsonpath='{.status.phase}' | grep -qx Building",
               timeout=60,
           )
+
+          # Snapshot progress BEFORE the kill. After reconnect, it must
+          # not go backward. Regression guard: §1.3 status-wipe — before
+          # fix #2/#3, reconnect's fresh-init BuildStatus stomped
+          # cached_derivations→0 and progress→"" via SSA-force.
+          # jsonpath on the whole status for one-shot capture.
+          status_before = json.loads(kubectl(
+              "get build test-reconnect -o jsonpath='{.status}'"
+          ))
+          cached_before = status_before.get("cachedDerivations", 0)
+          total_before = status_before.get("totalDerivations", 0)
+          print(f"build-crd-reconnect: pre-kill cached={cached_before} total={total_before}")

@@ -1197,6 +1209,35 @@
               k3s_server.wait_until_succeeds(
                   "k3s kubectl -n ${ns} get build test-reconnect "
                   "-o jsonpath='{.status.phase}' | "
                   "grep -E '^(Succeeded|Completed|Cached)$'",
                   timeout=180,
               )
+
+              # ── §1.3 regression: counts didn't go backward ─────────
+              # After reconnect, drain_stream's fresh-init status must
+              # NOT stomp cached_derivations (only Started sets it; a
+              # since_seq reconnect past Started never refreshes it).
+              # Fix #3's skip_serializing_if keeps the 0 out of the
+              # PATCH body → SSA preserves the old value.
+              status_after = json.loads(kubectl(
+                  "get build test-reconnect -o jsonpath='{.status}'"
+              ))
+              cached_after = status_after.get("cachedDerivations", 0)
+              total_after = status_after.get("totalDerivations", 0)
+              # >= not ==: totalDerivations could legitimately stay
+              # the same or (if Progress carried a corrected count) go
+              # up. It must NOT drop to 0.
+              assert cached_after >= cached_before, (
+                  f"cachedDerivations went BACKWARD across reconnect: "
+                  f"{cached_before} → {cached_after}. "
+                  f"Fix #3 (serde skip_serializing_if is_zero) regressed? "
+                  f"Full status: {status_after}"
+              )
+              assert total_after >= total_before and total_after > 0, (
+                  f"totalDerivations went backward or hit 0: "
+                  f"{total_before} → {total_after}"
+              )
+              # Phase is NOT "Unknown" — infinite-cycle didn't fire.
+              assert status_after.get("phase") != "Unknown", (
+                  f"phase=Unknown on a Build that DID complete — "
+                  f"infinite-cycle (fix #5) regressed? {status_after}"
+              )
           except Exception:
```

**Stuck-sentinel coverage is harder in the VM:** triggering "stream drops
before event 0" requires killing the scheduler in a ~millisecond window
(between `SubmitBuild` accept and first event send). Not reliably reproducible
in a VM test. The kube_mock test T2 covers it deterministically. The VM test
can add a **negative** assertion — no Build ever observed with
`build_id="submitted"` past a timeout — but it won't *trigger* the bug.

---

## Verification

### Existing VM tests that exercise controller reconnect

| Fragment | What it tests | Would it catch §1.3 bugs before this plan? |
|---|---|---|
| `lifecycle.nix` `build-crd-flow` | Stable scheduler, dedup gate, sentinel→UUID transition | **No** — scheduler never restarts |
| `lifecycle.nix` `controller-restart` | **Controller** pod kill → fresh DashMap → `spawn_reconnect_watch` | **Partially** — exercises `spawn_reconnect_watch` but scheduler stays up, so WatchBuild succeeds first try. Wouldn't catch #2 (exhaustion never reached) or #4 (no WatchBuild Err). |
| `lifecycle.nix` `build-crd-reconnect` | **Scheduler** pod kill → `drain_stream` non-terminal EOF → reconnect loop | **Closest.** Exercises `drain_stream` reconnect. With `sched_metric_wait` at L1186, it waits past the recovery window → budget never strained. Wouldn't reliably catch #4 (budget halving) or #2/#3 (status wipe — no assertion on counts). The T5 assertions above fix the latter. |
| `lifecycle.nix` `recovery` | Scheduler kill via ssh-ng build (no Build CR) | **No** — controller uninvolved |

### New coverage

| Bug | Caught by |
|---|---|
| #1 Stuck sentinel | T2 (kube_mock, deterministic) |
| #2 Status wipe on exhaust | T3 (kube_mock) + T5 VM assertion |
| #3 cached_derivations SSA stomp | T1 (serde unit) + T5 VM assertion |
| #4 Dead-stream double-burn | T4 (kube_mock, metric-based) |
| #5 Infinite Unknown cycle | T5 VM negative assertion (phase != Unknown); infinite-cycle has no standalone deterministic test because triggering it requires fix #5 absent AND scheduler permanently down — easier to assert the gate holds via a kube_mock test: `apply()` on a Build with `phase=Unknown, build_id=real` returns `Action::requeue(300s)` and `watching` stays empty (no spawn). |
| #6 Budget too tight | Indirectly by T4 (metric count) + manual inspection; no automated "is 79s enough" test (would need timed recovery injection) |

### tracey annotations

```
// r[impl ctrl.build.sentinel]  — already at build.rs:6; fix #1 goes under it
// r[impl ctrl.build.reconnect] — already at build.rs:811; fixes #4/#6 extend it
// r[verify ctrl.build.sentinel] — T2
// r[verify ctrl.build.reconnect] — T4 + T5 (T5 already has r[verify] at lifecycle.nix:53)
```

Add new spec rules (with `r[...]` markers in `docs/src/components/controller.md`
or wherever the Build reconciler spec lives):

- `r[ctrl.build.unknown-terminal]` — "`phase=Unknown` halts reconnect respawn;
  operator intervention required to retry" (fix #5)
- `r[ctrl.build.status-preserve]` — "reconnect/exhaustion patches preserve
  progress/count fields" (fixes #2 + #3)

### Execution order

1. Fix #3 (serde skip) — standalone, no behavior change on non-zero values,
   unit-testable in isolation. Land first.
2. Fix #4 (nested reconnect loop) + Fix #6 (hoist MAX_RECONNECT) — mechanical
   refactor of `drain_stream`'s reconnect body. Land together.
3. Fix #1 (clear sentinel) + Fix #2 (merge patch on exhaust) — both touch the
   exhaustion/exit paths, share the merge-patch pattern. Land together.
4. Fix #5 (Unknown gate in `apply()`) — depends on fix #2's `WatchLost`
   condition for the operator message. Land last.
5. Tests T1-T4 land with their respective fixes. T5 (VM assertion) lands with
   fix #2/#3 (it's the regression guard for those).
6. `nix-build-remote -- .#ci` after step 5. `build-crd-reconnect` fragment
   (`default.nix:265`) is the canary.

# §2.15 — Gateway SubmitBuild first-event / reconnect gap

**Parent:** [`phase4a.md` §2.15](../phase4a.md#215-gateway-submitreconnect-gap)
**Findings:** `tonic-gateway-first-event-no-reconnect`, `gw-submit-build-bare-question-mark-no-stderr`
**Status:** OPEN · P1 · structural fix (Option B), gateway+scheduler, no `.proto` break

---

## 1. The race

`rio-gateway/src/handler/build.rs:198-214`:

```rust
let mut event_stream = rio_common::grpc::with_timeout(
    "SubmitBuild", DEFAULT_GRPC_TIMEOUT,
    scheduler_client.submit_build(request),
).await?                                          // ← :203 bare ?
.into_inner();

let first = event_stream.message().await
    .map_err(|e| anyhow!("build event stream error: {e}"))?;  // ← :210 bare ?
let Some(first) = first else {
    return Err(anyhow!("empty build event stream"));          // ← :213
};
let build_id = first.build_id.clone();
// ... active_build_ids.insert, handle first.event, THEN enter reconnect loop
```

The reconnect loop at `:282-383` only wraps `process_build_events`. The first-event peek is **outside** it.

### The window

Scheduler `grpc/mod.rs:468-482`:

```rust
let bcast = self.send_and_await(cmd, reply_rx).await?;  // ← MergeDag COMMITTED (PG row, actor state)
// ...
Ok(Response::new(bridge_build_events(...)))             // ← headers sent, stream spawned
```

`send_and_await` returning `Ok` means the actor processed `MergeDag` — `build_id` is in PG, the DAG is merged, workers may already be dispatched. If the scheduler SIGTERMs after `:468` but before `bridge_build_events`' spawned task sends event 0 (`BuildStarted`):

- gRPC initial headers **do** reach the gateway (tonic sends them when `Ok(Response::new(...))` returns from the handler).
- The stream body has zero events. Graceful shutdown → spawned bridge task dropped → `tx` drops → gateway's `event_stream.message().await` → `Ok(None)`.
- Gateway: `"empty build event stream"` → `Err` → caller maps to `BuildResult::failure` (wopBuildDerivation `:544`) or `stderr_err!` (wopBuildPaths `:660`).
- Client retries → **second build submitted**. MergeDag dedups *derivations* by hash, but a fresh `build_id` row + broadcast channel leak. Controller comment at `reconcilers/build.rs:243-245` calls this a "zombie build."

The window is narrow (MergeDag returned → first event sent is typically sub-millisecond) but non-zero. Scheduler rollout during a submission burst hits it.

---

## 2. Option A vs Option B — picking B

| | Option A: resubmit loop | Option B: build_id in response header |
|---|---|---|
| Scope | gateway only | gateway + scheduler + `rio-proto` const |
| `.proto` change | no | **no** (gRPC metadata is out-of-band) |
| Eliminates race | **no** — still double-submits, just moves the retry from client to gateway | **yes** — build_id available before any stream read |
| Zombie build leak | yes (1 per scheduler blip during submit) | no |
| Controller (`reconcilers/build.rs:313-315`, same bug — file deleted in P0294) | still broken | fixed by same 3-line scheduler diff |
| Request clone cost | `SubmitBuildRequest` has `Vec<DerivationNode>` + `Vec<DerivationEdge>` — up to `MAX_DAG_NODES` × ~256 KB drv_content. Clone-before-send for retry. | none |

### Why A looks tempting but isn't

A wraps `submit_build` + first-event-peek in a bounded retry loop. On `Ok(None)` from the first peek, resubmit the (cloned) request. This *works* — the second `build_id` gets a stream, client sees success — but:

1. **The first build is orphaned.** Nobody watches it. It runs to completion on workers, emits events to a broadcast channel with zero subscribers, and sits in PG until the terminal-state cleanup sweep. At best wasted compute; at worst a noisy red herring in `rio_scheduler_builds_active` metrics during an incident.

2. **`SubmitBuildRequest` clone is not free.** `rio-common::limits::MAX_DAG_NODES` worth of `DerivationNode` each carrying `drv_content` (capped at 256 KB per node, 16 MB total per the comment at `grpc/mod.rs:396-405`). We'd hold a clone in memory for the lifetime of the submit+peek just in case we need to retry. Most submits don't retry.

3. **It's the same band-aid the client already applies.** The whole reason this bug matters is "client retries → double submit." A moves that retry one layer in. Same end state.

### Why B

Option B exploits a property already true today: **by the time `submit_build().await` returns `Ok`, the scheduler has assigned `build_id` and committed MergeDag.** `build_id = Uuid::now_v7()` at `grpc/mod.rs:418` happens *before* `send_and_await`. The UUID is known 50 lines before `Response::new`. We just aren't sending it to the gateway yet.

gRPC initial metadata (response headers) goes out when the server handler returns `Ok(Response)` — **before** any stream message. The gateway reads `resp.metadata()` before `resp.into_inner()`. If the scheduler dies between returning `Ok(Response)` and sending event 0, the gateway already has `build_id` and enters the reconnect loop immediately. `WatchBuild(build_id, since_sequence=0)` → PG replay → full event history.

No `.proto` change: the `rpc SubmitBuild(...) returns (stream BuildEvent)` signature is untouched. Headers are a separate channel.

**Picked: B.** Fits the 5-instance pattern — Option A is "retry the symptom", Option B is "close the window where the symptom is possible."

---

## 3. `rio-proto` — header name constant

**File:** `rio-proto/src/lib.rs` (next to `DEFAULT_MAX_MESSAGE_SIZE` at `:12`)

```diff
 pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 32 * 1024 * 1024;

+/// gRPC initial-metadata key carrying the scheduler-assigned build_id
+/// on `SubmitBuild` responses. Server-streaming RPCs send initial
+/// metadata (headers) BEFORE any stream message, so the client has
+/// `build_id` even if the stream delivers zero events (scheduler
+/// SIGTERM between MergeDag commit and first BuildEvent send).
+///
+/// Value: UUID v7 stringified (always ASCII, always a valid
+/// `MetadataValue<Ascii>`).
+///
+/// Introduced phase4a (§2.15 remediation). Absent header = legacy
+/// scheduler; callers fall back to first-event peek.
+pub const BUILD_ID_HEADER: &str = "x-rio-build-id";
```

`x-` prefix: gRPC reserves lowercase unprefixed names; `x-` is the convention for application-defined headers. tonic's `MetadataMap::insert` takes `&'static str` for the key.

---

## 4. Scheduler — set the header

**File:** `rio-scheduler/src/grpc/mod.rs:476-483`

```diff
         info!(build_id = %build_id, "build submitted");
         // No replay: fresh build, MergeDag subscribed BEFORE seq=1
         // (Started) was emitted. last_seq=0, no gap. Pure broadcast.
-        Ok(Response::new(bridge_build_events(
+        let mut resp = Response::new(bridge_build_events(
             "submit-build-bridge",
             bcast,
             None,
-        )))
+        ));
+        // Initial metadata: build_id. Reaches the client as soon as
+        // this function returns Ok — BEFORE bridge_build_events' task
+        // sends event 0. If we SIGTERM between here and event 0, the
+        // gateway has build_id and can WatchBuild-reconnect. Closes
+        // the "empty build event stream" gap (phase4a §2.15).
+        //
+        // UUID.to_string() is always ASCII-hex-and-dashes — the
+        // .parse::<MetadataValue<Ascii>>() cannot fail. expect() not
+        // unwrap() so the message is greppable if this invariant ever breaks.
+        resp.metadata_mut().insert(
+            rio_proto::BUILD_ID_HEADER,
+            build_id.to_string().parse()
+                .expect("UUID string is always valid ASCII metadata"),
+        );
+        Ok(resp)
     }
```

3 lines of state change, 9 lines of comment. No behaviour change for any client that ignores the header.

---

## 5. Gateway — read header, retire the peek

**File:** `rio-gateway/src/handler/build.rs:198-259`

The first-event peek (`:207-214`) and the first-event match (`:228-259`) are **both** deletable when the header is present. `process_build_events` already handles every event variant including `Started` — the inline match at `:228` was only there because we'd already consumed the first event to get `build_id` and had to do *something* with it.

### Diff

```diff
-    let mut event_stream = rio_common::grpc::with_timeout(
+    let resp = match rio_common::grpc::with_timeout(
         "SubmitBuild",
         DEFAULT_GRPC_TIMEOUT,
         scheduler_client.submit_build(request),
     )
-    .await?
-    .into_inner();
-
-    // Peek at first message to get build_id
-    let first = event_stream
-        .message()
-        .await
-        .map_err(|e| anyhow::anyhow!("build event stream error: {e}"))?;
-
-    let Some(first) = first else {
-        return Err(anyhow::anyhow!("empty build event stream"));
-    };
-    let build_id = first.build_id.clone();
-
-    active_build_ids.insert(build_id.clone(), 0);
+    .await
+    {
+        Ok(r) => r,
+        Err(e) => {
+            // STDERR_NEXT diagnostic before the Err propagates. Callers
+            // map this to BuildResult::failure (opcodes 36/46) or
+            // stderr_err! (opcode 9) — the client SEES the failure
+            // either way, but without this line the only context is
+            // the tonic Status string with no indication it was the
+            // INITIAL submit that failed (vs. a mid-stream event, vs.
+            // reconnect exhaustion — all three produce anyhow Errs).
+            // r[impl gw.stderr.error-before-return] — diagnostic, not
+            // STDERR_ERROR; caller decides terminal framing.
+            let _ = stderr
+                .log(&format!("SubmitBuild RPC failed: {e}\n"))
+                .await;
+            return Err(anyhow::anyhow!("SubmitBuild failed: {e}"));
+        }
+    };
+
+    // build_id from initial response metadata. Scheduler sets this
+    // AFTER MergeDag commits (grpc/mod.rs:~480) — if we have it, the
+    // build IS durable and WatchBuild can resume it. Reconnect
+    // protection is total: even zero stream events is recoverable.
+    //
+    // Fallback to first-event peek if the header is absent — legacy
+    // scheduler (pre-phase4a). After one deploy cycle this branch is
+    // dead; keep until the fleet is known-upgraded.
+    let header_build_id = resp
+        .metadata()
+        .get(rio_proto::BUILD_ID_HEADER)
+        .and_then(|v| v.to_str().ok())
+        .map(str::to_owned);
+    let mut event_stream = resp.into_inner();
+
+    let build_id = match header_build_id {
+        Some(id) => id,
+        None => {
+            // Legacy path. NOT reconnect-protected — that's the bug
+            // this remediation closes for the header path.
+            tracing::debug!("scheduler did not set x-rio-build-id header (legacy); peeking first event");
+            let first = match event_stream.message().await {
+                Ok(Some(ev)) => ev,
+                Ok(None) => {
+                    let _ = stderr
+                        .log("scheduler closed stream before first event (legacy path, no build_id to reconnect)\n")
+                        .await;
+                    return Err(anyhow::anyhow!(
+                        "empty build event stream (legacy scheduler, no header)"
+                    ));
+                }
+                Err(e) => {
+                    let _ = stderr
+                        .log(&format!("build event stream error on first read: {e}\n"))
+                        .await;
+                    return Err(anyhow::anyhow!("build event stream error: {e}"));
+                }
+            };
+            // Still need to handle the consumed event. Push it through
+            // the same match that was at :228-259 — NOW scoped to the
+            // legacy branch only. See the pre-fix code for the full
+            // match body; it is mechanically identical.
+            let id = first.build_id.clone();
+            active_build_ids.insert(id.clone(), 0);
+            if let Some(result) = handle_peeked_first_event(&first, active_build_ids) {
+                return Ok(result);
+            }
+            id
+        }
+    };

-    // Emit trace_id to the client via STDERR_NEXT — gives operators a
-    // grep handle for Tempo when debugging a user's build. Empty-guard
-    // suppresses output when no OTel tracer is configured
-    // (current_trace_id_hex returns "" for TraceId::INVALID).
+    // Header path: insert happens HERE (not inside the match) so we
+    // only insert once. Legacy path inserted inside its branch above.
+    // Idempotent for the legacy case (same key, same value).
+    active_build_ids.insert(build_id.clone(), 0);
+
+    // Emit trace_id to the client via STDERR_NEXT. With the header
+    // path this now fires BEFORE event 0 — operator gets the Tempo
+    // handle the moment the build is accepted, not after the first
+    // event arrives. Small UX win.
     let trace_id = rio_proto::interceptor::current_trace_id_hex();
     if !trace_id.is_empty() {
         let _ = stderr.log(&format!("rio trace_id: {trace_id}\n")).await;
     }
-
-    match &first.event {
-        Some(types::build_event::Event::Started(started)) => {
-            debug!(
-                build_id = %build_id,
-                total = started.total_derivations,
-                cached = started.cached_derivations,
-                "build started"
-            );
-        }
-        Some(types::build_event::Event::Completed(_)) => {
-            active_build_ids.remove(&build_id);
-            return Ok(BuildResult::success());
-        }
-        Some(types::build_event::Event::Failed(failed)) => {
-            active_build_ids.remove(&build_id);
-            return Ok(BuildResult::failure(
-                BuildStatus::MiscFailure,
-                failed.error_message.clone(),
-            ));
-        }
-        // Cancelled can be the first event on WatchBuild reconnect
-        // after the build was already cancelled — scheduler replays
-        // from build_event_log past since_sequence.
-        Some(types::build_event::Event::Cancelled(cancelled)) => {
-            active_build_ids.remove(&build_id);
-            return Ok(BuildResult::failure(
-                BuildStatus::MiscFailure,
-                format!("build cancelled: {}", cancelled.reason),
-            ));
-        }
-        _ => {}
-    }

     // Process remaining events, with reconnect on stream error.
```

### `handle_peeked_first_event` helper

The legacy branch still needs to process the consumed first event. Extract the old `:228-259` match into a helper so the legacy branch stays readable and the main flow (header path) has no distraction:

```rust
/// Legacy-path helper: the first event was consumed by the build_id
/// peek. Process it. Returns `Some(BuildResult)` if the first event
/// was terminal (Completed/Failed/Cancelled — caller returns early),
/// `None` otherwise (caller continues into process_build_events).
///
/// Delete when the legacy peek path is removed (after fleet-wide
/// scheduler upgrade).
fn handle_peeked_first_event(
    first: &types::BuildEvent,
    active_build_ids: &mut HashMap<String, u64>,
) -> Option<BuildResult> {
    let build_id = &first.build_id;
    match &first.event {
        Some(types::build_event::Event::Started(started)) => {
            debug!(
                build_id = %build_id,
                total = started.total_derivations,
                cached = started.cached_derivations,
                "build started"
            );
            None
        }
        Some(types::build_event::Event::Completed(_)) => {
            active_build_ids.remove(build_id);
            Some(BuildResult::success())
        }
        Some(types::build_event::Event::Failed(failed)) => {
            active_build_ids.remove(build_id);
            Some(BuildResult::failure(
                BuildStatus::MiscFailure,
                failed.error_message.clone(),
            ))
        }
        Some(types::build_event::Event::Cancelled(cancelled)) => {
            active_build_ids.remove(build_id);
            Some(BuildResult::failure(
                BuildStatus::MiscFailure,
                format!("build cancelled: {}", cancelled.reason),
            ))
        }
        _ => None,
    }
}
```

Place it above `submit_and_process_build` (after the `impl Display for StreamProcessError` at `:46`).

### Net effect on the header path

Submit → `Ok(resp)` → read header → have `build_id` → insert into `active_build_ids` with seq 0 → emit trace_id → enter the reconnect-protected `process_build_events` loop. Event 0 is the *first thing* the loop reads. If the stream is empty, `process_build_events` returns `EofWithoutTerminal` → reconnect arm at `:305` fires → `WatchBuild(build_id, since=0)`. **The bug is closed.**

---

## 6. `gw-submit-build-bare-question-mark-no-stderr` — resolved by §5

The three bare-`?` sites at `:203`, `:210`, `:213` now each have a `stderr.log()` call before `return Err`. Resolution per the finding's two offered choices: **send `STDERR_NEXT` diagnostic, accept the asymmetry** (callers still choose between `BuildResult::failure` and `stderr_err!`).

Why not `stderr_err!` directly: two of three callers (`:540` wopBuildDerivation, `:822` wopBuildPathsWithResults) catch the `Err` and convert to `BuildResult::failure` delivered via `STDERR_LAST`. Sending `STDERR_ERROR` inside `submit_and_process_build` would produce the exact `STDERR_ERROR → STDERR_LAST` desync that remediation 07 fixes. `STDERR_NEXT` is the right layer: informational log that reaches the client's terminal, leaves the terminal-frame decision to the caller.

The `:210` and `:213` diagnostics are in the legacy branch — they will only fire until the scheduler fleet is upgraded, but they fire today and give operators a "oh, scheduler rolled during my submit" signal instead of a naked `"empty build event stream"`.

---

## 7. Controller — same bug, follow-up

`rio-controller/src/reconcilers/build.rs:257` + `:313-315` (file deleted in P0294):

```rust
let stream = sched.submit_build(req).await?.into_inner();
// ...
// No known build_id yet — scheduler assigns it on MergeDag, first event carries it.
drain_stream(..., stream, ..., /* since_seq */ 0, /* build_id */ None, ...)
```

Identical bug. `drain_stream` peeks the first event for `build_id` inside the spawned task. If the stream is empty, the task exits, `watching` entry is removed, the Build CR stays at `phase=Pending` / `build_id="submitted"` forever (the sentinel from `:251`). Next reconcile sees the sentinel and skips resubmit (`:124-135`). **Stuck Build.**

The scheduler diff in §4 fixes the *sender side* for both consumers. The controller-side read is a 3-line diff at `:257`:

```diff
-    let stream = sched.submit_build(req).await?.into_inner();
+    let resp = sched.submit_build(req).await?;
+    let header_build_id = resp.metadata()
+        .get(rio_proto::BUILD_ID_HEADER)
+        .and_then(|v| v.to_str().ok())
+        .map(str::to_owned);
+    let stream = resp.into_inner();
```

Then pass `header_build_id` instead of `None` at `:315`. `drain_stream`'s existing logic (if `build_id.is_some()`, skip the peek and start the reconnect-protected loop immediately) already handles the rest — verify this at implementation time by reading `drain_stream`; if it doesn't branch on `Option`, it's a small addition.

**Not in scope for this remediation** (gateway-focused, per §2.15 title). Captured here so the §4 scheduler diff is reviewed with both consumers in mind. File a separate remediation or fold into the controller-stuck-build workstream (remediation 03 touches the same file).

---

## 8. MockScheduler — set the header

**File:** `rio-test-support/src/grpc.rs`

The mock becomes the ground truth for the new contract. Default to setting the header (matches the real scheduler post-§4); add an opt-out for legacy-path tests.

```diff
 pub struct MockSchedulerOutcome {
     pub submit_error: Option<tonic::Code>,
     pub send_completed: bool,
     // ...
+    /// If false, SubmitBuild omits the `x-rio-build-id` initial-metadata
+    /// header. For exercising the gateway's legacy first-event-peek
+    /// fallback. Default true (matches phase4a+ scheduler).
+    pub suppress_build_id_header: bool,
 }
```

In `submit_build` at `:447-449` and `:499-501` (both `return Ok(Response::new(...))` sites):

```diff
-            return Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
-                rx,
-            )));
+            let mut resp = Response::new(tokio_stream::wrappers::ReceiverStream::new(rx));
+            if !outcome.suppress_build_id_header {
+                resp.metadata_mut().insert(
+                    rio_proto::BUILD_ID_HEADER,
+                    build_id.parse().expect("test build_id is ASCII"),
+                );
+            }
+            return Ok(resp);
```

`build_id` is `"test-build-00000000-1111-2222-3333-444444444444"` at `:408` — ASCII, valid.

**Naming:** `suppress_build_id_header` (default `false` = header set) rather than `set_build_id_header` (default `true`). `#[derive(Default)]` on `MockSchedulerOutcome` gives `bool::default() = false` — we want `false` to mean "new behavior" so existing tests that use `..Default::default()` don't need touching.

---

## 9. Tests

### 9a. Flip existing `test_build_paths_empty_event_stream_failure`

**File:** `rio-gateway/tests/wire_opcodes/build.rs:762-788`

Current assertion: `scripted_events: Some(vec![])` → `drain_stderr_expecting_error` → `err.message.contains("empty")`.

After §4+§5+§8, the same setup takes the **header path** → reconnect loop fires on the empty stream. This test should now **pass via WatchBuild**, not error.

```diff
-/// Empty event stream (scripted_events = Some(vec![])) → first peek gets
-/// Ok(None) → gateway returns Err("empty build event stream") → opcode 9
-/// sends STDERR_ERROR.
+// r[verify gw.reconnect.backoff]
+/// Phase4a §2.15: scheduler accepts SubmitBuild (MergeDag committed,
+/// build_id in header) then drops the stream before event 0. Gateway
+/// reads build_id from initial metadata → enters reconnect loop →
+/// WatchBuild(build_id, since=0) → Completed. Client sees success, not
+/// "empty build event stream".
+///
+/// Before this fix: the first-event peek at build.rs:207 got Ok(None)
+/// → Err("empty build event stream") → opcode 9 sends STDERR_ERROR.
+/// The build was already committed but the gateway had no build_id
+/// to reconnect with.
 #[tokio::test]
-async fn test_build_paths_empty_event_stream_failure() -> anyhow::Result<()> {
+async fn test_build_paths_empty_stream_reconnects_via_header() -> anyhow::Result<()> {
     let mut h = GatewaySession::new_with_handshake().await?;
     h.scheduler.set_outcome(MockSchedulerOutcome {
+        // Zero events: tx drops immediately, stream is empty. Header
+        // IS set (default). Gateway reads build_id from header, enters
+        // the reconnect loop, process_build_events gets EofWithoutTerminal
+        // on its first read, reconnect arm fires.
         scripted_events: Some(vec![]),
+        // WatchBuild delivers the terminal event.
+        watch_scripted_events: Some(vec![ev(build_event::Event::Completed(
+            types::BuildCompleted {
+                output_paths: vec!["/nix/store/zzz-out".into()],
+            },
+        ))]),
         ..Default::default()
     });
     let drv_path = seed_minimal_drv(&h);

     wire_send!(&mut h.stream;
         u64: 9,
         strings: &[format!("{drv_path}!out")],
         u64: 0,
     );

-    let err = drain_stderr_expecting_error(&mut h.stream).await?;
-    assert!(
-        err.message.contains("empty"),
-        "expected 'empty build event stream', got: {}",
-        err.message
-    );
+    // PRIMARY ASSERTION: reconnect fired, no STDERR_ERROR. Same shape
+    // as test_build_paths_eof_triggers_reconnect_not_error — but that
+    // test has Started THEN EOF (first event arrives); here the stream
+    // is empty from event 0. Before this fix that distinction mattered;
+    // now both paths are identical (header means we don't need event 0).
+    let frames = collect_stderr_frames(&mut h.stream).await;
+    let saw_reconnect = frames
+        .iter()
+        .any(|m| matches!(m, StderrMessage::Next(s) if s.contains("reconnecting")));
+    assert!(
+        saw_reconnect,
+        "expected 'reconnecting...' STDERR_NEXT (empty stream \u{2192} EofWithoutTerminal \u{2192} reconnect), got: {frames:?}"
+    );
+    let saw_error = frames.iter().any(|m| matches!(m, StderrMessage::Error(_)));
+    assert!(!saw_error, "STDERR_ERROR should not be sent. frames: {frames:?}");
+
+    // wopBuildPaths success → u64(1).
+    let result = wire::read_u64(&mut h.stream).await?;
+    assert_eq!(result, 1, "BuildPaths returns u64(1) on success");
+
+    // Exactly ONE SubmitBuild (Option B's whole point: no resubmit).
+    // A hypothetical Option A fix would show 2 here.
+    assert_eq!(
+        h.scheduler.submit_calls.read().unwrap().len(),
+        1,
+        "build_id from header means NO resubmit — one SubmitBuild, one WatchBuild"
+    );
     h.finish().await;
     Ok(())
 }
```

**Real time, not paused.** One 1s backoff, same as `test_build_paths_reconnect_on_transport_error` at `:811`. `start_paused=true` fires the `DEFAULT_GRPC_TIMEOUT` wrapper on `submit_build` while TCP I/O pends (known gotcha, noted at `:796-801`).

### 9b. Legacy-path regression

The fallback must keep working until the scheduler fleet is upgraded.

```rust
/// Legacy scheduler (no x-rio-build-id header): gateway falls back to
/// first-event peek. Stream empty → no build_id → Err with diagnostic
/// STDERR_NEXT, then caller's stderr_err!. This is the PRE-fix behaviour,
/// preserved under `suppress_build_id_header` for deploy-order safety.
#[tokio::test]
async fn test_build_paths_empty_stream_legacy_no_header() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![]),
        suppress_build_id_header: true,   // ← legacy scheduler
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    // Drain all frames. We expect: STDERR_NEXT diagnostic ("legacy path,
    // no build_id to reconnect"), THEN STDERR_ERROR (opcode 9 wraps
    // the returned Err with stderr_err! at build.rs:660).
    let frames = collect_stderr_frames(&mut h.stream).await;

    // Diagnostic STDERR_NEXT — this is the §6 fix. Before, the user
    // saw only the STDERR_ERROR with no context.
    let saw_diag = frames.iter().any(|m| matches!(
        m, StderrMessage::Next(s) if s.contains("legacy") || s.contains("before first event")
    ));
    assert!(saw_diag, "expected diagnostic STDERR_NEXT on legacy empty-stream. frames: {frames:?}");

    // STDERR_ERROR follows (caller's stderr_err!).
    let err = frames.iter().find_map(|m| match m {
        StderrMessage::Error(e) => Some(e),
        _ => None,
    }).expect("expected STDERR_ERROR on legacy empty-stream path");
    assert!(
        err.message.contains("empty") || err.message.contains("legacy"),
        "error message: {}", err.message
    );

    // NO reconnect attempt — legacy path has no build_id.
    let saw_reconnect = frames
        .iter()
        .any(|m| matches!(m, StderrMessage::Next(s) if s.contains("reconnecting")));
    assert!(!saw_reconnect, "legacy path cannot reconnect (no build_id)");

    h.finish().await;
    Ok(())
}
```

### 9c. SubmitBuild RPC failure diagnostic

Exercises the `:203` bare-`?` → diagnostic path (`submit_error: Some(Code::Unavailable)`):

```rust
/// SubmitBuild RPC itself fails (Unavailable — scheduler down before
/// accept). Gateway emits "SubmitBuild RPC failed" STDERR_NEXT before
/// the caller's stderr_err!. No header, no build_id, nothing committed
/// scheduler-side — correctly NOT a reconnect case.
#[tokio::test]
async fn test_build_paths_submit_rpc_fail_diagnostic() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        submit_error: Some(tonic::Code::Unavailable),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    let frames = collect_stderr_frames(&mut h.stream).await;
    let saw_diag = frames.iter().any(|m| matches!(
        m, StderrMessage::Next(s) if s.contains("SubmitBuild RPC failed")
    ));
    assert!(saw_diag, "expected 'SubmitBuild RPC failed' STDERR_NEXT. frames: {frames:?}");

    h.finish().await;
    Ok(())
}
```

### 9d. VM test — **not added**

§2.15 is a scheduler-rollout-during-submit race. Reproducing it in a VM requires `systemctl kill -s SIGTERM rio-scheduler` timed between `send_and_await` return and `bridge_build_events`' first send — a microsecond window. The existing `vm-lifecycle-recovery-k3s` subtest kills the scheduler *mid-build* (after events are flowing), which exercises the reconnect loop but not this specific pre-first-event gap.

A reliable VM reproduction would need either (a) an artificial delay injected into `bridge_build_events` (test-only code path in the scheduler, undesirable), or (b) a probabilistic hammer loop (submit + kill + check, ×N). Neither is worth the maintenance cost when the unit tests in §9a directly exercise the code path with `scripted_events: Some(vec![])`.

The unit tests prove: header set + empty stream → reconnect works. The real scheduler's §4 diff proves: header is always set post-MergeDag. Transitively covered.

---

## 10. Sequencing

| Step | Change | Gate |
|---|---|---|
| 1 | §3 `rio-proto` const | `cargo build -p rio-proto` |
| 2 | §4 scheduler header set | `cargo nextest run -p rio-scheduler` — existing tests unaffected (header is additive) |
| 3 | §8 MockScheduler header (default on) | `cargo nextest run -p rio-gateway` — **expect `test_build_paths_empty_event_stream_failure` to FAIL** (it asserts `STDERR_ERROR` but now gets `STDERR_LAST`; MockScheduler sets the header, gateway's unchanged peek still runs but `process_build_events` now gets event 0 via WatchBuild... actually no — gateway is unchanged at this step, the peek runs first, gets `Ok(None)`, errors out. The header is set but nobody reads it. **This test passes unchanged at step 3.** |
| 4 | §5 gateway header-read + diagnostic | — |
| 5 | `cargo nextest run -p rio-gateway` — **`test_build_paths_empty_event_stream_failure` FAILS** (header path → reconnect → success; test expects `STDERR_ERROR`) | Proof that the header path is live |
| 6 | §9a flip the test, §9b+§9c new tests | `cargo nextest run -p rio-gateway` green |
| 7 | `cargo clippy --all-targets -- -D warnings` | — |
| 8 | `tracey query rule gw.reconnect.backoff` — confirm `verify` annotation on 9a is picked up | — |

**Step 5 is the proof step.** If `test_build_paths_empty_event_stream_failure` does NOT fail after step 4, the gateway isn't reading the header or the MockScheduler isn't setting it. Do not skip.

---

## Implementation checklist

- [ ] `rio-proto/src/lib.rs` — `BUILD_ID_HEADER` const (§3)
- [ ] `rio-scheduler/src/grpc/mod.rs:478` — set header on `Response` (§4)
- [ ] `rio-gateway/src/handler/build.rs` — `handle_peeked_first_event` helper, header read, diagnostic logs (§5)
- [ ] `rio-test-support/src/grpc.rs` — `suppress_build_id_header` field + header set in both `Response::new` sites (§8)
- [ ] `rio-gateway/tests/wire_opcodes/build.rs` — flip `:762` test (§9a), add legacy + diag tests (§9b, §9c)
- [ ] `nix develop -c cargo nextest run` green
- [ ] `nix develop -c cargo clippy --all-targets -- --deny warnings` green
- [ ] `tracey query rule gw.reconnect.backoff` — new `verify` site visible
- [ ] Follow-up: controller read-side (§7) — separate commit, cross-ref remediation 03

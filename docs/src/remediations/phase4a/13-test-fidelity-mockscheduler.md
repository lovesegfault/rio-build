# Remediation 13: Test fidelity — MockScheduler lies

**Parent finding:** [§2.7](../phase4a.md#27-test-fidelity-mockscheduler-lies)
**Findings:** `test-mock-watch-build-ignores-since-sequence`, `test-mock-sequence-starts-at-zero`, `test-start-paused-real-tcp-spawn-blocking`, `tonic-upload-start-paused-real-tcp`
**Blast radius:** P1 — reconnect tests prove "doesn't crash" not "works"; CI-loaded-runner flake risk on `start_paused`+TCP tests; future test drift where a green suite masks a real-store regression
**Effort:** ~4 h implementation (6 mock diffs + 1 helper + 3 test fixture edits + 1 new test) + 1 nextest run

---

## Ground truth

§2.7's remediation index points at `rio-gateway/tests/common/grpc.rs:507-549`. That path is
**stale** — the mocks were extracted to `rio-test-support/src/grpc.rs` (shared by worker,
gateway, scheduler, controller). All line numbers below are against `rio-test-support`.

Two findings (`test-start-paused-real-tcp-spawn-blocking`, `tonic-upload-start-paused-real-tcp`)
describe the same root cause from two angles. One helper (`spawn_mock_store_inproc`)
addresses both.

---

## Latent bug this remediation is designed to expose

**`rio-gateway/src/handler/build.rs:217`:**

```rust
active_build_ids.insert(build_id.clone(), 0);  // ← hardcodes 0
```

The gateway peeks the first `BuildEvent` to get `build_id`, but throws away
`first.sequence` and hardcodes `0`. The real scheduler starts sequences at 1,
so on the very first reconnect (stream dies right after `Started`), the gateway
sends `since_sequence=0` → scheduler replays `Started(seq=1)`. `process_build_events`
handles the replayed `Started` benignly (just `debug!()` at `build.rs:127-135`),
so there's no crash — but it's a silent off-by-one that replays one extra event
on every first-event reconnect.

The current mock starts sequences at 0 (`grpc.rs:431`, `:456`), so this bug is
invisible: the mock's `Started` has `sequence=0`, the gateway hardcodes `0`,
they accidentally agree.

§2 (below) shifts mock sequences to start at 1. §1 makes the mock honor
`since_sequence`. Together they **make a test writable** that proves
`active_build_ids.insert(build_id, first.sequence)` — see §7.

---

## 1. `MockScheduler::watch_build` — record + honor `since_sequence`

**Current** (`rio-test-support/src/grpc.rs:507-549`):

```rust
async fn watch_build(
    &self,
    _request: Request<types::WatchBuildRequest>,  // ← discarded
) -> Result<Response<Self::WatchBuildStream>, Status> {
```

Two changes: (a) record into `watch_calls` so tests can assert what
`since_sequence` the gateway actually sent; (b) skip scripted events whose
`sequence ≤ since_sequence`, mirroring the real scheduler's
`build_event_log` replay filter.

```diff
--- a/rio-test-support/src/grpc.rs
+++ b/rio-test-support/src/grpc.rs
@@ -372,6 +372,10 @@ pub struct MockScheduler {
     pub submit_calls: Arc<RwLock<Vec<types::SubmitBuildRequest>>>,
     /// CancelBuild calls received: (build_id, reason).
     pub cancel_calls: Arc<RwLock<Vec<(String, String)>>>,
+    /// WatchBuild calls received: (build_id, since_sequence). For asserting
+    /// the gateway's reconnect sent the correct resume point (see
+    /// `r[gw.reconnect.since-seq]`).
+    pub watch_calls: Arc<RwLock<Vec<(String, u64)>>>,
 }
```

```diff
@@ -507,9 +511,15 @@ impl SchedulerService for MockScheduler {
     async fn watch_build(
         &self,
-        _request: Request<types::WatchBuildRequest>,
+        request: Request<types::WatchBuildRequest>,
     ) -> Result<Response<Self::WatchBuildStream>, Status> {
+        let req = request.into_inner();
+        let since = req.since_sequence;
+        self.watch_calls
+            .write()
+            .unwrap()
+            .push((req.build_id, since));
+
         let outcome = self.outcome.read().unwrap().clone();

         // Injected-failure countdown: decrement and return Unavailable while > 0.
@@ -526,15 +536,24 @@ impl SchedulerService for MockScheduler {
         // Scripted WatchBuild stream — same auto-fill pattern as SubmitBuild.
+        // HONORS since_sequence: events with sequence ≤ `since` are skipped,
+        // mirroring rio-scheduler's build_event_log replay. Auto-fill runs
+        // FIRST (so `sequence: 0` in a scripted event becomes `(idx+1)`
+        // before the filter checks it), then the filter compares the
+        // FINAL sequence value against `since`.
         if let Some(events) = outcome.watch_scripted_events {
             let (tx, rx) = tokio::sync::mpsc::channel(32);
             let build_id = "test-build-00000000-1111-2222-3333-444444444444".to_string();
             tokio::spawn(async move {
                 for (seq, mut ev) in events.into_iter().enumerate() {
                     if ev.build_id.is_empty() {
                         ev.build_id = build_id.clone();
                     }
                     if ev.sequence == 0 {
-                        ev.sequence = seq as u64;
+                        ev.sequence = (seq as u64) + 1;
+                    }
+                    // Real scheduler: strictly-greater-than filter.
+                    if ev.sequence <= since {
+                        continue;
                     }
                     if tx.send(Ok(ev)).await.is_err() {
                         return;
```

**Filter ordering:** auto-fill happens before the `<= since` check. A test
that writes `watch_scripted_events: Some(vec![ev(Completed)])` gets the
event auto-filled to `sequence=1`, **then** filtered against `since`. If
the gateway sends `since=1` (correct behavior after the `build.rs:217`
fix), that event is skipped — which is exactly right: the test fixture
must set an explicit `sequence: 2` to simulate "event that happened after
the last one the client saw." See §8 for the two tests that need this.

---

## 2. Auto-fill `sequence` starts at 1, not 0

**Current** (`rio-test-support/src/grpc.rs:430-432, 456, 469, 482, 535-537`):

Five sites auto-fill or hardcode `sequence` as 0-indexed. The real scheduler
starts at 1 (sequence 0 is the proto's "from start" sentinel on the
`WatchBuildRequest` side; the scheduler never **emits** an event with
`sequence=0`).

```diff
--- a/rio-test-support/src/grpc.rs
+++ b/rio-test-support/src/grpc.rs
@@ -428,9 +428,9 @@ impl SchedulerService for MockScheduler {
                     if ev.build_id.is_empty() {
                         ev.build_id = build_id.clone();
                     }
                     if ev.sequence == 0 {
-                        ev.sequence = seq as u64;
+                        ev.sequence = (seq as u64) + 1;
                     }
```

```diff
@@ -454,7 +454,7 @@ impl SchedulerService for MockScheduler {
             let _ = tx
                 .send(Ok(types::BuildEvent {
                     build_id: build_id.clone(),
-                    sequence: 0,
+                    sequence: 1,
                     timestamp: None,
                     event: Some(types::build_event::Event::Started(types::BuildStarted {
```

```diff
@@ -467,7 +467,7 @@ impl SchedulerService for MockScheduler {
                 let _ = tx
                     .send(Ok(types::BuildEvent {
                         build_id: build_id.clone(),
-                        sequence: 1,
+                        sequence: 2,
                         timestamp: None,
                         event: Some(types::build_event::Event::Completed(
```

```diff
@@ -480,7 +480,7 @@ impl SchedulerService for MockScheduler {
                 let _ = tx
                     .send(Ok(types::BuildEvent {
                         build_id,
-                        sequence: 1,
+                        sequence: 2,
                         timestamp: None,
                         event: Some(types::build_event::Event::Failed(types::BuildFailed {
```

(The `watch_scripted_events` auto-fill at `:535-537` is already covered by §1's diff.)

**`error_after_n` semantics unchanged:** the `seq == n` comparison at `:420`
compares against the **enumerate index** (0-based), not the event's
`sequence` field. `error_after_n: Some((1, ...))` still means "inject error
before sending the event at iteration index 1 (the second event)." No test
fixture needs updating for `error_after_n`.

---

## 3. `MockStore::put_path` — reject non-empty `metadata.nar_hash`

**Current** (`rio-test-support/src/grpc.rs:116-121`): accepts any
`metadata.nar_hash`. The real store rejects non-empty
(`rio-store/src/grpc/put_path.rs:206-211`) because hash-upfront mode was
deleted pre-phase3a; all clients (gateway `chunk_nar_for_put`, worker
`do_upload_streaming`) send empty metadata hash and put the real hash in
the trailer.

```diff
--- a/rio-test-support/src/grpc.rs
+++ b/rio-test-support/src/grpc.rs
@@ -116,10 +116,18 @@ impl StoreService for MockStore {
         let info = match first.msg {
             Some(types::put_path_request::Msg::Metadata(m)) => m
                 .info
                 .ok_or_else(|| Status::invalid_argument("PutPathMetadata missing PathInfo"))?,
             _ => return Err(Status::invalid_argument("first message must be metadata")),
         };
+        // Mirror real store (put_path.rs:206-211): hash-upfront was removed
+        // pre-phase3a. A non-empty metadata.nar_hash means an un-updated
+        // client. The real store rejects it; the mock must too, or a
+        // regression in chunk_nar_for_put / do_upload_streaming that
+        // stops zeroing the metadata hash goes green.
+        if !info.nar_hash.is_empty() {
+            return Err(Status::invalid_argument(
+                "PutPath metadata.nar_hash must be empty (send hash in trailer)",
+            ));
+        }
```

**No prod-code breakage:** all three `PutPath` producers already send empty:

- `rio-proto/src/client/mod.rs:236`: `std::mem::take(&mut raw.nar_hash)` →
  trailer, leaves `Vec::new()` in metadata.
- `rio-gateway/src/handler/grpc.rs:70`: `raw.nar_hash = Vec::new()` explicit.
- `rio-worker/src/upload.rs:220`: `nar_hash: Vec::new()` explicit.

This is pure regression protection.

---

## 4. `MockStore::put_path` — verify trailer hash matches `sha256(nar_bytes)`

**Current** (`rio-test-support/src/grpc.rs:125-141`): drains chunks and
trailer, blindly copies `trailer.nar_hash` into `info.nar_hash`, returns
`Ok(created=true)`. The real store re-hashes and rejects mismatches
(`put_path.rs` step 7, `NarDigest::from_bytes` vs trailer).

```diff
--- a/rio-test-support/src/grpc.rs
+++ b/rio-test-support/src/grpc.rs
@@ -122,20 +122,43 @@ impl StoreService for MockStore {
-        // Drain NAR chunks. Trailer carries hash/size (metadata hash is
-        // always empty — hash-upfront was removed). Mirrors real-store
-        // behavior so upload tests see the right recorded PathInfo.
+        // Drain NAR chunks + trailer. Hash the chunks as they arrive and
+        // verify against the trailer — mirrors real store's independent
+        // digest check. Without this, a test that sends a bogus trailer
+        // hash goes green against the mock but red against rio-store.
+        use sha2::{Digest, Sha256};
         let mut nar = Vec::new();
+        let mut hasher = Sha256::new();
+        let mut trailer: Option<types::PutPathTrailer> = None;
         let mut info = info;
         while let Some(msg) = stream.message().await? {
             match msg.msg {
                 Some(types::put_path_request::Msg::NarChunk(chunk)) => {
+                    hasher.update(&chunk);
                     nar.extend_from_slice(&chunk);
                 }
                 Some(types::put_path_request::Msg::Trailer(t)) => {
-                    info.nar_hash = t.nar_hash;
-                    info.nar_size = t.nar_size;
+                    trailer = Some(t);
                 }
                 _ => {}
             }
         }
+        // Real store rejects missing-trailer as a protocol violation
+        // (truncated stream / client gave up mid-upload). Current mock
+        // silently accepts — that's how test_upload_output_nar_serialize_error
+        // passed: ENOENT in spawn_blocking → channel drops → no trailer →
+        // mock says Ok(created=true). The worker's OWN dump_task error
+        // saves the test, but the mock's "ok" was a lie.
+        let Some(t) = trailer else {
+            return Err(Status::invalid_argument(
+                "PutPath stream closed without trailer",
+            ));
+        };
+        let computed: [u8; 32] = hasher.finalize().into();
+        if computed.as_slice() != t.nar_hash.as_slice() {
+            return Err(Status::invalid_argument(format!(
+                "PutPath trailer hash mismatch: computed {}, trailer {}",
+                hex::encode(computed),
+                hex::encode(&t.nar_hash),
+            )));
+        }
+        info.nar_hash = t.nar_hash;
+        info.nar_size = t.nar_size;
         self.put_calls.write().unwrap().push(info.clone());
```

`sha2` and `hex` are already workspace deps (used by worker, store, gateway).
Add to `rio-test-support/Cargo.toml`:

```diff
+sha2 = { workspace = true }
+hex = { workspace = true }
```

---

## 5. In-process tonic transport for `start_paused` tests

**Current** (`rio-worker/src/upload.rs:538, 558, 616`): three tests use
`#[tokio::test(start_paused = true)]` + `spawn_mock_store_with_client()`,
which binds a real TCP socket on `127.0.0.1:0`. Under `start_paused`,
tokio auto-advances the virtual clock when all **tokio tasks** are idle —
but a real TCP socket is "idle" while the kernel is doing real work. The
`spawn_blocking` NAR hasher is the same: idle from tokio's view while
actively hashing.

So `upload_output`'s `GRPC_STREAM_TIMEOUT` wrapper (`upload.rs:277-282`)
auto-fires while the TCP accept + HTTP/2 handshake are mid-flight on a
loaded runner. Failure mode: `DeadlineExceeded` on `PutPath` even though
the mock is perfectly healthy.

**`stderr_loop.rs:559` already does this right**: `start_paused` +
`tokio::io::duplex` — in-memory I/O is a tokio task, so auto-advance
doesn't fire while it's reading. We want the same for tonic.

### Helper: `spawn_mock_store_inproc`

Add to `rio-test-support/src/grpc.rs` below `spawn_mock_store_with_client`:

```diff
+/// Spawn a MockStore over an in-process duplex transport (no real TCP).
+///
+/// Use this for `#[tokio::test(start_paused = true)]` tests. The regular
+/// [`spawn_mock_store_with_client`] binds a real TCP socket, which makes
+/// tokio's auto-advance fire `.timeout()` wrappers while kernel-side
+/// accept/handshake are pending — spurious `DeadlineExceeded` on a
+/// loaded CI runner (§2.7 `test-start-paused-real-tcp-spawn-blocking`).
+///
+/// The duplex halves are tokio tasks, so auto-advance sees them as
+/// "not idle" while they're doing I/O. Same pattern as
+/// `rio-worker/src/executor/daemon/stderr_loop.rs:559`.
+///
+/// No `JoinHandle` returned: the server task is fire-and-forget (dies
+/// with the test). No port to clean up.
+pub async fn spawn_mock_store_inproc() -> anyhow::Result<(
+    MockStore,
+    rio_proto::StoreServiceClient<tonic::transport::Channel>,
+)> {
+    use hyper_util::rt::TokioIo;
+    use tonic::transport::Endpoint;
+
+    let store = MockStore::new();
+    let svc = StoreServiceServer::new(store.clone());
+
+    // Channel of server-side duplex halves. Each client "connect" mints
+    // a duplex pair, hands one half to the server via this channel.
+    // Unbounded: connect is bounded by the test itself (1-3 connections
+    // per test), no backpressure needed.
+    let (conn_tx, conn_rx) = tokio::sync::mpsc::unbounded_channel();
+    let incoming = tokio_stream::wrappers::UnboundedReceiverStream::new(conn_rx)
+        .map(Ok::<_, std::io::Error>);
+
+    tokio::spawn(async move {
+        Server::builder()
+            .add_service(svc)
+            .serve_with_incoming(incoming)
+            .await
+            .expect("in-process gRPC server");
+    });
+    tokio::task::yield_now().await;
+
+    // Connector: on each poll, create a fresh duplex pair, ship the
+    // server half, wrap the client half. URI is a dummy — tonic parses
+    // it but the connector never uses it.
+    //
+    // 64 KiB duplex buffer: tonic's default HTTP/2 window is 64 KiB;
+    // smaller than a NAR chunk (256 KiB) so the writer blocks briefly,
+    // but that's fine under paused time (the blocked write is a tokio
+    // task, not real I/O).
+    let channel = Endpoint::try_from("http://inproc.mock")?
+        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
+            let conn_tx = conn_tx.clone();
+            async move {
+                let (client_io, server_io) = tokio::io::duplex(64 * 1024);
+                conn_tx
+                    .send(TokioIo::new(server_io))
+                    .map_err(|_| std::io::Error::other("inproc server dropped"))?;
+                Ok::<_, std::io::Error>(TokioIo::new(client_io))
+            }
+        }))
+        .await?;
+
+    Ok((store, rio_proto::StoreServiceClient::new(channel)))
+}
```

Deps for `rio-test-support/Cargo.toml` (both already in workspace):

```diff
+tower = { workspace = true }
+hyper-util = { workspace = true }
```

### Test migration

```diff
--- a/rio-worker/src/upload.rs
+++ b/rio-worker/src/upload.rs
@@ -499 +499 @@
-    use rio_test_support::grpc::spawn_mock_store_with_client;
+    use rio_test_support::grpc::{spawn_mock_store_inproc, spawn_mock_store_with_client};
@@ -538,3 +538,3 @@
     #[tokio::test(start_paused = true)]
     async fn test_upload_output_retries_then_succeeds() -> anyhow::Result<()> {
-        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
+        let (store, mut client) = spawn_mock_store_inproc().await?;
@@ -558,3 +558,3 @@
     #[tokio::test(start_paused = true)]
     async fn test_upload_output_exhausts_retries() -> anyhow::Result<()> {
-        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
+        let (store, mut client) = spawn_mock_store_inproc().await?;
@@ -616,3 +616,3 @@
     #[tokio::test(start_paused = true)]
     async fn test_upload_output_nar_serialize_error() -> anyhow::Result<()> {
-        let (_store, mut client, _h) = spawn_mock_store_with_client().await?;
+        let (_store, mut client) = spawn_mock_store_inproc().await?;
```

Keep `spawn_mock_store_with_client` for the non-paused tests at
`upload.rs:513, 583, 681` — those exercise real TCP intentionally.

### `spawn_blocking` caveat

The in-proc transport removes the **socket** dimension from auto-advance.
`spawn_blocking` (for the NAR hasher at `upload.rs:242`) is **still** idle
from tokio's view — but in these tests the blocking work is hashing a few
bytes (`b"retry me"`, `b"never uploads"`), which completes in microseconds.
The failure mode is `spawn_blocking` thread-pool pressure under heavy CI
load making a ~10 µs hash take long enough for auto-advance to fire the
5 min `GRPC_STREAM_TIMEOUT`. That's a much wider margin than TCP connect
(which can take 10-100 ms on a loaded runner vs microseconds for in-mem).
Good enough; not perfect.

### Sibling: `rio-gateway/tests/wire_opcodes/build.rs:869`

`test_build_paths_reconnect_exhausted_returns_failure` has the same
`start_paused` + real-TCP hazard (MockScheduler over TCP via
`GatewaySession::new_with_handshake`). **Out of scope here** — that
fixture is tied up with the SSH-session duplex plumbing (`common/mod.rs`);
a MockScheduler in-proc helper would cascade into the whole
`GatewaySession` setup. Flag as a follow-up (the test's comment at
`:796-801` already knows and notes the tradeoff).

---

## 6. `ApiServerVerifier::run` — `#[must_use]` drop-bomb guard

**Current** (`rio-test-support/src/kube_mock.rs:83-113`): returns a bare
`JoinHandle<()>`. Every caller manually wraps in
`tokio::time::timeout(.., task).await.unwrap().unwrap()`. If a future
test forgets that step, it silently passes even when the code under test
made zero HTTP calls — the verifier task just hangs, nobody notices.

```diff
--- a/rio-test-support/src/kube_mock.rs
+++ b/rio-test-support/src/kube_mock.rs
@@ -50,6 +50,46 @@ impl Scenario {
     }
 }

+/// Returned by [`ApiServerVerifier::run`]. Holds the verifier task handle;
+/// panics on drop if [`VerifierGuard::verified`] wasn't called.
+///
+/// Why a drop-bomb instead of just `#[must_use]` on the handle: a test
+/// that binds `let _task = verifier.run(...)` defeats `#[must_use]` but
+/// still forgets to join. The bomb catches both: unbound (`#[must_use]`
+/// compile warning) AND bound-but-unjoined (runtime panic).
+///
+/// Disarming via `ManuallyDrop` is deliberately not exposed — the only
+/// way to disarm is to actually verify.
+#[must_use = "call .verified().await or the verifier panics on drop"]
+pub struct VerifierGuard {
+    handle: JoinHandle<()>,
+    armed: bool,
+}
+
+impl VerifierGuard {
+    /// Join the verifier task under a 5 s timeout. Returns the number
+    /// of scenarios it processed (always `scenarios.len()` on success —
+    /// the assert is inside the spawned task, so any mismatch already
+    /// panicked before this point).
+    ///
+    /// The 5 s timeout catches code-under-test that made FEWER calls
+    /// than scenarios (verifier blocks on `next_request()` forever).
+    /// 5 s is well above any reconcile/election tick.
+    pub async fn verified(mut self) {
+        self.armed = false;
+        tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.handle)
+            .await
+            .expect("verifier consumed all scenarios (code made the expected number of calls)")
+            .expect("verifier assertions passed (method/path matched every scenario)");
+    }
+}
+
+impl Drop for VerifierGuard {
+    fn drop(&mut self) {
+        if self.armed && !std::thread::panicking() {
+            panic!(
+                "VerifierGuard dropped without .verified().await — \
+                 test never proved the code made the expected HTTP calls"
+            );
+        }
+    }
+}
+
 /// Wraps the tower-test Handle. `run()` spawns a task that
 /// processes scenarios in order until exhausted.
 pub struct ApiServerVerifier {
@@ -80,8 +120,8 @@ impl ApiServerVerifier {
     /// made FEWER calls, the verifier blocks on next_request()
     /// forever.
-    pub fn run(self, scenarios: Vec<Scenario>) -> JoinHandle<()> {
-        tokio::spawn(async move {
+    pub fn run(self, scenarios: Vec<Scenario>) -> VerifierGuard {
+        let handle = tokio::spawn(async move {
             let mut handle = pin!(self.handle);
             for (i, scenario) in scenarios.into_iter().enumerate() {
@@ -110,7 +150,8 @@ impl ApiServerVerifier {
                 );
             }
-        })
+        });
+        VerifierGuard { handle, armed: true }
     }
 }
```

`!std::thread::panicking()` guard: if the test is already unwinding from
a prior assertion, the drop-bomb firing would convert a useful test
failure message into a double-panic abort. Suppress in that case.

### Caller migration (mechanical, 7 sites)

All 7 existing callers do `timeout(.., task).await.unwrap().unwrap()` —
replace with `.verified().await`. The guard's internal timeout is 5 s
(was 1 s at the call sites); the slack doesn't matter because these are
mock in-memory round-trips that complete in milliseconds.

| File | Line | Migration |
|---|---|---|
| `rio-test-support/src/kube_mock.rs` | 143-146 | self-test → `guard.verified().await` |
| `rio-scheduler/src/lease/election.rs` | 507-510 | `renew_409_is_conflict` |
| `rio-scheduler/src/lease/election.rs` | 547-550 | `steal_409_is_conflict` |
| `rio-scheduler/src/lease/election.rs` | 577-580 | `create_on_404` |
| `rio-controller/src/reconcilers/workerpool/tests.rs` | 612-615 | `apply_patches_in_order` |
| `rio-controller/src/reconcilers/workerpool/tests.rs` | 679-682 | `apply_uses_server_side_apply` |
| `rio-controller/src/reconcilers/workerpool/tests.rs` | 722-725 | `cleanup_tolerates_missing_statefulset` |
| `rio-controller/src/reconcilers/workerpool/tests.rs` | (end) | `cleanup_polls_until_replicas_zero` |

Example diff (all follow the same shape):

```diff
--- a/rio-scheduler/src/lease/election.rs
+++ b/rio-scheduler/src/lease/election.rs
@@ -485,2 +485,2 @@
         let (client, verifier) = ApiServerVerifier::new();
-        let task = verifier.run(vec![
+        let guard = verifier.run(vec![
@@ -507,4 +507,1 @@
-        tokio::time::timeout(Duration::from_secs(1), task)
-            .await
-            .expect("verifier done")
-            .expect("no panic");
+        guard.verified().await;
```

---

## 7. New test — prove gateway tracks `first.sequence`

Once §1 + §2 + the `build.rs:217` fix are in, this test is writable.
It fails against current `build.rs:217` (hardcoded `0`), passes with
`active_build_ids.insert(build_id.clone(), first.sequence)`.

Add to `rio-gateway/tests/wire_opcodes/build.rs` alongside the other
reconnect tests:

```rust
/// Gateway must track first.sequence, not hardcode 0.
///
/// Mock's SubmitBuild sends Started(seq=1), then error. Gateway peeks
/// Started, inserts into active_build_ids. Reconnect reads that value
/// back as since_sequence. The mock records what it received.
///
/// With build.rs:217 hardcoding 0: watch_calls shows (build_id, 0).
/// With build.rs:217 fixed:        watch_calls shows (build_id, 1).
///
/// r[verify gw.reconnect.since-seq]
#[tokio::test]
async fn test_reconnect_sends_first_event_sequence_not_zero() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        // SubmitBuild: Started (auto-fill → seq=1), then transport error
        // at index 1 (before the second event, which is just a placeholder).
        scripted_events: Some(vec![
            ev(build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            })),
            ev(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec![],
            })),
        ]),
        error_after_n: Some((1, tonic::Code::Unavailable)),
        // WatchBuild: Completed at explicit seq=2 (> since_seq=1 → delivered).
        // Hand-construct instead of ev() so we control sequence directly.
        watch_scripted_events: Some(vec![types::BuildEvent {
            build_id: String::new(),
            sequence: 2,
            timestamp: None,
            event: Some(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec!["/nix/store/zzz".into()],
            })),
        }]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    let _frames = collect_stderr_frames(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "success after reconnect");

    // THE ASSERTION: gateway sent since_sequence=1 (Started's sequence),
    // not 0 (the hardcoded bug).
    let watches = h.scheduler.watch_calls.read().unwrap().clone();
    assert_eq!(watches.len(), 1, "exactly one WatchBuild call");
    assert_eq!(
        watches[0].1, 1,
        "since_sequence must be first.sequence (=1), not hardcoded 0. \
         This is the build.rs:217 bug: active_build_ids.insert(.., 0) \
         should be active_build_ids.insert(.., first.sequence)."
    );

    h.finish().await;
    Ok(())
}
```

The `r[verify gw.reconnect.since-seq]` annotation needs a matching spec
marker in `docs/src/components/gateway.md` (or wherever `gw.reconnect.*`
rules live) — one paragraph: "The gateway MUST track the sequence number
of the first peeked event and use it as the initial `since_sequence` for
reconnect, not hardcode 0."

---

## 8. Tests that break when mocks become strict

### BREAKS: `test_add_to_store_nar_passes_declared_hash`

**`rio-gateway/tests/wire_opcodes/opcodes_write.rs:53-87`**

Sends a NAR whose actual sha256 is `_actual_hash`, but declares
`[0xAB; 32]` on the wire. The gateway is a dumb pipe — forwards the
declared hash to the store via `chunk_nar_for_put`'s trailer. Current
mock accepts anything; test asserts `calls[0].nar_hash == [0xAB; 32]`
and passes.

With §4's hash verification, the mock computes `sha256(nar)`, compares
to trailer hash `[0xAB; 32]`, mismatch → `Status::invalid_argument`.
Gateway's `grpc_put_path` surfaces that as `STDERR_ERROR`. The test's
`drain_stderr_until_last` desyncs.

**Fix:** this test is testing against a mock that doesn't match prod.
The real store rejects this. Rewrite to assert the **correct** outcome:

```diff
-    let declared_hash = [0xABu8; 32]; // deliberately different from actual
+    // With strict mock (§4): a bogus declared hash is rejected by the
+    // store, just like rio-store does. The gateway is a dumb pipe —
+    // proving that means: send the RIGHT hash, assert it arrived
+    // unchanged. (A separate test below asserts the STDERR_ERROR path
+    // for a wrong hash.)
+    let (_nar, actual_hash) = make_nar(b"trust-test");
+    let declared_hash = actual_hash;  // correct — prove pass-through
```

Or split into two tests: one proving pass-through (correct hash), one
proving the mock rejects (wrong hash → `STDERR_ERROR`). The second is
the higher-value test — it's what `rio-store`'s
`test_put_path_hash_mismatch_cleans_up` asserts at the real-store
layer, but now caught earlier at the mock layer.

### BREAKS (after gateway fix): `test_build_paths_reconnect_on_transport_error`

**`rio-gateway/tests/wire_opcodes/build.rs:811-861`**

**Does not break from §1+§2 alone** — it breaks once `build.rs:217` is
also fixed. Trace:

| Step | Before gateway fix | After gateway fix |
|---|---|---|
| SubmitBuild Started (seq=1 after §2) | `active_build_ids.insert(.., 0)` | `active_build_ids.insert(.., 1)` |
| Reconnect `since_seq` | `0` | `1` |
| WatchBuild event (auto-fill → seq=1 after §1) | `1 > 0` → delivered | `1 <= 1` → **filtered** |
| Test outcome | passes | empty stream → EOF → retry → exhausted → **fail** |

**Fix:** set an explicit `sequence` on the `watch_scripted_events`
entry that's higher than any SubmitBuild sequence:

```diff
-        watch_scripted_events: Some(vec![ev(build_event::Event::Completed(
-            types::BuildCompleted {
-                output_paths: vec!["/nix/store/zzz-output".into()],
-            },
-        ))]),
+        // Explicit sequence=2: SubmitBuild's Started was seq=1, so
+        // gateway reconnects with since_seq=1. Mock's since-filter
+        // (§1) drops seq≤1; seq=2 survives.
+        watch_scripted_events: Some(vec![types::BuildEvent {
+            build_id: String::new(),
+            sequence: 2,
+            timestamp: None,
+            event: Some(build_event::Event::Completed(types::BuildCompleted {
+                output_paths: vec!["/nix/store/zzz-output".into()],
+            })),
+        }]),
```

### BREAKS (after gateway fix): `test_build_paths_eof_triggers_reconnect_not_error`

**`rio-gateway/tests/wire_opcodes/build.rs:98-148`**

Same breakage pattern as the transport-error test. Uses
`close_stream_early: true` (non-scripted SubmitBuild path), which after
§2 sends Started with hardcoded `sequence: 1` (`grpc.rs:456`). Gateway
(fixed) reconnects with `since_seq=1`. The `watch_scripted_events` has
one Completed event, auto-filled to `sequence=1`, filtered by §1's
`<= since` check.

Same fix: explicit `sequence: 2` on the watch event.

### Behavior change (still passes): `test_upload_output_nar_serialize_error`

**`rio-worker/src/upload.rs:616-638`**

ENOENT in `spawn_blocking` → channel drops without trailer. Current
mock returns `Ok(created=true)` (silently accepts the truncated stream).
Worker's `do_upload_streaming` at `upload.rs:299-300` does `put_result?;
dump_result` — `put_result` is `Ok`, so it falls through to
`dump_result`, which is `Err(NAR serialization failed)`. Retry, same
ENOENT, `UploadExhausted`.

With §4: mock returns `Err(stream closed without trailer)`. Now
`put_result` is `Err`, surfaces first (per the error-priority comment
at `upload.rs:296-298`). Same retry loop, same `UploadExhausted`.

**Test assertion is on `matches!(err, UploadError::UploadExhausted {..})` —
still passes.** Internal error path differs; if someone later adds a
`source.to_string().contains("NAR serialization")` assertion, it'd need
updating. Currently no such assertion.

### Behavior change (still passes): `test_add_multiple_to_store_truncated_nar`

**`rio-gateway/tests/wire_opcodes/opcodes_write.rs:161-197`**

Gateway's `grpc_put_path_streaming` detects the short read itself
(`read_exact` EOF at `handler/grpc.rs:105`) before the store's response
matters. Test asserts `STDERR_ERROR` with `"NAR read"` or `"eof"` in the
message — that error is from the **gateway**, not the mock. §4's
stricter mock changes what the mock would have said, but the gateway's
error wins the race.

**Still passes.** The comment at `opcodes_write.rs:159` ("MockStore is
lenient") becomes stale — delete it.

---

## Landing order

1. **§3 + §4 (MockStore strict) + `opcodes_write.rs:53` test fix** —
   atomic. §4 breaks the test; the fix ships with the breakage.
2. **§2 (seq → seq+1)** — no test breaks yet (gateway bug masks it).
3. **`build.rs:217` fix + §1 (honor since_seq) + two `build.rs`
   fixture edits + §7 new test** — atomic. §1 + gateway fix together
   break the two reconnect tests; their fixes + the new test ship
   together. §7 is the regression guard that proves the gateway fix.
4. **§5 (inproc transport helper + 3 test migrations)** — independent.
5. **§6 (drop-bomb + 7 mechanical migrations)** — independent.

Steps 4 and 5 can land in any order relative to 1-3.

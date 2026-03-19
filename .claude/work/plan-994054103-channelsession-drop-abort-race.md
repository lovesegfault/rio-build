# Plan 994054103: ChannelSession::Drop abort() races EOF-cancel — graceful-shutdown signal

Coordinator-surfaced followup from [P0331](plan-0331-eof-cancel-active-builds-ordering.md)'s investigation. P0331 T1-Q3 asks: "Does russh have a separate `channel_close` callback that ALSO runs the cancel loop? If so, Design B — [`session.rs:107`](../../rio-gateway/src/session.rs) EOF-cancel is belt-and-suspenders for between-opcode disconnects only." The answer is **Design-B-path-exists-but-broken:** [`server.rs:554-563`](../../rio-gateway/src/server.rs) `channel_close` exists, but it doesn't run a cancel loop — it does `self.sessions.remove(&channel)`, which drops `ChannelSession`, whose `Drop` impl at [`:227-236`](../../rio-gateway/src/server.rs) calls `self.proto_task.abort()`. Hard abort. No cleanup.

**The race:**

1. Client TCP-RST (kill -9 on client, NLB idle-timeout RST, cable pull)
2. russh fires `channel_eof` ([`:542`](../../rio-gateway/src/server.rs)) → `session.client_tx.take()` → the mpsc sender-half drops
3. `proto_task` (running [`session.rs:run_protocol`](../../rio-gateway/src/session.rs)) next tries to read an opcode from the mpsc receiver → `None` → Wire layer sees it as EOF → `UnexpectedEof` → [`session.rs:107`](../../rio-gateway/src/session.rs) arm → iterates `active_build_ids` → `CancelBuild` × N
4. **But concurrently:** russh fires `channel_close` ([`:554`](../../rio-gateway/src/server.rs)) → `self.sessions.remove(&channel)` → `ChannelSession::Drop` → `self.proto_task.abort()`

If step 4 wins (abort fires before step 3's cancel loop completes — or before it even starts), `CancelBuild` never reaches the scheduler. Worker slot leaked until `r[sched.backstop.timeout]`.

**The window is narrow but real:** `channel_eof` and `channel_close` arrive close-together on TCP RST (both derive from the same `SSH_MSG_CHANNEL_CLOSE` or TCP-level FIN/RST). The mpsc-receiver-reads-None → EOF-detection → cancel-loop path involves at least one scheduler yield (`tokio::select!` on the mpsc recv). If `channel_close` is already in russh's event queue, it can fire on that yield.

**Fix: graceful-shutdown signal, not hard abort.** `ChannelSession` holds a `CancellationToken` (or `oneshot::Sender<()>`); `Drop` fires it instead of `abort()`. `session.rs:run_protocol` takes the token as a param and selects on it alongside the opcode-read: `cancelled()` branch → run the same cancel-loop that the EOF arm runs. Both paths (EOF and graceful-signal) converge on `CancelBuild`.

This makes cancel-on-disconnect hold **regardless of which path wins the race.** P0331 still needs to resolve whether [`build.rs:462`](../../rio-gateway/src/handler/build.rs) should conditionally remove (Design-A fix) — that's the "is the map EMPTY when we iterate it" question. This plan fixes the "do we REACH the iteration at all" question. Orthogonal axes; both needed.

## Entry criteria

- [P0331](plan-0331-eof-cancel-active-builds-ordering.md) T1 resolved — confirms `channel_close` at [`server.rs:554`](../../rio-gateway/src/server.rs) is the Design-B path and it's abort-not-cancel. (If P0331 somehow finds a THIRD path that already gracefully cancels via `channel_close`, this plan is OBE — unlikely given `:227-230` is a plain `abort()` with no signal.)

## Tasks

### T1 — `fix(gateway):` CancellationToken in ChannelSession, fire in Drop

MODIFY [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) at the `ChannelSession` struct (`:218`) and `Drop` impl (`:227`).

Add a field:
```rust
struct ChannelSession {
    client_tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    proto_task: tokio::task::JoinHandle<()>,
    response_task: tokio::task::JoinHandle<()>,
    /// Fired in Drop to let proto_task run its cancel-on-disconnect
    /// loop before exiting. Replaces the hard abort() that raced the
    /// EOF-detection path (channel_close → Drop → abort could fire
    /// before session.rs saw UnexpectedEof from the dropped mpsc).
    shutdown: tokio_util::sync::CancellationToken,
}
```

Rewrite `Drop`:
```rust
impl Drop for ChannelSession {
    fn drop(&mut self) {
        // Signal graceful shutdown. proto_task's select picks this up
        // and runs the same CancelBuild loop as the EOF arm, THEN
        // returns naturally. The task's JoinHandle is dropped here
        // too, but dropping a JoinHandle doesn't abort the task —
        // only detaches it. The task will finish its cancel loop
        // (bounded by DEFAULT_GRPC_TIMEOUT × N builds) and then exit.
        self.shutdown.cancel();
        // response_task has nothing to clean up — it's a dumb pump.
        // Abort is still correct for it.
        self.response_task.abort();
        // Gauge decrement lives here so it fires on ALL drop paths.
        metrics::gauge!("rio_gateway_channels_active").decrement(1.0);
    }
}
```

**Why `CancellationToken` not `oneshot`:** the token is `Clone` (needed if `exec_request` wants to hand both the token and a `child_token()` to sub-tasks later) and `cancelled()` is `Future` + idempotent (select can poll it repeatedly). `oneshot::Receiver` is consume-once — works here but `CancellationToken` is the idiom. Already in the dep tree via `tokio-util` (check `Cargo.toml` at dispatch; if not, add it — `tokio-util = { version = "0.7", features = ["rt"] }`).

**The dropped JoinHandle doesn't leak:** dropping a `JoinHandle` detaches the task but does NOT abort it. The task runs to completion. `run_protocol`'s cancel loop is bounded (`DEFAULT_GRPC_TIMEOUT` per `CancelBuild` × `active_build_ids.len()`), then returns. Task deallocated. No unbounded growth.

**Spawn site:** grep `proto_task =` in [`server.rs`](../../rio-gateway/src/server.rs) (around `:448`) — create the token before spawning, clone into the closure, store the original in `ChannelSession`. Pass the clone to `run_protocol` as a new param.

### T2 — `fix(gateway):` session.rs select on shutdown token, converge on cancel-loop

MODIFY [`rio-gateway/src/session.rs`](../../rio-gateway/src/session.rs). Add a `shutdown: CancellationToken` param to `run_protocol`. The opcode-read loop is already a `tokio::select!` (the idle-timeout arm at `:95` proves this — `Ok(Err(wire::WireError::Io(e)))` pattern means the opcode-read is wrapped). Add a branch:

```rust
// In the opcode-read select:
_ = shutdown.cancelled() => {
    debug!("channel closed (graceful shutdown signal)");
    // Same cancel loop as the UnexpectedEof arm below. Extracted
    // to a local async fn or inlined — pick based on diff size.
    cancel_active_builds(&mut ctx, "channel_close").await;
    return Ok(());
}
Ok(Err(wire::WireError::Io(e))) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
    debug!("client disconnected (EOF)");
    cancel_active_builds(&mut ctx, "client_disconnect").await;
    return Ok(());
}
```

Extract the [`:110-134`](../../rio-gateway/src/session.rs) cancel loop into `async fn cancel_active_builds(ctx: &mut SessionContext, reason: &str)`. The `reason` string distinguishes the two paths in `CancelBuildRequest.reason` for debugging ("why was this build cancelled?" — `client_disconnect` vs `channel_close` answers "did the client send SSH-EOF cleanly or did the TCP connection die?").

**select ordering:** `shutdown.cancelled()` before the opcode-read. tokio biases toward the first ready branch by default (unless `biased;` is set). Doesn't matter for correctness (both branches do the same thing) but putting shutdown first means: if BOTH are ready on the same poll, we get `reason="channel_close"` — slightly more accurate since it was the russh-level signal that actually triggered the drop.

### T3 — `test(gateway):` regression test — abort-race does not leak build

[P0331](plan-0331-eof-cancel-active-builds-ordering.md) T3's test (`test_mid_opcode_disconnect_cancels_build`) covers the mid-opcode → EOF-detection path. This plan needs a test for the **channel_close path specifically** — proving that `ChannelSession::Drop` triggers cancel via the token, not via the mpsc-EOF route.

The distinguishing setup: **close the SSH channel without EOF-ing the mpsc first.** In P0331's test, the mock client does `client_writer.shutdown()` — the FIN on the client side → EOF on the reader → mpsc sender drops → `session.rs` sees EOF. Here instead: directly drop the `ChannelSession` (or the `HashMap` entry) while the mpsc sender is still held by something. Hard to arrange from the outside — this is white-box.

**Option A — unit-level:** test `run_protocol` directly. Create a `CancellationToken`, spawn `run_protocol` with a mock scheduler and a `CancelBuild` counter, insert a build_id into `ctx.active_build_ids`, then `token.cancel()`. Assert mock scheduler received `CancelBuild`. No SSH, no russh, no mpsc race — just "does the select branch work".

```rust
// rio-gateway/tests/functional/session.rs (or wherever run_protocol tests live)
#[tokio::test]
async fn test_shutdown_signal_cancels_active_builds() {
    let token = CancellationToken::new();
    let mock_sched = MockScheduler::with_cancel_capture();
    let mut ctx = SessionContext { /* ... */, active_build_ids: HashMap::new() };
    ctx.active_build_ids.insert("test-build-id".into(), 0);

    let task = tokio::spawn(run_protocol(
        /* mock reader that blocks forever */,
        /* mock writer */,
        ctx,
        token.child_token(),
    ));

    // Precondition self-check: task is alive, not yet cancelled.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!task.is_finished());
    assert_eq!(mock_sched.received_cancels().len(), 0);

    token.cancel();
    // Task should finish within GRPC_TIMEOUT + slack.
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
    assert_eq!(mock_sched.received_cancels(), &["test-build-id"]);
    // r[verify gw.conn.cancel-on-disconnect]
}
```

**Option B — integration-level:** extend P0331 T3's test to also cover the `channel_close` path by directly invoking the russh handler's `channel_close` method. Harder to set up; covers the full chain including `Drop`. Defer to Option A unless P0331's test infrastructure makes Option B trivial.

**Mutation check:** revert T1's `Drop` to `self.proto_task.abort()` → re-run this test → **must fail** (task aborted before the cancel fires; `received_cancels` stays empty). This proves the test is catching the actual race fix, not vacuously green.

## Exit criteria

- `/nbr .#ci` green
- `grep 'CancellationToken\|shutdown.cancel()' rio-gateway/src/server.rs` → ≥2 hits (field + fire-in-Drop)
- `grep 'proto_task.abort()' rio-gateway/src/server.rs` → 0 hits (replaced by token.cancel(); `response_task.abort()` stays)
- `grep 'shutdown.cancelled()\|cancel_active_builds' rio-gateway/src/session.rs` → ≥2 hits (select branch + extracted helper or inlined equivalent)
- `cargo nextest run -p rio-gateway test_shutdown_signal_cancels_active_builds` → pass
- **Mutation check:** revert T1's Drop to `abort()`, re-run T3's test → **fails** (assert on `received_cancels` doesn't see the build_id; OR the task panics with `Aborted` instead of `Ok(())`). Proves the test pins the fix.
- `grep 'r\[verify gw.conn.cancel-on-disconnect\]' rio-gateway/tests/` → ≥1 hit (T3's test carries the annotation — this plan's test is the `verify` for the marker, regardless of whether P0331 or this plan adds the `impl` annotation)

## Tracey

References existing markers:
- `r[gw.conn.session-error-visible]` — the cancel loop at [`session.rs:110-134`](../../rio-gateway/src/session.rs) sits next to this marker's impl at `:138`; T2's extracted `cancel_active_builds` is adjacent
- `r[sched.backstop.timeout]` — what fires if cancel DOESN'T reach the scheduler (the leak this plan closes)

Adds new marker to component spec:
- `r[gw.conn.cancel-on-disconnect]` → [`docs/src/components/gateway.md`](../../docs/src/components/gateway.md) after `:525` (see Spec additions below)

**Marker ownership between this plan and P0331:** P0331's Spec-additions section says the marker is added ONLY if Design-B resolves — but P0331's T2 "Design-B-broken" branch routes through coordinator to a fresh followup, which is **this plan**. So this plan adds the marker unconditionally. P0331's conditional-add is moot: Design-A (both fixes land) → this plan's marker covers both paths; Design-B-broken (this plan IS the fix) → this plan's marker. Either way, `r[gw.conn.cancel-on-disconnect]` lands here.

## Spec additions

Add to [`docs/src/components/gateway.md`](../../docs/src/components/gateway.md) after `r[gw.conn.session-error-visible]` at `:525`:

```
r[gw.conn.cancel-on-disconnect]
The gateway MUST send `CancelBuild` to the scheduler for every build in
`active_build_ids` when an SSH channel drops, via ALL disconnect shapes:
(1) clean EOF between opcodes — `session.rs` opcode-read returns
`UnexpectedEof`, iterates the map; (2) russh `channel_close` callback —
`ChannelSession::Drop` fires a graceful-shutdown signal that `session.rs`
selects on and runs the same cancel loop. Both paths MUST complete the
cancel loop before the protocol task exits; hard `abort()` on the task
handle defeats this (the original pre-P994054103 `Drop` impl). Builds not
cancelled leak a worker slot until `r[sched.backstop.timeout]`.
```

The `P994054103` reference gets string-replaced to the real number at merge.

## Files

```json files
[
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T1: +CancellationToken field at :218; Drop :227-236 abort→cancel(); spawn site ~:448 create+clone token"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "T2: +shutdown param to run_protocol; select branch on shutdown.cancelled(); extract :110-134 loop → cancel_active_builds(ctx, reason)"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T2: add r[gw.conn.cancel-on-disconnect] marker after :525"},
  {"path": "rio-gateway/tests/functional/session.rs", "action": "MODIFY", "note": "T3: test_shutdown_signal_cancels_active_builds — token.cancel() → mock scheduler received CancelBuild. NEW if no session.rs test file exists — grep at dispatch."},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "T3 MAYBE: MockScheduler::received_cancels capture — shared with P0331-T3; whichever lands first adds it"}
]
```

```
rio-gateway/src/
├── server.rs                       # T1: +token field, Drop abort→cancel
└── session.rs                      # T2: +shutdown param, select branch, extract helper
docs/src/components/gateway.md      # T2: new r[gw.conn.cancel-on-disconnect] marker
rio-gateway/tests/functional/
└── session.rs                      # T3: shutdown-signal regression test
rio-test-support/src/grpc.rs        # T3: cancel-capture (shared w/ P0331)
```

## Dependencies

```json deps
{"deps": [331], "soft_deps": [240], "note": "HARD dep P0331: T1-Q3 confirms channel_close at server.rs:554 is the Design-B path and it's abort-not-cancel. P0331's T2 Design-B-broken branch says 'route through coordinator for a fresh followup if the fix is non-trivial' — this IS that followup. If P0331 somehow finds a different graceful path, this is OBE (unlikely — :227-230 is unambiguous abort()). Also: P0331-T3's test_mid_opcode_disconnect_cancels_build covers the EOF-detection path; T3 here covers the channel_close→Drop→token path. Both tests need MockScheduler cancel-capture — shared in rio-test-support/src/grpc.rs, whoever lands first adds it. Soft-dep P0240 (DONE): scheduling.nix cancel-timing is the e2e VM version; unit-ish mock-scheduler test here is sufficient and doesn't need the VM. discovered_from=coordinator (surfaced during P0331 preliminary investigation)."}
```

**Depends on:** [P0331](plan-0331-eof-cancel-active-builds-ordering.md) — T1 investigation confirms the abort-race shape. This is P0331's "Design-B-broken → route to followup" branch made concrete.

**Conflicts with:** [`server.rs`](../../rio-gateway/src/server.rs) not in top-30 (last touched lightly by keepalive/nodelay plans). [`session.rs`](../../rio-gateway/src/session.rs) — P0331 T2-B adds a scope-clarifying comment at `:107`; T2 here extracts `:110-134` into a helper. P0331's comment lands on the EOF arm; T2's helper is called FROM the EOF arm. Sequence P0331 first (it's the dep anyway); rebase picks up the comment. [`gateway.md`](../../docs/src/components/gateway.md) count=26 — T2 adds one marker at `:525+`; P0331's Spec-additions conditionally adds the SAME marker at the same spot. **Coordination:** this plan adds the marker unconditionally; P0331 should NOT add it (update P0331's Spec-additions at dispatch to point here — "marker owned by P994054103"). [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) count=17 — shared `received_cancels` capture with P0331-T3. [P0330](plan-0330-test-recorder-extraction-test-support.md) also touches rio-test-support but at `metrics.rs`, not `grpc.rs`.

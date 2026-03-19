# Plan 993539802: EOF-cancel ordering — does premature `active_build_ids.remove` leak builds?

Correctness investigation (coordinator-surfaced, [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) pattern). [`session.rs:107-134`](../../rio-gateway/src/session.rs) implements "CancelBuild on disconnect" — the EOF arm of the opcode-read loop iterates `ctx.active_build_ids.keys()` and fires `CancelBuild` for each. This is **stated design intent** at [`gateway.md:717`](../../docs/src/components/gateway.md): "session.rs — Per-SSH-channel protocol session loop (`run_protocol`), CancelBuild on disconnect."

But [`handler/build.rs:462`](../../rio-gateway/src/handler/build.rs) removes the build_id from `active_build_ids` **unconditionally**, regardless of whether `outcome` was success, failure, or **stream error**. If `outcome` is `Err(e)` (stream failure mid-build), the removal at `:462` fires BEFORE control returns to `session.rs`. When `session.rs` tries to read the next opcode and gets EOF (client gone), the EOF-cancel loop at `:110` iterates an **empty map**.

**The race scenario:**
1. Client sends `wopBuildPathsWithResults`
2. Handler inserts `build_id` → `active_build_ids` at [`:282`](../../rio-gateway/src/handler/build.rs) or [`:317`](../../rio-gateway/src/handler/build.rs)
3. Client disconnects (TCP RST, SSH channel close, kill -9 on the client)
4. Handler's event-stream-forwarding writes `STDERR_NEXT` → `EPIPE` → `outcome = Err(...)`
5. **[`:462`](../../rio-gateway/src/handler/build.rs): `active_build_ids.remove(&build_id)`** ← map is now empty
6. Handler returns `Err` (wrapped in `BuildResult::failure` via `:474-477`) OR propagates `?` — **check which at dispatch**
7. `session.rs` reads next opcode → `UnexpectedEof` → `:107` arm fires
8. `:110` iterates empty `active_build_ids` → **no `CancelBuild` sent**
9. Scheduler keeps the build running until completion or backstop timeout

**Resource leak:** abandoned builds consume a worker slot until they finish naturally or hit `r[sched.backstop.timeout]`. For a 6h nixpkgs build, that's a 6h slot leak per dropped client.

**BUT — this may be BY-DESIGN.** Two possibilities:
- **Design A (bug):** the EOF-cancel was supposed to catch exactly this case; `:462` defeats it. Fix: don't remove on `Err`, OR move removal to session.rs post-dispatch.
- **Design B (intentional):** `:462` removal is the "clean error path" — the handler already wrapped the error into a `BuildResult::failure` at `:474-477`, which *writes a result to the client*. If the write succeeds, the session continues normally (no EOF). If the write fails (client gone), THAT write-failure propagates to session.rs as a **different** error, not clean EOF. The `:107` EOF arm is only reached for disconnects **between opcodes**, not mid-opcode. Under Design B, mid-opcode disconnects should trigger cancel via a **separate** path (session-error handler? russh `channel_eof`?) — **which may not exist.**

## Tasks

### T1 — `test(gateway):` trace the actual error-propagation path

**This is the investigation.** Read, don't write. Three questions:

**Q1:** At [`build.rs:474-477`](../../rio-gateway/src/handler/build.rs), `Err(e) => Ok(BuildResult::failure(...))` converts stream error to `Ok`. Does the **caller** (dispatch in `handler/mod.rs` or `session.rs`) then attempt to write this `BuildResult` back over the wire? Trace `handle_build_paths_with_results`'s return path — grep for where its `Ok(BuildResult)` is written.

**Q2:** If that write fails (client gone), what error type reaches `session.rs`'s opcode loop? Is it caught by `:107` `UnexpectedEof`, by `:138` generic `Err(e)`, or by russh's channel-level handling before `session.rs` ever sees it?

**Q3:** Does russh have a separate `channel_eof` / `channel_close` callback that fires on client disconnect **independently** of the data-stream EOF? Grep [`server.rs`](../../rio-gateway/src/server.rs) for `russh::server::Handler` impl methods — if there's a `channel_close` that ALSO runs the cancel loop, this is Design B and the `:107` arm is redundant-but-harmless belt-and-suspenders for the between-opcodes case only.

**Record findings inline** — doc-comment at [`build.rs:462`](../../rio-gateway/src/handler/build.rs) explaining whichever design holds:

```rust
// [Design A, if T1 confirms bug:]
// BUG(P993539802): removal here defeats session.rs:107 EOF-cancel
// for mid-opcode disconnects. See T2 for fix.
//
// [Design B, if T1 confirms intentional:]
// Removal here is correct: mid-opcode disconnects propagate as
// write-error (see Q1 trace) → session.rs:138 generic-Err arm,
// NOT :107 EOF. The :107 EOF-cancel handles between-opcode
// disconnects only. Mid-opcode cancel comes from <wherever T1-Q3 finds it>.
active_build_ids.remove(&build_id);
```

### T2 — `fix(gateway):` route by T1 outcome

**T1 answer: Design A (bug) — no separate mid-opcode cancel path exists:**

The `:462` removal must be conditional. Two fix shapes:

**Fix-1 (minimal):** only remove if `outcome.is_ok()`:
```rust
// Remove only on terminal outcome. If the event-stream errored out,
// leave the build_id in the map so session.rs:107 can cancel it
// when it reads EOF on the next opcode-read attempt.
if outcome.is_ok() {
    active_build_ids.remove(&build_id);
}
```
**Hazard:** `outcome` can be `Ok(BuildEventOutcome::Failed{...})` or `Ok(BuildEventOutcome::Cancelled{...})` — those ARE terminal, the scheduler already knows, cancel is redundant-but-harmless. So `is_ok()` is correct, not `matches!(outcome, Ok(Completed))`.

**Fix-2 (structural):** move the removal to `session.rs`'s post-dispatch point, AFTER the opcode handler returns. The handler never removes; session.rs owns `active_build_ids` lifetime fully. Cleaner, but larger diff and may fight the `&mut HashMap` borrowing in the dispatch call.

Prefer Fix-1 unless Fix-2's diff is comparably small.

**T1 answer: Design B — a separate cancel path exists (russh channel-close callback, or session-error handler calls cancel):**

No code fix. MODIFY [`session.rs:107`](../../rio-gateway/src/session.rs) — add a comment clarifying scope:

```rust
// EOF-cancel for between-opcode disconnects only. Mid-opcode
// disconnects (client drops during wopBuildPathsWithResults) are
// handled by <the path T1-Q3 found> — handler/build.rs:462 has
// already removed the build_id by the time we get here, which is
// correct because <reason from T1 trace>.
```

Also MODIFY [`gateway.md`](../../docs/src/components/gateway.md) — upgrade the file-manifest mention at `:717` to a proper `r[gw.conn.cancel-on-disconnect]` marker that states the TWO-path design explicitly (see Spec additions below).

**T1 answer: Design B claimed but the separate path is BROKEN:**

Hybrid — fix whichever path is the design-intended one. This is the most work; route through coordinator for a fresh followup if the fix is non-trivial.

### T3 — `test(gateway):` regression test for whichever design holds

Integration test in `rio-gateway/tests/functional/` (or wherever build-opcode tests live — grep for `handle_build_paths_with_results` test callers).

**For Design A fix:**
```rust
// Arrange: mock scheduler that accepts SubmitBuild, keeps WatchBuild
// stream open (never sends terminal event). Mock scheduler tracks
// CancelBuild calls.
// Act: client sends wopBuildPathsWithResults, then drops the socket
// mid-stream (client_writer.shutdown() + drop(client_reader)).
// Assert: mock scheduler received CancelBuild with the right build_id
// within a short timeout.
#[tokio::test]
async fn test_mid_opcode_disconnect_cancels_build() { ... }
```

**For Design B:** same test structure, but the assertion is that cancel arrives via whichever path T1-Q3 identified. The test still proves the chain — it's just documenting a different mechanism.

The mock scheduler in `rio-test-support/src/grpc.rs` may already have a `CancelBuild` handler — check. If not, add a `received_cancels: Arc<Mutex<Vec<String>>>` capture.

## Exit criteria

- **T1 has a recorded answer** — one of: Design A (bug, no separate path), Design B (intentional, separate path at `<location>`), Design-B-but-broken
- `grep 'P993539802\|active_build_ids.remove' rio-gateway/src/handler/build.rs` → ≥1 hit in a comment at `:462` (T1 doc-comment landed, references this plan or explains the design)
- If Design A: `cargo nextest run -p rio-gateway test_mid_opcode_disconnect_cancels_build` → pass (T3 test exists AND passes post-fix)
- If Design A: **mutation check** — revert T2's `if outcome.is_ok()` guard, re-run T3 test → **must fail** (proves the test catches the regression, not vacuously green)
- If Design B: `grep 'r\[gw.conn.cancel-on-disconnect\]' docs/src/components/gateway.md` → 1 hit (spec marker added)
- If Design B: `grep 'between-opcode\|mid-opcode' rio-gateway/src/session.rs` → ≥1 hit (T2 scope-clarifying comment)
- `/nbr .#ci` green (Design B route may be comment-only `.rs` + docs → clause-4 weaker than `.claude/`-only but rust-tier still runs; Design A is a real fix, full CI)

## Tracey

References existing markers:
- `r[gw.conn.session-error-visible]` — T1-Q2 cross-references (the session-error handling at [`session.rs:138`](../../rio-gateway/src/session.rs) that Design B would route through)

**T2 Design-B path adds a new marker** (see Spec additions below). Design-A path does NOT — the behavior was always intended, it's being fixed to match existing intent ("CancelBuild on disconnect" at [`gateway.md:717`](../../docs/src/components/gateway.md) was prose-only, never had a marker, and Design-A fix just makes it work without changing the spec contract).

## Spec additions

**ONLY if T1 resolves Design B** — add to [`docs/src/components/gateway.md`](../../docs/src/components/gateway.md) after `r[gw.conn.session-error-visible]` at `:525`:

```
r[gw.conn.cancel-on-disconnect]
The gateway MUST send `CancelBuild` to the scheduler for every build
that was active when an SSH session drops. Two paths: (1) clean EOF
between opcodes — `session.rs` opcode-read loop's `UnexpectedEof`
arm iterates `active_build_ids` and cancels each; (2) mid-opcode
disconnect (client drops during `wopBuildPathsWithResults`) —
handled by `<path-from-T1-Q3>`. Builds NOT cancelled on disconnect
leak a worker slot until backstop timeout (`r[sched.backstop.timeout]`).
```

If T1 resolves Design A, the marker is optional — the behavior is unified (one cancel path, handles both cases) and the existing `:717` file-manifest prose suffices. Add only if the implementer judges a formal marker useful for future test-coverage.

## Files

```json files
[
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T1: doc-comment at :462 explaining Design A or B; T2-A: guard removal with if outcome.is_ok()"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "T2-B: scope-clarifying comment at :107 (between-opcode vs mid-opcode)"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T2-B ONLY: add r[gw.conn.cancel-on-disconnect] marker after :525"},
  {"path": "rio-gateway/tests/functional/build.rs", "action": "MODIFY", "note": "T3: test_mid_opcode_disconnect_cancels_build — mock scheduler tracks CancelBuild; client drops mid-stream"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "T3 MAYBE: add received_cancels capture to mock scheduler if not already present (check at dispatch)"}
]
```

```
rio-gateway/src/
├── handler/build.rs            # T1 doc-comment; T2-A conditional remove
└── session.rs                  # T2-B scope comment
docs/src/components/gateway.md  # T2-B marker (conditional)
rio-gateway/tests/functional/
└── build.rs                    # T3 regression test
rio-test-support/src/grpc.rs    # T3 mock-scheduler cancel capture (maybe)
```

## Dependencies

```json deps
{"deps": [240], "soft_deps": [993539801], "note": "Followup Deps column says P0240 — P0240 is 'VM Section F+J scheduling (cancel timing + 50drv load)', UNIMPL, touches nix/tests (scheduling.nix cancel-timing assertions). Not a CODE dep — the code at build.rs:462 and session.rs:107 exists independently. But P0240's cancel-timing VM test WOULD be the natural place for an end-to-end version of T3 (drop client mid-build in a real VM, check scheduler state). Treating as hard-dep per the followup's Deps field; if P0240 hasn't landed by dispatch, T3 goes in rio-gateway/tests/functional/ as a unit-ish test with mock scheduler (sufficient). Soft-dep P993539801 (this docs run): T3's mock-scheduler extension in rio-test-support/src/grpc.rs — if 993539801 lands first, both plans touch rio-test-support; different modules (grpc.rs vs metrics.rs), no conflict. discovered_from=coordinator (no plan number — surfaced during session review)."}
```

**Depends on:** [P0240](plan-0240-vm-section-fj-scheduling.md) per followup Deps column — `nix/tests/scenarios/scheduling.nix` cancel-timing VM test is where an end-to-end version of T3 would live. Not a code dependency; T3 can proceed in `rio-gateway/tests/functional/` with a mock scheduler if P0240 hasn't landed.
**Conflicts with:** [`handler/build.rs`](../../rio-gateway/src/handler/build.rs) **count=25** — hot file. Grep other UNIMPL plans for `:462` specifically: none. T1's doc-comment is additive. T2-A's conditional-remove is a 2-line wrap. [`session.rs`](../../rio-gateway/src/session.rs) not in top-30. [`gateway.md`](../../docs/src/components/gateway.md) count=25 — T2-B adds one marker paragraph after `:525`; additive, non-overlapping with [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md) T2-A which edits near `r[gw.opcode.set-options.propagation]` at `:61`.

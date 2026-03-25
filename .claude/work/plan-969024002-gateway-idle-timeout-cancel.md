# Plan 969024002: Gateway idle-timeout exit path misses cancel_active_builds

Bughunter correctness finding at [`rio-gateway/src/session.rs:210-223`](../../rio-gateway/src/session.rs). `run_protocol()` has four exit paths; three correctly call `cancel_active_builds()` before returning, but the idle-timeout path at `:210-223` returns `Ok(())` bare. When `OPCODE_IDLE_TIMEOUT` (600s) fires after a client submitted a build and went idle, the session exits cleanly but the build leaks until the scheduler's 2h backstop. Burns worker capacity on builds nobody is listening for.

The other three exit paths already do it right:
- `:201-204` graceful shutdown — calls `cancel_active_builds`
- `:225-232` EOF between opcodes — calls `cancel_active_builds`
- `:243-254` handler error — calls `cancel_active_builds`

Simple missed-call bug: add the same call before `return Ok(())` at `:223`. The spec marker `r[gw.conn.cancel-on-disconnect]` at [`gateway.md:532`](../../docs/src/components/gateway.md) documents two disconnect shapes (EOF + `channel_close`) but doesn't mention idle-timeout — the spec should be extended to cover this third shape.

## Tasks

### T1 — `fix(gateway):` session.rs idle-timeout — add cancel_active_builds before return

At [`session.rs:223`](../../rio-gateway/src/session.rs), before `return Ok(())`:

```rust
cancel_active_builds(&mut ctx, "idle_timeout").await;
return Ok(());
```

Match the reason-string convention used by the other three exit paths (check `:201`, `:225`, `:243` for the exact parameter shape — may be an enum variant rather than a string).

### T2 — `docs(gateway):` extend cancel-on-disconnect spec to cover idle-timeout

At [`gateway.md:532-541`](../../docs/src/components/gateway.md), the `r[gw.conn.cancel-on-disconnect]` marker text lists two disconnect shapes. Extend to three: "(1) clean EOF … (2) russh `channel_close` … (3) `OPCODE_IDLE_TIMEOUT` expiry — `session.rs` idle-timer fires, runs the same cancel loop before returning." Run `tracey bump` if the marker text change is semantic enough to warrant a version bump (it is — adds a third MUST-cancel shape).

### T3 — `test(gateway):` regression test for idle-timeout cancel

Add to the gateway session tests (location: check where `cancel_active_builds` is already tested — likely [`rio-gateway/src/session.rs`](../../rio-gateway/src/session.rs) inline `#[cfg(test)]` or a sibling `tests/` module):

- `test_idle_timeout_cancels_active_builds` — submit a build, let mock clock exceed `OPCODE_IDLE_TIMEOUT`, assert `CancelBuild` was sent to the mock scheduler. Test MUST fail on pre-fix code (no CancelBuild sent).

## Exit criteria

- `/nbr .#ci` green
- `cargo nextest run -p rio-gateway test_idle_timeout_cancels_active_builds` → passed
- `grep -c 'cancel_active_builds' rio-gateway/src/session.rs` ≥ 4 (all four exit paths now cancel)
- `grep 'OPCODE_IDLE_TIMEOUT\|idle-timer' docs/src/components/gateway.md` → ≥1 hit in the `r[gw.conn.cancel-on-disconnect]` paragraph

## Tracey

References existing markers:
- `r[gw.conn.cancel-on-disconnect]` — T1 implements the third disconnect shape; T2 extends the spec text; T3 verifies

## Files

```json files
[
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "T1: :223 add cancel_active_builds before idle-timeout return. T3: +regression test"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T2: :532-541 extend cancel-on-disconnect to 3 shapes (+ idle-timeout). tracey bump candidate"}
]
```

```
rio-gateway/src/
└── session.rs             # T1: idle-timeout cancel; T3: regression test
docs/src/components/
└── gateway.md             # T2: spec extension
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "No hard deps — session.rs exit-path structure is stable since phase-2. gateway.md is moderate-traffic but T2 edits a single paragraph."}
```

**Depends on:** none — the four exit paths at `session.rs:201-254` have been stable since the SSH handler rework.
**Conflicts with:** [`gateway.md`](../../docs/src/components/gateway.md) is moderate-traffic — T2 edits `:532-541` only. `session.rs` is not in collisions top-30. Low collision risk.

# Plan 0215: worker — `maxSilentTime` enforcement

Wave 2. Rio-side enforcement of `maxSilentTime` (forwarded from client `--option max-silent-time`). The `max_silent_time` parameter already threads through to [`stderr_loop.rs:49`](../../rio-worker/src/executor/daemon/stderr_loop.rs); today it's dead. This plan adds a `select!` arm that fires when the build has been silent for `max_silent_time` seconds → `SilenceTimeout` outcome → caller does `cgroup.kill()` → `TimedOut` status.

The local nix-daemon MAY also enforce `maxSilentTime` itself (forwarded via `client_set_options` at [`daemon/mod.rs:80`](../../rio-worker/src/executor/daemon/mod.rs)) — rio-side is the **authoritative backstop** ensuring we get a correct `TimedOut` result regardless of what the daemon does.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (marker `r[worker.silence.timeout-kill]` exists in `worker.md`)

## Tasks

### T1 — `feat(worker):` `select!` silence arm in stderr loop

At [`rio-worker/src/executor/daemon/stderr_loop.rs`](../../rio-worker/src/executor/daemon/stderr_loop.rs):

```rust
// r[impl worker.silence.timeout-kill]
let mut last_output = Instant::now();

loop {
    tokio::select! {
        // Existing arms: STDERR_NEXT/READ/WRITE etc.
        // On each output-producing arm: last_output = Instant::now();

        // New arm — silence deadline.
        _ = tokio::time::sleep_until((last_output + Duration::from_secs(max_silent_time)).into()),
            if max_silent_time > 0
        => {
            return Ok(StderrLoopOutcome::SilenceTimeout);
        }
    }
}
```

Reset `last_output` on every `STDERR_NEXT` / `STDERR_READ` / `STDERR_WRITE` (the output-producing message types). Verify the actual return-type enum name in the file — may differ from `StderrLoopOutcome`.

### T2 — `feat(worker):` handle `SilenceTimeout` in caller

At [`rio-worker/src/executor/daemon/mod.rs`](../../rio-worker/src/executor/daemon/mod.rs), the caller of `stderr_loop`:

```rust
StderrLoopOutcome::SilenceTimeout => {
    // cgroup.kill() at cgroup.rs:180 — verify CgroupHandle is in scope here
    cgroup.kill();
    return BuildResult {
        status: BuildResultStatus::TimedOut,
        error_msg: format!("no output for {}s (maxSilentTime)", max_silent_time),
        // ... other fields ...
    };
}
```

Note at this site: local nix-daemon MAY already enforce `maxSilentTime` (forwarded via `client_set_options` at `:80`) — rio-side is the authoritative backstop.

### T3 — `test(worker):` paused-time silence timeout

With `tokio::time::pause` + mock stderr source: source sends one `STDERR_NEXT` then nothing. Advance time past `max_silent_time` → assert `SilenceTimeout`.

Marker: `// r[verify worker.silence.timeout-kill]`

### T4 — `test(vm):` scheduling.nix — silence kills, not wall-clock

Extend [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) (TAIL append): `buildCommand = "echo start; sleep 60"` + `--option max-silent-time 5` → assert `TimedOut` within ~10s wall-clock (proving the kill was at ~5s silence, NOT 60s overall).

### T5 — `docs(errors):` flip row 96 to "Implemented"

At [`docs/src/errors.md:96`](../../docs/src/errors.md) (the maxSilentTime row): "Not implemented" → "Implemented."

## Exit criteria

- `/nbr .#ci` green
- VM test: kill at ~5s silence, NOT 60s (proves silence detection, not just overall timeout)
- Paused-time unit test: `SilenceTimeout` outcome after advancing past `max_silent_time`

## Tracey

References existing markers:
- `r[worker.silence.timeout-kill]` — T1 implements (stderr_loop.rs select arm); T3+T4 verify

## Files

```json files
[
  {"path": "rio-worker/src/executor/daemon/stderr_loop.rs", "action": "MODIFY", "note": "T1: last_output + select! silence arm; reset on STDERR_NEXT/READ/WRITE; r[impl worker.silence.timeout-kill]"},
  {"path": "rio-worker/src/executor/daemon/mod.rs", "action": "MODIFY", "note": "T2: handle SilenceTimeout → cgroup.kill() → TimedOut result"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T4: maxSilentTime subtest (TAIL append): echo+sleep60 + max-silent-time 5 → TimedOut ~10s"},
  {"path": "docs/src/errors.md", "action": "MODIFY", "note": "T5: row 96 → Implemented"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "note authoritative-backstop semantics under r[worker.silence.timeout-kill]"}
]
```

```
rio-worker/src/executor/daemon/
├── stderr_loop.rs                 # T1: select! arm + r[impl]
└── mod.rs                         # T2: SilenceTimeout → cgroup.kill
nix/tests/scenarios/scheduling.nix # T4: VM test
docs/src/errors.md                 # T5: row 96
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [214], "note": "stderr_loop.rs disjoint. scheduling.nix+errors.md collide w/ P0214 (both TAIL-append, adjacent rows — low risk)."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — marker `r[worker.silence.timeout-kill]` must exist.

**Conflicts with:**
- `stderr_loop.rs` — single-writer in 4b.
- [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — **P0214** also TAIL-appends a subtest. Serialize merge if same-day, low risk.
- [`errors.md`](../../docs/src/errors.md) — P0214 touches row 97, this touches row 96. Adjacent but distinct — auto-merge.

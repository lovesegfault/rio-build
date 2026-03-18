# Plan 0092: Resource tracking — CompletionReport fields + VmHWM + EMA write

## Design

**Non-adjacent cluster:** commits `f2770c4` (seq #7) and `632e933` (seq #23) with a 16-commit gap. The first commit was explicit proto preparation — its body reads "F2 wires the actual sampling; here we just add the proto field with explicit 0 meaning 'no signal'". Three files, no independent narrative. The second commit is the implementation that reads `/proc/{pid}/status` `VmHWM` and writes the EMA. They're one feature (`build_history.ema_peak_memory_bytes` population) split by execution order; the gap is how the phase was worked, not how the feature is structured.

### Proto fields (prep)

For size-class routing (P0093): a derivation that fits the "small" duration class but historically OOM-kills on small workers gets bumped to "medium" based on `ema_peak_memory_bytes`. The EMA column exists (migration 006, P0082); this is the wire protocol to populate it.

`peak_memory_bytes`: `/proc/{pid}/status` `VmHWM` at build end. `VmHWM` is kernel-tracked process-lifetime peak — no polling needed, one read. Explicit `0` means "no signal" (scheduler treats 0 as missing data, doesn't drag the EMA toward zero). `output_size_bytes`: sum of uploaded NAR sizes. Not used for routing yet — informational for dashboards/capacity planning. TODO(phase3a) tagged for `peak_cpu_cores`: needs polling during the build (no `CpuHWM` equivalent). Column exists, stays NULL.

Proto3 new fields are wire-compatible but Rust struct initializers are exhaustive — 4 call sites needed explicit `0`.

### VmHWM sampling + scheduler EMA write (impl)

Worker samples `/proc/{pid}/status` `VmHWM` before `daemon.kill()`. Flows through `CompletionReport` → `ActorCommand::ProcessCompletion` → `update_build_history` → `ema_peak_memory_bytes` column.

`0` = no-signal sentinel (proc gone, early fail). Scheduler converts `0→None` at the boundary so the EMA isn't dragged toward zero. PG `COALESCE(blend, new, old)` handles all four `old×new` null cases via NULL arithmetic (`NULL*x = NULL`):
- `old=Some new=Some` → blend
- `old=None new=Some` → falls to new (first sample)
- `old=Some new=None` → falls to old (keep, no drag)
- `old=None new=None` → stays NULL

`output_size_bytes` (sum of uploaded NAR sizes) flows the same way to `ema_output_size_bytes` — dashboards-only, routing doesn't use it.

Resolved the phase2c-tagged `ProgressUpdate` item in `grpc/mod.rs`: mid-build samples aren't needed since `VmHWM` already IS the peak. `cpu_cores` remains TODO(phase3a) (needs polling, no kernel equivalent).

**Later phase note:** Phase 3a replaced `VmHWM` sampling with cgroup v2 `memory.peak` — the `VmHWM` approach had a latent bug: it measured the `nix-daemon` process RSS, not the builder's memory. The daemon is just a shim; the builder is a child process with the actual memory footprint. `memory.peak` on the build cgroup captures the whole tree. This plan's implementation was correct for its time (no cgroup-per-build infrastructure existed yet) but the measurement target was wrong.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "CompletionReport.peak_memory_bytes, output_size_bytes fields"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "read /proc/{pid}/status VmHWM before daemon.kill(), 0 sentinel"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "CompletionReport construction, 4 call sites explicit 0"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "update_build_history: COALESCE(blend, new, old) EMA write"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "0 -> None boundary conversion"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "proto field passthrough"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "ProcessCompletion flows peak_memory"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "ActorCommand field"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "COALESCE all 4 null cases, write->read roundtrip"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "completion fixtures"},
  {"path": "rio-scheduler/src/actor/tests/wiring.rs", "action": "MODIFY", "note": "end-to-end flow"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0082**: `build_history.ema_peak_memory_bytes`, `ema_output_size_bytes` columns from migration 006.
- Depends on **P0062** (2b actor-split, `288479d`): `actor/completion.rs` submodule exists.

## Exit

Merged as `f2770c4` + `632e933` (2 commits, non-adjacent: seq #7 and #23 in phase range). Tests: +4 across both commits (`read_vmhwm_bytes` vs `/proc/self/status` ×2, `COALESCE` semantics all 4 cases, write→`read_build_history` roundtrip).

Phase 3a completed the picture: cgroup v2 `cpu.stat` 1Hz polling for `ema_peak_cpu_cores`, and replaced `VmHWM` with cgroup `memory.peak` (fixing the daemon-PID-not-builder measurement bug).

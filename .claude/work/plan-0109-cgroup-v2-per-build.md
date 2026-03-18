# Plan 0109: cgroup v2 per-build tracking — FIXES phase2c VmHWM bug

## Design

**This plan fixes a silent measurement bug that had poisoned `build_history` since phase 2c.** The phase2c resource tracker read `/proc/{daemon_pid}/status` → `VmHWM` to get peak memory. But `nix-daemon` **forks** the builder and `waitpid()`s — the builder's memory never appeared in the daemon's `/proc`. A 16GB GCC build reported ~10MB (daemon RSS). Four commits replaced VmHWM with cgroup v2 `memory.peak` + polled `cpu.stat` — tree-wide, not daemon-PID.

`ff6791b` created `rio-worker/src/cgroup.rs` (482 LOC). cgroup v2 is a **hard requirement**: `own_cgroup()` + `enable_subtree_controllers()` both propagate `?` at startup. No degraded mode — a worker that silently falls back to the phase2c bug poisons `build_history` for ~10 EMA cycles. Fail loud at boot. `memory.peak` captures the whole tree (daemon + builder + every child compiler); kernel-tracked lifetime max, one read at build end, no polling. CPU: `cpu.stat usage_usec` is tree-cumulative with no kernel "peak CPU," so poll 1Hz: `cores = delta_usec / elapsed_usec`, max over build lifetime. Stored as `f64` bits in `AtomicU64` with compare-exchange loop for max — `fetch_max` on bits would compare **bit patterns** not float values (`2.0.to_bits() > 8.0.to_bits()` is not guaranteed). Ordering is **critical**: `BuildCgroup::create` + `add_process` happen BETWEEN `spawn_daemon` and `run_daemon_build`. The daemon must be in the cgroup **before** it forks the builder (fork inherits cgroup). If added after fork, builder inherits the parent cgroup — right back to the bug.

`a14d71e` fixed a design bug found while writing the NixOS module. The original `cgroup.rs` used `own_cgroup()` as the parent for per-build sub-cgroups. With `DelegateSubgroup=builds`, `own_cgroup()` returns `.../rio-worker.service/builds/` — where the worker process **is**. Per-build cgroups as children would violate cgroup v2's **no-internal-processes rule** (a cgroup with both processes AND controller-enabled sub-cgroups). `enable_subtree_controllers` would fail EBUSY; worker would never start. Fix: `delegated_root()` returns the **parent** of `own_cgroup()`. That's `.../rio-worker.service/` — EMPTY (DelegateSubgroup moved the worker out). Per-build cgroups become **siblings** of `builds/`, not children. Same shape in K8s: container cgroup under pod cgroup; `delegated_root()` returns pod cgroup; per-build siblings of container.

`a2eff3d` added the proto field `CompletionReport.peak_cpu_cores`. `09612ab` wired the scheduler side: `build_history.ema_peak_cpu_cores` column (already existed, now written). Same `COALESCE(blend, new, old)` pattern as memory: `0.0` from worker → `None` → column unchanged (build exited in <1s before the 1Hz poller sampled; no-signal doesn't drag EMA toward zero). `BuildHistoryRow` type alias: 5-element tuples trip clippy type-complexity; alias not struct — sqlx `query_as` maps tuples by ordinal position.

The NixOS module grew `Delegate=yes` + `DelegateSubgroup=builds` in the worker systemd unit (systemd v254+). Migration comment updated: old rows have ~10MB daemon-RSS not builder memory; EMA α=0.3 washes bad data out in ~10 completions (0.7^10 ≈ 2.8% old value remains). No migration — time heals.

## Files

```json files
[
  {"path": "rio-worker/src/cgroup.rs", "action": "NEW", "note": "482 LOC: own_cgroup, delegated_root, enable_subtree_controllers, BuildCgroup, memory.peak read, cpu.stat 1Hz poll with AtomicU64-bits CAS max"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "BuildCgroup::create + add_process BETWEEN spawn_daemon and run_daemon_build; ExecutionResult grows peak_cpu_cores"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "delegated_root() at startup BEFORE health server; fail loud on EBUSY"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "CompletionReport.peak_cpu_cores field"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "ema_peak_cpu_cores COALESCE write; BuildHistoryRow 5-tuple alias"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "peak_cpu_cores threaded through to update_build_history"},
  {"path": "rio-scheduler/src/estimator.rs", "action": "MODIFY", "note": "refresh accepts cpu_cores (doesn't store yet — HistoryEntry has no cpu field; phase4 cpu-bump)"},
  {"path": "migrations/001_scheduler.sql", "action": "MODIFY", "note": "comment updated: phase2c correction context"},
  {"path": "nix/modules/worker.nix", "action": "MODIFY", "note": "systemd Delegate=yes + DelegateSubgroup=builds"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[worker.cgroup.v2-required]`, `r[worker.cgroup.sibling-layout]` (the no-internal-processes fix), `r[worker.cgroup.create-before-fork]`, `r[worker.cgroup.memory-peak]`, `r[worker.cgroup.cpu-poll]`, `r[sched.history.ema-cpu]`. P0127's docs audit later fixed stale `own_cgroup` comments → `delegated_root` (`06c1356`).

## Entry

- Depends on P0096: phase 2c complete — this FIXES the phase2c VmHWM bug. `build_history` table existed with `ema_peak_memory_bytes` column; this plan wires the `ema_peak_cpu_cores` column that was present but never written.
- Depends on P0102: `rio-worker/src/executor/daemon/spawn.rs` was split out by P0102; `BuildCgroup::add_process` calls into it.

## Exit

Merged as `a2eff3d..a14d71e` (4 commits). `.#ci` green at merge. 10 new tests: parsers (v2 ok, v1 reject, root ok, empty/malformed reject, `cpu.stat` happy/missing/prefix-not-fooled), real-kernel `/proc/self/cgroup` canary, `memory.peak` parse from tempfile. `read_vmhwm_bytes` deleted along with its 2 tests. The **sibling** layout is exercised for real only by P0118's vm-phase3a k3s test — unit tests can't exercise delegation.

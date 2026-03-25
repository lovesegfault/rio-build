# Plan 436: Per-build cgroup memory.max/cpu.max enforcement

[`BuildCgroup`](../../rio-worker/src/cgroup.rs) today is measurement-only: `memory.peak` is read at build-end ([`cgroup.rs:125`](../../rio-worker/src/cgroup.rs), `r[worker.cgroup.memory-peak]`), but `memory.max`/`cpu.max` are never written. A runaway build can OOM the entire worker pod — the kernel OOMs the pod, not just the misbehaving build, taking down sibling builds and triggering a worker restart.

This is the intended next step after measurement landed. The scheduler already KNOWS each build's expected resource footprint (`build_history.ema_peak_memory_bytes`, `ema_peak_cpu_cores` — same data [P0435](plan-0435-wps-resource-requests-from-build-history.md) uses for pod-level requests). The scheduler can pass those EMAs in the `Assignment` message; the worker writes them (× headroom) to the per-build cgroup's `memory.max`/`cpu.max` before spawning the builder.

Enforcement semantics: `memory.max` is a hard cap — exceeding it triggers OOM-kill of the cgroup's processes (not the pod). `cpu.max` is throttling, not killing — a CPU-bound build runs slower, not fails. Both preserve sibling builds on the same worker. A build OOM-killed by its own `memory.max` surfaces as `BuildStatus::Failed` with `error_summary` citing the cgroup OOM (detectable via `memory.events`'s `oom_kill` counter).

Scoped as opt-in (`worker.cgroup_enforce_limits = false` default) so it can be canary-enabled per-pool before becoming default. The penalty-overwrite mechanism (`r[sched.classify.penalty-overwrite]`) already handles the "EMA was wrong, build got OOM-killed" case — misclassified builds get bumped to a larger class on retry.

## Entry criteria

- None (independent of open plans; `BuildCgroup` and `build_history` both shipped)

## Tasks

### T1 — `feat(proto):` Assignment carries per-build resource limits

MODIFY [`rio-proto/proto/scheduler.proto`](../../rio-proto/proto/scheduler.proto) — add to `Assignment`:

```proto
// Optional per-build cgroup limits. Worker writes these to the
// build's cgroup before spawn. 0 = no limit (measurement-only).
uint64 memory_max_bytes = N;   // → cgroup memory.max
double cpu_max_cores    = N+1; // → cgroup cpu.max
```

MODIFY [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) — populate from `build_history` EMA × configurable headroom (default 1.5 — more generous than [P0435](plan-0435-wps-resource-requests-from-build-history.md)'s 1.2 since this is a hard kill, not a scheduling hint). If no history row, send 0 (no limit).

### T2 — `feat(worker):` BuildCgroup writes memory.max/cpu.max

MODIFY [`rio-worker/src/cgroup.rs`](../../rio-worker/src/cgroup.rs). Add to `BuildCgroup`:

```rust
/// Set the hard memory limit. Writing to `memory.max` takes effect
/// immediately for all processes already in the cgroup. Call BEFORE
/// `add_process()` so the limit is active when the builder spawns.
///
/// `bytes = 0` → skip (no limit, measurement-only mode).
pub fn set_memory_max(&self, bytes: u64) -> io::Result<()> {
    if bytes == 0 { return Ok(()); }
    fs::write(self.path.join("memory.max"), bytes.to_string())
}

/// Set CPU throttle. cgroup2 `cpu.max` format is `$MAX $PERIOD`
/// where both are μs. We fix PERIOD=100000 (100ms, kernel default)
/// and compute MAX = cores × 100000.
///
/// `cores = 0.0` → skip (no throttle).
pub fn set_cpu_max(&self, cores: f64) -> io::Result<()> {
    if cores == 0.0 { return Ok(()); }
    let max_us = (cores * 100_000.0) as u64;
    fs::write(self.path.join("cpu.max"), format!("{max_us} 100000"))
}

/// Read `memory.events` oom_kill counter. Non-zero means the
/// build was killed by its own memory.max, not a general failure.
pub fn oom_kill_count(&self) -> io::Result<u64> {
    let events = fs::read_to_string(self.path.join("memory.events"))?;
    // Format: "low N\nhigh N\nmax N\noom N\noom_kill N\n"
    events.lines()
        .find_map(|l| l.strip_prefix("oom_kill ")?.parse().ok())
        .ok_or_else(|| io::Error::other("oom_kill not in memory.events"))
}
```

MODIFY [`rio-worker/src/executor/mod.rs`](../../rio-worker/src/executor/mod.rs) — call `set_memory_max`/`set_cpu_max` between `BuildCgroup::create` and `add_process`, gated on `config.cgroup_enforce_limits`. At build-end, if status is Failed, check `oom_kill_count() > 0` and set `error_summary = "cgroup memory.max exceeded (OOM-killed at {peak}B / {limit}B limit)"`.

### T3 — `feat(worker):` config knob + helm plumbing

MODIFY [`rio-worker/src/main.rs`](../../rio-worker/src/main.rs) config struct — add `cgroup_enforce_limits: bool` (default `false`). MODIFY [`infra/helm/rio-build/values.yaml`](../../infra/helm/rio-build/values.yaml) worker section — expose the knob. MODIFY worker ConfigMap template.

### T4 — `test(worker):` cgroup limit setters + OOM detection

NEW tests in [`rio-worker/src/cgroup.rs`](../../rio-worker/src/cgroup.rs) `mod tests`. These need a real cgroup2 mount — gate with `#[cfg_attr(not(target_os = "linux"), ignore)]` and a runtime check for writable cgroup root (same pattern as existing cgroup tests):

```rust
#[test]
fn set_memory_max_writes_bytes() {
    let cg = test_cgroup();
    cg.set_memory_max(512 * MI).unwrap();
    assert_eq!(read(cg.path().join("memory.max")), "536870912\n");
}

#[test]
fn set_cpu_max_writes_quota_period() {
    let cg = test_cgroup();
    cg.set_cpu_max(1.5).unwrap();
    assert_eq!(read(cg.path().join("cpu.max")), "150000 100000\n");
}

#[test]
fn zero_limit_skips_write() {
    let cg = test_cgroup();
    cg.set_memory_max(0).unwrap();
    // memory.max untouched → still "max" (kernel default)
    assert_eq!(read(cg.path().join("memory.max")), "max\n");
}
```

OOM-detection end-to-end test goes in VM tests (needs real kernel OOM behavior) — add `cgroup-oom` subtest to [`nix/tests/scenarios/worker.nix`](../../nix/tests/scenarios/worker.nix) that runs a deliberately-oversized build with a tight `memory.max`, asserts Failed status with `error_summary` containing "cgroup memory.max exceeded".

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'memory.max\|cpu.max' rio-worker/src/cgroup.rs | grep -c 'fn set_'` → 2 (both setters)
- `grep 'oom_kill_count' rio-worker/src/cgroup.rs rio-worker/src/executor/mod.rs` → ≥2 (defined + called)
- `grep 'cgroup_enforce_limits' rio-worker/src/main.rs infra/helm/rio-build/values.yaml` → ≥2 (knob plumbed)
- `cargo nextest run -p rio-worker set_memory_max set_cpu_max zero_limit` → 3 passed (on Linux with cgroup2)
- `grep 'memory_max_bytes\|cpu_max_cores' rio-proto/proto/scheduler.proto` → ≥2 (Assignment fields)
- `nix develop -c tracey query rule worker.cgroup.enforce-limits` → non-empty (new marker parsed)
- VM subtest `cgroup-oom` in `nix/tests/default.nix` subtests list with `# r[verify worker.cgroup.enforce-limits]` marker

## Tracey

References existing markers:
- `r[worker.cgroup.sibling-layout]` — T2 writes limits into the existing per-build sibling cgroup
- `r[worker.cgroup.memory-peak]` — T2's OOM detection reads `memory.peak` alongside `memory.events`
- `r[sched.classify.penalty-overwrite]` — the retry path when a build is OOM-killed by an undersized limit

Adds new markers to component specs:
- `r[worker.cgroup.enforce-limits]` → `docs/src/components/worker.md` (see ## Spec additions)

## Spec additions

Add to [`docs/src/components/worker.md`](../../docs/src/components/worker.md) after the `r[worker.cgroup.memory-peak]` paragraph:

```
r[worker.cgroup.enforce-limits]
When `cgroup_enforce_limits` is set, the worker writes
`memory.max = Assignment.memory_max_bytes` and
`cpu.max = (Assignment.cpu_max_cores × 100000) 100000` to the
per-build cgroup before spawning the builder. `memory.max` is a
hard cap: exceeding it OOM-kills the cgroup's process tree (NOT
the worker pod — sibling builds survive). The executor detects
this via `memory.events oom_kill > 0` and sets
`error_summary = "cgroup memory.max exceeded"`. `cpu.max`
throttles, not kills. Scheduler populates the Assignment limits
from `build_history` EMA × headroom (default 1.5); zero = no
limit. Opt-in per-pool; default off.
```

## Files

```json files
[
  {"path": "rio-proto/proto/scheduler.proto", "action": "MODIFY", "note": "T1: Assignment +memory_max_bytes +cpu_max_cores"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T1: populate limits from build_history EMA × headroom"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "T2: set_memory_max/set_cpu_max/oom_kill_count; T4: tests"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "T2: call setters pre-spawn, detect OOM post-fail"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "T3: cgroup_enforce_limits config knob"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T3: expose knob in worker section"},
  {"path": "infra/helm/rio-build/templates/worker-configmap.yaml", "action": "MODIFY", "note": "T3: plumb knob"},
  {"path": "nix/tests/scenarios/worker.nix", "action": "MODIFY", "note": "T4: cgroup-oom subtest"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T4: wire cgroup-oom subtest + r[verify] marker"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "Spec: add r[worker.cgroup.enforce-limits] marker"}
]
```

```
rio-proto/proto/
└── scheduler.proto          # T1: Assignment +2 limit fields
rio-scheduler/src/actor/
└── dispatch.rs              # T1: populate from EMA
rio-worker/src/
├── cgroup.rs                # T2+T4: setters + oom_kill_count + tests
├── executor/mod.rs          # T2: call setters, detect OOM
└── main.rs                  # T3: config knob
infra/helm/rio-build/
├── values.yaml              # T3: knob
└── templates/worker-configmap.yaml
nix/tests/
├── scenarios/worker.nix     # T4: cgroup-oom scenario
└── default.nix              # T4: wire + r[verify]
docs/src/components/
└── worker.md                # spec marker
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [435], "note": "Independent — BuildCgroup and build_history both shipped. Soft-dep on P0435 only for proto collision (both touch scheduler.proto — serialize or merge carefully)."}
```

**Depends on:** none — `BuildCgroup` (phase2c) and `build_history` EMA columns both shipped.

**Conflicts with:** [P0435](plan-0435-wps-resource-requests-from-build-history.md) — both MODIFY `rio-proto/proto/scheduler.proto` and `infra/helm/rio-build/values.yaml`. Non-overlapping edits (different messages / different sections) but serialize to avoid rebase noise. `rio-scheduler/src/actor/dispatch.rs` is HOT.

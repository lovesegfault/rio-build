# Plan 0188: Rem-09 — Build timeout orphans cgroup + status collapse + no-reassign

## Design

**P1 (HIGH).** Four distinct bugs with one precipitating event (build timeout). Causality chain: timeout → `Err` path → `InfrastructureFailure` → reassignment storm. Separately: timeout → daemon killed, builder orphaned → `EBUSY` leak → `EEXIST` on next assignment.

**`wkr-timeout-cgroup-orphan`:** `daemon.kill()` only kills the direct child; the builder is a grandchild forked by `nix-daemon` and survives in the cgroup. On timeout, `BuildCgroup::Drop` rmdir → `EBUSY` → leak → next build of same drv on same worker fails `EEXIST`. Fix: inline `build_cgroup.kill()` + bounded `cgroup.procs` drain poll before drop. Scopeguard hardening covers future `?` insertions.

**`wkr-timeout-is-infra`:** `stderr_loop.rs` mapped `Elapsed` → `Err` → runtime reports `InfrastructureFailure` → scheduler reassigns → same timeout forever. Fix: timeout is a build outcome, return `Ok(BuildResult{status: TimedOut})` not `Err`.

**`wkr-status-collapse`:** `mod.rs` match had `_` arm collapsing 9 distinct Nix statuses (including `TimedOut`) to `PermanentFailure`. Fix: extract exhaustive `nix_failure_to_proto` free fn (no `_` arm). Added proto variants `TimedOut`/`NotDeterministic`/`InputRejected` + scheduler `completion.rs` arms (permanent-no-reassign).

**`wkr-cancel-flag-stale`:** Cancel arrives before cgroup exists → `ENOENT` on kill, flag stays `true` → unrelated later `Err` misclassified as `Cancelled`. Fix: clear flag on failed kill.

Remediation doc: `docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md` (846 lines).

## Files

```json files
[
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "inline build_cgroup.kill() + cgroup.procs drain before drop; exhaustive nix_failure_to_proto (no _ arm)"},
  {"path": "rio-worker/src/executor/daemon/stderr_loop.rs", "action": "MODIFY", "note": "Elapsed → Ok(BuildResult{status: TimedOut}) not Err"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "clear cancel flag on ENOENT kill"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "metric for cancel flag clear"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "BuildStatus: TimedOut, NotDeterministic, InputRejected"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "TimedOut/NotDeterministic/InputRejected → permanent-no-reassign"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "4 spec markers"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "metric for cancel flag"}
]
```

## Tracey

- `r[impl worker.cgroup.kill-on-teardown]` — `75c4559` (**UNTESTED at phase-4a** — VM test proposed in remediation but different marker verified)
- `r[impl worker.status.nix-to-proto]` + `r[verify worker.status.nix-to-proto]` — `75c4559`
- `r[impl worker.timeout.no-reassign]` + `r[verify worker.timeout.no-reassign]` — `75c4559`
- `r[impl worker.cancel.flag-clear-enoent]` + `r[verify worker.cancel.flag-clear-enoent]` — `75c4559`

7 marker annotations (4 impl, 3 verify; `worker.cgroup.kill-on-teardown` shows UNTESTED in `tracey query untested`).

## Entry

- Depends on P0148: phase 3b complete (extends 2c executor)

## Exit

Merged as `3466888` (plan doc) + `75c4559` (fix). `.#ci` green.

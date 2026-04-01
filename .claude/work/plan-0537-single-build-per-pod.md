# Plan 0537: One build per builder pod — remove maxConcurrentBuilds

## Design

Architectural simplification: a builder pod executes **exactly one build at a time**. The `maxConcurrentBuilds` knob is removed entirely; there is no configuration that allows >1. Ephemeral becomes the default pool mode (one pod, one build, exit); long-lived (StatefulSet, pod resets after each build) is opt-in.

**Why:** multi-build-per-pod requires per-build resource fencing, per-build cgroup memory.max, semaphore machinery, and "build A OOMs the pod with build B running" reasoning. With one build, the pod's resources ARE the build's resources — k8s `resources.limits` is the fence. Isolation reasoning collapses to "one build, one pod, one cgroup."

**What stays:**
- Per-build sub-cgroup (`/sys/fs/cgroup/{hash}`) — `cgroup.kill` cancellation needs a cgroup that doesn't include rio-builder + FUSE threads. The pod's cgroup is too broad.
- Hash-named `{build_dir}` (`/var/rio/overlays/{hash}`) — leaked-mount cleanup and debugging both benefit from the hash; a fixed path saves nothing.
- "running build continues across stream reconnect" semantics (I-048 family) — one build, but it still survives reconnect.

**What goes:**
- `maxConcurrentBuilds` / `max_builds` / `RIO_MAX_BUILDS` (CRD field, builder config, scheduler ExecutorState, proto, helm, nix module)
- `RIO_BUILD_MEMORY_MAX_BYTES` and the memory-unset WARN — pod limit is the build limit
- `Semaphore(max_builds)` → `AtomicBool busy`
- `running_builds: HashSet<DrvHash>` → `Option<DrvHash>` (scheduler-side; cascades into reconcile/heartbeat phantom-detection)
- `assignment.rs` load-fraction scoring (`W_load`) — with binary capacity, `has_capacity()` already filters busy; the score reduces to locality only
- `available_build_slots` in ResourceUsage — derivable as `!busy`
- 3 CEL rules in CRD that enforce ephemeral/manifest → maxConcurrentBuilds=1 (moot)
- controller's force-to-1 + degrade-event paths

## Stages (6 commits)

| | Commit | Scope |
|---|---|---|
| 1 | `refactor(crds/proto/helm): remove maxConcurrentBuilds knob` | CRD field+CEL delete, proto `reserved 7/7/4`, helm values+templates delete, nix module option delete, controller env-var delete. Behavior becomes always-1 |
| 2 | `refactor(builder): Semaphore→busy, drop RIO_BUILD_MEMORY_MAX_BYTES` | config.rs+main.rs+runtime.rs+cgroup.rs simplification |
| 3 | `refactor(scheduler): max_builds removal, has_capacity→is_empty, drop load-fraction` | state/executor.rs+assignment.rs+actor threading |
| 4 | `refactor(scheduler): running_builds HashSet→Option` | Separate — cascades into reconcile/heartbeat phantom-detection (actor/mod.rs:576-731) |
| 5 | `feat(crds): ephemeral default true` | DEFAULT-FLIP. Existing StatefulSet pools must set `ephemeral: false` explicitly |
| 6 | `docs: single-build model + scheduling fixture rework` | controller.md+builder.md+ADR-020 rewrite, tracey marker delete/fold, `nix/tests/default.nix:99,103` 2×2-slot→3×1-slot |

Stages 1-3 can land together (one deploy). Stage 4 is internal-only. Stage 5 is operationally visible (existing pools need `ephemeral: false`). Stage 6 is docs+test.

## Files

```json files
[
  {"path": "rio-crds/src/builderpool.rs", "action": "MODIFY", "note": "delete max_concurrent_builds field + 3 CEL rules + schema tests; ephemeral default→true"},
  {"path": "rio-crds/src/builderpoolset.rs", "action": "MODIFY", "note": "delete template field"},
  {"path": "rio-proto/proto/build_types.proto", "action": "MODIFY", "note": "reserved 7 (HeartbeatRequest.max_builds)"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "reserved 7 (ResourceUsage.available_build_slots)"},
  {"path": "rio-proto/proto/admin_types.proto", "action": "MODIFY", "note": "reserved 4 (ExecutorInfo.max_builds)"},
  {"path": "rio-builder/src/config.rs", "action": "MODIFY", "note": "delete max_builds + build_memory_max_bytes"},
  {"path": "rio-builder/src/main.rs", "action": "MODIFY", "note": "Semaphore→AtomicBool, HashSet→Option, drain simplify, delete WARN"},
  {"path": "rio-builder/src/runtime.rs", "action": "MODIFY", "note": "delete slot-calc"},
  {"path": "rio-builder/src/cgroup.rs", "action": "MODIFY", "note": "BuildLimits cpu.max only (or delete memory.max set)"},
  {"path": "rio-scheduler/src/state/executor.rs", "action": "MODIFY", "note": "delete max_builds, has_capacity→is_empty, running_builds→Option"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "delete W_load scoring"},
  {"path": "rio-scheduler/src/actor/executor.rs", "action": "MODIFY", "note": "delete max_builds threading"},
  {"path": "rio-controller/src/reconcilers/common/sts.rs", "action": "MODIFY", "note": "delete RIO_MAX_BUILDS env"},
  {"path": "rio-controller/src/reconcilers/builderpool/ephemeral.rs", "action": "MODIFY", "note": "delete force-to-1"},
  {"path": "rio-controller/src/reconcilers/builderpool/manifest.rs", "action": "MODIFY", "note": "delete override"},
  {"path": "rio-controller/src/reconcilers/builderpool/mod.rs", "action": "MODIFY", "note": "delete degrade-event"},
  {"path": "rio-cli/src/status.rs", "action": "MODIFY", "note": "delete max_builds display"},
  {"path": "rio-cli/src/main.rs", "action": "MODIFY", "note": "delete max_builds display"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "delete maxConcurrentBuilds"},
  {"path": "infra/helm/rio-build/templates/builderpool.yaml", "action": "MODIFY", "note": "delete maxConcurrentBuilds"},
  {"path": "infra/helm/rio-build/crds/builderpools.rio.build.yaml", "action": "REGEN", "note": "from rio-crds"},
  {"path": "nix/modules/builder.nix", "action": "MODIFY", "note": "delete maxBuilds option+env"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "delete maxBuilds param"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "scheduling fixture: 2×2-slot→3×1-slot workers"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "rewrite + delete r[ctrl.pool.ephemeral-single-build]"},
  {"path": "docs/src/components/builder.md", "action": "MODIFY", "note": "rewrite max_builds section"},
  {"path": "docs/src/decisions/020-*.md", "action": "MODIFY", "note": "delete maxConcurrentBuilds clause"}
]
```

## Exit criteria

- [ ] `grep -r 'max_builds\|maxConcurrentBuilds\|max_concurrent_builds\|RIO_MAX_BUILDS\|RIO_BUILD_MEMORY_MAX_BYTES' rio-*/ infra/ nix/` returns only proto `reserved` lines and historical commit messages
- [ ] `rio-cli workers` no longer shows `N/M builds` — shows `[busy]` or `[idle]`
- [ ] `.#ci` green (vm-scheduling fixture passes with 3×1-slot topology)
- [ ] `tracey query validate` clean (markers deleted/bumped)
- [ ] `kubectl explain builderpool.spec` has no `maxConcurrentBuilds`
- [ ] BuilderPool with no `ephemeral` field reconciles as ephemeral (Job-per-build)

## Deps

None. Builds on sprint-1@6518c83e.

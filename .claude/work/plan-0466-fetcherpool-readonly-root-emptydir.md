# Plan 0466: FetcherPool readOnlyRootFilesystem — audit rio-builder rootfs writes → emptyDir tmpfs

**CI BLOCKER — vm-fetcher-split red on sprint-1.** [P0453](plan-0453-controller-builderpool-fetcherpool-split.md) shipped the FetcherPool reconciler with `read_only_root_fs: true` ([`fetcherpool/mod.rs:212`](../../rio-controller/src/reconcilers/fetcherpool/mod.rs)) per ADR-019 §Sandbox hardening. The `rio-builder` binary (same binary, fetcher role) writes to rootfs at startup — [`main.rs:148-165`](../../rio-builder/src/main.rs) does `create_dir_all` on `fuse_mount_point`, `overlay_base_dir`, `/nix/var/nix/{db,profiles,temproots,gcroots}` and `chmod 0755` them. With `readOnlyRootFilesystem: true`, these hit `EROFS` → pod `CrashLoopBackOff`. The `vm-fetcher-split` test is correct and catches this; it stays red until fixed.

Two approaches were considered:
- **(a)** Add `readOnlyRootFilesystem` escape hatch to `FetcherPoolSpec` — rejected. [P0460](plan-0460-psa-restricted-control-plane.md) is tightening PSA to `restricted`; an escape hatch works against that and the ADR-019 threat model (fetcher faces open internet, rootfs tampering is the #1 post-compromise move).
- **(b)** Audit `rio-builder` rootfs writes, cover them with emptyDir tmpfs mounts — **chosen**. [`common/sts.rs:404-429`](../../rio-controller/src/reconcilers/common/sts.rs) already has the pattern (`/tmp`, `fuse-store`, `/nix/var` emptyDirs when `read_only_root_fs`); the current set is incomplete. This plan closes the gap.

**Implementer note:** this plan was pre-allocated and an implementer is running in parallel in worktree `p466`. This doc captures the spec; their commits deliver the fix. Reconcile at merge.

## Entry criteria

- [P0453](plan-0453-controller-builderpool-fetcherpool-split.md) merged (FetcherPool reconciler + `read_only_root_fs: true` exist) — **DONE**

## Tasks

### T1 — `fix(builder):` audit main.rs startup rootfs writes

Enumerate every filesystem write in [`rio-builder/src/main.rs`](../../rio-builder/src/main.rs) that happens before the executor loop starts. Known sites from `:148-165`:

| Path | Op | Covered by existing emptyDir? |
|---|---|---|
| `cfg.fuse_mount_point` (default `/nix/store`) | `create_dir_all` | YES — `fuse-store` emptyDir ([`sts.rs:420-424`](../../rio-controller/src/reconcilers/common/sts.rs)) |
| `cfg.overlay_base_dir` (default `/var/rio/overlays`) | `create_dir_all` | YES — `overlays` emptyDir ([`sts.rs:391-402`](../../rio-controller/src/reconcilers/common/sts.rs)) |
| `/nix/var/nix/db`, `profiles`, `temproots`, `gcroots` | `create_dir_all` + `chmod` | PARTIAL — `/nix/var` emptyDir exists but check mount path vs write path |
| `/tmp` (tempfile crate) | implicit | YES — `tmp` emptyDir 64Mi ([`sts.rs:408-415`](../../rio-controller/src/reconcilers/common/sts.rs)) |

Grep `std::fs::\|tokio::fs::\|File::create\|create_dir\|fs::write\|set_permissions` across `rio-builder/src/` (not just `main.rs` — `lib.rs`, `executor/`, `upload.rs` may write outside the overlay upperdir too). Each uncovered path gets an emptyDir in T2.

**Deliverable:** a comment block in `main.rs` near `:148` listing every startup write path + which emptyDir mount covers it. Future additions to startup writes must extend the list AND the mount table.

### T2 — `fix(controller):` extend common/sts.rs emptyDir set to cover T1 findings

MODIFY [`rio-controller/src/reconcilers/common/sts.rs`](../../rio-controller/src/reconcilers/common/sts.rs) in the `if p.read_only_root_fs` block starting `:404`.

For each uncovered write path from T1, add a `Volume` + matching `VolumeMount` pair. The `/nix/var` mount likely needs verification — [`sts.rs:426`](../../rio-controller/src/reconcilers/common/sts.rs) comment says "Mount at /nix/var to cover both" but if `main.rs:164` writes `/nix/var/nix/db` and the mount is at `/nix/var`, `create_dir_all` still needs the intermediate `nix/` dir — check the mount's `subPath` vs the write target.

**Do not** add an escape hatch. If a write path is genuinely unmountable (e.g., writes to `/etc/passwd` — it shouldn't), fix the write, not the mount.

Each emptyDir added here SHOULD use `medium: Memory` with a small `size_limit` (tmpfs). Fetcher pods are short-lived FOD fetches; disk-backed emptyDir is unnecessary and the pod's memory limit bounds tmpfs.

### T3 — `test(vm):` vm-fetcher-split goes green

No new test file — [`vm-fetcher-split`](../../nix/tests/default.nix) already exists and catches the `CrashLoopBackOff`. This task is: run `/nixbuild .#checks.x86_64-linux.vm-fetcher-split` and confirm green.

If the test has a `kubectl wait --for=condition=Ready pod -l app.kubernetes.io/component=fetcher` with a timeout, verify the timeout is generous enough for the now-larger init sequence (more mounts → slightly slower startup, ~1-2s).

## Exit criteria

- `/nixbuild .#checks.x86_64-linux.vm-fetcher-split` → green (CI unblocked)
- `/nixbuild .#ci` → green (full gate)
- `grep -c 'create_dir_all\|set_permissions' rio-builder/src/main.rs` — every hit has a corresponding comment naming its covering emptyDir mount
- `kubectl get pod -n rio-fetchers -o jsonpath='{.items[0].spec.containers[0].securityContext.readOnlyRootFilesystem}'` → `true` (no escape hatch was added)
- `grep 'read_only_root.*false\|readOnlyRoot.*escape' rio-controller/src/crds/fetcherpool.rs rio-controller/src/reconcilers/fetcherpool/mod.rs` → 0 hits (approach-a was NOT taken)

## Tracey

References existing markers:
- `r[fetcher.sandbox.strict-seccomp]` — T2 completes the readOnlyRootFilesystem half ([ADR-019:91](../../docs/src/decisions/019-builder-fetcher-split.md)); the `TODO(P0455)` at [`fetcherpool/mod.rs:209`](../../rio-controller/src/reconcilers/fetcherpool/mod.rs) can be resolved to an `r[impl]` annotation once ADR-019 is in tracey `spec_include`
- `r[ctrl.fetcherpool.reconcile]` — T2 extends the reconciler's StatefulSet template ([ADR-019:44](../../docs/src/decisions/019-builder-fetcher-split.md))

No new markers — this is a correctness fix bringing implementation into alignment with existing ADR-019 spec text.

## Files

```json files
[
  {"path": "rio-builder/src/main.rs", "action": "MODIFY", "note": "T1: comment block near :148 listing startup write paths + covering mounts"},
  {"path": "rio-controller/src/reconcilers/common/sts.rs", "action": "MODIFY", "note": "T2: extend read_only_root_fs emptyDir set :404+ to cover T1 findings"}
]
```

```
rio-builder/src/
└── main.rs                           # T1: startup-write audit comment
rio-controller/src/reconcilers/common/
└── sts.rs                            # T2: emptyDir Volume+VolumeMount pairs
```

## Dependencies

```json deps
{"deps": [453], "soft_deps": [460, 467], "note": "CI BLOCKER — vm-fetcher-split red on sprint-1. P0453 shipped read_only_root_fs:true without full emptyDir coverage. Approach (b) chosen over escape-hatch to align with P0460 PSA-restricted tightening. Soft-dep P0467 (READ_ONLY_ROOT_MOUNTS const-table extraction) — that consolidation refactors the Volume+VolumeMount pairs T2 adds; sequence: P0466 FIRST (unblock CI), P0467 AFTER (refactor the now-complete set). discovered_from=coverage (vm-fetcher-split red post-p453)."}
```

**Depends on:** [P0453](plan-0453-controller-builderpool-fetcherpool-split.md) — `read_only_root_fs: true` + partial emptyDir set exist. **DONE**.
**Soft-dep:** [P0460](plan-0460-psa-restricted-control-plane.md) — PSA-restricted tightening; this plan's approach-b choice aligns with it. [P0467](plan-0467-readonly-root-mounts-const-table.md) — const-table extraction; sequence THIS first (CI unblock), consolidation after.
**Conflicts with:** [`common/sts.rs`](../../rio-controller/src/reconcilers/common/sts.rs) — T2 here and P0467 both touch the `:404+` read_only_root_fs block. P0466 adds mounts; P0467 extracts them to a const table. Serialize: P0466 → P0467.

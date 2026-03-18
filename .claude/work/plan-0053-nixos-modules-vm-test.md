# Plan 0053: NixOS modules + phase2a VM test — 4-VM distributed milestone validation

## Context

The phase2a.md milestone says: `nix build --store ssh-ng://rio nixpkgs#hello` completes across 2+ workers. Everything up to here — 129 commits — passed mocked tests. Nobody had actually run a real Nix client against the full stack with FUSE mounted, overlay active, `CLONE_NEWNS` in effect.

This plan is the NixOS VM test that does. Four VMs (control + 2 workers + client), a real `nix copy --to ssh-ng://control` seed, a real `nix-build --store ssh-ng://control` on a 5-node fan-out DAG, Prometheus metric assertions proving both workers actually built something.

The `#[ignore]`d integration tests needed `CAP_SYS_ADMIN` and never ran in CI. The VM test IS the CI solution — QEMU gives it root, FUSE, mount namespaces, everything.

Running this test found **ten bugs** the mocked tests missed. P0054, P0055, P0056 document them.

## Commits

- `81ab144` — feat(nix): add NixOS modules and phase2a milestone VM test
- `f5723b4` — docs(phases): add automated validation section for phase2a milestone
- `306038c` — test(nix): rename milestone→phase2a, add phase1a + phase1b VM tests

## Files

```json files
[
  {"path": "nix/modules/common.nix", "action": "NEW", "note": "shared options: gRPC endpoints, log level, Prometheus port"},
  {"path": "nix/modules/gateway.nix", "action": "NEW", "note": "systemd unit: SSH port, store/scheduler endpoints"},
  {"path": "nix/modules/scheduler.nix", "action": "NEW", "note": "systemd unit: PG URL, store endpoint"},
  {"path": "nix/modules/store.nix", "action": "NEW", "note": "systemd unit: backend=fs, PG URL, blob dir"},
  {"path": "nix/modules/worker.nix", "action": "NEW", "note": "systemd unit: CAP_SYS_ADMIN, /dev/fuse, fuse userAllowOther, kernel modules (fuse, overlay); P0054 adds fuse3 to PATH, removes CapabilityBoundingSet"},
  {"path": "nix/tests/phase2a.nix", "action": "NEW", "note": "4 VMs: control(gateway+scheduler+store+PG) + worker1 + worker2 + client; seed busybox via nix copy; build 5-node fan-out DAG; assert metrics"},
  {"path": "nix/tests/phase2a-derivation.nix", "action": "NEW", "note": "5-node test DAG: 4 parallel leaves (busybox sh echo), 1 collector (cat all 4)"},
  {"path": "nix/tests/phase1a.nix", "action": "NEW", "note": "2 VMs: read-only protocol (nix store info, path-info, store ls, verify, nonexistent→STDERR_ERROR)"},
  {"path": "nix/tests/phase1b.nix", "action": "NEW", "note": "3 VMs: single worker, 1 trivial derivation, assert builds_total{outcome=success}==1"},
  {"path": "flake.nix", "action": "MODIFY", "note": "nixosModules.{gateway,scheduler,store,worker}; checks.rio-phase{1a,1b,2a} (Linux+KVM)"},
  {"path": "docs/src/phases/phase2a.md", "action": "MODIFY", "note": "Automated Validation section referencing vm-phase2a"}
]
```

## Design

**Test shape:** `pkgsStatic.busybox` is the only seed — closure of 1 path, uploaded via `nix copy --to ssh-ng://control` (exercises `wopAddMultipleToStore`). The DAG is 4 leaves + 1 collector: each leaf is `busybox sh -c 'mkdir $out && echo leaf-N > $out/payload'`, the collector `cat`s all four. This is deliberately tiny — seeding the full stdenv closure would dominate test time.

**Metric assertions:** `rio_worker_builds_total{outcome="success"} >= 1` on *each* worker (proves distribution). `rio_worker_fuse_cache_misses_total >= 1` on *each* (proves FUSE lazy-fetch). `rio_store_put_path_total{result="created"} >= 5` (all outputs uploaded). `rio_scheduler_builds_total{outcome="succeeded"} == 1`.

**Worker module:** `AmbientCapabilities = CAP_SYS_ADMIN` (for mount/unshare). `/dev/fuse` via `boot.kernelModules`. `programs.fuse.userAllowOther = true`. Initially had `CapabilityBoundingSet` too tight — P0054 removes (nix-daemon's sandbox needs `CAP_SETUID`/`CHOWN`/`SYS_CHROOT`/`MKNOD`).

**phase1a/1b retrofit (`306038c`):** per-phase milestone validation. 1a (2 VMs, ~87s): read-only protocol only. 1b (3 VMs, ~87s): single worker, single build. 2a (4 VMs, ~90s): distributed. All exposed as `checks.x86_64-linux.rio-phase{1a,1b,2a}`.

**KVM:** without `/dev/kvm`, TCG emulation is 10–50× slower. GitHub Actions standard runners lack nested virt — needs self-hosted with KVM.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). VM test scripts later got `r[verify *]` markers at file-header level (tracey's `.nix` parser only reads col-0 comments before `{`).

## Outcome

Merged as `81ab144`, `f5723b4`, `306038c` (3 commits). **The first run of this test did NOT pass.** It found the bugs in P0054, P0055, P0056. The test *first* passed at `132de90` (P0054's second commit) — "VM milestone test now PASSES: all 5 builds succeed across 2 workers, all metric assertions pass." That was the actual phase 2a milestone.

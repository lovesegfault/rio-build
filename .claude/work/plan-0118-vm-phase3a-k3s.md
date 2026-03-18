# Plan 0118: vm-phase3a k3s end-to-end test

## Design

The phase3a proof-of-life: a single-commit NixOS VM test running a real k3s cluster with the controller managing a WorkerPool. 3 VMs: `k8s` (k3s server + controller as systemd), `control` (PG + store + scheduler + gateway, NOT in k3s), `client` (nix). Worker runs **as a pod** in k3s.

`WorkerPoolSpec` grew `privileged` + `host_network` (`Option<bool>`, `None` preserves current behavior) — VM-test concessions: `privileged=true` because k3s's containerd seccomp blocks `mount(2)` even with `SYS_ADMIN` cap (production uses granular caps); `hostNetwork=true` because pod needs to resolve `control` via node `/etc/hosts` (CoreDNS doesn't know NixOS-test VM hostnames).

Controller runs as **systemd** (not pod) — RBAC bootstrap ordering. As a pod, controller needs ServiceAccount+ClusterRole applied BEFORE it starts — two `kubectl apply` in sequence. Systemd service with `KUBECONFIG=/etc/rancher/k3s/k3s.yaml` gets cluster-admin via the node kubeconfig; starts right after k3s.

k3s `extraFlags`: `--flannel-iface eth1` (eth0 is qemu slirp, no broadcast), `--disable traefik,metrics-server` (fewer airgap images). Airgap: `services.k3s.images` preloads worker image + k3s's own airgap-images (no internet in NixOS test VMs). `globalTimeout=600s` — so a crash-loop fails in 10min not forever. `systemd.services.k3s.Delegate=yes` so containerd can create pod cgroups. `withMinCpu 4`: 3 VMs but k8s is 8-core.

Assertions sequence: CRD Established → StatefulSet created → pod Ready (THE BIG MOMENT: FUSE in pod works) → worker heartbeat at scheduler → build succeeds → finalizer drain → pod gone. `PrefetchHint` assertion weakened: trivial leaf derivation has no DAG children → `approx_input_closure` empty → `send_prefetch_hint` early-returns. Metric existence check only. Multi-node DAG deferred (closure-size-small tradeoff).

This test immediately surfaced 8 bugs (P0119), then more after expanding assertions (P0120, P0121) — it was the most productive VM test in the project. The test file grew from 437 LOC here to 956 LOC by P0125.

## Files

```json files
[
  {"path": "nix/tests/phase3a.nix", "action": "NEW", "note": "437 LOC: 3-VM k3s test; controller-as-systemd; worker-as-pod; airgap images; globalTimeout=600s"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "privileged + host_network Option<bool> escape hatches (VM-test concessions)"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "thread privileged/hostNetwork into pod template"},
  {"path": "infra/base/crds.yaml", "action": "MODIFY", "note": "regenerated for new fields"},
  {"path": "flake.nix", "action": "MODIFY", "note": "vm-phase3a check target; withMinCpu 4"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `.nix` files not parsed by tracey. VM-test-verified spec rules (e.g., `r[ctrl.sts.create]`) carry `r[verify]` annotations via `.sh` shims added in P0126.

## Entry

- Depends on P0117: kustomize manifests deployed by the test.
- Depends on P0105: `mkControlNode`, `seedBusybox`, `waitForControlPlane` from `common.nix`.
- Depends on P0112, P0109, P0111: the test exercises controller + cgroup + prefetch end-to-end.

## Exit

Merged as `59e9792` (1 commit). First run: FAILED — pod crash-looped immediately. Led directly to P0119's 8-bug fix wave. After P0119: `.#ci` green with vm-phase3a passing.

# ADR-012: Privileged Builder Pods

## Status
Accepted

## Context
Workers require Linux kernel capabilities for two operations: overlayfs mounts (for per-build isolation, ADR-005) and Nix build sandboxing (user namespaces, mount namespaces, PID namespaces, chroot). Kubernetes pod security must grant these capabilities without opening unnecessary attack surface.

## Decision
Worker pods request `CAP_SYS_ADMIN` + `CAP_SYS_CHROOT` via the container security context. Critically, `privileged: true` is NOT used, as it disables seccomp profiles entirely and grants all capabilities.

The recommended deployment configuration includes:
- Dedicated node pools with taints, so worker pods only schedule on designated nodes.
- A custom seccomp profile that allows the specific syscalls needed (mount, unshare, pivot_root, clone with namespace flags) while blocking everything else.
- Pod security admission at the namespace level to enforce these constraints.

## Alternatives Considered
- **`privileged: true` pods**: Simplest configuration but grants all Linux capabilities, disables seccomp, and gives access to host devices. Unacceptable security posture for a multi-tenant build service.
- **Unprivileged builds with user namespaces only**: Nix's sandbox uses user namespaces, which can work without `CAP_SYS_ADMIN` if the kernel allows unprivileged user namespaces. However, overlayfs still requires `CAP_SYS_ADMIN` in the initial user namespace. Some container runtimes support rootless overlayfs via fuse-overlayfs, but performance is significantly worse.
- **sysbox or Kata Containers runtime**: Specialized container runtimes that provide stronger isolation (VM-level for Kata, enhanced namespacing for sysbox). Adds runtime dependencies, complicates cluster management, and may not be available in managed Kubernetes services.
- **Build inside a nested VM (Firecracker/gVisor)**: Maximum isolation but adds significant startup latency and resource overhead. Nix builds already use namespace-based sandboxing, making VM isolation redundant for most threat models.

## Kubernetes User Namespace Isolation

Kubernetes 1.33 (April 2025) enabled user namespace support by default. Worker pods should set `hostUsers: false` in their pod spec to activate user namespace isolation:

- Container UIDs are remapped to unprivileged host UIDs, so even with `CAP_SYS_ADMIN`, the capability applies only within the user namespace, not on the host
- This significantly reduces the blast radius of a container escape: an attacker gaining `CAP_SYS_ADMIN` inside the pod cannot use it to affect the host or other pods
- `CAP_SYS_ADMIN` is still required (for FUSE mount + overlayfs + Nix sandbox), but its scope is contained to the user namespace
- rio-build requires Kubernetes 1.33+ as the minimum version to ensure user namespace support is available

> **Note:** `hostUsers: false` does NOT eliminate the need for `CAP_SYS_ADMIN` or the custom seccomp profile. It adds a defense-in-depth layer on top of the existing mitigations.

### Interaction with `/dev/fuse` Access (Phase 1a Spike Finding)

The Phase 1a spike discovered two constraints:

1. **`hostUsers: false` + hostPath `/dev/fuse` is incompatible.** The kernel rejects idmap mounts on device nodes, causing the container to fail at startup with `failed to set MOUNT_ATTR_IDMAP on /dev/fuse: invalid argument`. User namespace isolation requires injecting `/dev/fuse` without the hostPath volume mechanism — containerd `base_runtime_spec` (OCI `linux.devices`) does this: runc `mknod`s the node inside the container's `/dev` with container-namespace uid/gid.

2. **`CAP_SYS_ADMIN` alone is insufficient for `/dev/fuse` access.** The container's device cgroup does not include the FUSE character device (major 10, minor 229) by default. Without device injection, `privileged: true` is the only way to access `/dev/fuse`, which contradicts this ADR's recommendation. containerd `base_runtime_spec` resolves both constraints: it adds the device to the cgroup allowlist (OCI `linux.resources.devices`) AND avoids the hostPath volume, enabling both `hostUsers: false` and the non-privileged security context.

3. **`hostUsers: false` requires the runtime to chown the pod cgroup.** The worker creates sub-cgroups under its cgroup-namespace root for per-build resource tracking. Per the OCI runtime-spec, runc chowns the container's cgroup to the userns root UID only when the OCI config mounts `/sys/fs/cgroup` read-write. containerd passes `ro` for unprivileged containers unless `cgroup_writable = true` is set on the runc runtime section ([containerd#11131](https://github.com/containerd/containerd/pull/11131), v2.1+). Without the chown, `/sys/fs/cgroup/` appears as `nobody:nobody` inside the user namespace and the worker's `mkdir` fails `EACCES` — the rw remount in `rio-worker/src/cgroup.rs` fixes the mount flag but cannot fix inode ownership (`CAP_DAC_OVERRIDE` does not apply to unmapped UIDs). The NixOS node AMI ([ADR-021](021-nixos-node-ami.md)) sets `cgroup_writable = true` directly via `virtualisation.containerd.settings`, so EKS deployments default to `hostUsers: false`. (Historically: Bottlerocket's settings API did not expose this key, forcing `hostUsers: true` on the WorkerPool spec — that override was removed with the Bottlerocket→NixOS cutover.)

## Consequences
- **Positive**: Workers get exactly the capabilities they need without excessive privilege.
- **Positive**: Custom seccomp profile limits syscall surface to what is actually required.
- **Positive**: Dedicated node pools with taints prevent worker pods from affecting other workloads.
- **Negative**: Requires cluster-level configuration (node pools, taints, seccomp profiles) that complicates initial setup.
- **Negative**: `CAP_SYS_ADMIN` is a broad capability. The seccomp profile is the real security boundary.
- **Negative**: Not compatible with restrictive PodSecurityStandard `restricted` profile; requires `privileged` or `baseline` with custom exceptions.

## Implementation

The controller-generated pod spec (`rio-controller/src/reconcilers/workerpool/builders.rs`) matches the recommended configuration above:

- **`/dev/fuse` via containerd `base_runtime_spec`.** `nix/base-runtime-spec.nix` declares `/dev/{fuse,kvm}` in OCI `linux.devices` + `linux.resources.devices`; containerd's runc runtime is pointed at it (`nix/nixos-node/containerd-config.nix` on the NixOS AMI per [ADR-021](021-nixos-node-ami.md) §7; `services.k3s.containerdConfigTemplate` on the k3s VM fixture). Every pod gets both unconditionally. No hostPath volume — `hostUsers: false` works.
- **`hostUsers: false` set.** User-namespace isolation active on non-privileged pods. `CAP_SYS_ADMIN` is scoped to the user namespace; a container escape cannot use it on the host.
- **Helm chart default is `workerPool.privileged: false`.** No device plugin runs; no extended resource is requested.

The `WorkerPool` CRD exposes an optional `privileged: bool` field (`rio-crds/src/workerpool.rs`). When unset or `false` (production default), the container gets the granular `SYS_ADMIN` + `SYS_CHROOT` capabilities and `hostUsers: false`. When `true`, the container runs fully privileged with the hostPath `/dev/fuse` fallback — an escape hatch for clusters whose default seccomp profiles block `mount(2)` even with `SYS_ADMIN`, or whose containerd lacks idmap-mount support. Production deployments on EKS/GKE should not need this.

### Seccomp Profile Distribution

The custom Localhost profile is the same regardless of cluster (the JSON lives at `infra/helm/rio-build/files/seccomp-rio-{builder,fetcher}.json`; the chart's `localhostProfile` default `operator/rio-builder.json` is the path under `/var/lib/kubelet/seccomp/` where the profile must land on every node). HOW the file gets there is provider-specific:

| Provider | Mechanism | Per-pod cost | Chart values |
|---|---|---|---|
| EKS (NixOS AMI) | baked into the AMI via `systemd.tmpfiles` (`nix/nixos-node/hardening.nix`) | none | `controller.seccompPreinstalled=true` (set by `xtask k8s eks deploy`) |
| Any K8s ≥ 1.33 | security-profiles-operator (`SeccompProfile` CR + `spod` DaemonSet) | `wait-seccomp` initContainer polls until the file appears (5–15s on a fresh node) | `securityProfilesOperator.enabled=true` (default `false`) |
| k3s / kind | n/a — `privileged: true` escape-hatch in use, profile not loaded | none | `controller.privileged=true` |

**Why AMI-baked over SPO on EKS.** The profiles are store paths in the AMI, copied into `/var/lib/kubelet/seccomp/operator/` by `systemd-tmpfiles` BEFORE kubelet starts, so they're in place before any pod can schedule. SPO's `spod` DaemonSet runs concurrently with workload pods; without the `wait-seccomp` init the kubelet would `CreateContainerError` on the pod that races spod onto a fresh Karpenter node. Under ephemeral builders (one Job per derivation, thousands per hour), the init's 5–15s poll dominated cold-start latency, and SPO's controller OOMKilled under sustained node-churn (I-154). AMI-baked eliminates both: zero per-pod overhead, no in-cluster operator competing for memory, no bootstrap container to push.

**Profile-update path.** Under SPO a profile change is a `SeccompProfile` CR edit; spod reconciles every node in place. With the NixOS AMI the profile is a store path in the image; a profile change is `xtask k8s -p eks ami push` + `helm upgrade --set karpenter.amiTag=<new-sha>`. Karpenter Drift detects the resolved-AMI-ID change and rolls nodes. Same blast radius as a spod DaemonSet rollout (every builder/fetcher node) — but the cost is paid once per node lifetime instead of once per pod.

**`seccompPreinstalled` gate.** When `controller.seccompPreinstalled=true`, the controller (`rio-controller/src/reconcilers/common/sts.rs`) omits the `wait-seccomp` initContainer entirely. The setting is a *promise* from the deploy layer that the profile is already on disk when any pod schedules; setting it `true` without a working bootstrap-container (or equivalent node-init) gets `CreateContainerError` on every builder/fetcher pod.

**History.** I-020 (the original 7-minute init hang) → P0540 (rio-seccomp-installer DS → SPO `SeccompProfile` CRs) → I-154 (SPO operator OOM under ephemeral churn) → P0541 (SPO → Bottlerocket bootstrap-container) → ADR-021 (Bottlerocket → NixOS AMI, profiles baked in).

# ADR-012: Privileged Worker Pods

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

1. **`hostUsers: false` + hostPath `/dev/fuse` is incompatible.** The kernel rejects idmap mounts on device nodes, causing the container to fail at startup with `failed to set MOUNT_ATTR_IDMAP on /dev/fuse: invalid argument`. User namespace isolation requires a FUSE device plugin (e.g., `smarter-device-manager`) that injects `/dev/fuse` without the hostPath volume mechanism.

2. **`CAP_SYS_ADMIN` alone is insufficient for `/dev/fuse` access.** The container's device cgroup does not include the FUSE character device (major 10, minor 229) by default. Without a device plugin, `privileged: true` is the only way to access `/dev/fuse`, which contradicts this ADR's recommendation. A FUSE device plugin resolves both constraints: it adds the device to the cgroup allowlist AND avoids the hostPath volume, enabling both `hostUsers: false` and the non-privileged security context.

## Consequences
- **Positive**: Workers get exactly the capabilities they need without excessive privilege.
- **Positive**: Custom seccomp profile limits syscall surface to what is actually required.
- **Positive**: Dedicated node pools with taints prevent worker pods from affecting other workloads.
- **Negative**: Requires cluster-level configuration (node pools, taints, seccomp profiles) that complicates initial setup.
- **Negative**: `CAP_SYS_ADMIN` is a broad capability. The seccomp profile is the real security boundary.
- **Negative**: Not compatible with restrictive PodSecurityStandard `restricted` profile; requires `privileged` or `baseline` with custom exceptions.

## Implementation

The controller-generated pod spec (`rio-controller/src/reconcilers/workerpool/builders.rs`) matches the recommended configuration above:

- **`/dev/fuse` via device plugin.** The `smarter-device-manager` DaemonSet (`infra/helm/rio-build/templates/device-plugin.yaml`) exposes `/dev/fuse` as an extended resource `smarter-devices/fuse`. The worker container requests it via `resources.limits["smarter-devices/fuse"] = 1`; the kubelet+plugin inject the device node and add it to the container's device cgroup allowlist. No hostPath volume — `hostUsers: false` works.
- **`hostUsers: false` set.** User-namespace isolation active on non-privileged pods. `CAP_SYS_ADMIN` is scoped to the user namespace; a container escape cannot use it on the host.
- **Helm chart default is `workerPool.privileged: false`.** The device plugin is enabled by default (`devicePlugin.enabled: true`).

The `WorkerPool` CRD exposes an optional `privileged: bool` field (`rio-crds/src/workerpool.rs`). When unset or `false` (production default), the container gets the granular `SYS_ADMIN` + `SYS_CHROOT` capabilities, `hostUsers: false`, and the FUSE device-plugin resource. When `true`, the container runs fully privileged with the hostPath `/dev/fuse` fallback — an escape hatch for k3s/kind clusters whose default seccomp profiles block `mount(2)` even with `SYS_ADMIN`, or whose containerd lacks idmap-mount support. Production deployments on EKS/GKE with the device plugin deployed should not need this.

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

## Consequences
- **Positive**: Workers get exactly the capabilities they need without excessive privilege.
- **Positive**: Custom seccomp profile limits syscall surface to what is actually required.
- **Positive**: Dedicated node pools with taints prevent worker pods from affecting other workloads.
- **Negative**: Requires cluster-level configuration (node pools, taints, seccomp profiles) that complicates initial setup.
- **Negative**: `CAP_SYS_ADMIN` is a broad capability. The seccomp profile is the real security boundary.
- **Negative**: Not compatible with restrictive PodSecurityStandard `restricted` profile; requires `privileged` or `baseline` with custom exceptions.

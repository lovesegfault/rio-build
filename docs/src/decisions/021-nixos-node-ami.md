# ADR-021: NixOS EKS Worker Node AMI

## Status

Accepted (P1 spike)

## Context

Builder/fetcher Karpenter NodePools currently run `amiAlias: bottlerocket@latest`. Bottlerocket gives us a minimal, dm-verity-signed, read-only root with a TOML settings API that Karpenter populates via userData. It does **not** give us:

- **Kernel control.** Bottlerocket ships a fixed kernel; the per-page FUSE / `EROFS_FS_ONDEMAND` / `CACHEFILES_ONDEMAND` work needs custom Kconfig and (later) an out-of-tree `riofs` kmod. There is no supported path to either on Bottlerocket.
- **`cgroup_writable = true` on the runc runtime.** [ADR-012 §3](012-privileged-builder-pods.md) documents that Bottlerocket's settings API does not expose this containerd key, so the BuilderPool runs `hostUsers: true` on EKS today. That's the largest remaining gap between the EKS deploy and the privileged-hardening VM test.
- **Reproducibility.** `bottlerocket@latest` is a moving alias. The node image is the only deploy artifact that isn't a content-addressed flake output.

A NixOS AMI built as `.#node-ami-<arch>` makes the node image a derivation alongside `docker-*`: same reproducibility, same `nix build` workflow, same multi-arch story. The kernel becomes a `boot.kernelPatches.extraStructuredConfig` block; containerd config becomes `virtualisation.containerd.settings`.

## Decision

Build the worker-node AMI from nixpkgs `maintainers/scripts/ec2/amazon-image.nix`, package `awslabs/amazon-eks-ami/nodeadm` for cluster bootstrap, and declare `amiFamily: AL2023` on the EC2NodeClass so Karpenter emits the same NodeConfig MIME userData it would for a real AL2023 node. Tag-select the AMI by `rio.build/ami=<git-sha>`; `cargo xtask k8s -p eks ami push` builds, `coldsnap`-uploads, registers, and tags.

Key choices:

1. **`amiFamily: AL2023`, not `Custom`.** Under `Custom`, Karpenter passes no cluster info at all — re-implementing IMDSv2, max-pods-per-ENI, IPv6 cluster handling, and the `aws:///<az>/<instance-id>` providerID format is ~3k LoC of edge cases nodeadm already owns. Karpenter validates only that `amiSelectorTerms` resolve to ≥1 AMI, not that the AMI *is* AL2023; the family controls userData generation, nothing else.
2. **Thin `services.rio.eksNode` module, not nixpkgs `services.kubernetes.kubelet`.** The nixpkgs module assumes a self-managed cluster (PKI generation, kubeconfig rendering). Here all kubelet config is nodeadm's output; the NixOS unit is ~40 lines pointing at the files nodeadm wrote.
3. **`nix.enable = false` on the node.** Builds run inside the builder *pod*, which carries its own `nix`. Dropping the daemon saves ~80 MB closure and removes a root-socket attack surface. Debugging is `kubectl debug node/…` + SSM Session Manager — neither needs an on-image Nix.
4. **Pinned kernel minor in `nix/pins.nix`**, not `linuxPackages_latest`. A nixpkgs flake-input bump can't surprise-rebuild the ~40 min kernel derivation; bump deliberately when the per-page-FUSE work needs a particular patch level.
5. **Both arches from P1.** The NodePool requirements already span x86_64 + aarch64; a single-arch AMI would leave arm64 pods on Bottlerocket during migration with two userData formats live at once.
6. **Seccomp profiles move to `nix/nixos-node/seccomp/`** as the canonical source (the AMI bakes them in P2 via `systemd.tmpfiles` symlinks — no bootstrap container). For P1 the helm-chart copies remain (`.Files.Get` can't reach outside the chart dir under `cleanSource`); the `seccomp-fresh` flake check fails CI on drift. P2 collapses to a single source via `helm-render.nix`.

## Security posture vs Bottlerocket

| Property | Bottlerocket | NixOS node | Net |
|---|---|---|---|
| dm-verity root | yes | **no** | **loss** — irrelevant under ephemeral single-build-per-node (~5 min lifetime, no persistent state). An attacker with root-on-node already owns the build output; dm-verity protects against persistence across reboots, which doesn't apply. |
| Read-only system | `/usr` ro | `/nix/store` ro (stage-2 bind-mount); `/etc` is store symlinks | wash |
| Kernel lockdown LSM | `integrity` | off in P1; `lockdown=integrity` + `security.lockKernelModules` once P3's kmod list is final | deferred |
| SELinux | enforcing | n/a | loss (out of scope) |
| `hostUsers: false` deployable | **no** (ADR-012 §3) | **yes** — `virtualisation.containerd.settings…cgroup_writable = true` | **gain** |
| Package attack surface | minimal | minimal — `environment.defaultPackages = []`, `documentation.enable = false`, `nix.enable = false`, no sshd | wash |

**Verdict:** acceptable for ephemeral builder/fetcher pools. The dm-verity loss is real but not load-bearing under the single-build-per-pod lifetime; the `hostUsers: false` unlock is the bigger swing. The `system` managed nodegroup (long-lived, runs Karpenter/coredns) **stays AL2023 managed** — explicitly out of scope.

## Rollback

Two EC2NodeClasses coexist; NodePools pick via `nodeClassRef`. `helm upgrade … --set karpenter.nixosAmi.enabled=false` removes the `rio-nixos` class → Karpenter Drift rolls every NixOS node back to Bottlerocket within one consolidation cycle. No terraform changes to undo (node IAM role is shared). For the P1 canary specifically, `weight: 0` + `NoSchedule` taint means no production pod ever lands there — disabling is zero-blast-radius.

## Consequences

- **Positive:** kernel Kconfig + out-of-tree kmods become a `nix build` away. Unblocks the `riofs` / EROFS-ondemand track.
- **Positive:** `hostUsers: false` works on EKS (ADR-012 §3 caveat becomes historical in P2).
- **Positive:** node image is content-addressed; `karpenter.nixosAmi.tag=<sha>` pins it the same way `global.image.tag` pins the pod images.
- **Negative:** kernel rebuilds (~40 min cold) on every `extraStructuredConfig` change. Cached after first build; `packages.node-kernel` (P3) lets CI prebuild it independently of the disk image.
- **Negative:** AMI-per-SHA snapshot storage. ~$0.40/mo per 8 GB snapshot; `xtask ami gc --keep 5` (P2) bounds it to ~$2/mo.
- **Negative:** another moving part in `xtask k8s eks up` (`ami push` between `push` and `deploy`).

## Phasing

| Phase | Exit | Scope |
|---|---|---|
| **P1** (this ADR) | `nix build .#node-ami-<arch>` produces an image; canary NodePool renders; a hand-registered AMI joins the cluster and goes `Ready` | `nix/nixos-node/{default,nodeadm,eks-node,minimal}.nix`, flake targets, `rio-nixos` EC2NodeClass + canary NodePool, `xtask ami push` |
| P2 | `xtask k8s eks up` produces NixOS builder/fetcher nodes; `hostUsers: false` active; Bottlerocket pool at weight 50 | `hardening.nix` (sysctl/seccomp/cgroup_writable), `device-plugin.nix`, VM test, deploy.rs wiring |
| P3 | kernel with `CONFIG_EROFS_FS_ONDEMAND=y`; `riofs.ko` placeholder builds | `kernel.nix`, `kmod/` |

## References

- [PLAN-NIXOS-NODE.md](../../../PLAN-NIXOS-NODE.md) — full design doc (D1–D7, risks, prior art)
- [ADR-012](012-privileged-builder-pods.md) §3 — the `cgroup_writable` gap this closes
- nixpkgs [`amazon-image.nix`](https://github.com/NixOS/nixpkgs/blob/master/nixos/maintainers/scripts/ec2/amazon-image.nix)
- [`awslabs/amazon-eks-ami` `nodeadm/`](https://github.com/awslabs/amazon-eks-ami/tree/main/nodeadm)
- [`awslabs/coldsnap`](https://github.com/awslabs/coldsnap)

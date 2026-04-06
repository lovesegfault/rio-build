# ADR-021: NixOS EKS Worker Node AMI

## Status

Accepted (full cutover — Bottlerocket removed)

## Context

Builder/fetcher Karpenter NodePools previously ran `amiAlias: bottlerocket@latest`. Bottlerocket gave a minimal, dm-verity-signed, read-only root with a TOML settings API that Karpenter populated via userData. It did **not** give us:

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
6. **Seccomp profiles canonical at `nix/nixos-node/seccomp/`.** The AMI bakes them via `systemd.tmpfiles` (`hardening.nix`) — no bootstrap container, no SPO DaemonSet. The helm-chart `files/seccomp-rio-*.json` copies remain for the k3s/kind path (`.Files.Get` can't reach outside the chart dir under `cleanSource`); the `seccomp-fresh` flake check fails CI on drift.
7. **smarter-device-manager runs as a host systemd unit.** Packaged from source (`nix/nixos-node/smarter-device-manager/`), not a static pod or DaemonSet — no registry pull, no kubelet-manages-its-own-dependency loop. Registers on `/var/lib/kubelet/device-plugins/kubelet.sock` as soon as kubelet is up. The Karpenter NodeOverlay still declares synthetic `smarter-devices/{fuse,kvm}` capacity (cold-start bin-packing happens before any node exists).

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

There is one EC2NodeClass (`rio-default`, NixOS). Rollback to a known-good AMI is `helm upgrade … --set karpenter.amiTag=<prior-sha>` → Karpenter Drift rolls every node within one consolidation cycle. No terraform changes to undo (node IAM role is shared). Full revert to Bottlerocket would mean reverting this commit — we control the only deployment, so a dual-stack toggle is dead weight.

## Consequences

- **Positive:** kernel Kconfig + out-of-tree kmods become a `nix build` away. Unblocks the `riofs` / EROFS-ondemand track.
- **Positive:** `hostUsers: false` works on EKS (ADR-012 §3 caveat is historical).
- **Positive:** node image is content-addressed; `karpenter.amiTag=<sha>` pins it the same way `global.image.tag` pins the pod images.
- **Negative:** kernel rebuilds (~40 min cold) on every `extraStructuredConfig` change. Cached after first build; `packages.node-kernel` (P3) lets CI prebuild it independently of the disk image.
- **Negative:** AMI-per-SHA snapshot storage. ~$0.40/mo per 8 GB snapshot; `xtask ami gc --keep 5` (P2) bounds it to ~$2/mo.
- **Negative:** another moving part in `xtask k8s eks up` (`ami push` between `push` and `deploy`).

## Prebaked executor layer cache

The AMI bakes a single multi-manifest OCI archive containing the `rio-builder` and `rio-fetcher` images (deduplicated layers — they share every layer, only `config.Env` differs) and imports it into containerd's content store at kubelet `preStart`. PodSpec refs stay `<ECR>/rio-{builder,fetcher}:<git-sha>`; the seed is a content-store warm only. On first pod schedule, containerd checks each ECR-manifest layer by digest against the local store and fetches only the absent ones. Net: per-fresh-node ECR pull drops from ~400 MB to the delta since the AMI was cut (typically one ~10 MB layer).

**Rejected alternative — `localhost/` digest-pin:** PodSpec references the seed's local digest directly, `imagePullPolicy: Never`. Gives a fail-loud `ErrImagePull` on AMI/deploy drift and zero ECR dependency for executor pods. Rejected because every `rio-builder` code change becomes a 10–15 min AMI rebake instead of `up --push --deploy`; the layer-cache design degrades gracefully (delta-only pull) instead of failing hard.

**Consequence:** the AMI closure now includes `rio-workspace`, so `ami_tag()` changes per `rio-builder` commit. `.rio-ami-tag` (written by the last `up --ami`) keeps `up --deploy` on the registered AMI; the seed serving ~58/59 layers makes the staleness cheap. Rebake when `nix/docker.nix` `extraContents` changes (new layer the seed lacks) or opportunistically to keep the delta near zero.

## Phasing

| Phase | Exit | Scope |
|---|---|---|
| P1 (landed) | `nix build .#node-ami-<arch>` produces an image; a hand-registered AMI joins the cluster and goes `Ready` | `nix/nixos-node/{default,nodeadm,eks-node,minimal}.nix`, flake targets, `xtask ami push` |
| **P2** (this commit) | `xtask k8s eks up` produces NixOS builder/fetcher nodes; `hostUsers: false` active; Bottlerocket removed | `hardening.nix` (sysctl/seccomp), systemd device-plugin, kernel `EROFS_FS_ONDEMAND`/`CACHEFILES_ONDEMAND`, `karpenter.amiTag`, deploy.rs wiring |
| P3 | `riofs.ko` placeholder builds; `node-kernel-config` check | `kmod/`, `packages.node-kernel` |

## References

- [PLAN-NIXOS-NODE.md](../../../PLAN-NIXOS-NODE.md) — full design doc (D1–D7, risks, prior art)
- [ADR-012](012-privileged-builder-pods.md) §3 — the `cgroup_writable` gap this closes
- nixpkgs [`amazon-image.nix`](https://github.com/NixOS/nixpkgs/blob/master/nixos/maintainers/scripts/ec2/amazon-image.nix)
- [`awslabs/amazon-eks-ami` `nodeadm/`](https://github.com/awslabs/amazon-eks-ami/tree/main/nodeadm)
- [`awslabs/coldsnap`](https://github.com/awslabs/coldsnap)

# Vendored upstream Kubernetes manifests

Manifests here are applied by `xtask k8s deploy` BEFORE the rio helm
chart. They are NOT helm subcharts because either (a) the upstream
project doesn't publish a chart to a registry, or (b) the install
ordering doesn't fit subchart semantics.

## security-profiles-operator.yaml

From `https://raw.githubusercontent.com/kubernetes-sigs/security-profiles-operator/{spo_version}/deploy/operator.yaml`
(version pinned in `nix/pins.nix`).

Distributes the rio-builder / rio-fetcher seccomp profiles via
`SeccompProfile` CRs (rendered by `infra/helm/rio-build/templates/
seccomp-profiles.yaml`). The spod DaemonSet writes them to
`/var/lib/kubelet/seccomp/operator/{name}.json` on every node.

Prerequisite: cert-manager (tofu-managed, `infra/eks/addons.tf`).

**Local patches applied** (re-apply on bump until upstream releases include them):

- `openat2` + `statx` added to spod's own seccomp profile (the
  `security-profiles-operator-profile` ConfigMap, both copies). Upstream
  PR #3041 (merged 2025-11-14, not in v0.10.0). Without this, runc ≥1.3's
  `/proc/thread-self/fd` overmount check (`statx(STATX_MNT_ID_UNIQUE)`)
  is denied → containerd `cannot start a stopped process`.

- `spod-config.yaml` (separate file, applied via SSA after operator
  startup): `selinuxTypeTag: super_t` for Bottlerocket (default `spc_t`
  doesn't exist in Bottlerocket's SELinux policy) + nodeAffinity to
  builder/fetcher nodes only.

To bump:

```sh
v=$(nix eval --raw -f nix/pins.nix spo_version)
curl -sL "https://raw.githubusercontent.com/kubernetes-sigs/security-profiles-operator/${v}/deploy/operator.yaml" \
  -o infra/k8s/security-profiles-operator.yaml
# If upstream release < the PR-merge dates above, re-apply patches:
sed -i -e '/^ *"openat",$/a\            "openat2",' \
       -e '/^ *"stat",$/a\            "statx",' \
       infra/k8s/security-profiles-operator.yaml
```

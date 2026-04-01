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

To bump:

```sh
v=$(nix eval --raw -f nix/pins.nix spo_version)
curl -sL "https://raw.githubusercontent.com/kubernetes-sigs/security-profiles-operator/${v}/deploy/operator.yaml" \
  -o infra/k8s/security-profiles-operator.yaml
```

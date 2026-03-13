# rio-build Helm chart

Replaces the old `infra/k8s/` kustomize overlays. Deployed via
`helm upgrade --install` from the working tree (`just eks deploy`).

## Values layering

| Layer | Source | Set by |
|---|---|---|
| Defaults | `values.yaml` | git (prod topology: TLS on, external PG/S3, PDBs on) |
| Infra values | `helm upgrade --set` | `just eks deploy` reads `tofu output` (ECR registry, IRSA ARNs, bucket, Aurora) |
| Image tag | `helm upgrade --set global.image.tag=` | `just eks deploy` reads `.rio-image-tag` (from `just eks push`, never git) |

## Local rendering

```sh
# Prod profile (needs tag set)
helm template rio . --set global.image.tag=test

# Dev profile (in-cluster PG + MinIO subcharts)
helm template rio . -f values/dev.yaml

# VM test profile (controller only)
helm template rio . -f values/vmtest.yaml
```

## Values profiles

| Profile | TLS | PG | S3 | Replicas | Used by |
|---|---|---|---|---|---|
| `values.yaml` (default) | on | external | external | 2 | `just eks deploy` (EKS) |
| `values/dev.yaml` | off | bitnami subchart | bitnami MinIO | 1 | `just dev apply` (local k3s/kind) |
| `values/vmtest.yaml` | off | n/a | n/a | controller only | `nix/helm-render.nix` → VM tests |

## CRDs

`crds/` is generated: `nix build .#crds && ./scripts/split-crds.sh result`.
Helm's `crds/` semantics (install-once, never upgrade) are wrong for a
dev-phase project — `just eks deploy` runs `kubectl apply --server-side`
on `infra/helm/crds/` before `helm upgrade` so schema changes land.

## Subcharts

`charts/` is **gitignored**. `nix/helm-charts.nix` fetches the bitnami
PG + MinIO charts as FODs (hash-pinned `helm pull` wrapped as a fixed-
output derivation) — works in the nix sandbox, no binaries in git. The
`helm-lint` flake check symlinks them into `charts/` before running.

`just eks deploy` and `just dev apply` both set this up automatically.
For manual `helm template` outside those recipes:
```sh
mkdir -p charts
ln -sfn $(nix build --no-link --print-out-paths .#helm-postgresql) charts/postgresql
```

To bump a chart version: edit `nix/helm-charts.nix`, build once with the
old hash, copy the `got:` hash from the error.

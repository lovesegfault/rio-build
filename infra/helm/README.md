# rio-build Helm chart

Deployed via `helm upgrade --install` from the working tree
(`cargo xtask k8s -p eks up --deploy`).

## Values layering

| Layer | Source | Set by |
|---|---|---|
| Defaults | `values.yaml` | git (prod topology: TLS on, external PG/S3, PDBs on) |
| Infra values | `helm upgrade --set` | xtask reads `tofu output` (ECR registry, IRSA ARNs, bucket, Aurora) |
| Image tag | `helm upgrade --set global.image.tag=` | xtask reads `.rio-image-tag` (written by `up --push`, never git) |

## Local rendering

```sh
# Prod profile (needs tag set)
helm template rio . --set global.image.tag=test

# Dev profile (in-cluster PG subchart + Rook Ceph RGW)
helm template rio . -f values/dev.yaml

# VM test profile (controller only)
helm template rio . -f values/vmtest-full.yaml
```

## Values profiles

| Profile | TLS | PG | S3 | Replicas | Used by |
|---|---|---|---|---|---|
| `values.yaml` (default) | on | external | external | 2 | `cargo xtask k8s -p eks up --deploy` |
| `values/dev.yaml` | off | bitnami subchart | Rook Ceph RGW | 1 | `cargo xtask k8s -p k3s up` (local k3s) |
| `values/vmtest-full.yaml` | on | bitnami subchart | inline | 1-2 | `nix/helm-render.nix` → VM tests |

## CRDs

`crds/` is generated: `cargo xtask regen crds`.
Helm's `crds/` semantics (install-once, never upgrade) are wrong for a
dev-phase project — xtask runs `kubectl apply --server-side` on
`infra/helm/crds/` before `helm upgrade` so schema changes land.

## Subcharts

`charts/` is **gitignored**. `nix/helm-charts.nix` fetches the bitnami
PG chart as an FOD (hash-pinned `helm pull` wrapped as a fixed-output
derivation) — works in the nix sandbox, no binaries in git. The
`helm-lint` flake check symlinks it into `charts/` before running.

Dev S3 (Rook Ceph RGW) is **not** a subchart — operator-first-then-cluster
lifecycle doesn't fit; `cargo xtask k8s -p k3s up` installs it as separate
helm releases.

xtask sets this up automatically for both providers. For manual
`helm template` outside xtask:
```sh
mkdir -p charts
ln -sfn $(nix build --no-link --print-out-paths .#helm-postgresql) charts/postgresql
```

To bump a chart version: edit `nix/helm-charts.nix`, build once with the
old hash, copy the `got:` hash from the error.

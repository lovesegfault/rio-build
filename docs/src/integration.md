# Integration Patterns

rio-build is a build execution backend, not a CI/CD system. This page describes how to integrate it with external tooling for common workflows.

## Authentication Setup

### SSH Key Configuration

1. Generate an ed25519 key pair for each user/team:
   ```bash
   ssh-keygen -t ed25519 -f ~/.ssh/rio_key -N ""
   ```

2. Add the public key to the gateway's `authorized_keys` with a tenant annotation:
   ```
   ssh-ed25519 AAAA... team-infra
   ```

3. Configure the Nix client to use the key:
   ```bash
   # In ~/.config/nix/nix.conf or via NIX_SSHOPTS
   export NIX_SSHOPTS="-i ~/.ssh/rio_key"
   ```

### Client-Side `nix.conf`

For remote store usage, no special client configuration is needed beyond SSH access. For binary cache substitution, add:

```nix
# /etc/nix/nix.conf or flake.nix nixConfig
substituters = https://rio-cache.example.com https://cache.nixos.org
trusted-public-keys = rio-cache.example.com-1:AAAA... cache.nixos.org-1:BBBB...
```

## Direct Use: Interactive Developer Builds

The simplest integration --- a developer runs builds directly:

```bash
# Remote store mode (full DAG visibility, optimal scheduling)
nix build --store ssh-ng://rio:2222 .#myPackage

# Remote builder mode (per-derivation delegation, works with any Nix setup)
nix build --builders 'ssh-ng://rio:2222 x86_64-linux' .#myPackage

# Binary cache substitution (read-only, for fetching pre-built outputs)
nix build --substituters https://rio-cache.example.com .#myPackage
```

## CI/CD: GitHub Actions + rio-build

Use GitHub Actions for evaluation and triggering, rio-build for execution:

```yaml
# .github/workflows/build.yml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: cachix/install-nix-action@v30
      - name: Build via rio-build
        run: nix build --store ssh-ng://rio.internal:2222 .#default
        env:
          NIX_SSHOPTS: "-i ${{ secrets.RIO_SSH_KEY }}"
```

## CI/CD: Periodic Rebuilds with nix-eval-jobs

For Hydra-style periodic evaluation of large package sets:

```bash
#!/usr/bin/env bash
# Evaluate nixpkgs to discover what needs building
nix-eval-jobs --flake .#packages.x86_64-linux | \
  jq -r '.drvPath' | \
  xargs -P4 -I{} nix build --store ssh-ng://rio:2222 {}
```

For a more sophisticated setup, use `nix-eval-jobs` with `--check-cache-status` to skip already-cached derivations, and parse the JSON output to submit builds with appropriate priorities.

## Kubernetes: Build CRD

For programmatic build submission from within the cluster:

```yaml
apiVersion: rio.build/v1alpha1
kind: Build
metadata:
  name: nightly-nixpkgs
spec:
  derivation: /nix/store/abc...-hello.drv   # must be a store path
  priority: 50
  timeout: 7200s
  tenant: ci-team
```

Note: The `derivation` field must be a valid store path. Evaluation is external to rio-build (see [Non-Goals](./introduction.md#non-goals)). The `.drv` file must already exist in rio-store (uploaded via `wopAddToStoreNar` through a gateway session or `nix copy`).

## Pre-Populating the Store: `nix copy`

To seed rio-store with existing build outputs (e.g., from a local build or another cache):

```bash
# Copy a specific output to rio-store
nix copy --to ssh-ng://rio:2222 ./result

# Copy an entire closure (including all runtime dependencies)
nix copy --to ssh-ng://rio:2222 nixpkgs#hello

# Copy from another binary cache to rio-store
nix copy --from https://cache.nixos.org --to ssh-ng://rio:2222 nixpkgs#hello
```

This is useful for bootstrapping a new rio-build deployment with commonly-used packages (glibc, coreutils, stdenv) to avoid cold-cache latency on first builds.

## Multi-Architecture Builds

rio-build supports multiple architectures via separate worker pools. Each `WorkerPool` CRD targets a specific `system` (e.g., `x86_64-linux`, `aarch64-linux`). The scheduler matches derivation `system` to workers with compatible capabilities.

```bash
# Build for a specific architecture (requires workers with matching system)
nix build --store ssh-ng://rio:2222 --system aarch64-linux .#myPackage
```

> **Note:** Cross-compilation (building aarch64 packages on x86_64 workers via `binfmt_misc` / QEMU) is not explicitly supported. Workers should run on native hardware matching their declared `system`. For cross-compilation workflows, use Nix's cross-compilation support (`crossSystem`) on matching workers.

## Binary Cache as Substituter

rio-store's binary cache is compatible with the standard Nix substituter protocol. Configure it alongside other caches (see [Client-Side nix.conf](#client-side-nixconf) above for the configuration snippet).

## Monitoring Integration

### Prometheus Scrape Config

All rio-build components expose a `/metrics` endpoint. Configure Prometheus to scrape them:

```yaml
scrape_configs:
  - job_name: rio-build
    kubernetes_sd_configs:
      - role: pod
    relabel_configs:
      - source_labels: [__meta_kubernetes_pod_label_app]
        regex: rio-.*
        action: keep
      - source_labels: [__meta_kubernetes_pod_ip, __meta_kubernetes_pod_annotation_prometheus_io_port]
        separator: ":"
        target_label: __address__
```

### Grafana Dashboards

Recommended dashboards:

| Dashboard | Key Panels |
|-----------|-----------|
| **Build Overview** | Active builds, queue depth, cache hit rate, build duration p50/p95/p99 |
| **Worker Utilization** | CPU/memory per worker, FUSE cache hit ratio, builds/hour |
| **Store Health** | Chunk dedup ratio, S3 request rate, PutPath latency, GC progress |
| **Scheduler** | Assignment latency, critical path accuracy, DAG size distribution |

See [Observability](./observability.md) for the full list of available metrics.

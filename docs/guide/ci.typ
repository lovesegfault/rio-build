#import "/lib/rio.typ": *
#show: rio.with(domains: none)

= CI/CD: GitHub Actions + rio-build

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

= CI/CD: Periodic Rebuilds with nix-eval-jobs

For Hydra-style periodic evaluation of large package sets:

```bash
#!/usr/bin/env bash
# Evaluate nixpkgs to discover what needs building
nix-eval-jobs --flake .#packages.x86_64-linux | \
  jq -r '.drvPath' | \
  xargs -P4 -I{} nix build --store ssh-ng://rio:2222 {}
```

For a more sophisticated setup, use `nix-eval-jobs` with `--check-cache-status` to skip already-cached derivations, and parse the JSON output to submit builds with appropriate priorities.

= Multi-Architecture Builds

rio-build supports multiple architectures via separate #glspl("pool"). Each `Pool` @crd declares a `systems` list (e.g., `["x86_64-linux"]` or `["aarch64-linux"]`). The scheduler matches derivation `system` to executors whose `systems` list contains it, and also requires all derivation `requiredSystemFeatures` to be present in the executor's `features` list. (Darwin builders --- `aarch64-darwin`, `x86_64-darwin` --- are future work; see #cross-link("/intro.typ")[Introduction].)

```bash
# Build for a specific architecture (requires executors with matching system)
nix build --store ssh-ng://rio:2222 --system aarch64-linux .#myPackage
```

#info[
  Cross-compilation (building aarch64 packages on x86_64 executors via `binfmt_misc` / QEMU) is not explicitly supported. Executors should run on native hardware matching their declared `system`. For cross-compilation workflows, use Nix's cross-compilation support (`crossSystem`) on matching executors.
]

= Monitoring Integration

== Prometheus Scrape Config

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

== Grafana Dashboards

Recommended dashboards:

#table(
  columns: 2,
  table.header([Dashboard], [Key Panels]),
  [*Build Overview*],
  [Active builds, queue depth, cache hit rate, build duration p50/p95/p99],

  [*Executor Utilization*],
  [CPU/memory per executor, @fuse cache hit ratio, builds/hour],

  [*Store Health*],
  [Chunk dedup ratio, S3 request rate, PutPath latency, GC progress],

  [*Scheduler*],
  [Assignment latency, critical path accuracy, @dag size distribution],
)

See #cross-link("/spec/system/observability.typ")[Observability] for the full list of available metrics.

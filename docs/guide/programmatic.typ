#import "/lib/rio.typ": *
#show: rio.with(domains: none)

= Programmatic build submission: gRPC

For build submission from within the cluster or automation, use the `SubmitBuild` RPC directly:

```bash
# Via grpcurl (low-level)
grpcurl -plaintext -d '{"nodes": [{"drv_path": "/nix/store/abc...-hello.drv", "system": "x86_64-linux"}], "priority_class": "ci", "tenant_name": "ci-team"}' \
  rio-scheduler:9001 rio.scheduler.SchedulerService/SubmitBuild
```

(rio-cli has no `submit` subcommand --- use `nix build --store ssh-ng://…` for
the canonical client path, or `grpcurl` as above for raw RPC.)

Note: The derivation must be a valid #gls("store-path"). Evaluation is external to rio-build (see #cross-link("/intro.typ")[Non-Goals]). The `.drv` file must already exist in rio-store (uploaded via `wopAddToStoreNar` through a gateway session or `nix copy`).

= Pre-Populating the Store: `nix copy`

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

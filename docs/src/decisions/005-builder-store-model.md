# ADR-005: Builder Store Model (FUSE + Overlay + Synthetic SQLite DB)

## Status
Accepted

## Context
Nix builds require a populated `/nix/store` with all build inputs present, plus a valid SQLite store database. In a distributed system, workers must access potentially hundreds of gigabytes of store paths without pre-materializing everything. The store model must support concurrent builds with isolation, be Kubernetes-native, and avoid shared mutable state.

## Decision
Each worker runs a custom FUSE filesystem (the `fuse` module in `rio-builder`) mounted at a configurable path (default `/var/rio/fuse-store` — see `rio-builder/src/config.rs`). The FUSE daemon:

- Lazily fetches store path content from rio-store via gRPC on demand.
- Caches fetched content on local SSD with LRU eviction.
- Exploits store path immutability: cached data never needs invalidation.

Each build gets a per-build overlayfs (see `rio-builder/src/overlay.rs`, r[builder.overlay.stacked-lower]):
- **Lower layer:** the FUSE mount only — rio-store-served input paths. The host `/nix/store` is **not** in the lowerdir; the daemon's runtime closure is reached at the host store directly because the daemon runs with `--store 'local?root={build_dir}'` rather than with the overlay bind-mounted at `/nix/store` (see I-060 in [components/builder.md](../components/builder.md#namespace-ordering)).
- **Upper layer:** `{overlay_base_dir}/{build_id}/upper/nix/store/` on a local-disk emptyDir volume (controller-managed). Must be a real filesystem (ext4/xfs), not the container's overlayfs root — overlayfs-as-upperdir cannot create `trusted.*` xattrs and fails with `EINVAL`.
- **Merged:** mounted at `{build_dir}/nix/store`; nix-daemon's `realStoreDir` for the chroot store. Outputs written by `nix-daemon` land in `{upper}/nix/store/{hash}-{name}`.
- A synthetic SQLite store DB at `{build_dir}/nix/var/nix/db` is generated per-build from rio-store's PostgreSQL metadata, containing only the paths relevant to that build.

On completion, built outputs are scanned from the upper layer and uploaded to rio-store.

## Alternatives Considered
- **Shared NFS/EFS ReadWriteMany PersistentVolume**: Shared mutable state across workers. NFS performance under concurrent builds is poor. overlayfs-over-NFS is not a supported kernel configuration. Lock contention on the SQLite DB makes this impractical for concurrent builds.
- **Bind-mount with pre-materialization**: Copy all input paths to local storage before each build. Simple but slow: large closures (e.g., GHC) can be tens of gigabytes. Wastes bandwidth re-copying paths already present from previous builds.
- **Container image layering (store paths as OCI layers)**: Build OCI images containing required store paths and run builds inside them. Creative but OCI layer limits (~128), layer size overhead, and image build latency make this impractical for builds with hundreds of store paths.
- **nix-daemon on each worker with its own store**: Run a real nix-daemon per worker, copying paths in via `nix copy`. Works but duplicates store management logic, requires managing the daemon lifecycle, and the SQLite DB becomes a bottleneck under concurrent builds.

## Consequences
- **Positive**: No shared mutable state. Workers are independently scalable.
- **Positive**: Lazy fetching means builds only transfer the paths they actually access, not the full closure.
- **Positive**: Local SSD cache with LRU eviction gives warm builds near-local performance.
- **Negative**: FUSE adds a layer of complexity and a potential performance bottleneck for I/O-heavy builds.
- **Negative**: Requires CAP_SYS_ADMIN for overlayfs and FUSE, necessitating elevated pod security (see ADR-012).
- **Negative**: Synthetic SQLite DB generation must precisely match Nix's expected schema, requiring careful tracking of upstream changes.

# ADR-005: Worker Store Model (FUSE + Overlay + Synthetic SQLite DB)

## Status
Accepted

## Context
Nix builds require a populated `/nix/store` with all build inputs present, plus a valid SQLite store database. In a distributed system, workers must access potentially hundreds of gigabytes of store paths without pre-materializing everything. The store model must support concurrent builds with isolation, be Kubernetes-native, and avoid shared mutable state.

## Decision
Each worker runs a custom FUSE filesystem (the `fuse` module in `rio-worker`) mounted at a configurable path (default `/var/rio/fuse-store` — see `rio-worker/src/config.rs`). The FUSE daemon:

- Lazily fetches store path content from rio-store via gRPC on demand.
- Caches fetched content on local SSD with LRU eviction.
- Exploits store path immutability: cached data never needs invalidation.

Each build gets a per-build overlayfs (see `rio-worker/src/overlay.rs`, r[worker.overlay.stacked-lower]):
- **Lower layer (stacked):** `/nix/store:{fuse_mount}` — the host's real `/nix/store` first, then the FUSE mount. Host-store-first ensures `nix-daemon` and its runtime dependencies remain reachable after the overlay is bind-mounted at `/nix/store` in the child's mount namespace. FUSE second provides rio-store-served paths.
- **Upper layer:** `{overlay_base_dir}/{build_id}/upper/nix/store/` on a local-disk emptyDir volume (controller-managed). Must be a real filesystem (ext4/xfs), not the container's overlayfs root — overlayfs-as-upperdir cannot create `trusted.*` xattrs and fails with `EINVAL`.
- **Merged:** bind-mounted to `/nix/store` inside a per-build mount namespace. Outputs written by `nix-daemon` land in `{upper}/nix/store/{hash}-{name}`.
- A synthetic SQLite store DB is generated per-build from rio-store's PostgreSQL metadata, containing only the paths relevant to that build.

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

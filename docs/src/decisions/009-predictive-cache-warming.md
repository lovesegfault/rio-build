# ADR-009: Predictive Cache Warming

## Status
Accepted

## Context
Workers use a FUSE filesystem that lazily fetches store paths on demand (ADR-005). Without prefetching, the first file access in a build triggers a synchronous fetch from rio-store, adding latency to the critical path. Builds with large input closures suffer cumulative fetch latency.

## Decision
The scheduler drives FUSE cache pre-warming. When scheduling a derivation to a worker, the scheduler sends prefetch hints for the input closure paths via the bidirectional build execution stream (ADR-011). The worker's FUSE daemon begins fetching these paths into its local SSD cache before the build starts.

This converts serial "fetch then build" into overlapped execution. By the time the build sandbox is set up and the builder process starts accessing store paths, many or all inputs are already cached locally.

Unlike a shared PV approach, each worker manages its own cache independently with no coordination overhead. The scheduler's hints are best-effort: if prefetching is incomplete when the build starts, the FUSE daemon falls back to synchronous on-demand fetching.

## Alternatives Considered
- **No prefetching (pure lazy fetch)**: Simplest approach. Every store path is fetched on first access. Works correctly but adds latency proportional to the number of input paths and their sizes. Particularly painful for cold workers or large closures.
- **Full closure materialization before build start**: Download the entire input closure before starting the build. Guarantees no fetch latency during the build but wastes bandwidth (many paths may not be accessed) and adds a blocking pre-build phase.
- **Worker-side prediction based on history**: Workers predict needed paths from previous builds. Requires worker-side state and heuristics. The scheduler already has the derivation's `inputDrvs` and `inputSrcs`, making server-side prediction both simpler and more accurate.
- **Shared distributed cache (e.g., memcached/Redis) for hot chunks**: Cache popular chunks in a shared tier. Adds infrastructure complexity and a new failure domain. Local SSD cache with prefetching achieves similar hit rates for sequential builds on the same worker.

## Consequences
- **Positive**: Significantly reduces build start latency, especially for large closures on cold workers.
- **Positive**: Best-effort design means prefetch failures are not fatal; the FUSE fallback handles them.
- **Positive**: No additional infrastructure required; piggybacks on the existing build execution stream.
- **Negative**: Prefetch hints consume network bandwidth even if the build is cancelled before starting.
- **Negative**: Scheduler must compute or look up input closures to generate hints, adding scheduling overhead.

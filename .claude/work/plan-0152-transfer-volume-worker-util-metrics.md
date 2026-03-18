# Plan 0152: Transfer-volume counters + worker utilization gauges

## Design

Two observability gaps closed in one commit. **Transfer-volume counters** at each hop give byte-level visibility into data movement: `rio_store_put_path_bytes_total` / `rio_store_get_path_bytes_total` (store-side, incremented by `nar_size`), `rio_worker_upload_bytes_total` / `rio_worker_fuse_fetch_bytes_total` (worker-side output upload + FUSE cache miss), and `rio_gateway_bytes_total{direction=rx|tx}` (SSH channel data forwarded). These answer "where is the bandwidth going" without needing tcpdump.

**Worker utilization gauges** (`rio_worker_cpu_fraction`, `rio_worker_memory_fraction`) are populated by a new `spawn_utilization_reporter` in `cgroup.rs` — a 15-second poll of the **parent cgroup** (whole worker tree: `rio-worker` process + all per-build sub-cgroups + all subprocesses). CPU fraction is `delta cpu.stat usage_usec / wall-clock µs` (1.0 = one core; >1.0 on multi-core); memory fraction is `memory.current / memory.max` (0.0 when max is the unbounded `max` literal). Spawned from `main.rs` after `delegated_root()`.

Rounds 2-3 refined: `57` fixed unbounded memory to emit `0.0` instead of garbage; `65` extracted `compute_fractions` as a pure function with verify tests; `85` flattened a layer of loop indirection; `91` split `compute_fractions` into cpu/memory halves (callers never used both).

## Files

```json files
[
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "spawn_utilization_reporter + compute_fractions; cpu.stat delta + memory.current/max poll"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "spawn utilization reporter after delegated_root()"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "rio_worker_upload_bytes_total on output upload"},
  {"path": "rio-worker/src/fuse/fetch.rs", "action": "MODIFY", "note": "rio_worker_fuse_fetch_bytes_total on cache miss"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "rio_store_put_path_bytes_total by nar_size"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "rio_store_get_path_bytes_total"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "rio_gateway_bytes_total direction-labeled"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "describe_counter!/gauge! for new metrics"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "describe_counter! for transfer-volume"},
  {"path": "rio-gateway/src/lib.rs", "action": "MODIFY", "note": "describe_counter! for bytes_total"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "new metric rows + 2 tracey markers"}
]
```

## Tracey

- `r[impl obs.metric.transfer-volume]` — `14f3602` (store put_path)
- `r[impl obs.metric.worker-util]` — `14f3602` (cgroup.rs); re-tagged in `5882388` after loop flatten
- `r[verify obs.metric.worker-util]` — `05d3f35` (compute_fractions pure-fn test)
- `r[verify obs.metric.transfer-volume]` — added in `a89c7d0` (round 3) and `3330dd9` (VM sweep)

4 marker annotations total (2 impl, 2 verify).

## Entry

- Depends on P0148: phase 3b complete (extends existing cgroup infrastructure from phase 2c)

## Exit

Merged as `14f3602` (1 commit) + refinements in rounds 2-3 (`8a2cc66`, `05d3f35`, `5882388`, `a1df320`). `.#ci` green.

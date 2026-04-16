# SLA-Driven Sizing

See ADR-023 for design rationale. This document is the normative spec.

r[sched.sla.tier-envelope]

r[sched.sla.solve-per-band-cap]

r[sched.sla.model-key-tenant-scoped]

r[sched.sla.fit-nnls]

r[sched.sla.mem-coupled]

r[sched.sla.reactive-floor]
`SchedHint.resource_floor: ResourceFloor { mem_bytes, disk_bytes, deadline_secs }` (default zeros) is the per-dimension reactive floor for cold-start safety. An explicit resource-exhaustion signal (controller-reported `OomKilled`/`EvictedDiskPressure`/`DeadlineExceeded`, worker-reported `CgroupOom`/`TimedOut`) MUST call `bump_floor_or_count`: if the relevant dimension is already at its ceiling (`Ceilings.max_{mem,disk}` / `86400` for deadline), increment `infra_count` (or `timeout_count` for deadline) and return `promoted=false`; otherwise set the dimension to `min(max(floor, last_intent) * 2, ceiling)` and return `promoted=true`. `last_intent` is `state.sched.est_{memory,disk,deadline}_*` snapshotted at dispatch time. `solve_intent_for` MUST clamp its solved (mem, disk) at `resource_floor` before returning. Persisted as `derivations.floor_*` (`M_044`) so failover doesn't reset to zero. No `cores` floor: OOM/DiskPressure are mem/disk under-provision; DeadlineExceeded is a wall-time bound, not a parallelism bound.

r[sched.sla.disk-scalar]

r[sched.sla.explore-saturation-gate]

r[sched.sla.explore-x4-first-bump]

r[sched.sla.explore-freeze]

r[sched.sla.solve-citardauq]

r[sched.sla.solve-reject-not-clamp]

r[sched.sla.headroom-confidence-scaled]

r[sched.sla.reassign-schmitt]

r[sched.sla.hw-ref-seconds]

r[sched.sla.hw-bench-append-only]

r[sched.sla.quantile-geo-lognormal]

r[sched.sla.prior-partial-pool]

r[sched.sla.outlier-mad-reject]

r[sched.sla.override-precedence]

r[sched.sla.cores-reach-nix-build-cores]

r[sched.sla.disk-reaches-ephemeral-storage]

r[sched.sla.intent-from-solve]

The scheduler exposes one `SpawnIntent{intent_id, cores, mem, disk}` per
queued non-FOD derivation in `GetSizeClassStatus`. `cores` is
`ceil(solve_mvp(c_star))` for fitted keys, probe defaults otherwise;
`prefer_local_build` / `enable_parallel_building=false` pin `cores=1`.

r[sched.sla.intent-match]

`intent_id` is the derivation's `drv_hash`. A worker heartbeating
`intent_id == drv_hash` is preferred for that derivation over FIFO
pick-from-queue; on miss (drv re-planned, scheduler restart) dispatch
falls through to the regular overflow walk.

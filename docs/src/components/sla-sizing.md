# SLA-Driven Sizing

See ADR-023 for design rationale. This document is the normative spec.

r[sched.sla.tier-envelope]

r[sched.sla.solve-per-band-cap]

r[sched.sla.model-key-tenant-scoped]

r[sched.sla.fit-nnls]

r[sched.sla.mem-coupled]

r[sched.sla.reactive-floor+2]
`SchedHint.resource_floor: ResourceFloor { mem_bytes, disk_bytes, deadline_secs }` (default zeros) is the per-dimension reactive floor for cold-start safety. An explicit resource-exhaustion signal (controller-reported `OomKilled`/`EvictedDiskPressure`/`DeadlineExceeded`, worker-reported `CgroupOom`/`TimedOut`) MUST call `bump_floor_or_count`: if the relevant dimension is already at its ceiling (`Ceilings.max_{mem,disk}` / `86400` for deadline), increment `infra_count` (or `timeout_count` for deadline) and return `promoted=false`; otherwise set the dimension to `min(max(floor, last_intent) * 2, ceiling)` and return `promoted=true`. `last_intent` is `state.sched.last_intent.{mem,disk,deadline}_*` snapshotted at dispatch time. `solve_intent_for` MUST clamp its solved (mem, disk) at `resource_floor` before returning, and MUST clamp (cores, mem, disk) at `Ceilings.max_{cores,mem,disk}`. Persisted as `derivations.floor_*` (`M_044`) so failover doesn't reset to zero. No `cores` floor: OOM/DiskPressure are mem/disk under-provision; DeadlineExceeded is a wall-time bound, not a parallelism bound.

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

r[sched.sla.cost-leader-edge-reload]
On a false→true leader edge, the cost-table poller MUST reload `sla_ema_state` from PG before its first `persist()`. A failed reload MUST NOT proceed to `persist()` (which would overwrite the previous leader's evolved EMA with this replica's stale startup snapshot); retry on the next tick.

r[sched.sla.ice-ladder-cap]
A single derivation's ICE-backoff ladder MUST NOT exceed `ladder_cap = clamp(⌈max_tier_bound/hw_fallback_after/4⌉, 1, 8)` Pending-watch timeouts; on exhaustion the derivation MUST fall through to band-agnostic dispatch (`solve_intent_for` skips `solve_full`). The fleet-wide `IceBackoff::exhausted()` is unreachable from one build's serial probing because `ICE_TTL=60s < hw_fallback_after≈120s`. Attempted `(band, cap)` cells SHOULD be recorded for forensics.

`hw_class` is bounded at `MAX_HW_CLASS_LEN` (64) chars of `[a-z0-9-]`
(the controller-stamped 4-segment format); longer or out-of-charset
values are rejected with `INVALID_ARGUMENT`.

r[sched.sla.quantile-geo-lognormal]

r[sched.sla.prior-partial-pool]

r[sched.sla.outlier-mad-reject]

r[sched.sla.override-precedence]

r[sched.sla.cores-reach-nix-build-cores]

r[sched.sla.disk-reaches-ephemeral-storage]

r[sched.sla.intent-from-solve]

The scheduler exposes one `SpawnIntent{intent_id, cores, mem, disk}` per
queued derivation (FOD and non-FOD) in `GetSpawnIntents`. `cores` is
`ceil(solve_mvp(c_star))` for fitted keys, probe defaults otherwise;
`prefer_local_build` / `enable_parallel_building=false` pin `cores=1`.

r[sched.admin.spawn-intents.feature-filter]

When `GetSpawnIntentsRequest.filter_features` is set, the returned
`intents` MUST only include Ready derivations whose
`requiredSystemFeatures` is a subset of `features` --- i.e., derivations
an executor advertising exactly `features` would pass `hard_filter`'s
feature check for. Feature-gated pools (`features ≠ ∅`) MUST exclude
derivations with empty `requiredSystemFeatures` --- those are owned by
the featureless pool. The subset check alone (∅ ⊆ anything) would
over-count (I-181). The `kind` filter MUST be applied alongside (the
ADR-019 airgap boundary), and `systems` (when non-empty) MUST intersect
`intent.system` so a per-arch pool sees only its own backlog
(I-107/I-143). Unset `filter_features` (default) = unfiltered,
preserving CLI behavior.

r[sched.sla.intent-match]

`intent_id` is the derivation's `drv_hash`. A worker heartbeating
`intent_id == drv_hash` is preferred for that derivation over FIFO
pick-from-queue; on miss (drv re-planned, scheduler restart) dispatch
falls through to the regular overflow walk.

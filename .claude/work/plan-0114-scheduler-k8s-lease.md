# Plan 0114: Scheduler K8s Lease leader election

## Design

Replaced the `generation: 1` constant from phase 2a with real leader election via Kubernetes Lease. Two commits: the lease loop + readiness gating.

`5d36f95` added `rio-scheduler/src/lease.rs` (367 LOC). Gated on `RIO_LEASE_NAME` env. Unset → non-K8s mode: `is_leader=true` immediately, generation stays at 1. VM tests + single-scheduler deployments: zero behavior change. Set → K8s mode: `LeaderState::pending` (`is_leader=false`) until the lease loop acquires. `try_acquire_or_renew` every 5s, 15s TTL (K8s convention 3:1 ratio — survives 1-2 apiserver hiccups). On acquire transition: `fetch_add(1, Release)` then `is_leader=true`. On lose: `is_leader=false`, **don't** increment (new leader's job). K8s API error: warn + retry, don't flip `is_leader` — we might still hold the lease (apiserver down for everyone); let TTL expiry decide. `dispatch_ready` early-returns if `!is_leader`: standby schedulers merge DAGs (state warm for fast takeover) but don't send assignments. `Relaxed` load — one-pass lag either direction is harmless (DAG merge is idempotent; workers reject stale-gen via heartbeat).

No fencing (`kube-leader-election` is a polling loop). Brief dual-leader on partition is OK: DAG merge dedups by `drv_hash`; workers reject stale-gen assignments after new leader increments. Worst case: derivation builds twice, same output (deterministic). Wasteful but correct. Proper fenced lease (etcd linearizable) not worth the complexity today. `spawn_with_leader`: new `ActorHandle` entry. `leader=None` → always-leader (old `spawn()`). `Some` → inject shared `is_leader` + `generation`. The generation `Arc` is constructed in `main.rs` so both actor and lease task share the same instance — no chicken-and-egg. Rust 2024 gotcha: `std::env::remove_var` is unsafe; test wraps in `unsafe{}` with SAFETY comment (figment `Jail` serializes env access).

`da6ce05` gated readiness on `is_leader` via a health-toggle loop: poll `is_leader` every 1s, `set_serving`/`set_not_serving` on transition (edge-triggered). K8s Service routes only to SERVING pods → only to the leader. Standby stays live (liveness passes) but not ready. Why poll not watch: `AtomicBool` has no async wake-on-change; `tokio::sync::watch` would need lease task + `dispatch_ready` both adapted. 1Hz poll is simpler; K8s probes poll at 5-10s so 1s lag is imperceptible. **Critical** (load-bearing for P0117): K8s `readinessProbe` MUST use `grpc.service: rio.scheduler.SchedulerService` — named service, not empty-string. `set_not_serving` only affects named; `""` stays SERVING after first `set_serving`. Standby would pass on `""` → routed.

## Files

```json files
[
  {"path": "rio-scheduler/src/lease.rs", "action": "NEW", "note": "367 LOC: try_acquire_or_renew loop, 5s/15s ratio, LeaderState enum, spawn_with_leader"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "lease loop spawn gated on RIO_LEASE_NAME; health-toggle loop 1Hz edge-triggered"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "spawn_with_leader; with_leader_flag + with_generation builder methods"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "generation Arc<AtomicU64> shared with lease task"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "early-return if !is_leader; standby merges DAGs but doesn't dispatch"},
  {"path": "rio-scheduler/Cargo.toml", "action": "MODIFY", "note": "kube, kube-leader-election, k8s-openapi deps; figment dev-dep (Jail)"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[sched.lease.k8s]`, `r[sched.lease.generation-fence]`, `r[sched.lease.dual-leader-ok]`, `r[sched.health.leader-gated]`. P0127's docs audit rewrote `scheduler.md`'s leader election section to match this impl (`7e066e8` — spec previously described PG advisory locks that were never implemented).

## Entry

- Depends on P0106: `GenerationReader` was added in P0106 as prep; this plan is where the generation actually changes.
- Depends on P0107: `HealthReporter::set_not_serving` toggle depends on tonic-health wiring.

## Exit

Merged as `5d36f95..da6ce05` (2 commits). `.#ci` green at merge. Tests (4): `from_env_none_when_unset`, `reads_all_three`, `always_leader`, `pending_starts_false`. `LeaseLockResult` is an **enum** (`Acquired`/`NotAcquired`), not a struct with `.acquired_lease` like the crate doc says — `matches!` on variant. Lease loop tested for real by P0125's vm-phase3a lease exercises.

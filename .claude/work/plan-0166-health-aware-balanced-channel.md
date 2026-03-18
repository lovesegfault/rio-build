# Plan 0166: Health-aware balanced channel — headless Service + p2c + leader guard

## Design

First step toward zero-downtime scheduler failover. The problem: with two scheduler replicas behind a k8s Service, clients connecting via the Service ClusterIP get load-balanced to either pod — but only one is the leader. Standby RPCs fail. kube-proxy doesn't know about leadership.

**Leader guard (`9397e14`):** decouples K8s readiness from leadership. Both scheduler pods can be Ready (process up, gRPC listening) while only the leader serves RPCs. Standby returns `UNAVAILABLE` on every handler. `Arc<AtomicBool>` (same Arc the lease loop flips) threaded into `SchedulerGrpc` + `AdminServiceImpl`; `ensure_leader()` fires before `check_actor_alive` in all 6 scheduler + 9 admin handlers. Non-k8s mode unchanged: `always_leader()` defaults `true`.

**BalancedChannel (`70a7205`):** DNS-resolve a headless Service, probe each pod IP via `grpc.health.v1/Check` with the named service (`NOT_SERVING` on standby), feed `Change::Insert/Remove` to tonic's p2c balancer. p2c alone doesn't help — it only ejects on connection-level failure, and the standby's connection IS healthy; RPCs just return `UNAVAILABLE`. The out-of-band health probe is what routes correctly. **TLS domain override is load-bearing:** we connect to pod IPs (`https://10.42.2.140:9001`) but the cert's SAN is the service name; `ClientTlsConfig::domain_name()` decouples verify domain from connect URI. First tick blocks so the channel has ≥1 endpoint before the first RPC.

**Client adoption:** gateway (`cc158f4`) and worker (`facb4b1`) switch to balanced channel; worker also gets in-place reconnect on stream death. **pod-deletion-cost annotation (`252292c`):** leader annotates itself `controller.kubernetes.io/pod-deletion-cost` on lease acquire, so k8s scale-down prefers evicting standbys.

**RollingUpdate + headless Service (`7310cf8`):** the Recreate strategy from P0165 was the immediate fix; with balanced channel, RollingUpdate becomes viable again — clients route around the old leader as soon as it steps down.

## Files

```json files
[
  {"path": "rio-proto/src/client/balance.rs", "action": "NEW", "note": "BalancedChannel: headless DNS resolve \u2192 health probe \u2192 p2c Insert/Remove; TLS domain override"},
  {"path": "rio-proto/src/client/mod.rs", "action": "MODIFY", "note": "client.rs \u2192 client/mod.rs; balanced variants additive"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "ensure_leader() guard on all 6 handlers; Arc<AtomicBool> in SchedulerGrpc"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "ensure_leader() guard on all 9 admin handlers"},
  {"path": "rio-scheduler/src/lease/mod.rs", "action": "MODIFY", "note": "pod-deletion-cost annotation on lease acquire"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "scheduler via health-aware balanced channel"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "balanced channel + in-place reconnect on stream death"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "Option<String> for balance_host threaded through Ctx (2f2409b)"},
  {"path": "infra/base/scheduler.yaml", "action": "MODIFY", "note": "RollingUpdate strategy; headless Service"},
  {"path": "infra/overlays/prod/tls-mounts.yaml", "action": "MODIFY", "note": "drop scheduler readinessProbe patch"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "r[sched.grpc.leader-guard] + r[sched.lease.deletion-cost] markers; TODO(phase4b) for 2-replica failover VM test"}
]
```

## Tracey

- `r[impl sched.grpc.leader-guard]` ×3 — `9397e14` (2 sites) + `70a7205` (BalancedChannel reference)
- `r[verify sched.grpc.leader-guard]` — `9397e14`
- `r[impl sched.lease.deletion-cost]` ×2 — `252292c`

6 marker annotations (4 impl, 1 verify, 1 impl duplicate).

## Entry

- Depends on P0165: scheduler rollout deadlock fix (step_down is what flips the lease; balanced channel observes it)
- Depends on P0114: phase 3a lease (extends the same Arc<AtomicBool>)

## Exit

Merged as `9397e14`, `70a7205`, `252292c`, `cc158f4`, `facb4b1`, `7310cf8`, `2f2409b`, `853b719`, `a790435` (9 commits). `.#ci` green. Two-replica failover verified on EKS; VM test lands in P0173.

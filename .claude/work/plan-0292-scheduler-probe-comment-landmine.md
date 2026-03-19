# Plan 0292: Fix scheduler-probe CRITICAL-comment landmine

**Retro P0128 finding — USER ESCALATED from doc-rot to standalone.** This is a **maintenance trap**, not a stale comment. The CRITICAL block at [`rio-scheduler/src/main.rs:417-427`](../../rio-scheduler/src/main.rs) says:

```
// CRITICAL: the K8s readinessProbe MUST specify
// `grpc.service: rio.scheduler.SchedulerService` — the named
// service, not empty-string. ...
// The scheduler.yaml manifest MUST have:
//   readinessProbe:
//     grpc:
//       port: 9001
//       service: rio.scheduler.SchedulerService
```

**False since [`187a98a6`](https://github.com/search?q=187a98a6&type=commits).** The manifest at [`infra/helm/rio-build/templates/scheduler.yaml:113-114`](../../infra/helm/rio-build/templates/scheduler.yaml) uses `tcpSocket`:

```yaml
readinessProbe:
  tcpSocket: {port: 9001}
```

**A reader who "fixes" the manifest per this CRITICAL comment crash-loops the standby replica.** Per [`scheduler.yaml:124-127`](../../infra/helm/rio-build/templates/scheduler.yaml) and the HA design: the gRPC health service reports `NOT_SERVING` until leader election wins. Standby replicas stay live (liveness passes — they ARE healthy) but not ready. If readiness uses `grpc_health_probe`, it sees `NOT_SERVING` on the standby → readiness fails → K8s routes no traffic → fine so far. **But** livenessProbe is ALSO `tcpSocket` at `:121` — if someone "fixes" readiness AND liveness to gRPC (which is what the comment implies by saying "MUST specify"), liveness fails on standby → standby gets SIGKILLed → restarts → still standby → loop.

The comment's underlying `r[ctrl.probe.named-service]` constraint is REAL for the **client-side balancer** at `rio-proto/src/client/balance.rs` — it DOES probe the named service to find the leader. But K8s probes are a different layer. The comment conflates them.

Also: [`docs/src/components/controller.md:237`](../../docs/src/components/controller.md) Note block claims "the static Helm templates ... set `readinessProbe.grpc.service`" — equally false.

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `fix(scheduler):` rewrite the CRITICAL comment

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) at `:416-427`:

```rust
// r[impl ctrl.probe.named-service]
// The CLIENT-SIDE balancer (rio-proto/src/client/balance.rs) probes
// the NAMED service `rio.scheduler.SchedulerService` to find the
// leader — set_not_serving only affects named services, empty-string
// stays SERVING forever after first set_serving. A balancer probing
// "" would route to standby.
//
// K8S PROBES ARE A DIFFERENT LAYER: scheduler.yaml uses tcpSocket,
// NOT grpc. DO NOT "fix" the manifest to grpc probes — that crash-
// loops the standby (gRPC health reports NOT_SERVING until lease
// acquire; if liveness goes grpc, standby gets SIGKILLed → restart
// → still standby → loop). TCP-accept succeeding is the correct
// readiness/liveness signal for standby: the process is live, the
// port is bound, leader-election is the ONLY thing blocking serve.
```

### T2 — `docs:` fix controller.md Note block

MODIFY [`docs/src/components/controller.md`](../../docs/src/components/controller.md) at [`~:237`](../../docs/src/components/controller.md) — the Note block currently says:

> This requirement is satisfied by the static Helm templates (`infra/helm/rio-build/templates/scheduler.yaml` and `store.yaml`), which set `readinessProbe.grpc.service` to ...

Rewrite to:

> This requirement applies to the **client-side balancer** (`rio-proto/src/client/balance.rs`), which probes the named service to find the leader. It does NOT apply to K8s probes: `infra/helm/rio-build/templates/scheduler.yaml` intentionally uses `tcpSocket` for both readiness and liveness — standby replicas report `NOT_SERVING` on the gRPC health endpoint (they haven't won the lease), so gRPC-based K8s probes would crash-loop them. TCP-accept succeeding proves the process is live; leader-election is client-side routing's concern, not K8s'.

Also check the probe table at `:242` (per retrospective) — if the Scheduler row says "gRPC health check" for liveness/readiness, correct it to "TCP socket (gRPC health is leader-election-aware, unsuitable for K8s probes on HA standby)."

## Exit criteria

- `/nbr .#ci` green
- `grep -A5 'CRITICAL' rio-scheduler/src/main.rs | grep -c 'DO NOT.*grpc probe'` → ≥1 (the warning is now ANTI-footgun, not footgun)
- `grep 'readinessProbe.grpc.service' docs/src/components/controller.md` → 0 hits
- The manifest at `scheduler.yaml:114` unchanged (tcpSocket) — this plan fixes the COMMENT, the manifest was already correct

## Tracey

References existing markers:
- `r[ctrl.probe.named-service]` — T1 keeps the `r[impl]` marker but fixes the prose so it describes what the marker ACTUALLY constrains (client-side balancer, not K8s probes)

## Files

```json files
[
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T1: rewrite CRITICAL comment at :416-427 — TCP is correct for HA, grpc_health crash-loops standby"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T2: fix Note block at :237 + probe table at :242 — named-service applies to balancer not K8s probes"}
]
```

```
rio-scheduler/src/main.rs          # T1: fix CRITICAL comment
docs/src/components/controller.md  # T2: fix Note + table
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "retro P0128 — discovered_from=128. STANDALONE per user (landmine not doc-rot). CRITICAL comment says 'fix manifest to gRPC probe' — doing so crash-loops standby (gRPC health NOT_SERVING until lease won). Manifest was ALREADY correct (tcpSocket since 187a98a6); comment was the bug. controller.md:237 Note block also lies."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Conflicts with:** [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) moderate-traffic — also touched by [P0294](plan-0294-build-crd-full-rip.md) (different section, no overlap). [`controller.md`](../../docs/src/components/controller.md) heavy-traffic — also touched by P0285 (spec addition), P0294 (Build CRD section removal), P0296 (ephemeral spec addition). This plan's edits are localized to `:237-242`, low overlap.

# Plan 0177: Tonic reconnect class — gateway MAX_RECONNECT + EOF-reconnect + controller NotFound-retry + connect_timeout

## Design

A cluster of tonic reconnect bugs found during k3s-full stabilization. Each one is "reconnect logic doesn't handle k8s pod lifecycle correctly." These later became the substrate for remediations 04 and 20.

**`MAX_RECONNECT 5→10` (`05dedfd`):** the build-during-failover subtest's `nix-build` failed with "reconnect exhausted". 5 attempts over 1+2+4+8+16=31s. Replacement pod startup (container + mTLS cert mount + lease acquire on 5s tick) is ~20-30s. Marginal. 10 attempts = 31 + 5×16 = 111s (backoff capped at 16s via `.min(4)` shift).

**`EofWithoutTerminal` reconnect-worthy (`06a1fa0`):** the reconnect loop was NEVER ENTERED. Match arm was `Err(e @ (EofWithoutTerminal | Wire(_))) => break Err(e)` with comment "EOF = scheduler closed gracefully but incompletely (not a crash — crash would be Transport)". **WRONG.** k8s pod kill → SIGTERM → (with P0174) graceful shutdown → TCP FIN → client sees stream `Ok(None)`. `EofWithoutTerminal` is the PRIMARY failover signature in k8s. `Transport` (RST) only happens on OOM-SIGKILL or network partition. Moved to retry arm.

**10s `connect_timeout` (`82231d3`):** tonic's default is UNBOUNDED. After scheduler pod killed, if the controller's cached/DNS-resolved addr is stale, TCP SYN to the dead IP hangs — no RST (pod netns gone), maybe OS TCP timeout ~2min. `cleanup()` was designed for this ("best-effort scheduler connect. Failure → skip all drains") but never got to see the failure.

**Controller reconnect fixes (`5a89d75`, `35597c0`, `a4dca2c`):** non-terminal EOF reconnect-worthy in `drain_stream`; `WatchBuild` `NotFound` is retryable during failover (standby answers before leader's PG row is visible); retry scheduler connect instead of crashing (k3s flannel CNI race — `/run/flannel/subnet.env` written at t≈186s while sandboxes start at t≈185s).

**managedFields check (`8afab14`):** reconciler-replicas SSA check looked at value, not `managedFields` — autoscaler writing same value would pass falsely.

**`terminationGracePeriodSeconds` configurable (`f740147`).** **Balanced scheduler client in Ctx (`09f5cf9`).**

## Files

```json files
[
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "MAX_RECONNECT 5→10; EofWithoutTerminal moved to retry arm"},
  {"path": "rio-proto/src/client/mod.rs", "action": "MODIFY", "note": "10s connect_timeout on connect_channel"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "non-terminal EOF reconnect-worthy; WatchBuild NotFound retryable"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "retry scheduler connect loop; balanced scheduler client in Ctx"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "terminationGracePeriodSeconds configurable"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "managedFields check not value check"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "diagnostic dumps around 180s wait"}
]
```

## Tracey

- `r[impl ctrl.build.reconnect]` — `5a89d75`
- `r[verify ctrl.build.reconnect]` — `5a89d75`
- `r[verify sched.classify.penalty-overwrite]` — `73ef170` (misclass detection test, rider)
- `# r[verify sched.admin.create-tenant]` — `5a89d75` (VM test .nix)
- `# r[verify sched.admin.list-tenants]` — `5a89d75`
- `# r[verify sched.admin.list-workers]` — `5a89d75`
- `# r[verify sched.admin.list-builds]` — `5a89d75`

7 marker annotations (2 Rust, 5 .nix).

## Entry

- Depends on P0166: balanced channel (the reconnect logic routes via it)

## Exit

Merged as `05dedfd`, `06a1fa0`, `82231d3`, `f740147`, `5a89d75`, `73ef170`, `35597c0`, `8afab14`, `a4dca2c`, `09f5cf9` (10 commits). build-during-failover subtest passes reliably.

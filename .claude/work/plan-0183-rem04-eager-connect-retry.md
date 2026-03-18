# Plan 0183: Rem-04 — Eager connect retry: propagate controller's loop to gateway+worker

## Design

**P0 — simplest P0 in the batch.** Cold-start race: all `rio-*` pods start in parallel (helm install, node drain+reschedule). Gateway/worker can reach `connect_*` before the `rio-store`/`rio-scheduler` Services have endpoints. Eager `.connect().await` returns Err on refused; bare `?` meant process-exit → CrashLoopBackOff → kubelet's exponential backoff (10s/20s/40s) delayed recovery well past the point where deps were actually Ready.

The controller already fixed this in `a4dca2c` (P0177) — motivated by the k3s flannel CNI race (`/run/flannel/subnet.env` written at t≈186s while sandboxes start at t≈185s). This remediation propagates the same loop pattern to gateway and worker.

**Both connects in one loop body** so partial success (store OK, scheduler refused) reconnects store cleanly on next iteration instead of leaking a half-configured state.

**Gateway:** retry loop runs BEFORE tonic-health spawn — pod stays not-Ready while looping, which is the right gate (a gateway that can't reach the scheduler would accept SSH and then hang every `wopBuild*`).

**Worker:** health server already spawned above the loop — `/healthz` stays 200 (process IS alive, restart won't help), `/readyz` stays 503 (ready flag flips on first accepted heartbeat, far past this loop).

Remediation doc: `docs/src/remediations/phase4a/04-eager-connect-retry.md` (480 lines).

## Files

```json files
[
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "retry loop before tonic-health spawn; both connects in one body"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "retry loop; /healthz 200, /readyz 503 while looping"},
  {"path": "rio-proto/src/client/mod.rs", "action": "MODIFY", "note": "shared retry helper"}
]
```

## Tracey

No new markers. Covered by existing startup behavior spec.

## Entry

- Depends on P0177: tonic reconnect class (controller's reference pattern in a4dca2c)

## Exit

Merged as `5999be5` (plan doc) + `e17a554` (fix). `.#ci` green. Cold cluster start: no CrashLoopBackOff.

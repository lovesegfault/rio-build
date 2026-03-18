# Plan 0108: Worker SIGTERM graceful drain

## Design

K8s pod termination: kubelet sends SIGTERM, waits `terminationGracePeriodSeconds` (P0112 sets 7200 in the StatefulSet), then SIGKILL. A naive SIGTERM handler would kill the worker mid-build — loses up to 2 hours of compute. This single-commit plan wired the drain sequence: SIGTERM → call `DrainWorker` RPC (P0106) → wait for in-flight builds → exit 0.

The loop converts from `while let Some(msg) = rx.recv().await` to `tokio::select!` biased toward the SIGTERM arm: poll the signal **first** on each iteration so a queued SIGTERM preempts a queued work message. On SIGTERM: call `admin_client.drain_worker(worker_id, force=false)` — scheduler sets `draining=true`, stops dispatching new work. The worker then `acquire_many(max_builds)` on the build semaphore: when all permits return, all in-flight builds have completed. Exit 0.

Phase3a bug found during implementation (documented in `phase3a.md` "Key Bugs"): the initial version called `semaphore.close()` before `acquire_many`, thinking "no new acquires" was correct. But `close()` makes **waiting** `acquire_many` return `Err` even when permits become available — the drain wait would fail immediately. Fix: skip `close()` entirely. The loop already broke on SIGTERM; no new builds spawn. `acquire_many` waits for permits naturally.

The DrainWorker RPC is fire-and-forget from the worker's perspective: if scheduler is unreachable, the worker waits for in-flight anyway and exits. Scheduler's side notices via heartbeat timeout. If the RPC lands but scheduler is slow to ACK, the worker doesn't care — it's waiting on the semaphore, not the RPC response. `force=false` because the builds are genuinely finishing; `force=true` is for the controller finalizer path (P0116) when a pool is being torn down and the operator doesn't want to wait.

## Files

```json files
[
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "event loop: while-let → select! biased toward sigterm; drain_worker RPC + acquire_many(max_builds) wait"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "shutdown_signal wiring; admin_client passed to runtime"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "AdminServiceClient convenience wrapper for drain_worker"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[worker.drain.sigterm]`, `r[worker.drain.acquire-all]`.

## Entry

- Depends on P0106: calls the `DrainWorker` RPC which P0106 implemented.

## Exit

Merged as `29df7e8` (1 commit). `.#ci` green at merge. Tested by P0118's vm-phase3a `kubectl delete pod` → verifies the worker exits 0 within grace period and the in-flight build completed (output present in store).

# Plan 0130: State recovery on LeaderAcquired

## Design

Phase 3a's scheduler crashed cleanly: on leader lease acquisition, it came up with an empty in-memory DAG. All in-flight builds were lost — clients got "unknown build" and had to resubmit. Acceptable for dev; unacceptable for production where a scheduler restart shouldn't lose a 2-hour CUDA build at 95%. This plan implemented the full recovery path: on lease acquire, reload the entire scheduler state from PostgreSQL.

The first half was making the data recoverable at all. All the tables (`builds`, `derivations`, `edges`, `assignments`, `build_derivations`) existed and were being written — but no code ever READ them. They were write-only audit logs. Migration `004_recovery.sql` added the missing columns: `builds.keep_going`, `builds.options_json` (JSONB-serialized `BuildOptions`), `derivations.{expected_output_paths, output_names, is_fixed_output, failed_workers}`. The write paths (`batch_upsert_derivations`, `insert_build`, completion handlers) were extended to populate them.

The second half was `DagActor::recover_from_pg()` in new `actor/recovery.rs`: (1) clear in-mem `dag`, `builds`, `ready_queue`, `build_sequences`; (2) load all non-terminal builds/derivations/edges/assignments via new `load_*` queries in `db.rs`; (3) reconstruct `DerivationState` from rows — lossy fields handled conservatively (`drv_content=Vec::new()` since worker fetches from store, `ready_at=Some(now)` for Ready, `poisoned_at=Some(now)` for Poisoned restarts the 24h TTL); (4) new `DerivationDag::from_rows(nodes, edges)` constructor; (5) `critical_path::compute_initial()` over loaded DAG; (6) push Ready nodes; (7) rebuild `builds` map; (8) seed `build_sequences` from `MAX(sequence)`; (9) fresh broadcast channels per active build.

The non-obvious constraint: **lease renewal must NOT block on recovery**. A large DAG could take >15s to load; the lease expires at 15s; blocking → dual leader. The fix: lease loop does `fetch_add(1)` + `is_leader.store(true)` IMMEDIATELY (as before), then fire-and-forgets `ActorCommand::LeaderAcquired` (no reply channel) and continues renewing. The actor handles `LeaderAcquired`: runs `recover_from_pg().await` → on success sets `recovery_complete.store(true, Release)`; on Err logs+metrics, sets `recovery_complete=true` anyway with EMPTY DAG (downgrade to phase-3a "lost builds" behavior, not "cluster blocked"). `dispatch_ready` gates on BOTH `is_leader` AND `recovery_complete`. On lease loss: `recovery_complete.store(false)` so re-acquire re-triggers recovery.

Generation seeding uses `fetch_max(max_pg_gen + 1, Release)` not `store()` — the `Arc<AtomicU64>` is shared with the lease loop which already did `fetch_add(1)` on acquire; the two race. `store()` would clobber the lease's increment.

Post-recovery worker reconciliation (spec step 6): ~45s after recovery (3× heartbeat + slack, via `WeakSender` delayed command), `ActorCommand::ReconcileAssignments` checks each Assigned/Running derivation — if `assigned_worker` never reconnected, query store for output existence: all present → orphan-complete, else `reset_to_ready()` + `retry_count++`.

## Files

```json files
[
  {"path": "migrations/004_recovery.sql", "action": "NEW", "note": "ALTER builds ADD keep_going, options_json; ALTER derivations ADD expected_output_paths, output_names, is_fixed_output, failed_workers"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "NEW", "note": "recover_from_pg + handle_reconcile_assignments"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "NEW", "note": "seed PG, trigger LeaderAcquired, assert state populated"},
  {"path": "rio-scheduler/src/actor/tests/mod.rs", "action": "MODIFY", "note": "mod recovery"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "LeaderAcquired handler + recovery_complete: Arc<AtomicBool>"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "LeaderAcquired + ReconcileAssignments variants"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "send_unchecked for fire-and-forget"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "gate on recovery_complete"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "write options_json, keep_going"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "persist failed_workers"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "update_derivation_failed_workers on reassign"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "load_nonterminal_* queries + max_assignment_generation + extended upserts"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "DerivationDag::from_rows constructor"},
  {"path": "rio-scheduler/src/lease.rs", "action": "MODIFY", "note": "fire-and-forget LeaderAcquired + recovery_complete.store(false) on lose"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "pass actor handle to run_lease_loop"},
  {"path": "rio-scheduler/src/queue.rs", "action": "MODIFY", "note": "clear() for recovery"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "from_recovery_row constructor"},
  {"path": "rio-scheduler/src/state/build.rs", "action": "MODIFY", "note": "BuildInfo::from_row"}
]
```

## Tracey

Markers implemented:
- `r[verify sched.lease.recovery]` — recovery test (`4ca6cd3`). Seeds PG with 3-node DAG (1 complete, 1 running, 1 queued), fires `LeaderAcquired`, asserts DAG structure + `recovery_complete=true` + Ready nodes in queue.

The `r[impl sched.recovery.gate-dispatch]` marker was added retroactively in `dadc70c` (P0141) when the spec rule was written during the round-4 docs catchup.

## Entry

- Depends on P0127: phase 3a complete (lease loop, `LeaderState`, actor command pattern, `DagActor` all exist).
- Depends on P0129: `failed_workers` field exists on `DerivationState` (recovery reads/writes it).

## Exit

Merged as `649f024..4ca6cd3` (2 commits). `.#ci` green at merge. Test `test_recovery_seeded_dag` seeds 3-node DAG in PG, verifies `recover_from_pg` rebuilds structure + priorities + ready queue. Test `test_recovery_pg_error_empty_dag` mocks PG failure, asserts `recovery_complete=true` with empty DAG + error logged (graceful downgrade, not cluster block).

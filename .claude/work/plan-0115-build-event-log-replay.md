# Plan 0115: build_event_log table + since_sequence replay

## Design

The broadcast channel (cap 1024) handles late subscribers within a single scheduler instance. It doesn't handle gateway reconnect to a **new** leader that never had the old broadcast buffer. Two commits added persistent event log + replay.

`bc07829` created the `build_event_log` table (`migrations/003_event_log.sql`) and `EventPersister` — a bounded mpsc channel drained by a single task that INSERTs events in FIFO order. Why not fire-and-forget spawn per event: `emit_build_event` is called in-order by the actor, but spawned tasks execute in tokio scheduler order — no guarantee seq=5's INSERT commits before seq=6's. A mid-backlog reconnect could see neither, then get seq=7+ from broadcast. FIFO channel → FIFO writes. `Event::Log` is **filtered out** — ~20/sec under chatty rustc would flood PG. Log lines are already durable via S3 `LogFlusher`; gateway reconnect cares about state-machine events (`Started`/`Completed`/`Derivation*`), not log lines — it re-fetches those from S3. GC: `handle_cleanup_terminal_build` fire-and-forget DELETE (ordering irrelevant — `build_sequences` already removed, no more writes for this `build_id`).

`fd09ea4` wired the replay side in `bridge_build_events`. Gateway reconnect with `since_sequence=N` → replay (N, last_seq] from PG, then drain broadcast with dedup. Ordering is precise: subscribe **first**, then capture `last_seq` (`handle_watch_build` is sync in the single-threaded actor → no event can be emitted between `tx.subscribe()` and `build_sequences.get()`). `last_seq` is an exact watermark: ≤last_seq → pre-subscribe (PG has it, may also be in broadcast ring); >last_seq → post-subscribe (guaranteed on broadcast, not in PG yet). Dedup (bridge phase 2): skip broadcast events with seq ≤ last_seq — the 1024-event ring may still have pre-subscribe events; PG already delivered those. PG failure → `dedup_watermark` stays 0 → no dedup. A double is better than a hole. `SubmitBuild` path: `replay=None` — fresh build, `MergeDag` subscribes before seq=1, no gap possible.

## Files

```json files
[
  {"path": "rio-scheduler/src/event_log.rs", "action": "NEW", "note": "EventPersister: bounded mpsc → single drain task → FIFO INSERT; Event::Log filtered out"},
  {"path": "migrations/003_event_log.sql", "action": "NEW", "note": "build_event_log table: build_id, sequence, event_type, payload jsonb, created_at"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "event_persist_tx wiring; emit_build_event sends to both broadcast and persist channel"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "handle_cleanup_terminal_build DELETE"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "handle_watch_build: subscribe-first then capture last_seq"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "bridge_build_events: PG replay phase then broadcast drain with dedup_watermark"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "read_event_log query"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "EventPersister spawn"},
  {"path": "rio-scheduler/src/actor/tests/build.rs", "action": "MODIFY", "note": "replay tests: PG has N events, subscribe with since_sequence=N-2 → get 2 from PG + new from broadcast, no dupes"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[sched.event-log.persist]`, `r[sched.event-log.replay]`, `r[sched.event-log.dedup-watermark]`, `r[sched.event-log.log-filter]`.

## Entry

- Depends on P0114: replay is FOR post-failover reconnect. Without leader election, there's no failover to replay for.

## Exit

Merged as `bc07829..fd09ea4` (2 commits). `.#ci` green at merge. 134-line replay test in `actor/tests/build.rs`. Resolves the phase3a-tagged TODO at `actor/mod.rs` (`WatchBuild` arm).

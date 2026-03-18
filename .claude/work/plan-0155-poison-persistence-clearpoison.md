# Plan 0155: Poison persistence — migration 009B + poisoned_at + ClearPoison RPC

## Design

Pre-4a, `poisoned_at` was in-memory only (`state.poisoned_at = Some(Instant::now())` in `poison_and_cascade` + `handle_permanent_failure`). Scheduler restart → poison TTL reset to fresh 24 hours. Poison was a per-process-lifetime concept.

**Migration 009 Part B** adds `ALTER TABLE derivations ADD COLUMN poisoned_at TIMESTAMPTZ`. Db helpers: `set_poisoned_at(drv_hash)` (UPDATE poisoned_at = now()), `clear_poison(drv_hash)` (NULL poisoned_at + empty `failed_workers` + zero `retry_count` + status='created' in one roundtrip), `load_poisoned_derivations()` (SELECT with `EXTRACT(EPOCH FROM (now() - poisoned_at))::float8 AS elapsed_secs` — PG computes elapsed time so the caller reconstructs `Instant::now() - elapsed`). Write sites in `completion.rs` call `set_poisoned_at` after `persist_status(Poisoned)`; TTL expiry in `worker.rs` calls `clear_poison`.

**ClearPoison admin RPC** (`f3326ce`): `ActorCommand::ClearPoison{drv_hash, reply}` via `send_unchecked` (operator-initiated, should work under saturation). `handle_clear_poison`: not found OR not Poisoned → `cleared=false`; otherwise `reset_from_poison()` transitions Poisoned→Created in-mem, `db.clear_poison()` mirrors PG. Status is Created after — **don't push to ready_queue** (inputs may have changed; re-resolve on next submit).

**Rounds 2, 4, 6 critically rewrote this:** Round 2 found `set_poisoned_at`/`clear_poison` bound `drv_hash.as_bytes()` → BYTEA vs TEXT column → silently rejected — the whole point of migration 009B was non-functional (see P0158). Round 4 found the in-mem-first/PG-second ordering made ClearPoison retry-unsafe (see P0160). Round 6 found `reset_from_poison` left a stub-field zombie that hung resubmit forever, replaced with `remove_node()` (see P0162).

## Files

```json files
[
  {"path": "migrations/009_phase4.sql", "action": "MODIFY", "note": "Part B: ALTER TABLE derivations ADD COLUMN poisoned_at TIMESTAMPTZ"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "set_poisoned_at, clear_poison, load_poisoned_derivations helpers; PoisonedDerivationRow"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "set_poisoned_at after persist_status(Poisoned) in both paths"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "TTL expiry → clear_poison (replaces persist_status + clear_failed_workers_and_retry)"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "load_poisoned_derivations + from_poisoned_row"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "ClearPoison handler: link_parent, InvalidArgument on empty hash, send ActorCommand"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "reset_from_poison() transition (deleted in round 6)"}
]
```

## Tracey

- `r[impl sched.poison.ttl-persist]` — tagged retroactively in `20e557f`
- `r[impl sched.admin.clear-poison]` — tagged retroactively in `20e557f`
- `r[verify sched.poison.ttl-persist]` — `ea36f98` (round 2 db roundtrip test) + `eb578f9` (recovery test)
- `r[verify sched.admin.clear-poison]` — `eb578f9` (happy-path test)

4 marker annotations (2 impl retroactive, 2 verify in round 2).

## Entry

- Depends on P0148: phase 3b complete (extends existing poison infrastructure from 3a)

## Exit

Merged as `9c69ef6`, `f3326ce` (2 commits). `.#ci` green. **Critically revised in rounds 2/4/6** — see P0158, P0160, P0162.

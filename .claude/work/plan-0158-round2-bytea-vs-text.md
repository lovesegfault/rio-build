# Plan 0158: Round 2 — poison BYTEA-vs-TEXT silent bug + proto field gaps

## Design

First branch-review pass after round-1 merge. Headline bug: **`set_poisoned_at`/`clear_poison` bound `drv_hash.as_bytes()` which resolves via `Deref<Target=str>` to `str::as_bytes()` → `&[u8]` → sqlx encodes as BYTEA. But `derivations.drv_hash` is TEXT (migration 001).** PG rejects with `operator does not exist: text = bytea`. Call sites swallowed as best-effort — `poisoned_at` was NEVER persisted, `ClearPoison` always returned `cleared=false`, TTL-expiry left PG drift. The whole point of migration 009 Part B was non-functional.

Fix: `.as_str()` not `.as_bytes()`. The db-layer roundtrip test that landed with the fix would have caught this immediately — it's the canonical "why didn't a test catch this" case.

Also in round 2: missing `WorkerInfo.size_class` and `ListBuilds.tenant_filter` proto fields (`929261b`); `tenant_id` span field on SubmitBuild + `inject_current` on `TriggerGC` + cancel_signals backstop assertion (`b946cbc`); `store_size_refresh` shutdown token wiring + `TenantRow` epoch `created_at` (`ffdec1d`); cgroup util `0.0` for unbounded memory (`8a2cc66`); `handle_permanent_failure` early-return on transition failure (`23b757c`); ClearPoison happy-path + poison recovery verify tests (`eb578f9`).

## Files

```json files
[
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "as_bytes → as_str; updated_at=now(); dead clear_failed_workers_and_retry deleted"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "handle_permanent_failure early-return on transition failure"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "poison db roundtrip test (r[verify sched.poison.ttl-persist])"},
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "MODIFY", "note": "ClearPoison happy-path + poison recovery (r[verify sched.admin.clear-poison])"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "WorkerInfo.size_class; ListBuildsRequest.tenant_filter"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "tenant_id span field on SubmitBuild; inject_current TriggerGC"},
  {"path": "rio-scheduler/src/admin/builds.rs", "action": "MODIFY", "note": "store_size_refresh shutdown token; TenantRow epoch created_at"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "emit 0.0 for unbounded memory, not garbage"}
]
```

## Tracey

- `r[verify sched.poison.ttl-persist]` — `ea36f98` (db roundtrip test) + `eb578f9` (recovery test)
- `r[verify sched.admin.clear-poison]` — `eb578f9`
- `r[impl obs.metric.transfer-volume]` — re-tag in `b946cbc` (inject_current rider)

3 verify annotations, 1 impl re-tag.

## Entry

- Depends on P0155: poison persistence (this fixes its silent bug)
- Depends on P0156: admin RPCs (proto field additions)

## Exit

Merged as `ea36f98..0c3967f` (9 commits). `.#ci` green. Phase doc round-2 summary: "08f2058..875aa97, +9 commits".

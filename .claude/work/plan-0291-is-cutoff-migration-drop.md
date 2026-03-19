# Plan 0291: is_cutoff column migration DROP

**Retro P0027 finding.** `derivation_edges.is_cutoff BOOLEAN DEFAULT FALSE` was added for "future CA early cutoff support" ([`migrations/001_scheduler.sql:68`](../../migrations/001_scheduler.sql)). It will **remain dead** after CA cutoff lands — [P0252](plan-0252-ca-cutoff-skipped-variant.md) uses a completely different mechanism (`DerivationStatus::Skipped` enum variant + `find_cutoff_eligible()` DAG walker). P0252's text never mentions `is_cutoff`. Zero `.rs` references; INSERT at `db.rs:814` omits it (relies on DEFAULT); SELECT at `db.rs:1009` omits it.

**The upstream fix already landed at [`f8b2ef10`](https://github.com/search?q=f8b2ef10&type=commits):** [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) was edited to strip `is_cutoff` from `GraphEdge` (proto field 3 RESERVED) and from its `SELECT` ([`plan-0276:29,68-70`](plan-0276-getbuildgraph-rpc-pg-backed.md)). `GraphNode.status` already carries `"skipped"` after P0252, so edge-level `is_cutoff` is redundant.

This plan is just the migration + doc cleanup. Small, mechanical.

## Entry criteria

- [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) merged (last-would-be-reader already stripped; no live code references `is_cutoff` after P0276)

## Tasks

### T1 — `fix(scheduler):` migration 012 — DROP COLUMN

NEW [`migrations/012_drop_is_cutoff.sql`](../../migrations/012_drop_is_cutoff.sql):

```sql
-- retro P0027: derivation_edges.is_cutoff was "future CA early cutoff"
-- placeholder. P0252 (CA cutoff) uses DerivationStatus::Skipped node-status
-- instead — edge flag is dead. Zero .rs references (db.rs INSERT omits it,
-- relies on DEFAULT; SELECT omits it). P0276 already dropped it from
-- GraphEdge proto (field 3 RESERVED).
--
-- Non-blocking on PG 12+: column is at the end of the row, no rewrite.
ALTER TABLE derivation_edges DROP COLUMN is_cutoff;
```

Run `cargo sqlx migrate run` + `cargo sqlx prepare` to regenerate `.sqlx/` — should be a no-op since no queries reference the column, but confirm.

### T2 — `docs:` remove from scheduler.md schema block

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) at [`:506`](../../docs/src/components/scheduler.md) — delete the `is_cutoff   BOOLEAN NOT NULL DEFAULT FALSE,` line from the `derivation_edges` CREATE TABLE doc block.

## Exit criteria

- `/nbr .#ci` green
- `grep is_cutoff migrations/ -r` → only `001_scheduler.sql:68` (original CREATE; migrations are immutable history) + `012_drop_is_cutoff.sql` (the DROP)
- `grep is_cutoff rio-scheduler/ rio-proto/ -r` → 0 hits
- `grep is_cutoff docs/src/components/scheduler.md` → 0 hits
- `.sqlx/` regen is a no-op diff (confirms no live queries touched the column)

## Tracey

No markers. Dead-schema cleanup; no spec behavior changes.

## Files

```json files
[
  {"path": "migrations/012_drop_is_cutoff.sql", "action": "NEW", "note": "T1: ALTER TABLE derivation_edges DROP COLUMN is_cutoff"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T2: delete is_cutoff line from derivation_edges schema block at :506"}
]
```

```
migrations/012_drop_is_cutoff.sql    # T1: NEW
docs/src/components/scheduler.md     # T2: delete line
```

## Dependencies

```json deps
{"deps": [276], "soft_deps": [], "note": "retro P0027 — discovered_from=27. P0252 uses Skipped node-status not edge flag; is_cutoff will NEVER be wired. P0276 already edited (f8b2ef10: field 3 RESERVED, dropped from SELECT). This is just the migration. Dep 276: P0276 is last-would-be-reader; after it merges the column is provably dead."}
```

**Depends on:** [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) — its `GraphEdge` proto + SELECT were the last places that WOULD have read the column; already stripped at f8b2ef10. After P0276 merges, the column is provably unreachable.
**Conflicts with:** [`scheduler.md`](../../docs/src/components/scheduler.md) moderate-traffic doc. Migration number 012: check at impl time that no other plan grabbed it (P0206 uses 012 for path_tenants per its dag note — **coordinate**: if P0206 lands first, this becomes 013).

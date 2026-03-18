# Plan 0191: Rem-12 — PG transaction safety: commit-before-mutate + UNNEST batch + literal NOT IN

## Design

**P1 (HIGH) for three findings; P2 riders deferred.** Three independent P1 fixes that touch adjacent code — one commit.

**`pg-in-mem-mutation-inside-tx` (`merge.rs`):** move `self.dag.node_mut().db_id` write to AFTER `tx.commit()`. Edge resolution now uses the tx-local `id_map` instead of reading back through `self.dag` (which required the in-tx mutation). Previous code was accidentally correct — `rollback_merge` removed phantom-`db_id` nodes wholesale — but relied on three nonlocal invariants. Now trivially correct by construction. A `path_to_hash` local + resolve closure consult `id_map` first, fall back to `self.dag` only for cross-batch edges.

**`pg-querybuilder-param-limit` (`db.rs`):** `batch_upsert_derivations` + `batch_insert_build_derivations` + `batch_insert_edges` switched from `QueryBuilder::push_values` (9×N bind params, fails at 7282 rows) to `UNNEST` array params (9 binds total). NixOS system closures are ~30k derivations. Nested-array columns (`required_features` etc.) encoded via `encode_pg_text_array` helper as flat `text[]` of array-literal strings, cast back in SELECT — PG multidim arrays must be rectangular so `Vec<Vec<String>>` can't bind directly.

**`pg-partial-index-unused-parameterized` (`db.rs`):** inline `TERMINAL_STATUSES` as `TERMINAL_STATUS_SQL` const literal so the planner can prove the query implies the partial index. Parameterized `NOT IN ($1)` defeats the planner — it can't know `$1` matches the index predicate.

**P2 riders noted as deferred:** `pg-dual-migrate-race` (scheduler `--skip-migrations` — NOT landed, both scheduler and store still run `migrate!()`); `pg-pool-size-hardcoded` (figment key — NOT verified landed); **`pg-zero-compile-time-checked-queries` explicitly deferred — `TODO(phase4b)` at `rio-scheduler/src/db.rs:9`** ("convert query(…) → query!(…) for compile-time").

Remediation doc: `docs/src/remediations/phase4a/12-pg-transaction-safety.md` (1163 lines, longest remediation).

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "move db_id write after commit; tx-local id_map for edge resolution"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "UNNEST array params (9 binds not 9N); TERMINAL_STATUS_SQL const literal; encode_pg_text_array helper"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "3 spec markers"}
]
```

## Tracey

- `r[impl sched.db.tx-commit-before-mutate]` — `2560dec` (**UNTESTED at phase-4a** — verify test proposed in remediation doc but not landed; shows in `tracey query untested`)
- `r[impl sched.db.batch-unnest]` — `2560dec`
- `r[impl sched.db.partial-index-literal]` — `2560dec`
- `r[verify sched.db.batch-unnest]` ×2 — `2560dec`
- `r[verify sched.db.partial-index-literal]` ×2 — `2560dec`

7 marker annotations (3 impl, 4 verify; 1 impl UNTESTED).

## Entry

- Depends on P0180: rem-01 (merge.rs changes read the same `id_map` that rem-01's `persist_poisoned` writes through)

## Exit

Merged as `1b85af5` (plan doc) + `2560dec` (fix). `.#ci` green. **Partial:** §6 `sqlx query!` deferred → `TODO(phase4b)` at `db.rs:9`. `pg-dual-migrate-race` NOT landed (no `skip_migrations` flag found in codebase).

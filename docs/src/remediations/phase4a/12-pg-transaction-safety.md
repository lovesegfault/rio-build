# Remediation 12: PG transaction safety

**Parent:** [`phase4a.md` §2.6](../phase4a.md#26-pg-transaction-safety)
**Findings:** `pg-in-mem-mutation-inside-tx`, `pg-querybuilder-param-limit`, `pg-partial-index-unused-parameterized`, `pg-dual-migrate-race`, `pg-pool-size-hardcoded`, `pg-zero-compile-time-checked-queries`
**Priority:** P1 (HIGH) for the first three · P2 for the rest
**tracey:** no existing markers — add `r[sched.db.tx-commit-before-mutate]`, `r[sched.db.batch-unnest]`, `r[sched.db.partial-index-literal]` to `docs/src/components/scheduler.md` as part of this work

---

## Finding map

Three P1 findings are independent but touch adjacent code. One commit for all three — edge resolution in Finding 1 reads the same `id_map` that Finding 2 changes the type of.

| Finding | Site | Failure mode | Fix shape |
|---|---|---|---|
| `pg-in-mem-mutation-inside-tx` | `merge.rs:385-389` | `db_id` phantom on tx rollback | Move write after `commit()`; resolve edges via local map |
| `pg-querybuilder-param-limit` | `db.rs:630-700` | >7281 nodes → `"too many bind parameters"` | `UNNEST` array params (9 binds total, any row count) |
| `pg-partial-index-unused-parameterized` | `db.rs:23-25,579,859` | Seq scan on recovery load | Inline literal; drift test |

Three P2 riders ship in the same PR because they're trivially local:

| Finding | Site | Fix shape |
|---|---|---|
| `pg-dual-migrate-race` | `scheduler/main.rs:184`, `store/main.rs:180` | Store owns migrations; scheduler `--skip-migrations` + version check |
| `pg-pool-size-hardcoded` | `scheduler/main.rs:178`, `store/main.rs:175` | `pg_max_connections` figment key |
| `pg-zero-compile-time-checked-queries` | workspace-wide | **Deferred** — see §6 |

---

## 1. `pg-in-mem-mutation-inside-tx` — move `db_id` write after commit

### Current control flow (`rio-scheduler/src/actor/merge.rs:378-416`)

```rust
let mut tx = self.db.pool().begin().await?;                         // 379
let id_map = batch_upsert_derivations(&mut tx, &node_rows).await?;  // 382

for (hash, db_id) in &id_map {                                      // 385 ┐
    if let Some(state) = self.dag.node_mut(hash) {                  //     │ in-mem write
        state.db_id = Some(*db_id);                                 //     │ INSIDE tx
    }                                                               //     │ scope
}                                                                   // 389 ┘

let db_ids: Vec<Uuid> = id_map.values().copied().collect();         // 392
batch_insert_build_derivations(&mut tx, build_id, &db_ids).await?;  // 393 ← await point #1

let edge_rows = edges.iter().map(|e| {
    let parent = self.find_db_id_by_path(&e.parent_drv_path)?;      // 399 ← reads self.dag.node(h).db_id
    let child  = self.find_db_id_by_path(&e.child_drv_path)?;       // 404 ←
    Ok((parent, child))
}).collect()?;                                                      // 412
batch_insert_edges(&mut tx, &edge_rows).await?;                     // 413 ← await point #2

tx.commit().await?;                                                 // 415 ← await point #3
```

### Why it's subtle

The naive fix — "just move 385-389 below 415" — **breaks edge resolution**. `find_db_id_by_path` at `:399/:404` walks `self.dag.hash_for_path(path) → self.dag.node(h).db_id` (`actor/mod.rs:952-957`). For **newly-inserted** nodes, `db_id` is `None` until the loop at 385-389 sets it. Moving that loop past 415 means every edge touching a new node returns `MissingDbId` at 412.

The in-tx mutation is load-bearing. That's the real smell: **edge resolution has a hidden data dependency on an in-memory side effect that's sequenced between two tx-internal awaits.**

### What actually goes wrong today

The §2.6 writeup frames this as "commit fails → phantom db_id." That's narrower than it looks:

1. `persist_merge_to_db` returns `Err` → caller at `merge.rs:105-112` calls `cleanup_failed_merge`
2. `cleanup_failed_merge` → `rollback_merge` (`dag/mod.rs:358-399`)
3. `rollback_merge` removes **newly_inserted** nodes entirely (line 382-389: `self.nodes.remove(hash)`) — phantom `db_id` goes with them
4. Pre-existing nodes keep their `db_id`, but `ON CONFLICT ... RETURNING` hands back their **existing** `derivation_id`, so the re-set is idempotent

So the current code is **accidentally correct** — phantom db_ids are always attached to nodes that `rollback_merge` is about to delete. But this correctness relies on three nonlocal invariants:

- `cleanup_failed_merge` is called on every `Err` path out of `persist_merge_to_db` (currently true: `merge.rs:110`)
- `rollback_merge` removes **all** nodes whose `db_id` was freshly minted (true because `id_map` ⊇ `newly_inserted` and the loop at 385 only mutates nodes that exist in the DAG)
- `ON CONFLICT` returns the existing row's id, not a fresh one (PG semantics, stable)

Any of these breaks and the phantom becomes real. The fix makes the invariant **local and trivially true**: in-mem state is never mutated until the tx is durable.

### Diff

```diff
--- a/rio-scheduler/src/actor/merge.rs
+++ b/rio-scheduler/src/actor/merge.rs
@@ -375,41 +375,58 @@
             })
             .collect();

+        // drv_path → drv_hash lookup for edge resolution below. Edges
+        // carry paths (proto wire format); id_map keys by hash.
+        let path_to_hash: HashMap<&str, &str> = node_rows
+            .iter()
+            .map(|r| (r.drv_path.as_str(), r.drv_hash.as_str()))
+            .collect();
+
         // Transaction: 3 batched roundtrips instead of 2N+E serial.
         let mut tx = self.db.pool().begin().await?;

         // Batch 1: upsert all derivations, get back drv_hash -> db_id map.
         let id_map = crate::db::SchedulerDb::batch_upsert_derivations(&mut tx, &node_rows).await?;

-        // Update in-memory db_id from returned map.
-        for (hash, db_id) in &id_map {
-            if let Some(state) = self.dag.node_mut(hash) {
-                state.db_id = Some(*db_id);
-            }
-        }
-
         // Batch 2: link all nodes to this build.
         let db_ids: Vec<Uuid> = id_map.values().copied().collect();
         crate::db::SchedulerDb::batch_insert_build_derivations(&mut tx, build_id, &db_ids).await?;

-        // Batch 3: insert edges (resolve drv_path -> db_id via find_db_id_by_path).
+        // Batch 3: insert edges. Resolve drv_path -> db_id via:
+        //   1. this tx's id_map (covers newly-inserted + re-upserted
+        //      nodes from this batch — ON CONFLICT RETURNING gives
+        //      back existing ids for the latter),
+        //   2. fall back to self.dag (covers cross-batch edges to
+        //      nodes merged by a PRIOR SubmitBuild that aren't in
+        //      this request's `nodes` list at all — rare but legal
+        //      when gateway deduplicates against live DAG).
+        // Does NOT read self.dag.node().db_id for nodes in THIS
+        // batch — that field isn't set until after commit() below.
+        let resolve = |drv_path: &str| -> Option<Uuid> {
+            path_to_hash
+                .get(drv_path)
+                .and_then(|h| id_map.get(*h).copied())
+                .or_else(|| self.find_db_id_by_path(drv_path))
+        };
         let edge_rows: Result<Vec<(Uuid, Uuid)>, ActorError> = edges
             .iter()
             .map(|e| {
-                let parent = self.find_db_id_by_path(&e.parent_drv_path).ok_or_else(|| {
-                    ActorError::MissingDbId {
-                        drv_path: e.parent_drv_path.clone(),
-                    }
+                let parent = resolve(&e.parent_drv_path).ok_or_else(|| ActorError::MissingDbId {
+                    drv_path: e.parent_drv_path.clone(),
                 })?;
-                let child = self.find_db_id_by_path(&e.child_drv_path).ok_or_else(|| {
-                    ActorError::MissingDbId {
-                        drv_path: e.child_drv_path.clone(),
-                    }
+                let child = resolve(&e.child_drv_path).ok_or_else(|| ActorError::MissingDbId {
+                    drv_path: e.child_drv_path.clone(),
                 })?;
                 Ok((parent, child))
             })
             .collect();
         let edge_rows = edge_rows?;
         crate::db::SchedulerDb::batch_insert_edges(&mut tx, &edge_rows).await?;

         tx.commit().await?;
+
+        // r[impl sched.db.tx-commit-before-mutate]
+        // In-mem db_id write ONLY after the tx is durable. If anything
+        // above returned Err, the tx rolled back and we never reach
+        // here — cleanup_failed_merge sees nodes with db_id = None.
+        for (hash, db_id) in &id_map {
+            if let Some(state) = self.dag.node_mut(hash) {
+                state.db_id = Some(*db_id);
+            }
+        }
         Ok(())
     }
```

**Borrow note:** the `resolve` closure borrows `self` immutably via `self.find_db_id_by_path`. The `.map(|e| ...)` collects into an owned `Vec` before any `&mut self` is needed (`node_mut` is after `commit()`). If the borrow checker complains anyway (closure captures `self` by-ref across the `await?` at `batch_insert_edges`), inline the `or_else` arm — `self.dag.hash_for_path(drv_path).and_then(|h| self.dag.node(h)).and_then(|s| s.db_id)` — so the closure only borrows `self.dag` (immutable field borrow), not all of `self`.

### `cleanup_failed_merge` audit

**Task brief asks:** check `cleanup_failed_merge` doesn't clear `db_id` on pre-existing nodes.

Verified: `rollback_merge` (`dag/mod.rs:358-399`) has three actions:
- Remove `new_edges` — no `db_id` touched
- Remove `newly_inserted` nodes wholesale (`self.nodes.remove(hash)`, line 383) — removes the whole node, `db_id` goes with it, correct
- Remove `build_id` from `interest_added` nodes' `interested_builds` set (line 396) — does **not** touch `db_id`

Pre-existing nodes (in DAG before this merge, not in `newly_inserted`) fall into the `interest_added` bucket at most. Their `db_id` was set by a **prior successful** `persist_merge_to_db` and is untouched by rollback. **No change needed.**

---

## 2. `pg-querybuilder-param-limit` — switch to `UNNEST`

### The limit

PostgreSQL wire protocol caps bind parameters at 65535 (`int16` count in the Bind message). `QueryBuilder::push_values` generates one `$N` per column per row. Current column counts:

| Function | Cols/row | Max rows | Breaks at |
|---|---|---|---|
| `batch_upsert_derivations` | 9 | 7281 | **7282** |
| `batch_insert_build_derivations` | 2 | 32767 | 32768 |
| `batch_insert_edges` | 2 | 32767 | 32768 |

A NixOS system closure is ~30k derivations. A large monorepo CI closure can exceed 50k. `batch_upsert_derivations` is the first domino; once it's fixed, edges (E ≈ 2-3× N for typical DAGs) is the next wall.

### Why `UNNEST` over chunking

Chunking at 5000 rows works but:
- Turns one roundtrip into N/5000 roundtrips (30k nodes = 6 roundtrips for batch 1 alone)
- `RETURNING` from chunk k must be merged with chunks 1..k-1 before edge resolution can proceed — either collect all chunks first (memory spike for the same data you're already holding in `node_rows`) or interleave chunk-upsert with edge-resolve (complicates the tx control flow)
- Chunk boundary is another magic number that will drift from the column count. Add a tenth column, 5000 × 10 = 50000, still under limit. Add four more, 5000 × 14 = 70000, silently over.

`UNNEST($1::text[], $2::text[], ...)` binds one **array** per column. Nine columns = nine bind params, period. PostgreSQL unnests them row-wise server-side. No chunking, no roundtrip multiplication, no magic-number drift.

### Diff — `batch_upsert_derivations`

```diff
--- a/rio-scheduler/src/db.rs
+++ b/rio-scheduler/src/db.rs
@@ -617,11 +617,15 @@

-    /// Batch-upsert derivations. Returns a map `drv_hash -> derivation_id`.
-    ///
-    /// Uses QueryBuilder::push_values for multi-row INSERT. RETURNING includes
-    /// drv_hash because PG doesn't guarantee RETURNING order matches input.
+    // r[impl sched.db.batch-unnest]
+    /// Batch-upsert derivations. Returns a map `drv_hash -> derivation_id`.
+    ///
+    /// Array parameters via `UNNEST`: 9 bind params total regardless of
+    /// row count (vs `push_values`' 9×N, which hits PG's 65535-param
+    /// limit at 7282 rows). `RETURNING drv_hash` because PG doesn't
+    /// guarantee `RETURNING` order matches `UNNEST` input order either.
     pub async fn batch_upsert_derivations(
         tx: &mut PgConnection,
         rows: &[DerivationRow],
     ) -> Result<HashMap<String, Uuid>, sqlx::Error> {
         if rows.is_empty() {
             return Ok(HashMap::new());
         }

-        let mut qb = QueryBuilder::new(
-            "INSERT INTO derivations \
-             (drv_hash, drv_path, pname, system, status, required_features, \
-              expected_output_paths, output_names, is_fixed_output) ",
-        );
-        qb.push_values(rows, |mut b, row| {
-            b.push_bind(&row.drv_hash)
-                .push_bind(&row.drv_path)
-                .push_bind(&row.pname)
-                .push_bind(&row.system)
-                .push_bind(row.status.as_str())
-                .push_bind(&row.required_features)
-                .push_bind(&row.expected_output_paths)
-                .push_bind(&row.output_names)
-                .push_bind(row.is_fixed_output);
-        });
-        // ON CONFLICT: update the recovery columns too. A second
-        // build requesting the same derivation may have fresher
-        // expected_output_paths (same drv_hash → same outputs, so
-        // this is idempotent in practice, but keeps the row in sync
-        // with in-mem). status/retry etc stay as-is — those reflect
-        // LIVE state, not merge-time snapshot.
-        qb.push(
-            " ON CONFLICT (drv_hash) DO UPDATE SET \
-                updated_at = now(), \
-                expected_output_paths = EXCLUDED.expected_output_paths, \
-                output_names = EXCLUDED.output_names, \
-                is_fixed_output = EXCLUDED.is_fixed_output \
-             RETURNING drv_hash, derivation_id",
-        );
-
-        let result: Vec<(String, Uuid)> = qb.build_query_as().fetch_all(&mut *tx).await?;
+        // Decompose struct-of-rows into row-of-arrays. Nine parallel
+        // Vecs, one per column. This IS a transpose — lives for the
+        // duration of one INSERT, cheaper than N roundtrips.
+        //
+        // Nested-array columns (required_features, expected_output_paths,
+        // output_names) can't unnest as text[][] — PG's multidim arrays
+        // are rectangular, but per-row feature lists have variable
+        // length. Encode as pg text[] literals ("{a,b,c}") and cast
+        // back in the SELECT. sqlx doesn't expose a Vec<Vec<String>>
+        // → text[][] Encode anyway.
+        let mut drv_hash = Vec::with_capacity(rows.len());
+        let mut drv_path = Vec::with_capacity(rows.len());
+        let mut pname = Vec::with_capacity(rows.len());
+        let mut system = Vec::with_capacity(rows.len());
+        let mut status = Vec::with_capacity(rows.len());
+        let mut required_features = Vec::with_capacity(rows.len());
+        let mut expected_output_paths = Vec::with_capacity(rows.len());
+        let mut output_names = Vec::with_capacity(rows.len());
+        let mut is_fixed_output = Vec::with_capacity(rows.len());
+        for r in rows {
+            drv_hash.push(r.drv_hash.as_str());
+            drv_path.push(r.drv_path.as_str());
+            pname.push(r.pname.as_deref());
+            system.push(r.system.as_str());
+            status.push(r.status.as_str());
+            required_features.push(encode_pg_text_array(&r.required_features));
+            expected_output_paths.push(encode_pg_text_array(&r.expected_output_paths));
+            output_names.push(encode_pg_text_array(&r.output_names));
+            is_fixed_output.push(r.is_fixed_output);
+        }
+
+        // ON CONFLICT: update the recovery columns too. A second
+        // build requesting the same derivation may have fresher
+        // expected_output_paths (same drv_hash → same outputs, so
+        // this is idempotent in practice, but keeps the row in sync
+        // with in-mem). status/retry etc stay as-is — those reflect
+        // LIVE state, not merge-time snapshot.
+        let result: Vec<(String, Uuid)> = sqlx::query_as(
+            r#"
+            INSERT INTO derivations
+                (drv_hash, drv_path, pname, system, status, required_features,
+                 expected_output_paths, output_names, is_fixed_output)
+            SELECT
+                drv_hash, drv_path, pname, system, status,
+                required_features::text[],
+                expected_output_paths::text[],
+                output_names::text[],
+                is_fixed_output
+            FROM UNNEST(
+                $1::text[], $2::text[], $3::text[], $4::text[], $5::text[],
+                $6::text[], $7::text[], $8::text[], $9::bool[]
+            ) AS t(drv_hash, drv_path, pname, system, status,
+                   required_features, expected_output_paths, output_names,
+                   is_fixed_output)
+            ON CONFLICT (drv_hash) DO UPDATE SET
+                updated_at = now(),
+                expected_output_paths = EXCLUDED.expected_output_paths,
+                output_names = EXCLUDED.output_names,
+                is_fixed_output = EXCLUDED.is_fixed_output
+            RETURNING drv_hash, derivation_id
+            "#,
+        )
+        .bind(&drv_hash)
+        .bind(&drv_path)
+        .bind(&pname)
+        .bind(&system)
+        .bind(&status)
+        .bind(&required_features)
+        .bind(&expected_output_paths)
+        .bind(&output_names)
+        .bind(&is_fixed_output)
+        .fetch_all(&mut *tx)
+        .await?;
         Ok(result.into_iter().collect())
     }
```

New helper (put it near the top of `db.rs`, below `TERMINAL_STATUSES`):

```rust
/// Encode a `&[String]` as a PostgreSQL text-array literal: `{a,b,c}`.
/// Used for the nested-array columns in `batch_upsert_derivations` —
/// PG multidim arrays must be rectangular, so we can't bind
/// `Vec<Vec<String>>` directly. Instead: bind as flat `text[]` of
/// literals, cast back to `text[]` in the SELECT.
///
/// Escaping: double-quote each element, backslash-escape embedded
/// `"` and `\`. PG array-literal syntax, not SQL string syntax —
/// single quotes are literal, double quotes delimit.
fn encode_pg_text_array(items: &[String]) -> String {
    let mut out = String::with_capacity(2 + items.iter().map(|s| s.len() + 3).sum::<usize>());
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        for ch in item.chars() {
            match ch {
                '"' | '\\' => {
                    out.push('\\');
                    out.push(ch);
                }
                _ => out.push(ch),
            }
        }
        out.push('"');
    }
    out.push('}');
    out
}
```

**On `pname: Option<String>`:** `Vec<Option<&str>>` encodes correctly — sqlx's `text[]` encoder renders `None` as SQL `NULL`, and `UNNEST` preserves it. The nullable column stays nullable.

**Remove `QueryBuilder` import** if this is the last use site (`use sqlx::{PgConnection, PgPool, QueryBuilder}` → `use sqlx::{PgConnection, PgPool}`). Grep first — `rg 'QueryBuilder' rio-scheduler/src/db.rs`.

### Diff — `batch_insert_build_derivations`

Simpler — two scalar columns, no nested arrays.

```diff
     pub async fn batch_insert_build_derivations(
         tx: &mut PgConnection,
         build_id: Uuid,
         derivation_ids: &[Uuid],
     ) -> Result<(), sqlx::Error> {
         if derivation_ids.is_empty() {
             return Ok(());
         }
-
-        let mut qb = QueryBuilder::new("INSERT INTO build_derivations (build_id, derivation_id) ");
-        qb.push_values(derivation_ids, |mut b, did| {
-            b.push_bind(build_id).push_bind(did);
-        });
-        qb.push(" ON CONFLICT DO NOTHING");
-        qb.build().execute(&mut *tx).await?;
+        // build_id is constant across rows — bind once as scalar $1,
+        // cross-join UNNEST of the derivation_id array. Two binds total.
+        sqlx::query(
+            r#"
+            INSERT INTO build_derivations (build_id, derivation_id)
+            SELECT $1, derivation_id FROM UNNEST($2::uuid[]) AS t(derivation_id)
+            ON CONFLICT DO NOTHING
+            "#,
+        )
+        .bind(build_id)
+        .bind(derivation_ids)
+        .execute(&mut *tx)
+        .await?;
         Ok(())
     }
```

### Diff — `batch_insert_edges`

```diff
     pub async fn batch_insert_edges(
         tx: &mut PgConnection,
         edges: &[(Uuid, Uuid)],
     ) -> Result<(), sqlx::Error> {
         if edges.is_empty() {
             return Ok(());
         }
-
-        let mut qb = QueryBuilder::new("INSERT INTO derivation_edges (parent_id, child_id) ");
-        qb.push_values(edges, |mut b, (p, c)| {
-            b.push_bind(p).push_bind(c);
-        });
-        qb.push(" ON CONFLICT DO NOTHING");
-        qb.build().execute(&mut *tx).await?;
+        let (parents, children): (Vec<Uuid>, Vec<Uuid>) = edges.iter().copied().unzip();
+        sqlx::query(
+            r#"
+            INSERT INTO derivation_edges (parent_id, child_id)
+            SELECT * FROM UNNEST($1::uuid[], $2::uuid[])
+            ON CONFLICT DO NOTHING
+            "#,
+        )
+        .bind(&parents)
+        .bind(&children)
+        .execute(&mut *tx)
+        .await?;
         Ok(())
     }
```

---

## 3. `pg-partial-index-unused-parameterized` — inline the literal

### The lie

`db.rs:23-24` claims:

> Bound as `status <> ALL($1::text[])` — cleaner than a hardcoded `NOT IN (...)` literal and gets the planner the same predicate.

**False.** The migration's partial index (`migrations/004_recovery.sql:84-85`):

```sql
CREATE INDEX derivations_status_idx ON derivations (status)
    WHERE status NOT IN ('completed', 'poisoned', 'dependency_failed', 'cancelled');
```

For the planner to use a partial index, the query's `WHERE` clause must **provably imply** the index's `WHERE` clause. "Provably" means at **plan time**, before bind values are known. `$1::text[]` is opaque to the planner — it could be `{'completed'}`, it could be `{}`, it could be the full terminal set. The planner can't prove `status <> ALL($1)` ⊆ `status NOT IN (literal-set)`, so it doesn't. Seq scan.

Verify with `EXPLAIN (ANALYZE, BUFFERS)` on a populated `derivations` table:

```
-- Current (parameterized):
Seq Scan on derivations  (cost=0.00..4521.00 rows=... )
  Filter: (status <> ALL ('{completed,poisoned,dependency_failed,cancelled}'::text[]))

-- After fix (literal):
Index Scan using derivations_status_idx on derivations  (cost=0.29..812.00 rows=... )
```

Recovery runs once per leader acquisition. On a mature cluster with 500k terminal derivations and 200 in-flight, this is seq-scan-500k vs index-scan-200.

### Diff — `db.rs`

Two query sites. The `TERMINAL_STATUSES` const becomes test-only.

```diff
--- a/rio-scheduler/src/db.rs
+++ b/rio-scheduler/src/db.rs
@@ -13,17 +13,28 @@

 use crate::state::{BuildState, DerivationStatus, DrvHash, WorkerId};

-/// Terminal derivation statuses (wire strings). Single source of
-/// truth for SQL filters in both `sweep_stale_live_pins` (delete
-/// pins whose drv is terminal) and `load_nonterminal_derivations`
-/// (exclude terminal drvs from recovery). A new terminal status
-/// added to [`DerivationStatus::is_terminal`] must also be added
-/// here or recovery/GC will diverge.
-///
-/// Bound as `status <> ALL($1::text[])` — cleaner than a hardcoded
-/// `NOT IN (...)` literal and gets the planner the same predicate.
-const TERMINAL_STATUSES: &[&str] = &["completed", "poisoned", "dependency_failed", "cancelled"];
+// r[impl sched.db.partial-index-literal]
+/// Terminal statuses as a SQL `NOT IN` literal fragment.
+///
+/// MUST match both:
+///   - [`DerivationStatus::is_terminal`] (enum ground truth)
+///   - `migrations/004_recovery.sql:85` partial index predicate
+///
+/// Inlined as a literal (not bound as `$1::text[]`) so the planner
+/// can prove the query predicate implies the partial index predicate.
+/// With a bind parameter, that proof is impossible at plan time —
+/// the planner doesn't know what `$1` will contain — so the partial
+/// index is never chosen and recovery seq-scans the whole table.
+///
+/// The drift test [`tests::test_terminal_statuses_match_is_terminal`]
+/// iterates all `DerivationStatus` variants and asserts that
+/// `is_terminal() ⇔ as_str() ∈ TERMINAL_STATUSES`. Adding a new
+/// terminal status without updating this list fails that test.
+/// Updating this list without updating the migration fails the
+/// PG-side EXPLAIN check (`test_nonterminal_load_uses_partial_index`).
+const TERMINAL_STATUS_SQL: &str =
+    "('completed', 'poisoned', 'dependency_failed', 'cancelled')";
+
+#[cfg(test)]
+const TERMINAL_STATUSES: &[&str] = &["completed", "poisoned", "dependency_failed", "cancelled"];
```

The two query sites become `format!`-ed (the literal is a `const &str`, so this is still a static-shape query, no injection surface — `TERMINAL_STATUS_SQL` is a compile-time constant with no user input):

```diff
@@ -570,20 +581,22 @@
-    /// The subquery matches load_nonterminal_derivations' filter
-    /// (both use `TERMINAL_STATUSES`): a drv NOT in that set is
-    /// terminal (or deleted entirely).
+    /// The subquery matches `load_nonterminal_derivations`' filter
+    /// (both interpolate `TERMINAL_STATUS_SQL`): a drv NOT in that
+    /// set is terminal (or deleted entirely).
     pub async fn sweep_stale_live_pins(&self) -> Result<u64, sqlx::Error> {
-        let result = sqlx::query(
-            r#"
+        // format! of a compile-time const — no injection surface.
+        // See TERMINAL_STATUS_SQL doc for why this isn't a bind param.
+        let result = sqlx::query(&format!(
+            r"
             DELETE FROM scheduler_live_pins
              WHERE drv_hash NOT IN (
                SELECT drv_hash FROM derivations
-                WHERE status <> ALL($1::text[])
+                WHERE status NOT IN {TERMINAL_STATUS_SQL}
              )
-            "#,
-        )
-        .bind(TERMINAL_STATUSES)
+            "
+        ))
         .execute(&self.pool)
         .await?;
         Ok(result.rows_affected())
     }
```

```diff
@@ -845,22 +858,22 @@
-    /// Load all non-terminal derivations. Same terminal-exclusion
-    /// filter (`TERMINAL_STATUSES`) as sweep_stale_live_pins; the
-    /// partial index (status_idx) makes this efficient for large
-    /// tables.
+    /// Load all non-terminal derivations. Literal `NOT IN` so the
+    /// planner can prove the predicate implies the partial index
+    /// predicate (`migrations/004_recovery.sql:85`). Same exclusion
+    /// set as `sweep_stale_live_pins`.
     pub async fn load_nonterminal_derivations(
         &self,
     ) -> Result<Vec<RecoveryDerivationRow>, sqlx::Error> {
-        sqlx::query_as(
-            r#"
+        sqlx::query_as(&format!(
+            r"
             SELECT derivation_id, drv_hash, drv_path, pname, system, status,
                    required_features, assigned_worker_id, retry_count,
                    expected_output_paths, output_names, is_fixed_output,
                    failed_workers
             FROM derivations
-            WHERE status <> ALL($1::text[])
-            "#,
-        )
-        .bind(TERMINAL_STATUSES)
+            WHERE status NOT IN {TERMINAL_STATUS_SQL}
+            "
+        ))
         .fetch_all(&self.pool)
         .await
     }
```

### Drift test

Exhaustive check that the Rust const and the enum stay in sync. Goes in `db.rs`'s existing `#[cfg(test)] mod tests` block (near `test_assignment_status_is_terminal` at line ~1172):

```rust
// r[verify sched.db.partial-index-literal]
/// Drift guard: `TERMINAL_STATUSES` const ⇔ `DerivationStatus::is_terminal()`.
///
/// Exhaustive over all variants. If you add a variant without
/// updating the match arm below, `non_exhaustive_patterns` fires.
/// If you add it to the match but flip terminality without updating
/// `TERMINAL_STATUSES`, the assertion fires.
///
/// ALSO asserts `TERMINAL_STATUS_SQL` is the `(a,b,c)`-joined form
/// of `TERMINAL_STATUSES` — catches the case where someone edits
/// one but not the other.
#[test]
fn test_terminal_statuses_match_is_terminal() {
    use crate::state::DerivationStatus::*;
    // Exhaustive binding — not `_ =>` — so adding a variant to
    // the enum is a compile error here until you handle it.
    let all = [
        Created, Queued, Ready, Assigned, Running, Completed, Failed,
        Poisoned, DependencyFailed, Cancelled,
    ];
    // Compile-time exhaustiveness: this match has no wildcard.
    // Add a variant → this function stops compiling.
    for v in all {
        match v {
            Created | Queued | Ready | Assigned | Running | Completed
            | Failed | Poisoned | DependencyFailed | Cancelled => {}
        }
    }

    let terminal_set: std::collections::HashSet<&str> =
        TERMINAL_STATUSES.iter().copied().collect();

    for v in all {
        let in_const = terminal_set.contains(v.as_str());
        let is_term = v.is_terminal();
        assert_eq!(
            in_const, is_term,
            "TERMINAL_STATUSES drift: {v:?}.is_terminal()={is_term} but \
             presence in TERMINAL_STATUSES={in_const}. Update db.rs const \
             AND migrations/004_recovery.sql:85 partial index predicate."
        );
    }

    // SQL literal must be the quoted, comma-joined form of the const.
    // Order matters here only in the sense that both sides agree —
    // PG doesn't care about IN-list order.
    let expected_sql = format!(
        "({})",
        TERMINAL_STATUSES
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert_eq!(
        TERMINAL_STATUS_SQL, expected_sql,
        "TERMINAL_STATUS_SQL out of sync with TERMINAL_STATUSES const"
    );
}
```

**On the migration side:** there's no cheap way to assert the migration's index predicate from a unit test without booting PG. Add a second test that queries `pg_indexes`:

```rust
/// r[verify sched.db.partial-index-literal]
/// PG-side drift check: the partial index predicate in
/// `pg_indexes.indexdef` must match `TERMINAL_STATUS_SQL`.
///
/// PG normalizes the predicate (adds `::text` casts, may reorder),
/// so we check that every status in `TERMINAL_STATUSES` appears as
/// a quoted literal in the indexdef, not that the strings are equal.
#[tokio::test]
async fn test_partial_index_predicate_matches_const() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let (indexdef,): (String,) = sqlx::query_as(
        "SELECT indexdef FROM pg_indexes \
         WHERE tablename = 'derivations' AND indexname = 'derivations_status_idx'",
    )
    .fetch_one(&test_db.pool)
    .await?;

    for status in TERMINAL_STATUSES {
        assert!(
            indexdef.contains(&format!("'{status}'")),
            "partial index predicate missing '{status}': {indexdef}"
        );
    }
    // And nothing extra — count quoted literals. PG normalizes
    // NOT IN (a,b) to <> ALL(ARRAY[a,b]) in indexdef output, so
    // also count via that form.
    let literal_count = indexdef.matches("'::text").count();
    assert_eq!(
        literal_count,
        TERMINAL_STATUSES.len(),
        "partial index has {literal_count} status literals, \
         TERMINAL_STATUSES has {}: {indexdef}",
        TERMINAL_STATUSES.len()
    );
    Ok(())
}
```

**Optional EXPLAIN check** (catches the actual planner regression, not just textual drift):

```rust
#[tokio::test]
async fn test_nonterminal_load_uses_partial_index() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    // Need enough rows that the planner prefers an index over seq
    // scan. On an empty table it'll always seq scan (cheaper). Insert
    // 2000 terminal + 20 nonterminal.
    let mut tx = test_db.pool.begin().await?;
    for i in 0..2000 {
        let row = DerivationRow {
            drv_hash: format!("term{i:04}{}", "a".repeat(24)),
            drv_path: rio_test_support::fixtures::test_drv_path(&format!("term{i:04}")),
            pname: None,
            system: "x86_64-linux".into(),
            status: DerivationStatus::Completed,
            required_features: vec![],
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
        };
        SchedulerDb::batch_upsert_derivations(&mut tx, &[row]).await?;
    }
    // ... 20 nonterminal rows ...
    tx.commit().await?;
    sqlx::query("ANALYZE derivations").execute(&test_db.pool).await?;

    let (plan,): (String,) = sqlx::query_as(&format!(
        "EXPLAIN (FORMAT TEXT) SELECT derivation_id FROM derivations \
         WHERE status NOT IN {TERMINAL_STATUS_SQL}"
    ))
    .fetch_one(&test_db.pool)
    .await?;

    assert!(
        plan.contains("derivations_status_idx"),
        "query does not use partial index; plan:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan"),
        "query uses seq scan; plan:\n{plan}"
    );
    Ok(())
}
```

**Implementation note:** `EXPLAIN` returns multiple rows (one per plan line), not one. Use `fetch_all` and `join("\n")`, or `EXPLAIN (FORMAT JSON)` and parse. Sketch above is illustrative — fix at implementation time.

---

## 4. `pg-dual-migrate-race` — designate store as migration owner

### Current

Both binaries embed the same `migrations/` dir and both run it at startup:

- `rio-scheduler/src/main.rs:184`: `sqlx::migrate!("../migrations").run(&pool).await?;`
- `rio-store/src/main.rs:180-183`: same

sqlx's `Migrator::run` takes a PG advisory lock (`pg_advisory_lock(...)` on a hash of the migrations dir). Second caller blocks until first releases. Not a correctness bug — sqlx handles the serialization — but:

- The loser waits (~50-200ms typically, but unbounded if the winner's migration is slow)
- Two pods log "database migrations applied" — misleading during incident triage ("which one actually applied 009?")
- Adding a migration that's only safe to run from one binary's context (e.g., a data migration that reads scheduler-specific state) becomes a latent footgun

### Fix

Store owns migrations. Scheduler checks the schema is at the expected version and bails if not.

**Why store, not scheduler:**
- Store is the "smaller surface" service — fewer runtime dependencies, comes up faster
- Scheduler already depends on store being reachable (`main.rs:192` `connect_store`) so the implicit ordering is already store-first
- `rio-store/src/lib.rs:34` already has `MIGRATOR` as `pub(crate)`; scheduler's is `pub` (`lib.rs:41`) only for `TestDb` — the test helper can keep using it

```diff
--- a/rio-scheduler/src/main.rs
+++ b/rio-scheduler/src/main.rs
@@ -23,6 +23,9 @@
 struct Config {
     listen_addr: String,
     store_addr: String,
     database_url: String,
+    /// Skip running migrations at startup. Default `true` — the
+    /// store is the designated migration owner; scheduler only
+    /// verifies schema version. Set `false` for standalone dev.
+    skip_migrations: bool,
     metrics_addr: std::net::SocketAddr,
@@ -61,6 +64,7 @@
             database_url: String::new(),
+            skip_migrations: true,
             metrics_addr: "0.0.0.0:9091".parse().unwrap(),
@@ -181,8 +185,29 @@

     info!("connected to PostgreSQL");

-    sqlx::migrate!("../migrations").run(&pool).await?;
-    info!("database migrations applied");
+    if cfg.skip_migrations {
+        // Store is the migration owner. We just verify the schema
+        // is at (or past) the version we were compiled against.
+        // Under-version → bail; the store hasn't come up yet or
+        // is running an older image. Let k8s restart us.
+        let expected = rio_scheduler::MIGRATOR
+            .iter()
+            .map(|m| m.version)
+            .max()
+            .unwrap_or(0);
+        let applied: Option<i64> = sqlx::query_scalar(
+            "SELECT MAX(version) FROM _sqlx_migrations WHERE success",
+        )
+        .fetch_one(&pool)
+        .await?;
+        let applied = applied.unwrap_or(0);
+        anyhow::ensure!(
+            applied >= expected,
+            "schema at version {applied}, need >= {expected}; \
+             waiting for rio-store to apply migrations"
+        );
+        info!(applied, expected, "schema version verified");
+    } else {
+        sqlx::migrate!("../migrations").run(&pool).await?;
+        info!("database migrations applied (skip_migrations=false)");
+    }
```

**`nix/modules/scheduler.nix:33`** comment ("rio-scheduler applies migrations (sqlx migrate) on startup") becomes stale — update it. The NixOS VM tests in `nix/tests/common.nix:87` rely on whichever-comes-up-first applying migrations; after this change, store **must** come up first, or scheduler crash-loops until it does. That's fine for k3s tests (StatefulSet ordinals + both in same image pull), but check whether any test spawns scheduler-only without store.

```bash
rg -l 'rio-scheduler' nix/tests/ | xargs rg -L 'rio-store'
```

If any hits, either set `RIO_SKIP_MIGRATIONS=false` in that test's env or add store to the fixture.

---

## 5. `pg-pool-size-hardcoded` — figment key

Mechanical. Two sites, same shape.

```diff
--- a/rio-scheduler/src/main.rs
+++ b/rio-scheduler/src/main.rs
@@ -26,6 +26,10 @@
     database_url: String,
     skip_migrations: bool,
+    /// PG connection pool cap. Default 10: actor is single-task so
+    /// most tx load is serialized; the pool is mostly for the
+    /// event_log writer + admin endpoints. Raise for high-throughput
+    /// admin query load.
+    pg_max_connections: u32,
     metrics_addr: std::net::SocketAddr,
@@ -65,6 +69,7 @@
             skip_migrations: true,
+            pg_max_connections: 10,
             metrics_addr: "0.0.0.0:9091".parse().unwrap(),
@@ -177,7 +182,7 @@
     let pool = sqlx::postgres::PgPoolOptions::new()
-        .max_connections(10)
+        .max_connections(cfg.pg_max_connections)
         .connect(&cfg.database_url)
```

Same for `rio-store/src/main.rs:175` (default `20` per current hardcode — store has more concurrent readers: gRPC handlers + cache-server HTTP + GC sweep).

Env var: `RIO_PG_MAX_CONNECTIONS` (figment's default `snake_case` → `UPPER_SNAKE` with `RIO_` prefix).

---

## 6. `pg-zero-compile-time-checked-queries` — **tooling landed — P0297; incremental conversion ongoing**

### Why deferred (historical — tooling now in place)

`sqlx prepare` + `SQLX_OFFLINE=true` in the Nix build would give compile-time SQL checking for all `query!`/`query_as!` macro callsites. Zero currently — 194 unchecked `query(...)` strings.

**Invasive because:**

1. **`.sqlx/` directory in git.** `cargo sqlx prepare --workspace` writes one JSON file per query to `.sqlx/`. These must be committed. They're content-addressed (filename = hash of SQL + args), so changing any query creates new files and orphans old ones. Churn in every PR that touches SQL. Needs a `sqlx-prepare-check` in `treefmt` or `pre-commit` or it'll rot.

2. **Crane source filter.** `flake.nix:168` mentions migrations are in the filter; `.sqlx/*.json` must be added too, and the depsOnly build needs `SQLX_OFFLINE=true` set so the macro doesn't try to connect during the deps pass.

3. **Dev-shell DATABASE_URL.** Running `cargo sqlx prepare` requires a live database matching the current migration state. The ephemeral `rio-test-support` PG works, but `sqlx prepare` doesn't know about it. Either wire a `just sqlx-prepare` target that spins up `TestDb` and sets `DATABASE_URL`, or document "set `DATABASE_URL` to your dev PG before preparing."

4. **Conversion cost.** `query!` requires the output struct to derive `FromRow` **and** have named columns matching the SELECT. Most current callsites use tuple `query_as::<(String, Uuid)>` — converting means either adding throwaway structs or rewriting SELECTs with `AS` aliases matching field names. 194 sites × ~5min each = ~16h of mechanical work that should be a dedicated PR.

5. **`format!` queries can't convert.** §3's `TERMINAL_STATUS_SQL` interpolation doesn't work with `query!` — the macro needs a string literal. Those two sites stay unchecked forever (or we give up the partial-index optimization, which we won't).

**Recommendation:** file as a standalone follow-up task. Start with the 12 queries the §3 table mentions ("queries that have had runtime SQL errors in git history") — `git log -p --all -S 'sqlx::Error' rio-scheduler/src/db.rs | grep -B5 'query'` to find them. Land the tooling (Nix `SQLX_OFFLINE`, `just sqlx-prepare`, `.sqlx/` in source filter) with those 12 as the proof, then convert the rest incrementally.

Not blocking this remediation. `TODO(phase4b)` comment at the top of `db.rs`:

```rust
// TODO(phase4b): convert query("...") → query!(...) for compile-time
// SQL checking. Blocked on .sqlx/ in Crane source filter + just
// sqlx-prepare target. See remediations/phase4a/12-pg-transaction-safety.md §6.
```

---

## 7. Tests

### 10k-node param limit — the headline test

Proves §2's fix actually fixes the problem. Goes in `db.rs` tests (uses `TestDb`, runs against real PG).

```rust
// r[verify sched.db.batch-unnest]
/// Large-DAG persistence: 10k nodes, 15k edges. Would fail on main
/// with "bind message has 90000 parameter formats but 0 parameters"
/// (or similar — sqlx catches it before PG does) at 7282 nodes.
///
/// 10k is past the old derivations limit (7281) AND past the old
/// edges limit at E=15k (old: 2 cols × 32768 = limit, 15k is safe;
/// but we test 40k edges separately below to cover that too).
#[tokio::test]
async fn test_batch_upsert_10k_nodes() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    const N: usize = 10_000;
    let rows: Vec<DerivationRow> = (0..N)
        .map(|i| DerivationRow {
            drv_hash: format!("{i:032x}"),  // 32-hex-char fake hash
            drv_path: format!(
                "/nix/store/{}-test-{i}.drv",
                "a".repeat(32)
            ),
            pname: Some(format!("pkg-{i}")),
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            // Exercise the nested-array encoding: varied lengths
            // including empty (which was the rectangular-array
            // failure mode if we'd tried text[][]).
            required_features: match i % 3 {
                0 => vec![],
                1 => vec!["kvm".into()],
                _ => vec!["kvm".into(), "big-parallel".into()],
            },
            expected_output_paths: vec![format!(
                "/nix/store/{}-out-{i}",
                "b".repeat(32)
            )],
            output_names: vec!["out".into()],
            is_fixed_output: i % 7 == 0,
        })
        .collect();

    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows).await?;
    tx.commit().await?;

    assert_eq!(id_map.len(), N, "RETURNING gave back every row");
    // Spot-check: row 0 and row N-1 both present, distinct ids.
    let id0 = id_map.get(&format!("{:032x}", 0)).copied().unwrap();
    let idN = id_map.get(&format!("{:032x}", N - 1)).copied().unwrap();
    assert_ne!(id0, idN);

    // And they actually landed in PG, with nested arrays intact.
    let (features,): (Vec<String>,) = sqlx::query_as(
        "SELECT required_features FROM derivations WHERE drv_hash = $1",
    )
    .bind(format!("{:032x}", 2))  // i=2 → i%3==2 → [kvm, big-parallel]
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(features, vec!["kvm", "big-parallel"]);

    Ok(())
}

/// Edges: 40k rows. Old limit was 32767 (2 cols). Build a
/// linear chain over the 10k nodes from the previous test's
/// shape (fresh DB, so re-insert).
#[tokio::test]
async fn test_batch_insert_40k_edges() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Need N nodes first (FK constraint). Reuse shape from above.
    const N: usize = 10_000;
    let rows: Vec<DerivationRow> = (0..N)
        .map(|i| DerivationRow {
            drv_hash: format!("{i:032x}"),
            drv_path: format!("/nix/store/{}-e{i}.drv", "a".repeat(32)),
            pname: None,
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            required_features: vec![],
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
        })
        .collect();
    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows).await?;

    // 40k edges: each node i>0 has 4 parents among [i-1, i-2, ...].
    // ON CONFLICT DO NOTHING dedups any collisions.
    let ids: Vec<Uuid> = (0..N)
        .map(|i| *id_map.get(&format!("{i:032x}")).unwrap())
        .collect();
    let edges: Vec<(Uuid, Uuid)> = (1..N)
        .flat_map(|i| {
            (1..=4.min(i)).map(move |d| (ids[i - d], ids[i]))
        })
        .collect();
    assert!(edges.len() > 32_768, "test must exceed old 2-col limit");

    SchedulerDb::batch_insert_edges(&mut tx, &edges).await?;
    tx.commit().await?;

    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM derivation_edges",
    )
    .fetch_one(&test_db.pool)
    .await?;
    // ≤ edges.len() because of ON CONFLICT dedup, but > old limit.
    assert!(count > 32_768);
    Ok(())
}
```

### `encode_pg_text_array` roundtrip + escaping

Unit test, no PG needed:

```rust
#[test]
fn test_encode_pg_text_array() {
    assert_eq!(encode_pg_text_array(&[]), "{}");
    assert_eq!(encode_pg_text_array(&["a".into()]), r#"{"a"}"#);
    assert_eq!(
        encode_pg_text_array(&["a".into(), "b".into()]),
        r#"{"a","b"}"#
    );
    // Escaping: embedded double-quote and backslash.
    assert_eq!(
        encode_pg_text_array(&[r#"has"quote"#.into()]),
        r#"{"has\"quote"}"#
    );
    assert_eq!(
        encode_pg_text_array(&[r"has\backslash".into()]),
        r#"{"has\\backslash"}"#
    );
    // Comma inside a value is fine — double-quoting handles it.
    assert_eq!(
        encode_pg_text_array(&["a,b".into()]),
        r#"{"a,b"}"#
    );
}
```

And a PG roundtrip (proves PG's parser agrees with our encoder):

```rust
#[tokio::test]
async fn test_encode_pg_text_array_roundtrip() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let cases: &[&[&str]] = &[
        &[],
        &["plain"],
        &["has\"quote", "has\\slash", "has,comma", "has{brace}"],
    ];
    for case in cases {
        let input: Vec<String> = case.iter().map(|s| s.to_string()).collect();
        let encoded = encode_pg_text_array(&input);
        let (decoded,): (Vec<String>,) =
            sqlx::query_as("SELECT $1::text[]")
                .bind(&encoded)
                .fetch_one(&test_db.pool)
                .await?;
        assert_eq!(decoded, input, "roundtrip failed for {encoded:?}");
    }
    Ok(())
}
```

### In-mem mutation ordering (§1)

Harder to test directly — requires injecting a failure between `batch_insert_edges` and `commit`. Options:

**a) Mock the pool** — not worth it; sqlx doesn't have a test-double story and wrapping `PgPool` ripples.

**b) Assertion in the code path itself** — before `tx.commit()`, `debug_assert!` that no newly-inserted node has `db_id.is_some()`:

```rust
// Before tx.commit().await?:
#[cfg(debug_assertions)]
for hash in newly_inserted {
    if let Some(state) = self.dag.node(hash.as_str()) {
        debug_assert!(
            state.db_id.is_none(),
            "newly-inserted node {hash} has db_id set before commit — \
             in-mem mutation leaked into tx scope"
        );
    }
}
```

This fires in every test that exercises `persist_merge_to_db`, including the existing `rio-scheduler/src/actor/tests/merge.rs` suite. Cheap, catches regressions where someone re-introduces the in-tx write.

**c) Integration test via FK violation** — make `batch_insert_edges` fail by passing an edge with a `child_id` that doesn't exist (use a random `Uuid::new_v4()`). The tx rolls back. Assert that none of the newly-inserted nodes have `db_id` set afterward. Requires exposing `persist_merge_to_db` as `pub(crate)` or adding a test-only wrapper. Do this **only if** the debug_assert approach isn't catching it in existing merge tests.

Recommendation: **(b)** is sufficient. The existing merge tests (`actor/tests/merge.rs`) already cover the happy path; the debug_assert makes them also cover the ordering invariant for free.

---

## 8. Commit shape

Single commit. All three P1 findings + the two P2 riders. Message:

```
fix(scheduler): PG tx safety — commit-before-mutate, UNNEST batch, literal NOT IN

Three independent fixes that happen to touch adjacent code:

- merge.rs: move self.dag.node_mut().db_id write to AFTER tx.commit().
  Edge resolution now uses the tx-local id_map instead of reading
  back through self.dag (which required the in-tx mutation). The
  previous code was accidentally correct — rollback_merge removed
  the phantom-db_id nodes wholesale — but relied on three nonlocal
  invariants. Now trivially correct by construction.

- db.rs: batch_upsert_derivations + batch_insert_build_derivations +
  batch_insert_edges switch from QueryBuilder::push_values (9×N bind
  params, fails at 7282 rows) to UNNEST array params (9 binds total).
  NixOS system closures are ~30k derivations.

- db.rs: inline TERMINAL_STATUSES as SQL literal so the planner can
  prove the query implies the partial index predicate. The old
  `status <> ALL($1)` parameterized form was opaque at plan time —
  seq scan on every recovery. Drift test asserts the const, the
  enum's is_terminal(), and the migration's index predicate agree.

Also:
- Store designated as migration owner; scheduler skip_migrations=true
  by default (verifies schema version instead of racing the lock).
- pg_max_connections exposed via figment (was hardcoded 10/20).

Tests: 10k-node batch upsert, 40k-edge batch insert, TERMINAL_STATUSES
drift (enum + const + pg_indexes), encode_pg_text_array roundtrip.

Fixes: pg-in-mem-mutation-inside-tx, pg-querybuilder-param-limit,
pg-partial-index-unused-parameterized, pg-dual-migrate-race,
pg-pool-size-hardcoded
```

---

## 9. Validation checklist

- [ ] `nix develop -c cargo nextest run -p rio-scheduler` — unit + TestDb integration
- [ ] `nix develop -c cargo clippy --all-targets -- --deny warnings`
- [ ] `tracey query rule sched.db.tx-commit-before-mutate` — shows impl + verify
- [ ] `tracey query rule sched.db.batch-unnest` — shows impl + verify
- [ ] `tracey query rule sched.db.partial-index-literal` — shows impl + verify
- [ ] `rg 'QueryBuilder' rio-scheduler/src/db.rs` — empty (import removed if last use)
- [ ] VM test `phase3a` still green (exercises recovery → `load_nonterminal_derivations`)
- [ ] Check `nix/modules/scheduler.nix:33` comment updated for skip_migrations
- [ ] `rg -l 'rio-scheduler' nix/tests/ | xargs rg -L 'rio-store'` — no scheduler-only fixtures, or they set `RIO_SKIP_MIGRATIONS=false`

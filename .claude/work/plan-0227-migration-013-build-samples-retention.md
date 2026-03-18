# Plan 0227: Migration 013 — build_samples table + retention task spawn

**SITA-E spine head.** phase4c.md:13 — the `CutoffRebalancer` (P0229) needs historical per-build `(duration, peak_memory)` samples to compute equal-load cutoffs. Today the scheduler records EMAs per `(pname, system)` (at [`db.rs:1128,1141`](../../rio-scheduler/src/db.rs)), but EMAs destroy the distribution shape — you can't partition into equal-load buckets from a single smoothed number. The rebalancer needs **raw samples**.

**Migration number per A1:** `migrations/` ends at 011 today; [P0206](plan-0206-path-tenants-migration-upsert.md) creates 012. This is **013**. Verify at dispatch: `ls migrations/ | sort -V | tail -1` should show `012_path_tenants.sql`; if it shows something else, use next-free.

**NEVER append to 009.** The stale comment at `migrations/009_phase4.sql:8` ("Part D appended later") was the design intent before 010/011/012 landed. `sqlx::migrate!` is checksum-checked — any byte change to 009 breaks every persistent DB. This plan DELETES that stale comment line (a checksum-changing edit to a NEW migration is fine; to an OLD one is not — wait, deleting from 009 IS a checksum change. Instead: leave 009 alone, document the overtaken intent in 013's header).

## Entry criteria

- [P0206](plan-0206-path-tenants-migration-upsert.md) merged (migration 012 exists; same `migrations/` dir + `.sqlx/` regen coordination)

## Tasks

### T1 — `feat(scheduler):` migration 013 — build_samples table

NEW `migrations/013_build_samples.sql`:

```sql
-- migration 013: build_samples — raw per-build completion samples for
-- the CutoffRebalancer (P0229). EMAs destroy distribution shape; the
-- rebalancer needs raw (duration, peak_mem) pairs to partition into
-- equal-load size classes.
--
-- Retention: scheduler main.rs spawns a 1h-interval task that deletes
-- rows older than 30 days. Index on completed_at supports the range
-- delete.
--
-- NOTE: migrations/009_phase4.sql:8 has a stale "Part D appended later"
-- comment. That intent is overtaken — 010/011/012 landed after 009,
-- and sqlx migrate checksums prevent any 009 edit. 013 is correct.

CREATE TABLE build_samples (
    id                 BIGSERIAL PRIMARY KEY,
    pname              TEXT NOT NULL,
    system             TEXT NOT NULL,
    duration_secs      DOUBLE PRECISION NOT NULL,
    peak_memory_bytes  BIGINT NOT NULL,
    completed_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX build_samples_completed_at_idx ON build_samples (completed_at);
```

No FK to `build_history` — samples outlive individual build rows (GC may delete build_history; samples persist for the rebalancer's 7-day window).

### T2 — `feat(scheduler):` db.rs query stubs

MODIFY [`rio-scheduler/src/db.rs`](../../rio-scheduler/src/db.rs) — two functions. Near `:1087` (wherever the EMA write lives):

```rust
/// Insert one raw build sample. Called from completion.rs success path
/// alongside the EMA update. Best-effort: caller warns on Err.
pub async fn insert_build_sample(
    &self,
    pname: &str,
    system: &str,
    duration_secs: f64,
    peak_memory_bytes: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        "INSERT INTO build_samples (pname, system, duration_secs, peak_memory_bytes)
         VALUES ($1, $2, $3, $4)",
        pname, system, duration_secs, peak_memory_bytes
    )
    .execute(&self.pool)
    .await?;
    Ok(())
}

/// Delete samples older than the given cutoff. Returns rows deleted.
/// Called by the retention task on a 1h interval.
pub async fn delete_samples_older_than(
    &self,
    days: u32,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query!(
        "DELETE FROM build_samples WHERE completed_at < now() - ($1 || ' days')::interval",
        days.to_string()
    )
    .execute(&self.pool)
    .await?;
    Ok(result.rows_affected())
}
```

**MUST run `cargo sqlx prepare --workspace` and commit `.sqlx/`** — P0228's `insert_build_sample` call fails sqlx compile-check without it. This is a hard exit criterion (R13).

### T3 — `feat(scheduler):` retention task spawn in main.rs

MODIFY [`rio-scheduler/src/main.rs`](../../rio-scheduler/src/main.rs) — near the existing background-task spawn block (grep for `tokio::spawn` or `interval`):

```rust
// build_samples retention: delete rows older than 30 days, hourly.
// 30 days > rebalancer's 7-day query window (P0229) with margin.
tokio::spawn({
    let db = db.clone();
    async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3600));
        loop {
            interval.tick().await;
            match db.delete_samples_older_than(30).await {
                Ok(n) if n > 0 => info!(rows_deleted = n, "build_samples retention sweep"),
                Ok(_) => {},
                Err(e) => warn!(?e, "build_samples retention failed"),
            }
        }
    }
});
```

~5 lines in main.rs. Goes next to the other long-running bg tasks.

## Exit criteria

- `/nbr .#ci` green
- `sqlx migrate run` applies 013 cleanly (on ephemeral PG)
- `SELECT * FROM build_samples` returns empty result, schema correct (`\d build_samples` shows 5 columns + PK + index)
- `.sqlx/` regenerated and committed (`git diff --cached --name-only | grep .sqlx` non-empty)
- `grep 'delete_samples_older_than' rio-scheduler/src/main.rs` finds the retention spawn

## Tracey

No markers — the table is infrastructure for P0229's `r[sched.rebalancer.sita-e]`; that marker lands with the algorithm, not the schema.

## Files

```json files
[
  {"path": "migrations/013_build_samples.sql", "action": "NEW", "note": "T1: build_samples table + completed_at index"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "T2: insert_build_sample + delete_samples_older_than near :1087"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "T3: retention task spawn (~5 lines near existing bg-task block)"}
]
```

```
migrations/
└── 013_build_samples.sql      # T1 (NEW)
rio-scheduler/src/
├── db.rs                      # T2: 2 fns
└── main.rs                    # T3: retention spawn
.sqlx/                         # REGEN + COMMIT (not listed; git add .sqlx/)
```

## Dependencies

```json deps
{"deps": [206], "soft_deps": [], "note": "SITA-E spine head. deps:[P0206] — migration 012 must exist first (same dir + .sqlx regen). Unblocks entire spine (6 plans). Dispatch priority #2 in Wave 1."}
```

**Depends on:** [P0206](plan-0206-path-tenants-migration-upsert.md) — migration 012 must exist; `.sqlx/` regen coordination (both plans run `cargo sqlx prepare`).
**Conflicts with:** `db.rs` count=29, `main.rs` count=26 — both are small additions (2 fns + 1 spawn block). No other 4c plan touches main.rs. P0228/P0229 touch db.rs but dep-serialized via spine.

**Hidden checks at dispatch:**
1. `ls migrations/ | sort -V | tail -1` → expect `012_path_tenants.sql`; use next-free
2. `nix develop -c cargo sqlx prepare --workspace` MUST run before commit — P0228 sqlx compile-check fails without it

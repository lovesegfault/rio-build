//! `build_samples` retention + `sla_overrides` CRUD.

use super::SchedulerDb;

/// One `build_samples` row. ADR-023 SLA telemetry: every successful
/// completion appends a full-width row; the SLA fit reads these to
/// compute per-`(pname, system, tenant)` cores-vs-duration curves.
///
/// All telemetry columns are `Option` — old executors / recovered
/// derivations / non-k8s test runs leave them `None` → SQL NULL.
/// `hw_class` comes from `CompletionReport.hw_class` (builder reads
/// `RIO_HW_CLASS` from the controller-stamped pod annotation).
///
/// `Default` so tests can `BuildSampleRow { pname: "x".into(),
/// ..Default::default() }` without spelling out 15 Nones.
#[derive(Debug, Clone, Default)]
pub struct BuildSampleRow {
    /// `BIGSERIAL` PK. Ignored on write (server-assigned); populated on
    /// read so the MAD outlier sweep can `mark_outliers_excluded([id])`
    /// without re-keying on `(pname, system, tenant, completed_at)`.
    pub id: i64,
    pub pname: String,
    pub system: String,
    pub tenant: String,
    pub duration_secs: f64,
    pub peak_memory_bytes: i64,
    pub cpu_limit_cores: Option<f64>,
    pub peak_cpu_cores: Option<f64>,
    pub cpu_seconds_total: Option<f64>,
    pub peak_disk_bytes: Option<i64>,
    pub peak_io_pressure_pct: Option<f64>,
    pub version: Option<String>,
    pub hw_class: Option<String>,
    pub node_name: Option<String>,
    pub enable_parallel_building: Option<bool>,
    pub enable_parallel_checking: Option<bool>,
    pub prefer_local_build: Option<bool>,
    /// drv `outputHashMode` set (strict `is_fixed_output` predicate).
    /// ADR-023 §A17: FOD fleet-prior exclusion is keyed on this, not
    /// pname-absence — named FODs (`fetchurl { name = … }`) exist.
    /// `None` on rows written before migration 057.
    pub is_fixed_output: Option<bool>,
    /// Unix epoch seconds. Read via `EXTRACT(EPOCH FROM completed_at)` —
    /// the workspace sqlx has no chrono/time feature, so TIMESTAMPTZ
    /// round-trips as f64 epoch (matches `tenants.rs` pattern). On the
    /// write path this field is ignored; `write_build_sample` always
    /// uses server-side `now()`.
    pub completed_at: f64,
}

/// One `sla_overrides` row. ADR-023 phase-6 operator pins. NULL
/// `system`/`tenant` are wildcards — `sla::override::resolve`
/// matches most-specific first. `expires_at` is Unix-epoch f64 (same
/// no-chrono workaround as `BuildSampleRow.completed_at`); `None` =
/// never expires.
#[derive(Debug, Clone, Default)]
pub struct SlaOverrideRow {
    pub id: i64,
    pub pname: String,
    pub system: Option<String>,
    pub tenant: Option<String>,
    pub cluster: Option<String>,
    pub tier: Option<String>,
    // p50/p90/p99_secs + capacity_type: schema-only. Never wired into
    // the solver and dropped from proto/CLI; kept here so the
    // `query_as!` SELECT/INSERT round-trip stays compile-checked
    // without a `.sqlx` regen. Column GC is a future migration.
    pub p50_secs: Option<f64>,
    pub p90_secs: Option<f64>,
    pub p99_secs: Option<f64>,
    pub cores: Option<f64>,
    pub mem_bytes: Option<i64>,
    pub capacity_type: Option<String>,
    pub expires_at: Option<f64>,
    pub created_at: f64,
    pub created_by: Option<String>,
}

impl SchedulerDb {
    /// Insert one raw build sample with full ADR-023 telemetry. Called
    /// from completion.rs success path. Best-effort: caller warns on Err.
    ///
    /// Appends every completion — the SLA fit needs the full
    /// distribution, not a smoothed scalar. `completed_at` is
    /// server-side `now()`; `outlier_excluded` keeps its DEFAULT FALSE
    /// (the MAD sweep flips it later).
    pub async fn write_build_sample(&self, row: &BuildSampleRow) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "INSERT INTO build_samples
               (pname, system, tenant, duration_secs, peak_memory_bytes,
                cpu_limit_cores, peak_cpu_cores, cpu_seconds_total,
                peak_disk_bytes, peak_io_pressure_pct, version, hw_class,
                node_name, enable_parallel_building, enable_parallel_checking,
                prefer_local_build, is_fixed_output, completed_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,now())",
            row.pname,
            row.system,
            row.tenant,
            row.duration_secs,
            row.peak_memory_bytes,
            row.cpu_limit_cores,
            row.peak_cpu_cores,
            row.cpu_seconds_total,
            row.peak_disk_bytes,
            row.peak_io_pressure_pct,
            row.version,
            row.hw_class,
            row.node_name,
            row.enable_parallel_building,
            row.enable_parallel_checking,
            row.prefer_local_build,
            row.is_fixed_output,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete samples older than `days`. Returns rows deleted.
    /// Called by the retention task on a 1h interval.
    ///
    /// Range delete on `completed_at` — covered by
    /// `build_samples_completed_at_idx` (migration 013).
    pub async fn delete_samples_older_than(&self, days: u32) -> Result<u64, sqlx::Error> {
        // `$1 * interval '1 day'` with an i32 bind — PG interval
        // arithmetic. Avoids the `($1 || ' days')::interval` text-cast
        // detour (which would take a &str bind).
        let result = sqlx::query!(
            "DELETE FROM build_samples WHERE completed_at < now() - $1 * interval '1 day'",
            days as i32,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// All samples completed strictly after `since_epoch` (Unix seconds),
    /// excluding MAD-flagged outliers. Feeds [`SlaEstimator::refresh`] —
    /// the estimator only needs to know WHICH `(pname, system, tenant)`
    /// keys were touched since the last tick; the actual fit re-reads
    /// the per-key ring via [`Self::read_build_samples_for_key`].
    ///
    /// Range on `completed_at` — covered by `build_samples_incremental_idx`
    /// (migration 039).
    ///
    /// [`SlaEstimator::refresh`]: crate::sla::SlaEstimator::refresh
    pub async fn read_build_samples_incremental(
        &self,
        since_epoch: f64,
    ) -> Result<Vec<BuildSampleRow>, sqlx::Error> {
        sqlx::query_as!(
            BuildSampleRow,
            r#"
            SELECT id, pname, system, tenant, duration_secs, peak_memory_bytes,
                   cpu_limit_cores, peak_cpu_cores, cpu_seconds_total,
                   peak_disk_bytes, peak_io_pressure_pct, version, hw_class,
                   node_name, enable_parallel_building, enable_parallel_checking,
                   prefer_local_build, is_fixed_output,
                   EXTRACT(EPOCH FROM completed_at)::float8 AS "completed_at!"
            FROM build_samples
            WHERE completed_at > to_timestamp($1) AND NOT outlier_excluded
            ORDER BY completed_at
            "#,
            since_epoch,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Flip `outlier_excluded = TRUE` for a batch of rows by PK.
    /// Idempotent. Rows stay for forensics; both per-key and
    /// incremental reads already filter `WHERE NOT outlier_excluded`,
    /// so flagged samples drop out of the next refit without a DELETE.
    /// Empty `ids` → no-op (skips the round-trip).
    pub async fn mark_outliers_excluded(&self, ids: &[i64]) -> Result<(), sqlx::Error> {
        if ids.is_empty() {
            return Ok(());
        }
        sqlx::query!(
            "UPDATE build_samples SET outlier_excluded = TRUE WHERE id = ANY($1::bigint[])",
            ids,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Most-recent `limit` samples for one `(pname, system, tenant)` key,
    /// excluding MAD-flagged outliers, returned in **completed_at ASC**
    /// order (oldest first — `compute_vdists` walks newest→oldest by
    /// reverse iteration, and `rows.last()` is "current" by convention).
    ///
    /// Equality + range on the composite — covered by
    /// `build_samples_key_idx` (migration 039: `(pname, system, tenant,
    /// completed_at DESC)`). The index sort matches the inner `ORDER BY
    /// DESC LIMIT`, so this is an index-only top-N; the outer reverse is
    /// in-memory on ≤`limit` rows.
    pub async fn read_build_samples_for_key(
        &self,
        pname: &str,
        system: &str,
        tenant: &str,
        limit: u32,
    ) -> Result<Vec<BuildSampleRow>, sqlx::Error> {
        let mut rows = sqlx::query_as!(
            BuildSampleRow,
            r#"
            SELECT id, pname, system, tenant, duration_secs, peak_memory_bytes,
                   cpu_limit_cores, peak_cpu_cores, cpu_seconds_total,
                   peak_disk_bytes, peak_io_pressure_pct, version, hw_class,
                   node_name, enable_parallel_building, enable_parallel_checking,
                   prefer_local_build, is_fixed_output,
                   EXTRACT(EPOCH FROM completed_at)::float8 AS "completed_at!"
            FROM build_samples
            WHERE pname = $1 AND system = $2 AND tenant = $3
              AND NOT outlier_excluded
            ORDER BY completed_at DESC
            LIMIT $4
            "#,
            pname,
            system,
            tenant,
            limit as i64,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.reverse();
        Ok(rows)
    }

    /// Per-key ring-buffer trim: delete all but the `keep_n` most-recent
    /// rows for one `(pname, system, tenant)` key, **plus** the single
    /// newest row at each distinct `round(cpu_limit_cores)`. Called from
    /// [`SlaEstimator::refresh`] after refit so the table holds at most
    /// `ring_buffer + n_distinct_c` rows per key in steady state.
    /// Returns rows deleted.
    ///
    /// `rn_c > 1` is the anchor invariant — same one [`AnchorRing`]
    /// enforces in-memory. Without it, heavy churn at the converged c*
    /// pushes the lone widest-span explore row past `keep_n` and a
    /// cold-leader refit sees `span=1.0` (the fit collapses to
    /// flat-median). NULL `cpu_limit_cores` partition together: one
    /// pre-telemetry row is anchored, the rest are recency-ranked.
    ///
    /// `build_samples_key_idx` (migration 039: `(pname, system, tenant,
    /// completed_at DESC)`) covers the CTE scan and the recency-partition
    /// sort; the per-c partition is an in-memory sort over ≤`keep_n +
    /// n_distinct_c` rows.
    ///
    /// [`SlaEstimator::refresh`]: crate::sla::SlaEstimator::refresh
    /// [`AnchorRing`]: crate::sla::ingest::AnchorRing
    pub async fn trim_build_samples(
        &self,
        pname: &str,
        system: &str,
        tenant: &str,
        keep_n: u32,
    ) -> Result<u64, sqlx::Error> {
        // `NOT outlier_excluded`: outliers are fit-invisible
        // (`read_build_samples_for_keys` filters them), so they must
        // NOT occupy ring slots — otherwise an outlier burst displaces
        // older good samples and the next refit sees a permanently
        // thinned population. Outliers are reaped by the 30-day age
        // sweep (`delete_samples_older_than`) instead, preserving the
        // forensics intent of `mark_outliers_excluded`.
        let r = sqlx::query!(
            r#"
            WITH ranked AS (
              SELECT id,
                ROW_NUMBER() OVER
                  (ORDER BY completed_at DESC) AS rn,
                ROW_NUMBER() OVER
                  (PARTITION BY round(cpu_limit_cores) ORDER BY completed_at DESC) AS rn_c
              FROM build_samples
              WHERE pname = $1 AND system = $2 AND tenant = $3
                AND NOT outlier_excluded
            )
            DELETE FROM build_samples
            WHERE id IN (SELECT id FROM ranked WHERE rn > $4 AND rn_c > 1)
            "#,
            pname,
            system,
            tenant,
            keep_n as i64,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    /// Batch [`Self::read_build_samples_for_key`]: most-recent `limit`
    /// rows per `(pname, system, tenant)` for every key in the parallel
    /// arrays, in a single round-trip. Returned ASC per key; keys are
    /// interleaved — caller groups in-memory.
    ///
    /// `unnest($1,$2,$3)` zips three `text[]` binds into a row-set so
    /// the composite IN doesn't need a custom record-array type.
    /// `ROW_NUMBER() OVER (PARTITION BY key ORDER BY completed_at
    /// DESC)` gives per-key top-N in one scan; `build_samples_key_idx`
    /// (migration 039) covers both the key match and the partition
    /// sort. Used by [`SlaEstimator::refresh`] so a cold-start refit
    /// of N keys is one query, not N sequential awaits on the actor.
    ///
    /// [`SlaEstimator::refresh`]: crate::sla::SlaEstimator::refresh
    pub async fn read_build_samples_for_keys(
        &self,
        pnames: &[String],
        systems: &[String],
        tenants: &[String],
        limit: u32,
    ) -> Result<Vec<BuildSampleRow>, sqlx::Error> {
        debug_assert_eq!(pnames.len(), systems.len());
        debug_assert_eq!(pnames.len(), tenants.len());
        if pnames.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as!(
            BuildSampleRow,
            r#"
            SELECT id, pname, system, tenant, duration_secs, peak_memory_bytes,
                   cpu_limit_cores, peak_cpu_cores, cpu_seconds_total,
                   peak_disk_bytes, peak_io_pressure_pct, version, hw_class,
                   node_name, enable_parallel_building, enable_parallel_checking,
                   prefer_local_build, is_fixed_output,
                   EXTRACT(EPOCH FROM completed_at)::float8 AS "completed_at!"
            FROM (
              SELECT *, ROW_NUMBER() OVER
                (PARTITION BY pname, system, tenant ORDER BY completed_at DESC) AS rn
              FROM build_samples
              WHERE (pname, system, tenant) IN
                (SELECT * FROM unnest($1::text[], $2::text[], $3::text[]))
                AND NOT outlier_excluded
            ) t
            WHERE rn <= $4
            ORDER BY pname, system, tenant, completed_at ASC
            "#,
            pnames,
            systems,
            tenants,
            limit as i64,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Batch [`Self::trim_build_samples`]: delete all but the `keep_n`
    /// most-recent NON-OUTLIER rows — plus one anchor per distinct
    /// `round(cpu_limit_cores)` — for every `(pname, system, tenant)`
    /// in the parallel arrays, in a single round-trip. Returns total
    /// rows deleted across all keys. Same dual-window shape as the
    /// single-key variant; same `NOT outlier_excluded` filter so
    /// outliers don't occupy ring slots (they're age-swept by
    /// `delete_samples_older_than` instead).
    pub async fn trim_build_samples_batch(
        &self,
        pnames: &[String],
        systems: &[String],
        tenants: &[String],
        keep_n: u32,
    ) -> Result<u64, sqlx::Error> {
        debug_assert_eq!(pnames.len(), systems.len());
        debug_assert_eq!(pnames.len(), tenants.len());
        if pnames.is_empty() {
            return Ok(0);
        }
        let r = sqlx::query!(
            r#"
            WITH ranked AS (
              SELECT id,
                ROW_NUMBER() OVER
                  (PARTITION BY pname, system, tenant
                   ORDER BY completed_at DESC) AS rn,
                ROW_NUMBER() OVER
                  (PARTITION BY pname, system, tenant, round(cpu_limit_cores)
                   ORDER BY completed_at DESC) AS rn_c
              FROM build_samples
              WHERE (pname, system, tenant) IN
                (SELECT * FROM unnest($1::text[], $2::text[], $3::text[]))
                AND NOT outlier_excluded
            )
            DELETE FROM build_samples
            WHERE id IN (SELECT id FROM ranked WHERE rn > $4 AND rn_c > 1)
            "#,
            pnames,
            systems,
            tenants,
            keep_n as i64,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }

    // ─── sla_overrides CRUD (ADR-023 phase-6) ──────────────────────────

    /// All non-expired overrides, optionally filtered by exact `pname`.
    /// Feeds both [`SlaEstimator::refresh`] (full read, `pname=None`) and
    /// `AdminService.ListSlaOverrides`. Ordered `created_at DESC` so the
    /// CLI shows newest first; the in-memory resolver re-sorts by
    /// specificity anyway.
    ///
    /// Expired rows are filtered server-side (`expires_at IS NULL OR >
    /// now()`) — keeps the resolver pure (no clock read) and means a
    /// stale tick cache never resurrects a dead pin. They are NOT
    /// deleted: phase-7's audit view wants to show "this was forced
    /// 2026-04-01..04-08".
    ///
    /// `lookup_idx` (migration 040: `(pname, system, tenant)`) covers
    /// the `pname = $1` filter; the unfiltered tick read is a full scan
    /// — fine, the table is operator-written (tens of rows, not
    /// thousands).
    ///
    /// `cluster` filters out rows scoped to a different cluster:
    /// `cluster IS NULL OR cluster = $1`. Under the global-DB topology
    /// (ADR-023 §2.13) sibling tables `sla_ema_state`/`interrupt_samples`
    /// already scope `WHERE cluster = $1`; without this filter a row
    /// written with `cluster:"prod-east"` would match in every region.
    ///
    /// [`SlaEstimator::refresh`]: crate::sla::SlaEstimator::refresh
    pub async fn read_sla_overrides(
        &self,
        cluster: &str,
        pname: Option<&str>,
    ) -> Result<Vec<SlaOverrideRow>, sqlx::Error> {
        sqlx::query_as!(
            SlaOverrideRow,
            r#"
            SELECT id, pname, system, tenant, cluster, tier,
                   p50_secs, p90_secs, p99_secs, cores, mem_bytes, capacity_type,
                   EXTRACT(EPOCH FROM expires_at)::float8 AS "expires_at",
                   EXTRACT(EPOCH FROM created_at)::float8 AS "created_at!",
                   created_by
            FROM sla_overrides
            WHERE (expires_at IS NULL OR expires_at > now())
              AND (cluster IS NULL OR cluster = $1)
              AND ($2::text IS NULL OR pname = $2)
            ORDER BY created_at DESC
            "#,
            cluster,
            pname,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Insert one override. Returns the row with server-assigned `id` /
    /// `created_at`. `expires_at` round-trips as epoch f64 →
    /// `to_timestamp($n)`.
    pub async fn insert_sla_override(
        &self,
        row: &SlaOverrideRow,
    ) -> Result<SlaOverrideRow, sqlx::Error> {
        sqlx::query_as!(
            SlaOverrideRow,
            r#"
            INSERT INTO sla_overrides
              (pname, system, tenant, cluster, tier,
               p50_secs, p90_secs, p99_secs, cores, mem_bytes, capacity_type,
               expires_at, created_by)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,to_timestamp($12),$13)
            RETURNING id, pname, system, tenant, cluster, tier,
                      p50_secs, p90_secs, p99_secs, cores, mem_bytes, capacity_type,
                      EXTRACT(EPOCH FROM expires_at)::float8 AS "expires_at",
                      EXTRACT(EPOCH FROM created_at)::float8 AS "created_at!",
                      created_by
            "#,
            row.pname,
            row.system,
            row.tenant,
            row.cluster,
            row.tier,
            row.p50_secs,
            row.p90_secs,
            row.p99_secs,
            row.cores,
            row.mem_bytes,
            row.capacity_type,
            row.expires_at,
            row.created_by,
        )
        .fetch_one(&self.pool)
        .await
    }

    /// Delete one override by id. Idempotent — returns rows affected
    /// (0 if already gone).
    pub async fn delete_sla_override(&self, id: i64) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!("DELETE FROM sla_overrides WHERE id = $1", id)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// `ResetSlaModel`: drop every `build_samples` row for one key. The
    /// caller pairs this with [`SlaEstimator::evict`] so the next
    /// dispatch falls back to the cold-start probe path.
    ///
    /// [`SlaEstimator::evict`]: crate::sla::SlaEstimator::evict
    pub async fn delete_build_samples_for_key(
        &self,
        pname: &str,
        system: &str,
        tenant: &str,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            "DELETE FROM build_samples WHERE pname = $1 AND system = $2 AND tenant = $3",
            pname,
            system,
            tenant,
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected())
    }
}

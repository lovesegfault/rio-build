//! Build history EMA + build_samples retention — `build_history` /
//! `build_samples` tables.

use super::{BuildHistoryRow, EMA_ALPHA, SchedulerDb, TERMINAL_STATUS_SQL};

/// One `build_samples` row. ADR-023 SLA telemetry: every successful
/// completion appends a full-width row; the SLA fit reads these to
/// compute per-`(pname, system, tenant)` cores-vs-duration curves.
///
/// All telemetry columns are `Option` — old executors / recovered
/// derivations / non-k8s test runs leave them `None` → SQL NULL.
/// `hw_class` is always `None` from the scheduler; the controller
/// backfills it from the Node informer (joins on `node_name`).
///
/// `Default` so tests can `BuildSampleRow { pname: "x".into(),
/// ..Default::default() }` without spelling out 15 Nones.
#[derive(Debug, Clone, Default)]
pub struct BuildSampleRow {
    /// `BIGSERIAL` PK. Ignored on write (server-assigned); populated on
    /// read so the MAD outlier sweep can `mark_outlier_excluded(id)`
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
    pub prefer_local_build: Option<bool>,
    /// Unix epoch seconds. Read via `EXTRACT(EPOCH FROM completed_at)` —
    /// the workspace sqlx has no chrono/time feature, so TIMESTAMPTZ
    /// round-trips as f64 epoch (matches `tenants.rs` pattern). On the
    /// write path this field is ignored; `write_build_sample` always
    /// uses server-side `now()`.
    pub completed_at: f64,
}

/// One `sla_overrides` row. ADR-023 phase-6 operator pins. NULL
/// `system`/`tenant` are wildcards — [`crate::sla::r#override::resolve`]
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
    /// Read the full build_history table for estimator refresh.
    ///
    /// Return tuple: `(pname, system, ema_duration_secs, ema_peak_memory_bytes, ema_peak_cpu_cores, sample_count)`.
    /// Aliased as [`BuildHistoryRow`] — 5-element tuples trip clippy's
    /// type-complexity lint, and naming it documents the field order
    /// at the one place where it's easy to mix up (3 f64-ish fields).
    ///
    /// 5-tuple (cpu included). Estimator::refresh signature matches.
    /// The estimator doesn't USE cpu_cores yet (size-class routing
    /// bumps on memory only, not cpu) but reading it now means future
    /// cpu-bump logic is a pure-estimator change, no DB roundtrip.
    ///
    /// The estimator loads this into a HashMap at startup and refreshes
    /// on Tick (every 60s). A full table scan is fine: even 10k distinct
    /// (pname, system) pairs is ~1 MB and <10ms. The alternative —
    /// loading on-demand per estimate — would be an async PG roundtrip
    /// inside the single-threaded actor loop on every dispatch decision.
    /// Batch-load once, query in-memory.
    pub async fn read_build_history(&self) -> Result<Vec<BuildHistoryRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT pname, system, ema_duration_secs, ema_peak_memory_bytes, ema_peak_cpu_cores, sample_count
            FROM build_history
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Update the build history EMA for a (pname, system) pair.
    ///
    /// `peak_memory_bytes`/`peak_cpu_cores`: None means the worker had
    /// no signal (build failed before cgroup populated, or exited in
    /// <1s before CPU poll sampled). Must NOT drag the EMA toward zero
    /// — None → column unchanged.
    ///
    /// The COALESCE(blend, new, old) pattern handles all four
    /// old×new nullability combinations:
    ///   old=Some, new=Some → blend (normal EMA)
    ///   old=None, new=Some → blend is NULL (NULL*x), falls to new (first sample)
    ///   old=Some, new=None → blend is NULL (x+NULL), falls to new=NULL, falls to old (keep)
    ///   old=None, new=None → all NULL (still no signal)
    ///
    /// PG: any arithmetic with NULL yields NULL; COALESCE picks first non-NULL.
    ///
    /// # Historical memory data
    ///
    /// Older worker versions sent VmHWM of nix-daemon (~10MB
    /// regardless of builder memory). cgroup memory.peak fixes that.
    /// If existing build_history rows have wrong memory values, EMA
    /// alpha=0.3 means ~10 completions per (pname,system) washes the
    /// bad data out (0.7^10 ≈ 2.8% of the old value remains). No
    /// migration needed — time heals it.
    pub async fn update_build_history(
        &self,
        pname: &str,
        system: &str,
        actual_duration_secs: f64,
        peak_memory_bytes: Option<u64>,
        peak_cpu_cores: Option<f64>,
    ) -> Result<(), sqlx::Error> {
        // u64 → f64 for DOUBLE PRECISION binding. Precision loss at
        // ~2^53 bytes (~9 PB) is not a concern. cpu is already f64.
        let peak_mem = peak_memory_bytes.map(|b| b as f64);

        sqlx::query(
            r#"
            INSERT INTO build_history
              (pname, system, ema_duration_secs,
               ema_peak_memory_bytes, ema_peak_cpu_cores,
               sample_count, last_updated)
            VALUES ($1, $2, $3, $5, $6, 1, now())
            ON CONFLICT (pname, system) DO UPDATE SET
                ema_duration_secs = build_history.ema_duration_secs * (1.0 - $4) + $3 * $4,
                ema_peak_memory_bytes = COALESCE(
                    build_history.ema_peak_memory_bytes * (1.0 - $4) + $5 * $4,
                    $5,
                    build_history.ema_peak_memory_bytes),
                ema_peak_cpu_cores = COALESCE(
                    build_history.ema_peak_cpu_cores * (1.0 - $4) + $6 * $4,
                    $6,
                    build_history.ema_peak_cpu_cores),
                sample_count = build_history.sample_count + 1,
                last_updated = now()
            "#,
        )
        .bind(pname)
        .bind(system)
        .bind(actual_duration_secs)
        .bind(EMA_ALPHA)
        .bind(peak_mem)
        .bind(peak_cpu_cores)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Penalty-write for misclassified builds: a build that took >2×
    /// its class cutoff was routed wrong. Overwrite the EMA with the
    /// actual duration (NOT blend — a blend would take multiple
    /// overruns to correct) so the NEXT classify() picks a larger
    /// class.
    ///
    /// This is intentionally harsh: one bad classification is enough
    /// to fix the estimate. If the build was a fluke (transient slow
    /// disk), the next normal completion blends it back down. Better
    /// to over-correct once than to keep OOMing small workers.
    pub async fn update_build_history_misclassified(
        &self,
        pname: &str,
        system: &str,
        actual_duration_secs: f64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE build_history
            SET ema_duration_secs = $3,
                last_updated = now()
            WHERE pname = $1 AND system = $2
            "#,
            pname,
            system,
            actual_duration_secs,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Proactive mid-build memory EMA update: a running build's cgroup
    /// `memory.peak` already exceeds what the EMA predicted. Overwrite
    /// the EMA NOW so the NEXT submit of this (pname, system) is
    /// right-sized BEFORE the current build finishes (or OOMs).
    ///
    /// Same penalty-overwrite semantics as
    /// [`Self::update_build_history_misclassified`] (not a blend — a blend
    /// would need multiple mid-build samples to converge, defeating the
    /// point of "proactive"). Self-correcting: if the peak was a spike,
    /// the completion's normal EMA blend pulls it back down.
    ///
    /// Single-statement: joins `derivations` by `drv_path` (what
    /// `ProgressUpdate` carries — workers don't know their drv_hash),
    /// then updates `build_history` by the resolved `(pname, system)`.
    /// The `WHERE` clause's NULL/less-than guard makes the whole thing
    /// atomic — no read-compare-write race with concurrent completions.
    ///
    /// Filters to non-terminal status so the partial index applies
    /// (see `TERMINAL_STATUS_SQL`). A progress update for a terminal
    /// derivation is a late-arriving sample post-completion; harmless
    /// to drop.
    ///
    /// Returns `true` if a row was updated (observed > current EMA, or
    /// EMA was NULL). `false` covers: derivation not found / terminal /
    /// pname NULL / no build_history row / observed ≤ current EMA. All
    /// correct no-ops — the caller increments a counter on `true` only.
    ///
    /// `drv_path` is NOT indexed. This fires every 10s per running
    /// build — ~10 qps at 100 concurrent builds, scanning the
    /// non-terminal subset (small, via partial index). If this becomes
    /// hot, add an index on `(drv_path) WHERE status NOT IN (...)`.
    // r[impl sched.classify.proactive-ema]
    pub async fn update_ema_peak_memory_proactive(
        &self,
        drv_path: &str,
        observed_peak_bytes: u64,
    ) -> Result<bool, sqlx::Error> {
        // u64 → f64 for DOUBLE PRECISION. Precision loss at 2^53 bytes
        // (~9 PB) — not a concern for memory.peak.
        let observed = observed_peak_bytes as f64;

        let result = sqlx::query(&format!(
            r#"
            UPDATE build_history bh
            SET ema_peak_memory_bytes = $2,
                last_updated = now()
            FROM derivations d
            WHERE d.drv_path = $1
              AND d.status NOT IN {TERMINAL_STATUS_SQL}
              AND d.pname IS NOT NULL
              AND bh.pname = d.pname
              AND bh.system = d.system
              AND (bh.ema_peak_memory_bytes IS NULL
                   OR bh.ema_peak_memory_bytes < $2)
            "#
        ))
        .bind(drv_path)
        .bind(observed)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Insert one raw build sample with full ADR-023 telemetry. Called
    /// from completion.rs success path alongside the EMA update.
    /// Best-effort: caller warns on Err.
    ///
    /// Unlike `update_build_history` which folds into a single EMA row per
    /// `(pname, system)`, this appends every completion — the rebalancer
    /// and the SLA fit both need the full distribution, not a smoothed
    /// scalar. `completed_at` is server-side `now()`; `outlier_excluded`
    /// keeps its DEFAULT FALSE (the MAD sweep flips it later).
    pub async fn write_build_sample(&self, row: &BuildSampleRow) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "INSERT INTO build_samples
               (pname, system, tenant, duration_secs, peak_memory_bytes,
                cpu_limit_cores, peak_cpu_cores, cpu_seconds_total,
                peak_disk_bytes, peak_io_pressure_pct, version, hw_class,
                node_name, enable_parallel_building, prefer_local_build,
                completed_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,now())",
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
            row.prefer_local_build,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Minimal-row convenience for tests / admin probes that don't have
    /// full telemetry to hand. Thin wrapper over [`Self::write_build_sample`].
    pub async fn insert_build_sample(
        &self,
        pname: &str,
        system: &str,
        duration_secs: f64,
        peak_memory_bytes: i64,
    ) -> Result<(), sqlx::Error> {
        self.write_build_sample(&BuildSampleRow {
            pname: pname.into(),
            system: system.into(),
            duration_secs,
            peak_memory_bytes,
            ..Default::default()
        })
        .await
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

    /// Query raw `(duration_secs, peak_memory_bytes)` samples from the
    /// last `days`. Feeds `rebalancer::compute_cutoffs`.
    ///
    /// Range on `completed_at` — covered by `build_samples_completed_at_idx`
    /// (migration 013), same index that serves `delete_samples_older_than`.
    ///
    /// No `ORDER BY` — `compute_cutoffs` sorts internally. Returning
    /// `Vec<(f64,i64)>` directly (no intermediate row struct) since the
    /// rebalancer's signature takes exactly this tuple shape.
    pub async fn query_build_samples_last_days(
        &self,
        days: u32,
    ) -> Result<Vec<(f64, i64)>, sqlx::Error> {
        // `$1 * interval '1 day'` — same bind pattern as
        // `delete_samples_older_than`. Keeps PG's interval arithmetic,
        // avoids the text-cast detour.
        sqlx::query_as::<_, (f64, i64)>(
            "SELECT duration_secs, peak_memory_bytes
             FROM build_samples
             WHERE completed_at > now() - $1 * interval '1 day'",
        )
        .bind(days as i32)
        .fetch_all(&self.pool)
        .await
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
                   node_name, enable_parallel_building, prefer_local_build,
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

    /// Flip `outlier_excluded = TRUE` for one row by PK. Idempotent.
    /// The row stays for forensics; both per-key and incremental reads
    /// already filter `WHERE NOT outlier_excluded`, so a flagged sample
    /// drops out of the next refit without a DELETE.
    pub async fn mark_outlier_excluded(&self, id: i64) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE build_samples SET outlier_excluded = TRUE WHERE id = $1",
            id,
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
                   node_name, enable_parallel_building, prefer_local_build,
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
    /// rows for one `(pname, system, tenant)` key. Called from
    /// [`SlaEstimator::refresh`] after refit so the table holds at most
    /// `ring_buffer` rows per key in steady state. Returns rows deleted.
    ///
    /// `NOT IN (SELECT id … ORDER BY completed_at DESC LIMIT $4)` — both
    /// the outer scan and the subselect's top-N are covered by
    /// `build_samples_key_idx` (migration 039: `(pname, system, tenant,
    /// completed_at DESC)`). With ≤32 retained rows the subselect is an
    /// index-only top-N; the outer DELETE walks the same range.
    ///
    /// [`SlaEstimator::refresh`]: crate::sla::SlaEstimator::refresh
    pub async fn trim_build_samples(
        &self,
        pname: &str,
        system: &str,
        tenant: &str,
        keep_n: u32,
    ) -> Result<u64, sqlx::Error> {
        let r = sqlx::query!(
            "DELETE FROM build_samples
             WHERE pname = $1 AND system = $2 AND tenant = $3
               AND id NOT IN (
                 SELECT id FROM build_samples
                 WHERE pname = $1 AND system = $2 AND tenant = $3
                 ORDER BY completed_at DESC LIMIT $4)",
            pname,
            system,
            tenant,
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
    /// [`SlaEstimator::refresh`]: crate::sla::SlaEstimator::refresh
    pub async fn read_sla_overrides(
        &self,
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
              AND ($1::text IS NULL OR pname = $1)
            ORDER BY created_at DESC
            "#,
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

//! build_samples retention + sla_overrides CRUD tests.

use rio_test_support::TestDb;

use crate::db::{BuildSampleRow, SchedulerDb};
use crate::sla::config::SlaConfig;

/// bug_051: `validate_reload` was dead — `[sla]` has no SIGHUP path
/// and no `prev` config across restarts. The replacement persists
/// `reference_hw_class` in PG (M_058) and checks at startup.
///
/// Boot 1 records the value; boot 2 with the same value is a no-op;
/// boot 2 with a different value errors without the flag and resets
/// ref-second state with it.
// r[verify sched.sla.hw-class.config]
#[tokio::test]
async fn check_reference_epoch_guards_across_restarts() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let mut cfg = SlaConfig::test_default();
    cfg.cluster = "us-east-2".into();
    cfg.reference_hw_class = Some("intel-8-nvme".into());

    // Boot 1: no row → record at epoch 0.
    crate::sla::check_reference_epoch(&db, &cfg, false).await?;
    let (got, epoch): (Option<String>, i64) =
        sqlx::query_as("SELECT reference_hw_class, epoch FROM sla_config_epoch WHERE cluster = $1")
            .bind("us-east-2")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(got.as_deref(), Some("intel-8-nvme"));
    assert_eq!(epoch, 0);

    // Boot 2, same ref: idempotent no-op.
    crate::sla::check_reference_epoch(&db, &cfg, false).await?;

    // Seed ref-second state we expect to be reset.
    db.write_build_sample(&BuildSampleRow {
        pname: "x".into(),
        system: "x86_64-linux".into(),
        ..Default::default()
    })
    .await?;
    sqlx::query("INSERT INTO sla_ema_state (cluster, key, value) VALUES ('us-east-2', 'k', 1.0)")
        .execute(&test_db.pool)
        .await?;
    sqlx::query("INSERT INTO sla_ema_state (cluster, key, value) VALUES ('eu-west-1', 'k', 1.0)")
        .execute(&test_db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO hw_perf_samples (hw_class, pod_id, factor) \
         VALUES ('intel-8-nvme', 'p', '{}')",
    )
    .execute(&test_db.pool)
    .await?;

    // Boot 3, changed ref, no flag: error.
    cfg.reference_hw_class = Some("amd-8-nvme".into());
    let err = crate::sla::check_reference_epoch(&db, &cfg, false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("--allow-reference-change"), "{err}");
    // State untouched on the reject path.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM build_samples")
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(n, 1, "reject path must not reset");

    // Boot 3b, changed ref, WITH flag: reset + epoch bump.
    crate::sla::check_reference_epoch(&db, &cfg, true).await?;
    let (got, epoch): (Option<String>, i64) =
        sqlx::query_as("SELECT reference_hw_class, epoch FROM sla_config_epoch WHERE cluster = $1")
            .bind("us-east-2")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(got.as_deref(), Some("amd-8-nvme"));
    assert_eq!(epoch, 1);
    for t in ["build_samples", "hw_perf_samples"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {t}"))
            .fetch_one(&test_db.pool)
            .await?;
        assert_eq!(n, 0, "{t} reset");
    }
    // sla_ema_state cluster-scoped: us-east-2 cleared, eu-west-1 kept.
    let kept: Vec<String> = sqlx::query_scalar("SELECT cluster FROM sla_ema_state")
        .fetch_all(&test_db.pool)
        .await?;
    assert_eq!(kept, vec!["eu-west-1".to_string()]);

    // Boot 4, same (new) ref: idempotent again.
    crate::sla::check_reference_epoch(&db, &cfg, false).await?;

    Ok(())
}

/// build_samples insert + retention roundtrip. Also proves migration
/// 013 applies cleanly (every TestDb::new runs the full migrate!).
#[tokio::test]
async fn test_build_samples_insert_and_retention() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Two fresh samples.
    db.write_build_sample(&BuildSampleRow {
        pname: "hello".into(),
        system: "x86_64-linux".into(),
        duration_secs: 12.5,
        peak_memory_bytes: 4_194_304,
        ..Default::default()
    })
    .await?;
    db.write_build_sample(&BuildSampleRow {
        pname: "hello".into(),
        system: "x86_64-linux".into(),
        duration_secs: 8.0,
        peak_memory_bytes: 2_097_152,
        ..Default::default()
    })
    .await?;

    // Schema check: migration 013 created 6 columns (id + 5 data cols);
    // later migrations (039 telemetry) add more. Assert the 013 base
    // shape is present — exact count is brittle to additive migrations.
    let (col_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.columns
         WHERE table_name = 'build_samples'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert!(
        col_count >= 6,
        "expected >=6 columns (013 base: id + 5 data cols), got {col_count}"
    );

    let (idx_exists,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM pg_indexes
         WHERE tablename = 'build_samples'
           AND indexname = 'build_samples_completed_at_idx')",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert!(idx_exists, "completed_at index missing");

    // Retention with 30d cutoff: both rows are fresh (default now()),
    // so nothing deleted.
    let deleted = db.delete_samples_older_than(30).await?;
    assert_eq!(deleted, 0);

    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM build_samples")
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(count, 2);

    // Force-age one row past the cutoff, re-run retention.
    sqlx::query(
        "UPDATE build_samples SET completed_at = now() - interval '45 days'
         WHERE id = (SELECT MIN(id) FROM build_samples)",
    )
    .execute(&test_db.pool)
    .await?;

    let deleted = db.delete_samples_older_than(30).await?;
    assert_eq!(deleted, 1, "expected 1 aged row deleted");

    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM build_samples")
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(count, 1, "expected 1 fresh row remaining");

    Ok(())
}

/// ADR-023: full-width `build_samples` row roundtrips every telemetry
/// column. Proves migration 039 columns are wired through
/// `write_build_sample` (a missed bind would compile via `query!` but
/// land as the column DEFAULT, so assert non-default values).
#[tokio::test]
async fn test_write_build_sample_full_telemetry() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let row = BuildSampleRow {
        pname: "hello".into(),
        system: "x86_64-linux".into(),
        tenant: "t1".into(),
        duration_secs: 42.0,
        peak_memory_bytes: 256 << 20,
        cpu_limit_cores: Some(8.0),
        peak_cpu_cores: Some(6.5),
        cpu_seconds_total: Some(273.0),
        peak_disk_bytes: Some(1 << 30),
        peak_io_pressure_pct: Some(12.5),
        version: Some("2.12.1".into()),
        hw_class: None,
        node_name: Some("ip-10-0-1-42.ec2.internal".into()),
        enable_parallel_building: Some(true),
        enable_parallel_checking: Some(false),
        prefer_local_build: Some(false),
        is_fixed_output: Some(false),
        completed_at: 0.0, // ignored on write — server-side now()
        id: 0,
    };
    db.write_build_sample(&row).await?;

    // tenant + cpu_limit_cores: spot-check the two fields most likely
    // to be miswired (NOT NULL DEFAULT '' vs nullable Option).
    let (cpu_limit, tenant): (Option<f64>, String) =
        sqlx::query_as("SELECT cpu_limit_cores, tenant FROM build_samples WHERE pname = 'hello'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(cpu_limit, Some(8.0));
    assert_eq!(tenant, "t1");

    // Full-row roundtrip for the remaining 039 columns. Single
    // row in the table → fetch_one is unambiguous.
    #[allow(clippy::type_complexity)]
    let got: (
        Option<f64>,
        Option<f64>,
        Option<i64>,
        Option<f64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<bool>,
        Option<bool>,
        bool,
    ) = sqlx::query_as(
        "SELECT peak_cpu_cores, cpu_seconds_total, peak_disk_bytes,
                peak_io_pressure_pct, version, hw_class, node_name,
                enable_parallel_building, prefer_local_build, outlier_excluded
         FROM build_samples WHERE pname = 'hello'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(got.0, Some(6.5), "peak_cpu_cores");
    assert_eq!(got.1, Some(273.0), "cpu_seconds_total");
    assert_eq!(got.2, Some(1 << 30), "peak_disk_bytes");
    assert_eq!(got.3, Some(12.5), "peak_io_pressure_pct");
    assert_eq!(got.4.as_deref(), Some("2.12.1"), "version");
    assert_eq!(got.5, None, "hw_class NULL (row didn't set it)");
    assert_eq!(
        got.6.as_deref(),
        Some("ip-10-0-1-42.ec2.internal"),
        "node_name"
    );
    assert_eq!(got.7, Some(true), "enable_parallel_building");
    assert_eq!(got.8, Some(false), "prefer_local_build");
    assert!(!got.9, "outlier_excluded keeps DEFAULT FALSE");

    Ok(())
}

/// `SlaEstimator::refresh`: write→incremental-read→per-key-read→refit
/// end to end against a real PG. Seed 5 samples on a 4..64 core ladder
/// with a clean Amdahl curve; first refresh sees them as one touched
/// key and produces an Amdahl fit (n_eff≈5, span=16). Then prove the
/// `last_tick` high-water-mark sticks: a second refresh with no new
/// writes touches zero keys.
#[tokio::test]
async fn test_sla_estimator_incremental_refresh() -> anyhow::Result<()> {
    use crate::sla::{SlaEstimator, types::DurationFit, types::ModelKey};

    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    for c in [4.0, 8.0, 16.0, 32.0, 64.0] {
        db.write_build_sample(&BuildSampleRow {
            pname: "a".into(),
            system: "x86_64-linux".into(),
            tenant: "t".into(),
            duration_secs: 30.0 + 2000.0 / c,
            peak_memory_bytes: 256 << 20,
            cpu_limit_cores: Some(c),
            ..Default::default()
        })
        .await?;
    }
    // One outlier-flagged row for the same key — must be excluded by
    // both reads (would push distinct_c to 6 and skew span if not).
    db.write_build_sample(&BuildSampleRow {
        pname: "a".into(),
        system: "x86_64-linux".into(),
        tenant: "t".into(),
        duration_secs: 1.0,
        cpu_limit_cores: Some(128.0),
        ..Default::default()
    })
    .await?;
    sqlx::query("UPDATE build_samples SET outlier_excluded = TRUE WHERE cpu_limit_cores = 128")
        .execute(&test_db.pool)
        .await?;

    let est = SlaEstimator::new(&crate::sla::config::SlaConfig::test_default());
    let n = est.refresh(&db, &[]).await?;
    assert_eq!(n, 1, "one (pname,system,tenant) key touched");

    let key = ModelKey {
        pname: "a".into(),
        system: "x86_64-linux".into(),
        tenant: "t".into(),
    };
    let f = est.cached(&key).expect("cached after refresh");
    assert!(f.n_eff > 4.9, "n_eff={}", f.n_eff);
    assert!(f.span >= 16.0, "span={} (64/4)", f.span);
    assert!(
        matches!(f.fit, DurationFit::Amdahl { .. }),
        "fit={:?}",
        f.fit
    );
    assert_eq!(f.explore.distinct_c, 5, "outlier row excluded");

    // Incremental: no new writes → high-water-mark holds → zero refits.
    let n2 = est.refresh(&db, &[]).await?;
    assert_eq!(n2, 0, "second refresh with no new rows is a no-op");

    // read_build_samples_for_key returns ASC (oldest first → last is
    // newest). Spot-check the order contract refit() relies on.
    let rows = db
        .read_build_samples_for_key("a", "x86_64-linux", "t", 32)
        .await?;
    assert_eq!(rows.len(), 5);
    assert!(
        rows.first().unwrap().completed_at <= rows.last().unwrap().completed_at,
        "for_key must return completed_at ASC"
    );

    Ok(())
}

/// `trim_build_samples`: insert 40 rows with strictly-ascending
/// `completed_at`, trim to 32, assert the 32 newest survive (the
/// surviving min `completed_at` is the 9th-oldest). A different-key
/// row in the same table is untouched.
#[tokio::test]
async fn test_trim_build_samples_keeps_newest_n() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Insert with explicit completed_at (write_build_sample uses now()
    // server-side, so go raw). i seconds past epoch → strictly ascending.
    for i in 0..40 {
        sqlx::query(
            "INSERT INTO build_samples
               (pname, system, tenant, duration_secs, peak_memory_bytes, completed_at)
             VALUES ('p', 'x86_64-linux', 't', 1.0, 0, to_timestamp($1))",
        )
        .bind(i as f64)
        .execute(&test_db.pool)
        .await?;
    }
    // One row for a different key — must NOT be trimmed.
    sqlx::query(
        "INSERT INTO build_samples
           (pname, system, tenant, duration_secs, peak_memory_bytes, completed_at)
         VALUES ('other', 'x86_64-linux', 't', 1.0, 0, to_timestamp(0))",
    )
    .execute(&test_db.pool)
    .await?;

    let deleted = db.trim_build_samples("p", "x86_64-linux", "t", 32).await?;
    assert_eq!(deleted, 8, "40 inserted → 32 kept → 8 deleted");

    let (n, min_epoch): (i64, f64) = sqlx::query_as(
        "SELECT count(*), min(EXTRACT(EPOCH FROM completed_at))::float8
         FROM build_samples WHERE pname = 'p' AND system = 'x86_64-linux' AND tenant = 't'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(n, 32);
    assert_eq!(
        min_epoch, 8.0,
        "kept the 32 newest by completed_at (i=8..39)"
    );

    let (other,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM build_samples WHERE pname = 'other'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(other, 1, "different-key row untouched");

    // Idempotent: second trim is a no-op.
    let deleted2 = db.trim_build_samples("p", "x86_64-linux", "t", 32).await?;
    assert_eq!(deleted2, 0);

    Ok(())
}

/// `read_build_samples_for_key` filters on `tenant` — same `(pname,
/// system)` under two tenants are distinct ModelKeys (ADR-023: a
/// tenant's curve must never be polluted by another tenant's samples).
/// Two rows, identical pname/system, different tenant → reading for
/// tenant `a` returns exactly tenant `a`'s row.
// r[verify sched.sla.model-key-tenant-scoped]
#[tokio::test]
async fn test_read_build_samples_tenant_scoped() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    db.write_build_sample(&BuildSampleRow {
        pname: "shared".into(),
        system: "x86_64-linux".into(),
        tenant: "tenant-a".into(),
        duration_secs: 100.0,
        peak_memory_bytes: 1 << 30,
        ..Default::default()
    })
    .await?;
    db.write_build_sample(&BuildSampleRow {
        pname: "shared".into(),
        system: "x86_64-linux".into(),
        tenant: "tenant-b".into(),
        duration_secs: 999.0,
        peak_memory_bytes: 1 << 30,
        ..Default::default()
    })
    .await?;

    let rows_a = db
        .read_build_samples_for_key("shared", "x86_64-linux", "tenant-a", 32)
        .await?;
    assert_eq!(rows_a.len(), 1, "tenant-a should see only its own row");
    assert_eq!(rows_a[0].tenant, "tenant-a");
    assert!((rows_a[0].duration_secs - 100.0).abs() < 1e-9);

    let rows_b = db
        .read_build_samples_for_key("shared", "x86_64-linux", "tenant-b", 32)
        .await?;
    assert_eq!(rows_b.len(), 1, "tenant-b should see only its own row");
    assert_eq!(rows_b[0].tenant, "tenant-b");
    assert!((rows_b[0].duration_secs - 999.0).abs() < 1e-9);

    Ok(())
}

/// Batch read + batch trim: 3 keys × 5 rows each, request limit=3. One
/// query returns 9 rows (3 per key, ASC, newest 3). Batch-trim to 3
/// deletes 6 (2 per key); a fourth key not in the batch is untouched.
/// Covers the single-round-trip refresh path that replaces the per-key
/// sequential awaits.
#[tokio::test]
async fn test_build_samples_batch_read_and_trim() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    for key in ["a", "b", "c", "d"] {
        for i in 0..5 {
            sqlx::query(
                "INSERT INTO build_samples
                   (pname, system, tenant, duration_secs, peak_memory_bytes,
                    cpu_limit_cores, completed_at)
                 VALUES ($1, 'x86_64-linux', 't', $2, 0, 4, to_timestamp($3))",
            )
            .bind(key)
            .bind(i as f64)
            .bind(i as f64)
            .execute(&test_db.pool)
            .await?;
        }
    }
    // Mark one row outlier_excluded — batch read filters it.
    sqlx::query(
        "UPDATE build_samples SET outlier_excluded = TRUE
         WHERE pname = 'a' AND duration_secs = 4",
    )
    .execute(&test_db.pool)
    .await?;

    let pnames: Vec<String> = ["a", "b", "c"].into_iter().map(String::from).collect();
    let systems = vec!["x86_64-linux".to_string(); 3];
    let tenants = vec!["t".to_string(); 3];

    let rows = db
        .read_build_samples_for_keys(&pnames, &systems, &tenants, 3)
        .await?;
    // 3 keys × 3 rows = 9, minus 1 outlier-excluded for 'a' (its newest
    // 3 are i=4,3,2 but i=4 is excluded → returns i=3,2,1). Per-key
    // limit applies BEFORE the outlier filter? No — `WHERE NOT
    // outlier_excluded` is inside the subquery before ROW_NUMBER, so
    // 'a' returns 3 of its 4 remaining (i=3,2,1).
    assert_eq!(rows.len(), 9, "3 keys × limit 3 each, single round-trip");
    // Per-key ASC ordering + key 'd' not requested.
    for r in &rows {
        assert!(["a", "b", "c"].contains(&r.pname.as_str()));
    }
    let a_rows: Vec<_> = rows.iter().filter(|r| r.pname == "a").collect();
    assert_eq!(a_rows.len(), 3);
    assert!(a_rows[0].completed_at <= a_rows[2].completed_at, "ASC");
    assert!(
        !a_rows.iter().any(|r| (r.duration_secs - 4.0).abs() < 1e-9),
        "outlier_excluded row filtered"
    );

    // Batch trim to 3 → 'a' has 4 non-outlier rows so 1 deleted;
    // 'b'/'c' have 5 → 2 each. 'd' untouched. Outliers don't occupy
    // ring slots (rank only non-outlier rows).
    let deleted = db
        .trim_build_samples_batch(&pnames, &systems, &tenants, 3)
        .await?;
    assert_eq!(deleted, 5, "a:1 + b:2 + c:2 = 5 rows deleted");
    // 'a's outlier-excluded row is untouched by trim (forensics-kept;
    // age-swept by delete_samples_older_than).
    let (a_outlier_survives,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM build_samples WHERE pname = 'a' AND outlier_excluded")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(a_outlier_survives, 1, "outlier row untouched by trim");
    let (d_count,): (i64,) = sqlx::query_as("SELECT count(*) FROM build_samples WHERE pname = 'd'")
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(d_count, 5, "key not in batch untouched");

    // Batch outlier-mark roundtrip.
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM build_samples WHERE pname = 'b'")
        .fetch_all(&test_db.pool)
        .await?;
    db.mark_outliers_excluded(&ids).await?;
    let (b_excl,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM build_samples WHERE pname = 'b' AND outlier_excluded")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(b_excl, 3);
    // Empty slice → no-op (no error, no round-trip).
    db.mark_outliers_excluded(&[]).await?;

    Ok(())
}

/// bug_056: outlier burst must NOT displace good samples from the
/// ring. 32 good rows + 20 newer outlier-excluded rows; trim(32) →
/// 0 deleted (32 good kept, 20 outliers untouched). Before the fix,
/// the trim ranked all 52 rows and deleted the 20 oldest good ones,
/// permanently thinning the fit population to 12.
#[tokio::test]
async fn test_trim_build_samples_ignores_outliers() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // 32 good samples (completed_at = 0..31) + 20 newer outliers
    // (completed_at = 32..51) for one key.
    for i in 0..52 {
        sqlx::query(
            "INSERT INTO build_samples
               (pname, system, tenant, duration_secs, peak_memory_bytes,
                cpu_limit_cores, completed_at, outlier_excluded)
             VALUES ('burst', 'x86_64-linux', 't', $1, 0, 4, to_timestamp($1), $2)",
        )
        .bind(i as f64)
        .bind(i >= 32)
        .execute(&test_db.pool)
        .await?;
    }

    // Single-key trim: only 32 non-outlier rows ranked → all rn ≤ 32
    // → nothing deleted. Outliers untouched.
    let deleted = db
        .trim_build_samples("burst", "x86_64-linux", "t", 32)
        .await?;
    assert_eq!(deleted, 0, "32 good rows fit in keep_n=32; nothing deleted");

    let (good, outliers): (i64, i64) = sqlx::query_as(
        "SELECT \
           count(*) FILTER (WHERE NOT outlier_excluded), \
           count(*) FILTER (WHERE outlier_excluded) \
         FROM build_samples WHERE pname = 'burst'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(good, 32, "all good rows kept");
    assert_eq!(outliers, 20, "outliers untouched (forensics-kept)");

    // Batch variant: same result.
    let deleted = db
        .trim_build_samples_batch(
            &["burst".into()],
            &["x86_64-linux".into()],
            &["t".into()],
            32,
        )
        .await?;
    assert_eq!(deleted, 0, "batch trim: same — outliers don't rank");

    // Now read-side parity: refit sees all 32 good rows.
    let rows = db
        .read_build_samples_for_keys(
            &["burst".into()],
            &["x86_64-linux".into()],
            &["t".into()],
            32,
        )
        .await?;
    assert_eq!(rows.len(), 32, "refit population fully preserved");

    Ok(())
}

/// MAD outlier gate normalizes by hw_class before comparing against
/// the ref-second-denominated previous fit. An on-curve sample from a
/// fast hw_class (factor=2.0) has wall=ref/2; without normalization
/// `|ln(wall/ref)| = |ln(0.5)| ≈ 0.69` would falsely trip the 3·MAD
/// gate and the row would be permanently excluded.
// r[verify sched.sla.hw-ref-seconds]
// r[verify sched.sla.outlier-mad-reject]
#[tokio::test]
async fn test_refresh_outlier_gate_normalizes_hw_class() -> anyhow::Result<()> {
    use crate::sla::{
        SlaEstimator,
        types::{
            DurationFit, ExploreState, FittedParams, MemBytes, MemFit, ModelKey, RawCores,
            RefSeconds, WallSeconds,
        },
    };

    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // HwTable::aggregate needs ≥3 distinct pod_ids per hw_class.
    for (h, f) in [("ref", 1.0_f64), ("fast", 2.0)] {
        for p in 0..3 {
            sqlx::query(
                "INSERT INTO hw_perf_samples (hw_class, pod_id, factor)
                 VALUES ($1, $2, $3)",
            )
            .bind(h)
            .bind(format!("{h}-{p}"))
            // Isotropic [f;K] so `α·factor = f` regardless of the
            // seeded α (test is about hw normalization, not α-fit).
            .bind(sqlx::types::Json(
                serde_json::json!({ "alu": f, "membw": f, "ioseq": f }),
            ))
            .execute(&test_db.pool)
            .await?;
        }
    }

    let key = ModelKey {
        pname: "ff".into(),
        system: "x86_64-linux".into(),
        tenant: "t".into(),
    };
    let est = SlaEstimator::new(&crate::sla::config::SlaConfig::test_default());
    // Seed a tight prev fit directly (refit() partial-pools toward the
    // operator prior at low n_eff, which would skew predicted away from
    // T(c)=30+2000/c and confound the test). MAD([±0.02])=0.02 →
    // gate=3·1.4826·0.02≈0.089; |ln 0.5|=0.693 falsely trips that
    // without normalization, |ln 1.0|=0 with.
    est.seed(FittedParams {
        key: key.clone(),
        fit: DurationFit::Amdahl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
        },
        mem: MemFit::Independent { p90: MemBytes(0) },
        disk_p90: None,
        sigma_resid: 0.02,
        log_residuals: vec![0.02, -0.02, 0.02, -0.02, 0.02],
        n_eff: 8.0,
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 16.0,
        explore: ExploreState {
            distinct_c: 5,
            min_c: RawCores(2.0),
            max_c: RawCores(32.0),
            saturated: true,
            last_wall: WallSeconds(100.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: std::collections::HashMap::new(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });

    let row = |c: f64, t: f64, hw: &str| BuildSampleRow {
        pname: "ff".into(),
        system: "x86_64-linux".into(),
        tenant: "t".into(),
        duration_secs: t,
        cpu_limit_cores: Some(c),
        hw_class: Some(hw.into()),
        peak_memory_bytes: 256 << 20,
        ..Default::default()
    };
    // T_ref(16)=155s; on `fast` (factor 2.0) wall=77.5s — exactly on
    // the curve in ref-seconds, |ln 0.5| off in raw wall.
    db.write_build_sample(&row(16.0, 77.5, "fast")).await?;
    // Control: genuine 10× outlier on ref hw.
    db.write_build_sample(&row(16.0, 1550.0, "ref")).await?;

    est.refresh(&db, &[]).await?;

    // Precondition: HwTable loaded both classes (the gate is the test
    // subject; don't let a view-shape mismatch silently degrade to
    // factor=1.0 and "pass" by coincidence).
    let hw = est.hw_table();
    assert_eq!(
        hw.factor("fast"),
        Some([2.0; crate::sla::hw::K]),
        "hw_perf_samples → HwTable::aggregate"
    );
    assert_eq!(hw.factor("ref"), Some([1.0; crate::sla::hw::K]));

    let excluded: Vec<f64> = sqlx::query_scalar(
        "SELECT duration_secs FROM build_samples WHERE outlier_excluded ORDER BY duration_secs",
    )
    .fetch_all(&test_db.pool)
    .await?;
    assert_eq!(
        excluded,
        vec![1550.0],
        "on-curve fast-hw sample kept; only the 10× outlier flagged"
    );

    Ok(())
}

/// `read_sla_overrides` filters by cluster: rows scoped to a different
/// cluster are excluded; NULL-cluster rows match everywhere. Before
/// the `cluster IS NULL OR cluster = $1` clause, a `cluster:"east"`
/// row matched in every region under shared-PG.
#[tokio::test]
async fn read_sla_overrides_filters_cluster() -> anyhow::Result<()> {
    use crate::db::SlaOverrideRow;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let mk = |cluster: Option<&str>| SlaOverrideRow {
        pname: "hello".into(),
        cluster: cluster.map(Into::into),
        cores: Some(4.0),
        ..Default::default()
    };
    db.insert_sla_override(&mk(Some("east"))).await?;
    db.insert_sla_override(&mk(Some("west"))).await?;
    db.insert_sla_override(&mk(None)).await?;

    let east = db.read_sla_overrides("east", None).await?;
    let clusters: Vec<_> = east.iter().map(|r| r.cluster.as_deref()).collect();
    assert_eq!(east.len(), 2, "east + NULL only; got {clusters:?}");
    assert!(clusters.contains(&Some("east")));
    assert!(clusters.contains(&None));
    assert!(!clusters.contains(&Some("west")), "other-cluster filtered");

    // Single-cluster default (cluster="") sees only NULL rows.
    let global = db.read_sla_overrides("", None).await?;
    assert_eq!(global.len(), 1);
    assert_eq!(global[0].cluster, None);

    Ok(())
}

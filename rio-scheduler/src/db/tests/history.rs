//! build_samples retention + sla_overrides CRUD tests.

use rio_test_support::TestDb;

use crate::db::{BuildSampleRow, SchedulerDb};

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

    // Rebalancer query: same window (7d) should see the 1 fresh
    // row. The aged row (45d) is already deleted, but even if it
    // weren't, it's outside the 7d window. Roundtrip: insert with
    // (8.0, 2_097_152) survived → that's the row we expect.
    let samples = db.query_build_samples_last_days(7).await?;
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0], (8.0, 2_097_152));

    // And the window actually filters: 0 days → nothing is
    // strictly newer than now().
    let empty = db.query_build_samples_last_days(0).await?;
    assert!(empty.is_empty(), "0-day window should be empty");

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
        prefer_local_build: Some(false),
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
    assert_eq!(got.5, None, "hw_class stays NULL (controller fills)");
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

    let est = SlaEstimator::new(7.0 * 86400.0, 32, None);
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

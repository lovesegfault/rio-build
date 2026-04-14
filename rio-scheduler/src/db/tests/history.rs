//! EMA update + build_samples retention tests.

use rio_test_support::TestDb;

use super::insert_test_derivation;
use crate::db::{BuildSampleRow, EMA_ALPHA, SchedulerDb};

// r[verify sched.estimate.ema-alpha]
/// EMA: first insert uses the duration directly; second update blends.
/// ema = old * (1-ALPHA) + new * ALPHA = 10 * 0.7 + 20 * 0.3 = 13.
#[tokio::test]
async fn test_update_build_history_ema_accumulates() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    db.update_build_history("hello", "x86_64-linux", 10.0, None, None)
        .await?;
    let (ema1, count1): (f64, i32) = sqlx::query_as(
        "SELECT ema_duration_secs, sample_count FROM build_history \
         WHERE pname = 'hello' AND system = 'x86_64-linux'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert!((ema1 - 10.0).abs() < 0.001, "first insert: ema={ema1}");
    assert_eq!(count1, 1);

    db.update_build_history("hello", "x86_64-linux", 20.0, None, None)
        .await?;
    let (ema2, count2): (f64, i32) = sqlx::query_as(
        "SELECT ema_duration_secs, sample_count FROM build_history \
         WHERE pname = 'hello' AND system = 'x86_64-linux'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    let expected = 10.0 * (1.0 - EMA_ALPHA) + 20.0 * EMA_ALPHA;
    assert!(
        (ema2 - expected).abs() < 0.001,
        "second update: expected {expected}, got {ema2}"
    );
    assert_eq!(count2, 2);
    Ok(())
}

/// The COALESCE(blend, new, old) pattern for the nullable resource
/// EMAs. All four old×new combinations in one test: the hard part
/// is the `None` cases, where a naïve `old*0.7 + 0*0.3` would drag
/// a real EMA toward zero on every build with a failed proc read.
#[tokio::test]
async fn test_update_build_history_memory_ema_none_is_no_signal() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let fetch = || async {
        sqlx::query_as::<_, (Option<f64>, Option<f64>)>(
            "SELECT ema_peak_memory_bytes, ema_peak_cpu_cores \
             FROM build_history WHERE pname = 'mem' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await
    };

    // 1. old=None, new=None → stays None.
    db.update_build_history("mem", "x86_64-linux", 1.0, None, None)
        .await?;
    let (mem, cpu) = fetch().await?;
    assert_eq!(mem, None, "None×None: mem should stay NULL");
    assert_eq!(cpu, None, "None×None: cpu should stay NULL");

    // 2. old=None, new=Some → initializes (blend is NULL, falls
    //    to $new).
    db.update_build_history("mem", "x86_64-linux", 1.0, Some(100_000_000), Some(2.0))
        .await?;
    let (mem, cpu) = fetch().await?;
    assert!(
        mem.is_some_and(|m| (m - 100_000_000.0).abs() < 0.001),
        "None×Some: mem should initialize to 100M, got {mem:?}"
    );
    assert!(
        cpu.is_some_and(|c| (c - 2.0).abs() < 0.001),
        "None×Some: cpu should initialize to 2.0, got {cpu:?}"
    );

    // 3. old=Some, new=None → KEEPS old (the important case — no
    //    signal doesn't drag). blend is NULL (x+NULL), falls to
    //    $new=NULL, falls to old.
    db.update_build_history("mem", "x86_64-linux", 1.0, None, None)
        .await?;
    let (mem, cpu) = fetch().await?;
    assert!(
        mem.is_some_and(|m| (m - 100_000_000.0).abs() < 0.001),
        "Some×None: mem should be UNCHANGED at 100M (no drag), got {mem:?}"
    );
    assert!(
        cpu.is_some_and(|c| (c - 2.0).abs() < 0.001),
        "Some×None: cpu should be UNCHANGED at 2.0 (no drag), got {cpu:?}"
    );

    // 4. old=Some, new=Some → normal EMA blend.
    db.update_build_history("mem", "x86_64-linux", 1.0, Some(200_000_000), Some(4.0))
        .await?;
    let (mem, cpu) = fetch().await?;
    let expect_mem = 100_000_000.0 * (1.0 - EMA_ALPHA) + 200_000_000.0 * EMA_ALPHA;
    let expect_cpu = 2.0 * (1.0 - EMA_ALPHA) + 4.0 * EMA_ALPHA;
    assert!(
        mem.is_some_and(|m| (m - expect_mem).abs() < 0.001),
        "Some×Some: mem should blend to {expect_mem}, got {mem:?}"
    );
    assert!(
        cpu.is_some_and(|c| (c - expect_cpu).abs() < 0.001),
        "Some×Some: cpu should blend to {expect_cpu}, got {cpu:?}"
    );

    Ok(())
}

/// Misclassification penalty: overwrites EMA (not blends). Harsh
/// correction so the next classify() picks the right class after ONE
/// bad route, not several.
#[tokio::test]
async fn test_update_build_history_misclassified_overwrites() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Seed: EMA at 10s (would classify as "small").
    db.update_build_history("slowpoke", "x86_64-linux", 10.0, None, None)
        .await?;

    // Actual took 200s. Penalty write.
    db.update_build_history_misclassified("slowpoke", "x86_64-linux", 200.0)
        .await?;

    let ema: f64 = sqlx::query_scalar(
        "SELECT ema_duration_secs FROM build_history \
         WHERE pname = 'slowpoke' AND system = 'x86_64-linux'",
    )
    .fetch_one(&test_db.pool)
    .await?;

    // EMA is OVERWRITTEN (not 10*0.7+200*0.3=67). The next
    // classify() sees 200s, routes to large. That's the point.
    assert!(
        (ema - 200.0).abs() < 0.001,
        "penalty overwrites (not blends): expected 200, got {ema}"
    );

    // Second penalty: EMA overwritten again.
    db.update_build_history_misclassified("slowpoke", "x86_64-linux", 180.0)
        .await?;
    let ema2: f64 =
        sqlx::query_scalar("SELECT ema_duration_secs FROM build_history WHERE pname = 'slowpoke'")
            .fetch_one(&test_db.pool)
            .await?;
    assert!((ema2 - 180.0).abs() < 0.001);

    Ok(())
}

/// Proactive mid-build ema: a running build's cgroup memory.peak
/// (via ProgressUpdate) already exceeds the EMA. The overwrite
/// happens BEFORE the build completes — next submit of this
/// (pname, system) is right-sized without waiting for an OOM→retry.
///
/// Walks the conditional-update guard: observed>ema → overwrite;
/// observed≤ema → no-op (common case: worker emits every 10s,
/// most samples are under the EMA). Also: NULL ema initializes;
/// terminal derivation → no-op (late-arriving sample, completion
/// already wrote the authoritative value).
// r[verify sched.classify.proactive-ema]
#[tokio::test]
async fn test_mid_build_resource_sample_updates_ema() -> anyhow::Result<()> {
    const GB: u64 = 1 << 30;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Derivation: running (non-terminal), pname="test-pkg" via
    // insert_test_derivation's default. drv_path is what
    // ProgressUpdate carries.
    let drv_path = rio_test_support::fixtures::test_drv_path("proactive");
    insert_test_derivation(&db, "proactive").await?;
    sqlx::query("UPDATE derivations SET status = 'running' WHERE drv_path = $1")
        .bind(&drv_path)
        .execute(&test_db.pool)
        .await?;

    let fetch_ema = || async {
        sqlx::query_scalar::<_, Option<f64>>(
            "SELECT ema_peak_memory_bytes FROM build_history \
             WHERE pname = 'test-pkg' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await
    };

    // --- No build_history row yet (first-ever build) → no-op.
    // The join finds nothing to update. First-run has no EMA to
    // proactively correct.
    let updated = db
        .update_ema_peak_memory_proactive(&drv_path, 2 * GB)
        .await?;
    assert!(!updated, "no build_history row → no-op");

    // --- Seed build_history: prior completion left EMA at 1 GB.
    db.update_build_history("test-pkg", "x86_64-linux", 60.0, Some(GB), None)
        .await?;
    let seed = fetch_ema().await?;
    assert!(
        seed.is_some_and(|m| (m - GB as f64).abs() < 1.0),
        "precondition: EMA must actually be ~1 GB before the proactive \
         update, else the 'updated to 2 GB' assertion proves nothing; \
         got {seed:?}"
    );

    // --- observed (2 GB) > ema (1 GB) → overwrite. The mid-build
    // sample exceeds the estimate; next submit sees 2 GB.
    let updated = db
        .update_ema_peak_memory_proactive(&drv_path, 2 * GB)
        .await?;
    assert!(updated, "observed > ema → should update");
    let ema = fetch_ema().await?;
    assert!(
        ema.is_some_and(|m| (m - (2 * GB) as f64).abs() < 1.0),
        "overwrite (not blend): expected 2 GB, got {ema:?}. \
         A blend (1*0.7 + 2*0.3 = 1.3 GB) would take multiple \
         samples to converge — defeats 'proactive'."
    );

    // --- observed (1.5 GB) ≤ ema (2 GB) → no-op. Monotone:
    // memory.peak never decreases, but the EMA might exceed the
    // CURRENT peak if a prior build of this pname was bigger.
    let updated = db
        .update_ema_peak_memory_proactive(&drv_path, 3 * GB / 2)
        .await?;
    assert!(!updated, "observed ≤ ema → no-op");
    let ema = fetch_ema().await?;
    assert!(
        ema.is_some_and(|m| (m - (2 * GB) as f64).abs() < 1.0),
        "observed ≤ ema: unchanged at 2 GB, got {ema:?}"
    );

    // --- Terminal derivation → no-op. Completion already wrote the
    // authoritative value; a late-arriving Progress (race with
    // completion on the wire) must not clobber it.
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_path = $1")
        .bind(&drv_path)
        .execute(&test_db.pool)
        .await?;
    let updated = db
        .update_ema_peak_memory_proactive(&drv_path, 4 * GB)
        .await?;
    assert!(
        !updated,
        "terminal status → filtered by partial-index WHERE"
    );
    let ema = fetch_ema().await?;
    assert!(
        ema.is_some_and(|m| (m - (2 * GB) as f64).abs() < 1.0),
        "terminal: still 2 GB (4 GB sample rejected), got {ema:?}"
    );

    Ok(())
}

/// Proactive ema when the existing EMA is NULL: a build_history
/// row exists (duration seeded) but no memory signal yet (prior
/// completion had peak_memory_bytes=0 → None). The `IS NULL OR <`
/// guard initializes rather than blocking on `NULL < $2` (which is
/// NULL, not false, in PG — but NULL fails WHERE, so without the
/// `IS NULL OR` disjunct this case would silently no-op forever).
#[tokio::test]
async fn test_mid_build_sample_initializes_null_ema() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv_path = rio_test_support::fixtures::test_drv_path("nullmem");
    insert_test_derivation(&db, "nullmem").await?;

    // Seed: duration EMA exists, memory EMA is NULL (None in the
    // update_build_history call).
    db.update_build_history("test-pkg", "x86_64-linux", 30.0, None, None)
        .await?;
    let before: Option<f64> = sqlx::query_scalar(
        "SELECT ema_peak_memory_bytes FROM build_history WHERE pname = 'test-pkg'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(before, None, "precondition: mem EMA must be NULL");

    let updated = db
        .update_ema_peak_memory_proactive(&drv_path, 500_000_000)
        .await?;
    assert!(updated, "NULL < observed → initializes");
    let after: Option<f64> = sqlx::query_scalar(
        "SELECT ema_peak_memory_bytes FROM build_history WHERE pname = 'test-pkg'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert!(
        after.is_some_and(|m| (m - 500_000_000.0).abs() < 1.0),
        "NULL → initialized to observed, got {after:?}"
    );

    Ok(())
}

/// The estimator reads `ema_peak_memory_bytes` for memory-bump
/// classify(). Verify the write→read roundtrip works end to end.
#[tokio::test]
async fn test_build_history_memory_roundtrip_read() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    db.update_build_history(
        "roundtrip",
        "aarch64-linux",
        42.0,
        Some(1_073_741_824),
        Some(4.5), // peak_cpu_cores
    )
    .await?;

    let rows = db.read_build_history().await?;
    let row = rows
        .iter()
        .find(|(p, s, _, _, _, _)| p == "roundtrip" && s == "aarch64-linux")
        .expect("written row should be readable");

    assert!((row.2 - 42.0).abs() < 0.001, "duration: {}", row.2);
    assert!(
        row.3.is_some_and(|m| (m - 1_073_741_824.0).abs() < 0.001),
        "peak mem: {:?}",
        row.3
    );
    // cpu_cores round-trips too. Verifies the column is in both
    // the INSERT and the SELECT.
    assert!(
        row.4.is_some_and(|c| (c - 4.5).abs() < 0.001),
        "peak cpu cores: {:?}",
        row.4
    );
    Ok(())
}

/// cpu_cores uses the same COALESCE(blend, new, old) pattern as
/// memory. A build that exits in <1s (before the 1Hz poller
/// samples) reports 0.0 → None → column unchanged. The next real
/// build blends properly. Same "no signal doesn't drag" semantics.
///
/// Separate test from the memory one: checks the NEW column's
/// COALESCE specifically (easy to forget to add it to the UPSERT).
#[tokio::test]
async fn test_update_build_history_cpu_cores_coalesce() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let fetch_cpu = || async {
        sqlx::query_scalar::<_, Option<f64>>(
            "SELECT ema_peak_cpu_cores FROM build_history \
             WHERE pname = 'cpu' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await
    };

    // First: None (short build, no samples) → column stays NULL.
    db.update_build_history("cpu", "x86_64-linux", 0.5, None, None)
        .await?;
    assert_eq!(fetch_cpu().await?, None, "None initializes to NULL");

    // Second: Some(4.0) → initializes.
    db.update_build_history("cpu", "x86_64-linux", 120.0, None, Some(4.0))
        .await?;
    let cpu = fetch_cpu().await?;
    assert!(
        cpu.is_some_and(|c| (c - 4.0).abs() < 0.001),
        "Some initializes: expected 4.0, got {cpu:?}"
    );

    // Third: None again (another short build) → UNCHANGED at 4.0.
    // This is the bug-catcher: if COALESCE is missing from the
    // UPSERT for this column, NULL*0.7 + NULL*0.3 = NULL would
    // clobber the good data.
    db.update_build_history("cpu", "x86_64-linux", 0.5, None, None)
        .await?;
    let cpu = fetch_cpu().await?;
    assert!(
        cpu.is_some_and(|c| (c - 4.0).abs() < 0.001),
        "Some×None: UNCHANGED (no drag). If this fails, \
         COALESCE for ema_peak_cpu_cores is missing: {cpu:?}"
    );

    // Fourth: Some(8.0) → blends: 4.0*0.7 + 8.0*0.3 = 5.2.
    db.update_build_history("cpu", "x86_64-linux", 300.0, None, Some(8.0))
        .await?;
    let cpu = fetch_cpu().await?;
    let expected = 4.0 * (1.0 - EMA_ALPHA) + 8.0 * EMA_ALPHA;
    assert!(
        cpu.is_some_and(|c| (c - expected).abs() < 0.001),
        "Some×Some blends: expected {expected}, got {cpu:?}"
    );

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

    let est = SlaEstimator::new(7.0 * 86400.0, 32);
    let n = est.refresh(&db).await?;
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
    let n2 = est.refresh(&db).await?;
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

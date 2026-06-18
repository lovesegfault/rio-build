//! Batch upsert tests — COPY-streamed scaling + text-array encoding.

use crate::db::ServingGeneration;
use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::{DerivationRow, FencedOutcome, SchedulerDb, encode_pg_text_array};
use crate::state::DerivationStatus;

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
    assert_eq!(encode_pg_text_array(&["a,b".into()]), r#"{"a,b"}"#);
}

/// PG roundtrip: our encoder ⇔ PG's `::text[]` parser.
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
        let (decoded,): (Vec<String>,) = sqlx::query_as("SELECT $1::text[]")
            .bind(&encoded)
            .fetch_one(&test_db.pool)
            .await?;
        assert_eq!(decoded, input, "roundtrip failed for {encoded:?}");
    }
    Ok(())
}

#[tokio::test]
async fn test_insert_build_derivation_idempotent() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let build_id = Uuid::new_v4();
    db.insert_build(
        build_id,
        None,
        crate::state::PriorityClass::Scheduled,
        true,
        &Default::default(),
        None,
    )
    .await?;
    let drv_id = insert_test_derivation(&db, "aaa").await?;

    // Call twice — ON CONFLICT DO NOTHING should make the second call a no-op.
    db.insert_build_derivation(build_id, drv_id).await?;
    db.insert_build_derivation(build_id, drv_id).await?;

    let count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM build_derivations WHERE build_id = $1")
            .bind(build_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(count.0, 1, "ON CONFLICT should prevent duplicate");
    Ok(())
}

// r[verify sched.db.batch-unnest+2]
/// Large-DAG persistence: 10k nodes. Would fail on main
/// with "bind message has 90000 parameter formats" (or similar —
/// sqlx catches it before PG does) at 7282 nodes.
///
/// 10k is past the old derivations limit (7281 = 65535/9).
#[tokio::test]
async fn test_batch_upsert_10k_nodes() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    const N: usize = 10_000;
    let rows: Vec<DerivationRow> = (0..N)
        .map(|i| DerivationRow {
            drv_hash: format!("{i:032x}"), // 32-hex-char fake hash
            drv_path: format!("/nix/store/{}-test-{i}.drv", "a".repeat(32)),
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
            expected_output_paths: vec![format!("/nix/store/{}-out-{i}", "b".repeat(32))],
            output_names: vec!["out".into()],
            is_fixed_output: i % 7 == 0,
            is_ca: i % 11 == 0,
        })
        .collect();

    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows).await?;
    tx.commit().await?;

    assert_eq!(id_map.len(), N, "RETURNING gave back every row");
    // Spot-check: row 0 and row N-1 both present, distinct ids.
    let id0 = id_map.get(&format!("{:032x}", 0)).unwrap().0;
    let id_last = id_map.get(&format!("{:032x}", N - 1)).unwrap().0;
    assert_ne!(id0, id_last);

    // And they actually landed in PG, with nested arrays intact.
    let (features,): (Vec<String>,) =
        sqlx::query_as("SELECT required_features FROM derivations WHERE drv_hash = $1")
            .bind(format!("{:032x}", 2)) // i=2 → i%3==2 → [kvm, big-parallel]
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(features, vec!["kvm", "big-parallel"]);

    Ok(())
}

// r[verify sched.db.batch-unnest+2]
/// P0539 followup: "PG batch insert ~20s for ~1k rows in handle_merge_dag
/// is FK validation cost." Regression guard at the followup's exact shape
/// — 1k nodes, full `persist_merge_to_db` order (derivations →
/// build_derivations → edges in one tx) — pinned at <2s.
///
/// Research outcome (see [`rio_migrations::migrations::M_028`]):
///
/// - Migration 028 already dropped the three `→ derivations(derivation_id)`
///   FKs (`derivation_edges.{parent,child}_id`, `build_derivations.
///   derivation_id`). That was the 20s.
/// - The remaining `build_derivations.build_id_fkey` is `ON DELETE CASCADE`
///   (migration 008), kept intentionally — `delete_build` relies on it for
///   `cleanup_failed_merge` rollback. It validates the SAME `build_id` N
///   times against a tiny `builds` PK; cost is sub-ms at this N.
/// - Insert order is already FK-friendly (derivations first); `DEFERRABLE`
///   was considered and rejected (still N lookups at COMMIT).
///
/// The 2s bound is loose for ephemeral-PG debug builds (~80ms observed)
/// but hard-fails the original 20s class. Per-phase timings printed for
/// future "where did the time go" questions.
#[tokio::test]
async fn test_batch_persist_1k_fk_perf_bound() -> anyhow::Result<()> {
    use std::time::Instant;

    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    const N: usize = 1_000;
    // FK target for build_derivations.build_id (the one FK still in
    // this path post-028).
    let build_id = Uuid::new_v4();
    db.insert_build(
        build_id,
        None,
        crate::state::PriorityClass::Scheduled,
        true,
        &Default::default(),
        None,
    )
    .await?;

    let rows: Vec<DerivationRow> = (0..N)
        .map(|i| DerivationRow {
            drv_hash: format!("fk{i:030x}"),
            drv_path: format!("/nix/store/{}-fk{i}.drv", "a".repeat(32)),
            pname: Some(format!("p{i}")),
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            required_features: vec![],
            expected_output_paths: vec![format!("/nix/store/{}-out{i}", "b".repeat(32))],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            is_ca: false,
        })
        .collect();

    let mut tx = db.pool().begin().await?;
    let t0 = Instant::now();

    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows).await?;
    let t_derivs = t0.elapsed();

    let db_ids: Vec<Uuid> = id_map.values().map(|(id, _)| *id).collect();
    SchedulerDb::batch_insert_build_derivations(&mut tx, build_id, &db_ids).await?;
    let t_bd = t0.elapsed();

    // ~4× edges (matches the typical-fanout note in db/recovery.rs's
    // load_build_graph).
    let ids: Vec<Uuid> = (0..N)
        .map(|i| id_map.get(&format!("fk{i:030x}")).unwrap().0)
        .collect();
    let ids = &ids;
    let edges: Vec<(Uuid, Uuid)> = (1..N)
        .flat_map(|i| (1..=4.min(i)).map(move |d| (ids[i - d], ids[i])))
        .collect();
    SchedulerDb::batch_insert_edges(&mut tx, &edges).await?;
    let t_edges = t0.elapsed();

    tx.commit().await?;
    let total = t0.elapsed();

    eprintln!(
        "P0539 1k-row persist: derivations={t_derivs:?} \
         build_derivations(+{:?}) edges(+{:?}) commit→{total:?}",
        t_bd - t_derivs,
        t_edges - t_bd,
    );

    assert_eq!(id_map.len(), N);
    assert!(
        total.as_secs() < 2,
        "P0539: 1k-row persist_merge_to_db-shape batch took {total:?} (≥2s). \
         Original symptom was ~20s from per-row FK validation; migration 028 \
         dropped the →derivations FKs. If this fires, an FK or per-row \
         trigger came back on derivation_edges/build_derivations."
    );
    Ok(())
}

// r[verify sched.db.merge-batch-shape]
/// sh-036 §2: `merge_phase_seconds{phase="5-persist-and-activate"}` was
/// 4.91 s of an 11.63 s `MergeDag` actor turn; the cost was the
/// UNNEST-decode + `ON CONFLICT` processing over 14701 + ~45 k rows.
/// Streaming via `COPY` into `ON COMMIT DROP` temp tables and then one
/// `INSERT … SELECT … ON CONFLICT` cuts the wall-clock to ~100 ms.
///
/// `#[ignore]` perf-tier — wall-clock gates flake under parallel builder
/// load (ci-failure-patterns "Wall-clock gate under load"); the 500 ms
/// bound is ~5× headroom over the post-COPY observed time on ephemeral
/// PG. The structural temp-table existence assertions are the
/// load-independent property pin; the wall-clock bound is the
/// regression guard.
#[tokio::test]
#[ignore = "perf-tier wall-clock gate; run via --run-ignored"]
async fn persist_merge_14k_rows_under_500ms() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    async fn temp_table_exists(tx: &mut sqlx::PgConnection, name: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_tables \
             WHERE tablename = $1 AND schemaname LIKE 'pg_temp%')",
        )
        .bind(name)
        .fetch_one(tx)
        .await
        .unwrap()
    }

    // sh-036's measured shape: 14701 nodes, ~45 k edges (≈3×).
    const N: usize = 14_701;
    let rows: Vec<DerivationRow> = (0..N)
        .map(|i| DerivationRow {
            drv_hash: format!("{i:032x}"),
            drv_path: format!("/nix/store/{}-sh036-{i}.drv", "a".repeat(32)),
            pname: Some(format!("pkg-{i}")),
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            required_features: if i % 3 == 0 {
                vec![]
            } else {
                vec!["kvm".into()]
            },
            expected_output_paths: vec![format!("/nix/store/{}-out-{i}", "b".repeat(32))],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            is_ca: false,
        })
        .collect();

    let mut tx = db.pool().begin().await?;
    let t0 = Instant::now();
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows).await?;
    let t_derivs = t0.elapsed();

    // Structural pin (red-first): the COPY shape leaves an
    // ON COMMIT DROP temp table mid-transaction.
    assert!(
        temp_table_exists(&mut tx, "_merge_derivations").await,
        "batch_upsert_derivations MUST stream via COPY into an \
         ON COMMIT DROP temp table (_merge_derivations) before the \
         ON CONFLICT upsert — sh-036's 4.91 s in-cluster cost was the \
         UNNEST decode; the structural shape is the load-independent \
         property"
    );

    let ids: Vec<Uuid> = (0..N)
        .map(|i| id_map.get(&format!("{i:032x}")).unwrap().0)
        .collect();
    let ids = &ids;
    let edges: Vec<(Uuid, Uuid)> = (1..N)
        .flat_map(|i| (1..=3.min(i)).map(move |d| (ids[i - d], ids[i])))
        .collect();
    assert!(edges.len() >= 44_000);
    SchedulerDb::batch_insert_edges(&mut tx, &edges).await?;
    let total = t0.elapsed();

    assert!(
        temp_table_exists(&mut tx, "_merge_edges").await,
        "batch_insert_edges MUST stream via COPY into an ON COMMIT DROP \
         temp table (_merge_edges)"
    );
    tx.commit().await?;
    // ON COMMIT DROP cleaned both up.
    let mut post = db.pool().begin().await?;
    assert!(!temp_table_exists(&mut post, "_merge_derivations").await);
    assert!(!temp_table_exists(&mut post, "_merge_edges").await);
    post.rollback().await?;

    eprintln!(
        "sh-036 14701-row persist: derivations={t_derivs:?} \
         edges(+{:?}) total={total:?}",
        total - t_derivs,
    );
    assert_eq!(id_map.len(), N);
    assert!(
        total < Duration::from_millis(500),
        "sh-036: 14701-row + ~45k-edge persist took {total:?} (≥500 ms). \
         The UNNEST formulation measured ~2–5 s here (4.91 s in-cluster); \
         the COPY → ON COMMIT DROP temp → INSERT…SELECT shape lands \
         ~100 ms. If this fires under load, widen the slack; if it fires \
         solo, the COPY path regressed."
    );
    Ok(())
}

// r[verify sched.db.batch-unnest+2]
/// Edges: 40k rows. Old limit was 32767 (2 cols). Build a
/// dense DAG over 10k nodes (fresh DB, so re-insert).
#[tokio::test]
async fn test_batch_insert_40k_edges() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Need N nodes first (edge UUIDs come from id_map; the FKs themselves
    // were dropped in migration 028). Reuse shape from above.
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
            is_ca: false,
        })
        .collect();
    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows).await?;

    // 40k edges: each node i>0 has 4 parents among [i-1, i-2, ...].
    // ON CONFLICT DO NOTHING dedups any collisions.
    let ids: Vec<Uuid> = (0..N)
        .map(|i| id_map.get(&format!("{i:032x}")).unwrap().0)
        .collect();
    let ids = &ids; // borrow so inner `move` closure copies the ref
    let edges: Vec<(Uuid, Uuid)> = (1..N)
        .flat_map(|i| (1..=4.min(i)).map(move |d| (ids[i - d], ids[i])))
        .collect();
    assert!(edges.len() > 32_768, "test must exceed old 2-col limit");

    SchedulerDb::batch_insert_edges(&mut tx, &edges).await?;
    tx.commit().await?;

    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM derivation_edges")
        .fetch_one(&test_db.pool)
        .await?;
    // ≤ edges.len() because of ON CONFLICT dedup, but > old limit.
    assert!(count > 32_768);
    Ok(())
}

// -----------------------------------------------------------------------
// Claims-floor fence boundary semantics (`sched.evidence.durability`),
// pinned against a surviving fenced writer (the status persist). The
// below-floor refusal direction is pinned per-writer where each writer
// lives (db/tests/derivations.rs, db/tests/wanted.rs,
// db/tests/materialization.rs); these two pin the fence COMPARISON
// itself — at-floor and empty-floor must apply. (The walk-era evidence
// helpers these originally exercised died in T-D5.1; the fence
// infrastructure they rode is unchanged.)
// -----------------------------------------------------------------------

/// OQ4 / `sched.lease.generation-claim`: a write carrying a generation
/// EQUAL to the floor MUST apply — same generation ⇔ no holder change ⇔
/// no newer tenure's evidence exists, so this is the same-epoch
/// re-acquire keep, not a hazard. The fence comparison is `>=` and must
/// never be tightened to `>`: this test pins that.
// r[verify sched.evidence.durability+4]
#[tokio::test]
async fn same_generation_write_at_floor_applies() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let hash: crate::state::DrvHash = "fence-same".into();
    insert_test_derivation(&db, hash.as_str()).await?;
    // The floor is exactly 5 (this tenure's own claim, re-read after a
    // same-epoch re-acquire).
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (5, 'tenure-same')",
    )
    .execute(&test_db.pool)
    .await?;

    // A write carrying EXACTLY the floor generation applies and changes
    // the row. (`update_derivation_status` reports `Applied(0)` by
    // contract — it does not propagate the row count — so the SELECT
    // below is the actually-changed proof.)
    let outcome = db
        .update_derivation_status(
            &hash,
            DerivationStatus::Queued,
            None,
            ServingGeneration::stamp_from_claim(5),
        )
        .await?;
    assert!(
        matches!(outcome, FencedOutcome::Applied(_)),
        "a write at exactly the floor (same-epoch re-acquire) MUST apply — \
         the >= comparison is load-bearing; got {outcome:?}"
    );
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'fence-same'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        status, "queued",
        "the at-floor write must have actually changed the row"
    );
    Ok(())
}

/// The fence is not over-eager on a fresh cluster: with no claims and
/// no assignments the floor is empty, and any serving generation
/// applies.
// r[verify sched.evidence.durability+4]
#[tokio::test]
async fn empty_floor_write_applies() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let hash: crate::state::DrvHash = "fence-fresh".into();
    insert_test_derivation(&db, hash.as_str()).await?;
    let outcome = db
        .update_derivation_status(
            &hash,
            DerivationStatus::Queued,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert!(
        matches!(outcome, FencedOutcome::Applied(_)),
        "a fresh cluster (empty floor) must apply writes; got {outcome:?}"
    );
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'fence-fresh'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(status, "queued", "the empty-floor write must have landed");
    Ok(())
}

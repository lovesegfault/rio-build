//! Batch upsert tests — UNNEST scaling + text-array encoding.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::{DerivationRow, FencedWrite, SchedulerDb, encode_pg_text_array};
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

// r[verify sched.db.batch-unnest]
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
            wanted_output_names: vec![],
            topdown_pruned: false,
            closure_hole: false,
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

// r[verify sched.db.batch-unnest]
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
            wanted_output_names: vec![],
            topdown_pruned: false,
            closure_hole: false,
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

// D5-retarget: retires with the 062 dual-write (T-D5.3), not before — the
// stored-union round-trip is the dual-write's pin.
// r[verify sched.merge.wanted-outputs+2]
/// wanted_output_names persists, recovers, and UNIONS on conflict.
/// Two builds wanting different outputs of the same drv must leave the
/// row with the union — overwrite semantics would let build B's {out}
/// clobber build A's {out,dev} and un-want a still-needed output.
/// Empty-on-either-side saturates to empty (= "all wanted"), mirroring
/// `DerivationState::union_wanted`.
#[tokio::test]
async fn wanted_output_names_round_trip_and_union_on_conflict() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv_hash = "wanted-union-test";
    // One upsert of the same drv_hash with the given wanted set,
    // then read the column back.
    let upsert_and_read = async |wanted: &[&str]| -> anyhow::Result<Vec<String>> {
        let row = DerivationRow {
            drv_hash: drv_hash.into(),
            drv_path: rio_test_support::fixtures::test_drv_path(drv_hash),
            pname: Some("test-pkg".into()),
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            required_features: vec![],
            expected_output_paths: vec![
                format!("/nix/store/{}-out", "b".repeat(32)),
                format!("/nix/store/{}-dev", "c".repeat(32)),
            ],
            output_names: vec!["out".into(), "dev".into()],
            is_fixed_output: false,
            is_ca: false,
            wanted_output_names: wanted.iter().map(|s| s.to_string()).collect(),
            topdown_pruned: false,
            closure_hole: false,
        };
        let mut tx = db.pool().begin().await?;
        SchedulerDb::batch_upsert_derivations(&mut tx, &[row]).await?;
        tx.commit().await?;
        let (got,): (Vec<String>,) =
            sqlx::query_as("SELECT wanted_output_names FROM derivations WHERE drv_hash = $1")
                .bind(drv_hash)
                .fetch_one(&test_db.pool)
                .await?;
        Ok(got)
    };

    // 1. Fresh insert with wanted = ["out"] → reads back ["out"].
    assert_eq!(
        upsert_and_read(&["out"]).await?,
        vec!["out"],
        "fresh insert must persist the wanted set verbatim"
    );

    // 2. Same drv_hash, wanted = ["dev"] → sorted distinct union.
    assert_eq!(
        upsert_and_read(&["dev"]).await?,
        vec!["dev", "out"],
        "conflict must UNION, not overwrite — build B's {{dev}} must not \
         clobber build A's {{out}}"
    );

    // 2b. Re-upserting an already-present subset is idempotent (DISTINCT).
    assert_eq!(
        upsert_and_read(&["out"]).await?,
        vec!["dev", "out"],
        "re-upserting a subset must not duplicate or reorder"
    );

    // 3. Incoming empty (= all wanted) saturates the union to empty.
    assert_eq!(
        upsert_and_read(&[]).await?,
        Vec::<String>::new(),
        "empty incoming set means ALL outputs wanted; all ∪ X = all (= '{{}}')"
    );

    // 4. Existing empty (= all wanted) absorbs any later narrower set.
    assert_eq!(
        upsert_and_read(&["doc"]).await?,
        Vec::<String>::new(),
        "once saturated to all-wanted, a narrower incoming set must not \
         resurrect a finite set"
    );

    // 5. Recovery sees the persisted column (the round-trip half).
    let recovered = db.load_nonterminal_derivations().await?;
    let row = recovered
        .iter()
        .find(|r| r.drv_hash == drv_hash)
        .expect("upserted row is non-terminal and must be recovered");
    assert_eq!(
        row.wanted_output_names,
        Vec::<String>::new(),
        "recovery SELECT must carry wanted_output_names"
    );

    Ok(())
}

// r[verify sched.db.batch-unnest]
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
            wanted_output_names: vec![],
            topdown_pruned: false,
            closure_hole: false,
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
// Claims-floor fence for the evidence write helpers
// (`sched.evidence.durability`): a deposed tenure's late evidence write
// must be rolled back having written nothing; the current tenure's (and
// a same-generation tenure's) writes must apply.
// -----------------------------------------------------------------------

/// Stage one derivation carrying evidence (mark + hole) written by the
/// CURRENT tenure (serving at `floor_generation`, which is also the
/// durable claims floor).
async fn stage_evidence_at_floor(
    db: &SchedulerDb,
    pool: &sqlx::PgPool,
    drv_hash: &str,
    floor_generation: i64,
) -> anyhow::Result<()> {
    insert_test_derivation(db, drv_hash).await?;
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES ($1, 'tenure-current')",
    )
    .bind(floor_generation)
    .execute(pool)
    .await?;
    // The current tenure's evidence: mark + hole both set, written AT
    // the floor (its own claim) — the fence must let these through.
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE drv_hash = $1")
        .bind(drv_hash)
        .execute(pool)
        .await?;
    let stamped = db
        .set_closure_hole_by_hashes(&[drv_hash.to_string()], floor_generation)
        .await?;
    anyhow::ensure!(
        stamped == FencedWrite::Applied(1),
        "staging premise: the current tenure's stamp at the floor must apply, got {stamped:?}"
    );
    Ok(())
}

/// A17 / `sched.evidence.durability`: a deposed tenure's late evidence
/// clear must NOT erase evidence the newer tenure relies on. The
/// durable claims floor is 2 (the successor claimed); the deposed
/// tenure's in-flight batched clear (serving generation 1) arrives
/// afterwards and must be fenced — rolled back having written nothing —
/// so the successor's mark and closure-hole breadcrumb survive.
///
/// Pre-fence this was exactly the A17 stale-override window: the
/// pool-level clear had no idea it was stale and erased the evidence
/// (red transcript in the introducing commit).
// r[verify sched.evidence.durability+2]
#[tokio::test]
async fn stale_tenure_clear_does_not_erase_newer_evidence() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    stage_evidence_at_floor(&db, &test_db.pool, "fence-a", 2).await?;

    // The deposed tenure's late clear (it served at generation 1; the
    // floor is 2): every fenced helper must refuse it.
    assert_eq!(
        db.clear_topdown_pruned_by_hashes(&["fence-a".to_string()], 1)
            .await?,
        FencedWrite::Fenced,
        "the batched both-bits clear must be fenced below the floor"
    );
    assert_eq!(
        db.clear_topdown_pruned_by_hash("fence-a", 1).await?,
        FencedWrite::Fenced,
        "the single-row mark clear must be fenced below the floor"
    );
    assert_eq!(
        db.clear_closure_hole_by_hashes(&["fence-a".to_string()], 1)
            .await?,
        FencedWrite::Fenced,
        "the heal must be fenced below the floor"
    );
    assert_eq!(
        db.set_closure_hole_by_hashes(&["fence-b".to_string()], 1)
            .await?,
        FencedWrite::Fenced,
        "the stamp must be fenced below the floor (even for rows it would not touch)"
    );

    // The newer tenure's evidence must survive every stale write.
    let (pruned, hole): (bool, bool) = sqlx::query_as(
        "SELECT topdown_pruned, closure_hole FROM derivations WHERE drv_hash = 'fence-a'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert!(
        pruned && hole,
        "a deposed tenure's late batched clear must be fenced by the claims floor, \
         not erase the newer tenure's evidence (topdown_pruned={pruned}, closure_hole={hole})"
    );
    Ok(())
}

/// The fence is not over-eager: the CURRENT tenure's writes (serving at
/// the floor — its own claim) apply normally, and a fresh cluster (no
/// claims, no assignments — empty floor) applies everything.
// r[verify sched.evidence.durability+2]
#[tokio::test]
async fn current_tenure_evidence_writes_apply() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Fresh cluster: no claims rows at all — the floor is empty and any
    // generation passes (stage_evidence_at_floor's own ensure! covers
    // the at-the-floor stamp once the claim row exists).
    insert_test_derivation(&db, "fence-fresh").await?;
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE drv_hash = 'fence-fresh'")
        .execute(&test_db.pool)
        .await?;
    assert_eq!(
        db.set_closure_hole_by_hashes(&["fence-fresh".to_string()], 1)
            .await?,
        FencedWrite::Applied(1),
        "a fresh cluster (empty floor) must apply evidence writes"
    );

    // Current tenure at the floor: stamp, heal, and clear all apply.
    stage_evidence_at_floor(&db, &test_db.pool, "fence-cur", 7).await?;
    assert_eq!(
        db.clear_closure_hole_by_hashes(&["fence-cur".to_string()], 7)
            .await?,
        FencedWrite::Applied(1),
        "the current tenure's heal must apply"
    );
    assert_eq!(
        db.clear_topdown_pruned_by_hashes(&["fence-cur".to_string()], 7)
            .await?,
        FencedWrite::Applied(1),
        "the current tenure's both-bits clear must apply"
    );
    // A LATER tenure (above the floor) also applies — the fence is a
    // floor, not an exact-match check.
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE drv_hash = 'fence-cur'")
        .execute(&test_db.pool)
        .await?;
    assert_eq!(
        db.clear_topdown_pruned_by_hash("fence-cur", 8).await?,
        FencedWrite::Applied(1),
        "an above-floor serving generation must apply"
    );
    Ok(())
}

/// OQ4 / `sched.lease.generation-claim`: a write carrying a generation
/// EQUAL to the floor MUST apply — same generation ⇔ no holder change ⇔
/// no newer tenure's evidence exists, so this is the same-epoch
/// re-acquire keep, not a hazard. The fence comparison is `>=` and must
/// never be tightened to `>`: this test pins that.
// r[verify sched.evidence.durability+2]
#[tokio::test]
async fn same_generation_write_at_floor_applies() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    insert_test_derivation(&db, "fence-same").await?;
    // The floor is exactly 5 (this tenure's own claim, re-read after a
    // same-epoch re-acquire).
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (5, 'tenure-same')",
    )
    .execute(&test_db.pool)
    .await?;

    // A write carrying EXACTLY the floor generation applies and changes
    // the row.
    assert_eq!(
        db.set_closure_hole_by_hashes(&["fence-same".to_string()], 5)
            .await?,
        FencedWrite::Applied(1),
        "a write at exactly the floor (same-epoch re-acquire) MUST apply — \
         the >= comparison is load-bearing"
    );
    let (hole,): (bool,) =
        sqlx::query_as("SELECT closure_hole FROM derivations WHERE drv_hash = 'fence-same'")
            .fetch_one(&test_db.pool)
            .await?;
    assert!(
        hole,
        "the at-floor write must have actually changed the row"
    );
    Ok(())
}

//! Batch upsert tests — UNNEST scaling + text-array encoding.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::{DerivationRow, SchedulerDb, encode_pg_text_array};
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
            needs_resolve: false,
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
            drv_content: None,
            ca_modular_hash: None,
            ca_modular_hash_stripped: None,
            evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        })
        .collect();

    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows, &[]).await?;
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
            needs_resolve: false,
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
            drv_content: None,
            ca_modular_hash: None,
            ca_modular_hash_stripped: None,
            evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        })
        .collect();

    let mut tx = db.pool().begin().await?;
    let t0 = Instant::now();

    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows, &[]).await?;
    let t_derivs = t0.elapsed();

    let db_ids: Vec<Uuid> = id_map.values().map(|(id, _)| *id).collect();
    SchedulerDb::batch_insert_build_derivations(&mut tx, build_id, &db_ids).await?;
    let t_bd = t0.elapsed();

    // ~4× edges (matches typical fanout per recovery.rs:242 commentary).
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
            needs_resolve: false,
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
            drv_content: None,
            ca_modular_hash: None,
            ca_modular_hash_stripped: None,
            evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        };
        let mut tx = db.pool().begin().await?;
        SchedulerDb::batch_upsert_derivations(&mut tx, &[row], &[]).await?;
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

// r[verify sched.merge.substitute-topdown+14]
/// `topdown_pruned` persists with OR-on-conflict semantics (a pruned
/// merge sets it; an unrelated non-pruned merge of the same drv never
/// clears it), is cleared by the tx-scoped
/// `clear_topdown_pruned_for_parents` for the parent ids the caller
/// passes (now test-only — its merge-time production caller was
/// replaced by the post-reconciliation clear pass; production clears
/// go through `clear_topdown_pruned_by_hashes` /
/// `clear_topdown_pruned_by_hash`, callers per their docs), and rides
/// the recovery SELECT so a new leader can restore it.
#[tokio::test]
async fn topdown_pruned_or_on_conflict_clear_on_children_and_recovery() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv_hash = "topdown-pruned-test";
    let mk = |pruned: bool| DerivationRow {
        needs_resolve: false,
        drv_hash: drv_hash.into(),
        drv_path: rio_test_support::fixtures::test_drv_path(drv_hash),
        pname: Some("test-pkg".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![format!("/nix/store/{}-out", "b".repeat(32))],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        wanted_output_names: vec![],
        topdown_pruned: pruned,
        closure_hole: false,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
    };
    let upsert = async |row: DerivationRow| -> anyhow::Result<Uuid> {
        let mut tx = db.pool().begin().await?;
        let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &[row], &[]).await?;
        tx.commit().await?;
        Ok(id_map.get(drv_hash).unwrap().0)
    };
    let read = async || -> anyhow::Result<bool> {
        let (got,): (bool,) =
            sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = $1")
                .bind(drv_hash)
                .fetch_one(&test_db.pool)
                .await?;
        Ok(got)
    };

    // 1. Pruned merge sets it.
    let id = upsert(mk(true)).await?;
    assert!(read().await?, "pruned upsert must set topdown_pruned");

    // 2. A later non-pruned merge of the same drv must NOT clear it (OR).
    upsert(mk(false)).await?;
    assert!(
        read().await?,
        "non-pruned re-upsert must not clear the marker (OR-on-conflict)"
    );

    // 3. Recovery SELECT carries it (the restore half lives in
    //    `from_recovery_row`).
    let recovered = db.load_nonterminal_derivations().await?;
    let row = recovered
        .iter()
        .find(|r| r.drv_hash == drv_hash)
        .expect("non-terminal row must be recovered");
    assert!(
        row.topdown_pruned,
        "recovery SELECT must carry topdown_pruned"
    );

    // 4. The same-tx helper clears the rows it is handed (test-only
    //    today: its merge-time production caller was replaced by the
    //    post-reconciliation clear pass — see the docstring above).
    let mut tx = db.pool().begin().await?;
    SchedulerDb::clear_topdown_pruned_for_parents(&mut tx, &[id]).await?;
    tx.commit().await?;
    assert!(
        !read().await?,
        "the helper must clear the marker for the ids it is given"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown+14]
/// `closure_hole` (`migrations/064`) column semantics: stamped only via
/// `set_closure_holes` (the leader's reap hook, the
/// recovery-time stamp in `load_dag_from_rows`, and the poison-clear
/// paths; merge upserts always bind false), preserved across a later
/// re-upsert by the
/// OR-on-conflict SET (a non-edge-declaring merge must not launder the
/// truncation evidence), carried by the recovery SELECT so a new leader
/// can restore it, cleared on its own by the merge-heal helper
/// `clear_closure_hole_by_hashes`, and dropped together with the mark
/// by the batched `clear_topdown_pruned_by_hashes` helper (whose
/// widened WHERE would also catch a markless leftover hole — not
/// exercised here) — while the single-row `clear_topdown_pruned_by_hash`
/// is mark-only: the topdown fail-fast consumes the mark but retains
/// the hole for the directed resubmit it solicits (bug_006/round-23).
#[tokio::test]
async fn closure_hole_or_on_conflict_clear_helpers_and_recovery() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv_hash = "closure-hole-test";
    let mk = |pruned: bool| DerivationRow {
        needs_resolve: false,
        drv_hash: drv_hash.into(),
        drv_path: rio_test_support::fixtures::test_drv_path(drv_hash),
        pname: Some("test-pkg".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![format!("/nix/store/{}-out", "b".repeat(32))],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        wanted_output_names: vec![],
        topdown_pruned: pruned,
        // What every production merge binds — the upsert is never a
        // stamping site for the breadcrumb.
        closure_hole: false,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
    };
    let upsert = async |row: DerivationRow| -> anyhow::Result<()> {
        let mut tx = db.pool().begin().await?;
        SchedulerDb::batch_upsert_derivations(&mut tx, &[row], &[]).await?;
        tx.commit().await?;
        Ok(())
    };
    let read = async || -> anyhow::Result<(bool, bool)> {
        Ok(sqlx::query_as(
            "SELECT topdown_pruned, closure_hole FROM derivations WHERE drv_hash = $1",
        )
        .bind(drv_hash)
        .fetch_one(&test_db.pool)
        .await?)
    };
    let hashes = vec![drv_hash.to_string()];
    let witness_rows = async || -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM derivation_closure_missing WHERE drv_hash = $1",
        )
        .bind(drv_hash)
        .fetch_one(&test_db.pool)
        .await?)
    };

    // 1. A pruned merge sets the mark but never the breadcrumb.
    upsert(mk(true)).await?;
    assert_eq!(
        read().await?,
        (true, false),
        "merge upserts must not stamp the breadcrumb"
    );

    // 2. The stamp helper (shared by the reap hook, the recovery-time
    //    stamp, and the poison-clear paths) sets it.
    assert_eq!(
        db.set_closure_holes(&[(hashes[0].clone(), vec!["m-child".into()])])
            .await?,
        1,
        "stamp helper must report the row it stamped"
    );
    assert_eq!(read().await?, (true, true));
    assert_eq!(
        witness_rows().await?,
        1,
        "the stamp writes its 069 witness row in the same transaction"
    );
    // A second truncation of the SAME parent appends its child instead
    // of being filtered by the bool (the dropped AND NOT closure_hole).
    db.set_closure_holes(&[(hashes[0].clone(), vec!["m-child-2".into()])])
        .await?;
    assert_eq!(
        witness_rows().await?,
        2,
        "a second truncation appends to the witness set"
    );

    // 3. A later re-upsert of the same drv (breadcrumb bound false, the
    //    merge-time bind) must clear neither bit: OR-on-conflict.
    upsert(mk(false)).await?;
    assert_eq!(
        read().await?,
        (true, true),
        "a re-upsert must not launder the breadcrumb or the mark (OR-on-conflict)"
    );

    // 4. Recovery SELECT carries it (the restore half lives in
    //    `from_recovery_row`).
    let recovered = db.load_nonterminal_derivations().await?;
    let row = recovered
        .iter()
        .find(|r| r.drv_hash == drv_hash)
        .expect("non-terminal row must be recovered");
    assert!(
        row.closure_hole && row.topdown_pruned,
        "recovery SELECT must carry closure_hole alongside topdown_pruned"
    );

    // 5. The merge-heal helper clears the breadcrumb but not the mark —
    //    and DELETEs the witness rows in the same transaction (a healed
    //    parent's stale missing-set must not poison the next hole's
    //    coverage check).
    assert_eq!(db.clear_closure_hole_by_hashes(&hashes).await?, 1);
    assert_eq!(
        read().await?,
        (true, false),
        "the heal clears only the breadcrumb"
    );
    assert_eq!(
        witness_rows().await?,
        0,
        "the heal deletes the 069 witness rows with the flag"
    );

    // 6. The extended batched mark clear drops both bits.
    db.set_closure_holes(&[(hashes[0].clone(), vec!["m-child".into()])])
        .await?;
    assert_eq!(db.clear_topdown_pruned_by_hashes(&hashes).await?, 1);
    assert_eq!(
        read().await?,
        (false, false),
        "clearing the mark must drop the breadcrumb that qualifies it"
    );
    assert_eq!(
        witness_rows().await?,
        0,
        "the batched mark clear deletes the 069 witness rows too"
    );

    // 7. The single-row clear is mark-only: it consumes the mark but
    //    deliberately retains the breadcrumb (the topdown fail-fast
    //    solicits a directed resubmit that needs the hole — see
    //    bug_006/round-23), and it does not mop up a markless leftover
    //    hole either (lost-heal residue waits for the next full-merge
    //    heal).
    upsert(mk(true)).await?;
    db.set_closure_holes(&[(hashes[0].clone(), vec!["m-child".into()])])
        .await?;
    assert_eq!(read().await?, (true, true));
    db.clear_topdown_pruned_by_hash(drv_hash).await?;
    assert_eq!(
        read().await?,
        (false, true),
        "the single-row clear must consume the mark and retain the breadcrumb"
    );
    assert_eq!(
        witness_rows().await?,
        1,
        "mark-only clear retains the witness set with the breadcrumb"
    );
    db.clear_topdown_pruned_by_hash(drv_hash).await?;
    assert_eq!(
        read().await?,
        (false, true),
        "a markless leftover hole is not the single-row clear's to reset"
    );

    Ok(())
}

/// The transaction-scoped paired writers: `set_closure_holes_tx` and
/// `clear_closure_holes_tx` keep the flag ⇔ witness-rows invariant in
/// a CALLER-owned transaction, in both directions, and a rolled-back
/// caller transaction leaves neither half behind (the property the
/// pool wrappers cannot exercise — their transaction is their own).
#[tokio::test]
async fn closure_hole_tx_writers_pair_flag_and_rows() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv_hash = "closure-hole-tx-test";
    let row = DerivationRow {
        needs_resolve: false,
        drv_hash: drv_hash.into(),
        drv_path: rio_test_support::fixtures::test_drv_path(drv_hash),
        pname: Some("test-pkg".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![format!("/nix/store/{}-out", "c".repeat(32))],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
    };
    {
        let mut tx = db.pool().begin().await?;
        SchedulerDb::batch_upsert_derivations(&mut tx, &[row], &[]).await?;
        tx.commit().await?;
    }
    let state = async || -> anyhow::Result<(bool, i64)> {
        let flag: bool =
            sqlx::query_scalar("SELECT closure_hole FROM derivations WHERE drv_hash = $1")
                .bind(drv_hash)
                .fetch_one(&test_db.pool)
                .await?;
        let rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM derivation_closure_missing WHERE drv_hash = $1",
        )
        .bind(drv_hash)
        .fetch_one(&test_db.pool)
        .await?;
        Ok((flag, rows))
    };
    let holes = vec![(drv_hash.to_string(), vec!["tx-child-a".to_string()])];
    let hashes = vec![drv_hash.to_string()];

    // Rolled-back set: NEITHER half lands.
    {
        let mut tx = db.pool().begin().await?;
        SchedulerDb::set_closure_holes_tx(&mut tx, &holes).await?;
        drop(tx); // rollback
    }
    assert_eq!(
        state().await?,
        (false, 0),
        "a rolled-back caller tx must leave neither the flag nor the rows"
    );

    // Committed set: BOTH halves land together.
    {
        let mut tx = db.pool().begin().await?;
        assert_eq!(SchedulerDb::set_closure_holes_tx(&mut tx, &holes).await?, 1);
        tx.commit().await?;
    }
    assert_eq!(state().await?, (true, 1));

    // Rolled-back clear: BOTH halves survive.
    {
        let mut tx = db.pool().begin().await?;
        SchedulerDb::clear_closure_holes_tx(&mut tx, &hashes).await?;
        drop(tx); // rollback
    }
    assert_eq!(
        state().await?,
        (true, 1),
        "a rolled-back caller tx must clear neither the flag nor the rows"
    );

    // Committed clear: BOTH halves drop together.
    {
        let mut tx = db.pool().begin().await?;
        assert_eq!(
            SchedulerDb::clear_closure_holes_tx(&mut tx, &hashes).await?,
            1
        );
        tx.commit().await?;
    }
    assert_eq!(state().await?, (false, 0));

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
            needs_resolve: false,
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
            drv_content: None,
            ca_modular_hash: None,
            ca_modular_hash_stripped: None,
            evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        })
        .collect();
    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows, &[]).await?;

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

/// Authoritative inline drv_content (content-bound hook fallback) must
/// round-trip through batch upsert, be refreshed by a later
/// authoritative re-upsert, be cleared by a later non-authoritative
/// re-upsert (last write wins), and come back from the recovery query.
// r[verify sched.recovery.inline-drv-durability+3]
#[tokio::test]
async fn test_batch_upsert_persists_authoritative_drv_content() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let row = |hash: &str, content: Option<Vec<u8>>| DerivationRow {
        needs_resolve: false,
        drv_hash: hash.into(),
        drv_path: format!("/nix/store/{}-{hash}.drv", "c".repeat(32)),
        pname: Some("hook-fallback".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: content,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let aterm = b"Derive([(\"out\",\"\",\"r:sha256\",\"\")],[],[],\"x86_64-linux\",\"/bin/sh\",[\"-c\",\"echo hi\"],[(\"out\",\"\")])".to_vec();

    // Insert: one authoritative, one ordinary.
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(
        &mut tx,
        &[row("hookdrv", Some(aterm.clone())), row("plaindrv", None)],
        &[],
    )
    .await?;
    tx.commit().await?;

    let fetch = |hash: &'static str| {
        let pool = test_db.pool.clone();
        async move {
            let (content,): (Option<Vec<u8>>,) =
                sqlx::query_as("SELECT drv_content FROM derivations WHERE drv_hash = $1")
                    .bind(hash)
                    .fetch_one(&pool)
                    .await?;
            anyhow::Ok(content)
        }
    };
    assert_eq!(fetch("hookdrv").await?, Some(aterm.clone()), "persisted");
    assert_eq!(fetch("plaindrv").await?, None, "ordinary node stays NULL");

    // A later authoritative upsert refreshes the bytes.
    let aterm2 = b"Derive([],[],[],\"x86_64-linux\",\"/bin/sh\",[],[])".to_vec();
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &[row("hookdrv", Some(aterm2.clone()))], &[])
        .await?;
    tx.commit().await?;
    assert_eq!(fetch("hookdrv").await?, Some(aterm2.clone()), "refreshed");

    // The recovery query returns the bytes (status 'created' is
    // non-terminal, so the row qualifies).
    let recovered = db.load_nonterminal_derivations().await?;
    let hook = recovered
        .iter()
        .find(|r| r.drv_hash == "hookdrv")
        .expect("hookdrv loaded");
    assert_eq!(hook.drv_content.as_deref(), Some(aterm2.as_slice()));

    // Last write wins: a later NON-authoritative submission of the same
    // drv_hash (e.g. another tenant's ordinary full-DAG build of the
    // same derivation, whose .drv is in the store) clears the persisted
    // blob — a previously-written authoritative copy can never leak
    // into, or outlive, an unrelated later submission.
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &[row("hookdrv", None)], &[]).await?;
    tx.commit().await?;
    assert_eq!(
        fetch("hookdrv").await?,
        None,
        "non-authoritative re-upsert clears persisted bytes (last write wins)"
    );
    let plain = recovered
        .iter()
        .find(|r| r.drv_hash == "plaindrv")
        .expect("plaindrv loaded");
    assert_eq!(plain.drv_content, None);

    Ok(())
}

/// Helper for the recreate-refresh tests: ratchet every live accumulator
/// column on a row, simulating poison/floor history written by their own
/// writers.
async fn ratchet_accumulators(pool: &sqlx::PgPool, drv_hash: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE derivations
         SET poisoned_at = now(), failed_builders = '{w1,w2}', retry_count = 2,
             resubmit_cycles = 3, floor_mem_bytes = 4096,
             floor_disk_bytes = 8192, floor_deadline_secs = 600
         WHERE drv_hash = $1",
    )
    .bind(drv_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Accumulator snapshot used by the recreate-refresh tests.
async fn fetch_accumulators(
    pool: &sqlx::PgPool,
    drv_hash: &str,
) -> anyhow::Result<(bool, Vec<String>, i32, i32, i64, i64, i64)> {
    Ok(sqlx::query_as(
        "SELECT poisoned_at IS NOT NULL, failed_builders, retry_count,
                resubmit_cycles, floor_mem_bytes, floor_disk_bytes,
                floor_deadline_secs
         FROM derivations WHERE drv_hash = $1",
    )
    .bind(drv_hash)
    .fetch_one(pool)
    .await?)
}

/// Re-creation persistence for SAME-definition shapes: a later
/// creation-scoped upsert of the same drv_hash refreshes the full
/// creation-time snapshot — pname, system, required_features, status, and
/// the declared `.drv` path — so a leader failover rebuilds the node from
/// the identity that won the merge. Live accumulators (poison/failure/
/// floor columns) keep their own writers and must NOT be touched when the
/// definition did not change: a store-origin row re-created store-backed,
/// and an authoritative row re-created with byte-identical content, both
/// preserve them (the definition-change reset is covered by
/// `test_batch_upsert_resets_accumulators_on_definition_change`).
// r[verify sched.persist.recreate-refresh+2]
#[tokio::test]
async fn test_batch_upsert_refreshes_identity_snapshot_not_accumulators() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // ── Store-origin row, re-created store-backed ─────────────────────
    // The prior creation parked in a terminal FAILURE state: that is the
    // only settled-adjacent state a conflicting re-creation can reach
    // (a completed/skipped row is frozen — sched.persist.settled-identity-freeze+3).
    let first = DerivationRow {
        needs_resolve: false,
        drv_hash: "recreate-store".into(),
        drv_path: format!("/nix/store/{}-recreate-store-old.drv", "d".repeat(32)),
        pname: Some("old".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Poisoned,
        required_features: vec!["kvm".into()],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &[first], &[]).await?;
    tx.commit().await?;
    ratchet_accumulators(&test_db.pool, "recreate-store").await?;

    // The re-creating submission declares a different identity AND a
    // different .drv path; both must win in the persisted snapshot.
    let new_path = format!("/nix/store/{}-recreate-store-new.drv", "e".repeat(32));
    let recreator = DerivationRow {
        needs_resolve: false,
        drv_hash: "recreate-store".into(),
        drv_path: new_path.clone(),
        pname: Some("new".into()),
        system: "aarch64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &[recreator], &[]).await?;
    tx.commit().await?;

    let (pname, system, status, features, drv_path): (
        Option<String>,
        String,
        String,
        Vec<String>,
        String,
    ) = sqlx::query_as(
        "SELECT pname, system, status, required_features, drv_path
             FROM derivations WHERE drv_hash = 'recreate-store'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(pname.as_deref(), Some("new"), "pname refreshed");
    assert_eq!(system, "aarch64-linux", "system refreshed");
    assert_eq!(status, "created", "status reset to the creation snapshot");
    assert!(features.is_empty(), "required_features refreshed");
    assert_eq!(
        drv_path, new_path,
        "drv_path refreshed to the re-creating submission's declared path"
    );

    let (has_poison, failed, retry_count, resubmit_cycles, floor_mem, floor_disk, floor_deadline) =
        fetch_accumulators(&test_db.pool, "recreate-store").await?;
    assert!(has_poison, "poisoned_at untouched by a same-origin upsert");
    assert_eq!(
        failed,
        vec!["w1".to_string(), "w2".to_string()],
        "failed_builders untouched"
    );
    assert_eq!(retry_count, 2, "retry_count untouched");
    assert_eq!(resubmit_cycles, 3, "resubmit_cycles untouched");
    assert_eq!(
        (floor_mem, floor_disk, floor_deadline),
        (4096, 8192, 600),
        "floor columns untouched"
    );
    // RETURNING carries the preserved floors so the I-208 hydration sees
    // them.
    let (_, returned_floor) = &id_map["recreate-store"];
    assert_eq!(returned_floor.mem_bytes, 4096);
    assert_eq!(returned_floor.disk_bytes, 8192);
    assert_eq!(returned_floor.deadline_secs, 600);

    // ── Authoritative row, re-created with byte-identical content ─────
    let auth = |status: DerivationStatus| DerivationRow {
        needs_resolve: false,
        drv_hash: "recreate-auth".into(),
        drv_path: format!("/nix/store/{}-recreate-auth.drv", "f".repeat(32)),
        pname: Some("hook".into()),
        system: "x86_64-linux".into(),
        status,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: Some(b"Derive-same".to_vec()),
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &[auth(DerivationStatus::Failed)], &[]).await?;
    tx.commit().await?;
    ratchet_accumulators(&test_db.pool, "recreate-auth").await?;

    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &[auth(DerivationStatus::Created)], &[]).await?;
    tx.commit().await?;

    let (has_poison, failed, retry_count, resubmit_cycles, floor_mem, _, _) =
        fetch_accumulators(&test_db.pool, "recreate-auth").await?;
    assert!(
        has_poison,
        "byte-identical authoritative re-creation keeps poisoned_at"
    );
    assert_eq!(failed.len(), 2, "failed_builders untouched");
    assert_eq!(retry_count, 2);
    assert_eq!(resubmit_cycles, 3, "resubmit budget keeps accumulating");
    assert_eq!(floor_mem, 4096, "floors preserved for the same definition");
    Ok(())
}

/// Definition-change accumulator reset (the in-tx half of
/// `sched.merge.displaced-failure-reset`): when the row's prior creation
/// persisted authoritative bytes and the incoming creation's content is
/// not byte-identical — a store-backed takeover (displacement, the
/// identity-matching resubmit takeover, or a re-creation of a reaped
/// authoritative row) or a byte-different authoritative displacement —
/// the upsert itself zeroes every failure-derived column in the same
/// statement, refreshes the identity snapshot (including `drv_path`), and
/// RETURNING already reports the reset floors so the I-208 hydration
/// cannot resurrect the prior definition's sizing.
// r[verify sched.merge.displaced-failure-reset+2]
// r[verify sched.persist.recreate-refresh+2]
#[tokio::test]
async fn test_batch_upsert_resets_accumulators_on_definition_change() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let auth_row = |hash: &str, content: &[u8]| DerivationRow {
        needs_resolve: false,
        drv_hash: hash.into(),
        drv_path: format!("/nix/store/{}-{hash}-squat.drv", "a".repeat(32)),
        pname: Some("squat".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Poisoned,
        required_features: vec!["kvm".into()],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: Some(content.to_vec()),
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };

    // ── (a) authoritative squat → store-backed re-creation ────────────
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(
        &mut tx,
        &[auth_row("defchange-store", b"Derive-squat")],
        &[],
    )
    .await?;
    tx.commit().await?;
    ratchet_accumulators(&test_db.pool, "defchange-store").await?;

    let victim_path = format!("/nix/store/{}-defchange-store.drv", "b".repeat(32));
    let store_backed = DerivationRow {
        needs_resolve: false,
        drv_hash: "defchange-store".into(),
        drv_path: victim_path.clone(),
        pname: Some("victim".into()),
        system: "aarch64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &[store_backed], &[]).await?;
    tx.commit().await?;

    let (has_poison, failed, retry_count, resubmit_cycles, floor_mem, floor_disk, floor_deadline) =
        fetch_accumulators(&test_db.pool, "defchange-store").await?;
    assert!(!has_poison, "poisoned_at cleared on definition change");
    assert!(failed.is_empty(), "failed_builders cleared");
    assert_eq!(retry_count, 0, "retry_count reset");
    assert_eq!(resubmit_cycles, 0, "poison-resubmit budget starts fresh");
    assert_eq!(
        (floor_mem, floor_disk, floor_deadline),
        (0, 0, 0),
        "reactive floors reset"
    );
    let (system, drv_path, content): (String, String, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT system, drv_path, drv_content FROM derivations WHERE drv_hash = 'defchange-store'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(system, "aarch64-linux", "identity refreshed");
    assert_eq!(drv_path, victim_path, "squatter's decoy path replaced");
    assert_eq!(content, None, "authoritative bytes cleared");
    // RETURNING reflects the reset, so the in-memory hydration of the
    // displacing node sees zeros without any special-casing.
    let (_, returned_floor) = &id_map["defchange-store"];
    assert_eq!(returned_floor.mem_bytes, 0);
    assert_eq!(returned_floor.disk_bytes, 0);
    assert_eq!(returned_floor.deadline_secs, 0);

    // ── (b) authoritative squat → byte-different authoritative ────────
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(
        &mut tx,
        &[auth_row("defchange-auth", b"Derive-squat")],
        &[],
    )
    .await?;
    tx.commit().await?;
    ratchet_accumulators(&test_db.pool, "defchange-auth").await?;

    let mut victim = auth_row("defchange-auth", b"Derive-victim");
    victim.pname = Some("victim".into());
    victim.status = DerivationStatus::Created;
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &[victim], &[]).await?;
    tx.commit().await?;

    let (has_poison, failed, retry_count, resubmit_cycles, floor_mem, _, _) =
        fetch_accumulators(&test_db.pool, "defchange-auth").await?;
    assert!(
        !has_poison,
        "byte-different authoritative re-creation resets"
    );
    assert!(failed.is_empty());
    assert_eq!(retry_count, 0);
    assert_eq!(resubmit_cycles, 0);
    assert_eq!(floor_mem, 0);
    let (content,): (Option<Vec<u8>>,) =
        sqlx::query_as("SELECT drv_content FROM derivations WHERE drv_hash = 'defchange-auth'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        content.as_deref(),
        Some(b"Derive-victim".as_slice()),
        "incoming bytes persisted"
    );
    Ok(())
}

// r[verify sched.persist.atomic-activation+2]
/// The merge-time persist transaction (derivation upsert + build links +
/// the build's Pending→Active update) is one commit point: dropping the
/// transaction before commit leaves a pre-existing row's recreate-refresh
/// non-durable and the builds row still `pending`/`started_at IS NULL`;
/// committing the same statements makes the refresh and the activation
/// durable together.
#[tokio::test]
async fn test_merge_persist_tx_is_single_commit_point() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Pre-existing terminal-FAILURE authoritative squat row (a prior
    // creation's snapshot that a displacing merge would recreate-refresh
    // — only failure-parked rows are displaceable; completed/skipped
    // rows are frozen by sched.persist.settled-identity-freeze+3).
    let squat = DerivationRow {
        needs_resolve: false,
        drv_hash: "atomic-squat".into(),
        drv_path: format!("/nix/store/{}-atomic-squat.drv", "a".repeat(32)),
        pname: Some("squat".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Poisoned,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: Some(b"Derive-squat".to_vec()),
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, std::slice::from_ref(&squat), &[]).await?;
    tx.commit().await?;

    // Pending build that will perform the displacing merge.
    let build_id = Uuid::new_v4();
    db.insert_build(
        build_id,
        None,
        crate::state::PriorityClass::Scheduled,
        false,
        &crate::state::BuildOptions::default(),
        None,
    )
    .await?;

    let displacer = DerivationRow {
        needs_resolve: false,
        drv_hash: "atomic-squat".into(),
        drv_path: format!("/nix/store/{}-atomic-squat.drv", "a".repeat(32)),
        pname: Some("victim".into()),
        system: "aarch64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };

    // Run the merge-persist statement set in one tx, then DROP it
    // (simulated failure before the single commit point).
    {
        let mut tx = db.pool().begin().await?;
        let id_map =
            SchedulerDb::batch_upsert_derivations(&mut tx, std::slice::from_ref(&displacer), &[])
                .await?;
        let db_ids: Vec<Uuid> = id_map.values().map(|(id, _)| *id).collect();
        SchedulerDb::batch_insert_build_derivations(&mut tx, build_id, &db_ids).await?;
        SchedulerDb::update_build_status_tx(
            &mut tx,
            build_id,
            crate::state::BuildState::Active,
            None,
        )
        .await?;
        drop(tx);
    }

    let (system, status, content): (String, String, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT system, status, drv_content FROM derivations WHERE drv_hash = 'atomic-squat'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(system, "x86_64-linux", "squat identity untouched on drop");
    assert_eq!(status, "poisoned", "squat status untouched on drop");
    assert_eq!(
        content.as_deref(),
        Some(b"Derive-squat".as_slice()),
        "authoritative bytes untouched on drop"
    );
    let (b_status, started): (String, Option<f64>) = sqlx::query_as(
        "SELECT status, EXTRACT(EPOCH FROM started_at)::float8 FROM builds WHERE build_id = $1",
    )
    .bind(build_id)
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(b_status, "pending", "build not activated by a dropped tx");
    assert!(started.is_none(), "started_at not set by a dropped tx");
    let (links,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM build_derivations WHERE build_id = $1")
            .bind(build_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(links, 0, "no links survive a dropped tx");

    // Same statements, committed → recreate-refresh, links, and the
    // activation become durable together.
    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &[displacer], &[]).await?;
    let db_ids: Vec<Uuid> = id_map.values().map(|(id, _)| *id).collect();
    SchedulerDb::batch_insert_build_derivations(&mut tx, build_id, &db_ids).await?;
    SchedulerDb::update_build_status_tx(&mut tx, build_id, crate::state::BuildState::Active, None)
        .await?;
    tx.commit().await?;

    let (system, b_status, started): (String, String, Option<f64>) = sqlx::query_as(
        "SELECT d.system, b.status, EXTRACT(EPOCH FROM b.started_at)::float8
         FROM derivations d, builds b
         WHERE d.drv_hash = 'atomic-squat' AND b.build_id = $1",
    )
    .bind(build_id)
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(system, "aarch64-linux", "recreate-refresh committed");
    assert_eq!(b_status, "active", "build activated in the same commit");
    assert!(
        started.is_some(),
        "started_at set by the in-tx Active update"
    );
    let (links,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM build_derivations WHERE build_id = $1")
            .bind(build_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(links, 1, "link committed with the activation");
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// `delete_displaced_build_links` removes exactly the passed prior builds'
/// links to the displaced derivation — the displacing build's link and
/// other derivations' links are untouched.
#[tokio::test]
async fn test_delete_displaced_build_links_scoped() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let displaced_id = insert_test_derivation(&db, "displaced-links").await?;
    let other_id = insert_test_derivation(&db, "other-links").await?;

    let squatter = Uuid::new_v4();
    let joiner = Uuid::new_v4();
    let displacer = Uuid::new_v4();
    for b in [squatter, joiner, displacer] {
        db.insert_build(
            b,
            None,
            crate::state::PriorityClass::Scheduled,
            false,
            &crate::state::BuildOptions::default(),
            None,
        )
        .await?;
    }

    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_insert_build_derivations(&mut tx, squatter, &[displaced_id]).await?;
    SchedulerDb::batch_insert_build_derivations(&mut tx, joiner, &[displaced_id, other_id]).await?;
    SchedulerDb::batch_insert_build_derivations(&mut tx, displacer, &[displaced_id]).await?;
    tx.commit().await?;

    // Prune the squatter's and joiner's links to the displaced derivation.
    let mut tx = db.pool().begin().await?;
    let pruned = SchedulerDb::delete_displaced_build_links(
        &mut tx,
        displaced_id,
        &[squatter, joiner],
        false,
    )
    .await?;
    tx.commit().await?;
    assert_eq!(pruned, 2, "exactly the two prior builds' links removed");

    let remaining: Vec<Uuid> =
        sqlx::query_scalar("SELECT build_id FROM build_derivations WHERE derivation_id = $1")
            .bind(displaced_id)
            .fetch_all(&test_db.pool)
            .await?;
    assert_eq!(remaining, vec![displacer], "displacer keeps its link");

    let other_links: Vec<Uuid> =
        sqlx::query_scalar("SELECT build_id FROM build_derivations WHERE derivation_id = $1")
            .bind(other_id)
            .fetch_all(&test_db.pool)
            .await?;
    assert_eq!(
        other_links,
        vec![joiner],
        "links to other derivations are untouched"
    );

    // Empty prior set is a no-op (no statement issued).
    let mut tx = db.pool().begin().await?;
    let pruned =
        SchedulerDb::delete_displaced_build_links(&mut tx, displaced_id, &[], true).await?;
    tx.commit().await?;
    assert_eq!(pruned, 0);
    Ok(())
}

// r[verify sched.merge.authoritative-conflict+6]
/// `delete_displaced_build_links` decrements the pruned builds'
/// `builds.total_drvs` in the same statement when (and only when) the
/// caller asks for it — i.e. when the displaced result had not been
/// received — and never drives a total negative.
#[tokio::test]
async fn test_delete_displaced_build_links_adjusts_total_only_when_requested() -> anyhow::Result<()>
{
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let displaced_id = insert_test_derivation(&db, "displaced-total").await?;
    let other_id = insert_test_derivation(&db, "other-total").await?;

    // Two prior builds: one whose total is adjusted, one pruned without
    // adjustment (its result was already received), plus a zero-total
    // build pinning the GREATEST clamp.
    let pruned_pending = Uuid::new_v4();
    let pruned_credited = Uuid::new_v4();
    let zero_total = Uuid::new_v4();
    for b in [pruned_pending, pruned_credited, zero_total] {
        db.insert_build(
            b,
            None,
            crate::state::PriorityClass::Scheduled,
            false,
            &crate::state::BuildOptions::default(),
            None,
        )
        .await?;
    }
    db.persist_build_counts(pruned_pending, 2, 1, 0).await?;
    db.persist_build_counts(pruned_credited, 2, 2, 0).await?;
    db.persist_build_counts(zero_total, 0, 0, 0).await?;

    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_insert_build_derivations(&mut tx, pruned_pending, &[displaced_id, other_id])
        .await?;
    SchedulerDb::batch_insert_build_derivations(&mut tx, pruned_credited, &[displaced_id]).await?;
    SchedulerDb::batch_insert_build_derivations(&mut tx, zero_total, &[displaced_id]).await?;
    tx.commit().await?;

    let totals = |pool: sqlx::PgPool| async move {
        let rows: Vec<(Uuid, i32)> =
            sqlx::query_as("SELECT build_id, total_drvs FROM builds ORDER BY build_id")
                .fetch_all(&pool)
                .await?;
        anyhow::Ok(
            rows.into_iter()
                .collect::<std::collections::HashMap<_, _>>(),
        )
    };

    // adjust_total = true: the not-yet-received prune decrements totals
    // for exactly the pruned builds (clamped at zero).
    let mut tx = db.pool().begin().await?;
    let pruned = SchedulerDb::delete_displaced_build_links(
        &mut tx,
        displaced_id,
        &[pruned_pending, zero_total],
        true,
    )
    .await?;
    tx.commit().await?;
    assert_eq!(pruned, 2);
    let after = totals(test_db.pool.clone()).await?;
    assert_eq!(
        after[&pruned_pending], 1,
        "total decremented with the prune"
    );
    assert_eq!(after[&zero_total], 0, "zero total clamped, not negative");
    assert_eq!(after[&pruned_credited], 2, "unpruned build untouched");

    // adjust_total = false: the already-received prune deletes the link
    // but keeps the persisted total (the build keeps the credit).
    let mut tx = db.pool().begin().await?;
    let pruned =
        SchedulerDb::delete_displaced_build_links(&mut tx, displaced_id, &[pruned_credited], false)
            .await?;
    tx.commit().await?;
    assert_eq!(pruned, 1);
    let after = totals(test_db.pool.clone()).await?;
    assert_eq!(
        after[&pruned_credited], 2,
        "credited prune keeps the persisted total"
    );
    // Links to other derivations are never touched by either form.
    let other_links: Vec<Uuid> =
        sqlx::query_scalar("SELECT build_id FROM build_derivations WHERE derivation_id = $1")
            .bind(other_id)
            .fetch_all(&test_db.pool)
            .await?;
    assert_eq!(other_links, vec![pruned_pending]);
    Ok(())
}

// r[verify sched.persist.ca-modular-hash+2]
/// The CA modular hash rides the creation-time snapshot: persisted on
/// insert (for CA and deferred-IA rows alike), refreshed by a later
/// (re)creation, cleared when the (re)creating submission carries none,
/// and returned by both recovery queries.
#[tokio::test]
async fn test_batch_upsert_persists_and_refreshes_ca_modular_hash() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let row = |hash: &str, ca_hash: Option<[u8; 32]>| DerivationRow {
        needs_resolve: false,
        drv_hash: hash.into(),
        drv_path: format!("/nix/store/{}-{hash}.drv", "d".repeat(32)),
        pname: Some("ca-evidence".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: None,
        ca_modular_hash: ca_hash,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let fetch = |hash: &'static str| {
        let pool = test_db.pool.clone();
        async move {
            let (h,): (Option<Vec<u8>>,) =
                sqlx::query_as("SELECT ca_modular_hash FROM derivations WHERE drv_hash = $1")
                    .bind(hash)
                    .fetch_one(&pool)
                    .await?;
            anyhow::Ok(h)
        }
    };

    // Insert: one with evidence, one without.
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(
        &mut tx,
        &[row("ca-evi", Some([7u8; 32])), row("ca-bare", None)],
        &[],
    )
    .await?;
    tx.commit().await?;
    assert_eq!(fetch("ca-evi").await?, Some(vec![7u8; 32]), "persisted");
    assert_eq!(fetch("ca-bare").await?, None, "no evidence stays NULL");

    // A later (re)creation refreshes the value.
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &[row("ca-evi", Some([9u8; 32]))], &[]).await?;
    tx.commit().await?;
    assert_eq!(fetch("ca-evi").await?, Some(vec![9u8; 32]), "refreshed");

    // Both recovery queries return it.
    let recovered = db.load_nonterminal_derivations().await?;
    let evi = recovered
        .iter()
        .find(|r| r.drv_hash == "ca-evi")
        .expect("ca-evi loaded");
    assert_eq!(evi.ca_modular_hash.as_deref(), Some([9u8; 32].as_slice()));
    sqlx::query(
        "UPDATE derivations SET status = 'poisoned', poisoned_at = now() WHERE drv_hash = 'ca-evi'",
    )
    .execute(&test_db.pool)
    .await?;
    let poisoned = db.load_poisoned_derivations().await?;
    let evi = poisoned
        .iter()
        .find(|r| r.base.drv_hash == "ca-evi")
        .expect("ca-evi loaded poisoned");
    assert_eq!(
        evi.base.ca_modular_hash.as_deref(),
        Some([9u8; 32].as_slice())
    );

    // A (re)creation without evidence clears it (last write wins, same
    // as the rest of the snapshot).
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &[row("ca-evi", None)], &[]).await?;
    tx.commit().await?;
    assert_eq!(fetch("ca-evi").await?, None, "cleared when absent");

    // Deferred-IA shape (is_ca=false, unknown output path, hash present —
    // the gateway populates it for these too): persisted and returned by
    // the recovery query exactly like a CA row.
    let deferred = DerivationRow {
        is_ca: false,
        expected_output_paths: vec![String::new()],
        ..row("ia-deferred-evi", Some([3u8; 32]))
    };
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &[deferred], &[]).await?;
    tx.commit().await?;
    assert_eq!(
        fetch("ia-deferred-evi").await?,
        Some(vec![3u8; 32]),
        "deferred-IA rows persist the hash"
    );
    let recovered = db.load_nonterminal_derivations().await?;
    let deferred_row = recovered
        .iter()
        .find(|r| r.drv_hash == "ia-deferred-evi")
        .expect("deferred-IA row loaded");
    assert_eq!(
        deferred_row.ca_modular_hash.as_deref(),
        Some([3u8; 32].as_slice()),
        "recovery query returns the deferred-IA hash"
    );
    assert!(!deferred_row.is_ca);
    Ok(())
}

// r[verify sched.persist.settled-identity-freeze+3]
/// M_070 preservation-column write semantics, all three writers:
/// the creation upsert (insert + supersede-vs-carry on conflict) and
/// the dispatch strip mover (single-statement live→stripped move,
/// idempotent on re-strip).
#[tokio::test]
async fn test_preserved_stripped_hash_supersede_carry_and_move() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let row = |live: Option<[u8; 32]>, stripped: Option<[u8; 32]>| DerivationRow {
        needs_resolve: false,
        drv_hash: "strip-sem".into(),
        drv_path: format!("/nix/store/{}-strip-sem.drv", "d".repeat(32)),
        pname: Some("strip-sem".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: true,
        drv_content: None,
        ca_modular_hash: live,
        ca_modular_hash_stripped: stripped,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let fetch = || {
        let pool = test_db.pool.clone();
        async move {
            let r: (Option<Vec<u8>>, Option<Vec<u8>>) = sqlx::query_as(
                "SELECT ca_modular_hash, ca_modular_hash_stripped \
                 FROM derivations WHERE drv_hash = 'strip-sem'",
            )
            .fetch_one(&pool)
            .await?;
            anyhow::Ok(r)
        }
    };
    let upsert = |r: DerivationRow| {
        let db = db.clone();
        async move {
            let mut tx = db.pool().begin().await?;
            SchedulerDb::batch_upsert_derivations(&mut tx, &[r], &[]).await?;
            tx.commit().await?;
            anyhow::Ok(())
        }
    };

    // Insert with an ingress-stripped claim: preserved, live NULL.
    upsert(row(None, Some([0xAA; 32]))).await?;
    assert_eq!(
        fetch().await?,
        (None, Some(vec![0xAA; 32])),
        "preserved at insert"
    );

    // Bare re-creation (no live, no stripped): the older preserved
    // claim is CARRIED FORWARD — same drv_hash means the same text-CA
    // definition, so the prior claim is still about this row.
    upsert(row(None, None)).await?;
    assert_eq!(
        fetch().await?,
        (None, Some(vec![0xAA; 32])),
        "bare re-creation carries the preserved claim forward"
    );

    // Re-creation with a NEW stripped claim: overwritten.
    upsert(row(None, Some([0xBB; 32]))).await?;
    assert_eq!(
        fetch().await?,
        (None, Some(vec![0xBB; 32])),
        "newer stripped claim wins"
    );

    // Re-creation with a LIVE (verifiable) hash: strictly better
    // evidence — the preserved unverified claim is SUPERSEDED to NULL.
    upsert(row(Some([7u8; 32]), None)).await?;
    assert_eq!(
        fetch().await?,
        (Some(vec![7u8; 32]), None),
        "live hash supersedes the preserved claim"
    );

    // Dispatch strip mover: live → stripped in one statement.
    db.persist_evidence_rank_and_strip_modular_hash(
        "strip-sem",
        crate::state::DefinitionEvidence::PathBoundBytes,
        None,
    )
    .await?;
    assert_eq!(
        fetch().await?,
        (None, Some(vec![7u8; 32])),
        "strip mover moves the live value into preservation"
    );
    // Idempotent re-strip: live already NULL — preserved value KEPT
    // (COALESCE), never zeroed by a second strip.
    db.persist_evidence_rank_and_strip_modular_hash(
        "strip-sem",
        crate::state::DefinitionEvidence::PathBoundBytes,
        None,
    )
    .await?;
    assert_eq!(
        fetch().await?,
        (None, Some(vec![7u8; 32])),
        "re-strip keeps the preserved value"
    );
    Ok(())
}

// r[verify sched.persist.settled-identity-freeze+3]
/// The upsert's settled-row WHERE guard (defense-in-depth twin of the
/// pre-merge check): a `completed`/`skipped` row whose public identity
/// conflicts with the incoming re-creation is left completely untouched
/// — and excluded from RETURNING, so the caller fails loudly instead of
/// silently rewriting settled history. A matching-identity re-creation
/// updates normally (legitimate rebuild after store GC).
#[tokio::test]
async fn settled_row_upsert_guard_preserves_identity_and_content() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let settled_path = format!("/nix/store/{}-settled-guard.drv", "a".repeat(32));
    let settled = DerivationRow {
        needs_resolve: false,
        drv_hash: "settled-guard".into(),
        drv_path: settled_path.clone(),
        pname: Some("victim".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Completed,
        required_features: vec![],
        expected_output_paths: vec![format!("/nix/store/{}-settled-guard-out", "b".repeat(32))],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        drv_content: Some(b"Derive-victim".to_vec()),
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    let ids =
        SchedulerDb::batch_upsert_derivations(&mut tx, std::slice::from_ref(&settled), &[]).await?;
    tx.commit().await?;
    assert!(ids.contains_key("settled-guard"), "initial insert returned");

    // ── Conflicting re-creation: different system + output names ──────
    let conflicting = DerivationRow {
        needs_resolve: false,
        drv_hash: "settled-guard".into(),
        drv_path: format!("/nix/store/{}-settled-guard-evil.drv", "c".repeat(32)),
        pname: Some("attacker".into()),
        system: "aarch64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into(), "dev".into()],
        is_fixed_output: false,
        is_ca: false,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    let ids = SchedulerDb::batch_upsert_derivations(&mut tx, &[conflicting], &[]).await?;
    tx.commit().await?;
    // The guarded row is NOT in RETURNING — the caller (merge persist)
    // surfaces this as MissingDbId instead of corrupting the row.
    assert!(
        !ids.contains_key("settled-guard"),
        "conflicting re-creation must not update (or return) the settled row"
    );

    // The settled row is byte-for-byte what it was.
    let (pname, system, status): (Option<String>, String, String) = sqlx::query_as(
        "SELECT pname, system, status FROM derivations WHERE drv_hash = 'settled-guard'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(pname.as_deref(), Some("victim"));
    assert_eq!(system, "x86_64-linux");
    assert_eq!(status, "completed");
    let (drv_path, content, names): (String, Option<Vec<u8>>, Vec<String>) = sqlx::query_as(
        "SELECT drv_path, drv_content, output_names
             FROM derivations WHERE drv_hash = 'settled-guard'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(drv_path, settled_path);
    assert_eq!(content.as_deref(), Some(b"Derive-victim".as_slice()));
    assert_eq!(names, vec!["out".to_string()]);

    // ── Matching re-creation: same public identity → updates normally ─
    let matching = DerivationRow {
        needs_resolve: false,
        drv_hash: "settled-guard".into(),
        drv_path: settled_path.clone(),
        pname: Some("victim".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![format!("/nix/store/{}-settled-guard-out", "b".repeat(32))],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    let ids = SchedulerDb::batch_upsert_derivations(&mut tx, &[matching], &[]).await?;
    tx.commit().await?;
    assert!(
        ids.contains_key("settled-guard"),
        "matching re-creation updates the settled row (legitimate rebuild)"
    );
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'settled-guard'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(status, "created", "matching rebuild re-created the row");
    Ok(())
}

// r[verify sched.merge.store-evidence-displacement+3]
// r[verify sched.persist.settled-identity-freeze+3]
/// The settled-row WHERE guard's evidence carve-out: a conflicting
/// re-creation whose hash is in the per-merge approved array (the
/// actor's store-evidence verdict) updates the settled row — and an
/// identical re-creation WITHOUT the approval is still refused, pinning
/// that the carve-out is scoped to the array, not a weakening of the
/// guard.
#[tokio::test]
async fn settled_row_upsert_guard_admits_evidence_approved_hash() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let settled = DerivationRow {
        needs_resolve: false,
        drv_hash: "evidence-carveout".into(),
        drv_path: format!("/nix/store/{}-evidence-carveout.drv", "a".repeat(32)),
        pname: Some("squat".into()),
        system: "aarch64-linux".into(),
        status: DerivationStatus::Completed,
        required_features: vec![],
        expected_output_paths: vec![format!("/nix/store/{}-squat-out", "b".repeat(32))],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        drv_content: Some(b"Derive-squat".to_vec()),
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::ContentBoundClaim,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, std::slice::from_ref(&settled), &[]).await?;
    tx.commit().await?;

    let genuine = DerivationRow {
        needs_resolve: false,
        drv_hash: "evidence-carveout".into(),
        drv_path: format!("/nix/store/{}-evidence-carveout.drv", "a".repeat(32)),
        pname: Some("victim".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![format!("/nix/store/{}-victim-out", "c".repeat(32))],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: crate::state::DefinitionEvidence::PathBoundBytes,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
    };

    // Without the approval: the guard refuses (scoped carve-out, not a
    // weakening).
    let mut tx = db.pool().begin().await?;
    let ids =
        SchedulerDb::batch_upsert_derivations(&mut tx, std::slice::from_ref(&genuine), &[]).await?;
    tx.commit().await?;
    assert!(
        !ids.contains_key("evidence-carveout"),
        "unapproved conflicting re-creation must still be refused"
    );

    // With the approval: the row is rewritten to the verified identity.
    let mut tx = db.pool().begin().await?;
    let ids = SchedulerDb::batch_upsert_derivations(
        &mut tx,
        std::slice::from_ref(&genuine),
        &["evidence-carveout".to_string()],
    )
    .await?;
    tx.commit().await?;
    assert!(
        ids.contains_key("evidence-carveout"),
        "evidence-approved re-creation updates the settled row"
    );
    let (system, status, rank): (String, String, String) = sqlx::query_as(
        "SELECT system, status, evidence_rank FROM derivations \
         WHERE drv_hash = 'evidence-carveout'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(system, "x86_64-linux", "identity rewritten to the victim's");
    assert_eq!(
        status, "created",
        "settled status replaced by the fresh lifecycle"
    );
    assert_eq!(rank, "path_bound_bytes", "verified rank persisted");
    Ok(())
}

// r[verify sched.derivation.evidence-rank]
/// `evidence_rank` PG round-trip per variant, and the deliberate
/// creation-snapshot (EXCLUDED, NOT MAX) conflict semantics: a
/// re-creation starts a new lifecycle at its own ingress rank, so a
/// lower-ranked re-creation overwrites a higher persisted rank. The
/// recovery loader returns the column verbatim.
#[tokio::test]
async fn test_batch_upsert_evidence_rank_roundtrip_and_recreation() -> anyhow::Result<()> {
    use crate::state::DefinitionEvidence;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let row = |hash: &str, rank: DefinitionEvidence| DerivationRow {
        needs_resolve: false,
        drv_hash: hash.into(),
        drv_path: format!("/nix/store/{}-{hash}.drv", "e".repeat(32)),
        pname: Some("evidence-rank".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![format!("/nix/store/{}-{hash}-out", "f".repeat(32))],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
        wanted_output_names: vec![],
        topdown_pruned: false,
        closure_hole: false,
        drv_content: None,
        ca_modular_hash: None,
        ca_modular_hash_stripped: None,
        evidence_rank: rank,
    };

    // Round-trip every variant through the upsert + the recovery SELECT.
    let variants = [
        ("ev-unverified", DefinitionEvidence::UnverifiedClaim),
        ("ev-content", DefinitionEvidence::ContentBoundClaim),
        ("ev-pathbound", DefinitionEvidence::PathBoundBytes),
        ("ev-verified", DefinitionEvidence::VerifiedBuilt),
    ];
    let rows: Vec<DerivationRow> = variants.iter().map(|(h, r)| row(h, *r)).collect();
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(&mut tx, &rows, &[]).await?;
    tx.commit().await?;

    let recovered = db.load_nonterminal_derivations().await?;
    for (hash, rank) in &variants {
        let rec = recovered
            .iter()
            .find(|r| r.drv_hash == *hash)
            .expect("recovery loader returns the row");
        assert_eq!(
            rec.evidence_rank,
            rank.as_str(),
            "verbatim round-trip for {hash}"
        );
    }

    // The runtime upgrade writer is what settle/dispatch use.
    db.persist_evidence_rank("ev-unverified", DefinitionEvidence::PathBoundBytes, None)
        .await?;
    let (rank,): (String,) =
        sqlx::query_as("SELECT evidence_rank FROM derivations WHERE drv_hash = 'ev-unverified'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(rank, "path_bound_bytes", "runtime writer persists upgrades");

    // M_071 COALESCE tri-state on the same writer (round-16 bug_053):
    // Some sets the flag with the rank in one statement; None (the
    // settle chokepoint's shape) leaves whatever is there untouched.
    let flag = || async {
        let (f,): (Option<bool>,) = sqlx::query_as(
            "SELECT needs_resolve FROM derivations WHERE drv_hash = 'ev-unverified'",
        )
        .fetch_one(&test_db.pool)
        .await?;
        anyhow::Ok(f)
    };
    assert_eq!(
        flag().await?,
        Some(false),
        "creation upsert binds the merge-time flag (non-NULL from birth)"
    );
    db.persist_evidence_rank(
        "ev-unverified",
        DefinitionEvidence::PathBoundBytes,
        Some(true),
    )
    .await?;
    assert_eq!(
        flag().await?,
        Some(true),
        "Some(_) writes the byte-derived flag"
    );
    db.persist_evidence_rank("ev-unverified", DefinitionEvidence::VerifiedBuilt, None)
        .await?;
    assert_eq!(
        flag().await?,
        Some(true),
        "None (settle) leaves the persisted flag untouched (COALESCE)"
    );

    // Re-creation applies EXCLUDED (creation-snapshot) semantics —
    // deliberately NOT MAX: the verified_built row re-created by a
    // store-backed submission starts its new lifecycle at the floor.
    let mut tx = db.pool().begin().await?;
    SchedulerDb::batch_upsert_derivations(
        &mut tx,
        &[row("ev-verified", DefinitionEvidence::UnverifiedClaim)],
        &[],
    )
    .await?;
    tx.commit().await?;
    let (rank,): (String,) =
        sqlx::query_as("SELECT evidence_rank FROM derivations WHERE drv_hash = 'ev-verified'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        rank, "unverified_claim",
        "re-creation is a new lifecycle at its own ingress rank (EXCLUDED, not MAX)"
    );

    // The CHECK constraint rejects out-of-lattice values at the SQL
    // boundary (defense-in-depth under the Rust-side codec).
    let res =
        sqlx::query("UPDATE derivations SET evidence_rank = 'bogus' WHERE drv_hash = 'ev-content'")
            .execute(&test_db.pool)
            .await;
    assert!(res.is_err(), "CHECK constraint rejects unknown ranks");
    Ok(())
}

// r[verify sched.persist.settled-identity-freeze+3]
/// Round-16 merged_bug_087: AXIS-ISOLATED DIFFERENTIAL CONFORMANCE
/// between the in-memory settled matcher
/// (`actor::settled::settled_row_identity_matches`) and the SQL freeze
/// guard in `batch_upsert_derivations`. For every single-axis mutation
/// of an incoming re-creation against a settled baseline row, both
/// implementations must produce the SAME verdict: matcher-match ⇔
/// guard-admits (row appears in RETURNING). Pre-fix divergences pinned:
/// reordered output names (matcher matched, guard blocked → opaque
/// Internal for a legitimate set-equal resubmission) and the missing
/// expected-path / differing-live-hash axes (matcher conflicted, guard
/// silently overwrote settled history in exactly the bypass/race
/// window it exists for). The in-memory side reads the row through the
/// PRODUCTION loader (`load_settled_identity_rows`), so a loader
/// column omission also fails this test.
#[tokio::test]
async fn test_settled_freeze_guard_matches_matcher_axis_by_axis() -> anyhow::Result<()> {
    use crate::actor::settled::settled_row_identity_matches;
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let dev_path = "/nix/store/00000000000000000000000000000000-axis-dev";
    let out_path = "/nix/store/11111111111111111111111111111111-axis-out";

    struct Case {
        label: &'static str,
        // Mutations applied to the incoming re-creation.
        names: Vec<String>,
        paths: Vec<String>,
        system: &'static str,
        is_fixed_output: bool,
        is_ca: bool,
        incoming_hash: Option<[u8; 32]>,
        // Hash staged on the settled ROW (None = baseline no-hash).
        row_hash: Option<[u8; 32]>,
        expect_match: bool,
    }
    let base_names = || vec!["dev".to_string(), "out".to_string()];
    let base_paths = || vec![dev_path.to_string(), out_path.to_string()];
    let cases = vec![
        Case {
            label: "identical",
            names: base_names(),
            paths: base_paths(),
            system: "x86_64-linux",
            is_fixed_output: false,
            is_ca: false,
            incoming_hash: None,
            row_hash: None,
            expect_match: true,
        },
        Case {
            label: "names-reordered-set-equal",
            names: vec!["out".into(), "dev".into()],
            paths: vec![out_path.into(), dev_path.into()],
            system: "x86_64-linux",
            is_fixed_output: false,
            is_ca: false,
            incoming_hash: None,
            row_hash: None,
            expect_match: true,
        },
        Case {
            label: "system-differs",
            names: base_names(),
            paths: base_paths(),
            system: "aarch64-linux",
            is_fixed_output: false,
            is_ca: false,
            incoming_hash: None,
            row_hash: None,
            expect_match: false,
        },
        Case {
            label: "fixed-output-flag-differs",
            names: base_names(),
            paths: base_paths(),
            system: "x86_64-linux",
            is_fixed_output: true,
            is_ca: false,
            incoming_hash: None,
            row_hash: None,
            expect_match: false,
        },
        Case {
            label: "ca-flag-differs",
            names: base_names(),
            paths: base_paths(),
            system: "x86_64-linux",
            is_fixed_output: false,
            is_ca: true,
            incoming_hash: None,
            row_hash: None,
            expect_match: false,
        },
        Case {
            label: "names-different-set",
            names: vec!["doc".into(), "out".into()],
            paths: vec![dev_path.into(), out_path.into()],
            system: "x86_64-linux",
            is_fixed_output: false,
            is_ca: false,
            incoming_hash: None,
            row_hash: None,
            expect_match: false,
        },
        Case {
            label: "expected-path-differs-on-shared-name",
            names: base_names(),
            paths: vec![
                "/nix/store/22222222222222222222222222222222-axis-dev-evil".into(),
                out_path.into(),
            ],
            system: "x86_64-linux",
            is_fixed_output: false,
            is_ca: false,
            incoming_hash: None,
            row_hash: None,
            expect_match: false,
        },
        Case {
            label: "live-hash-differs-both-present",
            names: base_names(),
            paths: base_paths(),
            system: "x86_64-linux",
            is_fixed_output: false,
            is_ca: false,
            incoming_hash: Some([0xBB; 32]),
            row_hash: Some([0xAA; 32]),
            expect_match: false,
        },
        Case {
            label: "hash-one-sided-no-veto",
            names: base_names(),
            paths: base_paths(),
            system: "x86_64-linux",
            is_fixed_output: false,
            is_ca: false,
            incoming_hash: None,
            row_hash: Some([0xAA; 32]),
            expect_match: true,
        },
    ];

    for (i, c) in cases.iter().enumerate() {
        let hash = format!("axis-{i}");
        // Stage the settled baseline through the production upsert,
        // then settle it.
        let baseline = DerivationRow {
            drv_hash: hash.clone(),
            drv_path: format!("/nix/store/{:0>32}-axis.drv", i),
            pname: None,
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            required_features: vec![],
            expected_output_paths: base_paths(),
            output_names: base_names(),
            is_fixed_output: false,
            is_ca: false,
            wanted_output_names: vec![],
            topdown_pruned: false,
            closure_hole: false,
            drv_content: None,
            ca_modular_hash: c.row_hash,
            ca_modular_hash_stripped: None,
            evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
            needs_resolve: false,
        };
        let mut tx = db.pool().begin().await?;
        SchedulerDb::batch_upsert_derivations(&mut tx, &[baseline], &[]).await?;
        tx.commit().await?;
        sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_hash = $1")
            .bind(&hash)
            .execute(&test_db.pool)
            .await?;

        // In-memory verdict — row through the PRODUCTION loader.
        let rows = db
            .load_settled_identity_rows(std::slice::from_ref(&hash))
            .await?;
        assert_eq!(rows.len(), 1, "{}: settled row loads", c.label);
        let incoming: crate::domain::DerivationNode = rio_proto::types::DerivationNode {
            drv_hash: hash.clone(),
            drv_path: format!("/nix/store/{:0>32}-axis.drv", i),
            system: c.system.into(),
            output_names: c.names.clone(),
            expected_output_paths: c.paths.clone(),
            is_fixed_output: c.is_fixed_output,
            is_content_addressed: c.is_ca,
            ca_modular_hash: c.incoming_hash.map(|h| h.to_vec()).unwrap_or_default(),
            ..Default::default()
        }
        .into();
        let matcher_matches = settled_row_identity_matches(&rows[0], &incoming).is_some();

        // SQL verdict — re-creation upsert, empty carve-out; admitted
        // iff the hash appears in RETURNING.
        let recreation = DerivationRow {
            drv_hash: hash.clone(),
            drv_path: format!("/nix/store/{:0>32}-axis.drv", i),
            pname: None,
            system: c.system.into(),
            status: DerivationStatus::Created,
            required_features: vec![],
            expected_output_paths: c.paths.clone(),
            output_names: c.names.clone(),
            is_fixed_output: c.is_fixed_output,
            is_ca: c.is_ca,
            wanted_output_names: vec![],
            topdown_pruned: false,
            closure_hole: false,
            drv_content: None,
            ca_modular_hash: c.incoming_hash,
            ca_modular_hash_stripped: None,
            evidence_rank: crate::state::DefinitionEvidence::UnverifiedClaim,
            needs_resolve: false,
        };
        let mut tx = db.pool().begin().await?;
        let ids = SchedulerDb::batch_upsert_derivations(&mut tx, &[recreation], &[]).await?;
        tx.commit().await?;
        let guard_admits = ids.contains_key(&hash);

        assert_eq!(
            matcher_matches, c.expect_match,
            "{}: in-memory matcher verdict",
            c.label
        );
        assert_eq!(
            guard_admits, c.expect_match,
            "{}: SQL guard verdict diverges from the matcher",
            c.label
        );
    }
    Ok(())
}

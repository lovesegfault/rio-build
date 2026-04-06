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
    let id0 = id_map.get(&format!("{:032x}", 0)).copied().unwrap();
    let id_last = id_map.get(&format!("{:032x}", N - 1)).copied().unwrap();
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
/// Edges: 40k rows. Old limit was 32767 (2 cols). Build a
/// dense DAG over 10k nodes (fresh DB, so re-insert).
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
            is_ca: false,
        })
        .collect();
    let mut tx = db.pool().begin().await?;
    let id_map = SchedulerDb::batch_upsert_derivations(&mut tx, &rows).await?;

    // 40k edges: each node i>0 has 4 parents among [i-1, i-2, ...].
    // ON CONFLICT DO NOTHING dedups any collisions.
    let ids: Vec<Uuid> = (0..N)
        .map(|i| *id_map.get(&format!("{i:032x}")).unwrap())
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

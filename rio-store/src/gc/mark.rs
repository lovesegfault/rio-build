//! Mark phase: compute unreachable paths via recursive CTE.

use sqlx::PgPool;

/// Compute unreachable `store_path_hash` values.
///
/// A path is REACHABLE if it's reachable via `narinfo."references"`
/// from any of:
/// - `gc_roots` (explicit pins)
/// - uploading manifests (in-flight PutPath)
/// - recently-created paths (grace period)
/// - `extra_roots` (scheduler live-build outputs)
/// - `scheduler_live_pins` (scheduler auto-pinned live-build inputs)
///
/// Returns all COMPLETE manifest `store_path_hash` NOT in the
/// reachable set. These are sweep candidates.
///
/// # Recursive CTE
///
/// `narinfo."references"` is `TEXT[]` of store paths (not hashes —
/// Nix stores paths verbatim). The CTE walks this: seeds → their
/// references → their references → ... until fixed point. The final
/// SELECT finds complete manifests NOT in the reachable closure.
///
/// Seeds are ALL store_path TEXT (not hash) because the CTE joins
/// on `narinfo.store_path = reachable.store_path` (the references
/// array contains paths, not hashes). `gc_roots` keys on hash
/// (BYTEA PK) so we JOIN to narinfo to get the path.
///
/// `extra_roots` might contain paths NOT in narinfo yet (in-flight
/// build outputs). That's fine — the CTE's unnest of a non-existent
/// path produces 0 rows. The extra_root itself stays in the reachable
/// set (we SELECT it directly in the seed UNION) but nothing
/// downstream of it is reached through references.
///
/// # Grace period
///
/// `grace_hours` protects recently-uploaded paths that builds haven't
/// referenced YET. Scenario: worker uploads output A at T, scheduler
/// dispatches a build that needs A at T+5s. If GC runs at T+3s with
/// no grace, A has no root → deleted → build fails. With grace_hours=
/// 2 (default), A is protected until T+2h — plenty of time for the
/// referencing build to be registered.
pub async fn compute_unreachable(
    pool: &PgPool,
    grace_hours: u32,
    extra_roots: &[String],
) -> Result<Vec<Vec<u8>>, sqlx::Error> {
    // The CTE. Walking through the query:
    //
    // `reachable` recursive CTE:
    //   - Anchor (UNION of five seeds, all producing store_path TEXT):
    //     a) gc_roots JOIN narinfo on hash → get path
    //     b) uploading manifests JOIN narinfo on hash → get path
    //     c) narinfo created_at > now - grace → path directly
    //     d) unnest($2) extra_roots (already paths)
    //     e) scheduler_live_pins JOIN narinfo on hash → get path
    //   - Recursive: for each reachable path, unnest its references
    //     (which are store_path strings) — those are also reachable.
    //
    // Final SELECT: narinfo JOIN manifests on hash, WHERE complete
    //   AND store_path NOT IN reachable. These are the unreachable
    //   complete manifests — sweep candidates.
    //
    // `"references"` quoted: PG reserved keyword.
    //
    // `$1::int` cast for grace_hours: PG interval syntax wants
    // `interval '2 hours'` literally; we can't interpolate into
    // a string literal in a parameterized query, so use
    // `now() - ($1::int || ' hours')::interval` or `make_interval`.
    // `make_interval(hours => $1)` is the cleanest.
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        WITH RECURSIVE reachable(store_path) AS (
            -- Seed (a): explicit pins
            SELECT n.store_path
              FROM gc_roots g
              JOIN narinfo n USING (store_path_hash)
            UNION
            -- Seed (b): in-flight uploads
            SELECT n.store_path
              FROM manifests m
              JOIN narinfo n USING (store_path_hash)
             WHERE m.status = 'uploading'
            UNION
            -- Seed (c): grace period
            SELECT store_path FROM narinfo
             WHERE created_at > now() - make_interval(hours => $1::int)
            UNION
            -- Seed (d): scheduler live-build roots (may not be in narinfo)
            SELECT unnest($2::text[])
            UNION
            -- Seed (e): scheduler auto-pinned live-build INPUTS.
            -- JOIN narinfo naturally excludes pins for paths not
            -- yet in store (scheduler writes best-effort at dispatch
            -- time; some inputs may still be uploading).
            SELECT n.store_path
              FROM scheduler_live_pins p
              JOIN narinfo n USING (store_path_hash)
            UNION
            -- Recursive: references of reachable paths
            SELECT unnest(n."references")
              FROM narinfo n
              JOIN reachable r ON n.store_path = r.store_path
        )
        SELECT n.store_path_hash
          FROM narinfo n
          JOIN manifests m USING (store_path_hash)
         WHERE m.status = 'complete'
           AND n.store_path NOT IN (SELECT store_path FROM reachable)
        "#,
    )
    .bind(grace_hours as i32)
    .bind(extra_roots)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(h,)| h).collect())
}

// r[verify store.gc.two-phase]
// (mark is phase 1 of two-phase; sweep tests + these mark tests
// together verify the mark-then-sweep pattern)
#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// Minimal narinfo + manifest seeding. Tests need paths with
    /// known references to verify the CTE walks correctly.
    async fn seed_path(
        pool: &PgPool,
        path: &str,
        refs: &[&str],
        created_hours_ago: u32,
    ) -> Vec<u8> {
        use sha2::Digest;
        let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        sqlx::query(
            r#"
            INSERT INTO narinfo
                (store_path_hash, store_path, nar_hash, nar_size,
                 "references", created_at)
            VALUES ($1, $2, $3, 0, $4,
                    now() - make_interval(hours => $5::int))
            "#,
        )
        .bind(&hash)
        .bind(path)
        // nar_hash: any 32 bytes, not verified in mark phase.
        .bind(&hash)
        .bind(refs.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .bind(created_hours_ago as i32)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
            .bind(&hash)
            .execute(pool)
            .await
            .unwrap();
        hash
    }

    #[tokio::test]
    async fn unreachable_with_no_roots() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Seed one path, old (past grace), no roots, no refs →
        // unreachable.
        let hash = seed_path(&db.pool, "/nix/store/aaa-orphan", &[], 48).await;

        let unreachable = compute_unreachable(&db.pool, 2, &[]).await.unwrap();
        assert_eq!(unreachable, vec![hash]);
    }

    #[tokio::test]
    async fn grace_period_protects_recent() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Path created 1h ago, grace=2h → reachable (in grace).
        let _hash = seed_path(&db.pool, "/nix/store/bbb-recent", &[], 1).await;

        let unreachable = compute_unreachable(&db.pool, 2, &[]).await.unwrap();
        assert!(
            unreachable.is_empty(),
            "recent path should be protected by grace period"
        );
    }

    #[tokio::test]
    async fn extra_roots_protect_paths() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Old path, no other roots.
        let _hash = seed_path(&db.pool, "/nix/store/ccc-live-build", &[], 48).await;

        // Without extra_roots: unreachable.
        let without = compute_unreachable(&db.pool, 2, &[]).await.unwrap();
        assert_eq!(without.len(), 1);

        // WITH the path in extra_roots: reachable.
        let with = compute_unreachable(&db.pool, 2, &["/nix/store/ccc-live-build".to_string()])
            .await
            .unwrap();
        assert!(
            with.is_empty(),
            "extra_roots should protect the live-build path"
        );
    }

    #[tokio::test]
    async fn references_walk_transitively() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Chain: root → middle → leaf. All old. Root is pinned.
        // middle + leaf should be reachable through references.
        let _leaf = seed_path(&db.pool, "/nix/store/ddd-leaf", &[], 48).await;
        let _middle = seed_path(
            &db.pool,
            "/nix/store/eee-middle",
            &["/nix/store/ddd-leaf"],
            48,
        )
        .await;
        let root_hash = seed_path(
            &db.pool,
            "/nix/store/fff-root",
            &["/nix/store/eee-middle"],
            48,
        )
        .await;

        // Pin the root.
        sqlx::query("INSERT INTO gc_roots (store_path_hash, source) VALUES ($1, 'test')")
            .bind(&root_hash)
            .execute(&db.pool)
            .await
            .unwrap();

        let unreachable = compute_unreachable(&db.pool, 2, &[]).await.unwrap();
        assert!(
            unreachable.is_empty(),
            "transitive references from pinned root → all reachable"
        );
    }

    #[tokio::test]
    async fn scheduler_live_pins_protect() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Old path, no gc_roots, not in extra_roots → would be
        // unreachable. But scheduler has it pinned as a live-build
        // input → protected via seed (e).
        let hash = seed_path(&db.pool, "/nix/store/hhh-live-input", &[], 48).await;

        // Without pin: unreachable.
        let without = compute_unreachable(&db.pool, 2, &[]).await.unwrap();
        assert_eq!(without.len(), 1, "unpinned old path → unreachable");

        // Insert scheduler_live_pins row. drv_hash is arbitrary text
        // (scheduler-assigned); store_path_hash must match narinfo.
        sqlx::query(
            "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash) VALUES ($1, 'test-drv')",
        )
        .bind(&hash)
        .execute(&db.pool)
        .await
        .unwrap();

        // WITH pin: reachable.
        let with = compute_unreachable(&db.pool, 2, &[]).await.unwrap();
        assert!(
            with.is_empty(),
            "scheduler_live_pins should protect in-flight build input"
        );
    }

    #[tokio::test]
    async fn uploading_status_protects() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        use sha2::Digest;
        let path = "/nix/store/ggg-uploading";
        let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        // Seed with status='uploading' instead of 'complete'.
        // Old (past grace).
        sqlx::query(
            r#"
            INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size, created_at)
            VALUES ($1, $2, $1, 0, now() - interval '48 hours')
            "#,
        )
        .bind(&hash)
        .bind(path)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'uploading')")
            .bind(&hash)
            .execute(&db.pool)
            .await
            .unwrap();

        let unreachable = compute_unreachable(&db.pool, 2, &[]).await.unwrap();
        // uploading status → seed → NOT in "unreachable" (even
        // though it's past grace and has no refs). Also, the
        // final SELECT filters WHERE status='complete' so
        // uploading rows are never sweep candidates anyway —
        // belt and suspenders.
        assert!(unreachable.is_empty());
    }
}

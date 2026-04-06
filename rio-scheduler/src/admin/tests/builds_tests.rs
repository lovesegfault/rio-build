//! `ListBuilds`/`ListWorkers` RPC tests.
//!
//! Split from the 1732L monolithic `admin/tests.rs` (P0386) to mirror the
//! `admin/{builds,workers}.rs` submodule seams. `list_workers` test lives
//! here (not a separate `workers_tests.rs`) because it's a single ~60L
//! test — both cover list-RPCs, grouping by access pattern.

use std::collections::HashSet;

use super::*;
use rio_test_support::seed_tenant;
use tokio::sync::oneshot;

// r[verify sched.admin.list-builds]
#[tokio::test]
async fn test_list_builds_filter_and_pagination() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());

    // Seed 3 builds directly via db helper (bypasses the actor).
    use crate::state::{BuildOptions, PriorityClass};
    for i in 0..3 {
        sched_db
            .insert_build(
                uuid::Uuid::new_v4(),
                None,
                PriorityClass::Scheduled,
                false,
                &BuildOptions::default(),
                None,
            )
            .await?;
        // Small sleep so submitted_at ordering is deterministic.
        if i < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    // Transition one to succeeded.
    let builds: Vec<(uuid::Uuid,)> =
        sqlx::query_as("SELECT build_id FROM builds ORDER BY submitted_at LIMIT 1")
            .fetch_all(&db.pool)
            .await?;
    sqlx::query("UPDATE builds SET status = 'succeeded', finished_at = now() WHERE build_id = $1")
        .bind(builds[0].0)
        .execute(&db.pool)
        .await?;

    // No filter → 3 builds, total_count=3.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 3);
    assert_eq!(resp.total_count, 3);

    // filter=pending → 2.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            status_filter: "pending".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 2);
    assert_eq!(resp.total_count, 2);

    // Pagination: limit=1 offset=1 → 1 build (second-newest), total=3.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            limit: 1,
            offset: 1,
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1);
    assert_eq!(resp.total_count, 3, "total_count unaffected by pagination");

    // Pagination: offset past end → empty page, total still correct.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            limit: 10,
            offset: 99,
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert!(
        resp.builds.is_empty(),
        "offset >= total_count → empty page, got {} builds",
        resp.builds.len()
    );
    assert_eq!(
        resp.total_count, 3,
        "total_count unaffected by offset-past-end"
    );

    // tenant_filter: seed a tenant + one build tagged with it.
    let tenant_id = seed_tenant(&db.pool, "filter-test").await;
    sched_db
        .insert_build(
            uuid::Uuid::new_v4(),
            Some(tenant_id),
            PriorityClass::Scheduled,
            false,
            &BuildOptions::default(),
            None,
        )
        .await?;
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "filter-test".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1, "only the tenant-tagged build");
    assert_eq!(resp.total_count, 1);

    // Unknown tenant → InvalidArgument.
    let err = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "nonexistent-tenant".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// Cross-tenant isolation: filtering by tenant A must NOT return tenant B's
/// builds. The test above (`test_list_builds_filter_and_pagination`) proves
/// "tenant filter excludes NULL-tenant builds" (1 tagged vs 3 untagged);
/// this test proves "tenant A filter excludes tenant B builds" — the actual
/// multi-tenant safety property.
// r[verify sched.admin.list-builds]
#[tokio::test]
async fn test_list_builds_cross_tenant_isolation() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());
    use crate::state::{BuildOptions, PriorityClass};

    // Seed two tenants.
    let tenant_a = seed_tenant(&db.pool, "tenant-a").await;
    let tenant_b = seed_tenant(&db.pool, "tenant-b").await;

    // Seed one build per tenant. Capture build_id so we can assert
    // WHICH build appears (not just count — a buggy filter that
    // always returns the first row would pass a count-only check).
    let build_a = uuid::Uuid::new_v4();
    sched_db
        .insert_build(
            build_a,
            Some(tenant_a),
            PriorityClass::Scheduled,
            false,
            &BuildOptions::default(),
            None,
        )
        .await?;
    let build_b = uuid::Uuid::new_v4();
    sched_db
        .insert_build(
            build_b,
            Some(tenant_b),
            PriorityClass::Scheduled,
            false,
            &BuildOptions::default(),
            None,
        )
        .await?;

    // Filter by tenant A → exactly build_a, NOT build_b.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "tenant-a".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1, "tenant-a filter → exactly one build");
    assert_eq!(
        resp.builds[0].build_id,
        build_a.to_string(),
        "tenant-a filter must return build_a, not build_b"
    );
    assert_eq!(resp.total_count, 1);

    // Filter by tenant B → exactly build_b, NOT build_a.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "tenant-b".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1, "tenant-b filter → exactly one build");
    assert_eq!(
        resp.builds[0].build_id,
        build_b.to_string(),
        "tenant-b filter must return build_b, not build_a"
    );
    assert_eq!(resp.total_count, 1);

    // No filter → both builds visible.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 2, "no filter → both tenants' builds");
    assert_eq!(resp.total_count, 2);
    let ids: HashSet<_> = resp.builds.iter().map(|b| b.build_id.as_str()).collect();
    assert!(ids.contains(build_a.to_string().as_str()));
    assert!(ids.contains(build_b.to_string().as_str()));

    Ok(())
}

/// Keyset pagination walks the full result set with no duplicates, no
/// gaps, and cursors chain correctly across pages.
///
/// Seeds 250 builds with DETERMINISTIC `submitted_at` values (not
/// `DEFAULT now()`) so the expected page-walk order is stable across CI
/// hosts. Timestamps are planted in 50 "buckets" of 5 builds each —
/// same-timestamp rows exercise the `build_id` tiebreaker in the row-value
/// comparison `(submitted_at, build_id) < (cursor_ts, cursor_id)`. Without
/// the tiebreaker, page boundaries that split a bucket would drop or
/// double-count those rows.
///
/// Also proves the offset→cursor handoff: page 1 is fetched with
/// `offset=0, cursor=None` (backward-compat mode) and the returned
/// `next_cursor` drives pages 2+. This is how the dashboard migrates
/// off offset without a separate "give me a seed cursor" RPC.
// r[verify sched.admin.list-builds]
#[tokio::test]
async fn test_list_builds_cursor_pagination_walks_full_set() -> anyhow::Result<()> {
    const TOTAL: usize = 250;
    const PAGE: u32 = 100;
    const BUCKETS: usize = 50;
    // Expect ceil(250/100) = 3 pages: 100 + 100 + 50.
    const EXPECTED_PAGES: usize = TOTAL.div_ceil(PAGE as usize);

    let (svc, _actor, _task, db) = setup_svc_default().await;

    // Bulk insert 250 builds. `generate_series` for speed (single SQL
    // round-trip vs 250 insert_build calls). Timestamp formula:
    // `now() - interval '1 second' * (250 - i) / 5` — monotone-increasing
    // with `i`, and integer-divides by 5 so every 5 consecutive rows share
    // a timestamp (exercises UUID tiebreak at page boundaries). The
    // `250 - i` makes the FIRST-inserted rows OLDEST, matching real-world
    // submission order — not load-bearing for correctness, just intuitive
    // when debugging.
    sqlx::query(&format!(
        "INSERT INTO builds (build_id, status, priority_class, submitted_at)
         SELECT gen_random_uuid(), 'pending', 'scheduled',
                now() - (({TOTAL} - i) / {per_bucket}) * interval '1 second'
         FROM generate_series(1, {TOTAL}) AS i",
        per_bucket = TOTAL / BUCKETS,
    ))
    .execute(&db.pool)
    .await?;

    // Walk all pages. Page 1 uses offset (compat mode); pages 2+ use
    // the prior page's next_cursor. Collect every build_id we see.
    let mut seen: Vec<String> = Vec::with_capacity(TOTAL);
    let mut cursor: Option<String> = None;
    let mut pages_fetched = 0usize;

    loop {
        let resp = svc
            .list_builds(Request::new(ListBuildsRequest {
                limit: PAGE,
                cursor: cursor.clone(),
                ..Default::default()
            }))
            .await?
            .into_inner();
        pages_fetched += 1;

        // total_count: populated only on page 1 (offset mode). Cursor
        // pages return 0 — count_builds is an O(n) seq-scan, so
        // recomputing per page would defeat keyset's O(limit) win.
        // Client carries the page-1 total forward.
        if cursor.is_none() {
            assert_eq!(
                resp.total_count, TOTAL as u32,
                "first page (offset mode) → total_count populated"
            );
        } else {
            assert_eq!(
                resp.total_count, 0,
                "cursor page {pages_fetched} → total_count=0 (not recomputed)"
            );
        }
        // No page should be empty — next_cursor is withheld on a short
        // page, so the loop terminates before requesting an empty page.
        assert!(
            !resp.builds.is_empty(),
            "page {pages_fetched} is empty — next_cursor was set on last page"
        );
        // Only the final page may be short.
        if pages_fetched < EXPECTED_PAGES {
            assert_eq!(
                resp.builds.len(),
                PAGE as usize,
                "non-final page {pages_fetched} is short"
            );
            assert!(
                resp.next_cursor.is_some(),
                "full page {pages_fetched} must carry next_cursor"
            );
        } else {
            assert_eq!(
                resp.builds.len(),
                TOTAL - (EXPECTED_PAGES - 1) * PAGE as usize,
                "final page length"
            );
            assert!(
                resp.next_cursor.is_none(),
                "short final page must NOT carry next_cursor"
            );
        }

        seen.extend(resp.builds.into_iter().map(|b| b.build_id));

        match resp.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }

        // Defensive loop bound: if the cursor logic were broken (e.g.
        // constant next_cursor), this test would spin. 2× expected-pages
        // is generous headroom for off-by-one bugs to surface without a
        // timeout.
        assert!(
            pages_fetched <= 2 * EXPECTED_PAGES,
            "runaway pagination — cursor not advancing?"
        );
    }

    assert_eq!(pages_fetched, EXPECTED_PAGES, "page count");
    assert_eq!(seen.len(), TOTAL, "saw every row exactly once (count)");

    // No duplicates: set size == vec size. This is the load-bearing check
    // — a broken cursor that repeats a page-boundary row would pass the
    // count check above (if it also skipped a different row) but fail here.
    let unique: HashSet<_> = seen.iter().collect();
    assert_eq!(unique.len(), TOTAL, "no duplicate build_ids across pages");

    // No gaps: the set of returned ids equals the set of ids in PG.
    // Count + no-duplicates implies this IF we trust PG inserted exactly
    // TOTAL rows — verified independently so a partial-insert failure
    // doesn't mask a pagination bug.
    let pg_ids: HashSet<String> = sqlx::query_scalar("SELECT build_id::text FROM builds")
        .fetch_all(&db.pool)
        .await?
        .into_iter()
        .collect();
    assert_eq!(pg_ids.len(), TOTAL, "PG insert sanity");
    let seen_set: HashSet<_> = seen.into_iter().collect();
    assert_eq!(seen_set, pg_ids, "returned ids == PG ids (no gaps)");

    Ok(())
}

/// Keyset boundary exclusion holds to the microsecond.
///
/// The SQL row-value `(submitted_at, build_id) < (cursor_ts, cursor_id)`
/// must be a STRICT `<`. A page-boundary row whose cursor timestamp is
/// reconstructed with float8 jitter (the pre-P0403 `to_timestamp($3/1e6)`
/// path — ~16 significant figures, right at the IEEE754 double-precision
/// limit for 2026-era epoch-microseconds) could round the wrong way and
/// either include the boundary row (`<` becomes `<=` for that µs-bucket)
/// or exclude the row one µs below.
///
/// Inserts three rows at EXACT consecutive microseconds, uses the middle
/// row's `(micros, id)` as the cursor, and asserts:
///   * the row at T-1µs IS returned (strictly less)
///   * the boundary row at T is NOT returned (equal → excluded by `<`)
///   * the row at T+1µs is NOT returned (strictly greater)
///
/// Proves the integer-remainder reconstruction in `list_builds_keyset` is
/// µs-exact. If T2's fix were reverted (direct bigint÷1e6 → float8), a
/// jitter of ±1µs at the `to_timestamp` coercion would flip one of the
/// three asserts.
// r[verify sched.admin.list-builds]
#[tokio::test]
async fn test_list_builds_keyset_pagination_boundary_microsecond_exact() -> anyhow::Result<()> {
    let (_svc, _actor, _task, db) = setup_svc_default().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());

    // Three rows at consecutive microseconds. Fixed timestamp (not `now()`)
    // so the test is deterministic across CI runs AND sits in the 2026
    // range where float8 precision is marginal (~16 sig figs in
    // epoch-seconds-with-6-decimal-µs).
    //
    // `timestamptz` literal + `interval '1 microsecond'` arithmetic: PG
    // stores TIMESTAMPTZ as int64 microseconds internally, so these three
    // values are guaranteed-distinct and consecutive — no floating-point
    // in the INSERT path.
    let base = "2026-03-20T13:17:30.123456Z";
    let ids: Vec<uuid::Uuid> = (0..3).map(|_| uuid::Uuid::new_v4()).collect();
    for (i, id) in ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO builds (build_id, status, priority_class, submitted_at)
             VALUES ($1, 'pending', 'scheduled',
                     $2::timestamptz + $3::int * interval '1 microsecond')",
        )
        .bind(id)
        .bind(base)
        .bind(i as i32)
        .execute(&db.pool)
        .await?;
    }
    // ids[0] → T, ids[1] → T+1µs, ids[2] → T+2µs.

    // Read back the micros via the SAME projection the real query uses.
    // Don't compute Rust-side — the round-trip proof requires the SELECT's
    // EXTRACT → bigint and the WHERE's integer-remainder reconstruction to
    // agree, so both must go through PG.
    let micros: Vec<(uuid::Uuid, i64)> = sqlx::query_as(
        "SELECT build_id,
                (EXTRACT(EPOCH FROM submitted_at) * 1e6)::bigint
         FROM builds ORDER BY submitted_at",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(micros.len(), 3, "three builds inserted");
    // Precondition: micros are strictly consecutive. If PG's TIMESTAMPTZ
    // storage or the EXTRACT projection lost precision, these wouldn't be
    // m, m+1, m+2 — and the rest of the test would be testing the wrong
    // thing.
    assert_eq!(micros[1].1, micros[0].1 + 1, "T, T+1µs are consecutive");
    assert_eq!(micros[2].1, micros[1].1 + 1, "T+1µs, T+2µs are consecutive");

    // Cursor at the MIDDLE row (T+1µs). Row-value `<` should return ONLY
    // the row at T (ids[0]) — not the boundary row, not T+2µs.
    let (cursor_id, cursor_micros) = (micros[1].0, micros[1].1);
    let rows = sched_db
        .list_builds_keyset(None, None, 100, cursor_micros, cursor_id)
        .await?;

    assert_eq!(
        rows.len(),
        1,
        "cursor at T+1µs → exactly one row strictly below (got {} rows: {:?})",
        rows.len(),
        rows.iter().map(|r| r.build_id).collect::<Vec<_>>()
    );
    assert_eq!(
        rows[0].build_id, ids[0],
        "returned row is the T-row (one µs below boundary), NOT the boundary row"
    );
    assert_eq!(
        rows[0].submitted_at_micros, micros[0].1,
        "returned row's micros match the T-row's micros — projection and \
         reconstruction agree to the microsecond"
    );

    // Sanity-inversion: cursor one µs HIGHER (at T+2µs) → two rows below.
    // Proves the boundary moves by exactly one µs when cursor moves by one
    // µs — no lossy rounding collapsing adjacent µs-buckets.
    let rows_hi = sched_db
        .list_builds_keyset(None, None, 100, micros[2].1, micros[2].0)
        .await?;
    assert_eq!(
        rows_hi.len(),
        2,
        "cursor at T+2µs → T and T+1µs both strictly below"
    );
    let returned: HashSet<_> = rows_hi.iter().map(|r| r.build_id).collect();
    assert!(returned.contains(&ids[0]), "T-row included");
    assert!(returned.contains(&ids[1]), "T+1µs-row included");
    assert!(!returned.contains(&ids[2]), "T+2µs boundary excluded");

    Ok(())
}

/// `list_builds_keyset` uses `builds_keyset_idx` (migration 022), not a
/// seq-scan. Catches a dropped-index regression in local dev.
///
/// Seeds enough rows that the planner prefers the index over a seq-scan
/// (PG's cost model won't pick an index scan on a handful of rows).
/// `#[ignore]` by default: EXPLAIN output is a dev-time sanity check, not
/// a CI gate — the row-count in CI's ephemeral DB is small enough that PG
/// might reasonably seq-scan regardless of index presence. Run locally
/// with `cargo test -- --ignored keyset_query_uses`.
#[ignore = "EXPLAIN plan depends on row count + PG cost model; dev-only sanity"]
#[tokio::test]
async fn test_list_builds_keyset_query_uses_builds_keyset_idx() -> anyhow::Result<()> {
    let (_svc, _actor, _task, db) = setup_svc_default().await;

    // Seed enough rows that PG's cost model prefers an index scan.
    // 2000 rows × distinct timestamps → index is obviously cheaper than
    // seq-scan for a LIMIT 100 query. ANALYZE so pg_class.reltuples is
    // fresh (autovacuum might not have fired on an ephemeral test DB).
    sqlx::query(
        "INSERT INTO builds (build_id, status, priority_class, submitted_at)
         SELECT gen_random_uuid(), 'pending', 'scheduled',
                now() - i * interval '1 second'
         FROM generate_series(1, 2000) AS i",
    )
    .execute(&db.pool)
    .await?;
    sqlx::query("ANALYZE builds").execute(&db.pool).await?;

    // EXPLAIN (FORMAT TEXT) — text is easier to grep than JSON for a
    // simple index-name presence check. We pass a mid-range cursor so the
    // row-value filter is selective (not degenerate all-rows).
    let plan: Vec<(String,)> = sqlx::query_as(
        "EXPLAIN (FORMAT TEXT)
         SELECT b.build_id FROM builds b
         WHERE (b.submitted_at, b.build_id)
               < ( to_timestamp($1::bigint / 1000000)
                   + ($1::bigint % 1000000) * interval '1 microsecond',
                   $2::uuid )
         ORDER BY b.submitted_at DESC, b.build_id DESC
         LIMIT 100",
    )
    .bind(i64::MAX / 2)
    .bind(uuid::Uuid::max())
    .fetch_all(&db.pool)
    .await?;

    let joined: String = plan
        .into_iter()
        .map(|(l,)| l)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("builds_keyset_idx"),
        "EXPLAIN plan should reference builds_keyset_idx; got:\n{joined}"
    );
    // Negative: no Sort node — the index's DESC-DESC ordering satisfies
    // ORDER BY directly. If the planner injects a Sort, either the index
    // ordering doesn't match the query or the planner isn't using it.
    assert!(
        !joined.contains("Sort"),
        "EXPLAIN plan should NOT have a Sort node (index ordering matches ORDER BY); got:\n{joined}"
    );

    Ok(())
}

/// Malformed cursors are rejected at the RPC boundary with
/// `InvalidArgument` — not a silent page-1-reset (which would mask client
/// bugs) nor an `Internal` (which would page the on-call for a client's
/// URL-mangling).
#[tokio::test]
async fn test_list_builds_bad_cursor_rejected() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    for bad in ["not base64!", "dG9vLXNob3J0", ""] {
        let err = svc
            .list_builds(Request::new(ListBuildsRequest {
                cursor: Some(bad.into()),
                ..Default::default()
            }))
            .await
            .expect_err("bad cursor should be rejected");
        assert_eq!(
            err.code(),
            tonic::Code::InvalidArgument,
            "cursor {bad:?} → InvalidArgument (got {:?}: {})",
            err.code(),
            err.message()
        );
    }

    Ok(())
}

// r[verify sched.admin.list-workers]
#[tokio::test]
async fn test_list_workers_with_filter() -> anyhow::Result<()> {
    use crate::actor::tests::connect_worker;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Fully registered worker.
    let _rx1 = connect_worker(&actor, "alive-worker", "x86_64-linux", 4).await?;

    // Drain a second worker.
    let _rx2 = connect_worker(&actor, "drain-worker", "aarch64-linux", 2).await?;
    let (tx, rx) = oneshot::channel();
    actor
        .send_unchecked(ActorCommand::DrainWorker {
            worker_id: "drain-worker".into(),
            force: false,
            reply: tx,
        })
        .await?;
    let _ = rx.await?;

    // No filter → both.
    let resp = svc
        .list_workers(Request::new(ListWorkersRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.workers.len(), 2);

    // filter=alive → only alive-worker.
    let resp = svc
        .list_workers(Request::new(ListWorkersRequest {
            status_filter: "alive".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(resp.workers.len(), 1);
    let w = &resp.workers[0];
    assert_eq!(w.worker_id, "alive-worker");
    assert_eq!(w.status, "alive");
    assert_eq!(w.systems, vec!["x86_64-linux".to_string()]);
    assert_eq!(w.max_builds, 4);
    assert_eq!(w.running_builds, 0);
    assert!(w.connected_since.is_some());
    assert!(w.last_heartbeat.is_some());
    // size_class: connect_worker doesn't set it → empty string.
    // (Proves the field is wired — a worker heartbeating with
    // size_class="medium" would round-trip it here.)
    assert_eq!(w.size_class, "");

    // filter=draining → only drain-worker.
    let resp = svc
        .list_workers(Request::new(ListWorkersRequest {
            status_filter: "draining".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(resp.workers.len(), 1);
    assert_eq!(resp.workers[0].worker_id, "drain-worker");
    assert_eq!(resp.workers[0].status, "draining");

    Ok(())
}

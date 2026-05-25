//! The `AppendLog` binding + completeness gate.
//!
//! Runs once per `AppendLog` stream open, after the caller has
//! HMAC-verified the assignment token and rejected service-token
//! callers. Decides whether the stream may write to the execution it
//! claims: the token must be for this derivation, the claimed
//! execution must be the derivation's *latest* assignment to *this*
//! executor, and the execution's log must not already be complete.
//!
//! This is the security boundary between untrusted builder pods (which
//! run arbitrary derivation code) and the log store — the relocation of
//! the scheduler's recv-task `(executor, drv)` binding gate plus the
//! seal that used to live in the scheduler ring buffer's `is_complete`
//! latch (both deleted with the in-scheduler log path).
//!
//! The `builder_id` comparison is a *token-currency* check (is the
//! attempt this token was minted for still the derivation's live
//! assignment?), not a *presenter-identity* check: the assignment token
//! is a bearer credential and `AssignmentClaims::executor_id` is
//! audit-only attribution (see `rio-auth/src/hmac.rs`) — anyone holding
//! a current token passes.

use rio_auth::hmac::AssignmentClaims;
use rio_common::grpc::StatusExt;
use rio_migrations::schema::EXEC_STATUS_TERMINAL;
use rio_nix::store_path::drv_log_hash;
use rio_proto::store::AppendLogHeader;
use sqlx::PgPool;
use tonic::Status;
use uuid::Uuid;

/// A stream open that passed every check. Carries the values the
/// handler needs downstream: the normalized 32-char `drv_hash` (the
/// chunk-key / `drv_executions.drv_hash` form, NOT the DAG key), the
/// parsed `exec_id`, and — when the execution is already terminal —
/// the recorded `final_line_count` the ingest session enforces as its
/// per-append ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOk {
    /// `drv_log_hash()` of the derivation — the form chunk keys and
    /// `drv_executions.drv_hash` use.
    pub drv_hash: String,
    pub exec_id: Uuid,
    /// The execution's recorded end, if its lifecycle row is already
    /// terminal with a known count at open time (the late-replay
    /// case). `None` for a still-running execution — the handler's
    /// periodic refresh picks the count up if the seal lands
    /// mid-stream. See `store.log.completeness-gate`: accepted lines
    /// numbered at or past this are dropped.
    pub final_line_count: Option<i64>,
}

// r[impl store.log.append-auth]
/// May this (already HMAC-verified) token open an `AppendLog` stream
/// for this execution?
///
/// The checks, in order (the first failure wins):
///
/// 1. *(Caller's job — token signature, expiry, service-token
///    rejection.)*
/// 2. **Identity**: `header.derivation_path` and `claims.drv_hash` must
///    normalize to the same derivation. A token for derivation A cannot
///    open a stream claiming to write derivation B's log.
/// 3. **Latest assignment**: the claimed `exec_id` must be the
///    derivation's most recent assignment attempt, and that attempt
///    must have been assigned to `claims.executor_id`. Matching the
///    *latest* row — not only active rows — deliberately admits the
///    post-completion late replay (the builder's detached drain task
///    reconnecting after the build finished) while rejecting any
///    executor whose derivation has since been re-dispatched (a newer —
///    or rewritten-in-place via the scheduler's ON CONFLICT upsert —
///    assignment row with a different exec_id/builder_id exists; the
///    gate handles both identically).
/// 4. **Completeness (the seal)**: a log whose execution is terminal,
///    whose `final_line_count` is known, and whose chunk manifest
///    already covers a contiguous `[0, final_line_count)` can never be
///    appended to again. An *incomplete* terminal log keeps accepting
///    the late replay that makes it complete.
/// 5. *(Handler's job — the single-live-session check is
///    [`super::sessions::acquire`].)*
///
/// Error messages name the check that failed without disclosing other
/// executions' details (e.g. "re-assigned", not "re-assigned to
/// executor-foo").
pub async fn check_append_open(
    pool: &PgPool,
    claims: &AssignmentClaims,
    header: &AppendLogHeader,
) -> Result<GateOk, Status> {
    // -- Check 2: the token and the header must name the same derivation.
    let header_hash = drv_log_hash(&header.derivation_path);
    let claims_hash = drv_log_hash(&claims.drv_hash);
    if header_hash != claims_hash {
        return Err(Status::permission_denied(
            "AppendLog: the assignment token is not for this derivation",
        ));
    }

    let exec_id: Uuid = header
        .exec_id
        .parse()
        .map_err(|_| Status::invalid_argument("AppendLog: header.exec_id is not a valid UUID"))?;

    // -- Check 3: the claimed execution must be the derivation's latest
    // assignment, assigned to this executor.
    //
    // `claims.drv_hash` is bound VERBATIM (not normalized): both it and
    // `derivations.drv_hash` carry the DAG key (`derivations_drv_hash_uq`
    // is UNIQUE on that form), whereas `header_hash` above is the
    // 32-char `drv_log_hash()` form. The two hash vocabularies never
    // join.
    //
    // `query!` (compile-time checked) because this reads
    // scheduler-owned tables — the `STORE_READS` entries in
    // `cross_service_schema_contract` pin these columns and this query
    // is the reason they exist. The `exec_id DESC NULLS LAST`
    // tiebreaker makes same-timestamp attempts deterministic (UUIDv7 is
    // mint-time-ordered).
    let latest = sqlx::query!(
        r#"
        SELECT a.exec_id, a.builder_id
        FROM assignments a
        JOIN derivations d USING (derivation_id)
        WHERE d.drv_hash = $1
        ORDER BY a.assigned_at DESC, a.exec_id DESC NULLS LAST
        LIMIT 1
        "#,
        claims.drv_hash,
    )
    .fetch_optional(pool)
    .await
    .status_internal("AppendLog gate: latest-assignment lookup")?;

    let Some(latest) = latest else {
        return Err(Status::not_found(
            "AppendLog: no assignment recorded for this derivation",
        ));
    };
    // A NULL exec_id on the latest assignment (a pre-exec_id-era row)
    // cannot be matched against a claimed execution — fail closed.
    if latest.exec_id != Some(exec_id) || latest.builder_id != claims.executor_id {
        return Err(Status::permission_denied(
            "AppendLog: this execution is not the derivation's current assignment \
             (the derivation may have been re-assigned)",
        ));
    }

    // -- Check 4: the seal. A complete log accepts no more appends. The
    // recorded final line count (known here iff the execution is
    // already terminal — the late-replay case) also rides back to the
    // ingest session as its per-append ceiling, so an admitted replay
    // can fill the gap below the recorded end but never grow the log
    // past it.
    let final_line_count = sealed_final_line_count(pool, exec_id).await?;
    if let Some(up_to) = final_line_count
        && manifest_covers(pool, exec_id, up_to).await?
    {
        return Err(Status::failed_precondition(
            "AppendLog: this execution's log is already complete",
        ));
    }

    // `claims_hash`, not `header_hash`: the two are provably equal here
    // (check 2 rejected any mismatch), but the chunk-key prefix should
    // trace to the *signed* token input, not to the header that was
    // checked against it.
    Ok(GateOk {
        drv_hash: claims_hash,
        exec_id,
        final_line_count,
    })
}

// r[impl store.log.completeness-gate]
/// The completeness predicate: terminal status ∧ known
/// `final_line_count` ∧ a contiguous manifest covering
/// `[0, final_line_count)`.
///
/// A missing `drv_executions` row means the scheduler has not recorded
/// the execution yet — the log cannot be complete before the execution
/// exists. The same predicate (computed at read time, never latched)
/// backs `TailLogChunk.is_complete` — `pub(super)` so the TailLog read
/// path imports this exact function instead of growing a second copy
/// that could diverge from the seal.
pub(super) async fn log_is_complete(pool: &PgPool, exec_id: Uuid) -> Result<bool, Status> {
    match sealed_final_line_count(pool, exec_id).await? {
        Some(final_line_count) => manifest_covers(pool, exec_id, final_line_count).await,
        None => Ok(false),
    }
}

// r[impl store.log.completeness-gate]
/// The execution's recorded end — its `final_line_count` — if its
/// lifecycle row is terminal and the builder-reported count is known.
///
/// `None` means the execution is still running, has no row yet, or
/// never reported a count: there is no recorded end to seal against or
/// to enforce as the ingest session's per-append ceiling. The two
/// halves of the completeness predicate split here so the `AppendLog`
/// gate can hand the count to the session without a second
/// `drv_executions` read, and so the handler's mid-stream refresh asks
/// exactly the question it needs ("is there a recorded end yet?")
/// without folding the manifest.
pub(super) async fn sealed_final_line_count(
    pool: &PgPool,
    exec_id: Uuid,
) -> Result<Option<i64>, Status> {
    // Compile-time checked: `drv_executions` is scheduler-written /
    // store-read (see `DrvExecutionRow` and the `STORE_READS` contract).
    let row = sqlx::query!(
        r#"SELECT status, final_line_count FROM drv_executions WHERE exec_id = $1"#,
        exec_id,
    )
    .fetch_optional(pool)
    .await
    .status_internal("AppendLog gate: completeness check")?;

    let Some(row) = row else {
        return Ok(None);
    };
    let terminal = row
        .status
        .as_deref()
        .is_some_and(|s| EXEC_STATUS_TERMINAL.contains(&s));
    if !terminal {
        return Ok(None);
    }
    Ok(row.final_line_count)
}

/// Does the execution's chunk manifest contiguously cover `[0, up_to)`?
/// The second half of [`log_is_complete`].
async fn manifest_covers(pool: &PgPool, exec_id: Uuid, up_to: i64) -> Result<bool, Status> {
    // Store-owned table → runtime query (no cross-service contract to
    // enforce). Ordered by first_line so the contiguity fold is a
    // single pass.
    let chunks: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT first_line, line_count FROM drv_log_chunks \
         WHERE exec_id = $1 ORDER BY first_line",
    )
    .bind(exec_id)
    .fetch_all(pool)
    .await
    .status_internal("AppendLog gate: completeness check")?;

    Ok(manifest_covers_contiguously(&chunks, up_to))
}

/// Does an `ORDER BY first_line` manifest cover a contiguous
/// `[0, up_to)` with no gaps?
///
/// Chunks may overlap (two ingest sessions for one execution after a
/// replica failover) — overlap extends coverage, it never breaks it. A
/// chunk starting *past* the covered-through point is a gap and ends
/// the fold early.
fn manifest_covers_contiguously(chunks: &[(i64, i64)], up_to: i64) -> bool {
    let mut covered = 0i64;
    for &(first_line, line_count) in chunks {
        if first_line > covered {
            // A gap: nothing covers [covered, first_line).
            return false;
        }
        covered = covered.max(first_line.saturating_add(line_count));
        if covered >= up_to {
            return true;
        }
    }
    covered >= up_to
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_auth::hmac::AssignmentClaims;
    use rio_proto::store::AppendLogHeader;
    use rio_test_support::TestDb;
    use sqlx::PgPool;
    use uuid::Uuid;

    /// A DAG-key drv_hash (the form `AssignmentClaims.drv_hash` and
    /// `derivations.drv_hash` both carry). `drv_log_hash()` of this is
    /// the bare 32-char prefix.
    const DRV: &str = "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-hello-2.12.drv";
    /// The same derivation as a full store path (what a builder puts in
    /// `AppendLogHeader.derivation_path`). Normalizes to the same hash.
    const DRV_PATH: &str = "/nix/store/0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-hello-2.12.drv";
    /// A different derivation entirely.
    const OTHER_DRV: &str = "1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-other-1.0.drv";

    fn claims(executor_id: &str, drv_hash: &str) -> AssignmentClaims {
        AssignmentClaims {
            executor_id: executor_id.to_string(),
            drv_hash: drv_hash.to_string(),
            expected_outputs: vec![],
            is_ca: false,
            expiry_unix: u64::MAX,
            tenant: None,
        }
    }

    fn header(derivation_path: &str, exec_id: Uuid) -> AppendLogHeader {
        AppendLogHeader {
            derivation_path: derivation_path.to_string(),
            exec_id: exec_id.to_string(),
        }
    }

    /// Seed a derivation row (idempotent on drv_hash) and return its id.
    async fn seed_derivation(pool: &PgPool, drv_hash: &str) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO derivations (drv_hash, drv_path, system, status) \
             VALUES ($1, $2, 'x86_64-linux', 'assigned') \
             ON CONFLICT (drv_hash) DO UPDATE SET drv_path = EXCLUDED.drv_path \
             RETURNING derivation_id",
        )
        .bind(drv_hash)
        .bind(format!("/nix/store/{drv_hash}"))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Seed an assignment attempt for a derivation. `age_secs` orders
    /// attempts (larger = older); the gate must pick the newest.
    async fn seed_assignment(
        pool: &PgPool,
        derivation_id: Uuid,
        builder_id: &str,
        exec_id: Uuid,
        status: &str,
        age_secs: f64,
    ) {
        sqlx::query(
            "INSERT INTO assignments \
                 (derivation_id, builder_id, generation, status, assigned_at, exec_id) \
             VALUES ($1, $2, 1, $3, now() - make_interval(secs => $4), $5)",
        )
        .bind(derivation_id)
        .bind(builder_id)
        .bind(status)
        .bind(age_secs)
        .bind(exec_id)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Seed the scheduler-written lifecycle row for an execution.
    async fn seed_execution(
        pool: &PgPool,
        exec_id: Uuid,
        drv_hash_32: &str,
        status: Option<&str>,
        final_line_count: Option<i64>,
    ) {
        sqlx::query(
            "INSERT INTO drv_executions \
                 (exec_id, drv_hash, executor_id, started_at, status, final_line_count) \
             VALUES ($1, $2, 'builder-0', now(), $3, $4)",
        )
        .bind(exec_id)
        .bind(drv_hash_32)
        .bind(status)
        .bind(final_line_count)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Seed a manifest chunk row covering `[first_line, first_line + line_count)`.
    async fn seed_chunk(pool: &PgPool, exec_id: Uuid, seq: i32, first_line: i64, line_count: i64) {
        sqlx::query(
            "INSERT INTO drv_log_chunks \
                 (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, s3_key) \
             VALUES ($1, $2, $3, $4, $5, 1, $6)",
        )
        .bind(exec_id)
        .bind(Uuid::now_v7())
        .bind(seq)
        .bind(first_line)
        .bind(line_count)
        .bind(format!("logs/test/{exec_id}/{seq}"))
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn accepts_active_assignment_with_matching_exec_and_builder() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "builder-0", exec, "acknowledged", 0.0).await;

        let ok = check_append_open(&db.pool, &claims("builder-0", DRV), &header(DRV_PATH, exec))
            .await
            .expect("gate should accept the active assignment");
        assert_eq!(ok.exec_id, exec);
        // The normalized 32-char form, ready for chunk-key construction.
        assert_eq!(ok.drv_hash, "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm");
        // No drv_executions row at all (let alone a terminal one): the
        // session has no append ceiling yet.
        assert_eq!(ok.final_line_count, None);
    }

    // r[verify store.log.append-auth]
    #[tokio::test]
    async fn rejects_mismatched_exec_id() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let real_exec = Uuid::now_v7();
        let claimed_exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "builder-0", real_exec, "acknowledged", 0.0).await;

        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, claimed_exec),
        )
        .await
        .expect_err("a claimed exec_id that is not the latest assignment's must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err:?}");
    }

    #[tokio::test]
    async fn rejects_mismatched_builder_id() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let old_exec = Uuid::now_v7();
        let new_exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        // builder-0's attempt is OLDER; the derivation was re-dispatched
        // to builder-1 with a new execution. builder-0 still holds a
        // valid token for its (now superseded) attempt.
        seed_assignment(&db.pool, d, "builder-0", old_exec, "failed", 60.0).await;
        seed_assignment(&db.pool, d, "builder-1", new_exec, "acknowledged", 0.0).await;

        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, old_exec),
        )
        .await
        .expect_err("a superseded executor must not be able to keep writing");
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err:?}");
    }

    // r[verify store.log.append-auth]
    #[tokio::test]
    async fn rejects_derivation_path_not_matching_token() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "builder-0", exec, "acknowledged", 0.0).await;

        // The token is for DRV but the header claims to be writing
        // OTHER_DRV's log. The normalizer comparison must catch it
        // before any DB work.
        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(OTHER_DRV, exec),
        )
        .await
        .expect_err("a token for one derivation must not open a stream for another");
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err:?}");
    }

    // r[verify store.log.completeness-gate]
    #[tokio::test]
    async fn accepts_terminal_but_incomplete_execution() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        // The assignment has gone terminal (the build finished) but the
        // log is not complete: there is a gap in the manifest. This is
        // the post-completion late-replay case — the builder's detached
        // drain task reconnecting after a store outage — and it MUST be
        // admitted or the tail of every build that completes during a
        // store outage is lost.
        seed_assignment(&db.pool, d, "builder-0", exec, "completed", 0.0).await;
        seed_execution(
            &db.pool,
            exec,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm",
            Some("succeeded"),
            Some(100),
        )
        .await;
        // Coverage [0, 50) then a gap — lines 50..100 are missing.
        seed_chunk(&db.pool, exec, 0, 0, 50).await;

        let ok = check_append_open(&db.pool, &claims("builder-0", DRV), &header(DRV_PATH, exec))
            .await
            .expect("a terminal-but-incomplete execution must accept the late replay");
        // The admitted replay carries the recorded end with it: the
        // ingest session enforces it as the per-append ceiling so the
        // replay can fill [50, 100) but never append at or past 100.
        assert_eq!(
            ok.final_line_count,
            Some(100),
            "an already-terminal execution's recorded end must ride back to the session"
        );
    }

    // r[verify store.log.completeness-gate]
    #[tokio::test]
    async fn rejects_complete_execution() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "builder-0", exec, "completed", 0.0).await;
        seed_execution(
            &db.pool,
            exec,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm",
            Some("succeeded"),
            Some(100),
        )
        .await;
        // Two chunks (different sessions, as after a mid-build replica
        // failover) covering [0, 60) and [40, 100) — a contiguous
        // overlapping union of [0, 100). The log is complete; nothing
        // may be appended to it ever again.
        seed_chunk(&db.pool, exec, 0, 0, 60).await;
        seed_chunk(&db.pool, exec, 1, 40, 60).await;

        let err = check_append_open(&db.pool, &claims("builder-0", DRV), &header(DRV_PATH, exec))
            .await
            .expect_err("a complete log must be sealed against further appends");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
    }

    #[tokio::test]
    async fn rejects_no_assignment_row() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // The derivation exists but was never assigned (or doesn't exist
        // at all — same outcome).
        seed_derivation(&db.pool, DRV).await;

        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, Uuid::now_v7()),
        )
        .await
        .expect_err("no assignment recorded means nothing to authorize against");
        assert_eq!(err.code(), tonic::Code::NotFound, "{err:?}");
    }

    #[tokio::test]
    async fn rejects_unparseable_exec_id() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &AppendLogHeader {
                derivation_path: DRV_PATH.to_string(),
                exec_id: "not-a-uuid".to_string(),
            },
        )
        .await
        .expect_err("a garbage exec_id must be rejected before any DB work");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "{err:?}");
    }

    // -- manifest_covers_contiguously (pure function) --------------------

    #[test]
    fn contiguity_empty_manifest_covers_nothing() {
        assert!(!manifest_covers_contiguously(&[], 1));
        // Degenerate: a zero-length log is covered by an empty manifest.
        assert!(manifest_covers_contiguously(&[], 0));
    }

    #[test]
    fn contiguity_exact_coverage() {
        assert!(manifest_covers_contiguously(&[(0, 50), (50, 50)], 100));
    }

    #[test]
    fn contiguity_gap_in_the_middle() {
        assert!(!manifest_covers_contiguously(&[(0, 50), (60, 40)], 100));
    }

    #[test]
    fn contiguity_does_not_start_at_zero() {
        assert!(!manifest_covers_contiguously(&[(10, 90)], 100));
    }

    #[test]
    fn contiguity_overlapping_sessions() {
        // Two sessions' chunks overlap (a replica failover mid-build).
        assert!(manifest_covers_contiguously(&[(0, 60), (40, 60)], 100));
    }

    #[test]
    fn contiguity_over_coverage_is_still_coverage() {
        // The manifest extends past the target (a late replay appended
        // lines the builder later disclaimed). [0, n) is still covered.
        assert!(manifest_covers_contiguously(&[(0, 150)], 100));
    }

    #[test]
    fn contiguity_short_coverage() {
        assert!(!manifest_covers_contiguously(&[(0, 99)], 100));
    }
}

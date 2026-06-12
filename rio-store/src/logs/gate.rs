//! The `AppendLog` binding + completeness gate.
//!
//! Runs once per `AppendLog` stream open, after the caller has
//! HMAC-verified the assignment token and rejected service-token
//! callers. Decides whether the stream may write to the execution it
//! claims: the token must be for this derivation, the claimed
//! execution must be an assignment attempt of this derivation bound
//! to *this* executor (authority is keyed on the CLAIMED exec —
//! check 3 below is the single home of that law), and the execution's
//! log must not already be complete.
//!
//! This is the security boundary between untrusted builder pods (which
//! run arbitrary derivation code) and the log store — the relocation of
//! the scheduler's recv-task `(executor, drv)` binding gate plus the
//! seal that used to live in the scheduler ring buffer's `is_complete`
//! latch (both deleted with the in-scheduler log path).
//!
//! The `builder_id` comparison binds the claimed execution to the
//! executor the token was minted for (the claimed-exec authority
//! model, check 3), not a *presenter-identity* check: the assignment
//! token is a bearer credential and `AssignmentClaims::executor_id` is
//! audit-only attribution (see `rio-auth/src/hmac.rs`) — anyone
//! holding the token for the claimed attempt passes.

use rio_auth::hmac::AssignmentClaims;
use rio_common::grpc::StatusExt;
use rio_migrations::schema::EXEC_STATUS_TERMINAL;
use rio_nix::store_path::drv_log_hash;
use rio_proto::store::AppendLogHeader;
use sqlx::PgPool;
use tonic::Status;
use uuid::Uuid;

use super::kernel::manifest_covers_contiguously;

/// A stream open that passed every check. Carries the values the
/// handler needs downstream: the normalized 32-char `drv_hash` (the
/// chunk-key / `drv_executions.drv_hash` form, NOT the DAG key), the
/// parsed `exec_id`, the recorded `final_line_count` when the
/// execution is already terminal, the four-quantity durable account
/// ([`LogSeed`] — the frozen measure), and the durable covered ranges.
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
    /// THE FROZEN MEASURE (R28; merged_bug_002): the execution's
    /// durable log account at open time, all four quantities —
    /// `merged_bytes`/`merged_chunks` (the IDEMPOTENT-UNION algebra:
    /// what an honest retry may re-send without double-charge; seeds
    /// the session's lifetime byte and chunk-attempt counters so a
    /// reconnect resumes the account instead of zeroing it) and
    /// `raw_bytes`/`raw_rows` (the MONOTONE-SUM algebra: total durable
    /// writes, which no reconnect, replay, or dedup ever decreases;
    /// the open gate's ceiling preconditions). The quantities and
    /// their algebras are kernel-defined
    /// ([`rio_log_kernel::log_account`], kani-pinned) and
    /// differentially pinned against the seed SQL — a future edit that
    /// changes WHAT any field witnesses re-derives the whole seal
    /// (W11-A/W11-A2/W11-D go red), never edits inside it. Pre-089
    /// chunk rows account 0 bytes (see `M_089`).
    pub seed: LogSeed,
    /// The execution's durable covered line ranges at open time,
    /// normalized. Consumed by the ingest session's covered-replay
    /// consult: a batch fully inside durable coverage is dropped
    /// uncharged and un-written (it cannot mint objects, manifest
    /// rows, or raw charge), with the drop acked from the manifest
    /// truth — the write-path arm of the dual-axis law.
    pub covered: rio_log_kernel::CoverageMap,
}

/// The per-execution caps the gate enforces at open time (check 5) —
/// the same values the ingest session enforces mid-stream, so the open
/// check and the stream check cannot drift.
#[derive(Debug, Clone, Copy)]
pub struct OpenCaps {
    /// [`super::ingest::IngestConfig::per_exec_byte_cap`].
    pub per_exec_byte_cap: u64,
    /// The handler's `log_max_chunks_per_exec`.
    pub max_chunks_per_exec: u32,
}

// r[impl store.log.cap-reject-class]
/// THE constructor for permanent per-execution `AppendLog` rejections:
/// `FAILED_PRECONDITION` plus the `x-rio-log-reject` metadata naming
/// the reason class (`cap`, `complete`, `superseded`) so the builder's
/// uploader maps it onto its loss-disclosure `AbandonReason` without
/// parsing messages.
///
/// SOLE producer (bug_068): every cap/seal surface — the gate's
/// open-time checks here, ingest's lifetime byte cap, the driver's
/// mid-stream chunk cap, and the final drain's cap arm — builds its
/// status through this function. The defect class this kills: a
/// per-execution cap hand-rolled as bare `RESOURCE_EXHAUSTED`
/// (per-replica capacity vocabulary), which the builder dutifully
/// retried against another replica at 1 Hz forever — the cap travels
/// with the EXECUTION, so no retry anywhere can succeed. The
/// `log-cap-status-chokepoint` misc-check pins the producer set: this
/// file is the only `x-rio-log-reject` insert site and
/// [`replica_capacity_status`] the only `resource_exhausted` site in
/// the log plane.
pub(super) fn cap_rejection(class: &'static str, msg: String) -> Status {
    let mut status = Status::failed_precondition(msg);
    status.metadata_mut().insert(
        rio_proto::LOG_REJECT_METADATA_KEY,
        tonic::metadata::MetadataValue::from_static(class),
    );
    status
}

// r[impl store.log.cap-reject-class]
/// THE constructor for per-REPLICA capacity refusals:
/// `RESOURCE_EXHAUSTED`, no reject-class metadata. The builder's
/// uploader treats this as retryable-elsewhere (fail over to another
/// replica and replay) — the correct semantics for the admission gates
/// (stream-count cap, buffer byte budget), and PRECISELY the wrong one
/// for anything per-execution, which is why the two vocabularies get
/// two constructors and the misc-check forbids `resource_exhausted`
/// anywhere else in the log plane.
pub(super) fn replica_capacity_status(msg: &'static str) -> Status {
    Status::resource_exhausted(msg)
}

/// The execution's durable log account, measured on BOTH algebras of
/// the caps law (merged_bug_002): the MERGED COVERAGE projection
/// (idempotent union — what an honest retry may re-send without
/// double-charge) and the RAW MONOTONE totals (Σ over ALL committed
/// rows — what was actually durably written; no reconnect, replay, or
/// dedup ever decreases them). The two quantities have opposite
/// algebras, and a containment budget for an untrusted at-least-once
/// writer must consult BOTH: seeding from the idempotent projection
/// alone hands the budget delta to whoever controls duplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogSeed {
    /// Accounted bytes over merged coverage (idempotent union):
    /// contained/identical-interval duplicate rows charge zero.
    pub merged_bytes: u64,
    /// Committed chunk count over merged coverage — one survivor per
    /// covered interval.
    pub merged_chunks: u32,
    /// Accounted bytes over ALL rows (monotone sum): every durable
    /// write counts, duplicates included.
    pub raw_bytes: u64,
    /// Manifest row count over ALL rows (monotone sum) — the durable
    /// object-count quantity the chunk ceiling bounds.
    pub raw_rows: u64,
}

impl LogSeed {
    /// The merged pair `(bytes, chunks)` — the forgiveness-axis values
    /// the session counters are seeded from.
    pub fn merged_pair(self) -> (u64, u32) {
        (self.merged_bytes, self.merged_chunks)
    }
}

/// How many times the per-execution caps an execution's RAW durable
/// totals may reach before the open gate refuses outright
/// (merged_bug_002): `raw_bytes < REPLAY_ALLOWANCE × per_exec_byte_cap`
/// and `raw_rows < REPLAY_ALLOWANCE × max_chunks_per_exec` are open
/// preconditions.
///
/// Derivation (k = 2): the honest worst case is ONE full replay of a
/// session whose every ack was lost — the builder's retransmit buffer
/// re-sends committed-but-unacked content once per reconnect, so an
/// honest execution's raw total is bounded by 2× its merged content
/// (≤ 2× the cap). Consecutive total-ack-loss cycles beyond that are
/// indistinguishable from the adversarial replay loop — which is the
/// point: the ceiling must bound what the merged seed cannot see.
/// Honest steady state converges to ~1× once the open-time coverage
/// watermark lands (the builder trims committed content before
/// replaying); k is the operational tuning surface for the first soak
/// window.
pub(super) const REPLAY_ALLOWANCE: u64 = 2;

/// The execution's durable log account: [`LogSeed`] — merged coverage
/// AND raw totals, ONE query, run at every open (the gate side); the
/// only reader of `drv_log_chunks.accounted_bytes` besides the cut
/// that writes it.
///
/// Why merged (the forgiveness pair): a manifest INSERT that commits
/// server-side but surfaces as an error (lost response, or a
/// watchdog-abandoned cut dropped post-commit) is re-cut under a
/// freshly burned chunk_seq, so the write path is at-least-once with
/// per-attempt rekeying — duplicate rows covering overlapping
/// `[first_line, first_line + line_count)` are a NORMAL artifact of
/// that retry shape, and a bare count+sum charges the same lines
/// once per attempt against the permanent FAILED_PRECONDITION cap
/// class. The merged pair prunes every row CONTAINED in another row's
/// interval (deterministic tiebreak on identical intervals), which
/// is exact merged coverage for every overlap shape the write path
/// can produce: a failed run is restored to the FRONT of the buffer,
/// so a same-session re-cut starts at the same first_line and
/// extends at least as far (a containment chain), and a
/// cross-session resend starts at the builder's unacked point at or
/// before the orphan row's start. A true partial overlap is
/// unproducible by the write path; if one ever lands, both rows
/// count (a bounded over-count on the overlap tail — conservative
/// for a cap, never an under-count).
///
/// Why raw (the containment pair): the merged projection is
/// IDEMPOTENT — identical-interval replay rows witness zero — so it
/// structurally cannot bound total durable writes for a writer that
/// controls duplication. The raw aggregates are unconditioned over
/// the same `exec_id` scan (no second query): every committed row
/// counts once, forever.
pub(super) async fn log_seed(pool: &PgPool, exec_id: Uuid) -> Result<LogSeed, Status> {
    // Store-owned table → runtime query. A row is REDUNDANT iff some
    // other row covers its interval AND is either strictly wider or
    // (identical interval) orders after it — so exactly one row per
    // covered interval survives the merged FILTER; the raw pair
    // aggregates the same scan unconditioned.
    const NOT_REDUNDANT: &str = "NOT EXISTS ( \
              SELECT 1 FROM drv_log_chunks o \
               WHERE o.exec_id = r.exec_id \
                 AND (o.session_id, o.chunk_seq) <> (r.session_id, r.chunk_seq) \
                 AND o.first_line <= r.first_line \
                 AND o.first_line + o.line_count >= r.first_line + r.line_count \
                 AND (   o.first_line < r.first_line \
                      OR o.first_line + o.line_count > r.first_line + r.line_count \
                      OR (o.session_id, o.chunk_seq) > (r.session_id, r.chunk_seq)))";
    let sql = format!(
        "SELECT count(*) FILTER (WHERE {NOT_REDUNDANT}), \
                COALESCE(sum(accounted_bytes) FILTER (WHERE {NOT_REDUNDANT}), 0)::BIGINT, \
                count(*), \
                COALESCE(sum(accounted_bytes), 0)::BIGINT \
           FROM drv_log_chunks r \
          WHERE r.exec_id = $1"
    );
    // AssertSqlSafe: composed exclusively from const fragments — no
    // runtime data enters the text.
    let row: (i64, i64, i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(exec_id)
        .fetch_one(pool)
        .await
        .status_internal("AppendLog gate: durable cap seed")?;
    Ok(LogSeed {
        merged_bytes: row.1.max(0) as u64,
        merged_chunks: row.0.clamp(0, i64::from(u32::MAX)) as u32,
        raw_bytes: row.3.max(0) as u64,
        raw_rows: row.2.max(0) as u64,
    })
}

// r[impl store.log.append-auth+2]
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
/// 3. **Claimed-exec authority**: the claimed `exec_id` must be a
///    recorded assignment attempt of this derivation, assigned to
///    `claims.executor_id`, whose execution lifecycle row EXISTS with
///    kind `build` (the mint writes assignment + lifecycle row in one
///    transaction, so a missing row is never a live appender — it is
///    denied, never defaulted). The builder's OWN superseded attempt
///    stays writable (the post-completion late replay; containment is
///    exec-keyed chunks + the durable caps + the `final_line_count`
///    ceiling) — but another executor's execution, an
///    in-place-rewritten attempt, a row-less assignment, and any
///    materialization-kind execution are rejected.
/// 4. **Completeness (the seal)**: a log whose execution is terminal,
///    whose `final_line_count` is known, and whose chunk manifest
///    already covers a contiguous `[0, final_line_count)` can never be
///    appended to again. An *incomplete* terminal log keeps accepting
///    the late replay that makes it complete.
/// 5. **Durable caps**: an execution at or over its per-execution
///    byte or chunk cap (computed from the committed manifest, not
///    session counters) is rejected with the permanent `cap` class.
/// 6. *(Handler's job — the single-live-session check is
///    [`super::sessions::acquire`].)*
///
/// Error messages name the check that failed without disclosing other
/// executions' details (e.g. "re-assigned", not "re-assigned to
/// executor-foo").
pub async fn check_append_open(
    pool: &PgPool,
    claims: &AssignmentClaims,
    header: &AppendLogHeader,
    caps: OpenCaps,
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

    // -- Check 3: the claimed execution must be an assignment attempt
    // of THIS derivation, assigned to THIS executor, with execution
    // kind `build`.
    //
    // Authority is keyed on the CLAIMED exec, not on whichever attempt
    // is newest for the derivation: a newer attempt — most commonly a
    // materialization mint, which can never legitimately append log
    // lines — used to displace a terminal-but-incomplete build's late
    // replay (merged_bug_101), silently losing the tail of every build
    // that finished during a store outage and was then re-probed. A
    // superseded executor writing to ITS OWN execution is contained,
    // not contaminating: chunks are exec-keyed, the durable caps
    // (check 5) bound its volume, and `final_line_count` bounds its
    // range. The derivation-level "who is current" question belongs to
    // the scheduler's assignment table, not to log admission.
    //
    // The in-place ON-CONFLICT rewrite (the scheduler reusing the
    // assignment row for a new attempt) leaves the OLD exec with no
    // matching row — that path still rejects, and the builder
    // discloses the dropped tail loudly (`builder.log.loss-disclosure`)
    // instead of silently retrying forever.
    //
    // `claims.drv_hash` is bound VERBATIM (not normalized): both it and
    // `derivations.drv_hash` carry the DAG key (`derivations_drv_hash_uq`
    // is UNIQUE on that form), whereas `header_hash` above is the
    // 32-char `drv_log_hash()` form. The two hash vocabularies never
    // join.
    //
    // Runtime query over scheduler-owned tables: the `STORE_READS`
    // entries in `cross_service_schema_contract` pin every column this
    // reads (including `drv_executions.attempt_kind` — added with
    // migration 089's view for the same reason). Runtime rather than
    // `query!` because the compile-time macro would couple every
    // `cargo xtask` build to a live PG (xtask transitively builds this
    // crate; the regen chicken-and-egg recorded in the wave log) — the
    // contract test is the cross-service guarantee either way.
    //
    // INNER JOIN on drv_executions, deliberately: the mint writes the
    // assignment and its lifecycle row in ONE transaction
    // (`mint_pull_attempt_fenced`), so every legitimate appender's row
    // exists before its token can be presented. An assignments row
    // whose execution row is absent (a post-sweep lingering assignment,
    // or a claim fabricated around the mint) therefore yields no row
    // here and falls into the `superseded` rejection below — absence
    // denies. The first cut of this gate used `LEFT JOIN +
    // COALESCE(e.attempt_kind, 'build')`, which defaulted a MISSING
    // row to the authorized kind: absence-as-verdict in an
    // authorization predicate (security review, 2026-06-03).
    // r[impl store.log.read-authority]
    let claimed: Option<(String, String)> = sqlx::query_as(
        "SELECT a.builder_id, e.attempt_kind \
         FROM assignments a \
         JOIN derivations d USING (derivation_id) \
         JOIN drv_executions e ON e.exec_id = a.exec_id \
         WHERE d.drv_hash = $1 AND a.exec_id = $2",
    )
    .bind(&claims.drv_hash)
    .bind(exec_id)
    .fetch_optional(pool)
    .await
    .status_internal("AppendLog gate: claimed-assignment lookup")?;

    let Some((claimed_builder, claimed_kind)) = claimed else {
        // No assignment attempt of this derivation ever carried the
        // claimed exec_id, the row was rewritten in place, or the
        // execution lifecycle row is missing (the kind cannot be
        // verified — absence is not authorization). Distinct cases,
        // one disposition: the permanent `superseded` class.
        return Err(cap_rejection(
            "superseded",
            "AppendLog: the claimed execution is not a recorded assignment \
             of this derivation (it may have been re-assigned in place)"
                .to_string(),
        ));
    };
    if claimed_builder != claims.executor_id {
        return Err(cap_rejection(
            "superseded",
            "AppendLog: this execution was not assigned to this executor".to_string(),
        ));
    }
    if claimed_kind != "build" {
        // A materialization execution has no build log; admitting one
        // as an append target would let chunk rows shadow the real
        // build's log under the kind-blind reader.
        return Err(cap_rejection(
            "superseded",
            "AppendLog: the claimed execution is not a build attempt".to_string(),
        ));
    }

    // `claims_hash`, not `header_hash`: the two are provably equal here
    // (check 2 rejected any mismatch), but the chunk-key prefix should
    // trace to the *signed* token input, not to the header that was
    // checked against it.
    finish_open(pool, claims_hash, exec_id, caps).await
}

// r[impl store.log.caps-durable+2]
/// Checks 4 + 5 — the completeness seal and the durable per-execution
/// caps — plus the seed read every admitted open carries back to its
/// session. Shared by the token path ([`check_append_open`]) and the
/// handler's dev-mode path so the two cannot drift.
pub(super) async fn finish_open(
    pool: &PgPool,
    drv_hash: String,
    exec_id: Uuid,
    caps: OpenCaps,
) -> Result<GateOk, Status> {
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
        return Err(cap_rejection(
            "complete",
            "AppendLog: this execution's log is already complete".to_string(),
        ));
    }

    // -- Check 5: the durable caps — the DUAL-AXIS TWO-QUANTITY law
    // (merged_bug_002). Each cap quantity (bytes AND chunks) is
    // measured on BOTH algebras of [`LogSeed`]: the MERGED coverage
    // projection (idempotent union) seeds the session's counters so an
    // honest retry's re-send of committed content is never
    // double-charged (forgiveness — merged_bug_207's "a reconnect can
    // never reset either cap" lives on this axis), and the RAW
    // monotone totals (Σ over ALL committed rows) bound total durable
    // writes per execution at REPLAY_ALLOWANCE× the caps (containment
    // — the merged projection structurally cannot bound a writer that
    // controls duplication: identical-interval replays witness zero
    // there). An execution at or over any of the four preconditions
    // is rejected at open with the same permanent class the
    // mid-stream trip uses. RESOURCE_EXHAUSTED is deliberately NOT
    // used here — that code is reserved for per-replica capacity
    // (retry elsewhere can succeed; these caps travel with the
    // execution).
    let seed = log_seed(pool, exec_id).await?;
    let (prior_accounted_bytes, prior_chunks) = seed.merged_pair();
    if prior_accounted_bytes >= caps.per_exec_byte_cap {
        metrics::counter!("rio_store_log_ingest_rejected_total", "reason" => "byte_cap")
            .increment(1);
        return Err(cap_rejection(
            "cap",
            format!(
                "AppendLog: execution exceeded the {}-byte log ingest cap",
                caps.per_exec_byte_cap
            ),
        ));
    }
    if prior_chunks >= caps.max_chunks_per_exec {
        metrics::counter!("rio_store_log_ingest_rejected_total", "reason" => "chunk_cap")
            .increment(1);
        return Err(cap_rejection(
            "cap",
            format!(
                "AppendLog: execution exceeded the {}-chunk cap",
                caps.max_chunks_per_exec
            ),
        ));
    }

    // -- The raw monotone ceilings (merged_bug_002). The merged checks
    // above measure the IDEMPOTENT coverage projection — what an honest
    // retry may re-send without double-charge — which structurally
    // cannot bound total durable writes for a writer that controls
    // duplication: identical-interval replay rows witness zero there
    // while every cycle durably mints new objects and manifest rows.
    // These two arms bound the MONOTONE quantities (Σ raw
    // accounted_bytes, raw row count over ALL committed rows), which no
    // reconnect resets BY ALGEBRA: cycle-cumulative containment on both
    // cap quantities. Same permanent class as the merged trips — the
    // ceiling travels with the execution. `saturating_mul`: a test-tier
    // `u64::MAX` cap must read as "no ceiling", not overflow.
    // r[impl store.log.raw-ceiling]
    if seed.raw_bytes >= REPLAY_ALLOWANCE.saturating_mul(caps.per_exec_byte_cap) {
        metrics::counter!("rio_store_log_ingest_rejected_total", "reason" => "byte_cap")
            .increment(1);
        return Err(cap_rejection(
            "cap",
            format!(
                "AppendLog: execution exceeded the raw durable byte ceiling \
                 ({} bytes stored >= {}x the {}-byte cap); no reconnect resets it",
                seed.raw_bytes, REPLAY_ALLOWANCE, caps.per_exec_byte_cap
            ),
        ));
    }
    // r[impl store.log.raw-ceiling]
    if seed.raw_rows >= REPLAY_ALLOWANCE.saturating_mul(u64::from(caps.max_chunks_per_exec)) {
        metrics::counter!("rio_store_log_ingest_rejected_total", "reason" => "chunk_cap")
            .increment(1);
        return Err(cap_rejection(
            "cap",
            format!(
                "AppendLog: execution exceeded the raw durable chunk-row ceiling \
                 ({} manifest rows stored >= {}x the {}-chunk cap); no reconnect \
                 resets it",
                seed.raw_rows, REPLAY_ALLOWANCE, caps.max_chunks_per_exec
            ),
        ));
    }

    // The durable covered ranges, for the session's covered-replay
    // consult. A plain ordered scan, no anti-join: the union over ALL
    // rows equals the union over merged survivors (pruned rows are
    // contained), so normalization is the kernel's job. Bounded by the
    // raw row ceiling that just admitted this open.
    let intervals: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT first_line, line_count FROM drv_log_chunks \
         WHERE exec_id = $1 ORDER BY first_line",
    )
    .bind(exec_id)
    .fetch_all(pool)
    .await
    .status_internal("AppendLog gate: durable coverage read")?;
    let covered = rio_log_kernel::CoverageMap::from_intervals(
        intervals
            .into_iter()
            .map(|(f, c)| (f.max(0) as u64, c.max(0) as u64)),
    );

    Ok(GateOk {
        drv_hash,
        exec_id,
        final_line_count,
        seed,
        covered,
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
/// DEMOTED (merged_bug_063): cursor-blind — kept only as the test
/// oracle for the durable half of the predicate. The serve path mints
/// [`final_claim_for`] instead; a `TailLog` final message cannot be
/// stamped from this function (`send_final` takes the kernel claim).
#[cfg(test)]
pub(super) async fn log_is_complete(pool: &PgPool, exec_id: Uuid) -> Result<bool, Status> {
    match sealed_final_line_count(pool, exec_id).await? {
        Some(final_line_count) => manifest_covers(pool, exec_id, final_line_count).await,
        None => Ok(false),
    }
}

/// The serve path's ONLY completeness source: fetch the sealed witness
/// and the manifest fold, then mint the kernel
/// [`FinalClaim`](rio_log_kernel::FinalClaim)
/// correlated with the SERVED cursor — its watermark AND its latched
/// gap fact (bug_048: covers-now + cursor-reached is not delivery
/// evidence; a gap-crossing serve whose hole a late replay backfilled
/// must not stamp complete). The cursor-blind `log_is_complete` above
/// is demoted to the ingest/test side — a `TailLog` final message
/// cannot be stamped from it (merged_bug_063: `send_final` takes the
/// claim, not a bool).
// r[impl store.log.served-claim+2]
// r[impl store.log.final-served]
pub(super) async fn final_claim_for(
    pool: &PgPool,
    exec_id: Uuid,
    cursor: &super::tail::LineCursor,
) -> Result<rio_log_kernel::FinalClaim, Status> {
    let sealed = sealed_final_line_count(pool, exec_id).await?;
    let covers = match sealed {
        Some(n) => manifest_covers(pool, exec_id, n).await?,
        None => false,
    };
    Ok(rio_log_kernel::final_claim(
        cursor.next_line(),
        sealed.map(|n| n as u64),
        covers,
        cursor.gap_crossed(),
    ))
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
/// The second half of the (test-demoted) `log_is_complete`. The fold
/// itself is the pure
/// kernel [`manifest_covers_contiguously`]; this wrapper owns the SQL.
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

    /// Production-default caps: high enough that no fixture trips them
    /// unless it means to.
    fn caps() -> OpenCaps {
        OpenCaps {
            per_exec_byte_cap: super::super::ingest::DEFAULT_PER_EXEC_BYTE_CAP,
            max_chunks_per_exec: 100_000,
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
        // Production's mint writes both rows in one transaction; the
        // fixture mirrors it (the gate now REQUIRES the lifecycle row).
        seed_assignment(&db.pool, d, "builder-0", exec, "acknowledged", 0.0).await;
        seed_execution(
            &db.pool,
            exec,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm",
            None,
            None,
        )
        .await;

        let ok = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, exec),
            caps(),
        )
        .await
        .expect("gate should accept the active assignment");
        assert_eq!(ok.exec_id, exec);
        // The normalized 32-char form, ready for chunk-key construction.
        assert_eq!(ok.drv_hash, "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm");
        // A non-terminal execution row: the session has no append
        // ceiling yet.
        assert_eq!(ok.final_line_count, None);
    }

    // r[verify store.log.append-auth+2]
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
            caps(),
        )
        .await
        .expect_err("a claimed exec_id with no recorded assignment must be rejected");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "superseded"
        );
    }

    #[tokio::test]
    async fn rejects_another_executors_execution() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let old_exec = Uuid::now_v7();
        let new_exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "builder-0", old_exec, "failed", 60.0).await;
        seed_assignment(&db.pool, d, "builder-1", new_exec, "acknowledged", 0.0).await;
        // The lifecycle row exists (as production guarantees), so the
        // rejection below is the WRONG-EXECUTOR arm, not the missing-row
        // arm.
        seed_execution(
            &db.pool,
            new_exec,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm",
            None,
            None,
        )
        .await;

        // builder-0 claiming builder-1's execution: rejected — the only
        // executor with write authority over an exec is the one it was
        // assigned to.
        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, new_exec),
            caps(),
        )
        .await
        .expect_err("an executor must not write another executor's execution");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "superseded"
        );
    }

    /// v2 authority: the builder's OWN superseded attempt stays
    /// writable (the late replay is keyed on the claimed exec, not on
    /// the derivation's newest attempt). Containment: exec-keyed
    /// chunks + the durable caps + the final_line_count ceiling.
    #[tokio::test]
    async fn admits_own_superseded_execution() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let old_exec = Uuid::now_v7();
        let new_exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "builder-0", old_exec, "failed", 60.0).await;
        seed_execution(
            &db.pool,
            old_exec,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm",
            None,
            None,
        )
        .await;
        seed_assignment(&db.pool, d, "builder-1", new_exec, "acknowledged", 0.0).await;

        let ok = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, old_exec),
            caps(),
        )
        .await
        .expect("an executor's own superseded attempt stays writable");
        assert_eq!(ok.exec_id, old_exec);
    }

    // r[verify store.log.read-authority]
    /// The merged_bug_101 regression: a newer materialization mint
    /// becoming the derivation's newest attempt must not displace a
    /// terminal-but-incomplete build's late replay.
    #[tokio::test]
    async fn gate_admits_terminal_incomplete_build_after_materialization_mint() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let build_exec = Uuid::now_v7();
        let mat_exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "builder-0", build_exec, "completed", 60.0).await;
        seed_execution(
            &db.pool,
            build_exec,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm",
            Some("succeeded"),
            Some(100),
        )
        .await;
        // Coverage [0, 50): terminal but incomplete.
        seed_chunk(&db.pool, build_exec, 0, 0, 50).await;
        seed_assignment(&db.pool, d, "store-0-w1", mat_exec, "acknowledged", 0.0).await;
        sqlx::query(
            "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at, attempt_kind) \
             VALUES ($1, '0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm', 'store-0-w1', now(), 'materialization')",
        )
        .bind(mat_exec)
        .execute(&db.pool)
        .await
        .unwrap();

        let ok = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, build_exec),
            caps(),
        )
        .await
        .expect("the builder's own terminal-but-incomplete build exec must stay admittable");
        assert_eq!(ok.final_line_count, Some(100));
        assert_eq!(ok.seed.merged_chunks, 1);
    }

    // r[verify store.log.read-authority]
    /// Absence-as-verdict (security review, 2026-06-03): an assignments
    /// row whose `drv_executions` row is MISSING must be rejected, not
    /// defaulted to the authorized `build` kind. The mint writes both
    /// rows in one transaction (`mint_pull_attempt_fenced`), so a
    /// missing lifecycle row is never a live appender — it is a
    /// post-sweep lingering assignment or a hand-crafted claim, and the
    /// gate is an authorization predicate: absence denies.
    #[tokio::test]
    async fn rejects_assignment_without_execution_row() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        // Assignment row only — NO drv_executions row.
        seed_assignment(&db.pool, d, "builder-0", exec, "acknowledged", 0.0).await;

        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, exec),
            caps(),
        )
        .await
        .expect_err(
            "an assignment with no execution lifecycle row must be denied: \
             the kind cannot be verified, and absence is not authorization",
        );
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "superseded"
        );
    }

    // r[verify store.log.read-authority]
    #[tokio::test]
    async fn gate_rejects_materialization_exec_as_append_target() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let mat_exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "store-0-w1", mat_exec, "acknowledged", 0.0).await;
        sqlx::query(
            "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at, attempt_kind) \
             VALUES ($1, '0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm', 'store-0-w1', now(), 'materialization')",
        )
        .bind(mat_exec)
        .execute(&db.pool)
        .await
        .unwrap();

        let err = check_append_open(
            &db.pool,
            &claims("store-0-w1", DRV),
            &header(DRV_PATH, mat_exec),
            caps(),
        )
        .await
        .expect_err("a materialization execution must never be an append target");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "superseded"
        );
    }

    // r[verify store.log.caps-durable+2]
    /// The merged_bug_207 regression: an execution whose DURABLE chunk
    /// account already exceeds a per-execution cap is rejected at open
    /// — a reconnect can no longer reset the caps to zero.
    #[tokio::test]
    async fn caps_durable_across_reconnect() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "builder-0", exec, "acknowledged", 0.0).await;
        seed_execution(
            &db.pool,
            exec,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm",
            None,
            None,
        )
        .await;
        // One committed chunk holding 2 GiB of accounted bytes — over
        // the 1 GiB default cap.
        sqlx::query(
            "INSERT INTO drv_log_chunks \
                 (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, s3_key, \
                  accounted_bytes) \
             VALUES ($1, $2, 0, 0, 50, 1, 'logs/cap/x', 2147483648)",
        )
        .bind(exec)
        .bind(Uuid::now_v7())
        .execute(&db.pool)
        .await
        .unwrap();

        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, exec),
            caps(),
        )
        .await
        .expect_err("an over-cap execution must be rejected at open");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "cap"
        );
    }

    /// The chunk-count cap is durable the same way.
    #[tokio::test]
    async fn chunk_cap_durable_at_open() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let d = seed_derivation(&db.pool, DRV).await;
        seed_assignment(&db.pool, d, "builder-0", exec, "acknowledged", 0.0).await;
        seed_execution(
            &db.pool,
            exec,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm",
            None,
            None,
        )
        .await;
        for seq in 0..3 {
            seed_chunk(&db.pool, exec, seq, i64::from(seq) * 5, 5).await;
        }

        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, exec),
            OpenCaps {
                per_exec_byte_cap: u64::MAX,
                max_chunks_per_exec: 3,
            },
        )
        .await
        .expect_err("an at-chunk-cap execution must be rejected at open");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "cap"
        );
    }

    // r[verify store.log.append-auth+2]
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
            caps(),
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

        let ok = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, exec),
            caps(),
        )
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

        let err = check_append_open(
            &db.pool,
            &claims("builder-0", DRV),
            &header(DRV_PATH, exec),
            caps(),
        )
        .await
        .expect_err("a complete log must be sealed against further appends");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "complete"
        );
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
            caps(),
        )
        .await
        .expect_err("no assignment recorded means nothing to authorize against");
        // The claimed-exec rejection class: indistinguishable from a
        // rewritten-in-place attempt, and permanently fatal either way.
        assert_eq!(err.code(), tonic::Code::FailedPrecondition, "{err:?}");
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "superseded"
        );
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
            caps(),
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

    /// W10-BO (bug_139): a manifest INSERT that commits server-side
    /// but surfaces as an error (lost response → CutError::Manifest,
    /// or a watchdog-abandoned cut dropped post-commit) is re-cut
    /// under a freshly burned chunk_seq — the ON CONFLICT
    /// (exec, session, seq) idempotence is structurally dead for that
    /// retry shape, and duplicate rows covering overlapping
    /// [first_line, first_line + line_count) land in drv_log_chunks.
    /// The durable account must be total over MERGED coverage, never
    /// a bare count+sum — at the overlap quantifier: k contained
    /// duplicates, seed exact. Cells: a k=3 same-start re-cut chain
    /// (each retry extends the restored staged run), a disjoint
    /// committed chunk, a cross-session contained orphan resend, and
    /// an identical-interval pair (the tiebreak: exactly one
    /// survives).
    #[tokio::test]
    async fn log_seed_counts_merged_coverage_not_duplicate_rows() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let sess_a = Uuid::now_v7();
        let sess_b = Uuid::now_v7();
        let insert = |sess: Uuid, seq: i32, fl: i64, lc: i64, bytes: i64| {
            let pool = db.pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO drv_log_chunks \
                     (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, \
                      s3_key, accounted_bytes) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(exec)
                .bind(sess)
                .bind(seq)
                .bind(fl)
                .bind(lc)
                .bind(bytes / 2)
                .bind(format!("k/{sess}/{seq}"))
                .bind(bytes)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        // The commit-but-error chain (same session, same start,
        // growing run): [100,150) ⊂ [100,180) ⊂ [100,200).
        insert(sess_a, 0, 100, 50, 5_000).await;
        insert(sess_a, 1, 100, 80, 8_000).await;
        insert(sess_a, 2, 100, 100, 10_000).await;
        // A disjoint committed chunk: [0,100).
        insert(sess_a, 3, 0, 100, 9_000).await;
        // Cross-session: the orphan [300,400) committed in session A
        // post-watchdog-drop; the builder reconnected and re-sent the
        // unacked range wider in session B: [300,440) ⊇ [300,400).
        insert(sess_a, 4, 300, 100, 7_000).await;
        insert(sess_b, 0, 300, 140, 9_500).await;
        // Identical intervals (a same-content re-cut whose retry took
        // exactly the same run): exactly ONE may count.
        insert(sess_a, 5, 500, 20, 2_000).await;
        insert(sess_a, 6, 500, 20, 2_000).await;

        let (bytes, chunks) = log_seed(&db.pool, exec).await.unwrap().merged_pair();
        assert_eq!(
            (bytes, chunks),
            (10_000 + 9_000 + 9_500 + 2_000, 4),
            "left: the bare count+sum seed inflated by every contained \
             duplicate (36000+16500 bytes over 8 rows) / right: the seed \
             equals merged coverage — one survivor per covered interval"
        );
    }

    // -- the raw monotone ceilings (merged_bug_002) -----------------------

    use super::super::chunks::MemoryLogChunkStore;
    use super::super::ingest::{IngestConfig, IngestSession};
    use rio_proto::types::BuildLogBatch;
    use std::time::Duration;

    /// One adversarial session: open through the production gate, seed
    /// a session from the GateOk, send `batches` (each `(first_line,
    /// n_lines)` of `content_len`-byte lines, tag-varied content so a
    /// dedup-shaped wrong fix cannot green these), cutting after each
    /// accepted batch, then drop the session (the reconnect). Returns
    /// the gate refusal if the OPEN was refused, else the `GateOk` and
    /// the per-batch outcomes.
    async fn attack_session(
        pool: &PgPool,
        store: &MemoryLogChunkStore,
        exec: Uuid,
        caps: OpenCaps,
        batches: &[(u64, u64)],
        content_len: usize,
        tag: usize,
    ) -> Result<(GateOk, Vec<super::super::ingest::AcceptOutcome>), Status> {
        let gate_ok =
            finish_open(pool, "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".into(), exec, caps).await?;
        let mut session = IngestSession::new(
            &gate_ok,
            Uuid::now_v7(),
            IngestConfig {
                per_exec_byte_cap: caps.per_exec_byte_cap,
                cut_threshold_bytes: u64::MAX, // manual cuts only
                cut_interval: Duration::from_secs(60),
            },
        );
        let mut outcomes = Vec::new();
        for (b, &(first, n_lines)) in batches.iter().enumerate() {
            let lines: Vec<Vec<u8>> = (0..n_lines)
                .map(|i| {
                    let mut l = vec![b'x'; content_len];
                    let tag = format!("t{tag}b{b}l{i}");
                    let tag = tag.as_bytes();
                    l[..tag.len().min(content_len)]
                        .copy_from_slice(&tag[..tag.len().min(content_len)]);
                    l
                })
                .collect();
            let outcome = session
                .accept(BuildLogBatch {
                    derivation_path: DRV_PATH.to_string(),
                    lines,
                    first_line_number: first,
                    executor_id: "builder-0".to_string(),
                })
                .expect("attack batches stay under the session byte cap");
            if matches!(
                outcome,
                super::super::ingest::AcceptOutcome::Accepted { .. }
            ) {
                let ack = session.cut(store, pool).await.expect("cut commits");
                assert_eq!(ack, Some(first + n_lines - 1), "the run cuts durably");
            }
            outcomes.push(outcome);
        }
        Ok((gate_ok, outcomes))
    }

    /// The execution's raw durable account, straight off the manifest:
    /// `(rows, accounted-byte sum)` over ALL `drv_log_chunks` rows —
    /// the monotone quantity the ceiling law bounds.
    async fn raw_account(pool: &PgPool, exec: Uuid) -> (i64, i64) {
        sqlx::query_as(
            "SELECT count(*), COALESCE(sum(accounted_bytes), 0)::BIGINT \
             FROM drv_log_chunks WHERE exec_id = $1",
        )
        .bind(exec)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// W11-A (merged_bug_002 HIGH). Proposition: **total durable
    /// accounted bytes per execution ≤ REPLAY_ALLOWANCE ×
    /// per_exec_byte_cap under adversarial replay** — cumulative across
    /// reconnect cycles, the law's own quantifier; the asserted
    /// quantity IS the frozen measure (Σ raw accounted_bytes over ALL
    /// rows; object-count growth is the sibling chunk quantity, owned
    /// by W11-A2). Born red against the pre-ceiling tree on the
    /// identical-interval orbit (12288 raw bytes across 6 cycles with
    /// the merged seed flat at 2048 — the verbatim red rides the
    /// raw-ceiling commit). In the full-close tree the same orbit is
    /// killed even earlier — the covered-replay consult mints NOTHING
    /// (cell i) — and the byte ceiling stays armed underneath for
    /// orbits the consult cannot see (cell ii: raw ≥ k×cap with
    /// merged < cap refuses at open, pinned at the BYTE-measure face;
    /// the two cap trips share the class-indistinguishable
    /// `cap_rejection("cap", …)`). Cell iii is the TWO-EXEC partition
    /// cell: the sibling's seed is unchanged and its open still admits
    /// — the measure's per-exec denominator (R28).
    // r[verify store.log.raw-ceiling]
    #[tokio::test]
    async fn raw_byte_ceiling_bounds_adversarial_replay_cycles() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let caps = OpenCaps {
            per_exec_byte_cap: 4096,
            max_chunks_per_exec: 100_000,
        };

        // -- Cell i: the identical-interval replay loop. 4 lines × 480
        // content bytes = 4 × 512 accounted = 2048 per cycle.
        for cycle in 0..6 {
            let (gate_ok, outcomes) =
                attack_session(&db.pool, &store, exec, caps, &[(0, 4)], 480, cycle)
                    .await
                    .expect("the loop's opens stay under every cap: nothing accrues");
            if cycle >= 1 {
                // The swapped-measure face: the merged seed is FLAT —
                // and post-fix so is raw, because the consult refuses
                // the covered replay at the write path.
                assert_eq!(
                    gate_ok.seed.merged_bytes, 2048,
                    "the merged seed stays flat under identical-interval replay"
                );
                assert_eq!(
                    outcomes,
                    vec![super::super::ingest::AcceptOutcome::CoveredReplay { durable_through: 3 }],
                    "the covered replay is dropped at the write path"
                );
            }
        }
        let (raw_rows, raw_bytes) = raw_account(&db.pool, exec).await;
        assert!(
            raw_bytes <= 2 * 4096,
            "left (pre-fix): the identical-interval replay loop durably minted \
             {raw_bytes} raw accounted bytes ({raw_rows} manifest rows) across \
             open→replay→cut→reconnect cycles with the merged seed flat at 2048 \
             — uncharged attacker-mintable storage / right: cumulative raw \
             accounted bytes per exec stay ≤ REPLAY_ALLOWANCE×cap = 8192"
        );
        assert_eq!(
            (raw_rows, raw_bytes),
            (1, 2048),
            "the write-path arm kills the identical-interval mint entirely: \
             one durable chunk, ever"
        );
        assert_eq!(store.len(), 1);

        // -- Cell ii: the ceiling stays armed beneath the consult. An
        // execution whose raw account reached k×cap while merged stayed
        // under cap (duplicate rows the write path minted before the
        // consult existed, or any future consult-evading byte orbit) is
        // refused at open, at the BYTE-measure face.
        let exec2 = Uuid::now_v7();
        let sibling = Uuid::now_v7();
        seed_chunk(&db.pool, sibling, 0, 0, 5).await;
        let sibling_seed_before = log_seed(&db.pool, sibling).await.unwrap();
        for seq in 0..5 {
            // Five identical-interval rows of 1800 accounted bytes:
            // merged = 1800 < 4096, raw = 9000 ≥ 8192.
            sqlx::query(
                "INSERT INTO drv_log_chunks \
                 (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, \
                  s3_key, accounted_bytes) \
                 VALUES ($1, $2, $3, 0, 4, 1, $4, 1800)",
            )
            .bind(exec2)
            .bind(Uuid::now_v7())
            .bind(seq)
            .bind(format!("ceil/{exec2}/{seq}"))
            .execute(&db.pool)
            .await
            .unwrap();
        }
        let status = finish_open(
            &db.pool,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".into(),
            exec2,
            caps,
        )
        .await
        .expect_err("raw bytes at k×cap with merged under cap must refuse at open");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status:?}");
        assert_eq!(
            status
                .metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "cap",
            "the raw ceiling shares the permanent cap class"
        );
        assert!(
            status.message().contains("byte"),
            "the refusal names the byte quantity: {:?}",
            status.message()
        );

        // -- Cell iii: the partition cell. The sibling exec's seed is
        // unchanged and its open still admits — the ceiling's
        // denominator is per-exec.
        assert_eq!(
            log_seed(&db.pool, sibling).await.unwrap(),
            sibling_seed_before,
            "the attacker exec's ceiling trip must not move the sibling's seed"
        );
        finish_open(
            &db.pool,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".into(),
            sibling,
            caps,
        )
        .await
        .expect("the sibling exec's open admits: its budget is unconsumed");
    }

    /// W11-A2 (merged_bug_002, the chunk-axis orbit). Proposition:
    /// **total durable manifest rows per execution ≤ REPLAY_ALLOWANCE
    /// × max_chunks_per_exec under adversarial small-chunk replay** —
    /// the sibling quantity's own red, born failing against the
    /// pre-ceiling tree (9 rows minted with merged chunks flat at 1).
    /// The orbit here is COVER-THEN-PRUNE — blocks of fresh single-line
    /// rows pruned by a one-new-line containing wide — which the
    /// write-path consult structurally cannot stop (every batch carries
    /// ≥1 new line) and every byte quantity sleeps through (tiny
    /// lines): merged chunks stay far under the cap while raw rows
    /// grow ~3 per block. The open refuses BY THE CHUNK CEILING
    /// (raw_rows ≥ k×max_chunks_per_exec, the row-measure face) — the
    /// raw-row arm is load-bearing in the full-close tree, not just
    /// the cherry-pick.
    // r[verify store.log.raw-ceiling]
    #[tokio::test]
    async fn raw_row_ceiling_bounds_small_chunk_replay_orbit() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let caps = OpenCaps {
            per_exec_byte_cap: 1024 * 1024 * 1024,
            max_chunks_per_exec: 6,
        };

        // Per block at coverage end c: session A mints two fresh
        // singles [c,c+1), [c+1,c+2); session B mints the containing
        // wide [c,c+3) (one new line — never fully covered, so the
        // consult admits it), which prunes A's singles from the merged
        // account. 8-byte lines: 40 accounted bytes each. The orbit
        // holds a GAP AT LINE 0 (c starts at 1, line 0 never written):
        // contiguous-from-zero coverage would seed the session floor
        // and the floor trim would behead every wide into a fresh
        // single, collapsing the prune — the honest contiguous shape
        // is defended by the floor, so the adversarial orbit lives in
        // the gapped shapes, which is exactly why the raw-row ceiling
        // (not any coverage heuristic) is the load-bearing bound.
        let mut refusal: Option<(usize, Status)> = None;
        let mut c: u64 = 1;
        'blocks: for block in 0..8usize {
            for (open, batches) in [(0, vec![(c, 1), (c + 1, 1)]), (1, vec![(c, 3)])] {
                match attack_session(&db.pool, &store, exec, caps, &batches, 8, block * 2 + open)
                    .await
                {
                    Ok((gate_ok, outcomes)) => {
                        assert!(
                            outcomes.iter().all(|o| matches!(
                                o,
                                super::super::ingest::AcceptOutcome::Accepted { .. }
                            )),
                            "the orbit evades the covered-replay consult: every \
                             batch carries a new line, got {outcomes:?}"
                        );
                        assert!(
                            u64::from(gate_ok.seed.merged_chunks) < 6,
                            "merged chunks stay under the cap: the prune hides the orbit"
                        );
                    }
                    Err(status) => {
                        refusal = Some((block * 2 + open, status));
                        break 'blocks;
                    }
                }
            }
            c += 3;
        }

        let (raw_rows, _raw_bytes) = raw_account(&db.pool, exec).await;
        assert!(
            raw_rows <= 2 * 6,
            "left (pre-fix): the small-chunk replay orbit durably minted \
             {raw_rows} manifest rows / objects with merged chunks pruned flat \
             — unbounded object-count mint below every byte quantity / right: \
             cumulative raw rows per exec stay ≤ REPLAY_ALLOWANCE×max_chunks \
             = 12 because the open refuses at the raw row ceiling"
        );
        let (open_idx, status) = refusal.expect(
            "left (pre-fix): every open admits (merged chunks pruned under the \
             cap forever) / right: the open refuses once raw rows reach the \
             ceiling",
        );
        assert_eq!(
            open_idx, 8,
            "four full blocks mint 3 rows each (raw 12 = ceiling); block 5's \
             first open refuses"
        );
        assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status:?}");
        assert_eq!(
            status
                .metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "cap"
        );
        // The measure face: the CHUNK ceiling fired (not the byte arm).
        assert!(
            status.message().contains("chunk"),
            "the refusal names the row/chunk quantity: {:?}",
            status.message()
        );
        let seed = log_seed(&db.pool, exec).await.unwrap();
        assert_eq!(
            (seed.merged_chunks, seed.raw_rows),
            (4, 12),
            "the algebras diverge: four surviving wides vs twelve raw rows"
        );
        assert_eq!(store.len(), 12, "objects stop minting at the refused open");
    }

    /// W11-B (merged_bug_002). Proposition: **honest-retry forgiveness
    /// is preserved under the new arms — a disconnect-resume charges
    /// each line once**, at the session lifecycle's own quantifier
    /// (across reconnect). Two cells: (a) the resumed UNCOVERED range
    /// charges exactly once (the merged axis's own face — the ceilings
    /// must not break it); (b) a replay of the FULLY-COVERED range is
    /// dropped uncharged and un-written via `CoveredReplay`, with the
    /// ack value carrying the manifest truth. Falsifiable both ways: a
    /// consult that over-drops kills cell (a); one that under-drops
    /// (or a seed regression) kills cell (b).
    #[tokio::test]
    async fn honest_resume_charges_once_and_covered_replay_drops_uncharged() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let caps = OpenCaps {
            per_exec_byte_cap: 1024 * 1024,
            max_chunks_per_exec: 100,
        };
        let config = || IngestConfig {
            per_exec_byte_cap: 1024 * 1024,
            cut_threshold_bytes: u64::MAX,
            cut_interval: Duration::from_secs(60),
        };
        let mk_lines = |first: u64, n: u64| -> Vec<Vec<u8>> {
            (0..n)
                .map(|i| format!("line-{}", first + i).into_bytes())
                .collect()
        };

        // Session 1: lines [0,10) accepted, cut, durable.
        let gate1 = finish_open(
            &db.pool,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".into(),
            exec,
            caps,
        )
        .await
        .expect("fresh exec admits");
        let mut s1 = IngestSession::new(&gate1, Uuid::now_v7(), config());
        s1.accept(BuildLogBatch {
            derivation_path: DRV_PATH.into(),
            lines: mk_lines(0, 10),
            first_line_number: 0,
            executor_id: String::new(),
        })
        .unwrap();
        assert_eq!(s1.cut(&store, &db.pool).await.unwrap(), Some(9));
        drop(s1); // disconnect

        // Session 2: the reconnect. The gate seeds the merged account
        // and the covered ranges.
        let gate2 = finish_open(
            &db.pool,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".into(),
            exec,
            caps,
        )
        .await
        .expect("the resumed exec admits: merged seed under cap, raw under ceiling");
        let durable_bytes = gate2.seed.merged_bytes;
        assert!(durable_bytes > 0, "session 1's cut is durably accounted");
        assert_eq!(
            gate2.seed.raw_bytes, durable_bytes,
            "no duplicates yet: the two algebras agree"
        );
        let mut s2 = IngestSession::new(&gate2, Uuid::now_v7(), config());

        // Cell (b): the full committed-but-unacked replay of [0,10) is
        // already covered — dropped uncharged, acked from the manifest.
        let outcome = s2
            .accept(BuildLogBatch {
                derivation_path: DRV_PATH.into(),
                lines: mk_lines(0, 10),
                first_line_number: 0,
                executor_id: String::new(),
            })
            .unwrap();
        assert_eq!(
            outcome,
            super::super::ingest::AcceptOutcome::CoveredReplay { durable_through: 9 },
            "a fully-covered replay is dropped with the manifest-truth ack"
        );
        assert_eq!(
            s2.cut(&store, &db.pool).await.unwrap(),
            None,
            "nothing was buffered: the covered replay cannot mint a chunk"
        );

        // Cell (a): the uncovered resume [10,20) is accepted and
        // charged exactly once.
        let outcome = s2
            .accept(BuildLogBatch {
                derivation_path: DRV_PATH.into(),
                lines: mk_lines(10, 10),
                first_line_number: 10,
                executor_id: String::new(),
            })
            .unwrap();
        assert!(
            matches!(
                outcome,
                super::super::ingest::AcceptOutcome::Accepted { .. }
            ),
            "the uncovered resume is accepted, got {outcome:?}"
        );
        assert_eq!(s2.cut(&store, &db.pool).await.unwrap(), Some(19));

        // The account after the resume: disjoint coverage, zero
        // duplicate rows — merged == raw on both quantities, and the
        // resumed range charged once.
        let gate3 = finish_open(
            &db.pool,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".into(),
            exec,
            caps,
        )
        .await
        .expect("still admittable");
        assert_eq!(
            gate3.seed.merged_bytes, gate3.seed.raw_bytes,
            "an honest resume mints no duplicate rows: the algebras agree"
        );
        assert_eq!(gate3.seed.raw_rows, 2, "one chunk per session, no replays");
        assert_eq!(store.len(), 2);
    }

    /// W11-E (bug_032). Proposition: **re-sent committed bytes are
    /// never re-charged** — at the session lifecycle's own quantifier,
    /// across reconnect — and the cap trip threshold is unreachable
    /// below true content. The pre-fix shape seeded `accepted_bytes`
    /// from exec-lifetime merged coverage while the session floor was
    /// zero, so a reconnect frame straddling the durable prefix
    /// re-charged the committed lines and tripped the permanent
    /// `FAILED_PRECONDITION` cap below the true cap, abandoning the
    /// tail. Population includes the STRADDLING-CHUNK cell (the
    /// cross-WO composition pin against WO-S1-1's frozen measure): the
    /// stored chunk and its accounted bytes contain ONLY
    /// above-watermark lines — bytes durably written == bytes raw
    /// charged, never written-but-uncharged.
    #[tokio::test]
    async fn reconnect_replay_of_committed_tail_is_not_recharged() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        // 50-byte lines charge 82 accounted each. Ten committed lines
        // = 820; a 12-line straddling replay = 984; the cap admits the
        // true content (820 + 164 new = 984 ≤ 1024) but not a
        // double-charge (820 + 984 = 1804).
        let caps = OpenCaps {
            per_exec_byte_cap: 1024,
            max_chunks_per_exec: 100,
        };
        let config = || IngestConfig {
            per_exec_byte_cap: 1024,
            cut_threshold_bytes: u64::MAX,
            cut_interval: Duration::from_secs(60),
        };
        let mk_lines = |first: u64, n: u64| -> Vec<Vec<u8>> {
            (0..n)
                .map(|i| {
                    let mut l = vec![b'x'; 50];
                    let tag = format!("L{}", first + i);
                    l[..tag.len()].copy_from_slice(tag.as_bytes());
                    l
                })
                .collect()
        };

        // Session 1: lines [0,10) accepted and cut; the acks are lost
        // (fire-and-forget drain) — the builder still holds them.
        let gate1 = finish_open(
            &db.pool,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".into(),
            exec,
            caps,
        )
        .await
        .expect("fresh exec admits");
        let mut s1 = IngestSession::new(&gate1, Uuid::now_v7(), config());
        s1.accept(BuildLogBatch {
            derivation_path: DRV_PATH.into(),
            lines: mk_lines(0, 10),
            first_line_number: 0,
            executor_id: String::new(),
        })
        .unwrap();
        assert_eq!(s1.cut(&store, &db.pool).await.unwrap(), Some(9));
        drop(s1);

        // Session 2: the reconnect replays the un-acked frame [0,12) —
        // ten committed lines plus two new ones, one straddling frame.
        let gate2 = finish_open(
            &db.pool,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".into(),
            exec,
            caps,
        )
        .await
        .expect("the reconnect admits");
        assert_eq!(gate2.seed.merged_bytes, 820);
        let mut s2 = IngestSession::new(&gate2, Uuid::now_v7(), config());
        let outcome = s2
            .accept(BuildLogBatch {
                derivation_path: DRV_PATH.into(),
                lines: mk_lines(0, 12),
                first_line_number: 0,
                executor_id: String::new(),
            })
            .expect(
                "left (pre-fix): the straddling replay re-charges its committed \
                 prefix (820 + 984 = 1804 > 1024) and trips the permanent cap \
                 below true content — the tail is abandoned / right: the \
                 below-floor prefix is trimmed; only the two new lines charge \
                 (820 + 164 = 984 ≤ 1024)",
            );
        assert!(
            matches!(
                outcome,
                super::super::ingest::AcceptOutcome::Accepted { .. }
            ),
            "the straddling frame's tail is accepted, got {outcome:?}"
        );
        assert_eq!(
            s2.cut(&store, &db.pool).await.unwrap(),
            Some(11),
            "the new tail cuts durably"
        );

        // The straddling-chunk composition cell: the stored chunk holds
        // ONLY the above-watermark lines and charges exactly them.
        let (first_line, line_count, accounted): (i64, i64, i64) = sqlx::query_as(
            "SELECT first_line, line_count, accounted_bytes FROM drv_log_chunks \
             WHERE exec_id = $1 ORDER BY first_line DESC LIMIT 1",
        )
        .bind(exec)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            (first_line, line_count, accounted),
            (10, 2, 164),
            "the stored chunk contains only above-watermark lines and its \
             accounted bytes charge exactly them (written == charged)"
        );

        // And the next seed agrees on both algebras: no duplicate rows,
        // every line charged once.
        let gate3 = finish_open(
            &db.pool,
            "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".into(),
            exec,
            caps,
        )
        .await
        .expect("still under every cap: the true content never trips");
        assert_eq!(
            (gate3.seed.merged_bytes, gate3.seed.raw_bytes),
            (984, 984),
            "re-sent committed bytes were never re-charged"
        );
    }

    /// W11-D (merged_bug_002, the measure-freeze pin). The seed SQL is
    /// DIFFERENTIALLY pinned against the kernel account algebra
    /// ([`rio_log_kernel::log_account`], whose laws are kani-proven):
    /// over a row set exercising every duplicate shape the write path
    /// mints, all four quantities agree. A strawman that re-swaps any
    /// measure — seeding raw from the coverage projection (the
    /// merged_bug_002 shape) or merged from the bare sum (the bug_139
    /// shape) — diverges from the kernel fold on the duplicate rows
    /// and goes red here; the merged_bug_002 edit is unwritable
    /// without a red.
    #[tokio::test]
    async fn log_seed_sql_matches_the_kernel_account_algebra() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let sess_a = Uuid::now_v7();
        let sess_b = Uuid::now_v7();
        let rows = [
            // Same-session re-cut chain.
            (sess_a, 0i32, 100i64, 50i64, 5_000i64),
            (sess_a, 1, 100, 80, 8_000),
            (sess_a, 2, 100, 100, 10_000),
            // Disjoint chunk.
            (sess_a, 3, 0, 100, 9_000),
            // Cross-session wide over an orphan.
            (sess_a, 4, 300, 100, 7_000),
            (sess_b, 0, 300, 140, 9_500),
            // Identical-interval pair.
            (sess_a, 5, 500, 20, 2_000),
            (sess_a, 6, 500, 20, 2_000),
        ];
        for (sess, seq, fl, lc, bytes) in rows {
            sqlx::query(
                "INSERT INTO drv_log_chunks \
                 (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, \
                  s3_key, accounted_bytes) \
                 VALUES ($1, $2, $3, $4, $5, 1, $6, $7)",
            )
            .bind(exec)
            .bind(sess)
            .bind(seq)
            .bind(fl)
            .bind(lc)
            .bind(format!("dp/{sess}/{seq}"))
            .bind(bytes)
            .execute(&db.pool)
            .await
            .unwrap();
        }

        let sql_seed = log_seed(&db.pool, exec).await.unwrap();
        let kernel = rio_log_kernel::log_account(
            &rows
                .iter()
                .map(|&(sess, seq, fl, lc, bytes)| rio_log_kernel::AccountRow {
                    first_line: fl as u64,
                    line_count: lc as u64,
                    accounted_bytes: bytes as u64,
                    // PG orders uuids bytewise — exactly Uuid's u128.
                    key: (sess.as_u128(), seq as u32),
                })
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            (
                sql_seed.merged_bytes,
                u64::from(sql_seed.merged_chunks),
                sql_seed.raw_bytes,
                sql_seed.raw_rows
            ),
            (
                kernel.merged_bytes,
                kernel.merged_chunks,
                kernel.raw_bytes,
                kernel.raw_rows
            ),
            "the seed SQL and the kernel account algebra are ONE measure: \
             merged = idempotent union, raw = monotone sum, on both quantities"
        );
        // The duplicate rows make the algebras diverge — the cell where
        // a measure swap is visible at all.
        assert!(sql_seed.raw_bytes > sql_seed.merged_bytes);
        assert!(sql_seed.raw_rows > u64::from(sql_seed.merged_chunks));
    }
}

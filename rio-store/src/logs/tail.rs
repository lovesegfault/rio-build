//! The manifest-driven log read path for `LogService.TailLog`.
//!
//! A finished (or in-progress) execution's log is the union of its
//! `drv_log_chunks` manifest rows, each describing one immutable,
//! contiguous-run zstd object. Two ingest sessions for one execution (a
//! store-replica failover mid-build) can produce *overlapping* line
//! ranges — the same line stored in two chunks — so the read path's job
//! is to turn the manifest into one ordered, deduplicated stream of
//! `(line_number, bytes)` pairs:
//!
//! 1. [`read_manifest_range`] selects the chunks whose range intersects
//!    `[since_line, ∞)`, ordered by `(first_line, session_id)`.
//! 2. [`read_chunk`] fetches + decompresses ONE chunk and returns its
//!    contribution above a [`LineCursor`] watermark — the unit the
//!    `TailLog` handler streams (for follow and non-follow reads
//!    alike), one chunk resident at a time.
//! 3. [`authorize_tail`] turns a `TailLogRequest`'s `(derivation,
//!    exec_id)` pair plus the verified tenant (if any) into the
//!    [`OwnedExec`] the serve layer requires — resolution and
//!    build-membership ownership in one gate.
//!
//! The completeness predicate (`TailLogChunk.is_complete`) is
//! `super::gate::final_claim_for` — the same facts that seal the
//! write path. It is deliberately not reimplemented here.
//!
//! No gRPC, no live-tail subscription, no cross-replica proxy — those
//! are the handler's job (a sibling module).

use rio_common::grpc::StatusExt;
use rio_nix::store_path::drv_log_hash;
use sqlx::PgPool;
use tonic::Status;
use tracing::{error, warn};
use uuid::Uuid;

use super::chunks::{LogChunkError, LogChunkStore, decompress_lines};
use super::kernel::{ChunkVisit, ObjectDivergence, visit_chunk, visit_object};

/// One `drv_log_chunks` manifest row, as the read path needs it.
///
/// `first_line`/`line_count` describe the contiguous run
/// `[first_line, first_line + line_count)` the object at `s3_key`
/// holds. They are stored as `BIGINT` (the ingest path rejects line
/// numbers that cannot round-trip through `i64`), so the `u64`
/// conversion at the SQL boundary is infallible for any row the ingest
/// path wrote. `exec_id` is carried so failure paths deep in the read
/// pipeline (a missing object, a corrupt frame) can name the affected
/// execution as a structured field without re-parsing the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRef {
    pub exec_id: Uuid,
    pub s3_key: String,
    pub first_line: u64,
    pub line_count: u64,
}

/// The chunks whose line range intersects `[since_line, ∞)`, ordered by
/// `(first_line, session_id)`.
///
/// The ordering is load-bearing twice over: it is what makes the
/// [`LineCursor`] watermark a complete dedup (chunks are visited in
/// ascending `first_line` order, so the yielded set is always a
/// contiguous-or-gapped *prefix* and "already yielded" reduces to
/// "below the watermark"), and the `session_id` tiebreak makes the
/// winner of a same-`first_line` overlap deterministic across reads,
/// restarts, and replicas.
///
/// `first_line + line_count > $since` is a filter over the
/// `(exec_id, first_line)` index scan, not an index condition — fine at
/// the ≤ a-few-thousand chunks a single execution can accumulate.
pub async fn read_manifest_range(
    pool: &PgPool,
    exec_id: Uuid,
    since_line: u64,
) -> Result<Vec<ChunkRef>, Status> {
    // A since_line past i64::MAX cannot intersect any storable chunk
    // (the ingest path rejects line numbers above i64::MAX); clamping
    // keeps the bind in range and yields the correct empty set.
    let since = i64::try_from(since_line).unwrap_or(i64::MAX);
    // Runtime query: drv_log_chunks is store-owned (no cross-service
    // contract to enforce).
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT s3_key, first_line, line_count FROM drv_log_chunks \
         WHERE exec_id = $1 AND first_line + line_count > $2 \
         ORDER BY first_line, session_id",
    )
    .bind(exec_id)
    .bind(since)
    .fetch_all(pool)
    .await
    .status_internal("TailLog: manifest range read")?;

    rows.into_iter()
        .map(|(s3_key, first_line, line_count)| {
            Ok(ChunkRef {
                exec_id,
                s3_key,
                // Negative values are unrepresentable for rows the ingest
                // path wrote; a hand-edited row that violates that is a
                // corrupt manifest, not a client error.
                first_line: u64::try_from(first_line)
                    .map_err(|_| corrupt_manifest_row(&exec_id, first_line))?,
                line_count: u64::try_from(line_count)
                    .map_err(|_| corrupt_manifest_row(&exec_id, line_count))?,
            })
        })
        .collect()
}

fn corrupt_manifest_row(exec_id: &Uuid, value: i64) -> Status {
    // Operator-facing: the manifest can only get a negative line number
    // by hand-editing — but a corrupt row makes the execution's log
    // unreadable, so it is an `error!`, not a `warn!`. The detail stays
    // server-side; the client gets a redacted internal error.
    error!(%exec_id, value, "drv_log_chunks row with a negative line number/count");
    Status::internal("TailLog: corrupt manifest row")
}

/// The overlap-dedup watermark: the next line number not yet yielded.
///
/// Valid only for chunks visited in ascending `first_line` order (the
/// [`read_manifest_range`] ordering): under that order the set of
/// already-yielded line numbers is always `[since_line, next_line)`
/// minus any genuine storage gaps, so "skip lines already yielded"
/// reduces to "skip lines below `next_line`".
#[derive(Debug, Clone, Copy)]
pub struct LineCursor {
    next_line: u64,
}

impl LineCursor {
    pub fn new(since_line: u64) -> Self {
        Self {
            next_line: since_line,
        }
    }

    /// The next line number a subsequent chunk could contribute. After
    /// draining a manifest range this is one past the last stored line —
    /// the `since_line` a follow-up read (or the live-tail handoff)
    /// resumes from.
    pub fn next_line(&self) -> u64 {
        self.next_line
    }

    /// Advance the watermark to a kernel-verdict position. The
    /// argument is sealed (merged_bug_205): only
    /// `ChunkVisit::advance()` can mint a [`CursorAdvance`], so a
    /// serve path can move the watermark exclusively to a post-visit
    /// position some verdict computed — the open-coded
    /// `filter(>= cursor)` + `advance_to(end + 1)` shape that silently
    /// absorbed residual gaps no longer typechecks. A backwards
    /// advance is a no-op — the watermark is monotone by definition.
    pub fn advance_to(&mut self, adv: rio_log_kernel::CursorAdvance) {
        self.next_line = self.next_line.max(adv.to());
    }
}

// r[impl store.log.session-keyed]
/// Fetch, decompress, and dedup ONE chunk, returning its contribution
/// above the cursor as `(line_number, bytes)` pairs in increasing
/// line-number order.
///
/// One chunk's decompressed lines are resident at a time; the caller
/// (the `TailLog` handler) re-chunks the output into ≤256-line response
/// messages and drops it before fetching the next chunk.
///
/// A chunk whose manifest row exists but whose object GET returns
/// `NotFound` is **data loss** (the manifest is written only after the
/// object PUT succeeds): it is surfaced as an `Internal` error naming
/// the key so the operator can find the hole, never silently skipped —
/// a silent skip would present a gapped log as complete.
// r[impl store.log.read-divergence+1]
pub async fn read_chunk(
    store: &dyn LogChunkStore,
    chunk: &ChunkRef,
    remaining: &[ChunkRef],
    cursor: &mut LineCursor,
) -> Result<Vec<(u64, Vec<u8>)>, Status> {
    // A degenerate zero-line chunk (the cutter never writes one, but the
    // manifest is just a table) has nothing to contribute, and a chunk
    // whose last line is below the watermark is a same-range chunk from
    // a second session whose lines were all already yielded. Both take
    // the cheap skip branch — no GET. The verdict comes from the pure
    // kernel (`kernel::visit_chunk`), evaluated here against the
    // manifest's claimed line count.
    if visit_chunk(cursor.next_line, chunk.first_line, chunk.line_count).is_empty() {
        return Ok(Vec::new());
    }

    let blob = store.get(&chunk.s3_key).await.map_err(|e| match e {
        LogChunkError::NotFound { key } => {
            // The one condition in this file that means "lines are
            // gone": the manifest row is written only after the object
            // PUT succeeds, so a missing object is data loss (a deleted
            // or lifecycle-expired object whose row outlived it), not a
            // race. error! so the operator has a signal beyond the
            // client-visible Status. (A read-side data-loss counter
            // lands with the handler's metrics.)
            error!(
                s3_key = %key,
                exec_id = %chunk.exec_id,
                "TailLog: manifest references a missing chunk object (data loss)"
            );
            metrics::counter!(
                "rio_store_log_read_data_loss_total",
                "reason" => "missing_object"
            )
            .increment(1);
            Status::internal(format!(
                "TailLog: manifest references a missing chunk object (data loss): {key}"
            ))
        }
        other => {
            warn!(key = %chunk.s3_key, error = %other, "TailLog: chunk fetch failed");
            Status::internal("TailLog: chunk fetch failed")
        }
    })?;
    let lines = decompress_lines(&blob).map_err(|e| {
        warn!(key = %chunk.s3_key, error = %e, "TailLog: chunk decode failed");
        Status::internal(format!(
            "TailLog: stored chunk is not decodable (corruption): {}",
            chunk.s3_key
        ))
    })?;

    // The dedup proper, with the manifest/object clamp: the kernel
    // evaluates the visit over `min(manifest_count, object_count)`, so
    // the served range and the watermark are bounded by the manifest
    // claim BY CONSTRUCTION, and any disagreement between the row and
    // the object is classified for policy here:
    //
    // - A LONG object (holds more lines than the row claims) has its
    //   excess discarded — served, those lines would carry the NEXT
    //   chunk's line numbers (garbage attribution) and the advanced
    //   watermark would suppress that chunk's genuine lines. Disclosed
    //   via the data-loss counter and a warn; the lines that ARE
    //   claimed serve normally.
    // - A SHORT object (holds fewer lines than the row claims) is data
    //   loss with `NotFound` parity: the manifest promised lines that
    //   exist in no object. `Internal` naming the key, never a
    //   silently shorter stream presented as complete.
    //
    // Gap disposition (the enum forces the choice): the manifest walk
    // SERVES across a forward jump. A hole between manifest rows is
    // genuine storage loss — chunks are committed in line order, so a
    // missing span has no row to read — and the disclosure is the
    // completeness predicate (`manifest_covers_contiguously` drives
    // `is_complete=false` in the handler's final message), not a
    // refusal to serve the lines that DO exist.
    let object_visit = visit_object(
        cursor.next_line,
        chunk.first_line,
        chunk.line_count,
        lines.len() as u64,
    );
    match object_visit.divergence {
        Some(ObjectDivergence::ShortObject {
            missing_from,
            missing_until,
        }) => {
            error!(
                s3_key = %chunk.s3_key,
                exec_id = %chunk.exec_id,
                manifest_count = chunk.line_count,
                actual_count = lines.len(),
                missing_from,
                missing_until,
                "TailLog: chunk object holds fewer lines than its manifest row (data loss)"
            );
            // bug_233: the policy is COVERAGE, not arm ordering. A
            // missing span some remaining row covers is fully
            // servable — serve the clamped lines (the visit is
            // already clamped by construction) and let the covering
            // rows supply the rest. Only a span NO row covers is a
            // refusal — and a TYPED-permanent one, so the reader exit
            // law stops re-dialing an unservable hole instead of
            // wedging at 1 Hz forever.
            let rows: Vec<(u64, u64)> = remaining
                .iter()
                .map(|r| (r.first_line, r.line_count))
                .collect();
            match rio_log_kernel::short_object_policy(missing_from, missing_until, &rows) {
                rio_log_kernel::ShortObjectPolicy::ServeClamped => {
                    // A covered short is a DIVERGENCE disclosure, not
                    // data loss: every claimed line still serves (from
                    // the covering rows), so it must not feed
                    // rio_store_log_read_data_loss_total — that
                    // counter's contract is alert-on-ANY-increment for
                    // unrecoverable holes. The error! above carries
                    // the corruption-grade signal; the warn-severity
                    // divergence counter family is the m164 reroute.
                }
                rio_log_kernel::ShortObjectPolicy::UnservableHole => {
                    metrics::counter!(
                        "rio_store_log_read_data_loss_total",
                        "reason" => "short_object"
                    )
                    .increment(1);
                    let mut status = Status::internal(format!(
                        "TailLog: chunk object holds fewer lines than its manifest row \
                         (data loss, lines {missing_from}..{missing_until} missing): {}",
                        chunk.s3_key
                    ));
                    status.metadata_mut().insert(
                        rio_proto::LOG_UNSERVABLE_METADATA_KEY,
                        tonic::metadata::MetadataValue::from_static("short_object"),
                    );
                    return Err(status);
                }
            }
        }
        Some(ObjectDivergence::LongObject { excess }) => {
            warn!(
                key = %chunk.s3_key,
                manifest_count = chunk.line_count,
                actual_count = lines.len(),
                excess,
                "TailLog: chunk object holds more lines than its manifest row; \
                 excess discarded"
            );
            metrics::counter!(
                "rio_store_log_read_data_loss_total",
                "reason" => "overlong_object"
            )
            .increment(1);
        }
        None => {}
    }
    let visit = object_visit.visit;
    let (yield_from, yield_until) = match visit {
        ChunkVisit::Skip { .. } => {
            cursor.advance_to(visit.advance());
            return Ok(Vec::new());
        }
        ChunkVisit::Serve {
            yield_from,
            yield_until,
            ..
        } => (yield_from, yield_until),
        ChunkVisit::GapThenServe {
            yield_from,
            yield_until,
            ..
        } => (yield_from, yield_until),
    };
    let mut out = Vec::new();
    for (i, line) in lines.into_iter().enumerate() {
        let line_no = chunk.first_line.saturating_add(i as u64);
        if line_no < yield_from || line_no >= yield_until {
            continue;
        }
        out.push((line_no, line));
    }
    cursor.advance_to(visit.advance());
    Ok(out)
}

/// Test helper: drive [`read_chunk`] over a whole manifest range,
/// collecting every deduplicated line into one `Vec`.
///
/// Deliberately `#[cfg(test)]`: collecting a whole log re-creates the
/// exact whole-blob-in-memory profile this design exists to eliminate
/// (a 100 MiB log = ~100 MiB resident per concurrent reader). The
/// `TailLog` handler drives [`read_chunk`] directly for both follow and
/// non-follow reads, dropping each chunk's lines before fetching the
/// next.
#[cfg(test)]
async fn stream_chunks(
    store: &dyn LogChunkStore,
    refs: &[ChunkRef],
    since_line: u64,
) -> Result<Vec<(u64, Vec<u8>)>, Status> {
    let mut cursor = LineCursor::new(since_line);
    let mut out = Vec::new();
    for (i, chunk) in refs.iter().enumerate() {
        out.extend(read_chunk(store, chunk, &refs[i + 1..], &mut cursor).await?);
    }
    Ok(out)
}

/// Resolve a `TailLogRequest`'s `(derivation, exec_id)` pair to the
/// execution to read.
///
/// - A non-empty `pinned_exec_id` is used verbatim once it is shown to
///   exist (a `drv_executions` lifecycle row *or* at least one manifest
///   chunk). The write path admits appends only AFTER the lifecycle
///   row exists — the ordering law is single-homed at `logs/gate.rs`
///   check 3; cite it rather than restating — so the chunk leg is not
///   a pre-INSERT window: it is belt-and-braces for lifecycle/artifact
///   lifetime skew (a reaped lifecycle row whose log artifacts are
///   still readable). The `derivation` argument is not cross-checked against a
///   pinned execution: exec ids are unguessable UUIDv7s and `TailLog`
///   is a read-only, route-gated API — the pin *is* the selector.
/// - An empty `pinned_exec_id` resolves through the `latest_build_exec`
///   view: the newest **build-kind** execution for the derivation
///   (UUIDv7 mint order). The kind filter lives in the view definition
///   (migration 089), not here — a freshly-minted materialization
///   execution (which never has chunks) must not shadow the build whose
///   log the caller wants (`store.log.read-authority`).
///
/// The two failure modes are distinguishable `NotFound`s — "no log
/// recorded for execution …" (the caller pinned a bad id) vs "no
/// executions recorded for derivation …" (nothing was ever dispatched,
/// or it expired) — because they have different audiences: the first is
/// a dashboard deep-link gone stale, the second is `rio-cli logs` for a
/// derivation that never built.
///
/// Private: callers go through [`authorize_tail`], the sole
/// [`OwnedExec`] producer — resolution without the ownership gate is
/// unreachable from outside this module.
// r[impl obs.log.exec-keyed+2]
async fn resolve_exec(
    pool: &PgPool,
    derivation: &str,
    pinned_exec_id: &str,
) -> Result<Uuid, Status> {
    if !pinned_exec_id.is_empty() {
        let exec_id: Uuid = pinned_exec_id
            .parse()
            .map_err(|_| Status::invalid_argument("TailLog: exec_id is not a valid UUID"))?;
        // Compile-time checked: drv_executions is scheduler-owned (see
        // the STORE_READS contract entries).
        let lifecycle = sqlx::query_scalar!(
            r#"SELECT exec_id FROM drv_executions WHERE exec_id = $1"#,
            exec_id,
        )
        .fetch_optional(pool)
        .await
        .status_internal("TailLog: pinned execution lookup")?;
        if lifecycle.is_some() {
            return Ok(exec_id);
        }
        // Store-owned table → runtime query.
        let has_chunks: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM drv_log_chunks WHERE exec_id = $1)")
                .bind(exec_id)
                .fetch_one(pool)
                .await
                .status_internal("TailLog: pinned execution chunk lookup")?;
        if has_chunks {
            return Ok(exec_id);
        }
        return Err(Status::not_found(format!(
            "no log recorded for execution {exec_id}"
        )));
    }

    let drv_hash = drv_log_hash(derivation);
    // r[impl store.log.read-authority]
    // The kind-filtered view (089_log_authority) is THE unpinned resolver; the
    // `log-no-raw-latest-exec` policy check bans new raw
    // `ORDER BY exec_id DESC` reads of `drv_executions` so a second
    // kind-blind copy of this resolution cannot grow back. Runtime
    // query: the view's columns are pinned transitively by the
    // `STORE_READS` contract (`drv_executions.attempt_kind`).
    let latest: Option<Uuid> =
        sqlx::query_scalar("SELECT exec_id FROM latest_build_exec WHERE drv_hash = $1")
            .bind(&drv_hash)
            .fetch_optional(pool)
            .await
            .status_internal("TailLog: latest execution lookup")?;

    latest.ok_or_else(|| {
        Status::not_found(format!("no executions recorded for derivation {drv_hash}"))
    })
}

/// An execution the caller is authorized to read the log of.
///
/// Sole producer: [`authorize_tail`] — resolution and tenant ownership
/// fold into one gate, so a serve path that skips authorization does
/// not compile (`serve_tail` and the live-buffer lookup take this type,
/// never a raw [`Uuid`]).
#[derive(Debug, Clone, Copy)]
pub struct OwnedExec(Uuid);

impl OwnedExec {
    /// The authorized execution id.
    pub fn id(&self) -> Uuid {
        self.0
    }

    /// Test-only constructor for serve-layer unit tests that exercise
    /// chunk reading below the authorization gate. Production code has
    /// exactly one producer: [`authorize_tail`].
    #[cfg(test)]
    pub fn for_tests(exec_id: Uuid) -> Self {
        Self(exec_id)
    }
}

/// Resolve AND authorize a `TailLog` request: the sole [`OwnedExec`]
/// producer.
///
/// Ownership is **build-membership** over the production-written chain
/// `assignments → build_derivations → builds.tenant_id` (§5-S Q2,
/// 2026-06-04: any tenant whose build contains the content-addressed
/// derivation may read its execution logs; this amends round-1's
/// "derivation ownership" wording — the service-bypass prohibition is
/// unchanged). `derivations.tenant_id` was never production-written
/// (migration 095 census) and is read by NOTHING anymore.
///
/// Swept-assignment arm: the execution's own `drv_executions.drv_hash`
/// (server data, 32-char nixbase32) prefix-matches its `derivations`
/// row, then the same build-membership chain. The caller's request
/// string appears in **no ownership predicate** — a verbatim-own-drv +
/// foreign-pin request can no longer launder ownership through the
/// resolver's fallback (merged_bug_064-a).
///
/// Deny-with-claims is **absence-shaped**: the same `NotFound` text the
/// resolution arm produces for a row that does not exist (foreign ≡
/// never-built; the cross-tenant existence oracle of distinguishable
/// `PermissionDenied` is gone — merged_bug_064-c). `tenant == None`
/// (no JWT pubkey configured: the dev/VM posture) keeps the
/// distinguishable resolution errors.
///
/// Runtime queries over scheduler-owned tables; columns pinned by the
/// `STORE_READS` cross-service contract.
// r[impl store.log.tail-ownership]
pub async fn authorize_tail(
    pool: &PgPool,
    derivation: &str,
    pinned_exec_id: &str,
    tenant: Option<Uuid>,
) -> Result<OwnedExec, Status> {
    let exec_id = resolve_exec(pool, derivation, pinned_exec_id).await?;
    let Some(tenant) = tenant else {
        return Ok(OwnedExec(exec_id));
    };

    // Primary: the execution's assignment row → its build(s) → tenant.
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS( \
             SELECT 1 FROM assignments a \
             JOIN build_derivations bd USING (derivation_id) \
             JOIN builds b USING (build_id) \
             WHERE a.exec_id = $1 AND b.tenant_id = $2)",
    )
    .bind(exec_id)
    .bind(tenant)
    .fetch_one(pool)
    .await
    .status_internal("TailLog: ownership lookup")?;
    let owned = if owned {
        true
    } else {
        // Swept-assignment arm: key on the execution's OWN recorded
        // drv_hash (server data — no LIKE-metacharacter exposure, the
        // 32-char nixbase32 alphabet has no `%`/`_`), never on the
        // caller's request string.
        sqlx::query_scalar(
            "SELECT EXISTS( \
                 SELECT 1 FROM drv_executions e \
                 JOIN derivations d ON d.drv_hash LIKE e.drv_hash || '%' \
                 JOIN build_derivations bd USING (derivation_id) \
                 JOIN builds b USING (build_id) \
                 WHERE e.exec_id = $1 AND b.tenant_id = $2)",
        )
        .bind(exec_id)
        .bind(tenant)
        .fetch_one(pool)
        .await
        .status_internal("TailLog: ownership fallback lookup")?
    };
    if owned {
        return Ok(OwnedExec(exec_id));
    }
    // Absence-shaped deny: byte-identical to the resolution arm's
    // missing-row error so foreign and nonexistent are
    // indistinguishable to an authenticated caller.
    Err(if pinned_exec_id.is_empty() {
        Status::not_found(format!(
            "no executions recorded for derivation {}",
            drv_log_hash(derivation)
        ))
    } else {
        Status::not_found(format!("no log recorded for execution {exec_id}"))
    })
}

/// Test fixture: the PRODUCTION ownership shape — a `builds` row owned
/// by `tenant`, linked to `derivation_id` via `build_derivations`.
/// `derivations.tenant_id` was never production-written and is dropped
/// by migration 095; the `authz-fixture-policy` misc-check bans test
/// writes to it so fixtures cannot drift back to the dead shape
/// (merged_bug_064-b vacuity class).
#[cfg(test)]
pub(crate) async fn seed_production_ownership(
    pool: &PgPool,
    tenant: Uuid,
    derivation_id: Uuid,
) -> Uuid {
    let build_id: Uuid = sqlx::query_scalar(
        "INSERT INTO builds (tenant_id, status) VALUES ($1, 'active') RETURNING build_id",
    )
    .bind(tenant)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build_id)
        .bind(derivation_id)
        .execute(pool)
        .await
        .unwrap();
    build_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::chunks::{LogChunkStore, MemoryLogChunkStore, compress_lines, log_chunk_key};
    use crate::logs::gate::log_is_complete;
    use rio_test_support::TestDb;
    use sqlx::PgPool;
    use uuid::Uuid;

    /// The 32-char `drv_log_hash()` form used for chunk keys and
    /// `drv_executions.drv_hash`.
    const DRV_HASH_32: &str = "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm";
    /// A full store path that normalizes to [`DRV_HASH_32`].
    const DRV_PATH: &str = "/nix/store/0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-hello-2.12.drv";

    /// Seed one chunk: the `drv_log_chunks` manifest row AND the
    /// compressed object at the manifest's `s3_key`, from the same
    /// `lines` slice — the two cannot drift in a fixture built this way.
    /// Returns the s3 key.
    async fn seed_chunk(
        pool: &PgPool,
        store: &MemoryLogChunkStore,
        exec_id: Uuid,
        session_id: Uuid,
        seq: u32,
        first_line: u64,
        lines: &[&[u8]],
    ) -> String {
        let key = log_chunk_key(DRV_HASH_32, &exec_id, &session_id, seq);
        let owned: Vec<Vec<u8>> = lines.iter().map(|l| l.to_vec()).collect();
        let blob = compress_lines(&owned).unwrap();
        let byte_size = blob.len() as i64;
        store.put(&key, blob).await.unwrap();
        sqlx::query(
            "INSERT INTO drv_log_chunks \
                 (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, s3_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(exec_id)
        .bind(session_id)
        .bind(seq as i32)
        .bind(first_line as i64)
        .bind(lines.len() as i64)
        .bind(byte_size)
        .bind(&key)
        .execute(pool)
        .await
        .unwrap();
        key
    }

    /// Seed the scheduler-written lifecycle row for an execution.
    async fn seed_execution(
        pool: &PgPool,
        exec_id: Uuid,
        status: Option<&str>,
        final_line_count: Option<i64>,
    ) {
        sqlx::query(
            "INSERT INTO drv_executions \
                 (exec_id, drv_hash, executor_id, started_at, status, final_line_count) \
             VALUES ($1, $2, 'builder-0', now(), $3, $4)",
        )
        .bind(exec_id)
        .bind(DRV_HASH_32)
        .bind(status)
        .bind(final_line_count)
        .execute(pool)
        .await
        .unwrap();
    }

    /// `n` distinct lines whose content encodes `prefix` and the line
    /// number, so a dedup test can tell which session's copy of a line
    /// won.
    fn lines(prefix: &str, first: u64, n: u64) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| format!("{prefix}:{}", first + i).into_bytes())
            .collect()
    }
    fn line_refs(owned: &[Vec<u8>]) -> Vec<&[u8]> {
        owned.iter().map(Vec::as_slice).collect()
    }

    /// Chunks covering [0,100) [100,200) [200,300); since_line=150 must
    /// select only the 2nd and 3rd, and the streamed output must start
    /// at exactly line 150.
    #[tokio::test]
    async fn selects_only_intersecting_chunks() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let sess = Uuid::now_v7();
        for (seq, first) in [(0u32, 0u64), (1, 100), (2, 200)] {
            let content = lines("a", first, 100);
            seed_chunk(
                &db.pool,
                &store,
                exec,
                sess,
                seq,
                first,
                &line_refs(&content),
            )
            .await;
        }

        let refs = read_manifest_range(&db.pool, exec, 150).await.unwrap();
        assert_eq!(
            refs.iter().map(|c| c.first_line).collect::<Vec<_>>(),
            vec![100, 200],
            "only the chunks whose range intersects [150, ∞) are selected"
        );

        let out = stream_chunks(&store, &refs, 150).await.unwrap();
        assert_eq!(out.first().map(|(n, _)| *n), Some(150));
        assert_eq!(out.last().map(|(n, _)| *n), Some(299));
        assert_eq!(out.len(), 150);
    }

    // r[verify store.log.session-keyed]
    /// Session A covers [0,150), session B covers [100,300) with
    /// DIFFERENT bytes for the overlap. Every line 0..300 appears
    /// exactly once; lines 100-149 carry session A's bytes (the chunk
    /// with the lower first_line is visited first and the dedup keeps
    /// the first copy).
    #[tokio::test]
    async fn dedups_overlapping_sessions_keeps_first() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let sess_a = Uuid::now_v7();
        let sess_b = Uuid::now_v7();
        let a = lines("a", 0, 150);
        seed_chunk(&db.pool, &store, exec, sess_a, 0, 0, &line_refs(&a)).await;
        let b = lines("b", 100, 200);
        seed_chunk(&db.pool, &store, exec, sess_b, 0, 100, &line_refs(&b)).await;

        let refs = read_manifest_range(&db.pool, exec, 0).await.unwrap();
        let out = stream_chunks(&store, &refs, 0).await.unwrap();

        assert_eq!(out.len(), 300, "every line exactly once, no duplicates");
        for (i, (n, _)) in out.iter().enumerate() {
            assert_eq!(*n, i as u64, "strictly increasing, gap-free line numbers");
        }
        // The overlap carries session A's copy.
        assert_eq!(out[100].1, b"a:100".to_vec());
        assert_eq!(out[149].1, b"a:149".to_vec());
        // Past A's end, session B's copy is the only one.
        assert_eq!(out[150].1, b"b:150".to_vec());
        assert_eq!(out[299].1, b"b:299".to_vec());
    }

    /// Seed a DIVERGENT chunk: the manifest row claims `claimed_count`
    /// lines but the object holds exactly `lines` — the shape
    /// `seed_chunk` makes unconstructible, for the divergence-policy
    /// tests. Returns the s3 key.
    async fn seed_divergent_chunk(
        pool: &PgPool,
        store: &MemoryLogChunkStore,
        exec_id: Uuid,
        session_id: Uuid,
        first_line: u64,
        claimed_count: u64,
        lines: &[&[u8]],
    ) -> String {
        // Always chunk_seq 0: divergence fixtures need one chunk per
        // session, so the seq axis adds nothing but an argument.
        let key = log_chunk_key(DRV_HASH_32, &exec_id, &session_id, 0);
        let owned: Vec<Vec<u8>> = lines.iter().map(|l| l.to_vec()).collect();
        let blob = compress_lines(&owned).unwrap();
        let byte_size = blob.len() as i64;
        store.put(&key, blob).await.unwrap();
        sqlx::query(
            "INSERT INTO drv_log_chunks \
                 (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, s3_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(exec_id)
        .bind(session_id)
        .bind(0i32)
        .bind(first_line as i64)
        .bind(claimed_count as i64)
        .bind(byte_size)
        .bind(&key)
        .execute(pool)
        .await
        .unwrap();
        key
    }

    /// An over-length object (holds more lines than its manifest row
    /// claims) must be CLAMPED to the manifest claim: the excess lines
    /// are discarded — they would otherwise be served as garbage under
    /// the NEXT chunk's line numbers — and the next chunk's genuine
    /// lines must not be suppressed by an over-advanced watermark.
    // r[verify store.log.read-divergence+1]
    #[tokio::test]
    async fn over_length_object_clamps_and_preserves_next_chunk() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let sess_a = Uuid::now_v7();
        let sess_b = Uuid::now_v7();
        // Manifest claims 10 lines at 0..10; the object holds 15.
        let a = lines("a", 0, 15);
        seed_divergent_chunk(&db.pool, &store, exec, sess_a, 0, 10, &line_refs(&a)).await;
        // The next chunk genuinely owns 10..20.
        let b = lines("b", 10, 10);
        seed_chunk(&db.pool, &store, exec, sess_b, 0, 10, &line_refs(&b)).await;

        let refs = read_manifest_range(&db.pool, exec, 0).await.unwrap();
        let out = stream_chunks(&store, &refs, 0).await.unwrap();

        assert_eq!(out.len(), 20, "10 clamped from A + 10 genuine from B");
        for (i, (n, _)) in out.iter().enumerate() {
            assert_eq!(*n, i as u64);
        }
        // Lines 10..20 are chunk B's content — the over-length excess
        // of A must not displace them.
        assert_eq!(out[10].1, b"b:10".to_vec());
        assert_eq!(out[19].1, b"b:19".to_vec());
    }

    /// An under-length object (holds fewer lines than its manifest row
    /// claims) is data loss with NotFound parity: an `Internal` error
    /// naming the key, never a silently shorter stream.
    // r[verify store.log.read-divergence+1]
    #[tokio::test]
    async fn short_object_is_data_loss_error() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let sess = Uuid::now_v7();
        // Manifest claims 10 lines; the object holds 6.
        let content = lines("a", 0, 6);
        let key =
            seed_divergent_chunk(&db.pool, &store, exec, sess, 0, 10, &line_refs(&content)).await;

        let refs = read_manifest_range(&db.pool, exec, 0).await.unwrap();
        let err = stream_chunks(&store, &refs, 0)
            .await
            .expect_err("a short object is data loss, not a shorter stream");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(
            err.message().contains(&key),
            "the error names the lossy key: {}",
            err.message()
        );
    }

    /// A manifest row whose object is gone from the store is data loss:
    /// the stream must surface an error naming the key, not silently
    /// skip the chunk.
    #[tokio::test]
    async fn missing_object_for_manifest_row_is_an_error() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let sess = Uuid::now_v7();
        let content = lines("a", 0, 10);
        let key = seed_chunk(&db.pool, &store, exec, sess, 0, 0, &line_refs(&content)).await;
        store
            .delete_batch(std::slice::from_ref(&key))
            .await
            .unwrap();

        let refs = read_manifest_range(&db.pool, exec, 0).await.unwrap();
        let err = stream_chunks(&store, &refs, 0)
            .await
            .expect_err("a manifest row pointing at a missing object is data loss");
        assert!(
            err.message().contains(&key),
            "the error must name the missing key for the operator: {err:?}"
        );
    }

    /// The completeness predicate is `gate::log_is_complete` — imported,
    /// not reimplemented. Table-driven over (status, final_line_count,
    /// manifest coverage).
    #[tokio::test]
    async fn is_complete_predicate() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();

        // (status, final_line_count, chunk ranges, expected, why)
        #[allow(clippy::type_complexity)]
        let cases: Vec<(Option<&str>, Option<i64>, Vec<(u64, u64)>, bool, &str)> = vec![
            (None, None, vec![], false, "no status, no count"),
            (None, Some(10), vec![(0, 10)], false, "still running"),
            (Some("succeeded"), None, vec![(0, 10)], false, "no count"),
            (
                Some("succeeded"),
                Some(20),
                vec![(0, 10)],
                false,
                "gapped manifest",
            ),
            (
                Some("succeeded"),
                Some(20),
                vec![(0, 10), (10, 10)],
                true,
                "terminal + contiguous",
            ),
            (
                Some("cancelled"),
                Some(20),
                vec![(0, 10), (10, 10)],
                true,
                "cancelled is terminal",
            ),
        ];
        for (status, count, ranges, expected, why) in cases {
            let exec = Uuid::now_v7();
            let sess = Uuid::now_v7();
            seed_execution(&db.pool, exec, status, count).await;
            for (seq, (first, n)) in ranges.iter().enumerate() {
                let content = lines("x", *first, *n);
                seed_chunk(
                    &db.pool,
                    &store,
                    exec,
                    sess,
                    seq as u32,
                    *first,
                    &line_refs(&content),
                )
                .await;
            }
            assert_eq!(
                log_is_complete(&db.pool, exec).await.unwrap(),
                expected,
                "{why}"
            );
        }
    }

    /// RED (merged_bug_063): a final message stamped mid-serve — the
    /// reader's cursor at 10 while the seal+manifest committed to 20 —
    /// must NOT claim the served stream complete. Today the predicate
    /// is cursor-blind.
    #[tokio::test]
    async fn mid_serve_final_is_not_complete() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let sess = Uuid::now_v7();
        seed_execution(&db.pool, exec, Some("succeeded"), Some(20)).await;
        for (seq, first) in [(0u32, 0u64), (1, 10)] {
            let content = lines("x", first, 10);
            seed_chunk(
                &db.pool,
                &store,
                exec,
                sess,
                seq,
                first,
                &line_refs(&content),
            )
            .await;
        }
        // The serve cursor sits at 10: the commit+seal landed mid-serve.
        // The kernel claim is cursor-correlated — complete=false here,
        // complete=true only once the reader has been served to 20.
        // (The old cursor-blind `log_is_complete` returned true at
        // cursor 10 — the recorded red.)
        let mid = crate::logs::gate::final_claim_for(&db.pool, exec, 10)
            .await
            .unwrap();
        assert!(
            !mid.complete(),
            "a final stamped with the cursor at 10 (< sealed 20) must not claim complete"
        );
        assert_eq!(mid.cursor_next(), 10);
        let done = crate::logs::gate::final_claim_for(&db.pool, exec, 20)
            .await
            .unwrap();
        assert!(done.complete(), "served to the seal with full coverage");
    }

    /// RED (bug_233): a SHORT object whose missing span is covered by
    /// the remaining manifest rows (a second session's overlapping
    /// chunk) is fully servable — the walk must serve the clamped
    /// lines and let the covering row supply the rest, not wedge the
    /// whole read behind Status::internal.
    #[tokio::test]
    async fn short_object_with_covering_row_serves_fully() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let sess = Uuid::now_v7();
        seed_execution(&db.pool, exec, Some("succeeded"), Some(10)).await;
        // Row A claims [0,10) but its object holds only 5 lines.
        let content_a = lines("a", 0, 5);
        let key_a = seed_chunk(&db.pool, &store, exec, sess, 0, 0, &line_refs(&content_a)).await;
        sqlx::query("UPDATE drv_log_chunks SET line_count = 10 WHERE s3_key = $1")
            .bind(&key_a)
            .execute(&db.pool)
            .await
            .unwrap();
        // Row B (second session) covers [5,10) with a full object.
        let sess_b = Uuid::now_v7();
        let content_b = lines("b", 5, 5);
        seed_chunk(&db.pool, &store, exec, sess_b, 0, 5, &line_refs(&content_b)).await;

        let refs = read_manifest_range(&db.pool, exec, 0).await.unwrap();
        let got = stream_chunks(&store, &refs, 0)
            .await
            .expect("the overlap topology is fully servable — no wedge");
        let nums: Vec<u64> = got.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            nums,
            (0..10).collect::<Vec<u64>>(),
            "all ten lines, exactly once"
        );
    }

    /// bug_233's DR half: a short object whose missing span NO
    /// remaining row covers is genuine corruption-grade loss — typed
    /// permanent (`LOG_UNSERVABLE_METADATA_KEY`) so readers stop
    /// re-dialing it, never silently shortened.
    #[tokio::test]
    async fn short_object_without_cover_is_typed_permanent() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let sess = Uuid::now_v7();
        seed_execution(&db.pool, exec, Some("succeeded"), Some(10)).await;
        let content = lines("a", 0, 5);
        let key = seed_chunk(&db.pool, &store, exec, sess, 0, 0, &line_refs(&content)).await;
        sqlx::query("UPDATE drv_log_chunks SET line_count = 10 WHERE s3_key = $1")
            .bind(&key)
            .execute(&db.pool)
            .await
            .unwrap();
        let refs = read_manifest_range(&db.pool, exec, 0).await.unwrap();
        let err = stream_chunks(&store, &refs, 0)
            .await
            .expect_err("an uncovered short object is data loss");
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_UNSERVABLE_METADATA_KEY)
                .map(|v| v.to_str().unwrap_or("")),
            Some("short_object"),
            "the loss is TYPED permanent so the reader exit law can stop re-dialing: {err:?}"
        );
    }

    /// Latest-exec resolution: empty exec_id → the newest execution for
    /// the derivation (UUIDv7 order); a pinned exec_id is used verbatim;
    /// an unknown pinned exec_id and an unknown derivation are two
    /// distinguishable NotFounds.
    #[tokio::test]
    async fn latest_exec_resolution() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let older = Uuid::now_v7();
        let newer = Uuid::now_v7();
        assert!(older < newer, "UUIDv7 mint order is the resolution order");
        seed_execution(&db.pool, older, Some("failed"), None).await;
        seed_execution(&db.pool, newer, None, None).await;

        // Empty exec_id → the latest execution. The derivation is
        // normalized from the full store path.
        assert_eq!(resolve_exec(&db.pool, DRV_PATH, "").await.unwrap(), newer);
        // A pinned exec_id is used verbatim (here: the older attempt).
        assert_eq!(
            resolve_exec(&db.pool, DRV_PATH, &older.to_string())
                .await
                .unwrap(),
            older
        );
        // A pinned exec_id with no recorded execution and no chunks.
        let unknown = Uuid::now_v7();
        let err = resolve_exec(&db.pool, DRV_PATH, &unknown.to_string())
            .await
            .expect_err("an unknown pinned exec_id is NotFound");
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(
            err.message().contains("execution"),
            "the pinned-exec NotFound names the execution: {err:?}"
        );
        // An unknown derivation (no executions at all).
        let err = resolve_exec(
            &db.pool,
            "/nix/store/9zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-x.drv",
            "",
        )
        .await
        .expect_err("a derivation with no executions is NotFound");
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(
            err.message().contains("derivation"),
            "the no-executions NotFound names the derivation, not the execution: {err:?}"
        );
    }

    /// since_line in the middle of a chunk: the chunk is fetched and the
    /// lines below the cursor are dropped after decompression.
    #[tokio::test]
    async fn since_line_mid_chunk() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let exec = Uuid::now_v7();
        let sess = Uuid::now_v7();
        let content = lines("a", 0, 100);
        seed_chunk(&db.pool, &store, exec, sess, 0, 0, &line_refs(&content)).await;

        let refs = read_manifest_range(&db.pool, exec, 50).await.unwrap();
        assert_eq!(refs.len(), 1, "the containing chunk intersects [50, ∞)");
        let out = stream_chunks(&store, &refs, 50).await.unwrap();
        assert_eq!(out.first().map(|(n, _)| *n), Some(50));
        assert_eq!(out.last().map(|(n, _)| *n), Some(99));
        assert_eq!(out.len(), 50);
        assert_eq!(out[0].1, b"a:50".to_vec());
    }
}

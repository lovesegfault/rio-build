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
//! 3. [`resolve_exec`] turns a `TailLogRequest`'s `(derivation,
//!    exec_id)` pair into the execution to read.
//!
//! The completeness predicate (`TailLogChunk.is_complete`) is
//! `super::gate::log_is_complete` — the same function that seals the
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

    /// Advance the watermark past lines yielded from a source other
    /// than [`read_chunk`] (the live snapshot and the subscription
    /// stream in the `TailLog` handler use the same cursor to dedup
    /// across the manifest→snapshot→live seam). A backwards `advance_to`
    /// is a no-op — the watermark is monotone by definition.
    pub fn advance_to(&mut self, next_line: u64) {
        self.next_line = self.next_line.max(next_line);
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
pub async fn read_chunk(
    store: &dyn LogChunkStore,
    chunk: &ChunkRef,
    cursor: &mut LineCursor,
) -> Result<Vec<(u64, Vec<u8>)>, Status> {
    // A degenerate zero-line chunk (the cutter never writes one, but the
    // manifest is just a table) has nothing to contribute, and a chunk
    // whose last line is below the watermark is a same-range chunk from
    // a second session whose lines were all already yielded. Both take
    // the cheap skip branch — no GET.
    let last_line = chunk
        .first_line
        .saturating_add(chunk.line_count)
        .saturating_sub(1);
    if chunk.line_count == 0 || last_line < cursor.next_line {
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
            metrics::counter!("rio_store_log_read_data_loss_total").increment(1);
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

    if lines.len() as u64 != chunk.line_count {
        // The manifest row and the object are written from the same
        // line slice in the same call, so they cannot legitimately
        // disagree. Attribute and advance by what the object actually
        // holds (the conservative choice: it can only ever let a later
        // overlapping chunk fill in lines this one was supposed to
        // have, never skip lines that exist).
        warn!(
            key = %chunk.s3_key,
            manifest_count = chunk.line_count,
            actual_count = lines.len(),
            "TailLog: chunk line count disagrees with its manifest row"
        );
    }

    let mut out = Vec::new();
    for (i, line) in lines.into_iter().enumerate() {
        let line_no = chunk.first_line.saturating_add(i as u64);
        if line_no < cursor.next_line {
            continue;
        }
        out.push((line_no, line));
        cursor.next_line = line_no + 1;
    }
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
    for chunk in refs {
        out.extend(read_chunk(store, chunk, &mut cursor).await?);
    }
    Ok(out)
}

/// Resolve a `TailLogRequest`'s `(derivation, exec_id)` pair to the
/// execution to read.
///
/// - A non-empty `pinned_exec_id` is used verbatim once it is shown to
///   exist (a `drv_executions` lifecycle row *or* at least one manifest
///   chunk — an execution can have chunks before the scheduler's
///   lifecycle INSERT lands, and a lifecycle row before its first
///   chunk). The `derivation` argument is not cross-checked against a
///   pinned execution: exec ids are unguessable UUIDv7s and `TailLog`
///   is a read-only, route-gated API — the pin *is* the selector.
/// - An empty `pinned_exec_id` resolves to the **latest** execution for
///   the derivation: `ORDER BY exec_id DESC` over the UUIDv7 mint order
///   (`assign_to_worker` mints exec ids with `Uuid::now_v7()`).
///
/// The two failure modes are distinguishable `NotFound`s — "no log
/// recorded for execution …" (the caller pinned a bad id) vs "no
/// executions recorded for derivation …" (nothing was ever dispatched,
/// or it expired) — because they have different audiences: the first is
/// a dashboard deep-link gone stale, the second is `rio-cli logs` for a
/// derivation that never built.
// r[impl obs.log.exec-keyed+2]
pub async fn resolve_exec(
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
    let latest = sqlx::query_scalar!(
        r#"
        SELECT exec_id FROM drv_executions
        WHERE drv_hash = $1
        ORDER BY exec_id DESC
        LIMIT 1
        "#,
        drv_hash,
    )
    .fetch_optional(pool)
    .await
    .status_internal("TailLog: latest execution lookup")?;

    latest.ok_or_else(|| {
        Status::not_found(format!("no executions recorded for derivation {drv_hash}"))
    })
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

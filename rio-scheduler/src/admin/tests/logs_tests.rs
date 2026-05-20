//! `GetDerivationLogs` RPC tests + `drv_log_hash`/`decompress_and_chunk` helpers.
//!
//! Split from the 1732L monolithic `admin/tests.rs` (P0386) to mirror the
//! `admin/logs.rs` submodule seam introduced by P0383.

use super::*;
use crate::admin::logs::{decompress_and_chunk, s3_is_caught_up, try_ring_buffer};
use crate::logs::drv_log_hash;
use aws_sdk_s3::operation::get_object::GetObjectOutput;
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_mocks::{RuleMode, mock, mock_client};
use rio_proto::types::BuildLogBatch;

fn mk_batch(drv_path: &str, first_line: u64, lines: &[&[u8]]) -> BuildLogBatch {
    BuildLogBatch {
        derivation_path: drv_path.to_string(),
        lines: lines.iter().map(|l| l.to_vec()).collect(),
        first_line_number: first_line,
        executor_id: "test-worker".into(),
    }
}

#[tokio::test]
async fn get_derivation_logs_from_ring_buffer() -> anyhow::Result<()> {
    let buffers = Arc::new(LogBuffers::new());
    buffers.push(&mk_batch(
        "/nix/store/abc-test.drv",
        0,
        &[b"line0", b"line1", b"line2"],
    ));

    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

    let resp = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: String::new(), // not needed for ring buffer
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 0,
        }))
        .await?;

    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1, "3 lines < CHUNK_LINES → one chunk");
    assert_eq!(chunks[0].lines.len(), 3);
    assert_eq!(chunks[0].lines[0], b"line0");
    assert_eq!(chunks[0].first_line_number, 0);
    assert!(
        !chunks[0].is_complete,
        "ring buffer serve → still active, is_complete=false"
    );
    Ok(())
}

#[tokio::test]
async fn get_derivation_logs_since_line_filters() -> anyhow::Result<()> {
    let buffers = Arc::new(LogBuffers::new());
    buffers.push(&mk_batch(
        "/nix/store/abc-test.drv",
        0,
        &[b"l0", b"l1", b"l2", b"l3", b"l4"],
    ));

    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

    let resp = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: String::new(),
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 3,
        }))
        .await?;

    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 2, "since_line=3 → only lines 3,4");
    assert_eq!(chunks[0].first_line_number, 3);
    Ok(())
}

/// Compress a test log the same way the flusher does (zstd level 6,
/// each line `\n`-terminated). Shared by the S3-path tests.
fn compress_lines(lines: &[&str]) -> anyhow::Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 6)?;
    for line in lines {
        enc.write_all(line.as_bytes())?;
        enc.write_all(b"\n")?;
    }
    Ok(enc.finish()?)
}

/// Seed a `drv_logs` row the flusher would have written. `drv_hash` is
/// the 32-char store hash only (`drv_log_hash` output), NOT the
/// basename. `started_at` is `NOT NULL` — `now()` is fine for tests
/// (the GC sweep won't fire). Returns the row's `exec_id`.
///
/// `exec_id` is a `now_v7()` UUID. Multi-row seeds in one test rely on
/// `uuid 1.11+`'s per-process atomic counter for within-millisecond
/// monotonicity — that's a crate guarantee, not an RFC 9562 one (§6.2
/// makes within-ms ordering optional). Tests that depend on the seed
/// order MUST assert it explicitly so a counter-discipline regression
/// (crate update, multi-process generation) fails loudly at the
/// assumption rather than silently as a wrong-content mismatch.
async fn seed_drv_log(
    pool: &sqlx::PgPool,
    drv_hash: &str,
    s3_key: &str,
    line_count: i64,
    is_complete: bool,
) -> anyhow::Result<uuid::Uuid> {
    let exec_id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO drv_logs
             (exec_id, drv_hash, s3_key, line_count, is_complete, started_at)
         VALUES ($1, $2, $3, $4, $5, now())",
    )
    .bind(exec_id)
    .bind(drv_hash)
    .bind(s3_key)
    .bind(line_count)
    .bind(is_complete)
    .execute(pool)
    .await?;
    Ok(exec_id)
}

/// Build an `AdminServiceImpl` against an existing `TestDb` + mocked S3.
///
/// `setup_svc` creates its own `TestDb`, which is wrong when the test
/// needs to seed PG rows BEFORE constructing the svc (the flusher-written
/// `drv_logs` row must exist when the handler queries it). Shared by the
/// S3-path tests so the wiring boilerplate isn't repeated per test.
fn svc_with_db_and_s3(
    db: &TestDb,
    s3: aws_sdk_s3::Client,
) -> (AdminServiceImpl, ActorHandle, tokio::task::JoinHandle<()>) {
    svc_with_db_buffers_and_s3(db, Arc::new(LogBuffers::new()), s3)
}

/// Like [`svc_with_db_and_s3`] but with a caller-supplied `LogBuffers`,
/// for tests that need to pre-populate the ring buffer (e.g.
/// `pinned_exec_skips_mismatched_ring_buffer`).
fn svc_with_db_buffers_and_s3(
    db: &TestDb,
    buffers: Arc<LogBuffers>,
    s3: aws_sdk_s3::Client,
) -> (AdminServiceImpl, ActorHandle, tokio::task::JoinHandle<()>) {
    let (actor, task) = setup_actor(db.pool.clone());
    let svc = AdminServiceImpl::new(
        buffers,
        Some((s3, "test-bucket".into())),
        db.pool.clone(),
        actor.clone(),
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        crate::lease::LeaderState::default(),
        rio_common::signal::Token::new(),
        String::new(),
        Arc::new(crate::sla::config::SlaConfig::test_default()),
        None,
        Arc::default(),
    );
    (svc, actor, task)
}

#[tokio::test]
async fn get_derivation_logs_from_s3_fallback() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let compressed = compress_lines(&["from-s3-0", "from-s3-1", "from-s3-2"])?;

    let exec_id = seed_drv_log(&db.pool, "abc", "logs/abc/X.log.zst", 3, true).await?;

    // Mock S3 to return the zstd blob.
    let rule = mock!(S3Client::get_object).then_output(move || {
        GetObjectOutput::builder()
            .body(ByteStream::from(compressed.clone()))
            .build()
    });
    let s3 = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule]);

    // Ring buffer is EMPTY — forces S3 fallback.
    let (svc, _actor, _task) = svc_with_db_and_s3(&db, s3);

    let resp = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: exec_id.to_string(),
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 0,
        }))
        .await?;

    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 3);
    assert_eq!(chunks[0].lines[0], b"from-s3-0");
    assert_eq!(chunks[0].lines[2], b"from-s3-2");
    assert_eq!(
        chunks[0].exec_id,
        exec_id.to_string(),
        "S3 chunk reports which execution it served"
    );
    assert!(
        chunks[0].is_complete,
        "S3 serve from a final blob → is_complete=true"
    );
    Ok(())
}

/// Empty `exec_id` resolves to the latest execution (`MAX(exec_id)` —
/// UUIDv7 is time-sortable). Two seeds, the request asks for "latest",
/// and the chunk reports which exec it picked.
///
/// `r[verify obs.log.exec-keyed]` — proves the latest-exec resolution
/// the design promises: `rio-cli logs <drv>` works without a build_id
/// or exec_id.
#[tokio::test]
async fn get_derivation_logs_latest_exec_resolution() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;

    // Two executions for the same drv. The first carries the WRONG
    // content so the test fails loudly if the older one wins.
    let stale = compress_lines(&["stale-0", "stale-1"])?;
    let fresh = compress_lines(&["fresh-0", "fresh-1", "fresh-2"])?;
    let old_exec = seed_drv_log(&db.pool, "abc", "logs/abc/old.log.zst", 2, true).await?;
    let new_exec = seed_drv_log(&db.pool, "abc", "logs/abc/new.log.zst", 3, true).await?;
    // `ORDER BY exec_id DESC` only finds `new_exec` if the second seed
    // sorts after the first. Within-ms monotonicity is a `uuid 1.11+`
    // counter-discipline guarantee, NOT an RFC 9562 property — assert it
    // so an assumption break fails HERE, not as a wrong-content mismatch
    // 30 lines down.
    assert!(
        new_exec > old_exec,
        "uuid 1.11+ monotonic counter must order within-process now_v7() \
         calls; if this fails the latest-exec resolution test below is \
         meaningless"
    );

    // S3 mock: distinct content per key so we know which blob was hit.
    let rule_old = mock!(S3Client::get_object)
        .match_requests(|r| r.key() == Some("logs/abc/old.log.zst"))
        .then_output(move || {
            GetObjectOutput::builder()
                .body(ByteStream::from(stale.clone()))
                .build()
        });
    let rule_new = mock!(S3Client::get_object)
        .match_requests(|r| r.key() == Some("logs/abc/new.log.zst"))
        .then_output(move || {
            GetObjectOutput::builder()
                .body(ByteStream::from(fresh.clone()))
                .build()
        });
    let s3 = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule_old, &rule_new]);

    let (svc, _actor, _task) = svc_with_db_and_s3(&db, s3);

    // exec_id EMPTY → latest.
    let resp = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: String::new(),
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 0,
        }))
        .await?;
    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 3, "latest exec has 3 lines");
    assert_eq!(chunks[0].lines[0], b"fresh-0", "must be the LATEST blob");
    assert_eq!(
        chunks[0].exec_id,
        new_exec.to_string(),
        "chunk reports the resolved exec, not the older one"
    );
    Ok(())
}

#[tokio::test]
async fn get_derivation_logs_not_found_in_either() -> anyhow::Result<()> {
    let buffers = Arc::new(LogBuffers::new());
    // No S3 configured, buffer empty.
    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

    let result = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: uuid::Uuid::new_v4().to_string(),
            derivation_path: "/nix/store/nowhere.drv".into(),
            since_line: 0,
        }))
        .await;

    let status = expect_stream_err(result).await;
    assert_eq!(status.code(), tonic::Code::NotFound);
    Ok(())
}

#[tokio::test]
async fn get_derivation_logs_empty_drv_path_invalid() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let result = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: String::new(),
            derivation_path: String::new(),
            since_line: 0,
        }))
        .await;

    let status = expect_stream_err(result).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("derivation_path"));
    Ok(())
}

/// GetDerivationLogs with a non-empty but malformed exec_id → InvalidArgument.
/// The ring buffer is empty (no match on drv_path), so it falls through
/// to the S3 path, which parses exec_id.
#[tokio::test]
async fn test_get_derivation_logs_invalid_uuid() -> anyhow::Result<()> {
    // Ring buffer empty → forces S3 fallback → exec_id parse.
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let result = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: "not-a-uuid".into(),
            derivation_path: "/nix/store/nowhere.drv".into(),
            since_line: 0,
        }))
        .await;

    let status = expect_stream_err(result).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("exec_id"),
        "error should mention exec_id: {}",
        status.message()
    );
    Ok(())
}

#[test]
fn drv_log_hash_extracts_store_hash() {
    // Full realistic store path → 32-char hash only (the spec key shape).
    assert_eq!(
        drv_log_hash("/nix/store/amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2-firefox-unwrapped-149.0.drv"),
        "amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2"
    );
    // Basename → hash.
    assert_eq!(
        drv_log_hash("amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2-firefox.drv"),
        "amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2"
    );
    // Bare hash (dashboard input) → unchanged.
    assert_eq!(
        drv_log_hash("amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2"),
        "amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2"
    );
    // Short test fixture (parse fails on length) → still strips to hash part.
    assert_eq!(drv_log_hash("/nix/store/abc-foo.drv"), "abc");
}

#[test]
fn decompress_and_chunk_roundtrip() -> anyhow::Result<()> {
    // zstd → decompress_and_chunk → lines match.
    use std::io::Write;
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 6)?;
    for i in 0..5 {
        enc.write_all(format!("line-{i}").as_bytes())?;
        enc.write_all(b"\n")?;
    }
    let zst = enc.finish()?;

    let chunks = decompress_and_chunk(&zst, "test", "exec-1", 0, 0)?;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 5, "trailing \\n artifact stripped");
    assert_eq!(chunks[0].lines[0], b"line-0");
    assert_eq!(chunks[0].lines[4], b"line-4");
    assert_eq!(chunks[0].exec_id, "exec-1");
    // `is_complete` is the CALLER's responsibility (try_s3 stamps the
    // last chunk from the row). decompress_and_chunk doesn't know
    // whether the blob was final or `.partial`.
    assert!(!chunks[0].is_complete);
    Ok(())
}

/// Regression: a fast-polling dashboard caught up on an active build
/// (since=3, 3 lines buffered) used to get `None` → fall through to S3
/// → `NotFound`. Now: single empty `is_complete=false` chunk → re-poll.
#[test]
fn try_ring_buffer_caught_up_returns_empty_incomplete_chunk() {
    let bufs = LogBuffers::new();
    bufs.push(&mk_batch("drv-a", 0, &[b"l0", b"l1", b"l2"]));
    let chunks = try_ring_buffer(&bufs, "drv-a", 3).expect("buffer present → Some");
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].lines.is_empty());
    assert!(!chunks[0].is_complete, "active build → re-poll signal");
    assert_eq!(chunks[0].first_line_number, 3);
}

/// Absent buffer → `None` (caller falls through to S3). Distinct from
/// the caught-up case above.
#[test]
fn try_ring_buffer_absent_returns_none() {
    let bufs = LogBuffers::new();
    assert!(try_ring_buffer(&bufs, "neverseen", 0).is_none());
}

/// Regression for bug_126: ring-buffer lookup must normalize input the
/// same way the S3 path does. The worker pushes under the full
/// `/nix/store/...` path; `rio-cli logs <basename>` (or bare hash) must
/// hit the same buffer. Before: only the S3 fallback called
/// `drv_log_hash`, so a basename for an ACTIVE build missed the ring
/// buffer keyed on the full path → fell through → "no active ring
/// buffer" while the buffer existed under a different key form.
#[test]
fn try_ring_buffer_normalizes_input() {
    let bufs = LogBuffers::new();
    let full = "/nix/store/amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2-hello.drv";
    let basename = "amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2-hello.drv";
    let bare = "amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2";
    bufs.push(&mk_batch(full, 0, &[b"l0", b"l1"]));

    for input in [full, basename, bare] {
        let chunks = try_ring_buffer(&bufs, input, 0)
            .unwrap_or_else(|| panic!("input form {input:?} must hit the ring buffer"));
        assert_eq!(chunks[0].lines.len(), 2, "input form {input:?}");
    }
}

/// Regression: with 255 lines and `since=0`, `(255-0) % 256 == 255` —
/// the trailing-`\n` split artifact `""` was the 256th element, filling
/// `buf` to CHUNK_LINES and being `mem::take`n into `chunks` BEFORE the
/// post-loop strip ran. Result: 256 lines, last one empty. Now the
/// trailing `\n` is stripped before splitting.
#[test]
fn decompress_and_chunk_255_lines_no_trailing_empty() -> anyhow::Result<()> {
    use std::io::Write;
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 6)?;
    for i in 0..255 {
        enc.write_all(format!("l{i}").as_bytes())?;
        enc.write_all(b"\n")?;
    }
    let zst = enc.finish()?;

    let chunks = decompress_and_chunk(&zst, "test", "exec-1", 0, 0)?;
    assert_eq!(chunks.len(), 1, "255 < CHUNK_LINES → one chunk");
    assert_eq!(
        chunks[0].lines.len(),
        255,
        "trailing-\\n artifact must not leak as a 256th empty line"
    );
    assert!(!chunks[0].lines.last().unwrap().is_empty());
    Ok(())
}

/// `try_s3` short-circuits when `since ≥ line_count`: no S3 GET, single
/// empty terminal chunk. Proven by passing an S3 client mocked to PANIC
/// on GetObject — if the short-circuit doesn't fire, the test fails on
/// the mock.
#[tokio::test]
async fn try_s3_short_circuits_on_since_ge_line_count() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let exec_id = seed_drv_log(&db.pool, "abc", "logs/abc/X.log.zst", 5, true).await?;

    // S3 mock that fails any GetObject — proves we never call it.
    let rule = mock!(S3Client::get_object).then_error(|| {
        aws_sdk_s3::operation::get_object::GetObjectError::generic(
            aws_sdk_s3::error::ErrorMetadata::builder()
                .code("InternalError")
                .message("S3 GET should have been short-circuited")
                .build(),
        )
    });
    let s3 = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule]);

    let (svc, _actor, _task) = svc_with_db_and_s3(&db, s3);

    let resp = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: exec_id.to_string(),
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 5,
        }))
        .await?;
    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].lines.is_empty());
    assert_eq!(chunks[0].exec_id, exec_id.to_string());
    assert!(chunks[0].is_complete, "row was final → derivation finished");
    Ok(())
}

/// `chunk.exec_id` is set in BOTH the ring-buffer path and the S3 path.
/// Ring buffer: from the buffer entry's `set_exec` stamp. S3: from the
/// `drv_logs` row (resolved or provided). A client knows which execution
/// it's watching without a second lookup.
#[tokio::test]
async fn chunk_exec_id_reports_resolved_exec() -> anyhow::Result<()> {
    // — Ring buffer path —
    let buffers = Arc::new(LogBuffers::new());
    let exec_active = uuid::Uuid::now_v7();
    buffers.set_exec("/nix/store/abc-test.drv", exec_active, "executor-1");
    buffers.push(&mk_batch("/nix/store/abc-test.drv", 0, &[b"l0", b"l1"]));
    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;
    let resp = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: String::new(),
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 0,
        }))
        .await?;
    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].exec_id,
        exec_active.to_string(),
        "ring-buffer chunk carries the stamped exec_id"
    );

    // — S3 path —
    let db = TestDb::new(&crate::MIGRATOR).await;
    let compressed = compress_lines(&["l0", "l1"])?;
    let exec_done = seed_drv_log(&db.pool, "abc", "logs/abc/X.log.zst", 2, true).await?;
    let rule = mock!(S3Client::get_object).then_output(move || {
        GetObjectOutput::builder()
            .body(ByteStream::from(compressed.clone()))
            .build()
    });
    let s3 = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule]);
    let (svc, _actor, _task) = svc_with_db_and_s3(&db, s3);
    let resp = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: exec_done.to_string(),
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 0,
        }))
        .await?;
    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].exec_id,
        exec_done.to_string(),
        "S3 chunk carries the resolved exec_id"
    );
    Ok(())
}

/// A request that pins a specific `exec_id` MUST NOT be satisfied by the
/// ring buffer when the live entry is stamped with a *different*
/// execution. Without the gate, the dashboard's build view (which pins
/// `GraphNode.exec_id` for the exact execution that build observed)
/// would silently get the *current* execution's in-progress lines —
/// e.g., a rebuild of the same drv triggered by a later build — and the
/// "approximate / latest available" banner would stay hidden because
/// `execId` was non-empty. The handler must fall through to S3, which
/// has the pinned execution's blob (or returns NotFound).
#[tokio::test]
async fn pinned_exec_skips_mismatched_ring_buffer() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let drv_path = "/nix/store/abcdefghijklmnopqrstuvwxyz012345-pin.drv";
    let drv_hash = drv_log_hash(drv_path);

    // exec_old's blob is in S3. exec_new is currently in the ring buffer
    // (drv was rebuilt — by a retry or by a later build).
    let compressed = compress_lines(&["old0", "old1"])?;
    let exec_old = seed_drv_log(
        &db.pool,
        &drv_hash,
        &format!("logs/{drv_hash}/old.log.zst"),
        2,
        true,
    )
    .await?;
    let buffers = Arc::new(LogBuffers::new());
    let exec_new = uuid::Uuid::now_v7();
    buffers.set_exec(drv_path, exec_new, "executor-2");
    buffers.push(&mk_batch(drv_path, 0, &[b"new-in-progress"]));
    assert_ne!(
        exec_old, exec_new,
        "precondition: distinct executions (uuid 1.11+ monotonic counter)"
    );

    let rule = mock!(S3Client::get_object).then_output(move || {
        GetObjectOutput::builder()
            .body(ByteStream::from(compressed.clone()))
            .build()
    });
    let s3 = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule]);
    let (svc, _actor, _task) = svc_with_db_buffers_and_s3(&db, buffers, s3);

    // Pin exec_old. The ring buffer holds exec_new — the gate must fall
    // through to S3 and serve exec_old's blob.
    let resp = svc
        .get_derivation_logs(Request::new(GetDerivationLogsRequest {
            exec_id: exec_old.to_string(),
            derivation_path: drv_path.into(),
            since_line: 0,
        }))
        .await?;
    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].exec_id,
        exec_old.to_string(),
        "pinned request must serve the pinned execution, not the live ring buffer"
    );
    assert_eq!(
        chunks[0].lines,
        vec![b"old0".to_vec(), b"old1".to_vec()],
        "must serve exec_old's S3 blob, not exec_new's in-progress lines"
    );
    Ok(())
}

#[test]
fn decompress_and_chunk_since_filtering() -> anyhow::Result<()> {
    use std::io::Write;
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 6)?;
    for i in 0..5 {
        enc.write_all(format!("l{i}").as_bytes())?;
        enc.write_all(b"\n")?;
    }
    let zst = enc.finish()?;

    let chunks = decompress_and_chunk(&zst, "test", "exec-1", 0, 3)?;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 2, "since=3 → lines 3,4 only");
    assert_eq!(chunks[0].first_line_number, 3);
    Ok(())
}

/// Regression for bug_084: blob holds true lines [50_000..50_100); a
/// client at `since=50_070` must receive content of original lines
/// 50_070..50_099 labeled `first_line_number=50_070` — NOT blob
/// indices 70..99 mislabeled as 70, and NOT zero lines.
#[test]
fn decompress_and_chunk_with_first_line_offset() -> anyhow::Result<()> {
    use std::io::Write;
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 6)?;
    for i in 0..100 {
        enc.write_all(format!("orig-{}", 50_000 + i).as_bytes())?;
        enc.write_all(b"\n")?;
    }
    let zst = enc.finish()?;

    let chunks = decompress_and_chunk(&zst, "test", "exec-1", 50_000, 50_070)?;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 30, "true lines 50_070..50_099");
    assert_eq!(chunks[0].first_line_number, 50_070);
    assert_eq!(chunks[0].lines[0], b"orig-50070");
    assert_eq!(chunks[0].lines[29], b"orig-50099");

    // since < first_line (client never saw the evicted head — fresh
    // poll after completion). All 100 survivors returned, labeled
    // from first_line.
    let chunks = decompress_and_chunk(&zst, "test", "exec-1", 50_000, 0)?;
    assert_eq!(chunks[0].lines.len(), 100);
    assert_eq!(chunks[0].first_line_number, 50_000);
    assert_eq!(chunks[0].lines[0], b"orig-50000");
    Ok(())
}

/// Regression for bug_084: the short-circuit must compare `since`
/// against `first_line + line_count`, not bare `line_count`. A 150k-line
/// build whose ring evicted to survivors [50_000..150_000) has
/// `first_line=50_000, line_count=100_000`; a client at `since=120_000`
/// is NOT caught up (30k lines remain).
#[test]
fn s3_short_circuit_respects_first_line() {
    // The bug: previously `120_000 >= 100_000` → short-circuit → 30k
    // lines silently dropped.
    assert!(
        !s3_is_caught_up(120_000, 50_000, 100_000),
        "since=120k < first+count=150k → NOT caught up"
    );
    // Genuine caught-up at the boundary.
    assert!(s3_is_caught_up(150_000, 50_000, 100_000));
    // No-eviction case (first_line=0) reduces to the old comparison.
    assert!(s3_is_caught_up(5, 0, 5));
    assert!(!s3_is_caught_up(4, 0, 5));
}

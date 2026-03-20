//! `GetBuildLogs` RPC tests + `extract_drv_hash`/`gunzip_and_chunk` helpers.
//!
//! Split from the 1732L monolithic `admin/tests.rs` (P0386) to mirror the
//! `admin/logs.rs` submodule seam introduced by P0383.

use super::*;
use crate::admin::logs::{extract_drv_hash, gunzip_and_chunk};
use aws_sdk_s3::operation::get_object::GetObjectOutput;
use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_mocks::{RuleMode, mock, mock_client};
use rio_proto::types::BuildLogBatch;

fn mk_batch(drv_path: &str, first_line: u64, lines: &[&[u8]]) -> BuildLogBatch {
    BuildLogBatch {
        derivation_path: drv_path.to_string(),
        lines: lines.iter().map(|l| l.to_vec()).collect(),
        first_line_number: first_line,
        worker_id: "test-worker".into(),
    }
}

#[tokio::test]
async fn get_build_logs_from_ring_buffer() -> anyhow::Result<()> {
    let buffers = Arc::new(LogBuffers::new());
    buffers.push(&mk_batch(
        "/nix/store/abc-test.drv",
        0,
        &[b"line0", b"line1", b"line2"],
    ));

    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

    let resp = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: String::new(), // not needed for ring buffer
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
async fn get_build_logs_since_line_filters() -> anyhow::Result<()> {
    let buffers = Arc::new(LogBuffers::new());
    buffers.push(&mk_batch(
        "/nix/store/abc-test.drv",
        0,
        &[b"l0", b"l1", b"l2", b"l3", b"l4"],
    ));

    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

    let resp = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: String::new(),
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

#[tokio::test]
async fn get_build_logs_from_s3_fallback() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let build_id = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'succeeded')")
        .bind(build_id)
        .execute(&db.pool)
        .await?;

    // Gzip a test log the same way the flusher does.
    let gzipped = {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        for line in ["from-s3-0", "from-s3-1", "from-s3-2"] {
            enc.write_all(line.as_bytes())?;
            enc.write_all(b"\n")?;
        }
        enc.finish()?
    };

    // Seed the PG row the flusher would have written.
    sqlx::query(
        "INSERT INTO build_logs (build_id, drv_hash, s3_key, line_count, byte_size, is_complete)
         VALUES ($1, $2, $3, $4, $5, true)",
    )
    .bind(build_id)
    .bind("abc-test.drv")
    .bind(format!("logs/{build_id}/abc-test.drv.log.gz"))
    .bind(3_i64)
    .bind(gzipped.len() as i64)
    .execute(&db.pool)
    .await?;

    // Mock S3 to return the gzipped blob.
    let rule = mock!(S3Client::get_object).then_output(move || {
        GetObjectOutput::builder()
            .body(ByteStream::from(gzipped.clone()))
            .build()
    });
    let s3 = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule]);

    // Ring buffer is EMPTY — forces S3 fallback.
    //
    // Can't use setup_svc here: this test seeds PG rows BEFORE
    // constructing svc (the flusher-written build_logs row), and
    // setup_svc creates its own TestDb. Wire manually.
    let buffers = Arc::new(LogBuffers::new());
    let (actor, _task) = setup_actor(db.pool.clone());
    let svc = AdminServiceImpl::new(
        buffers,
        Some((s3, "test-bucket".into())),
        db.pool.clone(),
        actor,
        "127.0.0.1:1".into(),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        rio_common::signal::Token::new(),
    );

    let resp = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: build_id.to_string(),
            derivation_path: "/nix/store/abc-test.drv".into(),
            since_line: 0,
        }))
        .await?;

    let chunks = collect_stream(resp.into_inner()).await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 3);
    assert_eq!(chunks[0].lines[0], b"from-s3-0");
    assert_eq!(chunks[0].lines[2], b"from-s3-2");
    assert!(
        chunks[0].is_complete,
        "S3 serve → derivation finished, is_complete=true"
    );
    Ok(())
}

#[tokio::test]
async fn get_build_logs_not_found_in_either() -> anyhow::Result<()> {
    let buffers = Arc::new(LogBuffers::new());
    // No S3 configured, buffer empty.
    let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

    let result = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: uuid::Uuid::new_v4().to_string(),
            derivation_path: "/nix/store/nowhere.drv".into(),
            since_line: 0,
        }))
        .await;

    let status = expect_stream_err(result).await;
    assert_eq!(status.code(), tonic::Code::NotFound);
    Ok(())
}

#[tokio::test]
async fn get_build_logs_empty_drv_path_invalid() -> anyhow::Result<()> {
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let result = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: String::new(),
            derivation_path: String::new(),
            since_line: 0,
        }))
        .await;

    let status = expect_stream_err(result).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("derivation_path"));
    Ok(())
}

/// GetBuildLogs with a non-empty but malformed build_id → InvalidArgument.
/// The ring buffer is empty (no match on drv_path), so it falls through
/// to the S3 path, which parses build_id.
#[tokio::test]
async fn test_get_build_logs_invalid_uuid() -> anyhow::Result<()> {
    // Ring buffer empty → forces S3 fallback → build_id parse.
    let (svc, _actor, _task, _db) = setup_svc_default().await;

    let result = svc
        .get_build_logs(Request::new(GetBuildLogsRequest {
            build_id: "not-a-uuid".into(),
            derivation_path: "/nix/store/nowhere.drv".into(),
            since_line: 0,
        }))
        .await;

    let status = expect_stream_err(result).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("build_id"),
        "error should mention build_id: {}",
        status.message()
    );
    Ok(())
}

#[test]
fn extract_drv_hash_strips_store_prefix() {
    assert_eq!(extract_drv_hash("/nix/store/abc-foo.drv"), "abc-foo.drv");
    assert_eq!(extract_drv_hash("already-a-hash"), "already-a-hash");
}

#[test]
fn gunzip_and_chunk_roundtrip() -> anyhow::Result<()> {
    // Gzip → gunzip_and_chunk → lines match.
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    for i in 0..5 {
        enc.write_all(format!("line-{i}").as_bytes())?;
        enc.write_all(b"\n")?;
    }
    let gz = enc.finish()?;

    let chunks = gunzip_and_chunk(&gz, "test", 0)?;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 5, "trailing \\n artifact stripped");
    assert_eq!(chunks[0].lines[0], b"line-0");
    assert_eq!(chunks[0].lines[4], b"line-4");
    assert!(chunks[0].is_complete);
    Ok(())
}

#[test]
fn gunzip_and_chunk_since_filtering() -> anyhow::Result<()> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    for i in 0..5 {
        enc.write_all(format!("l{i}").as_bytes())?;
        enc.write_all(b"\n")?;
    }
    let gz = enc.finish()?;

    let chunks = gunzip_and_chunk(&gz, "test", 3)?;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].lines.len(), 2, "since=3 → lines 3,4 only");
    assert_eq!(chunks[0].first_line_number, 3);
    Ok(())
}

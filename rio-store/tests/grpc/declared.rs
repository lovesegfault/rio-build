//! Declared-mode PutPath protocol tests (N1 — reservation-mode ingest).
//!
//! The opt-in `declared_nar_size` metadata buys the sender single-shot
//! budget reservation: the whole charge BEFORE the first chunk, zero
//! hold-and-wait. These tests pin the acquire census (W9-BN), both
//! delivery-refusal axes (W9-BO), the batch fail-closed arm, and the
//! bound gate. Trailer-mode parity (W9-BP) is the rest of this
//! battery re-run unchanged — `declared_nar_size: 0` everywhere else
//! in this test crate IS the trailer-mode contrast baseline.

use super::*;

/// (nar, info) pair for declared tests — same fixture as trailer.rs.
fn declared_fixture(name: &str) -> (Vec<u8>, ValidatedPathInfo) {
    let (nar, _hash) = make_nar(name.as_bytes());
    let store_path = test_store_path(name);
    let info = make_path_info_for_nar(&store_path, &nar);
    (nar, info)
}

/// Trailer-empty metadata (the wire contract) with the declared field.
fn declared_metadata(info: &ValidatedPathInfo, declared: u64) -> PutPathRequest {
    let mut raw: PathInfo = info.clone().into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;
    PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
            declared_nar_size: declared,
        })),
    }
}

fn chunk_msg(bytes: Vec<u8>) -> PutPathRequest {
    PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(bytes)),
    }
}

fn trailer_msg(info: &ValidatedPathInfo, size: u64) -> PutPathRequest {
    PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(PutPathTrailer {
            nar_hash: info.nar_hash.to_vec(),
            nar_size: size,
        })),
    }
}

/// Poll until `budget.available_permits()` equals `target` (or ~2s).
async fn wait_permits(budget: &rio_store::budget::NarBudget, target: usize) -> usize {
    for _ in 0..200 {
        let n = budget.available_permits();
        if n == target {
            return n;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    budget.available_permits()
}

// r[verify store.put.declared-reserve]
/// W9-BN — the acquire census, structural: a declared upload's permit
/// acquisition is SINGLE-SHOT PRE-STREAM. The hand-paced stream lets
/// the budget semaphore witness each phase: the full charge is gone
/// BEFORE the first chunk is sent, NO further permits move while the
/// chunks stream (the per-chunk acquire site is unreachable), and the
/// whole charge restores after commit. The trailer-mode contrast
/// baseline (the disclosed pre-fix pin): `accumulate_chunk` charges
/// per-chunk WHILE HOLDING prior chunks' permits — the hold-and-wait
/// shape this mode exists to kill.
#[tokio::test]
async fn declared_upload_reserves_single_shot_pre_stream() -> TestResult {
    // Inline session built by hand so the budget Arc is observable.
    let db = TestDb::new(&MIGRATOR).await;
    let service = StoreServiceImpl::new(db.pool.clone());
    let budget = service.nar_budget().clone();
    let (mut client, _server) = spawn_store_server(service).await?;

    let (nar, info) = declared_fixture("declared-census");
    let declared = nar.len() as u64;
    // The charge law: declared.max(MIN_NAR_CHUNK_CHARGE).
    let charge = (nar.len()).max(256);
    let before = budget.available_permits();

    let (tx, rx) = mpsc::channel::<PutPathRequest>(8);
    let call = tokio::spawn({
        let mut client = client.clone();
        async move { client.put_path(ReceiverStream::new(rx)).await }
    });

    // Phase 1: metadata only. The reservation must land with ZERO
    // chunks sent — single-shot pre-stream.
    tx.send(declared_metadata(&info, declared)).await?;
    let after_meta = wait_permits(&budget, before - charge).await;
    assert_eq!(
        after_meta,
        before - charge,
        "declared upload must reserve its WHOLE charge ({charge}) before \
         the first chunk (single-shot pre-stream); permits {before} -> {after_meta}"
    );

    // Phase 2: stream the chunks. NO further permit movement — the
    // per-chunk acquire site is structurally unreachable in declared
    // mode.
    for half in nar.chunks(nar.len() / 2 + 1) {
        tx.send(chunk_msg(half.to_vec())).await?;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(
            budget.available_permits(),
            before - charge,
            "no per-chunk acquires may run on the declared path"
        );
    }

    // Phase 3: trailer + close → success, permits restored.
    tx.send(trailer_msg(&info, declared)).await?;
    drop(tx);
    let resp = call.await??;
    assert!(resp.into_inner().created);
    let restored = wait_permits(&budget, before).await;
    assert_eq!(restored, before, "whole charge restores after commit");

    // Parity: the stored row is the trailer-mode shape.
    let got = client
        .query_path_info(QueryPathInfoRequest {
            store_path: info.store_path.to_string(),
        })
        .await?
        .into_inner();
    assert_eq!(got.nar_size, declared);
    assert_eq!(got.nar_hash, info.nar_hash.to_vec());
    Ok(())
}

// r[verify store.put.declared-reserve]
/// W9-BO axis 1 — OVER-DELIVERY refuses typed AT the bound: the chunk
/// that would push the buffer past `declared_nar_size` dies
/// `InvalidArgument` mid-stream (no trailer ever sent), and the
/// placeholder is aborted so a retry can claim immediately.
#[tokio::test]
async fn declared_over_delivery_refuses_at_the_bound() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = declared_fixture("declared-over");

    let (tx, rx) = mpsc::channel::<PutPathRequest>(8);
    tx.send(declared_metadata(&info, nar.len() as u64 - 1))
        .await?;
    tx.send(chunk_msg(nar.clone())).await?; // crosses the bound
    drop(tx);

    let status = s
        .client
        .put_path(ReceiverStream::new(rx))
        .await
        .expect_err("over-delivery must refuse");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("declared_nar_size"),
        "got: {}",
        status.message()
    );
    Ok(())
}

// r[verify store.put.declared-reserve]
/// W9-BO axis 2a — UNDER-DELIVERY, coherent-short sender: the stream
/// carries fewer bytes than declared and the trailer truthfully says
/// so → refused at commit on the trailer/declaration equality (the
/// declaration is binding, not advisory).
#[tokio::test]
async fn declared_under_delivery_coherent_trailer_refuses_at_commit() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = declared_fixture("declared-under-coherent");

    let (tx, rx) = mpsc::channel::<PutPathRequest>(8);
    tx.send(declared_metadata(&info, nar.len() as u64 + 5))
        .await?;
    tx.send(chunk_msg(nar.clone())).await?;
    tx.send(trailer_msg(&info, nar.len() as u64)).await?; // truthful
    drop(tx);

    let status = s
        .client
        .put_path(ReceiverStream::new(rx))
        .await
        .expect_err("short stream with truthful trailer must refuse");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("contradicts declared_nar_size"),
        "got: {}",
        status.message()
    );
    Ok(())
}

// r[verify store.put.declared-reserve]
/// W9-BO axis 2b — UNDER-DELIVERY, lying-short sender: the trailer
/// repeats the declaration but the bytes are short → dies on
/// `verify_nar`'s size check at commit (both delivery axes typed; no
/// arm admits a declared count the bytes don't back).
#[tokio::test]
async fn declared_under_delivery_lying_trailer_refuses_at_verify() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (nar, info) = declared_fixture("declared-under-lying");
    let declared = nar.len() as u64 + 5;

    let (tx, rx) = mpsc::channel::<PutPathRequest>(8);
    tx.send(declared_metadata(&info, declared)).await?;
    tx.send(chunk_msg(nar.clone())).await?;
    tx.send(trailer_msg(&info, declared)).await?; // repeats the lie
    drop(tx);

    let status = s
        .client
        .put_path(ReceiverStream::new(rx))
        .await
        .expect_err("lying trailer must refuse");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("size mismatch"),
        "got: {}",
        status.message()
    );
    Ok(())
}

// r[verify store.put.declared-reserve]
/// The bound gate: a declaration at/above `MAX_NAR_SIZE` refuses
/// up-front (before any claim/budget work — also what keeps the
/// charge u32-expressible).
#[tokio::test]
async fn declared_above_max_nar_size_refuses_up_front() -> TestResult {
    let mut s = StoreSession::new().await?;
    let (_nar, info) = declared_fixture("declared-oversize");

    let (tx, rx) = mpsc::channel::<PutPathRequest>(8);
    tx.send(declared_metadata(&info, u64::MAX)).await?;
    drop(tx);

    let status = s
        .client
        .put_path(ReceiverStream::new(rx))
        .await
        .expect_err("oversize declaration must refuse");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("exceeds size bound"),
        "got: {}",
        status.message()
    );
    Ok(())
}

// r[verify store.put.declared-reserve]
/// PutPathBatch REJECTS a nonzero declaration fail-closed: silently
/// ignoring it would look accepted on the wire while delivering none
/// of the reservation semantics the sender asked for.
#[tokio::test]
async fn batch_rejects_declared_nar_size() -> TestResult {
    use rio_proto::types::PutPathBatchRequest;
    let mut s = StoreSession::new().await?;
    let (_nar, info) = declared_fixture("declared-batch");

    let (tx, rx) = mpsc::channel::<PutPathBatchRequest>(8);
    tx.send(PutPathBatchRequest {
        output_index: 0,
        inner: Some(declared_metadata(&info, 64)),
    })
    .await?;
    drop(tx);

    let status = s
        .client
        .put_path_batch(ReceiverStream::new(rx))
        .await
        .expect_err("batch must reject declared mode");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("not supported on PutPathBatch"),
        "got: {}",
        status.message()
    );
    Ok(())
}

//! Atomic multi-output upload via `PutPathBatch`.

use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use rio_proto::StoreServiceClient;
use rio_proto::types::{PutPathBatchRequest, PutPathMetadata, PutPathRequest, put_path_request};
use rio_proto::validated::ValidatedPathInfo;

use super::UploadError;
use super::common::{
    PreparedOutput, STREAM_CHANNEL_BUF, attach_assignment_token, await_dump_after_rx_drop,
    spawn_dump_tee, trailer_mode_path_info, uploaded_info,
};

/// Compile-time half of the round-17 bug_111 closure: the batch capacity
/// equals the protocol's output-name capacity, so no parseable derivation
/// can exceed the batch path. Lives here because rio-common and rio-nix do
/// not depend on each other; this consumer sees both.
const _: () = assert!(
    rio_common::limits::MAX_BATCH_OUTPUTS == rio_nix::protocol::derived_path::MAX_OUTPUT_NAMES
);

/// Fixed per-output allowance in [`batch_stream_timeout`]: placeholder row
/// insert + trailer hash/size verification + tee/dump task switchover.
/// Observed per-output overhead is milliseconds; 15 s is sized so a
/// 256-output batch on a loaded store cannot trip the deadline on
/// bookkeeping alone, while still bounding a hung 256-output stream at
/// ~69 min (300 + 256x15 = 4140 s) — BELOW the old formula's 16-output
/// ceiling (300x16 = 4800 s), not 16x above it.
const PER_OUTPUT_STREAM_OVERHEAD: Duration = Duration::from_secs(15);

/// Batch deadline, derived from what can actually FLOW rather than from
/// the declared output count.
///
/// The server caps a batch's cumulative charged NAR bytes at one
/// `MAX_NAR_SIZE` (4 GiB; beyond that it answers `FailedPrecondition` and
/// the builder takes the documented independent-PutPath fallback), and
/// `GRPC_STREAM_TIMEOUT` is doc-sized for streaming exactly that many
/// bytes (one 4 GiB NAR at the ~14 MB/s floor). One stream-timeout
/// therefore covers the byte transfer of ANY batch the server will
/// accept, regardless of count; the count contributes only fixed
/// per-output overhead. R4 dominance: the deadline tracks the byte
/// BUDGET, not the declared shape — the previous `GRPC_STREAM_TIMEOUT x N`
/// formula scaled the byte term by count, which at the protocol-aligned
/// 256 would be a 21-hour hang ceiling for a transfer the budget caps at
/// ~5 minutes.
pub(super) fn batch_stream_timeout(n_outputs: usize) -> Duration {
    rio_common::grpc::GRPC_STREAM_TIMEOUT
        + PER_OUTPUT_STREAM_OVERHEAD * (n_outputs.min(rio_common::limits::MAX_BATCH_OUTPUTS) as u32)
}

// r[impl builder.upload.batch+3]
/// Batch upload: all outputs in one `PutPathBatch` stream, committed
/// atomically server-side.
///
/// Outputs are streamed **serially** (output 0 fully, then output 1, …).
/// Each output's NAR is hashed+streamed via the same `spawn_blocking` +
/// `HashingChannelWriter` tee as `do_upload_streaming`, with an outer
/// forwarding loop that tags each `PutPathRequest` with its `output_index`.
/// Peak memory: `STREAM_CHANNEL_BUF × 256 KiB` (~1 MiB) — same as the
/// single-output path, since outputs are serial.
///
/// All per-output prep (parse, ref-scan) is done by the caller via
/// [`PreparedOutput`] BEFORE this is called, so the producer task is
/// prep-free: a prep failure cannot leave outputs 0..k-1 committed
/// while the client returns `Err` (`r[store.atomic.multi-output]`).
///
/// One-shot per call; the caller (`upload_all_outputs`) retries on
/// transient errors and falls back to independent `PutPath` on
/// `FailedPrecondition`.
#[instrument(skip_all, fields(outputs = prepared.len()))]
pub(super) async fn upload_outputs_batch(
    store_client: &StoreServiceClient<Channel>,
    prepared: &[PreparedOutput],
    assignment_token: &str,
    deriver: &str,
) -> Result<Vec<ValidatedPathInfo>, UploadError> {
    let (tx, rx) = mpsc::channel::<PutPathBatchRequest>(STREAM_CHANNEL_BUF);
    let batch_timeout = batch_stream_timeout(prepared.len());

    // Producer task: for each (already-prepared) output, send tagged
    // metadata → spawn_blocking dump into an inner channel → forward
    // inner messages tagged with output_index. Prep-free: returns only
    // (nar_hash, nar_size) per output; `uploaded_info` is built after
    // the gRPC commit so a post-stream failure cannot desync client and
    // server state.
    let prepared_owned: Vec<PreparedOutput> = prepared.to_vec();
    let deriver_owned = deriver.to_string();

    let producer = tokio::spawn(async move {
        let mut hashes: Vec<([u8; 32], u64)> = Vec::with_capacity(prepared_owned.len());
        for (idx, p) in prepared_owned.iter().enumerate() {
            let idx = idx as u32;

            // Metadata message (trailer mode — empty hash/size).
            let info = trailer_mode_path_info(
                &p.store_path,
                &deriver_owned,
                &p.references,
                p.content_address.as_deref(),
            );
            tx.send(PutPathBatchRequest {
                output_index: idx,
                inner: Some(PutPathRequest {
                    msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
                        info: Some(info),
                    })),
                }),
            })
            .await
            .map_err(|_| tonic::Status::internal("batch channel closed (metadata)"))?;

            // Inner channel: spawn_blocking produces PutPathRequest, async
            // side wraps + forwards. Same tee pattern as do_upload_streaming.
            let (inner_tx, mut inner_rx) = mpsc::channel::<PutPathRequest>(STREAM_CHANNEL_BUF);
            let dump_task = spawn_dump_tee(p.host_path.clone(), inner_tx);

            // Forward chunks + trailer, tagging with output_index.
            while let Some(inner) = inner_rx.recv().await {
                tx.send(PutPathBatchRequest {
                    output_index: idx,
                    inner: Some(inner),
                })
                .await
                .map_err(|_| tonic::Status::internal("batch channel closed (forward)"))?;
            }

            let (nar_hash, nar_size) = dump_task
                .await
                .map_err(|e| tonic::Status::internal(format!("dump task panicked: {e}")))??;
            hashes.push((nar_hash, nar_size));
        }
        // tx drops here → batch stream closes → server enters commit phase.
        Ok::<_, tonic::Status>(hashes)
    });

    // Drive the gRPC call. The producer feeds `rx` concurrently. One-shot
    // per call (caller retries) → all errors map to UploadExhausted with a
    // synthetic "<batch>" path.
    let batch_err = |source| UploadError::UploadExhausted {
        path: "<batch>".into(),
        source,
    };
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut req = tonic::Request::new(outbound);
    attach_assignment_token(&mut req, assignment_token).map_err(batch_err)?;

    let mut client = store_client.clone();
    let put_result = rio_common::grpc::with_timeout_status(
        "PutPathBatch",
        batch_timeout,
        client.put_path_batch(req),
    )
    .await;

    // Join producer with a bound: if a spawn_blocking dump is parked in a
    // sync read() (FIFO, wedged FUSE), the inner channel never closes and
    // `inner_rx.recv()` / `dump_task.await` inside the producer pend
    // forever — the outer rx-drop from the gRPC timeout is invisible at
    // those await points. The blocking thread leaks (tokio limitation);
    // the worker regains control and fails the build instead of hanging.
    // Producer already had `batch_timeout` of wall-clock concurrently with
    // the gRPC await above, so only `DUMP_JOIN_SLACK` is needed here.
    // Same error-priority logic as do_upload_streaming: gRPC error wins.
    let producer_result = await_dump_after_rx_drop("batch producer", producer).await;

    let resp = put_result.map_err(batch_err)?;
    let hashes = producer_result.and_then(|r| r).map_err(batch_err)?;

    // Server has committed all N. Build ValidatedPathInfo from prep +
    // hashes. `uploaded_info` can only fail on InvalidReference (a path
    // CandidateSet validated then failed to re-parse — invariant
    // violation); at this point state is consistent (atomic) and the
    // caller's retry idempotency-hits.
    let mut results: Vec<ValidatedPathInfo> = Vec::with_capacity(prepared.len());
    for (p, (nar_hash, nar_size)) in prepared.iter().zip(hashes) {
        results.push(uploaded_info(
            p.parsed.clone(),
            nar_hash,
            nar_size,
            p.references.clone(),
            deriver,
            p.content_address.clone(),
        )?);
    }

    let created = resp.into_inner().created;
    tracing::info!(
        outputs = results.len(),
        created = created.iter().filter(|&&c| c).count(),
        "batch upload committed atomically"
    );
    for r in &results {
        metrics::counter!("rio_builder_uploads_total", "status" => "success").increment(1);
        metrics::counter!("rio_builder_upload_bytes_total").increment(r.nar_size);
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::grpc::GRPC_STREAM_TIMEOUT;
    use rio_common::limits::MAX_BATCH_OUTPUTS;

    #[test]
    fn test_batch_stream_timeout_budget_derived() {
        // One byte-budget term plus fixed per-output overhead.
        assert_eq!(
            batch_stream_timeout(1),
            GRPC_STREAM_TIMEOUT + PER_OUTPUT_STREAM_OVERHEAD
        );
        assert_eq!(
            batch_stream_timeout(3),
            GRPC_STREAM_TIMEOUT + PER_OUTPUT_STREAM_OVERHEAD * 3
        );
        // Hostile/malformed n clamps at the protocol-aligned capacity...
        assert_eq!(
            batch_stream_timeout(usize::MAX),
            batch_stream_timeout(MAX_BATCH_OUTPUTS)
        );
        // ...and the 256-output worst case stays BELOW the old 16-output
        // ceiling (GRPC_STREAM_TIMEOUT x 16): the deadline tracks the byte
        // budget, not the count.
        assert!(batch_stream_timeout(MAX_BATCH_OUTPUTS) < GRPC_STREAM_TIMEOUT * 16);
        // Degenerate: empty slice still gets a non-zero budget.
        assert_eq!(batch_stream_timeout(0), GRPC_STREAM_TIMEOUT);
    }
}

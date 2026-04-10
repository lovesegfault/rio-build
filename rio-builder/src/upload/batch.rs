//! Atomic multi-output upload via `PutPathBatch`.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::refscan::CandidateSet;
use rio_nix::store_path::StorePath;
use rio_proto::StoreServiceClient;
use rio_proto::types::{PutPathBatchRequest, PutPathMetadata, PutPathRequest, put_path_request};
use rio_proto::validated::ValidatedPathInfo;

use super::UploadError;
use super::common::{
    STREAM_CHANNEL_BUF, attach_assignment_token, scan_references, spawn_dump_tee,
    trailer_mode_path_info, uploaded_info,
};

// r[impl builder.upload.batch]
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
/// No per-output retry: batch is all-or-nothing at the gRPC level too.
/// The caller (`upload_all_outputs`) falls back to independent `PutPath`
/// on `FailedPrecondition` (the one error code the batch handler uses to
/// signal "use the other path").
#[instrument(skip_all, fields(outputs = outputs.len()))]
pub(super) async fn upload_outputs_batch(
    store_client: &StoreServiceClient<Channel>,
    upper_store: &Path,
    outputs: &[String],
    assignment_token: &str,
    deriver: &str,
    candidates: &Arc<CandidateSet>,
) -> Result<Vec<ValidatedPathInfo>, UploadError> {
    let (tx, rx) = mpsc::channel::<PutPathBatchRequest>(STREAM_CHANNEL_BUF);

    // Producer task: for each output, pre-scan refs → send tagged metadata →
    // spawn_blocking dump into an inner channel → forward inner messages
    // tagged with output_index. Returns the Vec<ValidatedPathInfo> on success.
    //
    // Cloned inputs for the `spawn`ed task (it needs 'static). The outputs
    // list is cloned once (basenames, small).
    let upper_store = upper_store.to_path_buf();
    let outputs_owned: Vec<String> = outputs.to_vec();
    let deriver_owned = deriver.to_string();
    let candidates = Arc::clone(candidates);

    let producer = tokio::spawn(async move {
        let mut results: Vec<ValidatedPathInfo> = Vec::with_capacity(outputs_owned.len());
        for (idx, basename) in outputs_owned.iter().enumerate() {
            let output_path = upper_store.join(basename);
            let store_path = format!("/nix/store/{basename}");
            let idx = idx as u32;

            // Validate store path format (same guard as `upload_output`).
            let parsed_path = StorePath::parse(&store_path).map_err(|e| {
                tonic::Status::invalid_argument(format!(
                    "output {idx}: store path {store_path:?} malformed: {e}"
                ))
            })?;

            // Pre-scan refs (same pattern as upload_output).
            let references = scan_references(&output_path, &candidates).await?;

            // Metadata message (trailer mode — empty hash/size).
            let info = trailer_mode_path_info(&store_path, &deriver_owned, &references);
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
            let dump_task = spawn_dump_tee(output_path, inner_tx);

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

            results.push(
                uploaded_info(parsed_path, nar_hash, nar_size, references, &deriver_owned)
                    .map_err(|e| tonic::Status::internal(e.to_string()))?,
            );
        }
        // tx drops here → batch stream closes → server enters commit phase.
        Ok::<_, tonic::Status>(results)
    });

    // Drive the gRPC call. The producer feeds `rx` concurrently.
    // Batch is one-shot (no retries) → all errors map to UploadExhausted
    // with a synthetic "<batch>" path.
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
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
        client.put_path_batch(req),
    )
    .await;

    // Join producer. Same error-priority logic as do_upload_streaming:
    // gRPC error wins (it's what the operator cares about).
    let producer_result = producer
        .await
        .map_err(|e| tonic::Status::internal(format!("batch producer panicked: {e}")));

    let resp = put_result.map_err(batch_err)?;
    let results = producer_result.and_then(|r| r).map_err(batch_err)?;

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

//! Output collection: FOD verify, upload, proto BuildResult mapping.
//!
//! Runs after the native result pipeline (`native_result`) has
//! classified the exit and processed the outputs. On success: verifies
//! FOD hashes, uploads outputs, maps upload results to proto
//! `BuiltOutput` entries. On build failure: maps the classification to
//! the proto equivalent.

use std::collections::HashMap;

use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::derivation::Derivation;
use rio_proto::StoreServiceClient;
use rio_proto::types::{BuildResult as ProtoBuildResult, BuildResultStatus, BuiltOutput};

use crate::overlay;
use crate::upload;

use super::ExecutorError;
use super::inputs::verify_fod_hashes;
use super::native_result::ProcessedOutputs;

/// Collected build outputs: proto BuildResult.
pub(super) struct BuildOutputs {
    /// Proto BuildResult to send to the scheduler in CompletionReport.
    pub(super) proto_result: ProtoBuildResult,
}

impl BuildOutputs {
    /// Failure-path shorthand: status + error_msg only, all other
    /// `ProtoBuildResult` fields default.
    fn failed(status: BuildResultStatus, error_msg: impl Into<String>) -> Self {
        Self {
            proto_result: ProtoBuildResult {
                status: status.into(),
                error_msg: error_msg.into(),
                ..Default::default()
            },
        }
    }
}

/// Collect outputs for a build that ran through the NATIVE executor
/// pipeline (`rio_exec` + `native_result`): FOD verify, upload (with
/// the precomputed reference sets and content addresses), and proto
/// `BuildResult` assembly.
///
/// Differences from the daemon-era output collection:
/// - the per-output reference sets, NAR hashes, and realized (post
///   floating-CA-finalization) paths come from [`ProcessedOutputs`]
///   instead of the daemon's `BuildResult`, so the stray-file check is
///   "uploaded path ∉ processed outputs" rather than a name lookup;
/// - the upload still re-streams each output from disk (the store needs
///   the NAR bytes regardless); the scan pass inside `prepare_output`
///   is redundant with the pipeline's scan and is kept for now —
///   TODO: pass the precomputed reference sets into the upload module
///   and drop its rescan (it re-scans every output NAR for store-path
///   references the result pipeline has already extracted).
/// - `content_address` is threaded through so floating-CA narinfos
///   carry their `CA:` field.
#[instrument(skip_all, fields(drv_path = %drv_path, is_fod))]
#[allow(clippy::too_many_arguments)]
pub(super) async fn collect_native_outputs(
    processed: &ProcessedOutputs,
    store_client: &mut StoreServiceClient<Channel>,
    overlay_mount: &overlay::OverlayMount,
    drv: &Derivation,
    drv_path: &str,
    is_fod: bool,
    input_paths: &[String],
    assignment_token: &str,
    start_time: u64,
    stop_time: u64,
) -> Result<BuildOutputs, ExecutorError> {
    // FOD defense-in-depth BEFORE upload — same gate, same rationale as
    // the daemon-era path (the comment block on `collect_outputs`); the
    // native pipeline's policy checks do not verify the declared output
    // hash, only `verify_fod_hashes` does.
    if is_fod {
        let drv_for_verify = drv.clone();
        let upper_store_for_verify = overlay_mount.upper_store();
        let verify_result = crate::upload::common::await_dump_bounded(
            "FOD verify",
            rio_common::grpc::GRPC_STREAM_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                verify_fod_hashes(&drv_for_verify, &upper_store_for_verify)
            }),
        )
        .await
        .map_err(|s| ExecutorError::BuildFailed(s.message().to_owned()))?;

        if let Err(e) = verify_result {
            tracing::error!(
                drv_path = %drv_path,
                error = %e,
                "FOD output hash verification failed — NOT uploading"
            );
            // r[impl fetcher.upload.hash-verify-before]
            return Ok(BuildOutputs::failed(
                BuildResultStatus::OutputRejected,
                format!("FOD output hash verification failed: {e}"),
            ));
        }
    }

    tracing::info!(drv_path = %drv_path, "build succeeded, uploading outputs");

    // Reference-scan candidate set: the input closure ∪ every realized
    // output path (final CA paths included — self/sibling references in
    // floating-CA outputs resolve against these).
    let mut ref_candidates: Vec<String> = input_paths.to_vec();
    ref_candidates.extend(processed.outputs.iter().map(|o| o.store_path.clone()));

    // store path → content-address descriptor for floating-CA outputs.
    let content_addresses: HashMap<String, String> = processed
        .outputs
        .iter()
        .filter_map(|o| {
            o.content_address
                .clone()
                .map(|ca| (o.store_path.clone(), ca))
        })
        .collect();

    match upload::upload_all_outputs(
        store_client,
        &overlay_mount.upper_store(),
        assignment_token,
        drv_path,
        &ref_candidates,
        &content_addresses,
    )
    .await
    {
        Ok(upload_results) => {
            // Stray-file gate (wkr-scan-unfiltered): every uploaded path
            // must be one of the pipeline's processed outputs. The
            // realized paths are authoritative here — the .drv's static
            // outputs don't know floating-CA final paths.
            let processed_by_path: HashMap<&str, &super::native_result::ProcessedOutput> =
                processed
                    .outputs
                    .iter()
                    .map(|o| (o.store_path.as_str(), o))
                    .collect();
            let mut built_outputs: Vec<BuiltOutput> = Vec::with_capacity(upload_results.len());
            for result in &upload_results {
                let store_path = result.store_path.as_str();
                let Some(out) = processed_by_path.get(store_path) else {
                    tracing::warn!(
                        store_path = %store_path,
                        "uploaded path not in processed outputs — rejecting build"
                    );
                    return Err(ExecutorError::BuildFailed(format!(
                        "uploaded path {store_path} not in processed outputs \
                         (stray file in overlay upper /nix/store?)"
                    )));
                };
                built_outputs.push(BuiltOutput {
                    output_name: out.name.clone(),
                    output_path: store_path.to_string(),
                    output_hash: out.nar_hash.to_vec(),
                });
            }

            let to_proto_ts = |secs: u64| {
                (secs > 0).then_some(prost_types::Timestamp {
                    seconds: secs as i64,
                    nanos: 0,
                })
            };
            Ok(BuildOutputs {
                proto_result: ProtoBuildResult {
                    status: BuildResultStatus::Built.into(),
                    error_msg: String::new(),
                    start_time: to_proto_ts(start_time),
                    stop_time: to_proto_ts(stop_time),
                    built_outputs,
                },
            })
        }
        Err(e) => {
            tracing::error!(drv_path = %drv_path, error = %e, "output upload failed");
            Ok(BuildOutputs::failed(
                BuildResultStatus::InfrastructureFailure,
                format!("output upload failed: {e}"),
            ))
        }
    }
}

/// Failure-path constructor for the native pipeline: the build (or its
/// output processing) failed with an already-classified status.
pub(super) fn native_failed(
    status: BuildResultStatus,
    error_msg: impl Into<String>,
) -> BuildOutputs {
    BuildOutputs::failed(status, error_msg)
}

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
///   floating-CA-finalization) paths come from [`ProcessedOutputs`] and
///   are exactly what the upload registers — the upload neither
///   rediscovers outputs by scanning the overlay upper layer nor
///   re-scans references, so `unsafeDiscardReferences` decisions
///   survive to registration and a stray store path the build left
///   behind is never uploaded (CppNix discards such paths with the
///   chroot; strays are logged for observability, not fatal);
/// - `content_address` is threaded through so content-addressed
///   narinfos carry their `CA:` field — floating-CA outputs and strict
///   fixed-output derivations (`is_fixed_output()`) carry their
///   descriptor; input-addressed outputs carry `None`.
#[instrument(skip_all, fields(drv_path = %drv_path, is_fod))]
#[allow(clippy::too_many_arguments)]
pub(super) async fn collect_native_outputs(
    processed: &ProcessedOutputs,
    store_client: &mut StoreServiceClient<Channel>,
    overlay_mount: &overlay::OverlayMount,
    drv: &Derivation,
    drv_path: &str,
    is_fod: bool,
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

    // Upload exactly the pipeline's processed outputs: realized (post
    // floating-CA-finalization) store paths with their recorded
    // (post-`unsafeDiscardReferences`) reference sets and CA
    // descriptors.
    let to_upload: Vec<upload::OutputToUpload> = processed
        .outputs
        .iter()
        .map(|o| upload::OutputToUpload {
            store_path: o.store_path.clone(),
            host_path: o.host_path.clone(),
            references: o.references.clone(),
            content_address: o.content_address.clone(),
        })
        .collect();

    // Observability for stray store paths the build left behind: CppNix
    // silently discards anything in the chroot store that is not a
    // declared output, and so do we (they are simply never uploaded) —
    // but a build writing them is unusual enough to be worth a warning.
    match upload::scan_new_outputs(&overlay_mount.upper_store()) {
        Ok(found) => {
            let expected: std::collections::HashSet<&str> = processed
                .outputs
                .iter()
                .filter_map(|o| o.store_path.strip_prefix("/nix/store/"))
                .collect();
            for name in found {
                if !expected.contains(name.as_str()) {
                    tracing::warn!(
                        drv_path = %drv_path,
                        stray = %name,
                        "build left a stray store path in the overlay upper layer; not uploading it"
                    );
                }
            }
        }
        Err(e) => tracing::debug!(
            drv_path = %drv_path,
            error = %e,
            "could not scan the overlay upper store for stray paths"
        ),
    }

    match upload::upload_all_outputs(store_client, assignment_token, drv_path, &to_upload).await {
        Ok(upload_results) => {
            // Defensive map back to the pipeline's processed outputs (the
            // upload set is constructed from them, so a miss here is an
            // internal error, not a build failure mode). The realized
            // paths are authoritative — the .drv's static outputs don't
            // know floating-CA final paths.
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
                    tracing::error!(
                        store_path = %store_path,
                        "upload result does not correspond to any processed output"
                    );
                    return Err(ExecutorError::BuildFailed(format!(
                        "internal error: upload result {store_path} does not correspond to \
                         any processed output"
                    )));
                };
                built_outputs.push(BuiltOutput {
                    output_name: out.name.clone(),
                    output_path: store_path.to_string(),
                    // The hash comes from the upload result, not the local
                    // ProcessedOutput: for freshly-uploaded outputs the two
                    // are the same NAR hash, but for outputs skipped by the
                    // idempotent pre-check the result carries the STORE's
                    // nar_hash — the bytes that actually exist — which is
                    // what the scheduler's realisations row must record
                    // (the upload layer pins that contract in its
                    // skipped-path test).
                    output_hash: result.nar_hash.to_vec(),
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

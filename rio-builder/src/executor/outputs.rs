//! Output collection: FOD verify, upload, daemon→proto BuildResult mapping.
//!
//! Runs after daemon teardown. On success: verifies FOD hashes, uploads
//! outputs, maps upload results to proto `BuiltOutput` entries. On build
//! failure: maps the nix-daemon `BuildStatus` to the proto equivalent and
//! reclassifies input-materialization failures (I-178) before they poison
//! the derivation.

use std::collections::HashMap;

use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::derivation::{Derivation, DerivationLike};
use rio_proto::StoreServiceClient;
use rio_proto::types::{BuildResult as ProtoBuildResult, BuildResultStatus, BuiltOutput};

use crate::overlay;
use crate::upload;

use super::ExecutorError;
use super::inputs::verify_fod_hashes;

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

/// Collect build outputs: FOD verify, upload, map to proto BuildResult.
///
/// Runs after daemon teardown. On success: verifies FOD hashes (if
/// applicable), uploads outputs to the store, maps upload results to
/// proto BuiltOutput entries. On build failure: maps the nix-daemon
/// BuildStatus to the proto equivalent.
///
/// Reference-scan candidate set = input_paths ∪ drv.outputs() ∪
/// build_result.built_outputs (the last for floating-CA self-refs).
#[instrument(skip_all, fields(drv_path = %drv_path, is_fod))]
#[allow(clippy::too_many_arguments)]
pub(super) async fn collect_outputs(
    build_result: &rio_nix::protocol::build::BuildResult,
    store_client: &mut StoreServiceClient<Channel>,
    overlay_mount: &overlay::OverlayMount,
    drv: &Derivation,
    drv_path: &str,
    is_fod: bool,
    input_paths: &[String],
    assignment_token: &str,
) -> Result<BuildOutputs, ExecutorError> {
    if !build_result.status.is_success() {
        // I-178: daemon ENOENT on a closure input is worker-local
        // materialization failure (warm timeout / FUSE EIO / I-043
        // negative-dentry), NOT a build defect. Reclassify so the
        // scheduler retries instead of poisoning. Checked BEFORE the
        // generic BuildResultStatus::from collapse (MiscFailure →
        // PermanentFailure).
        if is_input_materialization_failure(
            build_result.status,
            &build_result.error_msg,
            input_paths,
        ) {
            // r[impl obs.metric.input-materialization-failures]
            metrics::counter!("rio_builder_input_materialization_failures_total").increment(1);
            tracing::warn!(
                drv_path = %drv_path,
                error = %build_result.error_msg,
                "daemon ENOENT on closure input — reclassifying MiscFailure → \
                 InfrastructureFailure (warm timeout / FUSE EIO / I-043 race)"
            );
            return Ok(BuildOutputs::failed(
                BuildResultStatus::InfrastructureFailure,
                format!(
                    "input materialization failed (I-043/I-178): {}",
                    build_result.error_msg
                ),
            ));
        }
        tracing::warn!(
            drv_path = %drv_path,
            status = ?build_result.status,
            error = %build_result.error_msg,
            "build failed"
        );
        return Ok(BuildOutputs::failed(
            BuildResultStatus::from(build_result.status),
            build_result.error_msg.clone(),
        ));
    }

    // FOD defense-in-depth BEFORE upload: verify_fod_hashes
    // computes local NAR hashes (via dump_path_streaming +
    // digest sink) so a bad output is rejected WITHOUT entering
    // the store. Verifying after upload would mean the bad
    // output is already content-indexed and manifest-complete
    // before the mismatch is noticed.
    //
    // spawn_blocking: verify_fod_hashes does sync filesystem I/O
    // (io::copy for flat, dump_path_streaming for recursive) +
    // hashing. Both branches stream — O(1) memory regardless of
    // output size (fetchurl flat-hashed blobs reach multi-GB).
    if is_fod {
        let drv_for_verify = drv.clone();
        let upper_store_for_verify = overlay_mount.upper_store();
        let verify_result = tokio::task::spawn_blocking(move || {
            verify_fod_hashes(&drv_for_verify, &upper_store_for_verify)
        })
        .await
        .map_err(|e| ExecutorError::BuildFailed(format!("FOD verify task panicked: {e}")))?;

        if let Err(e) = verify_result {
            tracing::error!(
                drv_path = %drv_path,
                error = %e,
                "FOD output hash verification failed — NOT uploading"
            );
            // Build DID run (FOD verification is post-build). Caller
            // already has peak_memory_bytes/peak_cpu_cores from the
            // cgroup; they're meaningful even though we reject output.
            // r[impl fetcher.upload.hash-verify-before]
            return Ok(BuildOutputs::failed(
                BuildResultStatus::OutputRejected,
                format!("FOD output hash verification failed: {e}"),
            ));
        }
    }

    tracing::info!(drv_path = %drv_path, "build succeeded, uploading outputs");

    // Upload outputs.
    //
    // Reference-scan candidate set = input_paths ∪ drv.outputs():
    //   - input_paths: the TRANSITIVE input closure, built above via
    //     compute_input_closure (BFS over QueryPathInfo.references,
    //     seeded from input_srcs + inputDrv outputs). This matches
    //     Nix's computeFSClosure — see derivation-building-goal.cc:444,450
    //     and derivation-builder.cc:1335-1344 in Nix 2.31.3. A build can
    //     legitimately embed any path reachable from its inputs: e.g.
    //     hello-2.12.2 references glibc, which is NOT a direct input
    //     but comes via closure(stdenv). Scanning only direct inputs
    //     would drop those references.
    //   - drv.outputs(): self-references and cross-output references are
    //     legal (e.g., a -dev output referencing the lib output's rpath,
    //     or a binary embedding its own store path in an rpath).
    let mut ref_candidates: Vec<String> = input_paths.to_vec();
    ref_candidates.extend(drv.static_outputs().map(|o| o.path().to_string()));
    // Floating-CA: .drv has path = ""; the real path comes from
    // the daemon's BuildResult. Needed for self-references.
    ref_candidates.extend(
        build_result
            .built_outputs
            .iter()
            .map(|bo| bo.out_path.clone()),
    );

    match upload::upload_all_outputs(
        store_client,
        &overlay_mount.upper_store(),
        // Pass the assignment token as gRPC metadata on each
        // PutPath. Store with hmac_verifier checks it. Empty
        // token (scheduler without hmac_signer, dev mode) →
        // no header → store with verifier=None accepts.
        assignment_token,
        drv_path,
        &ref_candidates,
    )
    .await
    {
        Ok(upload_results) => {
            // Map store_path → output_name. Upload results are
            // unordered (buffer_unordered), and even the prior
            // sequential scan had undefined order (read_dir).
            //
            // Two sources:
            //  - drv.outputs(): works for IA and fixed-CA, where
            //    the .drv has the output path baked in.
            //  - build_result.built_outputs: for floating-CA
            //    (__contentAddressed = true), the .drv has
            //    path = "" (computed post-build from NAR hash).
            //    The daemon's BuildResult carries the realized
            //    path in its Realisation entries; the output
            //    name is the suffix of drv_output_id after '!'.
            //
            // Without the second source, CA builds fail the
            // lookup below with "not in derivation outputs" —
            // the upload scanned the real /nix/store/<hash>-name
            // but path_to_name only had "" → name.
            let mut path_to_name: HashMap<&str, &str> =
                drv.static_outputs().map(|o| (o.path(), o.name())).collect();
            for bo in &build_result.built_outputs {
                if let Some(name) = bo.drv_output_id.rsplit('!').next() {
                    path_to_name.insert(bo.out_path.as_str(), name);
                }
            }
            // wkr-scan-unfiltered (21-p2-p3-rollup Batch B): if the
            // lookup misses, scan_new_outputs picked up a stray file
            // under /nix/store (tempfile leak, .drv, etc.) that is NOT
            // a declared derivation output. Prior behavior: warn and
            // upload anyway with basename-as-output-name → phantom
            // output reported to scheduler. Now: fail the build. The
            // stray upload has already hit the store (upload_all_outputs
            // ran first) but it's unreferenced and GC-eligible; the
            // scheduler won't mark this drv Built so nothing downstream
            // can depend on the phantom.
            let built_outputs: Vec<BuiltOutput> = upload_results
                .iter()
                .map(|result| {
                    let output_name = path_to_name
                        .get(result.store_path.as_str())
                        .map(|s| s.to_string())
                        .ok_or_else(|| {
                            tracing::warn!(
                                store_path = %result.store_path,
                                "uploaded path not in derivation outputs — rejecting build"
                            );
                            ExecutorError::BuildFailed(format!(
                                "uploaded path {} not in derivation outputs (stray file in overlay upper /nix/store?)",
                                result.store_path
                            ))
                        })?;
                    Ok::<_, ExecutorError>(BuiltOutput {
                        output_name,
                        output_path: result.store_path.to_string(),
                        output_hash: result.nar_hash.to_vec(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            // start/stop_time: nix-daemon's BuildResult already has
            // these as Unix epoch seconds (rio-nix/build.rs:118-120).
            // Scheduler guards write_build_sample on BOTH being Some
            // (completion.rs duration check) — without them, duration
            // can't be computed and the WHOLE build_samples write is
            // skipped (peak_memory_bytes included). VM tests bypass
            // this via direct psql INSERT — only live scheduler-worker
            // integration exercises it.
            //
            // 0 → None: nix-daemon sends 0 on some error paths.
            // A real build at 1970-01-01 doesn't exist.
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
                    start_time: to_proto_ts(build_result.start_time),
                    stop_time: to_proto_ts(build_result.stop_time),
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

/// True iff the daemon's `MiscFailure` is `getting attributes of path
/// '<p>': No such file or directory` where `<p>` is in the build's
/// input closure.
///
/// I-178: that pattern means the daemon's sandbox-setup `lstat(input)`
/// hit overlay → FUSE → ENOENT (warm timeout, FUSE EIO, or the I-043
/// negative-dentry race). The input was verified present in rio-store
/// by `compute_input_closure` (BatchQueryPathInfo only returns found
/// paths); its absence at sandbox-setup is a worker-local
/// materialization failure, NOT a build defect. Reporting
/// `PermanentFailure` poisons the derivation; `InfrastructureFailure`
/// lets the scheduler retry on a fresh worker.
///
/// String-matching the daemon's error is brittle but the message is
/// stable since Nix 2.3 (`libstore/posix-fs-canonicalise.cc`). The
/// `<p> ∈ input_paths` membership check is the load-bearing guard — a
/// genuinely-missing path NOT in the closure stays `PermanentFailure`.
///
/// I-178b: live cluster output is ANSI-colored AND the path the daemon
/// reports is the OVERLAY path (`/var/rio/overlays/<build_id>/nix/store/
/// <hash>-<name>`), not the bare `/nix/store/<hash>-<name>` we have in
/// `input_paths`. So: strip ANSI escapes first, then match by BASENAME
/// only — `<hash>-<name>` is unique (the nixbase32 hash makes
/// collisions practically impossible) and is the trailing path component
/// in both overlay and store-path forms. The errno suffix is NOT
/// matched: ENOENT (`No such file or directory`) and EIO (`Input/output
/// error`, see I-179) are both worker-local materialization failures.
///
// r[impl builder.result.input-enoent-is-infra+2]
pub(crate) fn is_input_materialization_failure(
    nix_status: rio_nix::protocol::build::BuildStatus,
    error_msg: &str,
    input_paths: &[String],
) -> bool {
    use rio_nix::protocol::build::BuildStatus;
    use std::sync::LazyLock;

    // ANSI SGR escapes: ESC [ <params> m. nix-daemon emits 31;1 (bold red)
    // and 35;1 (bold magenta) around `error:` and the quoted path. Stripping
    // BEFORE the substring split means the `'...'` extract sees clean text.
    static ANSI: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;]*m").expect("static regex"));

    if nix_status != BuildStatus::MiscFailure {
        return false;
    }
    let stripped = ANSI.replace_all(error_msg, "");
    // nix-daemon's input-stat path emits one of several phrasings
    // depending on which libutil helper failed (`lstat`/`stat`/
    // `getFileType`/`readDirectory`):
    //   • "getting attributes of path '<p>': <strerror>"  (lstat)
    //   • "getting status of '<p>': <strerror>"            (stat)
    //   • "reading directory '<p>': <strerror>"            (readdir)
    //   • "opening file '<p>': <strerror>"                 (open)
    // I-189: matching only the first missed `getting status of`. All
    // four indicate the same materialization failure when `<p>` is a
    // closure input. Closure ≤ ~2k entries; linear scan is fine.
    const MARKERS: &[&str] = &[
        "getting attributes of path '",
        "getting status of '",
        "reading directory '",
        "opening file '",
    ];
    let Some(rest) = MARKERS.iter().find_map(|m| stripped.split(m).nth(1)) else {
        return false;
    };
    let Some(path) = rest.split('\'').next() else {
        return false;
    };
    // Basename match: the daemon reports the overlay path (I-178b); the
    // closure has store paths. Both end in `<hash>-<name>`. An empty
    // basename (trailing slash — shouldn't happen, but defensive) must
    // not vacuously match every closure entry.
    let basename = path.rsplit('/').next().unwrap_or(path);
    if basename.is_empty() {
        return false;
    }
    input_paths
        .iter()
        .any(|p| p.rsplit('/').next().unwrap_or(p) == basename)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// I-178: daemon `MiscFailure` with `getting attributes of path
    /// '<input>': No such file or directory` is a worker-local
    /// materialization failure (warm timeout / FUSE EIO / I-043 race),
    /// not a build defect. The membership check is load-bearing — a
    /// path NOT in the closure stays PermanentFailure.
    ///
    /// I-178b: the live cluster message is ANSI-colored and reports the
    /// OVERLAY path, not the store path. Strip ANSI; match by basename.
    // r[verify builder.result.input-enoent-is-infra+2]
    #[test]
    fn test_is_input_materialization_failure() {
        use rio_nix::protocol::build::BuildStatus as Nix;

        let input = "/nix/store/54f75pjisgz20ql6azwmck1v779xs0a9-source".to_string();
        let other = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello".to_string();
        let closure = vec![input.clone(), other.clone()];
        let enoent = format!(
            "while setting up the build environment: getting attributes of \
             path '{input}': No such file or directory"
        );

        // MiscFailure + matching path → true (reclassify to infra).
        assert!(
            is_input_materialization_failure(Nix::MiscFailure, &enoent, &closure),
            "ENOENT on closure input must reclassify"
        );

        // I-178b regression: literal cluster output. ANSI SGR escapes
        // around `error:` and the quoted path; the path is the OVERLAY
        // path (`/var/rio/overlays/<build_id>/nix/store/<basename>`),
        // not the bare store path. Basename match must catch it.
        let ansi_overlay = "\u{1b}[31;1merror:\u{1b}[0m\n       \
             … while setting up the build environment\n\n       \
             \u{1b}[31;1merror:\u{1b}[0m getting attributes of path \
             '\u{1b}[35;1m/var/rio/overlays/\
             vwb2lprckpd4kbg67sczakiqqqd4jxzy-llvm-tblgen-src-21_1_8_drv\
             /nix/store/54f75pjisgz20ql6azwmck1v779xs0a9-source\u{1b}[0m': \
             \u{1b}[35;1mNo such file or directory\u{1b}[0m";
        assert!(
            is_input_materialization_failure(Nix::MiscFailure, ansi_overlay, &closure),
            "I-178b: ANSI-wrapped overlay path must reclassify by basename"
        );

        // I-179 coupling: EIO suffix (not ENOENT) is also a
        // materialization failure — the matcher keys on the prefix +
        // path, not the strerror suffix.
        let eio = format!("getting attributes of path '{input}': Input/output error");
        assert!(
            is_input_materialization_failure(Nix::MiscFailure, &eio, &closure),
            "EIO on closure input must reclassify (I-179 wait_for_fetcher)"
        );

        // I-189: nix's stat() helper says "getting status of" (not
        // "getting attributes of path"). Literal cluster output:
        // `getting status of '<overlay>/nix/store/<basename>': EIO`.
        let stat_eio = "while setting up the build environment: getting status of \
             '/var/rio/overlays/jrk1q0f3isaddmfgawh7k391fzsa0mc9-glibc_drv\
             /nix/store/54f75pjisgz20ql6azwmck1v779xs0a9-source': \
             Input/output error";
        assert!(
            is_input_materialization_failure(Nix::MiscFailure, stat_eio, &closure),
            "I-189: 'getting status of' phrasing must reclassify"
        );
        for marker in ["reading directory '", "opening file '"] {
            let msg = format!("{marker}{input}': Input/output error");
            assert!(
                is_input_materialization_failure(Nix::MiscFailure, &msg, &closure),
                "marker {marker:?} must reclassify"
            );
        }

        // MiscFailure + path NOT in closure → false (genuine missing
        // dep — leave as PermanentFailure).
        let foreign = "getting attributes of path \
                       '/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-ghost': \
                       No such file or directory";
        assert!(
            !is_input_materialization_failure(Nix::MiscFailure, foreign, &closure),
            "ENOENT on non-closure path must NOT reclassify"
        );

        // Non-MiscFailure status + matching path → false (status guard).
        assert!(
            !is_input_materialization_failure(Nix::PermanentFailure, &enoent, &closure),
            "status guard: only MiscFailure is reclassified"
        );

        // MiscFailure + unrelated message → false.
        assert!(
            !is_input_materialization_failure(
                Nix::MiscFailure,
                "builder for '/nix/store/...-foo.drv' failed with exit code 1",
                &closure,
            ),
            "unrelated MiscFailure message must NOT reclassify"
        );

        // Empty closure → false (vacuous membership).
        assert!(!is_input_materialization_failure(
            Nix::MiscFailure,
            &enoent,
            &[]
        ));
    }
}

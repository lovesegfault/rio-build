//! Native-executor result processing: everything nix-daemon does between
//! "the builder process exited" and "here is a BuildResult", reimplemented
//! for the native executor.
//!
//! Two halves:
//!
//! * [`classify_exit`] — turn a raw [`rio_exec::ExitOutcome`] into the
//!   scheduler-facing [`BuildResultStatus`], with the retry-relevant
//!   distinctions (network-dependent fetch failures are transient, OOM
//!   and disk-full are infrastructure, timeouts stay timeouts) that the
//!   daemon's wire status used to carry.
//! * [`process_outputs`] — the per-output pipeline on success:
//!   ownership/permission checks, canonicalisation, reference scanning
//!   (one NAR pass that also yields the SHA-256 NAR hash and size),
//!   inter-output cycle detection + topological ordering, and the output
//!   policy checks (`allowedReferences` family, `outputChecks`,
//!   `unsafeDiscardReferences`, the FOD no-references rule).
//!
//! Floating-CA finalization (scratch→final path rewriting) lives in
//! [`ca`] and runs between the topological ordering and the policy
//! checks, so the policy checks (and the upload metadata) always see
//! the final, content-derived store paths and reference sets.
//!
//! Both halves run in the live build path: the executor's native
//! lifecycle calls [`classify_exit`] on every finished execution and
//! [`process_outputs`] (via `spawn_blocking` — it does blocking I/O and
//! hashing) on success, before the upload step streams the canonicalised
//! outputs to the store.

pub(crate) mod ca;
pub(crate) mod canonicalise;
pub(crate) mod policy;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::Digest;
use tracing::{debug, warn};

use rio_exec::ExitOutcome;
use rio_nix::derivation::{Derivation, DerivationLike};
use rio_nix::refscan::{CandidateSet, RefScanSink};
use rio_proto::types::BuildResultStatus;
use rio_proto::validated::ValidatedPathInfo;

use canonicalise::{CanonicaliseError, canonicalise_output};
use policy::{OutputForPolicy, OutputPolicy, PolicyViolation};

/// Free-space floor below which a non-zero exit is attributed to the
/// worker's disk rather than the build itself (CppNix's
/// `decideWhetherDiskFull` uses the same 8 MiB threshold).
const DISK_FULL_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Exit classification
// ---------------------------------------------------------------------------

/// What [`classify_exit`] decided about a finished execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExitClassification {
    /// The builder exited 0 — proceed to output processing; the final
    /// `Built` status is only earned once the outputs pass.
    Success,
    /// The build failed; report this status and message.
    Failed {
        status: BuildResultStatus,
        error_msg: String,
    },
}

/// Classify a raw exit outcome into the scheduler-facing status.
///
/// `is_network_dependent` is true for fixed-output (and impure)
/// derivations: their failures are dominated by the network, which is
/// not the derivation's fault — the scheduler retries those on other
/// workers before poisoning (`TransientFailure`).
///
/// `disk_full` and `oom_killed` are probes the caller supplies
/// ([`disk_full_probe`] and the per-build cgroup's `memory.events`
/// `oom_kill` delta respectively) so this function stays pure and
/// testable.
///
/// Precedence (most-specific attribution wins):
/// timeout/silence/log-limit (inherent in the variant) → OOM → disk-full
/// → network-dependent-transient → permanent. OOM and disk-full beat the
/// network-transient rule deliberately: classifying an OOM-killed fetch
/// as "transient" would retry it at the same memory floor and fail the
/// same way, while `InfrastructureFailure` makes the scheduler resize.
pub(crate) fn classify_exit(
    exit: ExitOutcome,
    is_network_dependent: bool,
    disk_full: bool,
    oom_killed: bool,
) -> ExitClassification {
    use ExitClassification::Failed;

    match exit {
        ExitOutcome::Exited(0) => ExitClassification::Success,
        ExitOutcome::TimedOut => Failed {
            status: BuildResultStatus::TimedOut,
            error_msg: "build exceeded its wall-clock timeout".into(),
        },
        ExitOutcome::Silent => Failed {
            status: BuildResultStatus::TimedOut,
            error_msg: "build produced no output for longer than max-silent-time".into(),
        },
        ExitOutcome::LogLimitExceeded => Failed {
            status: BuildResultStatus::LogLimitExceeded,
            error_msg: "build exceeded its log size limit".into(),
        },
        ExitOutcome::Signaled(sig) if sig == libc::SIGKILL && oom_killed => Failed {
            status: BuildResultStatus::InfrastructureFailure,
            error_msg: "build process was OOM-killed (cgroup memory.events oom_kill); \
                        the scheduler will retry with a larger memory floor"
                .into(),
        },
        ExitOutcome::Exited(code) if disk_full => Failed {
            status: BuildResultStatus::InfrastructureFailure,
            error_msg: format!(
                "builder exited with code {code} and the worker's scratch space is nearly \
                 full (<8 MiB free) — attributing the failure to the worker, not the build"
            ),
        },
        // A signal-killed build on a full disk gets the same attribution
        // (CppNix's `decideWhetherDiskFull` looks only at the disk, not
        // at how the builder died — ENOSPC often surfaces as a crash).
        ExitOutcome::Signaled(sig) if disk_full => Failed {
            status: BuildResultStatus::InfrastructureFailure,
            error_msg: format!(
                "builder was killed by signal {sig} and the worker's scratch space is nearly \
                 full (<8 MiB free) — attributing the failure to the worker, not the build"
            ),
        },
        ExitOutcome::Exited(code) if is_network_dependent => Failed {
            status: BuildResultStatus::TransientFailure,
            error_msg: format!(
                "fixed-output builder exited with code {code}; network fetch failures are \
                 retried on a different worker before the derivation is poisoned"
            ),
        },
        ExitOutcome::Signaled(sig) if is_network_dependent => Failed {
            status: BuildResultStatus::TransientFailure,
            error_msg: format!(
                "fixed-output builder was killed by signal {sig}; retried as transient"
            ),
        },
        ExitOutcome::Exited(code) => Failed {
            status: BuildResultStatus::PermanentFailure,
            error_msg: format!("builder exited with code {code}"),
        },
        ExitOutcome::Signaled(sig) => Failed {
            status: BuildResultStatus::PermanentFailure,
            error_msg: format!("builder was killed by signal {sig}"),
        },
    }
}

/// Probe whether any of `paths` sits on a filesystem with less than
/// [`DISK_FULL_THRESHOLD_BYTES`] of free space. Probe errors count as
/// "not full" — a failed statvfs must not turn a genuine build failure
/// into an infrastructure retry loop.
pub(crate) fn disk_full_probe(paths: &[&Path]) -> bool {
    paths.iter().any(|p| match nix::sys::statvfs::statvfs(*p) {
        Ok(vfs) => {
            let free = vfs.blocks_available() * vfs.fragment_size();
            free < DISK_FULL_THRESHOLD_BYTES
        }
        Err(_) => false,
    })
}

// ---------------------------------------------------------------------------
// Output processing
// ---------------------------------------------------------------------------

/// One output to process: where the derivation says it should be and
/// where it actually is on the worker's disk (the overlay upper store).
#[derive(Debug, Clone)]
pub(crate) struct OutputToProcess {
    /// Output name (`out`, `dev`, …).
    pub(crate) name: String,
    /// The store path (`/nix/store/<hash>-<name>`). For floating-CA
    /// outputs this is the *scratch* path until CA finalization exists.
    pub(crate) store_path: String,
    /// Host-side location of the produced output.
    pub(crate) host_path: PathBuf,
}

/// One output after the pipeline: canonicalised on disk, scanned, hashed.
#[derive(Debug, Clone)]
pub(crate) struct ProcessedOutput {
    pub(crate) name: String,
    /// Final store path. For floating-CA outputs this is the realized
    /// (content-derived) path after [`ca`] finalization — the value the
    /// `built_outputs` map and the realisations table report.
    pub(crate) store_path: String,
    pub(crate) host_path: PathBuf,
    /// SHA-256 of the NAR serialization (the store's content hash).
    pub(crate) nar_hash: [u8; 32],
    /// Size of the NAR serialization in bytes.
    pub(crate) nar_size: u64,
    /// Sorted full-store-path references (post `unsafeDiscardReferences`).
    pub(crate) references: Vec<String>,
    /// Nix content-address descriptor (`fixed:r:sha256:…`) for
    /// floating-CA outputs; `None` for input-addressed and fixed-output
    /// outputs. Destined for the uploaded `PathInfo.content_address` /
    /// narinfo `CA:` field so substituting clients can verify the path.
    pub(crate) content_address: Option<String>,
}

/// All outputs of a successful build, in topological order (an output
/// that references a sibling comes *after* that sibling). This is the
/// order CA finalization must walk, and a convenient order for upload.
#[derive(Debug)]
pub(crate) struct ProcessedOutputs {
    pub(crate) outputs: Vec<ProcessedOutput>,
}

/// Why the outputs of a successful build were rejected.
///
/// Variants map to `BuildResultStatus::OutputRejected` — the build
/// *ran*, but what it produced is not acceptable — except
/// [`FodHasReferences`](Self::FodHasReferences), which reports as
/// `OutputRejected` like the rest (the proto status has no separate
/// hash-mismatch variant; it is still a content-integrity failure of a
/// fixed-output derivation, not a policy rejection). The distinction
/// the variants carry is for tests and precise tenant-facing messages.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OutputRejection {
    #[error("{0}")]
    Canonicalise(#[from] CanonicaliseError),
    #[error("{0}")]
    Policy(#[from] PolicyViolation),
    #[error("reference scan of output '{output}' failed: {message}")]
    Scan { output: String, message: String },
    #[error(
        "outputs reference each other in a cycle ({involving:?}); cyclic outputs cannot be \
         registered"
    )]
    Cycle { involving: Vec<String> },
    #[error(
        "fixed-output derivation output '{output}' references store paths {references:?}; \
         a fixed-output result must be reproducible from its declared hash alone and may \
         not reference the store"
    )]
    FodHasReferences {
        output: String,
        references: Vec<String>,
    },
    #[error(
        "floating content-addressed output '{output}' declares unsupported outputHashAlgo \
         '{algo}' (supported: sha1, sha256, sha512, optionally 'r:'-prefixed)"
    )]
    CaUnsupportedAlgo { output: String, algo: String },
    #[error(
        "floating content-addressed output '{output}' uses flat ingestion but is not a \
         single non-executable regular file"
    )]
    CaFlatNotSingleFile { output: String },
    #[error("finalizing floating content-addressed output '{output}': {message}")]
    CaFinalize { output: String, message: String },
}

/// Run the full output pipeline for a successful build.
///
/// `outputs` come from the executor's per-output reports (declared store
/// path + host path); `build_uid` is the uid the sandboxed build ran as;
/// `input_closure` is the resolved transitive input closure (the same
/// metadata the FUSE store and the upload candidate set are built from).
///
/// Blocking filesystem I/O and hashing — call via `spawn_blocking`.
///
/// On success the returned outputs are canonicalised on disk and carry
/// their NAR hash, NAR size, and reference set, in topological order —
/// exactly what the upload path needs (`PathInfo.references`,
/// `BuiltOutput.output_hash`) without re-scanning.
pub(crate) fn process_outputs(
    drv: &Derivation,
    outputs: &[OutputToProcess],
    build_uid: u32,
    input_closure: &[ValidatedPathInfo],
) -> Result<ProcessedOutputs, OutputRejection> {
    let result = process_outputs_inner(drv, outputs, build_uid, input_closure);
    if let Err(rejection) = &result {
        warn!(
            output = %outputs.first().map(|o| o.store_path.as_str()).unwrap_or("<none>"),
            error = %rejection,
            "build outputs rejected"
        );
    }
    result
}

fn process_outputs_inner(
    drv: &Derivation,
    outputs: &[OutputToProcess],
    build_uid: u32,
    input_closure: &[ValidatedPathInfo],
) -> Result<ProcessedOutputs, OutputRejection> {
    let policy = OutputPolicy::parse(drv.env());
    let ca_spec = ca::FloatingCaSpec::from_outputs(drv.outputs())?;
    let candidates = reference_candidates(outputs, input_closure);

    // Pass 1: canonicalise + scan each output (one NAR read per output).
    let processed = canonicalise_and_scan_outputs(outputs, build_uid, &candidates, &policy)?;

    // The FOD no-references rule applies before any reordering: a
    // fixed-output derivation's output must not reference the store at
    // all (it must be reproducible from its declared hash alone).
    if drv.is_fixed_output() {
        enforce_fod_has_no_references(&processed)?;
    }

    // Pass 2: inter-output cycle detection + topological order
    // (dependencies first), then floating-CA finalization in that order —
    // apply accumulated sibling rewrites, hash-modulo, compute the final
    // store path, rewrite content on disk, and update
    // `store_path`/`references`/`content_address` before the policy
    // checks below see them.
    let order = topo_order(&processed)?;
    let mut processed: Vec<ProcessedOutput> =
        order.into_iter().map(|i| processed[i].clone()).collect();
    ca::finalize_floating_ca(&mut processed, &ca_spec)?;
    let processed = processed;

    // Pass 3: output policy checks (both sources) against the final,
    // content-derived paths and reference sets.
    check_output_policies(&processed, &policy, input_closure)?;

    Ok(ProcessedOutputs { outputs: processed })
}

/// Candidate set for the reference scan: the transitive input closure
/// plus every output of this derivation (self- and cross-output
/// references are legal and must be detected).
fn reference_candidates(
    outputs: &[OutputToProcess],
    input_closure: &[ValidatedPathInfo],
) -> CandidateSet {
    let mut candidate_paths: Vec<String> = input_closure
        .iter()
        .map(|p| p.store_path.to_string())
        .collect();
    candidate_paths.extend(outputs.iter().map(|o| o.store_path.clone()));
    CandidateSet::from_paths(&candidate_paths)
}

/// Pass 1 of the output pipeline: canonicalise each output on disk
/// (ownership, permissions, timestamps, setuid stripping — see
/// [`canonicalise_output`]) and scan it in a single NAR pass for its
/// SHA-256 NAR hash, NAR size, and references against `candidates`.
///
/// `unsafeDiscardReferences` is applied here so everything downstream
/// (cycle detection, CA finalization, policy checks, upload metadata)
/// sees the recorded — not the raw scanned — reference set, exactly as
/// Nix registers it.
fn canonicalise_and_scan_outputs(
    outputs: &[OutputToProcess],
    build_uid: u32,
    candidates: &CandidateSet,
    policy: &OutputPolicy,
) -> Result<Vec<ProcessedOutput>, OutputRejection> {
    // Hard-link dedup state is shared across the whole output set: a
    // file hard-linked between two outputs must only be rewritten once.
    let mut inodes_seen: HashSet<canonicalise::InodeId> = HashSet::new();
    let mut processed: Vec<ProcessedOutput> = Vec::with_capacity(outputs.len());
    for out in outputs {
        canonicalise_output(&out.host_path, build_uid, &mut inodes_seen)?;

        let (nar_hash, nar_size, mut references) = scan_and_hash(&out.host_path, candidates)
            .map_err(|e| OutputRejection::Scan {
                output: out.name.clone(),
                message: e.to_string(),
            })?;

        // unsafeDiscardReferences: record an empty set no matter what
        // the scan found.
        if policy.discard_references_for(&out.name) {
            references.clear();
        }

        // Self-references are legal and are recorded in the reference
        // set, exactly as Nix registers them.
        debug!(
            output = %out.name,
            store_path = %out.store_path,
            nar_size,
            references = references.len(),
            "processed build output"
        );
        processed.push(ProcessedOutput {
            name: out.name.clone(),
            store_path: out.store_path.clone(),
            host_path: out.host_path.clone(),
            nar_hash,
            nar_size,
            references,
            content_address: None,
        });
    }
    Ok(processed)
}

/// The FOD no-references rule: a fixed-output derivation's output must
/// not reference the store at all. Self-references are equally
/// disallowed.
fn enforce_fod_has_no_references(processed: &[ProcessedOutput]) -> Result<(), OutputRejection> {
    for out in processed {
        if !out.references.is_empty() {
            return Err(OutputRejection::FodHasReferences {
                output: out.name.clone(),
                references: out.references.clone(),
            });
        }
    }
    Ok(())
}

/// Pass 3 of the output pipeline: the output policy checks
/// (`allowedReferences` family and structuredAttrs `outputChecks`),
/// evaluated against the final store paths and reference sets with the
/// input closure's metadata available for closure-size rules.
fn check_output_policies(
    processed: &[ProcessedOutput],
    policy: &OutputPolicy,
    input_closure: &[ValidatedPathInfo],
) -> Result<(), OutputRejection> {
    let closure_info: HashMap<String, (Vec<String>, u64)> = input_closure
        .iter()
        .map(|p| {
            (
                p.store_path.to_string(),
                (
                    p.references.iter().map(|r| r.to_string()).collect(),
                    p.nar_size,
                ),
            )
        })
        .collect();
    let for_policy: Vec<OutputForPolicy> = processed
        .iter()
        .map(|o| OutputForPolicy {
            name: o.name.clone(),
            store_path: o.store_path.clone(),
            references: o.references.clone(),
            nar_size: o.nar_size,
        })
        .collect();
    policy::check_outputs(&for_policy, policy, &closure_info)?;
    Ok(())
}

/// One streaming NAR pass over `path`: SHA-256 NAR hash, NAR size, and
/// the resolved (sorted) reference set against `candidates`.
fn scan_and_hash(
    path: &Path,
    candidates: &CandidateSet,
) -> std::io::Result<([u8; 32], u64, Vec<String>)> {
    /// Tee every NAR byte into both the reference scanner and a SHA-256.
    struct ScanHashSink {
        refs: RefScanSink,
        hasher: sha2::Sha256,
    }
    impl Write for ScanHashSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.refs.write_all(buf)?;
            self.hasher.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut sink = ScanHashSink {
        refs: RefScanSink::new(candidates.hashes()),
        hasher: sha2::Sha256::new(),
    };
    let nar_size = rio_nix::nar::dump_path_streaming(path, &mut sink)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let references = candidates.resolve(&sink.refs.into_found());
    let nar_hash: [u8; 32] = sink.hasher.finalize().into();
    Ok((nar_hash, nar_size, references))
}

/// Topological order of `outputs` by references-to-sibling-outputs
/// (dependencies first). A cycle is an [`OutputRejection::Cycle`].
fn topo_order(outputs: &[ProcessedOutput]) -> Result<Vec<usize>, OutputRejection> {
    let path_to_idx: HashMap<&str, usize> = outputs
        .iter()
        .enumerate()
        .map(|(i, o)| (o.store_path.as_str(), i))
        .collect();

    // For each output i, count how many siblings it references
    // (in_degree) and record the reverse edges (dependents) — all
    // Kahn's algorithm needs.
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); outputs.len()];
    let mut in_degree: Vec<usize> = vec![0; outputs.len()];
    for (i, out) in outputs.iter().enumerate() {
        for r in &out.references {
            if let Some(&j) = path_to_idx.get(r.as_str())
                && i != j
            {
                dependents[j].push(i);
                in_degree[i] += 1;
            }
        }
    }

    // Kahn's algorithm with deterministic tie-breaking (declaration
    // order) so the result is stable run-to-run.
    let mut ready: Vec<usize> = (0..outputs.len()).filter(|&i| in_degree[i] == 0).collect();
    let mut order = Vec::with_capacity(outputs.len());
    while let Some(&next) = ready.iter().min() {
        ready.retain(|&i| i != next);
        order.push(next);
        for &dep in &dependents[next] {
            in_degree[dep] -= 1;
            if in_degree[dep] == 0 {
                ready.push(dep);
            }
        }
    }
    if order.len() != outputs.len() {
        let involving: Vec<String> = (0..outputs.len())
            .filter(|i| !order.contains(i))
            .map(|i| outputs[i].name.clone())
            .collect();
        return Err(OutputRejection::Cycle { involving });
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn my_uid() -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    // -- classification ----------------------------------------------------

    #[test]
    fn classify_success() {
        assert_eq!(
            classify_exit(ExitOutcome::Exited(0), false, false, false),
            ExitClassification::Success
        );
    }

    #[test]
    fn classify_table() {
        use BuildResultStatus as S;
        let failed = |c: ExitClassification| match c {
            ExitClassification::Failed { status, .. } => status,
            ExitClassification::Success => panic!("expected failure"),
        };

        // Plain non-zero exit, sandboxed build → permanent.
        assert_eq!(
            failed(classify_exit(ExitOutcome::Exited(1), false, false, false)),
            S::PermanentFailure
        );
        // Same exit on a fixed-output (network) build → transient.
        assert_eq!(
            failed(classify_exit(ExitOutcome::Exited(1), true, false, false)),
            S::TransientFailure
        );
        // Signal on a FOD → transient.
        assert_eq!(
            failed(classify_exit(ExitOutcome::Signaled(15), true, false, false)),
            S::TransientFailure
        );
        // Disk full beats the FOD-transient rule.
        assert_eq!(
            failed(classify_exit(ExitOutcome::Exited(1), true, true, false)),
            S::InfrastructureFailure
        );
        // OOM (SIGKILL + cgroup counter) beats the FOD-transient rule.
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::Signaled(libc::SIGKILL),
                true,
                false,
                true
            )),
            S::InfrastructureFailure
        );
        // SIGKILL without the OOM counter on a non-FOD → permanent.
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::Signaled(libc::SIGKILL),
                false,
                false,
                false
            )),
            S::PermanentFailure
        );
        // Timeout / silence / log-limit are passed through.
        assert_eq!(
            failed(classify_exit(ExitOutcome::TimedOut, false, false, false)),
            S::TimedOut
        );
        assert_eq!(
            failed(classify_exit(ExitOutcome::Silent, true, true, true)),
            S::TimedOut
        );
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::LogLimitExceeded,
                false,
                false,
                false
            )),
            S::LogLimitExceeded
        );
    }

    #[test]
    fn disk_probe_handles_missing_path() {
        // A nonexistent path must not panic and must not report "full".
        assert!(!disk_full_probe(&[Path::new("/nonexistent/rio/probe")]));
        // The tempdir is on a real filesystem with space.
        let tmp = tempfile::tempdir().unwrap();
        assert!(!disk_full_probe(&[tmp.path()]));
    }

    // -- output pipeline ----------------------------------------------------

    /// 32-char fake nixbase32 hash parts (valid alphabet) for store paths.
    fn sp(c: char, name: &str) -> String {
        format!("/nix/store/{}-{name}", String::from(c).repeat(32))
    }

    /// Minimal input-closure metadata for a store path (everything the
    /// pipeline reads: the path itself, empty references, size 1).
    fn input_info(path: &str) -> ValidatedPathInfo {
        ValidatedPathInfo {
            store_path: rio_nix::store_path::StorePath::parse(path).unwrap(),
            store_path_hash: vec![],
            deriver: None,
            nar_hash: [0u8; 32],
            nar_size: 1,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        }
    }

    fn drv_from_aterm(outputs: &[(&str, &str)], env: &[(&str, &str)]) -> Derivation {
        let outs = outputs
            .iter()
            .map(|(n, p)| format!(r#"("{n}","{p}","","")"#))
            .collect::<Vec<_>>()
            .join(",");
        let envs = env
            .iter()
            .map(|(k, v)| {
                format!(
                    r#"("{k}","{}")"#,
                    v.replace('\\', r"\\").replace('"', r#"\""#)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let aterm = format!(r#"Derive([{outs}],[],[],"x86_64-linux","/bin/sh",[],[{envs}])"#);
        Derivation::parse(&aterm).expect("test ATerm parses")
    }

    /// Set up a fake overlay-upper store with one or more outputs
    /// containing the given file contents.
    fn fake_outputs(specs: &[(&str, &str, &[u8])]) -> (tempfile::TempDir, Vec<OutputToProcess>) {
        let tmp = tempfile::tempdir().unwrap();
        let mut outs = Vec::new();
        for (name, store_path, content) in specs {
            let basename = store_path.rsplit('/').next().unwrap();
            let host = tmp.path().join(basename);
            std::fs::create_dir_all(&host).unwrap();
            std::fs::write(host.join("payload"), content).unwrap();
            std::fs::set_permissions(host.join("payload"), std::fs::Permissions::from_mode(0o644))
                .unwrap();
            outs.push(OutputToProcess {
                name: (*name).to_string(),
                store_path: (*store_path).to_string(),
                host_path: host,
            });
        }
        (tmp, outs)
    }

    #[test]
    fn happy_path_multi_output_with_cross_reference() {
        let out_p = sp('a', "thing");
        let dev_p = sp('b', "thing-dev");
        // dev's payload embeds out's store path → dev references out →
        // topo order must put out first even though dev is declared first.
        let (_tmp, outputs) = fake_outputs(&[
            ("dev", &dev_p, format!("see also {out_p}").as_bytes()),
            ("out", &out_p, b"standalone"),
        ]);
        let drv = drv_from_aterm(&[("dev", &dev_p), ("out", &out_p)], &[]);

        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();
        assert_eq!(processed.outputs.len(), 2);
        assert_eq!(processed.outputs[0].name, "out", "dependency first");
        assert_eq!(processed.outputs[1].name, "dev");
        assert_eq!(
            processed.outputs[1].references,
            vec![out_p.clone()],
            "dev references out"
        );
        assert!(processed.outputs[0].references.is_empty());
        assert!(processed.outputs.iter().all(|o| o.nar_size > 0));
        assert!(processed.outputs.iter().all(|o| o.nar_hash != [0u8; 32]));
        // Canonicalisation actually happened on disk.
        let meta = std::fs::metadata(processed.outputs[0].host_path.join("payload")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o444);
    }

    #[test]
    fn missing_output_rejected() {
        let out_p = sp('a', "thing");
        let drv = drv_from_aterm(&[("out", &out_p)], &[]);
        let outputs = [OutputToProcess {
            name: "out".into(),
            store_path: out_p,
            host_path: PathBuf::from("/nonexistent/rio-test-output"),
        }];
        let err = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap_err();
        assert!(matches!(
            err,
            OutputRejection::Canonicalise(CanonicaliseError::Missing { .. })
        ));
    }

    #[test]
    fn cycle_between_outputs_rejected() {
        let out_p = sp('a', "thing");
        let dev_p = sp('b', "thing-dev");
        let (_tmp, outputs) = fake_outputs(&[
            ("out", &out_p, format!("points at {dev_p}").as_bytes()),
            ("dev", &dev_p, format!("points at {out_p}").as_bytes()),
        ]);
        let drv = drv_from_aterm(&[("out", &out_p), ("dev", &dev_p)], &[]);
        let err = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap_err();
        assert!(matches!(err, OutputRejection::Cycle { .. }), "got {err}");
    }

    #[test]
    fn fod_with_references_rejected() {
        let out_p = sp('a', "fetched");
        let input = sp('c', "glibc");
        let (_tmp, outputs) =
            fake_outputs(&[("out", &out_p, format!("embeds {input}").as_bytes())]);
        // A FOD: single "out" output with hash_algo+hash set.
        let aterm = format!(
            r#"Derive([("out","{out_p}","sha256","{h}")],[],[],"x86_64-linux","/bin/sh",[],[("out","{out_p}")])"#,
            h = "00".repeat(32)
        );
        let drv = Derivation::parse(&aterm).unwrap();

        // The embedded path must be a scan candidate → supply it as input
        // closure metadata.
        let input_info = input_info(&input);
        let err = process_outputs(&drv, &outputs, my_uid(), &[input_info]).unwrap_err();
        assert!(
            matches!(err, OutputRejection::FodHasReferences { .. }),
            "got {err}"
        );
    }

    #[test]
    fn disallowed_requisites_rejected_via_policy() {
        let out_p = sp('a', "thing");
        let bad = sp('d', "bootstrap-tools");
        let (_tmp, outputs) = fake_outputs(&[("out", &out_p, format!("uses {bad}").as_bytes())]);
        let drv = drv_from_aterm(&[("out", &out_p)], &[("disallowedRequisites", &bad)]);
        let bad_info = input_info(&bad);
        let err = process_outputs(&drv, &outputs, my_uid(), &[bad_info]).unwrap_err();
        assert!(matches!(err, OutputRejection::Policy(_)), "got {err}");
    }

    #[test]
    fn structured_attrs_unsafe_discard_references() {
        let out_p = sp('a', "image");
        let input = sp('c', "glibc");
        let (_tmp, outputs) =
            fake_outputs(&[("out", &out_p, format!("embeds {input}").as_bytes())]);
        let json = serde_json::json!({ "unsafeDiscardReferences": { "out": true } }).to_string();
        let drv = drv_from_aterm(&[("out", &out_p)], &[("__json", &json)]);
        let input_info = input_info(&input);
        let processed = process_outputs(&drv, &outputs, my_uid(), &[input_info]).unwrap();
        assert!(
            processed.outputs[0].references.is_empty(),
            "references must be discarded"
        );
    }

    #[test]
    fn structured_attrs_output_checks_max_size() {
        let out_p = sp('a', "fat");
        let (_tmp, outputs) = fake_outputs(&[("out", &out_p, &[0u8; 4096])]);
        let json = serde_json::json!({ "outputChecks": { "out": { "maxSize": 16 } } }).to_string();
        let drv = drv_from_aterm(&[("out", &out_p)], &[("__json", &json)]);
        let err = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap_err();
        assert!(
            matches!(&err, OutputRejection::Policy(v) if v.rule == "maxSize"),
            "got {err}"
        );
    }

    // -- floating-CA finalization -------------------------------------------

    /// ATerm derivation with full per-output specs `(name, path, algo, hash)`
    /// — needed for floating-CA outputs (`("out","","r:sha256","")`).
    fn drv_from_aterm_ca(outputs: &[(&str, &str, &str, &str)], env: &[(&str, &str)]) -> Derivation {
        let outs = outputs
            .iter()
            .map(|(n, p, a, h)| format!(r#"("{n}","{p}","{a}","{h}")"#))
            .collect::<Vec<_>>()
            .join(",");
        let envs = env
            .iter()
            .map(|(k, v)| {
                format!(
                    r#"("{k}","{}")"#,
                    v.replace('\\', r"\\").replace('"', r#"\""#)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let aterm = format!(r#"Derive([{outs}],[],[],"x86_64-linux","/bin/sh",[],[{envs}])"#);
        Derivation::parse(&aterm).expect("test ATerm parses")
    }

    /// Hand-compute the final CA path of a *standalone* recursive
    /// SHA-256 output (no references, no self-reference) from its
    /// canonicalised on-disk content — what finalization must produce.
    fn expected_standalone_ca_path(host: &Path, name: &str) -> String {
        let mut nar = Vec::new();
        rio_nix::nar::dump_path_streaming(host, &mut nar).unwrap();
        let hash = rio_nix::hash::NixHash::compute(rio_nix::hash::HashAlgo::SHA256, &nar);
        rio_nix::store_path::StorePath::make_fixed_output_with_self(name, &hash, true, &[], false)
            .unwrap()
            .as_str()
            .to_owned()
    }

    #[test]
    fn floating_ca_sibling_reference_finalized() {
        // Two floating-CA outputs; "doc" embeds a reference to "out"'s
        // scratch path. Finalization must (1) finalize out, (2) rewrite
        // doc's content to out's *final* path before hashing doc.
        let out_scratch = sp('s', "demo");
        let doc_scratch = sp('w', "demo-doc");
        let (_tmp, mut outputs) = fake_outputs(&[
            (
                "doc",
                &doc_scratch,
                format!("see {out_scratch}/payload").as_bytes(),
            ),
            ("out", &out_scratch, b"standalone content"),
        ]);
        // OutputToProcess order is declaration order; topo order will
        // put "out" first.
        let drv = drv_from_aterm_ca(
            &[("doc", "", "r:sha256", ""), ("out", "", "r:sha256", "")],
            &[],
        );
        outputs.sort_by(|a, b| a.name.cmp(&b.name)); // deterministic input order

        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();
        let out = processed.outputs.iter().find(|o| o.name == "out").unwrap();
        let doc = processed.outputs.iter().find(|o| o.name == "doc").unwrap();

        // "out" is standalone: its final path is the hand-computable CA
        // path of its (unchanged) content.
        assert_ne!(
            out.store_path, out_scratch,
            "out must leave its scratch path"
        );
        assert_eq!(
            out.store_path,
            expected_standalone_ca_path(&out.host_path, "demo"),
            "standalone output's final path is the content-derived CA path"
        );
        assert!(out.host_path.ends_with(std::path::Path::new(
            out.store_path.strip_prefix("/nix/store/").unwrap()
        )));
        assert_eq!(
            out.content_address.as_deref().map(|s| &s[..14]),
            Some("fixed:r:sha256"),
        );

        // "doc" referenced the scratch path; after finalization its
        // reference set and its *content* must carry out's final path.
        assert_eq!(doc.references, vec![out.store_path.clone()]);
        let doc_payload = std::fs::read_to_string(doc.host_path.join("payload")).unwrap();
        assert!(
            doc_payload.contains(&out.store_path),
            "doc's content must be rewritten to out's final path: {doc_payload}"
        );
        assert!(
            !doc_payload.contains(&out_scratch),
            "no scratch path may survive in doc's content"
        );
        assert_ne!(doc.store_path, doc_scratch);
        // The scratch trees are gone from disk.
        assert!(
            !_tmp
                .path()
                .join(out_scratch.strip_prefix("/nix/store/").unwrap())
                .exists()
        );
        assert!(
            !_tmp
                .path()
                .join(doc_scratch.strip_prefix("/nix/store/").unwrap())
                .exists()
        );
    }

    #[test]
    fn floating_ca_self_reference_fixed_point() {
        let scratch = sp('s', "selfy");
        let (_tmp, outputs) =
            fake_outputs(&[("out", &scratch, format!("I live at {scratch}").as_bytes())]);
        let drv = drv_from_aterm_ca(&[("out", "", "r:sha256", "")], &[]);

        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();
        let out = &processed.outputs[0];
        assert_ne!(out.store_path, scratch);
        // The self-reference is recorded at the final path.
        assert_eq!(out.references, vec![out.store_path.clone()]);
        // Content has been rewritten to the final path.
        let payload = std::fs::read_to_string(out.host_path.join("payload")).unwrap();
        assert!(payload.contains(&out.store_path));
        assert!(!payload.contains(&scratch));

        // Fixed point: hashing the final on-disk content modulo the
        // *final* hash part and re-deriving the path must yield the same
        // path (this is exactly the check rio-store's upload gate runs).
        let final_path = rio_nix::store_path::StorePath::parse(&out.store_path).unwrap();
        let mut nar = Vec::new();
        rio_nix::nar::dump_path_streaming(&out.host_path, &mut nar).unwrap();
        let mut sink = rio_nix::ca::HashModuloSink::new(
            rio_nix::hash::HashAlgo::SHA256,
            &final_path.hash_part(),
        );
        sink.write_all(&nar).unwrap();
        let (modulo, n) = sink.finish();
        assert!(n > 0, "the self-reference must be present in the final NAR");
        let rederived = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            "selfy",
            &modulo,
            true,
            &[],
            true,
        )
        .unwrap();
        assert_eq!(
            rederived.as_str(),
            out.store_path,
            "self-reference fixed point"
        );
        // NAR hash/size describe the *final* content.
        let expect_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
        assert_eq!(out.nar_hash, expect_hash);
        assert_eq!(out.nar_size, nar.len() as u64);
    }

    #[test]
    fn floating_ca_flat_single_file_renamed() {
        let scratch = sp('s', "blob");
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp
            .path()
            .join(scratch.strip_prefix("/nix/store/").unwrap());
        std::fs::write(&host, b"flat bytes, no references").unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o644)).unwrap();
        let outputs = [OutputToProcess {
            name: "out".into(),
            store_path: scratch.clone(),
            host_path: host,
        }];
        let drv = drv_from_aterm_ca(&[("out", "", "sha256", "")], &[]);
        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();
        let out = &processed.outputs[0];
        assert_ne!(out.store_path, scratch);
        assert_eq!(
            out.content_address.as_deref().map(|s| &s[..12]),
            Some("fixed:sha256")
        );
        assert!(out.host_path.exists());
        assert!(out.references.is_empty());
    }

    #[test]
    fn floating_ca_flat_rejects_directory() {
        let scratch = sp('s', "blob");
        let (_tmp, outputs) = fake_outputs(&[("out", &scratch, b"inside a dir")]);
        let drv = drv_from_aterm_ca(&[("out", "", "sha256", "")], &[]);
        let err = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap_err();
        assert!(
            matches!(err, OutputRejection::CaFlatNotSingleFile { .. }),
            "got {err}"
        );
    }

    #[test]
    fn floating_ca_unsupported_algo_rejected() {
        let scratch = sp('s', "old");
        let (_tmp, outputs) = fake_outputs(&[("out", &scratch, b"x")]);
        let drv = drv_from_aterm_ca(&[("out", "", "md5", "")], &[]);
        let err = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap_err();
        assert!(
            matches!(err, OutputRejection::CaUnsupportedAlgo { ref algo, .. } if algo == "md5"),
            "got {err}"
        );
    }

    #[test]
    fn mixed_ca_and_input_addressed_outputs() {
        // "lib" is input-addressed (declared path); "out" is floating-CA.
        // lib references out → after finalization lib's reference must
        // point at out's final path, and lib's own path must be
        // untouched.
        let lib_p = sp('a', "demo-lib");
        let out_scratch = sp('s', "demo");
        let (_tmp, outputs) = fake_outputs(&[
            ("lib", &lib_p, format!("needs {out_scratch}").as_bytes()),
            ("out", &out_scratch, b"ca content"),
        ]);
        let drv = drv_from_aterm_ca(&[("lib", &lib_p, "", ""), ("out", "", "r:sha256", "")], &[]);
        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();
        let lib = processed.outputs.iter().find(|o| o.name == "lib").unwrap();
        let out = processed.outputs.iter().find(|o| o.name == "out").unwrap();
        assert_eq!(lib.store_path, lib_p, "input-addressed path untouched");
        assert!(lib.content_address.is_none());
        assert_ne!(out.store_path, out_scratch);
        assert_eq!(
            lib.references,
            vec![out.store_path.clone()],
            "IA output's reference to the CA sibling is remapped to the final path"
        );
    }

    #[test]
    fn policy_checks_see_final_ca_paths() {
        // "doc" references its CA sibling "out" but declares an empty
        // allowedReferences list → the policy violation must name out's
        // *final* path, proving the checks run after finalization.
        let out_scratch = sp('s', "demo");
        let doc_scratch = sp('w', "demo-doc");
        let (_tmp, outputs) = fake_outputs(&[
            ("doc", &doc_scratch, format!("see {out_scratch}").as_bytes()),
            ("out", &out_scratch, b"standalone"),
        ]);
        let json = serde_json::json!({
            "outputChecks": { "doc": { "allowedReferences": [] } }
        })
        .to_string();
        let drv = drv_from_aterm_ca(
            &[("doc", "", "r:sha256", ""), ("out", "", "r:sha256", "")],
            &[("__json", &json)],
        );
        let err = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap_err();
        let OutputRejection::Policy(violation) = &err else {
            panic!("expected a policy violation, got {err}");
        };
        assert!(
            !violation.to_string().contains(&out_scratch),
            "the violation must NOT name the scratch path: {violation}"
        );
        assert!(
            violation.to_string().contains("/nix/store/"),
            "the violation names the offending (final) path: {violation}"
        );
    }
}

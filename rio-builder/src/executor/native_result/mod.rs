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
//! Floating-CA finalization (scratch→final path rewriting) is **not**
//! here yet: it slots in between the topological ordering and the policy
//! checks (see the marked seam in [`process_outputs`]) and is added by
//! the CA-finalization milestone (M6b). Until then floating-CA outputs
//! pass through under their scratch paths.
//!
//! Nothing in this module is wired into the live build path yet — the
//! activation milestone (M7) replaces the daemon lifecycle with
//! `rio_exec::execute` + this module.

#![allow(dead_code)] // removed at activation (M7)

pub(crate) mod canonicalise;
pub(crate) mod policy;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::Digest;

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
    pub(crate) store_path: String,
    pub(crate) host_path: PathBuf,
    /// SHA-256 of the NAR serialization (the store's content hash).
    pub(crate) nar_hash: [u8; 32],
    /// Size of the NAR serialization in bytes.
    pub(crate) nar_size: u64,
    /// Sorted full-store-path references (post `unsafeDiscardReferences`).
    pub(crate) references: Vec<String>,
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
/// All variants map to `BuildResultStatus::OutputRejected` — the build
/// *ran*, but what it produced is not acceptable. The distinction the
/// variants carry is for tests and for precise tenant-facing messages.
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
    let policy = OutputPolicy::parse(drv.env());
    let is_fod = drv.is_fixed_output();

    // Candidate set for the reference scan: the transitive input closure
    // plus every output of this derivation (self- and cross-output
    // references are legal and must be detected).
    let mut candidate_paths: Vec<String> = input_closure
        .iter()
        .map(|p| p.store_path.to_string())
        .collect();
    candidate_paths.extend(outputs.iter().map(|o| o.store_path.clone()));
    let candidates = CandidateSet::from_paths(&candidate_paths);

    // Pass 1: canonicalise + scan each output (one NAR read per output).
    let mut inodes_seen: HashSet<canonicalise::InodeId> = HashSet::new();
    let mut processed: Vec<ProcessedOutput> = Vec::with_capacity(outputs.len());
    for out in outputs {
        canonicalise_output(&out.host_path, build_uid, &mut inodes_seen)?;

        let (nar_hash, nar_size, mut references) = scan_and_hash(&out.host_path, &candidates)
            .map_err(|e| OutputRejection::Scan {
                output: out.name.clone(),
                message: e.to_string(),
            })?;

        // unsafeDiscardReferences: record an empty set no matter what
        // the scan found.
        if policy.discard_references_for(&out.name) {
            references.clear();
        }

        // A self-reference is legal but is not recorded in PathInfo
        // references? — it is: Nix records self-references. Keep them.
        processed.push(ProcessedOutput {
            name: out.name.clone(),
            store_path: out.store_path.clone(),
            host_path: out.host_path.clone(),
            nar_hash,
            nar_size,
            references,
        });
    }

    // The FOD no-references rule: a fixed-output derivation's output must
    // not reference the store at all (it must be reproducible from its
    // declared hash alone). Self-references are equally disallowed.
    if is_fod {
        for out in &processed {
            let refs_excluding_nothing: Vec<String> = out.references.clone();
            if !refs_excluding_nothing.is_empty() {
                return Err(OutputRejection::FodHasReferences {
                    output: out.name.clone(),
                    references: refs_excluding_nothing,
                });
            }
        }
    }

    // Pass 2: inter-output cycle detection + topological order
    // (dependencies first). Kahn's algorithm over the sibling-reference
    // edges.
    let order = topo_order(&processed)?;
    let processed: Vec<ProcessedOutput> = order.into_iter().map(|i| processed[i].clone()).collect();

    // --- CA finalization seam (M6b) ---------------------------------
    // Floating-CA outputs are finalized here, in this topological order:
    // apply accumulated sibling rewrites, hash-modulo, compute the final
    // store path, rewrite content, and update `store_path`/`references`
    // before the policy checks below see them.
    // -----------------------------------------------------------------

    // Pass 3: output policy checks (both sources).
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
    policy::check_outputs(&for_policy, &policy, &closure_info)?;

    Ok(ProcessedOutputs { outputs: processed })
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

    // edges[i] = set of sibling indexes that output i references
    // (i depends on them, so they must come first).
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); outputs.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); outputs.len()];
    let mut in_degree: Vec<usize> = vec![0; outputs.len()];
    for (i, out) in outputs.iter().enumerate() {
        for r in &out.references {
            if let Some(&j) = path_to_idx.get(r.as_str())
                && i != j
            {
                deps[i].push(j);
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
        let input_info = ValidatedPathInfo {
            store_path: rio_nix::store_path::StorePath::parse(&input).unwrap(),
            store_path_hash: vec![],
            deriver: None,
            nar_hash: [0u8; 32],
            nar_size: 1,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        };
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
        let bad_info = ValidatedPathInfo {
            store_path: rio_nix::store_path::StorePath::parse(&bad).unwrap(),
            store_path_hash: vec![],
            deriver: None,
            nar_hash: [0u8; 32],
            nar_size: 1,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        };
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
        let input_info = ValidatedPathInfo {
            store_path: rio_nix::store_path::StorePath::parse(&input).unwrap(),
            store_path_hash: vec![],
            deriver: None,
            nar_hash: [0u8; 32],
            nar_size: 1,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        };
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
}

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
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::refscan::{CandidateSet, RefScanSink};
use rio_proto::types::BuildResultStatus;
use rio_proto::validated::ValidatedPathInfo;

use canonicalise::{CanonicaliseError, canonicalise_output};
use policy::{OutputForPolicy, OutputPolicy, PolicyParseError, PolicyViolation};

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

/// Is this finished execution the canary signature for kill-verdict
/// misattribution: a limit-kill verdict (`TimedOut`/`Silent`/
/// `LogLimitExceeded`) whose declared outputs nevertheless ALL
/// materialized?
///
/// A correctly attributed limit kill interrupts the build before it
/// completes, so its outputs are missing (or partial). A kill verdict
/// with a full output set means one of exactly two things, both worth
/// an operator's eyes: the natural-137 coincidence (the build's last
/// act was to exit 137 on its own while a deadline raced it — residual
/// 1 of the executor's corroboration contract) or a supervision
/// regression re-opening the relabel-a-completed-build window the
/// principal-targeted kill closed (merged_bug_046). The counter this
/// feeds (`rio_builder_kill_verdict_outputs_present_total`) is
/// expected to stay at 0.
///
/// `outputs` empty is NOT canary territory: nothing materialized, the
/// verdict is self-consistent.
// r[impl builder.exec.kill-targets-principal]
pub(crate) fn kill_verdict_with_outputs_present(
    exit: ExitOutcome,
    outputs: &[rio_exec::OutputReport],
) -> bool {
    matches!(
        exit,
        ExitOutcome::TimedOut | ExitOutcome::Silent | ExitOutcome::LogLimitExceeded
    ) && !outputs.is_empty()
        && outputs.iter().all(|o| o.exists)
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
    log_cap_trip: Option<&crate::log_stream::LogCapTrip>,
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
            // Per-attempt figures from the typed trip (round-17
            // merged_bug_058 c2): WHICH cap, both sides of the
            // comparison. `None` means rio-exec's own byte limit
            // tripped (corroborated executor-side; it has no builder
            // figures to report).
            error_msg: match log_cap_trip {
                Some(trip) => format!(
                    "{trip}; terminal — the same build on another worker \
                     produces the same logs"
                ),
                None => "build exceeded the executor's log byte limit".into(),
            },
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
    /// Nix content-address descriptor (`fixed:[r:]<algo>:…`) for
    /// content-addressed outputs: filled by floating-CA finalization,
    /// and by `populate_fixed_output_descriptors` for outputs of
    /// derivations matching the strict FOD predicate
    /// (`is_fixed_output()`). `None` for input-addressed outputs.
    /// Destined for the uploaded `PathInfo.content_address` /
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
    /// The output policy itself could not be parsed (malformed __json,
    /// wrong-typed outputChecks/unsafeDiscardReferences). Gates the
    /// whole pipeline — matching the oracle, which fails such builds at
    /// options-parse time — and precedes the pass-1 consumption of
    /// unsafeDiscardReferences, so a wrong-typed discard flag can never
    /// have already influenced scanning by the time it is rejected.
    // r[impl builder.exec.structured-attrs-typed]
    #[error("{0}")]
    PolicyParse(#[from] PolicyParseError),
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
        "fixed-output output '{output}' declares an unusable outputHash/outputHashAlgo: {message}"
    )]
    FodDeclaredHashInvalid { output: String, message: String },
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
    let policy = OutputPolicy::parse(drv.env())?;
    let ca_spec = ca::FloatingCaSpec::from_outputs(drv.outputs())?;
    let candidates = reference_candidates(outputs, input_closure);

    // Pass 1: canonicalise + scan each output (one NAR read per output).
    let mut processed = canonicalise_and_scan_outputs(outputs, build_uid, &candidates, &policy)?;

    // Fixed-output (CAFixed) outputs: record the declared content-address
    // descriptor so registration carries the `CA:` field exactly like
    // CppNix. Floating-CA outputs get theirs minted during finalization;
    // input-addressed outputs carry none.
    populate_fixed_output_descriptors(&mut processed, drv)?;

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

/// Record the `fixed:[r:]<algo>:<hash>` content-address descriptor for
/// outputs whose derivation declares a hash (fixed-output / CAFixed
/// outputs), exactly as CppNix registers them — the descriptor flows
/// into upload registration and the narinfo `CA:` field, and is what
/// exempts fetched sources from the store's "non-CA path with zero
/// references" heuristics.
///
/// The declared `outputHash` in a `.drv` may be base16, nixbase32, or
/// base64 (length-discriminated, CppNix parity); the descriptor uses the
/// canonical nixbase32 rendering, so the hash is decoded with the shared
/// parser and re-encoded here. Malformed declarations are rejected
/// (fail-closed), consistent with the FOD hash verification gate.
fn populate_fixed_output_descriptors(
    processed: &mut [ProcessedOutput],
    drv: &Derivation,
) -> Result<(), OutputRejection> {
    // Strict-FOD gate: a `fixed:` descriptor asserts content the
    // pipeline verified, and `verify_fod_hashes` /
    // `enforce_fod_has_no_references` only run for derivations
    // classifying as Fixed. Non-conforming hash-declaring shapes are
    // unrepresentable past the typed parse boundary, so the stamping
    // and the verifiers key on one and the same classification.
    if !drv.is_fixed_output() {
        return Ok(());
    }
    for o in drv.outputs() {
        let rio_nix::derivation::OutputKind::Fixed {
            hash_algo: raw_algo,
            ..
        } = o.kind()
        else {
            continue;
        };
        // Through THE constructor for algo declarations
        // (r[nix.hash.algos+2]) — this site previously open-coded the
        // strip+parse, so a prefix-semantics change in the owner would
        // have silently diverged the descriptor stamping from every
        // other gate.
        // r[impl nix.hash.algos+2]
        let parsed = rio_nix::hash::OutputHashAlgo::parse(raw_algo).map_err(|_| {
            OutputRejection::FodDeclaredHashInvalid {
                output: o.name().to_owned(),
                message: format!("unsupported outputHashAlgo '{raw_algo}'"),
            }
        })?;
        let (recursive, algo): (bool, HashAlgo) = (parsed.recursive, parsed.algo);
        // Shared length-discriminated decode (base16 / nixbase32 / base64).
        // r[impl nix.hash.fod-decode+1]
        let hash = NixHash::parse_nonsri_unprefixed(algo, o.hash()).map_err(|e| {
            OutputRejection::FodDeclaredHashInvalid {
                output: o.name().to_owned(),
                message: format!(
                    "outputHash is not a valid base16, nixbase32, or base64 hash: {e}"
                ),
            }
        })?;
        let descriptor = format!(
            "fixed:{}{}",
            if recursive { "r:" } else { "" },
            hash.to_colon()
        );
        if let Some(p) = processed.iter_mut().find(|p| p.name == o.name()) {
            p.content_address = Some(descriptor);
        }
    }
    Ok(())
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

    /// The canary trips on exactly one shape: kill verdict + every
    /// declared output present. Partial outputs, no outputs, and
    /// non-kill exits all stay silent.
    // r[verify builder.exec.kill-targets-principal]
    #[test]
    fn kill_verdict_canary_matrix() {
        let report = |exists: bool| rio_exec::OutputReport {
            path: PathBuf::from("/nix/store/x-out"),
            host_path: PathBuf::from("/scratch/x-out"),
            exists,
            metadata: None,
        };

        for kill in [
            ExitOutcome::TimedOut,
            ExitOutcome::Silent,
            ExitOutcome::LogLimitExceeded,
        ] {
            assert!(
                kill_verdict_with_outputs_present(kill, &[report(true), report(true)]),
                "{kill:?} with all outputs present must trip the canary"
            );
            assert!(
                !kill_verdict_with_outputs_present(kill, &[report(true), report(false)]),
                "{kill:?} with a missing output is self-consistent"
            );
            assert!(
                !kill_verdict_with_outputs_present(kill, &[]),
                "{kill:?} with no declared outputs is not canary territory"
            );
        }
        for natural in [
            ExitOutcome::Exited(0),
            ExitOutcome::Exited(1),
            ExitOutcome::Signaled(9),
        ] {
            assert!(
                !kill_verdict_with_outputs_present(natural, &[report(true)]),
                "{natural:?} is not a kill verdict"
            );
        }
    }

    #[test]
    fn classify_success() {
        assert_eq!(
            classify_exit(ExitOutcome::Exited(0), false, false, false, None),
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
            failed(classify_exit(
                ExitOutcome::Exited(1),
                false,
                false,
                false,
                None
            )),
            S::PermanentFailure
        );
        // Same exit on a fixed-output (network) build → transient.
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::Exited(1),
                true,
                false,
                false,
                None
            )),
            S::TransientFailure
        );
        // Signal on a FOD → transient.
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::Signaled(15),
                true,
                false,
                false,
                None
            )),
            S::TransientFailure
        );
        // Disk full beats the FOD-transient rule.
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::Exited(1),
                true,
                true,
                false,
                None
            )),
            S::InfrastructureFailure
        );
        // OOM (SIGKILL + cgroup counter) beats the FOD-transient rule.
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::Signaled(libc::SIGKILL),
                true,
                false,
                true,
                None
            )),
            S::InfrastructureFailure
        );
        // SIGKILL without the OOM counter on a non-FOD → permanent.
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::Signaled(libc::SIGKILL),
                false,
                false,
                false,
                None
            )),
            S::PermanentFailure
        );
        // Timeout / silence / log-limit are passed through.
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::TimedOut,
                false,
                false,
                false,
                None
            )),
            S::TimedOut
        );
        assert_eq!(
            failed(classify_exit(ExitOutcome::Silent, true, true, true, None)),
            S::TimedOut
        );
        assert_eq!(
            failed(classify_exit(
                ExitOutcome::LogLimitExceeded,
                false,
                false,
                false,
                None
            )),
            S::LogLimitExceeded
        );
        // r[verify builder.log-limit+4]
        // The typed trip's per-attempt figures reach the verdict
        // message: WHICH cap and both sides of the comparison.
        let trip = crate::log_stream::LogCapTrip::Bytes {
            would_be: 67_108_865,
            limit: 67_108_864,
        };
        match classify_exit(
            ExitOutcome::LogLimitExceeded,
            false,
            false,
            false,
            Some(&trip),
        ) {
            ExitClassification::Failed { status, error_msg } => {
                assert_eq!(status, S::LogLimitExceeded);
                assert!(
                    error_msg.contains("67108865"),
                    "would-be total: {error_msg}"
                );
                assert!(error_msg.contains("67108864"), "limit: {error_msg}");
                assert!(error_msg.contains("log_size_limit"), "axis: {error_msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let lines = crate::log_stream::LogCapTrip::Lines {
            seen: 1001,
            cap: 1000,
        };
        match classify_exit(
            ExitOutcome::LogLimitExceeded,
            false,
            false,
            false,
            Some(&lines),
        ) {
            ExitClassification::Failed { error_msg, .. } => {
                assert!(
                    error_msg.contains("1001") && error_msg.contains("1000"),
                    "{error_msg}"
                );
                assert!(error_msg.contains("line cap"), "axis: {error_msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
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

    /// Fixed-output (CAFixed) outputs must carry the declared
    /// content-address descriptor, re-encoded from the .drv's base16
    /// `outputHash` to the canonical nixbase32 colon form CppNix
    /// registers (and serves as the narinfo `CA:` field).
    #[test]
    fn fixed_output_descriptor_recorded_recursive() {
        let out_p = sp('a', "src");
        let (_tmp, outputs) = fake_outputs(&[("out", &out_p, b"fetched bytes")]);
        let declared_hex = "11".repeat(32);
        let drv = drv_from_aterm_ca(&[("out", &out_p, "r:sha256", &declared_hex)], &[]);

        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();

        let expected = rio_nix::hash::NixHash::new(
            rio_nix::hash::HashAlgo::SHA256,
            hex::decode(&declared_hex).unwrap(),
        )
        .unwrap();
        assert_eq!(
            processed.outputs[0].content_address.as_deref(),
            Some(format!("fixed:r:{}", expected.to_colon()).as_str()),
            "recursive FOD output must carry its declared CA descriptor"
        );
        // The store path itself is the declared (input-independent) one.
        assert_eq!(processed.outputs[0].store_path, out_p);
    }

    #[test]
    fn fixed_output_descriptor_recorded_flat() {
        let out_p = sp('b', "tarball");
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp.path().join(out_p.strip_prefix("/nix/store/").unwrap());
        std::fs::write(&host, b"flat fetched bytes").unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o644)).unwrap();
        let outputs = [OutputToProcess {
            name: "out".into(),
            store_path: out_p.clone(),
            host_path: host,
        }];
        let declared_hex = "22".repeat(32);
        let drv = drv_from_aterm_ca(&[("out", &out_p, "sha256", &declared_hex)], &[]);

        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();

        let expected = rio_nix::hash::NixHash::new(
            rio_nix::hash::HashAlgo::SHA256,
            hex::decode(&declared_hex).unwrap(),
        )
        .unwrap();
        assert_eq!(
            processed.outputs[0].content_address.as_deref(),
            Some(format!("fixed:{}", expected.to_colon()).as_str()),
            "flat FOD output must carry its declared CA descriptor without the r: prefix"
        );
    }

    #[test]
    fn non_strict_hash_declaring_output_gets_no_descriptor() {
        // A hash-declaring output with an IA sibling does not satisfy
        // the strict FOD predicate — and the shape is now
        // unrepresentable outright (drv-level classification at parse,
        // oracle type() parity), so the stamping can never be asked to
        // vouch for content nothing verified.
        let out_p = sp('b', "tarball");
        let doc_p = sp('c', "tarball-doc");
        let declared_hex = "22".repeat(32);
        let aterm = format!(
            r#"Derive([("out","{out_p}","sha256","{declared_hex}"),("doc","{doc_p}","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#
        );
        let err = Derivation::parse(&aterm).unwrap_err();
        assert!(
            err.to_string()
                .contains("can't mix derivation output types"),
            "{err}"
        );

        // The surviving stamping property over a REPRESENTABLE shape:
        // an all-IA multi-output set gets no CA descriptors (nothing
        // hash-declaring to vouch for).
        let tmp = tempfile::tempdir().unwrap();
        let host_out = tmp.path().join(out_p.strip_prefix("/nix/store/").unwrap());
        let host_doc = tmp.path().join(doc_p.strip_prefix("/nix/store/").unwrap());
        std::fs::write(&host_out, b"flat fetched bytes").unwrap();
        std::fs::write(&host_doc, b"docs").unwrap();
        std::fs::set_permissions(&host_out, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&host_doc, std::fs::Permissions::from_mode(0o644)).unwrap();
        let outputs = [
            OutputToProcess {
                name: "out".into(),
                store_path: out_p.clone(),
                host_path: host_out,
            },
            OutputToProcess {
                name: "doc".into(),
                store_path: doc_p.clone(),
                host_path: host_doc,
            },
        ];
        let drv = drv_from_aterm_ca(&[("out", &out_p, "", ""), ("doc", &doc_p, "", "")], &[]);

        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();
        for p in &processed.outputs {
            assert_eq!(
                p.content_address, None,
                "non-strict hash-declaring shapes must not be stamped with a fixed: descriptor"
            );
        }
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

    /// A FLAT floating-CA output whose bytes embed a *sibling's* scratch
    /// path, with its references discarded via structured-attrs
    /// `unsafeDiscardReferences` (a flat output with *recorded*
    /// references is rejected outright). The flat hash must be computed
    /// over the sibling-rewritten bytes — the bytes that are actually
    /// restored and uploaded — exactly like CppNix, which runs
    /// `rewriteOutput` before hashing regardless of the ingestion mode.
    #[test]
    fn floating_ca_flat_sibling_reference_discarded_hashes_rewritten_bytes() {
        let dep_scratch = sp('s', "dep");
        let out_scratch = sp('w', "blob");
        let tmp = tempfile::tempdir().unwrap();

        let dep_host = tmp
            .path()
            .join(dep_scratch.strip_prefix("/nix/store/").unwrap());
        std::fs::create_dir_all(&dep_host).unwrap();
        std::fs::write(dep_host.join("payload"), b"dep content").unwrap();

        let out_host = tmp
            .path()
            .join(out_scratch.strip_prefix("/nix/store/").unwrap());
        std::fs::write(&out_host, format!("points at {dep_scratch}/payload\n")).unwrap();
        std::fs::set_permissions(&out_host, std::fs::Permissions::from_mode(0o644)).unwrap();

        let outputs = [
            OutputToProcess {
                name: "dep".into(),
                store_path: dep_scratch.clone(),
                host_path: dep_host,
            },
            OutputToProcess {
                name: "out".into(),
                store_path: out_scratch.clone(),
                host_path: out_host,
            },
        ];
        let json = serde_json::json!({ "unsafeDiscardReferences": { "out": true } }).to_string();
        let drv = drv_from_aterm_ca(
            &[("dep", "", "r:sha256", ""), ("out", "", "sha256", "")],
            &[("__json", &json)],
        );

        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();
        let dep = processed.outputs.iter().find(|o| o.name == "dep").unwrap();
        let out = processed.outputs.iter().find(|o| o.name == "out").unwrap();

        // The flat content on disk has been rewritten to dep's final path…
        let bytes = std::fs::read(&out.host_path).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(
            text.contains(&dep.store_path),
            "content must be rewritten to dep's final path: {text}"
        );
        assert!(
            !text.contains(&dep_scratch),
            "no scratch path may survive in the flat content: {text}"
        );
        assert!(out.references.is_empty(), "discarded references stay empty");

        // …and the minted path/descriptor describe exactly those rewritten
        // bytes (this is what the store's flat CA gate recomputes).
        let hash = rio_nix::hash::NixHash::compute(rio_nix::hash::HashAlgo::SHA256, &bytes);
        let want = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            "blob",
            &hash,
            false,
            &[],
            false,
        )
        .unwrap();
        assert_eq!(
            out.store_path,
            want.as_str(),
            "flat path must derive from the rewritten bytes"
        );
        assert_eq!(
            out.content_address.as_deref(),
            Some(format!("fixed:{}", hash.to_colon()).as_str())
        );
    }

    /// FLAT floating-CA output embedding its *own* scratch path under
    /// `unsafeDiscardReferences` — the unit-level mirror of the
    /// `ca-discard-self-flat` differential corpus entry. The embedded
    /// hash is rewritten to the final hash and the path satisfies the
    /// store gate's flat fixed-point check (hash the uploaded bytes
    /// modulo the claimed path's own hash, re-derive, compare).
    #[test]
    fn floating_ca_flat_discarded_self_reference_fixed_point() {
        let scratch = sp('s', "selfblob");
        let tmp = tempfile::tempdir().unwrap();
        let host = tmp
            .path()
            .join(scratch.strip_prefix("/nix/store/").unwrap());
        std::fs::write(&host, format!("I live at {scratch}\n")).unwrap();
        std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o644)).unwrap();
        let outputs = [OutputToProcess {
            name: "out".into(),
            store_path: scratch.clone(),
            host_path: host,
        }];
        let json = serde_json::json!({ "unsafeDiscardReferences": { "out": true } }).to_string();
        let drv = drv_from_aterm_ca(&[("out", "", "sha256", "")], &[("__json", &json)]);

        let processed = process_outputs(&drv, &outputs, my_uid(), &[]).unwrap();
        let out = &processed.outputs[0];
        assert!(out.references.is_empty());
        assert_ne!(out.store_path, scratch);

        let final_path = rio_nix::store_path::StorePath::parse(&out.store_path).unwrap();
        let scratch_parsed = rio_nix::store_path::StorePath::parse(&scratch).unwrap();
        let bytes = std::fs::read(&out.host_path).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(
            text.contains(&final_path.hash_part()),
            "embedded hash must be rewritten to the final hash: {text}"
        );
        assert!(
            !text.contains(&scratch_parsed.hash_part()),
            "the scratch hash must not survive finalization: {text}"
        );

        // Store-gate fixed point, flat edition.
        let mut sink = rio_nix::ca::HashModuloSink::new(
            rio_nix::hash::HashAlgo::SHA256,
            &final_path.hash_part(),
        );
        sink.write_all(&bytes).unwrap();
        let (modulo, hits) = sink.finish();
        assert!(hits > 0, "the rewritten self-hash must be present");
        let rederived = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            "selfblob",
            &modulo,
            false,
            &[],
            false,
        )
        .unwrap();
        assert_eq!(
            rederived.as_str(),
            out.store_path,
            "flat discarded-self fixed point"
        );
        assert_eq!(
            out.content_address.as_deref(),
            Some(format!("fixed:{}", modulo.to_colon()).as_str())
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
    fn mixed_ca_and_input_addressed_outputs_rejected() {
        // "lib" is input-addressed (declared path); "out" is floating-CA.
        // CppNix refuses this shape outright ("can't mix derivation output
        // types", BasicDerivation::type()), and finalizing it here would
        // remap lib's REFERENCES to out's final path while leaving lib's
        // BYTES naming the scratch path — a corrupt artifact. The pipeline
        // must reject, not half-finalize.
        //
        // The legal all-floating sibling case (references remapped AND
        // bytes rewritten) is pinned by
        // `floating_ca_sibling_reference_finalized`.
        // r[verify builder.exec.output-types-unmixed+1]
        // The shape is now unrepresentable: the typed parse boundary
        // classifies the output SET at parse, so the corrupt-artifact
        // hazard (references remapped to out's final path while lib's
        // bytes still name the scratch) has no constructible input.
        let lib_p = sp('a', "demo-lib");
        let aterm = format!(
            r#"Derive([("lib","{lib_p}","",""),("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[])"#
        );
        let err = Derivation::parse(&aterm).unwrap_err();
        assert!(
            err.to_string()
                .contains("can't mix derivation output types"),
            "error carries the oracle's wording: {err}"
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

//! Outcome comparison for `xtask k8s replay` — classify every replayed
//! derivation against its recorded outcome, tally verdicts, and stream
//! divergence records as they are found.
//!
//! [`classify`] turns one derived path's replay-side [`DerivedOutcome`] plus
//! its recorded [`BuildRecord`] into a [`Verdict`]; [`classify_request`]
//! applies it to a whole [`RequestOutcome`], updates the [`VerdictCounts`],
//! and appends [`DivergenceRecord`] lines to the [`DivergenceLog`]. The
//! verdict taxonomy and the precedence of the classification rules are the
//! contract the replay report is built on — see the `Verdict` variants for
//! what each bucket means.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rio_nix::protocol::build::BuildStatus;

use super::archive::{BuildRecord, prod_status};
use super::timeline::{DerivedOutcome, RequestError, RequestErrorKind, RequestOutcome};

/// How one replayed derivation behaved relative to its recorded outcome.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Behaved like the recording (hashes equal, or a recorded failure /
    /// cancellation that also failed here).
    Match,
    /// Not comparable; `reason` says why.
    Skip {
        /// Why no comparison was possible.
        reason: String,
    },
    /// The recording built it; the replay could not (after `attempts` tries).
    Regression {
        /// The replay-side build error message (or status name).
        error: String,
        /// Build attempts made before the failure was allowed to stand.
        attempts: u32,
    },
    /// Both built; at least one output's NAR hash differs or is missing.
    NonReproducible {
        /// Recorded output name → NAR hash (lowercase hex).
        recorded: BTreeMap<String, String>,
        /// Replay-collected output name → NAR hash (lowercase hex).
        replayed: BTreeMap<String, String>,
    },
    /// The recording failed deterministically; the replay succeeded.
    FailureNotReproduced {
        /// The recorded failure status code.
        recorded_status: i32,
    },
    /// The recording was cancelled; the replay succeeded.
    CancellationNotReproduced {
        /// The recorded cancellation status code.
        recorded_status: i32,
    },
    /// The request was replayed as a recorded client disconnect; nothing to
    /// compare.
    DisconnectReplayed,
    /// The daemon refused this request's upload (after one retry); the build
    /// was not attempted.
    UploadRejected {
        /// The daemon's refusal message.
        error: String,
    },
    /// The request hit an infrastructure error before/around the build;
    /// nothing attributable to the target's build behavior.
    RequestError {
        /// Which replay stage broke.
        #[serde(serialize_with = "serialize_error_kind")]
        kind: RequestErrorKind,
        /// Human-readable detail.
        message: String,
    },
}

/// Serialize a [`RequestErrorKind`] as its variant name (e.g. `"Collect"`);
/// the kind enum lives in the timeline module and carries no serde derives.
fn serialize_error_kind<S>(
    kind: &RequestErrorKind,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&format!("{kind:?}"))
}

impl Verdict {
    /// Whether this verdict is a divergence: the target's build behavior
    /// observably differed from the recording (regressions, non-reproducible
    /// outputs, unreproduced failures/cancellations, upload rejections).
    pub fn is_divergence(&self) -> bool {
        matches!(
            self,
            Verdict::Regression { .. }
                | Verdict::NonReproducible { .. }
                | Verdict::FailureNotReproduced { .. }
                | Verdict::CancellationNotReproduced { .. }
                | Verdict::UploadRejected { .. }
        )
    }

    /// Short stable name for counts and log lines.
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Match => "match",
            Verdict::Skip { .. } => "skip",
            Verdict::Regression { .. } => "regression",
            Verdict::NonReproducible { .. } => "non_reproducible",
            Verdict::FailureNotReproduced { .. } => "failure_not_reproduced",
            Verdict::CancellationNotReproduced { .. } => "cancellation_not_reproduced",
            Verdict::DisconnectReplayed => "disconnect_replayed",
            Verdict::UploadRejected { .. } => "upload_rejected",
            Verdict::RequestError { .. } => "request_error",
        }
    }
}

/// Classify ONE derived path of ONE request against its recorded outcome.
///
/// `prod` is the build record for (the request's session, this drv path);
/// `request_error` is the request-level infrastructure error, if any;
/// `demoted` marks drvs listed in `impure-env.json` (supplied instead of
/// rebuilt); `disconnected` marks requests replayed as a recorded client
/// disconnect; `attempts` is how many build attempts the request made.
///
/// The rules below are checked in order; the first that applies wins.
pub fn classify(
    prod: Option<&BuildRecord>,
    outcome: &DerivedOutcome,
    request_error: Option<&RequestError>,
    demoted: bool,
    disconnected: bool,
    attempts: u32,
) -> Verdict {
    // 0. The daemon refused the request's upload — the build never ran.
    if let Some(error) = &outcome.upload_rejected {
        return Verdict::UploadRejected {
            error: error.clone(),
        };
    }
    // 1. The request was replayed as a recorded client disconnect.
    if disconnected {
        return Verdict::DisconnectReplayed;
    }
    // 2. A request-level infrastructure error left this path without a usable
    //    build result.
    if let (Some(error), None) = (request_error, outcome.result.as_ref()) {
        return Verdict::RequestError {
            kind: error.kind,
            message: error.message.clone(),
        };
    }
    // 3. The drv's impure environment was not forwarded; its recorded outputs
    //    were supplied instead of rebuilding, so there is nothing to compare.
    if demoted {
        return Verdict::Skip {
            reason: "impure environment not forwarded; recorded outputs supplied instead of \
                     rebuilding"
                .to_string(),
        };
    }
    // 4. No recorded build at all (cache hit at record time).
    let Some(prod) = prod else {
        return Verdict::Skip {
            reason: "no recorded build (cache hit at record time)".to_string(),
        };
    };
    // 5. The recorded outcome was itself infrastructure-dependent.
    if prod.status == prod_status::BUILDER_ERROR || prod.status == prod_status::CLIENT_DISCONNECT {
        return Verdict::Skip {
            reason: format!(
                "recorded outcome was infrastructure-dependent (status {})",
                prod.status
            ),
        };
    }
    // 6. No replay result and no request error to blame — defensive; rule 2
    //    normally covers the missing-result case.
    let Some(result) = outcome.result.as_ref() else {
        return match request_error {
            Some(error) => Verdict::RequestError {
                kind: error.kind,
                message: error.message.clone(),
            },
            None => Verdict::RequestError {
                kind: RequestErrorKind::BuildTransport,
                message: "no build result".to_string(),
            },
        };
    };
    // 7. The replay "succeeded" without rebuilding (substituted / already
    //    valid) — there is no fresh build to compare.
    if result.status.is_success() && result.status != BuildStatus::Built {
        return Verdict::Skip {
            reason: "target already had the outputs (substituted or already valid — not rebuilt)"
                .to_string(),
        };
    }
    let replay_success = result.status.is_success();
    // 8. Recorded cancellation: a replay success means the cancellation was
    //    not reproduced; a replay failure behaves like the recording.
    if prod.status == prod_status::CANCELLED {
        return if replay_success {
            Verdict::CancellationNotReproduced {
                recorded_status: prod.status,
            }
        } else {
            Verdict::Match
        };
    }
    // 9. Recorded success: a replay failure is a regression; a replay build
    //    is compared output hash by output hash.
    if prod.status == prod_status::BUILT {
        if !replay_success {
            let error = if result.error_msg.is_empty() {
                format!("{:?}", result.status)
            } else {
                result.error_msg.clone()
            };
            return Verdict::Regression { error, attempts };
        }
        return compare_output_hashes(prod, outcome, request_error);
    }
    // 10. Any other recorded status is a deterministic failure (unknown codes
    //     included): a replay success means the failure was not reproduced; a
    //     replay failure behaves like the recording.
    if replay_success {
        Verdict::FailureNotReproduced {
            recorded_status: prod.status,
        }
    } else {
        Verdict::Match
    }
}

/// Rule 9 hash comparison: every recorded output's NAR hash must be present
/// in the replay-collected map and equal (case-insensitive hex). A missing
/// hash with a Collect-stage request error is our collection failure, not the
/// target's build behavior, and skips the comparison instead of blaming the
/// target.
fn compare_output_hashes(
    prod: &BuildRecord,
    outcome: &DerivedOutcome,
    request_error: Option<&RequestError>,
) -> Verdict {
    let mut missing = false;
    let mut differs = false;
    for (name, recorded) in &prod.outputs {
        match outcome.replay_nar_hashes.get(name) {
            Some(replayed) if replayed.eq_ignore_ascii_case(&recorded.nar_hash_hex) => {}
            Some(_) => differs = true,
            None => missing = true,
        }
    }
    if !missing && !differs {
        return Verdict::Match;
    }
    if missing && request_error.is_some_and(|error| error.kind == RequestErrorKind::Collect) {
        return Verdict::Skip {
            reason: "output hashes could not be collected (infrastructure)".to_string(),
        };
    }
    Verdict::NonReproducible {
        recorded: prod
            .outputs
            .iter()
            .map(|(name, output)| (name.clone(), output.nar_hash_hex.clone()))
            .collect(),
        replayed: outcome.replay_nar_hashes.clone(),
    }
}

/// Tally of verdicts across a replay run, one bucket per [`Verdict`] variant
/// plus the request-level flaky signal.
#[derive(Debug, Default, serde::Serialize)]
pub struct VerdictCounts {
    /// Derived paths that behaved like the recording.
    pub matches: u64,
    /// Derived paths that could not be compared.
    pub skips: u64,
    /// Recorded successes the replay could not rebuild.
    pub regressions: u64,
    /// Rebuilt paths whose output hashes differ from the recording.
    pub non_reproducible: u64,
    /// Recorded deterministic failures that succeeded on replay.
    pub failure_not_reproduced: u64,
    /// Recorded cancellations that succeeded on replay.
    pub cancellation_not_reproduced: u64,
    /// Derived paths replayed as recorded client disconnects.
    pub disconnect_replayed: u64,
    /// Derived paths whose request upload the daemon refused.
    pub upload_rejected: u64,
    /// Derived paths lost to request-level infrastructure errors.
    pub request_errors: u64,
    /// Requests that needed >1 attempt but ultimately matched (flaky signal).
    pub flaky: u64,
}

impl VerdictCounts {
    /// Bump the bucket for one verdict.
    pub fn record(&mut self, verdict: &Verdict) {
        match verdict {
            Verdict::Match => self.matches += 1,
            Verdict::Skip { .. } => self.skips += 1,
            Verdict::Regression { .. } => self.regressions += 1,
            Verdict::NonReproducible { .. } => self.non_reproducible += 1,
            Verdict::FailureNotReproduced { .. } => self.failure_not_reproduced += 1,
            Verdict::CancellationNotReproduced { .. } => self.cancellation_not_reproduced += 1,
            Verdict::DisconnectReplayed => self.disconnect_replayed += 1,
            Verdict::UploadRejected { .. } => self.upload_rejected += 1,
            Verdict::RequestError { .. } => self.request_errors += 1,
        }
    }

    /// Sum of the divergence buckets ([`Verdict::is_divergence`]): regressions,
    /// non-reproducible builds, unreproduced failures and cancellations, and
    /// upload rejections.
    pub fn divergences(&self) -> u64 {
        self.regressions
            + self.non_reproducible
            + self.failure_not_reproduced
            + self.cancellation_not_reproduced
            + self.upload_rejected
    }

    /// Total verdicts recorded across every bucket. The `flaky` counter is a
    /// per-request signal layered on top of (already counted) Match/Skip
    /// verdicts, so it is not part of the sum.
    pub fn total(&self) -> u64 {
        self.matches
            + self.skips
            + self.regressions
            + self.non_reproducible
            + self.failure_not_reproduced
            + self.cancellation_not_reproduced
            + self.disconnect_replayed
            + self.upload_rejected
            + self.request_errors
    }
}

/// One JSONL line per divergence (and per request-level error), streamed as
/// found and flushed per line so an interrupted run still leaves everything
/// found so far on disk.
#[derive(Debug)]
pub struct DivergenceLog {
    writer: BufWriter<File>,
    path: PathBuf,
}

impl DivergenceLog {
    /// Create `<dir>/divergences.jsonl` (truncating a previous run's file),
    /// creating `dir` itself if needed.
    pub fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create report directory {}", dir.display()))?;
        let path = dir.join("divergences.jsonl");
        let file = File::create(&path)
            .with_context(|| format!("create divergence log {}", path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
            path,
        })
    }

    /// Append one record as a JSON line and flush it.
    pub fn write(&mut self, record: &DivergenceRecord) -> Result<()> {
        let mut line = serde_json::to_vec(record).context("serialize divergence record")?;
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .and_then(|()| self.writer.flush())
            .with_context(|| format!("append to {}", self.path.display()))?;
        Ok(())
    }

    /// Where the log lives.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// One divergence (or request-level error) found during the replay — a line
/// of `divergences.jsonl`.
#[derive(Debug, serde::Serialize)]
pub struct DivergenceRecord {
    /// Recorded client session the request belonged to.
    pub ssh_session_id: i64,
    /// Schedule index of the replayed request.
    pub request_index: usize,
    /// The derived path the verdict is about.
    pub drv_path: String,
    /// The verdict itself.
    pub verdict: Verdict,
    /// Recorded status code for this drv, when a build record exists.
    pub recorded_status: Option<i32>,
    /// Recorded status message, when the recorder captured one.
    pub recorded_status_msg: Option<String>,
    /// Build attempts the request made.
    pub attempts: u32,
    /// How late the request was dispatched vs its recorded offset.
    pub dispatch_lateness_ms: u64,
    /// Other drv paths submitted in the same request (correlated-failure
    /// context: the gateway applies one DAG-level result per request, so a
    /// sibling's failure can drag recorded-success drvs down before
    /// confirmation re-runs).
    pub sibling_drvs: Vec<String>,
    /// Secondary request-level error (e.g. hash collection failed) that did
    /// not prevent classification but is worth surfacing.
    pub note: Option<String>,
}

/// Classify a whole [`RequestOutcome`]: one `(drv_path, Verdict)` per derived
/// path (in request order), recorded into `counts`, with divergence records
/// appended to `log` as they are found.
///
/// `demoted_drvs` is the set of drv paths listed in `impure-env.json` (their
/// recorded outputs were supplied instead of rebuilding).
///
/// Log records: one per derived path whose verdict
/// [`is_divergence`](Verdict::is_divergence), each carrying the request's
/// other drv paths as correlated-failure context; plus, for a request whose
/// verdicts are ALL [`Verdict::RequestError`], exactly ONE record (keyed to
/// the first derived path) so infrastructure noise does not multiply by
/// closure size. The counts intentionally do NOT dedupe the same way:
/// `counts.record()` is called for every verdict, so `request_errors` counts
/// every affected derived path while the log carries a single line for an
/// all-request-error request.
///
/// When a request-level error coexists with classifiable results (e.g. a
/// Collect failure next to a real divergence), its kind and message are
/// surfaced as `note` on each written record.
pub fn classify_request(
    outcome: &RequestOutcome,
    builds: &HashMap<(i64, String), BuildRecord>,
    demoted_drvs: &BTreeSet<String>,
    counts: &mut VerdictCounts,
    log: &mut DivergenceLog,
) -> Result<Vec<(String, Verdict)>> {
    let session = outcome.request.ssh_session_id;
    let mut verdicts: Vec<(String, Verdict)> = Vec::with_capacity(outcome.results.len());
    for derived in &outcome.results {
        let prod = builds.get(&(session, derived.drv_path.clone()));
        let verdict = classify(
            prod,
            derived,
            outcome.error.as_ref(),
            demoted_drvs.contains(&derived.drv_path),
            outcome.disconnected,
            outcome.attempts,
        );
        counts.record(&verdict);
        verdicts.push((derived.drv_path.clone(), verdict));
    }

    // Flaky signal: confirmation retries were needed but everything ultimately
    // matched (or was not comparable) — the recording was reproduced, just not
    // on the first try.
    if outcome.attempts > 1
        && !verdicts.is_empty()
        && verdicts
            .iter()
            .all(|(_, verdict)| matches!(verdict, Verdict::Match | Verdict::Skip { .. }))
    {
        counts.flaky += 1;
    }

    let all_request_errors = !verdicts.is_empty()
        && verdicts
            .iter()
            .all(|(_, verdict)| matches!(verdict, Verdict::RequestError { .. }));

    let make_record = |drv_path: &str, verdict: &Verdict, note: Option<String>| {
        let prod = builds.get(&(session, drv_path.to_string()));
        DivergenceRecord {
            ssh_session_id: session,
            request_index: outcome.index,
            drv_path: drv_path.to_string(),
            verdict: verdict.clone(),
            recorded_status: prod.map(|record| record.status),
            recorded_status_msg: prod.and_then(|record| record.status_msg.clone()),
            attempts: outcome.attempts,
            dispatch_lateness_ms: u64::try_from(outcome.dispatch_lateness.as_millis())
                .unwrap_or(u64::MAX),
            sibling_drvs: outcome
                .request
                .paths
                .iter()
                .map(|(drv, _outputs)| drv.clone())
                .filter(|drv| drv != drv_path)
                .collect(),
            note,
        }
    };

    if all_request_errors {
        // One line for the whole request: the infrastructure error already is
        // the verdict, and it must not multiply into one line per closure
        // member.
        let (drv_path, verdict) = &verdicts[0];
        log.write(&make_record(drv_path, verdict, None))?;
    } else {
        // Secondary request-level error to surface alongside real divergences
        // (it did not prevent classification, but explains e.g. skipped hash
        // comparisons).
        let note = outcome
            .error
            .as_ref()
            .map(|error| format!("{:?}: {}", error.kind, error.message));
        for (drv_path, verdict) in &verdicts {
            if !verdict.is_divergence() {
                continue;
            }
            log.write(&make_record(drv_path, verdict, note.clone()))?;
        }
    }

    Ok(verdicts)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rio_nix::protocol::build::BuildResult;

    use super::*;
    use crate::k8s::replay::archive::{OutputRecord, ReplayRequest};

    const DRV: &str = "/nix/store/d1111111111111111111111111111111-target.drv";
    const SIBLING_DRV: &str = "/nix/store/d2222222222222222222222222222222-sibling.drv";
    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Recorded build with `status` and the given output-name → NAR-hash map.
    fn record(status: i32, outputs: &[(&str, &str)]) -> BuildRecord {
        BuildRecord {
            ssh_session_id: 1,
            drv_path: DRV.to_string(),
            status,
            status_msg: Some("recorded message".to_string()),
            duration_s: None,
            stop_offset_s: None,
            outputs: outputs
                .iter()
                .map(|(name, hash)| {
                    (
                        name.to_string(),
                        OutputRecord {
                            nar_hash_hex: hash.to_string(),
                            nar_size: 1,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Replay-side derived outcome with a build result and collected hashes.
    fn outcome(result: Option<BuildResult>, hashes: &[(&str, &str)]) -> DerivedOutcome {
        DerivedOutcome {
            drv_path: DRV.to_string(),
            outputs: vec!["out".to_string()],
            result,
            replay_nar_hashes: hashes
                .iter()
                .map(|(name, hash)| (name.to_string(), hash.to_string()))
                .collect(),
            upload_rejected: None,
        }
    }

    fn built() -> Option<BuildResult> {
        Some(BuildResult::success())
    }

    fn failed(message: &str) -> Option<BuildResult> {
        Some(BuildResult::failure(BuildStatus::PermanentFailure, message))
    }

    fn not_rebuilt(status: BuildStatus) -> Option<BuildResult> {
        Some(BuildResult {
            status,
            ..BuildResult::default()
        })
    }

    fn request_error(kind: RequestErrorKind, message: &str) -> RequestError {
        RequestError {
            kind,
            message: message.to_string(),
        }
    }

    fn skip(reason: &str) -> Verdict {
        Verdict::Skip {
            reason: reason.to_string(),
        }
    }

    fn hash_map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, hash)| (name.to_string(), hash.to_string()))
            .collect()
    }

    /// Every classification rule, in precedence order, with minimal inputs.
    #[test]
    fn classify_table() {
        struct Case {
            name: &'static str,
            prod: Option<BuildRecord>,
            outcome: DerivedOutcome,
            request_error: Option<RequestError>,
            demoted: bool,
            disconnected: bool,
            attempts: u32,
            expected: Verdict,
        }
        let case = |name, prod, outcome, expected| Case {
            name,
            prod,
            outcome,
            request_error: None,
            demoted: false,
            disconnected: false,
            attempts: 1,
            expected,
        };

        let cases = vec![
            // Rule 0: an upload rejection wins over everything else.
            Case {
                name: "upload rejected",
                outcome: DerivedOutcome {
                    upload_rejected: Some("path info rejected".to_string()),
                    ..outcome(failed("boom"), &[])
                },
                disconnected: true,
                ..case(
                    "",
                    Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                    outcome(None, &[]),
                    Verdict::UploadRejected {
                        error: "path info rejected".to_string(),
                    },
                )
            },
            // Rule 1: disconnect replay.
            Case {
                disconnected: true,
                ..case(
                    "disconnect replayed",
                    Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                    outcome(None, &[]),
                    Verdict::DisconnectReplayed,
                )
            },
            // Rule 2: request-level error with no usable build result.
            Case {
                request_error: Some(request_error(RequestErrorKind::ChannelOpen, "dial failed")),
                ..case(
                    "request error without result",
                    Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                    outcome(None, &[]),
                    Verdict::RequestError {
                        kind: RequestErrorKind::ChannelOpen,
                        message: "dial failed".to_string(),
                    },
                )
            },
            // Rule 3: demoted (impure-env) drvs are supplied, not rebuilt.
            Case {
                demoted: true,
                ..case(
                    "demoted impure drv",
                    Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                    outcome(built(), &[("out", HASH_A)]),
                    skip(
                        "impure environment not forwarded; recorded outputs supplied instead of \
                         rebuilding",
                    ),
                )
            },
            // Rule 4: no recorded build.
            case(
                "no recorded build",
                None,
                outcome(built(), &[("out", HASH_A)]),
                skip("no recorded build (cache hit at record time)"),
            ),
            // Rule 5: infrastructure-dependent recorded outcomes.
            case(
                "recorded builder error",
                Some(record(prod_status::BUILDER_ERROR, &[])),
                outcome(built(), &[]),
                skip("recorded outcome was infrastructure-dependent (status 10)"),
            ),
            case(
                "recorded client disconnect",
                Some(record(prod_status::CLIENT_DISCONNECT, &[])),
                outcome(failed("boom"), &[]),
                skip("recorded outcome was infrastructure-dependent (status 13)"),
            ),
            // Rule 6: missing result without a request error (defensive).
            case(
                "missing result without request error",
                Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                outcome(None, &[]),
                Verdict::RequestError {
                    kind: RequestErrorKind::BuildTransport,
                    message: "no build result".to_string(),
                },
            ),
            // Rule 7: success without an actual rebuild.
            case(
                "substituted",
                Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                outcome(not_rebuilt(BuildStatus::Substituted), &[]),
                skip("target already had the outputs (substituted or already valid — not rebuilt)"),
            ),
            case(
                "already valid",
                Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                outcome(not_rebuilt(BuildStatus::AlreadyValid), &[]),
                skip("target already had the outputs (substituted or already valid — not rebuilt)"),
            ),
            // Rule 8: recorded cancellation.
            case(
                "cancellation not reproduced",
                Some(record(prod_status::CANCELLED, &[])),
                outcome(built(), &[]),
                Verdict::CancellationNotReproduced {
                    recorded_status: prod_status::CANCELLED,
                },
            ),
            case(
                "cancellation matched by failure",
                Some(record(prod_status::CANCELLED, &[])),
                outcome(failed("boom"), &[]),
                Verdict::Match,
            ),
            // Rule 9: recorded success.
            Case {
                attempts: 3,
                ..case(
                    "regression",
                    Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                    outcome(failed("boom"), &[]),
                    Verdict::Regression {
                        error: "boom".to_string(),
                        attempts: 3,
                    },
                )
            },
            case(
                "regression without error message uses the status name",
                Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                outcome(Some(BuildResult::failure(BuildStatus::TimedOut, "")), &[]),
                Verdict::Regression {
                    error: "TimedOut".to_string(),
                    attempts: 1,
                },
            ),
            case(
                "hashes equal (case-insensitive)",
                Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                outcome(built(), &[("out", HASH_A.to_uppercase().as_str())]),
                Verdict::Match,
            ),
            case(
                "hash differs",
                Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                outcome(built(), &[("out", HASH_B)]),
                Verdict::NonReproducible {
                    recorded: hash_map(&[("out", HASH_A)]),
                    replayed: hash_map(&[("out", HASH_B)]),
                },
            ),
            case(
                "hash missing",
                Some(record(
                    prod_status::BUILT,
                    &[("out", HASH_A), ("doc", HASH_B)],
                )),
                outcome(built(), &[("out", HASH_A)]),
                Verdict::NonReproducible {
                    recorded: hash_map(&[("out", HASH_A), ("doc", HASH_B)]),
                    replayed: hash_map(&[("out", HASH_A)]),
                },
            ),
            Case {
                request_error: Some(request_error(
                    RequestErrorKind::Collect,
                    "QueryPathInfo failed",
                )),
                ..case(
                    "hash missing because collection failed",
                    Some(record(prod_status::BUILT, &[("out", HASH_A)])),
                    outcome(built(), &[]),
                    skip("output hashes could not be collected (infrastructure)"),
                )
            },
            // Rule 10: recorded deterministic failures (and unknown codes).
            case(
                "deterministic failure not reproduced",
                Some(record(1, &[])),
                outcome(built(), &[]),
                Verdict::FailureNotReproduced { recorded_status: 1 },
            ),
            case(
                "deterministic failure matched by failure",
                Some(record(1, &[])),
                outcome(failed("boom"), &[]),
                Verdict::Match,
            ),
            case(
                "resource exhaustion not reproduced",
                Some(record(prod_status::RESOURCE_EXHAUSTED, &[])),
                outcome(built(), &[]),
                Verdict::FailureNotReproduced {
                    recorded_status: prod_status::RESOURCE_EXHAUSTED,
                },
            ),
            case(
                "unknown recorded status matched by failure",
                Some(record(99, &[])),
                outcome(failed("boom"), &[]),
                Verdict::Match,
            ),
        ];

        for case in cases {
            let got = classify(
                case.prod.as_ref(),
                &case.outcome,
                case.request_error.as_ref(),
                case.demoted,
                case.disconnected,
                case.attempts,
            );
            assert_eq!(got, case.expected, "case: {}", case.name);
        }
    }

    #[test]
    fn verdict_counts_and_divergence_set() {
        let verdicts = [
            Verdict::Match,
            Verdict::Skip {
                reason: "x".to_string(),
            },
            Verdict::Regression {
                error: "boom".to_string(),
                attempts: 2,
            },
            Verdict::NonReproducible {
                recorded: BTreeMap::new(),
                replayed: BTreeMap::new(),
            },
            Verdict::FailureNotReproduced { recorded_status: 1 },
            Verdict::CancellationNotReproduced { recorded_status: 6 },
            Verdict::DisconnectReplayed,
            Verdict::UploadRejected {
                error: "refused".to_string(),
            },
            Verdict::RequestError {
                kind: RequestErrorKind::Probe,
                message: "probe failed".to_string(),
            },
        ];
        let mut counts = VerdictCounts::default();
        for verdict in &verdicts {
            counts.record(verdict);
        }
        assert_eq!(counts.matches, 1);
        assert_eq!(counts.skips, 1);
        assert_eq!(counts.regressions, 1);
        assert_eq!(counts.non_reproducible, 1);
        assert_eq!(counts.failure_not_reproduced, 1);
        assert_eq!(counts.cancellation_not_reproduced, 1);
        assert_eq!(counts.disconnect_replayed, 1);
        assert_eq!(counts.upload_rejected, 1);
        assert_eq!(counts.request_errors, 1);
        assert_eq!(counts.flaky, 0);
        assert_eq!(counts.divergences(), 5);
        assert_eq!(counts.total(), 9);

        // is_divergence covers exactly the five divergence buckets.
        let divergent: Vec<&str> = verdicts
            .iter()
            .filter(|verdict| verdict.is_divergence())
            .map(Verdict::label)
            .collect();
        assert_eq!(
            divergent,
            vec![
                "regression",
                "non_reproducible",
                "failure_not_reproduced",
                "cancellation_not_reproduced",
                "upload_rejected",
            ]
        );
    }

    /// Two-drv request whose sibling matched but whose target regressed on the
    /// second attempt: the divergence record carries the sibling and the
    /// attempt count, and flaky is reserved for requests whose retries ended
    /// in a full match.
    #[test]
    fn classify_request_flaky_and_sibling_context() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = DivergenceLog::create(tmp.path()).unwrap();
        let mut counts = VerdictCounts::default();

        let mut builds: HashMap<(i64, String), BuildRecord> = HashMap::new();
        builds.insert(
            (1, DRV.to_string()),
            record(prod_status::BUILT, &[("out", HASH_A)]),
        );
        builds.insert(
            (1, SIBLING_DRV.to_string()),
            BuildRecord {
                drv_path: SIBLING_DRV.to_string(),
                ..record(prod_status::BUILT, &[("out", HASH_B)])
            },
        );
        builds.insert(
            (2, DRV.to_string()),
            record(prod_status::BUILT, &[("out", HASH_A)]),
        );

        // Request 0: target regressed after 2 attempts, sibling rebuilt with
        // the recorded hash; a Collect-stage error coexists and becomes the
        // record's note.
        let regressed = RequestOutcome {
            index: 0,
            request: ReplayRequest {
                ssh_session_id: 1,
                offset_s: 0.0,
                paths: vec![
                    (DRV.to_string(), vec!["out".to_string()]),
                    (SIBLING_DRV.to_string(), vec!["out".to_string()]),
                ],
            },
            results: vec![
                outcome(failed("boom"), &[]),
                DerivedOutcome {
                    drv_path: SIBLING_DRV.to_string(),
                    ..outcome(built(), &[("out", HASH_B)])
                },
            ],
            attempts: 2,
            disconnected: false,
            error: Some(request_error(
                RequestErrorKind::Collect,
                "hash collection failed",
            )),
            dispatch_lateness: Duration::from_millis(1500),
        };
        let verdicts =
            classify_request(&regressed, &builds, &BTreeSet::new(), &mut counts, &mut log).unwrap();
        assert_eq!(verdicts.len(), 2);
        assert_eq!(
            verdicts[0],
            (
                DRV.to_string(),
                Verdict::Regression {
                    error: "boom".to_string(),
                    attempts: 2,
                }
            )
        );
        assert_eq!(verdicts[1], (SIBLING_DRV.to_string(), Verdict::Match));
        assert_eq!(counts.regressions, 1);
        assert_eq!(counts.matches, 1);
        assert_eq!(counts.flaky, 0, "a divergence is never flaky");

        // Request 1: a single drv that matched after 3 attempts — flaky.
        let flaky = RequestOutcome {
            index: 1,
            request: ReplayRequest {
                ssh_session_id: 2,
                offset_s: 1.0,
                paths: vec![(DRV.to_string(), vec!["out".to_string()])],
            },
            results: vec![outcome(built(), &[("out", HASH_A)])],
            attempts: 3,
            disconnected: false,
            error: None,
            dispatch_lateness: Duration::ZERO,
        };
        classify_request(&flaky, &builds, &BTreeSet::new(), &mut counts, &mut log).unwrap();
        assert_eq!(counts.flaky, 1);
        assert_eq!(counts.matches, 2);

        // Request 2: a 2-drv request lost entirely to an infra error — every
        // derived path counts as a request error but the log gets ONE line.
        let infra = RequestOutcome {
            index: 2,
            request: ReplayRequest {
                ssh_session_id: 3,
                offset_s: 2.0,
                paths: vec![
                    (DRV.to_string(), vec!["out".to_string()]),
                    (SIBLING_DRV.to_string(), vec!["out".to_string()]),
                ],
            },
            results: vec![
                outcome(None, &[]),
                DerivedOutcome {
                    drv_path: SIBLING_DRV.to_string(),
                    ..outcome(None, &[])
                },
            ],
            attempts: 0,
            disconnected: false,
            error: Some(request_error(RequestErrorKind::ChannelOpen, "dial failed")),
            dispatch_lateness: Duration::ZERO,
        };
        classify_request(&infra, &builds, &BTreeSet::new(), &mut counts, &mut log).unwrap();
        assert_eq!(counts.request_errors, 2);
        assert_eq!(counts.divergences(), 1);
        assert_eq!(counts.total(), 5);

        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "one regression line + one infra line");

        // The regression record: sibling context, attempts, lateness, note.
        assert_eq!(lines[0]["verdict"]["verdict"], "regression");
        assert_eq!(lines[0]["drv_path"], DRV);
        assert_eq!(lines[0]["ssh_session_id"], 1);
        assert_eq!(lines[0]["request_index"], 0);
        assert_eq!(lines[0]["attempts"], 2);
        assert_eq!(lines[0]["verdict"]["attempts"], 2);
        assert_eq!(lines[0]["dispatch_lateness_ms"], 1500);
        assert_eq!(lines[0]["recorded_status"], 0);
        assert_eq!(lines[0]["sibling_drvs"], serde_json::json!([SIBLING_DRV]));
        assert_eq!(lines[0]["note"], "Collect: hash collection failed");

        // The all-request-error record: exactly one, keyed to the first drv.
        assert_eq!(lines[1]["verdict"]["verdict"], "request_error");
        assert_eq!(lines[1]["verdict"]["kind"], "ChannelOpen");
        assert_eq!(lines[1]["drv_path"], DRV);
        assert_eq!(lines[1]["ssh_session_id"], 3);
        assert_eq!(lines[1]["sibling_drvs"], serde_json::json!([SIBLING_DRV]));
        assert_eq!(lines[1]["note"], serde_json::Value::Null);
    }

    #[test]
    fn divergence_log_streams_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let mut log = DivergenceLog::create(tmp.path()).unwrap();
        assert_eq!(log.path(), tmp.path().join("divergences.jsonl"));

        let base = DivergenceRecord {
            ssh_session_id: 7,
            request_index: 3,
            drv_path: DRV.to_string(),
            verdict: Verdict::Regression {
                error: "boom".to_string(),
                attempts: 1,
            },
            recorded_status: Some(0),
            recorded_status_msg: None,
            attempts: 1,
            dispatch_lateness_ms: 250,
            sibling_drvs: vec![SIBLING_DRV.to_string()],
            note: None,
        };
        log.write(&base).unwrap();
        log.write(&DivergenceRecord {
            verdict: Verdict::NonReproducible {
                recorded: hash_map(&[("out", HASH_A)]),
                replayed: hash_map(&[("out", HASH_B)]),
            },
            drv_path: SIBLING_DRV.to_string(),
            sibling_drvs: Vec::new(),
            ..base
        })
        .unwrap();

        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["verdict"]["verdict"], "regression");
        assert_eq!(first["drv_path"], DRV);
        assert_eq!(second["verdict"]["verdict"], "non_reproducible");
        assert_eq!(second["verdict"]["recorded"]["out"], HASH_A);
        assert_eq!(second["verdict"]["replayed"]["out"], HASH_B);
    }
}

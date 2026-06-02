//! Batch submission to the rio gateway.
//!
//! [`Submitter`] is the trait the submit loop (and the timed dispatcher)
//! drives, so submission stays unit-testable against a scripted in-memory
//! fake. [`ClientOpsSubmitter`] is the production implementation: it drives
//! the gateway's nix-daemon worker protocol in-process over the SSH channel
//! pool — per batch it imports the batch's drv closure from the replay
//! archive, then issues one `BuildPathsWithResults` call whose per-root
//! results become the batch outcome; relayed stderr lines are captured as
//! evidence only (build id, failure reasons, tail). The engine spawns no
//! child processes for submission.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use rio_nix::protocol::client::{KeyedBuildResult, StoreEntry};

use super::batch::Batch;
use super::drv_import::DrvArchive;
use super::model::{PathOutcome, build_status_name};
use super::stderrparse::{ParsedStderr, parse_line};
use super::transport::{DaemonChannel, GatewayPool, TransportError};

/// Result of one batch submission attempt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatchOutcome {
    /// Build id parsed from the gateway's `rio: build <uuid>` line.
    pub build_id: Option<String>,
    /// In-band per-root results (one entry per requested root); empty for
    /// submitters that have none.
    pub results: Vec<PathOutcome>,
    /// drv path → relayed failure reason, captured live from stderr.
    pub reasons: BTreeMap<String, String>,
    /// Last ~200 stderr lines, kept verbatim as raw evidence for
    /// batches.jsonl.
    pub stderr_tail: String,
    /// True when the engine itself cancelled the submission (batch deadline,
    /// abort, or an evidence-stream failure that cut the capture short)
    /// rather than the submission settling on its own. A cancelled outcome's
    /// captured evidence may be incomplete and its `results` are empty —
    /// collect re-offers the members instead of charging them.
    pub engine_cancelled: bool,
}

/// One batch-submission backend. The submit loop only ever talks to this
/// trait; unit tests script it with an in-memory fake while a real campaign
/// uses [`ClientOpsSubmitter`].
#[async_trait]
pub trait Submitter: Send + Sync {
    /// Submit one batch under the given store URL and wait for it to settle
    /// (or for `timeout` to cancel it).
    async fn submit_batch(
        &self,
        store_url: &str,
        batch: &Batch,
        timeout: Duration,
    ) -> Result<BatchOutcome>;
}

/// How many trailing stderr lines to keep as raw evidence.
const STDERR_TAIL_LINES: usize = 200;

/// Append one line to the capped evidence tail, dropping the oldest line
/// once [`STDERR_TAIL_LINES`] is reached.
fn push_tail(tail: &mut VecDeque<String>, line: String) {
    if tail.len() == STDERR_TAIL_LINES {
        tail.pop_front();
    }
    tail.push_back(line);
}

/// Feed one observed stderr payload through the gateway-line parser (build
/// id, relayed per-derivation failure reasons) and append it to the capped
/// evidence tail.
///
/// The transport is message-oriented while the parser and the tail are
/// line-oriented: the gateway packs a non-cascaded failure reason and its
/// `↳ rio-cli logs` hint into ONE `STDERR_NEXT` payload separated by a
/// newline (`rio-gateway/src/handler/build.rs`), so every payload is split
/// into lines here, at the observer boundary — a multi-line payload
/// structurally cannot reach the line parser, and the evidence tail stays
/// one line per entry.
fn observe_line(parsed: &mut ParsedStderr, tail: &mut VecDeque<String>, payload: &str) {
    for line in payload.split('\n') {
        parse_line(parsed, line);
        push_tail(tail, line.to_string());
    }
}

/// Map the daemon's per-root keyed results onto the batch's roots
/// positionally: the daemon answers in submission order, so entry *i*
/// belongs to `root_drvs[i]`. The recorded `drv_path` is always the bare
/// root drv path (what collect indexes by), never the echoed `DerivedPath`
/// string, which carries the output selector (`…!*`). A short result vector
/// (fewer entries than roots) maps what it can — the uncovered roots are
/// handled by collect's missing-result rule — and extra entries are ignored.
fn path_outcomes_from_keyed(
    root_drvs: &[String],
    results: &[KeyedBuildResult],
) -> Vec<PathOutcome> {
    root_drvs
        .iter()
        .zip(results)
        .map(|(drv, keyed)| PathOutcome {
            drv_path: drv.clone(),
            status: build_status_name(keyed.result.status).to_string(),
            error_msg: keyed.result.error_msg.clone(),
            start_time: keyed.result.start_time,
            stop_time: keyed.result.stop_time,
        })
        .collect()
}

/// `AddMultipleToStore` entries per upload call during the drv-text import.
/// Each entry's NAR payload is materialized in memory
/// ([`DrvArchive::entry`] returns `NarPayload::Bytes`), so this cap also
/// bounds the upload's resident memory; derivation texts are tiny, so 500
/// of them stay well under a megabyte. Aligned with the supply planner's
/// default batch-entry cap (`upload_batch_max_entries`).
const DRV_UPLOAD_CHUNK: usize = 500;

/// Effective-throughput floor used to scale upload deadlines with payload
/// size, so a large `AddMultipleToStore` chunk is never cut off by a
/// deadline tuned for metadata-sized ops. Deliberately conservative
/// (1 MiB/s); drv-text chunks add nothing measurable on top of the base
/// deadline.
const UPLOAD_FLOOR_BYTES_PER_SEC: u64 = 1024 * 1024;

/// Deadline for one upload call: the configured per-op deadline plus
/// payload-proportional headroom at [`UPLOAD_FLOOR_BYTES_PER_SEC`].
fn upload_deadline(base: Duration, payload_bytes: u64) -> Duration {
    base + Duration::from_secs(payload_bytes / UPLOAD_FLOOR_BYTES_PER_SEC)
}

/// Submitter that drives the gateway's worker protocol directly: per batch
/// it imports the batch's drv closure from the replay archive
/// (a `QueryValidPaths` probe + `AddMultipleToStore` of the missing texts in
/// reference order), then issues one `BuildPathsWithResults` call whose
/// per-root results become the batch outcome. Relayed stderr lines are
/// captured as evidence only (build id, failure reasons, tail).
pub struct ClientOpsSubmitter {
    /// SSH channel pool to the gateway; one channel is held per in-flight
    /// submission.
    pub pool: Arc<GatewayPool>,
    /// Open replay archive the drv texts are imported from.
    pub archive: Arc<DrvArchive>,
    /// Per-op deadline for the probe and upload calls
    /// (`knobs.op_timeout_secs`); the build call uses the batch timeout.
    pub op_timeout: Duration,
    /// Store paths per `QueryValidPaths` probe call (`knobs.probe_chunk`).
    pub probe_chunk: usize,
}

impl ClientOpsSubmitter {
    /// Bounded engine-side error for an import-phase transport failure,
    /// naming the op and the pool connection it ran on. The submit loop
    /// records the message on the batch record and re-offers the jobs.
    fn import_error(op: &str, connection: usize, err: &TransportError) -> anyhow::Error {
        anyhow!(
            "drv import failed during {op} on gateway connection {connection}: {}",
            crate::body_snippet(&err.to_string())
        )
    }

    /// Materialize one slice of archive paths as upload entries plus their
    /// total NAR payload size (which scales the upload deadline).
    fn materialize_entries(&self, paths: &[String]) -> Result<(Vec<StoreEntry>, u64)> {
        let entries = paths
            .iter()
            .map(|path| self.archive.entry(path))
            .collect::<Result<Vec<_>>>()?;
        let payload_bytes = entries.iter().map(|entry| entry.info.nar_size).sum();
        Ok((entries, payload_bytes))
    }

    /// Upload one slice of missing drv texts on `chan`, returning the channel
    /// to keep using afterwards.
    ///
    /// A clean daemon refusal is retried exactly once on a fresh channel: an
    /// upload refusal can be a quota/policy answer, but it can also be the
    /// refusal racing session teardown (the wire error surfaces as `Refused`
    /// for upload ops), and either way the refused channel's wire position is
    /// unknown, so it is dropped. A second failure — or any timeout/transport
    /// error — is an engine-side submission failure (infrastructure, never
    /// charged to the workload): the jobs are re-offered by the submit loop.
    async fn upload_chunk(
        &self,
        mut chan: DaemonChannel,
        paths: &[String],
    ) -> Result<DaemonChannel> {
        let (entries, payload_bytes) = self.materialize_entries(paths)?;
        let op = format!("AddMultipleToStore ({} drv texts)", paths.len());
        let deadline = upload_deadline(self.op_timeout, payload_bytes);
        match chan.add_multiple_to_store(entries, deadline).await {
            Ok(()) => Ok(chan),
            Err(TransportError::Refused(msg)) => {
                tracing::warn!(
                    connection = chan.connection_index(),
                    error = %msg,
                    "drv upload refused; retrying once on a fresh gateway channel"
                );
                drop(chan);
                let mut fresh = self
                    .pool
                    .open_channel()
                    .await
                    .context("open a fresh gateway channel after a refused drv upload")?;
                let (entries, _) = self.materialize_entries(paths)?;
                match fresh.add_multiple_to_store(entries, deadline).await {
                    Ok(()) => Ok(fresh),
                    Err(err) => Err(Self::import_error(
                        &format!("{op} (retry on a fresh channel)"),
                        fresh.connection_index(),
                        &err,
                    )),
                }
            }
            Err(err) => Err(Self::import_error(&op, chan.connection_index(), &err)),
        }
    }
}

#[async_trait]
impl Submitter for ClientOpsSubmitter {
    async fn submit_batch(
        &self,
        _store_url: &str,
        batch: &Batch,
        timeout: Duration,
    ) -> Result<BatchOutcome> {
        // One submission = one daemon channel: the protocol is sequential per
        // channel, and any error that desyncs the wire means switching to a
        // fresh channel, so the import and the build share this one until an
        // error forces a replacement. A failed channel open is an engine-side
        // submission failure (the submit loop records it and re-offers the
        // jobs), exactly like a failed ssh handshake before the cutover.
        let mut chan = self
            .pool
            .open_channel()
            .await
            .context("open a gateway daemon channel for the batch submission")?;

        // ── Import: the batch's drv closure from the replay archive ────────
        let closure = self.archive.closure(&batch.root_drvs)?;
        let mut valid: BTreeSet<String> = BTreeSet::new();
        for chunk in closure.chunks(self.probe_chunk.max(1)) {
            match chan.query_valid_paths(chunk, self.op_timeout).await {
                Ok(present) => valid.extend(present),
                Err(err) => {
                    return Err(Self::import_error(
                        &format!("QueryValidPaths ({} paths)", chunk.len()),
                        chan.connection_index(),
                        &err,
                    ));
                }
            }
        }
        // The probe filter and the upload slicing both preserve the closure's
        // reference order — the archive's only ordering guarantee — so every
        // reference is registered before its referrers.
        let missing: Vec<String> = closure
            .iter()
            .filter(|path| !valid.contains(*path))
            .cloned()
            .collect();
        for chunk in missing.chunks(DRV_UPLOAD_CHUNK) {
            chan = self.upload_chunk(chan, chunk).await?;
        }

        // ── Build: one BuildPathsWithResults call over every root ──────────
        let derived: Vec<String> = batch.root_drvs.iter().map(|d| format!("{d}!*")).collect();
        let mut parsed = ParsedStderr::default();
        let mut tail: VecDeque<String> = VecDeque::new();
        let build_result = {
            let mut observer = |line: &str| observe_line(&mut parsed, &mut tail, line);
            chan.build_paths_with_results_observed(&derived, timeout, &mut observer)
                .await
        };
        let (results, engine_cancelled) = match build_result {
            Ok(keyed) => {
                if keyed.len() != batch.root_drvs.len() {
                    tracing::warn!(
                        connection = chan.connection_index(),
                        requested = batch.root_drvs.len(),
                        returned = keyed.len(),
                        "BuildPathsWithResults returned a different result count than requested \
                         roots; uncovered roots are handled by collect's missing-result rule"
                    );
                }
                (path_outcomes_from_keyed(&batch.root_drvs, &keyed), false)
            }
            Err(TransportError::Timeout { .. }) => {
                // The batch deadline fired mid-build. Abandoning the channel
                // IS the cancellation mechanism (the gateway cancels the
                // session's builds when the channel closes); the evidence
                // captured so far is kept and collect re-offers the members
                // via the engine-cancelled rule.
                tracing::warn!(
                    connection = chan.connection_index(),
                    roots = batch.root_drvs.len(),
                    timeout_secs = timeout.as_secs(),
                    "batch build deadline reached; abandoning the gateway channel"
                );
                chan.abandon();
                (Vec::new(), true)
            }
            Err(err @ (TransportError::Refused(_) | TransportError::Other(_))) => {
                // Daemon refusal (e.g. quota) or a transport/protocol failure:
                // an engine-side submission failure, recorded on the batch
                // record and re-offered — never charged to the workload.
                return Err(anyhow!(
                    "BuildPathsWithResults failed on gateway connection {}: {}",
                    chan.connection_index(),
                    crate::body_snippet(&err.to_string())
                ));
            }
        };

        Ok(BatchOutcome {
            build_id: parsed.build_id,
            results,
            reasons: parsed.reasons,
            stderr_tail: Vec::from(tail).join("\n"),
            engine_cancelled,
        })
    }
}

/// The per-job repro command recorded alongside each job result: the
/// engine-native single-unit re-run (`cargo xtask replay repro`), which
/// resolves the campaign's pinned archive and replays exactly the named
/// derivation over the same transport and supply policy the campaign used.
/// It needs no local archive copy, no local Nix store, and embeds no store
/// URL — so no secret (such as an `ssh-key=` query parameter) can ever land
/// in campaign artifacts through this field.
pub fn repro_command(campaign_id: &str, drv_path: &str) -> String {
    format!("cargo xtask replay repro {campaign_id} {drv_path}")
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Scripted [`Submitter`] for stage-level tests: pops pre-programmed
    /// results and records every submitted batch.
    #[derive(Default)]
    pub struct FakeSubmitter {
        /// Scripted results, popped from the BACK, so when scripting several
        /// batches push the LAST batch's result first. `Err` entries script
        /// engine-side submission failures (spawn/import/ssh errors). An
        /// exhausted script yields `Ok(BatchOutcome::default())`.
        pub outcomes: Mutex<Vec<Result<BatchOutcome>>>,
        /// `(store_url, batch, timeout)` of every `submit_batch` call, in
        /// call order.
        pub submitted: Mutex<Vec<(String, Batch, Duration)>>,
    }

    #[async_trait]
    impl Submitter for FakeSubmitter {
        async fn submit_batch(
            &self,
            store_url: &str,
            batch: &Batch,
            timeout: Duration,
        ) -> Result<BatchOutcome> {
            self.submitted
                .lock()
                .unwrap()
                .push((store_url.to_string(), batch.clone(), timeout));
            self.outcomes
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| Ok(BatchOutcome::default()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::protocol::build::{BuildResult, BuildStatus};

    fn batch() -> Batch {
        Batch {
            jobs: vec!["libfoo.x86_64-linux".into(), "app.x86_64-linux".into()],
            root_drvs: vec![
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv".into(),
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv".into(),
            ],
            est_nodes: 17,
        }
    }

    #[test]
    fn repro_command_is_the_engine_native_invocation() {
        // The recorded repro is the operator-CLI single-unit re-run, keyed by
        // campaign id + drv path; it never embeds the store URL, so secrets
        // (ssh-key query parameters) cannot leak into campaign artifacts.
        let r = repro_command(
            "replay-leaf-20260601-ab12",
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv",
        );
        assert_eq!(
            r,
            "cargo xtask replay repro replay-leaf-20260601-ab12 \
             /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv"
        );
        assert!(!r.contains("ssh-key"), "{r}");
        assert!(!r.contains("ssh-ng://"), "{r}");
    }

    /// Pin the [`test_support::FakeSubmitter`] scripting contract that the
    /// stage-level tests rely on: results pop from the BACK (push the last
    /// batch's result first), `Err` entries script engine-side submission
    /// failures, an exhausted script yields a default outcome, and every
    /// submission is recorded in call order together with the timeout it
    /// was given.
    #[tokio::test]
    async fn fake_submitter_pops_outcomes_from_the_back() {
        use super::test_support::FakeSubmitter;
        let fake = FakeSubmitter::default();
        fake.outcomes
            .lock()
            .unwrap()
            .push(Err(anyhow::anyhow!("scripted submission failure")));
        for id in ["second", "first"] {
            fake.outcomes.lock().unwrap().push(Ok(BatchOutcome {
                build_id: Some(id.to_string()),
                ..BatchOutcome::default()
            }));
        }
        let b = batch();
        let timeout = Duration::from_secs(1);
        let first = fake.submit_batch("ssh-ng://x", &b, timeout).await.unwrap();
        let second = fake.submit_batch("ssh-ng://x", &b, timeout).await.unwrap();
        let err = fake
            .submit_batch("ssh-ng://x", &b, timeout)
            .await
            .unwrap_err();
        let drained = fake.submit_batch("ssh-ng://x", &b, timeout).await.unwrap();
        assert_eq!(first.build_id.as_deref(), Some("first"));
        assert_eq!(second.build_id.as_deref(), Some("second"));
        assert!(err.to_string().contains("scripted submission failure"));
        assert_eq!(drained, BatchOutcome::default());
        let submitted = fake.submitted.lock().unwrap();
        assert_eq!(submitted.len(), 4);
        assert_eq!(submitted[0].0, "ssh-ng://x");
        assert_eq!(submitted[0].1, b);
        assert!(submitted.iter().all(|(_, _, t)| *t == timeout));
    }

    /// The positional mapping from the daemon's keyed results to in-band
    /// per-root outcomes: keyed by the bare ROOT drv path in submission
    /// order (never the echoed `DerivedPath` string, which carries the
    /// output selector), statuses written via `build_status_name`, error
    /// message and timestamps carried over, and a short result vector maps
    /// what it can without erroring.
    #[test]
    fn client_ops_outcome_maps_keyed_results_positionally() {
        let roots = batch().root_drvs;
        let keyed = vec![
            KeyedBuildResult {
                derived_path: format!("{}!*", roots[0]),
                result: BuildResult {
                    status: BuildStatus::Built,
                    start_time: 100,
                    stop_time: 200,
                    ..BuildResult::default()
                },
            },
            KeyedBuildResult {
                derived_path: format!("{}!*", roots[1]),
                result: BuildResult {
                    status: BuildStatus::PermanentFailure,
                    error_msg: "builder failed with exit code 2".into(),
                    start_time: 300,
                    stop_time: 400,
                    ..BuildResult::default()
                },
            },
        ];
        let outcomes = path_outcomes_from_keyed(&roots, &keyed);
        assert_eq!(outcomes.len(), 2);
        // The recorded drv path is the plain root drv path collect indexes
        // by — no `!*` / `^*` output-selector suffix from the echoed key.
        assert_eq!(outcomes[0].drv_path, roots[0]);
        assert_eq!(outcomes[1].drv_path, roots[1]);
        assert!(
            outcomes.iter().all(|o| o.drv_path.ends_with(".drv")),
            "{outcomes:?}"
        );
        assert_eq!(outcomes[0].status, build_status_name(BuildStatus::Built));
        assert_eq!(outcomes[0].error_msg, "");
        assert_eq!((outcomes[0].start_time, outcomes[0].stop_time), (100, 200));
        assert_eq!(
            outcomes[1].status,
            build_status_name(BuildStatus::PermanentFailure)
        );
        assert_eq!(outcomes[1].error_msg, "builder failed with exit code 2");
        assert_eq!((outcomes[1].start_time, outcomes[1].stop_time), (300, 400));

        // A short result vector (one entry for two roots) is not an error:
        // the uncovered root simply has no outcome.
        let short = path_outcomes_from_keyed(&roots, &keyed[..1]);
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].drv_path, roots[0]);
    }

    /// The client-ops stderr observer captures the same evidence the warm
    /// shell-out's stderr reader does: the gateway's build id, the relayed
    /// per-derivation failure reasons, and a tail capped at
    /// `STDERR_TAIL_LINES`.
    #[test]
    fn client_ops_observer_captures_build_id_reasons_and_tail() {
        let mut parsed = ParsedStderr::default();
        let mut tail: VecDeque<String> = VecDeque::new();
        for i in 0..250 {
            observe_line(&mut parsed, &mut tail, &format!("noise line {i}"));
        }
        observe_line(
            &mut parsed,
            &mut tail,
            "rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a (trace 4bf92f3577b34da6a3ce929d0e0e4736)",
        );
        observe_line(
            &mut parsed,
            &mut tail,
            "derivation '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv' failed: poison threshold reached after 3 distinct-worker failures",
        );
        assert_eq!(
            parsed.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        assert_eq!(parsed.reasons.len(), 1);
        assert_eq!(
            parsed.reasons["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv"],
            "poison threshold reached after 3 distinct-worker failures"
        );
        // The evidence tail is capped: 252 lines fed, the first 52 noise
        // lines dropped.
        assert_eq!(tail.len(), STDERR_TAIL_LINES);
        assert_eq!(tail.front().unwrap(), "noise line 52");
        assert!(tail.back().unwrap().starts_with("derivation '"));
    }

    /// Generate the gateway's per-derivation failure relay exactly as
    /// `rio-gateway/src/handler/build.rs` formats it (one `STDERR_NEXT`
    /// payload per scheduler terminal failure): for a derivation that
    /// actually executed (non-cascaded), the copy-pasteable
    /// `↳ rio-cli logs '<drv>'` hint is appended after a newline INSIDE
    /// the same payload; a cascaded `DependencyFailed` relay suppresses
    /// the hint and stays single-line. Conformance mirror of the
    /// gateway's format strings — when the gateway's relay format
    /// changes, update this generator in the same change.
    fn gateway_failure_relay(drv: &str, reason: &str, cascaded: bool) -> String {
        let hint = if cascaded {
            String::new()
        } else {
            format!("\n  ↳ rio-cli logs '{drv}'")
        };
        format!("derivation '{drv}' failed: {reason}{hint}")
    }

    /// The gateway packs a non-cascaded failure reason and its `rio-cli
    /// logs` hint into ONE multi-line `STDERR_NEXT` payload. The observer
    /// must split every payload into lines before parsing — otherwise the
    /// trigger derivation's relayed reason (collect's only reason signal
    /// for a failed dependency) is silently dropped and the evidence tail
    /// stops being line-shaped.
    #[test]
    fn client_ops_observer_splits_multi_line_failure_relays() {
        let trigger = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv";
        let dependent = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv";
        let reason = "builder failed with exit code 2";

        let mut parsed = ParsedStderr::default();
        let mut tail: VecDeque<String> = VecDeque::new();

        // The trigger drv executed and failed: its relay embeds the hint,
        // so the payload is genuinely multi-line.
        let relay = gateway_failure_relay(trigger, reason, false);
        assert!(relay.contains('\n'), "premise: {relay:?}");
        observe_line(&mut parsed, &mut tail, &relay);

        // The dependent cascades: hint suppressed, single-line payload.
        let cascade_reason = format!("dependency '{trigger}' failed: {reason}");
        let cascade = gateway_failure_relay(dependent, &cascade_reason, true);
        assert!(!cascade.contains('\n'), "premise: {cascade:?}");
        observe_line(&mut parsed, &mut tail, &cascade);

        // Both reasons captured — including the trigger's, whose payload
        // carries the embedded hint.
        assert_eq!(
            parsed.reasons.get(trigger).map(String::as_str),
            Some(reason)
        );
        assert_eq!(
            parsed.reasons.get(dependent).map(String::as_str),
            Some(cascade_reason.as_str())
        );

        // The evidence tail is line-shaped: one entry per line, the hint
        // on its own line, no entry spanning a newline.
        assert_eq!(
            Vec::from(tail),
            vec![
                format!("derivation '{trigger}' failed: {reason}"),
                format!("  ↳ rio-cli logs '{trigger}'"),
                format!("derivation '{dependent}' failed: {cascade_reason}"),
            ]
        );
    }

    /// Upload deadlines scale with the chunk's payload size on top of the
    /// configured per-op deadline, at the conservative 1 MiB/s floor.
    #[test]
    fn upload_deadline_scales_with_payload() {
        let base = Duration::from_secs(120);
        assert_eq!(upload_deadline(base, 0), base);
        // A drv-text-sized chunk adds nothing measurable.
        assert_eq!(upload_deadline(base, 512 * 1024), base);
        // A 100 MiB payload adds 100 seconds of headroom.
        assert_eq!(
            upload_deadline(base, 100 * 1024 * 1024),
            base + Duration::from_secs(100)
        );
    }
}

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
use super::model::{PathOutcome, path_outcomes_from_keyed};
use super::stderrparse::{ParsedStderr, parse_line};
use super::transport::{DaemonChannel, GatewayPool, TransportError};

/// The deadline governing one batch submission, typed by the logical clock
/// it encodes and carried as an absolute instant.
///
/// Absolute instants make "time elapses between computing a relative
/// timeout and consuming it" unrepresentable: however long the channel
/// open, drv-closure import, and uploads take inside a submitter, the build
/// op's budget is recomputed from the same fixed instant at the final await
/// ([`remaining_from`](Self::remaining_from)), so the effective deadline
/// never stretches. The variant names WHICH logical deadline this is, so
/// the submission chokepoint
/// ([`submit_one_batch`](super::submit::submit_one_batch)) can record which
/// one fired — an engine build-budget cut and a replayed recorded
/// disconnect must never be conflated downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchDeadline {
    /// The engine's own build budget for this submission, anchored at the
    /// moment the submission was committed (the timed dispatcher anchors at
    /// admission; the timeless loop at batch start).
    Build(tokio::time::Instant),
    /// A replayed recorded interruption: the channel must be abandoned at
    /// this instant (admission plus the recorded disconnect gap).
    DisconnectReplay(tokio::time::Instant),
}

impl BatchDeadline {
    /// Smallest budget the build op is ever given: a deadline that expired
    /// while the import phase ran still issues the build — reproducing a
    /// recorded interruption requires the build to actually start — and the
    /// timeout then fires through the normal path. Also the floor on the
    /// scheduled disconnect delay itself, so a tiny recorded gap or a high
    /// speedup cannot turn the replay into a no-op.
    pub const MIN_BUILD_BUDGET: Duration = Duration::from_secs(1);

    /// The absolute instant this deadline fires at.
    pub fn instant(&self) -> tokio::time::Instant {
        match self {
            Self::Build(at) | Self::DisconnectReplay(at) => *at,
        }
    }

    /// Remaining budget from `now`: saturating, never below
    /// [`MIN_BUILD_BUDGET`](Self::MIN_BUILD_BUDGET). Submitters call this
    /// immediately before the build op — never earlier — so import time
    /// cannot leak into the effective deadline.
    pub fn remaining_from(&self, now: tokio::time::Instant) -> Duration {
        self.instant()
            .saturating_duration_since(now)
            .max(Self::MIN_BUILD_BUDGET)
    }

    /// True when this deadline replays a recorded interruption.
    pub fn is_disconnect_replay(&self) -> bool {
        matches!(self, Self::DisconnectReplay(_))
    }
}

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
    /// Roots whose lost-terminal relay marker was captured live from
    /// stderr (see [`ParsedStderr::lost_terminals`]): their `Substituted`
    /// rows stand on a lost evidence channel, not a recorded substitution
    /// event. Empty for submitters that observe no stderr.
    pub lost_terminals: BTreeSet<String>,
    /// Last ~200 stderr lines, kept verbatim as raw evidence for
    /// batches.jsonl.
    pub stderr_tail: String,
    /// True when the [`BatchDeadline`] handed to
    /// [`Submitter::submit_batch`] cancelled the submission, rather than
    /// the submission settling on its own. A cancelled outcome's captured
    /// evidence may be incomplete and its `results` are empty — collect
    /// re-offers the members instead of charging them.
    ///
    /// CONTRACT: the handed deadline is the ONLY cause an impl may set
    /// this bit for. `submit_one_batch` derives the fidelity-critical
    /// `disconnect_deadline_fired` as `engine_cancelled` AND
    /// "the deadline was a [`BatchDeadline::DisconnectReplay`]" — an impl
    /// that sets the bit for any other cause (an abort, a failed evidence
    /// stream) under a disconnect-replay deadline would fabricate
    /// "interruption replayed" fidelity records for interruptions that
    /// never replayed. Other failures are an `Err` from
    /// [`Submitter::submit_batch`]: recorded as engine-side submission
    /// failures and re-offered, never classified against the deadline.
    pub engine_cancelled: bool,
    /// Interior input derivations the import walk reached but the archive
    /// does not embed (a non-conforming archive's import-gap set,
    /// sorted): recorded on the batch record as the operator-facing
    /// union view. Empty for submitters that import nothing.
    pub import_skipped_drvs: Vec<String>,
    /// Per-root attribution of `import_skipped_drvs` (root drv → the
    /// gaps ITS text closure reaches): the form collect consumes — a
    /// failed root retires as supply-failed exactly when its own
    /// closure carried a gap, so the failure is attributed to the
    /// archive instead of read as a unit regression, and an unrelated
    /// batch-mate's genuine failure is never excused.
    pub import_skipped_by_root: BTreeMap<String, Vec<String>>,
}

/// One batch-submission backend. The submit loop only ever talks to this
/// trait; unit tests script it with an in-memory fake while a real campaign
/// uses [`ClientOpsSubmitter`].
#[async_trait]
pub trait Submitter: Send + Sync {
    /// Submit one batch under the given store URL and wait for it to settle
    /// (or for `deadline` to cancel it).
    async fn submit_batch(
        &self,
        store_url: &str,
        batch: &Batch,
        deadline: BatchDeadline,
    ) -> Result<BatchOutcome>;
}

/// How many trailing stderr lines to keep as raw evidence.
const STDERR_TAIL_LINES: usize = 200;

/// Append one line to the capped evidence tail, dropping the oldest line
/// once [`STDERR_TAIL_LINES`] is reached.
///
/// Tail entries are LINES by contract — the cap budgets lines, and
/// batches.jsonl evidence is rendered one entry per line — so an entry
/// still carrying a newline means a caller skipped the observer-boundary
/// split.
fn push_tail(tail: &mut VecDeque<String>, line: String) {
    debug_assert!(
        !line.contains('\n'),
        "evidence-tail entries are single lines; split payloads before pushing: {line:?}"
    );
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
/// line-oriented, and the gateway's payloads come in both line shapes
/// (`rio-gateway/src/handler/build.rs`): a non-cascaded failure reason and
/// its `↳ rio-cli logs` hint arrive as ONE payload separated by an interior
/// newline, while the unconditional `rio: build <uuid>` announcement and
/// the `SubmitBuild RPC failed` diagnostic arrive newline-TERMINATED. Every
/// payload is therefore split here, at the observer boundary, with
/// `str::lines`'s terminator semantics — the same split `parse_stderr`
/// uses on whole captures — so a multi-line payload structurally cannot
/// reach the line parser, a trailing terminator contributes no blank
/// residue entry, and the evidence tail stays one line per entry.
fn observe_line(parsed: &mut ParsedStderr, tail: &mut VecDeque<String>, payload: &str) {
    for line in payload.lines() {
        parse_line(parsed, line);
        push_tail(tail, line.to_string());
    }
}

/// `AddMultipleToStore` entries per upload call during the drv-text import.
/// Each entry's NAR payload is materialized in memory
/// ([`DrvArchive::entry`] returns `NarPayload::Bytes`), so this cap also
/// bounds the upload's resident memory; derivation texts are tiny, so 500
/// of them stay well under a megabyte. Aligned with the supply planner's
/// default batch-entry cap (`upload_batch_max_entries`).
const DRV_UPLOAD_CHUNK: usize = 500;

/// The slice of the gateway daemon-channel surface one batch submission
/// drives, behind a seam so the submission chokepoint is testable without
/// SSH (the submit-side sibling of the supply stage's `SupplyTransport`
/// seam). Production is [`DaemonChannel`]; tests script it to pin the
/// values the chokepoint actually hands the wire layer — most importantly
/// the workload estimate that keys the build op's stderr drain budget,
/// which no scripted-[`Submitter`] test can observe.
#[async_trait]
pub trait SubmitChannel: Send {
    /// Index of the underlying pool connection (for triage logs).
    fn connection_index(&self) -> usize;
    /// `wopQueryValidPaths` (no daemon-side substitution): which of `paths`
    /// the target already has.
    async fn query_valid_paths(
        &mut self,
        paths: &[String],
        timeout: Duration,
    ) -> std::result::Result<BTreeSet<String>, TransportError>;
    /// `wopAddMultipleToStore` of drv-text upload entries.
    async fn add_multiple_to_store(
        &mut self,
        entries: Vec<StoreEntry>,
        base_timeout: Duration,
    ) -> std::result::Result<(), TransportError>;
    /// `wopBuildPathsWithResults` with a relayed-stderr observer.
    /// `closure_nodes` is the workload estimate that keys the op's stderr
    /// drain budget (see `DaemonChannel::build_paths_with_results`).
    async fn build_paths_with_results_observed(
        &mut self,
        derived: &[String],
        timeout: Duration,
        closure_nodes: usize,
        observer: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> std::result::Result<Vec<KeyedBuildResult>, TransportError>;
    /// Abandon the channel without waiting for protocol teardown (the
    /// gateway cancels the session's in-flight builds).
    fn abandon(self: Box<Self>);
}

#[async_trait]
impl SubmitChannel for DaemonChannel {
    fn connection_index(&self) -> usize {
        DaemonChannel::connection_index(self)
    }
    async fn query_valid_paths(
        &mut self,
        paths: &[String],
        timeout: Duration,
    ) -> std::result::Result<BTreeSet<String>, TransportError> {
        DaemonChannel::query_valid_paths(self, paths, timeout).await
    }
    async fn add_multiple_to_store(
        &mut self,
        entries: Vec<StoreEntry>,
        base_timeout: Duration,
    ) -> std::result::Result<(), TransportError> {
        DaemonChannel::add_multiple_to_store(self, entries, base_timeout).await
    }
    async fn build_paths_with_results_observed(
        &mut self,
        derived: &[String],
        timeout: Duration,
        closure_nodes: usize,
        observer: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> std::result::Result<Vec<KeyedBuildResult>, TransportError> {
        DaemonChannel::build_paths_with_results_observed(
            self,
            derived,
            timeout,
            closure_nodes,
            observer,
        )
        .await
    }
    fn abandon(self: Box<Self>) {
        DaemonChannel::abandon(*self)
    }
}

/// Source of [`SubmitChannel`]s for batch submissions. Production is
/// [`GatewayPool`] (one SSH-exec'd `nix-daemon --stdio` session per
/// channel); tests provide scripted channels.
#[async_trait]
pub trait SubmitChannelSource: Send + Sync {
    /// Open one daemon session ready for ops (handshake done).
    async fn open_channel(&self) -> Result<Box<dyn SubmitChannel>>;
}

#[async_trait]
impl SubmitChannelSource for GatewayPool {
    async fn open_channel(&self) -> Result<Box<dyn SubmitChannel>> {
        Ok(Box::new(GatewayPool::open_channel(self).await?))
    }
}

/// Submitter that drives the gateway's worker protocol directly: per batch
/// it imports the batch's drv closure from the replay archive
/// (a `QueryValidPaths` probe + `AddMultipleToStore` of the missing texts in
/// reference order), then issues one `BuildPathsWithResults` call whose
/// per-root results become the batch outcome. Relayed stderr lines are
/// captured as evidence only (build id, failure reasons, tail).
pub struct ClientOpsSubmitter {
    /// Daemon-channel source (the gateway SSH pool in production); one
    /// channel is held per in-flight submission.
    pub pool: Arc<dyn SubmitChannelSource>,
    /// Open replay archive the drv texts are imported from.
    pub archive: Arc<DrvArchive>,
    /// Per-op deadline for the probe and upload calls
    /// (`knobs.op_timeout_secs`); the build call's budget is whatever
    /// remains of the submission's [`BatchDeadline`] once the import phase
    /// is done.
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

    /// Materialize one slice of archive paths as upload entries.
    fn materialize_entries(&self, paths: &[String]) -> Result<Vec<StoreEntry>> {
        paths
            .iter()
            .map(|path| self.archive.entry(path))
            .collect::<Result<Vec<_>>>()
    }

    /// Upload one slice of missing drv texts on `chan`, returning the channel
    /// to keep using afterwards.
    ///
    /// A clean daemon refusal — or a mid-upload wire death
    /// ([`TransportError::MaybeRefused`]: possibly the refusal racing
    /// session teardown, possibly plain transport death) — is retried
    /// exactly once on a fresh channel; either way the first channel's wire
    /// position is unknown, so it is dropped. A second failure — or any
    /// timeout/transport error — is an engine-side submission failure
    /// (infrastructure, never charged to the workload): the jobs are
    /// re-offered by the submit loop. The two shapes need no separate
    /// settlement here, unlike the supply arms' (refusals there settle
    /// paths REFUSED, an irreversible daemon verdict): every exhausted
    /// outcome of this arm lands in the same engine-side bucket.
    async fn upload_chunk(
        &self,
        mut chan: Box<dyn SubmitChannel>,
        paths: &[String],
    ) -> Result<Box<dyn SubmitChannel>> {
        let entries = self.materialize_entries(paths)?;
        let op = format!("AddMultipleToStore ({} drv texts)", paths.len());
        // The base deadline is the metadata-scale per-op bound; the channel
        // derives payload-proportional headroom from the entries itself, so
        // every upload op — this drv-text arm and the supply arms alike —
        // shares one scaling rule at the transport seam.
        match chan.add_multiple_to_store(entries, self.op_timeout).await {
            Ok(()) => Ok(chan),
            Err(TransportError::Refused(msg) | TransportError::MaybeRefused(msg)) => {
                tracing::warn!(
                    connection = chan.connection_index(),
                    error = %msg,
                    "drv upload refused (or its wire died mid-upload); retrying once on a \
                     fresh gateway channel"
                );
                drop(chan);
                let mut fresh = self
                    .pool
                    .open_channel()
                    .await
                    .context("open a fresh gateway channel after a refused drv upload")?;
                let entries = self.materialize_entries(paths)?;
                match fresh.add_multiple_to_store(entries, self.op_timeout).await {
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
        deadline: BatchDeadline,
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
        // Workload basis for the build op's stderr drain budget: every
        // derivation the submitted DAG can REACH — the importable texts
        // (`order`) plus the gaps a non-conforming archive cannot offer
        // (`skipped`, which the target may still resolve from its own
        // store and build). Derived HERE, at the one chokepoint every
        // batch funnels through, never from the producer's
        // `Batch::est_nodes`: that field is the assembler's packing
        // estimate, and two producers (the timed dispatcher's initial and
        // confirmation-retry constructions) structurally lack adjacency
        // data — recorded request targets need not even be workload units
        // — so a producer-side key would under-budget legal one-root
        // deep-closure submissions down to the single-unit floor and trip
        // the drain belt mid-DAG (a wire error that cancels every
        // in-flight build in the batch). Target-side validity is
        // deliberately NOT subtracted: force-build campaigns rebuild
        // already-valid paths, and the estimate may only widen the belt
        // above its roots floor, never narrow it.
        let workload_nodes = closure.order.len() + closure.skipped.len();
        if !closure.skipped.is_empty() {
            // A conforming archive embeds the full requisite .drv closure
            // of every workload unit, so a non-empty skipped set means the
            // embedded ATerms reference members the archive does not carry
            // (plan-time membership checks cover the adjacency records,
            // not the ATerm texts). The batch is still submitted — the
            // target may have the paths — but a downstream failure of
            // these roots is now attributable to the archive gap instead
            // of silently charging the target.
            tracing::warn!(
                roots = batch.root_drvs.len(),
                skipped = closure.skipped.len(),
                paths = ?closure.skipped,
                "embedded ATerms reference derivations the archive does not embed; \
                 they cannot be imported from the archive"
            );
        }
        let mut valid: BTreeSet<String> = BTreeSet::new();
        for chunk in closure.order.chunks(self.probe_chunk.max(1)) {
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
            .order
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
        // The wire still wants a relative timeout: convert HERE, after the
        // import phase, so the time the probes and uploads took is charged
        // against the absolute deadline instead of silently extending it.
        let timeout = deadline.remaining_from(tokio::time::Instant::now());
        let build_result = {
            let mut observer = |line: &str| observe_line(&mut parsed, &mut tail, line);
            // `workload_nodes` is the realized import closure resolved
            // above — the workload estimate that keys the op's stderr
            // drain budget to the volume the DAG can healthily emit.
            chan.build_paths_with_results_observed(&derived, timeout, workload_nodes, &mut observer)
                .await
        };
        let (results, engine_cancelled) = match build_result {
            // The mapping checks the daemon's result count against the
            // submitted roots and warns on a mismatch; uncovered roots are
            // handled by collect's missing-result rule.
            Ok(keyed) => (path_outcomes_from_keyed(&batch.root_drvs, &keyed), false),
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
            Err(
                err @ (TransportError::Refused(_)
                | TransportError::MaybeRefused(_)
                | TransportError::Other(_)),
            ) => {
                // Daemon refusal (e.g. quota) or a transport/protocol failure
                // (`MaybeRefused` is unreachable here — it is minted only by
                // the upload ops — but it would belong in this bucket too):
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
            lost_terminals: parsed.lost_terminals,
            stderr_tail: Vec::from(tail).join("\n"),
            import_skipped_drvs: closure.skipped,
            import_skipped_by_root: closure.skipped_for,
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

    /// One recorded `BuildPathsWithResults` call a [`FakeChannelSource`]
    /// channel served: the submitted derived paths and — the value these
    /// fakes exist to observe — the `closure_nodes` workload estimate that
    /// keys the op's stderr drain budget.
    #[derive(Debug, Clone, PartialEq)]
    pub struct RecordedBuildCall {
        pub derived: Vec<String>,
        pub closure_nodes: usize,
    }

    /// Scripted [`SubmitChannelSource`] driving the REAL
    /// [`ClientOpsSubmitter`] without SSH: every channel it opens shares
    /// one log of probe/upload/build calls and one script of build
    /// results. Probes answer from `valid_paths`; uploads succeed and
    /// record their entry counts; build calls record `(derived,
    /// closure_nodes)` and pop one scripted result vector (an exhausted
    /// script reports every derived path Built). Tests keep their own
    /// `Arc` clone of the source to read the logs after the submitter ran.
    #[derive(Default)]
    pub struct FakeChannelSource {
        /// Paths every validity probe reports as already on the target.
        pub valid_paths: BTreeSet<String>,
        /// Scripted per-build-call results, popped from the FRONT (one
        /// entry per `BuildPathsWithResults` call, in call order).
        pub results: Arc<Mutex<VecDeque<Vec<KeyedBuildResult>>>>,
        /// Probed path slices, in call order.
        pub probes: Arc<Mutex<Vec<Vec<String>>>>,
        /// Upload entry counts, in call order.
        pub uploads: Arc<Mutex<Vec<usize>>>,
        /// Recorded build calls, in call order.
        pub build_calls: Arc<Mutex<Vec<RecordedBuildCall>>>,
    }

    /// One channel handed out by [`FakeChannelSource`]; shares its
    /// source's logs and script.
    pub struct FakeChannel {
        valid_paths: BTreeSet<String>,
        results: Arc<Mutex<VecDeque<Vec<KeyedBuildResult>>>>,
        probes: Arc<Mutex<Vec<Vec<String>>>>,
        uploads: Arc<Mutex<Vec<usize>>>,
        build_calls: Arc<Mutex<Vec<RecordedBuildCall>>>,
    }

    #[async_trait]
    impl SubmitChannelSource for FakeChannelSource {
        async fn open_channel(&self) -> Result<Box<dyn SubmitChannel>> {
            Ok(Box::new(FakeChannel {
                valid_paths: self.valid_paths.clone(),
                results: self.results.clone(),
                probes: self.probes.clone(),
                uploads: self.uploads.clone(),
                build_calls: self.build_calls.clone(),
            }))
        }
    }

    #[async_trait]
    impl SubmitChannel for FakeChannel {
        fn connection_index(&self) -> usize {
            0
        }
        async fn query_valid_paths(
            &mut self,
            paths: &[String],
            _timeout: Duration,
        ) -> std::result::Result<BTreeSet<String>, TransportError> {
            self.probes.lock().unwrap().push(paths.to_vec());
            Ok(paths
                .iter()
                .filter(|p| self.valid_paths.contains(*p))
                .cloned()
                .collect())
        }
        async fn add_multiple_to_store(
            &mut self,
            entries: Vec<StoreEntry>,
            _base_timeout: Duration,
        ) -> std::result::Result<(), TransportError> {
            self.uploads.lock().unwrap().push(entries.len());
            Ok(())
        }
        async fn build_paths_with_results_observed(
            &mut self,
            derived: &[String],
            _timeout: Duration,
            closure_nodes: usize,
            _observer: &mut (dyn for<'a> FnMut(&'a str) + Send),
        ) -> std::result::Result<Vec<KeyedBuildResult>, TransportError> {
            self.build_calls.lock().unwrap().push(RecordedBuildCall {
                derived: derived.to_vec(),
                closure_nodes,
            });
            let scripted = self.results.lock().unwrap().pop_front();
            Ok(scripted.unwrap_or_else(|| {
                derived
                    .iter()
                    .map(|path| KeyedBuildResult {
                        derived_path: path.clone(),
                        result: rio_nix::protocol::build::BuildResult {
                            status: rio_nix::protocol::build::BuildStatus::Built,
                            ..rio_nix::protocol::build::BuildResult::default()
                        },
                    })
                    .collect()
            }))
        }
        fn abandon(self: Box<Self>) {}
    }

    /// Scripted [`Submitter`] for stage-level tests: pops pre-programmed
    /// results and records every submitted batch.
    #[derive(Default)]
    pub struct FakeSubmitter {
        /// Scripted results, popped from the BACK, so when scripting several
        /// batches push the LAST batch's result first. `Err` entries script
        /// engine-side submission failures (spawn/import/ssh errors). An
        /// exhausted script yields `Ok(BatchOutcome::default())`.
        pub outcomes: Mutex<Vec<Result<BatchOutcome>>>,
        /// `(store_url, batch, deadline)` of every `submit_batch` call, in
        /// call order.
        pub submitted: Mutex<Vec<(String, Batch, BatchDeadline)>>,
    }

    #[async_trait]
    impl Submitter for FakeSubmitter {
        async fn submit_batch(
            &self,
            store_url: &str,
            batch: &Batch,
            deadline: BatchDeadline,
        ) -> Result<BatchOutcome> {
            self.submitted
                .lock()
                .unwrap()
                .push((store_url.to_string(), batch.clone(), deadline));
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
    use crate::run::model::build_status_name;
    use rio_nix::protocol::build::{BuildResult, BuildStatus};
    use rio_nix::protocol::client::KeyedBuildResult;

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
    /// submission is recorded in call order together with the deadline it
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
        let deadline = BatchDeadline::Build(tokio::time::Instant::now() + Duration::from_secs(1));
        let first = fake.submit_batch("ssh-ng://x", &b, deadline).await.unwrap();
        let second = fake.submit_batch("ssh-ng://x", &b, deadline).await.unwrap();
        let err = fake
            .submit_batch("ssh-ng://x", &b, deadline)
            .await
            .unwrap_err();
        let drained = fake.submit_batch("ssh-ng://x", &b, deadline).await.unwrap();
        assert_eq!(first.build_id.as_deref(), Some("first"));
        assert_eq!(second.build_id.as_deref(), Some("second"));
        assert!(err.to_string().contains("scripted submission failure"));
        assert_eq!(drained, BatchOutcome::default());
        let submitted = fake.submitted.lock().unwrap();
        assert_eq!(submitted.len(), 4);
        assert_eq!(submitted[0].0, "ssh-ng://x");
        assert_eq!(submitted[0].1, b);
        assert!(submitted.iter().all(|(_, _, d)| *d == deadline));
    }

    /// The typed deadline's wire conversion: the remaining budget is
    /// measured from the instant the BUILD op is issued (not when the
    /// deadline was computed), saturates instead of underflowing once the
    /// import phase has eaten past it, and never drops below the 1 s
    /// minimum build budget — the recorded interruption can only be
    /// reproduced if the build actually starts.
    #[tokio::test(start_paused = true)]
    async fn batch_deadline_remaining_is_anchored_and_floored() {
        let anchor = tokio::time::Instant::now();
        let deadline = BatchDeadline::DisconnectReplay(anchor + Duration::from_secs(30));
        assert!(deadline.is_disconnect_replay());
        assert_eq!(deadline.instant(), anchor + Duration::from_secs(30));
        // Converted at the anchor: the full budget.
        assert_eq!(deadline.remaining_from(anchor), Duration::from_secs(30));
        // 12s of import work later, the same deadline yields 18s — the
        // elapsed time is charged, not appended.
        tokio::time::advance(Duration::from_secs(12)).await;
        let now = tokio::time::Instant::now();
        assert_eq!(deadline.remaining_from(now), Duration::from_secs(18));
        // A deadline the import phase already blew past floors at the
        // minimum build budget instead of underflowing to zero.
        tokio::time::advance(Duration::from_secs(60)).await;
        let now = tokio::time::Instant::now();
        assert_eq!(
            deadline.remaining_from(now),
            BatchDeadline::MIN_BUILD_BUDGET
        );

        let build = BatchDeadline::Build(anchor + Duration::from_secs(5));
        assert!(!build.is_disconnect_replay());
        assert_eq!(build.remaining_from(now), BatchDeadline::MIN_BUILD_BUDGET);
    }

    /// The positional mapping from the daemon's keyed results to in-band
    /// per-root outcomes: keyed by the bare ROOT drv path in submission
    /// order (never the echoed `DerivedPath` string, which carries the
    /// output selector), statuses written via `build_status_name`, error
    /// message and timestamps carried over. A short result vector maps what
    /// it can without erroring — the shared mapping warns about the count
    /// mismatch and collect's missing-result rule covers the rest.
    #[test]
    #[tracing_test::traced_test]
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
        // A matched result count is the daemon honoring the contract — no
        // mismatch warning.
        assert!(!logs_contain("different result count"));
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
        // the uncovered root simply has no outcome, and the count mismatch
        // is logged so the degradation leaves a breadcrumb.
        let short = path_outcomes_from_keyed(&roots, &keyed[..1]);
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].drv_path, roots[0]);
        assert!(logs_contain(
            "BuildPathsWithResults returned a different result count than requested roots"
        ));
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
            &gateway_build_announcement(
                "0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a",
                "4bf92f3577b34da6a3ce929d0e0e4736",
            ),
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

    /// Generate the gateway's build announcement exactly as
    /// `rio-gateway/src/handler/build.rs` formats it (one `STDERR_NEXT`
    /// payload, emitted unconditionally per accepted submission): the
    /// ` (trace <hex>)` suffix appears only when a trace id exists, and
    /// the payload is newline-TERMINATED. Conformance mirror of the
    /// gateway's format string — when the gateway's announcement format
    /// changes, update this generator in the same change.
    fn gateway_build_announcement(build_id: &str, trace_id: &str) -> String {
        let trace_suffix = if trace_id.is_empty() {
            String::new()
        } else {
            format!(" (trace {trace_id})")
        };
        format!("rio: build {build_id}{trace_suffix}\n")
    }

    /// Generate the gateway's submit-failure diagnostic exactly as
    /// `rio-gateway/src/handler/build.rs` formats it (one `STDERR_NEXT`
    /// payload, newline-TERMINATED). Conformance mirror of the gateway's
    /// format string — when the gateway's diagnostic format changes,
    /// update this generator in the same change.
    fn gateway_submit_failed(err: &str) -> String {
        format!("SubmitBuild RPC failed: {err}\n")
    }

    /// Generate the gateway's lost-terminal relay payload. Unlike the
    /// mirrors above, the line text comes from the SHARED producer
    /// formatter ([`rio_nix::protocol::build::BuildResult::lost_terminal_relay_line`]
    /// — the exact fn the gateway emission calls), so producer and this
    /// fixture cannot drift; only the `STDERR_NEXT` framing newline is
    /// mirrored here.
    fn gateway_lost_terminal_relay(drv: &str) -> String {
        format!(
            "{}\n",
            rio_nix::protocol::build::BuildResult::lost_terminal_relay_line(drv)
        )
    }

    /// The lost-terminal relay marker is captured at the observer
    /// boundary from the producer's exact payload (the shared-formatter
    /// line plus the framing newline): the drv lands in
    /// `lost_terminals`, the line joins the evidence tail, and neither
    /// the build-id capture nor the reason capture is disturbed. The
    /// marker line never doubles as a failure reason.
    #[test]
    fn client_ops_observer_captures_lost_terminal_markers() {
        let lost = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv";
        let mut parsed = ParsedStderr::default();
        let mut tail: VecDeque<String> = VecDeque::new();

        observe_line(
            &mut parsed,
            &mut tail,
            &gateway_build_announcement("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a", ""),
        );
        let payload = gateway_lost_terminal_relay(lost);
        assert!(payload.ends_with('\n'), "premise: {payload:?}");
        observe_line(&mut parsed, &mut tail, &payload);

        assert_eq!(parsed.lost_terminals, BTreeSet::from([lost.to_string()]));
        assert!(parsed.reasons.is_empty(), "{:?}", parsed.reasons);
        assert_eq!(
            parsed.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        assert_eq!(
            tail.back().map(String::as_str),
            Some(format!("rio: terminal lost for '{lost}'").as_str()),
            "the marker line is evidence-tail-visible like every relay line"
        );
    }

    /// The gateway newline-TERMINATES its build announcement and its
    /// submit-failure diagnostic, and every relay layer (StderrWriter,
    /// the wire codec, the client drain, the transport observer) hands
    /// the payload to the observer verbatim. The observer must treat that
    /// newline as a line TERMINATOR, not a separator: a terminated
    /// payload contributes exactly its lines to the evidence tail — no
    /// blank residue entry burning cap budget and breaking the tail's
    /// one-line-per-entry invariant.
    #[test]
    fn client_ops_observer_drops_payload_terminators_from_the_tail() {
        let mut parsed = ParsedStderr::default();
        let mut tail: VecDeque<String> = VecDeque::new();

        let announcement = gateway_build_announcement(
            "0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a",
            "4bf92f3577b34da6a3ce929d0e0e4736",
        );
        assert!(announcement.ends_with('\n'), "premise: {announcement:?}");
        observe_line(&mut parsed, &mut tail, &announcement);

        let submit_failed = gateway_submit_failed("status: Unavailable, message: \"leader lost\"");
        assert!(submit_failed.ends_with('\n'), "premise: {submit_failed:?}");
        observe_line(&mut parsed, &mut tail, &submit_failed);

        assert_eq!(
            parsed.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        // One tail entry per payload line; the terminator itself
        // contributes nothing.
        assert_eq!(
            Vec::from(tail),
            vec![
                "rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a \
                 (trace 4bf92f3577b34da6a3ce929d0e0e4736)"
                    .to_string(),
                "SubmitBuild RPC failed: status: Unavailable, message: \"leader lost\"".to_string(),
            ]
        );
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

    /// Real [`ClientOpsSubmitter`] over a scripted channel source and a
    /// real mini archive (written by the production `ArchiveWriter` chain).
    fn client_ops(
        archive: Arc<crate::archive::reader::ReplayArchive>,
        source: &Arc<test_support::FakeChannelSource>,
        probe_chunk: usize,
    ) -> ClientOpsSubmitter {
        ClientOpsSubmitter {
            pool: source.clone(),
            archive: Arc::new(DrvArchive::new(archive)),
            op_timeout: Duration::from_secs(5),
            probe_chunk,
        }
    }

    /// Batch over the given roots with a PRODUCER-side node estimate — the
    /// field the budget chokepoint must not consult.
    fn batch_with_estimate(jobs: &[&str], roots: &[String], est_nodes: usize) -> Batch {
        Batch {
            jobs: jobs.iter().map(|s| s.to_string()).collect(),
            root_drvs: roots.to_vec(),
            est_nodes,
        }
    }

    /// The build op's stderr-drain-budget workload is derived HERE, at the
    /// submission chokepoint, from the realized import closure — never
    /// from the producer's `Batch::est_nodes`.
    ///
    /// Universe (the chokepoint's FEEDERS, not the assembler's output):
    /// every production batch funnels through
    /// `ClientOpsSubmitter::submit_batch` — `assemble_batches` waves,
    /// fail-fast isolation singletons, canary probe batches, and the timed
    /// dispatcher's two literal constructions (initial dispatch and
    /// confirmation retry), which never pass through the assembler and
    /// carry a roots-only `est_nodes`. The round-3 budget fix calibrated
    /// against the assembler's output and was escaped one level up by
    /// exactly those timed feeders; quantifying over the chokepoint's
    /// input data (the closure it just resolved) covers every feeder,
    /// including future ones, by construction.
    ///
    /// Mutation pin: the suite previously stayed green when the build
    /// call's workload argument was hard-flipped to `0` — no test observed
    /// the wire-layer argument through the real submitter. This test (and
    /// its timed sibling in `timeline.rs`) goes red for `0`,
    /// `batch.est_nodes`, and `root_drvs.len()` alike.
    #[tokio::test]
    async fn build_op_workload_is_the_realized_import_closure_not_the_producer_estimate() {
        use crate::run::archive_input::{load_units, write_mini_archive};

        let tmp = tempfile::tempdir().unwrap();
        write_mini_archive(tmp.path());
        let archive = Arc::new(crate::archive::reader::ReplayArchive::open(tmp.path()).unwrap());
        let app_b = load_units(&archive)
            .unwrap()
            .into_iter()
            .find(|u| u.job == "appB.x86_64-linux")
            .unwrap()
            .drv_path;
        // The realized import closure of appB is the 3-node chain
        // stdenv → libA → appB (pinned by the drv_import module tests).
        let closure = DrvArchive::new(archive.clone())
            .closure(std::slice::from_ref(&app_b))
            .unwrap();
        assert_eq!(
            closure.order.len(),
            3,
            "fixture premise: {:?}",
            closure.order
        );

        // The timed dispatcher's exact under-keyed shape: one root, the
        // producer estimate pinned to the ROOT COUNT (no adjacency data at
        // that producer).
        let source = Arc::new(test_support::FakeChannelSource::default());
        let outcome = client_ops(archive.clone(), &source, 2000)
            .submit_batch(
                "ssh-ng://test",
                &batch_with_estimate(&["appB.x86_64-linux"], std::slice::from_ref(&app_b), 1),
                BatchDeadline::Build(tokio::time::Instant::now() + Duration::from_secs(30)),
            )
            .await
            .unwrap();
        let calls = source.build_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].derived, vec![format!("{app_b}!*")]);
        assert_eq!(
            calls[0].closure_nodes, 3,
            "the budget workload must be the realized 3-node import closure, \
             not the producer's roots-only estimate (1)"
        );
        // The whole closure was probed and (nothing being valid) uploaded,
        // references before referrers.
        let probed: Vec<String> = source.probes.lock().unwrap().concat();
        assert_eq!(probed, closure.order);
        assert_eq!(source.uploads.lock().unwrap().iter().sum::<usize>(), 3);
        assert_eq!(outcome.results.len(), 1);
        assert!(!outcome.engine_cancelled);

        // Single-owner, both directions: an OVER-keyed producer estimate
        // is not consulted either — the chokepoint's realized closure wins
        // high and low alike, so no producer can widen the belt by lying.
        let source = Arc::new(test_support::FakeChannelSource::default());
        client_ops(archive.clone(), &source, 2000)
            .submit_batch(
                "ssh-ng://test",
                &batch_with_estimate(&["appB.x86_64-linux"], std::slice::from_ref(&app_b), 9_999),
                BatchDeadline::Build(tokio::time::Instant::now() + Duration::from_secs(30)),
            )
            .await
            .unwrap();
        assert_eq!(source.build_calls.lock().unwrap()[0].closure_nodes, 3);

        // Target-side validity does not shrink the workload basis: with
        // every closure path already valid (nothing to upload), the DAG
        // can still build — and stream logs for — all of it (force-build
        // campaigns rebuild valid paths), so the budget keeps the full
        // realized count.
        let source = Arc::new(test_support::FakeChannelSource {
            valid_paths: closure.order.iter().cloned().collect(),
            ..test_support::FakeChannelSource::default()
        });
        client_ops(archive, &source, 2)
            .submit_batch(
                "ssh-ng://test",
                &batch_with_estimate(&["appB.x86_64-linux"], std::slice::from_ref(&app_b), 1),
                BatchDeadline::Build(tokio::time::Instant::now() + Duration::from_secs(30)),
            )
            .await
            .unwrap();
        assert!(source.uploads.lock().unwrap().is_empty());
        assert_eq!(source.build_calls.lock().unwrap()[0].closure_nodes, 3);
        // probe_chunk 2 over 3 paths: the chunked probe still covered the
        // whole closure.
        assert_eq!(source.probes.lock().unwrap().len(), 2);
    }

    /// Closure gaps a non-conforming archive cannot offer still count into
    /// the budget workload: the target may resolve a skipped interior drv
    /// from its own store and build it — the conservative workload basis
    /// is everything the submitted DAG can REACH (`order` + `skipped`),
    /// not just what the archive could import. Fixture staged via the
    /// production `ArchiveWriter` chain (the same producer path the
    /// drv_import gap tests use), so the gap is a real records-vs-texts
    /// disagreement, not a hand-built `DrvClosure`.
    #[tokio::test]
    async fn build_op_workload_counts_unimportable_closure_gaps() {
        use crate::archive::schema::{Capabilities, RequestRecord, RequestTarget, Substituters};
        use crate::archive::writer::{ArchiveWriter, ManifestSeed};
        use crate::run::archive_input::fake_hash;

        let tmp = tempfile::tempdir().unwrap();
        let base_drv = format!("/nix/store/{}-base-1.0.drv", fake_hash("base-drv"));
        let base_out = format!("/nix/store/{}-base-1.0", fake_hash("base-out"));
        let extra_drv = format!("/nix/store/{}-extra-1.0.drv", fake_hash("extra-drv"));
        let extra_out = format!("/nix/store/{}-extra-1.0", fake_hash("extra-out"));
        let absent_drv = "/nix/store/yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy-absent.drv";
        let writer = ArchiveWriter::create(tmp.path()).unwrap();
        writer
            .add_drv(
                &base_drv,
                &format!(
                    r#"Derive([("out","{base_out}","","")],[],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{base_out}")])"#
                ),
            )
            .unwrap();
        writer
            .add_drv(
                &extra_drv,
                &format!(
                    r#"Derive([("out","{extra_out}","","")],[("{absent_drv}",["out"]),("{base_drv}",["out"])],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{extra_out}")])"#
                ),
            )
            .unwrap();
        // Only the dependency-free derivation is a workload target, so the
        // writer's closure-completeness walk tolerates the extra member's
        // non-embedded input.
        writer
            .write_requests(&[RequestRecord {
                session: 0,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: base_drv.clone(),
                    outputs: vec!["*".to_string()],
                }],
            }])
            .unwrap();
        let stamp: jiff::Timestamp = "2026-05-28T00:00:00Z".parse().unwrap();
        writer
            .finalize(ManifestSeed {
                created_at: stamp,
                from: stamp,
                to: stamp,
                capabilities: Capabilities::default(),
                substituters: Substituters {
                    relay: vec!["https://cache.example.org".to_string()],
                    target: Vec::new(),
                },
                fat: false,
                provenance: serde_json::Map::new(),
            })
            .unwrap();
        let archive = Arc::new(crate::archive::reader::ReplayArchive::open(tmp.path()).unwrap());

        let source = Arc::new(test_support::FakeChannelSource::default());
        let outcome = client_ops(archive, &source, 2000)
            .submit_batch(
                "ssh-ng://test",
                &batch_with_estimate(&["extra"], std::slice::from_ref(&extra_drv), 1),
                BatchDeadline::Build(tokio::time::Instant::now() + Duration::from_secs(30)),
            )
            .await
            .unwrap();
        // order = [base, extra] (2 importable) + skipped = [absent] (1):
        // the workload is 3 — the importable count alone (2) under-keys
        // the belt for exactly the DAG shapes gap retirement exists for.
        let calls = source.build_calls.lock().unwrap().clone();
        assert_eq!(calls[0].closure_nodes, 3, "order(2) + skipped(1)");
        assert_ne!(
            calls[0].closure_nodes, 2,
            "order alone must not key the budget"
        );
        assert_eq!(outcome.import_skipped_drvs, vec![absent_drv.to_string()]);
        // Only the importable texts were probed/uploaded.
        assert_eq!(source.probes.lock().unwrap().concat().len(), 2);
        assert_eq!(source.uploads.lock().unwrap().iter().sum::<usize>(), 2);
    }
}

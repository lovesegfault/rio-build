//! Batch submission to the rio gateway.
//!
//! [`Submitter`] is the trait the submit and warm stages drive, so both
//! stages stay unit-testable against a scripted in-memory fake.
//! [`ClientOpsSubmitter`] is the build-path implementation: it drives the
//! gateway's nix-daemon worker protocol in-process over the SSH channel
//! pool — per batch it imports the batch's drv closure from the replay
//! archive, then issues one `BuildPathsWithResults` call whose per-root
//! results become the batch outcome; relayed stderr lines are captured as
//! evidence only (build id, failure reasons, tail). [`WarmNixSubmitter`] is
//! the `nix` shell-out submitter, now scoped to the leaf-mode warm-stage
//! prefetch until the supply planner absorbs that stage.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use rio_nix::protocol::client::{KeyedBuildResult, StoreEntry};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

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

/// One batch-submission backend. The submit and warm stages only ever talk
/// to this trait; unit tests script it with an in-memory fake while a real
/// campaign uses [`ClientOpsSubmitter`] (build path) or [`WarmNixSubmitter`]
/// (warm-stage prefetch).
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

/// SSH options for the ssh-ng transport, exported to the `nix` children via
/// `NIX_SSHOPTS`: no host-key prompts (cluster endpoints are ephemeral),
/// client-side keepalives so long silent builds do not trip the gateway's
/// idle timeout, no connection multiplexing (a stale ControlMaster left by a
/// killed run would wedge later runs), and no ssh-agent involvement (a dead
/// forwarded agent socket hangs the handshake before key exchange). Same
/// option set as `xtask/src/k8s/shared.rs`'s `NIX_SSHOPTS_BASE`, which
/// documents the incident history behind each flag.
///
/// Security tradeoff: `StrictHostKeyChecking=no` plus a null known-hosts
/// file disables SSH host-key verification entirely. The gateway's host
/// keys are ephemeral (regenerated on every pod restart), so there is no
/// stable key to pin, and this mirrors the repo's existing xtask
/// convention for the same endpoint. The hardening alternatives — a
/// ConfigMap-provisioned `known_hosts` or SSH-CA-signed host keys — are an
/// infrastructure/enablement decision outside this crate. Flagged by an
/// automated security review and consciously accepted for now.
pub const NIX_SSHOPTS: &str = "-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
     -o ServerAliveInterval=30 -o ServerAliveCountMax=6 \
     -o ControlMaster=no -o ControlPath=none \
     -o IdentityAgent=none -o IdentitiesOnly=yes";

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

/// Feed one observed stderr line through the gateway-line parser (build id,
/// relayed per-derivation failure reasons) and append it to the capped
/// evidence tail. Shared by the client-ops build observer and the warm
/// shell-out's stderr reader so both capture identical evidence.
fn observe_line(parsed: &mut ParsedStderr, tail: &mut VecDeque<String>, line: &str) {
    parse_line(parsed, line);
    push_tail(tail, line.to_string());
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

/// Decode one captured stderr line for evidence capture and the
/// gateway-line regexes. Lossy on purpose: nix relays builder output
/// verbatim, so a non-UTF-8 byte sequence must never abort the stream and
/// discard the evidence captured so far. The replacement characters only
/// ever land in the evidence tail / relayed reason text — this is log
/// display, not a wire parse path (the case clippy.toml's
/// `disallowed-methods` rationale carves out).
fn lossy_stderr_line(buf: &[u8]) -> String {
    #[allow(clippy::disallowed_methods)]
    String::from_utf8_lossy(buf).into_owned()
}

/// Timeout for the pre-submission drv import. Generous on purpose: the
/// import copies tiny `.drv` text files from a local on-disk archive, so
/// even very large closures finish orders of magnitude faster than this.
const IMPORT_TIMEOUT: Duration = Duration::from_secs(1800);

/// What one `nix` child run produced: the assembled outcome plus the child's
/// exit code, which never leaves this module — the import gate needs it, but
/// [`BatchOutcome`] is transport-agnostic and does not carry one.
///
/// `outcome.engine_cancelled` is authoritative over `exit_code` when the two
/// disagree: in the narrow race where the child exits exactly as the
/// engine's deadline fires, `exit_code` can still be `Some(_)` — the run
/// must be treated as cancelled (the stderr stream was abandoned early, so
/// the captured evidence may be incomplete).
struct ChildCapture {
    outcome: BatchOutcome,
    /// Child exit code (`None` = killed by a signal, including the engine's
    /// own deadline kill).
    exit_code: Option<i32>,
}

/// [`Submitter`] that shells out to a stock `nix` binary. Serves ONLY the
/// leaf-mode warm-stage prefetch (dependency warming via the warm tenant);
/// it is removed once the supply planner takes over that stage. Build-path
/// batches go through [`ClientOpsSubmitter`] instead.
pub struct WarmNixSubmitter {
    /// Drv-archive directory (an uncompressed `file://` binary-cache
    /// layout) used to import each batch's drv closures into the local store
    /// before submission.
    pub drv_archive_dir: PathBuf,
    /// Program to invoke (`nix` from `PATH` by default). Tests point this at
    /// `sh` so the child-streaming and timeout paths are covered without a
    /// nix binary or a cluster.
    pub nix_bin: String,
    /// Extra environment for the children (HOME/XDG for the non-root
    /// container come from the pod env; this is for overrides/tests).
    pub extra_env: BTreeMap<String, String>,
}

impl WarmNixSubmitter {
    pub fn new(drv_archive_dir: PathBuf) -> Self {
        Self {
            drv_archive_dir,
            nix_bin: "nix".to_string(),
            extra_env: BTreeMap::new(),
        }
    }

    /// `nix copy --derivation --no-check-sigs --from file://<archive>
    /// <roots…>`: copies the root drvs and their derivation closures from
    /// the archive layout into the local store. Cheap (drvs are tiny text
    /// files) and idempotent (already-present paths are skipped);
    /// `--no-check-sigs` because the archive layout is unsigned.
    pub fn import_args(&self, batch: &Batch) -> Vec<String> {
        let mut args = vec![
            "copy".to_string(),
            "--extra-experimental-features".to_string(),
            "nix-command".to_string(),
            "--derivation".to_string(),
            "--no-check-sigs".to_string(),
            "--from".to_string(),
            format!("file://{}", self.drv_archive_dir.display()),
        ];
        args.extend(batch.root_drvs.iter().cloned());
        args
    }

    /// `nix build -L --no-link --store <url> <drv^*…>`: one submission per
    /// batch. `-L` is required so the gateway's relayed lines reach stderr
    /// in full; the installables are explicit `.drv^*` paths so nothing is
    /// evaluated client-side, and no `--eval-store` override is passed — the
    /// local store already holds the imported derivations and `--store`
    /// alone points the build at the gateway.
    pub fn build_args(store_url: &str, batch: &Batch) -> Vec<String> {
        let mut args = vec![
            "build".to_string(),
            "--extra-experimental-features".to_string(),
            "nix-command".to_string(),
            "-L".to_string(),
            "--no-link".to_string(),
            "--store".to_string(),
            store_url.to_string(),
        ];
        args.extend(batch.root_drvs.iter().map(|d| format!("{d}^*")));
        args
    }

    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.nix_bin);
        cmd.args(args)
            .env("NIX_SSHOPTS", NIX_SSHOPTS)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Dropping the future mid-flight (engine abort, task cancel)
            // must not orphan a still-running child.
            .kill_on_drop(true);
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        cmd
    }

    /// Run one child, streaming its stderr through the gateway-line parser
    /// and keeping the trailing lines as raw evidence; kill it at `timeout`.
    ///
    /// Stderr is read as raw bytes and decoded lossily: nix relays builder
    /// output verbatim, so a non-UTF-8 byte sequence must never abort the
    /// stream and discard the evidence captured so far. A genuine read
    /// error likewise keeps the partial capture: the child is killed (so
    /// the `wait` below cannot block forever on a full pipe nobody drains)
    /// and the outcome is returned with `engine_cancelled` set.
    async fn run_child(&self, args: &[String], timeout: Duration) -> Result<ChildCapture> {
        let mut child = self.command(args).spawn().with_context(|| {
            format!(
                "spawn {} {}",
                self.nix_bin,
                args.first().cloned().unwrap_or_default()
            )
        })?;
        let subcommand = args.first().cloned().unwrap_or_default();
        let stderr = child.stderr.take().expect("stderr piped");
        let mut reader = BufReader::new(stderr);
        let mut buf: Vec<u8> = Vec::new();
        let mut parsed = ParsedStderr::default();
        let mut tail: VecDeque<String> = VecDeque::new();

        let deadline = tokio::time::Instant::now() + timeout;
        let mut engine_cancelled = false;
        loop {
            tokio::select! {
                read = reader.read_until(b'\n', &mut buf) => {
                    match read {
                        Ok(0) => break,
                        Ok(_) => {
                            let line = lossy_stderr_line(&buf);
                            let line = line.trim_end_matches(['\r', '\n']);
                            observe_line(&mut parsed, &mut tail, line);
                            buf.clear();
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                program = %self.nix_bin,
                                subcommand = %subcommand,
                                "stderr read error; killing the child and keeping the partial \
                                 capture"
                            );
                            let _ = child.start_kill();
                            engine_cancelled = true;
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        program = %self.nix_bin,
                        subcommand = %subcommand,
                        "child deadline reached; killing it (an interrupted `nix build` \
                         submission is cancelled by the gateway on disconnect and re-offered \
                         on resume; an interrupted `nix copy` import just leaves the batch \
                         unsubmitted)"
                    );
                    let _ = child.start_kill();
                    engine_cancelled = true;
                    break;
                }
            }
        }
        // A line in flight when the loop was cut short (deadline / read
        // error) is still evidence — keep it in the tail, but don't feed a
        // possibly-truncated line to the parser.
        if !buf.is_empty() {
            push_tail(&mut tail, lossy_stderr_line(&buf));
        }
        let status = child.wait().await.context("wait for nix child")?;
        Ok(ChildCapture {
            outcome: BatchOutcome {
                build_id: parsed.build_id,
                // The nix-CLI child reports nothing in-band; per-root results
                // stay empty for this submitter.
                results: Vec::new(),
                reasons: parsed.reasons,
                stderr_tail: Vec::from(tail).join("\n"),
                engine_cancelled,
            },
            exit_code: status.code(),
        })
    }
}

#[async_trait]
impl Submitter for WarmNixSubmitter {
    async fn submit_batch(
        &self,
        store_url: &str,
        batch: &Batch,
        timeout: Duration,
    ) -> Result<BatchOutcome> {
        // Import the batch's drv closures from the archive into the local
        // store first (cheap; idempotent), then submit. `engine_cancelled`
        // is checked alongside the exit code: a child killed at the import
        // deadline counts as a failed import even if it managed to exit 0
        // in the kill race.
        let import = self
            .run_child(&self.import_args(batch), IMPORT_TIMEOUT)
            .await?;
        if import.exit_code != Some(0) || import.outcome.engine_cancelled {
            // The import child's stderr is not persisted anywhere else, so
            // carry a clipped tail of it in the error.
            let last_lines: Vec<&str> = import.outcome.stderr_tail.lines().rev().take(5).collect();
            let last_lines: Vec<&str> = last_lines.into_iter().rev().collect();
            let cancelled_note = if import.outcome.engine_cancelled {
                " (killed at the engine's import deadline)"
            } else {
                ""
            };
            anyhow::bail!(
                "drv import from {} failed (exit {:?}){}: {}",
                self.drv_archive_dir.display(),
                import.exit_code,
                cancelled_note,
                crate::body_snippet(&last_lines.join(" | "))
            );
        }
        let build = self
            .run_child(&Self::build_args(store_url, batch), timeout)
            .await?;
        Ok(build.outcome)
    }
}

/// `AddMultipleToStore` entries per upload call during the drv-text import.
/// Each entry's NAR payload is materialized in memory
/// ([`DrvArchive::entry`] returns `NarPayload::Bytes`), so this cap also
/// bounds the upload's resident memory; derivation texts are tiny, so 500
/// of them stay well under a megabyte. Aligned with the supply planner's
/// batch-entry cap; becomes a knob when the planner lands.
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

/// The per-job repro command recorded alongside each job result, so a human
/// can re-drive exactly one derivation through the same gateway. The
/// `ssh-key` query parameter is stripped from the store URL — secrets never
/// land in campaign artifacts.
pub fn repro_command(store_url: &str, drv_path: &str) -> String {
    let sanitized: String = match store_url.split_once('?') {
        Some((base, query)) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|kv| !kv.starts_with("ssh-key="))
                .collect();
            if kept.is_empty() {
                base.to_string()
            } else {
                format!("{base}?{}", kept.join("&"))
            }
        }
        None => store_url.to_string(),
    };
    format!("nix build -L --no-link --store '{sanitized}' '{drv_path}^*'")
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
    fn warm_nix_submitter_keeps_import_and_build_command_shapes() {
        let sub = WarmNixSubmitter::new(PathBuf::from("/scratch/drv-archive"));
        let import = sub.import_args(&batch());
        assert_eq!(
            import,
            vec![
                "copy",
                "--extra-experimental-features",
                "nix-command",
                "--derivation",
                "--no-check-sigs",
                "--from",
                "file:///scratch/drv-archive",
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv",
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
        let build = WarmNixSubmitter::build_args(
            "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/secrets/parity-leaf",
            &batch(),
        );
        assert_eq!(build[0], "build");
        assert!(
            build.contains(&"-L".to_string()),
            "must run with -L so relayed lines are captured"
        );
        assert!(build.contains(&"--no-link".to_string()));
        assert!(!build.contains(&"--eval-store".to_string()));
        assert_eq!(
            build.last().unwrap(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app-2.0.drv^*"
        );
    }

    #[test]
    fn nix_sshopts_match_harness_requirements() {
        for needle in [
            "StrictHostKeyChecking=no",
            "UserKnownHostsFile=/dev/null",
            "ServerAliveInterval=30",
            "ControlMaster=no",
            "IdentitiesOnly=yes",
        ] {
            assert!(NIX_SSHOPTS.contains(needle), "NIX_SSHOPTS missing {needle}");
        }
    }

    #[test]
    fn repro_command_strips_ssh_key() {
        let r = repro_command(
            "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/secrets/parity-leaf",
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv",
        );
        assert!(!r.contains("ssh-key"), "{r}");
        assert!(r.contains("compress=true"), "{r}");
        assert!(r.ends_with("-libfoo-1.0.drv^*'"), "{r}");
        // No query at all stays untouched.
        let r2 = repro_command("ssh-ng://rio@host:22", "/nix/store/x.drv");
        assert!(r2.contains("'ssh-ng://rio@host:22'"));
    }

    /// Outcome assembly from a real child process: `sh` stands in for `nix`
    /// so the streaming parse, the tail cap, and exit-code capture are
    /// covered without a nix binary or a cluster.
    #[tokio::test]
    async fn run_child_streams_and_parses_stderr() {
        let mut sub = WarmNixSubmitter::new(PathBuf::from("/nonexistent"));
        sub.nix_bin = "sh".to_string();
        let script = concat!(
            "i=0\n",
            "while [ \"$i\" -lt 250 ]; do echo \"noise line $i\" >&2; i=$((i+1)); done\n",
            "echo \"rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a (trace 4bf92f3577b34da6a3ce929d0e0e4736)\" >&2\n",
            "echo \"derivation '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv' failed: poison threshold reached after 3 distinct-worker failures\" >&2\n",
        );
        let capture = sub
            .run_child(
                &["-c".to_string(), script.to_string()],
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        let out = capture.outcome;
        assert_eq!(capture.exit_code, Some(0));
        assert!(!out.engine_cancelled);
        assert_eq!(
            out.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        assert_eq!(out.reasons.len(), 1);
        assert_eq!(
            out.reasons["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv"],
            "poison threshold reached after 3 distinct-worker failures"
        );
        // The tail keeps only the last STDERR_TAIL_LINES of the 252 emitted
        // lines, so the first 52 noise lines must have been dropped.
        let tail: Vec<&str> = out.stderr_tail.lines().collect();
        assert_eq!(tail.len(), STDERR_TAIL_LINES);
        assert_eq!(tail[0], "noise line 52");
        assert!(tail.last().unwrap().starts_with("derivation '"));
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

    /// Invalid UTF-8 on stderr must never abort the stream or discard the
    /// evidence captured so far: bytes are decoded lossily and parsing
    /// continues on the following lines.
    #[tokio::test]
    async fn run_child_keeps_evidence_across_invalid_utf8() {
        let mut sub = WarmNixSubmitter::new(PathBuf::from("/nonexistent"));
        sub.nix_bin = "sh".to_string();
        let script = concat!(
            "printf 'garbage \\377\\376 bytes\\n' >&2\n",
            "echo \"rio: build 0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a\" >&2\n",
            "echo \"derivation '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-libfoo-1.0.drv' failed: builder failed with exit code 2\" >&2\n",
        );
        let capture = sub
            .run_child(
                &["-c".to_string(), script.to_string()],
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        let out = capture.outcome;
        assert_eq!(capture.exit_code, Some(0));
        assert!(!out.engine_cancelled);
        assert_eq!(
            out.build_id.as_deref(),
            Some("0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a")
        );
        assert_eq!(out.reasons.len(), 1);
        // The undecodable bytes are kept (lossily replaced) in the tail.
        assert!(out.stderr_tail.contains("garbage"), "{}", out.stderr_tail);
        assert!(out.stderr_tail.contains('\u{FFFD}'), "{}", out.stderr_tail);
    }

    /// The engine deadline kills a child that outlives it and reports the
    /// kill as `engine_cancelled` with no exit code.
    #[tokio::test]
    async fn run_child_kills_the_child_at_the_timeout() {
        let mut sub = WarmNixSubmitter::new(PathBuf::from("/nonexistent"));
        sub.nix_bin = "sh".to_string();
        let capture = sub
            .run_child(
                &["-c".to_string(), "sleep 30".to_string()],
                Duration::from_millis(250),
            )
            .await
            .unwrap();
        assert!(capture.outcome.engine_cancelled);
        assert_eq!(capture.exit_code, None);
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
        // Same cap as the warm shell-out's evidence tail: 252 lines fed, the
        // first 52 noise lines dropped.
        assert_eq!(tail.len(), STDERR_TAIL_LINES);
        assert_eq!(tail.front().unwrap(), "noise line 52");
        assert!(tail.back().unwrap().starts_with("derivation '"));
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

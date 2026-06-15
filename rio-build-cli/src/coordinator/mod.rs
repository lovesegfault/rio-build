//! The pipelined coordinator (ADR-024 stages 1–5).
//!
//! Eval and upload overlap; nothing waits for "eval finished". One
//! main task owns the graph and the eval-channel writer; everything
//! else (channel reads, upload batches, per-root submit+watch) runs in
//! spawned tasks funneling internal messages back:
//!
//! ```text
//!  eval parent ──frames──▶ reader task ─┐
//!  upload tasks (stages 2+3) ───────────┼──▶ main loop: fold (1),
//!  submit+watch tasks (stages 4+5) ─────┘     gate check, spawn next
//! ```
//!
//! The main loop folds each result frame, hands fresh digests to an
//! upload task, re-checks the per-root all-acked gate on every ack,
//! and spawns a submit+watch task per ready root. Ctrl-C cancels by
//! default: every build this invocation submitted is cancelled via
//! `CancelBuild` and the run exits non-zero; `--detach` keeps the
//! builds running cluster-side instead and the summary lists their
//! ids with the `--attach` hint.

pub mod clients;
mod faillog;
pub mod graph;
pub mod submit;
pub mod upload;

pub use faillog::{FailureLogOpts, replay_failure_log};

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use rio_proto::evaljob::{
    IfdCompletion, ResultFrame, Shutdown, WorkItem, coordinator_frame, worker_frame,
};
use rio_proto::types::{BuildEvent, DrvBlob, build_event::Event};
use tokio::sync::mpsc;
use tracing::{debug, info, instrument, warn};

use crate::acks::ClusterAckTable;
use crate::evalchan::EvalChannel;
use crate::render::{RenderEvent, RenderHandle};
use clients::Clients;
use graph::{BuildGraph, Digest32, RootGate, SubmitOptions};
use submit::SubmitMaterials;
use upload::UploadReport;

pub struct CoordinatorOpts {
    pub priority_class: String,
    /// Body-field tenant name (single-tenant fallback; the attested
    /// identity is the JWT on the request).
    pub tenant_name: String,
    pub keep_going: bool,
    pub page_max_nodes: usize,
    pub fetch: bool,
    pub out_link: Option<PathBuf>,
    /// Flag-gated local IFD fallback (default off). P3b wires the
    /// actual local build; until then the flag is parsed and plumbed,
    /// and an IFD under it fails with an explicit message instead of
    /// silently going remote.
    pub local_ifd: bool,
    /// `--detach`: an interrupt exits the client and leaves builds
    /// running cluster-side instead of cancelling them.
    pub detach_on_interrupt: bool,
    /// `--log-lines` / `-L`: how a fail-fast failure's original log is
    /// replayed.
    pub failure_log: FailureLogOpts,
}

impl Default for CoordinatorOpts {
    fn default() -> Self {
        Self {
            priority_class: "interactive".into(),
            tenant_name: String::new(),
            keep_going: false,
            page_max_nodes: 50_000,
            fetch: false,
            out_link: None,
            local_ifd: false,
            detach_on_interrupt: false,
            failure_log: FailureLogOpts::default(),
        }
    }
}

/// Terminal state of one submitted build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeState {
    Completed {
        output_paths: Vec<String>,
    },
    Failed {
        message: String,
    },
    Cancelled {
        reason: String,
    },
    /// The run stopped watching while this build was still streaming
    /// (`--detach` interrupt, or a second interrupt that skipped the
    /// cancel acknowledgement) — the build keeps running cluster-side.
    Detached,
    /// The attr never produced a build (eval-side failure).
    EvalFailed {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct BuildOutcome {
    pub attr: String,
    /// Empty until the first event arrived (an interrupt can race it).
    pub build_id: String,
    pub state: OutcomeState,
    /// `(drv_path, DerivationEventKind)` edges observed, in order.
    pub drv_events: Vec<(String, i32)>,
    /// Materialized locations (`--fetch`).
    pub fetched: Vec<PathBuf>,
}

pub struct RunSummary {
    pub outcomes: Vec<BuildOutcome>,
    /// An interrupt ended the run early (the run must exit non-zero
    /// unless it detached).
    pub interrupted: bool,
    /// The interrupt ran under `--detach`: builds keep running
    /// cluster-side and the reattach hints were already printed.
    pub detached: bool,
}

/// Result of consuming one build's event stream to a terminal event.
struct WatchResult {
    build_id: String,
    state: OutcomeState,
    drv_events: Vec<(String, i32)>,
    last_sequence: u64,
}

enum Internal {
    /// Boxed: a worker frame (skeleton batch) dwarfs the other
    /// variants and these messages are transient.
    Worker(Box<worker_frame::Msg>),
    ChannelClosed,
    Uploaded(anyhow::Result<UploadReport>),
    /// Final page accepted — record digests as submitted, drop bodies.
    Accepted {
        root_idx: usize,
        digests: Vec<Digest32>,
    },
    /// First event arrived (the build id is now known).
    Started {
        root_idx: usize,
        build_id: String,
    },
    Finished {
        root_idx: usize,
        result: anyhow::Result<WatchResult>,
    },
    IfdFetched {
        drv_path: String,
        result: anyhow::Result<Vec<String>>,
    },
}

pub struct Coordinator {
    pub clients: Clients,
    pub acks: Arc<Mutex<ClusterAckTable>>,
    pub cas_root: PathBuf,
    pub opts: CoordinatorOpts,
    /// The renderer task's send handle. Every `BuildEvent` from a watch
    /// stream goes here; the renderer decides what to print.
    pub render: RenderHandle,
}

/// Wait for the next interrupt signal. A dropped sender must NOT read
/// as Ctrl-C — only an explicit send counts; on a closed channel the
/// future parks forever.
async fn next_interrupt(interrupt: &mut mpsc::UnboundedReceiver<()>) {
    if interrupt.recv().await.is_none() {
        std::future::pending::<()>().await;
    }
}

impl Coordinator {
    /// Drive a full `rio build` invocation: feed `attrs` to the eval
    /// parent over `chan`, run the pipeline, and return per-build
    /// outcomes. The first message on `interrupt` (Ctrl-C/SIGTERM) ends
    /// the run early: by default every build this invocation submitted
    /// is cancelled; under `--detach` the builds keep running
    /// cluster-side. A second interrupt skips waiting for cancel
    /// acknowledgements.
    #[instrument(skip_all, fields(component = "build-client", attrs = attrs.len()))]
    pub async fn run(
        &mut self,
        chan: EvalChannel,
        attrs: Vec<String>,
        mut interrupt: mpsc::UnboundedReceiver<()>,
    ) -> anyhow::Result<RunSummary> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Internal>();
        let EvalChannel {
            mut reader,
            mut writer,
        } = chan;

        // Reader task: frames → Internal.
        let reader_tx = tx.clone();
        let reader_task = tokio::spawn(async move {
            loop {
                match reader.recv().await {
                    Ok(Some(msg)) => {
                        if reader_tx.send(Internal::Worker(Box::new(msg))).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {
                        let _ = reader_tx.send(Internal::ChannelClosed);
                        return;
                    }
                    Err(e) => {
                        warn!(error = %e, "worker channel read failed");
                        let _ = reader_tx.send(Internal::ChannelClosed);
                        return;
                    }
                }
            }
        });

        for attr in &attrs {
            writer
                .send(coordinator_frame::Msg::Work(WorkItem {
                    attr: attr.clone(),
                }))
                .await
                .context("sending WorkItem")?;
        }

        let mut graph = BuildGraph::default();
        let mut expected: HashSet<String> = attrs.iter().cloned().collect();
        // Every attr ever sent as a WorkItem (user-typed or expansion
        // child) — the dedup set for attrset expansions.
        let mut requested: HashSet<String> = expected.clone();
        let mut eval_failures: Vec<BuildOutcome> = Vec::new();
        let mut outcomes: Vec<BuildOutcome> = Vec::new();
        let mut pending_uploads = 0usize;
        let mut finished_roots: HashSet<usize> = HashSet::new();
        let mut build_ids: HashMap<usize, String> = HashMap::new();
        // root digest → IFD drv_path (these roots answer the eval
        // parent instead of the user).
        let mut ifd_roots: HashMap<Digest32, String> = HashMap::new();
        let mut ifd_outstanding = 0usize;
        let mut shutdown_sent = false;
        let mut channel_closed = false;
        let mut interrupted = false;

        loop {
            // Spawn submits for every root whose gate opened.
            // `spawn_submit` claims the root's digests immediately, so
            // a sibling root becoming ready in the SAME pass already
            // excludes them from its pages (deterministic multi-root
            // overlap filter, index order).
            for (idx, gate) in graph.pending_roots() {
                match gate {
                    RootGate::Ready => self.spawn_submit(&mut graph, idx, tx.clone()),
                    RootGate::MissingNodes(_) | RootGate::PendingAcks(_) => {}
                }
            }

            // Completion check: every attr resolved, no uploads or
            // builds in flight, no IFD pending.
            let roots_done = graph
                .roots()
                .iter()
                .enumerate()
                .all(|(i, _)| finished_roots.contains(&i));
            let attrs_resolved = expected.is_empty();
            if attrs_resolved && pending_uploads == 0 && roots_done && ifd_outstanding == 0 {
                if !shutdown_sent {
                    shutdown_sent = true;
                    let _ = writer
                        .send(coordinator_frame::Msg::Shutdown(Shutdown {}))
                        .await;
                }
                if channel_closed {
                    break;
                }
            }

            // Biased toward the internal queue: already-arrived state
            // (a Started carrying the build id) is applied before a
            // concurrent Ctrl-C wins, so the interrupt path names every
            // build the scheduler has acked. The interrupt still fires
            // on the first empty poll — internal bursts are finite.
            let msg = tokio::select! {
                biased;
                msg = rx.recv() => match msg {
                    Some(m) => m,
                    None => break,
                },
                _ = next_interrupt(&mut interrupt) => {
                    interrupted = true;
                    break;
                }
            };

            match msg {
                Internal::Worker(worker_msg) => match *worker_msg {
                    worker_frame::Msg::Result(frame) => {
                        self.handle_frame(
                            &mut graph,
                            frame,
                            &mut expected,
                            &mut pending_uploads,
                            &tx,
                        )?;
                    }
                    worker_frame::Msg::IfdRequest(req) => {
                        let node = req
                            .node
                            .ok_or_else(|| anyhow::anyhow!("IfdRequest without node"))?;
                        let drv_path = node.drv_path.clone();
                        if self.opts.local_ifd {
                            // Plumbed but not wired (P3b owns the local
                            // eval-store build path).
                            writer
                                .send(coordinator_frame::Msg::IfdCompletion(IfdCompletion {
                                    drv_path,
                                    output_paths: vec![],
                                    error: "--local-ifd: local IFD fallback is not wired yet \
                                        (ADR-024 P3b)"
                                        .into(),
                                }))
                                .await
                                .context("sending IfdCompletion")?;
                            continue;
                        }
                        info!(drv = %drv_path, "IFD stall: building remotely");
                        self.render
                            .note(format!("IFD stall: building {drv_path} remotely"));
                        let digest: Digest32 =
                            node.drv_digest.as_slice().try_into().map_err(|_| {
                                anyhow::anyhow!("IfdRequest drv_digest is not 32 bytes")
                            })?;
                        ifd_roots.insert(digest, drv_path.clone());
                        ifd_outstanding += 1;
                        let frame = ResultFrame {
                            attr: format!("ifd:{drv_path}"),
                            nodes: vec![node],
                            drv_blobs: req.blob.into_iter().collect(),
                            source_roots: vec![],
                            root_drv_digest: digest.to_vec(),
                        };
                        self.handle_frame(
                            &mut graph,
                            frame,
                            &mut expected,
                            &mut pending_uploads,
                            &tx,
                        )?;
                    }
                    worker_frame::Msg::Expansion(exp) => {
                        // The attrset installable becomes one root per
                        // derivation child, named by its full attr
                        // path; children spread across the worker pool
                        // like explicitly listed attrs. A child already
                        // requested (explicitly or by another
                        // expansion) is not queued twice.
                        // r[impl bc.eval.attrset-expansion]
                        expected.remove(&exp.attr);
                        for skipped in &exp.skipped {
                            warn!(
                                attr = %skipped,
                                "skipping attrset entry: neither a derivation nor a recursable \
                                 attrset"
                            );
                            self.render.note(format!(
                                "skipping {skipped}: neither a derivation nor a recursable attrset"
                            ));
                        }
                        if exp.children.is_empty() {
                            // The worker normally errors instead of
                            // sending an empty expansion; never let the
                            // attr vanish silently.
                            eval_failures.push(BuildOutcome {
                                attr: exp.attr,
                                build_id: String::new(),
                                state: OutcomeState::EvalFailed {
                                    message: "attrset installable expanded to zero derivations"
                                        .into(),
                                },
                                drv_events: vec![],
                                fetched: vec![],
                            });
                            continue;
                        }
                        info!(
                            attr = %exp.attr,
                            children = exp.children.len(),
                            "attrset installable expanded"
                        );
                        self.render.note(format!(
                            "{}: expanded to {} derivations",
                            exp.attr,
                            exp.children.len()
                        ));
                        for child in exp.children {
                            if !requested.insert(child.clone()) {
                                continue;
                            }
                            expected.insert(child.clone());
                            writer
                                .send(coordinator_frame::Msg::Work(WorkItem { attr: child }))
                                .await
                                .context("sending WorkItem")?;
                        }
                    }
                    worker_frame::Msg::Recycle(n) => {
                        debug!(generation = n.generation, "eval worker recycled");
                    }
                    worker_frame::Msg::Note(n) => {
                        // Pre-fork warmup progress (libnix fetch
                        // activity) — visibility only.
                        self.render.note(n.text);
                    }
                    worker_frame::Msg::Error(e) => {
                        if e.fatal {
                            bail!("eval parent failed: {}", e.message);
                        }
                        // The empty attr is a real WorkItem in zero-installable
                        // file mode (the file's top-level value), so an Error
                        // naming it while it is still expected is that attr's
                        // eval failure — otherwise the build would wait on it
                        // forever. Only an empty attr that was never requested
                        // is a non-attr fault (e.g. a worker crash whose attr
                        // was re-queued): visibility only, no attr is lost.
                        if e.attr.is_empty() && !expected.contains("") {
                            warn!(error = %e.message, "eval parent reported a non-attr fault");
                            continue;
                        }
                        expected.remove(&e.attr);
                        warn!(attr = %e.attr, error = %e.message, "attr failed to evaluate");
                        self.render
                            .note(format!("{}: evaluation failed: {}", e.attr, e.message));
                        eval_failures.push(BuildOutcome {
                            attr: e.attr,
                            build_id: String::new(),
                            state: OutcomeState::EvalFailed { message: e.message },
                            drv_events: vec![],
                            fetched: vec![],
                        });
                    }
                },
                Internal::ChannelClosed => {
                    channel_closed = true;
                    if !shutdown_sent && !expected.is_empty() {
                        bail!(
                            "eval parent closed the channel before reporting attrs: {:?}",
                            expected
                        );
                    }
                }
                Internal::Uploaded(result) => {
                    pending_uploads -= 1;
                    let report = result.context("upload batch failed")?;
                    for d in &report.acked_drvs {
                        graph.mark_drv_acked(d);
                    }
                    for d in &report.acked_sources {
                        graph.mark_source_acked(d);
                    }
                    if !report.acked_drvs.is_empty() {
                        // Feed acks back so re-forked workers inherit
                        // the union and drop retained bytes.
                        let _ = writer
                            .send(coordinator_frame::Msg::AckFeedback(
                                rio_proto::evaljob::AckFeedback {
                                    digests: report.acked_drvs.iter().map(|d| d.to_vec()).collect(),
                                },
                            ))
                            .await;
                    }
                }
                Internal::Accepted { root_idx, digests } => {
                    debug!(
                        root_idx,
                        "submission accepted — dropping retained drv bodies"
                    );
                    graph.drop_bodies(&digests);
                }
                Internal::Started { root_idx, build_id } => {
                    info!(build_id = %build_id, attr = %graph.root(root_idx).attr, "build accepted");
                    build_ids.insert(root_idx, build_id);
                }
                Internal::Finished { root_idx, result } => {
                    finished_roots.insert(root_idx);
                    let root_digest = graph.root(root_idx).digest;
                    let attr = graph.root(root_idx).attr.clone();
                    if let Some(drv_path) = ifd_roots.get(&root_digest).cloned() {
                        // IFD mini-build: fetch outputs into the CAS,
                        // then resume the worker.
                        match result {
                            Ok(w) => match w.state {
                                OutcomeState::Completed { output_paths } => {
                                    let mut c = self.clients.clone();
                                    let cas = self.cas_root.clone();
                                    let itx = tx.clone();
                                    tokio::spawn(async move {
                                        let mut fetched = Vec::new();
                                        let mut err = None;
                                        for p in &output_paths {
                                            match crate::fetch::materialize(&mut c, &cas, p).await {
                                                Ok(_) => fetched.push(p.clone()),
                                                Err(e) => {
                                                    err = Some(e);
                                                    break;
                                                }
                                            }
                                        }
                                        let result = match err {
                                            None => Ok(fetched),
                                            Some(e) => Err(e),
                                        };
                                        let _ = itx.send(Internal::IfdFetched { drv_path, result });
                                    });
                                }
                                other => {
                                    ifd_outstanding -= 1;
                                    writer
                                        .send(coordinator_frame::Msg::IfdCompletion(
                                            IfdCompletion {
                                                drv_path,
                                                output_paths: vec![],
                                                error: format!("IFD build failed: {other:?}"),
                                            },
                                        ))
                                        .await
                                        .context("sending IfdCompletion")?;
                                }
                            },
                            Err(e) => {
                                ifd_outstanding -= 1;
                                writer
                                    .send(coordinator_frame::Msg::IfdCompletion(IfdCompletion {
                                        drv_path,
                                        output_paths: vec![],
                                        error: format!("IFD submission failed: {e:#}"),
                                    }))
                                    .await
                                    .context("sending IfdCompletion")?;
                            }
                        }
                        continue;
                    }
                    let outcome = match result {
                        Ok(w) => BuildOutcome {
                            attr,
                            build_id: w.build_id,
                            state: w.state,
                            drv_events: w.drv_events,
                            fetched: vec![],
                        },
                        Err(e) => BuildOutcome {
                            attr,
                            build_id: build_ids.get(&root_idx).cloned().unwrap_or_default(),
                            state: OutcomeState::Failed {
                                message: format!("{e:#}"),
                            },
                            drv_events: vec![],
                            fetched: vec![],
                        },
                    };
                    outcomes.push(outcome);
                }
                Internal::IfdFetched { drv_path, result } => {
                    ifd_outstanding -= 1;
                    let completion = match result {
                        Ok(paths) => IfdCompletion {
                            drv_path,
                            output_paths: paths,
                            error: String::new(),
                        },
                        Err(e) => IfdCompletion {
                            drv_path,
                            output_paths: vec![],
                            error: format!("fetching IFD outputs: {e:#}"),
                        },
                    };
                    writer
                        .send(coordinator_frame::Msg::IfdCompletion(completion))
                        .await
                        .context("sending IfdCompletion")?;
                }
            }
        }

        reader_task.abort();
        // Worker teardown on interrupt is identical to a completed run:
        // the eval channel writer drops on return and the eval parent
        // exits on EOF.
        let mut detached = false;
        if interrupted {
            // r[impl bc.interrupt.scope]
            // Only builds this invocation submitted (`build_ids`) are
            // ever touched; a build watched via `--attach` never
            // reaches this path.
            let pending: Vec<(String, String)> = build_ids
                .iter()
                .filter(|(idx, _)| !finished_roots.contains(idx))
                .map(|(idx, id)| (graph.root(*idx).attr.clone(), id.clone()))
                .collect();
            if self.opts.detach_on_interrupt {
                // r[impl bc.interrupt.detach-flag]
                // Builds keep running; tell the user how to come back.
                detached = true;
                for (attr, id) in &pending {
                    self.render.note(format!(
                        "detached: {attr} continues as build {id} — rio build --attach {id}"
                    ));
                    outcomes.push(BuildOutcome {
                        attr: attr.clone(),
                        build_id: id.clone(),
                        state: OutcomeState::Detached,
                        drv_events: vec![],
                        fetched: vec![],
                    });
                }
            } else {
                // r[impl bc.interrupt.cancel-default]
                // Cancel everything this invocation submitted. A second
                // interrupt stops waiting for cancel acknowledgements:
                // the remaining ids are printed with reattach hints so
                // nothing is lost.
                let mut aborted = false;
                for (attr, id) in &pending {
                    if !aborted {
                        tokio::select! {
                            biased;
                            _ = next_interrupt(&mut interrupt) => aborted = true,
                            res = cancel_build(&mut self.clients, id) => {
                                self.render.note(match res {
                                    Ok(true) => format!("interrupted: cancelled {attr} (build {id})"),
                                    Ok(false) => {
                                        format!("interrupted: {attr} (build {id}) already terminal")
                                    }
                                    Err(e) => format!(
                                        "interrupted: cancelling {attr} (build {id}) failed: {e:#}"
                                    ),
                                });
                                outcomes.push(BuildOutcome {
                                    attr: attr.clone(),
                                    build_id: id.clone(),
                                    state: OutcomeState::Cancelled {
                                        reason: "interrupted by user".into(),
                                    },
                                    drv_events: vec![],
                                    fetched: vec![],
                                });
                            }
                        }
                    }
                    if aborted {
                        self.render.note(format!(
                            "interrupted again: {attr} may still be running as build {id} — \
                             rio build --attach {id} (or --cancel {id})"
                        ));
                        outcomes.push(BuildOutcome {
                            attr: attr.clone(),
                            build_id: id.clone(),
                            state: OutcomeState::Detached,
                            drv_events: vec![],
                            fetched: vec![],
                        });
                    }
                }
            }
        }
        outcomes.extend(eval_failures);

        // `--fetch`: materialize completed outputs into the CAS.
        if self.opts.fetch && !interrupted {
            let mut link_idx = 0usize;
            for outcome in &mut outcomes {
                let OutcomeState::Completed { output_paths } = &outcome.state else {
                    continue;
                };
                for path in output_paths.clone() {
                    let dest =
                        crate::fetch::materialize(&mut self.clients, &self.cas_root, &path).await?;
                    if let Some(link) = &self.opts.out_link {
                        let target = if link_idx == 0 {
                            link.clone()
                        } else {
                            // result, result-2, result-3 … (nix's
                            // multi-output out-link numbering).
                            let mut name = link.file_name().unwrap_or_default().to_os_string();
                            name.push(format!("-{}", link_idx + 1));
                            link.with_file_name(name)
                        };
                        crate::fetch::out_link(&target, &dest)?;
                        link_idx += 1;
                    }
                    outcome.fetched.push(dest);
                }
            }
        }

        Ok(RunSummary {
            outcomes,
            interrupted,
            detached,
        })
    }

    /// Fold a frame and spawn the upload batch for whatever is new.
    fn handle_frame(
        &self,
        graph: &mut BuildGraph,
        frame: ResultFrame,
        expected: &mut HashSet<String>,
        pending_uploads: &mut usize,
        tx: &mpsc::UnboundedSender<Internal>,
    ) -> anyhow::Result<()> {
        let attr = frame.attr.clone();
        // Capture fresh bodies/sources for the upload task BEFORE the
        // fold consumes the frame.
        let fresh_blobs: Vec<DrvBlob> = frame.drv_blobs.clone();
        let outcome = graph.fold(frame)?;
        if outcome.completed_root.is_some() {
            expected.remove(&attr);
        }
        let new_drv_set: HashSet<Vec<u8>> = outcome.new_drvs.iter().map(|d| d.to_vec()).collect();
        let upload_blobs: Vec<DrvBlob> = fresh_blobs
            .into_iter()
            .filter(|b| new_drv_set.contains(&b.digest))
            .collect();
        let upload_sources: Vec<rio_proto::evaljob::SourceRoot> = outcome
            .new_sources
            .iter()
            .filter_map(|d| graph.source(d).map(|s| s.root.clone()))
            .collect();
        if upload_blobs.is_empty() && upload_sources.is_empty() && outcome.new_drvs.is_empty() {
            return Ok(());
        }
        // Nodes can arrive without bodies (dedup'd repeats carry none)
        // — those digests still need negotiation if unacked; but a
        // skeleton node ALWAYS rides with its body on first sight (the
        // worker contract), so negotiating `upload_blobs` covers them.
        *pending_uploads += 1;
        let clients = self.clients.clone();
        let acks = Arc::clone(&self.acks);
        // Streamed source roots (origin-less) are served from the
        // client CAS; the upload task opens its own read handle there.
        let cas_root = self.cas_root.clone();
        let itx = tx.clone();
        tokio::spawn(async move {
            let result =
                upload::upload_batch(clients, acks, cas_root, upload_blobs, upload_sources).await;
            let _ = itx.send(Internal::Uploaded(result));
        });
        Ok(())
    }

    /// Spawn the per-root submit+watch task (stages 4+5). Claims the
    /// root's digests synchronously so sibling roots exclude them.
    fn spawn_submit(
        &self,
        graph: &mut BuildGraph,
        idx: usize,
        tx: mpsc::UnboundedSender<Internal>,
    ) {
        let (nodes, bodies) = graph.submission_for(idx);
        let digests: Vec<Digest32> = nodes
            .iter()
            .filter_map(|n| n.drv_digest.as_slice().try_into().ok())
            .collect();
        graph.claim_submitted(idx, &digests);
        let mats = SubmitMaterials {
            nodes,
            bodies,
            opts: SubmitOptions {
                priority_class: self.opts.priority_class.clone(),
                tenant_name: self.opts.tenant_name.clone(),
                keep_going: self.opts.keep_going,
            },
            page_max_nodes: self.opts.page_max_nodes,
        };
        let mut clients = self.clients.clone();
        let acks = Arc::clone(&self.acks);
        let render = self.render.clone();
        let failure_log = self.opts.failure_log;
        tokio::spawn(async move {
            let result = async {
                let stream = submit::submit_root(&mut clients, &acks, &mats).await?;
                let _ = tx.send(Internal::Accepted {
                    root_idx: idx,
                    digests,
                });
                watch_stream(&mut clients, stream, idx, &tx, render, failure_log).await
            }
            .await;
            let _ = tx.send(Internal::Finished {
                root_idx: idx,
                result,
            });
        });
    }
}

/// Consume a build's event stream to its terminal event, reporting the
/// build id on first sight and resuming via `WatchBuild` once if the
/// stream ends without a terminal (transport hiccup — the build is
/// still running cluster-side).
async fn watch_stream(
    clients: &mut Clients,
    mut stream: tonic::Streaming<BuildEvent>,
    root_idx: usize,
    tx: &mpsc::UnboundedSender<Internal>,
    render: RenderHandle,
    failure_log: FailureLogOpts,
) -> anyhow::Result<WatchResult> {
    let mut result = WatchResult {
        build_id: String::new(),
        state: OutcomeState::Failed {
            message: "stream ended without a terminal event".into(),
        },
        drv_events: vec![],
        last_sequence: 0,
    };
    let mut resumed = false;
    loop {
        let ev = match stream.message().await {
            Ok(Some(ev)) => ev,
            Ok(None) | Err(_) => {
                if terminal(&result.state) {
                    return Ok(result);
                }
                if resumed || result.build_id.is_empty() {
                    return Ok(result);
                }
                resumed = true;
                let resp = clients
                    .scheduler
                    .watch_build(clients.req(rio_proto::types::WatchBuildRequest {
                        build_id: result.build_id.clone(),
                        since_sequence: result.last_sequence,
                    })?)
                    .await
                    .context("WatchBuild resume")?;
                stream = resp.into_inner();
                continue;
            }
        };
        if result.build_id.is_empty() && !ev.build_id.is_empty() {
            result.build_id = ev.build_id.clone();
            let _ = tx.send(Internal::Started {
                root_idx,
                build_id: ev.build_id.clone(),
            });
        }
        result.last_sequence = result.last_sequence.max(ev.sequence);
        render.send(RenderEvent::Build(ev.clone()));
        match ev.event {
            Some(Event::Derivation(d)) => {
                result.drv_events.push((d.derivation_path, d.kind));
            }
            Some(Event::Completed(c)) => {
                result.state = OutcomeState::Completed {
                    output_paths: c.output_paths,
                };
                return Ok(result);
            }
            Some(Event::Failed(f)) => {
                // Fail-fast on a previously-failed derivation: replay the
                // original culprit's log (or its persisted reason) before
                // reporting the terminal state. Live failures carry no
                // culprit fields — their log already streamed above.
                if !f.culprit_derivation.is_empty() {
                    replay_failure_log(clients, &result.build_id, &f, failure_log, &render).await;
                }
                result.state = OutcomeState::Failed {
                    message: f.error_message,
                };
                return Ok(result);
            }
            Some(Event::Cancelled(c)) => {
                result.state = OutcomeState::Cancelled { reason: c.reason };
                return Ok(result);
            }
            _ => {}
        }
    }
}

fn terminal(state: &OutcomeState) -> bool {
    matches!(
        state,
        OutcomeState::Completed { .. } | OutcomeState::Cancelled { .. }
    )
}

/// `--attach <id>`: resume a build's event stream from the start (or
/// `since`) and consume to terminal. Works from any machine with the
/// tenant credential.
pub async fn attach_build(
    clients: &mut Clients,
    build_id: &str,
    since_sequence: u64,
    render: RenderHandle,
    failure_log: FailureLogOpts,
) -> anyhow::Result<BuildOutcome> {
    let resp = clients
        .scheduler
        .watch_build(clients.req(rio_proto::types::WatchBuildRequest {
            build_id: build_id.to_string(),
            since_sequence,
        })?)
        .await
        .context("WatchBuild")?;
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut result = watch_stream(clients, resp.into_inner(), 0, &tx, render, failure_log).await?;
    if result.build_id.is_empty() {
        result.build_id = build_id.to_string();
    }
    Ok(BuildOutcome {
        attr: String::new(),
        build_id: result.build_id,
        state: result.state,
        drv_events: result.drv_events,
        fetched: vec![],
    })
}

/// `--cancel <id>`.
pub async fn cancel_build(clients: &mut Clients, build_id: &str) -> anyhow::Result<bool> {
    let resp = clients
        .scheduler
        .cancel_build(clients.req(rio_proto::types::CancelBuildRequest {
            build_id: build_id.to_string(),
            reason: "user_request".into(),
        })?)
        .await
        .context("CancelBuild")?;
    Ok(resp.into_inner().cancelled)
}

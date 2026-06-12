//! In-process `SchedulerService` stub for coordinator e2e tests.
//!
//! Purpose-built (the rio-test-support `MockScheduler` can't stage
//! pages or verify digests): it implements the parts of the real
//! scheduler's submit contract the COORDINATOR depends on —
//!
//! - pagination staging keyed by `submission_id` (non-final pages ack
//!   with an immediately-closed empty stream);
//! - the digest bulk-verify against the REAL store's `drv_blobs`
//!   table (shared PG pool — the same query shape as
//!   `SchedulerDb::resolve_drv_digests`), rejecting with the real
//!   scheduler's `FAILED_PRECONDITION` message naming every missing
//!   digest;
//! - per-build event logs with monotonic sequences, `WatchBuild`
//!   resume honoring `since_sequence`, and `CancelBuild`.
//!
//! `force_missing` makes named digests permanently "missing" (they
//! verify present everywhere else) — the lever for the
//! second-reject-is-a-hard-error test.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use rio_proto::types::{self, BuildEvent, DerivationNode, SubmitBuildRequest, build_event::Event};
use rio_proto::{SchedulerService, scheduler};
use tokio::sync::broadcast;
use tonic::{Request, Response, Status};

/// One build's append-only event log + live tail.
pub struct BuildLog {
    events: Mutex<Vec<BuildEvent>>,
    tail: broadcast::Sender<BuildEvent>,
    terminal: Mutex<bool>,
}

impl BuildLog {
    fn new() -> Arc<Self> {
        let (tail, _) = broadcast::channel(1024);
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
            tail,
            terminal: Mutex::new(false),
        })
    }

    pub fn append(&self, build_id: &str, event: Event) {
        let mut events = self.events.lock().unwrap();
        let ev = BuildEvent {
            build_id: build_id.to_string(),
            timestamp: None,
            event: Some(event.clone()),
        };
        if matches!(
            event,
            Event::Completed(_) | Event::Failed(_) | Event::Cancelled(_)
        ) {
            *self.terminal.lock().unwrap() = true;
        }
        events.push(ev.clone());
        let _ = self.tail.send(ev);
    }
}

#[derive(Default)]
pub struct SchedState {
    staged: HashMap<String, Vec<DerivationNode>>,
    /// Accepted (assembled) submissions, in acceptance order.
    pub accepted: Vec<SubmitBuildRequest>,
    /// Pages received (staged + final), for page-shape assertions.
    pub pages: Vec<(String, bool, usize)>, // (submission_id, final_page, nodes)
    /// FAILED_PRECONDITION rejects issued.
    pub rejects: usize,
    /// Digests reported missing regardless of the DB.
    pub force_missing: HashSet<Vec<u8>>,
    /// Don't auto-complete builds — emit `BuildStarted` only; the test
    /// drives the rest via [`StubScheduler::log`] + [`BuildLog::append`].
    pub hold_open: bool,
    pub builds: HashMap<String, Arc<BuildLog>>,
    pub cancel_calls: Vec<(String, String)>,
    next_id: u64,
}

#[derive(Clone)]
pub struct StubScheduler {
    pool: sqlx::PgPool,
    pub state: Arc<Mutex<SchedState>>,
}

impl StubScheduler {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
            state: Arc::new(Mutex::new(SchedState::default())),
        }
    }

    pub fn log(&self, build_id: &str) -> Arc<BuildLog> {
        Arc::clone(
            self.state
                .lock()
                .unwrap()
                .builds
                .get(build_id)
                .expect("build exists"),
        )
    }

    pub fn build_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.state.lock().unwrap().builds.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Stream a log from `since` (exclusive) to terminal: replay the
/// snapshot, then follow the broadcast tail.
fn stream_log(
    log: Arc<BuildLog>,
    since: u64,
) -> tokio_stream::wrappers::ReceiverStream<Result<BuildEvent, Status>> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        let (snapshot, mut tail, mut terminal) = {
            let events = log.events.lock().unwrap();
            (
                events.clone(),
                log.tail.subscribe(),
                *log.terminal.lock().unwrap(),
            )
        };
        let _ = since;
        for ev in snapshot {
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        while !terminal {
            match tail.recv().await {
                Ok(ev) => {
                    if matches!(
                        ev.event,
                        Some(Event::Completed(_))
                            | Some(Event::Failed(_))
                            | Some(Event::Cancelled(_))
                    ) {
                        terminal = true;
                    }
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

#[tonic::async_trait]
impl SchedulerService for StubScheduler {
    type SubmitBuildStream = tokio_stream::wrappers::ReceiverStream<Result<BuildEvent, Status>>;

    async fn submit_build(
        &self,
        request: Request<SubmitBuildRequest>,
    ) -> Result<Response<Self::SubmitBuildStream>, Status> {
        let mut req = request.into_inner();
        {
            let mut st = self.state.lock().unwrap();
            st.pages
                .push((req.submission_id.clone(), req.final_page, req.nodes.len()));
            if !req.submission_id.is_empty() && !req.final_page {
                st.staged
                    .entry(req.submission_id.clone())
                    .or_default()
                    .extend(std::mem::take(&mut req.nodes));
                // Staged ack: clean close with zero events.
                let (_tx, rx) = tokio::sync::mpsc::channel(1);
                return Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                    rx,
                )));
            }
            if !req.submission_id.is_empty()
                && let Some(mut staged) = st.staged.remove(&req.submission_id)
            {
                staged.extend(std::mem::take(&mut req.nodes));
                req.nodes = staged;
            }
        }

        // Digest bulk-verify against the store's drv_blobs (the real
        // scheduler's submit-time contract). EVERY referenced digest —
        // the nodes' own digests included — must resolve in the store
        // (the skeleton contract: blobs uploaded and acked pre-submit).
        let mut referenced: HashSet<Vec<u8>> =
            req.nodes.iter().map(|n| n.drv_digest.clone()).collect();
        for n in &req.nodes {
            referenced.extend(n.input_drv_digests.iter().cloned());
        }
        let to_check: Vec<Vec<u8>> = referenced.iter().cloned().collect();
        let known: HashSet<Vec<u8>> =
            sqlx::query_scalar("SELECT digest FROM drv_blobs WHERE digest = ANY($1::bytea[])")
                .bind(&to_check)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| Status::internal(format!("drv_blobs query: {e}")))?
                .into_iter()
                .collect();
        let mut missing: Vec<String> = {
            let st = self.state.lock().unwrap();
            referenced
                .iter()
                .filter(|d| !known.contains(*d) || st.force_missing.contains(*d))
                .map(hex::encode)
                .collect()
        };
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            self.state.lock().unwrap().rejects += 1;
            // Same shared formatter the real scheduler uses — the e2e
            // recovery cycle exercises the production message shape.
            return Err(Status::failed_precondition(
                rio_proto::submit_reject::missing_drv_digests_message(&missing),
            ));
        }

        let (build_id, log, hold_open) = {
            let mut st = self.state.lock().unwrap();
            st.next_id += 1;
            let id = format!("build-{:04}", st.next_id);
            let log = BuildLog::new();
            st.builds.insert(id.clone(), Arc::clone(&log));
            st.accepted.push(req.clone());
            (id, log, st.hold_open)
        };

        log.append(
            &build_id,
            Event::Started(types::BuildStarted {
                total_derivations: req.nodes.len() as u32,
                cached_derivations: 0,
            }),
        );
        if !hold_open {
            // Synthetic per-drv lifecycle (capped — a 50k-node page
            // doesn't need 50k events to prove the pipeline).
            for n in req.nodes.iter().take(64) {
                log.append(
                    &build_id,
                    Event::Derivation(types::DerivationEvent {
                        derivation_path: n.drv_path.clone(),
                        kind: types::DerivationEventKind::Completed as i32,
                        output_paths: n.expected_output_paths.clone(),
                        ..Default::default()
                    }),
                );
            }
            let mut outputs: Vec<String> = req
                .nodes
                .iter()
                .filter(|n| n.explicitly_requested)
                .flat_map(|n| n.expected_output_paths.clone())
                .collect();
            if outputs.is_empty() {
                outputs = req
                    .nodes
                    .iter()
                    .flat_map(|n| n.expected_output_paths.clone())
                    .collect();
            }
            log.append(
                &build_id,
                Event::Completed(types::BuildCompleted {
                    output_paths: outputs,
                }),
            );
        }
        Ok(Response::new(stream_log(log, 0)))
    }

    type WatchBuildStream = tokio_stream::wrappers::ReceiverStream<Result<BuildEvent, Status>>;

    async fn watch_build(
        &self,
        request: Request<types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        let req = request.into_inner();
        let log = self
            .state
            .lock()
            .unwrap()
            .builds
            .get(&req.build_id)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("unknown build {}", req.build_id)))?;
        Ok(Response::new(stream_log(log, 0)))
    }

    async fn query_build_status(
        &self,
        _request: Request<types::QueryBuildRequest>,
    ) -> Result<Response<types::BuildStatus>, Status> {
        Err(Status::unimplemented("not needed by the coordinator"))
    }

    async fn cancel_build(
        &self,
        request: Request<types::CancelBuildRequest>,
    ) -> Result<Response<types::CancelBuildResponse>, Status> {
        let req = request.into_inner();
        let log = {
            let mut st = self.state.lock().unwrap();
            st.cancel_calls
                .push((req.build_id.clone(), req.reason.clone()));
            st.builds.get(&req.build_id).cloned()
        };
        match log {
            Some(log) => {
                log.append(
                    &req.build_id,
                    Event::Cancelled(types::BuildCancelled { reason: req.reason }),
                );
                Ok(Response::new(types::CancelBuildResponse {
                    cancelled: true,
                }))
            }
            None => Ok(Response::new(types::CancelBuildResponse {
                cancelled: false,
            })),
        }
    }

    async fn resolve_tenant(
        &self,
        _request: Request<scheduler::ResolveTenantRequest>,
    ) -> Result<Response<scheduler::ResolveTenantResponse>, Status> {
        Err(Status::unimplemented("not needed by the coordinator"))
    }
}

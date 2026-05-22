//! `ExecutorService` gRPC implementation for [`SchedulerGrpc`].
//!
//! Worker-facing RPCs: the `BuildExecution` bidirectional stream and
//! the `Heartbeat` unary RPC. Split from `mod.rs` (P0356) — heartbeat
//! bounds-checking and the stream message-dispatch tree change on a
//! schedule independent of the client-facing SchedulerService RPCs.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{info, instrument, warn};

use rio_proto::ExecutorService;

use crate::actor::{ActorCommand, HeartbeatPayload};

use super::SchedulerGrpc;

/// Monotonic per-stream epoch source. Each `BuildExecution` stream gets
/// a fresh epoch on open; the reader task echoes it on
/// `ExecutorDisconnected`. The actor compares against
/// `ExecutorState::stream_epoch` to drop a stale disconnect from a
/// prior stream (I-056a's late-disconnect half — connect-before-
/// disconnect ordering observed live during deploy churn). Process-
/// global (not per-`SchedulerGrpc`) since `SchedulerGrpc` is `Clone`d
/// per-connection and all clones must share the sequence.
static STREAM_EPOCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Upper bound on distinct *accepted* `derivation_path` values one
/// `BuildExecution` stream may push to `LogBuffers`. Per
/// `[Single build per pod, no knob]` a legitimate stream pushes for
/// exactly ONE; 8 covers reassign/retry slop. With `push_for` (the
/// `(executor, drv)` binding gate) a fabricated path never allocates
/// a `LogBuffers` entry, and with the gated `seen_drvs.insert()` a
/// rejected path never grows the recv task's per-stream `seen_drvs:
/// HashSet<String>` either. The cap bounds the *accepted* population
/// — exactly the set that round-trips to the actor's
/// `handle_executor_disconnected` cleanup — and is a defense-in-depth
/// tripwire: if `push_for` or the gated insert ever regressed, this
/// is the only remaining bound on entry *count*. The bound on entry
/// *bytes* is `MAX_DRVS_PER_STREAM * MAX_DERIVATION_PATH_LEN` — the
/// count cap alone stopped bounding memory when `seen_drvs` switched
/// from 32-char `drv_log_hash` keys to full paths (which the
/// disconnect cleanup needs for its `dag.hash_for_path()` lookup).
const MAX_DRVS_PER_STREAM: usize = 8;

// ── Worker-supplied field bounds ─────────────────────────────────────
// r[impl sched.executor.input-bounds+2]
// EVERY string/bytes field a worker can set on the ExecutorService
// surface is listed here with its bound or an explicit reason it is left
// unbounded. Numeric fields are listed with their validation /
// total-arithmetic treatment when the scheduler folds them into persisted
// row metadata or per-execution ordering state; all other numerics stay
// `n/a`. When a field is added to ExecutorMessage / HeartbeatRequest
// or their nested messages, add a row — an unlisted field is a review
// rejection. Three enforcement styles:
//   reject   — drop the message / fail the RPC. For advisory messages
//              (Phase, Ack, LogBatch), for the heartbeat (a rejected
//              heartbeat reaps the worker — the designed recovery), and for
//              CompletionReport.drv_path (an over-bound path can never name
//              a live assignment — see the comment at the recv arm).
//   truncate — bound the field in place, keep the message. For
//              CompletionReport payload fields (the report itself must reach
//              the actor — a lost completion strands the derivation in
//              Running) and for LogBatch fields that must still reach the
//              ring/forward.
//   document — left at the gRPC decode cap, with the verified reason
//              (decoded then dropped before any retention).
//
// BuildExecution stream:
//   ExecutorRegister.executor_id      → executors-map key, log lines      → reject RPC > MAX_IDENT_LEN
//   WorkAssignmentAck.drv_path        → info! interpolation only          → skip arm > MAX_DERIVATION_PATH_LEN
//   WorkAssignmentAck.assignment_token→ never read in the recv arm        → document (decoded then dropped)
//   BuildLogBatch.derivation_path     → seen_drvs, ring key, actor fwd    → reject batch > MAX_DERIVATION_PATH_LEN
//   BuildLogBatch.lines[i]            → ring buffer + Event::Log ring     → truncate to logs::MAX_LINE_LEN
//   BuildLogBatch.executor_id         → retained whole in Event::Log      → truncate to MAX_IDENT_LEN
//   BuildLogBatch.first_line_number   → ring line numbering + drv_logs    → reject batch if non-monotone vs the
//                                       row span arithmetic                 ring's last line or if base+len would
//                                                                           wrap u64 (push_for arms `non_monotonic`
//                                                                           / `line_number_overflow`); flusher span
//                                                                           arithmetic is total and the drv_logs
//                                                                           binds clamp at i64::MAX as the
//                                                                           magnitude backstop
//   CompletionReport.drv_path         → actor hash_for_path lookup        → reject report > MAX_DERIVATION_PATH_LEN
//   CompletionReport.assignment_token → never read in the recv arm        → document (decoded then dropped)
//   CompletionReport.node_name        → build_samples.node_name           → None if > MAX_IDENT_LEN
//   CompletionReport.hw_class         → build_samples.hw_class            → None if > MAX_IDENT_LEN
//   CompletionReport.peak_memory_bytes / peak_cpu_cores → build_samples row → validated actor-side (completion.rs
//                                       record_build_sample: memory .min(i64::MAX) clamp; peak_cpu kept only if
//                                       finite, > 0 and ≤ sla::config::MAX_CORES_HARD, else NULL "not reported")
//   CompletionReport.final_resources.{cpu_limit_cores, cpu_seconds_total, peak_io_pressure_pct, peak_disk_bytes}
//                                     → same build_samples row            → validated actor-side (completion.rs
//                                       record_build_sample: floats kept only if finite and in-domain (cores > 0
//                                       and ≤ MAX_CORES_HARD, seconds ≥ 0 — magnitude not otherwise bounded,
//                                       pct ∈ [0,100]), else NULL; a kept cpu_limit_cores is still min()'d with
//                                       the dispatch intent; peak_disk_bytes .min(i64::MAX) clamp)
//   CompletionReport.final_resources.{cpu_fraction, memory_used_bytes, memory_total_bytes, disk_used_bytes,
//                                     disk_total_bytes}                   → numeric → n/a (decoded, forwarded to
//                                       the actor, dropped at the build_samples fold — never persisted)
//   BuildResult.start_time / stop_time → build_samples.duration_secs      → validated actor-side (domain.rs
//                                       duration(): out-of-order / out-of-range timestamps → None; completion.rs
//                                       0 < d < 30 days gate, else no sample row is written)
//   BuildResult.error_msg             → build_event_log × N, ring, term   → truncate to MAX_ERROR_MSG_LEN
//   BuildResult.built_outputs[].name/path/hash → PG realisations          → validated actor-side (declared-output
//                                       membership + StorePath::parse + [u8;32]); the pre-validation mailbox
//                                       transit is a documented transient residual
//   BuildPhase.derivation_path        → actor hash_for_path lookup        → reject phase > MAX_DERIVATION_PATH_LEN
//   BuildPhase.phase                  → build_event_log × N, ring, term   → reject phase > MAX_PHASE_LEN
//   PrefetchComplete.*                → numeric                           → n/a
// Heartbeat RPC:
//   executor_id, intent_id            → executor-lifetime actor state     → reject RPC > MAX_IDENT_LEN
//   systems[i], supported_features[i] → executor-lifetime actor state     → reject RPC > MAX_IDENT_LEN each
//   running_build                     → hash_for_path lookup              → reject RPC > MAX_DERIVATION_PATH_LEN
//   resources / kind / flags          → numeric                           → n/a

/// Upper bound on the byte length of a worker-supplied
/// `derivation_path` (`BuildLogBatch` and `BuildPhase`). A legitimate
/// Nix store path is at most ~259 bytes — `/nix/store/` (11) + 32-char
/// hash + `-` + ≤211-char name (the protocol-level store-path name
/// limit) + `.drv` — so 512 is generous margin that never affects real
/// traffic. The proto `string` field is otherwise bounded only by
/// `max_decoding_message_size` (256 MiB): without this check a
/// compromised worker assigned drv `{H}` can send
/// `"/nix/store/{H}-" + ~255 MiB` aliases that pass `push_for`'s
/// binding gate (`drv_log_hash` normalizes the alias back to `{H}`)
/// and pin `MAX_DRVS_PER_STREAM × 255 MiB ≈ 2 GiB` of resident
/// `String` in `seen_drvs` per stream — then ship the whole set to the
/// actor's single-threaded mailbox on disconnect. Checked at the top
/// of both recv arms, before the path is cloned, hashed, or forwarded.
pub(super) const MAX_DERIVATION_PATH_LEN: usize = 512;

/// Upper bound on the byte length of a worker-supplied `BuildPhase.phase`.
/// A legitimate phase name (`unpackPhase`, `buildPhase`, a custom
/// `runPhase` hook name) is tens of bytes; 256 never affects real
/// traffic. Unlike `Log`, `Event::Phase` is NOT `display_only` (event.rs):
/// after the actor's `(executor, drv)` binding gate it is cloned per
/// interested build, prost-encoded into a `build_event_log` row, pinned
/// in the per-build state broadcast ring, and rendered verbatim into
/// every interested tenant's `nix build -L` terminal as `SetPhase` — so
/// the 256 MiB message cap alone gives a hostile assigned worker a
/// `N_builds × (1 PG row + 1 ring slot + 1 terminal flood)` amplifier.
/// An over-long phase drops the UPDATE (cosmetic: nom misses one phase
/// column refresh), never the build.
pub(super) const MAX_PHASE_LEN: usize = 256;

/// Upper bound on worker-supplied identifier/label fields: `executor_id`
/// (stream register + heartbeat + log batch), `node_name`, `hw_class`,
/// `intent_id`, and each element of `systems` / `supported_features`. All
/// are either k8s object names (≤253 bytes by the DNS-subdomain rule),
/// UUIDs, or Nix system/feature strings (tens of bytes). They live for
/// the executor entry's lifetime in the actor's `executors` map, are
/// interpolated into log lines, ride the per-build log broadcast ring
/// inside `Event::Log`, or land in `build_samples` rows.
pub(super) const MAX_IDENT_LEN: usize = 256;

/// Upper bound on a worker-supplied `BuildResult.error_msg`. Truncated
/// (not rejected — a dropped `CompletionReport` strands the derivation in
/// `Running`). A legitimate daemon/executor error is well under 16 KiB;
/// the field is fanned out as `DerivationEvent::failed.error_message` to
/// `(1 + cascaded_ancestors) × interested_builds` `build_event_log` rows,
/// state-ring slots, and `nix build -L` terminals.
///
/// Head-truncation cannot break the scheduler's semantic dispatch on this
/// field: `handle_infrastructure_failure` greps for `CGROUP_OOM_MSG` and
/// `CONCURRENT_PUTPATH_MSG`, both short builder-constructed prefixes that
/// sit in the first few hundred bytes of a legitimate message. A hostile
/// builder padding the marker past 16 KiB only denies itself the
/// resource-floor bump.
pub(super) const MAX_ERROR_MSG_LEN: usize = 16 * 1024;

#[tonic::async_trait]
impl ExecutorService for SchedulerGrpc {
    type BuildExecutionStream = ReceiverStream<Result<rio_proto::types::SchedulerMessage, Status>>;

    // r[impl proto.stream.bidi]
    #[instrument(skip(self, request), fields(rpc = "BuildExecution"))]
    async fn build_execution(
        &self,
        request: Request<tonic::Streaming<rio_proto::types::ExecutorMessage>>,
    ) -> Result<Response<Self::BuildExecutionStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        // r[impl sec.executor.identity-token+2]
        // Bind this stream to the HMAC-attested intent the pod was
        // spawned for. A compromised builder cannot mint a token for
        // another pod's intent → cannot hijack its `stream_tx` (the
        // actor rejects on `auth_intent` mismatch) → cannot receive
        // its `WorkAssignment.assignment_token` → cannot poison its
        // outputs. `None` in dev mode (no HMAC key configured).
        let auth_intent = self.require_executor(&request)?.map(|c| c.intent_id);
        let mut stream = request.into_inner();

        // The first message MUST be a ExecutorRegister with the executor_id.
        // This ensures the stream and heartbeat use the same identity.
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty BuildExecution stream"))?;
        let executor_id = match first.msg {
            Some(rio_proto::types::executor_message::Msg::Register(reg)) => {
                if reg.executor_id.is_empty() {
                    return Err(Status::invalid_argument(
                        "ExecutorRegister.executor_id is empty",
                    ));
                }
                // r[impl sched.executor.input-bounds+2]
                rio_common::grpc::check_bound(
                    "executor_id bytes",
                    reg.executor_id.len(),
                    MAX_IDENT_LEN,
                )?;
                reg.executor_id
            }
            _ => {
                return Err(Status::invalid_argument(
                    "first BuildExecution message must be ExecutorRegister",
                ));
            }
        };
        // `executor_id` is body-supplied (not in `ExecutorClaims`); the
        // actor-side `auth_intent` checks (reconnect intent-mismatch +
        // heartbeat spoof guard) are the identity binding.
        info!(executor_id = %executor_id, "worker stream opened");

        // Create the internal channel for the actor to send SchedulerMessages to this worker.
        let (actor_tx, mut actor_rx) = mpsc::channel::<rio_proto::types::SchedulerMessage>(256);

        // Create the output channel wrapping messages in Result for tonic.
        let (output_tx, output_rx) =
            mpsc::channel::<Result<rio_proto::types::SchedulerMessage, Status>>(256);

        // Per-stream epoch: starts at 1 (0 = "no stream yet" in
        // ExecutorState::new). Captured into the reader closure below
        // and echoed on ExecutorDisconnected.
        let stream_epoch = STREAM_EPOCH_SEQ.fetch_add(1, Ordering::Relaxed) + 1;

        // Register the worker stream with the actor (blocking send — must not drop).
        // r[impl sec.executor.identity-token+2]
        // Accept-gate: `executor_id` is body-supplied (`ExecutorClaims`
        // can't carry it — the scheduler signs at SpawnIntent emission,
        // before the controller picks a pod name). The actor binds
        // `auth_intent ↔ executor_id` and rejects on live-stream /
        // intent-mismatch; we MUST learn that decision BEFORE spawning
        // the bridge + reader below. Without this gate, a spoofed
        // `Register{executor_id=E_victim}` is rejected actor-side but
        // the reader keeps forwarding `ProcessCompletion{E_victim,
        // D_victim}` — `handle_completion`'s stale-report guard checks
        // `assigned_executor == executor_id`, which the attacker
        // spoofed exactly → forged terminal result for another
        // tenant's build.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.actor
            .send_unchecked(ActorCommand::ExecutorConnected {
                executor_id: executor_id.as_str().into(),
                stream_tx: actor_tx,
                stream_epoch,
                auth_intent,
                reply: reply_tx,
            })
            .await
            .map_err(Self::actor_error_to_status)?;
        reply_rx
            .await
            .map_err(|_| Status::internal("actor dropped ExecutorConnected reply"))?
            .map_err(|reason| {
                Status::permission_denied(format!("ExecutorConnected rejected: {reason}"))
            })?;

        // Bridge actor_rx -> output_tx, wrapping in Ok()
        rio_common::task::spawn_monitored("build-exec-bridge", async move {
            while let Some(msg) = actor_rx.recv().await {
                if output_tx.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });

        // Spawn a task to read worker messages and forward to the actor
        let actor_for_recv = self.actor.clone();
        let log_buffers = Arc::clone(&self.log_buffers);
        let executor_id_for_recv = executor_id.clone();
        // r[impl sched.lease.standby-drops-writes]
        // Generation-fence the stream: capture the lease generation at
        // open-time. `ensure_leader()` above only checks at open; the
        // reader loop below sends `ProcessCompletion`/`PrefetchComplete`
        // via `send_unchecked` for the stream's lifetime. If the lease
        // is lost (or flapped) mid-stream, an ex-leader would otherwise
        // forward a `CompletionReport` and write terminal PG state for
        // a generation it no longer owns. Breaking the loop closes the
        // stream → worker reconnects to the new leader.
        let is_leader = Arc::clone(&self.is_leader);
        let generation = Arc::clone(&self.generation);
        let stream_gen = generation.load(std::sync::atomic::Ordering::Acquire);

        rio_common::task::spawn_monitored("worker-stream-reader", async move {
            let mut seen_drvs: std::collections::HashSet<String> = std::collections::HashSet::new();
            let stream_is_stale = || {
                !is_leader.load(std::sync::atomic::Ordering::SeqCst)
                    || generation.load(std::sync::atomic::Ordering::Acquire) != stream_gen
            };
            loop {
                let msg = match stream.message().await {
                    Ok(Some(m)) => m,
                    Ok(None) => break, // clean disconnect
                    Err(e) => {
                        warn!(
                            executor_id = %executor_id_for_recv,
                            error = %e,
                            "worker stream read error, treating as disconnect"
                        );
                        break;
                    }
                };
                if let Some(inner) = msg.msg {
                    match inner {
                        rio_proto::types::executor_message::Msg::Register(_) => {
                            warn!(
                                executor_id = %executor_id_for_recv,
                                "duplicate ExecutorRegister on established stream, ignoring"
                            );
                        }
                        rio_proto::types::executor_message::Msg::Ack(ack) => {
                            // Only consumer is the info! below — but that
                            // interpolates the worker-supplied path into a JSON
                            // log line, and a worker can send Acks at line rate.
                            // No counter: the Ack has no consumer beyond this log
                            // line, so there is no behavior to alert on — the
                            // debug! is the parity-with-other-arms observability.
                            // r[impl sched.executor.input-bounds+2]
                            if ack.drv_path.len() > MAX_DERIVATION_PATH_LEN {
                                tracing::debug!(
                                    executor_id = %executor_id_for_recv,
                                    len = ack.drv_path.len(),
                                    "ignoring assignment ack: drv_path too long"
                                );
                                continue;
                            }
                            info!(
                                executor_id = %executor_id_for_recv,
                                drv_path = %ack.drv_path,
                                "worker acknowledged assignment"
                            );
                        }
                        rio_proto::types::executor_message::Msg::PrefetchComplete(pc) => {
                            if stream_is_stale() {
                                info!(
                                    executor_id = %executor_id_for_recv,
                                    "lease lost/flapped mid-stream; closing worker stream"
                                );
                                break;
                            }
                            // r[sched.assign.warm-gate]: worker ACKed
                            // the initial PrefetchHint. Forward to
                            // the actor which flips ExecutorState.warm.
                            // send_unchecked (not try_send): dropping
                            // this under backpressure would leave a
                            // warmed worker permanently cold in the
                            // scheduler's view — idle capacity right
                            // when the scheduler is saturated.
                            if actor_for_recv
                                .send_unchecked(ActorCommand::PrefetchComplete {
                                    executor_id: executor_id_for_recv.clone().into(),
                                    paths_fetched: pc.paths_fetched,
                                })
                                .await
                                .is_err()
                            {
                                warn!("actor channel closed while sending PrefetchComplete");
                                break;
                            }
                        }
                        rio_proto::types::executor_message::Msg::Completion(mut report) => {
                            if stream_is_stale() {
                                info!(
                                    executor_id = %executor_id_for_recv,
                                    "lease lost/flapped mid-stream; closing worker stream"
                                );
                                break;
                            }
                            // Bound the worker-supplied path before it crosses into
                            // the actor (same threat as the Phase/LogBatch arms: one
                            // ~255 MiB mailbox transit + one actor-thread hash). A
                            // real store path is ≤259 bytes, so a >512-byte path can
                            // never name a live assignment — the actor would drop
                            // this report as "completion for unknown derivation"
                            // anyway; rejecting it here only moves that drop off the
                            // single-threaded event loop. Both paths leave
                            // running_build to the heartbeat reconcile (the actor's
                            // unknown-drv return precedes the free-worker-capacity
                            // block), so no behavior change. No legitimate
                            // completion is lost.
                            // r[impl sched.executor.input-bounds+2]
                            if report.drv_path.len() > MAX_DERIVATION_PATH_LEN {
                                tracing::debug!(
                                    executor_id = %executor_id_for_recv,
                                    len = report.drv_path.len(),
                                    "rejected completion report: derivation_path too long"
                                );
                                metrics::counter!(
                                    "rio_scheduler_completions_rejected_total",
                                    "reason" => "path_too_long"
                                )
                                .increment(1);
                                continue;
                            }
                            let drv_path = std::mem::take(&mut report.drv_path);
                            // A CompletionReport with result: None is malformed, but
                            // we must not silently drop it — the derivation would hang
                            // in Running forever. Synthesize an InfrastructureFailure.
                            let mut result = report.result.unwrap_or_else(|| {
                                warn!(
                                    executor_id = %executor_id_for_recv,
                                    drv_path = %drv_path,
                                    "completion with None result, synthesizing InfrastructureFailure"
                                );
                                rio_proto::types::BuildResult {
                                    status:
                                        rio_proto::types::BuildResultStatus::InfrastructureFailure
                                            .into(),
                                    error_msg: "worker sent CompletionReport with no result"
                                        .into(),
                                    ..Default::default()
                                }
                            });
                            // Bound-don't-reject: this message must reach the actor
                            // (a dropped completion strands the drv in Running), so
                            // oversized fields are truncated/nulled instead of
                            // failing the whole report.
                            // r[impl sched.executor.input-bounds+2]
                            rio_common::grpc::truncate_utf8(
                                &mut result.error_msg,
                                MAX_ERROR_MSG_LEN,
                            );
                            // Use blocking send for completion — dropping it would
                            // leave the derivation stuck in Running.
                            if actor_for_recv
                                .send_unchecked(ActorCommand::ProcessCompletion {
                                    executor_id: executor_id_for_recv.clone().into(),
                                    drv_key: drv_path,
                                    result,
                                    peak_memory_bytes: report.peak_memory_bytes,
                                    peak_cpu_cores: report.peak_cpu_cores,
                                    // Pod-identity stamps headed for build_samples
                                    // rows. Oversized → None, the existing "old
                                    // executor / unknown hw" path; the completion
                                    // itself is unaffected.
                                    node_name: report
                                        .node_name
                                        .filter(|s| s.len() <= MAX_IDENT_LEN),
                                    hw_class: report.hw_class.filter(|s| s.len() <= MAX_IDENT_LEN),
                                    final_resources: report.final_resources,
                                })
                                .await
                                .is_err()
                            {
                                warn!("actor channel closed while sending completion");
                                break;
                            }
                        }
                        rio_proto::types::executor_message::Msg::Phase(phase) => {
                            // Length-bound the worker-supplied path before it
                            // crosses into the actor: `handle_forward_phase`
                            // hashes it for the `dag.hash_for_path()` lookup
                            // on the single-threaded event loop. Not
                            // accumulated like `seen_drvs`, but the same
                            // untrusted field. r[impl sched.log.path-length]
                            if phase.derivation_path.len() > MAX_DERIVATION_PATH_LEN {
                                tracing::debug!(
                                    len = phase.derivation_path.len(),
                                    "rejected phase update: derivation_path too long"
                                );
                                metrics::counter!(
                                    "rio_scheduler_phases_rejected_total",
                                    "reason" => "path_too_long"
                                )
                                .increment(1);
                                continue;
                            }
                            // Sibling axis of the path check above: `phase.phase`
                            // is the OTHER worker-supplied string in this message,
                            // and unlike the path it is accumulated — see
                            // MAX_PHASE_LEN. Same reject-don't-truncate policy
                            // as the path: a >256-byte phase name is a hostile or
                            // broken worker, and a dropped phase update is
                            // cosmetic. r[impl sched.executor.input-bounds+2]
                            if phase.phase.len() > MAX_PHASE_LEN {
                                tracing::debug!(
                                    len = phase.phase.len(),
                                    "rejected phase update: phase text too long"
                                );
                                metrics::counter!(
                                    "rio_scheduler_phases_rejected_total",
                                    "reason" => "phase_too_long"
                                )
                                .increment(1);
                                continue;
                            }
                            // Same try_send semantics as ForwardLogBatch:
                            // a dropped phase update is cosmetic (nom
                            // misses one phase column refresh), not a hang.
                            //
                            // (executor, drv) binding is checked actor-side
                            // in handle_forward_phase — Phase has no
                            // ring-buffer write to colocate a recv check
                            // with. r[sched.log.phase-binding].
                            if actor_for_recv
                                .try_send(ActorCommand::ForwardPhase {
                                    phase,
                                    executor_id: executor_id_for_recv.clone().into(),
                                })
                                .is_err()
                            {
                                metrics::counter!("rio_scheduler_log_forward_dropped_total")
                                    .increment(1);
                            }
                        }
                        rio_proto::types::executor_message::Msg::LogBatch(mut log) => {
                            // Length-bound the worker-supplied path BEFORE the
                            // cap check, the binding gate, and the
                            // `seen_drvs.insert()` clone. The binding gate
                            // verifies the path's *normalized hash component*
                            // (`drv_log_hash` collapses `"{H}-<anything>"` to
                            // `{H}`), so a `"{H}-" + 255 MiB` alias for an
                            // assigned drv passes it — the length check is the
                            // only thing standing between that alias and a
                            // ~255 MiB resident `String` in `seen_drvs`.
                            // r[impl sched.log.path-length]
                            if log.derivation_path.len() > MAX_DERIVATION_PATH_LEN {
                                tracing::debug!(
                                    len = log.derivation_path.len(),
                                    "rejected log batch: derivation_path too long"
                                );
                                metrics::counter!(
                                    "rio_scheduler_log_batches_rejected_total",
                                    "reason" => "path_too_long"
                                )
                                .increment(1);
                                continue;
                            }
                            // Bound the batch's remaining worker-supplied fields
                            // BEFORE both consumers (ring buffer + actor forward →
                            // per-build log ring → WatchBuild wire). push_into()
                            // also truncates lines, but (a) it clones the full
                            // line first and Vec::truncate keeps the oversized
                            // capacity, so the ring's byte cap counts 64 KiB while
                            // holding a 255 MiB allocation, and (b) the
                            // ForwardLogBatch → Event::Log path doesn't go through
                            // push_into at all — it clones the ORIGINAL proto
                            // (including executor_id, which nothing reads but
                            // everything retains) per interested build.
                            // r[impl sched.executor.input-bounds+2]
                            for line in &mut log.lines {
                                if line.len() > crate::logs::MAX_LINE_LEN {
                                    line.truncate(crate::logs::MAX_LINE_LEN);
                                    line.shrink_to_fit();
                                }
                            }
                            rio_common::grpc::truncate_utf8(&mut log.executor_id, MAX_IDENT_LEN);
                            // Two-step: buffer (never blocks on actor), then forward.
                            //
                            // 0. Per-stream distinct-path cap. The worker is NOT
                            //    trusted. The cap is checked BEFORE `push_for`, but
                            //    the INSERT is gated on `accepted` (below): a path the
                            //    binding gate refused is unverified worker input and
                            //    MUST NOT round-trip into the actor's
                            //    `handle_executor_disconnected` cleanup — which would
                            //    let a fabricated suffix for a victim's hash {H} fail
                            //    the cleanup's `dag.hash_for_path()` exact-string gate
                            //    while `discard()` re-normalizes to {H} and removes the
                            //    victim's live buffer. r[impl sched.log.batch-binding]
                            //
                            //    Because rejected paths never enter `seen_drvs`, the
                            //    cap no longer throttles a 100%-rejected flood — that
                            //    flood is bounded only by `push_for`'s per-batch reject
                            //    cost (string parse, DashMap lookup, debug log, metric;
                            //    no allocation, no actor send), which a worker could
                            //    already pay by re-sending the same 8 rejected paths
                            //    under the old code. The cap bounds the *accepted*
                            //    population, which is exactly what the cleanup acts on.
                            //
                            //    `seen_drvs` holds the FULL `derivation_path` (not the
                            //    32-char `drv_log_hash`) because the cleanup looks
                            //    each entry up via `dag.hash_for_path()` — a map keyed
                            //    on full store paths. A bare hash would never match,
                            //    degrading the cleanup to "discard EVERY path the
                            //    stream touched."
                            if !seen_drvs.contains(&log.derivation_path)
                                && seen_drvs.len() >= MAX_DRVS_PER_STREAM
                            {
                                metrics::counter!("rio_scheduler_log_unknown_drv_dropped_total")
                                    .increment(1);
                                continue;
                            }
                            // 1. Ring buffer write — direct, no actor involvement.
                            //    This is the durability path: even if the actor is
                            //    backpressured or the gateway stream lags, the lines
                            //    land here and are serveable via AdminService.
                            //
                            //    push_for, not push: enforces the (executor, drv)
                            //    binding. Drops batches from executors not assigned
                            //    this drv (compromised builder spamming a fabricated
                            //    derivation_path; late batch after re-dispatch landing
                            //    after discard_log_buffer, where push()'s or_default()
                            //    would create an unstamped entry attributed to the
                            //    NEXT exec_id). The `seen_drvs` MAX_DRVS_PER_STREAM cap
                            //    above is a per-stream DoS bound; this is a per-batch
                            //    correctness gate. They're complementary, not redundant.
                            //    r[impl sched.log.batch-binding]
                            let accepted = log_buffers.push_for(
                                &log.derivation_path,
                                &log,
                                executor_id_for_recv.as_str(),
                            );

                            // 2. Gateway forward — via actor (it owns the
                            //    drv_path→hash→interested_builds resolution and the
                            //    broadcast senders). `try_send`, NOT send_unchecked:
                            //    if the actor channel is backpressured (80% full,
                            //    hysteresis), we drop the gateway-forward. The ring
                            //    buffer already has the lines; the gateway misses
                            //    *live* logs but can still get them via AdminService.
                            //
                            //    This is the opposite tradeoff from ProcessCompletion
                            //    (which MUST use send_unchecked — a dropped completion
                            //    leaves a derivation stuck Running forever). A dropped
                            //    log batch is a degraded-mode nuisance, not a hang.
                            //
                            //    Gated on `accepted`: a batch the binding gate dropped
                            //    is unverified worker input — fanning it out to
                            //    interested builds' live `nix build -L` streams would
                            //    let a compromised executor inject log lines into another
                            //    drv's tail. Same `r[sched.log.batch-binding]` invariant
                            //    as the ring-buffer write; both consumers of the
                            //    worker-supplied `derivation_path` must respect the gate.
                            if accepted {
                                // Bind-gate verified ⟹ this path identifies an
                                // assignment held by THIS stream. Safe to round-trip
                                // through the disconnect cleanup. Idempotent on
                                // already-seen.
                                seen_drvs.insert(log.derivation_path.clone());
                                let drv_path = log.derivation_path.clone();
                                if actor_for_recv
                                    .try_send(ActorCommand::ForwardLogBatch {
                                        drv_path,
                                        batch: log,
                                    })
                                    .is_err()
                                {
                                    metrics::counter!("rio_scheduler_log_forward_dropped_total")
                                        .increment(1);
                                }
                            }
                        }
                    }
                }
            }

            // Stream closed: worker disconnected. Use blocking send — if this
            // is dropped due to backpressure, running derivations won't be
            // reassigned and will hang forever. `seen_drvs` is forwarded
            // so the actor can do the log-buffer cleanup AFTER the epoch
            // check and with DAG-ownership awareness — doing it here
            // (pre-epoch-gate, branching on `is_sealed`) raced the
            // actor's seal (TOCTOU), let a stale reader wipe a
            // reconnected stream's fresh buffer, and let a compromised
            // worker discard a victim's buffer.
            if actor_for_recv
                .send_unchecked(ActorCommand::ExecutorDisconnected {
                    executor_id: executor_id_for_recv.into(),
                    stream_epoch,
                    seen_drvs: seen_drvs.into_iter().collect(),
                })
                .await
                .is_err()
            {
                warn!("actor channel closed while sending worker disconnect");
            }
        });

        Ok(Response::new(ReceiverStream::new(output_rx)))
    }

    #[instrument(skip(self, request), fields(rpc = "Heartbeat"))]
    async fn heartbeat(
        &self,
        request: Request<rio_proto::types::HeartbeatRequest>,
    ) -> Result<Response<rio_proto::types::HeartbeatResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        // r[impl sec.executor.identity-token+2]
        let auth_claims = self.require_executor(&request)?;
        let req = request.into_inner();

        if req.executor_id.is_empty() {
            return Err(Status::invalid_argument("executor_id is required"));
        }
        // Body `intent_id` and `kind` MUST equal the token's, so what
        // reaches the actor is cryptographically attested. `worker.kind`
        // is what `hard_filter` reads for the FOD/non-FOD airgap split —
        // a compromised open-egress Fetcher cannot heartbeat
        // `kind=Builder` and receive non-FOD builds with secret inputs
        // (its CNP stays wide open; only the work routed to it would
        // change). The actor then binds `hb.intent_id` to the target
        // executor's stored `auth_intent` (set at connect from THAT
        // executor's token): a compromised pod A heartbeating as B with
        // A's own intent passes THIS check (X==X) but fails the
        // actor-side check (B's auth_intent=Y ≠ X). `executor_id` is
        // body-supplied and unbound here; the actor-side `auth_intent`
        // check is the identity binding.
        if let Some(ref c) = auth_claims {
            if req.intent_id != c.intent_id {
                return Err(Status::unauthenticated(
                    "heartbeat intent_id does not match x-rio-executor-token",
                ));
            }
            if req.kind != c.kind {
                return Err(Status::unauthenticated(
                    "heartbeat kind does not match x-rio-executor-token",
                ));
            }
        }

        // Bound heartbeat payload sizes. Heartbeats bypass backpressure
        // (send_unchecked below), so unbounded payloads from a
        // malicious/buggy worker would stall the actor event loop with
        // no backpressure signal.
        const MAX_HEARTBEAT_FEATURES: usize = 64;
        // A worker advertising thousands of systems is buggy or
        // hostile. 16 covers native + linux-builder + the four
        // cross-arch targets × two OSes.
        const MAX_HEARTBEAT_SYSTEMS: usize = 16;
        rio_common::grpc::check_bound("systems", req.systems.len(), MAX_HEARTBEAT_SYSTEMS)?;
        rio_common::grpc::check_bound(
            "supported_features",
            req.supported_features.len(),
            MAX_HEARTBEAT_FEATURES,
        )?;
        // Element-length bounds for the same reason as the count bounds
        // above: heartbeats bypass backpressure (send_unchecked) and the
        // payload lives on ExecutorState for the executor's lifetime.
        // Rejecting a hostile heartbeat is the designed recovery path —
        // the worker times out and is reaped; nothing is stranded.
        // r[impl sched.executor.input-bounds+2]
        rio_common::grpc::check_bound("executor_id bytes", req.executor_id.len(), MAX_IDENT_LEN)?;
        rio_common::grpc::check_bound("intent_id bytes", req.intent_id.len(), MAX_IDENT_LEN)?;
        if let Some(rb) = &req.running_build {
            rio_common::grpc::check_bound(
                "running_build bytes",
                rb.len(),
                MAX_DERIVATION_PATH_LEN,
            )?;
        }
        for s in &req.systems {
            rio_common::grpc::check_bound("system bytes", s.len(), MAX_IDENT_LEN)?;
        }
        for f in &req.supported_features {
            rio_common::grpc::check_bound("supported_feature bytes", f.len(), MAX_IDENT_LEN)?;
        }

        // intent_id: empty-string in proto → None. Proto doesn't have
        // Option for strings; empty is the conventional "unset." Empty
        // = Static-sized pod (no SpawnIntent annotation on the pod
        // template).
        let intent_id = (!req.intent_id.is_empty()).then_some(req.intent_id);

        // kind: prost encodes enums as i32; decode via try_from.
        // Unknown value (future proto version) → Builder (safe default:
        // an unrecognized-kind executor won't receive FODs, so no
        // airgap violation). 0 = Builder (wire default for pre-ADR-019
        // executors that don't send this field). In HMAC mode, prefer
        // the attested `claims.kind` over the body so there is nothing
        // to lie about; the bind above already rejected a mismatch.
        let kind = rio_proto::types::ExecutorKind::try_from(
            auth_claims.as_ref().map_or(req.kind, |c| c.kind),
        )
        .unwrap_or(rio_proto::types::ExecutorKind::Builder);

        let cmd = ActorCommand::Heartbeat(HeartbeatPayload {
            executor_id: req.executor_id.into(),
            systems: req.systems,
            supported_features: req.supported_features,
            running_build: req.running_build,
            resources: req.resources,
            store_degraded: req.store_degraded,
            draining: req.draining,
            kind,
            intent_id,
        });

        // Heartbeats bypass backpressure: dropping a heartbeat under load
        // would cause a false worker timeout -> reassignment -> more load.
        // Same pattern as ExecutorConnected/ExecutorDisconnected.
        self.actor
            .send_unchecked(cmd)
            .await
            .map_err(Self::actor_error_to_status)?;

        Ok(Response::new(rio_proto::types::HeartbeatResponse {
            accepted: true,
            // r[impl sched.lease.claim-before-advertise]
            // The generation advertised here is gated on recovery
            // completion: 0 during the recovery window (the proto-unset
            // sentinel — workers' fetch_max latch treats it as no
            // information), the post-recovery generation after. Same
            // Arc<AtomicU64> the actor reads for
            // WorkAssignment.generation (dispatch.rs single-load); the
            // lease task writes it on each leadership acquisition, and
            // recovery's PG-floor seed can raise it.
            // Non-K8s mode: always-leader state is constructed with
            // recovery already complete, so this stays the raw value
            // (1) there. The RPC itself stays available during recovery
            // on purpose — rejecting heartbeats would break executor
            // re-registration and readiness while the new leader
            // recovers; only the generation payload is withheld.
            generation: self.actor.advertised_generation(),
        }))
    }
}

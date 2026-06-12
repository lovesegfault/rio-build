//! The `LogService` gRPC handlers: `AppendLog` (authenticated builder
//! ingest) and `TailLog` (read/follow for the gateway, dashboard, and
//! CLI).
//!
//! This module is the wiring layer over the rest of `logs::*`: the
//! [`gate`] authorizes a stream open, [`sessions`]
//! (super::sessions) bounds one live ingest per execution and routes
//! readers to it, [`ingest`](super::ingest) owns the per-stream buffer
//! and chunk cutter, and [`tail`] reassembles a log from
//! the chunk manifest. The handlers add: token verification, admission
//! (the per-replica stream-count and buffer-byte budgets), the
//! `select!` loop that drives ingest, the per-replica registry that
//! lets a `TailLog` reader find a live ingest buffer, and the
//! history→live seam.
//!
//! # Trust model
//!
//! `AppendLog` callers are untrusted builder pods presenting an
//! HMAC-signed assignment token; everything they send is bounded and
//! authorized per-stream. `TailLog` is **tenant-authenticated**: the
//! per-method credential layer ([`crate::authz`]) requires a verified
//! tenant token whenever a JWT pubkey is configured, and the handler
//! additionally requires build-membership ownership of the requested
//! execution (assignments→build_derivations→builds.tenant_id, with a
//! swept-assignment arm keyed on the execution's own recorded hash;
//! deny is absence-shaped — see [`tail::authorize_tail`]). The
//! gateway relay forwards the watching caller's session token; the
//! dashboard is registry-declared KeylessOnly (owner decision Q1,
//! 2026-06-04 — its Logs tab surfaces the terminal `authRequired`
//! state in jwt-enabled deployments); `rio-cli
//! logs` sends `--tenant-token`/`RIO_TENANT_TOKEN`. Builder/fetcher
//! network policy pins an L7 allow-list that excludes `TailLog`, so an
//! untrusted build cannot even reach the method. (The pre-authz
//! posture — network reachability as the only read gate — is retired;
//! bug_290.)

use std::sync::Arc;
use std::sync::Mutex;

use dashmap::DashMap;
use sqlx::PgPool;
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, instrument, warn};
use uuid::Uuid;

use rio_common::grpc::StatusExt;
use rio_proto::store::log_service_client::LogServiceClient;
use rio_proto::store::log_service_server::LogService;
use rio_proto::store::{
    AppendLogAck, AppendLogHeader, AppendLogRequest, TailLogChunk, TailLogRequest,
    append_log_request,
};

use super::chunks::LogChunkStore;
use super::gate::{self, OpenCaps};
use super::ingest::{
    AbortReason, AcceptOutcome, CutCommit, CutError, FanBatch, IngestConfig, IngestSession,
    IngestShared,
};
use super::sessions::{self, Acquire};
use super::tail::{self, LineCursor};
use rio_log_kernel::{ChunkVisit, GapProvenance, visit_chunk, visit_fanout_batch};

/// One live ingest registration: the shared buffer handle a `TailLog`
/// reader subscribes to, plus the cancel token that tears the driver
/// down when a NEWER session for the same execution displaces it. The
/// token (not the 15 s lease heartbeat) is what returns the displaced
/// driver's admission permits in O(task wakeup) — without it a
/// reconnect-heavy builder could pin `2 × cut_threshold` bytes per
/// abandoned driver for up to a heartbeat interval each
/// (merged_bug_207's admission-DoS half).
pub(super) struct IngestEntry {
    pub(super) shared: Arc<Mutex<IngestShared>>,
    pub(super) cancel: rio_common::signal::Token,
}

/// Max bytes of `AppendLogHeader.derivation_path` accepted. Ported from
/// the scheduler's recv-task gate (`executor_service.rs::
/// MAX_DERIVATION_PATH_LEN`, which is `pub(super)` there); the value is
/// part of the worker-facing contract. A legitimate `.drv` path is
/// ~100 bytes; 512 is generous headroom without letting a hostile
/// builder embed megabytes in a field that ends up in error messages
/// and chunk-key derivation.
const MAX_DERIVATION_PATH_LEN: usize = 512;

/// Max lines per `TailLogChunk` response message. Bounds the per-message
/// allocation on both ends without changing the data (a reader
/// concatenates messages). Matches the chunking of the scheduler's
/// (removed) `GetDerivationLogs` so the dashboard's per-message
/// handling carries over.
const TAIL_CHUNK_LINES: usize = 256;

/// Capacity of one TailLog subscriber's fan-out queue, in batches. At
/// the builder's 64-line/100 ms batch cadence this absorbs ~25 s of a
/// stalled reader before batches start dropping. A drop is NOT a lost
/// span for the reader: the serve loop observes the forward jump and
/// back-fills from manifest ∪ live buffer in-stream
/// (store.log.tail-fanout-recovery) — the capacity bounds memory and
/// the cost of a drop is one recovery pool read, not missing lines.
const TAIL_SUBSCRIBER_QUEUE: usize = 256;

/// gRPC metadata key marking a `TailLog` request as already having been
/// forwarded once by another store replica. A replica receiving a
/// request with this key serves it locally (live if it holds the ingest
/// session, history-only otherwise) and never proxies again — one hop
/// maximum, so a stale `log_ingest_sessions` row pointing back at the
/// caller (or two replicas pointing at each other) cannot loop.
/// Store↔store only; nothing outside this module sends or reads it.
const PROXIED_METADATA_KEY: &str = "x-rio-log-proxied";

/// How long the cross-replica `TailLog` proxy waits to establish a TCP
/// connection to the replica that owns an execution's live ingest
/// session before giving up and serving the history-only view. The
/// TCP connect is only half the story — the stream *open* has its own
/// bound, [`PROXY_OPEN_TIMEOUT`] — and only the pair bounds the worst
/// case for a stale session row pointing at a dead pod: every read for
/// that execution pays up to connect+open for at most the lease's
/// staleness window, after which `lookup_live` stops returning the
/// dead owner. Accepted rather than cached — the worst case is bounded
/// and self-healing, and a failure cache is more state to get wrong.
const PROXY_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the cross-replica proxy waits for the `TailLog` *open*
/// (HTTP/2 stream establishment + the owner's first response headers)
/// after the TCP connect succeeded. [`PROXY_CONNECT_TIMEOUT`] alone
/// never bounded this: a half-open owner pod (TCP accepting, process
/// wedged) parked the forwarding reader indefinitely. Together the two
/// bounds cap the worst case for a stale session row at
/// connect+open ≈ 7 s, for at most the lease's staleness window.
const PROXY_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Inbound-silence bound for an AppendLog driver with an EMPTY buffer:
/// the enforcement side of the bilateral liveness contract
/// ([`rio_common::liveness`]). Past this the driver aborts (counted
/// `reason="inbound_idle"`) instead of renewing the ingest lease
/// forever on behalf of a builder that no longer exists. The producer
/// side -- the builder uploader's empty-batch keepalive every
/// [`rio_common::liveness::UPLOADER_KEEPALIVE_PERIOD`] -- guarantees a
/// conformant peer always shows inbound traffic inside this bound
/// (the conformance test in rio-common is the relation's machine
/// witness). Locally this is also pinned to
/// 4 x [`sessions::HEARTBEAT_INTERVAL`] by an asserting test, so
/// lease renewal stays structurally coupled to observed stream
/// liveness.
///
/// SIGNED 2026-06-08 (owner, bughunt-4 fix-wave #5-S Q1): the
/// inbound_idle abort law is bilateral -- the builder uploader gains
/// an empty-batch keepalive producer (period << abort bound); this
/// abort keeps its bound as the contract's enforcement side;
/// conformance is proven by the shared const-pair test (period x
/// margin < abort). The round-3 dashboard test writer's keepalive is
/// RATIFIED as the law's client-side duty (its grpcurl writer is its
/// own producer under the bilateral contract -- never removed; R12
/// re-points its cadence comments and budget math at these consts;
/// the structural ingest-session-lease gates stay).
const INBOUND_IDLE_BOUND: std::time::Duration = rio_common::liveness::INBOUND_IDLE_ABORT;

/// Turns the owning replica's self-identity (the
/// `log_ingest_sessions.replica_pod` value it registered at
/// `sessions::acquire` time) into a URI the cross-replica `TailLog`
/// proxy can dial.
pub enum PeerResolver {
    /// Substitute every `{pod}` in the template with the peer's
    /// identity. Production: the template comes from
    /// `Config::log_peer_url_template`.
    Template(String),
    /// An explicit identity → URI map for tests (the in-process
    /// replicas listen on ephemeral loopback ports that no template can
    /// describe).
    #[cfg(test)]
    Static(std::collections::HashMap<String, String>),
}

impl PeerResolver {
    /// `None` = this resolver cannot name the peer (only possible for
    /// the test map); counted as a proxy failure by the caller. A
    /// disabled proxy is unrepresentable here — `LogServiceImpl`
    /// holds `Option<PeerResolver>` and an empty template constructs
    /// `None` at the boundary, so a constructed resolver always has a
    /// template.
    fn uri_for(&self, pod: &str) -> Option<String> {
        match self {
            PeerResolver::Template(t) => {
                // The address-form rule, in exactly one place: an
                // identity that parses as an IPv6 address is
                // bracketed for URI authority position; hostnames and
                // IPv4 pass through bare.
                let identity = if pod.parse::<std::net::Ipv6Addr>().is_ok() {
                    format!("[{pod}]")
                } else {
                    pod.to_string()
                };
                Some(t.replace("{pod}", &identity))
            }
            #[cfg(test)]
            PeerResolver::Static(m) => m.get(pod).cloned(),
        }
    }

    #[cfg(test)]
    fn static_map(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        PeerResolver::Static(entries.into_iter().collect())
    }
}

/// Capacity of the `AppendLog` ack channel. Acks are produced at most
/// once per chunk cut (≥1/60 s steady-state), so a tiny buffer is
/// enough.
///
/// The contract (every ack flows through exactly two named forms; the
/// in-file self-scan pins it): [`send_ack_bounded`] for in-loop acks —
/// bounded at one cut interval, an undelivered ack ends the stream as
/// a client disconnect — and [`ack_try_send`] for post-exit cleanup
/// and drain acks, which never wait at all. A builder that stops
/// reading acks but keeps the stream open therefore costs the driver
/// AT MOST one cut interval before the stream is cut off and the
/// un-acked tail is left to the reconnect replay (idempotent). The
/// previous contract awaited `send` raw inside the select loop and
/// described the resulting whole-driver stall (ingest, cutting, AND
/// the heartbeat) as "deliberate backpressure" bounded by a lease
/// steal at its "next heartbeat" — false: the next heartbeat never
/// came, because the heartbeat arm was parked behind the same send.
const ACK_QUEUE: usize = 16;

/// Send one in-loop ack, waiting at most `bound` (one cut interval at
/// the call sites). Returns false — undelivered — on a closed receiver
/// OR a queue that stayed full for the whole bound; the caller ends
/// the stream as a client disconnect and the un-acked tail rides the
/// reconnect replay. One of the exactly two callable ack forms (the
/// in-file self-scan pins this).
// r[impl store.log.driver-bounded]
async fn send_ack_bounded(
    tx: &mpsc::Sender<Result<AppendLogAck, Status>>,
    msg: Result<AppendLogAck, Status>,
    bound: std::time::Duration,
) -> bool {
    match tokio::time::timeout(bound, tx.send(msg)).await {
        Ok(Ok(())) => true,
        // Receiver gone or full past the bound: either way the builder
        // is not consuming; never park the driver behind it.
        Ok(Err(_)) | Err(_) => false,
    }
}

/// Fire-and-forget ack form for post-exit cleanup and drain acks: a
/// full queue or a gone receiver drops the message instead of waiting
/// even once. The builder that didn't read its queue gets the verdict
/// from the stream close itself; nothing here is load-bearing for
/// durability. The other of the two callable ack forms.
// r[impl store.log.driver-bounded]
fn ack_try_send(
    tx: &mpsc::Sender<Result<AppendLogAck, Status>>,
    msg: Result<AppendLogAck, Status>,
) {
    let _ = tx.try_send(msg);
}

/// The ownership-witnessed registry insert (bug_010): publish this
/// session's ingest entry, then verify the PG lease row still names
/// this session BEFORE cancelling whatever was displaced — the
/// two-store handoff (PG row → in-memory registry) carries its
/// ownership witness across the await.
///
/// Two concurrent same-pod opens can both pass `sessions::acquire`
/// (the same-pod arm deliberately steals from our own previous
/// session), and the awaited acquire can invert order against this
/// synchronous insert: the LATER PG acquirer owns the row regardless
/// of who registers first. The pre-fix shape cancelled the displaced
/// entry unconditionally — the earlier-acquiring, later-registering
/// open deposed the true owner (whose Displaced exit then stranded
/// the row for the stale window while the survivor died at its first
/// heartbeat with LeaseLost).
///
/// Post-insert re-check, not pre-insert: once our entry is visible we
/// re-read the row; OWNED ⇒ anything we displaced is genuinely
/// deposed (cancel it). NOT OWNED ⇒ we are the stale acquirer — the
/// displaced entry is restored (conditionally: only if our entry
/// still holds the slot; an even-newer registrant governs its own
/// slot via its own witness) and the open yields `ALREADY_EXISTS`,
/// never cancelling the owner. The restore window (our entry visible
/// while un-owned) costs a concurrent `TailLog` attach at most a
/// momentary history-only fallback.
async fn register_ingest_owner(
    registry: &DashMap<Uuid, IngestEntry>,
    pool: &PgPool,
    exec_id: Uuid,
    session_id: Uuid,
    entry: IngestEntry,
) -> Result<(), Status> {
    let ours = Arc::clone(&entry.shared);
    let displaced = registry.insert(exec_id, entry);

    // The ownership witness: "Displaced ⇒ displacer owns the row" is
    // a checked predicate, never an assumed insertion order.
    let owner = match sessions::current_session(pool, exec_id).await {
        Ok(owner) => owner,
        Err(e) => {
            // Fail closed: withdraw our entry (conditionally) and
            // surface the error — admitting a stream whose ownership
            // is unverifiable re-opens the race this witness closes.
            restore_displaced(registry, exec_id, &ours, displaced);
            return Err(e).status_internal("AppendLog: ingest ownership witness");
        }
    };
    if owner == Some(session_id) {
        if let Some(displaced) = displaced {
            displaced.cancel.cancel();
        }
        return Ok(());
    }

    // A concurrent open acquired the row after us: IT owns the lease.
    // Yield without cancelling anyone; the builder's retry lands on
    // the live owner (usually its own newer reconnect).
    restore_displaced(registry, exec_id, &ours, displaced);
    Err(Status::already_exists(
        "AppendLog: a newer ingest session acquired this execution's lease \
         during open; retry against it",
    ))
}

/// Conditionally undo [`register_ingest_owner`]'s insert: restore the
/// displaced entry (or clear the slot) only while OUR entry still
/// holds it — a newer registrant's entry is governed by its own
/// witness and must not be clobbered.
fn restore_displaced(
    registry: &DashMap<Uuid, IngestEntry>,
    exec_id: Uuid,
    ours: &Arc<Mutex<IngestShared>>,
    displaced: Option<IngestEntry>,
) {
    if let dashmap::Entry::Occupied(mut slot) = registry.entry(exec_id)
        && Arc::ptr_eq(&slot.get().shared, ours)
    {
        match displaced {
            Some(previous) => {
                slot.insert(previous);
            }
            None => {
                slot.remove();
            }
        }
    }
}

/// The `LogService` implementation. One per replica; cheap to clone
/// into the tonic server (everything is `Arc`/`PgPool`).
pub struct LogServiceImpl {
    pool: PgPool,
    /// Where chunk objects go. S3 in production; in-memory for tests
    /// and for stores with no S3 chunk backend configured (logs are
    /// then non-durable across restarts — `main.rs` warns).
    chunk_store: Arc<dyn LogChunkStore>,
    /// HMAC verifier for `x-rio-assignment-token` on AppendLog. `None`
    /// = dev mode: the token and the assignment-binding checks are
    /// skipped (mirrors `PutPath`'s dev mode). The completeness seal
    /// and every input bound still apply.
    hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    /// Live ingest sessions on THIS replica, keyed by `exec_id`. The
    /// value is the shared buffer + subscriber-list handle a `TailLog`
    /// reader subscribes to. Entries are inserted by `AppendLog` after
    /// the session lease is acquired and removed (identity-checked) on
    /// every stream exit path.
    active_ingests: Arc<DashMap<Uuid, IngestEntry>>,
    /// This replica's pod name — the `log_ingest_sessions.replica_pod`
    /// value that routes cross-replica `TailLog` readers here.
    replica_pod: String,
    /// How a `TailLog` reader landing on this replica dials the replica
    /// that owns an execution's live ingest session.
    peer_resolver: Option<PeerResolver>,
    /// Per-stream ingest tuning (cut threshold, cut interval, the
    /// per-execution byte cap), built from the store config.
    ingest_config: IngestConfig,
    /// Per-execution cap on chunk-cut attempts. See
    /// `Config::log_max_chunks_per_exec`.
    max_chunks_per_exec: u32,
    /// Per-replica concurrent-AppendLog-stream cap.
    stream_permits: Arc<tokio::sync::Semaphore>,
    /// Per-replica resident-ingest-buffer byte budget. Each stream
    /// reserves `2 × cut_threshold_bytes` for its lifetime.
    byte_budget: Arc<tokio::sync::Semaphore>,
    /// The per-stream byte reservation taken from `byte_budget` at
    /// open: `2 × cut_threshold_bytes` (one chunk mid-cut + one
    /// refilling), clamped to u32 (tokio's `acquire_many` takes u32).
    per_stream_byte_reservation: u32,
    /// Capacity of one TailLog subscriber's fan-out queue, in batches.
    /// Injectable so the in-stream recovery tests can force drops with
    /// a handful of batches.
    tail_subscriber_queue: usize,
}

impl LogServiceImpl {
    /// Construct with production defaults. Builder-style `with_*`
    /// methods override individual knobs (mirroring `StoreServiceImpl`).
    pub fn new(pool: PgPool, chunk_store: Arc<dyn LogChunkStore>, replica_pod: String) -> Self {
        let ingest_config = IngestConfig::default();
        let mut s = Self {
            pool,
            chunk_store,
            hmac_verifier: None,
            active_ingests: Arc::new(DashMap::new()),
            replica_pod,
            // Empty = proxy disabled until with_peer_url_template
            // supplies the deployment's template (fail-closed; matches
            // Config::default).
            peer_resolver: None,
            ingest_config,
            max_chunks_per_exec: 100_000,
            stream_permits: Arc::new(tokio::sync::Semaphore::new(256)),
            byte_budget: Arc::new(tokio::sync::Semaphore::new(1024 * 1024 * 1024)),
            per_stream_byte_reservation: 0,
            tail_subscriber_queue: TAIL_SUBSCRIBER_QUEUE,
        };
        s.per_stream_byte_reservation = per_stream_reservation(&s.ingest_config);
        s
    }

    pub fn with_hmac_verifier(mut self, v: Arc<rio_auth::hmac::HmacVerifier>) -> Self {
        self.hmac_verifier = Some(v);
        self
    }

    pub fn with_ingest_config(mut self, c: IngestConfig) -> Self {
        self.ingest_config = c;
        self.per_stream_byte_reservation = per_stream_reservation(&self.ingest_config);
        self
    }

    pub fn with_max_streams(mut self, n: usize) -> Self {
        self.stream_permits = Arc::new(tokio::sync::Semaphore::new(n));
        self
    }

    pub fn with_byte_budget(mut self, bytes: u64) -> Self {
        // Saturating conversion (validate() bounds the config from
        // below AND above; this is the last-line guard for direct
        // builder-style callers). The old `& (usize::MAX >> 3)` mask
        // was a bitmask, not a clamp — bug_131.
        self.byte_budget = Arc::new(tokio::sync::Semaphore::new(rio_common::semaphore_permits(
            bytes,
        )));
        self
    }

    pub fn with_max_chunks_per_exec(mut self, n: u32) -> Self {
        self.max_chunks_per_exec = n;
        self
    }

    /// Override the per-subscriber fan-out queue capacity (test knob;
    /// production uses `TAIL_SUBSCRIBER_QUEUE`, a private const).
    pub fn with_tail_subscriber_queue(mut self, n: usize) -> Self {
        self.tail_subscriber_queue = n.max(1);
        self
    }

    /// Set the `{pod}` → URI template the cross-replica `TailLog` proxy
    /// dials peers with. See `Config::log_peer_url_template`.
    ///
    /// An EMPTY template means the proxy is disabled — decided HERE,
    /// at construction, not deep in `uri_for`: a disabled deployment
    /// never queries `lookup_live`, never dials, never increments
    /// rio_store_log_tail_proxy_failures_total, and never warns per
    /// read. Disabled is a configuration, not a failure.
    pub fn with_peer_url_template(mut self, template: String) -> Self {
        self.peer_resolver = if template.is_empty() {
            None
        } else {
            Some(PeerResolver::Template(template))
        };
        self
    }

    /// Tests: an explicit pod-name → URI map (the in-process replicas
    /// listen on ephemeral ports).
    #[cfg(test)]
    fn with_peer_resolver(mut self, resolver: PeerResolver) -> Self {
        self.peer_resolver = Some(resolver);
        self
    }

    /// The live-ingest registry handle, for tests that assert an entry
    /// was cleaned up.
    #[cfg(test)]
    fn active_ingests(&self) -> Arc<DashMap<Uuid, IngestEntry>> {
        Arc::clone(&self.active_ingests)
    }

    /// The open-time caps the gate enforces (check 5) — derived from
    /// the same config the session enforces mid-stream.
    fn open_caps(&self) -> OpenCaps {
        OpenCaps {
            per_exec_byte_cap: self.ingest_config.per_exec_byte_cap,
            max_chunks_per_exec: self.max_chunks_per_exec,
        }
    }

    /// `AppendLog` token gate. Mirrors
    /// `StoreServiceImpl::verify_assignment_token` minus the
    /// service-token bypass: a log stream is always attributable to one
    /// build attempt, so only a per-dispatch assignment token is
    /// acceptable — the gateway/scheduler service identity has no
    /// business opening one (same posture as `AppendHwPerfSample`).
    ///
    /// `Ok(None)` = dev mode (no verifier configured): the caller skips
    /// the assignment-binding gate and trusts the header.
    fn verify_append_token<T>(
        &self,
        request: &Request<T>,
    ) -> Result<Option<rio_auth::hmac::AssignmentClaims>, Status> {
        let Some(verifier) = &self.hmac_verifier else {
            return Ok(None);
        };
        let token = request
            .metadata()
            .get(rio_proto::ASSIGNMENT_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok());
        match token {
            Some(t) => match verifier.verify::<rio_auth::hmac::AssignmentClaims>(t) {
                Ok(claims) => Ok(Some(claims)),
                Err(e) => {
                    metrics::counter!("rio_store_hmac_rejected_total", "reason" => "invalid_token")
                        .increment(1);
                    warn!(error = %e, "AppendLog: assignment token verification failed");
                    Err(Status::unauthenticated(format!("assignment token: {e}")))
                }
            },
            None => {
                metrics::counter!("rio_store_hmac_rejected_total", "reason" => "missing_token")
                    .increment(1);
                Err(Status::unauthenticated(format!(
                    "AppendLog: assignment token required ({} header)",
                    rio_proto::ASSIGNMENT_TOKEN_HEADER
                )))
            }
        }
    }
}

/// The per-stream reservation against `log_bytes_budget`: the worst-case
/// resident buffer for one stream (one cut threshold's worth staged in
/// flight + one refilling behind it), clamped to the u32 that
/// `Semaphore::try_acquire_many` takes.
fn per_stream_reservation(cfg: &IngestConfig) -> u32 {
    u32::try_from(cfg.cut_threshold_bytes.saturating_mul(2)).unwrap_or(u32::MAX)
}

/// Wrap a `Status` in a stream that yields a single `Err` then ends.
///
/// For server-streaming RPCs consumed via grpc-web (the dashboard),
/// returning `Err(Status)` directly from the handler makes tonic emit a
/// Trailers-Only response — `grpc-status` lives in the HTTP headers
/// with zero body, and the browser fetch API cannot read trailers, so
/// the dashboard sees a silent 200. Yielding the `Err` from the stream
/// body instead produces a normal HEADERS + body + TRAILERS sequence
/// that tonic-web encodes in-body. Same pattern (and same rationale
/// comment) as the scheduler's `admin::logs::err_stream`.
fn err_stream<T: Send + 'static>(status: Status) -> ReceiverStream<Result<T, Status>> {
    let (tx, rx) = mpsc::channel(1);
    let _ = tx.try_send(Err(status));
    ReceiverStream::new(rx)
}

#[tonic::async_trait]
impl LogService for LogServiceImpl {
    type AppendLogStream = ReceiverStream<Result<AppendLogAck, Status>>;
    type TailLogStream = ReceiverStream<Result<TailLogChunk, Status>>;

    /// Builder log ingest. See the module doc for the full flow; the
    /// ordering here is load-bearing:
    ///
    /// 1. token → 2. header → 3. gate → 4. admission → 5. lease →
    /// 6. register + spawn the driver.
    ///
    /// The gate runs before admission so an unauthorized caller cannot
    /// observe (or exhaust) the admission budgets; admission runs
    /// before the lease so a stream that will be rejected for capacity
    /// never takes (and then has to release) the PG lease row.
    ///
    /// **Client open contract:** because step 2 awaits the first
    /// request message, this handler does not return — and tonic does
    /// not send response headers — until the [`AppendLogHeader`] has
    /// been read and validated. A client that awaits the RPC call
    /// before writing the header into its request stream deadlocks:
    /// it is waiting for response headers the server will not send
    /// until it receives the header the client has not sent. Buffer
    /// the header into the request stream *first*, then await the
    /// call. (Every open-time rejection — bad token, gate, admission,
    /// lease — therefore surfaces as an error from the call itself,
    /// before the ack stream exists.) The same contract is stated on
    /// the `AppendLog` RPC in `store.proto`; the test helper
    /// `open_append` below is the reference client shape.
    #[instrument(skip_all, fields(rpc = "AppendLog"))]
    async fn append_log(
        &self,
        request: Request<Streaming<AppendLogRequest>>,
    ) -> Result<Response<Self::AppendLogStream>, Status> {
        // -- 1. The token, before reading anything off the stream.
        let claims = self.verify_append_token(&request)?;
        let mut inbound = request.into_inner();

        // -- 2. The first message must be the header.
        let header = read_header(&mut inbound).await?;

        // -- 3. The binding + completeness gate.
        let gate_ok = match &claims {
            Some(claims) => {
                gate::check_append_open(&self.pool, claims, &header, self.open_caps()).await?
            }
            // Dev mode (no HMAC verifier): no claims to bind against.
            // Trust the header's identity but run the SAME shared
            // finish (`gate::finish_open`) the token path runs — the
            // completeness seal, the durable caps, and the cap seed
            // must hold for dev-mode callers too, and a separate
            // hand-rolled arm is exactly how they would drift.
            None => {
                let exec_id: Uuid = header.exec_id.parse().map_err(|_| {
                    Status::invalid_argument("AppendLog: header.exec_id is not a valid UUID")
                })?;
                gate::finish_open(
                    &self.pool,
                    rio_nix::store_path::drv_log_hash(&header.derivation_path),
                    exec_id,
                    self.open_caps(),
                )
                .await?
            }
        };
        let exec_id = gate_ok.exec_id;

        // -- 4. Admission: the stream-count cap and the buffer-byte
        // budget. Non-blocking — a builder rejected here fails over to
        // another replica immediately rather than queueing.
        let stream_permit = Arc::clone(&self.stream_permits)
            .try_acquire_owned()
            .map_err(|_| {
                metrics::counter!("rio_store_log_ingest_rejected_total", "reason" => "max_streams")
                    .increment(1);
                gate::replica_capacity_status(
                    "AppendLog: this replica is at its concurrent log-stream cap; \
                     reconnect to another replica",
                )
            })?;
        let byte_permit = Arc::clone(&self.byte_budget)
            .try_acquire_many_owned(self.per_stream_byte_reservation)
            .map_err(|_| {
                metrics::counter!("rio_store_log_ingest_rejected_total", "reason" => "byte_budget")
                    .increment(1);
                gate::replica_capacity_status(
                    "AppendLog: this replica's log ingest buffer budget is exhausted; \
                     reconnect to another replica",
                )
            })?;

        // -- 5. The single-live-session lease.
        let session_id = Uuid::now_v7();
        match sessions::acquire(&self.pool, exec_id, session_id, &self.replica_pod)
            .await
            .status_internal("AppendLog: ingest session acquire")?
        {
            Acquire::Acquired => {}
            Acquire::Busy { current_owner } => {
                return Err(Status::already_exists(format!(
                    "AppendLog: another live ingest session holds this execution \
                     (on {current_owner:?}); retry after it closes or goes stale",
                )));
            }
        }

        // -- 6. The session, the registry entry, and the driver task.
        let session = IngestSession::new(&gate_ok, session_id, self.ingest_config.clone());
        let shared = Arc::clone(session.shared());
        let cancel = rio_common::signal::Token::new();
        // A previous session for the same execution on this replica
        // (its lease was stolen by `acquire`'s same-pod arm, or it is
        // mid-teardown) may still hold the registry slot; the
        // ownership-witnessed insert replaces it and cancels its
        // driver only when the row says we own the lease (bug_010 —
        // the awaited acquire above and this insert can invert order
        // across two concurrent same-pod opens; the later PG acquirer
        // owns the row regardless of who registers first).
        register_ingest_owner(
            &self.active_ingests,
            &self.pool,
            exec_id,
            session_id,
            IngestEntry {
                shared: Arc::clone(&shared),
                cancel: cancel.clone(),
            },
        )
        .await?;
        metrics::gauge!("rio_store_log_active_ingest_sessions").increment(1.0);
        info!(
            %exec_id, %session_id,
            executor = claims.as_ref().map(|c| c.executor_id.as_str()).unwrap_or("<dev>"),
            "AppendLog: ingest session opened"
        );

        let (ack_tx, ack_rx) = mpsc::channel(ACK_QUEUE);
        let driver = AppendDriver {
            pool: self.pool.clone(),
            chunk_store: Arc::clone(&self.chunk_store),
            active_ingests: Arc::clone(&self.active_ingests),
            shared,
            cancel,
            session,
            max_chunks_per_exec: self.max_chunks_per_exec,
        };
        // Detached task: it exits when the inbound stream ends (client
        // half-close, disconnect, or transport error) or when the
        // handler aborts the stream. Not cancelled on client
        // disconnect — that is what lets the cleanup (deregister,
        // final drain, lease release) run on every exit path.
        tokio::spawn(async move {
            // The permits ride into the task so they are held for the
            // stream's lifetime and released on any exit path.
            let _stream_permit = stream_permit;
            let _byte_permit = byte_permit;
            driver.run(inbound, ack_tx).await;
        });

        Ok(Response::new(ReceiverStream::new(ack_rx)))
    }

    /// Log read/follow. Errors are yielded **in-stream** (see
    /// `err_stream`) so grpc-web clients can read them.
    #[instrument(skip_all, fields(rpc = "TailLog"))]
    async fn tail_log(
        &self,
        request: Request<TailLogRequest>,
    ) -> Result<Response<Self::TailLogStream>, Status> {
        // One hop maximum: a request another replica already forwarded
        // is served from local state no matter what the session table
        // says (see PROXIED_METADATA_KEY).
        let proxied = request.metadata().contains_key(PROXIED_METADATA_KEY);
        // Verified by the JWT interceptor (the authz layer requires
        // presence when a pubkey is configured); ownership is checked
        // below against the resolved derivation. The raw token is kept
        // for the cross-replica relay so the peer can re-verify.
        let tenant = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub);
        let tenant_token = request
            .metadata()
            .get(rio_common::grpc::TENANT_TOKEN_HEADER)
            .cloned();
        let req = request.into_inner();
        Ok(Response::new(
            self.tail_log_stream(req, proxied, tenant, tenant_token)
                .await,
        ))
    }
}

impl LogServiceImpl {
    /// The fallible body of `tail_log`, with every error routed through
    /// [`err_stream`]. `proxied` = the request already crossed one
    /// replica hop and must be served from local state only.
    async fn tail_log_stream(
        &self,
        req: TailLogRequest,
        proxied: bool,
        tenant: Option<Uuid>,
        tenant_token: Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
    ) -> ReceiverStream<Result<TailLogChunk, Status>> {
        // Resolve AND authorize in one gate (the sole OwnedExec
        // producer): a verified tenant may only read logs of builds it
        // is a member-owner of (bug_290 / §5-S Q2 build-membership; no
        // service-token bypass). Enforce-when-presented mirrors the
        // authz layer: when no pubkey is configured no claims exist and
        // the deployment is in the dev/VM posture. Deny-with-claims is
        // absence-shaped NotFound (merged_bug_064-c) and must reach
        // grpc-web clients in-body, same as the plain missing-row case.
        let owned =
            match tail::authorize_tail(&self.pool, &req.derivation, &req.exec_id, tenant).await {
                Ok(owned) => owned,
                Err(status) => return err_stream(status),
            };
        let exec_id = owned.id();

        // Find the live ingest buffer, if any. Only a LOCAL session can
        // be subscribed to; a session on another replica is relayed by
        // the cross-replica proxy below. On proxy failure (or a request
        // that already crossed one hop) those readers get the
        // history-only view (every committed chunk, no live tail),
        // which is correct just laggier.
        let local = self
            .active_ingests
            .get(&exec_id)
            .map(|e| Arc::clone(&e.shared));
        let subscription = match &local {
            Some(shared) => {
                if req.follow {
                    // Register + snapshot atomically w.r.t. the cutter
                    // (one lock over the buffer, the in-flight staging
                    // area, and the subscriber list — the seam).
                    let (tx, rx) = mpsc::channel(self.tail_subscriber_queue);
                    let (snapshot, wake) = {
                        let mut s = lock_shared(shared);
                        (s.subscribe(tx, req.since_line), s.drop_watch())
                    };
                    metrics::gauge!("rio_store_log_tail_subscribers").increment(1.0);
                    Some((snapshot, rx, Arc::downgrade(shared), wake))
                } else {
                    // A non-follow read still wants the not-yet-durable
                    // lines (the dashboard's one-shot view should show
                    // the latest output), but no live subscription.
                    // subscribe() with an immediately-dropped receiver
                    // would register a permanently-full sender;
                    // snapshot without registering instead.
                    let snapshot = lock_shared(shared).snapshot_since(req.since_line);
                    Some((
                        snapshot,
                        {
                            // An already-closed channel: the live phase
                            // ends immediately.
                            let (_tx, rx) = mpsc::channel(1);
                            rx
                        },
                        Arc::downgrade(shared),
                        // Never fired (sender dropped immediately):
                        // the dummy epoch for a subscription-less
                        // read. The loop disarms it on first Err.
                        watch::channel(0u64).1,
                    ))
                }
            }
            None => {
                // Disabled proxy (peer_resolver None): skip the
                // lookup_live query entirely — a disabled deployment
                // pays no per-read PG roundtrip, counts no proxy
                // failures, and emits no warns. The boot log is the
                // single statement of the disabled posture.
                // r[impl store.log.proxy-disabled-not-failure]
                if !proxied && let Some(resolver) = self.peer_resolver.as_ref() {
                    // Every history-only downgrade of a FOLLOWABLE live
                    // view is DISCLOSED (bug_148: the pre-fix let-chain
                    // silently folded "stale row", "lookup failed", and
                    // "row names this pod but the buffer is gone" into
                    // the no-session quiet path — a fleet whose rows
                    // went stale behind parked owners downgraded every
                    // follower with zero operator signal).
                    match sessions::lookup_live(&self.pool, exec_id).await {
                        Ok(sessions::LiveLookup::Live(live))
                            if live.replica_pod != self.replica_pod =>
                        {
                            // Owned by another replica: relay its
                            // TailLog stream so the reader gets the
                            // live tail no matter which replica it hit.
                            // On any failure fall through to the
                            // history-only view — laggy but correct
                            // beats an error, and the reader's
                            // reconnect loop will find the new owner
                            // once the builder re-establishes its
                            // ingest stream.
                            match self
                                .proxy_tail(
                                    resolver,
                                    &live.replica_pod,
                                    req.clone(),
                                    tenant_token.clone(),
                                )
                                .await
                            {
                                Ok(upstream) => {
                                    metrics::counter!("rio_store_log_tail_proxied_total")
                                        .increment(1);
                                    debug!(
                                        %exec_id,
                                        owner = %live.replica_pod,
                                        "TailLog: relaying to the replica holding the live ingest session"
                                    );
                                    return relay_stream(upstream);
                                }
                                Err(error) => {
                                    metrics::counter!("rio_store_log_tail_proxy_failures_total")
                                        .increment(1);
                                    warn!(
                                        %exec_id,
                                        owner = %live.replica_pod,
                                        %error,
                                        "TailLog: cross-replica proxy failed; serving history only"
                                    );
                                }
                            }
                        }
                        Ok(sessions::LiveLookup::Live(live)) => {
                            // The row names THIS pod but the local
                            // registry has no entry: our own session is
                            // mid-teardown (release pending) or a
                            // restarted pod re-using the identity. The
                            // buffer is gone either way.
                            metrics::counter!(
                                "rio_store_log_tail_live_downgrades_total",
                                "reason" => "owner_local_gone"
                            )
                            .increment(1);
                            debug!(
                                %exec_id,
                                owner = %live.replica_pod,
                                "TailLog: live session row names this replica but no \
                                 local buffer exists; serving history only"
                            );
                        }
                        Ok(sessions::LiveLookup::Stale { replica_pod }) => {
                            // bug_148's reader-visible face: a session
                            // row exists but is not renewing. The owner
                            // died uncleanly — or is alive and parked.
                            metrics::counter!(
                                "rio_store_log_tail_live_downgrades_total",
                                "reason" => "stale_session"
                            )
                            .increment(1);
                            warn!(
                                %exec_id,
                                owner = %replica_pod,
                                "TailLog: ingest session row is stale; serving history \
                                 only (owner dead or not renewing)"
                            );
                        }
                        Ok(sessions::LiveLookup::Absent) => {
                            // No session at all — the normal case for a
                            // finished build. History-only is the
                            // complete view; nothing to disclose.
                        }
                        Err(error) => {
                            metrics::counter!(
                                "rio_store_log_tail_live_downgrades_total",
                                "reason" => "lookup_failed"
                            )
                            .increment(1);
                            warn!(
                                %exec_id,
                                %error,
                                "TailLog: live-session lookup failed; serving history only"
                            );
                        }
                    }
                }
                None
            }
        };

        let (tx, rx) = mpsc::channel::<Result<TailLogChunk, Status>>(4);
        let pool = self.pool.clone();
        let store = Arc::clone(&self.chunk_store);
        let follow = req.follow;
        let since_line = req.since_line;
        tokio::spawn(async move {
            let had_subscription = subscription.is_some();
            let r = serve_tail(pool, store, owned, since_line, follow, subscription, &tx).await;
            if had_subscription && follow {
                metrics::gauge!("rio_store_log_tail_subscribers").decrement(1.0);
            }
            if let Err(status) = r {
                // The client may already be gone; ignore the send error.
                let _ = tx.send(Err(status)).await;
            }
        });
        ReceiverStream::new(rx)
    }

    /// Open a `TailLog` against the replica that owns `owner_pod`'s live
    /// ingest session and return its response stream for relaying.
    ///
    /// The forwarded request is the caller's request verbatim plus the
    /// [`PROXIED_METADATA_KEY`] marker, so the receiving replica serves
    /// it from local state and the forward chain is one hop long no
    /// matter what the session table says.
    ///
    /// Failure is returned (not swallowed) so the caller can count it
    /// and fall back to the history-only view. The connect is bounded
    /// by [`PROXY_CONNECT_TIMEOUT`] AND the open by
    /// [`PROXY_OPEN_TIMEOUT`] — the connect bound alone never covered a
    /// half-open owner — so a stale session row pointing at a dead pod
    /// costs each reader at most connect+open, for at most the lease's
    /// staleness window.
    async fn proxy_tail(
        &self,
        resolver: &PeerResolver,
        owner_pod: &str,
        req: TailLogRequest,
        tenant_token: Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
    ) -> Result<Streaming<TailLogChunk>, anyhow::Error> {
        let uri = resolver
            .uri_for(owner_pod)
            .ok_or_else(|| anyhow::anyhow!("no peer URI mapping for replica {owner_pod:?}"))?;
        let endpoint = tonic::transport::Endpoint::from_shared(uri.clone())
            .map_err(|e| anyhow::anyhow!("invalid peer URI {uri:?}: {e}"))?
            .connect_timeout(PROXY_CONNECT_TIMEOUT);
        let channel = endpoint
            .connect()
            .await
            .map_err(|e| anyhow::anyhow!("connect to peer {uri}: {e}"))?;
        let mut client = LogServiceClient::new(channel);
        let mut request = Request::new(req);
        request.metadata_mut().insert(
            PROXIED_METADATA_KEY,
            tonic::metadata::MetadataValue::from_static("1"),
        );
        // Forward the caller's tenant token: the peer's interceptor
        // re-verifies and its handler re-checks ownership — the relay
        // never widens access.
        if let Some(token) = tenant_token {
            request
                .metadata_mut()
                .insert(rio_common::grpc::TENANT_TOKEN_HEADER, token);
        }
        // The connect above is bounded, but the *open* (HTTP/2 +
        // server processing) used to be a naked await: a half-open
        // owner (TCP up, HTTP/2 dead) parked every reader of this
        // execution forever. Bounded per streaming-open-ban; a timeout
        // surfaces as a Status and flows into the same proxy-failure
        // counter + history-only fallback as any other open error.
        Ok(rio_common::grpc::with_timeout_status(
            "proxy TailLog",
            PROXY_OPEN_TIMEOUT,
            client.tail_log(request),
        )
        .await
        .map_err(|e| anyhow::anyhow!("forwarded TailLog to {uri}: {e}"))?
        .into_inner())
    }
}

/// Relay a peer replica's `TailLog` response stream item-for-item.
/// Both `Ok(chunk)` items and a terminal `Err(status)` pass through;
/// nothing is buffered beyond the channel's 4-message slack (the same
/// bound as the locally-served path).
///
/// The `tx.closed()` arm is load-bearing: a `follow=true` relay of a
/// quiet build can sit in `upstream.message()` for minutes between
/// items, and a reader that disconnects in that window must promptly
/// drop `upstream` (tonic then cancels the peer call, releasing the
/// peer-side subscriber) rather than holding the cross-replica HTTP/2
/// stream until the build next emits a line.
fn relay_stream(
    mut upstream: Streaming<TailLogChunk>,
) -> ReceiverStream<Result<TailLogChunk, Status>> {
    let (tx, rx) = mpsc::channel::<Result<TailLogChunk, Status>>(4);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = upstream.message() => match msg {
                    Ok(Some(chunk)) => {
                        if tx.send(Ok(chunk)).await.is_err() {
                            // Our reader went away mid-send; tonic
                            // cancels the upstream call when `upstream`
                            // is dropped.
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(status) => {
                        // The peer yielded an in-stream error (or the
                        // transport died mid-relay). Forward it; the
                        // reader treats it exactly like a
                        // locally-produced error.
                        let _ = tx.send(Err(status)).await;
                        break;
                    }
                },
                // All downstream receivers are gone: stop relaying
                // without waiting for the peer's next item.
                _ = tx.closed() => break,
            }
        }
    });
    ReceiverStream::new(rx)
}

/// Lock the shared ingest state, recovering from a poisoned lock the
/// same way `IngestSession` itself does (the data has no invariant a
/// panic mid-update could break that costs more than one miscounted
/// batch).
fn lock_shared(shared: &Arc<Mutex<IngestShared>>) -> std::sync::MutexGuard<'_, IngestShared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Read and validate the mandatory first message of an `AppendLog`
/// stream.
async fn read_header(inbound: &mut Streaming<AppendLogRequest>) -> Result<AppendLogHeader, Status> {
    match inbound.message().await? {
        Some(AppendLogRequest {
            msg: Some(append_log_request::Msg::Header(h)),
        }) => {
            // The derivation path is untrusted input that ends up in
            // log lines and error messages; bound it the same way the
            // scheduler's recv task did.
            if h.derivation_path.len() > MAX_DERIVATION_PATH_LEN {
                return Err(Status::invalid_argument(
                    "AppendLog: header.derivation_path exceeds the path length bound",
                ));
            }
            Ok(h)
        }
        Some(_) => Err(Status::invalid_argument(
            "AppendLog: the first message must be the header",
        )),
        None => Err(Status::invalid_argument(
            "AppendLog: the stream closed before sending the header",
        )),
    }
}

/// Why the `AppendLog` driver loop stopped. The abort half is typed by
/// COMMIT-CAPABILITY (merged_bug_144): whether the replica can still
/// durably commit decides whether the accepted-but-undurable tail
/// drains before the trailer — pre-split, one `Abort` conflated the
/// two and the cap/idle refusals skipped the ClientFinished-only
/// drain, silently losing the tail of every cap-crossing log (the
/// builder maps the cap trailer to permanent CapExhausted, so
/// "retransmit rescues them" never applied).
enum LoopExit {
    /// The builder half-closed the stream: the normal end of a build's
    /// log. Drain, release, succeed.
    ClientFinished,
    /// Stream-fatal refusal minted while the replica CAN still commit
    /// (the per-execution caps, the inbound-idle law, a protocol
    /// violation): the accepted under-cap tail DRAINS FIRST, then the
    /// carried status is the trailer. The lease is still ours and is
    /// released.
    AbortRefusesInput(Status),
    /// The replica CANNOT durably commit (consecutive cut failures,
    /// stale buffer): draining would burn more attempts against a
    /// failing backend — the builder's retransmit buffer owns the
    /// un-acked tail and replays it elsewhere. The lease is still
    /// ours and is released.
    AbortCannotCommit(Status),
    /// The lease was stolen by another replica. Same teardown as the
    /// aborts EXCEPT the lease row is NOT deleted — it belongs to the
    /// new owner now, and `sessions::release` is session-id-predicated
    /// anyway, but skipping the call entirely makes the intent
    /// auditable.
    LeaseLost,
    /// A newer session for the same execution displaced this one from
    /// the registry (the same-pod reconnect path — the displacer
    /// verified row ownership before cancelling, bug_010). The lease
    /// posture differs from `LeaseLost`: the session-id-predicated
    /// release RUNS (a no-op when the displacer owns the row, an
    /// immediate free if this session somehow still does — never a
    /// strand for the staleness window).
    Displaced,
}

/// Everything the spawned `AppendLog` driver task owns.
struct AppendDriver {
    pool: PgPool,
    chunk_store: Arc<dyn LogChunkStore>,
    active_ingests: Arc<DashMap<Uuid, IngestEntry>>,
    /// The same handle `active_ingests[exec_id].shared` holds — kept
    /// here so the deregistration can be identity-checked (a newer
    /// session for the same execution must not be evicted by this
    /// one's teardown).
    shared: Arc<Mutex<IngestShared>>,
    /// Fired by `append_log` when a newer session displaces this one
    /// from the registry; the drive loop exits immediately so the
    /// stream permits return in O(task wakeup) instead of at the next
    /// lease-heartbeat observation.
    cancel: rio_common::signal::Token,
    session: IngestSession,
    max_chunks_per_exec: u32,
}

impl AppendDriver {
    /// The stream's whole lifetime: the select loop, then the cleanup
    /// that must run on every exit path (deregister, final drain,
    /// lease release).
    async fn run(
        mut self,
        mut inbound: Streaming<AppendLogRequest>,
        ack_tx: mpsc::Sender<Result<AppendLogAck, Status>>,
    ) {
        let exec_id = self.session.exec_id;
        let session_id = self.session.session_id;

        // 1. The deregistration + gauge decrement are a panic-safe
        // guard, not a post-`drive` statement: a panic anywhere in the
        // driver is caught by tokio (the task just ends), but a leaked
        // registry entry would hand every future TailLog reader for
        // this execution a buffer that never receives another line and
        // is never dropped — their follow streams would hang until the
        // client gives up. The lease row is NOT in the guard (releasing
        // it is async and it self-heals via the staleness window).
        //
        // The `remove_if` identity check: a reconnecting builder can
        // open a new session for the same execution on this replica
        // (the lease's same-pod steal arm) before this teardown runs;
        // its insert replaced our entry and we must not remove theirs.
        let deregister = scopeguard::guard(
            (Arc::clone(&self.active_ingests), Arc::clone(&self.shared)),
            move |(registry, shared)| {
                registry.remove_if(&exec_id, |_, v| Arc::ptr_eq(&v.shared, &shared));
                metrics::gauge!("rio_store_log_active_ingest_sessions").decrement(1.0);
            },
        );

        // The open-time coverage ack (bug_032): the FIRST message on
        // the ack stream, sent only when the execution already has
        // durable contiguous coverage. It tells the builder where the
        // committed prefix ends so its retransmit buffer trims BEFORE
        // replaying — without the letter, the only trim signal is
        // per-chunk acks, and a reconnect replays committed-but-
        // unacked content the session then has to drop server-side.
        // `durable_through_line` is the contiguous durable frontier —
        // consulted through the ONE producer (merged_bug_005), which
        // at this point (before any accept or cut) equals
        // `open_coverage_next_line() - 1` — keeping the message a
        // true durability statement even for clients that predate the
        // watermark field (they consume it as a plain chunk ack and
        // trim to the same point). try_send into the just-created
        // queue cannot fail on capacity; a torn-down receiver makes
        // the drop harmless (the stream is dying anyway).
        if let Some(frontier) = self.session.durable_frontier() {
            ack_try_send(
                &ack_tx,
                Ok(AppendLogAck {
                    durable_through_line: frontier,
                    open_coverage_next_line: Some(self.session.open_coverage_next_line()),
                }),
            );
        }

        // The session-lease heartbeat lives on its OWN task (bug_148,
        // R27): the drive loop's sibling arms await chunk cuts inline
        // for up to one cut interval plus an ack send — keeping the
        // heartbeat as a select arm made a healthy 31-60 s S3 PUT
        // stale the session row mid-stream (steal admitted, healthy
        // stream aborted). The dedicated task's cadence is bounded by
        // construction (sessions::HEARTBEAT_RPC_BOUND, compile-
        // asserted against the staleness margin) and it keeps beating
        // through the FINAL DRAIN below — the drain can span several
        // cut intervals and the pre-fix shape had no heartbeat at all
        // there. The handle is stopped (joined bounded) on every exit
        // path before the lease release.
        let lease = sessions::spawn_heartbeat(self.pool.clone(), exec_id, session_id);

        let exit = self.drive(&mut inbound, &ack_tx, lease.lost_watch()).await;

        // ---- Cleanup. Runs on every exit path of `drive` (which has
        // no early returns that bypass it — it returns `LoopExit`).
        // The task itself is detached and never cancelled short of
        // runtime shutdown, so this is as close to a `finally` as
        // tokio offers without a guard object holding an owned pool.
        drop(deregister);

        // 2. The final drain: one chunk per remaining contiguous run.
        // Runs for the clean close AND for every REFUSES-INPUT abort
        // (merged_bug_144: the replica can still commit, so the
        // accepted under-cap tail goes durable before the trailer —
        // pre-split, the cap/idle aborts skipped this and the tail of
        // every cap-crossing log was silently lost). Skipped when the
        // replica CANNOT commit (the drain would just burn three more
        // failed attempts — the builder's retransmit buffer still
        // holds every un-acked line) and when the lease is no longer
        // ours (the new owner's session is the committing one now).
        let drain_stop = match &exit {
            LoopExit::ClientFinished | LoopExit::AbortRefusesInput(_) => {
                self.drain(&ack_tx).await.err()
            }
            LoopExit::AbortCannotCommit(_) | LoopExit::LeaseLost | LoopExit::Displaced => None,
        };

        // The heartbeat covered the drain; stop it (bounded join)
        // before the lease is released — the JOIN OBLIGATION on the
        // spawned liveness task (a discarded handle would renew a
        // released lease until its next tick observed Lost).
        lease.stop().await;

        // 3. The lease. Released on every path except a stolen lease
        // (the row belongs to the new owner; the heartbeat OBSERVED
        // the steal, so skipping is auditable truth). A Displaced exit
        // DOES release (bug_010): displacement proves only that a
        // newer session won the registry slot — `release` is
        // session-id-predicated, so this is a no-op when the row
        // already belongs to the displacer (the designed reconnect)
        // and frees the row immediately if this session somehow still
        // owns it, instead of stranding it for the stale window.
        if !matches!(exit, LoopExit::LeaseLost)
            && let Err(e) = sessions::release(&self.pool, exec_id, session_id).await
        {
            // Non-fatal: the row goes stale at SESSION_STALE_AFTER and
            // is stolen by the next acquire. Losing the DELETE costs one
            // reader that window of "live session exists but the buffer
            // is gone" (they get the history-only view).
            warn!(%exec_id, error = %e, "AppendLog: ingest session release failed");
        }

        // 4. Tell the builder how it ended. A clean finish closes the
        // ack stream (the builder sees `None` and knows every ack it
        // received is the durable set); an abort sends the status.
        match exit {
            LoopExit::ClientFinished => {
                match drain_stop {
                    None => {}
                    // Replica-local failure: retry-elsewhere is correct.
                    Some(DrainStop::CutFailed) => {
                        ack_try_send(
                            &ack_tx,
                            Err(Status::unavailable(
                                "AppendLog: the final chunk flush failed; \
                                 un-acked lines were not stored — reconnect and replay",
                            )),
                        );
                    }
                    // Per-execution cap: replaying anywhere cannot
                    // succeed — the typed class stops the re-dial.
                    Some(DrainStop::CapExhausted) => {
                        ack_try_send(
                            &ack_tx,
                            Err(gate::cap_rejection(
                                "cap",
                                format!(
                                    "AppendLog: execution exceeded the {}-chunk cap \
                                     during the final drain; un-acked lines were not stored",
                                    self.max_chunks_per_exec
                                ),
                            )),
                        );
                    }
                }
            }
            LoopExit::AbortRefusesInput(status) => {
                match drain_stop {
                    // The accepted tail is durable (or there was none):
                    // the refusal itself is the honest trailer.
                    None => ack_try_send(&ack_tx, Err(status)),
                    // The drain hit the per-execution chunk cap: the
                    // cap verdict supersedes the original refusal —
                    // permanent everywhere, same class either way.
                    Some(DrainStop::CapExhausted) => {
                        ack_try_send(
                            &ack_tx,
                            Err(gate::cap_rejection(
                                "cap",
                                format!(
                                    "AppendLog: execution exceeded the {}-chunk cap \
                                     during the final drain; un-acked lines were not stored",
                                    self.max_chunks_per_exec
                                ),
                            )),
                        );
                    }
                    // The drain FAILED on this replica: the un-acked
                    // tail is rescuable elsewhere, so the trailer is
                    // the per-replica retry vocabulary — NOT the
                    // original (permanent) refusal, which would tell
                    // the builder to abandon lines a healthy replica
                    // would still commit. The byte/idle refusal
                    // re-derives wherever the builder replays (the
                    // caps are durable-seeded).
                    Some(DrainStop::CutFailed) => {
                        ack_try_send(
                            &ack_tx,
                            Err(Status::unavailable(
                                "AppendLog: the final tail flush failed on this replica; \
                                 un-acked lines were not stored — reconnect and replay",
                            )),
                        );
                    }
                }
            }
            LoopExit::AbortCannotCommit(status) => {
                ack_try_send(&ack_tx, Err(status));
            }
            LoopExit::LeaseLost => {
                ack_try_send(
                    &ack_tx,
                    Err(Status::aborted(
                        "AppendLog: the ingest lease for this execution was taken by \
                         another replica; reconnect and replay from the last ack",
                    )),
                );
            }
            LoopExit::Displaced => {
                ack_try_send(
                    &ack_tx,
                    Err(Status::aborted(
                        "AppendLog: a newer ingest session for this execution displaced \
                         this stream; replay from the last ack on the new stream",
                    )),
                );
            }
        }
        info!(%exec_id, %session_id, "AppendLog: ingest session closed");
    }

    /// The select loop. Returns how the stream ended; never performs
    /// cleanup itself. The session-lease heartbeat is NOT an arm here
    /// (bug_148): it runs on the dedicated task `run` spawned, and this
    /// loop only observes its Lost latch — no sibling await can starve
    /// the liveness cadence (`drive_loop_liveness_census` pins this).
    async fn drive(
        &mut self,
        inbound: &mut Streaming<AppendLogRequest>,
        ack_tx: &mpsc::Sender<Result<AppendLogAck, Status>>,
        mut lease_lost: watch::Receiver<bool>,
    ) -> LoopExit {
        let mut cut_interval = tokio::time::interval(self.session_cut_interval());
        cut_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick of a tokio interval fires immediately;
        // consume it so the first periodic cut happens one interval
        // from now, not at stream open.
        cut_interval.tick().await;
        // Housekeeping (the inbound-idle gate + the completeness-
        // ceiling refresh) keeps the heartbeat arm's old cadence; the
        // refresh's PG read is bounded so this arm cannot park the
        // loop past the census's bounds.
        let mut housekeeping_interval = tokio::time::interval(sessions::HEARTBEAT_INTERVAL);
        housekeeping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        housekeeping_interval.tick().await;
        let mut last_inbound = tokio::time::Instant::now();

        loop {
            tokio::select! {
                msg = inbound.message() => match msg {
                    Ok(Some(AppendLogRequest { msg: Some(append_log_request::Msg::Batch(b)) })) => {
                        last_inbound = tokio::time::Instant::now();
                        match self.session.accept(b) {
                            Ok(AcceptOutcome::Accepted { cut_due }) => {
                                if cut_due
                                    && let Some(exit) = self.cut_while_due(ack_tx).await
                                {
                                    return exit;
                                }
                            }
                            // Per-batch rejections: counted inside
                            // `accept`, the stream stays open.
                            Ok(AcceptOutcome::RejectedNonMonotone)
                            | Ok(AcceptOutcome::RejectedOverflow)
                            | Ok(AcceptOutcome::RejectedPastFinal)
                            | Ok(AcceptOutcome::RejectedOversizedBatch) => {}
                            // A replay of content the committed manifest
                            // already covers (merged_bug_002's write-path
                            // arm): nothing was buffered or charged, but
                            // the lines ARE durable — ack them so the
                            // builder's retransmit buffer trims instead
                            // of waiting for a cut that will never carry
                            // them. The ack value arrives pre-clamped to
                            // the contiguous durable frontier
                            // (merged_bug_005); `None` = no sound prefix
                            // claim exists (line 0 is not durable), so
                            // nothing is sent and the builder keeps its
                            // copies. try_send: a missed ack just means
                            // the next replay re-answers.
                            Ok(AcceptOutcome::CoveredReplay { durable_through }) => {
                                if let Some(durable_through_line) = durable_through {
                                    ack_try_send(
                                        ack_tx,
                                        Ok(AppendLogAck {
                                            durable_through_line,
                                            open_coverage_next_line: None,
                                        }),
                                    );
                                }
                            }
                            // Stream-fatal (the per-execution byte
                            // cap — accept()'s ONLY Err construction,
                            // census-pinned). A REFUSAL on a healthy
                            // replica: the accepted under-cap tail
                            // drains before this trailer.
                            Err(status) => return LoopExit::AbortRefusesInput(status),
                        }
                    }
                    Ok(Some(AppendLogRequest { msg: Some(append_log_request::Msg::Header(_)) })) => {
                        // A protocol violation, refused while the
                        // replica is healthy: accepted lines drain.
                        return LoopExit::AbortRefusesInput(Status::invalid_argument(
                            "AppendLog: duplicate header (the stream's identity is set once)",
                        ));
                    }
                    // An empty oneof: a client bug or a future field this
                    // version doesn't know. Ignore rather than abort —
                    // forward compatibility for a new request variant.
                    // It IS inbound traffic, though: liveness evidence
                    // for the idle-abort law, same as any frame.
                    Ok(Some(AppendLogRequest { msg: None })) => {
                        last_inbound = tokio::time::Instant::now();
                    }
                    // Clean half-close: the builder is done.
                    Ok(None) => return LoopExit::ClientFinished,
                    // Transport error: the builder is gone (or the
                    // connection broke). Nothing to tell it.
                    Err(status) => {
                        debug!(error = %status, "AppendLog: inbound stream error");
                        return LoopExit::ClientFinished;
                    }
                },
                _ = cut_interval.tick() => {
                    if let Some(exit) = self.cut_once_if_nonempty(ack_tx).await {
                        return exit;
                    }
                    // A wedged cut future is abandoned (and counted) by
                    // the do_cut watchdog, so hangs always produce
                    // countable failures; the per-tick check converts
                    // them into the failover abort promptly.
                    if let Some(reason) = self.session.should_abort() {
                        return LoopExit::AbortCannotCommit(self.abort_status(reason));
                    }
                }
                // Displaced by a newer session for the same execution
                // (the registry insert cancelled us). Exit now so the
                // admission permits return immediately; the newer
                // session owns the lease and the registry slot, and the
                // builder behind THIS stream is told to reconnect (it
                // is usually the same builder whose reconnect created
                // the newer session — its old stream is dead anyway).
                _ = self.cancel.cancelled() => {
                    metrics::counter!(
                        "rio_store_log_ingest_streams_aborted_total",
                        "reason" => "displaced"
                    )
                    .increment(1);
                    return LoopExit::Displaced;
                }
                // The Lost latch from the dedicated heartbeat task
                // (bug_148): the lease was stolen — stop spending
                // memory, stop acking; committed chunks stay valid.
                res = lease_lost.changed() => {
                    match res {
                        Ok(()) => {
                            // The latch only ever flips false→true.
                            metrics::counter!(
                                "rio_store_log_ingest_streams_aborted_total",
                                "reason" => "lease_lost"
                            )
                            .increment(1);
                            return LoopExit::LeaseLost;
                        }
                        // The beat task died WITHOUT latching Lost
                        // (panic). The lease's liveness is unknown
                        // from here and nothing is renewing it: fail
                        // closed instead of streaming until the
                        // un-renewed row is stolen out from under us.
                        Err(_) => {
                            metrics::counter!(
                                "rio_store_log_ingest_streams_aborted_total",
                                "reason" => "heartbeat_task_died"
                            )
                            .increment(1);
                            // The replica can still commit: the
                            // staged tail drains before the trailer
                            // (concurrent-session overlap is safe by
                            // the session-keyed chunk/dedup law).
                            return LoopExit::AbortRefusesInput(Status::unavailable(
                                "AppendLog: the session heartbeat task ended unexpectedly; \
                                 reconnect and replay from the last ack",
                            ));
                        }
                    }
                }
                _ = housekeeping_interval.tick() => {
                    // A silently-vanished builder (the pod was SIGKILLed,
                    // a netsplit ate the FIN) leaves this stream mute
                    // forever while the lease heartbeats keep renewing
                    // the ingest lease — an immortal driver pinned to a
                    // dead peer. With NOTHING PENDING (buffer and the
                    // cut-staged in-flight run both empty — derived
                    // from oldest_pending_since, never from buffer
                    // bytes alone: a watchdog-abandoned cut leaves
                    // committable lines staged with zero buffer bytes,
                    // and aborting then would destroy what the next
                    // cut's restore_in_flight retries; merged_bug_144)
                    // and inbound silence past four heartbeats, abort:
                    // nothing accepted can be lost, and a CONFORMANT
                    // peer cannot trip this arm — the bilateral
                    // contract (rio_common::liveness) obliges any
                    // client parked on an open, nothing-pending session
                    // to emit keepalive batches well inside the bound
                    // (the builder uploader's empty-batch arm; the
                    // dashboard test's grpcurl writer is its own
                    // producer). No transport-layer input reaches this
                    // arm — h2 PINGs vouch for the CONNECTION, not the
                    // stream's producer, which is exactly why the law
                    // needs an application-layer producer side. The
                    // pending-lines case is covered by the bounded ack
                    // send on the cut path, not this arm.
                    // r[impl store.log.ingest-idle-abort+2]
                    if self.session.no_pending_lines()
                        && last_inbound.elapsed() >= INBOUND_IDLE_BOUND
                    {
                        metrics::counter!(
                            "rio_store_log_ingest_streams_aborted_total",
                            "reason" => "inbound_idle"
                        )
                        .increment(1);
                        return LoopExit::AbortRefusesInput(Status::aborted(
                            "AppendLog: no inbound traffic for 60s with nothing pending; \
                             reconnect and resume from the last ack",
                        ));
                    }
                    // The completeness-ceiling refresh keeps the old
                    // heartbeat-arm cadence (15 s): a seal that lands
                    // mid-stream (the scheduler stamps the terminal row
                    // while the builder is still streaming) is observed
                    // within one tick, after which accept() drops lines
                    // at or past the recorded end. Skipped once known —
                    // the count is stamped once and never changes.
                    // Lines accepted between the stamp and this refresh
                    // are the bounded residual; they are over-coverage
                    // the completeness fold already tolerates, not
                    // silent loss. A refresh failure (PG blip or the
                    // bound expiring) just retries next tick. BOUNDED:
                    // this arm must never park the loop unbounded
                    // (store.log.driver-bounded; the census quotes the
                    // bound).
                    // r[impl store.log.completeness-gate]
                    if self.session.final_line_count().is_none()
                        && let Ok(Ok(Some(n))) = tokio::time::timeout(
                            sessions::HEARTBEAT_RPC_BOUND,
                            gate::sealed_final_line_count(&self.pool, self.session.exec_id),
                        )
                        .await
                    {
                        self.session.set_final_line_count(n.max(0) as u64);
                    }
                }
            }
        }
    }

    /// Cut until the buffer is back under the size threshold
    /// (level-triggered: a buffer with forward gaps drains one
    /// contiguous run per cut and may still be over the threshold).
    /// Returns `Some(exit)` if a cut failure tripped the abort bound.
    async fn cut_while_due(
        &mut self,
        ack_tx: &mpsc::Sender<Result<AppendLogAck, Status>>,
    ) -> Option<LoopExit> {
        while self.session.cut_due() {
            match self.do_cut(ack_tx).await {
                CutStep::Committed | CutStep::Empty => continue,
                CutStep::Failed => {
                    // Don't spin on a failing backend: one failure per
                    // trigger. The failure counter + should_abort()
                    // bound how long this can go on.
                    return self
                        .session
                        .should_abort()
                        .map(|r| LoopExit::AbortCannotCommit(self.abort_status(r)));
                }
                CutStep::Exit(exit) => return Some(exit),
            }
        }
        None
    }

    /// The periodic cut: at most one chunk per tick (the timer fires
    /// again in a minute; there is no need to drain a gap-fragmented
    /// buffer in one tick).
    async fn cut_once_if_nonempty(
        &mut self,
        ack_tx: &mpsc::Sender<Result<AppendLogAck, Status>>,
    ) -> Option<LoopExit> {
        match self.do_cut(ack_tx).await {
            CutStep::Committed | CutStep::Empty => None,
            CutStep::Failed => self
                .session
                .should_abort()
                .map(|r| LoopExit::AbortCannotCommit(self.abort_status(r))),
            CutStep::Exit(exit) => Some(exit),
        }
    }

    /// One cut attempt + the ack + the chunk-count cap.
    /// One chunk cut, watchdog-bounded at one cut interval — the ONLY
    /// callable form of `session.cut` on the driver path (the in-file
    /// self-scan pins this; the watchdog never awaits monitored work
    /// unbounded). `None` = abandoned by the watchdog (counted via
    /// `note_cut_abandoned`; three in a row trip the failover abort).
    /// The staged run is folded back by the next cut's
    /// restore_in_flight. Used by BOTH the periodic cut path and the
    /// final drain — the drain used to call the cut raw, so a wedged
    /// blob backend could hang stream teardown forever.
    async fn cut_bounded(&mut self) -> Option<Result<Option<CutCommit>, CutError>> {
        match tokio::time::timeout(
            self.session_cut_interval(),
            self.session.cut(self.chunk_store.as_ref(), &self.pool),
        )
        .await
        {
            Err(_elapsed) => {
                warn!(
                    exec_id = %self.session.exec_id,
                    bound_secs = self.session_cut_interval().as_secs_f64(),
                    "AppendLog: chunk cut abandoned by the watchdog (hung past one cut interval)"
                );
                self.session.note_cut_abandoned();
                None
            }
            Ok(result) => Some(result),
        }
    }

    async fn do_cut(&mut self, ack_tx: &mpsc::Sender<Result<AppendLogAck, Status>>) -> CutStep {
        if self.session.chunk_attempts() >= self.max_chunks_per_exec {
            metrics::counter!(
                "rio_store_log_ingest_streams_aborted_total",
                "reason" => "chunk_cap"
            )
            .increment(1);
            // bug_068: this status used to be bare RESOURCE_EXHAUSTED —
            // per-replica capacity vocabulary for a per-EXECUTION cap.
            // The builder's classifier read it as retryable-elsewhere
            // and re-dialed at 1 Hz forever; FAILED_PRECONDITION +
            // x-rio-log-reject: cap maps it onto
            // AbandonReason::CapExhausted (one disclosure, done).
            // A per-execution refusal on a healthy replica: the
            // under-cap tail drains first (the drain's own cap check
            // stops it, so the drain is a no-op exactly when the cap
            // arithmetic says nothing more may commit).
            return CutStep::Exit(LoopExit::AbortRefusesInput(gate::cap_rejection(
                "cap",
                format!(
                    "AppendLog: execution exceeded the {}-chunk cap",
                    self.max_chunks_per_exec
                ),
            )));
        }
        // Watchdog semantics (merged_bug_119) live in cut_bounded: a
        // hung PUT/INSERT is abandoned at one cut interval, the
        // driver's abort check, heartbeat, and inbound arms regain
        // liveness, and three abandonments in a row trip
        // ConsecutiveCutFailures → UNAVAILABLE → builder failover.
        match self.cut_bounded().await {
            None => CutStep::Failed,
            Some(Ok(None)) => CutStep::Empty,
            Some(Ok(Some(commit))) => {
                // A commit may carry no ack (merged_bug_005: the run
                // sits past a hole, so no value is a sound contiguous-
                // prefix claim). It is still progress — Committed, the
                // failure counter already reset — there is just
                // nothing true to tell the builder yet.
                let Some(durable_through_line) = commit.durable_ack else {
                    return CutStep::Committed;
                };
                let delivered = send_ack_bounded(
                    ack_tx,
                    Ok(AppendLogAck {
                        durable_through_line,
                        open_coverage_next_line: None,
                    }),
                    self.session_cut_interval(),
                )
                .await;
                if !delivered {
                    // The builder dropped the response stream — or
                    // stopped reading acks while keeping the stream
                    // open (the queue stayed full for a whole cut
                    // interval). The chunk is durable; the builder
                    // just doesn't know. Treat both as a client
                    // disconnect: stop ingesting, drain what's left,
                    // let the reconnect replay the un-acked tail
                    // (idempotently). The raw `send().await` here used
                    // to park the WHOLE driver — ingest, cutting, and
                    // the heartbeat — forever on a full queue.
                    return CutStep::Exit(LoopExit::ClientFinished);
                }
                CutStep::Committed
            }
            Some(Err(e)) => {
                warn!(
                    exec_id = %self.session.exec_id,
                    error = %e,
                    consecutive_failures = "see rio_store_log_chunk_write_failures_total",
                    "AppendLog: chunk cut failed"
                );
                CutStep::Failed
            }
        }
    }

    /// The final drain on clean stream end: one chunk per remaining
    /// contiguous run, until the buffer is empty. Stops at the first
    /// failure (the builder's retransmit buffer still holds the
    /// un-acked tail; burning two more attempts against a failing
    /// backend helps nobody).
    ///
    /// The stop reason is TYPED (bug_068's drain surface): the caller
    /// turns `CapExhausted` into the gate's permanent cap rejection —
    /// telling a builder to "reconnect and replay" against a
    /// per-execution cap is the same 1 Hz storm the mid-stream arm
    /// had — while `CutFailed` keeps the retry-elsewhere semantics
    /// (the failure is this replica's, the lines are still replayable).
    async fn drain(
        &mut self,
        ack_tx: &mpsc::Sender<Result<AppendLogAck, Status>>,
    ) -> Result<(), DrainStop> {
        loop {
            if self.session.chunk_attempts() >= self.max_chunks_per_exec {
                return Err(DrainStop::CapExhausted);
            }
            match self.cut_bounded().await {
                // Watchdog-abandoned mid-drain: stop. The builder's
                // retransmit buffer still holds the un-acked tail, and
                // a wedged backend must not hang stream teardown (the
                // raw cut here used to await unbounded).
                None => return Err(DrainStop::CutFailed),
                Some(Ok(None)) => return Ok(()),
                Some(Ok(Some(commit))) => {
                    // Best-effort: the builder may already be gone (or
                    // not reading); the drain never waits on it. A
                    // commit past a hole carries no ack
                    // (merged_bug_005) — the drain keeps cutting; the
                    // builder's copies of the hole lines replay on
                    // reconnect.
                    if let Some(durable_through_line) = commit.durable_ack {
                        ack_try_send(
                            ack_tx,
                            Ok(AppendLogAck {
                                durable_through_line,
                                open_coverage_next_line: None,
                            }),
                        );
                    }
                }
                Some(Err(e)) => {
                    warn!(
                        exec_id = %self.session.exec_id,
                        error = %e,
                        "AppendLog: final drain cut failed; un-acked lines not stored"
                    );
                    return Err(DrainStop::CutFailed);
                }
            }
        }
    }

    fn abort_status(&self, reason: AbortReason) -> Status {
        metrics::counter!(
            "rio_store_log_ingest_streams_aborted_total",
            "reason" => reason.as_label()
        )
        .increment(1);
        Status::unavailable(format!(
            "AppendLog: this replica cannot durably commit log chunks ({}); \
             reconnect to another replica and replay from the last ack",
            reason.as_label()
        ))
    }

    fn session_cut_interval(&self) -> std::time::Duration {
        // IngestConfig owns the value; the driver just needs it for the
        // timer.
        self.session.config().cut_interval
    }
}

/// Outcome of one `do_cut` call, for the two cut-trigger call sites.
/// Why the final drain stopped before emptying the buffer (bug_068:
/// the two reasons demand OPPOSITE builder dispositions, so the bit
/// must travel typed — `CapExhausted` becomes the gate's permanent
/// cap-class rejection, `CutFailed` keeps retry-elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainStop {
    /// The per-execution chunk-count cap: permanent for this
    /// execution everywhere.
    CapExhausted,
    /// A cut attempt failed or was watchdog-abandoned: this replica's
    /// problem; the un-acked tail replays elsewhere.
    CutFailed,
}

enum CutStep {
    /// A chunk was committed and acked.
    Committed,
    /// The buffer was empty; nothing happened.
    Empty,
    /// The cut failed; the failure counter was bumped.
    Failed,
    /// The stream must end now (the ack channel is closed, or the
    /// chunk cap was hit).
    Exit(LoopExit),
}

/// What `tail_log_stream` hands `serve_tail` when this replica holds
/// the execution's live ingest session: the atomically-taken snapshot
/// of the not-yet-manifest-visible lines, and the receiver for every
/// batch accepted after it. `None` = history-only (no live session
/// here).
type LiveSubscription = Option<(
    Vec<(u64, Vec<u8>)>,
    mpsc::Receiver<Arc<FanBatch>>,
    // Weak: serve_tail must NOT keep the IngestShared (and with it the
    // fan-out sender registered in `subscribers`) alive — the live
    // loop's rx closes exactly when the driver and the registry drop
    // their Arcs. Upgraded only for the moment of a gap back-fill.
    std::sync::Weak<Mutex<IngestShared>>,
    // The fan-out drop epoch: bumped when the ingest dropped batches
    // for ANY subscriber, so a burst-end drop is backfilled without
    // waiting for output that may never come (merged_bug_187).
    // Latching — a bump made while the loop was mid-send is observed
    // at its next select.
    watch::Receiver<u64>,
)>;

/// The whole TailLog response body: history (manifest chunks), then the
/// live snapshot, then the subscription stream, all deduplicated by one
/// [`LineCursor`] and re-chunked into ≤[`TAIL_CHUNK_LINES`]-line
/// messages.
///
/// Memory bound: one manifest chunk's decompressed lines, or one
/// fan-out batch, resident at a time — never the whole log.
// r[impl store.log.tail-reconnect]
async fn serve_tail(
    pool: PgPool,
    store: Arc<dyn LogChunkStore>,
    owned: tail::OwnedExec,
    since_line: u64,
    follow: bool,
    subscription: LiveSubscription,
    tx: &mpsc::Sender<Result<TailLogChunk, Status>>,
) -> Result<(), Status> {
    // The serve layer takes the authorization witness, never a raw
    // Uuid: a future serve path that skips `authorize_tail` does not
    // compile (merged_bug_064 chokepoint).
    let exec_id = owned.id();
    let mut cursor = LineCursor::new(since_line);
    let exec_str = exec_id.to_string();

    // -- Phase 1: the manifest (everything already durably chunked).
    let refs = tail::read_manifest_range(&pool, exec_id, since_line).await?;
    for (i, chunk) in refs.iter().enumerate() {
        let lines = tail::read_chunk(
            store.as_ref(),
            Some(&pool),
            chunk,
            &refs[i + 1..],
            &mut cursor,
        )
        .await?;
        send_lines(tx, &exec_str, lines, false).await?;
    }

    // -- Phase 2 + 3: the live snapshot and the subscription. Only
    // present when this replica holds the execution's ingest session.
    let Some((snapshot, mut rx, shared, mut wake)) = subscription else {
        // History-only. The final (possibly empty) message carries the
        // computed completeness so the CLI/dashboard can render their
        // "(log incomplete)" notice.
        let claim = gate::final_claim_for(&pool, exec_id, &cursor).await?;
        send_final(tx, &exec_str, claim).await?;
        return Ok(());
    };

    // The snapshot: every accepted-but-not-yet-manifest-visible line at
    // the instant the subscriber registered. Overlaps with the manifest
    // when a cut committed between the snapshot and the manifest read;
    // the kernel verdicts dedup run by run.
    serve_view_runs(tx, &exec_str, &mut cursor, snapshot).await?;

    if !follow {
        let claim = gate::final_claim_for(&pool, exec_id, &cursor).await?;
        send_final(tx, &exec_str, claim).await?;
        return Ok(());
    }

    // r[impl store.log.attach-hello]
    // The attach handshake (merged_bug_067): one zero-line, non-final,
    // exec-stamped hello chunk between the replayed history and the
    // live subscription. A reconnecting follower whose cursor sits
    // beyond everything this execution holds (the follow-the-retry
    // shape: the previous execution's log was longer) would otherwise
    // see total silence -- phases 1-2 emit nothing and the
    // subscription says nothing until the builder's next batch -- so a
    // dead-quiet fresh execution was indistinguishable from a live
    // stream on the old one. The client's exec-keyed visit fires its
    // execution switch off this one chunk. Zero-line chunks are
    // protocol-tolerated by every consumer (store.proto: "clients must
    // tolerate and skip them").
    let _ = tx
        .send(Ok(TailLogChunk {
            exec_id: exec_str.clone(),
            lines: Vec::new(),
            first_line_number: cursor.next_line(),
            is_complete: false,
        }))
        .await;

    // The subscription: every batch accepted after the snapshot, until
    // the ingest session ends (the sender side of `rx` is dropped when
    // the `IngestShared` it lives in is dropped, i.e. when the
    // AppendLog driver task and any TailLog snapshot holders release
    // their `Arc`s) or the client goes away. The `tx.closed()` arm
    // matters for the quiet-build case: without it a disconnected
    // reader is only noticed at the next `send_lines`, which for a
    // build that has gone silent leaves this task (and its subscriber
    // queue, and the `tail_subscribers` gauge) parked until the next
    // line arrives.
    // r[impl store.log.tail-fanout-recovery]
    // r[impl store.log.gap-provenance]
    let mut wake_armed = true;
    loop {
        enum Pulse {
            Batch(Arc<FanBatch>),
            Woken,
            WakeClosed,
            Ended,
        }
        let pulse = tokio::select! {
            batch = rx.recv() => match batch {
                Some(b) => Pulse::Batch(b),
                None => Pulse::Ended,
            },
            // Fan-out drops wake every subscriber: a burst that ENDS in
            // drops is backfilled here instead of parking until the
            // next accepted batch (which for a quiet build never
            // comes). A subscriber whose own queue kept the batch
            // recovers nothing and serves nothing — benign. The watch
            // epoch LATCHES: a drop that happened while this loop was
            // blocked in send_lines (the stalled-reader case — exactly
            // when drops occur) is observed here, not lost.
            changed = wake.changed(), if wake_armed => match changed {
                Ok(()) => Pulse::Woken,
                Err(_) => Pulse::WakeClosed,
            },
            _ = tx.closed() => return Ok(()),
        };
        let fan = match pulse {
            Pulse::Ended => break,
            Pulse::WakeClosed => {
                // The session (or the dummy sender) is gone; rx's
                // None will end the loop. Disarm so a closed watch
                // cannot busy-spin the select.
                wake_armed = false;
                continue;
            }
            Pulse::Woken => {
                backfill_from_views(
                    &pool,
                    store.as_ref(),
                    exec_id,
                    &exec_str,
                    &shared,
                    &mut cursor,
                    tx,
                )
                .await?;
                continue;
            }
            Pulse::Batch(fan) => fan,
        };
        let n_lines = fan.batch.lines.len() as u64;
        let verdict = visit_fanout_batch(
            cursor.next_line(),
            fan.batch.first_line_number,
            n_lines,
            fan.coverage_floor,
        );
        let visit = match verdict.provenance {
            // No gap — or a gap the store never accepted lines for
            // (worker-admitted): serve across with the typed verdict.
            // NO buffer clone, NO manifest read, NO recovery counter —
            // a worker cannot drive amplification by emitting jumps
            // (merged_bug_187).
            None | Some(GapProvenance::AdmittedHole) => verdict.visit,
            // The missing span WAS accepted and this subscriber's
            // lossy queue dropped it: ONE counted backfill from
            // manifest ∪ live buffer, then re-visit.
            Some(GapProvenance::DroppedSpan { .. }) => {
                backfill_from_views(
                    &pool,
                    store.as_ref(),
                    exec_id,
                    &exec_str,
                    &shared,
                    &mut cursor,
                    tx,
                )
                .await?;
                let revisit = visit_fanout_batch(
                    cursor.next_line(),
                    fan.batch.first_line_number,
                    n_lines,
                    fan.coverage_floor,
                );
                if matches!(revisit.provenance, Some(GapProvenance::DroppedSpan { .. })) {
                    // A residual ACCEPTED gap survived the backfill:
                    // the ingest coverage contract failed (the span is
                    // in neither the manifest nor the live buffer).
                    // Finalize at the UN-advanced cursor — the claim
                    // is structurally incomplete (the cursor is below
                    // the sealed end) — and let the client's reconnect
                    // contract take over. Advancing across the span,
                    // the old shape, silently disowned the lines
                    // (merged_bug_205).
                    let claim = gate::final_claim_for(&pool, exec_id, &cursor).await?;
                    send_final(tx, &exec_str, claim).await?;
                    return Ok(());
                }
                revisit.visit
            }
        };
        match visit {
            ChunkVisit::Skip { .. } => {}
            ChunkVisit::Serve {
                yield_from,
                yield_until,
                ..
            }
            | ChunkVisit::GapThenServe {
                yield_from,
                yield_until,
                ..
            } => {
                let first = fan.batch.first_line_number;
                let lines: Vec<(u64, Vec<u8>)> = fan.batch.lines
                    [(yield_from - first) as usize..(yield_until - first) as usize]
                    .iter()
                    .enumerate()
                    .map(|(i, l)| (yield_from + i as u64, l.clone()))
                    .collect();
                send_lines(tx, &exec_str, lines, false).await?;
                cursor.advance_to(visit.advance());
            }
        }
    }

    // The ingest session ended. Catch up to everything that became
    // durable after the last live batch before finalizing — the
    // driver's final drain cuts the remaining buffer, so a fan-out
    // drop on the LAST batches must not become a missing tail the
    // final message advertises past.
    let mut recovered = false;
    // INVARIANT (merged_bug_063 dead-arm deletion): `rx.recv()`
    // returned `None` ⇔ every `IngestShared` `Arc` was dropped ⇔ the
    // `Weak` cannot upgrade — the post-session "snapshot" half of this
    // catch-up was provably empty on every execution. The final drain
    // cuts the remaining buffer to chunks before the session drops, so
    // the manifest walk below holds everything.
    let refs = tail::read_manifest_range(&pool, exec_id, cursor.next_line()).await?;
    for (i, chunk) in refs.iter().enumerate() {
        let lines = tail::read_chunk(
            store.as_ref(),
            Some(&pool),
            chunk,
            &refs[i + 1..],
            &mut cursor,
        )
        .await?;
        recovered |= !lines.is_empty();
        send_lines(tx, &exec_str, lines, false).await?;
    }
    if recovered {
        metrics::counter!("rio_store_log_tail_fanout_recovered_total").increment(1);
    }
    // Tell the reader where things stand; the client's reconnect
    // contract takes it from here. The claim is minted against the
    // SERVED cursor — a seal+cut that committed mid-serve yields
    // complete=false and the reconnect heals.
    let claim = gate::final_claim_for(&pool, exec_id, &cursor).await?;
    send_final(tx, &exec_str, claim).await?;
    Ok(())
}

/// Serve a window of `(line_number, bytes)` pairs drawn from an
/// AUTHORITATIVE view — the live snapshot, or the backfill's recovered
/// buffer — one kernel verdict per contiguous run, the cursor moved
/// only by sealed advances (merged_bug_205: the open-coded
/// `filter(>= cursor)` + `advance_to(end + 1)` fold, which silently
/// absorbed residual gaps, no longer typechecks). A run starting past
/// the watermark here is an ingest-admitted hole BY CONSTRUCTION (the
/// view holds every accepted line at or past the cursor), so the
/// `GapThenServe` verdict serves across it.
///
/// Returns `true` if any line was sent (the backfill's recovery
/// telemetry).
async fn serve_view_runs(
    tx: &mpsc::Sender<Result<TailLogChunk, Status>>,
    exec_id: &str,
    cursor: &mut LineCursor,
    lines: Vec<(u64, Vec<u8>)>,
) -> Result<bool, Status> {
    let mut served = false;
    let mut i = 0usize;
    while i < lines.len() {
        // The maximal contiguous run starting at i.
        let first = lines[i].0;
        let mut len = 1usize;
        while i + len < lines.len() && lines[i + len].0 == first + len as u64 {
            len += 1;
        }
        let visit = visit_chunk(cursor.next_line(), first, len as u64);
        match visit {
            ChunkVisit::Skip { .. } => {}
            ChunkVisit::Serve {
                yield_from,
                yield_until,
                ..
            }
            | ChunkVisit::GapThenServe {
                yield_from,
                yield_until,
                ..
            } => {
                let lo = i + (yield_from - first) as usize;
                let hi = i + (yield_until - first) as usize;
                let out: Vec<(u64, Vec<u8>)> = lines[lo..hi].to_vec();
                served |= !out.is_empty();
                send_lines(tx, exec_id, out, false).await?;
                cursor.advance_to(visit.advance());
            }
        }
        i += len;
    }
    Ok(served)
}

/// The fan-out drop recovery: everything cut to chunks since the
/// cursor, then the live buffer's remainder, in the coverage-contract
/// order (snapshot CLONED first, served after the manifest — a cut
/// committing between a manifest-first read and the snapshot would
/// hide its lines from both views; overlap the other way is removed by
/// the kernel verdicts). Increments the recovery counter only when
/// lines were actually recovered.
async fn backfill_from_views(
    pool: &PgPool,
    store: &dyn LogChunkStore,
    exec_id: Uuid,
    exec_str: &str,
    shared: &std::sync::Weak<Mutex<IngestShared>>,
    cursor: &mut LineCursor,
    tx: &mpsc::Sender<Result<TailLogChunk, Status>>,
) -> Result<bool, Status> {
    // (a) The live buffer FIRST — `in_flight ++ buffer` in one critical
    // section, per the IngestShared coverage contract (ingest.rs): a
    // line leaves that view only once its cut has COMMITTED, so the
    // manifest read below is then guaranteed to hold it. The clone is
    // bounded below by the cursor (snapshot_since) — the reader never
    // pays in-lock for lines it will discard. The session may have
    // ended between the drop and now — then the manifest walk holds
    // everything.
    let snap: Vec<(u64, Vec<u8>)> = match shared.upgrade() {
        Some(shared) => lock_shared(&shared).snapshot_since(cursor.next_line()),
        None => Vec::new(),
    };
    // (b) Everything cut to chunks since the cursor.
    let mut recovered = false;
    let refs = tail::read_manifest_range(pool, exec_id, cursor.next_line()).await?;
    for (i, chunk) in refs.iter().enumerate() {
        let lines = tail::read_chunk(store, Some(pool), chunk, &refs[i + 1..], cursor).await?;
        recovered |= !lines.is_empty();
        send_lines(tx, exec_str, lines, false).await?;
    }
    // (c) The snapshot, deduplicated run-by-run through the
    // post-manifest cursor.
    recovered |= serve_view_runs(tx, exec_str, cursor, snap).await?;
    if recovered {
        metrics::counter!("rio_store_log_tail_fanout_recovered_total").increment(1);
    }
    Ok(recovered)
}

/// Send a run of `(line_number, bytes)` pairs as ≤[`TAIL_CHUNK_LINES`]-
/// line `TailLogChunk` messages. Splits at line-number discontinuities
/// as well as at the size bound — a `TailLogChunk` is implicitly
/// contiguous (`first_line_number + index`), so a gap needs a new
/// message.
async fn send_lines(
    tx: &mpsc::Sender<Result<TailLogChunk, Status>>,
    exec_id: &str,
    lines: Vec<(u64, Vec<u8>)>,
    is_complete: bool,
) -> Result<(), Status> {
    let mut iter = lines.into_iter().peekable();
    while iter.peek().is_some() {
        let (first, _) = *iter.peek().expect("peeked");
        let mut out: Vec<Vec<u8>> = Vec::new();
        let mut next = first;
        while out.len() < TAIL_CHUNK_LINES {
            match iter.peek() {
                Some((n, _)) if *n == next => {
                    out.push(iter.next().expect("peeked").1);
                    next += 1;
                }
                _ => break,
            }
        }
        let msg = TailLogChunk {
            exec_id: exec_id.to_string(),
            lines: out,
            first_line_number: first,
            is_complete,
        };
        if tx.send(Ok(msg)).await.is_err() {
            // The client is gone. Status::cancelled propagates up to
            // serve_tail's caller, which tries to send it (and fails,
            // harmlessly) — the point is to stop reading chunks for a
            // reader that left.
            return Err(Status::cancelled("TailLog: client disconnected"));
        }
    }
    Ok(())
}

/// The terminal message of a non-follow (or session-ended) read: zero
/// lines, the resume cursor, and the computed completeness.
async fn send_final(
    tx: &mpsc::Sender<Result<TailLogChunk, Status>>,
    exec_id: &str,
    claim: rio_log_kernel::FinalClaim,
) -> Result<(), Status> {
    let _ = tx
        .send(Ok(TailLogChunk {
            exec_id: exec_id.to_string(),
            lines: Vec::new(),
            first_line_number: claim.cursor_next(),
            is_complete: claim.complete(),
        }))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::chunks::MemoryLogChunkStore;
    use rio_auth::hmac::{AssignmentClaims, HmacSigner, HmacVerifier};
    use rio_proto::store::log_service_server::LogServiceServer;
    use rio_proto::types::BuildLogBatch;
    use rio_test_support::TestDb;
    use rio_test_support::grpc::spawn_grpc_server;
    use sqlx::PgPool;
    use tonic::transport::Server;
    use uuid::Uuid;

    const TEST_KEY: &[u8] = b"log-service-test-hmac-key-32byte";
    /// The DAG-key form (what the scheduler signs into the token and
    /// inserts into `derivations.drv_hash`).
    const DRV: &str = "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-hello-2.12.drv";
    /// The full-store-path form (what the builder puts in the header).
    const DRV_PATH: &str = "/nix/store/0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm-hello-2.12.drv";

    // ------------------------------------------------------------------
    // Harness
    // ------------------------------------------------------------------

    struct Harness {
        db: TestDb,
        chunk_store: Arc<MemoryLogChunkStore>,
        client: LogServiceClient<tonic::transport::Channel>,
        active: Arc<DashMap<Uuid, IngestEntry>>,
        _server: tokio::task::JoinHandle<()>,
    }

    /// Spawn an in-process LogService over real PG + an in-memory chunk
    /// store, with HMAC verification enabled and a tiny cut threshold so
    /// tests can force chunk cuts with a few hundred bytes.
    async fn harness_with(
        max_streams: usize,
        configure: impl FnOnce(LogServiceImpl) -> LogServiceImpl,
    ) -> Harness {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        let (client, active, server, _uri) =
            spawn_replica(&db.pool, &chunk_store, "test-pod", max_streams, configure).await;
        Harness {
            db,
            chunk_store,
            client,
            active,
            _server: server,
        }
    }

    /// Spawn one LogService replica against an existing pool + chunk
    /// store. Tests that need the cross-replica semantics (the
    /// single-live-session lease) spawn a second one with a different
    /// pod name.
    async fn spawn_replica(
        pool: &PgPool,
        chunk_store: &Arc<MemoryLogChunkStore>,
        pod: &str,
        max_streams: usize,
        configure: impl FnOnce(LogServiceImpl) -> LogServiceImpl,
    ) -> (
        LogServiceClient<tonic::transport::Channel>,
        Arc<DashMap<Uuid, IngestEntry>>,
        tokio::task::JoinHandle<()>,
        String,
    ) {
        let svc = LogServiceImpl::new(
            pool.clone(),
            Arc::clone(chunk_store) as Arc<dyn LogChunkStore>,
            pod.to_string(),
        )
        .with_hmac_verifier(Arc::new(HmacVerifier::from_key(TEST_KEY.to_vec())))
        .with_max_streams(max_streams)
        .with_ingest_config(IngestConfig {
            // Small enough that a handful of lines forces a cut;
            // large enough that single-batch tests stay un-cut.
            cut_threshold_bytes: 4096,
            ..IngestConfig::default()
        });
        let svc = configure(svc);
        let active = svc.active_ingests();
        let router = Server::builder().add_service(LogServiceServer::new(svc));
        let (addr, server) = spawn_grpc_server(router).await;
        let uri = format!("http://{addr}");
        let client = LogServiceClient::connect(uri.clone())
            .await
            .expect("connect to in-process LogService");
        (client, active, server, uri)
    }

    async fn harness() -> Harness {
        harness_with(256, |s| s).await
    }

    fn token(executor_id: &str, drv_hash: &str) -> String {
        HmacSigner::from_key(TEST_KEY.to_vec()).sign(&AssignmentClaims {
            executor_id: executor_id.to_string(),
            drv_hash: drv_hash.to_string(),
            expected_outputs: vec![],
            is_ca: false,
            expiry_unix: u64::MAX,
            tenant: None,
        })
    }

    /// Seed the scheduler-owned rows the gate and the read path need: a
    /// derivation, its (only) assignment attempt, and the
    /// `drv_executions` lifecycle row the scheduler INSERTs at dispatch
    /// (always before the builder opens its log stream). Returns the
    /// exec_id the token holder is authorized for.
    async fn seed_tenant(pool: &PgPool, name: &str) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO tenants (tenant_name) VALUES ($1) RETURNING tenant_id",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn tenant_claims(sub: Uuid) -> rio_auth::jwt::TenantClaims {
        rio_auth::jwt::TenantClaims {
            sub,
            iat: 0,
            exp: i64::MAX,
            jti: String::from("test-jti"),
        }
    }

    /// Direct-impl TailLog call with claims attached, returning the
    /// first stream item (None = empty history stream).
    async fn tail_first_item(
        svc: &LogServiceImpl,
        exec: Uuid,
        claims: Option<rio_auth::jwt::TenantClaims>,
    ) -> Option<Result<TailLogChunk, Status>> {
        let mut request = Request::new(TailLogRequest {
            derivation: DRV_PATH.to_string(),
            exec_id: exec.to_string(),
            since_line: 0,
            follow: false,
        });
        if let Some(c) = claims {
            request.extensions_mut().insert(c);
        }
        let mut stream = svc.tail_log(request).await.unwrap().into_inner();
        use tokio_stream::StreamExt as _;
        stream.next().await
    }

    /// The derivation_id `seed_assignment` upserted for [`DRV`].
    async fn derivation_id_of(pool: &PgPool) -> Uuid {
        sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(DRV)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Foreign tenant ⇒ absence-shaped NotFound: byte-identical to the
    /// missing-execution error, so an authenticated caller cannot
    /// distinguish "exists but foreign" from "never existed"
    /// (merged_bug_064-c — the old distinguishable PermissionDenied was
    /// a cross-tenant existence oracle).
    ///
    /// RED (pre-fix, production-shaped seeding): the OLD gate keyed on
    /// never-written `derivations.tenant_id`, denying via
    /// PermissionDenied — distinguishable from absent.
    // r[verify store.log.method-credential+2]
    // r[verify store.log.tail-ownership]
    #[tokio::test]
    async fn taillog_foreign_tenant_gets_absence_shaped_notfound() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        let exec = seed_assignment(&db.pool, "builder-own").await;
        let owner = seed_tenant(&db.pool, "tenant-owner").await;
        tail::seed_production_ownership(&db.pool, owner, derivation_id_of(&db.pool).await).await;
        let foreign = seed_tenant(&db.pool, "tenant-foreign").await;
        let svc = LogServiceImpl::new(db.pool.clone(), chunk_store, "test-pod".to_string());
        match tail_first_item(&svc, exec, Some(tenant_claims(foreign))).await {
            Some(Err(status)) => {
                assert_eq!(
                    status.code(),
                    tonic::Code::NotFound,
                    "foreign tenant must get the absence-shaped NOT_FOUND, got {status:?}"
                );
                // Oracle closure: the deny text equals the genuinely-
                // absent text for the same pinned id.
                assert_eq!(
                    status.message(),
                    format!("no log recorded for execution {exec}"),
                    "foreign deny must be byte-identical to the absent-row error"
                );
            }
            other => panic!(
                "a verified-but-foreign tenant read the log (got {other:?}); \
                 ownership must be enforced"
            ),
        }
    }

    /// The owning tenant is admitted via the PRODUCTION-written chain
    /// (assignments→build_derivations→builds.tenant_id).
    ///
    /// RED (pre-fix): under production-shaped seeding the OLD gate read
    /// `derivations.tenant_id` — never written ⇒ ownership was
    /// constant-FALSE and the OWNER was denied (merged_bug_064-b: the
    /// legacy fixtures wrote the dead column, so the suite proved a
    /// vacuous truth).
    // r[verify store.log.method-credential+2]
    // r[verify store.log.tail-ownership]
    #[tokio::test]
    async fn taillog_owner_admitted_under_production_seeding() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        let exec = seed_assignment(&db.pool, "builder-own2").await;
        let owner = seed_tenant(&db.pool, "tenant-owner2").await;
        tail::seed_production_ownership(&db.pool, owner, derivation_id_of(&db.pool).await).await;
        let svc = LogServiceImpl::new(db.pool.clone(), chunk_store, "test-pod".to_string());
        if let Some(Err(status)) = tail_first_item(&svc, exec, Some(tenant_claims(owner))).await {
            panic!("the owning tenant must be admitted, got {status:?}");
        }
    }

    /// Swept-assignment arm: assignment row gone (retention), ownership
    /// still resolves through drv_executions⨝derivations (the
    /// execution's OWN recorded hash) → build membership.
    // r[verify store.log.method-credential+2]
    // r[verify store.log.tail-ownership]
    #[tokio::test]
    async fn taillog_owner_admitted_via_swept_assignment_arm() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        let exec = seed_assignment(&db.pool, "builder-swept").await;
        let owner = seed_tenant(&db.pool, "tenant-swept").await;
        tail::seed_production_ownership(&db.pool, owner, derivation_id_of(&db.pool).await).await;
        sqlx::query("DELETE FROM assignments WHERE exec_id = $1")
            .bind(exec)
            .execute(&db.pool)
            .await
            .unwrap();
        let svc = LogServiceImpl::new(db.pool.clone(), chunk_store, "test-pod".to_string());
        if let Some(Err(status)) = tail_first_item(&svc, exec, Some(tenant_claims(owner))).await {
            panic!("swept-assignment owner must be admitted, got {status:?}");
        }
    }

    /// merged_bug_064-a (the IDOR): request the caller's OWN derivation
    /// verbatim while pinning a FOREIGN exec_id. The request string is
    /// in NO ownership predicate, so owning some unrelated derivation
    /// authorizes nothing about the pinned execution.
    ///
    /// RED (pre-fix): the old fallback matched the REQUEST string's
    /// hash prefix against any derivation the caller's tenant owned —
    /// own-drv + foreign-pin streamed the foreign log.
    // r[verify store.log.method-credential+2]
    // r[verify store.log.tail-ownership]
    #[tokio::test]
    async fn taillog_foreign_pin_with_own_derivation_rejected() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        // Foreign tenant's execution of DRV.
        let exec = seed_assignment(&db.pool, "builder-own4").await;
        let owner = seed_tenant(&db.pool, "tenant-owner4").await;
        tail::seed_production_ownership(&db.pool, owner, derivation_id_of(&db.pool).await).await;
        // The attacker tenant legitimately owns an UNRELATED derivation.
        let attacker = seed_tenant(&db.pool, "tenant-attacker").await;
        let attacker_drv: Uuid = sqlx::query_scalar(
            "INSERT INTO derivations (drv_hash, drv_path, system, status) \
             VALUES ('zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-other.drv', '/nix/store/zz-other.drv', \
                     'x86_64-linux', 'assigned') \
             RETURNING derivation_id",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        tail::seed_production_ownership(&db.pool, attacker, attacker_drv).await;
        let svc = LogServiceImpl::new(db.pool.clone(), chunk_store, "test-pod".to_string());
        // Verbatim own derivation + wildcards: every shape must take
        // the absence-shaped deny — the string reaches no predicate.
        for derivation in [
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-other.drv",
            "%",
            "_",
            "zz%",
        ] {
            let mut request = Request::new(TailLogRequest {
                derivation: derivation.to_string(),
                exec_id: exec.to_string(),
                since_line: 0,
                follow: false,
            });
            request.extensions_mut().insert(tenant_claims(attacker));
            let mut stream = svc.tail_log(request).await.unwrap().into_inner();
            use tokio_stream::StreamExt as _;
            match stream.next().await {
                Some(Err(status)) => assert_eq!(
                    status.code(),
                    tonic::Code::NotFound,
                    "derivation {derivation:?} must take the absence-shaped deny, got {status:?}"
                ),
                other => panic!(
                    "derivation {derivation:?} laundered ownership of a foreign pin \
                     (cross-tenant read); got {other:?}"
                ),
            }
        }
    }

    /// Execution with no owning build (no builds row at all) + verified
    /// claims ⇒ fail closed, absence-shaped.
    #[tokio::test]
    async fn taillog_buildless_execution_fails_closed() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        let exec = seed_assignment(&db.pool, "builder-own3").await;
        let any = seed_tenant(&db.pool, "tenant-any").await;
        let svc = LogServiceImpl::new(db.pool.clone(), chunk_store, "test-pod".to_string());
        match tail_first_item(&svc, exec, Some(tenant_claims(any))).await {
            Some(Err(status)) => assert_eq!(status.code(), tonic::Code::NotFound),
            other => panic!("build-less execution must fail closed, got {other:?}"),
        }
    }

    async fn seed_assignment(pool: &PgPool, builder_id: &str) -> Uuid {
        let exec = Uuid::now_v7();
        let derivation_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO derivations (drv_hash, drv_path, system, status) \
             VALUES ($1, $2, 'x86_64-linux', 'assigned') \
             ON CONFLICT (drv_hash) DO UPDATE SET drv_path = EXCLUDED.drv_path \
             RETURNING derivation_id",
        )
        .bind(DRV)
        .bind(DRV_PATH)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at) \
             VALUES ($1, $2, $3, now())",
        )
        .bind(exec)
        .bind(rio_nix::store_path::drv_log_hash(DRV))
        .bind(builder_id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO assignments \
                 (derivation_id, builder_id, generation, status, assigned_at, exec_id) \
             VALUES ($1, $2, 1, 'acknowledged', now(), $3)",
        )
        .bind(derivation_id)
        .bind(builder_id)
        .bind(exec)
        .execute(pool)
        .await
        .unwrap();
        exec
    }

    fn header_msg(exec_id: Uuid) -> AppendLogRequest {
        AppendLogRequest {
            msg: Some(append_log_request::Msg::Header(AppendLogHeader {
                derivation_path: DRV_PATH.to_string(),
                exec_id: exec_id.to_string(),
            })),
        }
    }

    fn batch_msg(first_line: u64, lines: &[&str]) -> AppendLogRequest {
        AppendLogRequest {
            msg: Some(append_log_request::Msg::Batch(BuildLogBatch {
                derivation_path: String::new(),
                lines: lines.iter().map(|l| l.as_bytes().to_vec()).collect(),
                first_line_number: first_line,
                executor_id: String::new(),
            })),
        }
    }

    /// Open an AppendLog stream: returns the request sender and the ack
    /// stream. Dropping the sender half-closes the stream.
    ///
    /// `first_msgs` are buffered into the request stream BEFORE the call
    /// is awaited — the server does not return its response headers (and
    /// so `client.append_log()` does not resolve) until it has read and
    /// validated the header message, so a client that awaits the call
    /// before sending the header deadlocks. This is the AppendLog open
    /// contract: send the header, then await the call, then stream
    /// batches. (The builder client in commit 4 does the same.)
    async fn open_append(
        client: &mut LogServiceClient<tonic::transport::Channel>,
        token: &str,
        first_msgs: Vec<AppendLogRequest>,
    ) -> Result<(mpsc::Sender<AppendLogRequest>, Streaming<AppendLogAck>), Status> {
        let (tx, rx) = mpsc::channel(16);
        for m in first_msgs {
            tx.send(m).await.expect("buffering into an open channel");
        }
        let mut req = Request::new(ReceiverStream::new(rx));
        req.metadata_mut().insert(
            rio_proto::ASSIGNMENT_TOKEN_HEADER,
            token.parse().expect("token is ascii"),
        );
        let resp = client.append_log(req).await?;
        Ok((tx, resp.into_inner()))
    }

    /// Drain a TailLog response stream into (lines, final_is_complete).
    /// Asserts the stream's line numbers are strictly increasing across
    /// messages (no duplicates, no reordering).
    async fn collect_tail(
        mut stream: Streaming<TailLogChunk>,
    ) -> Result<(Vec<(u64, Vec<u8>)>, bool), Status> {
        let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut is_complete = false;
        while let Some(chunk) = stream.message().await? {
            for (i, line) in chunk.lines.iter().enumerate() {
                let n = chunk.first_line_number + i as u64;
                if let Some((last, _)) = out.last() {
                    assert!(
                        n > *last,
                        "TailLog yielded line {n} after line {last}: duplicate or reordered"
                    );
                }
                out.push((n, line.clone()));
            }
            is_complete = chunk.is_complete;
        }
        Ok((out, is_complete))
    }

    fn tail_req(exec_id: Uuid, follow: bool) -> TailLogRequest {
        TailLogRequest {
            derivation: DRV_PATH.to_string(),
            exec_id: exec_id.to_string(),
            since_line: 0,
            follow,
        }
    }

    // ------------------------------------------------------------------
    // 1. The round trip
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn append_then_tail_roundtrip() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, mut acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        tx.send(batch_msg(0, &["line zero", "line one"]))
            .await
            .unwrap();
        tx.send(batch_msg(2, &["line two"])).await.unwrap();
        tx.send(batch_msg(3, &["line three"])).await.unwrap();
        drop(tx); // half-close: the builder is done

        // The final drain commits one chunk covering lines 0..=3 and
        // acks it before the stream closes.
        let ack = acks.message().await.expect("ack").expect("ack present");
        assert_eq!(ack.durable_through_line, 3);
        assert!(acks.message().await.expect("clean close").is_none());

        // One chunk object exists.
        assert_eq!(h.chunk_store.len(), 1, "one contiguous run = one chunk");

        // The registry entry is cleaned up after the stream closes.
        assert!(
            h.active.get(&exec).is_none(),
            "the active-ingest registry entry must be removed on stream close"
        );

        // A non-follow TailLog returns the same lines in order.
        let resp = h
            .client
            .tail_log(tail_req(exec, false))
            .await
            .expect("tail");
        let (lines, is_complete) = collect_tail(resp.into_inner()).await.expect("collect");
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(lines[0].1, b"line zero".to_vec());
        assert_eq!(lines[3].1, b"line three".to_vec());
        assert!(
            !is_complete,
            "no terminal drv_executions row yet => the log cannot be complete"
        );
    }

    /// W11-F (bug_032): the open-time coverage ack, end to end — the
    /// letter's emission face. A reconnect to an execution with
    /// durable contiguous coverage receives, as the FIRST ack-stream
    /// message, the open ack: `open_coverage_next_line = Some(W)` with
    /// `durable_through_line = W - 1` (a true durability statement
    /// even for a legacy client that ignores the watermark field). A
    /// fresh execution gets NO open ack — its first ack is a chunk
    /// ack, watermark-absent (the legacy chunk-ack shape; absent =
    /// no-trim semantics, per the field contract). The server-side
    /// trim/charge correctness for a NON-trimming client is W11-E's
    /// straddling cell (gate.rs).
    #[tokio::test]
    async fn open_ack_carries_the_coverage_watermark() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        // Session 1 (fresh exec): the first ack is the drain's CHUNK
        // ack — no watermark field (the legacy shape).
        let (tx, mut acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        tx.send(batch_msg(
            0,
            &["line zero", "line one", "line two", "line three"],
        ))
        .await
        .unwrap();
        drop(tx);
        let ack = acks.message().await.expect("ack").expect("ack present");
        assert_eq!(ack.durable_through_line, 3);
        assert_eq!(
            ack.open_coverage_next_line, None,
            "a fresh execution has no coverage to advertise: chunk acks \
             never carry the watermark"
        );
        assert!(acks.message().await.expect("clean close").is_none());

        // Session 2 (the reconnect): durable contiguous coverage is
        // [0,4) — the FIRST message on the ack stream is the open-time
        // coverage ack.
        let (tx, mut acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("reopen");
        let ack = acks.message().await.expect("ack").expect("ack present");
        assert_eq!(
            ack.open_coverage_next_line,
            Some(4),
            "left (pre-fix): no open-time letter exists — the builder \
             replays its full buffer blind / right: the open ack advertises \
             the durable coverage watermark before any replay"
        );
        assert_eq!(
            ack.durable_through_line, 3,
            "the open ack is a true durability statement for legacy clients"
        );
        drop(tx);
        assert!(
            acks.message().await.expect("clean close").is_none(),
            "nothing was sent: no further acks"
        );
    }

    /// The inbound-idle abort, end to end: a builder that opens the
    /// stream and then vanishes without a FIN (here: simply never
    /// sends) is aborted (`reason="inbound_idle"`) once the buffer is
    /// empty and four heartbeats pass with no inbound traffic — the
    /// driver does NOT renew the ingest lease forever.
    ///
    /// `#[ignore]`: 60–75 s of real time (HEARTBEAT_INTERVAL is a
    /// const). Run manually: `cargo nextest run -p rio-store \
    /// -E 'test(idle_inbound)' --run-ignored all`. RED RECORDED
    /// (2026-06-04, fix neutralized via `if false &&`): the await
    /// below outlived a 100 s timeout — the driver renewed the lease
    /// forever. GREEN: Aborted("no inbound traffic") at ~75 s.
    // r[verify store.log.ingest-idle-abort+2]
    #[tokio::test]
    #[ignore = "60-75s real time (HEARTBEAT_INTERVAL is a const); run with --run-ignored all"]
    async fn idle_inbound_with_empty_buffer_aborts() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, mut acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        // Keep tx alive (no half-close) and send NOTHING: the
        // silently-vanished-builder shape from the server's view.
        let verdict =
            tokio::time::timeout(std::time::Duration::from_secs(100), acks.message()).await;
        drop(tx);
        let status = verdict
            .expect("driver must abort within the idle bound; pre-fix this timed out (RED)")
            .expect_err("an idle abort is an in-stream error, not a clean close");
        assert_eq!(status.code(), tonic::Code::Aborted);
        assert!(
            status.message().contains("no inbound traffic"),
            "got: {status:?}"
        );
    }

    /// The driver self-scan: every ack flows through the two named
    /// forms and every cut through the one named form — a raw awaited
    /// ack send or a raw cut call is unwriteable in this file's
    /// production half without failing here (the scan stops at the
    /// test-module boundary). PRE-FIX census (2026-06-04, the recorded
    /// red): 6 raw ack sends + 2 raw cuts = the 8 park-capable sites
    /// this scan was born red on.
    // r[verify store.log.driver-bounded]
    #[test]
    fn driver_self_scan_no_raw_ack_sends_or_cuts() {
        let full: String = include_str!("service.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // Production half only: tests below may drive the session
        // directly.
        let boundary = format!("#[cfg({})]modtests", "test");
        let src = &full[..full.find(&boundary).expect("test module exists")];
        // The scan's own string literals would self-match; build the
        // needles at runtime.
        let raw_send = format!("ack_tx.{}(", "send");
        let raw_cut = format!("session.{}(", "cut");
        assert_eq!(
            src.matches(&raw_send).count(),
            0,
            "raw ack send — route through send_ack_bounded/ack_try_send"
        );
        assert_eq!(
            src.matches(&raw_cut).count(),
            1,
            "raw session.cut outside cut_bounded — route through cut_bounded"
        );
        // And the named forms are actually in use.
        assert!(src.matches("send_ack_bounded(").count() >= 2);
        assert!(src.matches("ack_try_send(").count() >= 6);
        assert!(src.matches("self.cut_bounded()").count() >= 2);
    }

    /// The production half of this file, whitespace-stripped — the
    /// embedded census universe ((wwwww): compile-time `include_str!`,
    /// never a runtime tree walk). Slices at the test-module boundary
    /// so test code may drive sessions directly.
    fn production_half() -> String {
        let full: String = include_str!("service.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let boundary = format!("#[cfg({})]modtests", "test");
        full[..full.find(&boundary).expect("test module exists")].to_string()
    }

    /// The liveness-census refusal predicate (bug_148): an inline
    /// `sessions::heartbeat` call in the driver's production half puts
    /// the lease cadence back behind sibling cut/ack awaits — the
    /// exact shape whose bounds (60 s cut watchdog + 60 s ack vs the
    /// staleness window) made a healthy PUT stale the row. The
    /// cadence lives on the dedicated task (`sessions::spawn_heartbeat`,
    /// RPC-bounded by the sessions.rs compile assert) or nowhere.
    fn liveness_census_violations(src: &str) -> Vec<&'static str> {
        let mut v = Vec::new();
        let inline_beat = format!("sessions::{}(", "heartbeat");
        if src.matches(&inline_beat).count() != 0 {
            v.push(
                "inline sessions::heartbeat call: the lease cadence must live on \
                 the dedicated task (sessions::spawn_heartbeat), where no sibling \
                 await can starve it",
            );
        }
        let spawn_beat = format!("sessions::{}(", "spawn_heartbeat");
        if src.matches(&spawn_beat).count() != 1 {
            v.push(
                "exactly one sessions::spawn_heartbeat site: the driver spawns the \
                 dedicated heartbeat task once per stream (run), and nothing else \
                 mints one",
            );
        }
        v
    }

    /// [GEN-SET] The drive-loop liveness census (bug_148): the member
    /// list is the file's own production text (the embed above), the
    /// law is the refusal predicate, and the R22′ plant below proves
    /// the predicate can refuse. Sibling-arm await bounds at this
    /// census (the assert's input, quoted from the arms): inbound arm
    /// ≤ per-cut 2x cut-interval via cut_bounded + send_ack_bounded
    /// (level-triggered loop, one bounded cut per due-check); cut-tick
    /// arm ≤ 2x cut-interval (one do_cut); housekeeping arm ≤
    /// HEARTBEAT_RPC_BOUND (the bounded ceiling refresh); lease-lost
    /// arm and cancel arm await nothing. NONE of these bounds is
    /// load-bearing for lease liveness any more — that is the point:
    /// the cadence path's only await is the beat task's own RPC,
    /// compile-asserted inside the staleness margin (sessions.rs).
    // r[verify store.log.driver-bounded]
    #[test]
    fn drive_loop_liveness_census() {
        let src = production_half();
        let violations = liveness_census_violations(&src);
        assert!(
            violations.is_empty(),
            "drive-loop liveness census violations: {violations:?}"
        );

        // R22′ plant, at the outermost (raw-source) layer: a strawman
        // select arm that re-inlines the heartbeat — built at runtime
        // so the needle never self-matches — must be refused.
        let strawman = format!(
            "_=heartbeat_interval.tick()=>{{matchsessions::{}(&self.pool,exec,session).await{{}}}}",
            "heartbeat"
        );
        let planted = format!("{src}{strawman}");
        assert!(
            !liveness_census_violations(&planted).is_empty(),
            "the census must refuse a re-inlined heartbeat arm (the plant went green)"
        );
        // And a second plant for the spawn-uniqueness axis: a duplicate
        // spawn site (two beat tasks for one stream fight over the
        // latch) is refused too.
        let dup_spawn = format!("sessions::{}(pool,exec,session)", "spawn_heartbeat");
        let planted2 = format!("{src}{dup_spawn}");
        assert!(
            !liveness_census_violations(&planted2).is_empty(),
            "the census must refuse a second spawn_heartbeat site"
        );
    }

    /// The abort-disposition census predicate (merged_bug_144): every
    /// `LoopExit` abort site is one of exactly two commit-capability
    /// classes, and the per-class populations are pinned so a NEW
    /// abort site cannot land without consciously declaring its drain
    /// disposition here. Current census ([GEN-SET], from the
    /// production text): `AbortRefusesInput` = 7 occurrences — 5
    /// constructions (duplicate header; the accept() byte-cap map; the
    /// heartbeat-task-died arm; the inbound-idle arm; the do_cut chunk
    /// cap) + 2 consumers in `run` (the drain gate, the trailer
    /// match). `AbortCannotCommit` = 5 occurrences — 3 constructions
    /// (the cut-tick `should_abort` arm; the `cut_while_due` map; the
    /// `cut_once_if_nonempty` map — all via `abort_status`, the
    /// replica-failure vocabulary) + the same 2 consumers.
    fn abort_disposition_census_violations(src: &str) -> Vec<&'static str> {
        let mut v = Vec::new();
        let refuses = format!("LoopExit::{}(", "AbortRefusesInput");
        if src.matches(&refuses).count() != 7 {
            v.push(
                "AbortRefusesInput population moved: a refusal site was added or \
                 removed without re-declaring its drain-first disposition in this \
                 census",
            );
        }
        let cannot = format!("LoopExit::{}(", "AbortCannotCommit");
        if src.matches(&cannot).count() != 5 {
            v.push(
                "AbortCannotCommit population moved: a replica-failure abort site \
                 was added or removed without re-declaring its no-drain \
                 disposition in this census",
            );
        }
        v
    }

    /// The accept()-Err census predicate: the driver maps accept()'s
    /// `Err` TOTALLY onto `AbortRefusesInput` — sound only while the
    /// byte-cap rejection is accept()'s ONLY `Err` construction. The
    /// member list is ingest.rs's own embedded text ((wwwww)); the
    /// extent is `pub fn accept` up to the next `pub fn`.
    fn accept_err_census_violations(ingest_src: &str) -> Vec<&'static str> {
        let stripped: String = ingest_src.chars().filter(|c| !c.is_whitespace()).collect();
        let head = format!("pubfn{}(", "accept");
        let Some(start) = stripped.find(&head) else {
            return vec!["accept() not found in ingest.rs"];
        };
        let tail = &stripped[start + head.len()..];
        let body = match tail.find("pubfn") {
            Some(end) => &tail[..end],
            None => tail,
        };
        let ret_err = format!("return{}(", "Err");
        let cap_form = format!("return{}(super::gate::cap_rejection(", "Err");
        if body.matches(&ret_err).count() != 1 || body.matches(&cap_form).count() != 1 {
            return vec![
                "accept() grew a second Err construction (or the cap rejection \
                 moved): the driver's Err -> AbortRefusesInput map is no longer \
                 total — re-derive the disposition for the new Err class",
            ];
        }
        Vec::new()
    }

    /// [GEN-SET] The abort-disposition + accept-Err censuses
    /// (merged_bug_144), with R22′ plants at the raw-source layer.
    #[test]
    fn abort_disposition_census() {
        let src = production_half();
        let violations = abort_disposition_census_violations(&src);
        assert!(
            violations.is_empty(),
            "abort-disposition census violations: {violations:?}"
        );
        // Plant: an undeclared new refusal site moves the population.
        let strawman = format!("returnLoopExit::{}(status);", "AbortRefusesInput");
        assert!(
            !abort_disposition_census_violations(&format!("{src}{strawman}")).is_empty(),
            "the census must refuse an undeclared AbortRefusesInput site"
        );
        // Plant: an undeclared replica-failure abort site likewise.
        let strawman2 = format!("returnLoopExit::{}(status);", "AbortCannotCommit");
        assert!(
            !abort_disposition_census_violations(&format!("{src}{strawman2}")).is_empty(),
            "the census must refuse an undeclared AbortCannotCommit site"
        );

        let ingest = include_str!("ingest.rs");
        let violations = accept_err_census_violations(ingest);
        assert!(
            violations.is_empty(),
            "accept-Err census violations: {violations:?}"
        );
        // Plant: a second Err construction inside accept()'s extent —
        // injected right after accept()'s head so it lands inside the
        // scanned extent.
        let planted = ingest.replace(
            "pub fn accept(",
            &format!(
                "pub fn accept() {{ return {}(Status::internal(\"strawman\")); }} fn x(",
                "Err"
            ),
        );
        assert!(
            !accept_err_census_violations(&planted).is_empty(),
            "the census must refuse a second Err construction in accept()"
        );
    }

    /// The ack-park red and its bounded green, side by side on a FULL
    /// 1-slot queue (paused clock — the hour elapses instantly):
    /// the raw send (the merged_bug_135 shape at every pre-fix site)
    /// parks past an HOUR; send_ack_bounded returns undelivered at its
    /// bound.
    // r[verify store.log.driver-bounded]
    #[tokio::test(start_paused = true)]
    async fn ack_send_park_red_vs_bounded_green() {
        let (tx, _rx) = mpsc::channel::<Result<AppendLogAck, Status>>(1);
        tx.try_send(Ok(AppendLogAck {
            durable_through_line: 0,
            open_coverage_next_line: None,
        }))
        .expect("fill the queue");
        // RED (kept as the falsify twin of the bounded form): raw send
        // on a full queue with a live-but-not-reading receiver never
        // resolves.
        let raw = tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            tx.send(Ok(AppendLogAck {
                durable_through_line: 1,
                open_coverage_next_line: None,
            })),
        )
        .await;
        assert!(
            raw.is_err(),
            "raw send resolved on a full queue — the recorded red would be stale"
        );
        // GREEN: the bounded form gives up at its bound and reports
        // undelivered.
        let t0 = tokio::time::Instant::now();
        let delivered = send_ack_bounded(
            &tx,
            Ok(AppendLogAck {
                durable_through_line: 1,
                open_coverage_next_line: None,
            }),
            std::time::Duration::from_secs(60),
        )
        .await;
        assert!(!delivered);
        assert_eq!(t0.elapsed(), std::time::Duration::from_secs(60));
    }

    /// The 4x relationship the idle bound's doc promises.
    #[test]
    fn inbound_idle_bound_is_four_heartbeats() {
        assert_eq!(INBOUND_IDLE_BOUND, sessions::HEARTBEAT_INTERVAL * 4);
    }

    /// A chunk store whose `put` parks until the test releases it: the
    /// healthy-but-slow S3 PUT face (bug_148). `get`/`delete_batch`
    /// pass through. Permits are added by the test to release parked
    /// puts; each put consumes one.
    struct GatedChunkStore {
        inner: MemoryLogChunkStore,
        gate: tokio::sync::Semaphore,
        /// How many puts are currently parked (peak-asserted by tests).
        parked: std::sync::atomic::AtomicU32,
    }

    impl GatedChunkStore {
        fn new() -> Self {
            Self {
                inner: MemoryLogChunkStore::default(),
                gate: tokio::sync::Semaphore::new(0),
                parked: std::sync::atomic::AtomicU32::new(0),
            }
        }

        fn release_one(&self) {
            self.gate.add_permits(1);
        }

        fn parked_now(&self) -> u32 {
            self.parked.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl LogChunkStore for GatedChunkStore {
        async fn put(
            &self,
            key: &str,
            body: Vec<u8>,
        ) -> Result<crate::logs::chunks::PutOutcome, crate::logs::chunks::LogChunkError> {
            self.parked
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let permit = self.gate.acquire().await.expect("gate never closed");
            permit.forget();
            self.parked
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.put(key, body).await
        }

        async fn get(&self, key: &str) -> Result<Vec<u8>, crate::logs::chunks::LogChunkError> {
            self.inner.get(key).await
        }

        async fn delete_batch(
            &self,
            keys: &[String],
        ) -> Result<(), crate::logs::chunks::LogChunkError> {
            self.inner.delete_batch(keys).await
        }
    }

    /// `EXTRACT(EPOCH FROM heartbeat_at)` for the exec's session row —
    /// the monotone freshness probe the W10-BC assertions compare.
    async fn heartbeat_epoch(pool: &PgPool, exec: Uuid) -> f64 {
        sqlx::query_scalar::<_, f64>(
            "SELECT EXTRACT(EPOCH FROM heartbeat_at)::float8 \
             FROM log_ingest_sessions WHERE exec_id = $1",
        )
        .bind(exec)
        .fetch_one(pool)
        .await
        .expect("session row exists")
    }

    /// W10-BC (bug_148), the end-to-end face: a HEALTHY chunk PUT that
    /// outlasts one heartbeat interval must not make the session row
    /// stale — the row stays fresh THROUGHOUT the in-flight cut, a
    /// foreign acquire is refused (no steal admitted against a healthy
    /// stream), lookup_live keeps routing readers to the live owner,
    /// and the stream then completes cleanly once the PUT lands.
    ///
    /// `#[ignore]`: ~17 s of real time (HEARTBEAT_INTERVAL is a const;
    /// the PG staleness predicate runs on PG's own clock so the clock
    /// cannot be paused). Run manually:
    /// `cargo nextest run -p rio-store -E 'test(slow_cut_keeps)' \
    ///  --run-ignored all`. The gate-time enforcement of the same law
    /// is structural: the `HEARTBEAT_RPC_BOUND` compile assert
    /// (sessions.rs) plus `drive_loop_liveness_census` — the assert's
    /// red is the build.
    // r[verify store.log.session-keyed]
    #[tokio::test]
    #[ignore = "~17s real time (HEARTBEAT_INTERVAL is a const); run with --run-ignored all"]
    async fn healthy_slow_cut_keeps_session_row_fresh() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(GatedChunkStore::new());
        let exec = seed_assignment(&db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let svc = LogServiceImpl::new(
            db.pool.clone(),
            Arc::clone(&chunk_store) as Arc<dyn LogChunkStore>,
            "test-pod".to_string(),
        )
        .with_hmac_verifier(Arc::new(HmacVerifier::from_key(TEST_KEY.to_vec())))
        .with_ingest_config(IngestConfig {
            // A few hundred bytes trip the size-triggered cut.
            cut_threshold_bytes: 64,
            // One cut interval is the cut watchdog: keep it far above
            // the parked-PUT window so the slow-but-healthy PUT is
            // never abandoned (the abandonment path is W10-BE's).
            cut_interval: std::time::Duration::from_secs(300),
            ..IngestConfig::default()
        });
        let router = Server::builder().add_service(LogServiceServer::new(svc));
        let (addr, server) = spawn_grpc_server(router).await;
        let mut client = LogServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("connect");

        let (tx, mut acks) = open_append(&mut client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        let t0 = heartbeat_epoch(&db.pool, exec).await;

        // Cross the cut threshold: the driver enters its in-arm cut and
        // parks inside the gated PUT.
        tx.send(batch_msg(0, &["x".repeat(80).as_str()]))
            .await
            .unwrap();
        for _ in 0..200 {
            if chunk_store.parked_now() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(chunk_store.parked_now(), 1, "the cut's PUT is parked");

        // One full heartbeat interval (plus margin) elapses while the
        // PUT is still parked. The session row must stay fresh: the
        // heartbeat cadence is independent of cut latency.
        tokio::time::sleep(sessions::HEARTBEAT_INTERVAL + std::time::Duration::from_secs(2)).await;
        assert_eq!(chunk_store.parked_now(), 1, "the PUT is still parked");
        let mid = heartbeat_epoch(&db.pool, exec).await;
        assert!(
            mid > t0,
            "the session heartbeat must advance while a healthy cut is \
             in flight (got t0={t0}, mid={mid}: the row went stale behind \
             a parked PUT — the bug_148 red)"
        );
        // The reader-visible face: the row is live, so followers are
        // routed to the owner instead of silently downgrading.
        assert!(
            matches!(
                sessions::lookup_live(&db.pool, exec).await.unwrap(),
                sessions::LiveLookup::Live(_)
            ),
            "lookup_live must keep naming the healthy owner mid-cut"
        );
        // The steal face: a foreign acquire is REFUSED while the owner
        // is healthy (pre-fix the stale row admitted the steal and the
        // owner's next heartbeat aborted a healthy stream).
        match sessions::acquire(&db.pool, exec, Uuid::now_v7(), "other-pod")
            .await
            .unwrap()
        {
            Acquire::Busy { current_owner } => assert_eq!(current_owner, "test-pod"),
            other => panic!("a healthy mid-cut session was stolen: {other:?}"),
        }

        // Release the PUT: the cut completes, the ack arrives, and the
        // stream closes cleanly — the slow PUT was healthy all along.
        chunk_store.release_one();
        let ack = acks.message().await.expect("ack").expect("ack present");
        assert_eq!(ack.durable_through_line, 0);
        drop(tx);
        // The final drain has nothing left; clean close.
        assert!(acks.message().await.expect("clean close").is_none());
        server.abort();
    }

    /// W10-BD (merged_bug_144): a cap-crossing log's accepted under-cap
    /// tail is committed BEFORE the cap trailer — the byte-cap abort is
    /// a REFUSAL on a healthy replica, so it drains first. Pre-fix the
    /// cap abort skipped the ClientFinished-only drain: the trailer is
    /// permanent (the builder's classifier maps it to CapExhausted and
    /// never retransmits), so the tail of EVERY cap-crossing log was
    /// silently lost.
    // r[verify store.log.cap-reject-class]
    #[tokio::test]
    async fn cap_crossing_log_drains_accepted_tail_before_trailer() {
        let mut h = harness_with(256, |s| {
            s.with_ingest_config(IngestConfig {
                // No size-triggered cut: the under-cap tail stays
                // buffered until the drain.
                cut_threshold_bytes: 1024 * 1024,
                // The lifetime byte cap the second batch crosses.
                per_exec_byte_cap: 1024,
                ..IngestConfig::default()
            })
        })
        .await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, mut acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        // Two small lines: accepted, buffered, well under the cap.
        tx.send(batch_msg(0, &["under-cap line zero", "under-cap line one"]))
            .await
            .unwrap();
        // One oversized line: crossing the cap is stream-fatal.
        let big = "x".repeat(2000);
        tx.send(batch_msg(2, &[big.as_str()])).await.unwrap();

        // The accepted tail commits FIRST: an ack for lines 0..=1
        // precedes the trailer, and the chunk is durable.
        let first = acks
            .message()
            .await
            .expect("the drain ack precedes the cap trailer (pre-fix the trailer came first and the tail was lost — the merged_bug_144 red)")
            .expect("ack present");
        assert_eq!(
            first.durable_through_line, 1,
            "the under-cap tail (lines 0..=1) is durable before the trailer"
        );
        // THEN the cap trailer, in the permanent per-execution class.
        let trailer = acks
            .message()
            .await
            .expect_err("the stream ends with the cap rejection");
        assert_eq!(trailer.code(), tonic::Code::FailedPrecondition);
        assert!(
            trailer.message().contains("byte"),
            "the trailer names the byte cap, got: {trailer:?}"
        );
        assert_eq!(h.chunk_store.len(), 1, "one chunk holds the drained tail");
        drop(tx);

        // The read path serves the committed tail.
        let resp = h
            .client
            .tail_log(tail_req(exec, false))
            .await
            .expect("tail");
        let (lines, _) = collect_tail(resp.into_inner()).await.expect("collect");
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1],
            "the cap-crossing log's accepted tail is readable"
        );
    }

    /// W10-BE (merged_bug_144): a watchdog-abandoned cut leaves its
    /// drained run STAGED in `in_flight` with `buffer_bytes == 0` —
    /// the pre-fix idle gate read exactly that weaker face, so an
    /// inbound-silent stream was aborted with committable lines
    /// staged, destroying what the next cut's `restore_in_flight`
    /// retries (the 15 s housekeeping tick deterministically preceded
    /// the next cut tick). Post-fix the gate input is
    /// `no_pending_lines()` — derived from `oldest_pending_since`, the
    /// one field spanning both vecs — so the abort defers and the
    /// retry commits the staged run.
    // r[verify store.log.ingest-idle-abort+2]
    #[tokio::test]
    async fn idle_gate_emptiness_spans_staged_lines() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = seed_assignment(&db.pool, "builder-0").await;
        let store = GatedChunkStore::new();
        let gate_ok = gate::GateOk {
            drv_hash: rio_nix::store_path::drv_log_hash(DRV_PATH),
            exec_id: exec,
            final_line_count: None,
            seed: gate::LogSeed {
                merged_bytes: 0,
                merged_chunks: 0,
                raw_bytes: 0,
                raw_rows: 0,
            },
            covered: Default::default(),
        };
        let mut session = IngestSession::new(&gate_ok, Uuid::now_v7(), IngestConfig::default());
        session
            .accept(rio_proto::types::BuildLogBatch {
                derivation_path: DRV_PATH.to_string(),
                lines: vec![b"staged line".to_vec()],
                first_line_number: 0,
                executor_id: String::new(),
            })
            .unwrap();
        assert!(
            !session.no_pending_lines(),
            "an accepted line is pending before any cut"
        );
        // Watchdog-abandon shape: the cut future is dropped mid-PUT;
        // the drained run stays STAGED in in_flight.
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                session.cut(&store, &db.pool)
            )
            .await
            .is_err(),
            "the gated PUT parks the cut past the probe timeout"
        );
        // The gate input the production idle arm consults: a staged
        // committable line is PENDING — the idle abort must defer.
        assert!(
            !session.no_pending_lines(),
            "the idle-gate emptiness must span the staged in-flight run \
             (a committable line is staged; aborting would destroy what \
             restore_in_flight retries — the merged_bug_144 red)"
        );
        // The retry face: the next cut restores the staged run and
        // commits it; only then is the session pending-empty (and only
        // then may the idle abort fire, losing nothing).
        store.release_one();
        let committed = session
            .cut(&store, &db.pool)
            .await
            .expect("the retried cut commits the restored run")
            .and_then(|c| c.durable_ack);
        assert_eq!(committed, Some(0), "line 0 is durable through the retry");
        assert!(
            session.no_pending_lines(),
            "nothing pending once the staged run committed"
        );
    }

    /// store.log.completeness-gate, the per-append half: a builder
    /// holding a still-current assignment token for a
    /// terminal-but-incomplete execution is admitted (the late replay
    /// that completes the log MUST be admitted) but cannot grow the log
    /// past the build's recorded end — accepted lines numbered at or
    /// past `final_line_count` are dropped at ingest, never stored, and
    /// never served. Without the per-append comparison the open-time
    /// seal is the only enforcement and a live stream can append
    /// arbitrary content past the end the build actually produced.
    // r[verify store.log.completeness-gate]
    #[tokio::test]
    async fn append_past_final_line_count_is_dropped() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        // The build finished and reported 2 lines (0 and 1), but no
        // chunk ever landed: terminal, incomplete — the gate admits the
        // replay that is supposed to fill the gap.
        sqlx::query(
            "UPDATE drv_executions SET status = 'succeeded', finished_at = now(), \
             final_line_count = 2 WHERE exec_id = $1",
        )
        .bind(exec)
        .execute(&h.db.pool)
        .await
        .unwrap();
        let tok = token("builder-0", DRV);

        let (tx, mut acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("a terminal-but-incomplete execution must admit the late replay");
        // The legitimate replay: the two lines the build actually
        // produced, ending exactly at final_line_count.
        tx.send(batch_msg(0, &["line zero", "line one"]))
            .await
            .unwrap();
        // The injection: a batch starting at the recorded end. Nothing
        // in it can be part of the log.
        tx.send(batch_msg(2, &["injected two", "injected three"]))
            .await
            .unwrap();
        drop(tx);
        // Drain the acks until the stream closes (the final drain cuts
        // whatever was buffered).
        while let Ok(Some(_)) = acks.message().await {}

        let resp = h
            .client
            .tail_log(tail_req(exec, false))
            .await
            .expect("tail");
        let (lines, is_complete) = collect_tail(resp.into_inner()).await.expect("collect");
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1],
            "lines at or past the recorded final_line_count must be dropped at ingest, \
             not stored and served"
        );
        assert!(
            is_complete,
            "the replay covered [0, 2): the log must read as complete"
        );
    }

    // ------------------------------------------------------------------
    // 2-4. Stream-open rejections
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn append_without_header_first_is_rejected() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let _ = exec;
        let tok = token("builder-0", DRV);

        // The first message is a batch, not the header: the open itself
        // is rejected (the server reads and validates the header before
        // returning its response headers).
        let err = open_append(&mut h.client, &tok, vec![batch_msg(0, &["no header"])])
            .await
            .expect_err("a batch before the header must reject the stream");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn append_with_bad_token_is_rejected() {
        let mut h = harness().await;
        seed_assignment(&h.db.pool, "builder-0").await;

        // A token signed with the WRONG key.
        let bad = HmacSigner::from_key(b"not-the-key-the-store-verifies-w".to_vec()).sign(
            &AssignmentClaims {
                executor_id: "builder-0".to_string(),
                drv_hash: DRV.to_string(),
                expected_outputs: vec![],
                is_ca: false,
                expiry_unix: u64::MAX,
                tenant: None,
            },
        );
        let err = open_append(&mut h.client, &bad, vec![])
            .await
            .expect_err("a bad token must be rejected at open");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        // No token at all.
        let (_tx, rx) = mpsc::channel::<AppendLogRequest>(1);
        let req = Request::new(ReceiverStream::new(rx));
        let err = h
            .client
            .append_log(req)
            .await
            .expect_err("a missing token must be rejected at open");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    /// W11-G (bug_010). Proposition: **the PG row's owner is never
    /// cancelled by a later acquirer** — at the race's own
    /// interleaving. The same-pod steal arm admits two concurrent
    /// opens for one execution; the awaited PG acquire and the
    /// synchronous registry insert can invert order across the await,
    /// and pre-fix the later REGISTRY inserter unconditionally
    /// cancelled whatever it displaced — the true lease owner. The
    /// orchestrated inversion: A acquires the row first, B steals it
    /// (B owns the lease), B registers first, A's late insert lands
    /// on top. The witness: the cancel site verifies the registry's
    /// session against the row before cancelling; the later acquirer
    /// yields with ALREADY_EXISTS, the owner's entry is restored, and
    /// the owner's lease row stays live (no 30 s strand — the W11-H
    /// no-strand half lives here too).
    #[tokio::test]
    async fn later_registry_insert_never_cancels_the_lease_owner() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let registry: DashMap<Uuid, IngestEntry> = DashMap::new();
        let exec = Uuid::now_v7();
        let session_a = Uuid::now_v7();
        let session_b = Uuid::now_v7();

        // A's PG acquire lands first…
        assert!(matches!(
            sessions::acquire(&db.pool, exec, session_a, "this-pod")
                .await
                .unwrap(),
            sessions::Acquire::Acquired
        ));
        // …then B's (the same-pod arm steals): B owns the row.
        assert!(matches!(
            sessions::acquire(&db.pool, exec, session_b, "this-pod")
                .await
                .unwrap(),
            sessions::Acquire::Acquired
        ));

        // Two real ingest sessions provide the shared handles (the
        // production constructor; the GateOk literal is the existing
        // fixture lane).
        let gate_ok = |exec: Uuid| gate::GateOk {
            drv_hash: rio_nix::store_path::drv_log_hash(DRV_PATH),
            exec_id: exec,
            final_line_count: None,
            seed: gate::LogSeed {
                merged_bytes: 0,
                merged_chunks: 0,
                raw_bytes: 0,
                raw_rows: 0,
            },
            covered: Default::default(),
        };
        let ingest_a = IngestSession::new(&gate_ok(exec), session_a, IngestConfig::default());
        let ingest_b = IngestSession::new(&gate_ok(exec), session_b, IngestConfig::default());
        let shared_b = Arc::clone(ingest_b.shared());
        let cancel_a = rio_common::signal::Token::new();
        let cancel_b = rio_common::signal::Token::new();

        // B registers first (it owns the row): fine.
        register_ingest_owner(
            &registry,
            &db.pool,
            exec,
            session_b,
            IngestEntry {
                shared: Arc::clone(&shared_b),
                cancel: cancel_b.clone(),
            },
        )
        .await
        .expect("the row owner's registration succeeds");

        // A's registry insert resumes late — the inversion.
        let res = register_ingest_owner(
            &registry,
            &db.pool,
            exec,
            session_a,
            IngestEntry {
                shared: Arc::clone(ingest_a.shared()),
                cancel: cancel_a.clone(),
            },
        )
        .await;

        assert!(
            !cancel_b.is_cancelled(),
            "left (pre-fix): the late registry insert displaces and cancels \
             the true lease owner — B holds the PG row, B's healthy stream \
             dies, and the survivor A dies at its first heartbeat with \
             LeaseLost while B's Displaced exit strands the row for the 30 s \
             stale window / right: the cancel site checks the row's \
             session_id and the later acquirer yields"
        );
        let err = res.expect_err("the later acquirer yields, never cancels");
        assert_eq!(err.code(), tonic::Code::AlreadyExists, "{err:?}");

        // The owner's entry is restored in the registry…
        let entry = registry.get(&exec).expect("the owner's entry survives");
        assert!(
            Arc::ptr_eq(&entry.shared, &shared_b),
            "the registry slot holds the row owner's entry"
        );
        drop(entry);
        // …and the owner's lease row is live: no strand, no deposed
        // stream (W11-H's no-strand half).
        assert_eq!(
            sessions::lookup_live(&db.pool, exec).await.unwrap(),
            sessions::LiveLookup::Live(sessions::LiveSession {
                session_id: session_b,
                replica_pod: "this-pod".to_string(),
            }),
            "the owner keeps streaming: row live, never stranded"
        );
    }

    /// W11-H (bug_010, the release half): a Displaced exit routes
    /// through the session-id-predicated release — end to end on the
    /// designed same-pod reconnect path. The displaced driver's
    /// teardown must NOT delete the new owner's row (the predicate
    /// no-ops on a row it no longer owns), and the displaced stream
    /// surfaces the typed Aborted status.
    #[tokio::test]
    async fn displaced_exit_releases_without_touching_the_new_owner() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx1, mut acks1) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open 1");
        tx1.send(batch_msg(0, &["session one line"])).await.unwrap();

        // The same-pod reconnect: steals the lease, displaces stream 1.
        let (_tx2, _acks2) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("the same-pod reconnect must be admitted");

        // Stream 1 observes the displacement (typed Aborted).
        let status = loop {
            match acks1.message().await {
                Ok(Some(_)) => continue, // a drain ack from stream 1
                Ok(None) => panic!("stream 1 must end with the Displaced status"),
                Err(status) => break status,
            }
        };
        assert_eq!(status.code(), tonic::Code::Aborted, "{status:?}");
        assert!(
            status.message().contains("displaced"),
            "the displaced stream is told why: {}",
            status.message()
        );

        // The new owner's row survives stream 1's release-on-Displaced
        // (the session-id predicate no-ops on a row it no longer owns).
        let live = sessions::lookup_live(&h.db.pool, exec).await.unwrap();
        assert!(
            matches!(
                &live,
                sessions::LiveLookup::Live(s) if s.replica_pod == "test-pod"
            ),
            "the new owner's lease row stays live through the displaced \
             teardown, got {live:?}"
        );
    }

    #[tokio::test]
    async fn second_concurrent_append_for_same_exec_is_rejected() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        // The first stream's open does not return until its lease row
        // exists (acquire happens before the response headers), so the
        // second open is guaranteed to see it.
        let (tx1, mut acks1) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open 1");
        tx1.send(batch_msg(0, &["hold the lease"])).await.unwrap();

        // The second open lands on a DIFFERENT replica (a same-replica
        // re-open deliberately steals the lease — that is the
        // builder-reconnected-after-a-blip path). A fresh session on
        // another replica is the case the lease rejects.
        let (mut client2, _active2, _server2, _uri2) =
            spawn_replica(&h.db.pool, &h.chunk_store, "other-pod", 256, |s| s).await;
        let err = open_append(&mut client2, &tok, vec![header_msg(exec)])
            .await
            .expect_err("a second live session for the same execution must be rejected");
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
        assert!(
            err.message().contains("test-pod"),
            "the rejection names the owning replica for operators: {}",
            err.message()
        );

        // The first stream is unaffected.
        drop(tx1);
        assert!(
            acks1
                .message()
                .await
                .expect("first stream closes cleanly")
                .is_some()
        );
    }

    // ------------------------------------------------------------------
    // 5-6. The live tail
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn tail_follow_streams_live_batches() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, _acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        tx.send(batch_msg(0, &["before the subscriber"]))
            .await
            .unwrap();
        // The open returning means the session is registered; the batch
        // is processed asynchronously by the driver task.
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| !lock_shared(&s.shared).snapshot().is_empty())
                .unwrap_or(false)
        })
        .await;

        // Subscribe while the session is live.
        let mut tail = h
            .client
            .tail_log(tail_req(exec, true))
            .await
            .expect("tail")
            .into_inner();

        // The snapshot phase delivers line 0 (buffered before the
        // subscriber arrived, not yet cut).
        let first = tail.message().await.expect("snapshot").expect("present");
        assert_eq!(first.first_line_number, 0);
        assert_eq!(first.lines, vec![b"before the subscriber".to_vec()]);

        // The attach hello (merged_bug_067) sits between the snapshot
        // and the live phase: zero lines, non-final, exec-stamped.
        // Clients tolerate-and-skip zero-line chunks by protocol
        // contract; this test does the same to reach the live line.
        let hello = tail.message().await.expect("hello").expect("present");
        assert!(hello.lines.is_empty() && !hello.is_complete);

        // Lines accepted after the subscription arrive live.
        tx.send(batch_msg(1, &["after the subscriber"]))
            .await
            .unwrap();
        let live = tail.message().await.expect("live").expect("present");
        assert_eq!(live.first_line_number, 1);
        assert_eq!(live.lines, vec![b"after the subscriber".to_vec()]);

        // Closing the AppendLog stream ends the follow stream (after
        // the final-state message).
        drop(tx);
        let mut saw_end = false;
        while let Some(chunk) = tail.message().await.expect("tail drains cleanly") {
            assert!(chunk.lines.is_empty(), "no further lines were sent");
            saw_end = true;
        }
        assert!(saw_end, "the follow stream ends with a final-state message");
    }

    // r[verify store.log.attach-hello]
    /// merged_bug_067 (red-first): a follow attach whose since_line is
    /// beyond everything stored AND buffered (a follow-the-retry
    /// reconnect carrying the previous execution's cursor) must
    /// immediately receive a zero-line, non-final, exec-stamped hello
    /// chunk -- the client's exec-keyed visit is what detects the
    /// execution switch. Pre-fix the stream stayed silent until the
    /// next accepted batch, so a dead-quiet fresh execution was
    /// indistinguishable from a live stream on the previous one.
    #[tokio::test]
    async fn follow_attach_beyond_watermark_gets_exec_stamped_hello() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, _acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        tx.send(batch_msg(0, &["one line"])).await.unwrap();
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| !lock_shared(&s.shared).snapshot().is_empty())
                .unwrap_or(false)
        })
        .await;

        // Attach with a cursor far beyond this execution's watermark.
        let mut req = tail_req(exec, true);
        req.since_line = 10_000;
        let mut tail = h.client.tail_log(req).await.expect("tail").into_inner();

        let hello = tokio::time::timeout(std::time::Duration::from_secs(5), tail.message())
            .await
            .expect("the attach hello must arrive immediately, not on the next batch")
            .expect("stream open")
            .expect("present");
        assert_eq!(hello.lines, Vec::<Vec<u8>>::new(), "hello carries no lines");
        assert!(!hello.is_complete, "hello is not a final chunk");
        assert_eq!(hello.exec_id, exec.to_string(), "hello is exec-stamped");
        drop(tx);
    }

    // r[verify store.log.tail-reconnect]
    #[tokio::test]
    async fn tail_history_then_live_has_no_gap_or_duplicate() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, mut acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");

        // Force two committed chunks: each batch is > the 4096-byte cut
        // threshold on its own.
        let big_line = "x".repeat(3000);
        tx.send(batch_msg(0, &[&big_line, &big_line]))
            .await
            .unwrap();
        let ack = acks.message().await.expect("ack 1").expect("present");
        assert_eq!(ack.durable_through_line, 1);
        tx.send(batch_msg(2, &[&big_line, &big_line]))
            .await
            .unwrap();
        let ack = acks.message().await.expect("ack 2").expect("present");
        assert_eq!(ack.durable_through_line, 3);
        assert_eq!(h.chunk_store.len(), 2, "two committed chunks");

        // Leave two more lines in the buffer (un-cut) and open the
        // follow stream: it must see manifest(0..=3) ++ snapshot(4..=5)
        // with no gap and no duplicate.
        tx.send(batch_msg(4, &["four", "five"])).await.unwrap();
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| !lock_shared(&s.shared).snapshot().is_empty())
                .unwrap_or(false)
        })
        .await;

        let mut tail = h
            .client
            .tail_log(tail_req(exec, true))
            .await
            .expect("tail")
            .into_inner();
        let mut seen: Vec<u64> = Vec::new();
        while seen.last() != Some(&5) {
            let chunk = tail
                .message()
                .await
                .expect("tail stream")
                .expect("more lines expected until line 5 arrives");
            for (i, _) in chunk.lines.iter().enumerate() {
                let n = chunk.first_line_number + i as u64;
                if let Some(last) = seen.last() {
                    assert!(
                        n > *last,
                        "line {n} after {last}: duplicate or gap-fill out of order"
                    );
                }
                seen.push(n);
            }
        }
        assert_eq!(
            seen,
            vec![0, 1, 2, 3, 4, 5],
            "history ++ snapshot must cover every line exactly once, in order"
        );
        drop(tx);
    }

    // ------------------------------------------------------------------
    // 7. NotFound semantics
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn tail_not_found_distinguishes_pinned_from_unknown() {
        let mut h = harness().await;

        // A pinned exec that was never recorded.
        let resp = h
            .client
            .tail_log(TailLogRequest {
                derivation: DRV_PATH.to_string(),
                exec_id: Uuid::now_v7().to_string(),
                since_line: 0,
                follow: false,
            })
            .await
            .expect("the handler returns Ok(stream); the error is in-stream");
        let err = collect_tail(resp.into_inner())
            .await
            .expect_err("unknown pinned exec");
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(
            err.message().contains("execution"),
            "the pinned-exec message names the execution: {}",
            err.message()
        );

        // A derivation with no executions at all.
        let resp = h
            .client
            .tail_log(TailLogRequest {
                derivation: "/nix/store/9zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-never-built.drv"
                    .to_string(),
                exec_id: String::new(),
                since_line: 0,
                follow: false,
            })
            .await
            .expect("in-stream error");
        let err = collect_tail(resp.into_inner())
            .await
            .expect_err("unknown derivation");
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(
            err.message().contains("derivation"),
            "the no-executions message names the derivation: {}",
            err.message()
        );
    }

    // ------------------------------------------------------------------
    // 8. Admission
    // ------------------------------------------------------------------

    // r[verify store.log.cap-reject-class]
    /// bug_068's recorded red: the mid-stream chunk cap aborted with
    /// bare RESOURCE_EXHAUSTED and no reject-class metadata (pre-fix:
    /// `left: ResourceExhausted, right: FailedPrecondition` plus a
    /// missing x-rio-log-reject) — per-replica vocabulary for a
    /// per-execution fact, which the builder retried at 1 Hz forever.
    /// Now: FAILED_PRECONDITION + x-rio-log-reject: cap, the exact
    /// shape `abandon_reason_for_rejection` maps to CapExhausted.
    #[tokio::test]
    async fn mid_stream_chunk_cap_is_typed_permanent() {
        let mut h = harness_with(4, |s| {
            s.with_max_chunks_per_exec(1)
                .with_ingest_config(IngestConfig {
                    // Every non-empty batch is immediately cut_due.
                    cut_threshold_bytes: 1,
                    ..IngestConfig::default()
                })
        })
        .await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);
        let (tx, mut acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");

        // Two batches with cut_threshold=1: the first cut commits
        // (attempts -> 1), the next due cut hits the cap check. The
        // ack/abort interleaving on the response stream is not the
        // subject — the STATUS SHAPE is. Read until the error.
        tx.send(batch_msg(0, &["one"])).await.unwrap();
        tx.send(batch_msg(1, &["two"])).await.unwrap();
        let err = loop {
            match acks.message().await {
                Ok(Some(_)) => continue,
                Ok(None) => panic!("stream ended without the cap status"),
                Err(status) => break status,
            }
        };
        assert_eq!(
            err.code(),
            tonic::Code::FailedPrecondition,
            "per-execution cap, not replica capacity: {}",
            err.message()
        );
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .and_then(|v| v.to_str().ok()),
            Some("cap"),
            "the class the builder's CapExhausted mapping consumes"
        );
        assert!(err.message().contains("chunk cap"));
    }

    #[tokio::test]
    async fn stream_count_semaphore_rejects_at_capacity() {
        let mut h = harness_with(1, |s| s).await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (_tx1, _acks1) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open 1");

        // A second stream for a DIFFERENT derivation/execution: rejected
        // by the count cap, not the per-execution lease.
        let other_drv = "1aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-other-1.0.drv";
        let other_exec = {
            let derivation_id = sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO derivations (drv_hash, drv_path, system, status) \
                 VALUES ($1, $2, 'x86_64-linux', 'assigned') RETURNING derivation_id",
            )
            .bind(other_drv)
            .bind(format!("/nix/store/{other_drv}"))
            .fetch_one(&h.db.pool)
            .await
            .unwrap();
            let e = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO assignments \
                     (derivation_id, builder_id, generation, status, assigned_at, exec_id) \
                 VALUES ($1, 'builder-1', 1, 'acknowledged', now(), $2)",
            )
            .bind(derivation_id)
            .bind(e)
            .execute(&h.db.pool)
            .await
            .unwrap();
            // The lifecycle row, as production's atomic mint guarantees —
            // without it the authority gate (which runs before the count
            // cap) rejects and the test would assert the wrong arm.
            sqlx::query(
                "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at) \
                 VALUES ($1, $2, 'builder-1', now())",
            )
            .bind(e)
            .bind(rio_nix::store_path::drv_log_hash(other_drv))
            .execute(&h.db.pool)
            .await
            .unwrap();
            e
        };
        let tok2 = token("builder-1", other_drv);
        let err = open_append(
            &mut h.client,
            &tok2,
            vec![AppendLogRequest {
                msg: Some(append_log_request::Msg::Header(AppendLogHeader {
                    derivation_path: format!("/nix/store/{other_drv}"),
                    exec_id: other_exec.to_string(),
                })),
            }],
        )
        .await
        .expect_err("the second stream must be rejected at the count cap");
        assert_eq!(err.code(), tonic::Code::ResourceExhausted);
        assert!(
            err.message().contains("stream cap"),
            "the rejection names the cap that fired: {}",
            err.message()
        );
    }

    // ------------------------------------------------------------------
    // 9. The cross-replica TailLog proxy
    // ------------------------------------------------------------------
    //
    // Two replicas share one PG pool (the session lease + the chunk
    // manifest) and one chunk store (production's S3 is shared), but
    // have SEPARATE `ActiveIngests` registries and different pod names
    // — the production multi-replica topology. The discriminator in
    // every test is a line that exists ONLY in replica A's live ingest
    // buffer (accepted but never cut): the only way replica B can serve
    // it is by proxying to A.

    /// Spawn replica A (holding the live ingest session) and replica B
    /// (the one the reader hits), with B's peer resolver mapping
    /// A's pod name to `a_uri_override` if given, else to A's real
    /// listen address.
    async fn proxy_topology(
        db: &TestDb,
        chunk_store: &Arc<MemoryLogChunkStore>,
        a_uri_override: Option<&str>,
    ) -> (
        LogServiceClient<tonic::transport::Channel>,
        Arc<DashMap<Uuid, IngestEntry>>,
        LogServiceClient<tonic::transport::Channel>,
        Vec<tokio::task::JoinHandle<()>>,
    ) {
        let (client_a, active_a, server_a, uri_a) =
            spawn_replica(&db.pool, chunk_store, "store-a", 256, |s| s).await;
        let mapped = a_uri_override.map(str::to_owned).unwrap_or(uri_a);
        let (client_b, _active_b, server_b, _uri_b) =
            spawn_replica(&db.pool, chunk_store, "store-b", 256, move |s| {
                s.with_peer_resolver(PeerResolver::static_map([("store-a".to_string(), mapped)]))
            })
            .await;
        (client_a, active_a, client_b, vec![server_a, server_b])
    }

    #[tokio::test]
    async fn tail_proxies_to_owning_replica() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        let (mut client_a, active_a, mut client_b, _servers) =
            proxy_topology(&db, &chunk_store, None).await;
        let exec = seed_assignment(&db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        // The line lives only in A's live buffer (4096-byte cut
        // threshold; this is ~25 bytes, so nothing is cut).
        let (tx, _acks) = open_append(&mut client_a, &tok, vec![header_msg(exec)])
            .await
            .expect("open on A");
        tx.send(batch_msg(0, &["only in A's live buffer"]))
            .await
            .unwrap();
        wait_for(|| {
            active_a
                .get(&exec)
                .map(|s| !lock_shared(&s.shared).snapshot().is_empty())
                .unwrap_or(false)
        })
        .await;
        assert_eq!(
            chunk_store.len(),
            0,
            "nothing was cut: the proxy is the only path"
        );

        // A follow read against B: B does not hold the session, the
        // lease row says store-a does, so B relays A's stream.
        let mut tail = client_b
            .tail_log(tail_req(exec, true))
            .await
            .expect("tail via B")
            .into_inner();
        let first = tail.message().await.expect("proxied").expect("present");
        assert_eq!(first.first_line_number, 0);
        assert_eq!(first.lines, vec![b"only in A's live buffer".to_vec()]);
        // The owning replica's attach hello (merged_bug_067) is relayed
        // verbatim; skip it per the zero-line tolerate-and-skip contract.
        let hello = tail
            .message()
            .await
            .expect("relayed hello")
            .expect("present");
        assert!(hello.lines.is_empty() && !hello.is_complete);

        // Lines accepted by A after the subscription flow through live.
        tx.send(batch_msg(1, &["live through the proxy"]))
            .await
            .unwrap();
        let live = tail.message().await.expect("live").expect("present");
        assert_eq!(live.first_line_number, 1);
        assert_eq!(live.lines, vec![b"live through the proxy".to_vec()]);

        // Closing A's ingest stream ends the relayed follow stream too.
        drop(tx);
        while tail
            .message()
            .await
            .expect("relay drains cleanly")
            .is_some()
        {}
    }

    /// bug_039: a deployment with the proxy DISABLED (no peer
    /// template — the single-replica/dev default) serves the
    /// history-only view with zero proxy machinery: no `lookup_live`
    /// query, no dial, no rio_store_log_tail_proxy_failures_total
    /// increment, no per-read warn. The never-fires contract is
    /// structural — `let Some(resolver)` gates the entire proxy arm,
    /// so a disabled deployment cannot reach the failure counter by
    /// construction (the boot warn is the one statement of the
    /// disabled posture).
    // r[verify store.log.proxy-disabled-not-failure]
    #[tokio::test]
    async fn disabled_proxy_serves_history_without_proxy_machinery() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        // B has NO peer resolver (the construction default): the
        // proxy is disabled, not failing.
        let (mut client_a, _active_a, server_a, _uri_a) =
            spawn_replica(&db.pool, &chunk_store, "store-a", 256, |s| s).await;
        let (mut client_b, _active_b, server_b, _uri_b) =
            spawn_replica(&db.pool, &chunk_store, "store-b", 256, |s| {
                // The empty template maps to None AT CONSTRUCTION (the
                // arm uri_for used to carry): proxy disabled is a
                // constructor property, not a per-read empty-check.
                let s = s.with_peer_url_template(String::new());
                assert!(
                    s.peer_resolver.is_none(),
                    "an empty template disables the proxy at construction"
                );
                s
            })
            .await;
        let exec = seed_assignment(&db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let big = "x".repeat(3000);
        let (tx, mut acks) = open_append(&mut client_a, &tok, vec![header_msg(exec)])
            .await
            .expect("open on A");
        tx.send(batch_msg(0, &[&big, &big])).await.unwrap();
        let ack = acks.message().await.expect("ack").expect("present");
        assert_eq!(ack.durable_through_line, 1);

        // The session is live and owned by store-a; B's proxy is
        // disabled, so B serves the committed history directly.
        let resp = client_b
            .tail_log(tail_req(exec, false))
            .await
            .expect("tail via B with the proxy disabled");
        let (lines, is_complete) = collect_tail(resp.into_inner())
            .await
            .expect("history-only view");
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1],
            "disabled proxy = history-only view, not an error"
        );
        assert!(!is_complete);
        drop(tx);
        server_a.abort();
        server_b.abort();
    }

    #[tokio::test]
    async fn proxy_failure_falls_back_to_history() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        // B resolves store-a to a port nothing listens on: every proxy
        // attempt fails with connection-refused.
        let (mut client_a, _active_a, mut client_b, _servers) =
            proxy_topology(&db, &chunk_store, Some("http://127.0.0.1:1")).await;
        let exec = seed_assignment(&db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        // Commit one chunk (lines 0..=1) and leave the session alive so
        // the lease row still points at store-a.
        let big = "x".repeat(3000);
        let (tx, mut acks) = open_append(&mut client_a, &tok, vec![header_msg(exec)])
            .await
            .expect("open on A");
        tx.send(batch_msg(0, &[&big, &big])).await.unwrap();
        let ack = acks.message().await.expect("ack").expect("present");
        assert_eq!(ack.durable_through_line, 1);

        // The proxy to store-a fails; the reader still gets the
        // committed history rather than an error.
        let resp = client_b
            .tail_log(tail_req(exec, false))
            .await
            .expect("tail via B");
        let (lines, is_complete) = collect_tail(resp.into_inner())
            .await
            .expect("history-only fallback");
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1],
            "the committed chunk is served even though the live owner is unreachable"
        );
        assert!(!is_complete);
        drop(tx);
    }

    #[tokio::test]
    async fn proxied_request_is_not_reproxied() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let chunk_store = Arc::new(MemoryLogChunkStore::default());
        // B CAN reach A — the test proves it deliberately does not.
        let (mut client_a, active_a, mut client_b, _servers) =
            proxy_topology(&db, &chunk_store, None).await;
        let exec = seed_assignment(&db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, _acks) = open_append(&mut client_a, &tok, vec![header_msg(exec)])
            .await
            .expect("open on A");
        tx.send(batch_msg(0, &["live only"])).await.unwrap();
        wait_for(|| {
            active_a
                .get(&exec)
                .map(|s| !lock_shared(&s.shared).snapshot().is_empty())
                .unwrap_or(false)
        })
        .await;

        // A request already carrying the proxied marker is served
        // locally: history-only (nothing was cut), one hop maximum,
        // even though B's resolver could reach A.
        let mut req = Request::new(tail_req(exec, false));
        req.metadata_mut()
            .insert(PROXIED_METADATA_KEY, "1".parse().unwrap());
        let resp = client_b.tail_log(req).await.expect("tail via B");
        let (lines, _) = collect_tail(resp.into_inner())
            .await
            .expect("local history-only");
        assert!(
            lines.is_empty(),
            "a pre-proxied request must not take a second hop to the live buffer; got {lines:?}"
        );

        // Sanity check that the topology is otherwise proxyable: the
        // same request WITHOUT the marker reaches A's live buffer.
        let resp = client_b
            .tail_log(tail_req(exec, false))
            .await
            .expect("tail via B");
        let (lines, _) = collect_tail(resp.into_inner()).await.expect("proxied");
        assert_eq!(
            lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0],
            "the un-marked request proxies to A and sees the un-cut live line"
        );
        drop(tx);
    }

    // ------------------------------------------------------------------
    // Support
    // ------------------------------------------------------------------

    /// Poll a condition until it holds or 5 s elapse. The AppendLog
    /// driver runs in a detached task; tests that need to observe its
    /// side effects (the registry insert, the buffered lines) have to
    /// wait for it to process the messages they sent.
    async fn wait_for(mut cond: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !cond() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "condition not reached within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    // r[verify store.log.gap-provenance]
    /// A worker-admitted forward jump (the builder transmitted line 0,
    /// then line 5 — the ingest accepted both) is served ACROSS by the
    /// follow loop: the missing span was never accepted, so there is
    /// nothing to recover — no backfill, no drop, every transmitted
    /// line exactly once. (Pre-fix, every jump took the full
    /// clone+manifest recovery path — worker-drivable amplification.)
    #[tokio::test]
    async fn follow_serves_across_admitted_jump_without_recovery() {
        let mut h = harness().await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, _acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        tx.send(batch_msg(0, &["line-0"])).await.unwrap();
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| !lock_shared(&s.shared).snapshot().is_empty())
                .unwrap_or(false)
        })
        .await;

        let mut tail = h
            .client
            .tail_log(tail_req(exec, true))
            .await
            .expect("tail")
            .into_inner();
        let first = tail.message().await.expect("snapshot").expect("present");
        assert_eq!(first.first_line_number, 0);
        // Skip the attach hello (merged_bug_067; tolerate-and-skip is
        // the protocol contract for zero-line chunks).
        let hello = tail.message().await.expect("hello").expect("present");
        assert!(hello.lines.is_empty() && !hello.is_complete);

        // The worker jumps: line 5 next (lines 1..5 never transmitted).
        tx.send(batch_msg(5, &["line-5"])).await.unwrap();
        let jumped = tail.message().await.expect("live").expect("present");
        assert_eq!(
            jumped.first_line_number, 5,
            "the admitted hole is served across, not recovered around"
        );
        assert_eq!(jumped.lines, vec![b"line-5".to_vec()]);
        assert_eq!(
            h.active
                .get(&exec)
                .map(|s| lock_shared(&s.shared).tail_dropped),
            Some(0),
            "an admitted jump is not a fan-out drop"
        );
        drop(tx);
        let mut final_seen = false;
        while let Some(chunk) = tail.message().await.expect("stream ok") {
            if chunk.lines.is_empty() {
                final_seen = true;
            }
        }
        assert!(final_seen);
    }

    // r[verify store.log.tail-fanout-recovery]
    /// RED (merged_bug_187, parked-tail half): a burst that ENDS in
    /// fan-out drops must be served WITHOUT new output — the drop-side
    /// wake triggers the backfill. Pre-fix the reader parks until the
    /// next accepted batch (never, for a quiet build) or session end.
    #[tokio::test]
    async fn burst_end_drop_served_without_new_output() {
        let mut h = harness_with(256, |s| s.with_tail_subscriber_queue(1)).await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, _acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        tx.send(batch_msg(0, &["line-00000"])).await.unwrap();
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| !lock_shared(&s.shared).snapshot().is_empty())
                .unwrap_or(false)
        })
        .await;

        let mut tail = h
            .client
            .tail_log(tail_req(exec, true))
            .await
            .expect("tail")
            .into_inner();
        let first = tail.message().await.expect("snapshot").expect("present");
        assert_eq!(first.first_line_number, 0);

        // Stall the reader; burst past the 1-slot queue so the TAIL of
        // the burst drops. The builder then goes quiet (tx stays open —
        // no session end, no further batches).
        for i in 1..=30u64 {
            let payload = format!("line-{i:05}-{}", "x".repeat(4000));
            tx.send(batch_msg(i, &[&payload])).await.unwrap();
        }
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| lock_shared(&s.shared).tail_dropped > 0)
                .unwrap_or(false)
        })
        .await;

        // Resume reading. The drop wake must deliver the dropped tail
        // from manifest ∪ buffer with NO new output and NO session end.
        let mut got: std::collections::BTreeSet<u64> = [0].into();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while *got.iter().next_back().expect("nonempty") < 30 {
            let next = tokio::time::timeout_at(deadline, tail.message())
                .await
                .expect("the dropped burst tail must be served without new output (drop wake)")
                .expect("stream ok")
                .expect("stream open");
            for (i, _) in next.lines.iter().enumerate() {
                got.insert(next.first_line_number + i as u64);
            }
        }
        assert_eq!(
            got.iter().copied().collect::<Vec<u64>>(),
            (0..=30).collect::<Vec<u64>>(),
            "every accepted line served exactly once, tail included"
        );
        drop(tx);
    }

    // r[verify store.log.tail-fanout-recovery]
    /// A stalled follow reader whose fan-out queue overflows gets the
    /// dropped span back IN-STREAM: the serve loop observes the jump
    /// and back-fills from manifest ∪ live buffer before serving the
    /// triggering batch. Every accepted line arrives exactly once.
    #[tokio::test]
    async fn follow_recovers_fanout_drops_in_stream() {
        let mut h = harness_with(256, |s| s.with_tail_subscriber_queue(1)).await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, _acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        tx.send(batch_msg(0, &["line-00000"])).await.unwrap();
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| !lock_shared(&s.shared).snapshot().is_empty())
                .unwrap_or(false)
        })
        .await;

        let mut tail = h
            .client
            .tail_log(tail_req(exec, true))
            .await
            .expect("tail")
            .into_inner();
        // Read ONLY the snapshot message, then stall the reader.
        let first = tail.message().await.expect("snapshot").expect("present");
        assert_eq!(first.first_line_number, 0);

        // With the reader stalled, the response channel (4) + the
        // subscriber queue (1) fill after a handful of batches; the
        // rest are dropped by the lossy fan-out.
        // 4 KiB lines: large enough that HTTP/2 flow control clogs
        // with the reader stalled, so the response channel and then
        // the 1-slot fan-out queue fill and batches drop.
        for i in 1..=30u64 {
            let payload = format!("line-{i:05}-{}", "x".repeat(4000));
            tx.send(batch_msg(i, &[&payload])).await.unwrap();
        }
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| lock_shared(&s.shared).tail_dropped > 0)
                .unwrap_or(false)
        })
        .await;

        // Resume reading; close the append so the stream finishes.
        drop(tx);
        let mut got: Vec<u64> = vec![0];
        let mut final_seen = false;
        while let Some(chunk) = tail.message().await.expect("stream ok") {
            if chunk.lines.is_empty() {
                final_seen = true;
                continue;
            }
            for (i, _) in chunk.lines.iter().enumerate() {
                got.push(chunk.first_line_number + i as u64);
            }
        }
        assert!(final_seen, "the stream ends with a final message");
        got.sort_unstable();
        got.dedup();
        assert_eq!(
            got,
            (0..=30).collect::<Vec<u64>>(),
            "every accepted line is served exactly once across the drop \
             (the fan-out drop is recovered in-stream, not a permanent hole)"
        );
    }

    // r[verify store.log.tail-fanout-recovery]
    /// The final message after a fan-out drop advertises a cursor the
    /// reader can trust: everything below it was actually served.
    #[tokio::test]
    async fn follow_final_after_drop_advertises_contiguous_cursor() {
        let mut h = harness_with(256, |s| s.with_tail_subscriber_queue(1)).await;
        let exec = seed_assignment(&h.db.pool, "builder-0").await;
        let tok = token("builder-0", DRV);

        let (tx, _acks) = open_append(&mut h.client, &tok, vec![header_msg(exec)])
            .await
            .expect("open");
        tx.send(batch_msg(0, &["line-00000"])).await.unwrap();
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| !lock_shared(&s.shared).snapshot().is_empty())
                .unwrap_or(false)
        })
        .await;

        let mut tail = h
            .client
            .tail_log(tail_req(exec, true))
            .await
            .expect("tail")
            .into_inner();
        let _ = tail.message().await.expect("snapshot").expect("present");

        // 4 KiB lines: large enough that HTTP/2 flow control clogs
        // with the reader stalled, so the response channel and then
        // the 1-slot fan-out queue fill and batches drop.
        for i in 1..=30u64 {
            let payload = format!("line-{i:05}-{}", "x".repeat(4000));
            tx.send(batch_msg(i, &[&payload])).await.unwrap();
        }
        wait_for(|| {
            h.active
                .get(&exec)
                .map(|s| lock_shared(&s.shared).tail_dropped > 0)
                .unwrap_or(false)
        })
        .await;
        drop(tx);

        let mut served: Vec<u64> = vec![0];
        let mut final_cursor = None;
        while let Some(chunk) = tail.message().await.expect("stream ok") {
            if chunk.lines.is_empty() {
                final_cursor = Some(chunk.first_line_number);
            } else {
                for (i, _) in chunk.lines.iter().enumerate() {
                    served.push(chunk.first_line_number + i as u64);
                }
            }
        }
        let final_cursor = final_cursor.expect("a final message");
        served.sort_unstable();
        served.dedup();
        for n in 0..final_cursor {
            assert!(
                served.contains(&n),
                "the final cursor {final_cursor} advertises line {n} as served, but it never was"
            );
        }
    }

    /// The address-form rule lives in uri_for alone: IPv6 identities
    /// are bracketed, hostnames and IPv4 pass through bare. (The old
    /// "empty template names nobody" leg moved to construction:
    /// `with_peer_url_template("")` builds `peer_resolver: None`, so an
    /// empty template can no longer reach `uri_for` at all — pinned in
    /// `disabled_proxy_serves_history_without_proxy_machinery`.)
    #[test]
    fn uri_for_brackets_only_ipv6() {
        let r = PeerResolver::Template("http://{pod}:9002".to_string());
        assert_eq!(
            r.uri_for("10.2.3.4").as_deref(),
            Some("http://10.2.3.4:9002"),
            "IPv4 stays bare"
        );
        assert_eq!(
            r.uri_for("2001:db8::8be7").as_deref(),
            Some("http://[2001:db8::8be7]:9002"),
            "IPv6 is bracketed for URI authority position"
        );
        assert_eq!(
            r.uri_for("rio-store-abc123").as_deref(),
            Some("http://rio-store-abc123:9002"),
            "hostnames stay bare"
        );
    }
}

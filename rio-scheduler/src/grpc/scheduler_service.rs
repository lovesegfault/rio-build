//! `SchedulerService` gRPC implementation for [`SchedulerGrpc`].
//!
//! Client-facing RPCs: SubmitBuild, WatchBuild, QueryBuildStatus,
//! CancelBuild, ResolveTenant. Split from `mod.rs` (P0356) to cut
//! collision rate — these RPCs are touched by proto-adjacent plans
//! independently of the ExecutorService streaming path.

use tokio::sync::oneshot;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{info, instrument};
use uuid::Uuid;

use rio_common::grpc::StatusExt;
use rio_common::tenant::NormalizedName;
use rio_proto::SchedulerService;

use crate::actor::{ActorCommand, MergeDagRequest};
use crate::state::BuildOptions;

use super::{EventReplay, SchedulerGrpc, bridge_build_events, resolve_tenant_name};

#[tonic::async_trait]
impl SchedulerService for SchedulerGrpc {
    type SubmitBuildStream = ReceiverStream<Result<rio_proto::types::BuildEvent, Status>>;

    #[instrument(skip(self, request), fields(rpc = "SubmitBuild", build_id = tracing::field::Empty, tenant_id = tracing::field::Empty))]
    async fn submit_build(
        &self,
        request: Request<rio_proto::types::SubmitBuildRequest>,
    ) -> Result<Response<Self::SubmitBuildStream>, Status> {
        // Link to the gateway's trace BEFORE doing anything else. The
        // #[instrument] span is already entered by the time we're here;
        // link_parent adds an OTel span LINK to the client's traceparent
        // — NOT a parent. This span keeps its own trace_id; Jaeger shows
        // two traces connected by the link. Everything below (actor calls,
        // DB writes, store RPCs) inherits THIS span's trace_id.
        // The gateway reads THIS trace_id from x-rio-trace-id response
        // metadata (set below) and emits it in STDERR_NEXT — see
        // r[obs.trace.scheduler-id-in-metadata].
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;

        // Grab JWT Claims BEFORE into_inner() consumes the request.
        // Extensions are part of the Request wrapper, not the proto
        // body — into_inner() drops them. Clone is cheap: Claims is
        // 4 fields (Uuid + 2×i64 + short String).
        //
        // `None` is the common path: dev mode (no pubkey configured),
        // dual-mode fallback (gateway in SSH-comment mode, no header),
        // or VM tests (no interceptor in test harness). `Some` only
        // when the gateway is in JWT mode AND set the header AND the
        // interceptor verified it — i.e., we have a cryptographically
        // attested jti to check against the revocation table.
        let jwt_claims = request
            .extensions()
            .get::<rio_common::jwt::TenantClaims>()
            .cloned();

        // Also grab the RAW token string for re-inject on downstream
        // store calls (merge-time FindMissingPaths). Claims are the
        // DECODED payload (jti for revocation); the raw token is the
        // OPAQUE header we re-emit so the store's interceptor can do
        // its own verify → tenant_id extraction → upstream probe.
        //
        // Read from metadata (the interceptor leaves it in place after
        // verify), not from extensions. to_str() failure is unreachable
        // here — the interceptor already rejected non-ASCII tokens
        // upstream with UNAUTHENTICATED — but map to None defensively
        // rather than unwrap so a future interceptor change can't
        // panic the handler.
        let jwt_token = request
            .metadata()
            .get(rio_proto::TENANT_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let req = request.into_inner();

        // Check backpressure before sending to actor
        if self.actor.is_backpressured() {
            return Err(Status::resource_exhausted(
                "scheduler is overloaded, please retry later",
            ));
        }

        // Validate DAG nodes before passing to the actor. Proto types have
        // all-public fields with no validation; an empty drv_hash would
        // become a DAG primary key, empty drv_path breaks the reverse
        // index, and empty system never matches any worker (derivation
        // stuck in Ready forever). Bound node count to protect memory.
        rio_common::grpc::check_bound("nodes", req.nodes.len(), rio_common::limits::MAX_DAG_NODES)?;
        rio_common::grpc::check_bound("edges", req.edges.len(), rio_common::limits::MAX_DAG_EDGES)?;
        for node in &req.nodes {
            if node.drv_hash.is_empty() {
                return Err(Status::invalid_argument("node drv_hash must be non-empty"));
            }
            // Structural validation: drv_path must parse as a valid
            // /nix/store/{32-char-nixbase32}-{name}.drv path. Checking
            // only !is_empty() would let a garbage path like "/tmp/evil"
            // become a DAG key. StorePath::parse catches: missing
            // /nix/store/ prefix, bad hash length, bad nixbase32 chars,
            // path traversal, oversized names.
            match rio_nix::store_path::StorePath::parse(&node.drv_path) {
                Ok(sp) if sp.is_derivation() => {}
                Ok(_) => {
                    return Err(Status::invalid_argument(format!(
                        "node {} drv_path {:?} is not a .drv path",
                        node.drv_hash, node.drv_path
                    )));
                }
                Err(e) => {
                    return Err(Status::invalid_argument(format!(
                        "node {} drv_path {:?} is malformed: {e}",
                        node.drv_hash, node.drv_path
                    )));
                }
            }
            if node.system.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "node {} system must be non-empty",
                    node.drv_hash
                )));
            }
            // Gateway caps per-node at 64 KB; this is a defensive
            // upper bound (256 KB). Per-node — the 16 MB TOTAL budget
            // is gateway-enforced; here we just stop one malformed
            // node from being pathological.
            const MAX_DRV_CONTENT_BYTES: usize = 256 * 1024;
            rio_common::grpc::check_bound(
                "node.drv_content",
                node.drv_content.len(),
                MAX_DRV_CONTENT_BYTES,
            )?;
        }

        // UUID v7 (time-ordered, RFC 9562): the high 48 bits are Unix-ms
        // timestamp, so lexicographic sort == chronological sort. This makes
        // the S3 log key space `logs/{build_id}/...` naturally prefix-scannable
        // by time range, gives chronologically sorted `ls` output for
        // debugging, and improves PG index locality on builds.build_id
        // (recent builds cluster at the end of the index, not scattered
        // randomly like v4).
        //
        // Test code still uses v4 (~60 sites in actor/tests/) — test IDs
        // don't need ordering; changing them is pure churn.
        let build_id = Uuid::now_v7();
        let (reply_tx, reply_rx) = oneshot::channel();

        let options = BuildOptions {
            max_silent_time: req.max_silent_time,
            build_timeout: req.build_timeout,
            build_cores: req.build_cores,
        };

        // r[impl sched.tenant.resolve]
        // Tenant resolution: proto field carries tenant NAME (from gateway's
        // authorized_keys comment); resolve to UUID here via the tenants
        // table. `from_maybe_empty` → None (single-tenant mode, no PG
        // roundtrip). Unknown name → InvalidArgument. Keeps gateway PG-free
        // (stateless N-replica HA).
        let tenant_id = match NormalizedName::from_maybe_empty(&req.tenant_name) {
            None => None,
            Some(name) => {
                let pool = self.pool.as_ref().ok_or_else(|| {
                    Status::failed_precondition("tenant lookup requires database connection")
                })?;
                Some(resolve_tenant_name(pool, &name).await?)
            }
        };

        // r[impl gw.jwt.verify] — scheduler-side jti revocation check.
        //
        // Gateway stays PG-free (stateless N-replica HA); the scheduler
        // already has the pool open for tenant resolve above, so the
        // revocation lookup piggybacks here. Store does NOT duplicate
        // this — SubmitBuild is the ingress choke point for builds;
        // everything downstream trusts that the scheduler validated.
        //
        // `jti` is read from the interceptor-attached Claims extension,
        // NOT from a proto body field. The gateway never constructs
        // `SubmitBuildRequest.jwt_jti` — that field does not exist.
        // Zero wire redundancy: the jti travels once (inside the JWT),
        // is parsed once (by the interceptor), and is read once (here).
        // See r[gw.jwt.issue] for the mint-side story.
        //
        // Revoked → UNAUTHENTICATED, same code as a bad signature or
        // expired token. From the client's perspective "your token is
        // no longer valid" is one failure mode regardless of WHY.
        //
        // No-Claims (dev/dual-mode) → skip. Can't revoke what wasn't
        // presented. In JWT mode, Claims are ALWAYS present by the
        // time we get here: the interceptor either attached them or
        // returned UNAUTHENTICATED upstream, never a third state.
        if let Some(claims) = &jwt_claims {
            // Same pool-presence gate as tenant resolve. If pool is
            // None AND Claims are Some, something is misconfigured
            // (JWT mode requires PG for revocation); fail loud.
            let pool = self.pool.as_ref().ok_or_else(|| {
                Status::failed_precondition(
                    "jti revocation check requires database connection \
                     (JWT mode enabled but scheduler pool is None)",
                )
            })?;
            // EXISTS — short-circuits at first match, no row data
            // transferred. PK index on jti makes this O(log n). The
            // table is small (revocations are rare events) so this
            // is ~1 index page hit.
            let revoked: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jwt_revoked WHERE jti = $1)")
                    .bind(&claims.jti)
                    .fetch_one(pool)
                    .await
                    .status_internal("jti revocation lookup failed")?;
            if revoked {
                return Err(Status::unauthenticated("token revoked"));
            }
        }

        // Capture the current span's traceparent BEFORE sending to the
        // actor. Span context does not cross the mpsc channel boundary;
        // the actor task's `handle_merge_dag` #[instrument] span is a
        // fresh root. Carrying traceparent as plain data lets dispatch
        // embed the gateway-linked trace in WorkAssignment.
        let traceparent = rio_proto::interceptor::current_traceparent();
        let req = MergeDagRequest {
            build_id,
            tenant_id,
            priority_class: if req.priority_class.is_empty() {
                crate::state::PriorityClass::default()
            } else {
                req.priority_class
                    .parse()
                    .status_invalid("priority_class")?
            },
            nodes: req.nodes,
            edges: req.edges,
            options,
            keep_going: req.keep_going,
            traceparent,
            jti: jwt_claims.as_ref().map(|c| c.jti.clone()),
            jwt_token,
        };
        let cmd = ActorCommand::MergeDag {
            req,
            reply: reply_tx,
        };

        let bcast = self.send_and_await(cmd, reply_rx).await?;
        // Record build_id + tenant_id on the span (declared Empty in #[instrument]).
        // Per observability.md these are required structured-log fields.
        tracing::Span::current().record("build_id", build_id.to_string());
        if let Some(tid) = tenant_id {
            tracing::Span::current().record("tenant_id", tracing::field::display(tid));
        }
        info!(build_id = %build_id, "build submitted");
        // No replay: fresh build, MergeDag subscribed BEFORE seq=1
        // (Started) was emitted. last_seq=0, no gap. Pure broadcast.
        let mut resp = Response::new(bridge_build_events("submit-build-bridge", bcast, None));
        // Initial metadata: build_id. Reaches the client as soon as
        // this function returns Ok — BEFORE bridge_build_events' task
        // sends event 0. If we SIGTERM between here and event 0, the
        // gateway has build_id and can WatchBuild-reconnect. Closes
        // the "empty build event stream" gap (phase4a remediation 20).
        //
        // UUID.to_string() is always ASCII-hex-and-dashes — the
        // .parse::<MetadataValue<Ascii>>() cannot fail. expect() not
        // unwrap() so the message is greppable if this invariant ever breaks.
        resp.metadata_mut().insert(
            rio_proto::BUILD_ID_HEADER,
            build_id
                .to_string()
                .parse()
                .expect("UUID string is always valid ASCII metadata"),
        );
        // r[impl obs.trace.scheduler-id-in-metadata]
        // Set x-rio-trace-id alongside x-rio-build-id. The #[instrument]
        // span was created BEFORE link_parent() ran, so it has its OWN
        // trace_id (LINKED to the gateway's, not parented). Gateway emits
        // THIS id in STDERR_NEXT — operators grep the scheduler trace,
        // which is the one that spans scheduler→worker via the data-carry
        // at r[sched.trace.assignment-traceparent]. The gateway's own
        // trace_id only gets them to a trace with gateway spans; this one
        // gets them to the full chain. Empty-guard: no-OTel unit tests
        // get TraceId::INVALID → "" → no header.
        let trace_id = rio_proto::interceptor::current_trace_id_hex();
        if !trace_id.is_empty() {
            resp.metadata_mut().insert(
                rio_proto::TRACE_ID_HEADER,
                trace_id
                    .parse()
                    .expect("32 lowercase-hex chars is always valid ASCII metadata"),
            );
        }
        Ok(resp)
    }

    type WatchBuildStream = ReceiverStream<Result<rio_proto::types::BuildEvent, Status>>;

    #[instrument(skip(self, request), fields(rpc = "WatchBuild"))]
    async fn watch_build(
        &self,
        request: Request<rio_proto::types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::WatchBuild {
            build_id,
            since_sequence: req.since_sequence,
            reply: reply_tx,
        };

        let (bcast, last_seq) = self.send_and_await(cmd, reply_rx).await?;

        // Replay IF: pool available AND there's a gap to fill.
        // `since_sequence >= last_seq` → gateway already saw
        // everything, empty range, skip the PG round-trip.
        // `since_sequence == 0 && last_seq == 0` → build just
        // started, nothing emitted yet — common case, cheap exit.
        let replay = match &self.pool {
            Some(pool) if req.since_sequence < last_seq => Some(EventReplay {
                pool: pool.clone(),
                build_id,
                since: req.since_sequence,
                last_seq,
            }),
            _ => None,
        };

        Ok(Response::new(bridge_build_events(
            "watch-build-bridge",
            bcast,
            replay,
        )))
    }

    // WONTFIX(P0146): zero production callers (CLI uses ListBuilds,
    // dashboard uses WatchBuild). Kept because the underlying
    // ActorCommand::QueryBuildStatus is the test-suite's primary
    // build-state introspection mechanism (query_status/try_query_status
    // helpers, 10+ test callers). Removing just the ~20-LOC gRPC
    // wrapper would orphan the actor command; removing both would
    // require a replacement test introspection path. Not worth the
    // proto-rebuild churn for ~20 LOC.
    #[instrument(skip(self, request), fields(rpc = "QueryBuildStatus"))]
    async fn query_build_status(
        &self,
        request: Request<rio_proto::types::QueryBuildRequest>,
    ) -> Result<Response<rio_proto::types::BuildStatus>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::QueryBuildStatus {
            build_id,
            reply: reply_tx,
        };

        let status = self.send_and_await(cmd, reply_rx).await?;
        Ok(Response::new(status))
    }

    #[instrument(skip(self, request), fields(rpc = "CancelBuild"))]
    async fn cancel_build(
        &self,
        request: Request<rio_proto::types::CancelBuildRequest>,
    ) -> Result<Response<rio_proto::types::CancelBuildResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::CancelBuild {
            build_id,
            reason: req.reason,
            reply: reply_tx,
        };

        let cancelled = self.send_and_await(cmd, reply_rx).await?;

        Ok(Response::new(rio_proto::types::CancelBuildResponse {
            cancelled,
        }))
    }

    // r[impl sched.tenant.resolve]
    /// Name→UUID resolution exposed as an RPC for the gateway's JWT
    /// mint path. Same `resolve_tenant_name` helper as SubmitBuild's
    /// inline resolve — one source of truth for the lookup.
    ///
    /// NOT leader-gated: tenant lookup is a read-only PG query, no
    /// actor interaction, safe on any replica. The gateway calls this
    /// during SSH `auth_publickey` (before any build submission), so
    /// gating on leadership would make SSH auth latency depend on
    /// leader-election state. A standby replica with a pool can
    /// answer just as correctly as the leader.
    ///
    /// Empty name → InvalidArgument (not Ok("") — the caller shouldn't
    /// be calling at all for single-tenant mode; empty here means a
    /// bug in the gateway's gate). This differs from SubmitBuild's
    /// inline resolve (empty → Ok(None)), which is intentional:
    /// SubmitBuild's empty-name is a VALID state (single-tenant),
    /// ResolveTenant's empty-name is a CALLER ERROR.
    #[instrument(skip(self, request), fields(rpc = "ResolveTenant"))]
    async fn resolve_tenant(
        &self,
        request: Request<rio_proto::scheduler::ResolveTenantRequest>,
    ) -> Result<Response<rio_proto::scheduler::ResolveTenantResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // `new` rejects empty AND interior whitespace — the gateway
        // gates single-tenant mode before calling this RPC, so an
        // invalid name here is a caller bug, not a valid state.
        // Surface the NameError detail so the operator sees whether
        // it was empty or malformed (`"team a"`).
        let name = NormalizedName::new(&req.tenant_name).map_err(|e| {
            Status::invalid_argument(format!(
                "tenant_name invalid: {e} (gateway should gate single-tenant mode before calling)"
            ))
        })?;

        let pool = self.pool.as_ref().ok_or_else(|| {
            Status::failed_precondition("tenant resolution requires database connection")
        })?;

        let tenant_id = resolve_tenant_name(pool, &name).await?;

        Ok(Response::new(rio_proto::scheduler::ResolveTenantResponse {
            tenant_id: tenant_id.to_string(),
        }))
    }
}

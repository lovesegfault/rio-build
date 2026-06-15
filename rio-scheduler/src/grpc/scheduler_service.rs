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

use super::{SchedulerGrpc, bridge_build_events, resolve_tenant_name};

impl SchedulerGrpc {
    /// sh-036.1 off-actor `FindMissingPaths` over every request
    /// `expected_output_paths`. `None` when `store_client` is unset
    /// (test constructors) or there are no path-based outputs;
    /// `Some(Err)` on handler-side timeout/gRPC-error (the actor folds
    /// it into the breaker — NOT a SubmitBuild failure).
    ///
    /// Uses `with_timeout_status` (timeout → `deadline_exceeded`) and
    /// `inject_metadata` (tenant-token header) — the same leaves every
    /// other FMP issuance site uses, so a future request-shape change
    /// (e.g. a `probe_intent` field) lands in fewer places. The
    /// conditional timeout (`breaker_open` mirror → 30s, else 90s) is
    /// the same shape as `find_missing_with_breaker`.
    // r[impl sched.merge.probe-off-actor]
    async fn precompute_fmp_probe(
        &self,
        nodes: &[rio_proto::types::DerivationNode],
        jwt_token: Option<&str>,
    ) -> Option<Result<rio_proto::types::FindMissingPathsResponse, Status>> {
        let store_client = self.off_actor_probe.store_client.as_ref()?;
        let store_paths: Vec<String> = nodes
            .iter()
            .flat_map(|n| n.expected_output_paths.iter())
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();
        if store_paths.is_empty() {
            return None;
        }
        let mut fmp = Request::new(rio_proto::types::FindMissingPathsRequest { store_paths });
        rio_proto::interceptor::inject_current(fmp.metadata_mut());
        if let Some(t) = jwt_token {
            // I-202: same JWT propagation as the actor-side FMP sites.
            // inject_metadata's parse failure is `Status::internal`; a
            // JWT is base64url ASCII so this can't fail — degrade to
            // header-absent (substitution probe won't fire) rather
            // than failing SubmitBuild.
            let _ = rio_common::grpc::inject_metadata(
                fmp.metadata_mut(),
                &[(rio_proto::TENANT_TOKEN_HEADER, t)],
            );
        }
        let fmp_timeout = if self
            .off_actor_probe
            .breaker_open
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT
        } else {
            crate::actor::MERGE_FMP_TIMEOUT
        };
        Some(
            rio_common::grpc::with_timeout_status(
                "off-actor FindMissingPaths",
                fmp_timeout,
                store_client.clone().find_missing_paths(fmp),
            )
            .await
            .map(|r| r.into_inner()),
        )
    }
}

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

        // r[impl sched.tenant.authz+3]
        // Tenant authorization chokepoint. In JWT mode this rejects
        // token-less calls with UNAUTHENTICATED — the permissive
        // interceptor lets builders (which reach :9001 for
        // ExecutorService and never set `x-rio-tenant-token`) call
        // SchedulerService too; this closes that. `caller_tenant` is
        // the cryptographically-attested `claims.sub` and is the
        // authoritative tenant identity below. `jti` is the
        // revocation-checked token id, kept for the `builds.jwt_jti`
        // audit insert (`r[gw.jwt.issue]`).
        let caller = self.require_tenant(&request).await?;
        let (caller_tenant, jti) = (caller.tenant(), caller.jti().map(str::to_owned));

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

        // Paginated submission: non-final pages are staged
        // keyed by (attested tenant, submission_id) and acked with an
        // empty, immediately-closed event stream; the final page
        // assembles every staged page plus itself and falls through to
        // the SAME validation + digest bulk-verify as an unpaged
        // request. Unpaged requests (empty submission_id) pass through
        // untouched.
        // r[impl sched.submit.paginate]
        let req = match super::paginate::stage_or_assemble(&self.staged_pages, caller_tenant, req)?
        {
            super::paginate::PageOutcome::Staged { total_nodes } => {
                let (_tx, rx) = tokio::sync::mpsc::channel(1);
                let mut resp = Response::new(ReceiverStream::new(rx));
                resp.metadata_mut().insert(
                    "x-rio-staged-nodes",
                    total_nodes
                        .to_string()
                        .parse()
                        .expect("usize decimal string is always valid ASCII metadata"),
                );
                return Ok(resp);
            }
            super::paginate::PageOutcome::Ready(assembled) => *assembled,
        };

        // Validate DAG nodes before passing to the actor. Proto types have
        // all-public fields with no validation; an empty drv_hash would
        // become a DAG primary key, empty drv_path breaks the reverse
        // index, and empty system never matches any worker (derivation
        // stuck in Ready forever). Bound node count to protect memory.
        rio_common::grpc::check_bound("nodes", req.nodes.len(), rio_common::limits::MAX_DAG_NODES)?;
        rio_common::grpc::check_bound("edges", req.edges.len(), rio_common::limits::MAX_DAG_EDGES)?;
        let mut seen_hashes = std::collections::HashSet::with_capacity(req.nodes.len());
        for node in &req.nodes {
            if node.drv_hash.is_empty() {
                return Err(Status::invalid_argument("node drv_hash must be non-empty"));
            }
            // bug_155: a duplicate drv_hash reaches
            // `batch_upsert_derivations`' `ON CONFLICT DO UPDATE` →
            // PG 21000 cardinality_violation ("cannot affect row a
            // second time") → opaque Internal. Reject at the boundary
            // so the error names the offending hash.
            if !seen_hashes.insert(node.drv_hash.as_str()) {
                return Err(Status::invalid_argument(format!(
                    "duplicate drv_hash {:?} in nodes[]",
                    node.drv_hash
                )));
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
        // timestamp, so lexicographic sort == chronological sort. This
        // improves PG index locality on builds.build_id (recent builds
        // cluster at the end of the index, not scattered randomly like
        // v4). Build logs are keyed by exec_id, not build_id (see
        // rio-store's `drv_log_chunks` manifest).
        //
        // Test code still uses v4 (~60 sites in actor/tests/) — test IDs
        // don't need ordering; changing them is pure churn.
        let build_id = Uuid::now_v7();
        let (reply_tx, reply_rx) = oneshot::channel();

        // r[impl sched.timeout.per-build+2]
        // The TENANT SEAM mint (merged_bug_034): wire-supplied timeout
        // seconds are bounds-typed here — WireSecs saturates anything
        // above the shared one-year absurdity ceiling (with a debug
        // log; Q-S8-B signed posture) and preserves 0-means-unset. A
        // u64::MAX submission becomes effectively-unbounded-but-
        // arithmetic-safe instead of a builder-side Instant+Duration
        // panic misclassified as infrastructure failure.
        let options = BuildOptions {
            max_silent_time: rio_common::clamped::WireSecs::from_wire(req.max_silent_time),
            build_timeout: rio_common::clamped::WireSecs::from_wire(req.build_timeout),
            build_cores: req.build_cores,
        };

        // r[impl sched.tenant.resolve+2]
        // Tenant resolution. Primary path: `claims.sub` from
        // `require_tenant` above — the attested identity wins, the body
        // `tenant_name` is ignored (a builder cannot attribute its
        // submission to another tenant by setting the body field).
        // Dev-mode fallback (`caller_tenant` is None): proto field
        // carries tenant NAME (from gateway's authorized_keys comment);
        // resolve to UUID via the tenants table. `from_maybe_empty` →
        // None (single-tenant mode, no PG roundtrip). Unknown name →
        // InvalidArgument. Keeps gateway PG-free (stateless N-replica
        // HA).
        let tenant_id = match (
            caller_tenant,
            NormalizedName::from_maybe_empty(&req.tenant_name),
        ) {
            (Some(sub), _) => Some(sub),
            (None, None) => None,
            (None, Some(name)) => {
                let db = self.db.as_ref().ok_or_else(|| {
                    Status::failed_precondition("tenant lookup requires database connection")
                })?;
                Some(resolve_tenant_name(db, &name).await?)
            }
        };

        // r[impl sched.merge.probe-off-actor]
        // sh-036.1: hoist phase-4's tenant-scoped FindMissingPaths
        // (4.99s of the 11.63s MergeDag actor turn) to run HERE — off
        // the single-threaded actor — so the actor turn applies a
        // pre-computed response without awaiting store I/O. The probe
        // is read-only over (request payload + store state); the only
        // actor state it touched was `cache_breaker`, whose fold stays
        // actor-side via `record_breaker_from_precomputed`.
        //
        // This OVER-PROBES relative to the actor's `probe_set`
        // (newly_inserted ∪ existing_reprobe — neither computable
        // pre-enqueue): every request `expected_output_paths`, not
        // just probe_set's. Correctness-safe — the actor partition
        // iterates `probe_set` only, so over-probed entries are never
        // applied. Cost: over-probed Completed/Skipped outputs are
        // PG-present → cheap; over-probed Assigned/Running/Cancelled
        // outputs are NOT yet in `path_infos` → reach `check_available`
        // → extra upstream HEADs, bounded by in-flight executor
        // capacity ≈ O(100s). Benign.
        //
        // Handler timeout/gRPC-error collapses into `Some(Err(Status))`
        // (NOT a SubmitBuild failure) — the actor folds it into the
        // breaker. `None` only when `store_client.is_none()` or no
        // path-based outputs (the actor's in-actor probe fallback runs;
        // identical to today's behaviour). Runs AFTER the
        // `is_backpressured()` gate so a refused request doesn't burn
        // 5s of upstream HEADs.
        let precomputed_probe = self
            .precompute_fmp_probe(&req.nodes, jwt_token.as_deref())
            .await;

        // ADR-024: digest-bearing submissions derive edges from
        // input_drv_digests; the request's `edges` list is ignored for
        // them (legacy submissions keep it unchanged). Every referenced
        // digest is bulk-verified against the store's drv_blobs BEFORE
        // the actor sees the submission: a digest that resolves neither
        // in-submission nor in the store is a FAILED_PRECONDITION
        // reject naming ALL missing digests (the client re-Has-es,
        // re-uploads, resubmits). Store verification runs after tenant
        // resolve because presence is tenant-scoped, mirroring HasDrvs.
        let edges = match super::digest_submit::classify_and_derive_edges(&req.nodes)? {
            None => req.edges,
            Some(d) => {
                // Deny-on-failure: digest submissions cannot be
                // accepted without verifying their blobs exist.
                let db = self.db.as_ref().ok_or_else(|| {
                    Status::failed_precondition(
                        "digest-bearing submission requires a database connection",
                    )
                })?;
                let mut referenced: Vec<Vec<u8>> =
                    d.own.iter().map(|(digest, _)| digest.clone()).collect();
                referenced.extend(d.external.iter().map(|(_, digest)| digest.clone()));
                referenced.sort();
                referenced.dedup();
                // r[impl sched.submit.digest-verify]
                let resolved = db
                    .resolve_drv_digests(&referenced, tenant_id)
                    .await
                    .map_err(|e| {
                        tracing::error!(error = %e, "drv digest bulk-verify query failed");
                        Status::unavailable(
                            "drv digest verification unavailable; retry the submission",
                        )
                    })?;
                let external_edges = super::digest_submit::verify_resolved(&d, &resolved)?;
                let mut edges = d.edges;
                edges.extend(external_edges);
                // classify_and_derive_edges bounded the IN-submission
                // edges; re-check now that store-resolved external
                // references were appended.
                rio_common::grpc::check_bound(
                    "edges",
                    edges.len(),
                    rio_common::limits::MAX_DAG_EDGES,
                )?;
                edges
            }
        };

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
            edges,
            options,
            keep_going: req.keep_going,
            traceparent,
            jti,
            jwt_token,
            precomputed_probe,
        };
        let cmd = ActorCommand::MergeDag {
            req,
            reply: reply_tx,
        };

        let bcast = self.send_and_await(cmd, reply_rx).await?;
        // Record build_id + tenant_id on the span (declared Empty in #[instrument]).
        // Per observability.typ these are required structured-log fields.
        tracing::Span::current().record("build_id", build_id.to_string());
        if let Some(tid) = tenant_id {
            tracing::Span::current().record("tenant_id", tracing::field::display(tid));
        }
        info!(build_id = %build_id, "build submitted");
        // No snapshot: fresh build, MergeDag subscribed BEFORE the first
        // event (Started) was emitted — there is no missed state to
        // summarize. Pure broadcast.
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

    // r[impl sched.watch.snapshot-first]
    #[instrument(skip(self, request), fields(rpc = "WatchBuild"))]
    async fn watch_build(
        &self,
        request: Request<rio_proto::types::WatchBuildRequest>,
    ) -> Result<Response<Self::WatchBuildStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        self.check_actor_alive()?;
        // r[impl sched.tenant.authz+3]
        let caller = self.require_tenant(&request).await?;
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::WatchBuild {
            build_id,
            caller_tenant: caller.tenant(),
            reply: reply_tx,
        };

        // Snapshot-first attach: the actor computed the snapshot
        // atomically with the broadcast subscription, so sending it as
        // the stream's first message followed by the live broadcast is
        // gap-free by construction — no sequence numbers, no PG replay,
        // no dedup.
        let (bcast, snapshot) = match self.send_and_await(cmd, reply_rx).await {
            Ok(x) => x,
            // r[impl sched.watch.terminal-from-durable-row+2]
            // The actor no longer holds the build (terminal cleanup ran,
            // or this is a fresh post-failover leader that only recovers
            // non-terminal builds). The builds row carries the settled
            // verdict (migration 087) — answer with ONE synthesized
            // terminal snapshot instead of NotFound, which the gateway
            // would convert into a fabricated failure after burning
            // ~111s of reconnect attempts (merged_bug_323).
            Err(status) if status.code() == tonic::Code::NotFound => {
                let Some(db) = &self.db else {
                    return Err(status);
                };
                let Some(row) = db
                    .get_build_terminal_row(build_id, &caller)
                    .await
                    .map_err(|e| Status::internal(format!("terminal-row lookup failed: {e}")))?
                else {
                    return Err(status);
                };
                let event = synthesize_terminal_snapshot(build_id, row);
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                let _ = tx.send(Ok(event)).await;
                return Ok(Response::new(ReceiverStream::new(rx)));
            }
            Err(e) => return Err(e),
        };

        Ok(Response::new(bridge_build_events(
            "watch-build-bridge",
            bcast,
            Some(snapshot),
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
        // r[impl sched.tenant.authz+3]
        let caller_tenant = self.require_tenant(&request).await?.tenant();
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::QueryBuildStatus {
            build_id,
            caller_tenant,
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
        // r[impl sched.tenant.authz+3]
        let caller_tenant = self.require_tenant(&request).await?.tenant();
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::CancelBuild {
            build_id,
            caller_tenant,
            reason: req.reason,
            reply: reply_tx,
        };

        let cancelled = self.send_and_await(cmd, reply_rx).await?;

        Ok(Response::new(rio_proto::types::CancelBuildResponse {
            cancelled,
        }))
    }

    type GetDerivationLogStream = super::derivation_log::LogStream;

    /// Stream a stored derivation-execution log for a build the caller
    /// owns (`rio log`, and the client's failure replay after a
    /// fail-fast). Tenant scoping, execution resolution and the
    /// server-side tail cursor live in `grpc/derivation_log.rs`; the
    /// byte-serving body is shared with `AdminService.GetDerivationLogs`.
    #[instrument(skip(self, request), fields(rpc = "GetDerivationLog"))]
    async fn get_derivation_log(
        &self,
        request: Request<rio_proto::scheduler::GetDerivationLogRequest>,
    ) -> Result<Response<Self::GetDerivationLogStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.ensure_leader()?;
        // r[impl sched.tenant.authz+2]
        let caller = self.require_tenant(&request).await?;
        let jwt_token = request
            .metadata()
            .get(rio_proto::TENANT_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        super::derivation_log::get_derivation_log(self, caller.tenant(), jwt_token, request).await
    }

    // r[impl sched.tenant.resolve+2]
    /// Name→UUID resolution exposed as an RPC for the gateway's JWT
    /// mint path. Same `resolve_tenant_name` helper as SubmitBuild's
    /// inline resolve — one source of truth for the lookup.
    ///
    /// NOT `require_tenant`-gated: the gateway calls this during SSH
    /// `auth_publickey` BEFORE a JWT exists (it's resolving the tenant
    /// to mint one). Read-only, idempotent, safe on any replica.
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

        let db = self.db.as_ref().ok_or_else(|| {
            Status::failed_precondition("tenant resolution requires database connection")
        })?;

        let tenant_id = resolve_tenant_name(db, &name).await?;

        Ok(Response::new(rio_proto::scheduler::ResolveTenantResponse {
            tenant_id: tenant_id.to_string(),
        }))
    }
}

/// Synthesize the one-message terminal snapshot for a build whose actor
/// state is gone but whose `builds` row records the settled verdict
/// (migration 087). Pre-087 terminal rows have NULL payload columns and
/// degrade to the old empty-payload snapshot — the state itself is
/// always correct.
// r[impl sched.watch.terminal-from-durable-row+2]
fn synthesize_terminal_snapshot(
    build_id: Uuid,
    row: crate::db::BuildTerminalRow,
) -> rio_proto::types::BuildEvent {
    use crate::state::{BuildState, BuildStateExt};
    use rio_proto::types;

    let state = BuildState::parse_db(&row.status).unwrap_or(BuildState::Unspecified);
    let failure_status = row
        .failure_status
        .as_deref()
        .and_then(types::BuildResultStatus::from_str_name)
        .map_or(0, |s| s as i32);
    let snapshot = types::BuildSnapshot {
        state: state.into(),
        total_derivations: row.total_drvs.unwrap_or(0).max(0) as u32,
        completed_derivations: row.completed_drvs.unwrap_or(0).max(0) as u32,
        cached_derivations: row.cached_drvs.unwrap_or(0).max(0) as u32,
        running_derivations: 0,
        failed_derivations: row.failed_drvs.unwrap_or(0).max(0) as u32,
        queued_derivations: 0,
        critical_path_remaining_secs: Some(0),
        assigned_executors: Vec::new(),
        running: Vec::new(),
        output_paths: row.output_paths.unwrap_or_default(),
        error_message: row.error_summary.unwrap_or_default(),
        failed_derivation: row.failed_derivation.unwrap_or_default(),
        failure_status,
        cancel_reason: row.cancel_reason.unwrap_or_default(),
    };
    types::BuildEvent {
        build_id: build_id.to_string(),
        timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
        event: Some(types::build_event::Event::Snapshot(snapshot)),
    }
}

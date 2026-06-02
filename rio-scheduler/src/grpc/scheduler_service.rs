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

        // r[impl sched.tenant.authz+2]
        // Tenant authorization chokepoint. In JWT mode this rejects
        // token-less calls with UNAUTHENTICATED — the permissive
        // interceptor lets builders (which reach :9001 for
        // ExecutorService and never set `x-rio-tenant-token`) call
        // SchedulerService too; this closes that. `caller_tenant` is
        // the cryptographically-attested `claims.sub` and is the
        // authoritative tenant identity below. `jti` is the
        // revocation-checked token id, kept for the `builds.jwt_jti`
        // audit insert (`r[gw.jwt.issue]`).
        let (caller_tenant, jti) = match self.require_tenant(&request).await? {
            Some((sub, jti)) => (Some(sub), Some(jti)),
            None => (None, None),
        };

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

        let mut req = request.into_inner();

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
        let mut seen_hashes = std::collections::HashSet::with_capacity(req.nodes.len());
        for node in &req.nodes {
            if node.drv_hash.is_empty() {
                return Err(Status::invalid_argument("node drv_hash must be non-empty"));
            }
            // bug_155: a duplicate drv_hash reaches
            // `batch_upsert_derivations`' UNNEST → PG 21000
            // cardinality_violation → opaque Internal. Reject at the
            // boundary so the error names the offending hash.
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
            // r[impl sched.merge.ingress-identity-binding]
            // The DAG (and the derivations table, the HMAC assignment
            // claims, the conflict/identity gates) is keyed by drv_hash,
            // while edges, dispatch, and recovery resolve the declared
            // drv_path. The gateway always submits drv_hash == drv_path;
            // an untrusted direct submitter must not be able to register
            // a node under someone else's DAG key (a predictable .drv
            // path) while pointing drv_path at an unrelated decoy that
            // workers would then fetch — or alias two nodes with one
            // path, corrupting the path_to_hash reverse index. Bind the
            // two at the door.
            if node.drv_hash != node.drv_path {
                return Err(Status::invalid_argument(format!(
                    "node {} drv_hash must equal the declared drv_path {:?} (the DAG key is the .drv store path)",
                    node.drv_hash, node.drv_path
                )));
            }
            // ca_modular_hash is identity evidence (merge-gate identity
            // matching, realisation keying, persisted for recovery). The
            // domain boundary coerces any non-32-byte value to None;
            // keep that coercion as defense in depth, but reject
            // malformed lengths here so bad evidence is named to the
            // submitter instead of silently dropped.
            if !node.ca_modular_hash.is_empty() && node.ca_modular_hash.len() != 32 {
                return Err(Status::invalid_argument(format!(
                    "node {} ca_modular_hash must be empty or exactly 32 bytes, got {} bytes",
                    node.drv_hash,
                    node.ca_modular_hash.len()
                )));
            }
            // The gateway derives is_content_addressed as
            // has_ca_floating_outputs() || is_fixed_output(); a node
            // claiming is_fixed_output without is_content_addressed is
            // never legitimate and would skip the CA gates downstream.
            if node.is_fixed_output && !node.is_content_addressed {
                return Err(Status::invalid_argument(format!(
                    "node {} sets is_fixed_output without is_content_addressed",
                    node.drv_hash
                )));
            }
            // r[impl sched.merge.ingress-output-path-shape]
            // expected_output_paths feed FindMissingPaths cache-hit
            // probing, the assignment-claims output allowlist that
            // authorizes worker uploads, and the merge gate's FOD
            // path-agreement evidence. Each entry must be empty
            // (floating-CA / deferred — the path is computed at
            // resolution) or parse as a non-.drv store path. Backstop
            // for direct submitters: the gateway re-derives and
            // validates every declared path before submitting
            // (gw.reject.output-path-mismatch+2), so nothing it
            // produces is rejected here.
            for (i, path) in node.expected_output_paths.iter().enumerate() {
                if path.is_empty() {
                    continue;
                }
                match rio_nix::store_path::StorePath::parse(path) {
                    Ok(sp) if !sp.is_derivation() => {}
                    Ok(_) => {
                        return Err(Status::invalid_argument(format!(
                            "node {} expected_output_paths[{i}] {path:?} is a .drv path — \
                             outputs cannot be derivations",
                            node.drv_hash
                        )));
                    }
                    Err(e) => {
                        return Err(Status::invalid_argument(format!(
                            "node {} expected_output_paths[{i}] {path:?} is not a valid store \
                             path: {e}",
                            node.drv_hash
                        )));
                    }
                }
            }
            if node.system.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "node {} system must be non-empty",
                    node.drv_hash
                )));
            }
            // Per-node drv_content bound, shared with the gateway
            // (rio_common::limits::MAX_DRV_CONTENT_BYTES = 1 MiB). Two
            // gateway producers fill this field: the inline-.drv
            // optimization (≤64 KiB per node, 16 MiB total budget,
            // gateway-enforced) and the content-bound hook fallback
            // (single node up to the shared cap). The bound here is
            // defense in depth against direct/hostile submitters that
            // bypass the gateway — it equals the gateway's fallback
            // cap, so nothing the gateway accepts is rejected here.
            rio_common::grpc::check_bound(
                "node.drv_content",
                node.drv_content.len(),
                rio_common::limits::MAX_DRV_CONTENT_BYTES,
            )?;
        }
        // r[impl sched.merge.ingress-edge-endpoints]
        // Dependency edges may only relate nodes of THIS submission. This
        // gate guarantees request-shape consistency (every endpoint is a
        // declared node, so a typo'd or dangling endpoint is a clean
        // INVALID_ARGUMENT instead of an opaque Internal MissingDbId at
        // persist time). It deliberately does NOT decide whether the
        // submitter may DEFINE dependencies for those nodes — a request
        // can legitimately re-declare a resident node it merely joins.
        // Protection of resident nodes' dependency sets lives at the
        // merge edge loop (sched.merge.edge-creation-scoped): only the
        // submission that (re)creates a node may extend its children.
        // Every legitimate producer conforms to both: the gateway emits
        // each edge alongside its parent node and includes every child
        // as a node (BFS over inputDrvs; dedup_dag never drops referenced
        // nodes), and the hook fallback submits a single node, no edges.
        let submitted_paths: std::collections::HashSet<&str> =
            req.nodes.iter().map(|n| n.drv_path.as_str()).collect();
        for edge in &req.edges {
            if !submitted_paths.contains(edge.parent_drv_path.as_str()) {
                return Err(Status::invalid_argument(format!(
                    "edge parent {:?} is not a node of this submission",
                    edge.parent_drv_path
                )));
            }
            if !submitted_paths.contains(edge.child_drv_path.as_str()) {
                return Err(Status::invalid_argument(format!(
                    "edge child {:?} is not a node of this submission",
                    edge.child_drv_path
                )));
            }
        }
        // r[impl sched.recovery.inline-drv-durability+3]
        // Authoritative inline derivations are persisted and rebuilt
        // verbatim after a failover, so the scheduler must not take the
        // submitter's word for them (workers and direct submitters are
        // untrusted; the gateway being the only intended producer is
        // not a defense). Bind the bytes to the node's claimed identity
        // before they can ever reach the derivations table.
        //
        // r[impl sched.merge.ingress-inline-drv-binding+1]
        // Non-authoritative inline content (the gateway's inline-.drv
        // optimization) is bound to its declared identity the same way.
        // Both validators are CPU-bound (SHA-256 over up to 1 MiB per
        // node, ATerm parses, modulo-hash derivations over the sibling
        // graph), so they run on the blocking pool — the backpressure
        // gate above has already bounded how much of this work a caller
        // can queue.
        let mut nodes_for_validation = std::mem::take(&mut req.nodes);
        req.nodes = tokio::task::spawn_blocking(
            move || -> Result<Vec<rio_proto::types::DerivationNode>, Status> {
                validate_authoritative_drv_content(&nodes_for_validation)?;
                validate_inline_drv_content(&mut nodes_for_validation)?;
                Ok(nodes_for_validation)
            },
        )
        .await
        .map_err(|e| Status::internal(format!("inline-content validation task failed: {e}")))??;

        // UUID v7 (time-ordered, RFC 9562): the high 48 bits are Unix-ms
        // timestamp, so lexicographic sort == chronological sort. This
        // improves PG index locality on builds.build_id (recent builds
        // cluster at the end of the index, not scattered randomly like
        // v4). Build logs are keyed by (drv_hash, exec_id), not build_id
        // (see crate::logs::log_s3_key).
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
            jti,
            jwt_token,
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
        // r[impl sched.tenant.authz+2]
        let caller_tenant = self.require_tenant(&request).await?.map(|(sub, _)| sub);
        let req = request.into_inner();
        let build_id = Self::parse_build_id(&req.build_id)?;

        let (reply_tx, reply_rx) = oneshot::channel();

        let cmd = ActorCommand::WatchBuild {
            build_id,
            caller_tenant,
            since_sequence: req.since_sequence,
            reply: reply_tx,
        };

        let (bcast, last_seq) = self.send_and_await(cmd, reply_rx).await?;

        // Replay IF: pool available AND there's a gap to fill.
        // `since_sequence >= last_seq` → gateway already saw
        // everything, empty range, skip the PG round-trip.
        // `since_sequence == 0 && last_seq == 0` → build just
        // started, nothing emitted yet — common case, cheap exit.
        //
        // `since_sequence > last_seq` (counter regressed): defensive
        // full replay from 0. Shouldn't happen now that Log doesn't
        // consume seq numbers (event.rs), but a gateway that connected
        // to a pre-fix scheduler can carry an inflated `since`.
        let replay = match &self.db {
            Some(db) if req.since_sequence < last_seq => Some(EventReplay {
                pool: db.pool().clone(),
                build_id,
                since: req.since_sequence,
                last_seq,
            }),
            Some(db) if req.since_sequence > last_seq && last_seq > 0 => Some(EventReplay {
                pool: db.pool().clone(),
                build_id,
                since: 0,
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
        // r[impl sched.tenant.authz+2]
        let caller_tenant = self.require_tenant(&request).await?.map(|(sub, _)| sub);
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
        // r[impl sched.tenant.authz+2]
        let caller_tenant = self.require_tenant(&request).await?.map(|(sub, _)| sub);
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

/// Oracle-parity fixed-output SHAPE rule, shared by both inline-content
/// validators (`validate_authoritative_drv_content` and
/// `validate_inline_drv_content`) so they cannot drift: if any output of
/// the parsed derivation declares a fixed hash (`hash_algo` AND `hash`
/// set), the derivation MUST have exactly one output and it MUST be
/// named `out`.
///
/// CppNix 2.34.7 `BasicDerivation::type()` (src/libstore/derivations.cc):
/// "only one fixed output is allowed for now" /
/// "single fixed output must be named \"out\"". The gateway gate and the
/// worker glue enforce the same shape via `DerivationLike::is_fixed_output`
/// (single-out strict predicate).
///
/// r[impl sched.recovery.inline-drv-durability+3]
fn validate_inline_fod_shape(
    drv: &rio_nix::derivation::Derivation,
    context: &str,
) -> Result<(), Status> {
    let fixed: Vec<&str> = drv
        .outputs()
        .iter()
        .filter(|o| !o.hash_algo().is_empty() && !o.hash().is_empty())
        .map(|o| o.name())
        .collect();
    if fixed.is_empty() {
        return Ok(());
    }
    if drv.outputs().len() != 1 {
        return Err(Status::invalid_argument(format!(
            "{context}: only one fixed output is allowed (got {} outputs, {} fixed)",
            drv.outputs().len(),
            fixed.len()
        )));
    }
    if fixed[0] != "out" {
        return Err(Status::invalid_argument(format!(
            "{context}: single fixed output must be named \"out\" (got '{}')",
            fixed[0]
        )));
    }
    Ok(())
}

/// Per-output fixed-output BINDING validation, shared by both inline-content
/// validators so they cannot drift: decode the declared hash with the
/// shared length-discriminated parser, derive the store path it commits
/// to (per-output name handling via `output_path_name`), and require that
/// path to equal both the ATerm-declared path and the node's
/// `expected_output_paths` entry (which must be present and non-empty —
/// the merge gate uses fixed-output path agreement as content evidence).
///
/// Returns the derived path on success.
///
/// r[impl sched.recovery.inline-drv-durability+3]
#[allow(clippy::too_many_arguments)]
fn validate_fixed_output_binding(
    drv_name: &str,
    out_name: &str,
    aterm_path: &str,
    algo: &str,
    hash: &str,
    expected: Option<&str>,
    context: &str,
) -> Result<String, Status> {
    use rio_nix::hash::NixHash;
    use rio_nix::store_path::{StorePath, output_path_name};

    let (recursive, algo_str) = match algo.strip_prefix("r:") {
        Some(rest) => (true, rest),
        None => (false, algo),
    };
    let parsed_algo = algo_str.parse().map_err(|_| {
        Status::invalid_argument(format!(
            "{context} output '{out_name}' declares unsupported outputHashAlgo '{algo}'"
        ))
    })?;
    // Shared length-discriminated decode (base16 / nixbase32 / base64) —
    // identical to the gateway gate and the worker glue, so no component
    // can decode the same declaration differently.
    // r[impl nix.hash.fod-decode]
    let nix_hash = NixHash::parse_nonsri_unprefixed(parsed_algo, hash).map_err(|e| {
        Status::invalid_argument(format!(
            "{context} output '{out_name}': outputHash is not a valid base16, nixbase32, or \
             base64 hash: {e}"
        ))
    })?;
    // Defense in depth: derive with the per-output path NAME
    // (`<drv-name>` for "out", `<drv-name>-<output>` otherwise — CppNix
    // outputPathName). Unreachable for non-"out" outputs once the
    // single-out shape rule above is enforced, but a future relaxation
    // of that rule must not silently derive every output to the same
    // path.
    let path_name = output_path_name(drv_name, out_name);
    let derived =
        StorePath::make_fixed_output(&path_name, &nix_hash, recursive, &[]).map_err(|e| {
            Status::invalid_argument(format!(
                "{context} output '{out_name}': cannot derive fixed-output path: {e}"
            ))
        })?;
    if derived.as_str() != aterm_path {
        return Err(Status::invalid_argument(format!(
            "{context} output '{out_name}' declares path {aterm_path} but its declared hash \
             derives to {} — content and identity do not match",
            derived.as_str()
        )));
    }
    match expected {
        Some(exp) if !exp.is_empty() => {
            if exp != derived.as_str() {
                return Err(Status::invalid_argument(format!(
                    "node.expected_output_paths['{out_name}'] = {exp} does not match the \
                     fixed-output path {} derived from the {context}",
                    derived.as_str()
                )));
            }
        }
        // The entry MUST be present and non-empty: the merge gate uses
        // fixed-output path agreement as content evidence, so an
        // undeclared path would leave the persisted identity unbound to
        // the bytes.
        _ => {
            return Err(Status::invalid_argument(format!(
                "node.expected_output_paths['{out_name}'] must declare the fixed-output path {} \
                 derived from the {context}",
                derived.as_str()
            )));
        }
    }
    Ok(derived.as_str().to_owned())
}

/// Validate a node that claims `drv_content_authoritative`.
///
/// The flag means "persist these bytes and rebuild them verbatim after a
/// failover", so accepting them on faith would let a direct submitter
/// poison the persisted derivation content for an arbitrary `drv_hash`
/// (sticky until the next submission of that derivation) and have the
/// post-failover dispatch build attacker-chosen content under another
/// derivation's identity. The only legitimate producer is the gateway's
/// content-bound single-node hook fallback, so enforce exactly that
/// shape and bind the bytes to the node's claimed identity:
///
/// - single-node submission (the hook fallback is never part of a DAG);
/// - the bytes parse as a derivation whose platform, output names,
///   fixed-output flag, and content-addressed flag match the node's
///   `system` / `output_names` / `is_fixed_output` / `is_content_addressed`;
/// - exactly one `expected_output_paths` entry per output, and every
///   output is content-bound — fixed-output (declared hash, whose
///   recomputed store path must equal both the path declared inside the
///   ATerm and the node's `expected_output_paths` entry, which MUST be
///   declared) or floating-CA (no declared path, empty expected path);
/// - plain input-addressed outputs are rejected outright (the gateway
///   refuses inline IA derivations, and an IA-shaped authoritative blob
///   is exactly the poisoning shape this check exists to stop);
/// - when any output is floating-CA the node's `ca_modular_hash` must
///   equal `hash_derivation_modulo` recomputed over the supplied bytes
///   (and when present it must match for fixed-output nodes too).
///
/// `drv_hash` itself cannot be bound to the bytes (the inline
/// re-serialization deliberately does not text-hash to the client's
/// claimed `.drv` path), but with the checks above whatever is persisted
/// can only describe a content-bound build whose outputs the store
/// verifies against their own content — it can never produce bytes at
/// another derivation's input-addressed output paths.
fn validate_authoritative_drv_content(
    nodes: &[rio_proto::types::DerivationNode],
) -> Result<(), Status> {
    use rio_nix::derivation::{Derivation, DerivationLike};
    use rio_nix::store_path::StorePath;

    let Some(node) = nodes.iter().find(|n| n.drv_content_authoritative) else {
        return Ok(());
    };
    if nodes.len() != 1 {
        return Err(Status::invalid_argument(format!(
            "drv_content_authoritative is only valid for a single-node submission \
             (the content-bound hook fallback), got {} nodes",
            nodes.len()
        )));
    }
    if node.drv_content.is_empty() {
        return Err(Status::invalid_argument(
            "drv_content_authoritative requires non-empty drv_content",
        ));
    }
    let text = std::str::from_utf8(&node.drv_content).map_err(|_| {
        Status::invalid_argument("authoritative drv_content is not valid UTF-8 ATerm")
    })?;
    let drv = Derivation::parse(text).map_err(|e| {
        Status::invalid_argument(format!(
            "authoritative drv_content does not parse as a derivation: {e}"
        ))
    })?;

    if drv.platform() != node.system {
        return Err(Status::invalid_argument(format!(
            "authoritative drv_content platform {:?} does not match node.system {:?}",
            drv.platform(),
            node.system
        )));
    }
    let mut parsed_names: Vec<&str> = drv.outputs().iter().map(|o| o.name()).collect();
    parsed_names.sort_unstable();
    let mut node_names: Vec<&str> = node.output_names.iter().map(String::as_str).collect();
    node_names.sort_unstable();
    if parsed_names != node_names {
        return Err(Status::invalid_argument(format!(
            "authoritative drv_content outputs {parsed_names:?} do not match node.output_names {node_names:?}"
        )));
    }
    if node.is_fixed_output != drv.is_fixed_output() {
        return Err(Status::invalid_argument(
            "node.is_fixed_output does not match the authoritative drv_content",
        ));
    }
    // The merge-time conflict gate (sched.merge.authoritative-conflict)
    // compares is_content_addressed and the per-output expected paths, so
    // both must be BOUND to the bytes here — otherwise a squatter could
    // freely choose the very fields a later victim submission is compared
    // against. The legitimate producer (the gateway's content-bound hook
    // fallback) always sends the derived flag and exactly one entry per
    // output, so nothing legitimate is rejected.
    if node.is_content_addressed != drv.is_content_addressed() {
        return Err(Status::invalid_argument(
            "node.is_content_addressed does not match the authoritative drv_content",
        ));
    }
    // Oracle-parity FOD shape rule (single output named "out") — shared
    // helper so this validator and validate_inline_drv_content cannot
    // drift.
    validate_inline_fod_shape(&drv, "authoritative drv_content")?;
    if node.expected_output_paths.len() != node.output_names.len() {
        return Err(Status::invalid_argument(format!(
            "authoritative submissions must declare exactly one expected_output_paths entry per \
             output: got {} entries for {} outputs",
            node.expected_output_paths.len(),
            node.output_names.len()
        )));
    }

    let expected: std::collections::HashMap<&str, &str> = node
        .output_names
        .iter()
        .map(String::as_str)
        .zip(node.expected_output_paths.iter().map(String::as_str))
        .collect();
    let drv_name = StorePath::parse(&node.drv_path)
        .ok()
        .map(|sp| {
            sp.name()
                .strip_suffix(".drv")
                .unwrap_or_else(|| sp.name())
                .to_owned()
        })
        .unwrap_or_default();

    let mut has_floating = false;
    for out in drv.outputs() {
        let (name, path, algo, hash) = (out.name(), out.path(), out.hash_algo(), out.hash());
        if !algo.is_empty() && !hash.is_empty() {
            // Fixed output: the declared hash must derive to the path the
            // ATerm declares AND to the node's expected output path.
            // Shared binding helper (decode → derive → compare) — also
            // used by validate_inline_drv_content so the two validators
            // cannot drift.
            validate_fixed_output_binding(
                &drv_name,
                name,
                path,
                algo,
                hash,
                expected.get(name).copied(),
                "authoritative drv_content",
            )?;
        } else if !algo.is_empty() {
            // Floating-CA output: no static path exists yet anywhere.
            has_floating = true;
            if !path.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "authoritative drv_content output '{name}' is floating-CA but declares a \
                     path"
                )));
            }
            if let Some(exp) = expected.get(name)
                && !exp.is_empty()
            {
                return Err(Status::invalid_argument(format!(
                    "node.expected_output_paths['{name}'] must be empty for floating-CA output"
                )));
            }
        } else {
            return Err(Status::invalid_argument(format!(
                "authoritative inline derivations must be content-bound (fixed-output or \
                 floating-CA); output '{name}' is input-addressed"
            )));
        }
    }

    // The realisation key the gateway carries must be the hash of the
    // bytes it is carrying — recompute and compare. Mandatory when a
    // floating output exists (the realisation is how the result becomes
    // consumable); for FOD-only nodes an empty value is tolerated but a
    // present one must still match.
    if has_floating || !node.ca_modular_hash.is_empty() {
        let mut cache = std::collections::HashMap::new();
        let recomputed = rio_nix::derivation::hash_derivation_modulo(
            &drv,
            &node.drv_path,
            &|_| None,
            &mut cache,
        )
        .map_err(|e| {
            Status::invalid_argument(format!("authoritative drv_content cannot be hashed: {e}"))
        })?;
        if node.ca_modular_hash != recomputed.to_vec() {
            return Err(Status::invalid_argument(
                "node.ca_modular_hash does not match the authoritative drv_content",
            ));
        }
    }
    Ok(())
}

/// Validate NON-authoritative inline derivation content against the
/// node's declared identity.
///
/// r[impl sched.merge.ingress-inline-drv-binding+1]
/// The gateway's inline-`.drv` optimization attaches store-backed
/// derivations' canonical ATerm bytes to will-dispatch nodes so workers
/// skip a store fetch. Those bytes flow into `WorkAssignment.drv_content`
/// (the worker builds what they say) and the node's declared identity
/// flows into upload-authorization claims — but ingress never bound the
/// two together for non-authoritative content, so a direct submitter
/// could declare arbitrary identity fields (expected output paths,
/// flags) alongside inline bytes describing something else entirely:
/// the variant-1 squat (register attacker content at a victim's
/// not-yet-built IA path via an honest worker) and the variant-3 flag
/// forgery both ride that gap.
///
/// Every node with non-empty `drv_content` and
/// `drv_content_authoritative == false` must now satisfy:
///
/// 1. the bytes are UTF-8, parse as a derivation, and are CANONICAL
///    (byte-identical to the parse→re-serialize round trip — what the
///    gateway inlines is exactly `Derivation::to_aterm()` output);
/// 2. the declared `drv_path` is the text content-address of those
///    bytes: `make_text(name, sha256(bytes), inputSrcs ∪ inputDrvs)` —
///    the same minting rule CppNix `writeDerivation` uses, so the path
///    is cryptographically bound to the content;
/// 3. the declared `system`, `output_names`, `is_fixed_output`, and
///    `is_content_addressed` equal the parsed derivation's;
/// 4. `expected_output_paths` (zipped with `output_names` BY NAME) are
///    bound per output kind: fixed-output → the hash-derived path
///    (shared helper with the authoritative validator; unsupported
///    legacy algos like md5 skip the derivation check — the text-CA
///    binding above and the store's content verification still hold);
///    floating-CA and deferred-IA → empty; declared-IA → equal to BOTH
///    the ATerm-declared path and the path recomputed via
///    `input_addressed_output_paths` (inputs resolved from sibling
///    inline derivations, hash cache seeded from sibling
///    `ca_modular_hash` declarations; an unresolvable input is a
///    rejection, not a skip);
/// 5. a non-empty `ca_modular_hash` equals the recomputed
///    `hash_derivation_modulo` over the bytes. The seed contains only
///    hashes whose published form IS the walk's input-position
///    (mask=false) form: store-backed IA/FOD/deferred-IA siblings.
///    Inline siblings recompute from bytes; store-backed FLOATING
///    siblings publish the masked-subject form (oracle parity), which
///    cannot stand in for the input form — a consumer that needs one
///    has an ingress-UNVERIFIABLE hash, and an unverifiable claim is NO
///    claim: the declaration is STRIPPED (the submission is otherwise
///    accepted — warm gateway submissions legitimately have this
///    shape), so nothing downstream consumes a value ingress never
///    checked; the dispatch-resolve/completion path re-establishes the
///    hash from bytes it can verify.
///
/// The seeded-sibling design is sound against squats: a forged sibling
/// hash moves every derived path AWAY from honest paths (SHA-256 second
/// preimage to collide one), so seeding cannot help an attacker reach a
/// victim's path — see `input_addressed_output_paths`' soundness note.
fn validate_inline_drv_content(
    nodes: &mut [rio_proto::types::DerivationNode],
) -> Result<(), Status> {
    use rio_nix::derivation::{Derivation, DerivationLike};
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use sha2::{Digest, Sha256};

    if !nodes
        .iter()
        .any(|n| !n.drv_content.is_empty() && !n.drv_content_authoritative)
    {
        return Ok(());
    }

    // Phase A runs over a shared reborrow; the only mutation (stripping
    // unverifiable hash claims) happens at the end, once every borrow
    // into the slice is dead.
    let nodes_ro = &*nodes;

    // Parse every inline derivation once (authoritative ones too — they
    // serve as sibling resolvers for IA path derivation below).
    let mut parsed: std::collections::HashMap<&str, Derivation> = std::collections::HashMap::new();
    for node in nodes_ro {
        if node.drv_content.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(&node.drv_content).map_err(|_| {
            Status::invalid_argument(format!(
                "node {} inline drv_content is not valid UTF-8 ATerm",
                node.drv_hash
            ))
        })?;
        let drv = Derivation::parse(text).map_err(|e| {
            Status::invalid_argument(format!(
                "node {} inline drv_content does not parse as a derivation: {e}",
                node.drv_hash
            ))
        })?;
        parsed.insert(node.drv_path.as_str(), drv);
    }

    // Sibling hash seed, keyed by drv_path. The walk's cache holds the
    // mask_outputs=FALSE form (the input-position digest), so a declared
    // hash may seed it ONLY when the published form equals that form:
    //
    //   - inline siblings are EXCLUDED — the walk recomputes their
    //     unmasked form from the bytes via `parsed` (and step 6 verifies
    //     their declarations on their own turn);
    //   - store-backed floating-CA siblings are EXCLUDED — their
    //     published hash is the masked-subject form (`mask_outputs =
    //     has_ca_floating_outputs()`, oracle parity), which is NOT the
    //     input form, and the unmasked form is underivable without the
    //     bytes. A consumer whose recompute needs one degrades to
    //     no-evidence below instead of false-rejecting.
    //   - store-backed IA / FOD / deferred-IA siblings seed soundly:
    //     for them mask_outputs=false, so published == input form.
    //
    // Forged seeds cannot steer derived paths toward any honest path
    // (see the soundness note).
    let sibling_seed: std::collections::HashMap<String, [u8; 32]> = nodes_ro
        .iter()
        .filter(|n| {
            // "Not a floating-CA node": floating published forms are
            // masked and must never seed the input-form cache.
            let not_floating = n.is_fixed_output || !n.is_content_addressed;
            n.ca_modular_hash.len() == 32 && n.drv_content.is_empty() && not_floating
        })
        .map(|n| {
            let mut h = [0u8; 32];
            h.copy_from_slice(&n.ca_modular_hash);
            (n.drv_path.clone(), h)
        })
        .collect();

    // Indices whose declared ca_modular_hash proved UNVERIFIABLE at
    // ingress: an unverifiable claim is NO claim — the field is cleared
    // before the request proceeds, so nothing downstream (merge-gate
    // identity comparisons, realisation keying, the persistence upsert)
    // can consume a value ingress never checked.
    let mut strip_unverifiable: Vec<usize> = Vec::new();

    for (idx, node) in nodes_ro.iter().enumerate() {
        if node.drv_content.is_empty() || node.drv_content_authoritative {
            continue;
        }
        let context = format!("inline drv_content for node {}", node.drv_hash);
        let drv = &parsed[node.drv_path.as_str()];

        // ── 1. Canonicality ─────────────────────────────────────────
        let canonical = drv.to_aterm();
        if canonical.as_bytes() != node.drv_content.as_slice() {
            return Err(Status::invalid_argument(format!(
                "{context}: bytes are not the canonical ATerm serialization of the derivation \
                 they parse as (the gateway inlines canonical bytes; non-canonical inline \
                 content cannot be path-bound)"
            )));
        }

        // ── 2. Text-CA binding of the declared .drv path ─────────────
        let drv_sp = StorePath::parse(&node.drv_path).map_err(|e| {
            Status::invalid_argument(format!("{context}: drv_path does not parse: {e}"))
        })?;
        let mut refs: Vec<StorePath> = Vec::new();
        for r in drv.input_srcs().iter().chain(drv.input_drvs().keys()) {
            refs.push(StorePath::parse(r).map_err(|e| {
                Status::invalid_argument(format!(
                    "{context}: derivation references invalid store path {r:?}: {e}"
                ))
            })?);
        }
        let content_hash = NixHash::new(
            HashAlgo::SHA256,
            Sha256::digest(node.drv_content.as_slice()).to_vec(),
        )
        .map_err(|e| Status::internal(format!("sha256 digest construction failed: {e}")))?;
        let text_ca = StorePath::make_text(drv_sp.name(), &content_hash, &refs).map_err(|e| {
            Status::invalid_argument(format!("{context}: cannot derive text CA path: {e}"))
        })?;
        if text_ca.as_str() != node.drv_path {
            return Err(Status::invalid_argument(format!(
                "{context}: declared drv_path {} is not the text content-address of the inline \
                 bytes (expected {}) — the path is not bound to this content",
                node.drv_path,
                text_ca.as_str()
            )));
        }

        // ── 3. Declared identity equals parsed identity ──────────────
        if drv.platform() != node.system {
            return Err(Status::invalid_argument(format!(
                "{context}: platform {:?} does not match node.system {:?}",
                drv.platform(),
                node.system
            )));
        }
        let mut parsed_names: Vec<&str> = drv.outputs().iter().map(|o| o.name()).collect();
        parsed_names.sort_unstable();
        let mut node_names: Vec<&str> = node.output_names.iter().map(String::as_str).collect();
        node_names.sort_unstable();
        if parsed_names != node_names {
            return Err(Status::invalid_argument(format!(
                "{context}: outputs {parsed_names:?} do not match node.output_names {node_names:?}"
            )));
        }
        if node.is_fixed_output != drv.is_fixed_output() {
            return Err(Status::invalid_argument(format!(
                "{context}: node.is_fixed_output does not match the parsed derivation"
            )));
        }
        if node.is_content_addressed != drv.is_content_addressed() {
            return Err(Status::invalid_argument(format!(
                "{context}: node.is_content_addressed does not match the parsed derivation"
            )));
        }
        // Oracle-parity FOD shape rule — shared with the authoritative
        // validator so the two cannot drift.
        validate_inline_fod_shape(drv, &context)?;

        // ── 4/5. Per-output binding (zipped BY NAME) ─────────────────
        if node.expected_output_paths.len() != node.output_names.len() {
            return Err(Status::invalid_argument(format!(
                "{context}: must declare exactly one expected_output_paths entry per output: got \
                 {} entries for {} outputs",
                node.expected_output_paths.len(),
                node.output_names.len()
            )));
        }
        let expected: std::collections::HashMap<&str, &str> = node
            .output_names
            .iter()
            .map(String::as_str)
            .zip(node.expected_output_paths.iter().map(String::as_str))
            .collect();
        let drv_name = drv_sp
            .name()
            .strip_suffix(".drv")
            .unwrap_or_else(|| drv_sp.name())
            .to_owned();

        // Pre-compute IA output paths if any output is declared-IA
        // (non-empty path, no hash algo). Inputs resolve from sibling
        // inline derivations; hashes seed from sibling declarations.
        let needs_ia = drv
            .outputs()
            .iter()
            .any(|o| o.hash_algo().is_empty() && !o.path().is_empty());
        let ia_paths = if needs_ia {
            let resolve = |p: &str| parsed.get(p);
            let mut hash_cache = sibling_seed.clone();
            // The node's own declared hash must never feed its own
            // path derivation (self-certification); inputs only.
            hash_cache.remove(&node.drv_path);
            Some(
                rio_nix::derivation::input_addressed_output_paths(
                    drv,
                    &node.drv_path,
                    &resolve,
                    &mut hash_cache,
                )
                .map_err(|e| {
                    Status::invalid_argument(format!(
                        "{context}: cannot derive input-addressed output paths from the inline \
                         bytes ({e}); every input must be a sibling inline derivation or carry \
                         a sibling ca_modular_hash declaration"
                    ))
                })?,
            )
        } else {
            None
        };

        for out in drv.outputs() {
            let (name, path, algo, hash) = (out.name(), out.path(), out.hash_algo(), out.hash());
            let exp = expected.get(name).copied();
            if !algo.is_empty() && !hash.is_empty() {
                // Fixed output. Supported algos get the full
                // decode→derive→compare binding (shared helper).
                // Unsupported legacy algos (md5): the binding cannot be
                // recomputed here — the text-CA binding above plus the
                // store's content verification on upload remain the
                // enforcement; the expected path must still equal the
                // ATerm-declared one.
                let algo_supported = algo
                    .strip_prefix("r:")
                    .unwrap_or(algo)
                    .parse::<HashAlgo>()
                    .is_ok();
                if algo_supported {
                    validate_fixed_output_binding(
                        &drv_name, name, path, algo, hash, exp, &context,
                    )?;
                } else {
                    match exp {
                        Some(e) if e == path && !e.is_empty() => {}
                        _ => {
                            return Err(Status::invalid_argument(format!(
                                "{context}: output '{name}' (legacy algo {algo:?}) must declare \
                                 expected_output_paths equal to the ATerm path {path:?}"
                            )));
                        }
                    }
                }
            } else if !algo.is_empty() {
                // Floating-CA: no path exists yet anywhere.
                if !path.is_empty() {
                    return Err(Status::invalid_argument(format!(
                        "{context}: output '{name}' is floating-CA but declares a path"
                    )));
                }
                if let Some(e) = exp
                    && !e.is_empty()
                {
                    return Err(Status::invalid_argument(format!(
                        "{context}: expected_output_paths['{name}'] must be empty for a \
                         floating-CA output"
                    )));
                }
            } else if path.is_empty() {
                // Deferred-IA: path unknown until inputs resolve.
                if let Some(e) = exp
                    && !e.is_empty()
                {
                    return Err(Status::invalid_argument(format!(
                        "{context}: expected_output_paths['{name}'] must be empty for a \
                         deferred input-addressed output"
                    )));
                }
            } else {
                // Declared-IA: the ATerm path, the recomputed path, and
                // the node's expected path must all agree.
                let derived = ia_paths.as_ref().and_then(|m| m.get(name)).ok_or_else(|| {
                    Status::invalid_argument(format!(
                        "{context}: output '{name}' has no derivable input-addressed path"
                    ))
                })?;
                if derived.as_str() != path {
                    return Err(Status::invalid_argument(format!(
                        "{context}: output '{name}' declares path {path} but the inline bytes \
                         derive to {} — the declared identity is not this derivation's",
                        derived.as_str()
                    )));
                }
                match exp {
                    Some(e) if e == path => {}
                    _ => {
                        return Err(Status::invalid_argument(format!(
                            "{context}: expected_output_paths['{name}'] must equal the derived \
                             input-addressed path {path}"
                        )));
                    }
                }
            }
        }

        // ── 6. ca_modular_hash binding ────────────────────────────────
        if !node.ca_modular_hash.is_empty() {
            let resolve = |p: &str| parsed.get(p);
            let mut hash_cache = sibling_seed.clone();
            // Never let the node's own declaration satisfy its own check
            // (belt-and-suspenders: inline nodes are never seeded).
            hash_cache.remove(&node.drv_path);
            match rio_nix::derivation::hash_derivation_modulo(
                drv,
                &node.drv_path,
                &resolve,
                &mut hash_cache,
            ) {
                Ok(recomputed) => {
                    if node.ca_modular_hash != recomputed.to_vec() {
                        return Err(Status::invalid_argument(format!(
                            "{context}: ca_modular_hash does not match the inline bytes"
                        )));
                    }
                }
                Err(rio_nix::derivation::DerivationError::InputNotFound(input)) => {
                    // A transitive input's unmasked form is unavailable:
                    // the input is store-backed AND floating-CA (its
                    // published hash is the masked-subject form, which
                    // cannot stand in for the input-position digest).
                    // The gateway legitimately produces this shape on
                    // warm submissions — an inline will-dispatch
                    // consumer of an already-realized floating drv whose
                    // bytes are not re-inlined — so rejecting would break
                    // honest traffic. But an UNVERIFIABLE claim is NO
                    // claim: the declaration is STRIPPED below, never
                    // forwarded unverified (it would otherwise flow into
                    // merge-gate identity evidence, realisation keys, and
                    // the persisted row). The authoritative value is
                    // re-established downstream by the dispatch-resolve /
                    // completion path, which has the bytes to verify.
                    tracing::info!(
                        node = %node.drv_hash,
                        unresolvable_input = %input,
                        "inline ca_modular_hash unverifiable at ingress \
                         (floating store-backed input); stripping the \
                         unverified declaration"
                    );
                    strip_unverifiable.push(idx);
                }
                Err(e) => {
                    return Err(Status::invalid_argument(format!(
                        "{context}: cannot be modulo-hashed: {e}"
                    )));
                }
            }
        }
    }

    // Phase B: an unverifiable claim is no claim. Mutation happens only
    // here, after every borrow into the slice from phase A is dead.
    for idx in strip_unverifiable {
        nodes[idx].ca_modular_hash.clear();
    }
    Ok(())
}

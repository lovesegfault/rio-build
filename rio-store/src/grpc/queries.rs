//! Read-side StoreService RPCs (QueryPathInfo, FindMissingPaths,
//! AddSignatures, realisations, TenantQuota). Inherent methods on
//! [`StoreServiceImpl`]; the `StoreService` trait impl in `mod.rs`
//! delegates here so the trait body stays a flat list of one-liners.

use tonic::{Request, Response, Status};
use tracing::{debug, warn};

use rio_proto::types::{
    AddSignaturesRequest, AddSignaturesResponse, BatchGetManifestRequest, BatchGetManifestResponse,
    BatchQueryPathInfoRequest, BatchQueryPathInfoResponse, ChunkRef, FindMissingPathsRequest,
    FindMissingPathsResponse, ManifestEntry, ManifestHint, PathInfo, PathInfoEntry,
    QueryPathFromHashPartRequest, QueryPathInfoRequest, QueryRealisationRequest, Realisation,
    RegisterRealisationRequest, RegisterRealisationResponse, TenantQuotaRequest,
    TenantQuotaResponse,
};

use rio_common::grpc::StatusExt;
use rio_common::tenant::NormalizedName;

use crate::metadata::{self, ManifestKind};
use crate::realisations;

use super::{StoreServiceImpl, metadata_status, validate_store_path};

impl StoreServiceImpl {
    /// DoS bound + per-path format check shared by the batch read RPCs.
    /// Rejects the whole batch on any malformed path (client bug indicator).
    fn validate_path_batch(&self, paths: &[String]) -> Result<(), Status> {
        if paths.len() > self.max_batch_paths {
            return Err(Status::invalid_argument(format!(
                "too many paths: {} (max {}; raise RIO_MAX_BATCH_PATHS to allow larger batches)",
                paths.len(),
                self.max_batch_paths
            )));
        }
        for p in paths {
            validate_store_path(p)?;
        }
        Ok(())
    }

    /// Query metadata for a single store path.
    ///
    /// Only returns paths with manifests.status='complete'.
    pub(super) async fn query_path_info_impl(
        &self,
        request: Request<QueryPathInfoRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant_id = self.request_tenant_id(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        let local = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("QueryPathInfo: query_path_info", e))?;

        let info = match local {
            Some(i) => {
                // r[impl store.substitute.tenant-sig-visibility+2]
                // Local hit — but is it visible to THIS tenant? A path
                // substituted by tenant A with sig_mode=keep is
                // invisible to tenant B unless B trusts A's upstream
                // key. Hide-as-NotFound on gate failure.
                if !self.sig_visibility_gate(tenant_id, &i).await? {
                    // Gate failed — but B's upstreams may ALSO have
                    // this path. Try substituting (which will
                    // append B-trusted sigs to the existing row).
                    if let Some(sub) = self
                        .try_substitute_on_miss(tenant_id, &req.store_path)
                        .await?
                    {
                        sub
                    } else {
                        return Err(Status::not_found(format!(
                            "path not found: {}",
                            req.store_path
                        )));
                    }
                } else {
                    i
                }
            }
            None => {
                // Local miss — try upstream substitution.
                self.try_substitute_on_miss(tenant_id, &req.store_path)
                    .await?
                    .ok_or_else(|| {
                        Status::not_found(format!("path not found: {}", req.store_path))
                    })?
            }
        };

        Ok(Response::new(info.into()))
    }

    /// Batch query metadata for many paths in one PG round-trip.
    ///
    /// I-110: builder closure-BFS path. Local-only — NO upstream
    /// substitution and NO sig-visibility gate (both add per-path
    /// round-trips, defeating the batch). Callers needing those use
    /// `query_path_info`. End-user tenant tokens are rejected
    /// (`reject_end_user_tenant`) so the gate-skip can't be used as
    /// a bypass.
    // r[impl store.api.batch-query+2]
    pub(super) async fn batch_query_path_info_impl(
        &self,
        request: Request<BatchQueryPathInfoRequest>,
    ) -> Result<Response<BatchQueryPathInfoResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.reject_end_user_tenant(&request, "BatchQueryPathInfo")?;
        let req = request.into_inner();

        self.validate_path_batch(&req.store_paths)?;

        let entries = metadata::query_path_info_batch(&self.pool, &req.store_paths)
            .await
            .map_err(|e| metadata_status("BatchQueryPathInfo: query_path_info_batch", e))?
            .into_iter()
            .map(|(store_path, info)| PathInfoEntry {
                store_path,
                info: info.map(Into::into),
            })
            .collect();

        Ok(Response::new(BatchQueryPathInfoResponse { entries }))
    }

    /// Batch (PathInfo, manifest) lookup for many paths in ≤2 PG round-trips.
    ///
    /// I-110c: builder FUSE-warm prefetch. Local-only — same caveats as
    /// `batch_query_path_info` (no upstream substitution, no
    /// sig-visibility gate; end-user tenant tokens rejected).
    // r[impl store.api.batch-manifest+2]
    pub(super) async fn batch_get_manifest_impl(
        &self,
        request: Request<BatchGetManifestRequest>,
    ) -> Result<Response<BatchGetManifestResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.reject_end_user_tenant(&request, "BatchGetManifest")?;
        let req = request.into_inner();

        self.validate_path_batch(&req.store_paths)?;

        let entries = metadata::get_manifest_batch(&self.pool, &req.store_paths)
            .await
            .map_err(|e| metadata_status("BatchGetManifest: get_manifest_batch", e))?
            .into_iter()
            .map(|(store_path, found)| {
                let hint = found.map(|(info, kind)| {
                    let (chunks, inline_blob) = match kind {
                        ManifestKind::Inline(b) => (Vec::new(), b.to_vec()),
                        ManifestKind::Chunked(entries) => (
                            entries
                                .into_iter()
                                .map(|(hash, size)| ChunkRef {
                                    hash: hash.to_vec(),
                                    size,
                                })
                                .collect(),
                            Vec::new(),
                        ),
                    };
                    ManifestHint {
                        info: Some(info.into()),
                        chunks,
                        inline_blob,
                    }
                });
                ManifestEntry { store_path, hint }
            })
            .collect();

        Ok(Response::new(BatchGetManifestResponse { entries }))
    }

    /// Batch check which paths are missing from the store.
    ///
    /// Only completed paths (manifests.status='complete') count as "present".
    pub(super) async fn find_missing_paths_impl(
        &self,
        request: Request<FindMissingPathsRequest>,
    ) -> Result<Response<FindMissingPathsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Capture before PG work: `find_missing_paths` (ANY over ≤153k)
        // + `sig_visibility_gate_batch` (3 PG round-trips) eat into the
        // 2s slack vs scheduler's 90s `MERGE_FMP_TIMEOUT` if the
        // budget is computed AFTER them.
        let entry = tokio::time::Instant::now();
        let tenant_id = self.request_tenant_id(&request);
        let req = request.into_inner();

        self.validate_path_batch(&req.store_paths)?;

        let mut missing = metadata::find_missing_paths(&self.pool, &req.store_paths)
            .await
            .map_err(|e| metadata_status("FindMissingPaths: find_missing_paths", e))?;

        // r[impl store.substitute.find-missing-gated]
        // Gate the locally-PRESENT paths: a substitution-only path
        // (zero `path_tenants` rows, no trusted sig) must be reported
        // as missing or the scheduler's `check_cached_outputs` →
        // `upsert_path_tenants_for_batch` launders it into "built" and
        // permanently defeats the gate for every tenant. Anonymous /
        // no-substituter requests pass through (gate_batch returns the
        // full set).
        let missing_set: std::collections::HashSet<&str> =
            missing.iter().map(String::as_str).collect();
        let present: Vec<String> = req
            .store_paths
            .iter()
            .filter(|p| !missing_set.contains(p.as_str()))
            .cloned()
            .collect();
        let visible = self.sig_visibility_gate_batch(tenant_id, &present).await?;
        for p in present {
            if !visible.contains(&p) {
                missing.push(p);
            }
        }

        // HEAD-probe each missing path against the tenant's upstreams.
        // Fails-open on probe errors (a down upstream shouldn't hide
        // paths the scheduler can otherwise substitute). Empty if no
        // substituter / no tenant / no upstreams — the normal case.
        let substitutable = match (&self.substituter, tenant_id) {
            (Some(sub), Some(tid)) if !missing.is_empty() => sub
                .check_available(
                    tid,
                    &missing,
                    entry + crate::substitute::CHECK_AVAILABLE_DEFAULT_BUDGET,
                )
                .await
                .unwrap_or_else(|e| {
                    warn!(error = %e, "check_available failed; empty substitutable_paths");
                    Vec::new()
                }),
            _ => Vec::new(),
        };

        debug!(
            n_requested = req.store_paths.len(),
            n_missing = missing.len(),
            n_substitutable = substitutable.len(),
            tenant_id = ?tenant_id,
            substituter = self.substituter.is_some(),
            "FindMissingPaths"
        );

        Ok(Response::new(FindMissingPathsResponse {
            missing_paths: missing,
            substitutable_paths: substitutable,
        }))
    }

    /// Resolve a store path from its 32-char nixbase32 hash part.
    // r[impl store.api.hash-part+2]
    pub(super) async fn query_path_from_hash_part_impl(
        &self,
        request: Request<QueryPathFromHashPartRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant_id = self.request_tenant_id(&request);
        let req = request.into_inner();

        // Validate BEFORE touching PG. The hash-part flows into a LIKE
        // pattern (metadata::query_by_hash_part builds `/nix/store/{hash}-%`);
        // an unvalidated `%` or `_` would be LIKE-injection. nixbase32's
        // alphabet has neither (0-9, a-z minus e/o/t/u), so a successful
        // decode blocks that.
        //
        // 32 chars = 20 bytes of hash (Nix's compressHash output). Anything
        // else is a client bug, not a missing path — INVALID_ARGUMENT, not
        // NOT_FOUND.
        //
        // nixbase32::decode() checks BOTH length-validity AND charset in one
        // call. We throw away the decoded bytes — it's purely a validator
        // here. 20-byte allocation + discard; negligible next to the PG query.
        if req.hash_part.len() != rio_nix::store_path::HASH_CHARS {
            return Err(Status::invalid_argument(format!(
                "hash_part must be {} chars (nixbase32), got {}",
                rio_nix::store_path::HASH_CHARS,
                req.hash_part.len()
            )));
        }
        if let Err(e) = rio_nix::store_path::nixbase32::decode(&req.hash_part) {
            return Err(Status::invalid_argument(format!(
                "hash_part is not valid nixbase32: {e}"
            )));
        }

        let info = metadata::query_by_hash_part(&self.pool, &req.hash_part)
            .await
            .map_err(|e| metadata_status("QueryPathFromHashPart: query_by_hash_part", e))?
            .ok_or_else(|| {
                Status::not_found(format!("no path with hash part: {}", req.hash_part))
            })?;

        // r[impl store.substitute.tenant-sig-visibility+2]
        // Same gate as QueryPathInfo. Hash-part lookup is local-only
        // (no upstream knows our hash space), so on gate failure
        // there's no try_substitute_on_miss fallback — just NotFound.
        if !self.sig_visibility_gate(tenant_id, &info).await? {
            return Err(Status::not_found(format!(
                "no path with hash part: {}",
                req.hash_part
            )));
        }

        Ok(Response::new(info.into()))
    }

    /// Append signatures to an existing store path.
    // r[impl store.api.add-signatures+2]
    pub(super) async fn add_signatures_impl(
        &self,
        request: Request<AddSignaturesRequest>,
    ) -> Result<Response<AddSignaturesResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant_id = self.request_tenant_id(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        // Bound the signatures list — a malicious client could send 1M sigs
        // and we'd append them all. MAX_SIGNATURES matches PutPath's bound.
        rio_common::grpc::check_bound(
            "signatures",
            req.signatures.len(),
            rio_common::limits::MAX_SIGNATURES,
        )?;

        // r[impl store.tenant.narinfo-filter]
        // Gate BEFORE the empty-list short-circuit so AddSignatures can't
        // be used as an existence probe (Ok vs NotFound) for paths the
        // sig-visibility gate hides, and BEFORE the write so a tenant
        // can't fill another tenant's path with junk sigs and DoS its
        // `nix store sign` via the MAX_SIGNATURES post-dedup cap. Same
        // pattern as `query_path_info_impl`; on gate failure return
        // NotFound (not PermissionDenied) — gate-hidden paths are
        // indistinguishable from absent.
        let info = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("AddSignatures: query_path_info", e))?
            .ok_or_else(|| Status::not_found(format!("path not found: {}", req.store_path)))?;
        if !self.sig_visibility_gate(tenant_id, &info).await? {
            return Err(Status::not_found(format!(
                "path not found: {}",
                req.store_path
            )));
        }

        // Empty sigs list: no-op. Don't hit PG for nothing. Not an error —
        // `nix store sign` with no configured keys can legitimately produce
        // this (it sends the opcode but with zero sigs).
        if req.signatures.is_empty() {
            return Ok(Response::new(AddSignaturesResponse {}));
        }

        let rows = metadata::append_signatures(&self.pool, &req.store_path, &req.signatures)
            .await
            .map_err(|e| metadata_status("AddSignatures: append_signatures", e))?;

        if rows == 0 {
            return Err(Status::not_found(format!(
                "path not found: {}",
                req.store_path
            )));
        }

        Ok(Response::new(AddSignaturesResponse {}))
    }

    /// Register a CA derivation realisation.
    ///
    /// Service-caller-only: the `realisations` table is a global
    /// namespace with no `tenant_id` column; an unauthenticated write
    /// is cross-tenant CA supply-chain injection (pre-register
    /// `(public_nixpkgs_hash → /nix/store/EVIL)`, every other tenant's
    /// resolve picks it up). The gateway no longer dispatches
    /// `wopRegisterDrvOutput` (rio has no trusted-user concept;
    /// realisations are scheduler-written at build-completion via
    /// `insert_realisation_batch`), so this gate is defense-in-depth
    /// for direct gRPC + Cilium misconfig.
    // r[impl store.realisation.register+2]
    pub(super) async fn register_realisation_impl(
        &self,
        request: Request<RegisterRealisationRequest>,
    ) -> Result<Response<RegisterRealisationResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Dev-mode pass-through (`service_verifier=None`) matches the
        // `hmac_verifier=None` semantics elsewhere — production always
        // configures both.
        if self.service_verifier.is_some() && self.verified_service_caller(&request).is_none() {
            return Err(Status::permission_denied(
                "RegisterRealisation requires a service token; \
                 realisations are scheduler-managed",
            ));
        }
        let proto = request
            .into_inner()
            .realisation
            .ok_or_else(|| Status::invalid_argument("realisation field is required"))?;

        // Validate hash lengths at the trust boundary. Proto bytes fields
        // are unbounded Vec<u8>; the DB layer expects [u8; 32]. Doing the
        // try_into here (not in realisations::insert) keeps the DB layer
        // free of proto-specific validation and gives a useful gRPC status
        // back to the client instead of an internal error.
        let drv_hash: [u8; 32] = proto.drv_hash.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "drv_hash must be 32 bytes (SHA-256), got {}",
                proto.drv_hash.len()
            ))
        })?;
        let output_hash: [u8; 32] = proto.output_hash.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "output_hash must be 32 bytes (SHA-256), got {}",
                proto.output_hash.len()
            ))
        })?;

        if proto.output_name.is_empty() {
            return Err(Status::invalid_argument("output_name must not be empty"));
        }
        // output_path validation: must be a well-formed store path. Same
        // check as PutPath — rejects traversal, bad nixbase32, etc.
        validate_store_path(&proto.output_path)?;

        // Bound sigs list. Same limit as narinfo.signatures.
        rio_common::grpc::check_bound(
            "signatures",
            proto.signatures.len(),
            rio_common::limits::MAX_SIGNATURES,
        )?;

        // TODO: realisation signature verification once the scheduler's
        // insert_realisation_batch path signs (store.md:237 — signed
        // tuple is (drv_hash, output_name, output_path, nar_hash)).
        // Adding sig-verify here without a signer would reject all
        // writes; the service-caller gate above + gateway opcode
        // rejection + RealisationConflict detection close the attack
        // without it.

        let r = realisations::Realisation {
            drv_hash,
            output_name: proto.output_name,
            output_path: proto.output_path,
            output_hash,
            signatures: proto.signatures,
        };

        realisations::insert(&self.pool, &r).await.map_err(|e| {
            if matches!(
                e,
                crate::metadata::MetadataError::RealisationConflict { .. }
            ) {
                // Loud: this is either a determinism bug or attempted
                // poison. WARN both paths so it surfaces in alerts.
                warn!(error = %e, "realisation conflict");
                Status::already_exists(e.to_string())
            } else {
                metadata_status("RegisterRealisation: insert", e)
            }
        })?;

        Ok(Response::new(RegisterRealisationResponse {}))
    }

    /// Look up a CA derivation realisation.
    // r[impl store.realisation.query]
    pub(super) async fn query_realisation_impl(
        &self,
        request: Request<QueryRealisationRequest>,
    ) -> Result<Response<Realisation>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        let drv_hash: [u8; 32] = req.drv_hash.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "drv_hash must be 32 bytes (SHA-256), got {}",
                req.drv_hash.len()
            ))
        })?;
        if req.output_name.is_empty() {
            return Err(Status::invalid_argument("output_name must not be empty"));
        }

        let r = realisations::query(&self.pool, &drv_hash, &req.output_name)
            .await
            .map_err(|e| metadata_status("QueryRealisation: query", e))?
            .ok_or_else(|| {
                // Cache miss, not an error. Gateway maps this to an
                // empty-set wire response.
                Status::not_found(format!(
                    "no realisation for ({}, {})",
                    hex::encode(drv_hash),
                    req.output_name
                ))
            })?;

        Ok(Response::new(Realisation {
            drv_hash: r.drv_hash.to_vec(),
            output_name: r.output_name,
            output_path: r.output_path,
            output_hash: r.output_hash.to_vec(),
            signatures: r.signatures,
        }))
    }

    /// Per-tenant store usage + configured quota. Backs the gateway's
    /// pre-SubmitBuild quota gate (`r[store.gc.tenant-quota-enforce]`).
    ///
    /// Takes `tenant_name` (not UUID) so the gateway can call in
    /// dual-mode fallback (JWT disabled → no tenant_id resolved). The
    /// store owns the `tenants` table; joining on name here keeps the
    /// gateway PG-free.
    ///
    /// NOT_FOUND on unknown tenant — the gateway treats that as "no
    /// quota, pass through" (same as single-tenant mode: the empty
    /// tenant_name never hits this RPC at all).
    pub(super) async fn tenant_quota_impl(
        &self,
        request: Request<TenantQuotaRequest>,
    ) -> Result<Response<TenantQuotaResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Invalid name is a gateway bug here — the quota gate
        // short-circuits single-tenant mode BEFORE hitting this RPC.
        // Reject explicitly. `NameError::InteriorWhitespace` catches
        // the `"team a"` case that an ad-hoc `.trim()` would miss —
        // a rejected InvalidArgument with the specific error beats a
        // NOT_FOUND from the PG lookup downstream.
        let name = NormalizedName::new(&req.tenant_name).map_err(|e| {
            Status::invalid_argument(format!(
                "tenant_name invalid: {e} (gateway should gate single-tenant mode before calling)"
            ))
        })?;

        let quota = crate::gc::tenant::tenant_quota_by_name(&self.pool, &name)
            .await
            .status_internal("TenantQuota: tenant_quota_by_name")?
            .ok_or_else(|| Status::not_found(format!("unknown tenant: {name}")))?;

        let (used, limit) = quota;
        // i64 → u64: used is SUM of non-negative nar_size, so ≥ 0.
        // limit is operator-set (CreateTenant validates range), so ≥ 0.
        // Both casts are safe; clamp defensively anyway — a negative
        // value here would mean PG corruption, and sending u64::MAX
        // on that path is better than a silent wrap.
        Ok(Response::new(TenantQuotaResponse {
            used_bytes: used.max(0) as u64,
            limit_bytes: limit.map(|l| l.max(0) as u64),
        }))
    }
}

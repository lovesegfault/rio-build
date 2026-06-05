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

use super::sign::{self, PathVisible};
use super::{
    EndUserRejected, ServiceCallerOk, StoreServiceImpl, metadata_status, validate_store_path,
};

/// The narinfo reply step: a [`PathVisible`] witness is REQUIRED to
/// surface path metadata to the caller — a read arm that skips the
/// sig-visibility gate (or the substitution mint) does not compile.
fn visible_narinfo(_vis: PathVisible, info: rio_proto::validated::ValidatedPathInfo) -> PathInfo {
    info.into()
}

impl StoreServiceImpl {
    /// The AddSignatures write step: gate witness required — appending
    /// sigs to a gate-hidden path (existence probe / junk-sig DoS) is
    /// not expressible.
    async fn append_signatures_visible(
        &self,
        _vis: PathVisible,
        store_path: &str,
        signatures: &[String],
    ) -> Result<u64, Status> {
        metadata::append_signatures(&self.pool, store_path, signatures)
            .await
            .map_err(|e| metadata_status("AddSignatures: append_signatures", e))
    }

    /// `BatchQueryPathInfo`'s data fetch: the [`EndUserRejected`]
    /// witness is REQUIRED — the deliberate sig-visibility gate-skip
    /// on this builder-internal RPC is safe only because end-user
    /// tenants are rejected first, and this signature makes deleting
    /// that check a compile error.
    async fn builder_internal_path_info_batch(
        &self,
        _builder_internal: EndUserRejected,
        store_paths: &[String],
    ) -> Result<Vec<(String, Option<rio_proto::validated::ValidatedPathInfo>)>, Status> {
        metadata::query_path_info_batch(&self.pool, store_paths)
            .await
            .map_err(|e| metadata_status("BatchQueryPathInfo: query_path_info_batch", e))
    }

    /// `BatchGetManifest`'s data fetch — same witness requirement as
    /// [`Self::builder_internal_path_info_batch`].
    async fn builder_internal_manifest_batch(
        &self,
        _builder_internal: EndUserRejected,
        store_paths: &[String],
    ) -> Result<
        Vec<(
            String,
            Option<(rio_proto::validated::ValidatedPathInfo, ManifestKind)>,
        )>,
        Status,
    > {
        metadata::get_manifest_batch(&self.pool, store_paths)
            .await
            .map_err(|e| metadata_status("BatchGetManifest: get_manifest_batch", e))
    }

    /// The realisation write step: a [`ServiceCallerOk`] witness is
    /// REQUIRED — realisations are scheduler-managed, and a write arm
    /// that skips the service-caller gate does not compile.
    async fn insert_realisation_as_service(
        &self,
        _write_auth: ServiceCallerOk,
        r: &realisations::Realisation,
    ) -> Result<bool, crate::metadata::MetadataError> {
        realisations::insert(&self.pool, r).await
    }

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

        let (info, vis) = match local {
            Some(i) => {
                // r[impl store.substitute.tenant-sig-visibility+2]
                // Local hit — but is it visible to THIS tenant? A path
                // substituted by tenant A with sig_mode=keep is
                // invisible to tenant B unless B trusts A's upstream
                // key. Hide-as-NotFound on gate failure.
                match self.sig_visibility_gate(tenant_id, &i).await? {
                    Some(vis) => (i, vis),
                    None => {
                        // Gate failed — but B's upstreams may ALSO have
                        // this path. Try substituting (which will
                        // append B-trusted sigs to the existing row).
                        if let Some(sub) = self
                            .try_substitute_on_miss(tenant_id, &req.store_path)
                            .await?
                        {
                            (sub, sign::PathVisible::substituted_for_tenant())
                        } else {
                            return Err(Status::not_found(format!(
                                "path not found: {}",
                                req.store_path
                            )));
                        }
                    }
                }
            }
            None => {
                // Local miss — try upstream substitution.
                let sub = self
                    .try_substitute_on_miss(tenant_id, &req.store_path)
                    .await?
                    .ok_or_else(|| {
                        Status::not_found(format!("path not found: {}", req.store_path))
                    })?;
                (sub, sign::PathVisible::substituted_for_tenant())
            }
        };

        Ok(Response::new(visible_narinfo(vis, info)))
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
        let builder_internal = self.reject_end_user_tenant(&request, "BatchQueryPathInfo")?;
        let req = request.into_inner();

        self.validate_path_batch(&req.store_paths)?;

        let n_paths = req.store_paths.len();
        let start = std::time::Instant::now();
        let entries = self
            .builder_internal_path_info_batch(builder_internal, &req.store_paths)
            .await?
            .into_iter()
            .map(|(store_path, info)| PathInfoEntry {
                store_path,
                info: info.map(Into::into),
            })
            .collect();
        tracing::debug!(n_paths, elapsed = ?start.elapsed(), "BatchQueryPathInfo");

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
        let builder_internal = self.reject_end_user_tenant(&request, "BatchGetManifest")?;
        let req = request.into_inner();

        self.validate_path_batch(&req.store_paths)?;

        let entries = self
            .builder_internal_manifest_batch(builder_internal, &req.store_paths)
            .await?
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
        // r[impl sched.merge.substitute-probe-indeterminate+2]
        // `indeterminate` = paths the probe couldn't classify (429,
        // 5xx, deadline). Scheduler treats them optimistically; without
        // this field they were silently treated as confirmed-miss and
        // dispatched as builds even when in cache.nixos.org.
        let (substitutable, indeterminate) = match (&self.substituter, tenant_id) {
            (Some(sub), Some(tid)) if !missing.is_empty() => sub
                .check_available(
                    tid,
                    &missing,
                    entry + crate::substitute::CHECK_AVAILABLE_DEFAULT_BUDGET,
                )
                .await
                .map(|r| (r.hits, r.indeterminate))
                .unwrap_or_else(|e| {
                    warn!(error = %e, "check_available failed; reporting all missing as indeterminate");
                    (Vec::new(), missing.clone())
                }),
            _ => (Vec::new(), Vec::new()),
        };

        debug!(
            n_requested = req.store_paths.len(),
            n_missing = missing.len(),
            n_substitutable = substitutable.len(),
            n_indeterminate = indeterminate.len(),
            tenant_id = ?tenant_id,
            substituter = self.substituter.is_some(),
            "FindMissingPaths"
        );

        Ok(Response::new(FindMissingPathsResponse {
            missing_paths: missing,
            substitutable_paths: substitutable,
            indeterminate_paths: indeterminate,
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
        let Some(vis) = self.sig_visibility_gate(tenant_id, &info).await? else {
            return Err(Status::not_found(format!(
                "no path with hash part: {}",
                req.hash_part
            )));
        };

        Ok(Response::new(visible_narinfo(vis, info)))
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
        let Some(vis) = self.sig_visibility_gate(tenant_id, &info).await? else {
            return Err(Status::not_found(format!(
                "path not found: {}",
                req.store_path
            )));
        };

        // Empty sigs list: no-op. Don't hit PG for nothing. Not an error —
        // `nix store sign` with no configured keys can legitimately produce
        // this (it sends the opcode but with zero sigs).
        if req.signatures.is_empty() {
            return Ok(Response::new(AddSignaturesResponse {}));
        }

        let rows = self
            .append_signatures_visible(vis, &req.store_path, &req.signatures)
            .await?;

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
        // Witness-producing gate: dev-mode pass-through
        // (`service_verifier=None`) matches the `hmac_verifier=None`
        // semantics elsewhere — production always configures both.
        let write_auth = self.require_service_caller(
            &request,
            "RegisterRealisation requires a service token; \
             realisations are scheduler-managed",
        )?;
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
        // insert_realisation_batch path signs (store.typ — signed
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

        self.insert_realisation_as_service(write_auth, &r)
            .await
            .map_err(|e| {
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
        // Caller identity BEFORE into_inner consumes the extensions.
        // The layer's TenantJwt class guarantees claims are present
        // whenever a JWT pubkey is configured; None here = dual-mode.
        let caller = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub);
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

        // Handler ownership (merged_bug_122): the named tenant must BE
        // the verified caller. The mismatch deny is byte-identical to
        // the no-such-tenant deny (absence-shaped), so a foreign
        // tenant name's existence cannot be probed through this RPC.
        // No-claims callers (dual-mode/dev, or scheduler probes that
        // never reach here) pass through unchanged.
        let owner: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT tenant_id FROM tenants WHERE tenant_name = $1")
                .bind(name.as_str())
                .fetch_optional(&self.pool)
                .await
                .status_internal("TenantQuota: tenant lookup")?;
        match (owner, caller) {
            (None, _) => {
                return Err(Status::not_found(format!("unknown tenant: {name}")));
            }
            (Some(owner_id), Some(sub)) if owner_id != sub => {
                return Err(Status::not_found(format!("unknown tenant: {name}")));
            }
            _ => {}
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::seed_tenant;
    use rio_test_support::TestDb;

    fn claims(sub: uuid::Uuid) -> rio_auth::jwt::TenantClaims {
        rio_auth::jwt::TenantClaims {
            sub,
            iat: 0,
            exp: i64::MAX,
            jti: String::from("quota-test-jti"),
        }
    }

    fn quota_req(name: &str, with_claims: Option<uuid::Uuid>) -> Request<TenantQuotaRequest> {
        let mut r = Request::new(TenantQuotaRequest {
            tenant_name: name.to_string(),
        });
        if let Some(sub) = with_claims {
            r.extensions_mut().insert(claims(sub));
        }
        r
    }

    /// 122's handler red (recorded pre-fix): tenant B's verified
    /// claims naming tenant A returned A's QUOTA (cross-tenant read).
    /// Post-fix: claims.sub must match the named tenant's row;
    /// mismatch is the ABSENCE-shaped `unknown tenant: {name}` —
    /// byte-identical to the no-such-tenant deny, so existence of a
    /// foreign tenant name cannot be probed (enumeration oracle dead).
    /// No-claims callers pass through (dual-mode preserved).
    // r[verify store.log.method-credential+2]
    #[tokio::test]
    async fn tenant_quota_foreign_claims_get_absence_shaped_deny() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = StoreServiceImpl::new(db.pool.clone());
        let tid_a = seed_tenant(&db.pool, "quota-owner").await;
        let tid_b = seed_tenant(&db.pool, "quota-foreign").await;

        // Foreign claims: B asks for A's quota -> absence-shaped deny.
        let err = svc
            .tenant_quota_impl(quota_req("quota-owner", Some(tid_b)))
            .await
            .expect_err("foreign claims must not read another tenant's quota");
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert_eq!(err.message(), "unknown tenant: quota-owner");

        // The deny is byte-identical to the genuinely-absent deny.
        let absent = svc
            .tenant_quota_impl(quota_req("quota-no-such", Some(tid_b)))
            .await
            .expect_err("absent tenant is NotFound");
        assert_eq!(absent.code(), tonic::Code::NotFound);
        assert_eq!(absent.message(), "unknown tenant: quota-no-such");

        // Own claims pass.
        svc.tenant_quota_impl(quota_req("quota-owner", Some(tid_a)))
            .await
            .expect("own claims read own quota");

        // No claims (dual-mode/dev) pass through unchanged.
        svc.tenant_quota_impl(quota_req("quota-owner", None))
            .await
            .expect("anonymous dual-mode passthrough preserved");
    }
}

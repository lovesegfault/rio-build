//! Read-side StoreService RPCs (QueryPathInfo, FindMissingPaths,
//! AddSignatures, realisations, TenantQuota). Inherent methods on
//! [`StoreServiceImpl`]; the `StoreService` trait impl in `mod.rs`
//! delegates here so the trait body stays a flat list of one-liners.

use super::*;

impl StoreServiceImpl {
    /// Query metadata for a single store path.
    ///
    /// Only returns paths with manifests.status='complete'.
    pub(super) async fn query_path_info_impl(
        &self,
        request: Request<QueryPathInfoRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant_id = Self::request_tenant_id(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        let local = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("QueryPathInfo: query_path_info", e))?;

        let info = match local {
            Some(i) => {
                // r[impl store.substitute.tenant-sig-visibility]
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
    /// `query_path_info`. The builder (the only current caller) sends
    /// no tenant token, so neither would apply anyway.
    pub(super) async fn batch_query_path_info_impl(
        &self,
        request: Request<BatchQueryPathInfoRequest>,
    ) -> Result<Response<BatchQueryPathInfoResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Same DoS bound as FindMissingPaths.
        if req.store_paths.len() > self.max_batch_paths {
            return Err(Status::invalid_argument(format!(
                "too many paths: {} (max {}; raise RIO_MAX_BATCH_PATHS to allow larger batches)",
                req.store_paths.len(),
                self.max_batch_paths
            )));
        }
        for p in &req.store_paths {
            validate_store_path(p)?;
        }

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
    /// sig-visibility gate).
    pub(super) async fn batch_get_manifest_impl(
        &self,
        request: Request<BatchGetManifestRequest>,
    ) -> Result<Response<BatchGetManifestResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Same DoS bound as BatchQueryPathInfo / FindMissingPaths.
        if req.store_paths.len() > self.max_batch_paths {
            return Err(Status::invalid_argument(format!(
                "too many paths: {} (max {}; raise RIO_MAX_BATCH_PATHS to allow larger batches)",
                req.store_paths.len(),
                self.max_batch_paths
            )));
        }
        for p in &req.store_paths {
            validate_store_path(p)?;
        }

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
        let tenant_id = Self::request_tenant_id(&request);
        let req = request.into_inner();

        // Bound request size to prevent DoS via huge path lists.
        // Inline (not check_bound) so the message names the env var.
        if req.store_paths.len() > self.max_batch_paths {
            return Err(Status::invalid_argument(format!(
                "too many paths: {} (max {}; raise RIO_MAX_BATCH_PATHS to allow larger batches)",
                req.store_paths.len(),
                self.max_batch_paths
            )));
        }
        // Validate each path format. Reject the whole batch on any malformed
        // path (client bug indicator).
        for p in &req.store_paths {
            validate_store_path(p)?;
        }

        let missing = metadata::find_missing_paths(&self.pool, &req.store_paths)
            .await
            .map_err(|e| metadata_status("FindMissingPaths: find_missing_paths", e))?;

        // HEAD-probe each missing path against the tenant's upstreams.
        // Fails-open on probe errors (a down upstream shouldn't hide
        // paths the scheduler can otherwise substitute). Empty if no
        // substituter / no tenant / no upstreams — the normal case.
        let substitutable = match (&self.substituter, tenant_id) {
            (Some(sub), Some(tid)) if !missing.is_empty() => sub
                .check_available(tid, &missing)
                .await
                .unwrap_or_else(|e| {
                    warn!(error = %e, "check_available failed; empty substitutable_paths");
                    Vec::new()
                }),
            _ => Vec::new(),
        };

        Ok(Response::new(FindMissingPathsResponse {
            missing_paths: missing,
            substitutable_paths: substitutable,
        }))
    }

    /// Resolve a store path from its 32-char nixbase32 hash part.
    pub(super) async fn query_path_from_hash_part_impl(
        &self,
        request: Request<QueryPathFromHashPartRequest>,
    ) -> Result<Response<PathInfo>, Status> {
        rio_proto::interceptor::link_parent(&request);
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

        Ok(Response::new(info.into()))
    }

    /// Append signatures to an existing store path.
    pub(super) async fn add_signatures_impl(
        &self,
        request: Request<AddSignaturesRequest>,
    ) -> Result<Response<AddSignaturesResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        // Bound the signatures list — a malicious client could send 1M sigs
        // and we'd append them all. MAX_SIGNATURES matches PutPath's bound.
        rio_common::grpc::check_bound(
            "signatures",
            req.signatures.len(),
            rio_common::limits::MAX_SIGNATURES,
        )?;

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
    pub(super) async fn register_realisation_impl(
        &self,
        request: Request<RegisterRealisationRequest>,
    ) -> Result<Response<RegisterRealisationResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
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

        let r = realisations::Realisation {
            drv_hash,
            output_name: proto.output_name,
            output_path: proto.output_path,
            output_hash,
            signatures: proto.signatures,
        };

        realisations::insert(&self.pool, &r)
            .await
            .map_err(|e| metadata_status("RegisterRealisation: insert", e))?;

        Ok(Response::new(RegisterRealisationResponse {}))
    }

    /// Look up a CA derivation realisation.
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

//! PutPath: upload a store path (streaming NAR data with metadata).
//!
//! Write-ahead flow:
//! 1. Receive first message: PutPathMetadata with PathInfo
//! 2. Check idempotency: if path already complete, return success
// r[impl store.put.wal-manifest]
// r[impl store.put.idempotent]
// r[impl store.integrity.verify-on-put]
//! 3. Insert manifest placeholder with status='uploading'
//! 4. Accumulate NAR chunks (bounded by MAX_NAR_SIZE)
//! 5. Verify SHA-256 matches trailer's declared nar_hash
//! 6. Store NAR (inline or chunked) + flip status to 'complete'
//!
//! Trailer-only: metadata.nar_hash MUST be empty; hash arrives in the
//! PutPathTrailer after all chunks (stream-and-hash — the client doesn't
//! know the hash until after transmitting all bytes).

use super::*;

/// Drain remaining messages from a streaming request.
///
/// Must be called before returning early from PutPath to avoid leaving
/// unconsumed data on the gRPC transport. Bounded by DEFAULT_GRPC_TIMEOUT
/// to prevent a slow client from holding the handler indefinitely.
async fn drain_stream(stream: &mut Streaming<PutPathRequest>) {
    let drain = async {
        while let Ok(Some(_)) = stream.message().await {
            // discard
        }
    };
    if tokio::time::timeout(rio_common::grpc::DEFAULT_GRPC_TIMEOUT, drain)
        .await
        .is_err()
    {
        warn!(
            timeout = ?rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            "drain_stream timed out; client may be sending slowly"
        );
    }
}

/// Extract the subject CN from a DER-encoded X509 certificate.
/// Returns None if parse fails or no CN found.
fn cert_cn(der: &[u8]) -> Option<String> {
    use x509_parser::prelude::*;
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    cert.subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(String::from)
}

impl StoreServiceImpl {
    /// Verify the `x-rio-assignment-token` metadata header.
    ///
    /// Returns:
    /// - `Ok(None)` if verifier disabled (dev mode) → no check
    /// - `Ok(None)` if mTLS bypass (cert CN = "rio-gateway") → no check
    /// - `Ok(Some(claims))` if token valid → caller checks path ∈ claims
    /// - `Err(PERMISSION_DENIED)` if token missing/invalid/expired
    ///
    /// mTLS bypass: `request.peer_certs()` returns the client's TLS
    /// cert chain (only set when ServerTlsConfig has client_ca_root).
    /// If the FIRST cert's subject CN is "rio-gateway" → bypass.
    /// This means: to upload without a token, you need a CA-signed
    /// cert with CN=rio-gateway. mTLS + HMAC together = defense in
    /// depth; a compromised worker cert (CN=rio-worker) does NOT
    /// bypass — it must present a valid token restricting it to the
    /// scheduler-assigned output paths.
    fn verify_assignment_token<T>(
        &self,
        request: &Request<T>,
    ) -> Result<Option<rio_common::hmac::Claims>, Status> {
        let Some(verifier) = &self.hmac_verifier else {
            // Verifier not configured = dev mode, accept all.
            return Ok(None);
        };

        // mTLS bypass: parse first peer cert's CN. Only bypass
        // for CN=rio-gateway. Previously: ANY peer cert + no token
        // → bypass. That defeated the entire HMAC threat model: a
        // compromised worker omits the token → uploads arbitrary
        // paths → backdoored libc injection.
        let peer_cn = request
            .peer_certs()
            .and_then(|certs| certs.first().and_then(|c| cert_cn(c.as_ref())));

        let token = request
            .metadata()
            .get("x-rio-assignment-token")
            .and_then(|v| v.to_str().ok());

        match token {
            Some(t) => {
                // Token present: verify regardless of peer certs.
                // A worker with mTLS still gets its token checked
                // (defense in depth — compromised worker certs
                // don't bypass the path restriction).
                verifier.verify(t).map(Some).map_err(|e| {
                    warn!(error = %e, "PutPath: assignment token verification failed");
                    metrics::counter!("rio_store_hmac_rejected_total", "reason" => "invalid_token")
                        .increment(1);
                    Status::permission_denied(format!("assignment token: {e}"))
                })
            }
            None => {
                match peer_cn.as_deref() {
                    Some("rio-gateway") => {
                        // Gateway-specific bypass. Track for visibility.
                        debug!("PutPath: CN=rio-gateway, bypassing HMAC");
                        metrics::counter!("rio_store_hmac_bypass_total", "cn" => "rio-gateway")
                            .increment(1);
                        Ok(None)
                    }
                    Some(other_cn) => {
                        // mTLS client with NON-gateway CN (worker,
                        // controller) and no token → REJECT. This is
                        // the threat model: compromised worker
                        // skipping its token to upload arbitrary paths.
                        warn!(cn = %other_cn,
                              "PutPath: mTLS client with non-gateway CN and no token, rejecting");
                        metrics::counter!("rio_store_hmac_rejected_total",
                                         "reason" => "non_gateway_cn_no_token")
                        .increment(1);
                        Err(Status::permission_denied(format!(
                            "assignment token required (CN={other_cn} is not rio-gateway)"
                        )))
                    }
                    None => {
                        // No mTLS (or cert parse failed), no token,
                        // verifier enabled → reject.
                        metrics::counter!("rio_store_hmac_rejected_total",
                                         "reason" => "missing_token")
                        .increment(1);
                        Err(Status::permission_denied(
                            "assignment token required (x-rio-assignment-token header)",
                        ))
                    }
                }
            }
        }
    }

    pub(super) async fn put_path_impl(
        &self,
        request: Request<Streaming<PutPathRequest>>,
    ) -> Result<Response<PutPathResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let start = std::time::Instant::now();
        // Record duration on ANY exit (success, error, early return)
        // via scopeguard on Drop. Recording only on the success path
        // would make p99 latency metrics artificially good (failures
        // not counted).
        let _duration_guard = scopeguard::guard((), move |()| {
            metrics::histogram!("rio_store_put_path_duration_seconds")
                .record(start.elapsed().as_secs_f64());
        });

        // r[impl sec.boundary.grpc-hmac]
        // HMAC token check BEFORE into_inner (need metadata access).
        // If verifier=None (dev mode): no-op, claims stays None.
        // If verifier=Some: token required + valid → claims Some,
        // OR mTLS bypass (gateway cert) → claims None (no path
        // restriction for gateway uploads).
        let hmac_claims = self.verify_assignment_token(&request)?;

        let mut stream = request.into_inner();

        // Step 1: Receive the first message (must be metadata)
        let first_msg = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty PutPath stream"))?;

        let raw_info = match first_msg.msg {
            Some(put_path_request::Msg::Metadata(meta)) => meta
                .info
                .ok_or_else(|| Status::invalid_argument("PutPathMetadata missing PathInfo"))?,
            Some(put_path_request::Msg::NarChunk(_)) => {
                return Err(Status::invalid_argument(
                    "first PutPath message must be metadata, not nar_chunk",
                ));
            }
            Some(put_path_request::Msg::Trailer(_)) => {
                return Err(Status::invalid_argument(
                    "first PutPath message must be metadata, not trailer",
                ));
            }
            None => {
                return Err(Status::invalid_argument("PutPath message has no content"));
            }
        };

        // Trailer-mode is the ONLY mode: metadata.nar_hash must be empty,
        // hash arrives in the PutPathTrailer after all chunks. Both the
        // gateway (chunk_nar_for_put) and worker (single-pass tee upload)
        // send trailers. Hash-upfront was deleted in the pre-phase3a
        // cleanup — a non-empty nar_hash means an un-updated client.
        //
        // ValidatedPathInfo::try_from hard-fails on empty nar_hash, so we
        // fill a placeholder, validate, then overwrite after the trailer
        // arrives. The server computes the digest either way
        // (NarDigest::from_bytes below), so the security property
        // (client-declared hash matches server-computed) is unchanged.
        if !raw_info.nar_hash.is_empty() {
            return Err(Status::invalid_argument(
                "PutPath metadata.nar_hash must be empty (hash-upfront mode removed; \
                 send hash in PutPathTrailer)",
            ));
        }
        let mut raw_info = raw_info;
        // Placeholder so TryFrom passes. Overwritten after trailer.
        // 32 zero bytes — unambiguously NOT a real SHA-256 (would be
        // the hash of a specific ~2^256-rare preimage).
        raw_info.nar_hash = vec![0u8; 32];
        // nar_size is also 0 here — real value arrives in trailer.

        // Bound repeated fields BEFORE validation (TryFrom validates each
        // reference's syntax but doesn't bound the count; an attacker could
        // send 10M valid references and we'd parse them all before failing).
        rio_common::grpc::check_bound(
            "references",
            raw_info.references.len(),
            rio_common::limits::MAX_REFERENCES,
        )?;
        rio_common::grpc::check_bound(
            "signatures",
            raw_info.signatures.len(),
            rio_common::limits::MAX_SIGNATURES,
        )?;

        // Centralized validation: store_path parses, nar_hash is 32 bytes
        // (placeholder), each reference parses.
        let mut info = ValidatedPathInfo::try_from(raw_info)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;

        // HMAC claims check: uploaded path must be in expected_outputs.
        // None = verifier disabled OR mTLS bypass (gateway) → no check.
        if let Some(claims) = &hmac_claims {
            let path_str = info.store_path.as_str();
            if !claims.expected_outputs.iter().any(|o| o == path_str) {
                warn!(
                    store_path = %path_str,
                    worker_id = %claims.worker_id,
                    drv_hash = %claims.drv_hash,
                    "PutPath: path not in assignment's expected_outputs"
                );
                metrics::counter!("rio_store_hmac_rejected_total", "reason" => "path_not_in_claims")
                    .increment(1);
                return Err(Status::permission_denied(
                    "path not authorized by assignment token",
                ));
            }
        }

        // Compute store_path_hash if not provided
        let store_path_hash = if info.store_path_hash.is_empty() {
            info.store_path.sha256_digest().to_vec()
        } else {
            info.store_path_hash.clone()
        };
        debug!(
            store_path = %info.store_path.as_str(),
            "PutPath: received metadata"
        );

        // Step 2: Check idempotency — if path already complete, return success
        match metadata::check_manifest_complete(&self.pool, &store_path_hash).await {
            Ok(true) => {
                debug!(store_path = %info.store_path.as_str(), "PutPath: path already complete, returning success");
                // Drain remaining stream messages (protocol contract)
                drain_stream(&mut stream).await;
                metrics::counter!("rio_store_put_path_total", "result" => "exists").increment(1);
                return Ok(Response::new(PutPathResponse { created: false }));
            }
            Ok(false) => {} // Not yet complete, proceed
            Err(e) => {
                // Drain remaining stream messages before returning error
                drain_stream(&mut stream).await;
                return Err(internal_error("PutPath: check_manifest_complete", e));
            }
        }

        // Step 3a: Take GC_MARK_LOCK_ID SHARED (mark-vs-PutPath protection).
        // Held from placeholder insert → complete_manifest (or abort).
        // Mark takes this EXCLUSIVE around compute_unreachable, so
        // mark sees a consistent reference graph: no PutPath can be
        // mid-write (placeholder exists, references='{}') while mark
        // runs. Shared-shared is compatible — multiple PutPaths don't
        // block each other.
        //
        // scopeguard: on ANY exit (error, cancel, early return), the
        // connection is detached (not returned to pool). PG auto-
        // releases session-scoped locks on connection close. On
        // success, we defuse (ScopeGuard::into_inner) and explicitly
        // unlock + return the conn to the pool.
        let mark_lock_conn = match self.pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                drain_stream(&mut stream).await;
                return Err(internal_error("PutPath: mark lock acquire", e));
            }
        };
        let mut mark_lock_guard = scopeguard::guard(mark_lock_conn, |c| {
            let _ = c.detach();
        });
        if let Err(e) = sqlx::query("SELECT pg_advisory_lock_shared($1)")
            .bind(crate::gc::GC_MARK_LOCK_ID)
            .execute(&mut **mark_lock_guard)
            .await
        {
            drain_stream(&mut stream).await;
            return Err(internal_error("PutPath: mark lock shared", e));
        }

        // Step 3b: Insert manifest placeholder with status='uploading'.
        // Returns false (ON CONFLICT DO NOTHING no-op) if another uploader
        // already holds a placeholder. In that case we must NOT proceed: if
        // we do and later fail validation, delete_manifest_uploading would
        // delete the OTHER uploader's placeholder, losing their valid upload.
        let inserted = match metadata::insert_manifest_uploading(
            &self.pool,
            &store_path_hash,
            &info.store_path,
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                drain_stream(&mut stream).await;
                return Err(metadata_status("PutPath: insert_manifest_uploading", e));
            }
        };
        if !inserted {
            // Another upload is in progress (or just completed in the window
            // between check_manifest_complete above and now). Re-check: if
            // it flipped to complete, return success; else tell client retry.
            drain_stream(&mut stream).await;
            match metadata::check_manifest_complete(&self.pool, &store_path_hash).await {
                Ok(true) => {
                    debug!(store_path = %info.store_path, "PutPath: concurrent upload won the race");
                    metrics::counter!("rio_store_put_path_total", "result" => "exists")
                        .increment(1);
                    return Ok(Response::new(PutPathResponse { created: false }));
                }
                _ => {
                    debug!(store_path = %info.store_path, "PutPath: concurrent upload in progress, aborting");
                    return Err(Status::aborted(
                        "concurrent PutPath in progress for this path; retry",
                    ));
                }
            }
        }

        // Step 4: Accumulate NAR chunks into a buffer.
        // Bound accumulation to prevent a malicious/buggy client OOMing us.
        // nar_size arrives in the trailer, so bound by MAX_NAR_SIZE during
        // accumulation; the trailer value is checked after.
        let mut nar_data = Vec::new();
        let mut trailer: Option<rio_proto::types::PutPathTrailer> = None;
        loop {
            let msg = match stream.message().await {
                Ok(Some(m)) => m,
                Ok(None) => break, // stream closed
                Err(e) => {
                    warn!(store_path = %info.store_path, error = %e, "PutPath: stream read error");
                    self.abort_upload(&store_path_hash).await;
                    return Err(e);
                }
            };
            match msg.msg {
                Some(put_path_request::Msg::NarChunk(chunk)) => {
                    // Chunk after trailer is a protocol violation — trailer
                    // MUST be last. Catches buggy clients that keep streaming
                    // after finalize() (would corrupt the hash validation).
                    if trailer.is_some() {
                        self.abort_upload(&store_path_hash).await;
                        return Err(Status::invalid_argument(
                            "PutPath: nar_chunk after trailer (trailer must be last)",
                        ));
                    }
                    let new_len = (nar_data.len() as u64).saturating_add(chunk.len() as u64);
                    if new_len > MAX_NAR_SIZE {
                        warn!(
                            store_path = %info.store_path,
                            received = new_len,
                            "PutPath: NAR chunks exceed size bound, rejecting"
                        );
                        self.abort_upload(&store_path_hash).await;
                        return Err(Status::invalid_argument(format!(
                            "NAR chunks exceed size bound {MAX_NAR_SIZE} (received {new_len}+ bytes)",
                        )));
                    }
                    nar_data.extend_from_slice(&chunk);
                }
                Some(put_path_request::Msg::Trailer(t)) => {
                    if trailer.is_some() {
                        self.abort_upload(&store_path_hash).await;
                        return Err(Status::invalid_argument("PutPath: duplicate trailer"));
                    }
                    trailer = Some(t);
                    // Don't break — keep reading to catch chunk-after-trailer.
                    // A well-behaved client closes the stream right after
                    // sending the trailer, so this is one extra recv()
                    // that immediately returns None.
                }
                Some(put_path_request::Msg::Metadata(_)) => {
                    // Protocol violation: metadata must be first-message-only.
                    // A buggy client sending duplicate metadata with different
                    // nar_hash would have its "correction" silently ignored.
                    warn!(
                        store_path = %info.store_path,
                        "PutPath: duplicate metadata mid-stream, rejecting"
                    );
                    self.abort_upload(&store_path_hash).await;
                    return Err(Status::invalid_argument(
                        "PutPath stream contained duplicate metadata (protocol violation)",
                    ));
                }
                None => {
                    // Empty message, skip
                }
            }
        }

        // Trailer resolution: overwrite the placeholder hash/size with the
        // trailer values. Trailer is MANDATORY — stream close without one
        // is a protocol violation.
        let Some(t) = trailer else {
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(
                "PutPath: no trailer received \
                 (PutPathTrailer is required as the last message)",
            ));
        };
        let Ok(hash): Result<[u8; 32], _> = t.nar_hash.as_slice().try_into() else {
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(format!(
                "PutPath trailer nar_hash must be 32 bytes (SHA-256), got {}",
                t.nar_hash.len()
            )));
        };
        if t.nar_size > MAX_NAR_SIZE {
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(format!(
                "PutPath trailer nar_size {} exceeds maximum {MAX_NAR_SIZE}",
                t.nar_size
            )));
        }
        info.nar_hash = hash;
        info.nar_size = t.nar_size;

        // Step 5: Verify SHA-256. The NAR is already fully buffered in memory,
        // so use from_bytes (single pass over the slice) instead of wrapping in
        // a HashingReader + read_to_end into a second Vec (peak ~8 GiB for a
        // 4 GiB NAR).
        let digest = crate::validate::NarDigest::from_bytes(&nar_data);

        if let Err(e) = validate_nar_digest(&digest, &info.nar_hash, info.nar_size) {
            warn!(
                store_path = %info.store_path,
                error = %e,
                "PutPath: NAR validation failed"
            );
            self.abort_upload(&store_path_hash).await;
            return Err(Status::invalid_argument(format!(
                "NAR validation failed: {e}"
            )));
        }

        // Step 6: Complete upload. Branches on size.
        let mut full_info = ValidatedPathInfo {
            store_path_hash,
            ..info
        };

        // Sign BEFORE writing narinfo. The signature goes into PG
        // alongside the other narinfo fields; the HTTP cache server
        // serves it from there without touching the privkey. Signing
        // now (not at serve time) means key rotation doesn't re-sign
        // old paths — they keep their old-key sig, which stays valid
        // as long as the old pubkey is in trusted-public-keys.
        self.maybe_sign(&mut full_info);

        // Size gate: small NARs inline, large NARs chunked (if backend
        // configured). `None` backend forces inline always — test
        // harnesses rely on this.
        //
        // The gate is on `nar_data.len()`, not `info.nar_size`. They should
        // match (validate_nar_digest above checks) but nar_data.len() is
        // what we actually have in hand — no chance of a drift where
        // info.nar_size says 200KB but we chunk 300KB.
        let use_chunked = self.chunk_backend.is_some() && nar_data.len() >= cas::INLINE_THRESHOLD;

        if use_chunked {
            // Chunked path: FastCDC + S3 + refcounts. cas::put_chunked
            // handles the whole write-ahead flow INCLUDING rollback on
            // its own errors. It consumed our step-3 placeholder; we
            // don't call abort_upload() on failure (that'd delete
            // a placeholder that no longer exists, or worse, one that
            // put_chunked's rollback just cleaned up).
            let backend = self.chunk_backend.as_ref().expect("checked is_some above");
            match cas::put_chunked(&self.pool, backend, &full_info, &nar_data).await {
                Ok(stats) => {
                    debug!(
                        store_path = %full_info.store_path.as_str(),
                        total_chunks = stats.total_chunks,
                        deduped = stats.deduped_chunks,
                        ratio = stats.dedup_ratio(),
                        "PutPath: chunked upload completed"
                    );
                    // The milestone metric. Gauge (per-upload ratio, not a
                    // running average — Prometheus rate() handles that).
                    metrics::gauge!("rio_store_chunk_dedup_ratio").set(stats.dedup_ratio());
                }
                Err(e) => {
                    // put_chunked already rolled back. Just the error metric.
                    metrics::counter!("rio_store_put_path_total", "result" => "error").increment(1);
                    return Err(internal_error("PutPath: put_chunked", e));
                }
            }
        } else {
            // Inline path: single-tx NAR-in-inline_blob. Atomic — no
            // orphan cleanup needed (unlike the chunked path's two-step).
            if let Err(e) =
                metadata::complete_manifest_inline(&self.pool, &full_info, Bytes::from(nar_data))
                    .await
            {
                self.abort_upload(&full_info.store_path_hash).await;
                return Err(metadata_status("PutPath: complete_manifest_inline", e));
            }
            debug!(store_path = %full_info.store_path.as_str(), "PutPath: inline upload completed");
        }

        // Content-index the upload. nar_hash IS the content identity for
        // CA purposes: same bytes → same SHA-256. Two input-addressed
        // builds producing identical output get the same content_hash
        // entry, pointing at different store_paths (both rows kept, PK
        // is the pair). Best-effort — failure here doesn't fail the
        // upload (path is still addressable by store_path); log only.
        // Done AFTER the manifest commits so lookup's INNER JOIN on
        // manifests.status='complete' always sees a complete row.
        if let Err(e) = crate::content_index::insert(
            &self.pool,
            &full_info.nar_hash,
            &full_info.store_path_hash,
        )
        .await
        {
            tracing::warn!(
                store_path = %full_info.store_path.as_str(),
                error = %e,
                "content_index insert failed (path still addressable by store_path)"
            );
        }

        // Upload complete — release mark lock (shared) and return
        // connection to pool. Defuse the scopeguard (we don't want
        // detach on the happy path — that'd leak a pool slot).
        let mut mark_lock_conn = scopeguard::ScopeGuard::into_inner(mark_lock_guard);
        if let Err(e) = sqlx::query("SELECT pg_advisory_unlock_shared($1)")
            .bind(crate::gc::GC_MARK_LOCK_ID)
            .execute(&mut *mark_lock_conn)
            .await
        {
            // Non-fatal: upload succeeded, lock will release when
            // conn recycles. Log so we notice if this is frequent.
            warn!(error = %e, "PutPath: mark lock shared unlock failed");
        }
        drop(mark_lock_conn);

        metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
        // Duration recorded by _duration_guard on Drop.
        Ok(Response::new(PutPathResponse { created: true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CN parsing: verify cert_cn extracts the CN correctly.
    /// Uses rcgen to build an in-memory cert (test-only dep).
    fn make_cert_with_cn(cn: &str) -> Vec<u8> {
        let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
        params.distinguished_name = {
            let mut dn = rcgen::DistinguishedName::new();
            dn.push(rcgen::DnType::CommonName, cn);
            dn
        };
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        cert.der().to_vec()
    }

    #[test]
    fn cert_cn_parses_gateway() {
        let der = make_cert_with_cn("rio-gateway");
        assert_eq!(cert_cn(&der), Some("rio-gateway".into()));
    }

    #[test]
    fn cert_cn_parses_worker() {
        let der = make_cert_with_cn("rio-worker");
        // CN=rio-worker → returned as-is. verify_assignment_token
        // will reject this in the None-token path (non-gateway CN with no token → PERMISSION_DENIED).
        assert_eq!(cert_cn(&der), Some("rio-worker".into()));
    }

    #[test]
    fn cert_cn_no_cn_returns_none() {
        // A cert with no CN (only SANs, which is actually the
        // modern best practice) → cert_cn returns None. The
        // verify_assignment_token None-token+None-CN path rejects
        // (no bypass for unidentified clients).
        //
        // rcgen defaults a CN="rcgen self signed cert" if not
        // explicitly overridden — set an empty DN to simulate
        // a SAN-only cert.
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.distinguished_name = rcgen::DistinguishedName::new();
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        let der = cert.der();

        assert_eq!(cert_cn(der), None, "cert without CN → None");
    }

    #[test]
    fn cert_cn_garbage_der_returns_none() {
        assert_eq!(cert_cn(b"not a cert"), None);
    }
}

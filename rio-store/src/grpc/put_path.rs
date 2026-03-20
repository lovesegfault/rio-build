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
/// Returns None if parse fails or no CN found. Kept for logging
/// (the rejected-cert path wants to name WHICH CN was rejected);
/// authorization goes through `cert_identity_in_allowlist` below.
fn cert_cn(der: &[u8]) -> Option<String> {
    use x509_parser::prelude::*;
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    cert.subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .map(String::from)
}

// r[impl store.hmac.san-bypass]
/// Check whether a DER-encoded X509 certificate's identity is in the
/// allowlist. Identity = subject CN OR any SAN DNSName entry — either
/// match grants bypass.
///
/// CN-first, SAN-second ordering is an optimization only (CN is
/// cheaper to extract); security-wise they're equivalent since any
/// match wins.
///
/// SAN matching enables cert-manager-issued certificates that place
/// identity in SAN extensions and leave CN empty (RFC 6125 deprecates
/// CN for hostname verification; SAN is the modern path).
///
/// `GeneralName::DNSName` wraps `&str` already decoded from IA5String
/// by x509-parser — no manual decoding. Other GeneralName variants
/// (IPAddress, URI, RFC822Name) are ignored; we only allowlist DNS
/// names because that's what cert-manager emits and what the gateway
/// cert carries.
///
/// Returns false on parse failure (malformed DER) — fail closed.
fn cert_identity_in_allowlist(der: &[u8], allowlist: &[String]) -> bool {
    use x509_parser::extensions::GeneralName;
    use x509_parser::prelude::*;

    let Ok((_, cert)) = X509Certificate::from_der(der) else {
        return false;
    };

    // CN check
    if let Some(cn) = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|a| a.as_str().ok())
        && allowlist.iter().any(|a| a == cn)
    {
        return true;
    }

    // SAN DNSName check. subject_alternative_name() returns
    // Result<Option<BasicExtension<&SubjectAlternativeName>>> — Err
    // only on malformed extension (duplicate SAN ext, invalid IA5).
    // Treat Err same as "no SAN" (fail closed — a cert with a mangled
    // SAN doesn't get to bypass).
    if let Ok(Some(san)) = cert.tbs_certificate.subject_alternative_name() {
        for gn in &san.value.general_names {
            if let GeneralName::DNSName(dns) = gn
                && allowlist.iter().any(|a| a == dns)
            {
                return true;
            }
        }
    }

    false
}

impl StoreServiceImpl {
    /// Verify the `x-rio-assignment-token` metadata header.
    ///
    /// Returns:
    /// - `Ok(None)` if verifier disabled (dev mode) → no check
    /// - `Ok(None)` if mTLS bypass (cert CN/SAN in allowlist) → no check
    /// - `Ok(Some(claims))` if token valid → caller checks path ∈ claims
    /// - `Err(PERMISSION_DENIED)` if token missing/invalid/expired
    ///
    /// mTLS bypass: `request.peer_certs()` returns the client's TLS
    /// cert chain (only set when ServerTlsConfig has client_ca_root).
    /// If the FIRST cert's CN **or** any SAN DNSName is in
    /// `hmac_bypass_cns` → bypass. This means: to upload without a
    /// token, you need a CA-signed cert whose identity is explicitly
    /// allowlisted. mTLS + HMAC together = defense in depth; a
    /// compromised worker cert (CN=rio-worker, not in allowlist)
    /// does NOT bypass — it must present a valid token restricting
    /// it to the scheduler-assigned output paths.
    pub(super) fn verify_assignment_token<T>(
        &self,
        request: &Request<T>,
    ) -> Result<Option<rio_common::hmac::Claims>, Status> {
        let Some(verifier) = &self.hmac_verifier else {
            // Verifier not configured = dev mode, accept all.
            return Ok(None);
        };

        // mTLS bypass: check first peer cert's CN + SAN DNSNames against
        // the allowlist. Previously: ANY peer cert + no token → bypass.
        // That defeated the entire HMAC threat model: a compromised
        // worker omits the token → uploads arbitrary paths → backdoored
        // libc injection. Now: only allowlisted identities bypass.
        //
        // peer_cert_der held separately from peer_in_allowlist so the
        // rejection branch can log the CN (allowlist says no, but WHICH
        // identity was rejected is useful for debugging misissued certs).
        let peer_cert_der = request
            .peer_certs()
            .and_then(|certs| certs.first().map(|c| c.as_ref().to_vec()));
        let peer_in_allowlist = peer_cert_der
            .as_deref()
            .is_some_and(|der| cert_identity_in_allowlist(der, &self.hmac_bypass_cns));

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
            None if peer_in_allowlist => {
                // Allowlisted identity bypass. Track for visibility.
                // The metric label stays "cn" (not "identity") for
                // dashboard backward-compat — the value IS the CN
                // when present, else "san-only" when the cert has no
                // CN (cert-manager default). Cardinality bounded by
                // allowlist size (typically 1-3 entries).
                let label = peer_cert_der
                    .as_deref()
                    .and_then(cert_cn)
                    .unwrap_or_else(|| "san-only".to_string());
                debug!(identity = %label, "PutPath: cert identity in HMAC bypass allowlist");
                metrics::counter!("rio_store_hmac_bypass_total", "cn" => label).increment(1);
                Ok(None)
            }
            None => {
                match peer_cert_der.as_deref().and_then(cert_cn) {
                    Some(rejected_cn) => {
                        // mTLS client with NON-allowlisted identity
                        // (worker, controller) and no token → REJECT.
                        // This is the threat model: compromised worker
                        // skipping its token to upload arbitrary paths.
                        warn!(cn = %rejected_cn,
                              "PutPath: mTLS client identity not in bypass allowlist and no token, rejecting");
                        metrics::counter!("rio_store_hmac_rejected_total",
                                         "reason" => "non_gateway_cn_no_token")
                        .increment(1);
                        Err(Status::permission_denied(format!(
                            "assignment token required (CN={rejected_cn} not in bypass allowlist)"
                        )))
                    }
                    None => {
                        // No mTLS (or cert parse failed / no CN and
                        // SAN also didn't match), no token, verifier
                        // enabled → reject.
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

        // JWT tenant-id extraction — independent of HMAC claims above.
        //
        // The P0259 interceptor (r[gw.jwt.verify]) runs BEFORE this
        // handler and attaches `jwt::Claims` to request extensions on
        // successful verify of the `x-rio-tenant-token` header.
        //
        // Distinct Claims types — don't confuse them:
        //   - `hmac::Claims` (above): worker_id + drv_hash + expected_outputs.
        //     Restricts WHICH paths this worker may upload. Per-assignment.
        //   - `jwt::Claims` (here): sub (tenant UUID) + iat/exp/jti.
        //     Says WHOSE tenant key signs the narinfo. Per-session.
        //
        // `None` here covers three cases, all cluster-key-correct:
        //   - No interceptor wired (dev mode) → no Claims ever attached
        //   - Interceptor wired but no x-rio-tenant-token header → dual-mode
        //     fallback (gateway in SSH-comment mode, or worker/health caller)
        //   - mTLS bypass (gateway cert, `nix copy` path) — the gateway
        //     doesn't propagate per-build tenant attribution on PutPath
        //
        // Worker uploads: if P0259's scope includes the worker→store call
        // path AND the worker forwards its assignment's JWT, this fires.
        // Otherwise None → cluster key, same as pre-P0338.
        let tenant_id: Option<uuid::Uuid> = request
            .extensions()
            .get::<rio_common::jwt::Claims>()
            .map(|c| c.sub);

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

        // Shared 6-step validation: nar_hash-empty, bounds, placeholder,
        // TryFrom, HMAC path-in-claims. See validate_put_metadata for the
        // full commentary (moved there because PutPathBatch has the same
        // dance). The server still computes the digest either way
        // (NarDigest::from_bytes below), so the security property
        // (client-declared hash matches server-computed) is unchanged.
        let mut info = validate_put_metadata(raw_info, hmac_claims.as_ref(), "PutPath")?;

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

        // Step 3: Insert manifest placeholder with status='uploading'.
        // Returns false (ON CONFLICT DO NOTHING no-op) if another uploader
        // already holds a placeholder. In that case we must NOT proceed: if
        // we do and later fail validation, delete_manifest_uploading would
        // delete the OTHER uploader's placeholder, losing their valid upload.
        //
        // STRUCTURAL: insert_manifest_uploading now takes references and
        // writes them into the placeholder narinfo. Mark's CTE walks them
        // from commit → the closure is GC-protected without holding a
        // session lock for the full upload. The lock is transaction-scoped
        // (pg_try_advisory_xact_lock_shared inside the tx, ~ms). On lock
        // contention (mark running), returns MetadataError::Serialization
        // → metadata_status maps to Status::aborted. Worker retries
        // (upload.rs:143-188, 3 attempts); gateway does NOT retry —
        // `nix copy` during mark (~1s window) gets STDERR_ERROR and the
        // client may re-issue. Blocking variant would deadlock if the pool
        // saturated during concurrent mark wait.
        //
        // References stringified: ValidatedPathInfo holds Vec<StorePath>;
        // the narinfo column is text[]. These are the same values that
        // update_narinfo_complete will write at completion (references
        // don't change between metadata arrival and trailer).
        let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
        let inserted = match metadata::insert_manifest_uploading(
            &self.pool,
            &store_path_hash,
            &info.store_path,
            &refs_str,
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

        // Step 4: Accumulate NAR chunks into a buffer. Two bounds:
        //   (a) per-request MAX_NAR_SIZE — enforced in-loop below
        //   (b) GLOBAL nar_bytes_budget — acquired per-chunk, released on
        //       handler drop. 10 concurrent 4 GiB uploads = 40 GiB RSS; the
        //       budget backpressures the 9th+ upload instead of OOM.
        // Permits held in a Vec so drop-on-any-exit releases them. No
        // scopeguard needed — SemaphorePermit's Drop does the right thing.
        let mut nar_data = Vec::new();
        let mut trailer: Option<rio_proto::types::PutPathTrailer> = None;
        let mut _held_permits: Vec<tokio::sync::SemaphorePermit<'_>> = Vec::new();
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
                    // `>=` so a single chunk of exactly 2^32 bytes is rejected
                    // here, before it reaches acquire_many(0) below and silently
                    // bypasses the byte budget.
                    if new_len >= MAX_NAR_SIZE {
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
                    // Global byte budget. acquire_many(u32) — chunk.len() is
                    // bounded by rio_proto::DEFAULT_MAX_MESSAGE_SIZE (32 MiB,
                    // env-configurable via RIO_GRPC_MAX_MESSAGE_SIZE), so the
                    // cast never truncates. `await` here backpressures the
                    // client: if the budget is exhausted, recv stalls,
                    // gRPC flow control propagates, client send blocks.
                    // acquire_many only errs if the semaphore is closed
                    // (never happens; budget lives for the process), so
                    // we map to resource_exhausted defensively.
                    let permit = self
                        .nar_bytes_budget
                        .acquire_many(chunk.len() as u32)
                        .await
                        .map_err(|_| Status::resource_exhausted("NAR buffer budget closed"))?;
                    _held_permits.push(permit);
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
        if let Err(e) = apply_trailer(&mut info, &t, "PutPath") {
            self.abort_upload(&store_path_hash).await;
            return Err(e);
        }

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
        self.maybe_sign(tenant_id, &mut full_info).await;

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

        metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
        // r[impl obs.metric.transfer-volume]
        metrics::counter!("rio_store_put_path_bytes_total").increment(info.nar_size);
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

    /// Build a cert with SAN DNSNames and optionally a CN. `cn = None`
    /// sets an EMPTY distinguished_name (rcgen otherwise defaults to
    /// CN="rcgen self signed cert", which would defeat the SAN-only
    /// test). This mirrors cert-manager's output shape: identity in
    /// SAN, CN left empty.
    fn make_cert_with_san(dns_names: &[&str], cn: Option<&str>) -> Vec<u8> {
        let sans: Vec<String> = dns_names.iter().map(|s| (*s).to_string()).collect();
        let mut params = rcgen::CertificateParams::new(sans).unwrap();
        params.distinguished_name = match cn {
            Some(cn) => {
                let mut dn = rcgen::DistinguishedName::new();
                dn.push(rcgen::DnType::CommonName, cn);
                dn
            }
            None => rcgen::DistinguishedName::new(), // empty — no CN
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

    // r[verify store.hmac.san-bypass]
    /// SAN-only cert (empty CN, identity in SAN DNSName) grants bypass.
    /// This is the cert-manager shape — RFC 6125 deprecates CN for
    /// hostname verification, SAN is the modern path.
    ///
    /// Precondition assert (cert_cn → None) proves the test is actually
    /// exercising the SAN branch, not accidentally matching on a CN
    /// rcgen might have defaulted.
    #[test]
    fn san_only_cert_no_cn_bypasses() {
        let der = make_cert_with_san(&["rio-gateway"], None);
        assert_eq!(
            cert_cn(&der),
            None,
            "precondition: cert must have NO CN for this test to prove SAN matching"
        );
        assert!(cert_identity_in_allowlist(
            &der,
            &["rio-gateway".to_string()]
        ));
    }

    /// SAN DNSName NOT in allowlist → no bypass. Worker-issued certs
    /// (SAN=rio-worker) must still present a token.
    #[test]
    fn san_mismatch_does_not_bypass() {
        let der = make_cert_with_san(&["rio-worker"], None);
        assert!(!cert_identity_in_allowlist(
            &der,
            &["rio-gateway".to_string()]
        ));
    }

    /// Backward-compat: CN-only certs (pre-cert-manager, manually
    /// issued) still bypass. The allowlist check is CN-OR-SAN, not
    /// SAN-only.
    #[test]
    fn cn_still_bypasses_backward_compat() {
        let der = make_cert_with_cn("rio-gateway");
        assert!(cert_identity_in_allowlist(
            &der,
            &["rio-gateway".to_string()]
        ));
    }

    /// Custom allowlist entries work (deployments that name their
    /// gateway cert differently). And the default "rio-gateway"
    /// is NOT hardcoded anywhere — an allowlist without it
    /// rejects even CN=rio-gateway.
    #[test]
    fn custom_allowlist_works() {
        let der = make_cert_with_cn("my-custom-gateway");
        assert!(cert_identity_in_allowlist(
            &der,
            &["my-custom-gateway".to_string()]
        ));
        assert!(
            !cert_identity_in_allowlist(&der, &["rio-gateway".to_string()]),
            "my-custom-gateway must NOT match rio-gateway allowlist"
        );
        // Inverse: rio-gateway cert against custom-only allowlist → reject.
        // Proves the string "rio-gateway" is NOT special-cased in the impl.
        let gw_der = make_cert_with_cn("rio-gateway");
        assert!(
            !cert_identity_in_allowlist(&gw_der, &["my-custom-gateway".to_string()]),
            "rio-gateway must NOT bypass when allowlist doesn't include it"
        );
    }

    /// Multi-SAN cert: any single match grants bypass. cert-manager
    /// typically emits multiple SANs (service DNS name + headless
    /// service name + namespaced FQDN).
    #[test]
    fn multi_san_any_match_bypasses() {
        let der = make_cert_with_san(
            &[
                "rio-gateway.rio.svc.cluster.local",
                "rio-gateway.rio",
                "rio-gateway",
            ],
            None,
        );
        // Only the short name is in the allowlist — still matches.
        assert!(cert_identity_in_allowlist(
            &der,
            &["rio-gateway".to_string()]
        ));
    }

    /// CN mismatch + SAN match → bypass. CN doesn't have to be in
    /// the allowlist if a SAN is.
    #[test]
    fn cn_mismatch_san_match_bypasses() {
        let der = make_cert_with_san(&["rio-gateway"], Some("some-other-cn"));
        assert_eq!(cert_cn(&der), Some("some-other-cn".into()));
        assert!(cert_identity_in_allowlist(
            &der,
            &["rio-gateway".to_string()]
        ));
    }

    /// Empty allowlist → nothing bypasses, including rio-gateway.
    /// Not a supported deployment shape but the config allows it
    /// (operator who wants HMAC-only, no mTLS bypass at all).
    #[test]
    fn empty_allowlist_bypasses_nothing() {
        let der = make_cert_with_cn("rio-gateway");
        assert!(!cert_identity_in_allowlist(&der, &[]));
        let san_der = make_cert_with_san(&["rio-gateway"], None);
        assert!(!cert_identity_in_allowlist(&san_der, &[]));
    }

    /// Malformed DER → false (fail closed). Same as cert_cn's
    /// garbage-returns-None, but for the authz path.
    #[test]
    fn garbage_der_does_not_bypass() {
        assert!(!cert_identity_in_allowlist(
            b"not a cert",
            &["rio-gateway".to_string()]
        ));
    }

    /// T3: NAR byte budget backpressure. `with_nar_budget(N)` sets a
    /// global semaphore; when exhausted, subsequent `acquire_many`
    /// blocks. Dropping a permit releases capacity. This proves the
    /// plumbing without needing to mock the full PutPath stream protocol
    /// (which would require a tonic Streaming<T> mock — invasive).
    ///
    /// The PutPath accumulation loop does exactly what this test does:
    /// `acquire_many(chunk.len())` before `extend_from_slice`, with
    /// permits held in a Vec that drops on handler exit.
    #[tokio::test]
    async fn nar_budget_backpressures_then_releases() {
        use rio_test_support::TestDb;
        use std::time::Duration;

        let db = TestDb::new(&crate::MIGRATOR).await;
        // Budget = 4096 bytes total. One 4096-byte "upload" fills it.
        let svc = StoreServiceImpl::new(db.pool.clone()).with_nar_budget(4096);
        let budget = svc.nar_bytes_budget();

        assert_eq!(budget.available_permits(), 4096, "with_nar_budget plumbed");

        // First "upload" acquires all 4096 permits. Simulates a handler
        // that received one 4096-byte chunk and is now paused (waiting
        // for more chunks / trailer).
        let permit1 = budget.acquire_many(4096).await.expect("available");
        assert_eq!(budget.available_permits(), 0, "budget exhausted");

        // Second "upload" tries to acquire 1 byte. Must BLOCK (budget
        // exhausted). Race a 100ms sleep against the acquire; the sleep
        // must win because acquire is blocked on the exhausted semaphore.
        let budget2 = budget.clone();
        let acquire2 = tokio::spawn(async move { budget2.acquire_many_owned(1).await });
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                // Expected: acquire is blocked.
            }
            _ = &mut tokio::spawn(async { tokio::time::sleep(Duration::from_secs(0)).await }) => {}
        }
        assert!(
            !acquire2.is_finished(),
            "second acquire should block on exhausted budget"
        );

        // Drop the first permit (simulates first handler exiting —
        // error, completion, or client disconnect). Capacity freed.
        drop(permit1);

        // Second acquire unblocks.
        let permit2 = tokio::time::timeout(Duration::from_secs(5), acquire2)
            .await
            .expect("acquire2 should unblock after permit1 dropped")
            .expect("join")
            .expect("semaphore not closed");
        assert_eq!(budget.available_permits(), 4095);
        drop(permit2);
        assert_eq!(budget.available_permits(), 4096, "fully released");
    }
}

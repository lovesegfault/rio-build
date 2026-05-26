//! Shared PutPath / PutPathBatch flow steps.
//!
//! Both upload RPCs walk the same write-ahead state machine:
//!
//! 1. **authorize** — HMAC token check + JWT tenant extraction
//!    ([`StoreServiceImpl::authorize`]; per-path allowlist applied in
//!    [`validate_put_metadata`])
//! 2. **claim_placeholder** — idempotency check, insert
//!    `status='uploading'` row, hot-path stale-reclaim
//!    ([`StoreServiceImpl::claim_placeholder`])
//! 3. **ingest** — accumulate NAR bytes ([`StoreServiceImpl::accumulate_chunk`]),
//!    apply trailer ([`apply_trailer`]), verify SHA-256 ([`verify_nar`]).
//!    PutPath drains a linear stream via
//!    [`StoreServiceImpl::ingest_nar_stream`]; PutPathBatch demuxes by
//!    `output_index` then verifies each.
//! 4. **finalize** — sign + persist inline-or-chunked
//!    ([`StoreServiceImpl::finalize_single`] for the standalone path;
//!    PutPathBatch stages then commits in one tx)
//!
//! These were duplicated across `put_path_impl` and
//! `put_path_batch_impl` and had already drifted once (batch lacked
//! chunked support). Factoring here keeps both impls thin wrappers
//! around the same state machine.

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tonic::{Request, Status, Streaming};
use tracing::warn;

use rio_proto::types::{PutPathRequest, PutPathTrailer, put_path_request};
use rio_proto::validated::ValidatedPathInfo;

use rio_common::grpc::StatusExt;
use rio_common::limits::{MAX_NAR_SIZE, nar_chunk_charge};

use crate::cas;
use crate::grpc::{StoreServiceImpl, putpath_metadata_status, storage_error};
use crate::ingest;
use crate::metadata;

/// Re-export of [`crate::ingest::PlaceholderClaim`] — the write-ahead
/// state machine lives in `ingest` (shared with `Substituter`); this
/// keeps existing `grpc::` callers' paths stable.
pub(in crate::grpc) use crate::ingest::PlaceholderClaim;

/// gRPC PutPath/PutPathBatch hooks for the shared ingest core.
const PUTPATH_HOOKS: ingest::IngestHooks = ingest::IngestHooks {
    stale_reclaimed_metric: "rio_store_putpath_stale_reclaimed_total",
    ctx_label: "PutPath",
};

/// How the NAR was persisted. Batch uses this to pick the right
/// `complete_manifest_*_in_tx` variant inside its atomic tx.
pub(in crate::grpc) enum NarPersist {
    /// `nar_data.len() < INLINE_THRESHOLD` (or no chunk backend).
    /// Bytes carried so the batch tx can write `inline_blob`.
    Inline(Bytes),
    /// `nar_data.len() >= INLINE_THRESHOLD` and a chunk backend is
    /// configured. Chunks already uploaded + refcounted via
    /// [`cas::stage_chunked`]; only the `status='complete'` flip
    /// remains.
    ChunkedStaged,
}

/// Auth context for PutPath / PutPathBatch: HMAC assignment claims
/// (path allowlist) + JWT tenant id (signing-key selection). Both
/// extracted from request metadata BEFORE `into_inner()` consumes it.
pub(in crate::grpc) struct PutAuth {
    pub hmac_claims: Option<rio_auth::hmac::AssignmentClaims>,
    pub tenant_id: Option<uuid::Uuid>,
}

/// Validate a raw PathInfo message for PutPath/PutPathBatch.
///
/// Shared validation shared by both upload RPCs: (1) nar_hash-empty
/// enforcement (trailer-only mode), (2) references bound,
/// (3) signatures bound, (4) placeholder hash fill,
/// (5) ValidatedPathInfo::try_from, (6) HMAC path-in-claims check,
/// (7) `store_path_hash` recomputed server-side.
///
/// `ctx_label` goes into error messages for client-side disambiguation
/// ("PutPath" vs "output N").
///
/// Returns the validated info with `store_path_hash` populated from the
/// HMAC-gated `store_path` (NEVER from the wire — see step 7); on HMAC
/// path-not-in-claims failure, increments the
/// `hmac_rejected_total{reason=path_not_in_claims}` counter before
/// erroring.
// r[impl sec.boundary.grpc-hmac]
pub(in crate::grpc) fn validate_put_metadata(
    mut raw_info: rio_proto::types::PathInfo,
    hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
    ctx_label: &str,
) -> Result<ValidatedPathInfo, Status> {
    // Step 1: trailer-only enforcement. metadata.nar_hash must be empty;
    // hash arrives in the PutPathTrailer after all chunks. Both gateway
    // (chunk_nar_for_put) and worker (single-pass tee upload) send
    // trailers. A non-empty nar_hash means an un-updated client.
    if !raw_info.nar_hash.is_empty() {
        return Err(Status::invalid_argument(format!(
            "{ctx_label}: metadata.nar_hash must be empty (hash-upfront mode removed; \
             send hash in PutPathTrailer)"
        )));
    }

    // Step 4: placeholder so TryFrom passes (it hard-fails on empty
    // nar_hash). Overwritten after trailer. 32 zero bytes — unambiguously
    // NOT a real SHA-256 (would be the hash of a specific ~2^256-rare
    // preimage). nar_size is also 0 here — real value from trailer.
    raw_info.nar_hash = vec![0u8; 32];

    // Steps 2-3: bound repeated fields BEFORE per-element validation
    // (TryFrom validates each reference's syntax but doesn't bound the
    // count; an attacker could send 10M valid references and we'd parse
    // them all before failing).
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

    // Step 5: centralized validation — store_path parses, nar_hash is
    // 32 bytes (placeholder), each reference parses.
    let mut info = ValidatedPathInfo::try_from(raw_info).status_invalid(ctx_label)?;

    // Step 7: derive store_path_hash from the parsed store_path,
    // unconditionally overwriting whatever the client sent. The HMAC
    // gate (step 6) binds `store_path`, NOT `store_path_hash` — a
    // worker holding a token for path A could otherwise send
    // {store_path: A, store_path_hash: sha256(B)} and key A's narinfo
    // under B's slot. Doing this at the chokepoint means no caller
    // can forget; all downstream `info.store_path_hash` reads are
    // server-derived.
    info.store_path_hash = info.store_path.sha256_digest().to_vec();

    // Step 6: HMAC path-in-claims check. `None` (verifier disabled OR
    // service-token bypass — see `StoreServiceImpl::
    // verify_assignment_token`) → no path-membership check.
    // Floating-CA (claims.is_ca) →
    // skip the membership check here: the output path is computed
    // post-build from the NAR hash, so expected_outputs is [""] at
    // sign time. Authorization for the CA case is enforced by
    // `verify_ca_store_path` (sec.authz.ca-path-derived) which runs
    // BEFORE `claim_placeholder` in both PutPath and PutPathBatch: the
    // store recomputes the CA path from the server-verified `nar_hash`
    // and rejects on mismatch, so an is_ca worker can never claim a
    // placeholder for a path it hasn't content-proven.
    if let Some(claims) = hmac_claims
        && !claims.is_ca
    {
        let path_str = info.store_path.as_str();
        if !claims.expected_outputs.iter().any(|o| o == path_str) {
            warn!(
                store_path = %path_str,
                executor_id = %claims.executor_id,
                drv_hash = %claims.drv_hash,
                "{ctx_label}: path not in assignment's expected_outputs",
            );
            metrics::counter!(
                "rio_store_hmac_rejected_total",
                "reason" => "path_not_in_claims"
            )
            .increment(1);
            return Err(Status::permission_denied(format!(
                "{ctx_label}: path not authorized by assignment token"
            )));
        }
    }

    Ok(info)
}

/// Apply a PutPathTrailer to a ValidatedPathInfo: 32-byte hash check,
/// nar_size bound, then overwrite the placeholder hash+size on `info`.
/// Caller handles async cleanup on error (abort_upload / bail!).
/// Callers that need the hash read `info.nar_hash` after the call.
pub(in crate::grpc) fn apply_trailer(
    info: &mut ValidatedPathInfo,
    t: &PutPathTrailer,
    ctx_label: &str,
) -> Result<(), Status> {
    let hash: [u8; 32] = t.nar_hash.as_slice().try_into().map_err(|_| {
        Status::invalid_argument(format!(
            "{ctx_label}: trailer nar_hash must be 32 bytes (SHA-256), got {}",
            t.nar_hash.len()
        ))
    })?;
    if t.nar_size > MAX_NAR_SIZE {
        return Err(Status::invalid_argument(format!(
            "{ctx_label}: trailer nar_size {} exceeds maximum {MAX_NAR_SIZE}",
            t.nar_size
        )));
    }
    info.nar_hash = hash;
    info.nar_size = t.nar_size;
    Ok(())
}

/// Read the first PutPath message; must be `Metadata` carrying a
/// `PathInfo`. Shared step-1 of the write-ahead flow.
pub(in crate::grpc) async fn read_first_metadata(
    stream: &mut Streaming<PutPathRequest>,
) -> Result<rio_proto::types::PathInfo, Status> {
    let first = stream
        .message()
        .await?
        .ok_or_else(|| Status::invalid_argument("empty PutPath stream"))?;
    match first.msg {
        Some(put_path_request::Msg::Metadata(meta)) => meta
            .info
            .ok_or_else(|| Status::invalid_argument("PutPathMetadata missing PathInfo")),
        Some(put_path_request::Msg::NarChunk(_)) => Err(Status::invalid_argument(
            "first PutPath message must be metadata, not nar_chunk",
        )),
        Some(put_path_request::Msg::Trailer(_)) => Err(Status::invalid_argument(
            "first PutPath message must be metadata, not trailer",
        )),
        None => Err(Status::invalid_argument("PutPath message has no content")),
    }
}

// r[impl store.integrity.verify-on-put]
// r[impl sec.drv.validate]
/// Compare a server-computed NAR digest+size against the
/// trailer-declared `nar_hash` / `nar_size` (already applied to `info`
/// via [`apply_trailer`]). The integrity gate of
/// `r[store.integrity.verify-on-put]` — server computes the digest
/// independently of the client.
///
/// `computed_hash` is the finalized output of an incremental
/// `sha2::Sha256` that [`StoreServiceImpl::accumulate_chunk`] fed every
/// chunk into. Hashing happens chunk-by-chunk during the gRPC receive
/// loop (which `await`s between chunks), so a 4 GiB NAR never blocks a
/// tokio worker for a multi-second one-shot `Sha256::digest`.
///
/// Status messages contain the substrings "size mismatch" / "hash
/// mismatch"; protocol tests assert on those.
pub(in crate::grpc) fn verify_nar(
    computed_hash: [u8; 32],
    actual_size: u64,
    info: &ValidatedPathInfo,
    ctx_label: &str,
) -> Result<(), Status> {
    let fail = |e: String| {
        warn!(store_path = %info.store_path, error = %e, "{ctx_label}: NAR validation failed");
        Status::invalid_argument(format!("{ctx_label}: NAR validation failed: {e}"))
    };
    if actual_size != info.nar_size {
        return Err(fail(format!(
            "NAR size mismatch: declared {}, actual {actual_size}",
            info.nar_size
        )));
    }
    if computed_hash != info.nar_hash {
        return Err(fail(format!(
            "NAR hash mismatch: declared {}, computed {}",
            hex::encode(info.nar_hash),
            hex::encode(computed_hash)
        )));
    }
    Ok(())
}

/// Parsed `fixed:` content-address descriptor of a floating-CA upload:
/// ingestion method (`r:` prefix → recursive/NAR, otherwise flat) plus
/// the declared content hash, exactly as the builder's
/// `finalize_floating_ca` records it (`fixed:[r:]<algo>:<hash>`).
struct FixedCaDescriptor {
    recursive: bool,
    hash: rio_nix::hash::NixHash,
}

/// Parse `info.content_address` for the CA gate. `text:` descriptors
/// (store-added .drv files) and anything else that is not a `fixed:`
/// descriptor are rejected — a floating-CA *build output* upload always
/// carries `fixed:`.
fn parse_fixed_ca_descriptor(s: &str, ctx_label: &str) -> Result<FixedCaDescriptor, Status> {
    let rest = s.strip_prefix("fixed:").ok_or_else(|| {
        Status::invalid_argument(format!(
            "{ctx_label}: unsupported content-address descriptor {s:?} on a floating-CA upload \
             (expected `fixed:[r:]<algo>:<hash>`)"
        ))
    })?;
    let (recursive, hash_str) = match rest.strip_prefix("r:") {
        Some(h) => (true, h),
        None => (false, rest),
    };
    let hash = rio_nix::hash::NixHash::parse_colon(hash_str).map_err(|e| {
        Status::invalid_argument(format!(
            "{ctx_label}: malformed content-address hash in {s:?}: {e}"
        ))
    })?;
    Ok(FixedCaDescriptor { recursive, hash })
}

// r[impl sec.authz.ca-path-derived+3]
/// Floating-CA path-authorization gate. When `claims.is_ca` is set,
/// [`validate_put_metadata`] skipped the `store_path ∈
/// expected_outputs` check (the path isn't known at sign time). This
/// is the replacement gate: recompute the CA store path SERVER-SIDE
/// from the NAR that [`verify_nar`] just confirmed, and reject if
/// it doesn't match `info.store_path`. A worker holding an
/// `is_ca=true` token therefore cannot upload to any path that isn't
/// the content-derived path of the NAR it actually sent.
///
/// The ingestion method comes from the upload's own `fixed:` content
/// address descriptor (`info.content_address`), exactly as the
/// builder's `finalize_floating_ca` records it: `r:`-prefixed →
/// recursive (NAR) hashing, otherwise flat (single regular file)
/// hashing, with sha1/sha256/sha512 all accepted. The declared
/// descriptor hash is cross-checked against the server-side recompute,
/// so the persisted/served `CA:` field can never lie about the bytes.
/// Uploads without a descriptor fall back to the historical
/// recursive-SHA256 assumption. FODs with known output paths go
/// through the IA `expected_outputs` check instead.
///
/// **Self-references**: a floating-CA output whose content embeds its
/// own store path declares itself in `references`. The expected path
/// is then `make_fixed_output_with_self` over the **hash modulo
/// self-references** — the NAR re-hashed with every occurrence of the
/// *claimed* path's hash part zeroed ([`HashModuloSink`]). This stays
/// self-certifying: a forged claim would need a path whose hash part,
/// once zeroed in the content, hashes back to exactly that path.
/// Non-self-referencing recursive-SHA256 uploads keep using the
/// already-verified plain NAR hash (identical to the modulo hash when
/// there are no occurrences) — no second pass over the bytes; other
/// algorithms re-hash the buffered NAR with the declared algorithm,
/// and flat ingestion hashes the single file's bytes in place from the
/// buffered NAR (no extraction copy, no artificial size ceiling).
///
/// When a `fixed:` descriptor is present but disagrees with the plain
/// hash, the gate retries once with the hash modulo the claimed path's
/// own hash part before rejecting: structured-attrs
/// `unsafeDiscardReferences` legitimately produces uploads whose bytes
/// embed their own path while declaring no self-reference, and CppNix
/// mints those paths from exactly that modulo hash.
///
/// `None`/non-CA claims → no-op (IA already gated, dev/service
/// bypass already trusted).
///
/// [`HashModuloSink`]: rio_nix::ca::HashModuloSink
pub(in crate::grpc) fn verify_ca_store_path(
    info: &ValidatedPathInfo,
    nar_data: &[u8],
    hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
    ctx_label: &str,
) -> Result<(), Status> {
    let Some(claims) = hmac_claims else {
        return Ok(());
    };
    if !claims.is_ca {
        return Ok(());
    }

    // The path under construction may appear in its own reference set
    // (declared self-reference). It is expressed via the `:self`
    // fingerprint token, never as an ordinary reference.
    let refs: Vec<rio_nix::store_path::StorePath> = info
        .references
        .iter()
        .filter(|r| r.as_str() != info.store_path.as_str())
        .cloned()
        .collect();
    let has_self_reference = refs.len() != info.references.len();

    // Ingestion method: from the upload's own `fixed:` descriptor when
    // present (the native builder records one for every floating-CA
    // output); absent → the historical recursive-SHA256 assumption.
    let descriptor = info
        .content_address
        .as_deref()
        .map(|s| parse_fixed_ca_descriptor(s, ctx_label))
        .transpose()?;
    let (recursive, algo) = match &descriptor {
        Some(d) => (d.recursive, d.hash.algo()),
        None => (true, rio_nix::hash::HashAlgo::SHA256),
    };

    use std::io::Write as _;
    let content_hash = if recursive {
        if has_self_reference {
            // Hash modulo self-references: zero every occurrence of the
            // claimed path's hash part, then hash. O(nar) — only paid
            // by self-referencing uploads.
            let mut sink = rio_nix::ca::HashModuloSink::new(algo, &info.store_path.hash_part());
            sink.write_all(nar_data)
                .map_err(|e| Status::internal(format!("{ctx_label}: hash-modulo: {e}")))?;
            sink.finish().0
        } else {
            let plain = if algo == rio_nix::hash::HashAlgo::SHA256 {
                // info.nar_hash is the SERVER-COMPUTED hash here (verify_nar
                // has already confirmed it equals SHA-256(stream)).
                rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, info.nar_hash.to_vec())
                    .map_err(|e| {
                        Status::internal(format!("{ctx_label}: nar_hash construct: {e}"))
                    })?
            } else {
                // Non-SHA-256 recursive ingestion: re-hash the buffered NAR
                // with the declared algorithm.
                let mut w = rio_nix::ca::HashWriter::new(algo);
                w.write_all(nar_data).map_err(|e| {
                    Status::internal(format!("{ctx_label}: nar hash ({algo}): {e}"))
                })?;
                w.finish()
            };
            // Descriptor-gated fallback for *unrecorded* self-references:
            // CppNix's structured-attrs `unsafeDiscardReferences` mints
            // paths whose bytes embed their own hash while declaring no
            // self-reference, and the `fixed:` descriptor then carries the
            // hash *modulo* those occurrences. If the descriptor disagrees
            // with the plain hash, re-hash modulo the claimed path's own
            // hash before concluding anything — still self-certifying (the
            // modulus is the claimed path itself), and the extra pass is
            // only paid when the plain hash already failed to match.
            match &descriptor {
                Some(d) if d.hash != plain => {
                    let mut sink =
                        rio_nix::ca::HashModuloSink::new(algo, &info.store_path.hash_part());
                    sink.write_all(nar_data).map_err(|e| {
                        Status::internal(format!("{ctx_label}: hash-modulo fallback: {e}"))
                    })?;
                    sink.finish().0
                }
                _ => plain,
            }
        }
    } else {
        // Flat ingestion: the content hash is over the bytes of the
        // single regular file the NAR must contain. Self-references are
        // unrepresentable for flat outputs (the path derivation rejects
        // them), so a declared one can never verify.
        if has_self_reference {
            return Err(Status::permission_denied(format!(
                "{ctx_label}: flat content-addressed upload declares a self-reference"
            )));
        }
        // Hash the file bytes in place from the already-buffered NAR: no
        // copy of the contents, and no size ceiling beyond the store's own
        // upload limit (the general-purpose NAR parser's 256 MiB
        // single-content cap does not apply to flat verification).
        let (off, len) = single_file_nar_content_range(nar_data).map_err(|e| {
            Status::invalid_argument(format!(
                "{ctx_label}: flat content-addressed upload is not a single-file NAR: {e}"
            ))
        })?;
        let len = usize::try_from(len).map_err(|_| {
            Status::invalid_argument(format!(
                "{ctx_label}: flat content length does not fit this platform"
            ))
        })?;
        let mut w = rio_nix::ca::HashWriter::new(algo);
        w.write_all(&nar_data[off..off + len])
            .map_err(|e| Status::internal(format!("{ctx_label}: flat hash ({algo}): {e}")))?;
        w.finish()
    };

    // The descriptor is persisted and served to substituting clients
    // (narinfo `CA:`); a descriptor whose hash does not match the bytes
    // actually sent is rejected even if the claimed path is otherwise
    // consistent.
    if let Some(d) = &descriptor
        && d.hash != content_hash
    {
        warn!(
            store_path = %info.store_path,
            declared = %d.hash.to_colon(),
            computed = %content_hash.to_colon(),
            executor_id = %claims.executor_id,
            "{ctx_label}: content-address descriptor does not match the uploaded NAR"
        );
        metrics::counter!(
            "rio_store_hmac_rejected_total",
            "reason" => "ca_descriptor_mismatch"
        )
        .increment(1);
        return Err(Status::permission_denied(format!(
            "{ctx_label}: content-address descriptor does not match the uploaded content"
        )));
    }

    let expected = rio_nix::store_path::StorePath::make_fixed_output_with_self(
        info.store_path.name(),
        &content_hash,
        recursive,
        &refs,
        has_self_reference,
    )
    .map_err(|e| Status::invalid_argument(format!("{ctx_label}: CA path derive: {e}")))?;

    if expected.as_str() != info.store_path.as_str() {
        warn!(
            store_path = %info.store_path,
            expected = %expected,
            executor_id = %claims.executor_id,
            drv_hash = %claims.drv_hash,
            "{ctx_label}: is_ca store_path does not match server-derived CA path"
        );
        metrics::counter!(
            "rio_store_hmac_rejected_total",
            "reason" => "ca_path_mismatch"
        )
        .increment(1);
        return Err(Status::permission_denied(format!(
            "{ctx_label}: store_path does not match content-derived CA path"
        )));
    }
    Ok(())
}

pub(in crate::grpc) use crate::ingest::PlaceholderGuard;

impl StoreServiceImpl {
    // r[impl sec.boundary.grpc-hmac]
    /// HMAC token verify + JWT tenant extraction. Shared step-0 of the
    /// write-ahead flow (see
    /// [`StoreServiceImpl::verify_assignment_token`] for the
    /// `Ok(None)` cases).
    ///
    /// Distinct claim types — don't confuse them:
    /// - `hmac::AssignmentClaims`: worker_id + drv_hash +
    ///   expected_outputs. Restricts WHICH paths this worker may upload.
    ///   Per-assignment.
    /// - `jwt::TenantClaims`: sub (tenant UUID) + iat/exp/jti. Says
    ///   WHOSE tenant key signs the narinfo. Per-session.
    ///
    /// `tenant_id = None` covers: no interceptor wired (dev mode), no
    /// `x-rio-tenant-token` header (dual-mode fallback), or
    /// service-token caller (gateway — no per-build assignment token;
    /// see [`StoreServiceImpl::verify_assignment_token`]) — all
    /// cluster-key-correct.
    pub(in crate::grpc) fn authorize<T>(&self, request: &Request<T>) -> Result<PutAuth, Status> {
        let hmac_claims = self.verify_assignment_token(request)?;
        let tenant_id = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub);
        Ok(PutAuth {
            hmac_claims,
            tenant_id,
        })
    }

    // r[impl store.put.nar-bytes-budget+3]
    /// Append a NAR chunk under both bounds: per-output [`MAX_NAR_SIZE`]
    /// and the GLOBAL `nar_bytes_budget` semaphore. Feeds the chunk
    /// into the caller's incremental `hasher` so [`verify_nar`] never
    /// has to one-shot-hash a multi-GiB buffer on a tokio worker.
    /// Returns the held permit; the caller pushes it into a `Vec` so
    /// drop-on-any-exit releases capacity. `await` here backpressures
    /// the client via gRPC flow control when the budget is exhausted.
    ///
    /// Empty chunks are rejected outright (legit clients never send
    /// them; an infinite empty-chunk stream would otherwise
    /// `acquire_many(0)` forever and grow `held_permits` unbounded by
    /// the byte budget). Tiny chunks are charged
    /// [`nar_chunk_charge`] (floored at `MIN_NAR_CHUNK_CHARGE`) so
    /// per-permit tracking overhead is itself bounded by the budget;
    /// callers track cumulative `nar_chunk_charge(len)` against
    /// `MAX_NAR_SIZE` BEFORE calling so a single handler never
    /// self-deadlocks on permits it holds.
    ///
    /// `>=` so a single chunk of exactly 2³² bytes is rejected before
    /// it reaches `acquire_many(0)` and silently bypasses the budget.
    /// `nar_chunk_charge(len) as u32` never truncates: chunks are
    /// bounded by `RIO_GRPC_MAX_MESSAGE_SIZE`.
    pub(in crate::grpc) async fn accumulate_chunk<'a>(
        &'a self,
        nar_data: &mut Vec<u8>,
        hasher: &mut Sha256,
        chunk: &[u8],
        ctx_label: &str,
    ) -> Result<tokio::sync::SemaphorePermit<'a>, Status> {
        if chunk.is_empty() {
            return Err(Status::invalid_argument(format!(
                "{ctx_label}: empty NarChunk (protocol violation)"
            )));
        }
        let new_len = (nar_data.len() as u64).saturating_add(chunk.len() as u64);
        if new_len >= MAX_NAR_SIZE {
            return Err(Status::invalid_argument(format!(
                "{ctx_label}: NAR chunks exceed size bound {MAX_NAR_SIZE} (received {new_len}+ bytes)"
            )));
        }
        let permit = self
            .nar_bytes_budget
            .acquire_many(nar_chunk_charge(chunk.len()) as u32)
            .await
            .map_err(|_| Status::resource_exhausted("NAR buffer budget closed"))?;
        nar_data.extend_from_slice(chunk);
        hasher.update(chunk);
        Ok(permit)
    }

    /// Thin wrapper over [`crate::ingest::spawn_placeholder_guard`]
    /// supplying `self.pool` / `self.chunk_backend`. See that fn's doc
    /// for the drop-cleanup + heartbeat invariants.
    pub(in crate::grpc) fn spawn_placeholder_guard(
        &self,
        store_path_hash: Vec<u8>,
        claim: uuid::Uuid,
    ) -> PlaceholderGuard {
        crate::ingest::spawn_placeholder_guard(
            self.pool.clone(),
            self.chunk_backend.clone(),
            store_path_hash,
            claim,
        )
    }

    /// Drain a single-output PutPath stream after metadata: accumulate
    /// chunks ([`Self::accumulate_chunk`]), receive the mandatory
    /// trailer, reject protocol violations (chunk-after-trailer,
    /// duplicate metadata/trailer), then [`apply_trailer`] +
    /// [`verify_nar`] + [`verify_ca_store_path`]. Returns the buffered
    /// NAR and held budget permits.
    ///
    /// Errors do NOT clean up the placeholder — caller wraps the call
    /// and `abort_upload`s on `Err`.
    pub(in crate::grpc) async fn ingest_nar_stream<'a>(
        &'a self,
        stream: &mut Streaming<PutPathRequest>,
        info: &mut ValidatedPathInfo,
        hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
    ) -> Result<(Vec<u8>, Vec<tokio::sync::SemaphorePermit<'a>>), Status> {
        let mut nar_data = Vec::new();
        let mut hasher = Sha256::new();
        let mut trailer: Option<PutPathTrailer> = None;
        let mut held_permits = Vec::new();
        // Cumulative permits charged (NOT raw bytes — `accumulate_chunk`
        // floors each chunk at MIN_NAR_CHUNK_CHARGE). Checked BEFORE
        // `accumulate_chunk` so a tiny-chunk stream that would exhaust
        // the global budget hits this cap instead of self-deadlocking on
        // `acquire_many` for permits this task already holds.
        // r[impl store.put.nar-bytes-budget+3]
        let mut charged: u64 = 0;
        loop {
            let msg = match stream.message().await {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => {
                    warn!(store_path = %info.store_path, error = %e, "PutPath: stream read error");
                    return Err(e);
                }
            };
            match msg.msg {
                Some(put_path_request::Msg::NarChunk(chunk)) => {
                    if trailer.is_some() {
                        return Err(Status::invalid_argument(
                            "PutPath: nar_chunk after trailer (trailer must be last)",
                        ));
                    }
                    charged = charged.saturating_add(nar_chunk_charge(chunk.len()));
                    if charged >= MAX_NAR_SIZE {
                        return Err(Status::invalid_argument(format!(
                            "PutPath: cumulative charged permits {charged} exceed \
                             MAX_NAR_SIZE {MAX_NAR_SIZE} (too many tiny chunks)"
                        )));
                    }
                    let permit = self
                        .accumulate_chunk(&mut nar_data, &mut hasher, &chunk, "PutPath")
                        .await?;
                    held_permits.push(permit);
                }
                Some(put_path_request::Msg::Trailer(t)) => {
                    if trailer.is_some() {
                        return Err(Status::invalid_argument("PutPath: duplicate trailer"));
                    }
                    trailer = Some(t);
                    // Don't break — keep reading to catch chunk-after-trailer.
                }
                Some(put_path_request::Msg::Metadata(_)) => {
                    warn!(store_path = %info.store_path,
                          "PutPath: duplicate metadata mid-stream, rejecting");
                    return Err(Status::invalid_argument(
                        "PutPath stream contained duplicate metadata (protocol violation)",
                    ));
                }
                None => {}
            }
        }
        let t = trailer.ok_or_else(|| {
            Status::invalid_argument(
                "PutPath: no trailer received \
                 (PutPathTrailer is required as the last message)",
            )
        })?;
        apply_trailer(info, &t, "PutPath")?;
        verify_nar(
            hasher.finalize().into(),
            nar_data.len() as u64,
            info,
            "PutPath",
        )?;
        verify_ca_store_path(info, &nar_data, hmac_claims, "PutPath")?;
        Ok((nar_data, held_permits))
    }

    /// Sign + persist + emit success metrics for a single validated
    /// output. On `persist_nar` error the placeholder is `abort_upload`ed
    /// here; the caller's drop-guard spawn is then a harmless no-op.
    /// `info.store_path_hash` MUST be populated.
    // r[impl obs.metric.transfer-volume]
    pub(in crate::grpc) async fn finalize_single(
        &self,
        mut info: ValidatedPathInfo,
        claim: uuid::Uuid,
        nar_data: Vec<u8>,
        tenant_id: Option<uuid::Uuid>,
    ) -> Result<(), Status> {
        self.maybe_sign(tenant_id, &mut info).await;
        if let Err(e) = self.persist_nar(&info, claim, nar_data, "PutPath").await {
            self.abort_upload(&info.store_path_hash, claim).await;
            return Err(e);
        }
        metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
        metrics::counter!("rio_store_put_path_bytes_total").increment(info.nar_size);
        Ok(())
    }

    /// gRPC wrapper around [`ingest::claim_placeholder`]: adds the
    /// PutPath-specific result counters (`put_path_total{result=exists}`,
    /// `putpath_retries_total{reason=concurrent_upload}`) on top of the
    /// shared write-ahead core. `ctx_label` is unused now that the core
    /// emits its own log prefix; kept for call-site readability between
    /// PutPath and PutPathBatch.
    pub(in crate::grpc) async fn claim_placeholder(
        &self,
        store_path_hash: &[u8],
        store_path: &str,
        refs: &[String],
        _ctx_label: &str,
    ) -> Result<PlaceholderClaim, metadata::MetadataError> {
        let claim = ingest::claim_placeholder(
            &self.pool,
            self.chunk_backend.as_ref(),
            store_path_hash,
            store_path,
            refs,
            PUTPATH_HOOKS,
        )
        .await?;
        match &claim {
            PlaceholderClaim::AlreadyComplete => {
                metrics::counter!("rio_store_put_path_total", "result" => "exists").increment(1);
            }
            PlaceholderClaim::Concurrent => {
                metrics::counter!("rio_store_putpath_retries_total",
                    "reason" => "concurrent_upload")
                .increment(1);
            }
            PlaceholderClaim::Owned(_) => {}
        }
        Ok(claim)
    }

    /// gRPC wrapper around [`ingest::persist_nar`]: maps
    /// [`ingest::PersistError`] → `tonic::Status` with the
    /// PutPath-specific code mapping (`storage_error` for the chunked
    /// branch so `BackendAuthError` → `FailedPrecondition` and the
    /// builder fails fast instead of retrying forever;
    /// `putpath_metadata_status` for the inline branch so retriable PG
    /// errors get retriable codes + the `putpath_retries_total`
    /// counter).
    ///
    /// Returns `true` iff the chunked branch was taken (legacy — every
    /// current caller `abort_upload`s on error regardless, which is a
    /// safe no-op when chunked already rolled back).
    pub(in crate::grpc) async fn persist_nar(
        &self,
        info: &ValidatedPathInfo,
        claim: uuid::Uuid,
        nar_data: Vec<u8>,
        ctx_label: &str,
    ) -> Result<bool, Status> {
        let chunked = cas::should_chunk(self.chunk_backend.as_ref(), nar_data.len()).is_some();
        ingest::persist_nar(
            &self.pool,
            self.chunk_backend.as_ref(),
            info,
            claim,
            nar_data,
            self.chunk_upload_max_concurrent,
            PUTPATH_HOOKS,
        )
        .await
        .map_err(|e| match e {
            ingest::PersistError::Chunked(e) => storage_error(ctx_label, e),
            ingest::PersistError::Inline(e) => putpath_metadata_status(ctx_label, e),
        })?;
        Ok(chunked)
    }

    /// Batch-phase staging: for outputs ≥ [`cas::INLINE_THRESHOLD`],
    /// upload chunks + increment refcounts via [`cas::stage_chunked`]
    /// WITHOUT flipping `status='complete'`. Returns the
    /// [`NarPersist`] discriminant so the batch's atomic tx can pick
    /// the `inline_blob` arg to [`metadata::complete_manifest_in_conn`].
    ///
    /// On `stage_chunked` error this output's placeholder is already
    /// rolled back; the batch's [`PlaceholderGuard`]s handle other
    /// outputs' placeholders on Drop.
    pub(in crate::grpc) async fn stage_nar_for_batch(
        &self,
        info: &ValidatedPathInfo,
        claim: uuid::Uuid,
        nar_data: Vec<u8>,
    ) -> Result<NarPersist, Status> {
        if let Some(backend) = cas::should_chunk(self.chunk_backend.as_ref(), nar_data.len()) {
            let stats = cas::stage_chunked(
                &self.pool,
                backend,
                info,
                claim,
                &nar_data,
                self.chunk_upload_max_concurrent,
            )
            .await
            .map_err(|e| storage_error("PutPathBatch: stage_chunked", e))?;
            metrics::gauge!("rio_store_chunk_dedup_ratio").set(stats.dedup_ratio());
            Ok(NarPersist::ChunkedStaged)
        } else {
            Ok(NarPersist::Inline(Bytes::from(nar_data)))
        }
    }
}

/// Locate the contents of a single-regular-file NAR inside `nar`,
/// returning `(offset, length)` of the file bytes without copying them
/// and without the general-purpose parser's per-file size cap.
///
/// The framing is validated strictly: magic, `regular` type, zero
/// padding, a closing parenthesis, and no trailing bytes. Anything
/// else — directories, symlinks, truncation — is an error.
///
/// The `executable` marker is rejected: the flat content hash is over
/// the file bytes only, so the executable and non-executable variants
/// of the same content would otherwise verify against the same
/// content-derived path, and every legitimate flat producer (CppNix,
/// the builder's flat shape rules in `verify_fod_hashes` /
/// `finalize_floating_ca`) refuses to mint a flat output that is
/// executable in the first place.
fn single_file_nar_content_range(nar: &[u8]) -> Result<(usize, u64), String> {
    fn read_u64(nar: &[u8], pos: &mut usize) -> Result<u64, String> {
        let end = pos
            .checked_add(8)
            .ok_or_else(|| "length field overflows".to_string())?;
        let bytes = nar
            .get(*pos..end)
            .ok_or_else(|| "truncated NAR: expected a length field".to_string())?;
        *pos = end;
        Ok(u64::from_le_bytes(bytes.try_into().expect("8-byte slice")))
    }
    fn read_token<'a>(nar: &'a [u8], pos: &mut usize) -> Result<&'a [u8], String> {
        let len = read_u64(nar, pos)?;
        let len = usize::try_from(len).map_err(|_| "token length overflows".to_string())?;
        let end = pos
            .checked_add(len)
            .ok_or_else(|| "token length overflows".to_string())?;
        let tok = nar
            .get(*pos..end)
            .ok_or_else(|| "truncated NAR: token".to_string())?;
        *pos = end;
        let pad = (8 - len % 8) % 8;
        let pad_end = pos
            .checked_add(pad)
            .ok_or_else(|| "padding overflows".to_string())?;
        let padding = nar
            .get(*pos..pad_end)
            .ok_or_else(|| "truncated NAR: token padding".to_string())?;
        if padding.iter().any(|b| *b != 0) {
            return Err("non-zero token padding".to_string());
        }
        *pos = pad_end;
        Ok(tok)
    }
    fn expect(nar: &[u8], pos: &mut usize, want: &[u8]) -> Result<(), String> {
        let tok = read_token(nar, pos)?;
        if tok != want {
            return Err(format!(
                "expected `{}`, found `{}`",
                std::str::from_utf8(want).unwrap_or("<non-utf8>"),
                std::str::from_utf8(tok).unwrap_or("<non-utf8>")
            ));
        }
        Ok(())
    }

    let mut pos = 0usize;
    expect(nar, &mut pos, b"nix-archive-1")?;
    expect(nar, &mut pos, b"(")?;
    expect(nar, &mut pos, b"type")?;
    let ty = read_token(nar, &mut pos)?;
    if ty != b"regular" {
        return Err(format!(
            "not a regular file (type `{}`)",
            std::str::from_utf8(ty).unwrap_or("<non-utf8>")
        ));
    }
    let tok = read_token(nar, &mut pos)?;
    if tok == b"executable" {
        return Err(
            "executable single-file NARs are not valid flat content-addressed outputs \
             (the flat hash ignores the bit; CppNix rejects this shape)"
                .to_string(),
        );
    }
    if tok != b"contents" {
        return Err(format!(
            "expected `contents`, found `{}`",
            std::str::from_utf8(tok).unwrap_or("<non-utf8>")
        ));
    }
    let len = read_u64(nar, &mut pos)?;
    let len_usize = usize::try_from(len).map_err(|_| "content length overflows".to_string())?;
    let offset = pos;
    let content_end = offset
        .checked_add(len_usize)
        .ok_or_else(|| "content length overflows".to_string())?;
    if nar.len() < content_end {
        return Err("truncated NAR: contents".to_string());
    }
    let pad = (8 - len_usize % 8) % 8;
    let pad_end = content_end
        .checked_add(pad)
        .ok_or_else(|| "content padding overflows".to_string())?;
    let padding = nar
        .get(content_end..pad_end)
        .ok_or_else(|| "truncated NAR: content padding".to_string())?;
    if padding.iter().any(|b| *b != 0) {
        return Err("non-zero content padding".to_string());
    }
    let mut tail = pad_end;
    expect(nar, &mut tail, b")")?;
    if tail != nar.len() {
        return Err("trailing bytes after the single-file NAR".to_string());
    }
    Ok((offset, len))
}

// r[verify sec.drv.validate]
// r[verify store.integrity.verify-on-put]
#[cfg(test)]
mod verify_nar_tests {
    use super::*;
    use rio_test_support::fixtures::{make_path_info_for_nar, test_drv_path, test_store_path};

    fn digest(d: &[u8]) -> [u8; 32] {
        Sha256::digest(d).into()
    }

    #[test]
    fn verify_nar_size_and_hash() {
        let data = b"valid nar data";
        let info = make_path_info_for_nar(&test_store_path("v"), data);
        assert!(verify_nar(digest(data), data.len() as u64, &info, "t").is_ok());

        let e = verify_nar(digest(b"short"), 5, &info, "t").unwrap_err();
        assert!(e.message().contains("size mismatch"), "got: {e:?}");

        let e = verify_nar(digest(b"different data"), data.len() as u64, &info, "t").unwrap_err();
        assert!(e.message().contains("hash mismatch"), "got: {e:?}");
    }

    /// Build the canonical NAR of a regular file with the given contents.
    fn nar_of_file(contents: &[u8]) -> Vec<u8> {
        let node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: contents.to_vec(),
        };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node).expect("in-memory NAR serialize");
        nar
    }

    fn ca_claims() -> rio_auth::hmac::AssignmentClaims {
        rio_auth::hmac::AssignmentClaims {
            executor_id: "test-executor".into(),
            drv_hash: "test-drv-hash".into(),
            expected_outputs: vec![String::new()],
            is_ca: true,
            expiry_unix: u64::MAX,
            tenant: None,
        }
    }

    /// Construct a ValidatedPathInfo claiming `path` with `references`,
    /// nar_hash = SHA-256(nar).
    fn ca_info(
        path: &rio_nix::store_path::StorePath,
        references: &[rio_nix::store_path::StorePath],
        nar: &[u8],
    ) -> ValidatedPathInfo {
        let mut info = make_path_info_for_nar(path.as_str(), nar);
        info.references = references.to_vec();
        info
    }

    /// The genuine content-derived path of a SELF-REFERENCING CA output:
    /// fixed-point construction — content embeds the scratch path, the
    /// final path is derived from the content with the scratch hash
    /// zeroed, then the content is rewritten scratch→final. The
    /// underlying primitives are golden-tested against real `nix` in
    /// rio-nix (`tests/ca_golden.rs`); these tests pin the GATE's
    /// behavior (declared-self detection, modulus choice, rejection).
    fn build_self_referencing_upload(name: &str) -> (rio_nix::store_path::StorePath, Vec<u8>) {
        use std::io::Write as _;
        let drv = rio_nix::store_path::StorePath::parse(&test_drv_path(name)).unwrap();
        let scratch =
            rio_nix::store_path::StorePath::make_scratch_output_path(&drv, "out").unwrap();
        let content_at_scratch = format!("I live at {}\n", scratch.as_str()).into_bytes();
        let nar_at_scratch = nar_of_file(&content_at_scratch);

        let mut sink =
            rio_nix::ca::HashModuloSink::new(rio_nix::hash::HashAlgo::SHA256, &scratch.hash_part());
        sink.write_all(&nar_at_scratch).unwrap();
        let (modulo, _) = sink.finish();
        let final_path = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            name,
            &modulo,
            true,
            &[],
            true,
        )
        .unwrap();

        // Rewrite scratch→final in the content (same-length hash parts).
        let final_content = String::from_utf8(content_at_scratch)
            .unwrap()
            .replace(&scratch.hash_part(), &final_path.hash_part())
            .into_bytes();
        (final_path, nar_of_file(&final_content))
    }

    #[test]
    fn ca_self_reference_accepted_and_wrong_path_rejected() {
        let (path, nar) = build_self_referencing_upload("selfref-gate");
        let info = ca_info(&path, std::slice::from_ref(&path), &nar);
        verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t")
            .expect("genuine self-referencing upload must pass");

        // Same NAR claimed under a different (well-formed) path: the
        // modulus changes, the recomputed path can't match the claim.
        let other = rio_nix::store_path::StorePath::parse(&test_store_path("imposter")).unwrap();
        let info = ca_info(&other, std::slice::from_ref(&other), &nar);
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "got: {e:?}");
    }

    #[test]
    fn ca_undeclared_self_reference_rejected() {
        // Content embeds the claimed path but `references` does not
        // declare it: the gate must use the plain NAR hash (no modulo),
        // which cannot reproduce the claimed path.
        let (path, nar) = build_self_referencing_upload("selfref-undeclared");
        let info = ca_info(&path, &[], &nar);
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "got: {e:?}");
    }

    /// Discarded self-reference (structured-attrs `unsafeDiscardReferences`):
    /// the bytes embed the claimed path, `references` is empty, and the
    /// `fixed:` descriptor carries the modulo hash — the shape both CppNix
    /// and the native builder produce. The gate must accept it, and still
    /// reject the same NAR claimed under a different path.
    #[test]
    fn ca_discarded_self_reference_with_descriptor_accepted() {
        use std::io::Write as _;
        let drv =
            rio_nix::store_path::StorePath::parse(&test_drv_path("selfref-discarded")).unwrap();
        let scratch =
            rio_nix::store_path::StorePath::make_scratch_output_path(&drv, "out").unwrap();
        let content_at_scratch = format!("I live at {}\n", scratch.as_str()).into_bytes();
        let nar_at_scratch = nar_of_file(&content_at_scratch);
        let mut sink =
            rio_nix::ca::HashModuloSink::new(rio_nix::hash::HashAlgo::SHA256, &scratch.hash_part());
        sink.write_all(&nar_at_scratch).unwrap();
        let (modulo, _) = sink.finish();
        // Self flag OFF: the references were discarded.
        let final_path = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            "selfref-discarded",
            &modulo,
            true,
            &[],
            false,
        )
        .unwrap();
        let final_content = String::from_utf8(content_at_scratch)
            .unwrap()
            .replace(&scratch.hash_part(), &final_path.hash_part())
            .into_bytes();
        let nar = nar_of_file(&final_content);

        let mut info = ca_info(&final_path, &[], &nar);
        info.content_address = Some(format!("fixed:r:{}", modulo.to_colon()));
        verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t")
            .expect("discarded-self upload with a modulo descriptor must pass");

        // Same NAR + descriptor claimed under a different path: the
        // fallback modulus changes, nothing matches, still rejected.
        let other = rio_nix::store_path::StorePath::parse(&test_store_path("imposter2")).unwrap();
        let mut info = ca_info(&other, &[], &nar);
        info.content_address = Some(format!("fixed:r:{}", modulo.to_colon()));
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "got: {e:?}");
    }

    /// Self-embedding content (so the fallback hash-modulo recompute is
    /// exercised) with a LYING `fixed:` descriptor: neither the plain nor
    /// the modulo recompute matches the declared hash, so the gate must
    /// reject for the descriptor mismatch — not fall through to a path
    /// derivation that could mask the lie.
    #[test]
    fn ca_discarded_self_descriptor_lie_rejected() {
        let (path, nar) = build_self_referencing_upload("selfref-desc-lie");
        // References empty (discarded-self shape) so the plain hash cannot
        // match a modulo descriptor, forcing the fallback recompute path.
        let mut info = ca_info(&path, &[], &nar);
        let other_digest: [u8; 32] = Sha256::digest(b"not the uploaded bytes").into();
        let other_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, other_digest.to_vec())
                .unwrap();
        info.content_address = Some(format!("fixed:r:{}", other_hash.to_colon()));
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "got: {e:?}");
        assert!(
            e.message().contains("descriptor does not match"),
            "rejection must name the descriptor mismatch, got: {}",
            e.message()
        );
    }

    /// Flat verification has no 256 MiB ceiling: a single-file NAR whose
    /// contents exceed the general-purpose parser's per-file cap still
    /// verifies (the store's own upload limit is the only ceiling).
    #[test]
    fn ca_flat_contents_beyond_the_parser_cap_accepted() {
        fn push_tok(buf: &mut Vec<u8>, tok: &[u8]) {
            buf.extend_from_slice(&(tok.len() as u64).to_le_bytes());
            buf.extend_from_slice(tok);
            buf.resize(buf.len() + (8 - tok.len() % 8) % 8, 0);
        }
        // 256 MiB + 9 bytes of zeros, framed by hand so the test does not
        // depend on the writer (or pay for a second copy).
        let len: usize = 256 * 1024 * 1024 + 9;
        let mut nar = Vec::with_capacity(len + 192);
        for t in [
            b"nix-archive-1".as_slice(),
            b"(",
            b"type",
            b"regular",
            b"contents",
        ] {
            push_tok(&mut nar, t);
        }
        nar.extend_from_slice(&(len as u64).to_le_bytes());
        let contents_start = nar.len();
        nar.resize(nar.len() + len, 0);
        nar.resize(nar.len() + (8 - len % 8) % 8, 0);
        push_tok(&mut nar, b")");

        let digest: [u8; 32] = Sha256::digest(&nar[contents_start..contents_start + len]).into();
        let flat_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest.to_vec()).unwrap();
        let path =
            rio_nix::store_path::StorePath::make_fixed_output("flat-big", &flat_hash, false, &[])
                .unwrap();
        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(format!("fixed:{}", flat_hash.to_colon()));
        verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t")
            .expect("flat upload larger than the old parser cap must pass");
    }

    #[test]
    fn single_file_nar_content_range_validates_framing() {
        // Round-trip against the writer for the non-executable shape.
        let node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: b"hello".to_vec(),
        };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node).unwrap();
        let (off, len) = single_file_nar_content_range(&nar).expect("valid single-file NAR");
        assert_eq!(&nar[off..off + len as usize], b"hello");
        // The executable marker is rejected: the flat hash ignores the
        // bit, so accepting it would let two different NARs verify
        // against one content-derived path (CppNix rejects the shape).
        let node = rio_nix::nar::NarNode::Regular {
            executable: true,
            contents: b"hello".to_vec(),
        };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node).unwrap();
        let err = single_file_nar_content_range(&nar).unwrap_err();
        assert!(err.contains("executable"), "got: {err}");
        // Directory NARs are not single files.
        let dir = rio_nix::nar::NarNode::Directory { entries: vec![] };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &dir).unwrap();
        assert!(single_file_nar_content_range(&nar).is_err());
        // Trailing garbage is rejected.
        let mut nar = nar_of_file(b"x");
        nar.extend_from_slice(b"garbage!");
        assert!(single_file_nar_content_range(&nar).is_err());
        // Truncation is rejected.
        let nar = nar_of_file(b"some contents here");
        assert!(single_file_nar_content_range(&nar[..nar.len() - 4]).is_err());
    }

    #[test]
    fn ca_non_self_referencing_unchanged() {
        // No self-reference: expected path = make_fixed_output over the
        // plain NAR hash (the pre-existing behavior).
        let nar = nar_of_file(b"no self references here\n");
        let digest: [u8; 32] = Sha256::digest(&nar).into();
        let hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest.to_vec()).unwrap();
        let path = rio_nix::store_path::StorePath::make_fixed_output("plain-ca", &hash, true, &[])
            .unwrap();
        let info = ca_info(&path, &[], &nar);
        verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").expect("plain CA must pass");

        // And a wrong claim is still rejected.
        let other = rio_nix::store_path::StorePath::parse(&test_store_path("wrong")).unwrap();
        let info = ca_info(&other, &[], &nar);
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied);
    }

    #[test]
    fn ca_gate_noop_without_ca_claims() {
        let nar = nar_of_file(b"whatever\n");
        let path = rio_nix::store_path::StorePath::parse(&test_store_path("ia-path")).unwrap();
        let info = ca_info(&path, &[], &nar);
        // No claims at all.
        verify_ca_store_path(&info, &nar, None, "t").expect("no claims = no-op");
        // Claims with is_ca = false.
        let mut claims = ca_claims();
        claims.is_ca = false;
        verify_ca_store_path(&info, &nar, Some(&claims), "t").expect("non-CA claims = no-op");
    }

    /// Flat-method floating CA (`fixed:sha256:…`): the expected path is
    /// derived from the hash of the FILE bytes, not the NAR — the gate
    /// must honor the declared method instead of assuming `r:sha256`.
    #[test]
    fn ca_flat_descriptor_accepted_and_method_mismatch_rejected() {
        let contents = b"flat ca payload\n";
        let digest: [u8; 32] = Sha256::digest(contents).into();
        let flat_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest.to_vec()).unwrap();
        let path =
            rio_nix::store_path::StorePath::make_fixed_output("flat-ca", &flat_hash, false, &[])
                .unwrap();
        let nar = nar_of_file(contents);

        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(format!("fixed:{}", flat_hash.to_colon()));
        verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t")
            .expect("flat floating-CA upload with a matching descriptor must pass");

        // The same content claimed under the recursive-sha256-derived
        // path while declaring the flat method: the recompute follows
        // the declared (flat) method, so the claim cannot match.
        let nar_digest: [u8; 32] = Sha256::digest(&nar).into();
        let nar_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, nar_digest.to_vec())
                .unwrap();
        let recursive_path =
            rio_nix::store_path::StorePath::make_fixed_output("flat-ca", &nar_hash, true, &[])
                .unwrap();
        let mut info = ca_info(&recursive_path, &[], &nar);
        info.content_address = Some(format!("fixed:{}", flat_hash.to_colon()));
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "got: {e:?}");
    }

    /// Recursive non-SHA-256 floating CA (`fixed:r:sha512:…`): accepted
    /// when the path is derived with the declared algorithm; the same
    /// upload claimed under an unrelated path is rejected.
    #[test]
    fn ca_recursive_sha512_descriptor_accepted() {
        use std::io::Write as _;
        let nar = nar_of_file(b"sha512-hashed nar payload\n");
        let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA512);
        w.write_all(&nar).unwrap();
        let nar_sha512 = w.finish();
        let path = rio_nix::store_path::StorePath::make_fixed_output(
            "r-sha512-ca",
            &nar_sha512,
            true,
            &[],
        )
        .unwrap();

        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(format!("fixed:r:{}", nar_sha512.to_colon()));
        verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t")
            .expect("r:sha512 floating-CA upload with a matching descriptor must pass");

        let other = rio_nix::store_path::StorePath::parse(&test_store_path("imposter512")).unwrap();
        let mut info = ca_info(&other, &[], &nar);
        info.content_address = Some(format!("fixed:r:{}", nar_sha512.to_colon()));
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "got: {e:?}");
    }

    /// A descriptor whose hash does not match the uploaded bytes is
    /// rejected even when the claimed path is consistent with the
    /// content — the descriptor is persisted/served and must not lie.
    #[test]
    fn ca_descriptor_hash_mismatch_rejected() {
        let nar = nar_of_file(b"descriptor mismatch payload\n");
        let digest: [u8; 32] = Sha256::digest(&nar).into();
        let nar_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest.to_vec()).unwrap();
        // Path genuinely derived from the content (r:sha256)…
        let path =
            rio_nix::store_path::StorePath::make_fixed_output("desc-lie", &nar_hash, true, &[])
                .unwrap();
        // …but the descriptor declares a different hash.
        let other_digest: [u8; 32] = Sha256::digest(b"other bytes").into();
        let other_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, other_digest.to_vec())
                .unwrap();
        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(format!("fixed:r:{}", other_hash.to_colon()));
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "got: {e:?}");
    }

    /// An executable single-file NAR claimed as a flat CA upload is
    /// rejected: the flat hash ignores the executable bit, so accepting
    /// it would register the executable variant at the path honest
    /// producers only ever mint for the non-executable bytes.
    #[test]
    fn ca_flat_executable_nar_rejected() {
        let contents = b"#!/bin/sh\necho pwned\n";
        let digest: [u8; 32] = Sha256::digest(contents).into();
        let flat_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest.to_vec()).unwrap();
        let path =
            rio_nix::store_path::StorePath::make_fixed_output("flat-exec", &flat_hash, false, &[])
                .unwrap();
        let node = rio_nix::nar::NarNode::Regular {
            executable: true,
            contents: contents.to_vec(),
        };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node).unwrap();
        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(format!("fixed:{}", flat_hash.to_colon()));
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "got: {e:?}");
        assert!(e.message().contains("executable"), "got: {}", e.message());
    }

    /// Flat ingestion cannot carry references or self-references; a
    /// flat-declared upload with references is rejected outright.
    #[test]
    fn ca_flat_with_references_rejected() {
        let contents = b"flat with refs\n";
        let digest: [u8; 32] = Sha256::digest(contents).into();
        let flat_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest.to_vec()).unwrap();
        let path =
            rio_nix::store_path::StorePath::make_fixed_output("flat-refs", &flat_hash, false, &[])
                .unwrap();
        let nar = nar_of_file(contents);
        let dep = rio_nix::store_path::StorePath::parse(&test_store_path("some-dep")).unwrap();
        let mut info = ca_info(&path, std::slice::from_ref(&dep), &nar);
        info.content_address = Some(format!("fixed:{}", flat_hash.to_colon()));
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::InvalidArgument, "got: {e:?}");
    }

    /// Incremental hashing through `accumulate_chunk` produces the same
    /// digest as one-shot `Sha256::digest`. Structural — not timing-based.
    #[tokio::test]
    async fn verify_nar_incremental_matches_oneshot() {
        let svc = StoreServiceImpl::new(
            sqlx::PgPool::connect_lazy("postgres://unused").expect("lazy pool"),
        );
        let chunks: &[&[u8]] = &[b"first ", b"second ", b"third"];
        let mut buf = Vec::new();
        let mut hasher = Sha256::new();
        let mut permits = Vec::new();
        for c in chunks {
            permits.push(
                svc.accumulate_chunk(&mut buf, &mut hasher, c, "t")
                    .await
                    .expect("chunk under bounds"),
            );
        }
        let incremental: [u8; 32] = hasher.finalize().into();
        assert_eq!(incremental, digest(&buf));
        assert_eq!(buf, b"first second third");
    }
}

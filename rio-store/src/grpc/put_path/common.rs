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

/// Verify that a `.drv` upload is the text content-address of its bytes.
///
/// CppNix mints every derivation path as
/// `makeTextPath(name, sha256(text), inputSrcs ∪ inputDrvs)` and an
/// untrusted `nix-daemon` client can only add an unsigned path when it
/// is genuinely content-addressed; rio's multi-tenant gateway clients
/// (and any worker relay) are the analogue, so the store enforces the
/// same invariant at ingestion: the claimed path MUST equal
/// `make_text(name, sha256(single-file contents), declared references)`.
/// This binds the registered bytes to the path for every caller —
/// including service-token relays — so the derivation a gateway
/// validated at submission is byte-identical to the one a worker later
/// fetches from the store (`gw.dag.drv-cache-text-ca` is the cache-side
/// half of the same invariant).
///
/// Non-`.drv` paths are untouched. Error split follows the existing
/// gate convention: a NAR that is not a single regular file is
/// `InvalidArgument`; a well-formed upload whose path does not derive
/// from its bytes is `PermissionDenied`.
// r[impl store.put.drv-text-ca]
pub(in crate::grpc) fn verify_drv_text_path(
    info: &ValidatedPathInfo,
    nar_data: &[u8],
    ctx_label: &str,
) -> Result<(), Status> {
    if !info.store_path.is_derivation() {
        return Ok(());
    }
    let (off, len) = single_file_nar_content_range(nar_data).map_err(|e| {
        Status::invalid_argument(format!(
            "{ctx_label}: a .drv upload must be a single regular-file NAR: {e}"
        ))
    })?;
    let len = usize::try_from(len).map_err(|_| {
        Status::invalid_argument(format!(
            "{ctx_label}: .drv content length does not fit this platform"
        ))
    })?;
    let file_bytes = &nar_data[off..off + len];
    use std::io::Write as _;
    let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
    w.write_all(file_bytes)
        .map_err(|e| Status::internal(format!("{ctx_label}: .drv hash: {e}")))?;
    let hash = w.finish();
    let expected =
        rio_nix::store_path::StorePath::make_text(info.store_path.name(), &hash, &info.references)
            .map_err(|e| {
                Status::invalid_argument(format!(
                    "{ctx_label}: cannot derive the text content-address: {e}"
                ))
            })?;
    if expected != info.store_path {
        warn!(
            claimed = %info.store_path,
            derived = %expected,
            "{ctx_label}: .drv path is not the text content-address of its bytes"
        );
        return Err(Status::permission_denied(format!(
            "{ctx_label}: .drv path {} is not the text content-address of the uploaded \
             bytes with the declared references (derived {})",
            info.store_path, expected
        )));
    }
    Ok(())
}

/// Parsed `fixed:` content-address descriptor of a build-output upload
/// (floating-CA or fixed-output): ingestion method (`r:` prefix →
/// recursive/NAR, otherwise flat) plus the declared content hash,
/// exactly as the builder records it (`fixed:[r:]<algo>:<hash>`).
struct FixedCaDescriptor {
    recursive: bool,
    hash: rio_nix::hash::NixHash,
}

/// Parse `info.content_address` for the CA gate. `text:` descriptors
/// (store-added .drv files) and anything else that is not a `fixed:`
/// descriptor are rejected — a worker-uploaded *build output* (floating
/// CA or fixed-output) always carries `fixed:`; text paths are added by
/// the trusted control plane via service tokens, which never reach this
/// gate.
fn parse_fixed_ca_descriptor(s: &str, ctx_label: &str) -> Result<FixedCaDescriptor, Status> {
    let rest = s.strip_prefix("fixed:").ok_or_else(|| {
        Status::invalid_argument(format!(
            "{ctx_label}: unsupported content-address descriptor {s:?} on a build-output upload \
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

/// The worker-upload classes this gate distinguishes, derived ONCE at
/// gate entry from the **scheduler-signed token kind** crossed with the
/// upload's own descriptor (pattern R5: the population × producer cross
/// product is decided here, per arm, at compile-forced exhaustiveness —
/// not rediscovered shape-by-shape inside the verification flow).
///
/// The obligation set is selected by the CLASS, never by incidental
/// shape: the hash-modulo arithmetic ([`rio_nix::ca::HashModuloSink`])
/// is constructible only inside the floating-CA arm, because its
/// soundness theorem ("the claimed path derives FROM the modulo hash")
/// holds for floating tokens only — re-routing a declared-hash class
/// through it is the splice forgery (merged_bug_076).
enum UploadClass {
    /// `claims.is_ca`: the path is content-derived; membership was
    /// skipped at sign time. Descriptor optional (descriptor-less
    /// floating uploads keep the historical recursive-SHA256
    /// assumption).
    FloatingCa {
        descriptor: Option<FixedCaDescriptor>,
    },
    /// `claims.is_fixed_output`: the path derives from the derivation's
    /// DECLARED hash. The `fixed:` descriptor is mandatory (the trigger
    /// is the trusted-plane bit, never the worker's own claim).
    FixedOutput { descriptor: FixedCaDescriptor },
    /// Plain input-addressed: the path is not content-derived;
    /// authorization is `expected_outputs` membership plus the deriver
    /// proof. A volunteered descriptor is verified against the bytes
    /// (the persisted/served `CA:` field must not lie) but NEVER used
    /// to re-derive the path.
    InputAddressed {
        descriptor: Option<FixedCaDescriptor>,
    },
}

/// Classify a signed worker upload into its [`UploadClass`].
///
/// `is_ca ∧ is_fixed_output` is a malformed token (the scheduler signs
/// exactly one kind; `AssignmentClaims::is_ca` is documented as
/// `is_ca && !is_fixed_output` at the signer) and is rejected loudly
/// instead of silently resolving to whichever class the old
/// shape-dispatch happened to reach first.
fn classify_upload(
    claims: &rio_auth::hmac::AssignmentClaims,
    info: &ValidatedPathInfo,
    ctx_label: &str,
) -> Result<UploadClass, Status> {
    if claims.is_ca && claims.is_fixed_output {
        warn!(
            store_path = %info.store_path,
            executor_id = %claims.executor_id,
            drv_hash = %claims.drv_hash,
            "{ctx_label}: assignment claims mark the upload both floating-CA and fixed-output"
        );
        metrics::counter!(
            "rio_store_hmac_rejected_total",
            "reason" => "claims_kind_conflict"
        )
        .increment(1);
        return Err(Status::invalid_argument(format!(
            "{ctx_label}: assignment claims mark the upload both floating-CA and \
             fixed-output; the token kind is trusted-plane data and must name \
             exactly one class"
        )));
    }
    let descriptor = info
        .content_address
        .as_deref()
        .map(|s| parse_fixed_ca_descriptor(s, ctx_label))
        .transpose()?;
    if claims.is_ca {
        Ok(UploadClass::FloatingCa { descriptor })
    } else if claims.is_fixed_output {
        match descriptor {
            Some(descriptor) => Ok(UploadClass::FixedOutput { descriptor }),
            None => {
                // The expected output is content-bound by the
                // derivation's declared hash, so a descriptor-less
                // upload would skip the only server-side content⇔path
                // verification a FOD gets. Workers are untrusted — the
                // trigger for verification must be the signed claims,
                // not the worker's own descriptor — so reject instead
                // of falling back to membership-only acceptance.
                warn!(
                    store_path = %info.store_path,
                    executor_id = %claims.executor_id,
                    drv_hash = %claims.drv_hash,
                    "{ctx_label}: fixed-output upload without a content-address descriptor"
                );
                metrics::counter!(
                    "rio_store_hmac_rejected_total",
                    "reason" => "fod_descriptor_missing"
                )
                .increment(1);
                Err(Status::permission_denied(format!(
                    "{ctx_label}: fixed-output upload must carry a `fixed:` \
                     content-address descriptor so the store can verify the \
                     content against the path"
                )))
            }
        }
    } else {
        Ok(UploadClass::InputAddressed { descriptor })
    }
}

// r[impl sec.authz.ca-path-derived+9]
/// Content-address authorization gate. Workers are untrusted, so the
/// store — not the builder — is the authority on whether a claimed
/// store path is actually derivable from the uploaded bytes. The gate
/// dispatches on [`UploadClass`] — the obligation set is selected by
/// the signed token kind, per arm:
///
/// - **Floating-CA** ([`UploadClass::FloatingCa`]):
///   [`validate_put_metadata`] skipped the `store_path ∈
///   expected_outputs` check (the path isn't known at sign time). This
///   is the replacement gate: recompute the CA store path SERVER-SIDE
///   from the NAR that [`verify_nar`] just confirmed, and reject if it
///   doesn't match `info.store_path`. A worker holding an `is_ca=true`
///   token therefore cannot upload to any path that isn't the
///   content-derived path of the NAR it sent.
/// - **Fixed-output** ([`UploadClass::FixedOutput`]): the path already
///   passed the `expected_outputs` membership check, but membership
///   only proves the *scheduler* expected this path — not that the
///   bytes match the derivation's declared hash. The descriptor hash is
///   cross-checked against the server recompute by PLAIN equality (no
///   modulo arithmetic — see [`UploadClass`]) and the claimed path must
///   re-derive from it. The builder-side `verify_fod_hashes` /
///   declared-path binding are defense-in-depth only.
/// - **Input-addressed** ([`UploadClass::InputAddressed`]): an
///   input-addressed path is not content-derived, so there is no path
///   claim to verify here (membership + the deriver proof own
///   authorization). A volunteered descriptor is still verified against
///   the bytes so the persisted/served `CA:` field can never lie, but
///   the path is NEVER re-derived from it.
///
/// The ingestion method comes from the upload's own `fixed:` content
/// address descriptor (`info.content_address`), exactly as the
/// builder records it (`finalize_floating_ca` for floating-CA,
/// `populate_fixed_output_descriptors` for declared-hash FODs):
/// `r:`-prefixed → recursive (NAR) hashing, otherwise flat (single
/// regular file) hashing, with sha1/sha256/sha512 all accepted. The
/// declared descriptor hash is cross-checked against the server-side
/// recompute, so the persisted/served `CA:` field can never lie about
/// the bytes. `is_ca` uploads without a descriptor fall back to the
/// historical recursive-SHA256 assumption.
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
/// hash **on a floating-CA token** (`claims.is_ca`), the gate retries
/// once with the hash modulo the claimed path's own hash part before
/// rejecting: structured-attrs `unsafeDiscardReferences` legitimately
/// produces floating uploads whose bytes embed their own path while
/// declaring no self-reference, and CppNix mints those paths from
/// exactly that modulo hash. Declared-hash uploads (fixed-output, or an
/// input-addressed claim volunteering a descriptor) get NO modulo
/// retry — their paths derive from the declared hash, so the retry
/// would admit spliced content whose plain hash differs from it.
///
/// `None` claims (dev mode / service-token bypass) → no-op: the
/// trusted control plane (gateway `nix copy` ingestion, store-added
/// text paths) is not subject to worker authorization.
///
/// Descriptor-less worker uploads are membership-only **unless** the
/// scheduler-signed claims mark the assignment as fixed-output
/// (`AssignmentClaims::is_fixed_output`): a FOD's expected output is
/// content-bound by the derivation's declared hash, so the store
/// requires the `fixed:` descriptor and rejects its absence — the
/// verification trigger is trusted-plane data, never the (untrusted)
/// worker's own claim.
///
/// Error split (deliberate, pre-existing pattern): a descriptor that
/// cannot be parsed at all is `InvalidArgument` (malformed request),
/// while content/path mismatches against a well-formed descriptor are
/// `PermissionDenied` (authorization failure).
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
    match classify_upload(claims, info, ctx_label)? {
        UploadClass::FloatingCa { descriptor } => {
            verify_floating_ca(info, nar_data, descriptor, claims, ctx_label)
        }
        UploadClass::FixedOutput { descriptor } => {
            verify_fixed_output(info, nar_data, descriptor, claims, ctx_label)
        }
        UploadClass::InputAddressed {
            descriptor: Some(descriptor),
        } => verify_ia_volunteered_descriptor(info, nar_data, descriptor, claims, ctx_label),
        // Descriptor-less input-addressed (and daemon-era / legacy
        // worker) uploads: path authorization is the `store_path ∈
        // expected_outputs` membership check in `validate_put_metadata`
        // plus the deriver proof, and there is no content-address claim
        // to verify — an input-addressed path is not content-derived.
        // Descriptor-less non-FOD uploads MUST keep working exactly
        // like this (legacy workers / daemon-era replays).
        UploadClass::InputAddressed { descriptor: None } => Ok(()),
    }
}

/// References split shared by every descriptor-verifying arm: the path
/// under construction may appear in its own reference set (declared
/// self-reference). It is expressed via the `:self` fingerprint token,
/// never as an ordinary reference.
fn split_self_reference(info: &ValidatedPathInfo) -> (Vec<rio_nix::store_path::StorePath>, bool) {
    let refs: Vec<rio_nix::store_path::StorePath> = info
        .references
        .iter()
        .filter(|r| r.as_str() != info.store_path.as_str())
        .cloned()
        .collect();
    let has_self_reference = refs.len() != info.references.len();
    (refs, has_self_reference)
}

/// Locate the single regular file inside a flat-ingestion NAR. Shared
/// by the flat verification arms; hashes nothing itself.
fn flat_file_bytes<'a>(nar_data: &'a [u8], ctx_label: &str) -> Result<&'a [u8], Status> {
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
    Ok(&nar_data[off..off + len])
}

/// Plain (non-modulo) content hash for a recursive (NAR) ingestion.
/// SHA-256 reuses the server-computed `info.nar_hash` (already verified
/// equal to the stream); other algorithms re-hash the buffered NAR.
fn recursive_plain_hash(
    info: &ValidatedPathInfo,
    nar_data: &[u8],
    algo: rio_nix::hash::HashAlgo,
    ctx_label: &str,
) -> Result<rio_nix::hash::NixHash, Status> {
    use std::io::Write as _;
    if algo == rio_nix::hash::HashAlgo::SHA256 {
        rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, info.nar_hash.to_vec())
            .map_err(|e| Status::internal(format!("{ctx_label}: nar_hash construct: {e}")))
    } else {
        let mut w = rio_nix::ca::HashWriter::new(algo);
        w.write_all(nar_data)
            .map_err(|e| Status::internal(format!("{ctx_label}: nar hash ({algo}): {e}")))?;
        Ok(w.finish())
    }
}

/// Plain content hash over the bytes of a flat ingestion's single file.
fn flat_plain_hash(
    file_bytes: &[u8],
    algo: rio_nix::hash::HashAlgo,
    ctx_label: &str,
) -> Result<rio_nix::hash::NixHash, Status> {
    use std::io::Write as _;
    let mut w = rio_nix::ca::HashWriter::new(algo);
    w.write_all(file_bytes)
        .map_err(|e| Status::internal(format!("{ctx_label}: flat hash ({algo}): {e}")))?;
    Ok(w.finish())
}

/// Reject a well-formed descriptor whose hash does not match the bytes
/// actually sent. The descriptor is persisted and served to
/// substituting clients (narinfo `CA:`), so it must not lie even when
/// the claimed path is otherwise consistent.
fn descriptor_mismatch(
    info: &ValidatedPathInfo,
    declared: &rio_nix::hash::NixHash,
    computed: &rio_nix::hash::NixHash,
    claims: &rio_auth::hmac::AssignmentClaims,
    ctx_label: &str,
) -> Status {
    warn!(
        store_path = %info.store_path,
        declared = %declared.to_colon(),
        computed = %computed.to_colon(),
        executor_id = %claims.executor_id,
        "{ctx_label}: content-address descriptor does not match the uploaded NAR"
    );
    metrics::counter!(
        "rio_store_hmac_rejected_total",
        "reason" => "ca_descriptor_mismatch"
    )
    .increment(1);
    Status::permission_denied(format!(
        "{ctx_label}: content-address descriptor does not match the uploaded content"
    ))
}

/// Derive the expected CA path from the verified content hash and
/// compare it to the claimed path. Shared by the two content-derived
/// arms (floating-CA and fixed-output) — never called for
/// input-addressed uploads.
fn require_content_derived_path(
    info: &ValidatedPathInfo,
    content_hash: &rio_nix::hash::NixHash,
    recursive: bool,
    refs: &[rio_nix::store_path::StorePath],
    has_self_reference: bool,
    claims: &rio_auth::hmac::AssignmentClaims,
    ctx_label: &str,
) -> Result<(), Status> {
    let expected = rio_nix::store_path::StorePath::make_fixed_output_with_self(
        info.store_path.name(),
        content_hash,
        recursive,
        refs,
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

/// Floating-CA arm. The ONLY production constructor of
/// [`rio_nix::ca::HashModuloSink`] in this gate: the modulo arithmetic
/// is sound here because a floating path derives FROM the modulo hash —
/// a forged claim would need a path whose hash part, once zeroed in the
/// content, hashes back to exactly that path (self-certifying).
fn verify_floating_ca(
    info: &ValidatedPathInfo,
    nar_data: &[u8],
    descriptor: Option<FixedCaDescriptor>,
    claims: &rio_auth::hmac::AssignmentClaims,
    ctx_label: &str,
) -> Result<(), Status> {
    use std::io::Write as _;
    let (refs, has_self_reference) = split_self_reference(info);

    // Ingestion method: from the upload's own `fixed:` descriptor when
    // present (the native builder records one for every floating-CA
    // output); absent → the historical recursive-SHA256 assumption.
    let (recursive, algo) = match &descriptor {
        Some(d) => (d.recursive, d.hash.algo()),
        None => (true, rio_nix::hash::HashAlgo::SHA256),
    };

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
            let plain = recursive_plain_hash(info, nar_data, algo, ctx_label)?;
            // Descriptor-gated fallback for *unrecorded* self-references.
            // CppNix's structured-attrs `unsafeDiscardReferences` mints
            // floating-CA paths whose bytes embed their own hash while
            // declaring no self-reference, and the `fixed:` descriptor
            // then carries the hash *modulo* those occurrences. The
            // claimed path is content-derived FROM that modulo hash, so
            // the retry stays self-certifying. (Declared-hash classes
            // never reach this arm — see [`UploadClass`] and the splice
            // forgery note, merged_bug_076.)
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
        let file_bytes = flat_file_bytes(nar_data, ctx_label)?;
        let plain = flat_plain_hash(file_bytes, algo, ctx_label)?;
        // Same descriptor-gated fallback as the recursive branch:
        // `unsafeDiscardReferences` flat outputs whose bytes embed their
        // own path are minted (by CppNix and the native builder alike)
        // from the hash *modulo* those occurrences, with no
        // self-reference declared — sound because a floating path
        // derives FROM the modulo hash. Only paid when the plain hash
        // already failed to match.
        match &descriptor {
            Some(d) if d.hash != plain => {
                let mut sink = rio_nix::ca::HashModuloSink::new(algo, &info.store_path.hash_part());
                sink.write_all(file_bytes).map_err(|e| {
                    Status::internal(format!("{ctx_label}: flat hash-modulo fallback: {e}"))
                })?;
                sink.finish().0
            }
            _ => plain,
        }
    };

    if let Some(d) = &descriptor
        && d.hash != content_hash
    {
        return Err(descriptor_mismatch(
            info,
            &d.hash,
            &content_hash,
            claims,
            ctx_label,
        ));
    }

    require_content_derived_path(
        info,
        &content_hash,
        recursive,
        &refs,
        has_self_reference,
        claims,
        ctx_label,
    )
}

/// Fixed-output arm: PLAIN hash equality only. The path derives from
/// the derivation's DECLARED hash, so accepting any modulo arithmetic
/// here would admit spliced content whose plain hash differs from the
/// declared one (the splice forgery, merged_bug_076; parent fix
/// d6c083a1a re-routed declared-hash FODs through the floating arm
/// without re-deriving its floating-only soundness theorem — pattern
/// R5).
fn verify_fixed_output(
    info: &ValidatedPathInfo,
    nar_data: &[u8],
    descriptor: FixedCaDescriptor,
    claims: &rio_auth::hmac::AssignmentClaims,
    ctx_label: &str,
) -> Result<(), Status> {
    let (refs, has_self_reference) = split_self_reference(info);

    // A fixed-output path exists BEFORE its content (it derives from
    // the declared hash), so a self-reference can never legitimately
    // occur — CppNix's `makeFixedOutputPath` has no self-reference
    // parameter at all. A declared one is an authorization lie, not a
    // hashing mode.
    if has_self_reference {
        warn!(
            store_path = %info.store_path,
            executor_id = %claims.executor_id,
            drv_hash = %claims.drv_hash,
            "{ctx_label}: fixed-output upload declares a self-reference"
        );
        metrics::counter!(
            "rio_store_hmac_rejected_total",
            "reason" => "fod_self_reference"
        )
        .increment(1);
        return Err(Status::permission_denied(format!(
            "{ctx_label}: fixed-output upload declares a self-reference;              fixed-output paths derive from the declared hash and cannot              reference themselves"
        )));
    }

    let content_hash = if descriptor.recursive {
        recursive_plain_hash(info, nar_data, descriptor.hash.algo(), ctx_label)?
    } else {
        let file_bytes = flat_file_bytes(nar_data, ctx_label)?;
        flat_plain_hash(file_bytes, descriptor.hash.algo(), ctx_label)?
    };

    if descriptor.hash != content_hash {
        return Err(descriptor_mismatch(
            info,
            &descriptor.hash,
            &content_hash,
            claims,
            ctx_label,
        ));
    }

    require_content_derived_path(
        info,
        &content_hash,
        descriptor.recursive,
        &refs,
        false,
        claims,
        ctx_label,
    )
}

/// Input-addressed arm for a VOLUNTEERED descriptor: verify the
/// descriptor against the bytes (the persisted/served `CA:` field must
/// not lie) but never re-derive the path — an input-addressed path is
/// not content-derived, and authorization is membership + the deriver
/// proof. Honest producers do not currently volunteer descriptors on IA
/// uploads (the builder records them for floating-CA and fixed-output
/// outputs only); this arm makes the cell's decision explicit instead
/// of letting it fall through whichever content-derived arm the old
/// shape-dispatch reached (which rejected it with an unsatisfiable
/// path-derivation comparison).
fn verify_ia_volunteered_descriptor(
    info: &ValidatedPathInfo,
    nar_data: &[u8],
    descriptor: FixedCaDescriptor,
    claims: &rio_auth::hmac::AssignmentClaims,
    ctx_label: &str,
) -> Result<(), Status> {
    // IA outputs may legitimately self-reference; the descriptor hash
    // for them is over the plain bytes (modulo arithmetic is
    // floating-CA-only — see [`UploadClass`]).
    let content_hash = if descriptor.recursive {
        recursive_plain_hash(info, nar_data, descriptor.hash.algo(), ctx_label)?
    } else {
        let file_bytes = flat_file_bytes(nar_data, ctx_label)?;
        flat_plain_hash(file_bytes, descriptor.hash.algo(), ctx_label)?
    };
    if descriptor.hash != content_hash {
        return Err(descriptor_mismatch(
            info,
            &descriptor.hash,
            &content_hash,
            claims,
            ctx_label,
        ));
    }
    Ok(())
}

/// Map a proof-walk absence verdict to its gRPC status. Total over
/// [`AbsentReason`] so a new verdict arm cannot silently inherit an
/// old code (pattern R1); `PERMISSION_DENIED` for the deriver proof is
/// constructible ONLY through this function's closure-verdict arms.
///
/// [`AbsentReason`]: crate::metadata::drv_modulo::AbsentReason
fn absent_to_status(
    reason: &crate::metadata::drv_modulo::AbsentReason,
    deriver: &str,
    ctx_label: &str,
) -> Status {
    use crate::metadata::drv_modulo::{AbsentReason, PROOF_WALK_WORK_MAX};
    match reason {
        AbsentReason::NotResident { path } => Status::permission_denied(format!(
            "{ctx_label}: deriver closure unverifiable — {path} is not resident in \
             this store, so the claimed output path cannot be proven to belong to \
             deriver {deriver}; upload the deriver closure (.drv files) first"
        )),
        AbsentReason::Unparseable { path, why } => Status::permission_denied(format!(
            "{ctx_label}: deriver closure unverifiable — {path} cannot be used: {why}"
        )),
        AbsentReason::Cycle => Status::permission_denied(format!(
            "{ctx_label}: deriver closure unverifiable — the input metadata of \
             deriver {deriver} forms a cycle; no derivation order exists (fail-closed)"
        )),
        AbsentReason::OverBudget {
            persisted,
            work_used,
        } => Status::resource_exhausted(format!(
            "{ctx_label}: deriver-closure proof for {deriver} exceeded its work \
             budget ({work_used} of {PROOF_WALK_WORK_MAX} units); {persisted} proven \
             rows were persisted — retrying resumes from durable progress"
        )),
    }
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
        let hmac_claims = self.verify_assignment_token_put(request)?;
        let tenant_id = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub);
        Ok(PutAuth {
            hmac_claims,
            tenant_id,
        })
    }

    // r[impl store.put.ia-deriver-proof+3]
    /// Descriptor-less INPUT-ADDRESSED uploads must prove deriver
    /// membership against the store's OWN bytes: the claims' deriver
    /// `.drv` (claims.drv_hash == its store path, bound at scheduler
    /// ingress) must be store-resident, and the claimed path must be
    /// among the IA output paths the store derives from its own copy
    /// via the modulo cache (budgeted monotone read-through on miss).
    /// `expected_outputs` membership alone is no longer sufficient for
    /// IA registration — a forged-claims signer could put arbitrary
    /// paths there; it cannot make the store's bytes derive them.
    ///
    /// Applies iff signed claims are present and the upload is plain IA
    /// (not is_ca, not is_fixed_output — those flow through the
    /// descriptor gates). Deferred derivers (floating-CA self /
    /// deferred-IA) are membership-only: their true paths come from
    /// realisations (claims for them are realisation-derived at
    /// dispatch; documented residual is compromised-scheduler-only).
    ///
    /// Status mapping is total over [`AbsentReason`] (pinned by
    /// `absent_to_status_bijection`): closure verdicts →
    /// `PERMISSION_DENIED` with the reason named; budget exhaustion →
    /// `RESOURCE_EXHAUSTED` (retriable; progress was persisted);
    /// infrastructure errors → `INTERNAL` (cause server-logged only).
    pub(in crate::grpc) async fn verify_ia_registration_proof(
        &self,
        info: &ValidatedPathInfo,
        claims: Option<&rio_auth::hmac::AssignmentClaims>,
        ctx_label: &str,
    ) -> Result<(), Status> {
        use crate::metadata::drv_modulo::ProofOutcome;

        let Some(claims) = claims else {
            return Ok(()); // dev mode / service relay: no claims to prove
        };
        if claims.is_ca || claims.is_fixed_output {
            return Ok(()); // descriptor gates own CA/FOD verification
        }
        // TODO: F3 (round-15 C4 follow-up) — this branch keys on the
        // `.drv` NAME SUFFIX, not a parsed upload type. After UploadClass
        // (C4c2) the gate arms are typed, but the .drv detection sites
        // remain suffix-keyed; F3 unifies them on a typed PathKind so a
        // source path NAMED `*.drv` and a real derivation cannot diverge
        // by call-site discipline. See store.put.drv-text-ca+2.
        if info.store_path.is_derivation() {
            return Ok(()); // .drv text-CA gate owns derivation uploads
        }
        let deriver = claims.drv_hash.as_str();
        let outcome = crate::metadata::drv_modulo::prove_drv_modulo(
            &self.pool,
            self.chunk_cache.as_deref(),
            deriver,
        )
        .await
        .map_err(|e| {
            // Unlike this file's content-derived hash errors (computed
            // over bytes the caller already holds), this error wraps a
            // database lookup: sqlx errors can carry SQL fragments and
            // connection details, and PutPath callers are untrusted
            // workers. Log the cause server-side; return only a generic
            // marker the caller can correlate by ctx_label.
            tracing::error!(deriver, ctx_label, error = %e, "deriver proof lookup failed");
            Status::internal(format!(
                "{ctx_label}: deriver proof lookup failed (see store logs)"
            ))
        })?;
        let row = match outcome {
            ProofOutcome::Proven(row) => row,
            ProofOutcome::Absent(reason) => {
                return Err(absent_to_status(&reason, deriver, ctx_label));
            }
        };
        if row.deferred {
            metrics::counter!("rio_store_ia_proof_total", "result" => "deferred_exempt")
                .increment(1);
            return Ok(());
        }
        let claimed = info.store_path.as_str();
        if row.ia_output_paths.values().any(|p| p == claimed) {
            metrics::counter!("rio_store_ia_proof_total", "result" => "ok").increment(1);
            Ok(())
        } else {
            metrics::counter!("rio_store_ia_proof_total", "result" => "rejected").increment(1);
            warn!(
                claimed,
                deriver,
                "{ctx_label}: claimed path is not among the deriver's store-derived IA outputs"
            );
            Err(Status::permission_denied(format!(
                "{ctx_label}: path {claimed} is not an output the store derives from \
                 deriver {deriver}"
            )))
        }
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
        // r[impl store.put.drv-text-ca]
        verify_drv_text_path(info, &nar_data, "PutPath")?;
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
        // Capture the .drv file bytes BEFORE persist_nar consumes the
        // NAR (store.ingest.drv-modulo-cache+2): the modulo-cache hook
        // runs only after the persist succeeds, but the bytes are gone
        // by then. ~KBs for any real .drv; non-derivations skip.
        // TODO: F3 (round-15 C4 follow-up) — this branch keys on the
        // `.drv` NAME SUFFIX, not a parsed upload type. After UploadClass
        // (C4c2) the gate arms are typed, but the .drv detection sites
        // remain suffix-keyed; F3 unifies them on a typed PathKind so a
        // source path NAMED `*.drv` and a real derivation cannot diverge
        // by call-site discipline. See store.put.drv-text-ca+2.
        let drv_bytes_for_cache: Option<Vec<u8>> = if info.store_path.ends_with(".drv") {
            rio_nix::nar::extract_single_file(&nar_data).ok()
        } else {
            None
        };
        if let Err(e) = self.persist_nar(&info, claim, nar_data, "PutPath").await {
            self.abort_upload(&info.store_path_hash, claim).await;
            return Err(e);
        }
        // r[impl store.ingest.drv-modulo-cache+2]
        // Best-effort, AFTER the text-CA gate (verify_drv_text_path ran
        // before this finalize) and AFTER the NAR is durable — a
        // population failure never fails the upload.
        if let Some(bytes) = drv_bytes_for_cache {
            metadata::drv_modulo::populate_on_ingest(&self.pool, &info.store_path, &bytes).await;
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
            is_fixed_output: false,
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

    /// Flat-mode discarded self-reference: the file bytes embed the
    /// claimed path, `references` is empty, and the flat `fixed:`
    /// descriptor carries the modulo hash — the shape the builder's
    /// flat floating-CA finalization produces under structured-attrs
    /// `unsafeDiscardReferences`. The gate must accept it via the same
    /// fallback as the recursive branch, and still reject the same
    /// upload claimed under a different path.
    #[test]
    fn ca_flat_discarded_self_reference_with_descriptor_accepted() {
        use std::io::Write as _;
        let drv =
            rio_nix::store_path::StorePath::parse(&test_drv_path("flat-self-discarded")).unwrap();
        let scratch =
            rio_nix::store_path::StorePath::make_scratch_output_path(&drv, "out").unwrap();
        let content_at_scratch = format!("I live at {}\n", scratch.as_str()).into_bytes();
        let mut sink =
            rio_nix::ca::HashModuloSink::new(rio_nix::hash::HashAlgo::SHA256, &scratch.hash_part());
        sink.write_all(&content_at_scratch).unwrap();
        let (modulo, _) = sink.finish();
        // Self flag OFF and method = flat: the references were discarded.
        let final_path = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            "flat-self-discarded",
            &modulo,
            false,
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
        info.content_address = Some(format!("fixed:{}", modulo.to_colon()));
        verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t")
            .expect("flat discarded-self upload with a modulo descriptor must pass");

        // Same NAR + descriptor claimed under a different path: the
        // fallback modulus changes, nothing matches, still rejected.
        let other =
            rio_nix::store_path::StorePath::parse(&test_store_path("flat-imposter")).unwrap();
        let mut info = ca_info(&other, &[], &nar);
        info.content_address = Some(format!("fixed:{}", modulo.to_colon()));
        let e = verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t").unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "got: {e:?}");
    }

    /// Flat self-embedding content with a LYING flat descriptor: neither
    /// the plain nor the modulo recompute matches, so the gate must
    /// reject for the descriptor mismatch.
    #[test]
    fn ca_flat_discarded_self_descriptor_lie_rejected() {
        use std::io::Write as _;
        let drv = rio_nix::store_path::StorePath::parse(&test_drv_path("flat-desc-lie")).unwrap();
        let scratch =
            rio_nix::store_path::StorePath::make_scratch_output_path(&drv, "out").unwrap();
        let content_at_scratch = format!("I live at {}\n", scratch.as_str()).into_bytes();
        let mut sink =
            rio_nix::ca::HashModuloSink::new(rio_nix::hash::HashAlgo::SHA256, &scratch.hash_part());
        sink.write_all(&content_at_scratch).unwrap();
        let (modulo, _) = sink.finish();
        let final_path = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            "flat-desc-lie",
            &modulo,
            false,
            &[],
            false,
        )
        .unwrap();
        let final_content = String::from_utf8(content_at_scratch)
            .unwrap()
            .replace(&scratch.hash_part(), &final_path.hash_part())
            .into_bytes();
        let nar = nar_of_file(&final_content);

        let other_digest: [u8; 32] = Sha256::digest(b"not the uploaded bytes").into();
        let other_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, other_digest.to_vec())
                .unwrap();
        let mut info = ca_info(&final_path, &[], &nar);
        info.content_address = Some(format!("fixed:{}", other_hash.to_colon()));
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

    // r[verify store.put.drv-text-ca]
    /// A `.drv` upload is bound to its bytes: the canonical text-CA path
    /// (with and without references) is accepted, anything else is not.
    #[test]
    fn drv_text_path_binds_path_bytes_and_references() {
        use std::io::Write as _;
        let drv_text = br#"Derive([("out","/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x-out","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let nar = nar_of_file(drv_text);
        let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
        w.write_all(drv_text).unwrap();
        let hash = w.finish();

        // No references.
        let path = rio_nix::store_path::StorePath::make_text("x.drv", &hash, &[]).unwrap();
        let info = ca_info(&path, &[], &nar);
        verify_drv_text_path(&info, &nar, "t").expect("canonical .drv path accepted");

        // With references: the path embeds them, and the declared set
        // must match what the path was minted with.
        let dep = rio_nix::store_path::StorePath::parse(&test_store_path("some-input")).unwrap();
        let path_with_refs =
            rio_nix::store_path::StorePath::make_text("x.drv", &hash, std::slice::from_ref(&dep))
                .unwrap();
        let info = ca_info(&path_with_refs, std::slice::from_ref(&dep), &nar);
        verify_drv_text_path(&info, &nar, "t").expect("canonical .drv path with refs accepted");

        // Same bytes, but the declared references do not match the path.
        let info = ca_info(&path_with_refs, &[], &nar);
        let err = verify_drv_text_path(&info, &nar, "t")
            .expect_err("reference mismatch must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);

        // Different bytes under the canonical path of the original.
        let other_nar = nar_of_file(b"Derive(tampered)");
        let info = ca_info(&path, &[], &other_nar);
        let err = verify_drv_text_path(&info, &other_nar, "t")
            .expect_err("non-derived .drv path must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(err.message().contains("text content-address"), "msg: {err}");

        // A non-.drv path is not subject to the check.
        let plain = rio_nix::store_path::StorePath::parse(&test_store_path("not-a-drv")).unwrap();
        let info = ca_info(&plain, &[], &other_nar);
        verify_drv_text_path(&info, &other_nar, "t").expect("non-.drv paths ignored");
    }

    // r[verify store.put.drv-text-ca]
    /// A `.drv` claiming path with a directory NAR is malformed input.
    #[test]
    fn drv_text_path_requires_single_file_nar() {
        let node = rio_nix::nar::NarNode::Directory { entries: vec![] };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node).unwrap();
        let path = rio_nix::store_path::StorePath::parse(&test_drv_path("dir-shaped")).unwrap();
        let info = ca_info(&path, &[], &nar);
        let err = verify_drv_text_path(&info, &nar, "t")
            .expect_err("directory NAR claiming a .drv path must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
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

    /// Worker (assignment-token) claims with `is_ca = false`, as a
    /// fixed-output upload presents them. The membership check ran in
    /// `validate_put_metadata`; this gate only sees the token kind.
    fn ia_claims() -> rio_auth::hmac::AssignmentClaims {
        let mut c = ca_claims();
        c.is_ca = false;
        c
    }

    /// Worker claims as a fixed-output assignment carries them: the
    /// scheduler signs `is_fixed_output = true` (and `is_ca = false`,
    /// since the FOD path is known at dispatch time).
    fn fod_claims() -> rio_auth::hmac::AssignmentClaims {
        let mut c = ia_claims();
        c.is_fixed_output = true;
        c
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// A fixed-output assignment (signed `is_fixed_output = true`) may
    /// not skip content verification by omitting the descriptor: the
    /// store rejects rather than falling back to membership-only
    /// acceptance. Non-FOD descriptor-less uploads stay membership-only
    /// (pinned by `ca_gate_noop_without_ca_claims`).
    #[test]
    fn fod_descriptorless_upload_rejected() {
        let (path, nar, _descriptor) =
            build_fod_recursive_upload("fod-silent", b"attacker bytes\n");
        let info = ca_info(&path, &[], &nar); // no content_address
        let err = verify_ca_store_path(&info, &nar, Some(&fod_claims()), "t")
            .expect_err("descriptor-less upload under FOD-flagged claims must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(
            err.message()
                .contains("must carry a `fixed:` content-address descriptor"),
            "unexpected error: {err}"
        );
    }

    /// A fixed-output-shaped upload: content, its recursive (NAR)
    /// SHA-256, the path derived from that hash, and the `fixed:r:`
    /// descriptor the builder records for it.
    fn build_fod_recursive_upload(
        name: &str,
        content: &[u8],
    ) -> (rio_nix::store_path::StorePath, Vec<u8>, String) {
        use std::io::Write as _;
        let nar = nar_of_file(content);
        let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
        w.write_all(&nar).unwrap();
        let hash = w.finish();
        let path = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            name,
            &hash,
            true,
            &[],
            false,
        )
        .unwrap();
        let descriptor = format!("fixed:r:{}", hash.to_colon());
        (path, nar, descriptor)
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// Fixed-output uploads (is_ca = false, `fixed:` descriptor present)
    /// are content-verified server-side: the descriptor must match the
    /// uploaded bytes AND the claimed path must re-derive from it. A
    /// compromised worker holding a membership-authorized path cannot
    /// register arbitrary bytes there.
    #[test]
    fn fod_descriptor_binds_path_and_content_for_non_ca_uploads() {
        let (path, nar, descriptor) = build_fod_recursive_upload("fod-out", b"genuine bytes\n");
        let claims = fod_claims();

        // Honest upload: descriptor matches the bytes, path derives from it.
        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(descriptor.clone());
        verify_ca_store_path(&info, &nar, Some(&claims), "t")
            .expect("consistent fixed-output upload accepted");

        // Same descriptor + path, different bytes: the descriptor
        // cross-check rejects (the lie would otherwise be persisted and
        // served as the narinfo `CA:` field).
        let attacker_nar = nar_of_file(b"attacker bytes\n");
        let mut lying = ca_info(&path, &[], &attacker_nar);
        lying.content_address = Some(descriptor.clone());
        let err = verify_ca_store_path(&lying, &attacker_nar, Some(&claims), "t")
            .expect_err("descriptor/content mismatch must be rejected");
        assert!(
            err.message().contains("descriptor does not match"),
            "unexpected error: {err}"
        );

        // Descriptor honestly matches the attacker bytes, but the claimed
        // path is some other (legitimately membership-authorized)
        // fixed-output path: the path re-derivation rejects.
        let (victim_path, _, _) = build_fod_recursive_upload("victim-out", b"victim bytes\n");
        let (_, attacker_nar2, attacker_descriptor) =
            build_fod_recursive_upload("victim-out", b"attacker bytes\n");
        let mut squatted = ca_info(&victim_path, &[], &attacker_nar2);
        squatted.content_address = Some(attacker_descriptor);
        let err = verify_ca_store_path(&squatted, &attacker_nar2, Some(&claims), "t")
            .expect_err("path not derived from the content must be rejected");
        assert!(
            err.message().contains("content-derived"),
            "unexpected error: {err}"
        );
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// Flat-mode fixed-output uploads get the same binding, using the
    /// file-bytes hash instead of the NAR hash.
    #[test]
    fn fod_flat_descriptor_binds_path_and_content_for_non_ca_uploads() {
        use std::io::Write as _;
        let content = b"flat fixed-output bytes\n";
        let nar = nar_of_file(content);
        let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
        w.write_all(content).unwrap();
        let hash = w.finish();
        let path = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            "fod-flat",
            &hash,
            false,
            &[],
            false,
        )
        .unwrap();
        let claims = fod_claims();

        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(format!("fixed:{}", hash.to_colon()));
        verify_ca_store_path(&info, &nar, Some(&claims), "t")
            .expect("consistent flat fixed-output upload accepted");

        // Same bytes claimed at a path derived from different content.
        let (other_path, _, _) = build_fod_recursive_upload("fod-flat", b"other\n");
        let mut wrong = ca_info(&other_path, &[], &nar);
        wrong.content_address = Some(format!("fixed:{}", hash.to_colon()));
        let err = verify_ca_store_path(&wrong, &nar, Some(&claims), "t")
            .expect_err("flat path not derived from the content must be rejected");
        assert!(
            err.message().contains("content-derived"),
            "unexpected error: {err}"
        );
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// Method confusion on the fixed-output arm: the descriptor honestly
    /// matches the bytes under the WRONG method (flat) while the claimed
    /// path was minted recursively — the path re-derivation rejects, the
    /// same way the floating-CA arm pins its method-mismatch case.
    #[test]
    fn fod_method_confusion_rejected_for_non_ca_uploads() {
        use std::io::Write as _;
        let content = b"method confusion bytes\n";
        // Claimed path minted with the recursive (NAR) hash.
        let (path, nar, _recursive_descriptor) = build_fod_recursive_upload("fod-method", content);
        // Descriptor presented with the flat (file-bytes) hash — honest
        // for the bytes, but for the wrong ingestion method.
        let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
        w.write_all(content).unwrap();
        let flat_hash = w.finish();
        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(format!("fixed:{}", flat_hash.to_colon()));
        let err = verify_ca_store_path(&info, &nar, Some(&fod_claims()), "t")
            .expect_err("flat descriptor against a recursively-minted path must be rejected");
        assert!(
            err.message().contains("content-derived"),
            "unexpected error: {err}"
        );
    }

    /// Build the splice-forgery shape (merged_bug_076): bytes that embed
    /// the claimed path's own hash part with NO declared self-reference,
    /// plus a descriptor carrying the hash MODULO those occurrences. For
    /// a floating-CA token this is the legitimate discarded-self shape
    /// (the path derives FROM the modulo hash); for a declared-hash
    /// token it is a forgery (content whose PLAIN hash differs from the
    /// hash the path was minted from). Returns
    /// `(path, nar, modulo_descriptor, modulo_hash, plain_hash)`.
    fn build_spliced_modulo_upload(
        name: &str,
        recursive: bool,
    ) -> (
        rio_nix::store_path::StorePath,
        Vec<u8>,
        String,
        rio_nix::hash::NixHash,
        rio_nix::hash::NixHash,
    ) {
        use std::io::Write as _;
        let drv = rio_nix::store_path::StorePath::parse(&test_drv_path(name)).unwrap();
        let scratch =
            rio_nix::store_path::StorePath::make_scratch_output_path(&drv, "out").unwrap();
        let content_at_scratch = format!("I live at {}\n", scratch.as_str()).into_bytes();
        let hashed_at_scratch = if recursive {
            nar_of_file(&content_at_scratch)
        } else {
            content_at_scratch.clone()
        };
        let mut sink =
            rio_nix::ca::HashModuloSink::new(rio_nix::hash::HashAlgo::SHA256, &scratch.hash_part());
        sink.write_all(&hashed_at_scratch).unwrap();
        let (modulo, _) = sink.finish();
        let path = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            name,
            &modulo,
            recursive,
            &[],
            false,
        )
        .unwrap();
        let final_content = String::from_utf8(content_at_scratch)
            .unwrap()
            .replace(&scratch.hash_part(), &path.hash_part())
            .into_bytes();
        let nar = nar_of_file(&final_content);
        let plain_input: &[u8] = if recursive { &nar } else { &final_content };
        let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
        w.write_all(plain_input).unwrap();
        let plain = w.finish();
        let method = if recursive { "r:" } else { "" };
        let descriptor = format!("fixed:{method}{}", modulo.to_colon());
        (path, nar, descriptor, modulo, plain)
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// THE splice forgery (merged_bug_076, fix-child of d6c083a1a /
    /// 79c68a4ca): under a declared-hash (fixed-output) token, content
    /// embedding the claimed path's own hash part — chosen so the hash
    /// MODULO the path equals the descriptor while the PLAIN hash does
    /// not — must be rejected. Pre-fix, the discarded-self fallback
    /// (sound only for floating-CA, where the path derives from the
    /// modulo) accepted exactly this shape and registered content whose
    /// plain hash differs from the hash the path was minted from.
    #[test]
    fn fod_spliced_self_hash_descriptor_rejected() {
        use std::io::Write as _;
        let (path, nar, descriptor, modulo, plain) =
            build_spliced_modulo_upload("fod-splice", true);

        // Tripwire: the pre-fix acceptance conditions all hold — the
        // modulo recompute over the uploaded bytes EQUALS the descriptor
        // hash, the plain hash does NOT, and the claimed path re-derives
        // from the modulo. If any of these stops holding, the test has
        // gone vacuous (it would reject for fixture reasons, not because
        // the fallback is gated).
        let mut sink =
            rio_nix::ca::HashModuloSink::new(rio_nix::hash::HashAlgo::SHA256, &path.hash_part());
        sink.write_all(&nar).unwrap();
        let (modulo_recomputed, _) = sink.finish();
        assert_eq!(modulo_recomputed, modulo, "tripwire: modulo_P(B') == H");
        assert_ne!(plain, modulo, "tripwire: plain(B') != H");
        let rederived = rio_nix::store_path::StorePath::make_fixed_output_with_self(
            "fod-splice",
            &modulo,
            true,
            &[],
            false,
        )
        .unwrap();
        assert_eq!(
            rederived.as_str(),
            path.as_str(),
            "tripwire: P derives from H"
        );

        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(descriptor.clone());
        let err = verify_ca_store_path(&info, &nar, Some(&fod_claims()), "t")
            .expect_err("spliced modulo descriptor under a FOD token must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "got: {err:?}");
        assert!(
            err.message().contains("descriptor does not match"),
            "rejection must be the descriptor mismatch (no modulo retry), got: {}",
            err.message()
        );

        // Companion: the SAME NAR + descriptor under a floating-CA token
        // is the legitimate discarded-self shape and must stay accepted —
        // the fix gates the retry on the token class, it does not remove
        // it.
        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(descriptor);
        verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t")
            .expect("same upload under a floating-CA token must keep passing");
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// Flat sibling of the splice forgery: the verbatim-copied flat
    /// fallback arm (79c68a4ca) gets the same floating-only restriction.
    #[test]
    fn fod_flat_spliced_self_hash_descriptor_rejected() {
        let (path, nar, descriptor, _modulo, _plain) =
            build_spliced_modulo_upload("fod-flat-splice", false);

        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(descriptor.clone());
        let err = verify_ca_store_path(&info, &nar, Some(&fod_claims()), "t")
            .expect_err("flat spliced modulo descriptor under a FOD token must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "got: {err:?}");
        assert!(
            err.message().contains("descriptor does not match"),
            "got: {}",
            err.message()
        );

        // Floating-CA companion (flat discarded-self stays legitimate).
        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(descriptor);
        verify_ca_store_path(&info, &nar, Some(&ca_claims()), "t")
            .expect("flat discarded-self under a floating-CA token must keep passing");
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// An input-addressed token volunteering a descriptor gets the same
    /// no-modulo-retry verification as the fixed-output class: the
    /// spliced shape is rejected at the descriptor mismatch, never
    /// rescued by the floating-only fallback.
    #[test]
    fn ia_voluntary_descriptor_no_modulo_retry() {
        let (path, nar, descriptor, _modulo, _plain) =
            build_spliced_modulo_upload("ia-splice", true);
        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(descriptor);
        let err = verify_ca_store_path(&info, &nar, Some(&ia_claims()), "t")
            .expect_err("spliced descriptor under an IA token must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "got: {err:?}");
        assert!(
            err.message().contains("descriptor does not match"),
            "got: {}",
            err.message()
        );
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// A token marked both floating-CA and fixed-output is malformed
    /// trusted-plane data: rejected loudly at classification instead of
    /// silently resolving to whichever class the old shape-dispatch
    /// reached first (R5 — the population cross product is decided per
    /// cell).
    #[test]
    fn claims_kind_conflict_rejected() {
        let (path, nar, descriptor) = build_fod_recursive_upload("conflict-out", b"bytes\n");
        let mut claims = fod_claims();
        claims.is_ca = true; // conflict: also fixed-output
        let mut info = ca_info(&path, &[], &nar);
        info.content_address = Some(descriptor);
        let err = verify_ca_store_path(&info, &nar, Some(&claims), "t")
            .expect_err("kind-conflicted claims must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "got: {err:?}");
        assert!(
            err.message().contains("exactly one class"),
            "got: {}",
            err.message()
        );
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// A fixed-output upload declaring a self-reference is an
    /// authorization lie, not a hashing mode: fixed-output paths derive
    /// from the declared hash and exist before their content, so the
    /// gate rejects instead of switching to modulo arithmetic (which is
    /// floating-CA-only — merged_bug_076's splice surface).
    #[test]
    fn fod_declared_self_reference_rejected() {
        let (path, nar, descriptor) = build_fod_recursive_upload("selfish-out", b"bytes\n");
        let mut info = ca_info(&path, std::slice::from_ref(&path), &nar);
        info.content_address = Some(descriptor);
        let err = verify_ca_store_path(&info, &nar, Some(&fod_claims()), "t")
            .expect_err("self-referencing fixed-output upload must be rejected");
        assert_eq!(err.code(), tonic::Code::PermissionDenied, "got: {err:?}");
        assert!(
            err.message().contains("declares a self-reference"),
            "got: {}",
            err.message()
        );
    }

    // r[verify sec.authz.ca-path-derived+9]
    /// An input-addressed upload volunteering an HONEST descriptor is
    /// accepted with the descriptor verified against the bytes — and
    /// the path is NEVER re-derived from the content (an IA path is not
    /// content-derived; under the old shape-dispatch this cell fell
    /// through the content-derived arm and failed an unsatisfiable
    /// path comparison).
    #[test]
    fn ia_honest_voluntary_descriptor_accepted_without_rederive() {
        use std::io::Write as _;
        let nar = nar_of_file(b"ia bytes\n");
        let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
        w.write_all(&nar).unwrap();
        let hash = w.finish();
        // A plausible input-addressed path: NOT derivable from the
        // content hash (that is the point of the test).
        let ia_path = rio_nix::store_path::StorePath::parse(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-ia-out",
        )
        .unwrap();
        let mut info = ca_info(&ia_path, &[], &nar);
        info.content_address = Some(format!("fixed:r:{}", hash.to_colon()));
        verify_ca_store_path(&info, &nar, Some(&ia_claims()), "t")
            .expect("honest volunteered descriptor on an IA upload is accepted");

        // Self-referencing IA uploads stay legitimate (IA outputs
        // routinely embed their own path) — plain hash, no modulo.
        let mut self_ref = ca_info(&ia_path, std::slice::from_ref(&ia_path), &nar);
        self_ref.content_address = Some(format!("fixed:r:{}", hash.to_colon()));
        verify_ca_store_path(&self_ref, &nar, Some(&ia_claims()), "t")
            .expect("self-referencing IA upload with honest descriptor is accepted");
    }

    // r[verify store.put.ia-deriver-proof+3]
    /// gRPC code bijection over the absence verdicts: closure verdicts
    /// are PERMISSION_DENIED with the reason named; budget exhaustion
    /// is RESOURCE_EXHAUSTED (retriable) and names the persisted
    /// progress. Total match — adding a verdict arm without a code
    /// decision cannot compile.
    #[test]
    fn absent_to_status_bijection() {
        use crate::metadata::drv_modulo::AbsentReason;
        let d = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv";

        let s = absent_to_status(&AbsentReason::NotResident { path: d.into() }, d, "t");
        assert_eq!(s.code(), tonic::Code::PermissionDenied);
        assert!(s.message().contains("not resident"), "{}", s.message());

        let s = absent_to_status(
            &AbsentReason::Unparseable {
                path: d.into(),
                why: "junk".into(),
            },
            d,
            "t",
        );
        assert_eq!(s.code(), tonic::Code::PermissionDenied);
        assert!(s.message().contains("junk"), "{}", s.message());

        let s = absent_to_status(&AbsentReason::Cycle, d, "t");
        assert_eq!(s.code(), tonic::Code::PermissionDenied);
        assert!(s.message().contains("cycle"), "{}", s.message());

        let s = absent_to_status(
            &AbsentReason::OverBudget {
                persisted: 7,
                work_used: 99,
            },
            d,
            "t",
        );
        assert_eq!(s.code(), tonic::Code::ResourceExhausted);
        assert!(
            s.message().contains("7 proven rows were persisted"),
            "{}",
            s.message()
        );
        assert!(s.message().contains("retrying resumes"), "{}", s.message());
    }

    /// A worker upload may only claim `fixed:` content addresses; a
    /// `text:` (or otherwise unparseable) descriptor from a worker is
    /// rejected rather than persisted unverified — store-added text
    /// paths come from the trusted control plane via service tokens,
    /// which never reach this gate.
    #[test]
    fn worker_non_fixed_descriptor_rejected() {
        let nar = nar_of_file(b"drv text\n");
        let path = rio_nix::store_path::StorePath::parse(&test_store_path("some-drv")).unwrap();
        let mut info = ca_info(&path, &[], &nar);
        info.content_address =
            Some("text:sha256:0c1dab1sr734bnyivlkjcbpyylcbqqvf5b1zl0wdj0pylqrjg5aw".into());
        let err = verify_ca_store_path(&info, &nar, Some(&ia_claims()), "t")
            .expect_err("text: descriptor on a worker upload must be rejected");
        assert!(
            err.message()
                .contains("unsupported content-address descriptor"),
            "unexpected error: {err}"
        );
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

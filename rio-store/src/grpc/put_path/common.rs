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

use std::time::Duration;

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tonic::{Request, Status, Streaming};
use tracing::{debug, warn};

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

// r[impl store.put.nar-hold-envelope+2]
/// Bound on any single charged budget wait at the per-chunk acquire
/// chokepoint ([`StoreServiceImpl::accumulate_chunk`]): a holder's
/// next-chunk acquire that parks longer than this sheds typed
/// (`ResourceExhausted`, the retryable class the upload plane already
/// absorbs — the logs-plane byte-budget shed is the house precedent)
/// instead of hold-and-waiting forever behind a parked whole-NAR head
/// (merged_bug_001's wedge edge). One authority, no mirrored literal:
/// equals the substitute plane's stall window — a budget park longer
/// than a full stall window means the pod has been at its
/// OOM-protection bound for minutes, and shedding to the LB is the
/// designed capacity answer. Uniform over ALL chunk acquires, first
/// included: the chokepoint cannot see caller holdings, and the
/// uniform bound is what makes the no-deadlock theorem's wait premise
/// total over the census.
pub(crate) const BUDGET_WAIT_GRACE: Duration = crate::substitute::DEFAULT_SUBSTITUTE_STALL_WINDOW;

// Const-relation pin `wait_grace_within_hold_floor` (R17 ordering law
// between nested budgets): the wait grace is strictly inside the
// smallest hold envelope's fixed grace (`NAR_HOLD_GRACE_FACTOR ×
// stall_window`), so a waiting holder always sheds its WAIT before
// its own HOLD deadline can fire — wait-shedding is the first line
// of defense, hold expiry the backstop.
const _: () = assert!(
    BUDGET_WAIT_GRACE.as_secs()
        < crate::substitute::DEFAULT_SUBSTITUTE_STALL_WINDOW.as_secs()
            * crate::substitute::NAR_HOLD_GRACE_FACTOR as u64
);

/// Typed NAR-budget envelope knobs for the gRPC ingest plane
/// (PutPath/PutPathBatch) — the R17 violability lane for the
/// service-side axes, mirroring the `Substituter` builder overrides.
/// Production uses the `Default` impl; tests shrink via
/// [`StoreServiceImpl::with_nar_ingest_envelope`] so the typed aborts
/// are exercisable in seconds, and the violation reds prove each knob
/// binds by flipping it.
#[derive(Debug, Clone, Copy)]
pub struct NarIngestEnvelopeCfg {
    /// Bound on a single charged chunk-acquire wait
    /// (`BUDGET_WAIT_GRACE`).
    pub budget_wait_grace: Duration,
    /// Stall window the ingest hold envelope derives its grace from
    /// (`NAR_HOLD_GRACE_FACTOR ×` this; the substitute plane's
    /// default window is the one authority).
    pub hold_stall_window: Duration,
    /// Floor decompressed-throughput for the ingest hold envelope
    /// (`crate::substitute::NAR_HOLD_FLOOR_RATE`, bytes/second).
    pub hold_floor_rate: u64,
}

impl Default for NarIngestEnvelopeCfg {
    fn default() -> Self {
        Self {
            budget_wait_grace: BUDGET_WAIT_GRACE,
            hold_stall_window: crate::substitute::DEFAULT_SUBSTITUTE_STALL_WINDOW,
            hold_floor_rate: crate::substitute::NAR_HOLD_FLOOR_RATE,
        }
    }
}

impl StoreServiceImpl {
    /// Override the ingest-plane NAR-budget envelope knobs
    /// ([`NarIngestEnvelopeCfg`]). Builder-style — the R17 violability
    /// lane: tests shrink the wait grace / hold envelope so the typed
    /// sheds and residency aborts are exercisable in seconds (and the
    /// violation reds flip single knobs to prove each binds).
    /// Production keeps the `Default`.
    pub fn with_nar_ingest_envelope(mut self, cfg: NarIngestEnvelopeCfg) -> Self {
        self.nar_ingest_envelope = cfg;
        self
    }
}

// r[impl store.put.nar-hold-envelope+2]
// census[gen: rio-store/tests/gensets/put-path-await-census.txt]
/// The ingest plane's budget HOLDER (bug_114, the tiling law): the
/// granted permits and the ONE envelope armed at the FIRST grant,
/// fused into a type whose awaits are reachable only through the
/// deadline-consulting combinator [`Self::bounded`].
///
/// The wave-8 close armed per-SPAN fresh clocks (stream / stage /
/// commit), which left the inter-span awaits — the per-output
/// `claim_placeholder` PG loop, `resolve_batch_signer`, the CA-arm
/// `claim_or_return` — holding up to `MAX_NAR_SIZE` of shared budget
/// with NO deadline at all, and let every later span buy itself a
/// fresh allowance. This type is the structural inverse:
///
/// - the permits are PRIVATE — a caller holding a `NarIngestHold`
///   has no bare `SemaphorePermit`s to sit on; the only way to await
///   while holding is [`Self::bounded`], which derives its timeout
///   from the one envelope's `remaining()` (the `read_nar_capped`
///   laws-as-types move generalized to the time axis);
/// - the envelope is armed ONCE, at the first grant (`MAX_NAR_SIZE`
///   ingest-cap basis — total size is unknown until the trailer);
///   [`Self::tighten_for_tail`] may only ever SHRINK the deadline
///   (monotone knowledge improvement at stream drain, when the
///   buffered byte count becomes known);
/// - dropping the hold releases every permit (unchanged semantics —
///   the semaphore credit-back rides the `SemaphorePermit` drops).
///
/// Zero-holding spans (before the first chunk grant) stay OUTSIDE
/// this type by construction: callers carry `Option<NarIngestHold>`
/// and bare-await while it is `None` — waiters park free; holders
/// expire (the substitute-plane discipline, verbatim).
pub(in crate::grpc) struct NarIngestHold<'a> {
    /// Granted budget permits (TRAILER mode: the delivery-priced
    /// per-chunk grants). Private: see the type doc — bare awaits
    /// while holding must not typecheck outside this module.
    permits: Vec<tokio::sync::SemaphorePermit<'a>>,
    /// The DECLARED-mode single-shot acquisition: permits fused to
    /// the tenant's cost-axis charge (merged_bug_005 — see
    /// [`crate::budget::DeclaredCharge`]). Exactly one of
    /// `permits`/`declared_charge` is populated per mode; dropping
    /// the hold releases either through its own RAII.
    _declared_charge: Option<crate::budget::DeclaredCharge>,
    /// THE one hold envelope, armed at the first grant.
    envelope: crate::substitute::NarHoldEnvelope,
    /// Envelope knobs (carried for [`Self::tighten_for_tail`]'s
    /// re-derivation).
    cfg: NarIngestEnvelopeCfg,
}

impl<'a> NarIngestHold<'a> {
    /// The hold begins: arm the ONE envelope (ingest-cap basis) and
    /// take ownership of the first granted permit.
    pub(in crate::grpc) fn arm(
        first_permit: tokio::sync::SemaphorePermit<'a>,
        cfg: NarIngestEnvelopeCfg,
    ) -> Self {
        Self {
            permits: vec![first_permit],
            _declared_charge: None,
            envelope: crate::substitute::NarHoldEnvelope::for_ingest_cap(
                cfg.hold_stall_window,
                cfg.hold_floor_rate,
            ),
            cfg,
        }
    }

    // r[impl store.put.declared-reserve]
    /// The DECLARED-mode hold (N1): the whole charge was granted in
    /// ONE pre-stream acquisition — fused to the tenant's cost-axis
    /// charge by [`crate::budget::DeclaredCharge`] (merged_bug_005)
    /// — so the envelope arms on the `declared`-byte basis (the
    /// substitute leg's
    /// [`crate::substitute::NarHoldEnvelope::for_declared`] form) —
    /// tighter than the ingest-cap basis whenever declared < cap,
    /// and exact from the first byte. Everything else COMPOSES with
    /// the trailer-mode law unchanged: same private permit storage,
    /// same [`Self::bounded`] combinator, same monotone
    /// [`Self::tighten_for_tail`]; [`Self::push`] is simply never
    /// called (there are no later grants to join).
    pub(in crate::grpc) fn arm_declared(
        whole_charge: crate::budget::DeclaredCharge,
        declared: u64,
        cfg: NarIngestEnvelopeCfg,
    ) -> Self {
        Self {
            permits: Vec::new(),
            _declared_charge: Some(whole_charge),
            envelope: crate::substitute::NarHoldEnvelope::for_declared(
                declared,
                cfg.hold_stall_window,
                cfg.hold_floor_rate,
            ),
            cfg,
        }
    }

    /// Add a later grant to the hold. Deliberately does NOT touch the
    /// envelope: one clock, armed at the first grant.
    pub(in crate::grpc) fn push(&mut self, permit: tokio::sync::SemaphorePermit<'a>) {
        self.permits.push(permit);
    }

    /// THE await combinator while holding: run `fut` under the hold
    /// envelope's remaining budget; elapse sheds typed
    /// (`ResourceExhausted`, retryable — permits release with the
    /// handler frame). `what` names the span for the error text.
    pub(in crate::grpc) async fn bounded<T>(
        &self,
        what: &str,
        fut: impl std::future::Future<Output = T>,
    ) -> Result<T, Status> {
        match tokio::time::timeout(self.envelope.remaining(), fut).await {
            Ok(v) => Ok(v),
            Err(_) => Err(Status::resource_exhausted(format!(
                "{what}: NAR-budget hold exceeded its envelope ({:?} for the \
                 {}-byte basis); permits released — retry",
                self.envelope.hold_budget(),
                self.envelope.bytes_basis(),
            ))),
        }
    }

    /// Monotone tighten at stream drain: the bytes still to move are
    /// now known, so the tail bound may shrink from the cap basis to
    /// `derive(buffered)` — never grow (see
    /// [`crate::substitute::NarHoldEnvelope::tightened_for_remaining`]).
    pub(in crate::grpc) fn tighten_for_tail(&mut self, buffered: u64) {
        self.envelope = self.envelope.tightened_for_remaining(
            buffered,
            self.cfg.hold_stall_window,
            self.cfg.hold_floor_rate,
        );
    }

    /// The armed (possibly tightened) hold envelope.
    pub(in crate::grpc) fn envelope(&self) -> &crate::substitute::NarHoldEnvelope {
        &self.envelope
    }
}

/// Poll curve for [`StoreServiceImpl::wait_for_concurrent_upload`].
/// Full jitter so N losers waiting on the same winner don't hit PG in
/// lockstep; 1 s cap keeps the post-commit skip latency ≤1 s while the
/// per-poll cost (one indexed SELECT + one ON CONFLICT no-op INSERT)
/// stays negligible.
const CONCURRENT_WAIT_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: std::time::Duration::from_millis(100),
    mult: 2.0,
    cap: std::time::Duration::from_secs(1),
    jitter: rio_common::backoff::Jitter::Full,
};

/// How the NAR was persisted. Batch uses this to pick the right
/// `complete_manifest_*_in_tx` variant inside its atomic tx. Both
/// variants carry the [`cas::ParsedNar`] derived at staging time so
/// the commit transaction can write the castore index (boxed — the
/// entry list + DAG for a large output is non-trivial and this enum
/// sits in a per-output accumulator).
pub(in crate::grpc) enum NarPersist {
    /// `nar_data.len() < INLINE_THRESHOLD` (or no chunk backend).
    /// Carries the **blob stream** (regular-file contents in walk
    /// order, not the NAR) so the batch tx can write `inline_blob`.
    Inline(Bytes, Box<cas::ParsedNar>),
    /// `nar_data.len() >= INLINE_THRESHOLD` and a chunk backend is
    /// configured. Chunks already uploaded + their rows written via
    /// [`cas::stage_chunked`]; the `status='complete'` flip and the
    /// castore index write remain.
    ChunkedStaged(Box<cas::ParsedNar>),
}

/// The ingest write authority — produced ONLY by
/// [`StoreServiceImpl::verify_assignment_token`]. The tri-state makes
/// the gate's divergent service-caller policy explicit at every
/// consumer: PutPath* accept `ServiceBypass` (the gateway/scheduler
/// upload path); `AppendHwPerfSample` rejects it with a visible match
/// arm. The pre-witness `Option<AssignmentClaims>` collapsed
/// dev-mode and service-bypass into one ambiguous `None` that
/// consumers had to disambiguate by re-probing verifier knobs.
pub(in crate::grpc) enum IngestAuthority {
    /// HMAC-verified builder assignment token (path allowlist rides
    /// in the claims).
    Builder(rio_auth::hmac::AssignmentClaims),
    /// Allowlisted, VERIFIED service caller (no per-build token).
    ServiceBypass {
        /// The verified `ServiceClaims.caller`.
        #[allow(dead_code)] // named for tracing/debug; policy is the variant
        caller: String,
    },
    /// No HMAC verifier configured (dev mode) — accept all.
    DevMode,
}

impl IngestAuthority {
    /// The builder's claims when (and only when) the authority is a
    /// verified assignment token — the path-allowlist consumers'
    /// view (`validate_put_metadata`, `ingest_nar_stream`).
    pub(in crate::grpc) fn builder_claims(&self) -> Option<&rio_auth::hmac::AssignmentClaims> {
        match self {
            IngestAuthority::Builder(c) => Some(c),
            IngestAuthority::ServiceBypass { .. } | IngestAuthority::DevMode => None,
        }
    }
}

/// Auth context for PutPath / PutPathBatch: the typed ingest authority
/// (path allowlist) + JWT tenant id (signing-key selection). Both
/// extracted from request metadata BEFORE `into_inner()` consumes it.
/// Constructible only via [`StoreServiceImpl::authorize`] — a put
/// path that skips the token gate does not compile.
pub(in crate::grpc) struct PutAuth {
    pub authority: IngestAuthority,
    pub tenant_id: Option<uuid::Uuid>,
}

impl PutAuth {
    /// See [`IngestAuthority::builder_claims`].
    pub(in crate::grpc) fn builder_claims(&self) -> Option<&rio_auth::hmac::AssignmentClaims> {
        self.authority.builder_claims()
    }

    /// bug_155 (the evidence-producer half): the tenant the ingest
    /// REGISTRATION stamp records — the per-session JWT tenant (the
    /// wire/client lane) or, on the builder lane, the SIGNED
    /// `AssignmentClaims.tenant` (scheduler-minted at dispatch from
    /// the build's attributed cohort; the same trust rule as
    /// `hw_perf_samples.submitting_tenant` and the declared-mode
    /// charge bucket: attribution from CLAIMS, never the request
    /// body — a compromised worker cannot choose the tenant it
    /// stamps). Builder uploads previously stamped NOTHING (the stamp
    /// consumed only the JWT tenant, which builders never carry), so
    /// no worker-built floating-CA path ever acquired the
    /// store-recorded production evidence the scheduler's
    /// realisation/visibility consult demands — the consult refused
    /// the HONEST tenanted flow end-to-end. A malformed signed tenant
    /// fails CLOSED on this axis (warn + no stamp: evidence is a
    /// grant, never defaulted; the upload itself still succeeds and
    /// the registration grace covers a lawful re-stamp).
    pub(in crate::grpc) fn registration_tenant(&self) -> Option<uuid::Uuid> {
        if let Some(t) = self.tenant_id {
            return Some(t);
        }
        let signed = self.builder_claims().and_then(|c| c.tenant.as_deref())?;
        match uuid::Uuid::parse_str(signed) {
            Ok(u) => Some(u),
            Err(e) => {
                tracing::warn!(
                    tenant = %signed,
                    error = %e,
                    "ingest registration: malformed signed tenant attribution; \
                     skipping the stamp (re-stamp covered by the registration \
                     grace)"
                );
                None
            }
        }
    }
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
/// `PathInfo`. Shared step-1 of the write-ahead flow. The second
/// element is the N1 `declared_nar_size` opt-in, normalized to
/// `None` for 0 (the trailer-mode wire state — 0 is never a valid
/// NAR size, so zero-as-absent is unambiguous).
pub(in crate::grpc) async fn read_first_metadata(
    stream: &mut Streaming<PutPathRequest>,
) -> Result<(rio_proto::types::PathInfo, Option<u64>), Status> {
    let first = stream
        .message()
        .await?
        .ok_or_else(|| Status::invalid_argument("empty PutPath stream"))?;
    match first.msg {
        Some(put_path_request::Msg::Metadata(meta)) => {
            let declared = (meta.declared_nar_size > 0).then_some(meta.declared_nar_size);
            meta.info
                .ok_or_else(|| Status::invalid_argument("PutPathMetadata missing PathInfo"))
                .map(|info| (info, declared))
        }
        Some(put_path_request::Msg::NarChunk(_)) => Err(Status::invalid_argument(
            "first PutPath message must be metadata, not nar_chunk",
        )),
        Some(put_path_request::Msg::Trailer(_)) => Err(Status::invalid_argument(
            "first PutPath message must be metadata, not trailer",
        )),
        None => Err(Status::invalid_argument("PutPath message has no content")),
    }
}

/// Drain and discard the remaining frames of a client-streaming upload
/// before returning early.
///
/// Must be called before an early return that would leave unconsumed
/// frames on the gRPC transport — they stall the client's send loop
/// until the RST propagates. Bounded by `DEFAULT_GRPC_TIMEOUT` to
/// prevent a slow client from holding the handler indefinitely. `rpc`
/// names the calling RPC in the timeout warning.
pub(in crate::grpc) async fn drain_stream<T>(rpc: &'static str, stream: &mut Streaming<T>) {
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
            "{rpc}: drain_stream timed out; client may be sending slowly"
        );
    }
}

// r[impl store.integrity.verify-on-put+3]
// r[impl sec.drv.validate]
/// Compare a server-computed NAR digest+size against the
/// trailer-declared `nar_hash` / `nar_size` (already applied to `info`
/// via [`apply_trailer`]). The integrity gate of
/// `r[store.integrity.verify-on-put+3]` — server computes the digest
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

// r[impl sec.authz.ca-path-derived+3]
/// Floating-CA path-authorization gate. When `claims.is_ca` is set,
/// [`validate_put_metadata`] skipped the `store_path ∈
/// expected_outputs` check (the path isn't known at sign time). This
/// is the replacement gate: recompute the CA store path SERVER-SIDE
/// from the NAR hash that [`verify_nar`] just confirmed, and reject if
/// it doesn't match `info.store_path`. A worker holding an
/// `is_ca=true` token therefore cannot upload to any path that isn't
/// the content-derived path of the NAR it actually sent.
///
/// Floating-CA (`__contentAddressed = true`, non-FOD) always uses
/// `nar:sha256` recursive hashing — see `dispatch.rs`'s `is_ca` gate
/// (`state.ca.is_ca && !state.is_fixed_output`). FODs with known
/// output paths go through the IA `expected_outputs` check instead.
///
/// `None`/non-CA claims → no-op (IA already gated, dev/service
/// bypass already trusted).
pub(in crate::grpc) fn verify_ca_store_path(
    info: &ValidatedPathInfo,
    hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
    ctx_label: &str,
) -> Result<(), Status> {
    let Some(claims) = hmac_claims else {
        return Ok(());
    };
    if !claims.is_ca {
        return Ok(());
    }

    // Self-reference: Nix's `:self` token in the source-type
    // fingerprint isn't yet implemented in rio-nix's
    // `make_fixed_output`. Filter the path-under-construction out of
    // refs and reject explicitly if it was present — none in the
    // current build graph.
    let refs: Vec<rio_nix::store_path::StorePath> = info
        .references
        .iter()
        .filter(|r| r.as_str() != info.store_path.as_str())
        .cloned()
        .collect();
    if refs.len() != info.references.len() {
        return Err(Status::unimplemented(format!(
            "{ctx_label}: self-referencing floating-CA not yet supported \
             (extend make_fixed_output with :self)"
        )));
    }

    // info.nar_hash is the SERVER-COMPUTED hash here (verify_nar has
    // already confirmed it equals SHA-256(stream)).
    let nar_hash =
        rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, info.nar_hash.to_vec())
            .map_err(|e| Status::internal(format!("{ctx_label}: nar_hash construct: {e}")))?;
    let expected = rio_nix::store_path::StorePath::make_fixed_output(
        info.store_path.name(),
        &nar_hash,
        /* recursive */ true,
        &refs,
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
        let authority = self.verify_assignment_token(request)?;
        let tenant_id = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub);
        Ok(PutAuth {
            authority,
            tenant_id,
        })
    }

    // r[impl store.put.nar-bytes-budget+6]
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
        // r[impl store.put.nar-hold-envelope+2]
        // Wait axis (merged_bug_001): the acquire is grace-bounded —
        // grant or typed shed within `budget_wait_grace`, UNIFORM over
        // all chunk acquires (first included: the chokepoint cannot
        // see caller holdings). During a parked-head freeze a holder's
        // next-chunk acquire cannot be granted at all and MUST shed to
        // release; `ResourceExhausted` is the same retryable class the
        // budget-closed arm already maps to, absorbed by the upload
        // plane's retry machinery. Behavior delta (priced): deep
        // saturation longer than the grace converts from an unbounded
        // client hang into a typed shed.
        let charge = nar_chunk_charge(chunk.len()) as u32;
        let acquire = self.nar_budget.acquire_chunk(charge);
        let permit =
            match tokio::time::timeout(self.nar_ingest_envelope.budget_wait_grace, acquire).await {
                Ok(r) => r.map_err(|_| Status::resource_exhausted("NAR buffer budget closed"))?,
                Err(_) => {
                    return Err(Status::resource_exhausted(shed_message(
                        ctx_label,
                        self.nar_ingest_envelope.budget_wait_grace,
                        charge,
                        self.nar_budget.available_permits(),
                    )));
                }
            };
        nar_data.extend_from_slice(chunk);
        hasher.update(chunk);
        Ok(permit)
    }

    // r[impl store.put.declared-reserve]
    // r[impl store.budget.cost-axis]
    /// N1 declared-mode reservation: the WHOLE charge in ONE
    /// pre-stream acquisition — the ingest twin of the substitute
    /// leg's `NarBudgetReservation::reserve` (park free, THEN hold),
    /// and since merged_bug_005's close the SAME constructor: both
    /// declaration-priced modes acquire via
    /// [`crate::budget::DeclaredCharge::new`], whose signature
    /// REQUIRES the cost axis — `charge_tenant`'s aggregate
    /// outstanding declared charge is capped at
    /// [`crate::budget::TENANT_RESERVATION_CAP`] and an over-cap
    /// caller REFUSES typed (`ResourceExhausted`, retryable), never
    /// queues. Wave-9 shipped this fn as the bare sibling (whole
    /// wire-supplied charge, no ledger consult): eight ~4 GiB
    /// declarations from one worker pinned the full 32 GiB pool at
    /// zero bandwidth for the whole hold envelope, renewable.
    ///
    /// The acquiring task holds NOTHING while parked (zero permits,
    /// zero claim — the claim is taken before this call but carries
    /// no budget), so the under-cap park is the lawful unbounded
    /// zero-holding wait ("waiters park free; holders expire" —
    /// boundedness is the FIFO induction over the envelope-bounded
    /// holders, not a clock); the client's own RPC deadline bounds
    /// its patience, exactly like the trailer path's pre-first-chunk
    /// stream read. Parking is the POOL wait only — the tenant cap
    /// refuses, never queues (the substitute plane's "Refusal, never
    /// queueing" discipline, verbatim).
    ///
    /// `declared >= MAX_NAR_SIZE` refuses up front (which also makes
    /// the u32 charge cast lossless). The floor mirrors the per-chunk
    /// charge floor so a 1-byte declared upload charges what its
    /// trailer-mode twin would.
    pub(in crate::grpc) async fn reserve_declared(
        &self,
        declared: u64,
        charge_tenant: uuid::Uuid,
        ctx_label: &str,
    ) -> Result<NarIngestHold<'_>, Status> {
        let charge = crate::budget::DeclaredCharge::new(
            &self.nar_budget,
            charge_tenant,
            crate::budget::TENANT_RESERVATION_CAP,
            declared,
            || {},
        )
        .await
        .map_err(|e| match e {
            crate::budget::DeclaredRefusal::TooLarge { limit } => Status::invalid_argument(
                format!("{ctx_label}: declared_nar_size {declared} exceeds size bound {limit}"),
            ),
            // The TenantBudgetExhausted mirror on the PutPath error
            // surface: the same typed retryable class the upload
            // plane already absorbs (the BUDGET_WAIT_GRACE shed
            // precedent).
            crate::budget::DeclaredRefusal::TenantBudgetExhausted { cap } => {
                Status::resource_exhausted(format!(
                    "{ctx_label}: tenant aggregate declared reservations reached \
                     the {cap}-byte cap; retry once earlier uploads drain"
                ))
            }
            crate::budget::DeclaredRefusal::BudgetClosed => {
                Status::resource_exhausted("NAR buffer budget closed")
            }
        })?;
        Ok(NarIngestHold::arm_declared(
            charge,
            declared,
            self.nar_ingest_envelope,
        ))
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
        declared: Option<u64>,
    ) -> Result<(Vec<u8>, Option<NarIngestHold<'a>>), Status> {
        let mut nar_data = Vec::new();
        let mut hasher = Sha256::new();
        let mut trailer: Option<PutPathTrailer> = None;
        // r[impl store.put.nar-hold-envelope+2]
        // Hold axis (merged_bug_021's ingest sibling; bug_114's tiling
        // form): once this handler HOLDS budget permits, EVERY await
        // until release rides the ONE envelope armed at the first
        // grant — the stream reads here, and (via the returned
        // [`NarIngestHold`]) the caller's claim/persist tail too.
        // Before the first grant the read is untimed zero-holding
        // backpressure, symmetric with the substitute park. A
        // stopped-but-connected client (h2 keepalives held open, no
        // messages) previously pinned its permits forever: PutPath
        // claims keep `fetched_bytes` NULL — the structural exemption
        // from every download-stall rule — so no watchdog ever reaps
        // an ingest holder. Expiry aborts typed (`ResourceExhausted`);
        // placeholder cleanup stays the caller's `abort_upload`
        // contract.
        // r[impl store.put.declared-reserve]
        // N1 fork: a DECLARED sender gets reservation-mode ingest —
        // the whole charge granted single-shot BEFORE the first chunk
        // (park free, then hold), so the per-chunk acquire-while-
        // holding below is structurally unreachable on this path
        // (zero hold-and-wait). The hold is Some from here on: every
        // stream read rides the declared-basis envelope via the
        // existing `bounded` arm. Trailer senders (declared None)
        // keep the incremental path byte-for-byte.
        let mut hold: Option<NarIngestHold<'a>> = match declared {
            Some(d) => {
                // The charge tenant derives from the HMAC-VERIFIED
                // claims (scheduler-signed attribution), never the
                // request body; unattributed authorities (dev mode,
                // service bypass, tenant-less builder tokens) share
                // the capped nil bucket (merged_bug_005's cost axis).
                let h = self
                    .reserve_declared(
                        d,
                        crate::budget::declared_charge_tenant(hmac_claims),
                        "PutPath",
                    )
                    .await?;
                // Capacity is budget-backed: the permits for `d`
                // bytes are already held.
                nar_data.reserve_exact(d as usize);
                Some(h)
            }
            None => None,
        };
        // Cumulative permits charged (NOT raw bytes — `accumulate_chunk`
        // floors each chunk at MIN_NAR_CHUNK_CHARGE). Checked BEFORE
        // `accumulate_chunk` so a tiny-chunk stream that would exhaust
        // the global budget hits this cap instead of self-deadlocking on
        // `acquire_many` for permits this task already holds.
        // r[impl store.put.nar-bytes-budget+6]
        let mut charged: u64 = 0;
        loop {
            let read = stream.message();
            let msg = match &hold {
                // Zero-holding: untimed backpressure (exempt).
                None => read.await,
                // Holding: the stream read is under the one hold clock.
                Some(h) => h.bounded("PutPath ingest", read).await?,
            };
            let msg = match msg {
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
                    if let Some(d) = declared {
                        // r[impl store.put.declared-reserve]
                        // Reservation-mode accumulate: NO acquire (the
                        // whole charge is held) — only the declared
                        // BOUND, enforced at the crossing chunk
                        // (over-delivery refuses typed AT the bound,
                        // never buffers past the reservation).
                        if chunk.is_empty() {
                            return Err(Status::invalid_argument(
                                "PutPath: empty NarChunk (protocol violation)",
                            ));
                        }
                        let new_len = (nar_data.len() as u64).saturating_add(chunk.len() as u64);
                        if new_len > d {
                            return Err(Status::invalid_argument(format!(
                                "PutPath: stream exceeds its declared_nar_size {d} \
                                 (received {new_len}+ bytes) — the declaration is a \
                                 binding bound"
                            )));
                        }
                        nar_data.extend_from_slice(&chunk);
                        hasher.update(&chunk);
                        continue;
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
                    // First grant: the hold begins — arm the ONE
                    // envelope; later grants join the same hold.
                    match &mut hold {
                        None => hold = Some(NarIngestHold::arm(permit, self.nar_ingest_envelope)),
                        Some(h) => h.push(permit),
                    }
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
        // r[impl store.put.declared-reserve]
        // The declaration is BINDING: the (still mandatory) trailer
        // must equal it. Under-delivery refuses here either way — a
        // coherent-but-short sender (trailer == actual < declared)
        // dies on this equality; a lying-short sender (trailer ==
        // declared > actual) dies on verify_nar's size check below.
        // Both axes typed at commit; over-delivery already refused at
        // the bound mid-stream.
        if let Some(d) = declared
            && t.nar_size != d
        {
            return Err(Status::invalid_argument(format!(
                "PutPath: trailer nar_size {} contradicts declared_nar_size {d} — \
                 the declaration is a binding bound",
                t.nar_size
            )));
        }
        apply_trailer(info, &t, "PutPath")?;
        verify_nar(
            hasher.finalize().into(),
            nar_data.len() as u64,
            info,
            "PutPath",
        )?;
        verify_ca_store_path(info, hmac_claims, "PutPath")?;
        // Stream drained: the bytes still to move are known — the one
        // clock may tighten (never extend) for the claim/persist tail.
        if let Some(h) = &mut hold {
            h.tighten_for_tail(nar_data.len() as u64);
        }
        Ok((nar_data, hold))
    }

    /// Sign + persist + emit success metrics for a single validated
    /// output. On `persist_nar` error the placeholder is `abort_upload`ed
    /// here; the caller's drop-guard spawn is then a harmless no-op.
    /// `info.store_path_hash` MUST be populated.
    ///
    /// The caller's [`NarIngestHold`] spans this whole call, so the
    /// persist span is a budget HOLD and rides THE SAME envelope that
    /// was armed at the first grant (tightened to the buffered bytes
    /// at stream drain — bug_114: a fresh per-span clock here let the
    /// tail buy itself a new allowance after the inter-span awaits
    /// had already run unbounded). Same Q-108 rationale as the
    /// substitute tail: rio-common's S3 client ships NO TimeoutConfig
    /// by design, so an established-then-black-holed persist
    /// connection would otherwise pin the handler's permits forever —
    /// a holder span the no-deadlock theorem must bound. Expiry
    /// aborts the placeholder and sheds typed (`ResourceExhausted`,
    /// retryable). `hold: None` (a zero-chunk stream holds nothing —
    /// empty NAR) bounds the tail with a fresh minimal envelope so
    /// the span stays clocked even when the budget law does not bind.
    // r[impl obs.metric.transfer-volume]
    // r[impl store.put.nar-hold-envelope+2]
    // r[impl store.put.tenant-junction]
    pub(in crate::grpc) async fn finalize_single(
        &self,
        mut info: ValidatedPathInfo,
        claim: uuid::Uuid,
        nar_data: Vec<u8>,
        // JWT/session tenant: signing-key selection only (builders
        // carry none; the wire lane's session tenant signs narinfo).
        tenant_id: Option<uuid::Uuid>,
        // bug_155: the REGISTRATION tenant ([`PutAuth::registration_tenant`]
        // — JWT or the signed claims attribution); drives the ingest
        // evidence stamp AND the `path_tenants` junction write inside
        // `persist_nar`; never signing.
        registration_tenant: Option<uuid::Uuid>,
        hold: Option<NarIngestHold<'_>>,
    ) -> Result<(), Status> {
        // BY VALUE (bug_094): this fn owns the hold's tail so the
        // error arm can release the budget BEFORE the abort — the
        // wave-8/-9 inverse-arm class (an unbounded await sandwiched
        // between two enveloped ones while the caller's frame pinned
        // the permits) is structurally unwritable from here.
        let envelope = match &hold {
            Some(h) => *h.envelope(),
            None => crate::substitute::NarHoldEnvelope::for_declared(
                nar_data.len() as u64,
                self.nar_ingest_envelope.hold_stall_window,
                self.nar_ingest_envelope.hold_floor_rate,
            ),
        };
        let tail = async {
            self.maybe_sign(tenant_id, &mut info).await;
            self.persist_nar(&info, claim, nar_data, "PutPath", registration_tenant)
                .await
        };
        let persisted = match tokio::time::timeout(envelope.remaining(), tail).await {
            Ok(r) => r.map(|_| ()),
            Err(_) => Err(Status::resource_exhausted(format!(
                "PutPath: NAR-budget hold exceeded its persist envelope \
                 ({:?}); permits released — retry",
                envelope.hold_budget()
            ))),
        };
        if let Err(e) = persisted {
            // r[impl store.put.nar-hold-envelope+2]
            // bug_094 (drop-before-abort): the budget releases FIRST
            // — the abort needs no permits (the NAR buffer is already
            // gone: the timed-out tail future dropped it), so a
            // black-holed abort_placeholder (the bug_114 scenario) or
            // 600s row-lock contention pins NOTHING, and the
            // already-minted "permits released — retry" message is
            // true the moment it exists. Placeholder cleanup stays
            // best-effort with the orphan scanner as the recorded
            // fallback (and the reap itself is hold-consulted —
            // store.gc.hold-lanes).
            drop(hold);
            self.abort_upload(&info.store_path_hash, claim).await;
            return Err(e);
        }
        // Round-9 WO-S1-2: registration evidence at the ingest seam —
        // immediately after the persist commit, before the response
        // (the client's durability signal). Best-effort like every
        // registration stamp: the bytes ARE durable here, so a failed
        // stamp must not trigger a useless re-upload — the 2h grace +
        // the signature fallback cover until a re-stamp (warn keeps it
        // visible). The batch lane stamps INSIDE its atomic tx; this
        // single-path lane's persist owns its own tx, so post-commit
        // is the closest seam (divergence recorded in the owning
        // commit).
        //
        // r[impl store.put.nar-hold-envelope+2]
        // Merged-seam composition (S1 registration × bug_114 tiling):
        // the handler frame still HOLDS its budget permits here, so
        // this PG await rides the same ONE clock as the persist tail
        // — a blocked-PG black hole must release by the hold
        // deadline, never pin the budget on a best-effort stamp.
        // Elapse degrades exactly like a failed stamp (skip + warn;
        // the registration grace covers the re-stamp).
        let stamping = stamp_ingest_tenant(&self.pool, &info.store_path_hash, registration_tenant);
        if tokio::time::timeout(envelope.remaining(), stamping)
            .await
            .is_err()
        {
            warn!(
                store_path = %info.store_path.as_str(),
                "PutPath: ingest-stamp skipped (hold envelope elapsed); \
                 re-stamp covered by the registration grace"
            );
        }
        metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
        metrics::counter!("rio_store_put_path_bytes_total").increment(info.nar_size);
        Ok(())
    }

    /// `path_tenants` junction for an idempotent-skipped path
    /// (r[store.put.tenant-junction]): the prior commit may belong to
    /// another tenant (or predate tenancy — legacy uploads), and the
    /// skipping caller still needs castore read access and a GC pin.
    /// Tolerates a tenant deleted mid-flight, same as the in-tx variant.
    // r[impl store.put.tenant-junction]
    pub(in crate::grpc) async fn insert_path_tenant_skipped(
        &self,
        store_path_hash: &[u8],
        tenant_id: Option<uuid::Uuid>,
    ) -> Result<(), Status> {
        if tenant_id.is_none() {
            return Ok(());
        }
        let result = async {
            let mut conn = self.pool.acquire().await?;
            metadata::insert_path_tenant_in_conn(&mut conn, store_path_hash, tenant_id).await
        }
        .await;
        match result {
            Ok(()) => Ok(()),
            Err(e) if metadata::is_deleted_tenant_fk(&e) => {
                warn!(
                    store_path_hash = hex::encode(store_path_hash),
                    "PutPath: path_tenants junction skipped — tenant was deleted while the \
                     upload was in flight"
                );
                Ok(())
            }
            Err(e) => Err(putpath_metadata_status("PutPath: path_tenants", e)),
        }
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
        // `stall: None`, `progress: None` — PutPath claims carry no
        // narinfo-declared size and write no progress evidence; the
        // download-stall takeover arm is structurally unreachable for
        // them (`r[store.substitute.stale-reclaim+4]`), and
        // `fetched_bytes` stays NULL — the structural exemption from
        // every download-stall rule
        // (`r[store.substitute.progress-heartbeat]`). sh-023: the
        // guard is pre-armed inside `claim_placeholder` and carried by
        // value in `Owned`; this wrapper no longer spawns it.
        let claim = ingest::claim_placeholder(
            &self.pool,
            store_path_hash,
            store_path,
            refs,
            PUTPATH_HOOKS,
            None,
            None,
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

    /// Bounded wait for a concurrent same-path uploader to resolve.
    ///
    /// Layer rationale: the loser of a same-path race needs no bytes to
    /// finish — its optimal outcome is the idempotent skip — so waiting
    /// HERE, where the winner's placeholder row is directly observable,
    /// fixes every client at once: the gateway's buffered re-send retry
    /// (`gw.put.aborted-retry`, ~6 s budget tuned for KB `.drv` NARs —
    /// no match for a winner streaming a chunked NAR for tens of
    /// seconds), the gateway's streaming path (cannot retry at all —
    /// bytes already consumed off the wire), and the builder's upload
    /// retry. The wire contract is unchanged; the RPC just resolves
    /// later, bounded by `concurrent_put_wait` ≪ `GRPC_STREAM_TIMEOUT`.
    ///
    /// Re-runs the full [`ingest::claim_placeholder`] per poll so every
    /// transition is handled: winner commits → `AlreadyComplete`
    /// (caller takes the idempotent-skip path), winner aborts/dies →
    /// placeholder reaped → `Owned` (caller takes over the upload),
    /// winner still streaming → keep polling. Returns `Concurrent` only
    /// when the budget expires with the uploader still live — the
    /// caller surfaces the original `ABORTED` and the client's retry
    /// logic stays in charge.
    // r[impl store.put.concurrent-wait]
    pub(in crate::grpc) async fn wait_for_concurrent_upload(
        &self,
        store_path_hash: &[u8],
        store_path: &str,
        refs: &[String],
    ) -> Result<PlaceholderClaim, metadata::MetadataError> {
        let budget = self.concurrent_put_wait;
        let start = std::time::Instant::now();
        let mut attempt = 0u32;
        // Entry marker, before the first poll: operators see waiters
        // that are still parked (the outcome labels below only fire on
        // resolution), and tests latch on it instead of sleeping.
        metrics::counter!("rio_store_putpath_concurrent_wait_total",
            "outcome" => "waiting")
        .increment(1);
        loop {
            let elapsed = start.elapsed();
            if elapsed >= budget {
                metrics::counter!("rio_store_putpath_concurrent_wait_total",
                    "outcome" => "timeout")
                .increment(1);
                return Ok(PlaceholderClaim::Concurrent);
            }
            let delay = CONCURRENT_WAIT_BACKOFF
                .duration(attempt)
                .min(budget - elapsed);
            tokio::time::sleep(delay).await;
            attempt = attempt.saturating_add(1);
            // Direct ingest call, NOT the metric-wrapping
            // `claim_placeholder` method: the per-RPC
            // `concurrent_upload` retry counter fired once on the
            // initial claim; per-poll increments would inflate it ~1/s
            // per waiter.
            match ingest::claim_placeholder(
                &self.pool,
                store_path_hash,
                store_path,
                refs,
                PUTPATH_HOOKS,
                None,
                None,
            )
            .await?
            {
                PlaceholderClaim::Concurrent => {}
                PlaceholderClaim::AlreadyComplete => {
                    debug!(%store_path, waited = ?start.elapsed(),
                        "PutPath: concurrent upload committed; idempotent skip");
                    metrics::counter!("rio_store_putpath_concurrent_wait_total",
                        "outcome" => "completed")
                    .increment(1);
                    return Ok(PlaceholderClaim::AlreadyComplete);
                }
                PlaceholderClaim::Owned(claim) => {
                    debug!(%store_path, waited = ?start.elapsed(),
                        "PutPath: concurrent upload aborted; taking over the placeholder");
                    metrics::counter!("rio_store_putpath_concurrent_wait_total",
                        "outcome" => "takeover")
                    .increment(1);
                    return Ok(PlaceholderClaim::Owned(claim));
                }
            }
        }
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
        junction_tenant: Option<uuid::Uuid>,
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
            junction_tenant,
        )
        .await
        .map_err(|e| match e {
            ingest::PersistError::Chunked(e) => storage_error(ctx_label, e),
            ingest::PersistError::Inline(e) => putpath_metadata_status(ctx_label, e),
            // Hash matched but the bytes are not a structurally valid
            // NAR — a client bug, not a storage failure. Non-retryable.
            ingest::PersistError::Malformed(e) => {
                Status::invalid_argument(format!("{ctx_label}: {e:#}"))
            }
        })?;
        Ok(chunked)
    }

    /// Batch-phase staging: for outputs ≥ [`cas::INLINE_THRESHOLD`],
    /// upload chunks + write their rows via [`cas::stage_chunked`]
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
        // Same parse-once as `ingest::persist_nar` — the batch's atomic
        // commit needs the castore representation per output, and the
        // chunked staging needs the per-file content ranges.
        let parsed = cas::cpu_bound(|| cas::ParsedNar::parse(&nar_data)).map_err(|e| {
            Status::invalid_argument(format!(
                "PutPathBatch: NAR for {} failed structural validation: {e:#}",
                info.store_path.as_str()
            ))
        })?;
        if let Some(backend) = cas::should_chunk(self.chunk_backend.as_ref(), nar_data.len()) {
            let stats = cas::stage_chunked(
                &self.pool,
                backend,
                info,
                claim,
                &nar_data,
                &parsed,
                self.chunk_upload_max_concurrent,
            )
            .await
            .map_err(|e| storage_error("PutPathBatch: stage_chunked", e))?;
            metrics::gauge!("rio_store_chunk_dedup_ratio").set(stats.dedup_ratio());
            Ok(NarPersist::ChunkedStaged(Box::new(parsed)))
        } else {
            let blob = parsed.blob_stream(&nar_data);
            Ok(NarPersist::Inline(Bytes::from(blob), Box::new(parsed)))
        }
    }
}

// r[verify sec.drv.validate]
// r[verify store.integrity.verify-on-put+3]
#[cfg(test)]
mod verify_nar_tests {
    use super::*;
    use rio_test_support::fixtures::{make_path_info_for_nar, test_store_path};

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

// ===========================================================================
// Round-9 WO-S1-2 — the ingestion-lane registration stamp (the signed
// Q1 invariant's generality leg: every byte-complete upload the store
// accepted is registered evidence). The ingest commit seam is the one
// place EVERY upload passes; the witness here is the AUTHENTICATED
// PutAuth tenant itself (the JWT-verified uploader — the store-side
// evidence class beside the scheduler's BuiltLocally lane: stamps
// EXACTLY the uploading tenant, never a wider set). Anonymous
// uploads (no tenant claims) register no per-tenant ownership — the
// anonymous view is unfiltered by design.
// ===========================================================================

/// THE one ingest-lane INSERT body (census-pinned: the store crate's
/// sole production `path_tenants` writer — see the
/// `registration_writer_census` test below). Shared by the
/// single-path post-persist stamp and the batch in-tx stamp via the
/// generic executor bound. `ON CONFLICT DO NOTHING`: a re-upload by
/// the same tenant is idempotent.
// r[impl store.registration.ingest-stamps]
async fn insert_path_tenant_rows<'e, E>(
    exec: E,
    hashes: &[Vec<u8>],
    tenant_id: uuid::Uuid,
) -> Result<u64, sqlx::Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if hashes.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        r#"
        INSERT INTO path_tenants (store_path_hash, tenant_id)
        SELECT u.h, $2 FROM UNNEST($1::bytea[]) AS u(h)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(hashes)
    .bind(tenant_id)
    .execute(exec)
    .await?;
    Ok(result.rows_affected())
}

/// Single-path lane (PutPath): stamp immediately after the persist
/// commit, before the response. Best-effort — the bytes ARE durable
/// here, so a failed stamp must not trigger a useless re-upload; the
/// 2h GC grace + the signature visibility fallback cover until a
/// re-stamp (warn keeps it operator-visible). `None` tenant
/// (anonymous/dev) is a typed no-op.
pub(in crate::grpc) async fn stamp_ingest_tenant(
    pool: &sqlx::PgPool,
    store_path_hash: &[u8],
    tenant_id: Option<uuid::Uuid>,
) {
    let Some(tid) = tenant_id else {
        return;
    };
    if let Err(e) = insert_path_tenant_rows(pool, &[store_path_hash.to_vec()], tid).await {
        warn!(
            tenant_id = %tid,
            error = %e,
            "PutPath: ingest registration stamp failed; tenant visibility \
             rides the signature fallback and GC retention rides the \
             grace window until a re-stamp"
        );
    }
}

/// Batch lane (PutPathBatch): stamp every CREATED output inside the
/// SAME atomic transaction that completes the manifests — a torn
/// stamp/commit pair is unrepresentable. Err propagates: the batch's
/// whole point is atomicity, and a failing INSERT here means the tx
/// is dying anyway.
pub(in crate::grpc) async fn stamp_ingest_tenant_in_tx(
    conn: &mut sqlx::PgConnection,
    hashes: &[Vec<u8>],
    tenant_id: uuid::Uuid,
) -> Result<(), sqlx::Error> {
    insert_path_tenant_rows(conn, hashes, tenant_id)
        .await
        .map(|_| ())
}

// ===========================================================================
// W9-E (round-9 WO-S1-2) — the registration-writer census, store crate
// half (the scheduler half lives beside `upsert_path_tenants_raw` in
// rio-scheduler/src/db/live_pins.rs; per-crate censuses only).
// Proposition: the store's ONE production ownership-INSERT is
// `insert_path_tenant_rows` above; every other occurrence is a test
// seed pinned by exact count. Source-scanning generator over the
// EMBEDDED whole-crate universe (the substitute.rs CENSUS_SOURCES
// hybrid — hazard (vvvvv): the nix gate runs test binaries without
// the source tree on disk, so a runtime-only walk is
// premise-unreachable exactly where it gates); completeness vs the
// live tree pinned BOTH directions on dev runs, sandbox skip
// disclosed.
// ===========================================================================
#[cfg(test)]
mod registration_writer_census {
    use std::collections::BTreeMap;

    /// EVERY `.rs` under `rio-store/src`, embedded at compile time.
    /// Machine-generated (generator command in the owning commit
    /// body); the completeness pin below tracks the live tree.
    const CENSUS_SOURCES: &[(&str, &str)] = &[
        ("admission.rs", include_str!("../../admission.rs")),
        ("authz.rs", include_str!("../../authz.rs")),
        ("backend/mod.rs", include_str!("../../backend/mod.rs")),
        ("backend/tiered.rs", include_str!("../../backend/tiered.rs")),
        ("budget.rs", include_str!("../../budget.rs")),
        ("cas.rs", include_str!("../../cas.rs")),
        ("castore.rs", include_str!("../../castore.rs")),
        ("castore_nar.rs", include_str!("../../castore_nar.rs")),
        ("chunker.rs", include_str!("../../chunker.rs")),
        ("config.rs", include_str!("../../config.rs")),
        ("error.rs", include_str!("../../error.rs")),
        ("gc/collect.rs", include_str!("../../gc/collect.rs")),
        ("gc/drain.rs", include_str!("../../gc/drain.rs")),
        ("gc/lane.rs", include_str!("../../gc/lane.rs")),
        ("gc/lock.rs", include_str!("../../gc/lock.rs")),
        ("gc/mark.rs", include_str!("../../gc/mark.rs")),
        (
            "gc/mark_scan_bench.rs",
            include_str!("../../gc/mark_scan_bench.rs"),
        ),
        ("gc/mod.rs", include_str!("../../gc/mod.rs")),
        ("gc/orphan.rs", include_str!("../../gc/orphan.rs")),
        ("gc/state.rs", include_str!("../../gc/state.rs")),
        ("gc/sweep.rs", include_str!("../../gc/sweep.rs")),
        ("gc/tenant.rs", include_str!("../../gc/tenant.rs")),
        ("grpc/admin.rs", include_str!("../../grpc/admin.rs")),
        ("grpc/chunk.rs", include_str!("../../grpc/chunk.rs")),
        ("grpc/directory.rs", include_str!("../directory.rs")),
        ("grpc/get_path.rs", include_str!("../../grpc/get_path.rs")),
        ("grpc/mod.rs", include_str!("../../grpc/mod.rs")),
        (
            "grpc/put_path/common.rs",
            include_str!("../../grpc/put_path/common.rs"),
        ),
        (
            "grpc/put_path/mod.rs",
            include_str!("../../grpc/put_path/mod.rs"),
        ),
        (
            "grpc/put_path_batch.rs",
            include_str!("../../grpc/put_path_batch.rs"),
        ),
        (
            "grpc/put_path_chunked/commit.rs",
            include_str!("../put_path_chunked/commit.rs"),
        ),
        (
            "grpc/put_path_chunked/file_digest.rs",
            include_str!("../put_path_chunked/file_digest.rs"),
        ),
        (
            "grpc/put_path_chunked/mod.rs",
            include_str!("../put_path_chunked/mod.rs"),
        ),
        (
            "grpc/put_path_chunked/validate.rs",
            include_str!("../put_path_chunked/validate.rs"),
        ),
        (
            "grpc/put_path_chunked/verify.rs",
            include_str!("../put_path_chunked/verify.rs"),
        ),
        ("grpc/queries.rs", include_str!("../../grpc/queries.rs")),
        ("grpc/sign.rs", include_str!("../../grpc/sign.rs")),
        ("ingest.rs", include_str!("../../ingest.rs")),
        ("lib.rs", include_str!("../../lib.rs")),
        (
            "logs/ack_census.rs",
            include_str!("../../logs/ack_census.rs"),
        ),
        ("logs/chunks.rs", include_str!("../../logs/chunks.rs")),
        ("logs/gate.rs", include_str!("../../logs/gate.rs")),
        ("logs/ingest.rs", include_str!("../../logs/ingest.rs")),
        ("logs/loss.rs", include_str!("../../logs/loss.rs")),
        ("logs/mbt_tests.rs", include_str!("../../logs/mbt_tests.rs")),
        ("logs/mod.rs", include_str!("../../logs/mod.rs")),
        ("logs/service.rs", include_str!("../../logs/service.rs")),
        ("logs/sessions.rs", include_str!("../../logs/sessions.rs")),
        ("logs/sweep.rs", include_str!("../../logs/sweep.rs")),
        ("logs/tail.rs", include_str!("../../logs/tail.rs")),
        ("main.rs", include_str!("../../main.rs")),
        ("manifest.rs", include_str!("../../manifest.rs")),
        (
            "materialize/client.rs",
            include_str!("../../materialize/client.rs"),
        ),
        (
            "materialize/executor.rs",
            include_str!("../../materialize/executor.rs"),
        ),
        (
            "materialize/mod.rs",
            include_str!("../../materialize/mod.rs"),
        ),
        (
            "metadata/chunked.rs",
            include_str!("../../metadata/chunked.rs"),
        ),
        (
            "metadata/cluster_key_history.rs",
            include_str!("../../metadata/cluster_key_history.rs"),
        ),
        (
            "metadata/inline.rs",
            include_str!("../../metadata/inline.rs"),
        ),
        ("metadata/mod.rs", include_str!("../../metadata/mod.rs")),
        (
            "metadata/queries.rs",
            include_str!("../../metadata/queries.rs"),
        ),
        (
            "metadata/tenant_keys.rs",
            include_str!("../../metadata/tenant_keys.rs"),
        ),
        (
            "metadata/upstreams.rs",
            include_str!("../../metadata/upstreams.rs"),
        ),
        ("nar_index.rs", include_str!("../../nar_index.rs")),
        ("realisations.rs", include_str!("../../realisations.rs")),
        ("signing.rs", include_str!("../../signing.rs")),
        ("substitute.rs", include_str!("../../substitute.rs")),
        ("test_helpers.rs", include_str!("../../test_helpers.rs")),
        ("visibility.rs", include_str!("../../visibility.rs")),
    ];

    /// Needles assembled at runtime so the census never matches its
    /// own source text.
    fn census(parts: &[&str]) -> BTreeMap<String, usize> {
        let needle = parts.join("");
        let mut hits = BTreeMap::new();
        for (rel, text) in CENSUS_SOURCES {
            let n = text.matches(&needle).count();
            if n > 0 {
                *hits.entry((*rel).to_string()).or_insert(0) += n;
            }
        }
        hits
    }

    /// Dev-tree completeness pin (both directions; sandbox skip
    /// disclosed — the substitute.rs form).
    #[test]
    fn census_universe_matches_live_tree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        if !root.exists() {
            eprintln!(
                "src/ not on disk (nix sandbox): universe pinned by the \
                 dev-tree run of this same commit"
            );
            return;
        }
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("readable src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(
                        path.strip_prefix(root)
                            .expect("under root")
                            .to_str()
                            .expect("source paths are utf-8")
                            .to_owned(),
                    );
                }
            }
        }
        let mut live: Vec<String> = Vec::new();
        walk(&root, &root, &mut live);
        live.sort();
        let mut embedded: Vec<String> = CENSUS_SOURCES.iter().map(|(f, _)| f.to_string()).collect();
        embedded.sort();
        assert_eq!(
            embedded, live,
            "census universe drifted from the live tree: add/remove the \
             named files in CENSUS_SOURCES so the registration census sees \
             the whole crate in the nix sandbox too"
        );
    }

    /// ONE production ownership-INSERT (this file's
    /// `insert_path_tenant_rows`); every other occurrence is a
    /// `#[cfg(test)]` seed, pinned by exact count — a count drift in a
    /// pinned file means the diff touched a seed (re-derive); a NEW
    /// file means an uncensused writer (the W9-E reject).
    #[test]
    fn store_ownership_insert_census() {
        let hits = census(&["INSERT INTO ", "path_tenants"]);
        let expected: BTreeMap<String, usize> = [
            // the TWO production writers: insert_path_tenant_rows
            // (PutPath/PutPathBatch's pool-level stamp) and
            // metadata/chunked.rs's insert_path_tenant_in_conn
            // (PutPathChunked's in-transaction variant — the output
            // commit transaction stamps tenant atomically with the
            // manifest_data insert, ADR-022 §6.4). Both route through
            // the same `path_tenants` UPSERT shape; the in-conn one
            // exists so PutPathChunked's single commit transaction
            // does not re-acquire the pool mid-commit.
            ("grpc/put_path/common.rs".to_string(), 1),
            ("metadata/chunked.rs".to_string(), 1),
            // test seeds (visibility/gc/executor/castore fixtures)
            ("grpc/directory.rs".to_string(), 1),
            ("grpc/sign.rs".to_string(), 5),
            ("gc/mark.rs".to_string(), 4),
            ("gc/mod.rs".to_string(), 2),
            ("gc/sweep.rs".to_string(), 4),
            ("materialize/executor.rs".to_string(), 1),
            ("metadata/queries.rs".to_string(), 2),
        ]
        .into();
        assert_eq!(
            hits, expected,
            "the store-crate ownership-INSERT census moved — the ingest \
             stamp (insert_path_tenant_rows) is the sole production \
             writer; route new registration writes through it or census \
             them here with their witness rationale"
        );
    }

    /// W9-G (round-9 WO-S1-3), store half: the store's
    /// realisation-INSERT population is `realisations.rs` (the
    /// RegisterRealisation authority) plus one gc/mod.rs identity seed
    /// and one gc/sweep.rs tombstone-battery seed, pinned by exact
    /// count.
    #[test]
    fn realisation_writers_pinned() {
        let hits = census(&["INSERT INTO ", "realisations"]);
        let expected: BTreeMap<String, usize> = [
            ("realisations.rs".to_string(), 1),
            ("gc/mod.rs".to_string(), 1),
            ("gc/sweep.rs".to_string(), 1),
        ]
        .into();
        assert_eq!(
            hits, expected,
            "the store realisation-INSERT census moved — identity rows \
             are written by the realisations.rs authority only; census \
             new writers here with their witness rationale"
        );
    }
}

// r[verify store.budget.cost-axis]
#[cfg(test)]
mod declared_budget_tests {
    use super::*;

    fn lazy_svc() -> StoreServiceImpl {
        StoreServiceImpl::new(sqlx::PgPool::connect_lazy("postgres://unused").expect("lazy pool"))
    }

    /// W10-A (merged_bug_005): a single tenant's AGGREGATE declared
    /// charges cannot exceed `TENANT_RESERVATION_CAP` — the law at
    /// its own quantifier (aggregate, not per-charge). Pre-fix,
    /// `reserve_declared` granted the whole wire-supplied size from
    /// the shared pool with no ledger consult: eight (4 GiB − 1)
    /// declarations from ONE caller pinned the full 32 GiB pool at
    /// zero bandwidth (the red ran in the pre-fix one-arg form; this
    /// is the same proposition at the post-fix signature). Post-fix:
    /// charges 1–2 grant (the preserved two-max-closure warm),
    /// charge 3+ refuses typed at the 8 GiB aggregate — even a
    /// minimum-size declaration.
    #[tokio::test]
    async fn declared_reservations_charge_the_tenant_ledger() {
        let svc = lazy_svc();
        let tenant = uuid::Uuid::from_u128(0x51);
        let big = rio_common::limits::MAX_NAR_SIZE - 1;
        let mut holds = Vec::new();
        let mut refusal = None;
        for i in 0..8u32 {
            match svc.reserve_declared(big, tenant, "W10-A").await {
                Ok(h) => holds.push(h),
                Err(e) => {
                    refusal = Some((i, e));
                    break;
                }
            }
        }
        let (at, e) = refusal.expect(
            "W10-A RED: a single tenant pinned the whole pool — all 8 \
             declaration-priced charges granted with no ledger consult",
        );
        assert_eq!(
            at, 2,
            "the third aggregate charge must be the refusal point \
             (2 x (4GiB-1) holds the two-max-closure warm; +1 exceeds \
             the 8 GiB cap)"
        );
        assert_eq!(
            e.code(),
            tonic::Code::ResourceExhausted,
            "over-cap refusal is the typed retryable class, got {e:?}"
        );
        assert!(
            e.message().contains("tenant"),
            "the refusal names the cost axis (tenant aggregate), got: {}",
            e.message()
        );

        // The aggregate quantifier: even a MINIMUM-size declaration
        // refuses while the tenant sits at its cap — the per-charge
        // axis cannot launder the cost axis.
        let min_refused = svc.reserve_declared(1, tenant, "W10-A").await;
        assert!(
            min_refused.is_err_and(|e| e.code() == tonic::Code::ResourceExhausted),
            "a min-size declaration must also refuse at the aggregate cap"
        );

        // The cap is PER-TENANT (the pool survives one tenant's cap):
        // a different tenant's first charge grants immediately.
        let other = uuid::Uuid::from_u128(0x52);
        let other_hold = svc
            .reserve_declared(big, other, "W10-A")
            .await
            .expect("a different tenant has its own headroom");
        drop(other_hold);

        // RAII release restores headroom: dropping one of the held
        // reservations lets the refused tenant charge again.
        drop(holds.pop());
        let _re = svc
            .reserve_declared(big, tenant, "W10-A")
            .await
            .expect("released charge restores tenant headroom");
    }

    /// bug_155 (the evidence-producer half): the registration-tenant
    /// derivation is total over the authority × session matrix — the
    /// JWT/session tenant wins when present (the wire lane); the
    /// builder lane falls back to the SIGNED claims attribution (the
    /// scheduler-minted cohort — previously the builder lane stamped
    /// NOTHING and no worker-built floating-CA path ever acquired
    /// evidence); malformed signed tenants fail CLOSED (no stamp —
    /// evidence is a grant, never defaulted); dev/service authorities
    /// without a session tenant stamp nothing.
    #[test]
    fn registration_tenant_derivation_is_total() {
        let claims = |tenant: Option<&str>| rio_auth::hmac::AssignmentClaims {
            executor_id: "w".into(),
            drv_hash: "d".into(),
            expected_outputs: vec![],
            is_ca: true,
            expiry_unix: 0,
            tenant: tenant.map(|t| t.to_string()),
            input_closure_digest: String::new(),
        };
        let t_jwt = uuid::Uuid::from_u128(11);
        let t_claims = uuid::Uuid::from_u128(7);

        // Builder lane, signed cohort: the claims tenant stamps.
        let auth = PutAuth {
            authority: IngestAuthority::Builder(claims(Some(&t_claims.to_string()))),
            tenant_id: None,
        };
        assert_eq!(
            auth.registration_tenant(),
            Some(t_claims),
            "left (pre-fix): builder uploads stamped NOTHING — the \
             consult refused the honest tenanted flow / right: the \
             signed claims attribution is the evidence producer"
        );

        // Session tenant wins when present (the wire lane).
        let auth = PutAuth {
            authority: IngestAuthority::Builder(claims(Some(&t_claims.to_string()))),
            tenant_id: Some(t_jwt),
        };
        assert_eq!(auth.registration_tenant(), Some(t_jwt));

        // Malformed signed tenant: fail closed, no stamp.
        let auth = PutAuth {
            authority: IngestAuthority::Builder(claims(Some("not-a-uuid"))),
            tenant_id: None,
        };
        assert_eq!(auth.registration_tenant(), None);

        // Tenant-less claims / dev / service: nothing to stamp.
        for authority in [
            IngestAuthority::Builder(claims(None)),
            IngestAuthority::ServiceBypass {
                caller: "gw".into(),
            },
            IngestAuthority::DevMode,
        ] {
            let auth = PutAuth {
                authority,
                tenant_id: None,
            };
            assert_eq!(auth.registration_tenant(), None);
        }
    }

    /// The unattributed-bucket derivation (the cost axis is total
    /// over authorities): no claims, tenant-less claims, and
    /// malformed signed tenants all charge the capped nil bucket;
    /// a well-formed signed tenant charges its own.
    #[test]
    fn declared_charge_tenant_derivation_is_total() {
        use crate::budget::{UNATTRIBUTED_DECLARED_BUCKET, declared_charge_tenant};
        assert_eq!(declared_charge_tenant(None), UNATTRIBUTED_DECLARED_BUCKET);
        let mut claims = rio_auth::hmac::AssignmentClaims {
            executor_id: "w".into(),
            drv_hash: "d".into(),
            expected_outputs: vec![],
            is_ca: false,
            expiry_unix: 0,
            tenant: None,
            input_closure_digest: String::new(),
        };
        assert_eq!(
            declared_charge_tenant(Some(&claims)),
            UNATTRIBUTED_DECLARED_BUCKET,
            "tenant-less builder claims share the capped bucket"
        );
        claims.tenant = Some("not-a-uuid".into());
        assert_eq!(
            declared_charge_tenant(Some(&claims)),
            UNATTRIBUTED_DECLARED_BUCKET,
            "malformed signed attribution fails closed into the capped bucket"
        );
        let t = uuid::Uuid::from_u128(7);
        claims.tenant = Some(t.to_string());
        assert_eq!(
            declared_charge_tenant(Some(&claims)),
            t,
            "the signed tenant is the charge authority"
        );
    }
}

/// The chunk-shed disclosure (merged_bug_101): state the MEASURED
/// cause, never an unconditional claim. The wave-10 text said "pod at
/// its in-flight NAR-bytes bound" on every shed, which lied exactly in
/// the parked-head case (gigabytes free, the chunk merely queued
/// behind a whole-NAR declared reservation). Both arms disclose the
/// shed-instant measurement and keep the retry honesty (the same
/// retryable class the upload plane's machinery absorbs).
fn shed_message(ctx_label: &str, grace: Duration, charge: u32, available: usize) -> String {
    if available >= charge as usize {
        format!(
            "{ctx_label}: NAR buffer budget wait exceeded {grace:?} \
             (queued behind outstanding NAR reservations; {available} \
             bytes free at shed >= the {charge} B charge); retry"
        )
    } else {
        format!(
            "{ctx_label}: NAR buffer budget wait exceeded {grace:?} \
             (at the pod's in-flight NAR-bytes bound; {available} \
             bytes free at shed < the {charge} B charge); retry"
        )
    }
}

#[cfg(test)]
mod shed_message_tests {
    use super::*;
    use rio_test_support::TestDb;

    // r[verify store.budget.lane-fairness]
    /// W11-AS, the put_path face (merged_bug_101): the chunk-lane
    /// fairness at the production chokepoint plus the honest shed
    /// message. Schedule: near-full pool, a declared whole-NAR
    /// reservation parked at the declared face's head (through the
    /// production `DeclaredCharge` constructor on the same budget
    /// instance the service wires), then an in-flight chunk arrives
    /// at `accumulate_chunk`.
    ///
    /// Pre-fix red (verbatim in the commit body): the chunk acquire
    /// starved behind the parked head despite ~1 GiB free and shed
    /// with the LYING message "(pod at its in-flight NAR-bytes
    /// bound)". Post-fix: the chunk grants through the lane within
    /// the grace; the lying static claim is gone from the shed arm.
    #[tokio::test]
    async fn chunk_accumulate_drains_past_parked_declaration_with_honest_shed() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let pool_bytes = (rio_common::limits::MAX_NAR_SIZE + (1 << 30)) as usize;
        let svc = StoreServiceImpl::new(db.pool.clone())
            .with_nar_budget(pool_bytes)
            .with_nar_ingest_envelope(NarIngestEnvelopeCfg {
                budget_wait_grace: Duration::from_millis(100),
                hold_stall_window: Duration::from_millis(5_000),
                hold_floor_rate: 1,
            });
        let budget = svc.nar_budget().clone();
        let big = rio_common::limits::MAX_NAR_SIZE - 1;

        // d1 holds most of the declared face (production constructor).
        let d1 = crate::budget::DeclaredCharge::new(
            &budget,
            uuid::Uuid::from_u128(0xA54),
            crate::budget::TENANT_RESERVATION_CAP,
            big,
            || {},
        )
        .await
        .expect("d1 grants");
        // d2 parks at the declared face's head.
        let d2 = tokio::spawn({
            let budget = budget.clone();
            async move {
                crate::budget::DeclaredCharge::new(
                    &budget,
                    uuid::Uuid::from_u128(0xA55),
                    crate::budget::TENANT_RESERVATION_CAP,
                    big,
                    || {},
                )
                .await
            }
        });
        tokio::task::yield_now().await;

        // The in-flight chunk at the production chokepoint.
        let chunk = vec![0u8; 64 << 20];
        let mut nar_data = Vec::new();
        let mut hasher = Sha256::new();
        let permit = svc
            .accumulate_chunk(&mut nar_data, &mut hasher, &chunk, "w11-as")
            .await
            .expect(
                "W11-AS: the in-flight chunk drains through the lane within \
                 the grace; pre-fix it starved behind the parked declared \
                 head and shed with the lying at-bound message",
            );

        // W11-AT control: the parked declaration retains liveness.
        drop(d1);
        let d2 = tokio::time::timeout(Duration::from_secs(5), d2)
            .await
            .expect("W11-AT: the parked declaration grants when the face frees")
            .expect("join")
            .expect("d2 grants");
        drop(d2);
        drop(permit);
    }

    /// The shed message's two measured arms (the honest causes —
    /// pure cells over the extracted constructor): free >= charge is
    /// the queue-position disclosure; free < charge is the in-flight
    /// bound. The wave-10 message claimed the bound unconditionally.
    #[test]
    fn shed_message_states_the_measured_cause() {
        let q = shed_message("ctx", Duration::from_secs(3), 65_536, 1 << 30);
        assert!(
            q.contains("queued behind outstanding NAR reservations"),
            "free >= charge names queue position, got: {q}"
        );
        assert!(q.contains("retry"), "retry honesty rides every arm: {q}");
        let b = shed_message("ctx", Duration::from_secs(3), 65_536, 256);
        assert!(
            b.contains("at the pod's in-flight NAR-bytes bound"),
            "free < charge names the bound, got: {b}"
        );
        assert!(b.contains("retry"), "retry honesty rides every arm: {b}");
        // Both arms disclose the measurement (free bytes at shed).
        for m in [&q, &b] {
            assert!(
                m.contains("free at shed"),
                "the measurement is disclosed: {m}"
            );
        }
    }
}

// r[verify store.put.nar-hold-envelope+2]
#[cfg(test)]
mod drop_before_abort_tests {
    use super::*;
    use rio_test_support::TestDb;
    use rio_test_support::fixtures::{make_path_info_for_nar, test_store_path};

    /// W10-J (bug_094): permit hold time <= the envelope under a
    /// black-holed abort. Harness: the placeholder row is FOR
    /// UPDATE-locked by an open tx (the bug_114 / 600s row-lock
    /// contention shape, deterministic — a Notify-free structural
    /// gate), the envelope is shrunk so the persist tail times out
    /// instantly, and the probe asserts the budget returns to FULL
    /// while the abort is still blocked on the lock. Pre-fix the
    /// caller's frame pinned the declared-size permits for the whole
    /// abort (red: the probe deadline fires with permits missing);
    /// post-fix drop-before-abort releases them before the abort's
    /// first PG statement.
    #[tokio::test]
    async fn budget_releases_before_blocked_abort() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc =
            StoreServiceImpl::new(db.pool.clone()).with_nar_ingest_envelope(NarIngestEnvelopeCfg {
                budget_wait_grace: Duration::from_millis(50),
                hold_stall_window: Duration::from_millis(10),
                hold_floor_rate: u64::MAX, // transfer allowance ~0
            });
        let full = svc.nar_budget().available_permits();

        // The placeholder the abort will try to reap. The NAR must be
        // structurally valid: persist_nar's `ParsedNar::parse` runs
        // synchronously BEFORE the envelope-guarded PG writes, so a
        // garbage payload short-circuits to InvalidArgument before the
        // timeout this test asserts can fire.
        let payload = vec![0u8; 1024 * 1024];
        let (nar, _h) = rio_test_support::fixtures::make_nar(&payload);
        let mut info = make_path_info_for_nar(&test_store_path("w10j"), &nar);
        let claim = crate::metadata::insert_manifest_uploading(
            &db.pool,
            &info.store_path_hash.clone(),
            info.store_path.as_str(),
            &[],
        )
        .await
        .unwrap()
        .expect("fresh placeholder");
        info.nar_hash = [0u8; 32]; // irrelevant: the tail times out first

        // Declared-mode hold: the permits the law is about.
        let hold = svc
            .reserve_declared(nar.len() as u64, uuid::Uuid::from_u128(0x94), "W10-J")
            .await
            .expect("reservation grants");
        assert!(svc.nar_budget().available_permits() < full);

        // Black-hole the abort: FOR UPDATE the manifests row so
        // reap_one blocks until we release.
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM manifests WHERE store_path_hash = $1 FOR UPDATE")
            .bind(&info.store_path_hash)
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
        let tx_cell = std::cell::RefCell::new(Some(tx));

        let fin = svc.finalize_single(info.clone(), claim, nar.clone(), None, None, Some(hold));
        let probe = async {
            // The witness: the budget restores to FULL while the
            // abort still cannot finish (the row lock is ours).
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while svc.nar_budget().available_permits() != full {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "W10-J RED: permits pinned past the envelope while the \
                     abort is black-holed on row-lock contention"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Budget restored BEFORE the abort could complete; now
            // release the lock so the (detached, best-effort) abort
            // proceeds.
            drop(tx_cell.borrow_mut().take());
        };
        let (fin_res, ()) = tokio::join!(fin, probe);
        let err = fin_res.expect_err("the shrunk envelope aborts the persist tail");
        assert_eq!(
            err.code(),
            tonic::Code::ResourceExhausted,
            "the typed envelope abort, got {err:?}"
        );
        assert!(
            err.message().contains("permits released"),
            "the retry message is minted true (the permits ARE released), got: {}",
            err.message()
        );
        assert_eq!(
            svc.nar_budget().available_permits(),
            full,
            "every permit restored"
        );
    }
}

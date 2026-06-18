//! HMAC-SHA256 assignment tokens.
//!
//! The scheduler signs each `WorkAssignment.assignment_token` with a
//! shared secret. The store verifies the token on `PutPath` — a worker
//! can only upload outputs that match a valid assignment. This prevents
//! a compromised worker from uploading arbitrary paths (e.g., injecting
//! a backdoored libc).
//!
//! # Token format
//!
//! `base64url(serde_json(Claims)).base64url(hmac_sha256(key, claims_json))`
//!
//! JSON for the claims: self-delimiting, human-debuggable (base64-decode
//! a failing token → readable JSON), no delimiter-safety reasoning for
//! store paths. The `.` separator is URL-safe (base64url alphabet doesn't
//! use it).
//!
//! # Gateway bypass: service-identity tokens
//!
//! The gateway also calls `PutPath` (for `nix copy --to`). It doesn't
//! have an assignment token. The store grants bypass when the request
//! carries an `x-rio-service-token` header signed with a SEPARATE HMAC
//! key (`RIO_SERVICE_HMAC_KEY_PATH`) and `caller` is in the store's
//! allowlist. The gateway mints a fresh [`ServiceClaims`] per call
//! (60s expiry — sub-µs to sign, no caching).

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Claims types signed by [`HmacSigner`] / verified by [`HmacVerifier`].
///
/// The trait exists so `sign`/`verify` are generic over the claims
/// shape — [`AssignmentClaims`] (scheduler→builder) and
/// [`ServiceClaims`] (gateway→store) share the envelope and expiry
/// machinery without duplicating sign/verify. The expiry accessor is
/// the only behaviour [`HmacVerifier::verify`] needs beyond serde.
// r[impl sec.authz.service-token]
pub trait HmacClaims: Serialize + serde::de::DeserializeOwned {
    fn expiry_unix(&self) -> u64;
    /// The family clamp's write face (merged_bug_045):
    /// [`HmacKey::sign`] uses it to bound `expiry_unix − now ≤`
    /// [`MAX_HMAC_LIFETIME_SECS`] for EVERY claims type a family key
    /// signs — implementing this trait is what makes a type signable,
    /// so an unclamped family member is unrepresentable.
    fn set_expiry_unix(&mut self, expiry_unix: u64);
}

/// The FAMILY lifetime law (merged_bug_045, E3 widened): no token
/// signed by a family key may carry `expiry_unix − now` greater than
/// this. Seven days bounds a leaked token's replay window; the value
/// moved here from the scheduler's dispatch-local clamp so the law
/// has exactly one home — the signer — and every present and future
/// claims type sharing a key inherits it (the dispatch's ×2 grace
/// derivation stays a consumer of this constant: timeout ≤ lifetime ÷
/// grace keeps the doubled window inside the bound).
pub const MAX_HMAC_LIFETIME_SECS: u64 = 7 * 86400;

/// Claims embedded in an assignment token. The scheduler builds these
/// at dispatch time; the store verifies them on PutPath.
///
/// Named `AssignmentClaims` (not bare `Claims`) to disambiguate from
/// [`crate::jwt::TenantClaims`] — both appear together in PutPath
/// handlers, and `hmac::Claims` vs `jwt::Claims` was a recurring
/// source of confusion.
// r[impl common.hmac.claims+3]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssignmentClaims {
    /// Worker the assignment was for. Not checked on verify (the store
    /// has no transport-level worker identity, so this field is the
    /// only attribution). Audit-only — not authz.
    pub executor_id: String,
    /// Derivation hash. Ties the token to a specific build; a worker
    /// can't reuse one derivation's token for another.
    pub drv_hash: String,
    /// Store paths this token authorizes uploading. The store checks
    /// `ValidatedPathInfo.store_path ∈ expected_outputs` — uploading a
    /// path NOT in this list → PERMISSION_DENIED.
    ///
    /// For floating-CA derivations this is `[""]` at dispatch time (the
    /// output path is computed post-build from the NAR hash) — the store
    /// skips the membership check when [`is_ca`](Self::is_ca) is set.
    pub expected_outputs: Vec<String>,
    /// Floating-CA derivation: output paths are not known at dispatch
    /// time (computed post-build from the NAR hash). When set, the
    /// store skips the `store_path ∈ expected_outputs` check on
    /// PutPath and instead RECOMPUTES the CA store path server-side
    /// from the verified NAR hash (`r[sec.authz.ca-path-derived]`),
    /// rejecting on mismatch. The `'uploading'` placeholder is NOT
    /// claimed until that recompute passes, so a worker holding an
    /// `is_ca=true` token cannot upload to (or squat the placeholder
    /// for) a path that doesn't match the content it actually sent —
    /// same blast radius as IA (one content-determined path per NAR).
    pub is_ca: bool,
    /// Unix timestamp (seconds). Token invalid after this. Scheduler
    /// sets it to ~2× build_timeout; a worker legitimately uploading
    /// after build completion is well within that window. Prevents
    /// replay from a leaked token months later.
    pub expiry_unix: u64,
    /// Attributing tenant UUID (hyphenated). When set, the store
    /// writes it to `hw_perf_samples.submitting_tenant`
    /// (`r[sched.sla.threat.hw-median-of-medians]`), charges the
    /// declared-mode NAR budget to this tenant's bucket, and — bug_155
    /// — stamps the ingest REGISTRATION rows (`path_tenants`) for the
    /// builder lane's uploads: this signed attribution is how a
    /// worker-built floating-CA output acquires the store-recorded
    /// production evidence the scheduler's realisation/visibility
    /// consult demands (builders carry no per-session tenant JWT; the
    /// claims are the cohort's signed carrier). The store derives
    /// tenant from CLAIMS (signed) — never from the request body — so
    /// a compromised worker cannot fabricate tenant identities to
    /// defeat the median-of-medians defence, choose a foreign charge
    /// bucket, or mint evidence for a cohort the scheduler never
    /// attributed. `None` for orphaned/recovered nodes (no live owning
    /// build).
    ///
    /// **Wire-compat (bug_011).** This struct carries
    /// `#[serde(deny_unknown_fields)]`, so adding a field is a wire
    /// break in the *forward* skew direction (new token → old store).
    /// During a `helm upgrade` the scheduler (leader-elected
    /// singleton, ~30s) rolls before the store fleet (multi-replica,
    /// minutes); for that window every `PutPath` to a not-yet-rolled
    /// store would reject with `unknown field 'tenant'` →
    /// `permission_denied`. Both skew directions and how each serde
    /// attribute closes them:
    ///
    /// - **old token → new store:** a pre-tenant scheduler's token
    ///   lacks `"tenant"`. `#[serde(default)]` covers it — missing key
    ///   deserializes to `None`. (`deny_unknown_fields` only rejects
    ///   *extra* keys, not *missing* ones.)
    /// - **new token → old store:** a post-tenant scheduler's token
    ///   carrying `"tenant"` is rejected by a pre-tenant store.
    ///   `skip_serializing_if = "Option::is_none"` closes it whenever
    ///   `tenant=None`: the key is never emitted, so the wire body is
    ///   byte-identical to the pre-tenant shape. (`Option<T>` without
    ///   `skip_serializing_if` would emit `"tenant": null`, which is
    ///   *still* an unknown key.)
    ///
    /// Rolled out in two phases (Option H1). **Phase 1** (`fb096e50f`)
    /// added the field READ-only: store can parse `tenant`, scheduler
    /// signed `tenant: None`. **Phase 2** (this commit) flips
    /// `dispatch.rs` to set `tenant` from `attributed_tenant`; the
    /// rollout precondition (no `Some(_)` token reaches a pre-Phase-1
    /// store) was satisfied by a wipe deploy. The serde attributes stay
    /// — they remain the wire-compat mechanism for any future rolling
    /// upgrade of this struct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// `blake3(sorted(input_closure).join("\n"))` over the closure the
    /// scheduler computed at dispatch (`WorkAssignment.input_closure`).
    /// The store's `Begin` handler recomputes from the builder's echoed
    /// closure and compares: an attestation that the closure the
    /// refscan validates against is the one the builder was given.
    ///
    /// Hex so the JSON body stays readable. Empty = no attestation
    /// (scheduler couldn't compute the closure; refscan falls back).
    ///
    /// Wire-compat (bug_011): `default` makes a missing key parse as
    /// empty; `skip_serializing_if` keeps a default-valued token
    /// byte-identical to the pre-P0589 shape so an unrolled store's
    /// `deny_unknown_fields` still accepts it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub input_closure_digest: String,
}

impl AssignmentClaims {
    /// `blake3(closure.join("\n"))` as lowercase hex. `closure` must
    /// already be sorted (both producers emit sorted output).
    ///
    /// One definition shared by the scheduler (sign) and the store
    /// (verify); drift here is a silent attestation bypass.
    pub fn digest_input_closure(closure: &[String]) -> String {
        blake3::hash(closure.join("\n").as_bytes())
            .to_hex()
            .to_string()
    }
}

impl HmacClaims for AssignmentClaims {
    fn expiry_unix(&self) -> u64 {
        self.expiry_unix
    }
    fn set_expiry_unix(&mut self, expiry_unix: u64) {
        self.expiry_unix = expiry_unix;
    }
}

/// Claims for a service-identity token. Minted by trusted control-plane
/// callers (gateway) on each `PutPath`; verified by the store as the
/// HMAC-bypass condition. Transport-agnostic replacement for the mTLS
/// CN-allowlist check.
///
/// Signed with a SEPARATE key from [`AssignmentClaims`] so a leaked
/// assignment key (or a stolen assignment token) cannot satisfy
/// `verify::<ServiceClaims>` — wrong key → `InvalidSignature`, and the
/// serde shape diverges (`ServiceClaims` lacks `drv_hash`/
/// `expected_outputs`) as a second independent reject.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceClaims {
    /// Caller identity. Checked against the store's
    /// `service_bypass_callers` allowlist (default `["rio-gateway"]`).
    pub caller: String,
    /// Unix seconds. Gateway sets `now + 60`; store rejects past
    /// expiry. No nonce — replay-within-60s is a no-op given
    /// idempotent PutPath (`r[store.put.idempotent]`).
    pub expiry_unix: u64,
    /// Substitution-replacement Phase B (security obligation 1): the
    /// store replica's pod identity (DNS-1123 label), bound into the
    /// signed claims so the scheduler VERIFIES the `executor_instance`
    /// a materialization pull asserts. `None` for every
    /// non-materialization mint (gateway PutPath, controller, rio-cli,
    /// the scheduler's own probe tokens).
    ///
    /// **Threat model (merged_bug_115, scoped honestly).** The service
    /// HMAC key is FLEET-SHARED and symmetric: any key holder can mint
    /// a `ServiceClaims` with ANY `instance` string that verifies
    /// (`with_instance` takes an arbitrary label; the negative test
    /// `any_key_holder_mints_any_instance` pins this). What the binding
    /// delivers is therefore: (a) cross-SERVICE narrowing — a
    /// gateway-style instance-less token no longer authorizes
    /// materialization work; (b) composite-identity INJECTION
    /// detection — a request asserting an instance its own signed
    /// claims do not carry is rejected (the misconfiguration class,
    /// Phase A finding 4); (c) forgery resistance against NON-key-
    /// holders. It is explicitly NOT intra-fleet attestation: one
    /// compromised store replica can mint claims for a sibling's
    /// instance. Per-replica keys were considered and deliberately not
    /// built — no consumer derives destructive attribution from
    /// `instance`, and `credential_for` (the scheduler's family
    /// chokepoint) is the single place a future per-replica binding
    /// would land (the Phase-B obligation-1 record).
    ///
    /// **Wire-compat (the bug_011 pattern, same as
    /// [`AssignmentClaims::tenant`] — scoped per the precedent's own
    /// documentation):**
    ///
    /// - (i) old token → new verifier: missing key → `None` (serde
    ///   default; `deny_unknown_fields` rejects only *extra* keys).
    /// - (ii) new token minting `instance: None` → old verifier: the key
    ///   is never emitted (`skip_serializing_if`) → byte-identical wire
    ///   shape (gateway PutPath tokens and all in-flight pre-T-5.1
    ///   tokens are this leg).
    /// - (iii) new token with `instance: Some(_)` → pre-T-5.1 verifier:
    ///   REJECTED (`unknown field 'instance'` — same as the
    ///   AssignmentClaims forward-skew test's half 1, fail-closed).
    ///   Unreachable in any supported deployment: the only Some-minter
    ///   is the store's flag-gated materialization client; the flag is
    ///   a pod-template env var that rolls atomically with the image (a
    ///   pod is either {old binary + flag off} or {new binary + flag
    ///   on}, never a cross); production deployment is post-D′. Pinned
    ///   by `service_claims_instance_forward_skew`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

impl HmacClaims for ServiceClaims {
    fn expiry_unix(&self) -> u64 {
        self.expiry_unix
    }
    fn set_expiry_unix(&mut self, expiry_unix: u64) {
        self.expiry_unix = expiry_unix;
    }
}

/// Claims for an executor-identity token. Minted by the scheduler per
/// `SpawnIntent`, threaded through the controller as the
/// `RIO_EXECUTOR_TOKEN` pod env var, presented by builders on every
/// executor unary (`PullAssignment` / `ReportOutcome`) as
/// `x-rio-executor-token` metadata (or the in-body `executor_token`
/// field). The scheduler verifies it to bind each pull/report to the
/// intent the pod was spawned for — a compromised pod cannot claim
/// another pod's assignment or spoof its report, because it cannot
/// mint a token for a different `intent_id`.
///
/// Signed with the SAME assignment-HMAC key as [`AssignmentClaims`]:
/// both are scheduler-minted, scheduler-verified; the serde shape
/// (`intent_id` vs `drv_hash`/`expected_outputs`) provides the
/// cross-type isolation (the manual `Deserialize` below rejects
/// unknown fields exactly like the old `deny_unknown_fields`).
///
/// # Per-spawn uniqueness (merged_bug_079)
///
/// The wire form carries a fourth field the struct does not: a
/// `spawn` nonce (uuid-v4 hex), minted fresh by `Serialize` on EVERY
/// serialization and required-then-discarded by `Deserialize`. The
/// confirm fence (`executor_confirm_fences`, migration 097) keys
/// durable rows on `sha256(token bytes)`; without the nonce, the
/// target-anchored expiry (`now + deadline + eta + 300` with
/// `eta = t − elapsed`) made re-mints across controller ticks
/// byte-identical, so a replacement forecast pod inherited its
/// predecessor's fence identity and was screened to `Gone` without
/// ever building.
// r[impl sec.executor.identity-token+3]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorClaims {
    /// `SpawnIntent.intent_id` (= drv_hash) the token authorizes.
    /// Checked against `PullAssignmentRequest.intent_id` (and the
    /// report's bound exec row) on every unary.
    pub intent_id: String,
    /// `SpawnIntent.kind` (proto `ExecutorKind` wire i32: 0=Builder,
    /// 1=Fetcher) the token authorizes. Checked when the pod pulls —
    /// `kind` decides the FOD/non-FOD
    /// airgap routing, and the worker is NOT trusted (a compromised
    /// open-egress Fetcher heartbeating `kind=Builder` would
    /// otherwise receive non-FOD builds with secret inputs).
    ///
    /// Stored as the wire i32 (not the proto enum) because rio-auth
    /// is dependency-minimal and the proto enum doesn't derive serde;
    /// callers convert via `ExecutorKind::try_from(claims.kind)`.
    pub kind: i32,
    /// Unix seconds. Scheduler sets `now + deadline_secs + grace`;
    /// pod outliving its `activeDeadlineSeconds` is a bug, so the
    /// token outliving the pod is fine.
    pub expiry_unix: u64,
}

/// Entropy-in-Serialize, contained and disclosed (RULED S4-OQ1): this
/// impl emits the three claim fields PLUS a fresh `spawn` uuid-v4-hex
/// nonce per call, so serializing the same value twice produces
/// different bytes. Production serializes `ExecutorClaims` exactly
/// once, inside [`HmacKey::sign`] ("the claims JSON is what's signed")
/// — the nonce IS the per-spawn token identity the confirm fence keys
/// on. Do NOT reuse this serialization for caching, equality,
/// dedup keys, or any context expecting value-determinism;
/// `PartialEq` on the struct (three fields, nonce excluded) is the
/// value-equality surface.
impl Serialize for ExecutorClaims {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("ExecutorClaims", 4)?;
        st.serialize_field("intent_id", &self.intent_id)?;
        st.serialize_field("kind", &self.kind)?;
        st.serialize_field("expiry_unix", &self.expiry_unix)?;
        st.serialize_field("spawn", &uuid::Uuid::new_v4().as_simple().to_string())?;
        st.end()
    }
}

/// The decode half of the nonce law: all FOUR fields are REQUIRED —
/// `spawn` is checked-then-discarded (the struct keeps three fields)
/// and unknown fields are rejected, preserving the old
/// `deny_unknown_fields` cross-type isolation. There is no
/// optional-decode window for nonce-free (pre-fix) tokens: SIGNED Q6
/// — the rollout model is `--wipe` (full teardown + redeploy), so no
/// pre-fix token can ever reach this decoder; accepting the
/// three-field shape would only ever accept a stale-world credential.
impl<'de> Deserialize<'de> for ExecutorClaims {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ExecutorClaimsWire {
            intent_id: String,
            kind: i32,
            expiry_unix: u64,
            // Required (no default, no Option): a missing nonce is a
            // decode error. Discarded — the nonce's only job is making
            // the SIGNED BYTES unique per spawn.
            #[expect(dead_code, reason = "required-then-discarded spawn nonce")]
            spawn: String,
        }
        let wire = ExecutorClaimsWire::deserialize(deserializer)?;
        Ok(Self {
            intent_id: wire.intent_id,
            kind: wire.kind,
            expiry_unix: wire.expiry_unix,
        })
    }
}

impl HmacClaims for ExecutorClaims {
    fn expiry_unix(&self) -> u64 {
        self.expiry_unix
    }
    fn set_expiry_unix(&mut self, expiry_unix: u64) {
        self.expiry_unix = expiry_unix;
    }
}

/// tonic client interceptor that mints `x-rio-service-token` (HMAC-signed
/// [`ServiceClaims`]) on every outgoing request.
///
/// Shared by every control-plane caller of a service-token-gated RPC:
/// rio-controller (`caller="rio-controller"`) and rio-cli
/// (`caller="rio-cli"`). The verifier side checks `claims.caller` against
/// a per-RPC allowlist. `signer = None` → no-op (dev-mode pass-through;
/// the verifier is also `None` in that mode). See
/// `r[sec.authz.service-token]`.
#[derive(Clone)]
pub struct ServiceTokenInterceptor {
    signer: Option<std::sync::Arc<HmacSigner>>,
    caller: &'static str,
    /// Replica identity bound into every minted token
    /// ([`ServiceClaims::instance`]). `None` for every control-plane
    /// caller except the store's materialization client — see the
    /// field's wire-compat documentation.
    instance: Option<String>,
}

impl ServiceTokenInterceptor {
    pub fn new(signer: Option<std::sync::Arc<HmacSigner>>, caller: &'static str) -> Self {
        Self {
            signer,
            caller,
            instance: None,
        }
    }

    /// An interceptor whose minted tokens are instance-bound
    /// ([`ServiceClaims::instance`] = `Some(instance)`): the store's
    /// materialization client, which must prove WHICH replica is
    /// claiming (substitution-replacement Phase B obligation 1). Every
    /// other caller uses [`Self::new`] (instance-less, wire-identical
    /// to pre-Phase-B tokens).
    pub fn with_instance(
        signer: Option<std::sync::Arc<HmacSigner>>,
        caller: &'static str,
        instance: String,
    ) -> Self {
        Self {
            signer,
            caller,
            instance: Some(instance),
        }
    }
}

impl tonic::service::Interceptor for ServiceTokenInterceptor {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(signer) = &self.signer {
            let claims = ServiceClaims {
                caller: self.caller.to_string(),
                // 60s: sub-µs to sign, no caching. Same window as the
                // gateway's PutPath token.
                expiry_unix: crate::now_unix()
                    .map_err(|e| tonic::Status::internal(e.to_string()))?
                    + 60,
                instance: self.instance.clone(),
            };
            // sign() output is base64url + '.' — always ASCII.
            if let Ok(v) = signer.sign(&claims).parse() {
                req.metadata_mut()
                    .insert(rio_common::grpc::SERVICE_TOKEN_HEADER, v);
            }
        }
        Ok(req)
    }
}

/// Gate for control-plane-only RPCs. Verifies `x-rio-service-token`
/// (HMAC-signed [`ServiceClaims`]) and checks `claims.caller ∈ allowed`.
/// `verifier == None` → dev-mode pass-through (parity with the
/// assignment-token verifier; returns synthetic `caller="dev-mode"`).
///
/// Shared by scheduler `AdminService` and store `StoreAdminService` —
/// both share a port with builder-reachable services, so without this
/// gate a compromised builder could call `AddUpstream` cross-tenant
/// (cache poisoning) or `AppendInterruptSample` (poison λ\[h\]). See
/// `r[sec.authz.service-token]`.
// r[impl sec.authz.service-token]
pub fn ensure_service_caller(
    md: &tonic::metadata::MetadataMap,
    verifier: Option<&HmacVerifier>,
    allowed: &[&str],
) -> Result<ServiceClaims, tonic::Status> {
    let Some(verifier) = verifier else {
        return Ok(ServiceClaims {
            caller: "dev-mode".to_string(),
            expiry_unix: u64::MAX,
            instance: None,
        });
    };
    let tok = md
        .get(rio_common::grpc::SERVICE_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            tonic::Status::permission_denied(format!(
                "{} header required for this RPC",
                rio_common::grpc::SERVICE_TOKEN_HEADER
            ))
        })?;
    let claims = verifier
        .verify::<ServiceClaims>(tok)
        .map_err(|e| tonic::Status::permission_denied(format!("service token: {e}")))?;
    if !allowed.contains(&claims.caller.as_str()) {
        return Err(tonic::Status::permission_denied(format!(
            "service-token caller {:?} not in allowlist {allowed:?}",
            claims.caller
        )));
    }
    Ok(claims)
}

/// Shared HMAC key. The scheduler signs, the store verifies — same key
/// file, same field, and a process is one role or the other (never
/// both), so a single struct with both methods is sufficient. The
/// [`HmacSigner`]/[`HmacVerifier`] aliases keep call sites readable.
///
/// Deliberately not `Clone`: callers needing to share a verifier across
/// service impls wrap one in `Arc` so there's exactly one copy of the
/// key bytes in memory.
pub struct HmacKey {
    key: Vec<u8>,
}

/// Scheduler-side alias. See [`HmacKey`].
pub type HmacSigner = HmacKey;
/// Store-side alias. See [`HmacKey`].
pub type HmacVerifier = HmacKey;

#[derive(Debug, thiserror::Error)]
pub enum HmacError {
    #[error("key file I/O ({path}): {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("key file is empty")]
    EmptyKey,
    #[error("token format invalid: expected 'claims.hmac', got {0} '.'-separated parts")]
    Format(usize),
    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("JSON decode failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HMAC verification failed (tampered or wrong key)")]
    InvalidSignature,
    #[error("token expired (expiry={expiry_unix}, now={now_unix})")]
    Expired { expiry_unix: u64, now_unix: u64 },
    #[error(transparent)]
    Clock(#[from] crate::ClockBeforeEpoch),
}

/// Load a key from a file. Returns `Ok(None)` if `path` is `None` —
/// HMAC disabled, not an error. Same pattern as `Signer::load` in
/// rio-store.
///
/// The key file contains raw bytes (no encoding). Operators generate
/// it with e.g. `openssl rand -out /etc/rio/hmac-key 32`. We don't
/// impose a length minimum (hmac handles short keys by padding) but
/// 32 bytes (256 bits) matches SHA-256's output and is the standard
/// recommendation.
fn load_key(path: Option<&std::path::Path>) -> Result<Option<Vec<u8>>, HmacError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let key = std::fs::read(path).map_err(|source| HmacError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Trim trailing newline: `echo -n` is the correct way to write
    // a key file, but `echo` (no -n) is an easy mistake. A trailing
    // \n would be part of the key — not WRONG per se (hmac doesn't
    // care), but it means the scheduler's key file and the store's
    // key file must BOTH have or not-have the newline. Trimming
    // makes it forgiving.
    //
    // CRLF first, then LF: a Windows-edited key file has \r\n;
    // stripping only \n would leave \r in the key → scheduler and
    // store key mismatch → all uploads rejected with opaque
    // InvalidSignature.
    //
    // Every consumer of the key file MUST mirror this trim. The
    // dashboard njs (nix/docker.nix dashboardServiceTokenJs) does so
    // at the byte level; nix/tests/lib/hmac-keys.nix appends a LF to
    // the fixture so any consumer that doesn't fails CI.
    let key = key
        .strip_suffix(b"\r\n")
        .or_else(|| key.strip_suffix(b"\n"))
        .map(|s| s.to_vec())
        .unwrap_or(key);
    if key.is_empty() {
        return Err(HmacError::EmptyKey);
    }
    Ok(Some(key))
}

impl HmacKey {
    /// Load from a key file. `None` path → `None` key (HMAC disabled).
    pub fn load(path: Option<&std::path::Path>) -> Result<Option<Self>, HmacError> {
        Ok(load_key(path)?.map(|key| Self { key }))
    }

    /// Construct from raw key bytes (for tests).
    pub fn from_key(key: Vec<u8>) -> Self {
        Self { key }
    }

    /// Sign claims into a token string.
    ///
    /// Format: `base64url(json(claims)).base64url(hmac(key, json(claims)))`.
    /// The claims JSON is what's signed — the base64 is just transport
    /// encoding.
    ///
    /// THE FAMILY LIFETIME LAW lives here (merged_bug_045, R24): the
    /// expiry is clamped to `now + `[`MAX_HMAC_LIFETIME_SECS`] before
    /// signing, for EVERY claims type — this is the only signing body,
    /// so an over-long mint is unrepresentable for every present and
    /// future family member; per-mint clamps (the dispatch-local form
    /// this replaced) are derivation conveniences, never the law. A
    /// pre-epoch clock degrades fail-closed: `now_unix()` errors fold
    /// to `now = 0`, the cap lands in 1970, and the clamped token is
    /// already expired at verify (the same degradation the dispatch
    /// mint's `unwrap_or(0)` has always had).
    // r[impl common.hmac.claims+3]
    pub fn sign<C: HmacClaims + Clone>(&self, claims: &C) -> String {
        let now = crate::now_unix().unwrap_or(0);
        let cap = now.saturating_add(MAX_HMAC_LIFETIME_SECS);
        let clamped_holder;
        let claims = if claims.expiry_unix() > cap {
            let mut c = claims.clone();
            c.set_expiry_unix(cap);
            clamped_holder = c;
            &clamped_holder
        } else {
            claims
        };
        let claims_json = serde_json::to_vec(claims)
            .expect("HmacClaims serialization can't fail (no maps with non-string keys)");
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .expect("HMAC::new_from_slice accepts any key length");
        mac.update(&claims_json);
        let tag = mac.finalize().into_bytes();

        // base64url (URL_SAFE_NO_PAD): no '/' (path separator) or
        // '+' (would need URL encoding), no '=' padding (not needed,
        // length is known from the '.' split). The '.' separator is
        // not in base64url's alphabet — unambiguous split.
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}", b64.encode(&claims_json), b64.encode(tag))
    }

    /// Verify a token and return its claims.
    ///
    /// Checks signature THEN expiry. Signature first so we don't
    /// leak timing information about expiry parsing (not a real
    /// concern here — the JSON decode is after sig check anyway —
    /// but good discipline). Constant-time compare via
    /// `Mac::verify_slice` (never `==` on raw tag bytes).
    pub fn verify<C: HmacClaims>(&self, token: &str) -> Result<C, HmacError> {
        // Split on the single '.'. More parts → someone injected a
        // '.' into claims (impossible with our encoding) or
        // tampered.
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 2 {
            return Err(HmacError::Format(parts.len()));
        }

        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let claims_json = b64.decode(parts[0])?;
        let tag = b64.decode(parts[1])?;

        // Signature check FIRST. Constant-time via verify_slice —
        // the hmac crate's MacError from this is a generic "verify
        // failed" (no detail), which is what we want (don't tell
        // attackers WHICH byte differed).
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .expect("HMAC::new_from_slice accepts any key length");
        mac.update(&claims_json);
        mac.verify_slice(&tag)
            .map_err(|_| HmacError::InvalidSignature)?;

        // Now safe to decode — the JSON came from us (signature
        // verified), so malicious-input concerns don't apply.
        // (serde_json is safe on untrusted input anyway, but the
        // principle holds.)
        let claims: C = serde_json::from_slice(&claims_json)?;

        // Expiry check. SystemTime::now → Unix secs. Clock skew
        // concern: scheduler + store + worker clocks may drift.
        // The expiry is ~2× build_timeout (hours); NTP keeps skew
        // under seconds. Non-issue in practice. If clocks are
        // wildly wrong, tokens fail with a clear "Expired" error
        // — debuggable. Pre-epoch clock → `Clock` error (NOT the
        // old `unwrap_or(0)`, which silently accepted every token).
        let now_unix = crate::now_unix()?;
        if now_unix > claims.expiry_unix() {
            return Err(HmacError::Expired {
                expiry_unix: claims.expiry_unix(),
                now_unix,
            });
        }

        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &[u8] = b"test-key-at-least-32-bytes-long!";

    fn test_claims(expiry_offset_secs: i64) -> AssignmentClaims {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        AssignmentClaims {
            executor_id: "test-builder".into(),
            drv_hash: "abc123".into(),
            expected_outputs: vec![
                "/nix/store/aaa-hello".into(),
                "/nix/store/bbb-hello-dev".into(),
            ],
            is_ca: false,
            expiry_unix: (now as i64 + expiry_offset_secs).max(0) as u64,
            tenant: None,
            input_closure_digest: String::new(),
        }
    }

    /// `digest_input_closure` is order-sensitive over its newline-join
    /// — the spec says "sorted closure" because the scheduler and the
    /// store must produce the same bytes from the same set.
    // r[verify common.hmac.claims+3]
    #[test]
    fn closure_digest_deterministic_and_order_sensitive() {
        let a = vec![
            "/nix/store/aaa-foo".to_string(),
            "/nix/store/bbb-bar".to_string(),
        ];
        let b = vec![
            "/nix/store/bbb-bar".to_string(),
            "/nix/store/aaa-foo".to_string(),
        ];
        assert_eq!(
            AssignmentClaims::digest_input_closure(&a),
            AssignmentClaims::digest_input_closure(&a),
        );
        assert_ne!(
            AssignmentClaims::digest_input_closure(&a),
            AssignmentClaims::digest_input_closure(&b),
        );
        // Empty closure → digest of empty input, NOT "".
        assert_eq!(
            AssignmentClaims::digest_input_closure(&[]),
            blake3::hash(b"").to_hex().to_string(),
        );
        // Golden vector: pins separator placement (round-trip can't).
        assert_eq!(
            AssignmentClaims::digest_input_closure(&a),
            blake3::hash(b"/nix/store/aaa-foo\n/nix/store/bbb-bar")
                .to_hex()
                .to_string(),
        );
        assert_eq!(
            AssignmentClaims::digest_input_closure(&["x".to_string()]),
            blake3::hash(b"x").to_hex().to_string(),
        );
    }

    #[test]
    fn sign_verify_roundtrip() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let claims = test_claims(3600); // 1h future
        let token = signer.sign(&claims);

        let verified = verifier
            .verify::<AssignmentClaims>(&token)
            .expect("valid token should verify");
        assert_eq!(verified, claims);
    }

    #[test]
    fn tampered_signature_rejected() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let token = signer.sign(&test_claims(3600));
        // Decode the signature, flip one byte, re-encode. Doing
        // this at the raw-bytes level (not base64-char-flip)
        // guarantees the result is valid base64 that DECODES to
        // a different byte sequence — so the error is definitely
        // InvalidSignature, not Base64. A char-flip in base64
        // might produce an invalid byte that fails at decode
        // (last-char carries only 2 bits; flipping it wrong
        // gives out-of-range).
        let parts: Vec<&str> = token.split('.').collect();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut sig = b64.decode(parts[1]).unwrap();
        sig[0] ^= 0xFF; // flip all bits in first byte
        let tampered = format!("{}.{}", parts[0], b64.encode(&sig));

        assert!(matches!(
            verifier.verify::<AssignmentClaims>(&tampered),
            Err(HmacError::InvalidSignature)
        ));
    }

    #[test]
    fn tampered_claims_rejected() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let token = signer.sign(&test_claims(3600));
        // Decode claims, modify, re-encode with ORIGINAL signature.
        let parts: Vec<&str> = token.split('.').collect();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut claims: AssignmentClaims =
            serde_json::from_slice(&b64.decode(parts[0]).unwrap()).unwrap();
        claims
            .expected_outputs
            .push("/nix/store/evil-backdoor".into());
        let tampered_claims = b64.encode(serde_json::to_vec(&claims).unwrap());
        let tampered = format!("{tampered_claims}.{}", parts[1]);

        // Signature was over ORIGINAL claims; tampered claims →
        // signature mismatch.
        assert!(matches!(
            verifier.verify::<AssignmentClaims>(&tampered),
            Err(HmacError::InvalidSignature)
        ));
    }

    #[test]
    fn expired_token_rejected() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let claims = test_claims(-60); // expired 1 minute ago
        let token = signer.sign(&claims);

        let result = verifier.verify::<AssignmentClaims>(&token);
        assert!(
            matches!(result, Err(HmacError::Expired { .. })),
            "expected Expired, got {result:?}"
        );
    }

    #[test]
    fn wrong_key_rejected() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(b"completely-different-key-32bytes".to_vec());

        let token = signer.sign(&test_claims(3600));
        assert!(matches!(
            verifier.verify::<AssignmentClaims>(&token),
            Err(HmacError::InvalidSignature)
        ));
    }

    #[test]
    fn malformed_token_rejected() {
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        // No '.'
        assert!(matches!(
            verifier.verify::<AssignmentClaims>("nodot"),
            Err(HmacError::Format(1))
        ));
        // Too many '.'
        assert!(matches!(
            verifier.verify::<AssignmentClaims>("a.b.c"),
            Err(HmacError::Format(3))
        ));
        // Bad base64
        assert!(matches!(
            verifier.verify::<AssignmentClaims>("!!!.!!!"),
            Err(HmacError::Base64(_))
        ));
    }

    #[test]
    fn service_token_interceptor_mints_header_and_noop_when_unsigned() {
        use tonic::service::Interceptor;
        // signer present → header attached, verifies, caller matches.
        let signer = std::sync::Arc::new(HmacSigner::from_key(TEST_KEY.to_vec()));
        let mut int = ServiceTokenInterceptor::new(Some(signer), "rio-cli");
        let req = int.call(tonic::Request::new(())).unwrap();
        let tok = req
            .metadata()
            .get(rio_common::grpc::SERVICE_TOKEN_HEADER)
            .expect("interceptor should attach header")
            .to_str()
            .unwrap();
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());
        let claims = verifier.verify::<ServiceClaims>(tok).expect("verifies");
        assert_eq!(claims.caller, "rio-cli");

        // signer=None → no header (dev-mode pass-through).
        let mut noop = ServiceTokenInterceptor::new(None, "rio-cli");
        let req = noop.call(tonic::Request::new(())).unwrap();
        assert!(
            req.metadata()
                .get(rio_common::grpc::SERVICE_TOKEN_HEADER)
                .is_none()
        );
    }

    // r[verify sec.authz.service-token]
    // r[verify store.admin.service-gate]
    #[test]
    fn ensure_service_caller_gates() {
        use tonic::service::Interceptor;
        let key = HmacVerifier::from_key(TEST_KEY.to_vec());
        let signer = std::sync::Arc::new(HmacSigner::from_key(TEST_KEY.to_vec()));

        // None verifier → dev-mode pass-through (synthetic caller).
        let md = tonic::metadata::MetadataMap::new();
        let c = ensure_service_caller(&md, None, &["rio-cli"]).expect("dev-mode passes");
        assert_eq!(c.caller, "dev-mode");

        // Verifier present, no header → PermissionDenied.
        let err = ensure_service_caller(&md, Some(&key), &["rio-cli"]).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(err.message().contains("required"), "msg: {}", err.message());

        // Valid token, caller in allowlist → Ok.
        let mut int = ServiceTokenInterceptor::new(Some(signer), "rio-cli");
        let req = int.call(tonic::Request::new(())).unwrap();
        let c = ensure_service_caller(req.metadata(), Some(&key), &["rio-cli", "rio-controller"])
            .expect("allowlisted caller passes");
        assert_eq!(c.caller, "rio-cli");

        // Valid token, caller NOT in allowlist → PermissionDenied.
        let err =
            ensure_service_caller(req.metadata(), Some(&key), &["rio-controller"]).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert!(
            err.message().contains("not in allowlist"),
            "msg: {}",
            err.message()
        );

        // Wrong-key token (e.g. assignment key leaked) → PermissionDenied.
        let bad_signer = std::sync::Arc::new(HmacSigner::from_key(b"wrong-key-32-bytes!!!".into()));
        let mut bad = ServiceTokenInterceptor::new(Some(bad_signer), "rio-cli");
        let req = bad.call(tonic::Request::new(())).unwrap();
        let err = ensure_service_caller(req.metadata(), Some(&key), &["rio-cli"]).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[test]
    fn service_claims_roundtrip_and_shape_isolation() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let svc = ServiceClaims {
            caller: "rio-gateway".into(),
            expiry_unix: now + 60,
            instance: None,
        };
        let svc_token = signer.sign(&svc);
        assert_eq!(
            verifier
                .verify::<ServiceClaims>(&svc_token)
                .expect("service token verifies"),
            svc
        );
        // Shape isolation, both directions: a token signed as one
        // claims type cannot verify as the other, even with the
        // correct key — serde rejects the mismatched JSON shape after
        // the signature passes. This is the second independent
        // defence (the primary one is separate keys in production).
        assert!(matches!(
            verifier.verify::<AssignmentClaims>(&svc_token),
            Err(HmacError::Json(_))
        ));
        let asn_token = signer.sign(&test_claims(3600));
        assert!(matches!(
            verifier.verify::<ServiceClaims>(&asn_token),
            Err(HmacError::Json(_))
        ));
    }

    /// Back-compat: a token minted WITHOUT the post-P0589 fields
    /// (`tenant`, `input_closure_digest`) by an older scheduler must
    /// still verify under the new struct — `#[serde(default)]` on
    /// those fields is load-bearing for in-flight tokens at deploy
    /// time. `deny_unknown_fields` only rejects EXTRA fields, not
    /// MISSING ones.
    // r[verify common.hmac.claims+3]
    #[test]
    fn assignment_claims_tenant_backcompat() {
        let key = HmacKey::from_key(TEST_KEY.to_vec());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Hand-roll the pre-tenant claims JSON (no `tenant` key).
        let old_json = serde_json::json!({
            "executor_id": "w",
            "drv_hash": "abc",
            "expected_outputs": ["/nix/store/aaa-x"],
            "is_ca": false,
            "expiry_unix": now + 3600,
        });
        let old_bytes = serde_json::to_vec(&old_json).unwrap();
        let mut mac = HmacSha256::new_from_slice(TEST_KEY).unwrap();
        mac.update(&old_bytes);
        let tag = mac.finalize().into_bytes();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let token = format!("{}.{}", b64.encode(&old_bytes), b64.encode(tag));

        let claims = key
            .verify::<AssignmentClaims>(&token)
            .expect("pre-tenant token still verifies (serde default)");
        assert_eq!(claims.tenant, None);
        assert!(claims.input_closure_digest.is_empty());
        assert_eq!(claims.executor_id, "w");

        // And a token WITH tenant round-trips it.
        let mut with_tenant = test_claims(3600);
        with_tenant.tenant = Some("4f8a3c0e-0000-4000-8000-000000000001".into());
        let tok = key.sign(&with_tenant);
        assert_eq!(
            key.verify::<AssignmentClaims>(&tok).unwrap().tenant,
            with_tenant.tenant
        );
    }

    /// Forward-skew direction (bug_011): a token minted by a
    /// post-tenant scheduler, presented to a PRE-tenant store during a
    /// rolling upgrade. The HMAC tag check passes (tag is over raw
    /// bytes); the parse step is where the skew breaks. With
    /// `deny_unknown_fields` on the OLD struct and `"tenant"` in the
    /// NEW struct's wire body, the old store rejects with
    /// `unknown field 'tenant'` → `permission_denied` → every
    /// `PutPath` to a not-yet-rolled store pod fails for the rollout
    /// window.
    ///
    /// **Wire-shape pin (Option H1, bug_011).** This test pins both
    /// halves of the serde shape — independent of what `dispatch.rs`
    /// actually writes (that's `test_hmac_assignment_carries_tenant`
    /// in `rio-scheduler`):
    ///
    /// 1. `tenant: Some(_)` → the wire body carries `"tenant"` → a
    ///    pre-tenant store CANNOT parse it. This was the hazard that
    ///    gated Phase 2 (`dispatch.rs` setting `tenant` from
    ///    `attributed_tenant`) on the store fleet first carrying the
    ///    Phase 1 reader (`fb096e50f`); satisfied via a wipe deploy.
    /// 2. `tenant: None` → `skip_serializing_if = "Option::is_none"`
    ///    omits the key entirely → the wire body is byte-identical to
    ///    the pre-tenant shape → a pre-tenant store parses it fine.
    ///    This is what made Phase 1 (scheduler signed `None`) safe to
    ///    roll in any order against pre-tenant stores, and what keeps
    ///    `None` (orphaned/recovered nodes) wire-compatible today.
    ///
    /// If this test starts failing on the `Some(_)` half — i.e. the
    /// pre-tenant struct started ACCEPTING `"tenant"` — somebody
    /// dropped `deny_unknown_fields` from `AssignmentClaims`, which is
    /// a different rollout strategy (Option H3 in the bug_011 plan)
    /// and this test should be rewritten to pin that property instead.
    #[test]
    fn assignment_claims_tenant_forward_skew() {
        // Pre-tenant store struct (hand-rolled snapshot of
        // `AssignmentClaims` before `tenant` was added). Deliberately
        // local so it never drifts to track the real struct.
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)] // fields read by serde, not by the test body
        struct OldAssignmentClaims {
            executor_id: String,
            drv_hash: String,
            expected_outputs: Vec<String>,
            is_ca: bool,
            expiry_unix: u64,
        }

        let key = HmacKey::from_key(TEST_KEY.to_vec());
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let claims_body = |c: &AssignmentClaims| -> Vec<u8> {
            let token = key.sign(c);
            let (claims_b64, _) = token.split_once('.').unwrap();
            b64.decode(claims_b64).unwrap()
        };

        // Half 1 — `tenant: Some(_)` is a wire break against a
        // pre-tenant store. This is why Phase 2 had to wait for the
        // store fleet to carry the Phase 1 reader (fb096e50f).
        let mut with_tenant = test_claims(3600);
        with_tenant.tenant = Some("4f8a3c0e-0000-4000-8000-000000000001".into());
        let body = claims_body(&with_tenant);
        assert!(
            std::str::from_utf8(&body).unwrap().contains("\"tenant\""),
            "Some(_) must emit the tenant key (precondition for the rejection assertion)"
        );
        let err = serde_json::from_slice::<OldAssignmentClaims>(&body)
            .expect_err("pre-tenant store must reject a tenant-bearing body");
        assert!(
            err.to_string().contains("unknown field `tenant`"),
            "expected `unknown field 'tenant'`, got: {err}"
        );

        // Half 2 — `tenant: None` is wire-compatible with a
        // pre-tenant store: `skip_serializing_if` omits the key, so
        // the body has the exact pre-tenant shape. This is what made
        // Phase 1 safe to roll in any order, and keeps the
        // orphaned-node `None` case wire-compatible today.
        let without_tenant = test_claims(3600); // tenant: None
        let body = claims_body(&without_tenant);
        assert!(
            !std::str::from_utf8(&body).unwrap().contains("tenant"),
            "None must NOT emit the tenant key (skip_serializing_if)"
        );
        let parsed = serde_json::from_slice::<OldAssignmentClaims>(&body)
            .expect("pre-tenant store must accept a tenant-less body");
        assert_eq!(parsed.executor_id, without_tenant.executor_id);
        assert_eq!(parsed.expiry_unix, without_tenant.expiry_unix);
    }

    /// Back-compat (T-5.1, the instance-binding analog of
    /// `assignment_claims_tenant_backcompat`): a service token minted
    /// WITHOUT the `instance` field (every pre-T-5.1 minter, and every
    /// post-T-5.1 non-store minter) must still verify under the new
    /// struct — `#[serde(default)]` on `instance` is load-bearing for
    /// gateway PutPath tokens and all in-flight tokens at deploy time.
    #[test]
    fn service_claims_without_instance_round_trips() {
        let key = HmacKey::from_key(TEST_KEY.to_vec());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Hand-roll the pre-instance claims JSON (no `instance` key).
        let old_json = serde_json::json!({
            "caller": "rio-gateway",
            "expiry_unix": now + 60,
        });
        let old_bytes = serde_json::to_vec(&old_json).unwrap();
        let mut mac = HmacSha256::new_from_slice(TEST_KEY).unwrap();
        mac.update(&old_bytes);
        let tag = mac.finalize().into_bytes();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let token = format!("{}.{}", b64.encode(&old_bytes), b64.encode(tag));

        let claims = key
            .verify::<ServiceClaims>(&token)
            .expect("pre-instance service token still verifies (serde default)");
        assert_eq!(claims.instance, None);
        assert_eq!(claims.caller, "rio-gateway");

        // The new struct minting `instance: None` produces the
        // byte-identical pre-instance wire shape (skip_serializing_if).
        let minted = ServiceClaims {
            caller: "rio-gateway".into(),
            expiry_unix: now + 60,
            instance: None,
        };
        let body = serde_json::to_vec(&minted).unwrap();
        assert!(
            !String::from_utf8(body).unwrap().contains("instance"),
            "instance: None must never emit the key (wire-identity for every \
             non-materialization mint)"
        );
    }

    /// T-5.1: an instance-bound service token round-trips the replica
    /// identity through sign → verify, and the verifying side reads it
    /// from the CLAIMS (the signed body), never from anything the
    /// caller could tamper with.
    #[test]
    fn service_claims_instance_signed_and_verified() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = ServiceClaims {
            caller: "rio-store".into(),
            expiry_unix: now + 60,
            instance: Some("rio-store-7d4b8f9c6-x2vpl".into()),
        };
        let token = signer.sign(&claims);
        let verified = verifier
            .verify::<ServiceClaims>(&token)
            .expect("instance-bound token verifies");
        assert_eq!(verified, claims);
        assert_eq!(
            verified.instance.as_deref(),
            Some("rio-store-7d4b8f9c6-x2vpl"),
            "the replica identity survives the round trip inside the signed body"
        );

        // Tampering with the instance inside the claims body breaks the
        // signature (the binding is what makes the claim an authority).
        let parts: Vec<&str> = token.split('.').collect();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut tampered_claims: ServiceClaims =
            serde_json::from_slice(&b64.decode(parts[0]).unwrap()).unwrap();
        tampered_claims.instance = Some("evil-replica".into());
        let tampered_body = b64.encode(serde_json::to_vec(&tampered_claims).unwrap());
        let tampered = format!("{tampered_body}.{}", parts[1]);
        assert!(matches!(
            verifier.verify::<ServiceClaims>(&tampered),
            Err(HmacError::InvalidSignature)
        ));

        // The interceptor's with_instance constructor mints exactly this
        // shape; ensure_service_caller still accepts it (the instance
        // claim narrows nothing at the generic service-caller gate — the
        // narrowing is the materialization verifier's job).
        use tonic::service::Interceptor;
        let arc_signer = std::sync::Arc::new(HmacSigner::from_key(TEST_KEY.to_vec()));
        let mut int = ServiceTokenInterceptor::with_instance(
            Some(arc_signer),
            "rio-store",
            "store-replica-0".into(),
        );
        let req = int.call(tonic::Request::new(())).unwrap();
        let c = ensure_service_caller(req.metadata(), Some(&verifier), &["rio-store"])
            .expect("instance-bound tokens pass the generic service-caller gate");
        assert_eq!(c.instance.as_deref(), Some("store-replica-0"));
    }

    /// Forward-skew direction (T-5.1, adjudication PDB-8's required pin,
    /// mirroring `assignment_claims_tenant_forward_skew`): a service
    /// token minted by a post-T-5.1 store (instance-bound), presented to
    /// a PRE-T-5.1 verifier. With `deny_unknown_fields` on the OLD
    /// struct and `"instance"` in the NEW struct's wire body, the old
    /// verifier rejects with `unknown field 'instance'` — **fail-closed,
    /// never fail-open**: a pre-Phase-B scheduler can never accept an
    /// instance-bound token while silently skipping the instance check
    /// (there is no enforcement gap in any mixed-version window; the
    /// failure mode is claim retries, not unenforced binding).
    ///
    /// This leg is unreachable in any supported deployment (the only
    /// Some-minter is the store's flag-gated materialization client; the
    /// flag is a pod-template env var that rolls atomically with the
    /// image; production deployment is post-D′) — see the
    /// `ServiceClaims::instance` doc-comment, leg (iii).
    ///
    /// If this test starts failing on the `Some(_)` half — i.e. the
    /// pre-instance struct started ACCEPTING `"instance"` — somebody
    /// dropped `deny_unknown_fields` from `ServiceClaims`, which
    /// destroys the cross-type isolation between claims families and is
    /// a different rollout strategy entirely (stop condition 9).
    #[test]
    fn service_claims_instance_forward_skew() {
        // Pre-T-5.1 verifier struct (hand-rolled snapshot of
        // `ServiceClaims` before `instance` was added). Deliberately
        // local so it never drifts to track the real struct.
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)] // fields read by serde, not by the test body
        struct OldServiceClaims {
            caller: String,
            expiry_unix: u64,
        }

        let key = HmacKey::from_key(TEST_KEY.to_vec());
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims_body = |c: &ServiceClaims| -> Vec<u8> {
            let token = key.sign(c);
            let (claims_b64, _) = token.split_once('.').unwrap();
            b64.decode(claims_b64).unwrap()
        };

        // Half 1 — `instance: Some(_)` is a wire break against a
        // pre-T-5.1 verifier: REJECTED, fail-closed. The old verifier
        // can never authenticate-but-not-enforce an instance-bound
        // token.
        let bound = ServiceClaims {
            caller: "rio-store".into(),
            expiry_unix: now + 60,
            instance: Some("store-replica-0".into()),
        };
        let body = claims_body(&bound);
        assert!(
            std::str::from_utf8(&body).unwrap().contains("\"instance\""),
            "Some(_) must emit the instance key (precondition for the rejection assertion)"
        );
        let err = serde_json::from_slice::<OldServiceClaims>(&body)
            .expect_err("a pre-T-5.1 verifier must reject an instance-bearing body");
        assert!(
            err.to_string().contains("unknown field `instance`"),
            "expected `unknown field 'instance'`, got: {err}"
        );

        // Half 2 — `instance: None` is wire-compatible with a pre-T-5.1
        // verifier: skip_serializing_if omits the key, so the body has
        // the exact pre-instance shape. This is what keeps every
        // non-store mint (gateway PutPath, controller, rio-cli) and the
        // flag-off store deployable in any order against pre-T-5.1
        // verifiers.
        let unbound = ServiceClaims {
            caller: "rio-gateway".into(),
            expiry_unix: now + 60,
            instance: None,
        };
        let body = claims_body(&unbound);
        assert!(
            !std::str::from_utf8(&body).unwrap().contains("instance"),
            "None must NOT emit the instance key (skip_serializing_if)"
        );
        let parsed = serde_json::from_slice::<OldServiceClaims>(&body)
            .expect("a pre-T-5.1 verifier must accept an instance-less body");
        assert_eq!(parsed.caller, unbound.caller);
        assert_eq!(parsed.expiry_unix, unbound.expiry_unix);
    }

    #[test]
    fn load_none_returns_none() {
        assert!(HmacSigner::load(None).unwrap().is_none());
        assert!(HmacVerifier::load(None).unwrap().is_none());
    }

    #[test]
    fn load_empty_file_errors() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Write nothing — empty file.
        assert!(matches!(
            HmacSigner::load(Some(tmp.path())),
            Err(HmacError::EmptyKey)
        ));
    }

    #[test]
    fn load_trims_trailing_newline() {
        // `echo "secret" > keyfile` produces trailing \n. Both
        // signer and verifier should get the SAME key regardless.
        let tmp1 = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp1.path(), b"secret-key-32-bytes-long-here!!\n").unwrap();
        let tmp2 = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp2.path(), b"secret-key-32-bytes-long-here!!").unwrap();

        let signer = HmacSigner::load(Some(tmp1.path())).unwrap().unwrap();
        let verifier = HmacVerifier::load(Some(tmp2.path())).unwrap().unwrap();

        // Sign with newline-file, verify with no-newline-file.
        // Should match (both trimmed to same key).
        let claims = test_claims(3600);
        let token = signer.sign(&claims);
        let verified = verifier
            .verify::<AssignmentClaims>(&token)
            .expect("trailing newline trimmed → same key → verify succeeds");
        assert_eq!(verified, claims);
    }

    #[test]
    fn load_trims_trailing_crlf() {
        // Windows-edited key file: CRLF line ending. Stripping only
        // \n would leave \r in the key → mismatch with a Unix-edited
        // key file (or a K8s Secret created from a literal).
        let tmp_crlf = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp_crlf.path(), b"secret-key-32-bytes-long-here!!\r\n").unwrap();
        let tmp_bare = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp_bare.path(), b"secret-key-32-bytes-long-here!!").unwrap();

        let signer = HmacSigner::load(Some(tmp_crlf.path())).unwrap().unwrap();
        let verifier = HmacVerifier::load(Some(tmp_bare.path())).unwrap().unwrap();

        let claims = test_claims(3600);
        let token = signer.sign(&claims);
        let verified = verifier
            .verify::<AssignmentClaims>(&token)
            .expect("CRLF trimmed → same key → verify succeeds");
        assert_eq!(verified, claims);
    }

    /// Token is human-debuggable: base64-decode the first part →
    /// readable JSON. Not a functional test, more a documentation-
    /// by-example that the format is what we claimed.
    #[test]
    fn token_is_human_debuggable() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let claims = test_claims(3600);
        let token = signer.sign(&claims);

        let claims_b64 = token.split('.').next().unwrap();
        let claims_json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(claims_b64)
            .unwrap();
        let parsed: AssignmentClaims = serde_json::from_slice(&claims_json).unwrap();
        assert_eq!(parsed, claims);
        // The JSON string is human-readable (operator can debug a
        // failing token by base64-decoding the first part).
        let json_str = String::from_utf8(claims_json).unwrap();
        assert!(json_str.contains("\"executor_id\":\"test-builder\""));
    }

    /// merged_bug_115's boundary, pinned (no behavior change): the
    /// service key is fleet-shared and symmetric, so ANY key holder
    /// mints a `ServiceClaims` carrying ANY instance that verifies —
    /// the instance binding is cross-service narrowing + injection
    /// detection, NOT intra-fleet attestation. If this test ever needs
    /// to change, the threat-model doc on `ServiceClaims::instance`
    /// (and the Phase-B obligation-1 record) changes with it.
    #[test]
    fn any_key_holder_mints_any_instance() {
        let key = HmacKey::from_key(b"shared-service-key-32-bytes-ok!!".to_vec());
        // A "compromised sibling replica" (any holder of the shared
        // key) mints claims for a victim replica's instance...
        let forged = ServiceClaims {
            caller: "rio-store".into(),
            expiry_unix: crate::now_unix().expect("clock") + 60,
            instance: Some("victim-replica-0".into()),
        };
        let token = key.sign(&forged);
        // ...and it VERIFIES as the victim's instance-bound credential.
        let verified: ServiceClaims = key.verify(&token).expect("symmetric key verifies");
        assert_eq!(verified.instance.as_deref(), Some("victim-replica-0"));
    }

    fn executor_claims(expiry_offset_secs: i64) -> ExecutorClaims {
        let now = crate::now_unix().expect("clock");
        ExecutorClaims {
            intent_id: "drv-abc123".into(),
            kind: 0,
            expiry_unix: ((now as i64) + expiry_offset_secs).max(0) as u64,
        }
    }

    /// merged_bug_079: two mints over EQUAL claims must produce
    /// byte-distinct tokens. The confirm fence keys durable rows on
    /// `sha256(token bytes)` (`executor_confirm_fences`, GC'd past the
    /// credential-derived `CONFIRM_FENCE_GC_SECS` horizon), so a
    /// deterministic re-mint across controller ticks hands a
    /// replacement forecast pod its predecessor's fence identity — the
    /// DeliverNew screen answers the NEW pod `Gone` for an exit the OLD
    /// pod declared, and the replacement never builds. Uniqueness is
    /// minted at the token constructor (`HmacKey::sign` — the one
    /// production serialization site), not at the call site: the
    /// snapshot.rs mint literal stays untouched.
    #[test]
    fn executor_token_remint_is_byte_unique() {
        let key = HmacKey::from_key(TEST_KEY.to_vec());
        let claims = executor_claims(3600);
        let t1 = key.sign(&claims);
        let t2 = key.sign(&claims);
        assert_ne!(
            t1, t2,
            "re-minting equal ExecutorClaims must never reproduce the same token bytes \
             (the confirm fence keys on sha256(token))"
        );
    }

    /// The spawn nonce is transport-only entropy: it must not leak into
    /// the decoded claims (the struct keeps three fields; `PartialEq`
    /// is over them) and a signed token must round-trip its semantic
    /// content exactly.
    #[test]
    fn executor_token_roundtrip_preserves_claims() {
        let key = HmacKey::from_key(TEST_KEY.to_vec());
        let claims = executor_claims(3600);
        let verified: ExecutorClaims = key.verify(&key.sign(&claims)).expect("fresh token");
        assert_eq!(verified, claims);
    }

    /// SIGNED Q6 (--wipe rollout): the `spawn` nonce is REQUIRED at
    /// decode in the same release that mints it — no optional-decode
    /// window. A nonce-free claims JSON signed through the PRIOR serde
    /// shape (the pre-fix three-field form) must be REJECTED at decode:
    /// post-wipe no such token can exist, so accepting one would only
    /// ever accept a forgery-shaped or stale-world credential.
    #[test]
    fn noncefree_token_rejected_at_decode() {
        let key = HmacKey::from_key(TEST_KEY.to_vec());
        // r13-allow(refusal-probe): asserts the typed refusal of a
        // shape the producing constructor can no longer emit — the
        // pre-fix three-field claims JSON, hand-assembled and signed
        // with the real key precisely so the SIGNATURE passes and the
        // decode layer is what rejects.
        let noncefree = serde_json::json!({
            "intent_id": "drv-abc123",
            "kind": 0,
            "expiry_unix": crate::now_unix().expect("clock") + 3600,
        });
        let claims_json = serde_json::to_vec(&noncefree).expect("serialize");
        let mut mac = HmacSha256::new_from_slice(TEST_KEY).expect("any key length");
        mac.update(&claims_json);
        let tag = mac.finalize().into_bytes();
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let token = format!("{}.{}", b64.encode(&claims_json), b64.encode(tag));
        let result = key.verify::<ExecutorClaims>(&token);
        assert!(
            matches!(result, Err(HmacError::Json(_))),
            "a nonce-free executor token must be rejected at decode (Q6: no \
             pre-fix token survives a --wipe rollout), got {result:?}"
        );
    }

    /// W10-P (merged_bug_045) — the FAMILY lifetime law at its own
    /// quantifier: NO token signed by a family key carries
    /// `expiry_unix − now > MAX_HMAC_LIFETIME_SECS`, for EVERY claims
    /// type sharing the key — not just the assignment mint the
    /// dispatch-local clamp covered. The matrix drives both family
    /// claims types × {at-cap, over-cap}: at-cap mints sign verbatim
    /// (the clamp is a bound, not a haircut); over-cap mints come out
    /// clamped to the cap. The red this test was born failing on
    /// (pre-fix): a synthetic over-cap mint signs successfully and
    /// verifies with the full over-long replay window — the law's
    /// negation at the family quantifier (under production defaults
    /// the executor mint is bounded ≈24.25h by DEADLINE_CAP_SECS +
    /// eta + 300, so the red drives the LAW with a synthetic mint,
    /// not a live overflow).
    ///
    /// WO-S8-6 (bug_141, R29): the witness derives its bound from a
    /// clock sample taken AFTER each sign call — `HmacKey::sign`
    /// re-samples its own clock and clamps to that LATER cap, so a
    /// pre-sampled bound false-fails on a second-boundary crossing
    /// (correct clamp at cap+k, stale bound at cap — the catalogued
    /// wall-clock-gate flake class inside an R24 law witness). With
    /// the post-call re-sample, `signer_now <= now_after` holds by
    /// sampling order, so `clamped <= cap_after` is deterministic.
    /// The pre-sample strawman is kept as the disclosed reversal red
    /// (`family_lifetime_pin_strawman_presample_false_fails`).
    #[test]
    fn family_lifetime_clamp_bounds_every_claims_type() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());
        let now = crate::now_unix().expect("test clock after epoch");
        let cap = now + MAX_HMAC_LIFETIME_SECS;

        // {AssignmentClaims, ExecutorClaims} × {at-cap, over-cap}
        for (label, requested) in [("at-cap", cap), ("over-cap", cap + 3600)] {
            let mut ac = test_claims(0);
            ac.expiry_unix = requested;
            let verified: AssignmentClaims = verifier
                .verify(&signer.sign(&ac))
                .expect("family token verifies");
            // R29: the bound's clock is sampled AFTER the sign call.
            let cap_after =
                crate::now_unix().expect("test clock after epoch") + MAX_HMAC_LIFETIME_SECS;
            assert!(
                verified.expiry_unix <= cap_after,
                "left (pre-fix): an AssignmentClaims {label} mint signed with \
                 expiry−now = {}s (> {MAX_HMAC_LIFETIME_SECS}s — the leaked-token \
                 replay window the seven-day law bounds) / right: the signer \
                 clamps the family expiry to the cap",
                verified.expiry_unix - now
            );
            if requested <= cap {
                assert_eq!(
                    verified.expiry_unix, requested,
                    "an at-cap mint must sign verbatim (the clamp is a bound)"
                );
            }

            let ec = ExecutorClaims {
                intent_id: "drv-w10p".into(),
                kind: 0,
                expiry_unix: requested,
            };
            let verified: ExecutorClaims = verifier
                .verify(&signer.sign(&ec))
                .expect("family token verifies");
            // R29: each sign gets its own post-call bound sample.
            let cap_after =
                crate::now_unix().expect("test clock after epoch") + MAX_HMAC_LIFETIME_SECS;
            assert!(
                verified.expiry_unix <= cap_after,
                "left (pre-fix): an ExecutorClaims {label} mint signed with \
                 expiry−now = {}s (> {MAX_HMAC_LIFETIME_SECS}s) — the SIBLING \
                 mint the dispatch-local clamp never covered / right: the \
                 signer clamps the family expiry",
                verified.expiry_unix - now
            );
            if requested <= cap {
                assert_eq!(
                    verified.expiry_unix, requested,
                    "an at-cap executor mint must sign verbatim"
                );
            }
        }
    }

    /// W11-BY (bug_141, R29) — the split-clock witness mode, killed
    /// deterministically: a law pin deriving its bound from a clock
    /// sample strictly EARLIER than the producer's own re-sample
    /// false-fails on CORRECT behavior across a second boundary
    /// (CI-only flake; production clamp correct). The boundary cross
    /// is FORCED by construction instead of raced: `cap_pre` derives
    /// from a sample one second older than anything the signer can
    /// read, so the signer's correct clamp of an over-cap mint
    /// PROVABLY exceeds the stale bound — exactly the shape the old
    /// `<= cap` assert misreported as a seven-day-law regression.
    /// The strawman is the disclosed reversal red; the law pin above
    /// re-samples after the call (signer_now <= now_after by
    /// sampling order, so clamped <= cap_after deterministically).
    #[test]
    fn family_lifetime_pin_strawman_presample_false_fails() {
        let signer = HmacSigner::from_key(TEST_KEY.to_vec());
        let verifier = HmacVerifier::from_key(TEST_KEY.to_vec());
        // The stale pre-sample: one second older than any clock the
        // signer can observe — the forced boundary cross.
        let now_pre = crate::now_unix().expect("test clock after epoch") - 1;
        let cap_pre = now_pre + MAX_HMAC_LIFETIME_SECS;

        let mut ac = test_claims(0);
        ac.expiry_unix = cap_pre + 7200; // over-cap from every clock
        let verified: AssignmentClaims = verifier
            .verify(&signer.sign(&ac))
            .expect("family token verifies");

        // The signer clamped CORRECTLY (to signer_now + cap), yet the
        // stale pre-sample bound reads the clamp as a law violation:
        // the deterministic false-fail of the old witness shape.
        assert!(
            verified.expiry_unix > cap_pre,
            "the strawman's premise broke: a correct clamp no longer exceeds \
             the stale pre-sample bound (clamp or clock semantics changed — \
             re-derive the witness)"
        );
        // Same mint, right clock: the post-call re-sample bound holds.
        let cap_after = crate::now_unix().expect("test clock after epoch") + MAX_HMAC_LIFETIME_SECS;
        assert!(
            verified.expiry_unix <= cap_after,
            "the law pin's own bound must hold on the same mint"
        );
    }

    /// W10-Q (merged_bug_045, R15/R22′) — the signing-body census:
    /// the family lifetime law is only as total as the claim that
    /// [`HmacKey::sign`] is the ONLY mint. Generated member list (the
    /// raw-MAC instantiation scan over every src file in this crate,
    /// embedded per (wwwww) with the dev-tree completeness pin
    /// below): exactly TWO production sites — THE sign body (clamped)
    /// and THE verify body (mints nothing) — plus three DISCLOSED
    /// test fixtures that hand-roll tampered/legacy tokens to drive
    /// verify's rejects. A new raw-MAC site anywhere in the crate fails this census until filed — quantifier: census(mac_census_universe_matches_live_tree)
    /// (WO-S8-5: the binding walk is RECURSIVE; the depth-1 walk
    /// this replaced made the word quantify over the flat layer
    /// only). Out-of-crate raw mints are
    /// outside this census's jurisdiction by construction (the key
    /// loader and the claims types live here; consumers sign through
    /// this module) — disclosed, not silently claimed.
    #[test]
    fn raw_mac_mint_sites_pinned() {
        let hits = mac_census_over(&embedded_sources());
        let expected: std::collections::BTreeMap<String, usize> =
            [("hmac.rs".to_string(), 5)].into();
        mac_assert_census(&hits, &expected).unwrap();
    }

    /// W10-Q's planted red (R22′): a strawman source file carrying a
    /// raw-MAC mint that computes its own expiry OUTSIDE the clamped
    /// signer enters at the SCANNER layer (raw source text); the
    /// census comparison goes red and names the file. Needle and
    /// strawman are runtime-assembled so this file's static text
    /// never matches itself.
    #[test]
    fn mac_census_plants_red_on_unclamped_mint() {
        let strawman = format!(
            "fn rogue_mint(key: &[u8], json: &[u8]) -> Vec<u8> {{\n\
             let mut mac = HmacSha256{}{}(key).unwrap();\n\
             mac.update(json); mac.finalize().into_bytes().to_vec() }}\n",
            "::", "new_from_slice"
        );
        let mut universe = embedded_sources();
        universe.push(("rogue.rs".to_string(), strawman));
        let hits = mac_census_over(&universe);
        let expected: std::collections::BTreeMap<String, usize> =
            [("hmac.rs".to_string(), 5)].into();
        let err = mac_assert_census(&hits, &expected)
            .expect_err("an unlisted raw-MAC mint MUST go census-red");
        assert!(
            err.contains("rogue.rs"),
            "the red must NAME the unlisted mint site; got: {err}"
        );
    }

    /// Recursive census-universe walk (WO-S8-5, merged_bug_122):
    /// RELATIVE paths (`sub/mod.rs` form) of every `.rs` FILE under
    /// `root`, recursing into subdirectories and asserting every
    /// entry's type — a directory's `.rs` content JOINS the universe
    /// (so a flat embedded list fails the equality with the nested
    /// path NAMED), and a non-file/non-dir entry panics. The
    /// completeness witness FAILS CLOSED on population-SHAPE change:
    /// a subdirectory appearing is a census red, never a silent
    /// exclusion. The depth-1 `read_dir` + name-filter form this
    /// replaced silently dropped directory entries, so a future
    /// `src/<subdir>/foo.rs` raw mint evaded BOTH the scan and the
    /// parity pin, voiding the crate-wide census guarantee the
    /// CONFIRM_FENCE_GC_SECS derivation leans on.
    fn walk_rs_files(root: &std::path::Path, prefix: &str) -> Vec<String> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(root).expect("readable census dir") {
            let e = e.expect("entry");
            let ft = e.file_type().expect("entry file type");
            let name = e
                .file_name()
                .to_str()
                .expect("source file names are utf-8")
                .to_owned();
            if ft.is_dir() {
                out.extend(walk_rs_files(&e.path(), &format!("{prefix}{name}/")));
            } else {
                assert!(
                    ft.is_file(),
                    "census walk: refusing non-file non-dir entry {prefix}{name} \
                     (fail-closed: the universe must classify every entry)"
                );
                if name.ends_with(".rs") {
                    out.push(format!("{prefix}{name}"));
                }
            }
        }
        out
    }

    /// The (wwwww) dual obligation: the embedded census universe
    /// equals the live `src/` tree exactly (both directions); in the
    /// nix sandbox (no source dir) the embedded scan is the same
    /// commit's content — skip disclosed, never silent. "Every src
    /// file" quantifies over the RECURSIVE walk (WO-S8-5) — a nested
    /// `src/<subdir>/*.rs` appears in `live` and fails this equality
    /// by name until embedded.
    #[test]
    fn mac_census_universe_matches_live_tree() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        if !root.exists() {
            eprintln!("src/ not on disk (nix sandbox): universe pinned by the dev-tree run");
            return;
        }
        let mut live = walk_rs_files(&root, "");
        live.sort();
        let mut embedded: Vec<String> = embedded_sources().iter().map(|(f, _)| f.clone()).collect();
        embedded.sort();
        assert_eq!(embedded, live, "embed every src file in embedded_sources()");
    }

    /// W11-BX (merged_bug_122): the population-growth mode the
    /// universe pin exists for — a SUBDIRECTORY appearing in the
    /// crate. The recursive walk must return the nested file (so the
    /// embedded-equality pin goes red NAMING it), while the depth-1
    /// strawman — the exact pre-fix form, kept as the disclosed
    /// reversal red — silently drops the directory entry and reports
    /// the flat file only. If the walk ever regresses to depth-1,
    /// the first assertion here is the red.
    #[test]
    fn mac_census_walk_fails_closed_on_crate_shape_change() {
        let td = tempfile::tempdir().expect("tempdir");
        std::fs::write(td.path().join("a.rs"), "// flat").unwrap();
        std::fs::create_dir(td.path().join("sub")).unwrap();
        std::fs::write(td.path().join("sub").join("b.rs"), "// nested").unwrap();

        let mut walked = walk_rs_files(td.path(), "");
        walked.sort();
        assert_eq!(
            walked,
            vec!["a.rs".to_string(), "sub/b.rs".to_string()],
            "left (pre-fix): the depth-1 walk returns [\"a.rs\"] — the nested \
             census member is silently excluded / right: the recursive walk \
             carries the population-shape change into the universe"
        );

        // The strawman (the pre-fix depth-1 form, verbatim shape):
        // directory entries fail the name filter and vanish.
        let mut strawman: Vec<String> = std::fs::read_dir(td.path())
            .expect("readable dir")
            .map(|e| {
                e.expect("entry")
                    .file_name()
                    .to_str()
                    .expect("utf-8")
                    .to_owned()
            })
            .filter(|n| n.ends_with(".rs"))
            .collect();
        strawman.sort();
        assert_eq!(
            strawman,
            vec!["a.rs".to_string()],
            "the disclosed reversal red: the old depth-1 form cannot see sub/b.rs"
        );
    }

    fn embedded_sources() -> Vec<(String, String)> {
        vec![
            ("hmac.rs".to_string(), include_str!("hmac.rs").to_string()),
            ("jwt.rs".to_string(), include_str!("jwt.rs").to_string()),
            (
                "jwt_interceptor.rs".to_string(),
                include_str!("jwt_interceptor.rs").to_string(),
            ),
            ("lib.rs".to_string(), include_str!("lib.rs").to_string()),
        ]
    }

    /// Needle assembled at runtime so the census never matches its
    /// own source text.
    fn mac_census_over(universe: &[(String, String)]) -> std::collections::BTreeMap<String, usize> {
        let needle = format!("HmacSha256{}{}", "::", "new_from_slice");
        let mut hits = std::collections::BTreeMap::new();
        for (rel, text) in universe {
            let n = text.matches(&needle).count();
            if n > 0 {
                *hits.entry(rel.clone()).or_insert(0) += n;
            }
        }
        hits
    }

    fn mac_assert_census(
        actual: &std::collections::BTreeMap<String, usize>,
        expected: &std::collections::BTreeMap<String, usize>,
    ) -> Result<(), String> {
        if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "raw-MAC mint census drifted.\n  actual:   {actual:?}\n  expected: {expected:?}\n\
                 every signing body must be the clamped HmacKey::sign — file a new \
                 site here only with its lifetime-law rationale"
            ))
        }
    }
}

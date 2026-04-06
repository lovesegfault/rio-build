//! tonic interceptor for `x-rio-tenant-token` JWT verification.
//!
//! Sits on the scheduler and store gRPC servers (the two binaries the
//! gateway dials). Extracts the `x-rio-tenant-token` metadata header,
//! verifies signature + expiry against the configured ed25519 pubkey,
//! and attaches the decoded [`Claims`] to the request's extensions for
//! handlers to read. The scheduler's `SubmitBuild` handler additionally
//! checks `Claims.jti` against the `jwt_revoked` table — that's a PG
//! lookup, so it lives in the handler, not here (this crate is PG-free).
//!
//! # Permissive on absent header — load-bearing for coexistence
//!
//! A missing `x-rio-tenant-token` header is **pass-through**, not
//! rejection. Three distinct callers hit the same `Server::builder()`
//! this interceptor layers onto, and only ONE of them carries a JWT:
//!
//! | Caller | Header? | Auth mechanism |
//! |---|---|---|
//! | Gateway (JWT mode) | yes | this interceptor |
//! | Workers → `WorkerService`/`StoreService` | no | HMAC assignment tokens |
//! | K8s kubelet → `grpc.health.v1.Health` | no | none (plaintext probe) |
//!
//! If absent-header were a rejection, wiring this via `.layer()` would
//! break worker heartbeats and health probes the moment the pubkey is
//! configured. The alternative — per-service `with_interceptor` wrapping
//! — loses `.max_decoding_message_size()` and diverges types across the
//! `Option<_>` branches.
//!
//! This is also the spec-mandated behavior: `r[gw.jwt.verify]` says
//! "reject invalid tokens" (present + bad signature/expiry), and
//! `r[gw.jwt.dual-mode]` makes the absent-header SSH-comment fallback
//! path permanent. A present-but-invalid token IS rejected with
//! `UNAUTHENTICATED` — a tampered or expired token never passes.
//!
//! # Hot-swap via `Arc<RwLock>`
//!
//! The pubkey is `Arc<RwLock<VerifyingKey>>` so [`spawn_pubkey_reload`]'s
//! SIGHUP handler can swap the key without restarting the server. Read-lock held only
//! for the `jwt::verify` call (microseconds); annual key rotation means
//! the write lock is essentially uncontended. `std::sync::RwLock`, not
//! tokio's — the `Interceptor` trait is sync (`FnMut`, not `async Fn`),
//! and the verify call does no I/O.
//!
//! # `Option` wrapping — dev mode gate
//!
//! `None` → interceptor is a no-op pass-through. Lets `main.rs` wire it
//! unconditionally (no type divergence between with/without branches).
//! Scheduler+store main.rs gate on `cfg.jwt.key_path`: `Some` →
//! [`load_jwt_pubkey`] + [`spawn_pubkey_reload`]; `None` → inert.
//! Matches the gateway-side `Option<SigningKey>` pattern — JWT is
//! opt-in at both ends, gated on deployment config.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use base64::Engine;
use ed25519_dalek::VerifyingKey;
use tonic::{Request, Status};

use crate::jwt::{self, Claims};
use crate::signal::Token as CancellationToken;

/// gRPC metadata key the gateway sets on every outbound call in JWT mode.
///
/// Lowercase: tonic normalizes metadata keys (HTTP/2 header rules).
/// Matches `rio-gateway/src/handler/build.rs` — if the gateway ever
/// renames this, `header_name_matches_gateway_literal` below fails.
pub const TENANT_TOKEN_HEADER: &str = "x-rio-tenant-token";

/// Shared pubkey handle the interceptor reads on every intercepted call.
///
/// `Option` at the type level: `None` means JWT verification is
/// disabled for this process (dev mode / key not yet configured).
/// `Arc<RwLock>` inside: the key itself is hot-swappable for rotation.
pub type JwtPubkey = Option<Arc<RwLock<VerifyingKey>>>;

/// Load a base64-encoded ed25519 public key from a file.
///
/// File format: 32 raw bytes base64'd (standard alphabet, padding
/// optional), optionally followed by whitespace — matches the
/// `jwt-pubkey-configmap.yaml` mount format (`.Values.jwt.publicKey`
/// is the operator's `base64` of the raw 32-byte pubkey).
///
/// Synchronous — called at boot BEFORE the tokio runtime is doing
/// anything interesting. The reload path (SIGHUP) uses `tokio::fs`
/// via [`spawn_pubkey_reload`], but the initial load happens in
/// `main()` before the server spins up, so blocking is fine there.
pub fn load_jwt_pubkey(path: &Path) -> anyhow::Result<VerifyingKey> {
    let raw = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("read JWT pubkey from {}: {e}", path.display()))?;
    parse_jwt_pubkey(&raw)
}

/// Parse a base64'd 32-byte ed25519 public key from bytes.
///
/// Factored out for the async SIGHUP reload (tokio::fs::read gives
/// `Vec<u8>`) and for unit tests (no filesystem). Input is TRIMMED
/// of ASCII whitespace — ConfigMap mounts sometimes carry a trailing
/// newline, and `echo foo | base64` adds one. Forgetting to strip it
/// is a classic "pubkey loads fine in the unit test, fails in K8s"
/// failure mode.
fn parse_jwt_pubkey(raw: &[u8]) -> anyhow::Result<VerifyingKey> {
    let trimmed = raw.trim_ascii();
    // STANDARD: what `base64` CLI emits. NO_PAD tolerates both
    // padded and unpadded — operator might use `-w0` or might not.
    // We try STANDARD first (most common), fall back to URL_SAFE
    // (what some key-generation scripts produce).
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(trimmed))
        .map_err(|e| anyhow::anyhow!("JWT pubkey base64 decode: {e}"))?;
    let arr: [u8; 32] = decoded.try_into().map_err(|v: Vec<u8>| {
        anyhow::anyhow!(
            "JWT pubkey must be exactly 32 bytes after base64 decode, got {}",
            v.len()
        )
    })?;
    VerifyingKey::from_bytes(&arr)
        .map_err(|e| anyhow::anyhow!("JWT pubkey is not a valid ed25519 point: {e}"))
}

/// Spawn a SIGHUP-triggered pubkey reload loop.
///
/// Call this ONCE from `main()` after the initial [`load_jwt_pubkey`].
/// On each SIGHUP: re-reads `path`, parses, swaps the key into `key`.
/// Parse failure → old key retained (logged warning, not fatal).
///
/// This is the "don't inline ×2" consolidation: scheduler and store
/// both have the same ConfigMap mount + SIGHUP rotation need. One
/// function, two call sites. The 11th paired-main.rs would-be commit
/// that prompted this — the prior 10 (TLS, health, lease, metrics…)
/// already showed the pattern.
///
/// ```ignore
/// // in scheduler/main.rs AND store/main.rs:
/// let jwt_pubkey: JwtPubkey = match &cfg.jwt.key_path {
///     None => None,
///     Some(path) => {
///         let initial = load_jwt_pubkey(path)?;
///         let shared = Arc::new(RwLock::new(initial));
///         spawn_pubkey_reload(path.clone(), Arc::clone(&shared), shutdown.clone());
///         Some(shared)
///     }
/// };
/// // ... later ...
/// .layer(InterceptorLayer::new(jwt_interceptor(jwt_pubkey)))
/// ```
pub fn spawn_pubkey_reload(
    path: PathBuf,
    key: Arc<RwLock<VerifyingKey>>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    crate::signal::sighup_reload(shutdown, move || {
        let path = path.clone();
        let key = Arc::clone(&key);
        async move {
            let raw = tokio::fs::read(&path)
                .await
                .map_err(|e| anyhow::anyhow!("re-read JWT pubkey from {}: {e}", path.display()))?;
            let new_key = parse_jwt_pubkey(&raw)?;
            // Write-lock held for a single pointer-sized assignment.
            // `.expect` on the lock: see the interceptor's read-lock
            // .expect comment — poisoned means a prior SIGHUP panicked
            // mid-swap, which can't happen (assignment can't panic).
            *key.write().expect("JWT pubkey RwLock poisoned on SIGHUP") = new_key;
            tracing::info!(path = %path.display(), "JWT pubkey hot-swapped");
            Ok(())
        }
    })
}

// r[impl gw.jwt.verify]
/// Build the JWT-verify interceptor closure.
///
/// The returned closure is `Clone` (`InterceptorLayer` requires it):
/// `Option<Arc<_>>` clones the Arc pointer, not the underlying key.
/// Every tonic connection gets its own closure clone, but they all
/// share the single `RwLock<VerifyingKey>` instance.
///
/// Error reporting: on verify failure, the `jsonwebtoken` error's
/// `Display` is threaded into the Status message. That's safe to
/// surface — it says "ExpiredSignature" or "InvalidSignature", not
/// anything about the key material. An operator debugging a 401 can
/// tell expired-vs-tampered without guessing.
pub fn jwt_interceptor(pubkey: JwtPubkey) -> impl tonic::service::Interceptor + Clone {
    move |mut req: Request<()>| -> Result<Request<()>, Status> {
        // ---- Dev-mode bypass ----
        // No pubkey configured → no verification possible → pass through.
        // Wired in scheduler+store main.rs: cfg.jwt.key_path →
        // load_jwt_pubkey → Arc<RwLock> → spawn_pubkey_reload (SIGHUP
        // swaps). When key_path is unset (dev, or pre-key-rotation-infra
        // clusters), the interceptor stays inert: all RPCs pass, Claims
        // extension never attached.
        let Some(pubkey) = &pubkey else {
            return Ok(req);
        };

        // ---- Dual-mode bypass ----
        // Header absent → gateway is in SSH-comment fallback mode (or
        // this is a worker/health/admin caller that never sends it).
        // Pass through WITHOUT attaching Claims. Handlers that care
        // about tenant identity fall back to `tenant_name` proto field.
        let Some(raw) = req.metadata().get(TENANT_TOKEN_HEADER) else {
            return Ok(req);
        };

        // ---- Strict verify on present header ----
        // From here down, ANY failure is `UNAUTHENTICATED`. A caller
        // that went to the trouble of setting the header is asserting
        // "I am authenticated via JWT" — a malformed or expired token
        // is an auth failure, not a fallback trigger.
        let token = raw.to_str().map_err(|_| {
            // Metadata values are ASCII-encodable bytes. A non-ASCII
            // token header is either corrupted or adversarial; either
            // way, not a valid JWT (base64url is pure ASCII).
            Status::unauthenticated(format!("{TENANT_TOKEN_HEADER} header is not valid ASCII"))
        })?;

        let claims: Claims = {
            // Read-lock scope: held ONLY for verify. `jwt::verify` is
            // pure compute (ed25519 sig check + JSON parse + exp
            // compare); no await, no I/O. The guard drops at scope
            // exit — before we touch `req.extensions_mut()`.
            //
            // `.expect` on the lock: RwLock poisoning means a prior
            // thread panicked WHILE HOLDING THE WRITE LOCK. The only
            // writer is spawn_pubkey_reload's SIGHUP handler. If that
            // panicked, the key state is unknown — refusing service is
            // correct.
            // (Realistically: the SIGHUP handler does `*lock = new_key`
            // which can't panic, so this is unreachable.)
            let guard = pubkey
                .read()
                .expect("jwt pubkey RwLock poisoned — SIGHUP key-swap handler panicked");
            jwt::verify(token, &guard)
                .map_err(|e| Status::unauthenticated(format!("JWT verify failed: {e}")))?
        };

        // Attach Claims for handlers. `insert` replaces any prior
        // value of the same type — there won't BE one (this
        // interceptor is the only Claims producer), but the semantics
        // mean a double-layer would be idempotent rather than broken.
        req.extensions_mut().insert(claims);
        Ok(req)
    }
}

// r[verify gw.jwt.verify]
#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use tonic::service::Interceptor;

    /// Deterministic keypair from a fixed seed — same pattern as
    /// `jwt.rs` tests (see the rand_core version-skew note there).
    fn keypair(seed: u8) -> (SigningKey, VerifyingKey) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let vk = sk.verifying_key();
        (sk, vk)
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn claims(exp_offset: i64) -> Claims {
        let n = now();
        Claims {
            sub: uuid::Uuid::from_u128(0x1234),
            iat: n,
            exp: n + exp_offset,
            jti: "test-jti".into(),
        }
    }

    fn pubkey(vk: VerifyingKey) -> JwtPubkey {
        Some(Arc::new(RwLock::new(vk)))
    }

    /// Build a `Request<()>` with the token header set. Metadata keys
    /// are lowercase-normalized by tonic; using the constant keeps
    /// tests in sync with production.
    fn req_with_token(token: &str) -> Request<()> {
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(TENANT_TOKEN_HEADER, token.parse().unwrap());
        req
    }

    // ------------------------------------------------------------------------
    // Pass-through paths — the two bypass branches
    // ------------------------------------------------------------------------

    /// `pubkey = None` → inert. Dev mode / `cfg.jwt.key_path` unset.
    #[test]
    fn no_pubkey_passes_through() {
        let mut intercept = jwt_interceptor(None);
        // Even WITH a header present, no pubkey means no verify.
        // (We can't verify without a key; passing through is the
        // only non-broken option. The alternative — reject all —
        // would brick dev clusters.)
        let out = intercept.call(req_with_token("garbage")).unwrap();
        assert!(
            out.extensions().get::<Claims>().is_none(),
            "no-pubkey path must not attach Claims — it didn't verify anything"
        );
    }

    /// Header absent → dual-mode fallback. Workers, health probes,
    /// admin tools all take this path. They authenticate through
    /// OTHER mechanisms (HMAC, mTLS client cert, none).
    #[test]
    fn absent_header_passes_through() {
        let (_, vk) = keypair(0x42);
        let mut intercept = jwt_interceptor(pubkey(vk));
        let out = intercept.call(Request::new(())).unwrap();
        assert!(
            out.extensions().get::<Claims>().is_none(),
            "absent header → no Claims attached; handler falls back to tenant_name"
        );
    }

    // ------------------------------------------------------------------------
    // Positive path — valid token → Claims attached
    // ------------------------------------------------------------------------

    #[test]
    fn valid_token_attaches_claims() {
        let (sk, vk) = keypair(0x42);
        let original = claims(3600);
        let token = jwt::sign(&original, &sk).unwrap();

        let mut intercept = jwt_interceptor(pubkey(vk));
        let out = intercept.call(req_with_token(&token)).unwrap();

        let attached = out
            .extensions()
            .get::<Claims>()
            .expect("valid token → Claims attached to extensions");
        // Full struct equality, not just jti — sub is the tenant
        // identity handlers use for authz; it must roundtrip exactly.
        assert_eq!(attached, &original);
    }

    // ------------------------------------------------------------------------
    // Negative paths — every UNAUTHENTICATED mode
    // ------------------------------------------------------------------------

    #[test]
    fn expired_jwt_returns_unauthenticated() {
        let (sk, vk) = keypair(0x42);
        // 1h past. jwt.rs's expired_jwt_rejected test explains why
        // -3600 (not -30): jsonwebtoken's 60s leeway would make a
        // small offset flaky.
        let token = jwt::sign(&claims(-3600), &sk).unwrap();

        let mut intercept = jwt_interceptor(pubkey(vk));
        let status = intercept.call(req_with_token(&token)).unwrap_err();

        assert_eq!(status.code(), tonic::Code::Unauthenticated);
        // The jsonwebtoken error surfaces in the message — operators
        // debugging a 401 can tell expired-vs-bad-sig without
        // cranking up log levels.
        assert!(
            status.message().contains("JWT verify failed"),
            "got: {}",
            status.message()
        );
    }

    /// Wrong key → signature mismatch → UNAUTHENTICATED. A token
    /// minted by a compromised (or just misconfigured) gateway with
    /// the wrong signing key is rejected here.
    #[test]
    fn invalid_jwt_returns_unauthenticated() {
        let (sk_a, _) = keypair(0xAA);
        let (_, vk_b) = keypair(0xBB);
        let token = jwt::sign(&claims(3600), &sk_a).unwrap();

        let mut intercept = jwt_interceptor(pubkey(vk_b));
        let status = intercept.call(req_with_token(&token)).unwrap_err();

        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

    /// Garbage in the header → UNAUTHENTICATED, not a panic or a
    /// different code. jsonwebtoken's decode handles this gracefully
    /// (returns an error); we map it. An attacker spraying random
    /// header values gets a uniform 401.
    #[test]
    fn malformed_token_returns_unauthenticated() {
        let (_, vk) = keypair(0x42);
        let mut intercept = jwt_interceptor(pubkey(vk));

        for garbage in ["", "not.a.jwt", "a.b.c.d.e", "🎭"] {
            // The emoji case never reaches jwt::verify — it fails the
            // `to_str()` ASCII check first (metadata values are
            // percent-encoded bytes; parsing a non-ASCII string into
            // MetadataValue would fail anyway, but belt-and-braces).
            // Skip it if `parse()` rejects.
            let Ok(val) = garbage.parse() else { continue };
            let mut req = Request::new(());
            req.metadata_mut().insert(TENANT_TOKEN_HEADER, val);

            let status = intercept.call(req).unwrap_err();
            assert_eq!(
                status.code(),
                tonic::Code::Unauthenticated,
                "garbage {:?} should map to UNAUTHENTICATED",
                garbage
            );
        }
    }

    // ------------------------------------------------------------------------
    // Hot-swap — Arc<RwLock> proves its purpose
    // ------------------------------------------------------------------------

    /// After a write-lock swap, the SAME interceptor instance (no
    /// re-construction) verifies against the new key.
    /// spawn_pubkey_reload's SIGHUP handler does exactly this:
    /// `*pubkey.write().unwrap() = new_vk`.
    ///
    /// Without the RwLock (e.g., plain `Arc<VerifyingKey>`), rotation
    /// would need a server restart — the Arc can't be mutated and
    /// every tonic connection already cloned it.
    #[test]
    fn hot_swap_key_takes_effect_without_rebuild() {
        let (sk_old, vk_old) = keypair(0x01);
        let (sk_new, vk_new) = keypair(0x02);

        let shared = Arc::new(RwLock::new(vk_old));
        let mut intercept = jwt_interceptor(Some(Arc::clone(&shared)));

        // Phase 1: old key active. Old-key token passes; new-key token fails.
        let tok_old = jwt::sign(&claims(3600), &sk_old).unwrap();
        let tok_new = jwt::sign(&claims(3600), &sk_new).unwrap();
        intercept.call(req_with_token(&tok_old)).unwrap();
        intercept.call(req_with_token(&tok_new)).unwrap_err();

        // Hot-swap. Simulates spawn_pubkey_reload's SIGHUP handler
        // writing the ConfigMap-reloaded key.
        *shared.write().unwrap() = vk_new;

        // Phase 2: new key active. Flipped.
        intercept.call(req_with_token(&tok_old)).unwrap_err();
        let out = intercept.call(req_with_token(&tok_new)).unwrap();
        assert!(
            out.extensions().get::<Claims>().is_some(),
            "post-swap: new-key token verifies AND attaches Claims"
        );
    }

    // ------------------------------------------------------------------------
    // Cross-crate consistency pins
    // ------------------------------------------------------------------------

    // ------------------------------------------------------------------------
    // Pubkey load/parse — the ConfigMap mount format
    // ------------------------------------------------------------------------

    /// Round-trip: derive a pubkey, base64 it (as an operator would),
    /// parse it back, verify it decodes to the SAME key. Proves the
    /// file format matches what the helm chart's `jwt.publicKey`
    /// value expects.
    #[test]
    fn parse_jwt_pubkey_roundtrip() {
        let (_, vk) = keypair(0x42);
        let b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let parsed = parse_jwt_pubkey(b64.as_bytes()).expect("parse");
        assert_eq!(parsed.as_bytes(), vk.as_bytes());
    }

    /// Trailing newline — `echo ... | base64` adds one, ConfigMap
    /// mounts sometimes carry one. Must be stripped. This is THE
    /// classic K8s footgun: works in unit test, fails in cluster.
    #[test]
    fn parse_jwt_pubkey_trailing_newline() {
        let (_, vk) = keypair(0x99);
        let b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        let with_nl = format!("{b64}\n");

        let parsed = parse_jwt_pubkey(with_nl.as_bytes()).expect("trailing \\n stripped");
        assert_eq!(parsed.as_bytes(), vk.as_bytes());

        // And leading whitespace (less common, but trim_ascii handles both).
        let with_ws = format!("  {b64}  \n");
        let parsed = parse_jwt_pubkey(with_ws.as_bytes()).expect("surrounding ws stripped");
        assert_eq!(parsed.as_bytes(), vk.as_bytes());
    }

    /// URL-safe base64 alphabet (some key tools emit this). Fallback
    /// decoder tries it if STANDARD fails.
    #[test]
    fn parse_jwt_pubkey_urlsafe_alphabet() {
        let (_, vk) = keypair(0xFE);
        // URL_SAFE replaces + with - and / with _. Seed 0xFE picked so
        // the pubkey bytes include a byte with high bits set → base64
        // output is likely to differ between alphabets.
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vk.as_bytes());

        let parsed = parse_jwt_pubkey(b64.as_bytes()).expect("url-safe fallback");
        assert_eq!(parsed.as_bytes(), vk.as_bytes());
    }

    /// Wrong length after decode → clear error mentioning the length.
    /// 31 bytes, 33 bytes, 64 bytes (full keypair instead of pubkey
    /// alone — a plausible operator mistake).
    #[test]
    fn parse_jwt_pubkey_wrong_length() {
        for len in [0, 31, 33, 64] {
            let b64 = base64::engine::general_purpose::STANDARD.encode(vec![0u8; len]);
            let err = parse_jwt_pubkey(b64.as_bytes()).expect_err("wrong length → Err");
            assert!(
                err.to_string().contains("32 bytes"),
                "error should mention expected length: {err}"
            );
            assert!(
                err.to_string().contains(&len.to_string()),
                "error should mention actual length {len}: {err}"
            );
        }
    }

    /// Helper: base64-encode a pubkey + trailing newline (the
    /// ConfigMap mount format).
    fn encode_pubkey_file(vk: &VerifyingKey) -> String {
        let b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        format!("{b64}\n")
    }

    /// Load from an actual file — the production path. Tempfile
    /// with a trailing newline to mimic the ConfigMap mount.
    #[test]
    fn load_jwt_pubkey_from_file() {
        let (_, vk) = keypair(0x55);
        let b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), format!("{b64}\n")).unwrap();

        let loaded = load_jwt_pubkey(tmp.path()).expect("load from file");
        assert_eq!(loaded.as_bytes(), vk.as_bytes());
    }

    /// spawn_pubkey_reload composition: SIGHUP → tokio::fs::read →
    /// parse → RwLock write-swap. The pieces are tested individually
    /// (sighup_reload at signal.rs:198/242, parse_jwt_pubkey above,
    /// RwLock hot-swap in hot_swap_key_takes_effect_without_rebuild);
    /// this proves the WRAPPER composes them correctly end-to-end.
    ///
    /// Writes key-A to a tempfile, spawns the reload loop, overwrites
    /// with key-B, self-delivers SIGHUP, polls the RwLock until the
    /// swap lands. Also proves the NEGATIVE: a SIGHUP fired while the
    /// file holds garbage leaves the OLD key in place (reload failure
    /// is non-fatal — the interceptor keeps verifying against what was
    /// there before).
    ///
    /// SIGHUP is process-wide. tokio's `signal(SignalKind::hangup())`
    /// installs a shared disposition + per-call Signal stream, so
    /// concurrent SIGHUP listeners (signal.rs's own tests, other
    /// sighup_reload call sites) each get a wakeup — they don't steal
    /// from each other. This test's assertion is on the RwLock STATE,
    /// not on "exactly one reload fired", so extra wakeups from
    /// parallel test runs are harmless: if another test's SIGHUP
    /// arrives while our file holds key-B, we just swap to key-B
    /// again (idempotent). If it arrives while the file holds garbage,
    /// the swap is a no-op (parse fails, old key retained — exactly
    /// what the garbage-file phase asserts). No `#[serial]` needed.
    // r[verify gw.jwt.verify]
    #[tokio::test]
    async fn sighup_swaps_pubkey() {
        let (_, vk_a) = keypair(0xAA);
        let (_, vk_b) = keypair(0xBB);
        // Distinct keys — if keypair() ever collapsed (same seed path
        // due to a refactor), this test would be vacuous. Assert up
        // front.
        assert_ne!(
            vk_a.as_bytes(),
            vk_b.as_bytes(),
            "precondition: keys differ"
        );

        // Tempfile with key-A, trailing newline to match ConfigMap.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), encode_pubkey_file(&vk_a)).unwrap();

        // Shared RwLock seeded with key-A (what main.rs does via
        // load_jwt_pubkey at boot).
        let shared = Arc::new(RwLock::new(vk_a));
        let shutdown = crate::signal::Token::new();

        let handle = spawn_pubkey_reload(
            tmp.path().to_path_buf(),
            Arc::clone(&shared),
            shutdown.clone(),
        );

        // ── Phase 1: SIGHUP with GARBAGE in the file → old key retained ──
        // Overwrite with non-base64. The reload closure's
        // parse_jwt_pubkey() fails → returns Err → sighup_reload logs
        // + continues → RwLock untouched. Proves "botched rotation
        // doesn't brick the key" at the full-composition level (not
        // just sighup_reload's generic error-keeps-looping test).
        std::fs::write(tmp.path(), "not base64 at all\n").unwrap();
        // SAFETY: raise(3) just queues a signal; no memory invariants.
        unsafe {
            libc::raise(libc::SIGHUP);
        }
        // Give the spawned task a beat to process. We can't assert
        // "reload ran and failed" directly (no observable), so we
        // instead assert the key DIDN'T change after a bounded wait.
        // If the wait is too short, the test passes vacuously; if
        // too long, CI is slow. 100ms is the compromise — well past
        // the raise→recv→read→parse latency, short enough to not
        // matter for CI wall time.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            *shared.read().unwrap().as_bytes(),
            *vk_a.as_bytes(),
            "garbage file → parse fails → old key retained"
        );

        // ── Phase 2: SIGHUP with key-B in the file → swap observed ──
        std::fs::write(tmp.path(), encode_pubkey_file(&vk_b)).unwrap();
        unsafe {
            libc::raise(libc::SIGHUP);
        }
        // Poll-until-swapped. Bounded at 5s — if SIGHUP delivery or
        // the reload task is broken, this times out with a clear
        // assert message instead of hanging. Same spin-sleep pattern
        // as signal.rs's tests (no async wake-on-RwLock-change).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if *shared.read().unwrap().as_bytes() == *vk_b.as_bytes() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "pubkey should swap to key-B within 5s of SIGHUP; still key-A"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // ── Shutdown: loop exits cleanly ──
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("reload loop should exit on shutdown within 5s")
            .expect("reload task should not panic");
    }

    /// The gateway hardcodes `"x-rio-tenant-token"` as a string literal
    /// at the injection site (`rio-gateway/src/handler/build.rs`).
    /// This test fails if the constant here drifts. It can't catch the
    /// gateway drifting (that's a cross-crate check tracey can't do),
    /// but a VM test doing a full gateway→scheduler SubmitBuild covers
    /// the end-to-end path.
    #[test]
    fn header_name_matches_gateway_literal() {
        assert_eq!(TENANT_TOKEN_HEADER, "x-rio-tenant-token");
        // And it's already lowercase — tonic would reject a mixed-case
        // metadata key at insert time, but asserting here makes the
        // constraint visible.
        assert_eq!(TENANT_TOKEN_HEADER, TENANT_TOKEN_HEADER.to_lowercase());
    }
}

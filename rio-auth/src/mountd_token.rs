//! Scheduler-signed Ed25519 Mount-admission tokens (the `rmt2` envelope).
//!
//! The ADR-022 mount-admission credentials decision (the
//! `022-mountd-admission-credentials` chapter of the design book) replaces the
//! never-provisioned per-cluster symmetric `rio/mountd-hmac` scheme with an
//! asymmetric one: the scheduler holds the only signing key and signs a
//! small per-build claims blob at dispatch; every rio-mountd holds public
//! trust roots only and verifies offline. Compromising a builder node then
//! yields no credential-minting ability anywhere — the node-resident
//! material is public.
//!
//! This module is the credential library only (ADR Phase 1): key loading,
//! claims, envelope, sign, verify. Daemon/scheduler wiring, the node-name
//! flag, helm/ESO distribution, and the spec-rule updates land with the
//! later phases; nothing here changes the behavior of any running
//! component.
//!
//! # Envelope
//!
//! `rmt2.<base64url(claims_json)>.<base64url(ed25519_signature)>`
//!
//! The signature covers the transmitted bytes before the last dot
//! (`rmt2.<base64url(claims_json)>`), so the version tag is bound by the
//! signature and cannot be swapped to downgrade a token. Verification
//! never runs serde on unauthenticated bytes: split → decode signature →
//! try each trust root → only then parse claims → expiry → audience →
//! build_id → node.
//!
//! Legacy two-segment HMAC tokens (today's [`MountdClaims`] envelope) are
//! accepted only while an HMAC key is configured — see [`MountdVerifier`].
//! Production never configures one; the symmetric arm is deleted in the
//! final ADR phase.
//!
//! # Key material
//!
//! Same `name:base64` file encoding as the narinfo signing keypair, but
//! key names MUST carry the `rio-mountd-` prefix ([`MOUNTD_KEY_NAME_PREFIX`])
//! and the loaders hard-fail otherwise — that prefix check is what keeps a
//! narinfo key file from being cross-wired into the mountd trust chain (the
//! two formats are otherwise indistinguishable).
//!
//! - signing key (scheduler only): `rio-mountd-<n>:base64(64-byte ed25519
//!   keypair)` — the 32-byte seed-only form is also accepted.
//! - trust roots (every mountd): one `rio-mountd-<n>:base64(32-byte public
//!   key)` line per active key. More than one line is the rotation-overlap
//!   state: a token verifying under ANY listed root is valid, so the
//!   signer can be flipped to a new key while tokens signed by the old one
//!   are still in flight.

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::hmac::HmacKey;
// Re-exports: the legacy symmetric claims/verify entry points stay where
// they are (current callers import them from `crate::hmac`), but new code
// touching mountd admission should only need this module. The final ADR
// phase deletes the symmetric arm; until then the two families coexist
// here side by side.
pub use crate::hmac::{MOUNTD_TOKEN_AUDIENCE, MountdClaims, MountdTokenError};

/// Version tag of the Ed25519 mountd-token envelope. First dot-separated
/// segment of every [`MountdSigningKey::sign`] output; anything else is
/// not an `rmt2` token.
pub const MOUNTD_TOKEN_V2_PREFIX: &str = "rmt2";

/// Required prefix for mountd signing-key / trust-root names.
///
/// The key files share their `name:base64` encoding with the narinfo
/// signing keypair (`rio-store`'s `Signer`); the prefix is the structural
/// guard that a narinfo key (or any other `name:base64` file) pointed at a
/// mountd key path fails loudly at load time instead of silently joining
/// the mount-admission trust chain.
pub const MOUNTD_KEY_NAME_PREFIX: &str = "rio-mountd-";

/// Hard cap on the number of trust roots a verifier will load. Rotation
/// needs two (old + new), three covers an overlapping double-rotation;
/// more than eight is operator error (or an attempt to grow the
/// per-Mount verify loop), so the loader rejects it.
pub const MOUNTD_TRUST_ROOT_MAX: usize = 8;

/// Claims carried in an `rmt2` Mount-admission token.
///
/// The field set is today's [`MountdClaims`] (audience, build_id, tenant,
/// issued, expiry — same names, same JSON shape) plus the node-scoping
/// fields the ADR adds for the first asymmetric claims version:
///
/// - [`node`](Self::node): the kube `spec.nodeName` the token is valid
///   on. Carried and signature-covered from day one because the claims
///   are `deny_unknown_fields` — adding it after `rmt2` verifiers are
///   deployed would be a breaking claims change. How the scheduler
///   resolves it and how the daemon enforces it is later-phase wiring;
///   this module only checks it when the caller supplies an
///   `expected_node`.
/// - [`node_fp`](Self::node_fp): reserved for a future per-node identity
///   fingerprint. Never enforced here; carried so identity-keyed binding
///   can be added later without another claims-format break.
///
/// The legacy [`MountdClaims`] struct itself is left untouched in this
/// phase (its construction sites in the scheduler and the spike client
/// stay valid); [`From<MountdClaims>`](Self::from) gives the
/// claims-with-node view of a legacy token so verifiers hand callers one
/// type regardless of which envelope admitted the peer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MountdAdmissionClaims {
    /// Always [`MOUNTD_TOKEN_AUDIENCE`]. Anything else is rejected —
    /// belt-and-suspenders against a token minted for some other consumer
    /// being replayed at a mountd socket.
    pub aud: String,
    /// The mountd `build_id` this token authorizes — the exact string the
    /// builder sends in `Mount{build_id}`, i.e.
    /// [`MountdClaims::build_id_for_drv_path`] of the assignment's
    /// `drv_path`. Compared against the requested id on verify so a stolen
    /// token cannot claim (squat) a different build's id.
    pub build_id: String,
    /// Attributing tenant UUID (hyphenated), when the dispatch had one.
    /// Audit/debug only — mountd performs no tenant-scoped decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Unix seconds when the token was minted. Audit/debug only.
    pub issued_unix: u64,
    /// Unix seconds; token invalid after this. The scheduler mints with
    /// the assignment-token TTL so a mid-build re-Mount after a mountd
    /// restart stays within validity for the whole build.
    pub expiry_unix: u64,
    /// Kube `spec.nodeName` of the node this token admits a Mount on.
    /// Empty / absent ⇒ the token is unbound (the scheduler's `prefer`
    /// escape hatch); verifiers given an `expected_node` reject unbound
    /// tokens with [`MountdVerifyError::NodeMissing`].
    ///
    /// `skip_serializing_if` keeps an unbound token's claims JSON
    /// byte-identical to the legacy field set, and `default` lets a
    /// node-less claims body parse — "absent" and "empty" are the same
    /// state on both ends.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node: String,
    /// Reserved: fingerprint of a per-node identity key, for a future
    /// upgrade that binds tokens to a node identity rather than a node
    /// name. Carried opaquely and covered by the signature, but never
    /// inspected by any verifier in this phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_fp: Option<String>,
}

impl From<MountdClaims> for MountdAdmissionClaims {
    /// View a legacy (symmetric-scheme) claims blob as the superset type.
    /// Legacy tokens cannot carry a node binding, so `node` is empty —
    /// i.e. an unbound token — and `node_fp` is unset.
    fn from(c: MountdClaims) -> Self {
        Self {
            aud: c.aud,
            build_id: c.build_id,
            tenant: c.tenant,
            issued_unix: c.issued_unix,
            expiry_unix: c.expiry_unix,
            node: String::new(),
            node_fp: None,
        }
    }
}

/// A successfully verified Mount-admission token: the claims plus which
/// trust root vouched for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedMountdToken {
    /// The verified claims. For legacy HMAC tokens this is the
    /// [`From<MountdClaims>`](MountdAdmissionClaims#impl-From<MountdClaims>-for-MountdAdmissionClaims)
    /// view (no node binding).
    pub claims: MountdAdmissionClaims,
    /// Name of the trust root whose key verified the signature
    /// (`rio-mountd-<n>`), taken from the verifier's own list — never
    /// from attacker-controlled input. `None` for a token admitted by the
    /// legacy HMAC arm (a shared secret has no per-key name). The daemon
    /// logs this on success; it is the observable that makes trust-root
    /// rotation auditable.
    pub verified_by: Option<String>,
}

/// Why mountd key material (signing key or trust roots) failed to load.
///
/// Variants never carry key bytes — at most the offending key *name* and
/// the file path, which is what an operator needs to fix the file. A
/// configured-but-unreadable/empty/malformed file is a hard error by
/// design (fail closed at startup); only an *unset* path means "scheme
/// not enabled".
#[derive(Debug, thiserror::Error)]
pub enum MountdKeyError {
    /// Reading the key file failed.
    #[error("mountd key file I/O ({path}): {source}")]
    Io {
        /// Path the caller configured.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file was configured but contains no key entry (empty or only
    /// blank lines).
    #[error("mountd key file is empty")]
    Empty,
    /// An entry is not `name:base64` shaped (no `:` separator).
    #[error("mountd key entry format: expected 'name:base64', missing ':' separator")]
    MissingSeparator,
    /// The key name does not carry the required `rio-mountd-` prefix —
    /// most likely a narinfo (or other) keypair cross-wired into a mountd
    /// key path.
    #[error("mountd key name {name:?} must start with \"rio-mountd-\"")]
    BadKeyName {
        /// The offending name (the part before `:`; never the key bytes).
        name: String,
    },
    /// The base64 payload of an entry does not decode.
    #[error("mountd key base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
    /// The signing key decodes to neither a 64-byte keypair nor a 32-byte
    /// seed.
    #[error("mountd signing key must decode to 32 (seed) or 64 (seed+pubkey) bytes, got {got}")]
    SigningKeyLength {
        /// Decoded length found.
        got: usize,
    },
    /// The 64-byte form embeds a public key that does not match the one
    /// derived from the seed half — a corrupt or hand-spliced key file.
    #[error("mountd signing key: embedded public key does not match the seed")]
    SigningKeyMismatch,
    /// The signing-key file contains more than one entry. Unlike the
    /// trust-roots file (where multiple lines are the rotation-overlap
    /// state), the signer is exactly one key — rotation swaps the file
    /// content, it never appends. Most likely the trust-roots file (or a
    /// rotation overlap) was pointed at the signing-key path.
    #[error("mountd signing key file must contain exactly one 'rio-mountd-<n>:base64' entry")]
    SigningKeyMultipleEntries,
    /// A trust-root entry does not decode to exactly 32 bytes.
    #[error("mountd trust root {name:?} must be a 32-byte ed25519 public key, got {got} bytes")]
    TrustRootLength {
        /// Name of the offending root.
        name: String,
        /// Decoded length found.
        got: usize,
    },
    /// A trust-root entry is 32 bytes but not a valid ed25519 point.
    #[error("mountd trust root {name:?} is not a valid ed25519 public key")]
    TrustRootInvalid {
        /// Name of the offending root.
        name: String,
    },
    /// Two trust-root entries share a name. Names identify keys in logs
    /// and the rotation runbook; duplicates would make "which key
    /// verified" ambiguous, so the loader rejects them.
    #[error("duplicate mountd trust root name {name:?}")]
    DuplicateTrustRoot {
        /// The duplicated name.
        name: String,
    },
    /// More than [`MOUNTD_TRUST_ROOT_MAX`] roots configured.
    #[error("too many mountd trust roots: {count} (maximum {MOUNTD_TRUST_ROOT_MAX})")]
    TooManyTrustRoots {
        /// Number of entries found in the file.
        count: usize,
    },
}

/// Why a presented Mount-admission token does not authorize the requested
/// `Mount{}`.
///
/// Variants carry no key material and no full token/claims content — at
/// most a single offending byte/offset reported by the base64 decoder
/// ([`Self::Base64`]) or a claim field name reported by serde
/// ([`Self::Json`], reachable only after a trust root has verified the
/// signature). Same discipline as the legacy [`MountdTokenError`]: the
/// daemon logs the variant and replies with one opaque `Unauthorized` on
/// the wire.
#[derive(Debug, thiserror::Error)]
pub enum MountdVerifyError {
    /// Not an `rmt2.<claims>.<signature>` shape (wrong segment count or
    /// version tag). Carries the number of `.`-separated segments seen.
    #[error("mountd token format invalid: expected 'rmt2.<claims>.<signature>', got {0} segments")]
    Format(usize),
    /// A legacy two-segment token was presented but this verifier has no
    /// HMAC key configured. Production never configures one, so this is
    /// the expected rejection for every pre-`rmt2` token.
    #[error("legacy HMAC mountd token presented but no HMAC key is configured")]
    LegacyNotConfigured,
    /// An `rmt2` token was presented but this verifier has no Ed25519
    /// trust roots configured.
    #[error("rmt2 mountd token presented but no Ed25519 trust roots are configured")]
    TrustRootsNotConfigured,
    /// A base64url segment did not decode.
    #[error("mountd token base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
    /// The signature segment is not exactly 64 bytes.
    #[error("mountd token signature must be 64 bytes, got {0}")]
    SignatureLength(usize),
    /// No configured trust root verifies the signature: tampered token,
    /// a key the verifier does not trust, or not an Ed25519 signature at
    /// all. Deliberately not distinguished further.
    #[error("mountd token signature verification failed")]
    InvalidSignature,
    /// The (signature-verified) claims segment is not a valid
    /// [`MountdAdmissionClaims`] JSON body.
    #[error("mountd token claims decode failed: {0}")]
    Json(#[from] serde_json::Error),
    /// The token is past its expiry.
    #[error("mountd token expired (expiry={expiry_unix}, now={now_unix})")]
    Expired {
        /// Claimed expiry (unix seconds).
        expiry_unix: u64,
        /// Verifier's clock at check time (unix seconds).
        now_unix: u64,
    },
    /// The verifier's clock predates the unix epoch — expiry undecidable.
    #[error(transparent)]
    Clock(#[from] crate::ClockBeforeEpoch),
    /// The token verified but its audience is not
    /// [`MOUNTD_TOKEN_AUDIENCE`] — minted for some other consumer.
    #[error("mountd token audience mismatch")]
    Audience,
    /// The token verified but was minted for a different `build_id` than
    /// the one the Mount requests.
    #[error("mountd token build_id mismatch")]
    BuildIdMismatch,
    /// The verifier was given an expected node but the token carries no
    /// node claim (minted unbound).
    #[error("mountd token carries no node claim but a node binding is required")]
    NodeMissing,
    /// The verifier was given an expected node and the token names a
    /// different one — the cross-node replay the node claim exists to
    /// stop.
    #[error("mountd token node claim does not match this node")]
    NodeMismatch,
    /// A legacy two-segment token was presented and the configured HMAC
    /// key rejected it (signature/expiry/shape/audience/build_id — see
    /// [`MountdTokenError`]).
    #[error("legacy mountd token rejected: {0}")]
    Legacy(#[from] MountdTokenError),
}

/// Trim leading/trailing whitespace (trailing newlines included) from a
/// key-file payload. Same forgiveness as every other key loader in the
/// workspace: `echo` vs `echo -n` (or a Windows-edited Secret) must not
/// produce a different key.
fn trim_key_file(content: &str) -> &str {
    content.trim()
}

/// Split a `name:base64` entry and enforce the mountd name prefix.
fn split_named_entry(entry: &str) -> Result<(&str, &str), MountdKeyError> {
    let (name, b64) = entry
        .split_once(':')
        .ok_or(MountdKeyError::MissingSeparator)?;
    if !name.starts_with(MOUNTD_KEY_NAME_PREFIX) || name.len() <= MOUNTD_KEY_NAME_PREFIX.len() {
        return Err(MountdKeyError::BadKeyName {
            name: name.to_string(),
        });
    }
    Ok((name, b64))
}

/// The scheduler-side signing key for `rmt2` Mount-admission tokens.
///
/// Loaded once at startup from `RIO_MOUNTD_SIGNING_KEY_PATH` (later
/// phase); only the control plane ever holds it. Deliberately not
/// `Clone` and without a `Debug` impl — same hygiene as [`HmacKey`]:
/// no copies are handed out and there is no formatting path that could
/// land key material in a log line. The `ed25519_dalek::SigningKey`
/// inside zeroizes its secret half on drop; the transient file/base64
/// buffers in [`Self::load`]/[`Self::parse`] are dropped without
/// explicit zeroization, same as [`HmacKey::load`].
pub struct MountdSigningKey {
    /// Key name (`rio-mountd-<n>`), logged at mint time and echoed in
    /// [`Self::trust_root_entry`]. Never part of the token itself.
    key_name: String,
    key: SigningKey,
}

impl MountdSigningKey {
    /// Load from a key file. `None` path → `Ok(None)` (signing not
    /// enabled — the caller keeps minting nothing, exactly today's
    /// keyless behavior). A configured path that is unreadable, empty, or
    /// malformed is an error: fail closed at startup, never silently mint
    /// unverifiable tokens.
    ///
    /// File format: `rio-mountd-<n>:base64(64-byte ed25519 keypair)`,
    /// with the 32-byte seed-only form also accepted. Standard (RFC 4648)
    /// base64, matching the narinfo keypair files the bootstrap tooling
    /// already generates.
    pub fn load(path: Option<&std::path::Path>) -> Result<Option<Self>, MountdKeyError> {
        let Some(path) = path else {
            return Ok(None);
        };
        let content = std::fs::read_to_string(path).map_err(|source| MountdKeyError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(trim_key_file(&content)).map(Some)
    }

    /// Parse a `name:base64(secret)` string. Extracted from
    /// [`Self::load`] so tests and the keygen tooling can construct a
    /// signer without touching the filesystem.
    pub fn parse(content: &str) -> Result<Self, MountdKeyError> {
        let content = trim_key_file(content);
        if content.is_empty() {
            return Err(MountdKeyError::Empty);
        }
        // More than one line here is almost always the trust-roots file
        // (or a rotation overlap) misapplied to the signer half — say so,
        // instead of failing on the embedded newline with a cryptic
        // base64 "invalid byte 10" error. This is the file an operator
        // edits during rotation; the error has to name the actual fix.
        if content.lines().count() > 1 {
            return Err(MountdKeyError::SigningKeyMultipleEntries);
        }
        let (name, b64) = split_named_entry(content)?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
        let seed: [u8; 32] = match bytes.len() {
            // 64-byte form: seed (32) + embedded public key (32). The
            // embedded half is redundant — we derive the public key from
            // the seed — but a mismatch means the file was hand-spliced
            // or corrupted, and signing with the seed half of one key
            // while operators believe they deployed another is exactly
            // the kind of silent confusion the name-prefix check exists
            // to prevent. Validate it.
            64 => {
                let seed: [u8; 32] = bytes[..32]
                    .try_into()
                    .expect("slice of len-64 at [..32] is 32 bytes");
                let embedded: [u8; 32] = bytes[32..]
                    .try_into()
                    .expect("slice of len-64 at [32..] is 32 bytes");
                if SigningKey::from_bytes(&seed).verifying_key().to_bytes() != embedded {
                    return Err(MountdKeyError::SigningKeyMismatch);
                }
                seed
            }
            32 => bytes.as_slice().try_into().expect("checked len == 32"),
            other => return Err(MountdKeyError::SigningKeyLength { got: other }),
        };
        Ok(Self {
            key_name: name.to_string(),
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Construct from a raw 32-byte seed and a key name (tests, keygen).
    /// The name must carry the [`MOUNTD_KEY_NAME_PREFIX`] like every
    /// other constructor — the prefix is a property of the key family,
    /// not of the file format.
    pub fn from_seed(key_name: impl Into<String>, seed: &[u8; 32]) -> Result<Self, MountdKeyError> {
        let key_name = key_name.into();
        if !key_name.starts_with(MOUNTD_KEY_NAME_PREFIX)
            || key_name.len() <= MOUNTD_KEY_NAME_PREFIX.len()
        {
            return Err(MountdKeyError::BadKeyName { name: key_name });
        }
        Ok(Self {
            key_name,
            key: SigningKey::from_bytes(seed),
        })
    }

    /// The key name (`rio-mountd-<n>`), for the dispatch log line.
    pub fn key_name(&self) -> &str {
        &self.key_name
    }

    /// This key's `name:base64(pubkey)` trust-root line — what operators
    /// append to the trust-roots file so verifiers accept tokens signed
    /// by this key. Public material only.
    pub fn trust_root_entry(&self) -> String {
        let pk = self.key.verifying_key();
        format!(
            "{}:{}",
            self.key_name,
            base64::engine::general_purpose::STANDARD.encode(pk.to_bytes())
        )
    }

    /// This key's `name:base64(seed ‖ pubkey)` signing-key-file line —
    /// what the keygen tooling (the bootstrap Job's keypair block,
    /// `spike_mountd_client keygen` for VM tests/standalone) writes to
    /// the file [`Self::load`] reads. The 64-byte form is emitted so the
    /// embedded-pubkey consistency check applies on every load.
    ///
    /// This is the PRIVATE key serialization: callers must treat the
    /// returned string with the same care as the signing-key file
    /// itself (0600, scheduler/control-plane only, never logged).
    pub fn secret_key_entry(&self) -> String {
        let mut bytes = Vec::with_capacity(64);
        bytes.extend_from_slice(self.key.as_bytes());
        bytes.extend_from_slice(&self.key.verifying_key().to_bytes());
        format!(
            "{}:{}",
            self.key_name,
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        )
    }

    /// Sign claims into an `rmt2` token string.
    ///
    /// The signature is computed over the literal ASCII prefix
    /// `rmt2.<base64url(claims_json)>` — the same bytes the verifier
    /// receives before the last dot — so the version tag is covered and
    /// the verifier never has to re-serialize anything to check the
    /// signature.
    pub fn sign(&self, claims: &MountdAdmissionClaims) -> String {
        let claims_json = serde_json::to_vec(claims)
            .expect("MountdAdmissionClaims serialization can't fail (no non-string map keys)");
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let message = format!("{MOUNTD_TOKEN_V2_PREFIX}.{}", b64.encode(&claims_json));
        let signature = self.key.sign(message.as_bytes());
        format!("{message}.{}", b64.encode(signature.to_bytes()))
    }
}

/// The mountd-side trust roots: every public key whose signatures this
/// verifier accepts.
///
/// Loaded once at startup from `RIO_MOUNTD_PUBKEY_PATH` (later phase).
/// Multiple entries are the rotation-overlap state — verification tries
/// every root, so tokens signed by either the outgoing or the incoming
/// key stay valid until the operator drops the old line and restarts the
/// DaemonSet. Public material only; holding it mints nothing — this is
/// the only mountd-side credential material the rmt2 scheme ever places
/// on a builder/fetcher node.
// r[impl builder.mountd.token-no-node-mint]
#[derive(Clone)]
pub struct MountdTrustRoots {
    /// `(name, key)` in file order. Names are unique (load rejects
    /// duplicates) and prefix-checked.
    roots: Vec<(String, VerifyingKey)>,
}

impl MountdTrustRoots {
    /// Load from a trust-roots file. `None` path → `Ok(None)` (token
    /// verification not enabled). A configured path that is unreadable,
    /// empty, or malformed is an error — a daemon told to verify tokens
    /// but given no usable roots must fail at startup, not fall back to
    /// rejecting (or worse, admitting) silently.
    pub fn load(path: Option<&std::path::Path>) -> Result<Option<Self>, MountdKeyError> {
        let Some(path) = path else {
            return Ok(None);
        };
        let content = std::fs::read_to_string(path).map_err(|source| MountdKeyError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&content).map(Some)
    }

    /// Parse trust-roots file content: one `rio-mountd-<n>:base64(32-byte
    /// pubkey)` entry per line, blank lines ignored. Standard (RFC 4648)
    /// base64, same as the narinfo `trusted-public-keys` entries.
    pub fn parse(content: &str) -> Result<Self, MountdKeyError> {
        let mut roots: Vec<(String, VerifyingKey)> = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (name, b64) = split_named_entry(line)?;
            if roots.iter().any(|(n, _)| n == name) {
                return Err(MountdKeyError::DuplicateTrustRoot {
                    name: name.to_string(),
                });
            }
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
            let arr: [u8; 32] =
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| MountdKeyError::TrustRootLength {
                        name: name.to_string(),
                        got: bytes.len(),
                    })?;
            let key =
                VerifyingKey::from_bytes(&arr).map_err(|_| MountdKeyError::TrustRootInvalid {
                    name: name.to_string(),
                })?;
            roots.push((name.to_string(), key));
            if roots.len() > MOUNTD_TRUST_ROOT_MAX {
                return Err(MountdKeyError::TooManyTrustRoots {
                    count: content.lines().filter(|l| !l.trim().is_empty()).count(),
                });
            }
        }
        if roots.is_empty() {
            return Err(MountdKeyError::Empty);
        }
        Ok(Self { roots })
    }

    /// Names of the loaded roots, in file order — the label set for the
    /// trust-root gauge and the rotation runbook's "every mountd reports
    /// the new key" precondition.
    pub fn key_names(&self) -> impl Iterator<Item = &str> {
        self.roots.iter().map(|(n, _)| n.as_str())
    }

    /// Verify the envelope of an `rmt2` token: shape, signature (against
    /// every root), then claims decode. Claim-level checks (expiry,
    /// audience, build_id, node) belong to
    /// [`MountdVerifier::verify_for_build`], the only public entry point
    /// — keeping this private means no caller can accidentally accept a
    /// token on signature alone. Attacker-reachable via the rio-mountd
    /// UDS in token mode; fuzzed by `fuzz/rio-auth`'s
    /// `mountd_token_verify` target.
    fn verify_envelope(
        &self,
        token: &str,
    ) -> Result<(MountdAdmissionClaims, &str), MountdVerifyError> {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() != 3 || parts[0] != MOUNTD_TOKEN_V2_PREFIX {
            return Err(MountdVerifyError::Format(parts.len()));
        }
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        // Signature first — decode it and check it against the trust
        // roots before touching the claims segment. The signed message is
        // the literal transmitted bytes before the last dot (version tag
        // included), so nothing needs to be re-encoded to verify.
        let sig_bytes = b64.decode(parts[2])?;
        let sig_arr: [u8; 64] = sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| MountdVerifyError::SignatureLength(sig_bytes.len()))?;
        let signature = Signature::from_bytes(&sig_arr);
        let (message, _) = token
            .rsplit_once('.')
            .expect("3-segment token has at least one '.'");

        let Some(verified_by) = self.roots.iter().find_map(|(name, key)| {
            key.verify_strict(message.as_bytes(), &signature)
                .is_ok()
                .then_some(name.as_str())
        }) else {
            return Err(MountdVerifyError::InvalidSignature);
        };

        // Only now is the claims segment authenticated — safe to decode
        // and parse.
        let claims_json = b64.decode(parts[1])?;
        let claims: MountdAdmissionClaims = serde_json::from_slice(&claims_json)?;
        Ok((claims, verified_by))
    }
}

/// Which Mount-admission credential schemes this verifier accepts,
/// holding the key material for each.
///
/// Built from whichever of the two key paths are configured
/// ([`Self::from_parts`]); "neither" is not a variant — that is the
/// keyless gid-only mode, represented by the caller holding no verifier
/// at all. Not `Clone` (it can hold the symmetric secret) — share via
/// `Arc` like [`HmacKey`].
pub enum MountdVerifier {
    /// Legacy symmetric scheme only (today's deployed shape). Two-segment
    /// HMAC tokens are accepted; `rmt2` tokens are rejected with
    /// [`MountdVerifyError::TrustRootsNotConfigured`].
    Hmac(HmacKey),
    /// Ed25519 scheme only (the ADR's end state before the symmetric arm
    /// is deleted). `rmt2` tokens are accepted; legacy tokens are
    /// rejected with [`MountdVerifyError::LegacyNotConfigured`].
    Ed25519(MountdTrustRoots),
    /// Both configured — the contingency overlap for an operator already
    /// running the symmetric scheme (production never is; the keyless →
    /// Ed25519 cutover skips this state). Routing is by envelope shape,
    /// never by trying both keys.
    Dual {
        /// Key for the legacy two-segment arm.
        hmac: HmacKey,
        /// Roots for the `rmt2` arm.
        trust_roots: MountdTrustRoots,
    },
}

impl MountdVerifier {
    /// Combine independently loaded key material into a verifier.
    /// `(None, None)` → `None`: nothing configured, token mode stays off
    /// and the socket stays gid-gated exactly as today.
    pub fn from_parts(
        hmac: Option<HmacKey>,
        trust_roots: Option<MountdTrustRoots>,
    ) -> Option<Self> {
        match (hmac, trust_roots) {
            (None, None) => None,
            (Some(hmac), None) => Some(Self::Hmac(hmac)),
            (None, Some(trust_roots)) => Some(Self::Ed25519(trust_roots)),
            (Some(hmac), Some(trust_roots)) => Some(Self::Dual { hmac, trust_roots }),
        }
    }

    /// The Ed25519 trust roots, when this verifier has any — for the
    /// trust-root gauge and startup logging.
    pub fn trust_roots(&self) -> Option<&MountdTrustRoots> {
        match self {
            Self::Hmac(_) => None,
            Self::Ed25519(roots)
            | Self::Dual {
                trust_roots: roots, ..
            } => Some(roots),
        }
    }

    /// Verify `token` as a Mount-admission credential for `build_id`,
    /// optionally bound to `expected_node`. This is the single
    /// verification entry point: signature/scheme, expiry, audience,
    /// build_id and node checks all happen here, in that order.
    ///
    /// Routing is by envelope shape: a token starting with `rmt2.` is
    /// only ever checked against the Ed25519 trust roots, anything else
    /// only against the legacy HMAC key — whichever of the two is not
    /// configured rejects that family outright. Keys are never tried
    /// across schemes.
    ///
    /// `expected_node` is the verifier's own node name (`None` = no node
    /// binding enforced, the standalone/gid-990 posture). The node check
    /// applies to `rmt2` tokens only: an `rmt2` token must name exactly
    /// `expected_node` (unbound ⇒ [`MountdVerifyError::NodeMissing`],
    /// other node ⇒ [`MountdVerifyError::NodeMismatch`]). Legacy tokens
    /// have no node claim to check and are admitted by the legacy arm
    /// unchanged — that arm exists only for the never-deployed
    /// contingency overlap and is deleted in the final ADR phase.
    pub fn verify_for_build(
        &self,
        token: &str,
        build_id: &str,
        expected_node: Option<&str>,
    ) -> Result<VerifiedMountdToken, MountdVerifyError> {
        let is_rmt2 = token
            .split_once('.')
            .is_some_and(|(tag, _)| tag == MOUNTD_TOKEN_V2_PREFIX);

        if is_rmt2 {
            let Some(roots) = self.trust_roots() else {
                return Err(MountdVerifyError::TrustRootsNotConfigured);
            };
            let (claims, verified_by) = roots.verify_envelope(token)?;

            // Claim checks, in the ADR's order: expiry → audience →
            // build_id → node.
            let now_unix = crate::now_unix()?;
            if now_unix > claims.expiry_unix {
                return Err(MountdVerifyError::Expired {
                    expiry_unix: claims.expiry_unix,
                    now_unix,
                });
            }
            if claims.aud != MOUNTD_TOKEN_AUDIENCE {
                return Err(MountdVerifyError::Audience);
            }
            if claims.build_id != build_id {
                return Err(MountdVerifyError::BuildIdMismatch);
            }
            // r[impl builder.mountd.token-node-scoped]
            if let Some(expected) = expected_node {
                if claims.node.is_empty() {
                    return Err(MountdVerifyError::NodeMissing);
                }
                if claims.node != expected {
                    return Err(MountdVerifyError::NodeMismatch);
                }
            }
            let verified_by = verified_by.to_string();
            Ok(VerifiedMountdToken {
                claims,
                verified_by: Some(verified_by),
            })
        } else {
            let hmac = match self {
                Self::Hmac(hmac) | Self::Dual { hmac, .. } => hmac,
                Self::Ed25519(_) => return Err(MountdVerifyError::LegacyNotConfigured),
            };
            let claims = MountdClaims::verify(hmac, token, build_id)?;
            Ok(VerifiedMountdToken {
                claims: claims.into(),
                verified_by: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hmac::{
        AssignmentClaims, ExecutorClaims, HmacSigner, HmacVerifier, ServiceClaims, TokenRole,
    };

    const SEED_1: [u8; 32] = [0x41; 32];
    const SEED_2: [u8; 32] = [0x42; 32];
    const HMAC_TEST_KEY: &[u8] = b"mountd-test-token-key-32-bytes!!";

    fn now() -> u64 {
        crate::now_unix().expect("clock after epoch")
    }

    fn key(n: u32, seed: &[u8; 32]) -> MountdSigningKey {
        MountdSigningKey::from_seed(format!("rio-mountd-{n}"), seed).expect("valid name")
    }

    fn roots_of(keys: &[&MountdSigningKey]) -> MountdTrustRoots {
        let content = keys
            .iter()
            .map(|k| k.trust_root_entry())
            .collect::<Vec<_>>()
            .join("\n");
        MountdTrustRoots::parse(&content).expect("valid trust roots")
    }

    fn ed25519_verifier(keys: &[&MountdSigningKey]) -> MountdVerifier {
        MountdVerifier::Ed25519(roots_of(keys))
    }

    fn admission_claims(build_id: &str, node: &str, expiry_offset: i64) -> MountdAdmissionClaims {
        let now = now();
        MountdAdmissionClaims {
            aud: MOUNTD_TOKEN_AUDIENCE.into(),
            build_id: build_id.into(),
            tenant: Some("4f8a3c0e-0000-4000-8000-000000000001".into()),
            issued_unix: now,
            expiry_unix: (now as i64 + expiry_offset).max(0) as u64,
            node: node.into(),
            node_fp: None,
        }
    }

    fn legacy_claims(build_id: &str, expiry_offset: i64) -> MountdClaims {
        let now = now();
        MountdClaims {
            aud: MOUNTD_TOKEN_AUDIENCE.into(),
            build_id: build_id.into(),
            tenant: None,
            issued_unix: now,
            expiry_unix: (now as i64 + expiry_offset).max(0) as u64,
        }
    }

    /// Decode an rmt2 token's claims segment, mutate it, and reassemble
    /// the token with the ORIGINAL signature — the canonical tampering
    /// shape every "covered by the signature" assertion uses.
    fn tamper_claims(token: &str, mutate: impl FnOnce(&mut serde_json::Value)) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let parts: Vec<&str> = token.split('.').collect();
        let mut claims: serde_json::Value =
            serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
        mutate(&mut claims);
        let tampered = b64.encode(serde_json::to_vec(&claims).unwrap());
        format!("{}.{}.{}", parts[0], tampered, parts[2])
    }

    /// Sign an arbitrary claims-segment byte string with `signer`'s raw
    /// ed25519 key, producing a well-formed rmt2 envelope around it.
    /// Lets tests prove ordering properties (signature before parse) for
    /// bodies `MountdSigningKey::sign` would never produce.
    fn raw_rmt2(seed: &[u8; 32], claims_segment_bytes: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let message = format!("rmt2.{}", b64.encode(claims_segment_bytes));
        let sig = ed25519_dalek::SigningKey::from_bytes(seed).sign(message.as_bytes());
        format!("{message}.{}", b64.encode(sig.to_bytes()))
    }

    // ------------------------------------------------------------------
    // Round-trip + wire shape
    // ------------------------------------------------------------------

    #[test]
    fn rmt2_sign_verify_roundtrip() {
        let signer = key(1, &SEED_1);
        let verifier = ed25519_verifier(&[&signer]);
        let claims = admission_claims("b-alpha_drv", "node-a", 3600);
        let token = signer.sign(&claims);

        // Node-checking verifier (the k8s DaemonSet posture).
        let got = verifier
            .verify_for_build(&token, "b-alpha_drv", Some("node-a"))
            .expect("valid token verifies");
        assert_eq!(got.claims, claims);
        assert_eq!(got.verified_by.as_deref(), Some("rio-mountd-1"));

        // No expected node (standalone posture): same token, still valid.
        let got = verifier
            .verify_for_build(&token, "b-alpha_drv", None)
            .expect("valid token verifies without a node binding");
        assert_eq!(got.claims, claims);
    }

    /// Pins the envelope + claims wire shape: three segments, the rmt2
    /// tag, base64url payloads, and the field-elision rules that keep an
    /// unbound token's claims JSON identical to the legacy field set
    /// (no spurious `node`/`node_fp`/`tenant` keys).
    #[test]
    fn rmt2_wire_shape_pinned() {
        let signer = key(1, &SEED_1);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        // Key sets are compared SORTED so the assertions hold regardless
        // of serde_json's preserve_order feature (a workspace-level flag
        // this crate does not control).
        let sorted_keys = |json: &serde_json::Value| -> Vec<String> {
            let mut keys: Vec<String> = json.as_object().unwrap().keys().cloned().collect();
            keys.sort_unstable();
            keys
        };

        let bound = signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));
        let parts: Vec<&str> = bound.split('.').collect();
        assert_eq!(parts.len(), 3, "rmt2 token is three '.'-separated parts");
        assert_eq!(parts[0], "rmt2");
        let json: serde_json::Value =
            serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            sorted_keys(&json),
            [
                "aud",
                "build_id",
                "expiry_unix",
                "issued_unix",
                "node",
                "tenant"
            ],
            "bound token carries node"
        );
        assert!(
            !json.as_object().unwrap().contains_key("node_fp"),
            "unset node_fp must be elided"
        );
        // 64-byte ed25519 signature.
        assert_eq!(b64.decode(parts[2]).unwrap().len(), 64);

        // Unbound token (node empty, no tenant): the optional fields are
        // elided entirely — same key set as a legacy claims body.
        let mut unbound_claims = admission_claims("b-alpha_drv", "", 3600);
        unbound_claims.tenant = None;
        let unbound = signer.sign(&unbound_claims);
        let parts: Vec<&str> = unbound.split('.').collect();
        let json: serde_json::Value =
            serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            sorted_keys(&json),
            ["aud", "build_id", "expiry_unix", "issued_unix"],
            "unbound token carries exactly the legacy key set"
        );
        for elided in ["node", "node_fp", "tenant"] {
            assert!(
                !json.as_object().unwrap().contains_key(elided),
                "{elided}: empty/unset field must be elided"
            );
        }
    }

    /// `node_fp` is reserved: carried, signature-covered, round-tripped,
    /// but never enforced.
    #[test]
    fn rmt2_node_fp_reserved_roundtrip_unenforced() {
        let signer = key(1, &SEED_1);
        let verifier = ed25519_verifier(&[&signer]);
        let mut claims = admission_claims("b-alpha_drv", "node-a", 3600);
        claims.node_fp = Some("fp:abc123".into());
        let token = signer.sign(&claims);

        let got = verifier
            .verify_for_build(&token, "b-alpha_drv", Some("node-a"))
            .expect("node_fp is not enforced");
        assert_eq!(got.claims.node_fp.as_deref(), Some("fp:abc123"));
    }

    // ------------------------------------------------------------------
    // Expiry / wrong key / tampering
    // ------------------------------------------------------------------

    #[test]
    fn rmt2_expired_rejected() {
        let signer = key(1, &SEED_1);
        let verifier = ed25519_verifier(&[&signer]);
        let token = signer.sign(&admission_claims("b-alpha_drv", "node-a", -120));
        assert!(matches!(
            verifier.verify_for_build(&token, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::Expired { .. })
        ));
    }

    #[test]
    fn rmt2_wrong_key_rejected() {
        let signer = key(1, &SEED_1);
        let other = key(2, &SEED_2);
        // Verifier only trusts key 2; token is signed by key 1.
        let verifier = ed25519_verifier(&[&other]);
        let token = signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));
        assert!(matches!(
            verifier.verify_for_build(&token, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::InvalidSignature)
        ));
    }

    /// Claims are signature-covered: editing any field (build_id here,
    /// node in the dedicated test below) or the signature itself
    /// invalidates the token.
    #[test]
    fn rmt2_tampered_claims_or_signature_rejected() {
        let signer = key(1, &SEED_1);
        let verifier = ed25519_verifier(&[&signer]);
        let token = signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));

        // Tampered build_id (re-target the token at another build).
        let tampered = tamper_claims(&token, |c| {
            c["build_id"] = serde_json::Value::String("b-other_drv".into());
        });
        assert!(matches!(
            verifier.verify_for_build(&tampered, "b-other_drv", Some("node-a")),
            Err(MountdVerifyError::InvalidSignature)
        ));

        // Tampered expiry (extend a stolen token's lifetime).
        let tampered = tamper_claims(&token, |c| {
            c["expiry_unix"] = serde_json::Value::from(u64::MAX);
        });
        assert!(matches!(
            verifier.verify_for_build(&tampered, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::InvalidSignature)
        ));

        // Bit-flipped signature.
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let parts: Vec<&str> = token.split('.').collect();
        let mut sig = b64.decode(parts[2]).unwrap();
        sig[0] ^= 0xFF;
        let flipped = format!("{}.{}.{}", parts[0], parts[1], b64.encode(&sig));
        assert!(matches!(
            verifier.verify_for_build(&flipped, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::InvalidSignature)
        ));
    }

    /// The node claim is covered by the signature: a thief on node B
    /// cannot take a token scoped to node A and rewrite the binding.
    #[test]
    fn rmt2_node_claim_is_signature_covered() {
        let signer = key(1, &SEED_1);
        let verifier = ed25519_verifier(&[&signer]);
        let token = signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));

        let retargeted = tamper_claims(&token, |c| {
            c["node"] = serde_json::Value::String("node-b".into());
        });
        assert!(matches!(
            verifier.verify_for_build(&retargeted, "b-alpha_drv", Some("node-b")),
            Err(MountdVerifyError::InvalidSignature)
        ));

        // Stripping the node claim entirely is also tampering.
        let stripped = tamper_claims(&token, |c| {
            c.as_object_mut().unwrap().remove("node");
        });
        assert!(matches!(
            verifier.verify_for_build(&stripped, "b-alpha_drv", None),
            Err(MountdVerifyError::InvalidSignature)
        ));
    }

    // ------------------------------------------------------------------
    // Node matrix (match / mismatch / missing / no expected node)
    // ------------------------------------------------------------------

    // r[verify builder.mountd.token-node-scoped]
    #[test]
    fn rmt2_node_matrix() {
        let signer = key(1, &SEED_1);
        let verifier = ed25519_verifier(&[&signer]);
        let bound = signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));
        let unbound = signer.sign(&admission_claims("b-alpha_drv", "", 3600));

        // Match.
        assert!(
            verifier
                .verify_for_build(&bound, "b-alpha_drv", Some("node-a"))
                .is_ok()
        );
        // Mismatch — the cross-node replay this claim exists to stop.
        assert!(matches!(
            verifier.verify_for_build(&bound, "b-alpha_drv", Some("node-b")),
            Err(MountdVerifyError::NodeMismatch)
        ));
        // Unbound token presented to a node-checking verifier.
        assert!(matches!(
            verifier.verify_for_build(&unbound, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::NodeMissing)
        ));
        // No expected node: both bound and unbound tokens are admitted
        // (standalone / node-name-unset posture).
        assert!(
            verifier
                .verify_for_build(&bound, "b-alpha_drv", None)
                .is_ok()
        );
        assert!(
            verifier
                .verify_for_build(&unbound, "b-alpha_drv", None)
                .is_ok()
        );
    }

    // ------------------------------------------------------------------
    // Audience / build_id
    // ------------------------------------------------------------------

    #[test]
    fn rmt2_audience_and_build_id_mismatch_rejected() {
        let signer = key(1, &SEED_1);
        let verifier = ed25519_verifier(&[&signer]);

        let mut wrong_aud = admission_claims("b-alpha_drv", "node-a", 3600);
        wrong_aud.aud = "rio-store".into();
        let token = signer.sign(&wrong_aud);
        assert!(matches!(
            verifier.verify_for_build(&token, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::Audience)
        ));

        let token = signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));
        assert!(matches!(
            verifier.verify_for_build(&token, "b-other_drv", Some("node-a")),
            Err(MountdVerifyError::BuildIdMismatch)
        ));
    }

    // ------------------------------------------------------------------
    // Verification order + format rejections
    // ------------------------------------------------------------------

    /// "No serde on unauthenticated bytes": a claims segment that is not
    /// valid claims JSON only surfaces as a Json error when the signature
    /// over it is genuine; under a wrong/garbage signature the rejection
    /// is InvalidSignature — proof the signature check runs first.
    #[test]
    fn rmt2_signature_checked_before_claims_parse() {
        let signer = key(1, &SEED_1);
        let verifier = ed25519_verifier(&[&signer]);

        // Correctly signed, but the signed body is not claims JSON.
        let signed_garbage = raw_rmt2(&SEED_1, b"not json at all");
        assert!(matches!(
            verifier.verify_for_build(&signed_garbage, "b-alpha_drv", None),
            Err(MountdVerifyError::Json(_))
        ));

        // Same garbage body signed by a key outside the trust roots:
        // rejected at the signature step, the body is never parsed.
        let unsigned_garbage = raw_rmt2(&SEED_2, b"not json at all");
        assert!(matches!(
            verifier.verify_for_build(&unsigned_garbage, "b-alpha_drv", None),
            Err(MountdVerifyError::InvalidSignature)
        ));

        // Unknown extra claim field (deny_unknown_fields), correctly
        // signed → Json error, not silent acceptance.
        let mut extra = serde_json::to_value(admission_claims("b-alpha_drv", "node-a", 3600))
            .expect("claims serialize");
        extra["surprise"] = serde_json::Value::Bool(true);
        let extra_token = raw_rmt2(&SEED_1, &serde_json::to_vec(&extra).unwrap());
        assert!(matches!(
            verifier.verify_for_build(&extra_token, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::Json(_))
        ));
    }

    #[test]
    fn rmt2_malformed_envelope_rejected() {
        let signer = key(1, &SEED_1);
        let verifier = ed25519_verifier(&[&signer]);
        let valid = signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));
        let parts: Vec<&str> = valid.split('.').collect();

        // Two segments under the rmt2 tag.
        assert!(matches!(
            verifier.verify_for_build("rmt2.onlyonesegment", "b", None),
            Err(MountdVerifyError::Format(2))
        ));
        // Four segments.
        let four = format!("{valid}.extra");
        assert!(matches!(
            verifier.verify_for_build(&four, "b-alpha_drv", None),
            Err(MountdVerifyError::Format(4))
        ));
        // Signature segment is not base64url.
        let bad_sig_b64 = format!("{}.{}.{}", parts[0], parts[1], "!!!not-base64!!!");
        assert!(matches!(
            verifier.verify_for_build(&bad_sig_b64, "b-alpha_drv", None),
            Err(MountdVerifyError::Base64(_))
        ));
        // Signature decodes but is not 64 bytes.
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let short_sig = format!("{}.{}.{}", parts[0], parts[1], b64.encode([0u8; 32]));
        assert!(matches!(
            verifier.verify_for_build(&short_sig, "b-alpha_drv", None),
            Err(MountdVerifyError::SignatureLength(32))
        ));
    }

    // ------------------------------------------------------------------
    // Trust-root rotation / multi-root
    // ------------------------------------------------------------------

    /// Rotation overlap end to end: a verifier listing both the outgoing
    /// and incoming roots accepts tokens signed by either (and reports
    /// which); dropping the old root afterwards stops accepting its
    /// tokens while the new root keeps verifying.
    #[test]
    fn rmt2_trust_root_rotation_overlap() {
        let key1 = key(1, &SEED_1);
        let key2 = key(2, &SEED_2);
        let claims = admission_claims("b-alpha_drv", "node-a", 3600);
        let token1 = key1.sign(&claims);
        let token2 = key2.sign(&claims);

        // Before rotation: only key 1 trusted.
        let only_old = ed25519_verifier(&[&key1]);
        assert!(
            only_old
                .verify_for_build(&token1, "b-alpha_drv", Some("node-a"))
                .is_ok()
        );
        assert!(matches!(
            only_old.verify_for_build(&token2, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::InvalidSignature)
        ));

        // Overlap: both roots listed, both tokens valid, each attributed
        // to the root that actually signed it.
        let overlap = ed25519_verifier(&[&key1, &key2]);
        let got1 = overlap
            .verify_for_build(&token1, "b-alpha_drv", Some("node-a"))
            .expect("old key still valid during overlap");
        assert_eq!(got1.verified_by.as_deref(), Some("rio-mountd-1"));
        let got2 = overlap
            .verify_for_build(&token2, "b-alpha_drv", Some("node-a"))
            .expect("new key valid during overlap");
        assert_eq!(got2.verified_by.as_deref(), Some("rio-mountd-2"));

        // After rotation: old root removed, its tokens stop verifying.
        let only_new = ed25519_verifier(&[&key2]);
        assert!(matches!(
            only_new.verify_for_build(&token1, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::InvalidSignature)
        ));
        assert!(
            only_new
                .verify_for_build(&token2, "b-alpha_drv", Some("node-a"))
                .is_ok()
        );
    }

    // ------------------------------------------------------------------
    // Cross-scheme: legacy v1 ⇄ rmt2, same-key-bytes mix-ups
    // ------------------------------------------------------------------

    /// The two mountd schemes never verify each other's tokens — even
    /// when the same key bytes are mistakenly configured for both (an
    /// HMAC key file whose bytes equal the Ed25519 seed).
    // r[verify builder.mountd.token-no-node-mint]
    #[test]
    fn mountd_v1_and_v2_tokens_cross_rejected_even_with_same_key_bytes() {
        // Deliberate worst case: the HMAC secret IS the Ed25519 seed.
        let hmac_signer = HmacSigner::from_key(SEED_1.to_vec());
        let ed_signer = key(1, &SEED_1);

        let v1_token = hmac_signer.sign(&legacy_claims("b-alpha_drv", 3600));
        let v2_token = ed_signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));

        // v1 token → Ed25519-only verifier: rejected (no HMAC key
        // configured; the trust roots are never tried against it).
        let ed_only = ed25519_verifier(&[&ed_signer]);
        assert!(matches!(
            ed_only.verify_for_build(&v1_token, "b-alpha_drv", None),
            Err(MountdVerifyError::LegacyNotConfigured)
        ));

        // v2 token → HMAC-only verifier: rejected (no trust roots).
        let hmac_only = MountdVerifier::Hmac(HmacKey::from_key(SEED_1.to_vec()));
        assert!(matches!(
            hmac_only.verify_for_build(&v2_token, "b-alpha_drv", None),
            Err(MountdVerifyError::TrustRootsNotConfigured)
        ));

        // v2 token → the legacy verify path used by today's daemon:
        // three segments is not the legacy envelope.
        let hmac_verifier = HmacVerifier::from_key(SEED_1.to_vec());
        assert!(matches!(
            MountdClaims::verify(&hmac_verifier, &v2_token, "b-alpha_drv"),
            Err(MountdTokenError::Token(crate::hmac::HmacError::Format(3)))
        ));

        // A forged rmt2 envelope whose "signature" is an HMAC tag over
        // the message (what a confused signer holding only the symmetric
        // key could produce): rejected — an HMAC-SHA256 tag is 32 bytes,
        // not an ed25519 signature.
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let claims_json =
            serde_json::to_vec(&admission_claims("b-alpha_drv", "node-a", 3600)).unwrap();
        let message = format!("rmt2.{}", b64.encode(&claims_json));
        let two_part_hmac = hmac_signer.sign(&legacy_claims("ignored", 3600));
        let hmac_tag_b64 = two_part_hmac.split('.').next_back().unwrap();
        let forged = format!("{message}.{hmac_tag_b64}");
        assert!(matches!(
            ed_only.verify_for_build(&forged, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::SignatureLength(32))
        ));
    }

    /// The Dual contingency verifier accepts both envelopes, routing by
    /// shape: legacy tokens go to the HMAC arm (no node check — they
    /// cannot carry one), rmt2 tokens to the trust roots — and neither
    /// arm's key material can be smuggled across to the other envelope.
    #[test]
    fn dual_verifier_accepts_both_schemes_during_overlap() {
        let hmac_key_bytes = HMAC_TEST_KEY.to_vec();
        let ed_signer = key(1, &SEED_1);
        let hmac_signer = HmacSigner::from_key(hmac_key_bytes.clone());
        let dual = MountdVerifier::from_parts(
            Some(HmacKey::from_key(hmac_key_bytes)),
            Some(roots_of(&[&ed_signer])),
        )
        .expect("both parts configured");

        let v1_token = hmac_signer.sign(&legacy_claims("b-alpha_drv", 3600));
        let got = dual
            .verify_for_build(&v1_token, "b-alpha_drv", Some("node-a"))
            .expect("legacy token admitted by the HMAC arm");
        assert_eq!(got.verified_by, None);
        assert_eq!(got.claims.node, "", "legacy tokens are unbound");

        let v2_token = ed_signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));
        let got = dual
            .verify_for_build(&v2_token, "b-alpha_drv", Some("node-a"))
            .expect("rmt2 token admitted by the trust roots");
        assert_eq!(got.verified_by.as_deref(), Some("rio-mountd-1"));

        // Wrong-key rmt2 token is still rejected — the HMAC arm is never
        // consulted for an rmt2-shaped token.
        let stranger = key(9, &SEED_2);
        let foreign = stranger.sign(&admission_claims("b-alpha_drv", "node-a", 3600));
        assert!(matches!(
            dual.verify_for_build(&foreign, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::InvalidSignature)
        ));

        // A forged rmt2 envelope whose third segment is a real HMAC tag,
        // computed with the dual verifier's own HMAC key over the exact
        // bytes an Ed25519 signature would cover. Routing by envelope
        // means the legacy arm is never consulted for it, and the trust
        // roots reject the 32-byte tag — holding the symmetric key never
        // mints an rmt2 credential, even where both arms are configured.
        use hmac::{KeyInit as _, Mac as _};
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let claims_json =
            serde_json::to_vec(&admission_claims("b-alpha_drv", "node-a", 3600)).unwrap();
        let message = format!("rmt2.{}", b64.encode(&claims_json));
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(HMAC_TEST_KEY)
            .expect("any key length works");
        mac.update(message.as_bytes());
        let forged = format!("{message}.{}", b64.encode(mac.finalize().into_bytes()));
        assert!(matches!(
            dual.verify_for_build(&forged, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::SignatureLength(32))
        ));

        // The reverse smuggling direction: a legacy two-segment token
        // whose claims body carries a `node` field (what a confused
        // signer bolting node scoping onto the symmetric scheme would
        // mint). The legacy arm's `deny_unknown_fields` claims shape
        // rejects it — node scoping exists only in the rmt2 scheme.
        #[derive(Serialize, Deserialize)]
        struct LegacyClaimsWithNode {
            aud: String,
            build_id: String,
            issued_unix: u64,
            expiry_unix: u64,
            node: String,
        }
        impl crate::hmac::HmacClaims for LegacyClaimsWithNode {
            fn expiry_unix(&self) -> u64 {
                self.expiry_unix
            }
        }
        let node_bearing_legacy = hmac_signer.sign(&LegacyClaimsWithNode {
            aud: MOUNTD_TOKEN_AUDIENCE.into(),
            build_id: "b-alpha_drv".into(),
            issued_unix: now(),
            expiry_unix: now() + 3600,
            node: "node-a".into(),
        });
        assert!(matches!(
            dual.verify_for_build(&node_bearing_legacy, "b-alpha_drv", Some("node-a")),
            Err(MountdVerifyError::Legacy(MountdTokenError::Token(
                crate::hmac::HmacError::Json(_)
            )))
        ));
    }

    // ------------------------------------------------------------------
    // Cross-family: assignment / service / executor / tenant JWT
    // ------------------------------------------------------------------

    /// No other token family is a Mount-admission credential and the
    /// Mount-admission token satisfies none of their verifiers — in both
    /// directions, even when every family is signed with the same key
    /// bytes (the worst-case operational mix-up).
    #[test]
    fn rmt2_and_other_claim_families_mutually_unverifiable() {
        let shared_key = HMAC_TEST_KEY.to_vec();
        let hmac_signer = HmacSigner::from_key(shared_key.clone());
        let hmac_verifier = HmacVerifier::from_key(shared_key.clone());
        let ed_signer = key(1, &SEED_1);

        let assignment_token = hmac_signer.sign(&AssignmentClaims {
            executor_id: "w".into(),
            drv_hash: "abc123".into(),
            expected_outputs: vec!["/nix/store/aaa-x".into()],
            is_ca: false,
            expiry_unix: u64::MAX,
            tenant: None,
            role: TokenRole::Builder,
            input_closure_digest: String::new(),
        });
        let service_token = hmac_signer.sign(&ServiceClaims {
            caller: "rio-gateway".into(),
            expiry_unix: u64::MAX,
        });
        let executor_token = hmac_signer.sign(&ExecutorClaims {
            intent_id: "abc123".into(),
            kind: 0,
            expiry_unix: u64::MAX,
        });
        let rmt2_token = ed_signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));

        // Other-family tokens never admit a Mount, under either verifier
        // shape. Ed25519-only: rejected without an HMAC key. Dual (same
        // HMAC key bytes those tokens were signed with!): the legacy arm
        // parses them and rejects on claims shape.
        let ed_only = ed25519_verifier(&[&ed_signer]);
        let dual = MountdVerifier::from_parts(
            Some(HmacKey::from_key(shared_key)),
            Some(roots_of(&[&ed_signer])),
        )
        .expect("dual");
        for foreign in [&assignment_token, &service_token, &executor_token] {
            assert!(matches!(
                ed_only.verify_for_build(foreign, "b-alpha_drv", None),
                Err(MountdVerifyError::LegacyNotConfigured)
            ));
            assert!(matches!(
                dual.verify_for_build(foreign, "b-alpha_drv", None),
                Err(MountdVerifyError::Legacy(MountdTokenError::Token(
                    crate::hmac::HmacError::Json(_)
                )))
            ));
        }

        // The rmt2 token satisfies none of the other families' verifiers:
        // its envelope is not the two-segment HMAC shape at all.
        assert!(matches!(
            hmac_verifier.verify::<AssignmentClaims>(&rmt2_token),
            Err(crate::hmac::HmacError::Format(3))
        ));
        assert!(matches!(
            hmac_verifier.verify::<ServiceClaims>(&rmt2_token),
            Err(crate::hmac::HmacError::Format(3))
        ));
        assert!(matches!(
            hmac_verifier.verify::<ExecutorClaims>(&rmt2_token),
            Err(crate::hmac::HmacError::Format(3))
        ));
    }

    /// Tenant JWTs are also ed25519-signed three-segment tokens — the
    /// nearest-neighbour confusion. Even when the SAME ed25519 key signs
    /// both families, neither verifies as the other: the JWT's first
    /// segment is its header (not the rmt2 tag) and the rmt2 signature
    /// domain (`rmt2.<claims>`) is not a JWT signing input.
    #[test]
    fn rmt2_and_tenant_jwt_mutually_unverifiable_even_with_same_key() {
        let ed_signer = key(1, &SEED_1);
        let dalek_key = ed25519_dalek::SigningKey::from_bytes(&SEED_1);

        let jwt = crate::jwt::sign(
            &crate::jwt::TenantClaims {
                sub: uuid::Uuid::from_u128(1),
                iat: now() as i64,
                exp: now() as i64 + 3600,
                jti: "test-jti".into(),
            },
            &dalek_key,
        )
        .expect("jwt signs");
        let rmt2_token = ed_signer.sign(&admission_claims("b-alpha_drv", "node-a", 3600));

        // JWT → mountd verifier trusting the very key that signed it.
        let verifier = ed25519_verifier(&[&ed_signer]);
        assert!(
            verifier
                .verify_for_build(&jwt, "b-alpha_drv", None)
                .is_err(),
            "a tenant JWT must never admit a Mount"
        );

        // rmt2 token → JWT verification under the same public key.
        assert!(
            crate::jwt::verify(&rmt2_token, &dalek_key.verifying_key()).is_err(),
            "a Mount-admission token must never pass tenant-JWT verification"
        );
    }

    // ------------------------------------------------------------------
    // Key loading: signing key
    // ------------------------------------------------------------------

    #[test]
    fn signing_key_load_matrix() {
        let std_b64 = base64::engine::general_purpose::STANDARD;
        let dalek = ed25519_dalek::SigningKey::from_bytes(&SEED_1);
        let pubkey = dalek.verifying_key().to_bytes();

        // 32-byte seed-only form.
        let seed_only = format!("rio-mountd-1:{}", std_b64.encode(SEED_1));
        let k = MountdSigningKey::parse(&seed_only).expect("seed-only form accepted");
        assert_eq!(k.key_name(), "rio-mountd-1");

        // 64-byte seed+pubkey form (what keygen emits), via a tempfile
        // with a trailing newline — the loader trims it.
        let mut keypair = Vec::with_capacity(64);
        keypair.extend_from_slice(&SEED_1);
        keypair.extend_from_slice(&pubkey);
        let full = format!("rio-mountd-1:{}\n", std_b64.encode(&keypair));
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &full).unwrap();
        let k = MountdSigningKey::load(Some(tmp.path()))
            .expect("loads")
            .expect("present");
        // Both forms derive the same public key, and a token signed by
        // the file-loaded key verifies under the seed-built roots.
        assert_eq!(k.trust_root_entry(), key(1, &SEED_1).trust_root_entry());
        let token = k.sign(&admission_claims("b-x", "node-a", 3600));
        assert!(
            ed25519_verifier(&[&key(1, &SEED_1)])
                .verify_for_build(&token, "b-x", Some("node-a"))
                .is_ok()
        );

        // None path → not configured.
        assert!(MountdSigningKey::load(None).unwrap().is_none());
        // Missing file → Io.
        assert!(matches!(
            MountdSigningKey::load(Some(std::path::Path::new("/nonexistent/mountd.key"))),
            Err(MountdKeyError::Io { .. })
        ));

        // Rejections: empty, no separator, bad prefix, bad base64, wrong
        // length, spliced keypair (embedded pubkey from another key).
        assert!(matches!(
            MountdSigningKey::parse(""),
            Err(MountdKeyError::Empty)
        ));
        assert!(matches!(
            MountdSigningKey::parse("no-separator-here"),
            Err(MountdKeyError::MissingSeparator)
        ));
        let narinfo_named = format!("cache.example.org-1:{}", std_b64.encode(SEED_1));
        assert!(matches!(
            MountdSigningKey::parse(&narinfo_named),
            Err(MountdKeyError::BadKeyName { .. })
        ));
        // Prefix alone (empty suffix) is not a usable key name either.
        let bare_prefix = format!("rio-mountd-:{}", std_b64.encode(SEED_1));
        assert!(matches!(
            MountdSigningKey::parse(&bare_prefix),
            Err(MountdKeyError::BadKeyName { .. })
        ));
        assert!(matches!(
            MountdSigningKey::parse("rio-mountd-1:!!!not-base64!!!"),
            Err(MountdKeyError::Base64(_))
        ));
        let short = format!("rio-mountd-1:{}", std_b64.encode([0u8; 16]));
        assert!(matches!(
            MountdSigningKey::parse(&short),
            Err(MountdKeyError::SigningKeyLength { got: 16 })
        ));
        let mut spliced = Vec::with_capacity(64);
        spliced.extend_from_slice(&SEED_1);
        spliced.extend_from_slice(
            &ed25519_dalek::SigningKey::from_bytes(&SEED_2)
                .verifying_key()
                .to_bytes(),
        );
        let spliced = format!("rio-mountd-1:{}", std_b64.encode(&spliced));
        assert!(matches!(
            MountdSigningKey::parse(&spliced),
            Err(MountdKeyError::SigningKeyMismatch)
        ));

        // A multi-entry file (a trust-roots file, or a misguided attempt
        // at rotation overlap on the signer side) gets the dedicated
        // actionable error, not a cryptic base64 failure on the newline.
        let two_entries = format!(
            "rio-mountd-1:{}\nrio-mountd-2:{}",
            std_b64.encode(SEED_1),
            std_b64.encode(SEED_2)
        );
        assert!(matches!(
            MountdSigningKey::parse(&two_entries),
            Err(MountdKeyError::SigningKeyMultipleEntries)
        ));

        // from_seed enforces the same name discipline.
        assert!(matches!(
            MountdSigningKey::from_seed("narinfo-1", &SEED_1),
            Err(MountdKeyError::BadKeyName { .. })
        ));
    }

    // ------------------------------------------------------------------
    // Key loading: trust roots
    // ------------------------------------------------------------------

    #[test]
    fn trust_roots_load_matrix() {
        let std_b64 = base64::engine::general_purpose::STANDARD;
        let entry = |n: u32, seed: &[u8; 32]| key(n, seed).trust_root_entry();

        // Single root, trailing newline, via a real file.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), format!("{}\n", entry(1, &SEED_1))).unwrap();
        let roots = MountdTrustRoots::load(Some(tmp.path()))
            .expect("loads")
            .expect("present");
        assert_eq!(roots.key_names().collect::<Vec<_>>(), ["rio-mountd-1"]);

        // Multiple roots with blank lines between them (rotation overlap
        // file as an operator would actually write it).
        let multi = format!("{}\n\n{}\n", entry(1, &SEED_1), entry(2, &SEED_2));
        let roots = MountdTrustRoots::parse(&multi).expect("two roots parse");
        assert_eq!(
            roots.key_names().collect::<Vec<_>>(),
            ["rio-mountd-1", "rio-mountd-2"]
        );

        // None path → not configured; missing file → Io; empty /
        // blank-only files → Empty (configured-but-empty fails closed).
        assert!(MountdTrustRoots::load(None).unwrap().is_none());
        assert!(matches!(
            MountdTrustRoots::load(Some(std::path::Path::new("/nonexistent/mountd.pub"))),
            Err(MountdKeyError::Io { .. })
        ));
        assert!(matches!(
            MountdTrustRoots::parse(""),
            Err(MountdKeyError::Empty)
        ));
        assert!(matches!(
            MountdTrustRoots::parse("\n\n"),
            Err(MountdKeyError::Empty)
        ));

        // Duplicate names rejected (which key verified would be
        // ambiguous in logs and the rotation gauge).
        let dup = format!("{}\n{}", entry(1, &SEED_1), entry(1, &SEED_2));
        assert!(matches!(
            MountdTrustRoots::parse(&dup),
            Err(MountdKeyError::DuplicateTrustRoot { .. })
        ));

        // Name-prefix discipline: a narinfo public key cannot join the
        // mountd trust chain.
        let narinfo = format!(
            "cache.example.org-1:{}",
            std_b64.encode(
                ed25519_dalek::SigningKey::from_bytes(&SEED_1)
                    .verifying_key()
                    .to_bytes()
            )
        );
        assert!(matches!(
            MountdTrustRoots::parse(&narinfo),
            Err(MountdKeyError::BadKeyName { .. })
        ));

        // Wrong key size (a 64-byte SECRET pasted where the pubkey goes
        // must not load).
        let mut secret64 = Vec::with_capacity(64);
        secret64.extend_from_slice(&SEED_1);
        secret64.extend_from_slice(
            &ed25519_dalek::SigningKey::from_bytes(&SEED_1)
                .verifying_key()
                .to_bytes(),
        );
        let secret_as_root = format!("rio-mountd-1:{}", std_b64.encode(&secret64));
        assert!(matches!(
            MountdTrustRoots::parse(&secret_as_root),
            Err(MountdKeyError::TrustRootLength { got: 64, .. })
        ));

        // Hard cap: nine roots is operator error.
        let nine = (1..=9)
            .map(|i| entry(i, &[i as u8; 32]))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(matches!(
            MountdTrustRoots::parse(&nine),
            Err(MountdKeyError::TooManyTrustRoots { count: 9 })
        ));
        // Eight is the maximum allowed.
        let eight = (1..=8)
            .map(|i| entry(i, &[i as u8; 32]))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            MountdTrustRoots::parse(&eight).unwrap().key_names().count(),
            8
        );
    }

    // ------------------------------------------------------------------
    // Verifier assembly
    // ------------------------------------------------------------------

    #[test]
    fn from_parts_combinations() {
        let hmac = || HmacKey::from_key(HMAC_TEST_KEY.to_vec());
        let roots = || roots_of(&[&key(1, &SEED_1)]);

        assert!(MountdVerifier::from_parts(None, None).is_none());

        let v = MountdVerifier::from_parts(Some(hmac()), None).expect("hmac-only");
        assert!(matches!(v, MountdVerifier::Hmac(_)));
        assert!(v.trust_roots().is_none());

        let v = MountdVerifier::from_parts(None, Some(roots())).expect("ed25519-only");
        assert!(matches!(v, MountdVerifier::Ed25519(_)));
        assert_eq!(
            v.trust_roots().unwrap().key_names().collect::<Vec<_>>(),
            ["rio-mountd-1"]
        );

        let v = MountdVerifier::from_parts(Some(hmac()), Some(roots())).expect("dual");
        assert!(matches!(v, MountdVerifier::Dual { .. }));
        assert_eq!(
            v.trust_roots().unwrap().key_names().collect::<Vec<_>>(),
            ["rio-mountd-1"]
        );
    }

    // ------------------------------------------------------------------
    // Property tests
    // ------------------------------------------------------------------

    proptest::proptest! {
        /// Sign→verify round-trip over arbitrary claim contents and
        /// seeds. Expiry is always future; the dedicated unit test covers
        /// rejection. Mirrors the jwt.rs proptest discipline: keys are
        /// built from generated seeds, never `generate()`, so shrinking
        /// is meaningful.
        #[test]
        fn prop_rmt2_roundtrip(
            build_id in "[A-Za-z0-9_-]{1,64}",
            node in proptest::option::of("[a-z0-9.-]{1,40}"),
            tenant in proptest::option::of("[a-f0-9-]{8,36}"),
            expiry_delta in 60u64..86_400,
            seed: [u8; 32],
        ) {
            let signer = MountdSigningKey::from_seed("rio-mountd-prop", &seed).expect("name ok");
            let now = crate::now_unix().expect("clock after epoch");
            let claims = MountdAdmissionClaims {
                aud: MOUNTD_TOKEN_AUDIENCE.into(),
                build_id: build_id.clone(),
                tenant,
                issued_unix: now,
                expiry_unix: now + expiry_delta,
                node: node.clone().unwrap_or_default(),
                node_fp: None,
            };
            let token = signer.sign(&claims);
            let verifier = MountdVerifier::Ed25519(
                MountdTrustRoots::parse(&signer.trust_root_entry()).expect("roots parse"),
            );
            let got = verifier
                .verify_for_build(&token, &build_id, node.as_deref())
                .expect("round-trip verifies");
            proptest::prop_assert_eq!(got.claims, claims);
            proptest::prop_assert_eq!(got.verified_by.as_deref(), Some("rio-mountd-prop"));
        }

        /// A token signed under seed A never verifies under trust roots
        /// built from seed B ≠ A.
        #[test]
        fn prop_rmt2_wrong_key_always_rejected(seed_a: [u8; 32], seed_b: [u8; 32]) {
            proptest::prop_assume!(seed_a != seed_b);
            let signer = MountdSigningKey::from_seed("rio-mountd-a", &seed_a).expect("name ok");
            let other = MountdSigningKey::from_seed("rio-mountd-b", &seed_b).expect("name ok");
            let token = signer.sign(&admission_claims("b-prop", "node-a", 3600));
            let verifier = MountdVerifier::Ed25519(
                MountdTrustRoots::parse(&other.trust_root_entry()).expect("roots parse"),
            );
            proptest::prop_assert!(matches!(
                verifier.verify_for_build(&token, "b-prop", Some("node-a")),
                Err(MountdVerifyError::InvalidSignature)
            ));
        }
    }
}

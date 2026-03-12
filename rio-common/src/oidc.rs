//! OIDC token validation for external push authentication.
//!
//! Validates JWT tokens from OIDC providers (e.g., GitHub Actions) as an
//! alternative to HMAC assignment tokens. Each provider is configured with
//! an issuer URL, expected audience, and optional bound claims that must
//! match (e.g., `repository_owner = "myorg"`).
//!
//! JWKS keys are fetched from the provider's `.well-known/openid-configuration`
//! endpoint and cached for 1 hour. On signature verification failure or unknown
//! `kid`, the JWKS cache is force-refreshed once to handle key rotation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// How long to cache JWKS keys before re-fetching.
const JWKS_CACHE_TTL: Duration = Duration::from_secs(3600);

/// Minimum interval between forced JWKS refreshes per issuer.
/// Prevents attacker-triggered refresh floods via fake `kid` values.
const JWKS_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);

/// OIDC provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcProvider {
    /// The issuer URL (e.g., `https://token.actions.githubusercontent.com`).
    /// Must match the `iss` claim in the JWT exactly.
    pub issuer: String,
    /// Expected `aud` claim value.
    pub audience: String,
    /// Additional claims that must match exactly. Keys are claim names,
    /// values are expected string values. For GitHub Actions, typical
    /// bound claims include `repository_owner`.
    #[serde(default)]
    pub bound_claims: HashMap<String, String>,
}

/// Verified claims extracted from a valid OIDC token.
///
/// Fields are private to ensure instances can only be constructed by
/// `OidcVerifier::verify()` -- preventing forgery elsewhere in the codebase.
#[derive(Debug, Clone)]
pub struct OidcClaims {
    issuer: String,
    subject: String,
    extra: HashMap<String, serde_json::Value>,
}

impl OidcClaims {
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn claim(&self, key: &str) -> Option<&serde_json::Value> {
        self.extra.get(key)
    }
}

/// Errors from OIDC token validation.
#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("no configured provider matches issuer {0:?}")]
    UnknownIssuer(String),
    #[error("failed to fetch JWKS from {url}: {source}")]
    JwksFetch { url: String, source: reqwest::Error },
    #[error("OIDC discovery failed for {issuer}: {reason}")]
    Discovery { issuer: String, reason: String },
    #[error("JWT validation failed: {0}")]
    Validation(#[from] jsonwebtoken::errors::Error),
    #[error("bound claim {key:?} mismatch: expected {expected:?}, got {got:?}")]
    BoundClaimMismatch {
        key: String,
        expected: String,
        got: String,
    },
    #[error("bound claim {key:?} missing from token")]
    BoundClaimMissing { key: String },
    #[error("bound claim {key:?} is not a string")]
    BoundClaimNotString { key: String },
    #[error("JWT header missing kid")]
    MissingKid,
    #[error("no matching key found for kid {0:?}")]
    KeyNotFound(String),
    #[error("JWT missing required 'sub' claim")]
    MissingSubject,
    #[error("invalid OIDC provider config: {0}")]
    InvalidConfig(String),
}

/// JWKS key set response from the provider.
#[derive(Debug, Clone, Deserialize)]
struct JwksResponse {
    keys: Vec<JwkKey>,
}

/// A single JWK key. Fields match the JWK spec (RFC 7517).
/// `crv` is deserialized to tolerate spec-compliant JWKS responses
/// but is currently unused -- EC curve selection is delegated to
/// `DecodingKey::from_ec_components`.
#[derive(Debug, Clone, Deserialize)]
struct JwkKey {
    kid: Option<String>,
    kty: String,
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
    x: Option<String>,
    y: Option<String>,
    #[allow(dead_code)]
    crv: Option<String>,
}

/// OpenID Connect discovery document.
#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    jwks_uri: String,
}

/// Cached JWKS for a single issuer.
struct CachedJwks {
    keys: Vec<JwkKey>,
    fetched_at: Instant,
}

/// OIDC token verifier. Thread-safe, caches JWKS per issuer.
pub struct OidcVerifier {
    providers: Vec<OidcProvider>,
    http: reqwest::Client,
    /// JWKS cache keyed by issuer URL.
    jwks_cache: RwLock<HashMap<String, CachedJwks>>,
}

impl OidcVerifier {
    /// Create a new verifier for the given providers.
    ///
    /// Returns `None` if providers is empty (no OIDC configured).
    /// Returns `Err` if any provider has an empty issuer or audience.
    pub fn new(providers: Vec<OidcProvider>) -> Result<Option<Arc<Self>>, OidcError> {
        if providers.is_empty() {
            return Ok(None);
        }
        for p in &providers {
            if p.issuer.is_empty() {
                return Err(OidcError::InvalidConfig(
                    "provider has empty issuer URL".into(),
                ));
            }
            if p.audience.is_empty() {
                return Err(OidcError::InvalidConfig(
                    "provider has empty audience".into(),
                ));
            }
            for (key, val) in &p.bound_claims {
                if val.is_empty() {
                    return Err(OidcError::InvalidConfig(format!(
                        "bound claim {key:?} has empty expected value"
                    )));
                }
            }
            info!(issuer = %p.issuer, audience = %p.audience, "OIDC provider configured");
        }
        Ok(Some(Arc::new(Self {
            providers,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .connect_timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client builder with static config cannot fail"),
            jwks_cache: RwLock::new(HashMap::new()),
        })))
    }

    /// Validate an OIDC JWT token.
    ///
    /// 1. Decode header to get `kid` and peek at unverified `iss`
    /// 2. Find matching provider by issuer
    /// 3. Fetch/cache JWKS, find key by `kid`
    /// 4. Full JWT validation (signature, expiry, audience, issuer)
    /// 5. Check bound claims
    /// 6. On InvalidSignature or unknown `kid`, force-refresh JWKS once and retry
    pub async fn verify(&self, token: &str) -> Result<OidcClaims, OidcError> {
        let header = decode_header(token)?;
        let kid = header.kid.ok_or(OidcError::MissingKid)?;

        // Peek at unverified claims to find the issuer. Disable all
        // validation -- this is just to extract `iss` for provider lookup.
        // Real validation (sig, exp, aud, iss) happens in verify_with_jwks.
        let unverified: GenericClaims = {
            let mut no_verify = Validation::new(header.alg);
            no_verify.insecure_disable_signature_validation();
            no_verify.validate_aud = false;
            no_verify.validate_exp = false;
            no_verify.required_spec_claims.clear();
            let data = decode::<GenericClaims>(token, &DecodingKey::from_secret(&[]), &no_verify)?;
            data.claims
        };

        let issuer = unverified
            .iss
            .as_deref()
            .ok_or_else(|| OidcError::UnknownIssuer("(missing iss claim)".into()))?;

        let provider = self
            .providers
            .iter()
            .find(|p| p.issuer == issuer)
            .ok_or_else(|| OidcError::UnknownIssuer(issuer.to_string()))?;

        // Try verification, with one JWKS refresh retry on signature failure
        // or unknown kid (key rotation scenario).
        match self.verify_with_jwks(token, &kid, provider, false).await {
            Ok(claims) => Ok(claims),
            Err(OidcError::Validation(ref e))
                if matches!(e.kind(), jsonwebtoken::errors::ErrorKind::InvalidSignature) =>
            {
                debug!(
                    kid,
                    issuer, "signature verification failed, refreshing JWKS and retrying"
                );
                self.verify_with_jwks(token, &kid, provider, true).await
            }
            Err(OidcError::KeyNotFound(_)) => {
                debug!(
                    kid,
                    issuer, "key not found in cached JWKS, refreshing and retrying"
                );
                self.verify_with_jwks(token, &kid, provider, true).await
            }
            Err(e) => Err(e),
        }
    }

    async fn verify_with_jwks(
        &self,
        token: &str,
        kid: &str,
        provider: &OidcProvider,
        force_refresh: bool,
    ) -> Result<OidcClaims, OidcError> {
        let keys = self.get_jwks(&provider.issuer, force_refresh).await?;

        let jwk = keys
            .iter()
            .find(|k| k.kid.as_deref() == Some(kid))
            .ok_or_else(|| OidcError::KeyNotFound(kid.to_string()))?;

        // C1: Derive algorithm from the JWK, not from the attacker-controlled
        // JWT header. This prevents algorithm confusion attacks.
        let (decoding_key, alg) = jwk_to_decoding_key(jwk)?;

        let mut validation = Validation::new(alg);
        validation.set_audience(&[&provider.audience]);
        validation.set_issuer(&[&provider.issuer]);

        let data = decode::<GenericClaims>(token, &decoding_key, &validation)?;
        let claims = data.claims;

        // Check bound claims with distinct error cases for missing vs non-string.
        for (key, expected) in &provider.bound_claims {
            let Some(raw) = claims.extra.get(key) else {
                return Err(OidcError::BoundClaimMissing { key: key.clone() });
            };
            let Some(got) = raw.as_str() else {
                return Err(OidcError::BoundClaimNotString { key: key.clone() });
            };
            if got != expected {
                return Err(OidcError::BoundClaimMismatch {
                    key: key.clone(),
                    expected: expected.clone(),
                    got: got.to_string(),
                });
            }
        }

        // S1: Require `sub` claim -- missing subject is useless for audit trails.
        let subject = claims.sub.ok_or(OidcError::MissingSubject)?;

        Ok(OidcClaims {
            issuer: claims.iss.unwrap_or_default(),
            subject,
            extra: claims.extra,
        })
    }

    async fn get_jwks(&self, issuer: &str, force_refresh: bool) -> Result<Vec<JwkKey>, OidcError> {
        // Check cache first (unless force refresh).
        if !force_refresh {
            let cache = self.jwks_cache.read().await;
            if let Some(cached) = cache.get(issuer)
                && cached.fetched_at.elapsed() < JWKS_CACHE_TTL
            {
                return Ok(cached.keys.clone());
            }
        }

        // Rate-limit forced refreshes to prevent attacker-triggered JWKS floods.
        if force_refresh {
            let cache = self.jwks_cache.read().await;
            if let Some(cached) = cache.get(issuer)
                && cached.fetched_at.elapsed() < JWKS_REFRESH_COOLDOWN
            {
                debug!(issuer, "JWKS refresh cooldown active, using cached keys");
                return Ok(cached.keys.clone());
            }
        }

        // Fetch OIDC discovery document.
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let resp =
            self.http
                .get(&discovery_url)
                .send()
                .await
                .map_err(|e| OidcError::JwksFetch {
                    url: discovery_url.clone(),
                    source: e,
                })?;
        if !resp.status().is_success() {
            return Err(OidcError::Discovery {
                issuer: issuer.to_string(),
                reason: format!("HTTP {} from {discovery_url}", resp.status()),
            });
        }
        let discovery: OidcDiscovery = resp.json().await.map_err(|e| OidcError::Discovery {
            issuer: issuer.to_string(),
            reason: format!("failed to parse discovery document: {e}"),
        })?;

        debug!(issuer, jwks_uri = %discovery.jwks_uri, "fetched OIDC discovery");

        // Fetch JWKS.
        let resp = self
            .http
            .get(&discovery.jwks_uri)
            .send()
            .await
            .map_err(|e| OidcError::JwksFetch {
                url: discovery.jwks_uri.clone(),
                source: e,
            })?;
        if !resp.status().is_success() {
            return Err(OidcError::Discovery {
                issuer: issuer.to_string(),
                reason: format!("HTTP {} from {}", resp.status(), discovery.jwks_uri),
            });
        }
        let jwks: JwksResponse = resp.json().await.map_err(|e| OidcError::Discovery {
            issuer: issuer.to_string(),
            reason: format!("failed to parse JWKS: {e}"),
        })?;

        debug!(issuer, keys = jwks.keys.len(), "fetched JWKS");

        // Update cache.
        let keys = jwks.keys.clone();
        let mut cache = self.jwks_cache.write().await;
        cache.insert(
            issuer.to_string(),
            CachedJwks {
                keys: jwks.keys,
                fetched_at: Instant::now(),
            },
        );

        Ok(keys)
    }
}

/// Generic JWT claims for initial decoding.
#[derive(Debug, Deserialize)]
struct GenericClaims {
    iss: Option<String>,
    sub: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

/// Convert a JWK to a `DecodingKey` and its algorithm.
///
/// The algorithm is derived from the JWK's `alg` field (preferred) or
/// inferred from `kty` as a fallback. This avoids using the attacker-
/// controlled JWT header `alg` for validation (algorithm confusion).
fn jwk_to_decoding_key(jwk: &JwkKey) -> Result<(DecodingKey, Algorithm), OidcError> {
    // Determine algorithm from JWK metadata (not from JWT header).
    let alg = match jwk.alg.as_deref() {
        Some("RS256") => Algorithm::RS256,
        Some("RS384") => Algorithm::RS384,
        Some("RS512") => Algorithm::RS512,
        Some("ES256") => Algorithm::ES256,
        Some("ES384") => Algorithm::ES384,
        // No explicit alg -- infer from key type with safe default.
        None => match jwk.kty.as_str() {
            "RSA" => Algorithm::RS256,
            "EC" => Algorithm::ES256,
            other => {
                return Err(OidcError::Discovery {
                    issuer: String::new(),
                    reason: format!("unsupported JWK key type without alg: {other}"),
                });
            }
        },
        Some(other) => {
            warn!(alg = other, "JWK has unsupported algorithm, rejecting");
            return Err(OidcError::Discovery {
                issuer: String::new(),
                reason: format!("unsupported JWK algorithm: {other}"),
            });
        }
    };

    let key = match jwk.kty.as_str() {
        "RSA" => {
            let n = jwk.n.as_deref().ok_or_else(|| OidcError::Discovery {
                issuer: String::new(),
                reason: "RSA JWK missing 'n' field".into(),
            })?;
            let e = jwk.e.as_deref().ok_or_else(|| OidcError::Discovery {
                issuer: String::new(),
                reason: "RSA JWK missing 'e' field".into(),
            })?;
            DecodingKey::from_rsa_components(n, e).map_err(OidcError::Validation)?
        }
        "EC" => {
            let x = jwk.x.as_deref().ok_or_else(|| OidcError::Discovery {
                issuer: String::new(),
                reason: "EC JWK missing 'x' field".into(),
            })?;
            let y = jwk.y.as_deref().ok_or_else(|| OidcError::Discovery {
                issuer: String::new(),
                reason: "EC JWK missing 'y' field".into(),
            })?;
            DecodingKey::from_ec_components(x, y).map_err(OidcError::Validation)?
        }
        other => {
            return Err(OidcError::Discovery {
                issuer: String::new(),
                reason: format!("unsupported JWK key type: {other}"),
            });
        }
    };

    Ok((key, alg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_providers_returns_none() {
        assert!(OidcVerifier::new(vec![]).unwrap().is_none());
    }

    #[test]
    fn some_providers_returns_some() {
        let providers = vec![OidcProvider {
            issuer: "https://example.com".into(),
            audience: "test".into(),
            bound_claims: HashMap::new(),
        }];
        assert!(OidcVerifier::new(providers).unwrap().is_some());
    }

    #[test]
    fn empty_issuer_rejected() {
        let providers = vec![OidcProvider {
            issuer: String::new(),
            audience: "test".into(),
            bound_claims: HashMap::new(),
        }];
        assert!(OidcVerifier::new(providers).is_err());
    }

    #[test]
    fn empty_audience_rejected() {
        let providers = vec![OidcProvider {
            issuer: "https://example.com".into(),
            audience: String::new(),
            bound_claims: HashMap::new(),
        }];
        assert!(OidcVerifier::new(providers).is_err());
    }

    #[test]
    fn empty_bound_claim_value_rejected() {
        let providers = vec![OidcProvider {
            issuer: "https://example.com".into(),
            audience: "test".into(),
            bound_claims: [("repo".into(), String::new())].into(),
        }];
        assert!(OidcVerifier::new(providers).is_err());
    }

    #[test]
    fn oidc_provider_deserialize() {
        let json = r#"{
            "issuer": "https://token.actions.githubusercontent.com",
            "audience": "rio-store",
            "bound_claims": { "repository_owner": "myorg" }
        }"#;
        let provider: OidcProvider = serde_json::from_str(json).unwrap();
        assert_eq!(
            provider.issuer,
            "https://token.actions.githubusercontent.com"
        );
        assert_eq!(provider.audience, "rio-store");
        assert_eq!(
            provider.bound_claims.get("repository_owner"),
            Some(&"myorg".to_string())
        );
    }
}

//! OIDC token validation for external push authentication.
//!
//! Validates JWT tokens from OIDC providers (e.g., GitHub Actions) as an
//! alternative to HMAC assignment tokens. Each provider is configured with
//! an issuer URL, expected audience, and optional bound claims that must
//! match (e.g., `repository_owner = "myorg"`).
//!
//! JWKS keys are fetched from the provider's `.well-known/openid-configuration`
//! endpoint and cached for 1 hour. On signature verification failure, the
//! JWKS cache is force-refreshed once to handle key rotation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info};

/// How long to cache JWKS keys before re-fetching.
const JWKS_CACHE_TTL: Duration = Duration::from_secs(3600);

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
#[derive(Debug, Clone)]
pub struct OidcClaims {
    pub issuer: String,
    pub subject: String,
    pub extra: HashMap<String, serde_json::Value>,
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
    #[error("JWT header missing kid")]
    MissingKid,
    #[error("no matching key found for kid {0:?}")]
    KeyNotFound(String),
}

/// JWKS key set response from the provider.
#[derive(Debug, Clone, Deserialize)]
struct JwksResponse {
    keys: Vec<JwkKey>,
}

/// A single JWK key. Fields match the JWK spec (RFC 7517).
/// `alg` and `crv` are part of the spec and deserialized from the
/// provider's JWKS — they're consumed indirectly via `kty` dispatch.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct JwkKey {
    kid: Option<String>,
    kty: String,
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
    x: Option<String>,
    y: Option<String>,
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
    pub fn new(providers: Vec<OidcProvider>) -> Option<Arc<Self>> {
        if providers.is_empty() {
            return None;
        }
        for p in &providers {
            info!(issuer = %p.issuer, audience = %p.audience, "OIDC provider configured");
        }
        Some(Arc::new(Self {
            providers,
            http: reqwest::Client::new(),
            jwks_cache: RwLock::new(HashMap::new()),
        }))
    }

    /// Validate an OIDC JWT token.
    ///
    /// 1. Decode header to get `kid` and peek at unverified `iss`
    /// 2. Find matching provider by issuer
    /// 3. Fetch/cache JWKS, find key by `kid`
    /// 4. Full JWT validation (signature, expiry, audience, issuer)
    /// 5. Check bound claims
    /// 6. On InvalidSignature, force-refresh JWKS once and retry
    pub async fn verify(&self, token: &str) -> Result<OidcClaims, OidcError> {
        let header = decode_header(token)?;
        let kid = header.kid.ok_or(OidcError::MissingKid)?;

        // Peek at unverified claims to find the issuer.
        let unverified: GenericClaims = {
            let mut no_verify = Validation::new(header.alg);
            no_verify.insecure_disable_signature_validation();
            no_verify.validate_aud = false;
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

        // Try verification, with one JWKS refresh retry on signature failure.
        match self
            .verify_with_jwks(token, &header.alg, &kid, provider, false)
            .await
        {
            Ok(claims) => Ok(claims),
            Err(OidcError::Validation(ref e))
                if matches!(e.kind(), jsonwebtoken::errors::ErrorKind::InvalidSignature) =>
            {
                debug!(
                    kid,
                    issuer, "signature verification failed, refreshing JWKS and retrying"
                );
                self.verify_with_jwks(token, &header.alg, &kid, provider, true)
                    .await
            }
            Err(OidcError::KeyNotFound(_)) => {
                debug!(
                    kid,
                    issuer, "key not found in cached JWKS, refreshing and retrying"
                );
                self.verify_with_jwks(token, &header.alg, &kid, provider, true)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    async fn verify_with_jwks(
        &self,
        token: &str,
        alg: &Algorithm,
        kid: &str,
        provider: &OidcProvider,
        force_refresh: bool,
    ) -> Result<OidcClaims, OidcError> {
        let keys = self.get_jwks(&provider.issuer, force_refresh).await?;

        let jwk = keys
            .iter()
            .find(|k| k.kid.as_deref() == Some(kid))
            .ok_or_else(|| OidcError::KeyNotFound(kid.to_string()))?;

        let decoding_key = jwk_to_decoding_key(jwk)?;

        let mut validation = Validation::new(*alg);
        validation.set_audience(&[&provider.audience]);
        validation.set_issuer(&[&provider.issuer]);

        let data = decode::<GenericClaims>(token, &decoding_key, &validation)?;
        let claims = data.claims;

        // Check bound claims.
        for (key, expected) in &provider.bound_claims {
            let got = claims.extra.get(key).and_then(|v| v.as_str()).unwrap_or("");
            if got != expected {
                return Err(OidcError::BoundClaimMismatch {
                    key: key.clone(),
                    expected: expected.clone(),
                    got: got.to_string(),
                });
            }
        }

        Ok(OidcClaims {
            issuer: claims.iss.unwrap_or_default(),
            subject: claims.sub.unwrap_or_default(),
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

        // Fetch OIDC discovery document.
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let discovery: OidcDiscovery = self
            .http
            .get(&discovery_url)
            .send()
            .await
            .map_err(|e| OidcError::JwksFetch {
                url: discovery_url.clone(),
                source: e,
            })?
            .json()
            .await
            .map_err(|e| OidcError::Discovery {
                issuer: issuer.to_string(),
                reason: format!("failed to parse discovery document: {e}"),
            })?;

        debug!(issuer, jwks_uri = %discovery.jwks_uri, "fetched OIDC discovery");

        // Fetch JWKS.
        let jwks: JwksResponse = self
            .http
            .get(&discovery.jwks_uri)
            .send()
            .await
            .map_err(|e| OidcError::JwksFetch {
                url: discovery.jwks_uri.clone(),
                source: e,
            })?
            .json()
            .await
            .map_err(|e| OidcError::Discovery {
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

/// Convert a JWK to a `DecodingKey`.
fn jwk_to_decoding_key(jwk: &JwkKey) -> Result<DecodingKey, OidcError> {
    match jwk.kty.as_str() {
        "RSA" => {
            let n = jwk.n.as_deref().ok_or_else(|| OidcError::Discovery {
                issuer: String::new(),
                reason: "RSA JWK missing 'n' field".into(),
            })?;
            let e = jwk.e.as_deref().ok_or_else(|| OidcError::Discovery {
                issuer: String::new(),
                reason: "RSA JWK missing 'e' field".into(),
            })?;
            DecodingKey::from_rsa_components(n, e).map_err(OidcError::Validation)
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
            DecodingKey::from_ec_components(x, y).map_err(OidcError::Validation)
        }
        other => Err(OidcError::Discovery {
            issuer: String::new(),
            reason: format!("unsupported JWK key type: {other}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_providers_returns_none() {
        assert!(OidcVerifier::new(vec![]).is_none());
    }

    #[test]
    fn some_providers_returns_some() {
        let providers = vec![OidcProvider {
            issuer: "https://example.com".into(),
            audience: "test".into(),
            bound_claims: HashMap::new(),
        }];
        assert!(OidcVerifier::new(providers).is_some());
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

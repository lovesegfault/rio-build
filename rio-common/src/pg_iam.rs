//! RDS IAM database authentication: one [`TokenSource`] per database
//! URL owning credential resolution, SigV4 token minting, freshness,
//! metrics, and live-pool refresh.
//!
//! Why this exists: the AWS-managed Aurora master password rotates
//! every 7 days (`manage_master_user_password`, infra/eks/rds.tf).
//! Pods read `RIO_DATABASE_URL` once at start, so every rotation broke
//! all NEW DB connections until pods restarted — and the pools are
//! churny (`idle_timeout=60s`), so breakage landed within minutes.
//! IAM auth eliminates the static password: the pod's IRSA role mints
//! a 15-minute token per connection-options refresh, and the
//! refresher task swaps it into the live pool via
//! [`sqlx::Pool::set_connect_options`] (future connections use the new
//! options; established connections are untouched — Postgres only
//! authenticates at connection start, so the swap is hitless).
//!
//! Why ONE component instead of per-consumer mint helpers: every PG
//! consumer (store, scheduler, controller) needs the same three
//! things — a config preflight that fails fast, a fresh token per
//! connect attempt, and a background refresher — and hand-rolling
//! the combination produced three distinct bugs (a per-mint
//! `aws_config::load_defaults` that discarded the SDK credential
//! cache, a one-token-for-a-whole-retry-loop mint, and a refresher
//! whose first tick was spawn-relative). The split here:
//!
//! - [`TokenSource::new`]: parses the URL, validates the TLS posture,
//!   and resolves the AWS `SdkConfig` ONCE. Permanent (config) errors
//!   are only producible HERE — callers `?` it before entering any
//!   retry loop, so a bad URL/missing IRSA region crash-loops the pod
//!   visibly instead of warn-retrying forever.
//! - [`TokenSource::fresh_options`]: re-mints when the cached token
//!   is older than `FRESH_WINDOW` (60s); retry loops call it PER ATTEMPT.
//!   Minting is pure SigV4 over the SDK's cached (lazily refreshed)
//!   session credentials — an STS outage is survivable for as long as
//!   those session credentials remain valid (~hours), not just the
//!   token's own stale budget.
//! - [`TokenSource::spawn_refresher`]: keeps a live pool's stored
//!   options fresh. Scheduled relative to the LAST SUCCESSFUL MINT
//!   (wherever it happened — a connect-loop mint counts), not task
//!   spawn time.
//!
//! Security: the token is a bearer credential presented as the PG
//! password. `sslmode=require` does NOT verify the server certificate,
//! so a VPC-adjacent MITM could harvest and replay it. IAM mode
//! therefore REQUIRES `sslmode=verify-full` plus an `sslrootcert=`
//! query parameter pointing at an existing file (the vendored AWS RDS
//! trust bundle — RDS certs chain to Amazon-private CAs, not public
//! PKI). verify-full completes server verification before the
//! password is sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context as _;
use aws_credential_types::Credentials;
use aws_credential_types::provider::ProvideCredentials as _;
use aws_sigv4::http_request::{
    SignableBody, SignableRequest, SignatureLocation, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use futures_util::FutureExt as _;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use tracing::{error, info, warn};

use crate::config::PgAuthMode;

/// RDS IAM auth tokens are valid for at most 15 minutes (AWS-fixed
/// ceiling; we request exactly that).
const TOKEN_TTL: Duration = Duration::from_secs(900);

/// Refresher re-mint cadence, relative to the last successful mint.
/// 10 of the 15 token minutes — leaves a 5-minute budget of
/// [`MINT_RETRY_INTERVAL`] retries before the pool's stored token goes
/// stale. A stale token only affects NEW connections; established
/// ones keep serving regardless.
const REFRESH_INTERVAL: Duration = Duration::from_secs(600);

/// Retry cadence after a mint failure (STS hiccup, IRSA token rotation
/// race). Short relative to the 5-minute stale budget.
const MINT_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// [`TokenSource::fresh_options`] reuses a cached token younger than
/// this. Retry loops calling per-attempt therefore never present a
/// token older than ~1 minute (vs the 15-minute TTL), while tight
/// loops don't re-sign on every iteration.
const FRESH_WINDOW: Duration = Duration::from_secs(60);

/// Register descriptions for the shared `rio_pg_iam_*` metric family.
/// Called from each PG consumer's `describe_metrics()` (store,
/// scheduler, controller) — registration and emission are separate
/// call sites, and rio-common has no exporter of its own.
pub fn describe_metrics() {
    metrics::describe_counter!(
        "rio_pg_iam_mint_failures_total",
        "RDS IAM auth token mint failures (STS/credential errors). \
         Sustained nonzero rate = new PG connections will start \
         failing once the cached token passes its 15-minute TTL."
    );
    metrics::describe_gauge!(
        "rio_pg_iam_token_minted_timestamp_seconds",
        "Unix timestamp of the last successful RDS IAM token mint. \
         Alert on time() - this approaching 900 (the token TTL): it \
         means mints are failing or the refresher died."
    );
}

/// Cached mint state. `minted_at` is monotonic (freshness math);
/// the emitted gauge uses wall-clock time separately.
struct CachedToken {
    token: String,
    minted_at: Instant,
}

/// IAM-mode state: the once-resolved SDK config plus the token cache.
struct IamState {
    sdk: aws_config::SdkConfig,
    region: String,
    cached: tokio::sync::Mutex<Option<CachedToken>>,
    /// Successful mints — observability for tests; the prometheus
    /// counter only counts failures.
    mints: AtomicU64,
}

/// Credential source for one database URL. Construct once per process
/// ([`TokenSource::new`] — the ONLY place permanent config errors can
/// surface), then call [`fresh_options`](Self::fresh_options) per
/// connect attempt and [`spawn_refresher`](Self::spawn_refresher) once
/// per pool.
pub struct TokenSource {
    base: PgConnectOptions,
    mode: PgAuthMode,
    iam: Option<IamState>,
}

impl std::fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No token/credential material — host+mode is all diagnostics
        // need.
        f.debug_struct("TokenSource")
            .field("mode", &self.mode)
            .field("host", &self.base.get_host())
            .finish_non_exhaustive()
    }
}

impl TokenSource {
    /// Parse + validate `database_url` under `mode` and (in IAM mode)
    /// resolve the AWS `SdkConfig` once.
    ///
    /// `Password`: plain parse — exactly the options
    /// `Pool::connect(url)` would use; no TLS upgrade, no AWS
    /// involvement (k3s/local URLs pass through untouched).
    ///
    /// `Iam`: errors unless the URL requests `sslmode=verify-full`
    /// AND carries an `sslrootcert=` query parameter naming an
    /// existing file. Both checks run BEFORE any AWS call: never
    /// produce a token that could be sent over a connection that
    /// skips server verification, and fail the missing-bundle case at
    /// startup with an actionable message instead of per-connect TLS
    /// errors.
    pub async fn new(database_url: &str, mode: PgAuthMode) -> anyhow::Result<Self> {
        let base: PgConnectOptions = database_url
            .parse()
            .context("invalid PostgreSQL database_url")?;
        if mode != PgAuthMode::Iam {
            return Ok(Self {
                base,
                mode,
                iam: None,
            });
        }

        anyhow::ensure!(
            matches!(base.get_ssl_mode(), PgSslMode::VerifyFull),
            "pg_auth=iam requires sslmode=verify-full in database_url: the IAM \
             token is a bearer credential and must only be sent to a verified server"
        );
        // sqlx exposes no root-cert getter, so check the URL query
        // directly. (sqlx also honors the PGSSLROOTCERT env var — a
        // deployment supplying the cert that way would verify fine —
        // but rio always uses the URL parameter, so its absence here
        // is a misconfiguration.)
        let parsed = url::Url::parse(database_url).context("invalid PostgreSQL database_url")?;
        let rootcert = parsed
            .query_pairs()
            .find(|(k, _)| k == "sslrootcert")
            .map(|(_, v)| v.into_owned())
            .filter(|v| !v.is_empty());
        let rootcert = rootcert.ok_or_else(|| {
            anyhow::anyhow!(
                "pg_auth=iam requires an sslrootcert=<RDS CA bundle> query parameter \
                 in database_url (the helm chart mounts the vendored bundle and \
                 renders the parameter; sqlx would also honor PGSSLROOTCERT, but \
                 rio deployments always pass the URL parameter)"
            )
        })?;
        anyhow::ensure!(
            std::path::Path::new(&rootcert).exists(),
            "pg_auth=iam: sslrootcert file {rootcert:?} does not exist — is the \
             rio-rds-ca ConfigMap mounted? (rdsCa mount family, gated on \
             externalSecrets.enabled)"
        );

        // Resolve the SDK config ONCE (mirrors s3.rs::default_client).
        // The credentials provider caches session credentials and
        // refreshes them lazily — subsequent mints are pure SigV4, so
        // an STS outage is survivable for the session lifetime, not
        // merely the token-stale budget.
        let sdk = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let region = sdk
            .region()
            .context("no AWS region resolved (AWS_REGION unset? IRSA injects it)")?
            .to_string();
        anyhow::ensure!(
            sdk.credentials_provider().is_some(),
            "no AWS credentials provider configured (IRSA role attached?)"
        );
        Ok(Self {
            base,
            mode,
            iam: Some(IamState {
                sdk,
                region,
                cached: tokio::sync::Mutex::new(None),
                mints: AtomicU64::new(0),
            }),
        })
    }

    /// Test-only constructor with a caller-supplied `SdkConfig`
    /// (static credentials, no network). Skips the rootcert
    /// file-existence check's dependency on real deployment paths —
    /// the caller passes a URL whose sslrootcert points at a temp
    /// file it created.
    #[cfg(test)]
    async fn new_with_sdk_for_tests(
        database_url: &str,
        sdk: aws_config::SdkConfig,
    ) -> anyhow::Result<Self> {
        let base: PgConnectOptions = database_url.parse()?;
        let region = sdk
            .region()
            .context("test SdkConfig needs a region")?
            .to_string();
        Ok(Self {
            base,
            mode: PgAuthMode::Iam,
            iam: Some(IamState {
                sdk,
                region,
                cached: tokio::sync::Mutex::new(None),
                mints: AtomicU64::new(0),
            }),
        })
    }

    /// Connect options carrying a token no older than
    /// `FRESH_WINDOW` (60s). Password mode: the plain parsed options.
    ///
    /// Retry loops MUST call this per attempt (not once before the
    /// loop): a connect loop can outlive a token's TTL, and a fresh
    /// mint costs one cached-credential SigV4 signature.
    pub async fn fresh_options(&self) -> anyhow::Result<PgConnectOptions> {
        let Some(iam) = &self.iam else {
            return Ok(self.base.clone());
        };
        let mut cached = iam.cached.lock().await;
        if let Some(c) = cached
            .as_ref()
            .filter(|c| c.minted_at.elapsed() < FRESH_WINDOW)
        {
            return Ok(self.base.clone().password(&c.token));
        }
        let token = self.mint(iam).await?;
        *cached = Some(CachedToken {
            token: token.clone(),
            minted_at: Instant::now(),
        });
        Ok(self.base.clone().password(&token))
    }

    /// Mint a token now. Emits `rio_pg_iam_mint_failures_total` on
    /// failure and sets `rio_pg_iam_token_minted_timestamp_seconds`
    /// on success — the gauge is set per MINT (not per refresher
    /// tick), so `time() - gauge` in PromQL is the true token age.
    async fn mint(&self, iam: &IamState) -> anyhow::Result<String> {
        let result = async {
            let creds = iam
                .sdk
                .credentials_provider()
                .context("no AWS credentials provider configured")?
                .provide_credentials()
                .await
                .context("AWS credential resolution failed (IRSA role attached?)")?;
            mint_token_at(
                self.base.get_host(),
                self.base.get_port(),
                self.base.get_username(),
                &iam.region,
                creds,
                SystemTime::now(),
            )
        }
        .await;
        match &result {
            Ok(_) => {
                iam.mints.fetch_add(1, Ordering::Relaxed);
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();
                metrics::gauge!("rio_pg_iam_token_minted_timestamp_seconds").set(now);
            }
            Err(_) => {
                metrics::counter!("rio_pg_iam_mint_failures_total").increment(1);
            }
        }
        result
    }

    /// Wall-clock instant of the last successful mint (any caller).
    async fn last_mint(&self) -> Option<Instant> {
        match &self.iam {
            Some(iam) => iam.cached.lock().await.as_ref().map(|c| c.minted_at),
            None => None,
        }
    }

    /// In IAM mode, spawn a background task that keeps `pool`'s
    /// stored connect options fresh. No-op in password mode. The task
    /// is detached — no shutdown coupling; it dies with the process.
    ///
    /// Scheduling is relative to the LAST SUCCESSFUL MINT — a token
    /// minted by a connect retry loop pushes the next refresh out, and
    /// a refresher spawned long after construction does not wait a
    /// full interval on top of an already-aging token.
    ///
    /// On mint failure the task warns and retries on a short cadence;
    /// the pool keeps its previous options, so established connections
    /// carry traffic and only new-connection attempts can fail (and
    /// only once the stored token actually expires).
    ///
    /// Panic containment: each iteration runs under `catch_unwind`
    /// (log + retry-interval backoff). `spawn_monitored` alone
    /// re-panics and the TASK DIES — for a refresher that must outlive
    /// any single bad iteration, that silently downgrades the pod to
    /// "fails 15 minutes after the last good mint". There are no known
    /// panic sites left in the mint path (the one `expect` died with
    /// the rewrite), so this is a backstop, not a crutch.
    pub fn spawn_refresher(self: &Arc<Self>, pool: PgPool) {
        if self.mode != PgAuthMode::Iam {
            return;
        }
        let ts = Arc::clone(self);
        crate::task::spawn_monitored("pg_iam_refresher", async move {
            loop {
                let due = match ts.last_mint().await {
                    Some(at) => (at + REFRESH_INTERVAL).saturating_duration_since(Instant::now()),
                    None => Duration::ZERO,
                };
                tokio::time::sleep(due).await;

                let iteration = std::panic::AssertUnwindSafe(async {
                    let Some(iam) = &ts.iam else { return };
                    let mut cached = iam.cached.lock().await;
                    match ts.mint(iam).await {
                        Ok(token) => {
                            pool.set_connect_options(ts.base.clone().password(&token));
                            *cached = Some(CachedToken {
                                token,
                                minted_at: Instant::now(),
                            });
                            info!("refreshed RDS IAM auth token for PG pool");
                        }
                        Err(e) => {
                            drop(cached);
                            warn!(
                                error = format!("{e:#}"),
                                retry_secs = MINT_RETRY_INTERVAL.as_secs(),
                                "RDS IAM token mint failed; existing connections \
                                 unaffected, retrying"
                            );
                            tokio::time::sleep(MINT_RETRY_INTERVAL).await;
                        }
                    }
                })
                .catch_unwind();
                if iteration.await.is_err() {
                    error!("pg_iam refresher iteration panicked; backing off and continuing");
                    tokio::time::sleep(MINT_RETRY_INTERVAL).await;
                }
            }
        });
    }

    #[cfg(test)]
    fn mint_count(&self) -> u64 {
        self.iam
            .as_ref()
            .map(|i| i.mints.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Rewind the cached token's age past [`FRESH_WINDOW`] so the next
    /// [`fresh_options`](Self::fresh_options) re-mints.
    #[cfg(test)]
    async fn force_stale_for_tests(&self) {
        if let Some(iam) = &self.iam
            && let Some(c) = &mut *iam.cached.lock().await
        {
            c.minted_at = Instant::now() - FRESH_WINDOW - Duration::from_secs(1);
        }
    }
}

/// The actual token construction: a SigV4 query-presigned
/// `GET https://<endpoint>:<port>/?Action=connect&DBUser=<user>` for
/// service `rds-db`, with the `https://` scheme stripped. Split from
/// the [`TokenSource`] plumbing so the signing shape is unit-testable
/// with static credentials and a fixed timestamp.
fn mint_token_at(
    host: &str,
    port: u16,
    user: &str,
    region: &str,
    credentials: Credentials,
    at: SystemTime,
) -> anyhow::Result<String> {
    let identity: Identity = credentials.into();
    let mut settings = SigningSettings::default();
    settings.signature_location = SignatureLocation::QueryParams;
    settings.expires_in = Some(TOKEN_TTL);
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name("rds-db")
        .time(at)
        .settings(settings)
        .build()
        .context("building RDS SigV4 signing params")?;

    // Build the canonical URL with the query writer, NOT format!():
    // DBUser is interpolated, and reserved characters in a username
    // (`+`, `@`, …) must be percent-encoded in the canonical request
    // or RDS rejects the signature.
    let mut url =
        url::Url::parse(&format!("https://{host}:{port}/")).context("building RDS connect URL")?;
    url.query_pairs_mut()
        .append_pair("Action", "connect")
        .append_pair("DBUser", user);
    let url = url.to_string();
    let signable = SignableRequest::new("GET", &url, std::iter::empty(), SignableBody::Bytes(&[]))
        .context("building signable RDS connect request")?;
    let (instructions, _signature) = sign(signable, &params.into())
        .context("signing RDS connect request")?
        .into_parts();

    let mut signed = url::Url::parse(&url).context("re-parsing RDS connect URL")?;
    for (name, value) in instructions.params() {
        signed.query_pairs_mut().append_pair(name, value);
    }
    // PG expects the password WITHOUT the scheme: `host:port/?X-Amz-…`.
    let mut token = signed.to_string();
    Ok(token.split_off("https://".len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::provider::SharedCredentialsProvider;

    fn static_creds(session_token: Option<&str>) -> Credentials {
        Credentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            session_token.map(str::to_owned),
            None,
            "test",
        )
    }

    fn test_sdk() -> aws_config::SdkConfig {
        aws_config::SdkConfig::builder()
            .credentials_provider(SharedCredentialsProvider::new(static_creds(None)))
            .region(aws_config::Region::new("us-east-1"))
            .behavior_version(aws_config::BehaviorVersion::latest())
            .build()
    }

    /// Temp file standing in for the mounted RDS CA bundle.
    fn temp_bundle() -> tempfile::NamedTempFile {
        tempfile::NamedTempFile::new().expect("temp bundle")
    }

    /// IAM URL whose sslrootcert points at `bundle`.
    fn iam_url(bundle: &tempfile::NamedTempFile) -> String {
        format!(
            "postgres://rio_app@db.example.com:5432/rio?sslmode=verify-full&sslrootcert={}",
            bundle.path().display()
        )
    }

    /// Credentials provider that fails every resolution — the
    /// expired/revoked-IRSA shape construction-time presence checks
    /// cannot catch.
    #[derive(Debug)]
    struct FailingCreds;
    impl aws_credential_types::provider::ProvideCredentials for FailingCreds {
        fn provide_credentials<'a>(
            &'a self,
        ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            aws_credential_types::provider::future::ProvideCredentials::ready(Err(
                aws_credential_types::provider::error::CredentialsError::not_loaded(
                    "web identity token expired (test)",
                ),
            ))
        }
    }

    fn failing_sdk() -> aws_config::SdkConfig {
        aws_config::SdkConfig::builder()
            .credentials_provider(SharedCredentialsProvider::new(FailingCreds))
            .region(aws_config::Region::new("us-east-1"))
            .behavior_version(aws_config::BehaviorVersion::latest())
            .build()
    }

    /// The token must be scheme-less (`host:port/?…`) and carry the
    /// canonical RDS connect parameters plus the SigV4 presign
    /// parameters. A malformed shape here means every IAM-mode connect
    /// fails against real RDS — this pins the contract.
    #[test]
    fn token_shape() {
        let token = mint_token_at(
            "rio-pg.cluster-abc.eu-central-1.rds.amazonaws.com",
            5432,
            "rio_app",
            "eu-central-1",
            static_creds(None),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_780_000_000),
        )
        .unwrap();

        assert!(
            token.starts_with("rio-pg.cluster-abc.eu-central-1.rds.amazonaws.com:5432/?"),
            "token must start with host:port/? — got {token}"
        );
        assert!(
            !token.contains("https://"),
            "scheme must be stripped: {token}"
        );
        assert!(token.contains("Action=connect"), "{token}");
        assert!(token.contains("DBUser=rio_app"), "{token}");
        assert!(token.contains("X-Amz-Signature="), "{token}");
        assert!(token.contains("X-Amz-Expires=900"), "{token}");
        // Credential scope ties the signature to the rds-db service in
        // the right region ('/' percent-encoded by the query writer).
        assert!(token.contains("eu-central-1%2Frds-db"), "{token}");
    }

    /// IRSA credentials always carry a session token; if it is dropped
    /// from the presign, RDS rejects the signature. Guard that the
    /// X-Amz-Security-Token parameter survives into the token.
    #[test]
    fn token_carries_session_token() {
        let token = mint_token_at(
            "h.example.com",
            5432,
            "rio_app",
            "us-east-1",
            static_creds(Some("THE-SESSION-TOKEN")),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_780_000_000),
        )
        .unwrap();
        assert!(token.contains("X-Amz-Security-Token="), "{token}");
    }

    /// Reserved characters in the DB username must be percent-encoded
    /// into the canonical request — a format!()-interpolated DBUser
    /// would corrupt both the URL and the signature for usernames
    /// containing `+` or `@`.
    #[test]
    fn token_percent_encodes_reserved_chars_in_dbuser() {
        let token = mint_token_at(
            "h.example.com",
            5432,
            "we+ird@user",
            "us-east-1",
            static_creds(None),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_780_000_000),
        )
        .unwrap();
        assert!(token.contains("DBUser=we%2Bird%40user"), "{token}");
        assert!(!token.contains("we+ird@user"), "{token}");
    }

    /// IAM mode with anything weaker than verify-full must be rejected
    /// BEFORE a token is minted (and before any AWS call — this test
    /// runs without credentials or network).
    #[tokio::test]
    async fn iam_mode_rejects_unverified_tls() {
        for url in [
            "postgres://rio_app@db.example.com:5432/rio?sslmode=require",
            // verify-ca checks the chain but NOT the hostname — any
            // cert from the same CA could harvest the token.
            "postgres://rio_app@db.example.com:5432/rio?sslmode=verify-ca",
            "postgres://rio_app@db.example.com:5432/rio", // sqlx default: prefer
        ] {
            let err = TokenSource::new(url, PgAuthMode::Iam).await.unwrap_err();
            assert!(
                err.to_string().contains("verify-full"),
                "want verify-full error for {url}, got: {err:#}"
            );
        }
    }

    /// verify-full alone is not enough: without sslrootcert the RDS
    /// chain (Amazon-private CAs) cannot validate, so every connect
    /// fails at runtime — reject at construction with an actionable
    /// message instead. Same for a configured-but-absent bundle file.
    #[tokio::test]
    async fn iam_mode_rejects_missing_or_absent_sslrootcert() {
        let err = TokenSource::new(
            "postgres://rio_app@db.example.com:5432/rio?sslmode=verify-full",
            PgAuthMode::Iam,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("sslrootcert"),
            "missing param: {err:#}"
        );

        let err = TokenSource::new(
            "postgres://rio_app@db.example.com:5432/rio?sslmode=verify-full\
             &sslrootcert=/nonexistent/rds-bundle.pem",
            PgAuthMode::Iam,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "absent file: {err:#}"
        );
    }

    /// Password mode must be a plain parse — no TLS upgrade, no AWS
    /// involvement — so k3s/local URLs pass through untouched.
    #[tokio::test]
    async fn password_mode_is_plain_parse() {
        let ts = TokenSource::new(
            "postgres://rio:secret@localhost:5432/rio",
            PgAuthMode::Password,
        )
        .await
        .unwrap();
        let opts = ts.fresh_options().await.unwrap();
        assert_eq!(opts.get_host(), "localhost");
        assert_eq!(opts.get_port(), 5432);
        assert_eq!(opts.get_username(), "rio");
        assert!(matches!(opts.get_ssl_mode(), PgSslMode::Prefer));
    }

    /// Runtime credential failure (expired/revoked IRSA web identity —
    /// construction-time presence checks can't catch it) must surface
    /// as a mint error from fresh_options, not a panic and not a
    /// stale-token success. This is the path the
    /// rio_pg_iam_mint_failures_total tripwire counts.
    #[tokio::test]
    async fn fresh_options_surfaces_runtime_credential_failure() {
        let bundle = temp_bundle();
        let url = iam_url(&bundle);
        let ts = TokenSource::new_with_sdk_for_tests(&url, failing_sdk())
            .await
            .unwrap();

        let err = ts.fresh_options().await.unwrap_err();
        assert!(
            err.to_string().contains("credential resolution failed"),
            "got: {err:#}"
        );
        assert_eq!(ts.mint_count(), 0, "failed mint must not count as success");
    }

    /// fresh_options reuses a young token (one mint for back-to-back
    /// calls) and re-mints once the cache passes FRESH_WINDOW — the
    /// per-attempt-call contract for connect retry loops.
    #[tokio::test]
    async fn fresh_options_caches_within_window_and_remints_after() {
        let bundle = temp_bundle();
        let url = format!(
            "postgres://rio_app@db.example.com:5432/rio?sslmode=verify-full&sslrootcert={}",
            bundle.path().display()
        );
        let ts = TokenSource::new_with_sdk_for_tests(&url, test_sdk())
            .await
            .unwrap();

        let a = ts.fresh_options().await.unwrap();
        let b = ts.fresh_options().await.unwrap();
        assert_eq!(ts.mint_count(), 1, "second call within window must reuse");
        assert_eq!(a.get_host(), b.get_host());

        ts.force_stale_for_tests().await;
        let _ = ts.fresh_options().await.unwrap();
        assert_eq!(ts.mint_count(), 2, "stale cache must re-mint");
    }

    /// The Debug impl is a redaction boundary: TokenSource carries the
    /// URL password (password mode) and a cached bearer token (iam
    /// mode), and `{:?}` output lands in logs via error contexts. It
    /// must surface host+mode only.
    #[tokio::test]
    async fn debug_output_carries_no_credential_material() {
        let ts = TokenSource::new(
            "postgres://rio:supersecret@localhost:5432/rio",
            PgAuthMode::Password,
        )
        .await
        .unwrap();
        let dbg = format!("{ts:?}");
        assert!(
            dbg.contains("localhost") && dbg.contains("Password"),
            "{dbg}"
        );
        assert!(!dbg.contains("supersecret"), "password leaked: {dbg}");

        let bundle = temp_bundle();
        let ts = TokenSource::new_with_sdk_for_tests(&iam_url(&bundle), test_sdk())
            .await
            .unwrap();
        let _ = ts.fresh_options().await.unwrap(); // populate the token cache
        let dbg = format!("{ts:?}");
        assert!(
            !dbg.contains("X-Amz-Signature") && !dbg.contains("AKIDEXAMPLE"),
            "token/credential material leaked: {dbg}"
        );
    }

    /// The production construction path: `TokenSource::new` in iam
    /// mode resolves the ambient SDK chain — the same env-variable
    /// chain IRSA populates (AWS_REGION + web-identity credentials).
    /// Env-provider static credentials keep it hermetic (no IMDS/STS
    /// call: region and credentials both short-circuit at the env
    /// step), and the follow-up mint proves the resolved SdkConfig is
    /// actually usable, not just present.
    #[tokio::test]
    async fn new_iam_resolves_ambient_env_chain_and_mints() {
        let bundle = temp_bundle();
        let url = iam_url(&bundle);
        // SAFETY: process-global env. Under nextest each test is its
        // own process; under `cargo test` no other rio-common test
        // reads the AWS chain (s3::default_client is never invoked in
        // tests). Removed again below.
        unsafe {
            std::env::set_var("AWS_REGION", "eu-central-1");
            std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
            std::env::set_var("AWS_SECRET_ACCESS_KEY", "testsecretkey");
        }
        let result = TokenSource::new(&url, PgAuthMode::Iam).await;
        let mint = match &result {
            Ok(ts) => ts.fresh_options().await.map(|o| o.get_host().to_string()),
            Err(_) => Ok(String::new()),
        };
        unsafe {
            std::env::remove_var("AWS_REGION");
            std::env::remove_var("AWS_ACCESS_KEY_ID");
            std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        }
        let ts = result.expect("iam construction must resolve the env chain");
        assert_eq!(ts.mint_count(), 1, "fresh_options must have minted once");
        assert_eq!(mint.unwrap(), "db.example.com");
    }

    /// First refresher iteration: nothing cached → mint immediately,
    /// then reschedule relative to that mint (next due is ~10 minutes
    /// out — the count must NOT keep climbing). A hot-looping or
    /// never-minting refresher both fail this.
    #[tokio::test]
    async fn refresher_mints_immediately_then_reschedules() {
        let bundle = temp_bundle();
        let url = iam_url(&bundle);
        let ts = std::sync::Arc::new(
            TokenSource::new_with_sdk_for_tests(&url, test_sdk())
                .await
                .unwrap(),
        );
        let opts: PgConnectOptions = url.parse().unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(opts);

        ts.spawn_refresher(pool);
        let deadline = Instant::now() + Duration::from_secs(5);
        while ts.mint_count() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(ts.mint_count(), 1, "refresher must mint immediately");
        assert!(
            ts.last_mint().await.is_some(),
            "mint must populate the cache"
        );

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            ts.mint_count(),
            1,
            "next refresh must be mint-relative (~10min out), not a hot loop"
        );
    }

    /// Refresher mint-failure arm: the task must survive (warn +
    /// retry-interval backoff), leave the cache empty, and NOT panic —
    /// a dead refresher silently downgrades the pod to "new
    /// connections fail 15 minutes after the last good mint".
    #[tokio::test]
    async fn refresher_survives_mint_failure() {
        let bundle = temp_bundle();
        let url = iam_url(&bundle);
        let ts = std::sync::Arc::new(
            TokenSource::new_with_sdk_for_tests(&url, failing_sdk())
                .await
                .unwrap(),
        );
        let opts: PgConnectOptions = url.parse().unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(opts);

        ts.spawn_refresher(pool);
        // Give the first (immediately-due) iteration time to fail.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(ts.mint_count(), 0, "mint must have failed");
        assert!(
            ts.last_mint().await.is_none(),
            "failed mint must not populate the cache"
        );
        // The task is now in its retry sleep; reaching this line at
        // all proves the failure arm neither panicked nor exited.
    }

    /// Password mode spawns no refresher at all — there is nothing to
    /// refresh, and a spurious task would mint against a URL that
    /// already carries its credential.
    #[tokio::test]
    async fn refresher_is_noop_in_password_mode() {
        let url = "postgres://rio:secret@localhost:5432/rio";
        let ts = std::sync::Arc::new(TokenSource::new(url, PgAuthMode::Password).await.unwrap());
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(url.parse().unwrap());
        ts.spawn_refresher(pool);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ts.mint_count(), 0);
        assert!(ts.last_mint().await.is_none());
    }
}

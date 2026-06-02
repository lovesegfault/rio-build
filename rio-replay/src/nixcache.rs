//! cache.nixos.org narinfo client (mass per-path ground truth; Fastly
//! CDN, so no request budget — but the same descriptive User-Agent).
//!
//! Lookups take `&self`, so one client can serve many concurrent
//! narinfo fetches; bulk callers are expected to bound their own
//! concurrency politely.

use std::collections::{BTreeMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};

use anyhow::Context as _;
use futures_util::StreamExt;
use rio_nix::narinfo::NarInfo;
use rio_nix::store_path::StorePath;

/// One normalized binary-cache base URL: parsed once, query/fragment
/// stripped, and the path slash-terminated. Owns the single narinfo/object
/// join implementation every cache client in this crate uses — and with it
/// the same-origin screen on object names ([`Self::object_url`]), so no
/// caller can be steered off the cache by an absolute URL smuggled into an
/// object name (e.g. a narinfo's `URL:` field).
///
/// nix.conf substituter strings often carry parameters (`?priority=40`,
/// `?trusted=1`); they tune client-side substituter selection, not how
/// objects are fetched, so they are stripped here (with one log line saying
/// so). The trailing slash matters because RFC 3986 relative resolution
/// replaces the last path segment of a base that lacks one — exactly how a
/// `/prefix?priority=10` cache URL used to lose its prefix when the slash
/// was appended to the raw string and landed inside the query.
///
/// Construction performs no trust screening; the trust levels live above
/// it: [`CacheUrl`] (operator-supplied, normalized only) and the
/// archive-screened substituter types minted by the validators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedCacheBase {
    /// Invariant: hierarchical, no query, no fragment, path ends with `/`.
    base: reqwest::Url,
}

impl NormalizedCacheBase {
    /// Parse and normalize a cache URL. Errors when the URL does not parse
    /// or is not hierarchical (object names cannot be joined onto it).
    pub(crate) fn parse(url: &str) -> anyhow::Result<Self> {
        let mut base =
            reqwest::Url::parse(url).with_context(|| format!("parse cache URL {url:?}"))?;
        anyhow::ensure!(
            !base.cannot_be_a_base(),
            "cache URL {url:?} is not a hierarchical URL; object names cannot be joined onto it"
        );
        let ignored = match (base.query(), base.fragment()) {
            (None, None) => None,
            (query, fragment) => Some(format!(
                "{}{}",
                query.map(|q| format!("?{q}")).unwrap_or_default(),
                fragment.map(|f| format!("#{f}")).unwrap_or_default()
            )),
        };
        base.set_query(None);
        base.set_fragment(None);
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        if let Some(ignored) = ignored {
            tracing::warn!(
                cache = %base,
                ignored = %ignored,
                "ignoring cache URL parameters; they do not affect how objects are fetched"
            );
        }
        Ok(Self { base })
    }

    /// `<base><hash-part>.narinfo` for a full store path — THE narinfo join.
    pub fn narinfo_url(&self, store_path: &str) -> anyhow::Result<reqwest::Url> {
        let parsed = StorePath::parse(store_path)
            .map_err(|e| anyhow::anyhow!("not a store path: {store_path}: {e}"))?;
        self.object_url(&format!("{}.narinfo", parsed.hash_part()))
    }

    /// Join a cache-relative object name (`<hash>.narinfo`, `nar/….nar.zst`)
    /// onto the base. The base's trailing slash guarantees the join appends
    /// to the path instead of replacing its last segment.
    ///
    /// The joined URL must stay on the base's origin (scheme, host, and
    /// port unchanged). Object names are cache-relative by convention — a
    /// narinfo's `URL:` field is a `nar/<hash>.nar.xz`-style path — but
    /// narinfo bodies come from the cache server, and RFC 3986 resolution
    /// replaces the base wholesale when the input is an absolute URL:
    /// without this screen, an admitted-but-hostile cache could steer a NAR
    /// fetch at an arbitrary endpoint by inlining one into a narinfo field,
    /// sidestepping the substituter admission screen (which vets the base)
    /// and the per-hop redirect screen (which only sees `Location`
    /// headers). Cross-origin results are refused naming the offending URL;
    /// absolute SAME-origin spellings are accepted, since no endpoint the
    /// base did not already reach becomes reachable. Caches that
    /// legitimately hand object fetches to another host (CDN layouts) do so
    /// via HTTP redirects, which the engine-facing clients re-screen per
    /// hop instead (see `substituter_redirect_policy`).
    pub fn object_url(&self, object: &str) -> anyhow::Result<reqwest::Url> {
        let base = &self.base;
        let url = base
            .join(object.trim_start_matches('/'))
            .with_context(|| format!("join object {object:?} onto cache base {base}"))?;
        // Compare scheme/host/port directly rather than via `Url::origin`:
        // the WHATWG origin of non-special schemes is opaque (never equal,
        // not even to itself), which would spuriously refuse relative joins
        // onto exotic operator-supplied bases.
        let same_origin = url.scheme() == base.scheme()
            && url.host() == base.host()
            && url.port_or_known_default() == base.port_or_known_default();
        anyhow::ensure!(
            same_origin,
            "refusing cache object {object:?}: it resolves to {url}, which leaves the origin \
             of the cache base {base}; object names (a narinfo's URL field included) are \
             cache-relative paths like \"nar/<hash>.nar.xz\", so a cross-origin absolute URL \
             is anomalous — caches that relocate objects to another host do so via redirects, \
             which are screened per hop"
        );
        Ok(url)
    }

    /// The normalized base URL string (slash-terminated, parameter-free).
    pub fn as_str(&self) -> &str {
        self.base.as_str()
    }
}

impl std::fmt::Display for NormalizedCacheBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.base, f)
    }
}

/// Operator-trust cache URL: the recorder's `--cache-url` and loopback
/// fixtures in tests. Normalized through [`NormalizedCacheBase`] but NOT
/// screened — the operator chose the endpoint, so internal mirrors and
/// plain-http dev caches are legitimate here.
///
/// This is one of the two trust levels a [`NixCacheClient`] can be built
/// from; the other is an archive-nominated substituter, which additionally
/// passes the public-HTTPS screen before the engine will probe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheUrl {
    base: NormalizedCacheBase,
}

impl CacheUrl {
    /// Parse and normalize an operator-supplied cache URL (no screening).
    pub fn parse(url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            base: NormalizedCacheBase::parse(url)?,
        })
    }
}

/// One archive-manifest substituter entry, classified once against the
/// admission screen. This is the archive trust level: the lists are
/// external input, so what each entry may be used for is decided here —
/// by [`classify_substituter`], the only mint — and nowhere else.
///
/// Classification is total: a rejected entry becomes [`Unusable`] instead
/// of an error, so an already-published (write-once) archive whose lists
/// carry an entry the screen rejects still opens and replays — the entry
/// is skipped, and an error surfaces only at a point of use that has no
/// usable alternative.
///
/// [`Unusable`]: ArchiveSubstituterUrl::Unusable
#[derive(Debug, Clone)]
pub enum ArchiveSubstituterUrl {
    /// Public-HTTPS cache: usable for live narinfo probing (and supply).
    Https(HttpsSubstituter),
    /// `s3://bucket[/prefix]` cache: usable by the supply stage's relay
    /// fetches (object access goes through the AWS endpoint), but not
    /// probeable over HTTP.
    S3(S3Substituter),
    /// An entry the screen rejected (unsupported scheme, non-public host,
    /// unparseable URL), kept with the reason so consumers can log or
    /// report it without re-deriving the verdict.
    Unusable {
        /// The entry as the manifest spelled it.
        url: String,
        /// Why the screen rejected it.
        reason: String,
    },
}

/// A public-HTTPS substituter minted by [`classify_substituter`] — the only
/// thing [`NixCacheClient::for_substituter`] accepts, so an archive-supplied
/// URL cannot reach the engine's probe client without passing the screen.
#[derive(Debug, Clone)]
pub struct HttpsSubstituter {
    base: NormalizedCacheBase,
}

impl HttpsSubstituter {
    /// The screened, normalized cache base.
    pub fn base(&self) -> &NormalizedCacheBase {
        &self.base
    }
}

/// An `s3://` substituter minted by [`classify_substituter`]: bucket-name
/// screened; the supply stage's client does its own parameter handling.
#[derive(Debug, Clone)]
pub struct S3Substituter {
    url: String,
}

impl S3Substituter {
    /// The entry as the manifest spelled it (for the supply admission).
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl ArchiveSubstituterUrl {
    /// The entry's URL for logs and reports (normalized for the https
    /// variant, verbatim otherwise).
    pub fn url(&self) -> &str {
        match self {
            Self::Https(https) => https.base.as_str(),
            Self::S3(s3) => &s3.url,
            Self::Unusable { url, .. } => url,
        }
    }
}

/// Classify one archive-manifest substituter entry against the admission
/// screen: `https://` entries must pass [`validate_probe_substituter`]
/// (public host) and normalize cleanly; `s3://` entries must pass
/// [`validate_supply_substituter`]'s bucket screen; everything else is
/// [`ArchiveSubstituterUrl::Unusable`] with the screen's reason. Total —
/// never an error — so archive open/bootstrap cannot fail on a list entry.
pub fn classify_substituter(url: &str) -> ArchiveSubstituterUrl {
    let unusable = |reason: anyhow::Error| ArchiveSubstituterUrl::Unusable {
        url: url.to_string(),
        reason: format!("{reason:#}"),
    };
    let scheme = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed.scheme().to_string(),
        Err(e) => {
            return unusable(anyhow::anyhow!(
                "substituter {url:?} is not a valid URL: {e}"
            ));
        }
    };
    match scheme.as_str() {
        "https" => {
            match validate_probe_substituter(url).and_then(|()| NormalizedCacheBase::parse(url)) {
                Ok(base) => ArchiveSubstituterUrl::Https(HttpsSubstituter { base }),
                Err(err) => unusable(err),
            }
        }
        "s3" => match validate_supply_substituter(url) {
            Ok(()) => ArchiveSubstituterUrl::S3(S3Substituter {
                url: url.to_string(),
            }),
            Err(err) => unusable(err),
        },
        _ => match validate_supply_substituter(url) {
            Err(err) => unusable(err),
            // The supply validator admits only https/s3, so reaching here
            // would mean the two screens disagree — refuse rather than mint.
            Ok(()) => unusable(anyhow::anyhow!(
                "substituter {url:?} uses unsupported scheme {scheme:?}"
            )),
        },
    }
}

/// The archive manifest's substituter lists, classified entry by entry
/// (see [`classify_substituter`]). Built once at campaign bootstrap from
/// [`crate::archive::schema::Substituters`].
#[derive(Debug, Clone)]
pub struct ClassifiedSubstituters {
    /// Advisory list of caches the recorder expected the target's tenants
    /// to use; never scheme-checked at archive open, so any entry here may
    /// be unusable.
    pub target: Vec<ArchiveSubstituterUrl>,
    /// Caches the engine may relay from. The v1 writer refuses non-https/s3
    /// entries at finalize (and v1 open re-checks), but v0 recordings carry
    /// their recorded list verbatim, so any entry here may be unusable —
    /// the screen (scheme and public host) is applied per entry, here.
    pub relay: Vec<ArchiveSubstituterUrl>,
}

impl ClassifiedSubstituters {
    /// Classify every entry of the manifest's lists. Total: unusable
    /// entries are carried as [`ArchiveSubstituterUrl::Unusable`].
    pub fn classify(substituters: &crate::archive::schema::Substituters) -> Self {
        Self {
            target: substituters
                .target
                .iter()
                .map(|url| classify_substituter(url))
                .collect(),
            relay: substituters
                .relay
                .iter()
                .map(|url| classify_substituter(url))
                .collect(),
        }
    }

    /// Every entry, target list first then relay — the probe-selection
    /// precedence (the target list describes what the recorded clients'
    /// own substituters served, so it is the better coverage signal).
    pub fn iter(&self) -> impl Iterator<Item = &ArchiveSubstituterUrl> {
        self.target.iter().chain(self.relay.iter())
    }

    /// The first probeable (public-HTTPS) entry across target then relay,
    /// skipping s3 and unusable entries instead of failing on them. `None`
    /// when no entry is probeable — the caller decides whether that matters
    /// (only the warm-set coverage probe needs one).
    pub fn first_probeable(&self) -> Option<&HttpsSubstituter> {
        self.iter().find_map(|entry| match entry {
            ArchiveSubstituterUrl::Https(https) => Some(https),
            _ => None,
        })
    }
}

/// cache.nixos.org (or any Nix binary cache) narinfo reader.
pub struct NixCacheClient {
    http: reqwest::Client,
    base: NormalizedCacheBase,
}

impl NixCacheClient {
    /// Build a narinfo client for an operator-supplied [`CacheUrl`] (the
    /// recorder's `--cache-url`, test fixtures).
    pub fn new(cache: &CacheUrl, user_agent: &str) -> anyhow::Result<Self> {
        Self::with_base(cache.base.clone(), user_agent)
    }

    /// Build a narinfo client for an archive-nominated substituter. Accepts
    /// only the screened [`HttpsSubstituter`] (minted by
    /// [`classify_substituter`]), so "validated URL" and "constructed
    /// client" can never disagree: an archive entry that did not pass the
    /// public-HTTPS screen has no way to reach this constructor.
    pub fn for_substituter(
        substituter: &HttpsSubstituter,
        user_agent: &str,
    ) -> anyhow::Result<Self> {
        Self::with_base(substituter.base.clone(), user_agent)
    }

    /// Shared construction: every [`NixCacheClient`] — whatever trust level
    /// minted its base — gets the same hardened HTTP client.
    ///
    /// Deliberately not built on `crate::http_client` (the recorder-side
    /// constructor for Hydra and tarball downloads, which legitimately
    /// follow redirects wherever they lead): cache base URLs come from
    /// replay archives and operator flags, so this client re-screens every
    /// redirect hop through `substituter_redirect_policy` — a screened
    /// cache must not be able to 302 the engine at an endpoint the screen
    /// would have rejected.
    fn with_base(base: NormalizedCacheBase, user_agent: &str) -> anyhow::Result<Self> {
        // Same construction shape as `crate::http_client`: try the
        // platform trust store first, fall back to an explicit empty root
        // store when none is available (the hermetic test sandbox); see
        // there for the full rationale.
        let builder = || {
            reqwest::Client::builder()
                .user_agent(user_agent)
                .timeout(std::time::Duration::from_secs(60))
                .redirect(substituter_redirect_policy())
        };
        let http = match builder().build() {
            Ok(client) => client,
            Err(_) => builder()
                .tls_certs_only(std::iter::empty())
                .build()
                .context("build cache HTTP client")?,
        };
        Ok(Self { http, base })
    }

    /// `<base><hash-part>.narinfo` for a full store path (the one join,
    /// via [`NormalizedCacheBase::narinfo_url`]).
    pub fn narinfo_url(&self, store_path: &str) -> anyhow::Result<reqwest::Url> {
        self.base.narinfo_url(store_path)
    }

    /// Fetch a narinfo as raw text. 404 ⇒ `Ok(None)` (path not upstream);
    /// any other non-200 is an error carrying a body snippet. The campaign
    /// engine's upstream-coverage probe uses this form so it can decide how
    /// to record a body that fails to parse.
    pub async fn fetch_narinfo_text(&self, store_path: &str) -> anyhow::Result<Option<String>> {
        let url = self.narinfo_url(store_path)?;
        tracing::debug!(%url, "cache GET");
        let resp = self
            .http
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        match resp.status() {
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => Ok(Some(
                resp.text()
                    .await
                    .with_context(|| format!("read body from {url}"))?,
            )),
            s => anyhow::bail!(
                "GET {url}: HTTP {s}: {}",
                crate::body_snippet(&resp.text().await.unwrap_or_default())
            ),
        }
    }

    /// Fetch and parse a narinfo. 404 ⇒ `Ok(None)` (path not upstream);
    /// any other non-200 or an unparseable body is an error.
    pub async fn fetch_narinfo(&self, store_path: &str) -> anyhow::Result<Option<NarInfo>> {
        match self.fetch_narinfo_text(store_path).await? {
            None => Ok(None),
            Some(text) => {
                Ok(Some(NarInfo::parse(&text).map_err(|e| {
                    anyhow::anyhow!("parse narinfo for {store_path}: {e}")
                })?))
            }
        }
    }
}

/// Validate a substituter URL taken from a replay archive's manifest
/// (the `substituters.target` / `substituters.relay` lists) before the
/// campaign engine points a [`NixCacheClient`] at it for live narinfo
/// probing.
///
/// The archive is external input, so the substituter it nominates must not
/// be able to steer the engine's HTTP client at internal endpoints. A URL is
/// accepted only if it parses, its scheme is exactly `https`, and — when the
/// host is an IP literal — the address is not unspecified (0.0.0.0, ::),
/// loopback, link-local (169.254.0.0/16, fe80::/10), RFC 1918 private space
/// (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16), CGNAT shared space
/// (100.64.0.0/10), or IPv6 unique-local (fc00::/7). IPv4-mapped IPv6
/// literals are unwrapped so `[::ffff:10.0.0.1]` cannot bypass the IPv4
/// ranges. The check is purely syntactic: hostnames are never resolved, so a
/// DNS name that happens to resolve to a private address is out of scope
/// here.
///
/// Redirects are not a way around this screen: it covers only the URL the
/// client is pointed at, so the [`NixCacheClient`] handed a validated URL
/// re-screens every redirect hop with the same contract (see
/// `substituter_redirect_policy`).
pub fn validate_probe_substituter(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| {
        format!(
            "archive substituter {url:?} is not a valid URL; refusing to probe narinfos from it"
        )
    })?;
    anyhow::ensure!(
        parsed.scheme() == "https",
        "archive substituter {url:?} uses scheme {:?}: archive substituters used for live \
         narinfo probing must be public HTTPS caches",
        parsed.scheme()
    );
    let host = parsed
        .host_str()
        .with_context(|| format!("archive substituter {url:?} has no host"))?;
    if let Some(ip) = non_public_ip_literal(host) {
        anyhow::bail!(
            "archive substituter {url:?} points at the non-public address {ip}: archive \
             substituters used for live narinfo probing must be public HTTPS caches"
        );
    }
    Ok(())
}

/// Validate a substituter URL provided by the campaign spec
/// (`supply.target_substituters`) or by a replay archive's manifest before
/// the supply stage hands it to the engine's binary-cache client
/// ([`crate::substituter::Substituter`]).
///
/// Accepted forms:
///
/// - `https://` caches whose host passes the same non-public-address screen
///   as [`validate_probe_substituter`];
/// - `s3://bucket[/prefix]` caches — object access goes through the AWS
///   endpoint resolved from the ambient configuration, and URL parameters
///   that would redirect it (`endpoint`, `scheme`, `profile`) are rejected
///   by the substituter client itself.
///
/// Everything else — plain `http://`, `file://`, loopback or private-address
/// HTTPS hosts — is rejected: these URLs are external input and must not be
/// able to steer the engine's HTTP client at internal endpoints. Like the
/// probe validator, the check is purely syntactic (no DNS resolution), and
/// like there, redirects cannot bypass it: the substituter's HTTP client
/// re-screens every redirect hop with the same contract (see
/// `substituter_redirect_policy`).
pub fn validate_supply_substituter(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url)
        .with_context(|| format!("supply substituter {url:?} is not a valid URL"))?;
    match parsed.scheme() {
        "https" => {
            let host = parsed
                .host_str()
                .with_context(|| format!("supply substituter {url:?} has no host"))?;
            if let Some(ip) = non_public_ip_literal(host) {
                anyhow::bail!(
                    "supply substituter {url:?} points at the non-public address {ip}: \
                     substituter URLs from the campaign spec or an archive manifest must be \
                     public HTTPS caches or s3:// buckets"
                );
            }
            Ok(())
        }
        "s3" => {
            anyhow::ensure!(
                parsed.host_str().is_some_and(|bucket| !bucket.is_empty()),
                "supply substituter {url:?} has no S3 bucket name"
            );
            Ok(())
        }
        other => anyhow::bail!(
            "supply substituter {url:?} uses scheme {other:?}: substituter URLs from the \
             campaign spec or an archive manifest must be public HTTPS caches or s3:// buckets"
        ),
    }
}

/// IP-literal screening shared by the substituter URL validators (no DNS
/// resolution). `host` is a URL host as returned by `Url::host_str`, which
/// keeps the square brackets around IPv6 literals, so they are stripped
/// before parsing; anything that does not parse as an address is a hostname
/// and passes (`None`). Returns the parsed address when it is unspecified
/// (0.0.0.0, ::), loopback, link-local (169.254.0.0/16, fe80::/10), RFC 1918
/// private space (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16), CGNAT shared
/// space (100.64.0.0/10), or IPv6 unique-local (fc00::/7). IPv4-mapped IPv6
/// literals are unwrapped so `[::ffff:10.0.0.1]` cannot bypass the IPv4
/// ranges.
fn non_public_ip_literal(host: &str) -> Option<IpAddr> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let ip = bare.parse::<IpAddr>().ok()?;
    let private_v4 = |v4: Ipv4Addr| {
        // 100.64.0.0/10 is the RFC 6598 carrier-grade NAT shared address space.
        let cgnat = (u32::from(v4) & 0xffc0_0000) == 0x6440_0000;
        v4.is_unspecified() || v4.is_loopback() || v4.is_link_local() || v4.is_private() || cgnat
    };
    let private = match ip {
        IpAddr::V4(v4) => private_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => private_v4(v4),
            None => {
                // fc00::/7 is the RFC 4193 unique-local address (ULA) range.
                let ula = (v6.segments()[0] & 0xfe00) == 0xfc00;
                v6.is_unspecified() || v6.is_loopback() || v6.is_unicast_link_local() || ula
            }
        },
    };
    private.then_some(ip)
}

/// Redirect-chain cap for the engine-facing substituter clients. A custom
/// reqwest policy replaces the built-in loop protection, so reqwest's
/// default limit of 10 hops is re-imposed here.
const MAX_REDIRECT_HOPS: usize = 10;

/// Redirect policy for the engine-facing substituter clients
/// ([`NixCacheClient`] and the HTTP client in [`crate::substituter`]).
///
/// [`validate_probe_substituter`] / [`validate_supply_substituter`] screen
/// the URL an archive or campaign spec nominates, but that is only the
/// first hop: a host that passes the screen can still answer every request
/// with a redirect, and a default reqwest client would follow it anywhere —
/// straight past the screen to loopback, link-local, or RFC 1918 space.
/// This policy re-applies the screen's contract to every hop via
/// [`validate_redirect_hop`]: a redirect may stay on the origin that issued
/// it (e.g. a relative `Location` relocating an object within the cache) or
/// move to public HTTPS space (the cache-in-front-of-CDN layout, where NAR
/// requests bounce to a different public host); anything else aborts the
/// request with an error naming the refused target. Chains longer than
/// [`MAX_REDIRECT_HOPS`] are refused, matching the default policy's cap.
pub(crate) fn substituter_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECT_HOPS {
            return attempt.error(format!(
                "substituter redirect chain exceeded {MAX_REDIRECT_HOPS} hops"
            ));
        }
        let Some(from) = attempt.previous().last() else {
            // Unreachable — a redirect always has at least the original URL
            // in its chain — but refuse rather than guess if it ever isn't.
            return attempt.error("substituter redirect with an empty redirect chain");
        };
        match validate_redirect_hop(from, attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(err) => attempt.error(err),
        }
    })
}

/// Screen one redirect hop from `from` (the URL that answered with the
/// redirect) to `next` (the resolved target) for the engine-facing
/// substituter clients — the per-hop core of
/// [`substituter_redirect_policy`].
///
/// A hop is accepted when it stays on the origin that issued it (scheme,
/// host, and port unchanged — the server is redirecting to itself, so no
/// endpoint the initial screen did not already vet becomes reachable), or
/// when the target is public HTTPS: scheme exactly `https` and a host that
/// passes [`non_public_ip_literal`]. Everything else — scheme downgrades,
/// loopback/link-local/private/CGNAT/ULA targets — is an error naming the
/// refused target. Like the URL validators above, the check is purely
/// syntactic: hostnames are never resolved.
fn validate_redirect_hop(from: &reqwest::Url, next: &reqwest::Url) -> anyhow::Result<()> {
    if next.origin() == from.origin() {
        return Ok(());
    }
    anyhow::ensure!(
        next.scheme() == "https",
        "refusing substituter redirect from {from} to {next}: redirects leaving the cache's \
         origin must use https"
    );
    let host = next.host_str().with_context(|| {
        format!("refusing substituter redirect from {from} to {next}: target has no host")
    })?;
    if let Some(ip) = non_public_ip_literal(host) {
        anyhow::bail!(
            "refusing substituter redirect from {from} to {next}: the target points at the \
             non-public address {ip}; substituter redirects must stay on the cache's own \
             origin or move to a public HTTPS host"
        );
    }
    Ok(())
}

/// Upstream narinfo facts for one store path, as collected by
/// [`sweep_narinfos`]: presence plus the NAR identity (NarHash digest and
/// NarSize) when the narinfo carried a usable hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarinfoFact {
    /// The cache served a narinfo for this path (HTTP 200). A 200 body
    /// that fails to parse still counts as found — the path demonstrably
    /// exists upstream — it just carries no usable NAR identity.
    pub found: bool,
    /// The narinfo's `NarHash`, present only when the narinfo parsed and
    /// the hash decoded cleanly through [`crate::narhash::NarHash::parse`].
    pub nar_hash: Option<crate::narhash::NarHash>,
    /// `NarSize` in bytes, present whenever the narinfo parsed.
    pub nar_size: Option<u64>,
}

/// Retry policy for transient cache errors (5xx, connection resets)
/// during a bulk sweep. `Backoff` has no `Default` by design — per-site
/// constants stay local (`rio-common/src/backoff.rs`); these match the
/// campaign engine's narinfo sweep so recorder and engine retry the same
/// upstream with the same cadence. Full jitter desynchronizes the
/// concurrent fetchers so retries don't re-arrive as a synchronized
/// burst.
const SWEEP_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: std::time::Duration::from_millis(500),
    mult: 2.0,
    cap: std::time::Duration::from_secs(15),
    jitter: rio_common::backoff::Jitter::Full,
};

/// Emit a sweep-progress log line every this many completed fetches, so
/// an operator watching the logs can tell a long-but-moving sweep apart
/// from one wedged in retry backoff.
const SWEEP_PROGRESS_LOG_EVERY: usize = 500;

/// Fetch one path's narinfo with bounded retries (transient errors only)
/// and fold it into a [`NarinfoFact`]. A 200 body that fails to parse,
/// or whose `NarHash` does not decode, is recorded as found with no
/// usable hash: the path demonstrably exists upstream, so treating it as
/// absent would mis-classify it, but its hash cannot be compared.
async fn fetch_one_fact(
    client: &NixCacheClient,
    path: &str,
    max_attempts: u32,
) -> anyhow::Result<NarinfoFact> {
    let mut attempt = 0u32;
    let text = loop {
        match client.fetch_narinfo_text(path).await {
            Ok(text) => break text,
            Err(e) if attempt + 1 < max_attempts => {
                let delay = SWEEP_BACKOFF.duration(attempt);
                tracing::warn!(path, attempt, error = %e, "narinfo sweep fetch failed; retrying");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => return Err(e.context(format!("narinfo sweep fetch for {path}"))),
        }
    };
    Ok(match text {
        None => NarinfoFact {
            found: false,
            nar_hash: None,
            nar_size: None,
        },
        Some(text) => match NarInfo::parse(&text) {
            Ok(ni) => {
                let nar_hash = match crate::narhash::NarHash::parse(&ni.nar_hash) {
                    Ok(hash) => Some(hash),
                    Err(e) => {
                        tracing::warn!(path, error = %format!("{e:#}"), "narinfo NarHash unusable; recorded as found without a hash");
                        None
                    }
                };
                NarinfoFact {
                    found: true,
                    nar_hash,
                    nar_size: Some(ni.nar_size),
                }
            }
            Err(e) => {
                tracing::warn!(path, error = %e, "malformed narinfo treated as found (hash unusable)");
                NarinfoFact {
                    found: true,
                    nar_hash: None,
                    nar_size: None,
                }
            }
        },
    })
}

/// Bulk narinfo sweep: fetch every distinct path in `paths` with bounded
/// concurrency and per-path retries, returning each path's upstream
/// presence and NAR identity. This is how the recorder acquires ground
/// truth at archive-creation time (cache.nixos.org sits behind a CDN, so
/// there is no request budget — `concurrency` is the politeness bound).
///
/// Duplicate input paths are fetched once. A malformed store path is a
/// hard error up front (a bug in the caller's path set, not a transient
/// fetch failure), and a path that still fails after `max_attempts` is
/// reached aborts the sweep with an error naming it.
pub async fn sweep_narinfos(
    client: &NixCacheClient,
    paths: &[String],
    concurrency: usize,
    max_attempts: u32,
) -> anyhow::Result<BTreeMap<String, NarinfoFact>> {
    // Validate before fetching anything: fail fast on malformed paths
    // instead of discovering them mid-sweep after spending fetches.
    for path in paths {
        let _ =
            StorePath::parse(path).map_err(|e| anyhow::anyhow!("bad store path {path}: {e}"))?;
    }
    // Dedupe preserving first occurrence so each path is fetched once.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut want: Vec<&str> = Vec::new();
    for path in paths {
        if seen.insert(path.as_str()) {
            want.push(path.as_str());
        }
    }
    let to_fetch = want.len();
    tracing::info!(to_fetch, "narinfo sweep starting");

    let mut fetches = futures_util::stream::iter(want.into_iter().map(|path| async move {
        let fact = fetch_one_fact(client, path, max_attempts).await;
        (path, fact)
    }))
    .buffer_unordered(concurrency.max(1));

    let mut facts = BTreeMap::new();
    let mut completed = 0usize;
    while let Some((path, fact)) = fetches.next().await {
        let fact = fact?;
        completed += 1;
        facts.insert(path.to_string(), fact);
        if completed.is_multiple_of(SWEEP_PROGRESS_LOG_EVERY) {
            tracing::info!(completed, to_fetch, "narinfo sweep progress");
        }
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::HeaderMap;

    const HELLO_PATH: &str = "/nix/store/10s5j3mfdg22k1597x580qrhprnzcjwb-hello-2.12.3";

    /// Operator-trust client for a test base URL.
    fn client(base: &str) -> NixCacheClient {
        NixCacheClient::new(&CacheUrl::parse(base).unwrap(), &crate::user_agent(None)).unwrap()
    }

    #[test]
    fn narinfo_url_uses_hash_part() {
        let c = client("https://cache.nixos.org");
        let url = c.narinfo_url(HELLO_PATH).unwrap();
        assert_eq!(
            url.as_str(),
            "https://cache.nixos.org/10s5j3mfdg22k1597x580qrhprnzcjwb.narinfo"
        );
        assert!(c.narinfo_url("not-a-store-path").is_err());
    }

    #[test]
    fn narinfo_url_preserves_path_prefix_and_strips_query() {
        // The idiomatic nix.conf substituter shape: path prefix plus
        // parameters (`?priority=N`). The parameters tune substituter
        // selection, not object fetching, so they are stripped — and the
        // prefix must survive the join instead of being replaced by it.
        for spelling in [
            "https://cache.example.org/prefix?priority=10",
            "https://cache.example.org/prefix",
            "https://cache.example.org/prefix/",
        ] {
            let c = client(spelling);
            assert_eq!(
                c.narinfo_url(HELLO_PATH).unwrap().as_str(),
                "https://cache.example.org/prefix/10s5j3mfdg22k1597x580qrhprnzcjwb.narinfo",
                "cache URL spelling {spelling:?}"
            );
        }
    }

    #[test]
    fn object_url_joins_relative_and_absolute_same_origin_names() {
        let base = NormalizedCacheBase::parse("https://cache.example.org/prefix").unwrap();
        // Relative names — the conventional narinfo URL field shapes.
        assert_eq!(
            base.object_url("nar/abcd.nar.xz").unwrap().as_str(),
            "https://cache.example.org/prefix/nar/abcd.nar.xz"
        );
        assert_eq!(
            base.object_url("abcd.narinfo").unwrap().as_str(),
            "https://cache.example.org/prefix/abcd.narinfo"
        );
        // Leading slashes are trimmed, keeping the name cache-relative.
        // This also defuses scheme-relative (`//host/...`) spellings: they
        // join as a path on the base instead of replacing its host.
        assert_eq!(
            base.object_url("/nar/abcd.nar.xz").unwrap().as_str(),
            "https://cache.example.org/prefix/nar/abcd.nar.xz"
        );
        assert_eq!(
            base.object_url("//evil.example.org/x").unwrap().as_str(),
            "https://cache.example.org/prefix/evil.example.org/x"
        );
        // Absolute spellings that stay on the cache's own origin are
        // accepted — the screen is about where a fetch can be steered, not
        // about the path within the cache.
        assert_eq!(
            base.object_url("https://cache.example.org/nar/abcd.nar.xz")
                .unwrap()
                .as_str(),
            "https://cache.example.org/nar/abcd.nar.xz"
        );
        // An explicit default port is the same origin.
        assert_eq!(
            base.object_url("https://cache.example.org:443/nar/abcd.nar.xz")
                .unwrap()
                .as_str(),
            "https://cache.example.org/nar/abcd.nar.xz"
        );
    }

    #[test]
    fn object_url_refuses_cross_origin_objects() {
        let base = NormalizedCacheBase::parse("https://cache.example.org/prefix").unwrap();
        let refused = [
            // Non-public addresses: the channel the substituter screens
            // exist to close (an admitted cache steering a fetch inward).
            "https://10.96.0.1/x",
            "https://169.254.169.254/latest/meta-data",
            // A public host is refused all the same: narinfo URL fields are
            // cache-relative by convention, so ANY cross-origin absolute
            // value is anomalous — CDN handoffs happen via redirects, which
            // the clients screen per hop.
            "https://evil.example.org/nar/abcd.nar.xz",
            // Same host, different scheme or port: a different origin.
            "http://cache.example.org/nar/abcd.nar.xz",
            "https://cache.example.org:8443/nar/abcd.nar.xz",
        ];
        for object in refused {
            let err = base
                .object_url(object)
                .expect_err(&format!("{object} must be refused as a cache object"));
            let msg = format!("{err:#}");
            assert!(
                msg.contains(object) && msg.contains("narinfo"),
                "error for {object} must name the offending URL and the narinfo URL field: {msg}"
            );
        }
    }

    #[test]
    fn screened_substituter_join_refuses_cross_origin_objects() {
        // The same-origin screen rides the shared join, so the screened
        // substituter trust level (validator-minted HttpsSubstituter, what
        // the probe client is built from) cannot be steered through it
        // either.
        let ArchiveSubstituterUrl::Https(https) = classify_substituter("https://cache.example.org")
        else {
            panic!("public https must classify as Https");
        };
        let err = https
            .base()
            .object_url("https://10.96.0.1/x")
            .expect_err("a cross-origin object must be refused on the screened base");
        assert!(
            format!("{err:#}").contains("https://10.96.0.1/x"),
            "error must name the refused URL"
        );
    }

    #[test]
    fn substituter_classification_screens_per_entry() {
        // Public HTTPS → probeable, carrying the normalized base (prefix
        // kept, parameters stripped).
        match classify_substituter("https://cache.example.org/prefix?priority=10") {
            ArchiveSubstituterUrl::Https(https) => {
                assert_eq!(https.base().as_str(), "https://cache.example.org/prefix/");
            }
            other => panic!("expected Https, got {other:?}"),
        }
        // s3 → supply-only, never probeable.
        match classify_substituter("s3://nix-cache-bucket/prefix?region=eu-central-1") {
            ArchiveSubstituterUrl::S3(s3) => {
                assert_eq!(s3.url(), "s3://nix-cache-bucket/prefix?region=eu-central-1");
            }
            other => panic!("expected S3, got {other:?}"),
        }
        // Everything the screen rejects becomes Unusable with a reason —
        // classification is total, so a published archive carrying such an
        // entry stays openable and replayable.
        for bad in [
            "http://internal-cache:8080",
            "https://10.0.0.1",
            "https://169.254.169.254/latest/meta-data",
            "ssh://build-cache.internal",
            "s3://",
            "not-a-url",
        ] {
            match classify_substituter(bad) {
                ArchiveSubstituterUrl::Unusable { url, reason } => {
                    assert_eq!(url, bad);
                    assert!(
                        reason.contains(bad) || reason.contains("not a valid URL"),
                        "reason for {bad} must name the entry: {reason}"
                    );
                }
                other => panic!("{bad} must classify as Unusable, got {other:?}"),
            }
        }
    }

    #[test]
    fn probe_selection_takes_first_usable_https_across_target_then_relay() {
        use crate::archive::schema::Substituters;
        // A bad target[0] (or an s3 target) with a good https relay must
        // select the relay instead of failing the campaign: format-valid
        // archives stay replayable, and every SELECTED entry still passed
        // the screen.
        let classified = ClassifiedSubstituters::classify(&Substituters {
            target: vec![
                "http://internal-cache:8080".into(),
                "s3://team-bucket".into(),
            ],
            relay: vec!["https://cache.nixos.org".into()],
        });
        let probe = classified
            .first_probeable()
            .expect("the https relay entry is probeable");
        assert_eq!(probe.base().as_str(), "https://cache.nixos.org/");

        // Target-list precedence: a usable target entry wins over the relay.
        let classified = ClassifiedSubstituters::classify(&Substituters {
            target: vec!["https://target.example.org".into()],
            relay: vec!["https://relay.example.org".into()],
        });
        assert_eq!(
            classified.first_probeable().unwrap().base().as_str(),
            "https://target.example.org/"
        );

        // s3-only relay (a format-valid archive shape): nothing is
        // probeable, but classification still succeeds — the probe is
        // simply absent and only its point of use may complain.
        let classified = ClassifiedSubstituters::classify(&Substituters {
            target: vec![],
            relay: vec!["s3://my-cache".into()],
        });
        assert!(classified.first_probeable().is_none());
        assert!(matches!(classified.relay[0], ArchiveSubstituterUrl::S3(_)));
    }

    #[test]
    fn probe_substituter_validation_accepts_public_https_caches() {
        validate_probe_substituter("https://cache.nixos.org").unwrap();
        // Path/query suffixes (Nix substituters often carry ?priority=N) and
        // public IP literals are fine — only the scheme and address class
        // are screened.
        validate_probe_substituter("https://cache.example.org/prefix?priority=10").unwrap();
        validate_probe_substituter("https://151.101.65.55").unwrap();
    }

    #[test]
    fn probe_substituter_validation_rejects_non_https_and_non_public_hosts() {
        let rejected = [
            // Wrong scheme: the probe only ever talks HTTPS.
            "http://cache.nixos.org",
            "s3://nix-cache-bucket",
            "file:///var/lib/nix-cache",
            // Loopback, link-local, and RFC 1918 IP literals (including the
            // IPv4-mapped IPv6 spelling of a private address).
            "https://127.0.0.1",
            "https://[::1]",
            "https://169.254.169.254/latest/meta-data",
            "https://[fe80::1]",
            "https://10.0.0.1",
            "https://172.16.0.1:8443",
            "https://192.168.1.5",
            "https://[::ffff:10.0.0.1]",
            // Unspecified, IPv6 unique-local, and CGNAT shared-space literals.
            "https://0.0.0.0",
            "https://[::]",
            "https://[fd12:3456:789a::1]",
            "https://100.64.0.1",
            // Not a URL at all.
            "not-a-url",
        ];
        for url in rejected {
            let err = validate_probe_substituter(url)
                .expect_err(&format!("{url} must be rejected as a probe substituter"));
            let msg = format!("{err:#}");
            assert!(
                msg.contains("archive substituter") && msg.contains(url),
                "error for {url} must name the archive-substituter origin and the URL: {msg}"
            );
        }
    }

    #[test]
    fn supply_substituter_validation_accepts_public_https_and_s3() {
        validate_supply_substituter("https://cache.nixos.org").unwrap();
        validate_supply_substituter("https://cache.example.org/prefix?priority=10").unwrap();
        validate_supply_substituter("https://151.101.65.55").unwrap();
        // S3 caches are reached through the AWS endpoint, not an attacker
        // chosen host, so bucket URLs (with or without prefix/region) pass.
        validate_supply_substituter("s3://nix-cache-bucket").unwrap();
        validate_supply_substituter("s3://nix-cache-bucket/prefix?region=eu-central-1").unwrap();
    }

    #[test]
    fn supply_substituter_validation_rejects_http_file_and_non_public_hosts() {
        let rejected = [
            // Plaintext HTTP and local files are never acceptable for
            // spec/archive-provided caches.
            "http://cache.nixos.org",
            "http://127.0.0.1:8080",
            "file:///var/lib/nix-cache",
            // Loopback, link-local, RFC 1918, CGNAT, ULA, and unspecified
            // HTTPS hosts: same screen as the probe validator.
            "https://127.0.0.1",
            "https://[::1]",
            "https://169.254.169.254/latest/meta-data",
            "https://10.0.0.1",
            "https://172.16.0.1:8443",
            "https://192.168.1.5",
            "https://[::ffff:10.0.0.1]",
            "https://100.64.0.1",
            "https://[fd12:3456:789a::1]",
            "https://0.0.0.0",
            // An s3 URL with no bucket, and garbage.
            "s3://",
            "not-a-url",
        ];
        for url in rejected {
            let err = validate_supply_substituter(url)
                .expect_err(&format!("{url} must be rejected as a supply substituter"));
            let msg = format!("{err:#}");
            assert!(
                msg.contains("supply substituter") && msg.contains(url),
                "error for {url} must name the supply-substituter origin and the URL: {msg}"
            );
        }
    }

    /// Loopback fake binary cache: serves one canned narinfo (for the
    /// real hello-2.12.3 store path; hash values are SYNTHETIC — the
    /// real upstream values are deliberately not asserted offline),
    /// one narinfo whose NarHash uses an unsupported algorithm (md5),
    /// one 200 body that is not a narinfo at all, one always-broken
    /// path, and 404s everything else. Requests without the rio-replay
    /// User-Agent get 406 so a politeness regression fails these tests.
    async fn spawn_fake_cache() -> (String, tokio::task::JoinHandle<()>) {
        use axum::response::IntoResponse;
        use axum::routing::get;
        async fn narinfo(
            axum::extract::Path(file): axum::extract::Path<String>,
            headers: HeaderMap,
        ) -> axum::response::Response {
            let ua_ok = headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ua| ua.starts_with("rio-replay/"));
            if !ua_ok {
                return (
                    axum::http::StatusCode::NOT_ACCEPTABLE,
                    "missing rio-replay User-Agent",
                )
                    .into_response();
            }
            if file == "10s5j3mfdg22k1597x580qrhprnzcjwb.narinfo" {
                let body = "\
StorePath: /nix/store/10s5j3mfdg22k1597x580qrhprnzcjwb-hello-2.12.3
URL: nar/0000000000000000000000000000000000000000000000000000.nar.xz
Compression: xz
FileHash: sha256:0000000000000000000000000000000000000000000000000000
FileSize: 50000
NarHash: sha256:0000000000000000000000000000000000000000000000000000
NarSize: 226504
References: 10s5j3mfdg22k1597x580qrhprnzcjwb-hello-2.12.3 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc-2.40
Deriver: 7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv
Sig: cache.nixos.org-1:c2lnbmF0dXJlLWJ5dGVzLW5vdC1yZWFsLWp1c3QtZml4dHVyZQ==
";
                (
                    [(axum::http::header::CONTENT_TYPE, "text/x-nix-narinfo")],
                    body,
                )
                    .into_response()
            } else if file == "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm.narinfo" {
                // Parses as a narinfo, but the NarHash algorithm is one the
                // sweep cannot convert to hex — the "found but hash
                // unusable" case.
                let body = "\
StorePath: /nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-md5hashed-1.0
URL: nar/1111111111111111111111111111111111111111111111111111.nar.xz
Compression: xz
NarHash: md5:0123456789abcdef0123456789abcdef
NarSize: 4242
";
                (
                    [(axum::http::header::CONTENT_TYPE, "text/x-nix-narinfo")],
                    body,
                )
                    .into_response()
            } else if file == "gggggggggggggggggggggggggggggggg.narinfo" {
                // A 200 response whose body does not parse as a narinfo at
                // all (e.g. a captive portal or a misrouted error page) —
                // the "found but no usable identity" case.
                (
                    [(axum::http::header::CONTENT_TYPE, "text/html")],
                    "<html><body>storage gateway maintenance page</body></html>",
                )
                    .into_response()
            } else if file == "cccccccccccccccccccccccccccccccc.narinfo" {
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "cache backend exploded",
                )
                    .into_response()
            } else {
                (axum::http::StatusCode::NOT_FOUND, "404").into_response()
            }
        }
        let app = axum::Router::new().route("/{file}", get(narinfo));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn fetch_narinfo_present_and_absent() {
        let (base, _srv) = spawn_fake_cache().await;
        let c = client(&base);

        let info = c.fetch_narinfo(HELLO_PATH).await.unwrap().expect("present");
        assert_eq!(info.store_path, HELLO_PATH);
        assert_eq!(info.nar_size, 226504);
        assert_eq!(
            info.deriver.as_deref(),
            Some("7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv")
        );
        assert_eq!(info.references.len(), 2);

        let absent = c
            .fetch_narinfo("/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-nope-1.0")
            .await
            .unwrap();
        assert!(absent.is_none(), "404 must map to Ok(None)");
    }

    #[tokio::test]
    async fn http_errors_name_the_url_and_include_a_body_snippet() {
        let (base, _srv) = spawn_fake_cache().await;
        let c = client(&base);
        let err = c
            .fetch_narinfo("/nix/store/cccccccccccccccccccccccccccccccc-broken-1.0")
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("500") && msg.contains("cccccccccccccccccccccccccccccccc.narinfo"),
            "got: {msg}"
        );
        assert!(msg.contains("cache backend exploded"), "got: {msg}");
    }

    #[tokio::test]
    async fn sweep_collects_facts_and_dedupes_paths() {
        let (base, _srv) = spawn_fake_cache().await;
        let c = client(&base);
        let absent = "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-nope-1.0".to_string();
        let paths = vec![
            HELLO_PATH.to_string(),
            HELLO_PATH.to_string(),
            absent.clone(),
        ];
        let facts = sweep_narinfos(&c, &paths, 4, 3).await.unwrap();
        assert_eq!(facts.len(), 2, "duplicate input paths are fetched once");
        let hello = &facts[HELLO_PATH];
        assert!(hello.found);
        assert_eq!(hello.nar_size, Some(226504));
        assert!(hello.nar_hash.is_some());
        let gone = &facts[&absent];
        assert!(!gone.found);
        assert!(gone.nar_hash.is_none());
    }

    #[tokio::test]
    async fn sweep_records_unusable_narhash_as_found_without_hash() {
        let (base, _srv) = spawn_fake_cache().await;
        let c = client(&base);
        let unusable = "/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-md5hashed-1.0".to_string();
        let facts = sweep_narinfos(&c, std::slice::from_ref(&unusable), 1, 2)
            .await
            .unwrap();
        let fact = &facts[&unusable];
        assert!(fact.found, "the cache served a narinfo, so the path exists");
        assert!(
            fact.nar_hash.is_none(),
            "an md5 NarHash cannot be converted to a hex sha256"
        );
        assert_eq!(fact.nar_size, Some(4242), "NarSize is still usable");
    }

    #[tokio::test]
    async fn sweep_records_unparseable_narinfo_body_as_found_without_identity() {
        // A 200 body that fails narinfo parsing entirely still proves the
        // path exists upstream, so it is recorded as found — but with no
        // NAR identity at all, which keeps the recorder's outcome mapping
        // at `built` without an output-hash entry rather than `unknown`.
        let (base, _srv) = spawn_fake_cache().await;
        let c = client(&base);
        let garbled = "/nix/store/gggggggggggggggggggggggggggggggg-garbled-1.0".to_string();
        let facts = sweep_narinfos(&c, std::slice::from_ref(&garbled), 1, 2)
            .await
            .unwrap();
        let fact = &facts[&garbled];
        assert!(fact.found, "a 200 body counts as present upstream");
        assert!(fact.nar_hash.is_none(), "no parseable NarHash");
        assert!(fact.nar_size.is_none(), "no parseable NarSize");
    }

    #[tokio::test]
    async fn sweep_propagates_persistent_errors() {
        let (base, _srv) = spawn_fake_cache().await;
        let c = client(&base);
        let broken = "/nix/store/cccccccccccccccccccccccccccccccc-broken-1.0".to_string();
        let err = sweep_narinfos(&c, &[broken], 2, 2).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("cccccccccccccccccccccccccccccccc"),
            "error names the path"
        );
    }

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn redirect_hop_screen_allows_same_origin_and_public_https() {
        // Same origin (scheme+host+port): the cache redirecting within
        // itself, e.g. a relative `Location`. Plain-http loopback is what
        // offline tests and dev flows use, so it must stay followable.
        validate_redirect_hop(
            &url("http://127.0.0.1:8080/a.narinfo"),
            &url("http://127.0.0.1:8080/moved/a.narinfo"),
        )
        .unwrap();
        validate_redirect_hop(
            &url("https://cache.example.org/a.narinfo"),
            &url("https://cache.example.org/b.narinfo?token=x"),
        )
        .unwrap();
        // Cross-origin to public HTTPS: the cache-in-front-of-CDN layout
        // (e.g. a Cachix-style 302 handing a NAR GET to a CDN host).
        validate_redirect_hop(
            &url("https://cache.example.org/nar/x.nar.zst"),
            &url("https://cdn.example.net/nar/x.nar.zst"),
        )
        .unwrap();
        validate_redirect_hop(
            &url("https://cache.example.org/x"),
            &url("https://151.101.65.55/x"),
        )
        .unwrap();
        // Same host but a different port is cross-origin — still fine while
        // it stays public https (ports are not screened, matching the URL
        // validators).
        validate_redirect_hop(
            &url("https://cache.example.org/x"),
            &url("https://cache.example.org:8443/x"),
        )
        .unwrap();
    }

    #[test]
    fn redirect_hop_screen_refuses_downgrades_and_non_public_targets() {
        let refused = [
            // Scheme downgrade off the issuing origin, even to the same
            // host: a plaintext hop would expose the request path and could
            // be re-steered by an on-path attacker.
            ("https://cache.example.org/x", "http://cache.example.org/x"),
            ("https://cache.example.org/x", "ftp://cache.example.org/x"),
            // Cross-origin plain http (different loopback port: leaving the
            // origin means the public-HTTPS contract applies).
            ("http://127.0.0.1:8080/x", "http://127.0.0.1:9090/x"),
            // Non-public targets across the screened address classes.
            ("https://cache.example.org/x", "https://10.0.0.1/x"),
            (
                "https://cache.example.org/x",
                "https://169.254.169.254/latest/meta-data",
            ),
            ("https://cache.example.org/x", "https://127.0.0.1/x"),
            ("https://cache.example.org/x", "https://[::1]/x"),
            ("https://cache.example.org/x", "https://[::ffff:10.0.0.1]/x"),
            ("https://cache.example.org/x", "https://192.168.1.5/x"),
            ("https://cache.example.org/x", "https://172.16.0.1:8443/x"),
            ("https://cache.example.org/x", "https://100.64.0.1/x"),
            (
                "https://cache.example.org/x",
                "https://[fd12:3456:789a::1]/x",
            ),
            ("https://cache.example.org/x", "https://0.0.0.0/x"),
        ];
        for (from, next) in refused {
            let next_url = url(next);
            let err = validate_redirect_hop(&url(from), &next_url)
                .expect_err(&format!("redirect {from} -> {next} must be refused"));
            let msg = format!("{err:#}");
            assert!(
                msg.contains("refusing substituter redirect") && msg.contains(next_url.as_str()),
                "error for {from} -> {next} must name the refused target: {msg}"
            );
        }
    }

    /// Loopback cache that answers narinfo GETs with redirects: one hash
    /// 302s within the same origin to `/moved/...` (where a valid narinfo
    /// is served), one 302s at a non-public HTTPS address, and one 302s to
    /// itself forever.
    async fn spawn_redirecting_cache() -> (String, tokio::task::JoinHandle<()>) {
        use axum::response::IntoResponse;
        use axum::routing::get;
        async fn narinfo(
            axum::extract::Path(file): axum::extract::Path<String>,
        ) -> axum::response::Response {
            match file.as_str() {
                "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr.narinfo" => (
                    axum::http::StatusCode::FOUND,
                    [(
                        axum::http::header::LOCATION,
                        "/moved/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr.narinfo",
                    )],
                )
                    .into_response(),
                "ssssssssssssssssssssssssssssssss.narinfo" => (
                    axum::http::StatusCode::FOUND,
                    [(
                        axum::http::header::LOCATION,
                        "https://10.0.0.1/internal.narinfo",
                    )],
                )
                    .into_response(),
                "llllllllllllllllllllllllllllllll.narinfo" => (
                    axum::http::StatusCode::FOUND,
                    [(
                        axum::http::header::LOCATION,
                        "/llllllllllllllllllllllllllllllll.narinfo",
                    )],
                )
                    .into_response(),
                _ => (axum::http::StatusCode::NOT_FOUND, "404").into_response(),
            }
        }
        async fn moved(
            axum::extract::Path(file): axum::extract::Path<String>,
        ) -> axum::response::Response {
            if file == "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr.narinfo" {
                let body = "\
StorePath: /nix/store/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-relocated-1.0
URL: nar/2222222222222222222222222222222222222222222222222222.nar.xz
Compression: xz
NarHash: sha256:0000000000000000000000000000000000000000000000000000
NarSize: 7777
";
                (
                    [(axum::http::header::CONTENT_TYPE, "text/x-nix-narinfo")],
                    body,
                )
                    .into_response()
            } else {
                (axum::http::StatusCode::NOT_FOUND, "404").into_response()
            }
        }
        let app = axum::Router::new()
            .route("/{file}", get(narinfo))
            .route("/moved/{file}", get(moved));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn same_origin_narinfo_redirect_is_followed() {
        let (base, _srv) = spawn_redirecting_cache().await;
        let c = client(&base);
        let info = c
            .fetch_narinfo("/nix/store/rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr-relocated-1.0")
            .await
            .unwrap()
            .expect("a same-origin redirect to the relocated narinfo must be followed");
        assert_eq!(info.nar_size, 7777);
    }

    #[tokio::test]
    async fn redirect_to_non_public_address_is_refused() {
        let (base, _srv) = spawn_redirecting_cache().await;
        let c = client(&base);
        let err = c
            .fetch_narinfo("/nix/store/ssssssssssssssssssssssssssssssss-redirector-1.0")
            .await
            .expect_err("a redirect at a non-public address must abort the fetch");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("https://10.0.0.1/internal.narinfo"),
            "error must name the refused redirect target: {msg}"
        );
        assert!(
            msg.contains("non-public address 10.0.0.1"),
            "error must name the non-public address: {msg}"
        );
    }

    #[tokio::test]
    async fn redirect_loops_are_capped() {
        // Same-origin hops are followed, so loop protection must come from
        // the policy's own hop cap (a custom reqwest policy replaces the
        // built-in one).
        let (base, _srv) = spawn_redirecting_cache().await;
        let c = client(&base);
        let err = c
            .fetch_narinfo("/nix/store/llllllllllllllllllllllllllllllll-loop-1.0")
            .await
            .expect_err("a redirect loop must error out, not spin");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("redirect chain exceeded"),
            "error must name the hop cap: {msg}"
        );
    }
}

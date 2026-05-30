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

/// Convert a narinfo `NarHash` value (`sha256:<52-char nixbase32>` as
/// served by cache.nixos.org, or `sha256:<64-char hex>` as stored by
/// rio-store) to lowercase hex. Anything else is an error.
///
/// Lives here rather than reusing [`rio_nix::hash::NixHash::parse_colon`]
/// because that parser accepts only the nixbase32 digest form, while this
/// helper also normalizes the already-hex form rio-store records.
pub fn narhash_to_hex(nar_hash: &str) -> anyhow::Result<String> {
    let rest = nar_hash
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("unsupported NarHash algo (want sha256:…): {nar_hash}"))?;
    match rest.len() {
        52 => {
            let bytes = rio_nix::store_path::nixbase32::decode(rest)
                .map_err(|e| anyhow::anyhow!("decode nixbase32 NarHash {nar_hash}: {e}"))?;
            anyhow::ensure!(
                bytes.len() == 32,
                "NarHash decoded to {} bytes, want 32",
                bytes.len()
            );
            Ok(hex::encode(bytes))
        }
        64 => {
            let bytes =
                hex::decode(rest).with_context(|| format!("decode hex NarHash {nar_hash}"))?;
            anyhow::ensure!(
                bytes.len() == 32,
                "NarHash decoded to {} bytes, want 32",
                bytes.len()
            );
            Ok(hex::encode(bytes))
        }
        n => anyhow::bail!("NarHash digest has unexpected length {n}: {nar_hash}"),
    }
}

/// cache.nixos.org (or any Nix binary cache) narinfo reader.
pub struct NixCacheClient {
    http: reqwest::Client,
    base: reqwest::Url,
}

impl NixCacheClient {
    pub fn new(base_url: &str, user_agent: &str) -> anyhow::Result<Self> {
        let mut base = base_url.to_string();
        if !base.ends_with('/') {
            base.push('/');
        }
        Ok(Self {
            http: crate::http_client(user_agent, std::time::Duration::from_secs(60))
                .context("build cache HTTP client")?,
            base: reqwest::Url::parse(&base).with_context(|| format!("parse cache URL {base}"))?,
        })
    }

    /// `<base>/<hash-part>.narinfo` for a full store path.
    pub fn narinfo_url(&self, store_path: &str) -> anyhow::Result<reqwest::Url> {
        let parsed = StorePath::parse(store_path)
            .map_err(|e| anyhow::anyhow!("not a store path: {store_path}: {e}"))?;
        self.base
            .join(&format!("{}.narinfo", parsed.hash_part()))
            .context("join narinfo URL")
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
/// probe validator, the check is purely syntactic (no DNS resolution).
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

/// Upstream narinfo facts for one store path, as collected by
/// [`sweep_narinfos`]: presence plus the NAR identity (lowercase hex
/// NarHash and NarSize) when the narinfo carried a usable hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarinfoFact {
    /// The cache served a narinfo for this path (HTTP 200). A 200 body
    /// that fails to parse still counts as found — the path demonstrably
    /// exists upstream — it just carries no usable NAR identity.
    pub found: bool,
    /// Lowercase hex sha256 NarHash, present only when the narinfo
    /// parsed and its `NarHash` converted cleanly via [`narhash_to_hex`].
    pub nar_hash_hex: Option<String>,
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
/// or whose `NarHash` cannot be converted to hex, is recorded as found
/// with no usable hash: the path demonstrably exists upstream, so
/// treating it as absent would mis-classify it, but its hash cannot be
/// compared.
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
            nar_hash_hex: None,
            nar_size: None,
        },
        Some(text) => match NarInfo::parse(&text) {
            Ok(ni) => {
                let nar_hash_hex = match narhash_to_hex(&ni.nar_hash) {
                    Ok(hex) => Some(hex),
                    Err(e) => {
                        tracing::warn!(path, error = %e, "narinfo NarHash unusable; recorded as found without a hash");
                        None
                    }
                };
                NarinfoFact {
                    found: true,
                    nar_hash_hex,
                    nar_size: Some(ni.nar_size),
                }
            }
            Err(e) => {
                tracing::warn!(path, error = %e, "malformed narinfo treated as found (hash unusable)");
                NarinfoFact {
                    found: true,
                    nar_hash_hex: None,
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

    #[test]
    fn narinfo_url_uses_hash_part() {
        let c = NixCacheClient::new("https://cache.nixos.org", &crate::user_agent(None)).unwrap();
        let url = c.narinfo_url(HELLO_PATH).unwrap();
        assert_eq!(
            url.as_str(),
            "https://cache.nixos.org/10s5j3mfdg22k1597x580qrhprnzcjwb.narinfo"
        );
        assert!(c.narinfo_url("not-a-store-path").is_err());
    }

    #[test]
    fn narhash_base32_to_hex_roundtrip() {
        // Use a known digest (sha256 of "hello") and rio-nix's own
        // encoder to build the cache.nixos.org-style NarHash, then
        // assert the conversion recovers the hex form.
        let digest =
            hex::decode("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
                .unwrap();
        let b32 = rio_nix::store_path::nixbase32::encode(&digest);
        let narhash = format!("sha256:{b32}");
        assert_eq!(
            narhash_to_hex(&narhash).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        // Independent known-value pair (not via rio-nix): the base32
        // form below is `nix-hash --type sha256 --to-base32` of the
        // same digest, so an encoder/decoder bug that cancels out in
        // the roundtrip above would still be caught here.
        assert_eq!(
            narhash_to_hex("sha256:094qif9n4cq4fdg459qzbhg1c6wywawwaaivx0k0x8xhbyx4vwic").unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        // Already-hex form is normalized (rio-store stores hex).
        assert_eq!(
            narhash_to_hex(
                "sha256:2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824"
            )
            .unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert!(narhash_to_hex("sha256:short").is_err());
        assert!(narhash_to_hex("md5:abcd").is_err());
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
        let c = NixCacheClient::new(&base, &crate::user_agent(None)).unwrap();

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
        let c = NixCacheClient::new(&base, &crate::user_agent(None)).unwrap();
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
        let c = NixCacheClient::new(&base, &crate::user_agent(None)).unwrap();
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
        assert!(hello.nar_hash_hex.is_some());
        let gone = &facts[&absent];
        assert!(!gone.found);
        assert!(gone.nar_hash_hex.is_none());
    }

    #[tokio::test]
    async fn sweep_records_unusable_narhash_as_found_without_hash() {
        let (base, _srv) = spawn_fake_cache().await;
        let c = NixCacheClient::new(&base, &crate::user_agent(None)).unwrap();
        let unusable = "/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-md5hashed-1.0".to_string();
        let facts = sweep_narinfos(&c, std::slice::from_ref(&unusable), 1, 2)
            .await
            .unwrap();
        let fact = &facts[&unusable];
        assert!(fact.found, "the cache served a narinfo, so the path exists");
        assert!(
            fact.nar_hash_hex.is_none(),
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
        let c = NixCacheClient::new(&base, &crate::user_agent(None)).unwrap();
        let garbled = "/nix/store/gggggggggggggggggggggggggggggggg-garbled-1.0".to_string();
        let facts = sweep_narinfos(&c, std::slice::from_ref(&garbled), 1, 2)
            .await
            .unwrap();
        let fact = &facts[&garbled];
        assert!(fact.found, "a 200 body counts as present upstream");
        assert!(fact.nar_hash_hex.is_none(), "no parseable NarHash");
        assert!(fact.nar_size.is_none(), "no parseable NarSize");
    }

    #[tokio::test]
    async fn sweep_propagates_persistent_errors() {
        let (base, _srv) = spawn_fake_cache().await;
        let c = NixCacheClient::new(&base, &crate::user_agent(None)).unwrap();
        let broken = "/nix/store/cccccccccccccccccccccccccccccccc-broken-1.0".to_string();
        let err = sweep_narinfos(&c, &[broken], 2, 2).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("cccccccccccccccccccccccccccccccc"),
            "error names the path"
        );
    }
}

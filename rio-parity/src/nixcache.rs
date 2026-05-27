//! cache.nixos.org narinfo client (mass per-path ground truth; Fastly
//! CDN, so no request budget — but the same descriptive User-Agent).
//!
//! Lookups take `&self`, so one client can serve many concurrent
//! narinfo fetches; bulk callers are expected to bound their own
//! concurrency politely.

use anyhow::Context as _;
use rio_nix::narinfo::NarInfo;
use rio_nix::store_path::StorePath;

/// Convert a narinfo `NarHash` value (`sha256:<52-char nixbase32>` as
/// served by cache.nixos.org, or `sha256:<64-char hex>` as stored by
/// rio-store) to lowercase hex. Anything else is an error.
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
            http: reqwest::Client::builder()
                .user_agent(user_agent)
                .timeout(std::time::Duration::from_secs(60))
                .build()
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

    /// Fetch and parse a narinfo. 404 ⇒ `Ok(None)` (path not upstream);
    /// any other non-200 is an error.
    pub async fn fetch_narinfo(&self, store_path: &str) -> anyhow::Result<Option<NarInfo>> {
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
            s if s.is_success() => {
                let text = resp
                    .text()
                    .await
                    .with_context(|| format!("read body from {url}"))?;
                let info = NarInfo::parse(&text)
                    .map_err(|e| anyhow::anyhow!("parse narinfo from {url}: {e}"))?;
                Ok(Some(info))
            }
            s => anyhow::bail!(
                "GET {url}: HTTP {s}: {}",
                crate::body_snippet(&resp.text().await.unwrap_or_default())
            ),
        }
    }
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

    /// Loopback fake binary cache: serves one canned narinfo (for the
    /// real hello-2.12.3 store path; hash values are SYNTHETIC — the
    /// real upstream values are deliberately not asserted offline),
    /// one always-broken path, and 404s everything else. Requests
    /// without the rio-parity User-Agent get 406 so a politeness
    /// regression fails these tests.
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
                .is_some_and(|ua| ua.starts_with("rio-parity/"));
            if !ua_ok {
                return (
                    axum::http::StatusCode::NOT_ACCEPTABLE,
                    "missing rio-parity User-Agent",
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
}

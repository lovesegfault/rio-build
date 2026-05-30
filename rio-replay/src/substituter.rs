//! Binary-cache (substituter) client — narinfo probing and NAR fetching
//! over HTTPS and S3.
//!
//! This is the engine's binary-cache client, used by the supply planner
//! (target-substituter narinfo coverage probes for the source ladder's
//! first rung, relay fetches for its third rung) and by recorders. It
//! asks caches two questions:
//!
//! - "do you have `<hash>`?" ([`Substituter::narinfo`]) — used to decide
//!   which store paths the target cluster can substitute on its own and
//!   which must be relayed;
//! - "give me the NAR behind this narinfo" ([`Substituter::fetch_nar`] /
//!   [`Substituter::fetch_nar_streaming`]) — the relay fetch: decompressed,
//!   optionally verified, then re-uploaded to the target.
//!
//! Caches are HTTPS (`https://cache.nixos.org`-style) or S3
//! (`s3://bucket/prefix?region=…`-style, as found in archive manifests).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::TryStreamExt as _;
use rio_nix::narinfo::NarInfo;
use rio_nix::store_path::nixbase32;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// TCP/TLS connect timeout for HTTP substituters. There is deliberately no
/// overall request timeout: NAR bodies can be multi-GB and legitimately
/// take minutes on a slow link.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Overall timeout for one narinfo probe. Probes are ~1 KB and the supply
/// planner issues thousands of them — a stalled server must not wedge a
/// probe slot forever. NAR fetches deliberately get NO overall timeout
/// (bodies can be huge); only [`CONNECT_TIMEOUT`] applies there.
const NARINFO_TIMEOUT: Duration = Duration::from_secs(30);

/// A binary cache reachable over HTTPS or S3.
///
/// Built once per `supply.target_substituters` entry / manifest source URL
/// by [`Substituter::parse`] and shared by the supply planner and prewarm
/// phases. All methods are async and hold no global state.
#[derive(Debug, Clone)]
pub enum Substituter {
    /// An HTTP(S) cache, e.g. `https://cache.nixos.org`.
    Http {
        /// Cache base URL; object names are appended to its path.
        base: reqwest::Url,
        /// Pooled client for this cache.
        client: reqwest::Client,
    },
    /// An S3-backed cache, e.g. `s3://my-cache/nix?region=eu-central-1`.
    S3 {
        /// S3 client built once at parse time (region from the URL when
        /// given, else the ambient AWS configuration).
        client: aws_sdk_s3::Client,
        /// Bucket name (the URL host).
        bucket: String,
        /// Key prefix (the URL path); empty or `…/`-terminated.
        prefix: String,
    },
}

impl Substituter {
    /// Parse a substituter URL: `https://…` / `http://…` or
    /// `s3://bucket[/prefix]?region=…`. Unsupported schemes are an error.
    ///
    /// For S3, the region comes from the `region` query parameter, falling
    /// back to the ambient AWS configuration (env / profile / IMDS) at call
    /// time. Credentials always come from the ambient configuration and are
    /// resolved lazily on first request, so parsing needs no network access.
    pub async fn parse(url: &str) -> Result<Self> {
        let parsed =
            reqwest::Url::parse(url).with_context(|| format!("invalid substituter URL {url:?}"))?;
        match parsed.scheme() {
            "http" | "https" => {
                let https = parsed.scheme() == "https";
                // Substituter strings copied out of nix.conf can carry
                // parameters (`?priority=40`, `?trusted=1`); they tune
                // client-side substituter selection, not how objects are
                // fetched, so strip them from the base (object names are
                // appended to its path) and say so once.
                let ignored = match (parsed.query(), parsed.fragment()) {
                    (None, None) => None,
                    (query, fragment) => Some(format!(
                        "{}{}",
                        query.map(|q| format!("?{q}")).unwrap_or_default(),
                        fragment.map(|f| format!("#{f}")).unwrap_or_default()
                    )),
                };
                let mut base = parsed;
                base.set_query(None);
                base.set_fragment(None);
                if let Some(ignored) = ignored {
                    tracing::warn!(
                        substituter = %base,
                        ignored = %ignored,
                        "ignoring substituter URL parameters; they do not affect how objects are fetched"
                    );
                }
                let mut builder = reqwest::Client::builder()
                    .user_agent(crate::user_agent(None))
                    .connect_timeout(CONNECT_TIMEOUT)
                    .https_only(https);
                if !https {
                    // Plaintext cache: TLS never engages, so skip loading
                    // the platform trust store — loading it fails outright
                    // in CA-bundle-less environments (e.g. the nix build
                    // sandbox) and a plain-http substituter doesn't need it.
                    builder = builder.tls_certs_only(std::iter::empty());
                }
                let client = builder.build().with_context(|| {
                    format!("failed to build the HTTP client for substituter {url}")
                })?;
                Ok(Self::Http { base, client })
            }
            "s3" => {
                let bucket = parsed
                    .host_str()
                    .filter(|host| !host.is_empty())
                    .ok_or_else(|| anyhow!("substituter URL {url} has no S3 bucket name"))?
                    .to_string();
                let mut prefix = parsed.path().trim_matches('/').to_string();
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                let mut region = None;
                let mut ignored: Vec<String> = Vec::new();
                for (key, value) in parsed.query_pairs() {
                    match key.as_ref() {
                        "region" => region = Some(value.into_owned()),
                        // Parameters that change WHERE or HOW the bucket is
                        // reached: silently dropping them would probe the
                        // wrong place and report misleading misses.
                        "endpoint" | "scheme" | "profile" => {
                            bail!("unsupported substituter parameter `{key}` in {url}")
                        }
                        _ => ignored.push(key.into_owned()),
                    }
                }
                if !ignored.is_empty() {
                    tracing::warn!(
                        substituter = url,
                        ignored = %ignored.join(", "),
                        "ignoring substituter URL parameters; they do not affect how objects are fetched"
                    );
                }
                // Per-substituter SDK config: the region is a property of
                // the URL, so the shared ambient-region client constructor
                // (`rio_common::s3::default_client`) is not a fit here.
                // Same `from_env()` provider chain otherwise.
                let mut loader = aws_config::from_env();
                if let Some(region) = region {
                    loader = loader.region(aws_config::Region::new(region));
                }
                let config = loader.load().await;
                let client = aws_sdk_s3::Client::new(&config);
                Ok(Self::S3 {
                    client,
                    bucket,
                    prefix,
                })
            }
            other => bail!(
                "unsupported substituter scheme {other:?} in {url} (supported: https://, http://, s3://)"
            ),
        }
    }

    /// The canonical URL string (for logs/reports).
    pub fn url(&self) -> String {
        match self {
            Self::Http { base, .. } => base.as_str().trim_end_matches('/').to_string(),
            Self::S3 { bucket, prefix, .. } => {
                if prefix.is_empty() {
                    format!("s3://{bucket}")
                } else {
                    format!("s3://{bucket}/{prefix}")
                }
            }
        }
    }

    /// Fetch and parse `<hash>.narinfo` for a store-path hash part.
    ///
    /// `Ok(None)` when the cache definitively does not have the path
    /// (HTTP 404 / S3 `NoSuchKey`). Authorization problems (HTTP 403 / S3
    /// `AccessDenied`) are errors — they must stay visible, never silently
    /// become "not present".
    pub async fn narinfo(&self, hash_part: &str) -> Result<Option<NarInfo>> {
        let object = format!("{hash_part}.narinfo");
        match self {
            Self::Http { base, client } => {
                let url = object_url(base, &object)?;
                let resp = client
                    .get(url.clone())
                    .timeout(NARINFO_TIMEOUT)
                    .send()
                    .await
                    .with_context(|| format!("GET {url}"))?;
                let status = resp.status();
                if status == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                if status == reqwest::StatusCode::FORBIDDEN {
                    bail!("{url}: HTTP 403 Forbidden — substituter denies access, not a miss");
                }
                if !status.is_success() {
                    bail!("{url}: HTTP {status}");
                }
                let text = resp
                    .text()
                    .await
                    .with_context(|| format!("read narinfo body from {url}"))?;
                let info =
                    NarInfo::parse(&text).with_context(|| format!("{url}: malformed narinfo"))?;
                Ok(Some(info))
            }
            Self::S3 {
                client,
                bucket,
                prefix,
            } => {
                let key = format!("{prefix}{object}");
                let send = client.get_object().bucket(bucket).key(&key).send();
                let resp = match tokio::time::timeout(NARINFO_TIMEOUT, send).await {
                    Err(_) => bail!(
                        "narinfo probe for s3://{bucket}/{key} timed out after {}s",
                        NARINFO_TIMEOUT.as_secs()
                    ),
                    Ok(Ok(resp)) => resp,
                    Ok(Err(err)) if err.as_service_error().is_some_and(|e| e.is_no_such_key()) => {
                        return Ok(None);
                    }
                    Ok(Err(err)) => {
                        return Err(anyhow::Error::new(err).context(format!(
                            "GET s3://{bucket}/{key} (access problems are not treated as a miss)"
                        )));
                    }
                };
                let bytes = resp
                    .body
                    .collect()
                    .await
                    .with_context(|| format!("read narinfo body from s3://{bucket}/{key}"))?
                    .into_bytes();
                let text = std::str::from_utf8(&bytes)
                    .with_context(|| format!("s3://{bucket}/{key}: narinfo is not valid UTF-8"))?;
                let info = NarInfo::parse(text)
                    .with_context(|| format!("s3://{bucket}/{key}: malformed narinfo"))?;
                Ok(Some(info))
            }
        }
    }

    /// Fetch the NAR named by `info.url`, decompress it per
    /// `info.compression` (`none`, `zstd`, `xz`, `bzip2`, `gzip`, `br`),
    /// verify the decompressed length against `info.nar_size` and its
    /// SHA-256 against `info.nar_hash`, and return the decompressed bytes.
    pub async fn fetch_nar(&self, info: &NarInfo) -> Result<Vec<u8>> {
        let (declared_size, reader) = self.fetch_nar_streaming(info).await?;
        // Pre-size from the (untrusted) declared size, capped so a bogus
        // narinfo can't make us reserve gigabytes up front.
        let reserve = usize::try_from(declared_size.min(64 * 1024 * 1024)).unwrap_or(0);
        let mut nar = Vec::with_capacity(reserve);
        // Bound the decompressed read at declared-size + 1: a decompression
        // bomb stays bounded no matter what the narinfo claimed, and the
        // size-mismatch check below then fires deterministically.
        let mut reader = reader.take(declared_size.saturating_add(1));
        reader
            .read_to_end(&mut nar)
            .await
            .with_context(|| format!("read NAR {} from {}", info.url, self.url()))?;
        ensure!(
            nar.len() as u64 == info.nar_size,
            "NAR size mismatch for {} from {}: narinfo declares {} bytes, got {}",
            info.store_path,
            self.url(),
            info.nar_size,
            nar.len()
        );
        let digest: [u8; 32] = Sha256::digest(&nar).into();
        let got = format!("sha256:{}", nixbase32::encode(&digest));
        ensure!(
            got == info.nar_hash,
            "NAR hash mismatch for {} from {}: narinfo declares {}, got {got}",
            info.store_path,
            self.url(),
            info.nar_hash,
        );
        Ok(nar)
    }

    /// Streaming variant of [`fetch_nar`](Self::fetch_nar) for large paths:
    /// returns the expected decompressed size (`info.nar_size`) and a reader
    /// of the decompressed bytes.
    ///
    /// No client-side hash verification — the daemon verifies on ingest and
    /// the caller knows the expected length.
    ///
    /// Multi-frame compressed bodies are truncated at the first frame by the
    /// decoders (the same behavior as the other decompression call sites in
    /// this codebase); the daemon's ingest verification catches the
    /// resulting short data, so callers should treat daemon-side rejection
    /// of a relayed path as "the cache served unusable content".
    pub async fn fetch_nar_streaming(
        &self,
        info: &NarInfo,
    ) -> Result<(u64, Box<dyn AsyncRead + Send + Unpin>)> {
        let raw: Box<dyn AsyncRead + Send + Unpin> = match self {
            Self::Http { base, client } => {
                let url = object_url(base, &info.url)?;
                let resp = client
                    .get(url.clone())
                    .send()
                    .await
                    .with_context(|| format!("GET {url}"))?;
                let status = resp.status();
                if status == reqwest::StatusCode::FORBIDDEN {
                    bail!("{url}: HTTP 403 Forbidden — substituter denies access");
                }
                if !status.is_success() {
                    bail!("{url}: HTTP {status}");
                }
                let stream_url = url.clone();
                let stream = resp
                    .bytes_stream()
                    .map_err(move |err| std::io::Error::other(format!("{stream_url}: {err}")));
                Box::new(tokio_util::io::StreamReader::new(stream))
            }
            Self::S3 {
                client,
                bucket,
                prefix,
            } => {
                let key = format!("{prefix}{}", info.url);
                let object = client
                    .get_object()
                    .bucket(bucket)
                    .key(&key)
                    .send()
                    .await
                    .with_context(|| format!("GET s3://{bucket}/{key}"))?;
                Box::new(object.body.into_async_read())
            }
        };
        let reader = decompress(raw, &info.compression, &self.url())?;
        Ok((info.nar_size, reader))
    }
}

/// Join a cache-relative object name (`<hash>.narinfo`, `nar/….nar.zst`)
/// onto the cache base URL.
fn object_url(base: &reqwest::Url, object: &str) -> Result<reqwest::Url> {
    let joined = format!(
        "{}/{}",
        base.as_str().trim_end_matches('/'),
        object.trim_start_matches('/')
    );
    reqwest::Url::parse(&joined)
        .with_context(|| format!("invalid substituter object URL {joined:?}"))
}

/// Wrap a raw (still-compressed) NAR body reader in a streaming decoder for
/// the narinfo `Compression` value. Unsupported kinds are an error naming
/// the compression and the cache.
fn decompress(
    raw: Box<dyn AsyncRead + Send + Unpin>,
    compression: &str,
    cache: &str,
) -> Result<Box<dyn AsyncRead + Send + Unpin>> {
    use async_compression::tokio::bufread::{
        BrotliDecoder, BzDecoder, GzipDecoder, XzDecoder, ZstdDecoder,
    };

    let buffered = tokio::io::BufReader::new(raw);
    Ok(match compression {
        "" | "none" => Box::new(buffered),
        "zstd" => Box::new(ZstdDecoder::new(buffered)),
        "xz" => Box::new(XzDecoder::new(buffered)),
        "bzip2" => Box::new(BzDecoder::new(buffered)),
        "gzip" => Box::new(GzipDecoder::new(buffered)),
        "br" => Box::new(BrotliDecoder::new(buffered)),
        other => bail!(
            "unsupported NAR compression {other:?} from substituter {cache} (supported: none, zstd, xz, bzip2, gzip, br)"
        ),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    use super::*;

    /// Minimal HTTP/1.1 server: reads the request line, drains headers, and
    /// answers with the canned `(status, body)` registered for the path
    /// (404 for anything else). Returns the base URL and the accept-loop
    /// task (aborted on drop at test end).
    async fn spawn_test_server(
        routes: HashMap<String, (u16, Vec<u8>)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes = Arc::new(routes);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let (read_half, mut write_half) = socket.split();
                    let mut lines = tokio::io::BufReader::new(read_half);
                    let mut request_line = String::new();
                    if lines.read_line(&mut request_line).await.is_err() {
                        return;
                    }
                    loop {
                        let mut header = String::new();
                        match lines.read_line(&mut header).await {
                            Ok(0) | Err(_) => return,
                            Ok(_) if header == "\r\n" || header == "\n" => break,
                            Ok(_) => {}
                        }
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    let (status, body) = routes
                        .get(&path)
                        .cloned()
                        .unwrap_or((404, b"not found".to_vec()));
                    let reason = match status {
                        200 => "OK",
                        403 => "Forbidden",
                        404 => "Not Found",
                        _ => "Other",
                    };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = write_half.write_all(head.as_bytes()).await;
                    let _ = write_half.write_all(&body).await;
                    let _ = write_half.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    /// zstd-compress `data` with the same async-compression codec the
    /// fetch path decodes with.
    async fn zstd_compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = async_compression::tokio::bufread::ZstdEncoder::new(data);
        let mut out = Vec::new();
        encoder.read_to_end(&mut out).await.unwrap();
        out
    }

    /// xz-compress `data` (used to cover the second supported codec).
    async fn xz_compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = async_compression::tokio::bufread::XzEncoder::new(data);
        let mut out = Vec::new();
        encoder.read_to_end(&mut out).await.unwrap();
        out
    }

    /// Hand-built narinfo whose hash/size describe `nar` and whose `URL`
    /// points at `url` with the given compression.
    fn narinfo_for(nar: &[u8], url: &str, compression: &str) -> NarInfo {
        let digest: [u8; 32] = Sha256::digest(nar).into();
        NarInfo {
            store_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-replay-test".into(),
            url: url.into(),
            compression: compression.into(),
            nar_hash: format!("sha256:{}", nixbase32::encode(&digest)),
            nar_size: nar.len() as u64,
            references: Vec::new(),
            deriver: None,
            sigs: Vec::new(),
            ca: None,
            file_hash: None,
            file_size: None,
        }
    }

    #[tokio::test]
    async fn parse_accepts_http_and_s3_and_rejects_others() {
        // Plain-http parses without touching the network (and without a
        // platform trust store).
        let http = Substituter::parse("http://127.0.0.1:1").await.unwrap();
        assert_eq!(http.url(), "http://127.0.0.1:1");
        assert!(matches!(http, Substituter::Http { .. }));

        // https needs the platform trust store; in CA-bundle-less
        // environments (the nix build sandbox) building the client fails by
        // design. Branch on what the default reqwest builder can do here so
        // both environments exercise their realistic outcome. The
        // `?priority=40` parameter (as found in nix.conf substituter lists)
        // must be stripped from the stored base either way.
        let https = Substituter::parse("https://cache.nixos.org?priority=40").await;
        if reqwest::Client::builder().build().is_ok() {
            assert_eq!(https.unwrap().url(), "https://cache.nixos.org");
        } else {
            let err = format!("{:#}", https.unwrap_err());
            assert!(err.contains("https://cache.nixos.org"), "{err}");
        }

        // S3 with region + prefix: no credentials and no network needed at
        // parse time.
        let s3 = Substituter::parse("s3://my-cache/some/prefix?region=eu-central-1")
            .await
            .unwrap();
        assert_eq!(s3.url(), "s3://my-cache/some/prefix/");
        match &s3 {
            Substituter::S3 { bucket, prefix, .. } => {
                assert_eq!(bucket, "my-cache");
                assert_eq!(prefix, "some/prefix/");
            }
            other => panic!("expected S3, got {other:?}"),
        }
        // Bare bucket, no prefix.
        let s3_bare = Substituter::parse("s3://other-cache?region=us-east-1")
            .await
            .unwrap();
        assert_eq!(s3_bare.url(), "s3://other-cache");

        // S3 parameters that change where/how the bucket is reached are
        // refused instead of silently ignored.
        let err = Substituter::parse("s3://my-cache?region=us-east-1&endpoint=http://minio.local")
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unsupported substituter parameter") && msg.contains("endpoint"),
            "{msg}"
        );

        // Unsupported scheme is an error naming the scheme.
        let err = Substituter::parse("ftp://example.org/cache")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("unsupported substituter scheme"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn narinfo_404_is_none_and_403_is_error() {
        let routes = HashMap::from([
            (
                "/missing0000000000000000000000000.narinfo".to_string(),
                (404, b"not here".to_vec()),
            ),
            (
                "/forbidden000000000000000000000000.narinfo".to_string(),
                (403, b"no".to_vec()),
            ),
        ]);
        let (base, server) = spawn_test_server(routes).await;
        let sub = Substituter::parse(&base).await.unwrap();

        let missing = sub
            .narinfo("missing0000000000000000000000000")
            .await
            .unwrap();
        assert!(missing.is_none(), "404 must map to Ok(None)");

        let forbidden = sub
            .narinfo("forbidden000000000000000000000000")
            .await
            .unwrap_err();
        let msg = format!("{forbidden:#}");
        assert!(msg.contains("403"), "403 must stay an error: {msg}");

        server.abort();
    }

    #[tokio::test]
    async fn narinfo_success_path_ignores_url_params() {
        let narinfo_text = "\
StorePath: /nix/store/b2222222222222222222222222222222-present
URL: nar/b2222222222222222222222222222222.nar.zst
Compression: zstd
NarHash: sha256:0000000000000000000000000000000000000000000000000000
NarSize: 4242
References:
";
        let routes = HashMap::from([(
            "/b2222222222222222222222222222222.narinfo".to_string(),
            (200, narinfo_text.as_bytes().to_vec()),
        )]);
        let (base, server) = spawn_test_server(routes).await;
        // `?priority=40` on the substituter URL must not leak into object
        // URLs — the canned server only answers the clean path.
        let sub = Substituter::parse(&format!("{base}?priority=40"))
            .await
            .unwrap();
        assert_eq!(sub.url(), base);

        let info = sub
            .narinfo("b2222222222222222222222222222222")
            .await
            .unwrap()
            .expect("present narinfo must be Some");
        assert_eq!(info.nar_size, 4242);
        assert_eq!(info.compression, "zstd");

        server.abort();
    }

    /// Probe outcomes against a loopback axum cache: a present narinfo
    /// parses to `Some`, a 404 is a definitive miss (`None`), and a 403
    /// stays an error so an authorization problem can never masquerade as
    /// "not cached". The fake cache also rejects requests without the
    /// rio-replay User-Agent, so a politeness regression fails this test.
    #[tokio::test]
    async fn http_probe_distinguishes_miss_from_denial() {
        use axum::response::IntoResponse;
        use axum::routing::get;

        async fn narinfo_route(
            axum::extract::Path(file): axum::extract::Path<String>,
            headers: axum::http::HeaderMap,
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
            match file.as_str() {
                "b2222222222222222222222222222222.narinfo" => {
                    let body = "\
StorePath: /nix/store/b2222222222222222222222222222222-present
URL: nar/b2222222222222222222222222222222.nar.zst
Compression: zstd
NarHash: sha256:0000000000000000000000000000000000000000000000000000
NarSize: 4242
References:
";
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/x-nix-narinfo")],
                        body,
                    )
                        .into_response()
                }
                "f3333333333333333333333333333333.narinfo" => {
                    (axum::http::StatusCode::FORBIDDEN, "denied").into_response()
                }
                _ => (axum::http::StatusCode::NOT_FOUND, "404").into_response(),
            }
        }

        let app = axum::Router::new().route("/{file}", get(narinfo_route));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let sub = Substituter::parse(&format!("http://{addr}")).await.unwrap();

        let present = sub
            .narinfo("b2222222222222222222222222222222")
            .await
            .unwrap()
            .expect("present narinfo must parse to Some");
        assert_eq!(present.nar_size, 4242);
        assert_eq!(present.compression, "zstd");

        let missing = sub
            .narinfo("m1111111111111111111111111111111")
            .await
            .unwrap();
        assert!(missing.is_none(), "404 must map to Ok(None)");

        let denied = sub
            .narinfo("f3333333333333333333333333333333")
            .await
            .unwrap_err();
        let msg = format!("{denied:#}");
        assert!(msg.contains("403"), "403 must stay an error: {msg}");

        server.abort();
    }

    #[tokio::test]
    async fn fetch_nar_decompresses_and_verifies() {
        let nar: Vec<u8> = b"replay fixture NAR payload 0123456789".repeat(64);
        let zstd_body = zstd_compress(&nar).await;
        let xz_body = xz_compress(&nar).await;
        // Same length as `nar`, different content: triggers the hash check
        // (not the size check).
        let mut corrupted = nar.clone();
        corrupted[0] ^= 0xff;
        let corrupted_zstd = zstd_compress(&corrupted).await;

        let routes = HashMap::from([
            ("/nar/good.nar.zst".to_string(), (200, zstd_body)),
            ("/nar/good.nar.xz".to_string(), (200, xz_body)),
            ("/nar/corrupt.nar.zst".to_string(), (200, corrupted_zstd)),
        ]);
        let (base, server) = spawn_test_server(routes).await;
        let sub = Substituter::parse(&base).await.unwrap();

        let zstd_info = narinfo_for(&nar, "nar/good.nar.zst", "zstd");
        assert_eq!(sub.fetch_nar(&zstd_info).await.unwrap(), nar);

        let xz_info = narinfo_for(&nar, "nar/good.nar.xz", "xz");
        assert_eq!(sub.fetch_nar(&xz_info).await.unwrap(), nar);

        // Body decompresses to the right length but the wrong bytes → the
        // error names the hash mismatch.
        let corrupt_info = narinfo_for(&nar, "nar/corrupt.nar.zst", "zstd");
        let err = format!("{:#}", sub.fetch_nar(&corrupt_info).await.unwrap_err());
        assert!(err.contains("hash mismatch"), "{err}");

        // Unsupported compression names the compression and the cache.
        let lzip_info = narinfo_for(&nar, "nar/good.nar.zst", "lzip");
        let err = format!("{:#}", sub.fetch_nar(&lzip_info).await.unwrap_err());
        assert!(err.contains("lzip") && err.contains(&sub.url()), "{err}");

        server.abort();
    }

    #[tokio::test]
    async fn fetch_nar_streaming_yields_decompressed_stream() {
        let nar: Vec<u8> = b"streaming fixture payload abcdefgh".repeat(128);
        let zstd_body = zstd_compress(&nar).await;
        let routes = HashMap::from([("/nar/stream.nar.zst".to_string(), (200, zstd_body))]);
        let (base, server) = spawn_test_server(routes).await;
        let sub = Substituter::parse(&base).await.unwrap();

        let info = narinfo_for(&nar, "nar/stream.nar.zst", "zstd");
        let (size, mut reader) = sub.fetch_nar_streaming(&info).await.unwrap();
        assert_eq!(size, nar.len() as u64);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, nar);

        server.abort();
    }
}

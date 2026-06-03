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
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _};

use crate::narhash::NarHash;
use crate::nixcache::NormalizedCacheBase;

/// TCP/TLS connect timeout for HTTP substituters. There is deliberately no
/// overall request timeout: NAR bodies can be multi-GB and legitimately
/// take minutes on a slow link.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Overall timeout for one narinfo probe, covering the request dispatch AND
/// the body read on both arms (reqwest's per-request timeout spans the body
/// on the HTTP arm; one `tokio::time::timeout` spans send + collect on the
/// S3 arm). Probes are ~1 KB and the supply planner issues thousands of
/// them — a stalled server must not wedge a probe slot forever, and a
/// deadline that releases at response headers would still leave the body
/// read unbounded. NAR fetches deliberately get NO overall timeout (bodies
/// can be huge); only [`CONNECT_TIMEOUT`] applies there, and their callers
/// own any header-phase deadline.
const NARINFO_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum narinfo body size buffered by a probe, on both arms. Real
/// narinfos are ~1 KB (a long references list reaches tens of KB), so
/// 1 MiB is pure headroom: the cap exists because probe bodies are
/// buffered wholesale at `narinfo_concurrency` fan-out, and a hostile or
/// misconfigured cache serving multi-GB objects under `<hash>.narinfo`
/// names must exhaust its budget here — loudly, naming the cap — instead
/// of OOMing the engine. NAR fetch bodies are NOT under this cap; they are
/// bounded by their own declared-size discipline (`fetch_nar`'s
/// `take(declared_size + 1)`). Shared with the operator-cache narinfo read
/// in [`crate::nixcache`] so every narinfo buffer in the crate sits under
/// the one cap.
pub(crate) const MAX_NARINFO_BYTES: u64 = 1024 * 1024;

/// A binary cache reachable over HTTPS or S3.
///
/// Built once per `supply.target_substituters` entry / manifest source URL
/// by [`Substituter::parse`] and shared by the supply planner and prewarm
/// phases. All methods are async and hold no global state.
#[derive(Debug, Clone)]
pub enum Substituter {
    /// An HTTP(S) cache, e.g. `https://cache.nixos.org`.
    Http {
        /// Normalized cache base; object names are joined onto its path
        /// through the shared [`NormalizedCacheBase`] join.
        base: NormalizedCacheBase,
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
    ///
    /// The HTTP client re-screens every redirect hop (see
    /// `crate::nixcache::substituter_redirect_policy`): a cache may
    /// redirect within its own origin or to a public HTTPS host, but a hop
    /// at a non-public address or a non-https scheme aborts the fetch —
    /// spec- and archive-provided caches must not be able to steer the
    /// engine at internal endpoints via `Location` headers. Narinfo `URL:`
    /// fields cannot steer it either, with a per-arm screen: the HTTP arm
    /// joins object names through [`NormalizedCacheBase::object_url`],
    /// which refuses values that leave the cache's origin, and the S3 arm
    /// derives its object keys through `s3_object_key`, which refuses
    /// names that do not stay strictly under the cache's key prefix.
    pub async fn parse(url: &str) -> Result<Self> {
        let parsed =
            reqwest::Url::parse(url).with_context(|| format!("invalid substituter URL {url:?}"))?;
        match parsed.scheme() {
            "http" | "https" => {
                let https = parsed.scheme() == "https";
                // Normalization (parameter stripping, trailing slash, the
                // object-URL join) is shared with every other cache client
                // via NormalizedCacheBase, so the two clients can never
                // again disagree on how a substituter string becomes an
                // object URL. nix.conf-style parameters (`?priority=40`)
                // are stripped there with a log line saying so.
                let base = NormalizedCacheBase::parse(url)?;
                let mut builder = reqwest::Client::builder()
                    .user_agent(crate::user_agent(None))
                    .connect_timeout(CONNECT_TIMEOUT)
                    // Cache URLs come from campaign specs and archive
                    // manifests; the admission screen covers only the URL
                    // itself, so every redirect hop is re-screened against
                    // the same contract (same origin, or public HTTPS).
                    .redirect(crate::nixcache::substituter_redirect_policy())
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
    ///
    /// Bounded in time by `NARINFO_TIMEOUT` (dispatch + body, both arms)
    /// and in size by `MAX_NARINFO_BYTES` (both arms).
    pub async fn narinfo(&self, hash_part: &str) -> Result<Option<NarInfo>> {
        self.narinfo_at(hash_part, NARINFO_TIMEOUT).await
    }

    /// [`narinfo`](Self::narinfo) with an explicit deadline, so tests can
    /// pin the deadline's scope (it must cover the body read, not just the
    /// request dispatch) without waiting out the production constant.
    async fn narinfo_at(&self, hash_part: &str, deadline: Duration) -> Result<Option<NarInfo>> {
        let object = format!("{hash_part}.narinfo");
        match self {
            Self::Http { base, client } => {
                let url = base.object_url(&object)?;
                // reqwest's per-request timeout covers connect, headers,
                // and every body read below — one deadline for the whole
                // probe, same scope as the S3 arm's wrapper.
                let resp = client
                    .get(url.clone())
                    .timeout(deadline)
                    // bounded-io: per-request timeout(deadline) spans
                    // dispatch, headers, and the body read below
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
                let stream_url = url.clone();
                let stream = resp
                    .bytes_stream()
                    .map_err(move |err| std::io::Error::other(format!("{stream_url}: {err}")));
                let bytes = read_capped_narinfo_body(
                    tokio_util::io::StreamReader::new(stream),
                    url.as_str(),
                )
                .await?;
                let text = std::str::from_utf8(&bytes)
                    .with_context(|| format!("{url}: narinfo is not valid UTF-8"))?;
                let info =
                    NarInfo::parse(text).with_context(|| format!("{url}: malformed narinfo"))?;
                Ok(Some(info))
            }
            Self::S3 {
                client,
                bucket,
                prefix,
            } => {
                let key = s3_object_key(bucket, prefix, &object)?;
                // ONE deadline over BOTH phases — the GetObject dispatch
                // and the body read. A timeout around the send alone would
                // discharge at response headers and leave the collect free
                // to pend on a stalled or trickling backend forever.
                let fetch = async {
                    // bounded-io: dispatch and body both run inside the
                    // probe deadline wrapped around this whole block
                    let resp = match client.get_object().bucket(bucket).key(&key).send().await {
                        Ok(resp) => resp,
                        Err(err) if err.as_service_error().is_some_and(|e| e.is_no_such_key()) => {
                            return Ok(None);
                        }
                        Err(err) => {
                            return Err(anyhow::Error::new(err).context(format!(
                                "GET s3://{bucket}/{key} (access problems are not treated as a \
                                 miss)"
                            )));
                        }
                    };
                    let bytes = read_capped_narinfo_body(
                        resp.body.into_async_read(),
                        &format!("s3://{bucket}/{key}"),
                    )
                    .await?;
                    Ok(Some(bytes))
                };
                let fetched = match tokio::time::timeout(deadline, fetch).await {
                    Err(_) => bail!(
                        "narinfo probe for s3://{bucket}/{key} timed out after {}s (the \
                         deadline covers the GET dispatch and the body read)",
                        deadline.as_secs()
                    ),
                    Ok(fetched) => fetched?,
                };
                let Some(bytes) = fetched else {
                    return Ok(None);
                };
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
            // bounded-io: size-capped by the take(declared_size + 1) above;
            // deliberately no overall deadline (NAR bodies are huge), the
            // streaming fetch's header phase is bounded by the caller
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
        // Digest-to-digest verification through the one NarHash decoder: a
        // byte-correct NAR must verify whatever spelling (nixbase32 or hex)
        // the cache's narinfo used, and an undecodable declared hash is its
        // own error — the NAR cannot be verified at all.
        let declared = NarHash::parse(&info.nar_hash).map_err(|err| {
            anyhow!(
                "narinfo NarHash {:?} for {} from {} is not decodable: {err:#}",
                info.nar_hash,
                info.store_path,
                self.url()
            )
        })?;
        let digest: [u8; 32] = Sha256::digest(&nar).into();
        ensure!(
            NarHash::from_digest(digest) == declared,
            "NAR hash mismatch for {} from {}: narinfo declares {}, got sha256:{}",
            info.store_path,
            self.url(),
            info.nar_hash,
            hex::encode(digest),
        );
        Ok(nar)
    }

    /// Streaming variant of [`fetch_nar`](Self::fetch_nar) for large paths:
    /// returns the expected decompressed size (`info.nar_size`) and a reader
    /// of the decompressed bytes.
    ///
    /// `info.url` comes from a narinfo body the cache served, so each
    /// transport arm screens it before fetching: the HTTP arm joins it
    /// through [`NormalizedCacheBase::object_url`], which refuses values
    /// that leave the cache's origin (absolute SAME-origin spellings stay
    /// fetchable), and the S3 arm derives the object key through
    /// `s3_object_key`, which refuses names that do not stay strictly
    /// under the cache's key prefix (no absolute spellings at all — keys
    /// have no origin to compare against). Either way a hostile narinfo
    /// cannot steer this fetch outside the cache the substituter URL
    /// admitted.
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
                let url = base.object_url(&info.url)?;
                let resp = client
                    .get(url.clone())
                    // bounded-io: header-phase wait; supply callers wrap
                    // this call in their op deadline, and the returned
                    // body reader is consumed under the wire op's
                    // payload-scaled deadline (no overall bound here by
                    // design: NAR bodies are huge)
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
                let key = s3_object_key(bucket, prefix, &info.url)?;
                let object = client
                    .get_object()
                    .bucket(bucket)
                    .key(&key)
                    // bounded-io: header-phase wait; same caller-owned
                    // deadline contract as the HTTP arm above, body
                    // consumed under the wire op's payload-scaled deadline
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

/// Derive the S3 object key for one cache-relative object name under the
/// cache's key prefix — the S3 arm's screen on untrusted object names, the
/// equivalent of the HTTP arm's [`NormalizedCacheBase::object_url`]
/// same-origin refusal.
///
/// Object names are cache-relative paths by convention (`<hash>.narinfo`,
/// `nar/<hash>.nar.xz` — the same convention `object_url` documents, from
/// the Nix binary-cache format where a narinfo's `URL:` field names an
/// object relative to the cache root). Narinfo bodies come from the cache
/// server, so the name is untrusted input. An S3 GET is *keyed* rather
/// than URL-resolved, which changes what a hostile name can buy compared
/// to the HTTP arm: a `..` segment escapes the configured key prefix on
/// S3-compatible backends and proxies that normalize key paths (the
/// engine's ambient credentials would then read same-bucket objects the
/// substituter URL never admitted), and an absolute-URL spelling splices
/// into a garbage key that fails as a mystifying `NoSuchKey` instead of an
/// anomaly-naming refusal. Refuse every name that does not stay strictly
/// under the prefix: scheme-carrying values, backslashes, and empty, `.`,
/// or `..` path segments (which also covers leading `/`).
///
/// Unlike the HTTP join there is no absolute same-origin leniency: keys
/// have no origin to compare against, so no absolute spelling can be
/// proven to stay on the admitted cache.
fn s3_object_key(bucket: &str, prefix: &str, object: &str) -> Result<String> {
    let refuse = |why: &str| {
        anyhow!(
            "refusing cache object {object:?} on s3://{bucket}/{prefix}: {why}; object names \
             (a narinfo's URL field included) are cache-relative paths like \
             \"nar/<hash>.nar.xz\" and must resolve under the cache's key prefix"
        )
    };
    if object.is_empty() {
        return Err(refuse("the name is empty"));
    }
    if object.contains("://") {
        return Err(refuse(
            "the name is an absolute URL, not a cache-relative path",
        ));
    }
    if object.contains('\\') {
        return Err(refuse("the name contains a backslash"));
    }
    if object
        .split('/')
        .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(refuse(
            "the name has a leading separator or an empty, `.`, or `..` path segment",
        ));
    }
    Ok(format!("{prefix}{object}"))
}

/// Read a narinfo body through a [`MAX_NARINFO_BYTES`]-bounded `take`, so a
/// probe can never buffer more than the cap no matter what the backend
/// serves: at most cap + 1 bytes are read, and a body that exceeds the cap
/// is an error naming it. Both probe arms (HTTP and S3) read through this
/// helper; time-boundedness is the caller's deadline (it spans this read on
/// both arms).
async fn read_capped_narinfo_body(reader: impl AsyncRead + Unpin, source: &str) -> Result<Vec<u8>> {
    let mut reader = reader.take(MAX_NARINFO_BYTES + 1);
    let mut bytes = Vec::new();
    reader
        // bounded-io: size-capped by the take(MAX_NARINFO_BYTES + 1)
        // above; time-bounded by the probe deadline both callers hold
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("read narinfo body from {source}"))?;
    ensure!(
        bytes.len() as u64 <= MAX_NARINFO_BYTES,
        "{source}: narinfo body exceeds the {MAX_NARINFO_BYTES}-byte probe cap — narinfos are \
         ~1 KB; refusing to buffer a body this large",
    );
    Ok(bytes)
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

    use rio_nix::store_path::nixbase32;
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
    async fn fetch_nar_verifies_hex_form_narhash() {
        // rio-store-recorded narinfos carry `sha256:<hex>`; a byte-correct
        // NAR must verify against that spelling exactly like the nixbase32
        // one — verification compares digests, not formatted strings.
        let nar: Vec<u8> = b"hex narinfo NAR payload 0123456789".repeat(64);
        let zstd_body = zstd_compress(&nar).await;
        let routes = HashMap::from([("/nar/hex.nar.zst".to_string(), (200, zstd_body))]);
        let (base, server) = spawn_test_server(routes).await;
        let sub = Substituter::parse(&base).await.unwrap();

        let digest: [u8; 32] = Sha256::digest(&nar).into();
        let mut info = narinfo_for(&nar, "nar/hex.nar.zst", "zstd");
        info.nar_hash = format!("sha256:{}", hex::encode(digest));
        assert_eq!(
            sub.fetch_nar(&info).await.unwrap(),
            nar,
            "a byte-correct NAR must verify against a hex-spelled NarHash"
        );

        // An undecodable NarHash fails the fetch naming the value — the NAR
        // cannot be verified — rather than reporting a spurious mismatch.
        let mut bad = narinfo_for(&nar, "nar/hex.nar.zst", "zstd");
        bad.nar_hash = "md5:0123".into();
        let err = format!("{:#}", sub.fetch_nar(&bad).await.unwrap_err());
        assert!(
            err.contains("md5:0123") && err.contains("not decodable"),
            "{err}"
        );

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

    /// The transport arm of a [`Substituter`], for tests that must
    /// enumerate the variant axis. Exhaustive on purpose: a new transport
    /// variant fails this match at compile time, forcing the hostile-name
    /// suite below to grow an arm for it before anything builds — the
    /// narinfo `URL:` screen is per-arm, so a per-entry-point test alone
    /// cannot prove a new arm is covered.
    fn arm_label(sub: &Substituter) -> &'static str {
        match sub {
            Substituter::Http { .. } => "http",
            Substituter::S3 { .. } => "s3",
        }
    }

    #[tokio::test]
    async fn fetch_nar_refuses_hostile_narinfo_url_on_every_arm() {
        // A cache that passed the admission screen can still serve hostile
        // narinfo BODIES, and the screen on the `URL:` field lives in each
        // transport arm of the fetch match — so the refusal is asserted
        // per (transport arm × hostile name × entry point), not just per
        // entry point over one hardcoded transport.
        //
        // Contract (Nix binary-cache format, as pinned by
        // `NormalizedCacheBase::object_url` and `s3_object_key`): a
        // narinfo `URL:` names an object relative to the cache root, like
        // "nar/<hash>.nar.xz". On the HTTP arm a violation steers the GET
        // off the admitted origin (RFC 3986 resolution replaces the base
        // wholesale); on the S3 arm it splices into the GetObject key,
        // escaping the admitted prefix on dot-normalizing backends.
        let (base, server) = spawn_test_server(HashMap::new()).await;
        let http = Substituter::parse(&base).await.unwrap();
        // Parsing s3:// needs no credentials and no network; the screen
        // fires before any request is built, so neither do the fetches.
        let s3 = Substituter::parse("s3://test-cache/some/prefix?region=us-east-1")
            .await
            .unwrap();

        // Absolute URLs at foreign endpoints are hostile on every arm.
        let common = [
            "https://10.96.0.1/x.nar.zst",
            "https://evil.example.org/nar/x.nar.zst",
        ];
        // Key splices the HTTP origin comparison cannot see: traversal out
        // of the prefix, absolute paths/spellings (the HTTP arm's
        // same-origin leniency has no keyed equivalent — `s3_object_key`
        // refuses ALL absolute spellings, its own bucket included),
        // backslashes, and empty segments.
        let s3_only = [
            "nar/../../other-prefix/x.nar.zst",
            "/nar/x.nar.zst",
            "s3://test-cache/some/prefix/nar/x.nar.zst",
            "nar\\x.nar.zst",
            "nar//x.nar.zst",
        ];

        let nar: Vec<u8> = b"hostile-name fixture".repeat(8);
        let arms: [(&Substituter, Vec<&str>); 2] = [
            (&http, common.to_vec()),
            (&s3, common.iter().chain(&s3_only).copied().collect()),
        ];
        // The refusal must name the offending value (verbatim or in its
        // Debug spelling — backslashes render escaped) and the narinfo
        // field the convention binds.
        let names_value = |err: &str, hostile: &str| {
            (err.contains(hostile) || err.contains(&format!("{hostile:?}")))
                && err.contains("narinfo")
        };
        for (sub, hostile_names) in arms {
            let arm = arm_label(sub);
            for hostile in hostile_names {
                let info = narinfo_for(&nar, hostile, "zstd");
                let err = format!("{:#}", sub.fetch_nar(&info).await.unwrap_err());
                assert!(
                    names_value(&err, hostile),
                    "[{arm}] fetch_nar of {hostile} must be refused naming the value and \
                     the narinfo field: {err}"
                );
                let err = format!(
                    "{:#}",
                    sub.fetch_nar_streaming(&info)
                        .await
                        .map(|_| ())
                        .unwrap_err()
                );
                assert!(
                    names_value(&err, hostile),
                    "[{arm}] fetch_nar_streaming of {hostile} must be refused naming the \
                     value and the narinfo field: {err}"
                );
            }
        }
        server.abort();
    }

    #[test]
    fn s3_object_key_admits_relative_names_and_refuses_escapes() {
        // Both directions of the S3 arm's screen, over the key derivation
        // itself (the arm is `s3_object_key` + an SDK call, so the key
        // shape IS the decision; no live S3 needed).
        //
        // Must-admit: the convention shapes the Nix binary-cache format
        // produces — `<hash>.narinfo` probes and `nar/…` objects — resolve
        // to exactly prefix + name, with and without a configured prefix.
        // Dots inside a segment are content, not traversal.
        for (prefix, object, want) in [
            ("some/prefix/", "abcd.narinfo", "some/prefix/abcd.narinfo"),
            (
                "some/prefix/",
                "nar/abcd.nar.xz",
                "some/prefix/nar/abcd.nar.xz",
            ),
            ("", "nar/abcd.nar.zst", "nar/abcd.nar.zst"),
            ("p/", "nar/x..y.nar.zst", "p/nar/x..y.nar.zst"),
        ] {
            assert_eq!(
                s3_object_key("test-cache", prefix, object).unwrap(),
                want,
                "cache-relative {object:?} under prefix {prefix:?}"
            );
        }

        // Must-block: anything that does not stay strictly under the
        // prefix, each refusal naming the value and the convention.
        for hostile in [
            "",
            "https://evil.example.org/x.nar.zst",
            "s3://test-cache/some/prefix/x.nar.zst",
            "/nar/x.nar.zst",
            "nar/../../escape.nar.zst",
            "..",
            ".",
            "nar/./x.nar.zst",
            "nar//x.nar.zst",
            "nar\\x.nar.zst",
            "nar/x.nar.zst/",
        ] {
            let err = format!(
                "{:#}",
                s3_object_key("test-cache", "some/prefix/", hostile).unwrap_err()
            );
            assert!(
                err.contains(&format!("{hostile:?}")) && err.contains("cache-relative"),
                "{hostile:?} must be refused naming the value and the convention: {err}"
            );
        }
    }

    #[tokio::test]
    async fn fetch_nar_accepts_absolute_same_origin_narinfo_url() {
        // A `URL:` field spelled absolute but on the cache's own origin
        // (scheme, host, and port all match) stays fetchable: no endpoint
        // the admission screen did not already vet becomes reachable.
        let nar: Vec<u8> = b"absolute same-origin payload".repeat(64);
        let zstd_body = zstd_compress(&nar).await;
        let routes = HashMap::from([("/nar/abs.nar.zst".to_string(), (200, zstd_body))]);
        let (base, server) = spawn_test_server(routes).await;
        let sub = Substituter::parse(&base).await.unwrap();
        let info = narinfo_for(&nar, &format!("{base}/nar/abs.nar.zst"), "zstd");
        assert_eq!(sub.fetch_nar(&info).await.unwrap(), nar);
        server.abort();
    }

    #[tokio::test]
    async fn narinfo_redirect_to_non_public_address_is_refused() {
        // The substituter URL itself passed the admission screen (or is a
        // dev/test loopback); a probe answered with a redirect at a
        // non-public address must abort instead of following it.
        use axum::response::IntoResponse as _;
        use axum::routing::get;

        let app = axum::Router::new().route(
            "/d4444444444444444444444444444444.narinfo",
            get(|| async {
                (
                    axum::http::StatusCode::FOUND,
                    [(
                        axum::http::header::LOCATION,
                        "https://10.0.0.1/internal.narinfo",
                    )],
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let sub = Substituter::parse(&format!("http://{addr}")).await.unwrap();
        let err = sub
            .narinfo("d4444444444444444444444444444444")
            .await
            .expect_err("a redirect at a non-public address must abort the probe");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("https://10.0.0.1/internal.narinfo"),
            "error must name the refused redirect target: {msg}"
        );
        assert!(
            msg.contains("non-public address 10.0.0.1"),
            "error must name the non-public address: {msg}"
        );

        server.abort();
    }

    #[tokio::test]
    async fn fetch_nar_follows_same_origin_redirect() {
        // A cache may relocate objects within itself (relative `Location`);
        // the per-hop redirect screen must keep that working.
        use axum::response::IntoResponse as _;
        use axum::routing::get;

        let nar: Vec<u8> = b"redirected NAR payload 0123456789".repeat(64);
        let zstd_body = zstd_compress(&nar).await;
        let app = axum::Router::new()
            .route(
                "/nar/moved.nar.zst",
                get(|| async {
                    (
                        axum::http::StatusCode::FOUND,
                        [(axum::http::header::LOCATION, "/nar/relocated.nar.zst")],
                    )
                        .into_response()
                }),
            )
            .route(
                "/nar/relocated.nar.zst",
                get(move || async move { zstd_body }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let sub = Substituter::parse(&format!("http://{addr}")).await.unwrap();
        let info = narinfo_for(&nar, "nar/moved.nar.zst", "zstd");
        assert_eq!(
            sub.fetch_nar(&info).await.unwrap(),
            nar,
            "a same-origin redirect must be followed and the NAR verified"
        );

        server.abort();
    }

    /// The probe body cap admits exactly `MAX_NARINFO_BYTES` and refuses
    /// one byte more (limit / limit+1): the must-admit direction keeps
    /// legitimate large-references narinfos working, the must-block
    /// direction is the OOM belt.
    #[tokio::test]
    async fn narinfo_body_cap_boundary() {
        let at_cap = vec![b'x'; MAX_NARINFO_BYTES as usize];
        let read = read_capped_narinfo_body(std::io::Cursor::new(at_cap.clone()), "test://cap")
            .await
            .expect("a body at exactly the cap is admitted");
        assert_eq!(read.len() as u64, MAX_NARINFO_BYTES);

        let over_cap = vec![b'x'; MAX_NARINFO_BYTES as usize + 1];
        let err = format!(
            "{:#}",
            read_capped_narinfo_body(std::io::Cursor::new(over_cap), "test://cap")
                .await
                .unwrap_err()
        );
        assert!(
            err.contains(&MAX_NARINFO_BYTES.to_string()),
            "the refusal names the cap: {err}"
        );
    }

    /// HTTP arm: an oversized narinfo body is refused naming the cap
    /// (must-block), while a normal-sized narinfo on the same server keeps
    /// parsing (must-admit) — the cap discriminates on size, not on the
    /// probe path.
    #[tokio::test]
    async fn http_narinfo_body_size_is_capped() {
        let narinfo_text = "\
StorePath: /nix/store/b2222222222222222222222222222222-present
URL: nar/b2222222222222222222222222222222.nar.zst
Compression: zstd
NarHash: sha256:0000000000000000000000000000000000000000000000000000
NarSize: 4242
References:
";
        let routes = HashMap::from([
            (
                "/b2222222222222222222222222222222.narinfo".to_string(),
                (200, narinfo_text.as_bytes().to_vec()),
            ),
            (
                "/e5555555555555555555555555555555.narinfo".to_string(),
                (200, vec![b'x'; MAX_NARINFO_BYTES as usize + 1024]),
            ),
        ]);
        let (base, server) = spawn_test_server(routes).await;
        let sub = Substituter::parse(&base).await.unwrap();

        let present = sub
            .narinfo("b2222222222222222222222222222222")
            .await
            .unwrap()
            .expect("a normal-sized narinfo still parses");
        assert_eq!(present.nar_size, 4242);

        let err = format!(
            "{:#}",
            sub.narinfo("e5555555555555555555555555555555")
                .await
                .unwrap_err()
        );
        assert!(
            err.contains(&MAX_NARINFO_BYTES.to_string()),
            "an oversized narinfo body is refused naming the cap: {err}"
        );

        server.abort();
    }

    /// S3 arm: the same size cap applies (must-block, via the SDK mock's
    /// canned oversized body), and `NoSuchKey` keeps mapping to a
    /// definitive miss (must-admit for the error taxonomy).
    #[tokio::test]
    async fn s3_narinfo_body_size_is_capped_and_no_such_key_is_none() {
        use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
        use aws_smithy_mocks::{RuleMode, mock, mock_client};

        let oversized = mock!(aws_sdk_s3::Client::get_object).then_output(|| {
            GetObjectOutput::builder()
                .body(aws_sdk_s3::primitives::ByteStream::from(vec![
                    b'x';
                    MAX_NARINFO_BYTES
                        as usize
                        + 1024
                ]))
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&oversized]);
        let sub = Substituter::S3 {
            client,
            bucket: "test-cache".into(),
            prefix: String::new(),
        };
        let err = format!(
            "{:#}",
            sub.narinfo("a1111111111111111111111111111111")
                .await
                .unwrap_err()
        );
        assert!(
            err.contains(&MAX_NARINFO_BYTES.to_string()),
            "an oversized S3 narinfo body is refused naming the cap: {err}"
        );

        let missing = mock!(aws_sdk_s3::Client::get_object).then_error(|| {
            GetObjectError::NoSuchKey(aws_sdk_s3::types::error::NoSuchKey::builder().build())
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&missing]);
        let sub = Substituter::S3 {
            client,
            bucket: "test-cache".into(),
            prefix: String::new(),
        };
        assert!(
            sub.narinfo("a1111111111111111111111111111111")
                .await
                .unwrap()
                .is_none(),
            "NoSuchKey stays a definitive miss"
        );
    }

    /// S3 arm: the probe deadline covers the BODY read, not just the
    /// GetObject dispatch. A backend that returns response headers and then
    /// stalls the body forever must fail the probe at the deadline — before
    /// this bound, the `collect` pended indefinitely (modulo an undocumented
    /// SDK stalled-stream floor) while holding a probe slot.
    #[tokio::test]
    async fn s3_narinfo_deadline_covers_the_body_read() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        // Minimal HTTP/1.1 endpoint: answer the GET with 200 + a large
        // declared content-length, then never send a single body byte.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                                content-length: 1048576\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    // Hold the socket open without writing the body.
                    std::future::pending::<()>().await;
                });
            }
        });

        // Build the S3 variant directly: `parse` refuses `endpoint=` by
        // design, and the stall scope under test is the probe's, not the
        // admission screen's.
        let config = aws_sdk_s3::config::Builder::new()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(format!("http://{addr}"))
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "test", "test", None, None, "test",
            ))
            .force_path_style(true)
            .build();
        let sub = Substituter::S3 {
            client: aws_sdk_s3::Client::from_conf(config),
            bucket: "stalled".into(),
            prefix: String::new(),
        };

        let probe = sub.narinfo_at(
            "a1111111111111111111111111111111",
            Duration::from_millis(300),
        );
        let err = tokio::time::timeout(Duration::from_secs(20), probe)
            .await
            .expect("the probe deadline must fire while the body stalls — it may not pend")
            .expect_err("a stalled body cannot produce a narinfo");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("timed out"),
            "the failure names the probe deadline: {msg}"
        );

        server.abort();
    }
}

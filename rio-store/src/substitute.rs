//! Upstream binary-cache substitution: block-and-fetch narinfo + NAR
//! from a tenant's configured upstreams, ingest via the same CAS path
//! as PutPath.
//!
// r[impl store.substitute.upstream]
//! Flow (see [`Substituter::try_substitute`]):
//!
//! 1. Load the tenant's upstream list (`tenant_upstreams`, priority ASC)
//! 2. Per upstream: GET `{url}/{hash_part}.narinfo` → parse → `verify_sig`
//! 3. GET `{url}/{narinfo.url}` (the NAR, possibly xz/zstd compressed)
//! 4. Decompress stream → accumulate → write-ahead ingest
//! 5. Apply `sig_mode` (keep/add/replace) to stored signatures
//! 6. Return `ValidatedPathInfo`
//!
//! [`check_available`](Substituter::check_available) is the HEAD-only
//! cousin that feeds `FindMissingPathsResponse.substitutable_paths`.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use moka::future::Cache;
use sqlx::PgPool;
use tokio::io::AsyncReadExt;
use tracing::{debug, instrument, warn};
use uuid::Uuid;

use rio_nix::narinfo::NarInfo;
use rio_nix::store_path::StorePath;
use rio_proto::validated::ValidatedPathInfo;

use crate::backend::chunk::ChunkBackend;
use crate::cas;
use crate::metadata::{self, SigMode, Upstream};
use crate::signing::TenantSigner;

/// Errors surfaced by the substitution path. Callers map these to gRPC
/// status; `NotFound` is the normal miss case (no upstream has the
/// path, or the tenant has no upstreams configured).
#[derive(Debug, thiserror::Error)]
pub enum SubstituteError {
    /// Upstream HTTP request failed (connect, TLS, 5xx). The upstream
    /// URL is folded into the message for operator-side debugging.
    #[error("upstream fetch failed: {0}")]
    Fetch(String),

    /// narinfo parse failed, or the `NarHash:` line didn't decode to
    /// a 32-byte SHA-256. Upstream served garbage.
    #[error("narinfo parse error: {0}")]
    NarInfo(String),

    /// The ingested NAR's SHA-256 didn't match the narinfo's `NarHash:`
    /// line. Upstream served corrupt bytes (or lied in the narinfo).
    #[error("NAR hash mismatch: expected {expected}, got {got}")]
    HashMismatch { expected: String, got: String },

    /// Metadata-layer failure during ingest (write-ahead,
    /// complete_manifest). Boxed so this enum stays small.
    #[error("ingest failed: {0}")]
    Ingest(#[from] metadata::MetadataError),

    /// cas::put_chunked failure (S3 upload, refcount upsert).
    #[error("chunked ingest failed: {0}")]
    Chunked(String),
}

/// HTTP narinfo + NAR fetcher with per-tenant upstream lookup.
///
/// Constructed once at server startup and shared via `Arc` across
/// `StoreServiceImpl`'s RPC handlers. The `reqwest::Client` is
/// connection-pooled; the moka singleflight cache coalesces concurrent
/// substitutions of the same `(tenant, path)` pair.
pub struct Substituter {
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    http: reqwest::Client,
    /// The signer for `sig_mode = add|replace`. `None` means those
    /// modes fall back to `keep` behavior (we can't sign without a
    /// key). Per-tenant key resolution inside.
    signer: Option<Arc<TenantSigner>>,
    // r[impl store.singleflight]
    /// Singleflight: `(tenant_id, store_path)` → cached result. TTL
    /// keeps a recently-substituted path hot for the next caller
    /// without re-checking PG. Entries are cheap (the `PathInfo` is
    /// already in narinfo; this is just the gRPC-shaped copy). moka
    /// handles concurrent `get_with` on the same key by coalescing —
    /// N callers become one `do_substitute` call.
    inflight: Cache<(Uuid, String), Option<Arc<ValidatedPathInfo>>>,
}

impl Substituter {
    pub fn new(pool: PgPool, chunk_backend: Option<Arc<dyn ChunkBackend>>) -> Self {
        Self {
            pool,
            chunk_backend,
            http: reqwest::Client::new(),
            signer: None,
            // Short TTL + small cap: this is a singleflight coalescer,
            // not a PathInfo cache. The narinfo table IS the cache.
            // 30s is long enough to coalesce a burst of GetPaths for
            // the same path from N workers; short enough that a
            // subsequent substitution-miss doesn't stay stale.
            inflight: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(std::time::Duration::from_secs(30))
                .build(),
        }
    }

    /// Enable `sig_mode = add|replace` signing. Builder-style.
    pub fn with_signer(mut self, signer: Arc<TenantSigner>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Replace the HTTP client. Test hook — production uses the
    /// default (connection-pooled, rustls, native certs).
    #[cfg(test)]
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Try to substitute `store_path` from any of `tenant_id`'s
    /// configured upstreams. Returns `Ok(None)` on miss (no upstream
    /// has it, OR the tenant has no upstreams). Returns `Ok(Some)`
    /// with the ingested `PathInfo` on success.
    ///
    /// Singleflight-wrapped: concurrent calls for the same `(tenant,
    /// path)` coalesce into one fetch. The moka TTL (30s) means a hit
    /// within the window returns the cached result without re-checking.
    #[instrument(skip(self), fields(tenant = %tenant_id, path = store_path))]
    pub async fn try_substitute(
        &self,
        tenant_id: Uuid,
        store_path: &str,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        let key = (tenant_id, store_path.to_string());
        // moka's get_with: if another caller is already computing this
        // key, we wait and share its result. The init future runs at
        // most once per key-per-TTL-window. Result must be `Clone` —
        // hence `Arc<ValidatedPathInfo>` (cheap: mostly Strings+Vecs).
        //
        // `try_get_with` would propagate the error but moka then also
        // caches NEGATIVE results. A transient upstream 503 would
        // poison the cache for 30s. `get_with` doesn't cache on
        // error-in-init; we eat the error inside and return `None` +
        // warn, which is the correct fallback (try again on next miss).
        let cached = self
            .inflight
            .get_with(key, async {
                match self.do_substitute(tenant_id, store_path).await {
                    Ok(v) => v.map(Arc::new),
                    Err(e) => {
                        // Transient upstream failure → log + miss.
                        // Caller returns NotFound; next attempt retries.
                        warn!(error = %e, "substitution failed");
                        metrics::counter!(
                            "rio_store_substitute_total",
                            "result" => "error"
                        )
                        .increment(1);
                        None
                    }
                }
            })
            .await;
        Ok(cached.map(|arc| (*arc).clone()))
    }

    /// One full fetch cycle — the singleflight body.
    async fn do_substitute(
        &self,
        tenant_id: Uuid,
        store_path: &str,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        let upstreams = metadata::upstreams::list_for_tenant(&self.pool, tenant_id).await?;
        if upstreams.is_empty() {
            // Normal — most tenants don't configure upstreams. No
            // metric increment; this isn't a miss, it's a no-op.
            return Ok(None);
        }

        let sp = StorePath::parse(store_path)
            .map_err(|e| SubstituteError::NarInfo(format!("bad store path: {e}")))?;
        let hash_part = sp.hash_part();

        // Check if the NAR is already local under the same nar_hash
        // (another tenant substituted it). We'll re-check per-upstream
        // after verify_sig — but early exit here avoids N narinfo
        // round-trips when the path is already there with complete
        // manifest. We still need to go through the sig-append flow
        // though, so this can't return early with just the existing
        // row. Skip.

        let start = Instant::now();
        for upstream in &upstreams {
            match self
                .try_upstream(tenant_id, upstream, store_path, &hash_part)
                .await
            {
                Ok(Some(info)) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    metrics::histogram!("rio_store_substitute_duration_seconds").record(elapsed);
                    metrics::counter!(
                        "rio_store_substitute_total",
                        "result" => "hit",
                        "upstream" => upstream.url.clone()
                    )
                    .increment(1);
                    metrics::counter!("rio_store_substitute_bytes_total").increment(info.nar_size);
                    return Ok(Some(info));
                }
                Ok(None) => {
                    // This upstream doesn't have it. Try the next.
                    debug!(upstream = %upstream.url, "upstream miss, trying next");
                }
                Err(e) => {
                    // This upstream failed (down, sig invalid, parse
                    // error). Log and try the next one — a single bad
                    // upstream shouldn't block substitution entirely.
                    warn!(upstream = %upstream.url, error = %e, "upstream fetch failed, trying next");
                }
            }
        }

        metrics::counter!("rio_store_substitute_total", "result" => "miss").increment(1);
        Ok(None)
    }

    /// Steps 2-6 for one upstream.
    async fn try_upstream(
        &self,
        tenant_id: Uuid,
        upstream: &Upstream,
        store_path: &str,
        hash_part: &str,
    ) -> Result<Option<ValidatedPathInfo>, SubstituteError> {
        // — Step 2: GET narinfo + parse + verify_sig —
        let base = upstream.url.trim_end_matches('/');
        let narinfo_url = format!("{base}/{hash_part}.narinfo");
        let resp = self
            .http
            .get(&narinfo_url)
            .send()
            .await
            .map_err(|e| SubstituteError::Fetch(format!("{narinfo_url}: {e}")))?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(SubstituteError::Fetch(format!(
                "{narinfo_url}: HTTP {}",
                resp.status()
            )));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| SubstituteError::Fetch(format!("{narinfo_url} body: {e}")))?;
        let ni = NarInfo::parse(&text)
            .map_err(|e| SubstituteError::NarInfo(format!("{narinfo_url}: {e}")))?;

        // Sig gate: MUST verify against this upstream's trusted_keys.
        // Reject on None — an upstream we don't have a trust anchor
        // for is as good as 404.
        let Some(trusted_key) = ni.verify_sig(&upstream.trusted_keys) else {
            warn!(
                upstream = %upstream.url,
                path = store_path,
                "narinfo signature did not verify against upstream.trusted_keys"
            );
            return Ok(None);
        };
        debug!(upstream = %upstream.url, trusted_key, "narinfo signature verified");

        // Parse the nar_hash into raw bytes for the ingest path. The
        // narinfo text has `sha256:nixbase32`; we need `[u8; 32]` for
        // ValidatedPathInfo + the post-decompress hash check.
        let expected_hash = parse_nar_hash(&ni.nar_hash)?;

        // — Dedup check — If another tenant already substituted this
        // exact NAR (same nar_hash), the chunks are already in CAS.
        // Just append our sigs and return. `path_by_nar_hash` filters
        // on `manifests.status = 'complete'` so partial uploads don't
        // count.
        if let Some(existing) = metadata::path_by_nar_hash(&self.pool, &expected_hash).await?
            && existing == store_path
        {
            debug!(
                path = store_path,
                "NAR already present, appending sigs only"
            );
            let info = metadata::query_path_info(&self.pool, store_path)
                .await?
                .ok_or_else(|| {
                    SubstituteError::Ingest(metadata::MetadataError::InvariantViolation(
                        "path_by_nar_hash hit but query_path_info miss".into(),
                    ))
                })?;
            let sigs = self
                .sigs_for_mode(tenant_id, upstream.sig_mode, &ni, &info)
                .await;
            metadata::append_signatures(&self.pool, store_path, &sigs).await?;
            let mut info = info;
            for s in sigs {
                if !info.signatures.contains(&s) {
                    info.signatures.push(s);
                }
            }
            return Ok(Some(info));
        }

        // — Steps 3-4: GET NAR + decompress —
        let nar_url = format!("{base}/{}", ni.url);
        let nar_bytes = self.fetch_nar(&nar_url, &ni.compression).await?;

        // Hash-check the decompressed NAR against the narinfo's claim.
        // Upstream could serve bytes that don't match its narinfo
        // (bug, bitrot, or malice) — reject before ingest.
        let got_hash: [u8; 32] = sha2::Sha256::digest(&nar_bytes).into();
        if got_hash != expected_hash {
            return Err(SubstituteError::HashMismatch {
                expected: hex::encode(expected_hash),
                got: hex::encode(got_hash),
            });
        }

        // — Step 5-6: ingest via write-ahead + sig_mode —
        let info = self
            .ingest(tenant_id, upstream.sig_mode, &ni, nar_bytes, expected_hash)
            .await?;
        Ok(Some(info))
    }

    /// GET the NAR body and decompress. Returns the raw NAR bytes.
    ///
    /// Accumulates fully before ingest — `cas::put_chunked` needs the
    /// whole `&[u8]` for FastCDC. Streaming-chunker would avoid the
    /// full buffer but isn't here yet; TODO(P0463) tracks it.
    async fn fetch_nar(&self, nar_url: &str, compression: &str) -> Result<Bytes, SubstituteError> {
        let resp = self
            .http
            .get(nar_url)
            .send()
            .await
            .map_err(|e| SubstituteError::Fetch(format!("{nar_url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(SubstituteError::Fetch(format!(
                "{nar_url}: HTTP {}",
                resp.status()
            )));
        }
        // bytes_stream → StreamReader → decoder → read_to_end. The
        // decoder layer is a no-op passthrough for `none`.
        use futures_util::TryStreamExt;
        use tokio_util::io::StreamReader;
        let stream = resp
            .bytes_stream()
            .map_err(|e| std::io::Error::other(format!("NAR stream: {e}")));
        let reader = StreamReader::new(stream);

        let mut out = Vec::new();
        match compression {
            "xz" => {
                let mut dec = async_compression::tokio::bufread::XzDecoder::new(reader);
                dec.read_to_end(&mut out)
                    .await
                    .map_err(|e| SubstituteError::Fetch(format!("{nar_url} xz: {e}")))?;
            }
            "zstd" => {
                let mut dec = async_compression::tokio::bufread::ZstdDecoder::new(reader);
                dec.read_to_end(&mut out)
                    .await
                    .map_err(|e| SubstituteError::Fetch(format!("{nar_url} zstd: {e}")))?;
            }
            "none" | "" => {
                let mut r = reader;
                r.read_to_end(&mut out)
                    .await
                    .map_err(|e| SubstituteError::Fetch(format!("{nar_url} body: {e}")))?;
            }
            other => {
                return Err(SubstituteError::NarInfo(format!(
                    "unsupported Compression: {other:?}"
                )));
            }
        }
        Ok(Bytes::from(out))
    }

    /// Write-ahead ingest: placeholder → chunked-or-inline → complete.
    /// Same flow as PutPath (grpc/put_path.rs:511-580), minus the
    /// streaming/HMAC bits — we already have the full NAR in memory.
    async fn ingest(
        &self,
        tenant_id: Uuid,
        sig_mode: SigMode,
        ni: &NarInfo,
        nar_bytes: Bytes,
        nar_hash: [u8; 32],
    ) -> Result<ValidatedPathInfo, SubstituteError> {
        let mut info = narinfo_to_validated(ni, nar_hash)?;
        info.nar_size = nar_bytes.len() as u64;

        // sig_mode handling — compute the sigs we'll store.
        info.signatures = self.sigs_for_mode(tenant_id, sig_mode, ni, &info).await;

        let store_path_hash = info.store_path.sha256_digest();
        info.store_path_hash = store_path_hash.to_vec();
        let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();

        // Write-ahead placeholder. If another uploader raced us (or
        // another substituter for the same path), the `inserted`
        // return is false and complete_manifest will PlaceholderMissing
        // — which we treat as a retriable race (caller can re-query).
        let inserted = metadata::insert_manifest_uploading(
            &self.pool,
            &store_path_hash,
            info.store_path.as_str(),
            &refs_str,
        )
        .await?;
        if !inserted {
            // Lost the race. Re-check if the winner completed — if so,
            // append our sigs to the existing row and return it. If
            // still uploading, return a miss (next try picks it up).
            if let Some(existing) =
                metadata::query_path_info(&self.pool, info.store_path.as_str()).await?
            {
                metadata::append_signatures(&self.pool, info.store_path.as_str(), &info.signatures)
                    .await?;
                return Ok(existing);
            }
            return Err(SubstituteError::Ingest(
                metadata::MetadataError::PlaceholderMissing {
                    store_path: info.store_path.to_string(),
                },
            ));
        }

        // Inline vs chunked. Same threshold as PutPath.
        let use_chunked = self.chunk_backend.is_some() && nar_bytes.len() >= cas::INLINE_THRESHOLD;
        let result = if use_chunked {
            let backend = self.chunk_backend.as_ref().unwrap();
            cas::put_chunked(&self.pool, backend, &info, &nar_bytes)
                .await
                .map(|_| ())
                .map_err(|e| SubstituteError::Chunked(e.to_string()))
        } else {
            metadata::complete_manifest_inline(&self.pool, &info, nar_bytes)
                .await
                .map_err(SubstituteError::Ingest)
        };

        // On failure, clean up the placeholder. Best-effort — if
        // cleanup also fails, the orphan-sweeper will reclaim it.
        if let Err(e) = result {
            if let Err(ce) = metadata::delete_manifest_uploading(&self.pool, &store_path_hash).await
            {
                warn!(error = %ce, "cleanup after failed ingest also failed");
            }
            return Err(e);
        }

        Ok(info)
    }

    // r[impl store.substitute.sig-mode]
    /// Compute the `narinfo.signatures` to store for `sig_mode`.
    ///
    /// `keep` → upstream sigs as-is. `add` → upstream + fresh rio
    /// sig. `replace` → only the fresh rio sig. If the signer isn't
    /// configured, `add`/`replace` degrade to `keep` (we can't produce
    /// a fresh sig without a key). Dedup happens at store time via
    /// `append_signatures`.
    async fn sigs_for_mode(
        &self,
        tenant_id: Uuid,
        mode: SigMode,
        ni: &NarInfo,
        info: &ValidatedPathInfo,
    ) -> Vec<String> {
        let fresh = match &self.signer {
            Some(ts) if mode != SigMode::Keep => match ts.resolve_once(Some(tenant_id)).await {
                Ok((signer, _)) => {
                    let fp = rio_nix::narinfo::fingerprint(
                        info.store_path.as_str(),
                        &info.nar_hash,
                        info.nar_size,
                        &info
                            .references
                            .iter()
                            .map(|r| r.to_string())
                            .collect::<Vec<_>>(),
                    );
                    Some(signer.sign(&fp))
                }
                Err(e) => {
                    warn!(error = %e, "signer resolve failed; degrading to keep");
                    None
                }
            },
            _ => None,
        };

        match (mode, fresh) {
            (SigMode::Replace, Some(s)) => vec![s],
            (SigMode::Add, Some(s)) => {
                let mut v = ni.sigs.clone();
                v.push(s);
                v
            }
            _ => ni.sigs.clone(),
        }
    }

    /// HEAD-only batch probe: which of `paths` exist on ANY of the
    /// tenant's upstreams. No NAR download, no sig verification —
    /// this feeds `FindMissingPathsResponse.substitutable_paths` for
    /// the scheduler's "can I skip building this?" check.
    ///
    /// Parallelized per-path × per-upstream via `join_all`. Fails-open
    /// on individual HEAD errors (a down upstream shouldn't hide paths
    /// that OTHER upstreams have).
    #[instrument(skip(self, paths), fields(tenant = %tenant_id, n = paths.len()))]
    pub async fn check_available(
        &self,
        tenant_id: Uuid,
        paths: &[String],
    ) -> Result<Vec<String>, SubstituteError> {
        let upstreams = metadata::upstreams::list_for_tenant(&self.pool, tenant_id).await?;
        if upstreams.is_empty() || paths.is_empty() {
            return Ok(Vec::new());
        }

        // Build (path, hash_part) pairs up front so the inner closure
        // doesn't reparse N×M times.
        let parsed: Vec<(&str, String)> = paths
            .iter()
            .filter_map(|p| {
                StorePath::parse(p)
                    .ok()
                    .map(|sp| (p.as_str(), sp.hash_part()))
            })
            .collect();

        let bases: Vec<String> = upstreams
            .iter()
            .map(|u| u.url.trim_end_matches('/').to_string())
            .collect();

        let http = &self.http;
        let futs = parsed.iter().map(|(path, hash_part)| {
            let bases = &bases;
            async move {
                for base in bases {
                    let url = format!("{base}/{hash_part}.narinfo");
                    if let Ok(r) = http.head(&url).send().await
                        && r.status().is_success()
                    {
                        return Some((*path).to_string());
                    }
                }
                None
            }
        });
        Ok(futures_util::future::join_all(futs)
            .await
            .into_iter()
            .flatten()
            .collect())
    }
}

/// Parse a narinfo `NarHash:` value (`sha256:nixbase32...`) into raw
/// 32 bytes.
fn parse_nar_hash(s: &str) -> Result<[u8; 32], SubstituteError> {
    let h = rio_nix::hash::NixHash::parse_colon(s)
        .map_err(|e| SubstituteError::NarInfo(format!("NarHash {s:?}: {e}")))?;
    h.digest()
        .try_into()
        .map_err(|_| SubstituteError::NarInfo(format!("NarHash {s:?}: not 32 bytes")))
}

/// Convert a parsed `NarInfo` to the store's `ValidatedPathInfo`.
///
/// narinfo stores references as BASENAMES; `ValidatedPathInfo` wants
/// full `/nix/store/...` paths (`StorePath::parse` enforces the
/// prefix). Re-prepend the store dir derived from `store_path`.
fn narinfo_to_validated(
    ni: &NarInfo,
    nar_hash: [u8; 32],
) -> Result<ValidatedPathInfo, SubstituteError> {
    use rio_proto::types::PathInfo;

    let store_dir = &ni.store_path[..=ni
        .store_path
        .rfind('/')
        .ok_or_else(|| SubstituteError::NarInfo("store_path has no '/'".into()))?];
    let full_refs: Vec<String> = ni
        .references
        .iter()
        .map(|r| format!("{store_dir}{r}"))
        .collect();
    let deriver = ni
        .deriver
        .as_ref()
        .map(|d| format!("{store_dir}{d}"))
        .unwrap_or_default();

    ValidatedPathInfo::try_from(PathInfo {
        store_path: ni.store_path.clone(),
        store_path_hash: Vec::new(),
        deriver,
        nar_hash: nar_hash.to_vec(),
        nar_size: ni.nar_size,
        references: full_refs,
        registration_time: 0,
        ultimate: false,
        signatures: Vec::new(), // filled by sigs_for_mode
        content_address: ni.ca.clone().unwrap_or_default(),
    })
    .map_err(|e| SubstituteError::NarInfo(format!("narinfo→PathInfo: {e}")))
}

use sha2::Digest as _;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signing::Signer;
    use rio_nix::narinfo::fingerprint;
    use rio_test_support::{TestDb, seed_tenant};
    use std::net::SocketAddr;

    // — test fixture: an in-process upstream cache —
    //
    // Wires an axum server on an ephemeral port serving a single
    // narinfo + NAR. `wiremock` isn't in the deptree; axum already is.
    // The signing key is generated fresh per-test so we control what
    // `verify_sig` accepts.

    struct FakeUpstream {
        url: String,
        trusted_key: String,
        /// Abort handle — dropping stops the server.
        _task: tokio::task::JoinHandle<()>,
    }

    async fn spawn_fake_upstream(
        store_path: &str,
        nar_bytes: Vec<u8>,
        key_name: &str,
    ) -> FakeUpstream {
        use axum::{Router, routing::get};
        use base64::Engine;

        let seed = [0x42u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );

        let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar_bytes).into();
        let nar_hash_str = format!(
            "sha256:{}",
            rio_nix::store_path::nixbase32::encode(&nar_hash)
        );

        let fp = fingerprint(store_path, &nar_hash, nar_bytes.len() as u64, &[]);
        let sig = signer.sign(&fp);

        let sp = StorePath::parse(store_path).unwrap();
        let hash_part = sp.hash_part();

        let narinfo = format!(
            "StorePath: {store_path}\n\
             URL: nar/{hash_part}.nar\n\
             Compression: none\n\
             NarHash: {nar_hash_str}\n\
             NarSize: {}\n\
             References: \n\
             Sig: {sig}\n",
            nar_bytes.len()
        );

        let narinfo_path = format!("/{hash_part}.narinfo");
        let nar_path = format!("/nar/{hash_part}.nar");
        let narinfo_c = narinfo.clone();
        let nar_c = nar_bytes.clone();

        let app = Router::new()
            .route(&narinfo_path, get(move || async move { narinfo_c }))
            .route(&nar_path, get(move || async move { nar_c }));

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key,
            _task: task,
        }
    }

    fn make_path() -> (String, Vec<u8>) {
        let path = rio_test_support::fixtures::test_store_path("substituted");
        let (nar, _hash) = rio_test_support::fixtures::make_nar(b"hi");
        (path, nar)
    }

    // r[verify store.substitute.upstream]
    // r[verify store.substitute.sig-mode]
    #[tokio::test]
    async fn substitute_keep_mode_end_to_end() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-keep").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar.clone(), "cache.test-1").await;

        // Configure the upstream for this tenant.
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        let sub = Substituter::new(db.pool.clone(), None);
        let got = sub.try_substitute(tid, &path).await.unwrap();
        let got = got.expect("upstream has the path");

        // Path landed in narinfo + manifests.
        assert_eq!(got.store_path.as_str(), path);
        assert_eq!(got.nar_size, nar.len() as u64);

        // sig_mode=keep → upstream's Sig: is stored verbatim.
        assert_eq!(got.signatures.len(), 1);
        assert!(
            got.signatures[0].starts_with("cache.test-1:"),
            "keep mode should store upstream sig: {:?}",
            got.signatures
        );

        // Verify persistence: re-query via metadata layer.
        let stored = metadata::query_path_info(&db.pool, &path)
            .await
            .unwrap()
            .expect("path should be in narinfo table");
        assert_eq!(stored.nar_size, nar.len() as u64);
        assert_eq!(stored.signatures.len(), 1);
    }

    #[tokio::test]
    async fn substitute_replace_mode() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-replace").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.test-2").await;

        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Replace,
        )
        .await
        .unwrap();

        // Signer with a distinct key name so we can tell upstream vs
        // rio sigs apart.
        let cluster = Signer::from_seed("rio-cluster-1", &[0x99u8; 32]);
        let ts = Arc::new(TenantSigner::new(cluster, db.pool.clone()));
        let sub = Substituter::new(db.pool.clone(), None).with_signer(ts);

        let got = sub.try_substitute(tid, &path).await.unwrap().unwrap();

        // sig_mode=replace → ONLY rio's sig, upstream's dropped.
        assert_eq!(
            got.signatures.len(),
            1,
            "replace: exactly one sig, got {:?}",
            got.signatures
        );
        assert!(
            got.signatures[0].starts_with("rio-cluster-1:"),
            "replace: should be rio-signed, got {:?}",
            got.signatures
        );
    }

    #[tokio::test]
    async fn substitute_add_mode() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-add").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.test-3").await;

        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Add,
        )
        .await
        .unwrap();

        let cluster = Signer::from_seed("rio-cluster-2", &[0x88u8; 32]);
        let ts = Arc::new(TenantSigner::new(cluster, db.pool.clone()));
        let sub = Substituter::new(db.pool.clone(), None).with_signer(ts);

        let got = sub.try_substitute(tid, &path).await.unwrap().unwrap();

        // sig_mode=add → upstream + rio.
        assert_eq!(got.signatures.len(), 2);
        let has_upstream = got
            .signatures
            .iter()
            .any(|s| s.starts_with("cache.test-3:"));
        let has_rio = got
            .signatures
            .iter()
            .any(|s| s.starts_with("rio-cluster-2:"));
        assert!(
            has_upstream && has_rio,
            "add: both sigs, got {:?}",
            got.signatures
        );
    }

    #[tokio::test]
    async fn substitute_miss_no_upstreams() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-none").await;
        let (path, _) = make_path();

        let sub = Substituter::new(db.pool.clone(), None);
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(got.is_none(), "no upstreams → None");
    }

    #[tokio::test]
    async fn substitute_rejects_bad_sig() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-badsig").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.test-4").await;

        // WRONG trusted_key — upstream signs with cache.test-4, we
        // trust only cache.WRONG.
        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            &["cache.WRONG:abcd".into()],
            SigMode::Keep,
        )
        .await
        .unwrap();

        let sub = Substituter::new(db.pool.clone(), None);
        let got = sub.try_substitute(tid, &path).await.unwrap();
        assert!(got.is_none(), "sig verification failed → treat as miss");
    }

    #[tokio::test]
    async fn check_available_head_probe() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "sub-head").await;
        let (path, nar) = make_path();
        let fake = spawn_fake_upstream(&path, nar, "cache.test-5").await;

        metadata::upstreams::insert(
            &db.pool,
            tid,
            &fake.url,
            50,
            std::slice::from_ref(&fake.trusted_key),
            SigMode::Keep,
        )
        .await
        .unwrap();

        // Second path with a DISTINCT hash_part — test_store_path
        // uses a fixed TEST_HASH, so both would resolve to the same
        // narinfo URL otherwise. rand_store_hash gives us a distinct
        // 32-char nixbase32 prefix the axum router won't match.
        let absent = format!(
            "/nix/store/{}-not-on-upstream",
            rio_test_support::fixtures::rand_store_hash()
        );
        let sub = Substituter::new(db.pool.clone(), None);
        let missing = vec![path.clone(), absent];
        let available = sub.check_available(tid, &missing).await.unwrap();
        assert_eq!(available, vec![path], "only the seeded path is available");
    }
}

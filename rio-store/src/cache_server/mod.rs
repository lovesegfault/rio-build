//! Nix binary cache HTTP server.
//!
//! Serves the standard Nix binary cache protocol so clients can use us
//! as a substituter: `nix.settings.substituters = ["http://rio-store:PORT"]`.
// r[impl store.http.narinfo]
//!
//! Three routes:
//! - `/nix-cache-info` — static metadata (store dir, priority)
//! - `/{hash}.narinfo` — path metadata with signatures
//! - `/nar/{narhash}.nar.zst` — NAR content, zstd-compressed on the fly
//!
//! # Why on-the-fly compression
//!
//! We store uncompressed chunks (better dedup — two paths sharing 70%
//! content share 70% of chunks; if we compressed first, FastCDC would
//! find different boundaries). Compressing at serve time costs CPU but:
//! - zstd level 3 is ~500 MB/s — faster than most clients' downlink
//! - The CPU cost is per-serve, not per-store; cold paths cost nothing
//! - No pre-compressed file to store (saves S3 space, avoids staleness)
//!
//! # No privkey here
//!
//! Signatures were computed at PutPath time and stored in the DB. This
//! server reads them from narinfo.signatures and emits `Sig:` lines.
//! It never touches the signing key — a compromised HTTP server can
//! serve wrong data, but it can't forge signatures for data we didn't
//! actually store.

use std::sync::Arc;

use async_compression::tokio::bufread::ZstdEncoder;
use axum::{
    Extension, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use sqlx::PgPool;
use tokio_util::io::{ReaderStream, StreamReader};
use tracing::{debug, instrument, warn};

use rio_nix::narinfo::NarInfoBuilder;
#[cfg(test)]
use rio_nix::store_path::StorePath;
use rio_nix::store_path::nixbase32;

use crate::cas::ChunkCache;
use crate::metadata::{self, ManifestKind};

pub mod auth;

/// Shared state for all routes. Wrapped in Arc for axum's State extractor.
pub struct CacheServerState {
    pub pool: PgPool,
    /// Chunk cache for NAR reassembly. `None` = inline-only store;
    /// chunked paths will 500 (same precondition as GetPath).
    pub chunk_cache: Option<Arc<ChunkCache>>,
    /// When `true`, requests with no `Authorization` header pass
    /// through (single-tenant mode). Default `false` — Bearer
    /// token required via the `tenants.cache_token` column.
    pub allow_unauthenticated: bool,
}

/// Build the axum Router. Caller spawns it with axum::serve.
///
/// Separate from the spawn so tests can get the Router and use
/// axum-test helpers without binding a real port.
pub fn router(state: Arc<CacheServerState>) -> Router {
    let auth = auth::CacheAuth {
        pool: state.pool.clone(),
        allow_unauthenticated: state.allow_unauthenticated,
    };

    // narinfo + NAR content require Bearer auth (tenants.cache_token).
    // .layer() applied here scopes to THESE routes only — the merge
    // below keeps /nix-cache-info outside the auth layer.
    let authed = Router::new()
        // {hash}.narinfo — the hash is 32 nixbase32 chars (store-path
        // hash-part). Axum's path param captures up to the next `/`;
        // we strip the `.narinfo` suffix in the handler.
        .route("/{filename}", get(narinfo))
        .route("/nar/{filename}", get(nar))
        .layer(axum::middleware::from_fn_with_state(
            auth,
            auth::auth_middleware,
        ));

    // /nix-cache-info is PUBLIC: Nix clients probe it FIRST (before
    // authenticating) to discover StoreDir + Priority — they can't know
    // which token to present until they know it's a Nix cache at all.
    // 401 here breaks `nix build --substituters=…` at discovery time.
    // The response is static (no state, no DB); it leaks nothing.
    Router::new()
        .route("/nix-cache-info", get(nix_cache_info))
        .merge(authed)
        .with_state(state)
}

// ============================================================================
// /nix-cache-info
// ============================================================================

/// Static cache metadata. Nix fetches this once to learn what store
/// directory we serve and our priority relative to other substituters.
///
/// `WantMassQuery: 1` tells Nix it's ok to hammer us with batch queries
/// (we can handle it — PG-backed, not a flat file server). Without this,
/// Nix throttles to 1 narinfo at a time.
///
/// `Priority: 40` is "slightly less preferred than cache.nixos.org (40)"
/// — actually, same priority. Lower is better; 40 is the conventional
/// default for org-internal caches.
#[instrument(skip_all)]
async fn nix_cache_info() -> &'static str {
    "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n"
}

// ============================================================================
// /{hash}.narinfo
// ============================================================================

/// Serve narinfo for a path. The URL is `/{hash}.narinfo` where `{hash}`
/// is the 32-char nixbase32 store-path hash-part.
///
/// Response format is the standard Nix narinfo text:
/// ```text
/// StorePath: /nix/store/abc...-hello
/// URL: nar/xyz...nar.zst
/// Compression: zstd
/// NarHash: sha256:...
/// NarSize: 12345
/// References: dep1 dep2
/// Deriver: xyz.drv
/// Sig: cache.example.org-1:base64...
/// ```
#[instrument(skip(state), fields(filename = %filename))]
async fn narinfo(
    State(state): State<Arc<CacheServerState>>,
    Extension(tenant): Extension<auth::AuthenticatedTenant>,
    Path(filename): Path<String>,
) -> Response {
    // Strip `.narinfo` suffix. Anything else (/.foo, /{hash} without
    // extension) → 404. Don't 400 — Nix probes unknown extensions
    // sometimes, 404 is the expected "not a narinfo" response.
    let Some(hash_part) = filename.strip_suffix(".narinfo") else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Validate: 32 nixbase32 chars. Same check as QueryPathFromHashPart.
    // Without this, the LIKE pattern in query_by_hash_part is injectable.
    if hash_part.len() != rio_nix::store_path::HASH_CHARS || nixbase32::decode(hash_part).is_err() {
        debug!(hash_part, "narinfo: invalid hash part");
        return StatusCode::NOT_FOUND.into_response();
    }

    // r[impl store.tenant.narinfo-filter]
    // Tenant-scoped lookup when the request is authenticated: JOIN
    // path_tenants on tenant_id. A path the tenant never built (no
    // path_tenants row) is invisible — 404, same as "doesn't exist".
    // Anonymous requests (tenant_id=None, only possible when
    // allow_unauthenticated=true) skip the JOIN — backward compat
    // for single-tenant / public-cache deployments.
    //
    // The 404 is deliberate (vs 403): no existence oracle. A tenant
    // probing random hash-parts learns nothing about what other
    // tenants have stored.
    let lookup = match tenant.tenant_id {
        Some(tid) => metadata::query_by_hash_part_for_tenant(&state.pool, hash_part, tid).await,
        None => metadata::query_by_hash_part(&state.pool, hash_part).await,
    };
    let info = match lookup {
        Ok(Some(info)) => info,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!(error = %e, "narinfo: query_by_hash_part failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Build the narinfo text. NarInfoBuilder keeps field validation
    // centralized — we don't hand-format here.
    //
    // URL: nar/{nixbase32(nar_hash)}.nar.zst — the nar_hash identifies
    // the CONTENT (multiple paths with the same content share a URL,
    // which is fine; we serve the same bytes). User decision from
    // planning: NarHash URL (standard-ish, matches cache.nixos.org).
    let nar_hash_b32 = nixbase32::encode(&info.nar_hash);
    let url = format!("nar/{nar_hash_b32}.nar.zst");

    // References in narinfo TEXT are basenames (not full paths — that's
    // the fingerprint format).
    let ref_basenames: Vec<String> = info
        .references
        .iter()
        .map(|r| r.basename().to_string())
        .collect();

    let deriver_basename = info.deriver.as_ref().map(|d| d.basename().to_string());

    let mut builder = NarInfoBuilder::new(
        info.store_path.as_str(),
        &url,
        "zstd",
        format!("sha256:{nar_hash_b32}"),
        info.nar_size,
    )
    .references(ref_basenames);

    if let Some(d) = deriver_basename {
        builder = builder.deriver(d);
    }
    for sig in &info.signatures {
        builder = builder.sig(sig);
    }
    if let Some(ca) = &info.content_address {
        builder = builder.ca(ca);
    }

    // NarInfoBuilder::build can only fail on empty required fields.
    // We populated all of them from ValidatedPathInfo (which itself
    // can't have empty store_path — StorePath doesn't construct from
    // empty). If this fires, it's a ValidatedPathInfo invariant bug.
    let narinfo = match builder.build() {
        Ok(n) => n,
        Err(e) => {
            warn!(error = %e, "narinfo: builder validation failed (bug?)");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let body = narinfo.serialize();

    // Headers: narinfos are content-addressed (the hash-part IS the key),
    // so they're immutable. Cache forever. ETag is the nar_hash (changes
    // if the content ever would, which it won't — immutable).
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/x-nix-narinfo"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    headers.insert(
        header::ETAG,
        // hex output is [0-9a-f]+, double-quote is 0x22, both valid header
        // chars — HeaderValue::from_str can only fail on control bytes.
        HeaderValue::from_str(&format!("\"{}\"", hex::encode(info.nar_hash)))
            .expect("hex is always valid header value"),
    );

    (headers, body).into_response()
}

// ============================================================================
// /nar/{narhash}.nar.zst
// ============================================================================

/// Serve a NAR, zstd-compressed as a true stream.
///
/// URL is `/nar/{nixbase32(nar_hash)}.nar.zst` — the narinfo's `URL:`
/// field points here. The client already knows what nar_hash to expect
/// (from the narinfo it just fetched); we look up the path by nar_hash,
/// reassemble the NAR (inline or chunked), compress, stream.
///
/// # Streaming pipeline
///
/// The pipe is `chunk-stream → StreamReader → ZstdEncoder →
/// ReaderStream → Body`. Memory is O(chunk_size × K=8 + zstd
/// window) instead of O(nar_size). For a 4 GiB NAR: ~2 MB live
/// instead of 4 GB.
///
/// The round-trip through AsyncRead (StreamReader then ReaderStream)
/// is because `async-compression::ZstdEncoder` wants an `AsyncBufRead`
/// input and produces an `AsyncRead` output, but axum's Body wants a
/// `Stream<Bytes>`. tokio-util provides both adapters.
///
/// Error handling: if a chunk fetch fails mid-stream (S3 transient,
/// corruption detected by BLAKE3 verify), the stream yields an `Err`
/// → the HTTP response is ALREADY partially sent, so the connection
/// just drops. The client sees a truncated zstd stream and fails its
/// own decompression. Not elegant but correct — we can't go back
/// and change the 200 to a 500. The alternative (buffer everything
/// to detect errors before sending headers) is the old O(nar_size)
/// design we're replacing.
#[instrument(skip(state), fields(filename = %filename))]
async fn nar(State(state): State<Arc<CacheServerState>>, Path(filename): Path<String>) -> Response {
    // Strip `.nar.zst`. Other compressions (`.nar.xz`, `.nar`) are not
    // supported per store.md:138 — we serve zstd only.
    let Some(hash_b32) = filename.strip_suffix(".nar.zst") else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Decode nixbase32 → 32-byte nar_hash. 52 chars for SHA-256.
    // Reject early: bad length, non-nixbase32 chars → 404 (not 400;
    // Nix expects 404 for "not a NAR we have").
    if hash_b32.len() != 52 {
        return StatusCode::NOT_FOUND.into_response();
    }
    let nar_hash: [u8; 32] = match nixbase32::decode(hash_b32) {
        Ok(bytes) if bytes.len() == 32 => bytes.try_into().expect("checked len"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    // Look up the store path. Uses idx_narinfo_nar_hash (migration 002_store.sql).
    let store_path = match metadata::path_by_nar_hash(&state.pool, &nar_hash).await {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!(error = %e, "nar: path_by_nar_hash failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Get the manifest.
    let manifest = match metadata::get_manifest(&state.pool, &store_path).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            // narinfo row exists but manifest doesn't — inconsistent
            // state (shouldn't happen; both filter on status=complete).
            warn!(store_path, "nar: manifest missing for known path");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(e) => {
            warn!(error = %e, "nar: get_manifest failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Build the chunk stream. For inline, it's a trivial 1-element
    // stream. For chunked, it's the K=8 buffered reassembly (same
    // concurrency as GetPath). Chunked-without-cache is the one
    // hard error we catch BEFORE sending headers (config bug,
    // deterministic — every request for this path would fail).
    let nar_stream = match nar_chunk_stream(manifest, state.chunk_cache.clone()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, store_path, "nar: cannot stream");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Pipeline: Stream<Result<Bytes, io::Error>> → AsyncBufRead
    // → zstd compress → AsyncRead → Stream<Result<Bytes, io::Error>>
    // → axum Body.
    //
    // map_err: StreamReader wants io::Error. ChunkError carries the
    // interesting info (which hash, NotFound vs Corrupt) — we log
    // it here and pass a generic "other" to the io layer. The log
    // line is the operator's signal; the client just sees a dropped
    // connection either way.
    //
    // ZstdEncoder default level = 3 (same as encode_all's default).
    // ~500 MB/s, good ratio. async-compression yields to the runtime
    // between its internal buffer flushes so no spawn_blocking needed
    // — it's not a tight CPU loop like zstd::encode_all, it compresses
    // buffer-by-buffer as poll_read is called.
    let io_stream = nar_stream.map_err(|e| {
        warn!(error = %e, "nar: chunk fetch failed mid-stream (response truncated)");
        std::io::Error::other(e)
    });
    let reader = StreamReader::new(io_stream);
    let compressed = ZstdEncoder::new(reader);
    let body_stream = ReaderStream::new(compressed);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-nix-nar"),
    );
    // Same immutable cache policy as narinfo — content-addressed.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", hex::encode(nar_hash)))
            .expect("hex is valid header"),
    );
    // NO Content-Length: we don't know the compressed size until the
    // stream finishes. axum sends `Transfer-Encoding: chunked`
    // automatically. The buffered version COULD set it (compressed
    // bytes were in hand before responding); streaming can't. Clients
    // don't need it — Nix reads until EOF.

    (headers, Body::from_stream(body_stream)).into_response()
}

/// Build a stream of NAR bytes (uncompressed) from a manifest.
///
/// Inline: 1-element stream wrapping the Bytes. Boxed to unify the
/// return type with the chunked path (two different stream types;
/// impl Trait can't handle "one of two" without boxing or an
/// enum-of-streams, and boxing is simpler).
///
/// Chunked: K=8 buffered fetch, same concurrency as GetPath. Order
/// preserved. Errors (ChunkError) propagate as stream items — the
/// caller maps them to io::Error before feeding StreamReader.
///
/// The ONLY error returned synchronously (before streaming starts)
/// is "chunked manifest but no cache" — a config bug that's
/// deterministic per-path. We catch that before sending headers so
/// the client gets a 500, not a truncated body.
///
/// Takes `Option<Arc<ChunkCache>>` by value (clones in the caller):
/// the returned stream is 'static (Body::from_stream needs that) so
/// it can't borrow. Inline doesn't use the cache; passing None is
/// fine for it.
type NarStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<Bytes, crate::cas::ChunkError>> + Send>,
>;

fn nar_chunk_stream(
    manifest: ManifestKind,
    cache: Option<Arc<ChunkCache>>,
) -> anyhow::Result<NarStream> {
    match manifest {
        ManifestKind::Inline(bytes) => {
            // `once` + `ready` → a stream that yields one item then ends.
            // The item is `Ok(bytes)` to match the chunked arm's
            // `Result<Bytes, ChunkError>` shape. ChunkError is a bit
            // odd for inline (there ARE no chunks) but it unifies the
            // type and the Ok path is all that matters here.
            Ok(Box::pin(stream::once(std::future::ready(Ok(bytes)))))
        }
        ManifestKind::Chunked(entries) => {
            let cache = cache
                .ok_or_else(|| anyhow::anyhow!("chunked manifest but no chunk cache configured"))?;
            // buffered(8) preserves order. Each future resolves to
            // `Result<Bytes, ChunkError>` directly from get_verified.
            // The stream is lazy: chunks are fetched as the downstream
            // (zstd encoder) pulls. With K=8, at most 8 chunks in
            // flight = ~8 × 256 KiB = 2 MB live. Plus zstd's window
            // (~8 MB default). That's the memory bound.
            let cache = Arc::clone(&cache);
            Ok(Box::pin(
                stream::iter(entries)
                    .map(move |(hash, _size)| {
                        let cache = Arc::clone(&cache);
                        async move { cache.get_verified(&hash).await }
                    })
                    .buffered(8),
            ))
        }
    }
}

// r[verify store.http.narinfo]
// r[verify store.nar.reassembly]
// r[verify store.integrity.verify-on-get]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::chunk::{ChunkBackend, MemoryChunkBackend};
    use crate::grpc::StoreServiceImpl;
    use crate::signing::Signer;
    use axum::body::Body;
    use axum::http::Request;
    use rio_test_support::{TenantSeed, TestDb};
    use tower::ServiceExt;

    /// Build a Router backed by real PG + memory chunk backend.
    ///
    /// Returns `TestDb` (not just the pool) because TestDb's Drop
    /// deletes the database — dropping it inside setup() means the
    /// router's pool points at a dead DB by the time the test runs.
    /// The `_db` binding in each test keeps it alive.
    async fn setup() -> (Router, TestDb, Arc<MemoryChunkBackend>) {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend = Arc::new(MemoryChunkBackend::new());
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        let state = Arc::new(CacheServerState {
            pool: db.pool.clone(),
            chunk_cache: Some(cache),
            // Tests use allow_unauthenticated — auth is tested
            // separately in auth.rs.
            allow_unauthenticated: true,
        });
        (router(state), db, backend)
    }

    /// Upload a path via the store gRPC layer (reuses the same PG).
    /// Returns (store_path, nar_hash, nar_bytes) for assertions.
    async fn seed_path(
        pool: &PgPool,
        backend: Arc<MemoryChunkBackend>,
        name: &str,
        signer: Option<Signer>,
    ) -> (StorePath, [u8; 32], Vec<u8>) {
        use rio_test_support::fixtures::{make_nar, make_path_info_for_nar, test_store_path};

        // Direct seeding via metadata functions — bypasses the gRPC
        // layer but produces the SAME DB state the cache server reads.
        // Constructing a tonic Streaming without a real transport is
        // awkward; this is simpler and tests what matters (the HTTP
        // side reads the right DB state).
        let _ = (StoreServiceImpl::new, backend); // silence unused-import; kept for doc context
        let store_path = StorePath::parse(&test_store_path(name)).unwrap();
        let (nar, _) = make_nar(name.as_bytes());
        let info = make_path_info_for_nar(store_path.as_str(), &nar);
        let nar_hash = info.nar_hash;

        let store_path_hash = store_path.sha256_digest().to_vec();

        // Signature computed manually (same as maybe_sign would do).
        let mut sigs = Vec::new();
        if let Some(ref s) = signer {
            let fp = rio_nix::narinfo::fingerprint(
                store_path.as_str(),
                &nar_hash,
                nar.len() as u64,
                &[],
            );
            sigs.push(s.sign(&fp));
        }
        let _ = signer;

        metadata::insert_manifest_uploading(pool, &store_path_hash, store_path.as_str(), &[])
            .await
            .unwrap();

        let validated = rio_proto::validated::ValidatedPathInfo {
            store_path: store_path.clone(),
            store_path_hash,
            deriver: None,
            nar_hash,
            nar_size: nar.len() as u64,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: sigs,
            content_address: None,
        };
        metadata::complete_manifest_inline(pool, &validated, Bytes::from(nar.clone()))
            .await
            .unwrap();

        (store_path, nar_hash, nar)
    }

    // ------------------------------------------------------------------------
    // /nix-cache-info
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn nix_cache_info_static() {
        let (app, _db, _backend) = setup().await;
        let resp = app
            .oneshot(Request::get("/nix-cache-info").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        // Test assertion display, not parse-path.
        #[allow(clippy::disallowed_methods)]
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("StoreDir: /nix/store"));
        assert!(text.contains("WantMassQuery: 1"));
        assert!(text.contains("Priority:"));
    }

    /// /nix-cache-info must be reachable WITHOUT auth, even when
    /// `allow_unauthenticated=false` and a tenant token is configured.
    /// Nix clients probe this endpoint before they know which token to
    /// present; 401 here breaks substituter discovery.
    ///
    /// Same request to a narinfo route MUST 401 — proves the auth
    /// layer still gates everything else (we didn't accidentally
    /// disable it globally).
    #[tokio::test]
    async fn nix_cache_info_public_when_auth_required() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Seed a tenant with a token so unauthed requests to authed
        // routes get 401 (not the 503 "misconfigured" fallback — we
        // want to prove /nix-cache-info bypasses a LIVE auth layer).
        TenantSeed::new("team-a")
            .with_cache_token("secret")
            .seed(&db.pool)
            .await;

        let backend = Arc::new(MemoryChunkBackend::new());
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        let state = Arc::new(CacheServerState {
            pool: db.pool.clone(),
            chunk_cache: Some(cache),
            // Auth REQUIRED. The standard setup() uses true; we can't
            // reuse it.
            allow_unauthenticated: false,
        });
        let app = router(state);

        // No Authorization header on either request.

        // /nix-cache-info → 200. Public route; auth layer never sees it.
        let resp = app
            .clone()
            .oneshot(Request::get("/nix-cache-info").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "/nix-cache-info must be public even when auth is required"
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        // Test assertion display, not parse-path.
        #[allow(clippy::disallowed_methods)]
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("StoreDir: /nix/store"),
            "public route should return the real body, not just a 200: {text}"
        );

        // narinfo → 401. Auth layer fires. (Valid hash-part format
        // so we're testing auth, not the 404-on-bad-hash path.)
        let resp = app
            .oneshot(
                Request::get("/00000000000000000000000000000000.narinfo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "narinfo must still require auth — the public merge must \
             not have disabled the auth layer on other routes"
        );
    }

    // ------------------------------------------------------------------------
    // /{hash}.narinfo
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn narinfo_found() {
        let (app, db, backend) = setup().await;
        let (store_path, nar_hash, _) = seed_path(&db.pool, backend, "narinfo-test", None).await;
        let hash_part = store_path.hash_part();

        let resp = app
            .oneshot(
                Request::get(format!("/{hash_part}.narinfo"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Headers: content-type, cache-control, etag.
        let headers = resp.headers();
        assert_eq!(
            headers.get(header::CONTENT_TYPE).unwrap(),
            "text/x-nix-narinfo"
        );
        assert!(
            headers
                .get(header::CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("immutable")
        );
        assert!(headers.contains_key(header::ETAG));

        let body = axum::body::to_bytes(resp.into_body(), 16 * 1024)
            .await
            .unwrap();
        // Test assertion display, not parse-path.
        #[allow(clippy::disallowed_methods)]
        let text = String::from_utf8_lossy(&body);

        assert!(text.contains(&format!("StorePath: {store_path}")));
        assert!(text.contains("Compression: zstd"));
        // URL points to nar/{nixbase32(nar_hash)}.nar.zst
        let expected_url = format!("URL: nar/{}.nar.zst", nixbase32::encode(&nar_hash));
        assert!(text.contains(&expected_url), "missing URL line: {text}");
    }

    #[tokio::test]
    async fn narinfo_includes_signature() {
        let (app, db, backend) = setup().await;
        use base64::Engine;
        let seed = [0x55u8; 32];
        let b64 = base64::engine::general_purpose::STANDARD.encode(seed);
        let signer = Signer::parse(&format!("test-cache:{b64}")).unwrap();

        let (store_path, _, _) = seed_path(&db.pool, backend, "narinfo-signed", Some(signer)).await;
        let hash_part = store_path.hash_part();

        let resp = app
            .oneshot(
                Request::get(format!("/{hash_part}.narinfo"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 16 * 1024)
            .await
            .unwrap();
        // Test assertion display, not parse-path.
        #[allow(clippy::disallowed_methods)]
        let text = String::from_utf8_lossy(&body);

        // Sig: line present with our key name.
        assert!(
            text.contains("Sig: test-cache:"),
            "narinfo should have Sig: line from DB: {text}"
        );
    }

    #[tokio::test]
    async fn narinfo_404_for_unknown() {
        let (app, _db, _backend) = setup().await;
        // Valid hash-part format but not in DB.
        let resp = app
            .oneshot(
                Request::get("/00000000000000000000000000000000.narinfo")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn narinfo_404_for_bad_hash() {
        let (app, _db, _backend) = setup().await;
        // Wrong length — 404, not 400. LIKE-injection attempt blocked.
        for bad in ["short.narinfo", "/%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%.narinfo"] {
            let resp = app
                .clone()
                .oneshot(Request::get(bad).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "bad hash-part {bad} should 404"
            );
        }
    }

    // r[verify store.tenant.narinfo-filter]
    /// Tenant-scoped narinfo: authenticated as tenant A, a path only
    /// in tenant B's `path_tenants` → 404. Same path after inserting
    /// a `path_tenants` row for tenant A → 200.
    ///
    /// Proves BOTH directions of the filter: the JOIN eliminates
    /// (negative case, no existence oracle) AND the JOIN passes through
    /// when the tenant owns the path (positive case). Without the
    /// positive case, a query that always returns `None` would pass
    /// the negative assertion — the classic "proves nothing" trap.
    #[tokio::test]
    async fn narinfo_tenant_filter_scopes_visibility() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let backend = Arc::new(MemoryChunkBackend::new());
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));

        // Two tenants with distinct cache_tokens.
        let tenant_a = TenantSeed::new("tenant-a")
            .with_cache_token("token-a")
            .seed(&db.pool)
            .await;
        let tenant_b = TenantSeed::new("tenant-b")
            .with_cache_token("token-b")
            .seed(&db.pool)
            .await;

        // Seed a path. seed_path writes narinfo + manifest (both
        // status=complete). No path_tenants row yet — neither tenant
        // owns it.
        let (store_path, _, _) =
            seed_path(&db.pool, Arc::clone(&backend), "tenant-scoped", None).await;
        let hash_part = store_path.hash_part();
        // Keying for path_tenants matches scheduler's upsert
        // (db.rs:661 — sha2::Sha256 of the full path string).
        let store_path_hash = store_path.sha256_digest().to_vec();

        // Attribute the path to tenant B only.
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(&store_path_hash)
            .bind(tenant_b)
            .execute(&db.pool)
            .await
            .unwrap();

        // Auth REQUIRED (unlike setup() which uses allow_unauthenticated).
        // The Bearer token selects which tenant_id the handler filters on.
        let state = Arc::new(CacheServerState {
            pool: db.pool.clone(),
            chunk_cache: Some(cache),
            allow_unauthenticated: false,
        });
        let app = router(state);

        // ── Negative: tenant A → 404 (not in path_tenants for A) ──
        // 404 not 403: no existence oracle. Tenant A can't tell the
        // difference between "path exists for tenant B" and "path
        // doesn't exist at all".
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/{hash_part}.narinfo"))
                    .header(header::AUTHORIZATION, "Bearer token-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "tenant A should NOT see tenant B's path"
        );

        // ── Positive (control): tenant B → 200 (IS in path_tenants) ──
        // Self-invalidation guard: without this, a JOIN that matches
        // nothing (typo in column name, wrong table) would pass the
        // 404 assert above trivially.
        let resp = app
            .clone()
            .oneshot(
                Request::get(format!("/{hash_part}.narinfo"))
                    .header(header::AUTHORIZATION, "Bearer token-b")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "tenant B owns the path — filter must pass through"
        );

        // ── Grant tenant A access → 200 ───────────────────────────────
        // N:M — a path can belong to multiple tenants (composite PK
        // on (store_path_hash, tenant_id)). Proves the filter is
        // per-tenant-row, not per-path (i.e., we didn't accidentally
        // write "WHERE tenant_id = (SELECT first tenant for path)").
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(&store_path_hash)
            .bind(tenant_a)
            .execute(&db.pool)
            .await
            .unwrap();

        let resp = app
            .oneshot(
                Request::get(format!("/{hash_part}.narinfo"))
                    .header(header::AUTHORIZATION, "Bearer token-a")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "tenant A granted via path_tenants INSERT → should now see path"
        );
    }

    /// Anonymous (allow_unauthenticated=true, no Authorization header)
    /// → unfiltered. Backward compat for single-tenant / public caches.
    /// The path has NO `path_tenants` row at all — if the filter
    /// applied here, it would 404.
    #[tokio::test]
    async fn narinfo_anonymous_unfiltered() {
        let (app, db, backend) = setup().await;
        // seed_path writes narinfo but NOT path_tenants. setup() uses
        // allow_unauthenticated=true → tenant_id=None → unfiltered query.
        let (store_path, _, _) = seed_path(&db.pool, backend, "anon-unfiltered", None).await;
        let hash_part = store_path.hash_part();

        // Precondition: confirm no path_tenants row — otherwise this
        // test proves nothing (the filter WOULD pass if a row existed).
        let pt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(pt_count, 0, "test precondition: path_tenants must be empty");

        let resp = app
            .oneshot(
                Request::get(format!("/{hash_part}.narinfo"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "anonymous requests bypass tenant filter"
        );
    }

    // ------------------------------------------------------------------------
    // /nar/{narhash}.nar.zst
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn nar_roundtrip_decompresses() {
        let (app, db, backend) = setup().await;
        let (_, nar_hash, original_nar) = seed_path(&db.pool, backend, "nar-roundtrip", None).await;

        let hash_b32 = nixbase32::encode(&nar_hash);
        let resp = app
            .oneshot(
                Request::get(format!("/nar/{hash_b32}.nar.zst"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Decompress and compare. This is what a Nix client does:
        // fetch the .nar.zst, decompress, verify NarHash.
        let compressed = axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        let decompressed = zstd::decode_all(&compressed[..]).unwrap();

        assert_eq!(
            decompressed, original_nar,
            "zstd-decompressed NAR must match original byte-for-byte"
        );
    }

    #[tokio::test]
    async fn nar_404_for_unknown_hash() {
        let (app, _db, _backend) = setup().await;
        // Valid nixbase32 format but not in DB.
        let resp = app
            .oneshot(
                Request::get(format!("/nar/{}.nar.zst", "0".repeat(52)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn nar_404_for_wrong_extension() {
        let (app, _db, _backend) = setup().await;
        // .nar.xz not supported (store.md:138: zstd only).
        for ext in [".nar.xz", ".nar", ".nar.gz", ".tar"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::get(format!("/nar/{}{}", "0".repeat(52), ext))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "unsupported extension {ext} should 404"
            );
        }
    }

    /// Streaming response has NO Content-Length header.
    ///
    /// The buffered version COULD set it (compressed bytes in hand
    /// before responding). The streaming version CAN'T (compressed
    /// size unknown until stream ends). Absence of Content-Length
    /// proves we're actually streaming, not secretly buffering.
    ///
    /// axum sends `Transfer-Encoding: chunked` automatically when
    /// Content-Length is absent. Nix clients read until EOF so they
    /// don't need the length.
    #[tokio::test]
    async fn nar_streaming_no_content_length() {
        let (app, db, backend) = setup().await;
        let (_, nar_hash, _) = seed_path(&db.pool, backend, "streaming", None).await;

        let hash_b32 = nixbase32::encode(&nar_hash);
        let resp = app
            .oneshot(
                Request::get(format!("/nar/{hash_b32}.nar.zst"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The observable difference from buffered: no Content-Length.
        // If this fails with a Content-Length present, someone
        // regressed back to buffering (or axum started adding it
        // for streams, which would be weird).
        assert!(
            resp.headers().get(header::CONTENT_LENGTH).is_none(),
            "streaming response should NOT set Content-Length \
             (compressed size unknown until stream ends). \
             Got: {:?}",
            resp.headers().get(header::CONTENT_LENGTH)
        );

        // Body still decompresses correctly — roundtrip sanity.
        let compressed = axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        assert!(
            zstd::decode_all(&compressed[..]).is_ok(),
            "streamed zstd is still valid"
        );
    }

    // Note: the "chunked without cache → 500" case needs a CHUNKED
    // seed (seed_path always goes inline via complete_manifest_inline).
    // Chunked seeding via gRPC PutPath requires a real transport
    // (constructing tonic Streaming by hand is awkward — see seed_path
    // comment). The inline path works without cache (correct: inline
    // bytes are in PG, no chunks to fetch). The chunked-no-cache
    // synchronous Err in nar_chunk_stream is covered by the code's
    // structure; a unit test on nar_chunk_stream directly would need
    // a synthetic ManifestKind::Chunked which is pub-in-crate
    // metadata — doable but the integration test in chunk_service.rs
    // exercises the chunked path end-to-end already.
}

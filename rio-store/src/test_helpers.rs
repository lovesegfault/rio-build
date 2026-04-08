//! Shared test-only seeding helpers.
//!
//! Consolidates the `seed_*` helpers that were scattered across 9 test
//! modules, all doing variations of `sha2::Sha256::digest(path) → INSERT
//! INTO narinfo/manifests/chunks`. The builder pattern is the same across
//! [`TenantSeed`] / [`StoreSeed`] / [`ChunkSeed`]: chain only the columns
//! a test actually exercises, the rest take schema defaults.
//!
//! Scope: this module covers the **raw-SQL fixture seeding** pattern
//! (tenants + narinfo + manifests + chunks tables). The SQL is co-located
//! with the migrations that own these tables — a column rename now breaks
//! at this crate's `cargo test`, not at a downstream runtime panic. Helpers
//! that exercise the production metadata functions
//! (`insert_manifest_uploading` → `complete_manifest_inline`) stay in their
//! own modules — those test the real write path, not just seed DB state.

use rio_test_support::fixtures::test_store_path;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

use rio_proto::types::{
    PathInfo, PutPathMetadata, PutPathRequest, PutPathTrailer, put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;
use rio_proto::{StoreServiceClient, StoreServiceServer};

use crate::grpc::StoreServiceImpl;

// ---------------------------------------------------------------------------
// In-process gRPC server + PutPath stream helpers
// ---------------------------------------------------------------------------

/// Spawn an in-process [`StoreServiceImpl`] on an ephemeral TCP port and
/// return a connected client + the server's `JoinHandle`.
///
/// Consolidates the `Server::builder().add_service(StoreServiceServer::new(_))`
/// → `spawn_grpc_server` → `Channel::from_shared(...).connect()` →
/// `StoreServiceClient::new` chain that was duplicated across
/// `rio-store/tests/grpc/`, `rio-gateway/tests/functional/`, and
/// `rio-scheduler/src/actor/tests/integration.rs`.
///
/// Uses [`rio_proto::client::connect_store`] (which sets
/// `max_decoding_message_size`) so large-NAR tests don't hit tonic's
/// 4 MiB default. Tests that don't init `client_tls()` get plaintext.
pub async fn spawn_store_service(
    service: StoreServiceImpl,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    let router = Server::builder().add_service(StoreServiceServer::new(service));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server(router).await;
    let client = rio_proto::client::connect_store(&addr.to_string()).await?;
    Ok((client, server))
}

/// Upload a path via `PutPath`, sending metadata + one nar_chunk + trailer.
///
/// Takes `ValidatedPathInfo` (the common case from `make_path_info_for_nar`)
/// and converts to raw `PathInfo` internally. For tests that need to send
/// DELIBERATELY INVALID data (bad references, etc.) to exercise server-side
/// validation, use [`put_path_raw`] instead.
pub async fn put_path(
    client: &mut StoreServiceClient<Channel>,
    info: ValidatedPathInfo,
    nar: Vec<u8>,
) -> Result<bool, tonic::Status> {
    put_path_raw(client, info.into(), nar).await
}

/// Raw variant: takes unvalidated `PathInfo` directly. Use this to test
/// server-side rejection of malformed input (e.g., bad reference strings).
///
/// Trailer-mode: extracts `nar_hash`/`nar_size` from `info`, zeroes them
/// in the metadata, and sends them in a `PutPathTrailer`. (Hash-upfront
/// was deleted — store rejects non-empty metadata `nar_hash`.)
pub async fn put_path_raw(
    client: &mut StoreServiceClient<Channel>,
    mut info: PathInfo,
    nar: Vec<u8>,
) -> Result<bool, tonic::Status> {
    let (tx, rx) = mpsc::channel(8);

    // Extract hash/size for trailer, zero them in metadata.
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut info.nar_hash),
        nar_size: std::mem::take(&mut info.nar_size),
    };

    // Send metadata first. Fresh channel, buffer=8, receiver alive:
    // these sends cannot fail.
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(info),
        })),
    })
    .await
    .expect("fresh channel");

    // Send NAR data as one chunk
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .expect("fresh channel");

    // Send trailer
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(trailer)),
    })
    .await
    .expect("fresh channel");

    // Close the stream
    drop(tx);

    let outbound = ReceiverStream::new(rx);
    let response = client.put_path(outbound).await?;
    Ok(response.into_inner().created)
}

/// `sha2::Sha256::digest(path) → Vec<u8>`. The store-path-hash function
/// every `seed_*` helper was copy-pasting. Matches
/// `StorePath::sha256_digest()` exactly (see rio-nix/src/store_path.rs).
pub fn path_hash(path: &str) -> Vec<u8> {
    Sha256::digest(path.as_bytes()).to_vec()
}

// ---------------------------------------------------------------------------
// TenantSeed — tenants/tenant_keys fixture builder
// ---------------------------------------------------------------------------

/// Seed a tenant row and return its UUID. The simplest case: just a
/// name. Covers the majority of call sites — tests that only need a
/// tenant to exist for FK purposes.
///
/// For tests that need `gc_retention_hours`, `gc_max_store_bytes`, or
/// `cache_token`, use [`TenantSeed`] instead.
pub async fn seed_tenant(pool: &PgPool, name: &str) -> uuid::Uuid {
    sqlx::query_scalar("INSERT INTO tenants (tenant_name) VALUES ($1) RETURNING tenant_id")
        .bind(name)
        .fetch_one(pool)
        .await
        .expect("seed_tenant INSERT failed")
}

/// Builder for the non-trivial tenant seed cases. Every field except
/// `name` takes its schema default when not `.with_*`'d. Chain only
/// the columns the test actually cares about:
///
/// ```ignore
/// let tid = TenantSeed::new("gc-test")
///     .with_retention_hours(48)
///     .seed(&db.pool).await;
/// ```
///
/// The bare [`seed_tenant`] covers the common name-only case without
/// builder noise.
#[derive(Debug)]
pub struct TenantSeed {
    name: String,
    gc_retention_hours: Option<i32>,
    gc_max_store_bytes: Option<i64>,
    cache_token: Option<String>,
    ed25519_seed: Option<[u8; 32]>,
    key_name: Option<String>,
}

impl TenantSeed {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            gc_retention_hours: None,
            gc_max_store_bytes: None,
            cache_token: None,
            ed25519_seed: None,
            key_name: None,
        }
    }

    pub fn with_retention_hours(mut self, hours: i32) -> Self {
        self.gc_retention_hours = Some(hours);
        self
    }

    pub fn with_max_store_bytes(mut self, bytes: i64) -> Self {
        self.gc_max_store_bytes = Some(bytes);
        self
    }

    pub fn with_cache_token(mut self, token: impl Into<String>) -> Self {
        self.cache_token = Some(token.into());
        self
    }

    /// Also seed a row in `tenant_keys`. Chain after `.seed()` consumers
    /// that need a per-tenant signing key (most signing.rs tests).
    /// Default `key_name` is `"{tenant_name}-1"` unless overridden via
    /// [`with_key_name`](Self::with_key_name).
    pub fn with_ed25519_key(mut self, seed: [u8; 32]) -> Self {
        self.ed25519_seed = Some(seed);
        self
    }

    /// Override the default `key_name` (`"{tenant_name}-1"`). Only
    /// meaningful if [`with_ed25519_key`](Self::with_ed25519_key) is
    /// also called.
    pub fn with_key_name(mut self, name: impl Into<String>) -> Self {
        self.key_name = Some(name.into());
        self
    }

    /// INSERT and return `tenant_id`. Columns not `.with_*`'d take
    /// their schema default. `gc_retention_hours` is `NOT NULL
    /// DEFAULT 168` — COALESCE on the SQL side so `None` means
    /// "schema default", not "NULL" (which would violate NOT NULL).
    ///
    /// The `COALESCE($2, 168)` magic-168 duplicates
    /// `rio_scheduler::db::DEFAULT_GC_RETENTION_HOURS`; rio-store
    /// doesn't depend on rio-scheduler, so the constant is inlined
    /// here. Bump both if the default ever changes.
    pub async fn seed(self, pool: &PgPool) -> uuid::Uuid {
        let tid: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO tenants \
             (tenant_name, gc_retention_hours, gc_max_store_bytes, cache_token) \
             VALUES ($1, COALESCE($2, 168), $3, $4) RETURNING tenant_id",
        )
        .bind(&self.name)
        .bind(self.gc_retention_hours)
        .bind(self.gc_max_store_bytes)
        .bind(&self.cache_token)
        .fetch_one(pool)
        .await
        .expect("TenantSeed INSERT failed");

        if let Some(seed) = self.ed25519_seed {
            let key_name = self.key_name.unwrap_or_else(|| format!("{}-1", &self.name));
            sqlx::query(
                "INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed) \
                 VALUES ($1, $2, $3)",
            )
            .bind(tid)
            .bind(key_name)
            .bind(&seed[..])
            .execute(pool)
            .await
            .expect("TenantSeed tenant_keys INSERT failed");
        }

        tid
    }
}

// ---------------------------------------------------------------------------
// StoreSeed — narinfo + manifests fixture builder
// ---------------------------------------------------------------------------

/// Builder for seeding a `narinfo` + `manifests` row pair. Covers the
/// raw-SQL "seed a complete path" pattern that was duplicated across
/// gc/sweep.rs, gc/mark.rs, metadata/queries.rs, grpc/admin.rs.
///
/// ```ignore
/// let hash = StoreSeed::path("orphan")
///     .with_refs(&[&dep_path])
///     .created_hours_ago(48)
///     .seed(&db.pool).await;
/// ```
///
/// Every column not `.with_*`'d takes its schema default. `seed()`
/// returns the `store_path_hash` (the most common thing tests key on).
/// Tests that also need the path string should build it separately
/// via [`test_store_path`] and use [`StoreSeed::raw_path`].
pub struct StoreSeed {
    path: String,
    refs: Vec<String>,
    status: &'static str,
    inline_blob: Option<Vec<u8>>,
    created_hours_ago: Option<i32>,
    nar_hash: [u8; 32],
    nar_size: i64,
    ca: Option<String>,
}

impl StoreSeed {
    /// Seed a path at `/nix/store/{TEST_HASH}-{name}` (via
    /// [`test_store_path`]). Most tests want distinct NAMES, not
    /// distinct hashes — `StorePath::parse` keys on the full string.
    pub fn path(name: &str) -> Self {
        Self::raw_path(test_store_path(name))
    }

    /// Seed at an explicit full store-path string. Use when the test
    /// needs a specific hash-part (e.g., backfill tests that discriminate
    /// paths by hash-part, not name).
    pub fn raw_path(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            refs: Vec::new(),
            status: "complete",
            inline_blob: None,
            created_hours_ago: None,
            nar_hash: [0u8; 32],
            nar_size: 0,
            ca: None,
        }
    }

    /// Set `narinfo.references`. The CTE in mark.rs walks these.
    pub fn with_refs(mut self, refs: &[impl AsRef<str>]) -> Self {
        self.refs = refs.iter().map(|s| s.as_ref().to_string()).collect();
        self
    }

    /// Set `manifests.status`. Default is `"complete"`. Tests for the
    /// orphan scanner or upload-race want `"uploading"`.
    pub fn with_manifest_status(mut self, status: &'static str) -> Self {
        self.status = status;
        self
    }

    /// Set `manifests.inline_blob`. `None` (the default) means NULL —
    /// which per the inline/chunked invariant implies a `manifest_data`
    /// row should exist (up to the test to seed that separately if the
    /// code path reads it).
    pub fn with_inline_blob(mut self, blob: impl Into<Vec<u8>>) -> Self {
        self.inline_blob = Some(blob.into());
        self
    }

    /// Backdate `narinfo.created_at`. The mark-phase grace check and
    /// the GC empty-refs gate both pivot on this.
    pub fn created_hours_ago(mut self, hours: i32) -> Self {
        self.created_hours_ago = Some(hours);
        self
    }

    /// Set `narinfo.nar_hash`. Default is `[0u8; 32]` — fine for tests
    /// that don't assert on the hash, but anything touching
    /// `path_by_nar_hash` needs a distinct value.
    pub fn with_nar_hash(mut self, h: [u8; 32]) -> Self {
        self.nar_hash = h;
        self
    }

    /// Set `narinfo.nar_size`. Default 0.
    pub fn with_nar_size(mut self, size: i64) -> Self {
        self.nar_size = size;
        self
    }

    /// Set `narinfo.ca`. The GC empty-refs gate excludes CA paths.
    pub fn with_ca(mut self, ca: impl Into<String>) -> Self {
        self.ca = Some(ca.into());
        self
    }

    /// INSERT and return the `store_path_hash`.
    ///
    /// Two statements (not a transaction): test fixtures don't need
    /// atomicity, and `TestDb` is per-test anyway.
    pub async fn seed(self, pool: &PgPool) -> Vec<u8> {
        let hash = path_hash(&self.path);
        // nar_hash defaults to the path-hash if left at [0; 32] — keeps
        // the "any 32 bytes, not verified" contract the old seed_path
        // helpers relied on while still being distinct per path.
        let nar_hash: Vec<u8> = if self.nar_hash == [0u8; 32] {
            hash.clone()
        } else {
            self.nar_hash.to_vec()
        };

        sqlx::query(
            r#"
            INSERT INTO narinfo
                (store_path_hash, store_path, nar_hash, nar_size,
                 "references", ca, created_at)
            VALUES ($1, $2, $3, $4, $5, $6,
                    now() - make_interval(hours => COALESCE($7, 0)::int))
            "#,
        )
        .bind(&hash)
        .bind(&self.path)
        .bind(&nar_hash)
        .bind(self.nar_size)
        .bind(&self.refs)
        .bind(self.ca.as_deref())
        .bind(self.created_hours_ago)
        .execute(pool)
        .await
        .expect("StoreSeed narinfo INSERT failed");

        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, $2, $3)",
        )
        .bind(&hash)
        .bind(self.status)
        .bind(self.inline_blob.as_deref())
        .execute(pool)
        .await
        .expect("StoreSeed manifests INSERT failed");

        hash
    }
}

// ---------------------------------------------------------------------------
// ChunkSeed — chunks table fixture builder
// ---------------------------------------------------------------------------

/// Builder for seeding a `chunks` row. Consolidates `seed_chunk`
/// (gc/mod.rs) and `seed_orphan_chunk` (gc/sweep.rs). The blake3 hash
/// is synthesized from a single `tag` byte (distinct first byte,
/// zero-padded) — enough to discriminate chunks in a single-test DB.
pub struct ChunkSeed {
    tag: u8,
    refcount: i32,
    size: i64,
    age_secs: Option<i64>,
}

impl ChunkSeed {
    pub fn new(tag: u8) -> Self {
        Self {
            tag,
            refcount: 0,
            size: 0,
            age_secs: None,
        }
    }

    pub fn with_refcount(mut self, rc: i32) -> Self {
        self.refcount = rc;
        self
    }

    pub fn with_size(mut self, size: i64) -> Self {
        self.size = size;
        self
    }

    /// Backdate `created_at`. The orphan-chunk sweeper's grace check
    /// pivots on `now() - created_at > grace`.
    pub fn age_secs(mut self, secs: i64) -> Self {
        self.age_secs = Some(secs);
        self
    }

    /// INSERT and return the synthesized blake3 hash.
    pub async fn seed(self, pool: &PgPool) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[0] = self.tag;
        sqlx::query(
            "INSERT INTO chunks (blake3_hash, refcount, size, created_at) \
             VALUES ($1, $2, $3, now() - make_interval(secs => COALESCE($4, 0)))",
        )
        .bind(&hash[..])
        .bind(self.refcount)
        .bind(self.size)
        .bind(self.age_secs)
        .execute(pool)
        .await
        .expect("ChunkSeed INSERT failed");
        hash
    }
}

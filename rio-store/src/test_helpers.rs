//! Shared test-only seeding helpers.
//!
//! Consolidates the `seed_*` helpers that were scattered across 9 test
//! modules, all doing variations of `sha2::Sha256::digest(path) → INSERT
//! INTO narinfo/manifests/chunks`. The builder pattern matches
//! [`rio_test_support::TenantSeed`] — chain only the columns a test
//! actually exercises, the rest take schema defaults.
//!
//! Scope: this module covers the **raw-SQL fixture seeding** pattern
//! (narinfo + manifests + chunks tables). Helpers that exercise the
//! production metadata functions (`insert_manifest_uploading` →
//! `complete_manifest_inline`) stay in their own modules — those test
//! the real write path, not just seed DB state.

use rio_test_support::fixtures::test_store_path;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// `sha2::Sha256::digest(path) → Vec<u8>`. The store-path-hash function
/// every `seed_*` helper was copy-pasting. Matches
/// `StorePath::sha256_digest()` exactly (see rio-nix/src/store_path.rs).
pub fn path_hash(path: &str) -> Vec<u8> {
    Sha256::digest(path.as_bytes()).to_vec()
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
    refs_backfilled: Option<bool>,
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
            refs_backfilled: None,
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
    /// `path_by_nar_hash` or `content_index` needs a distinct value.
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

    /// Set `narinfo.refs_backfilled`. Migration-010 column; only the
    /// ResignPaths backfill tests care about this. Schema default is
    /// NULL (post-fix rows); backfill tests want `Some(false)`
    /// (pre-fix rows needing scan).
    pub fn with_refs_backfilled(mut self, v: bool) -> Self {
        self.refs_backfilled = Some(v);
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
                 "references", ca, refs_backfilled, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7,
                    now() - make_interval(hours => COALESCE($8, 0)::int))
            "#,
        )
        .bind(&hash)
        .bind(&self.path)
        .bind(&nar_hash)
        .bind(self.nar_size)
        .bind(&self.refs)
        .bind(self.ca.as_deref())
        .bind(self.refs_backfilled)
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

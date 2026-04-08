//! Per-tenant upstream binary-cache config (`tenant_upstreams` table,
//! migration 026).
//!
//! Kept OUT of `db.rs` (hottest file in the crate per collision matrix).
//! Mirrors [`super::tenant_keys`]'s shape: per-tenant config accessor
//! that the hot path (QueryPathInfo/GetPath substitution miss) queries
//! once per RPC. CRUD here feeds the StoreAdmin RPCs.

use super::MetadataError;
use sqlx::PgPool;
use uuid::Uuid;

/// Per-upstream signature-handling mode. Controls what goes into
/// `narinfo.signatures` after a successful substitution.
///
/// PG stores this as `TEXT` with a `CHECK (sig_mode IN ('keep','add',
/// 'replace'))` constraint (migration 026), NOT a native enum — adding
/// a variant later is an `ALTER CHECK`, not an `ALTER TYPE ... ADD
/// VALUE` (which takes `ACCESS EXCLUSIVE` on PG <14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum SigMode {
    /// Store the upstream's `Sig:` lines unchanged. Verifiers must
    /// trust the upstream's key.
    Keep,
    /// Store upstream sigs PLUS a fresh rio-generated signature.
    /// Verifiers may trust either.
    Add,
    /// Discard upstream sigs, store only the rio-generated signature.
    /// Verifiers only need to trust rio's key.
    Replace,
}

impl SigMode {
    /// Parse from the proto's string field. Empty → `Keep` (the
    /// migration's `DEFAULT 'keep'`). Unrecognized → `None` (caller
    /// maps to INVALID_ARGUMENT).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "keep" => Some(Self::Keep),
            "add" => Some(Self::Add),
            "replace" => Some(Self::Replace),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Add => "add",
            Self::Replace => "replace",
        }
    }
}

/// One row from `tenant_upstreams`. The Substituter iterates these in
/// `priority ASC` order trying each until one has the path.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Upstream {
    pub id: i32,
    pub tenant_id: Uuid,
    pub url: String,
    pub priority: i32,
    pub trusted_keys: Vec<String>,
    pub sig_mode: SigMode,
}

/// Fetch all upstreams for a tenant, ordered by priority (lowest
/// tried first — matches Nix's `substituters` semantics).
///
/// Empty Vec is the normal case (most tenants don't configure
/// upstreams; substitution is opt-in). The composite index
/// `tenant_upstreams_tenant_idx (tenant_id, priority)` makes this a
/// cheap indexed scan.
pub async fn list_for_tenant(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Vec<Upstream>, MetadataError> {
    sqlx::query_as(
        "SELECT id, tenant_id, url, priority, trusted_keys, sig_mode \
         FROM tenant_upstreams WHERE tenant_id = $1 ORDER BY priority ASC",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// Union of all `trusted_keys` arrays across a tenant's upstreams.
///
/// Feeds the `r[store.substitute.tenant-sig-visibility]` gate: when
/// tenant B queries a path that tenant A substituted, B sees it IFF
/// one of the stored signatures verifies against B's trust-set — and
/// B's trust-set is exactly this union.
///
/// PG's `unnest()` over the `TEXT[]` column + `DISTINCT` dedups
/// server-side in one round-trip. Empty table → empty Vec → the
/// visibility gate in the caller returns `NotFound` (correct: a tenant
/// with no upstreams trusts no upstream-substituted paths).
pub async fn tenant_trusted_keys(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Vec<String>, MetadataError> {
    sqlx::query_scalar(
        "SELECT DISTINCT unnest(trusted_keys) \
         FROM tenant_upstreams WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

/// Insert a new upstream row. Returns the inserted row (with its
/// `SERIAL` id populated) so the RPC handler can echo it back.
///
/// `UNIQUE (tenant_id, url)` means a duplicate add returns
/// `MetadataError::Conflict` (23505) — the caller maps that to
/// `ALREADY_EXISTS`, not an error-per-se (idempotent-ish: client can
/// check the existing row via ListUpstreams).
pub async fn insert(
    pool: &PgPool,
    tenant_id: Uuid,
    url: &str,
    priority: i32,
    trusted_keys: &[String],
    sig_mode: SigMode,
) -> Result<Upstream, MetadataError> {
    sqlx::query_as(
        "INSERT INTO tenant_upstreams (tenant_id, url, priority, trusted_keys, sig_mode) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, tenant_id, url, priority, trusted_keys, sig_mode",
    )
    .bind(tenant_id)
    .bind(url)
    .bind(priority)
    .bind(trusted_keys)
    .bind(sig_mode.as_str())
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Delete one upstream by `(tenant_id, url)` (the UNIQUE key).
/// Returns rows_affected — 0 means the pair didn't exist (caller
/// maps to NOT_FOUND).
pub async fn delete(pool: &PgPool, tenant_id: Uuid, url: &str) -> Result<u64, MetadataError> {
    sqlx::query("DELETE FROM tenant_upstreams WHERE tenant_id = $1 AND url = $2")
        .bind(tenant_id)
        .bind(url)
        .execute(pool)
        .await
        .map(|r| r.rows_affected())
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::seed_tenant;
    use rio_test_support::TestDb;

    #[test]
    fn sig_mode_parse() {
        assert_eq!(SigMode::parse(""), Some(SigMode::Keep));
        assert_eq!(SigMode::parse("keep"), Some(SigMode::Keep));
        assert_eq!(SigMode::parse("add"), Some(SigMode::Add));
        assert_eq!(SigMode::parse("replace"), Some(SigMode::Replace));
        assert_eq!(SigMode::parse("KEEP"), None, "case-sensitive");
        assert_eq!(SigMode::parse("junk"), None);
    }

    #[tokio::test]
    async fn empty_tenant_no_upstreams() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "ups-empty").await;

        let ups = list_for_tenant(&db.pool, tid).await.unwrap();
        assert!(ups.is_empty());

        let keys = tenant_trusted_keys(&db.pool, tid).await.unwrap();
        assert!(keys.is_empty());
    }

    #[tokio::test]
    async fn insert_list_delete_roundtrip() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "ups-crud").await;

        // Insert two with different priorities.
        let _ = insert(
            &db.pool,
            tid,
            "https://low-prio.example",
            80,
            &["k1:abc".into()],
            SigMode::Keep,
        )
        .await
        .unwrap();
        let u2 = insert(
            &db.pool,
            tid,
            "https://high-prio.example",
            10,
            &["k2:def".into(), "k3:ghi".into()],
            SigMode::Add,
        )
        .await
        .unwrap();

        // list_for_tenant: priority ASC → high-prio first.
        let ups = list_for_tenant(&db.pool, tid).await.unwrap();
        assert_eq!(ups.len(), 2);
        assert_eq!(ups[0].url, "https://high-prio.example");
        assert_eq!(ups[0].priority, 10);
        assert_eq!(ups[0].sig_mode, SigMode::Add);
        assert_eq!(ups[1].url, "https://low-prio.example");

        // RETURNING populated id.
        assert!(u2.id > 0);

        // trusted_keys union (deduped).
        let mut keys = tenant_trusted_keys(&db.pool, tid).await.unwrap();
        keys.sort();
        assert_eq!(keys, vec!["k1:abc", "k2:def", "k3:ghi"]);

        // Delete one.
        let n = delete(&db.pool, tid, "https://low-prio.example")
            .await
            .unwrap();
        assert_eq!(n, 1);
        let ups = list_for_tenant(&db.pool, tid).await.unwrap();
        assert_eq!(ups.len(), 1);

        // Delete nonexistent → 0.
        let n = delete(&db.pool, tid, "https://nope.example").await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn duplicate_insert_is_conflict() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "ups-dup").await;

        insert(&db.pool, tid, "https://x.example", 50, &[], SigMode::Keep)
            .await
            .unwrap();
        let err = insert(&db.pool, tid, "https://x.example", 50, &[], SigMode::Keep)
            .await
            .expect_err("UNIQUE(tenant_id, url) should reject");
        assert!(matches!(err, MetadataError::Conflict(_)));
    }

    /// Migration 026 has `ON DELETE CASCADE` on the tenants FK.
    /// Deleting a tenant should clear its upstream rows.
    #[tokio::test]
    async fn tenant_delete_cascades() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "ups-cascade").await;
        insert(&db.pool, tid, "https://x.example", 50, &[], SigMode::Keep)
            .await
            .unwrap();

        sqlx::query("DELETE FROM tenants WHERE tenant_id = $1")
            .bind(tid)
            .execute(&db.pool)
            .await
            .unwrap();

        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM tenant_upstreams WHERE tenant_id = $1")
                .bind(tid)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "CASCADE should delete upstream rows");
    }
}

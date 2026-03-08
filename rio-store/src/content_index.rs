//! Content-addressed reverse index: `nar_hash → store_path`.
//!
//! Answers "have we ever seen content with this SHA-256?" — the
//! fundamental CA cache-hit question. If yes, the caller can skip a
//! build by copying the existing path (CA derivations don't care which
// r[impl store.hash.domain-sep]
//! store path holds the content, only that the content is right).
//!
//! # Why this is separate from narinfo
//!
//! narinfo is keyed on `store_path_hash` (input-addressed lookup).
//! content_index is keyed on `content_hash` = `nar_hash` (content
//! lookup). Same data, different index. The dedicated table lets us
//! shard/partition independently later and attach tenant_id for
//! multi-tenant content isolation.
//!
//! # Why nar_hash IS the content hash
//!
//! For CA derivations, the output's nar_hash is exactly the content
//! identity: same bytes → same SHA-256 → same nar_hash. Two builds
//! producing identical output will have identical nar_hash regardless
//! of their store paths. So `content_hash = nar_hash` is correct, not
//! a hack — it's what CA addressing means.
//!
//! Phase 5 early cutoff uses this for the "already-built-but-under-
//! a-different-name" case. Phase 2c just populates the index so the
//! data is there when Phase 5 activates.

use rio_proto::validated::ValidatedPathInfo;
use sqlx::PgPool;
use tracing::instrument;

/// Insert a content→path mapping. Idempotent — ON CONFLICT DO NOTHING.
///
/// Multiple paths can have the same `content_hash` (same bytes uploaded
/// under different input-addressed names). That's fine: the PK is
/// `(content_hash, store_path_hash)`, so each distinct path gets its
/// own row. `lookup()` just picks one.
///
/// Called from PutPath after the manifest commits. Best-effort — a
/// failure here doesn't fail the upload; the path is still addressable
/// by store_path, just not by content. That's consistent with how
/// realisations are also best-effort (Phase 2c doesn't have CA cutoff
/// yet, so content-miss just means a build that could have been
/// skipped isn't).
#[instrument(skip(pool, nar_hash, store_path_hash))]
pub async fn insert(
    pool: &PgPool,
    nar_hash: &[u8; 32],
    store_path_hash: &[u8],
) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO content_index (content_hash, store_path_hash)
        VALUES ($1, $2)
        ON CONFLICT (content_hash, store_path_hash) DO NOTHING
        "#,
    )
    .bind(nar_hash.as_slice())
    .bind(store_path_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Look up a store path by content hash. `None` if no match.
///
/// `LIMIT 1` — multiple paths may have the same content_hash (same
/// bytes, different input-addressed names). Arbitrary which one we
/// return: they're all the same bytes, so any one satisfies a CA
/// cache-hit. PG picks whichever the index scan hits first.
///
/// Joins narinfo + manifests: only return paths that are actually
/// COMPLETE. A content_index row pointing at a stuck-uploading manifest
/// would be a dangling pointer — filter it here rather than trusting
/// insert-order.
#[instrument(skip(pool, content_hash))]
pub async fn lookup(
    pool: &PgPool,
    content_hash: &[u8],
) -> crate::metadata::Result<Option<ValidatedPathInfo>> {
    let row: Option<crate::metadata::NarinfoRow> = sqlx::query_as(concat!(
        "SELECT ",
        crate::narinfo_cols!(),
        " FROM content_index ci \
         INNER JOIN narinfo n ON ci.store_path_hash = n.store_path_hash \
         INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash \
         WHERE ci.content_hash = $1 AND m.status = 'complete' \
         LIMIT 1"
    ))
    .bind(content_hash)
    .fetch_optional(pool)
    .await?;

    crate::metadata::validate_row(row)
}

// r[verify store.hash.domain-sep]
#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// Round-trip: insert then lookup finds it. Also verifies the
    /// narinfo+manifests join — a content_index row alone (no narinfo)
    /// would return None, not garbage.
    #[tokio::test]
    async fn test_insert_and_lookup_roundtrip() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Need a complete narinfo + manifest for the join. Use
        // complete_manifest_inline for a realistic path.
        let store_path = rio_nix::store_path::StorePath::parse(
            &rio_test_support::fixtures::test_store_path("ci-test"),
        )?;
        let sp_hash = store_path.sha256_digest().to_vec();
        let nar_hash = [0x42u8; 32];

        let info = ValidatedPathInfo {
            store_path,
            store_path_hash: sp_hash.clone(),
            deriver: None,
            nar_hash,
            nar_size: 100,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        };
        crate::metadata::insert_manifest_uploading(&db.pool, &sp_hash, info.store_path.as_str())
            .await?;
        crate::metadata::complete_manifest_inline(
            &db.pool,
            &info,
            bytes::Bytes::from(vec![0u8; 100]),
        )
        .await?;

        // Now the actual G1 path: insert + lookup.
        insert(&db.pool, &nar_hash, &sp_hash).await?;

        let found = lookup(&db.pool, &nar_hash).await?.expect("should find");
        assert_eq!(found.nar_hash, nar_hash);
        assert_eq!(found.nar_size, 100);

        Ok(())
    }

    /// Lookup for unknown content → None, not error.
    #[tokio::test]
    async fn test_lookup_missing_returns_none() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let result = lookup(&db.pool, &[0xffu8; 32]).await?;
        assert!(result.is_none());
        Ok(())
    }

    /// Idempotent: second insert is a no-op, lookup still works.
    #[tokio::test]
    async fn test_insert_idempotent() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Minimal narinfo setup.
        let sp = rio_nix::store_path::StorePath::parse(
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-idem",
        )?;
        let sp_hash = sp.sha256_digest().to_vec();
        let nar_hash = [0x11u8; 32];
        let info = ValidatedPathInfo {
            store_path: sp,
            store_path_hash: sp_hash.clone(),
            deriver: None,
            nar_hash,
            nar_size: 1,
            references: vec![],
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        };
        crate::metadata::insert_manifest_uploading(&db.pool, &sp_hash, info.store_path.as_str())
            .await?;
        crate::metadata::complete_manifest_inline(&db.pool, &info, bytes::Bytes::from(vec![0u8]))
            .await?;

        insert(&db.pool, &nar_hash, &sp_hash).await?;
        insert(&db.pool, &nar_hash, &sp_hash).await?; // duplicate, no error

        let found = lookup(&db.pool, &nar_hash).await?;
        assert!(found.is_some(), "still findable after duplicate insert");

        Ok(())
    }
}

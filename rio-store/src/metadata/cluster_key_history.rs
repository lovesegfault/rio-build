//! Prior cluster signing keys (`cluster_key_history` table, migration 027).
//!
//! One query: load all unretired prior-cluster pubkey entries for the
//! sig-visibility gate. Called once at startup (main.rs); the returned
//! `Vec<String>` goes into `TenantSigner::with_prior_cluster`.
//!
//! Insert/retire are admin-CLI territory (rotation is a human-driven
//! operation, not hot-path).

use super::MetadataError;
use sqlx::PgPool;

// r[impl store.key.rotation-cluster-history]
/// Load prior cluster public keys still within their grace period.
///
/// Returns `name:base64(pubkey)` strings — what `Signer::trusted_key_entry`
/// writes during rotation. Each is ready for direct push into
/// `sig_visibility_gate`'s trusted set (`any_sig_trusted` parses this
/// format).
///
/// `WHERE retired_at IS NULL` — retired rows are audit-only, not
/// trusted. An operator ends the grace period by setting `retired_at`,
/// not by deleting the row.
///
/// Ordered by `created_at DESC` so the most recently rotated-out key
/// (most likely to have live sigs) is first in the verify loop. Minor
/// optimization — `any_sig_trusted` is O(keys×sigs) regardless, but
/// early-exit on common case is free.
pub async fn load_cluster_key_history(pool: &PgPool) -> Result<Vec<String>, MetadataError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT pubkey FROM cluster_key_history \
         WHERE retired_at IS NULL \
         ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|(pk,)| pk).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    #[tokio::test]
    async fn empty_history_is_empty_vec() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let keys = load_cluster_key_history(&db.pool).await.unwrap();
        assert!(keys.is_empty(), "fresh DB → no prior keys");
    }

    #[tokio::test]
    async fn retired_keys_excluded() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        sqlx::query(
            "INSERT INTO cluster_key_history (pubkey, retired_at) VALUES \
             ('active-key:aaaa', NULL), \
             ('retired-key:bbbb', now())",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        let keys = load_cluster_key_history(&db.pool).await.unwrap();
        assert_eq!(keys, vec!["active-key:aaaa".to_string()]);
    }

    #[tokio::test]
    async fn ordered_newest_first() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Insert with explicit timestamps so ordering is deterministic
        // regardless of insert speed.
        sqlx::query(
            "INSERT INTO cluster_key_history (pubkey, created_at) VALUES \
             ('old:a', '2026-01-01T00:00:00Z'), \
             ('new:b', '2026-02-01T00:00:00Z')",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        let keys = load_cluster_key_history(&db.pool).await.unwrap();
        assert_eq!(keys, vec!["new:b".to_string(), "old:a".to_string()]);
    }
}

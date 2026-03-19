//! Per-tenant signing key lookup (`tenant_keys` table, migration 014).
//!
//! Kept OUT of `db.rs` (hottest file in the crate per collision matrix).
//! One query: fetch the newest unrevoked key for a tenant. Everything
//! else (insert, revoke, rotate) is admin-CLI territory, not hot-path.

use super::MetadataError;
use crate::signing::Signer;
use sqlx::PgPool;
use uuid::Uuid;

/// Fetch the tenant's active signing key, or `None` if the tenant has
/// no unrevoked key.
///
/// "Active" = newest `created_at` with `revoked_at IS NULL`. Key rotation
/// inserts a new row (doesn't UPDATE) — old signatures stay valid under
/// the old key's pubkey, new paths get the new key. `ORDER BY created_at
/// DESC LIMIT 1` picks the newest.
///
/// `None` is the normal case for most tenants (cluster key fallback).
/// The partial index `tenant_keys_active_idx` (`WHERE revoked_at IS
/// NULL`) makes this a cheap indexed lookup even when a tenant has many
/// revoked keys.
///
/// `InvariantViolation` if the stored seed isn't 32 bytes — `BYTEA` has
/// no length constraint in PG; the 32-byte invariant is code-enforced.
/// A wrong-length seed means someone inserted garbage (manual SQL, bad
/// admin tool); fail loud rather than ed25519-panic or silently drop.
pub async fn get_active_signer(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Option<Signer>, MetadataError> {
    let row: Option<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT key_name, ed25519_seed FROM tenant_keys \
         WHERE tenant_id = $1 AND revoked_at IS NULL \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;

    let Some((name, seed)) = row else {
        return Ok(None);
    };

    let seed_len = seed.len();
    let seed: [u8; 32] = seed.as_slice().try_into().map_err(|_| {
        MetadataError::InvariantViolation(format!(
            "tenant_keys.ed25519_seed for tenant {tenant_id} is {seed_len} bytes, expected 32"
        ))
    })?;

    Ok(Some(Signer::from_seed(name, &seed)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// Insert a tenant, return its UUID. `tenant_name` must be unique
    /// per test (UNIQUE constraint).
    async fn seed_tenant(pool: &PgPool, name: &str) -> Uuid {
        sqlx::query_scalar("INSERT INTO tenants (tenant_name) VALUES ($1) RETURNING tenant_id")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn seed_key(pool: &PgPool, tenant_id: Uuid, key_name: &str, seed: &[u8]) {
        sqlx::query(
            "INSERT INTO tenant_keys (tenant_id, key_name, ed25519_seed) \
             VALUES ($1, $2, $3)",
        )
        .bind(tenant_id)
        .bind(key_name)
        .bind(seed)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn no_key_returns_none() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "tk-no-key").await;

        let result = get_active_signer(&db.pool, tid).await.unwrap();
        assert!(result.is_none(), "tenant with no key row → None");
    }

    #[tokio::test]
    async fn active_key_returns_signer() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "tk-active").await;
        seed_key(&db.pool, tid, "tenant-active-1", &[0x11u8; 32]).await;

        let signer = get_active_signer(&db.pool, tid).await.unwrap().unwrap();
        assert_eq!(signer.key_name(), "tenant-active-1");

        // Round-trip: the seed we inserted produces a signature that
        // verifies under the derived pubkey. Proves from_seed() wired
        // correctly, not just that key_name plumbed through.
        use ed25519_dalek::{Signature, SigningKey, Verifier};
        let expected_pk = SigningKey::from_bytes(&[0x11u8; 32]).verifying_key();
        let sig_str = signer.sign("fp");
        let (_, sig_b64) = sig_str.split_once(':').unwrap();
        let sig_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, sig_b64).unwrap();
        let sig: [u8; 64] = sig_bytes.as_slice().try_into().unwrap();
        expected_pk
            .verify(b"fp", &Signature::from_bytes(&sig))
            .expect("DB-loaded seed should produce verifiable sigs");
    }

    /// Revoked key is invisible. Only the partial index branch
    /// (`WHERE revoked_at IS NULL`) should match.
    #[tokio::test]
    async fn revoked_key_ignored() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "tk-revoked").await;

        // Insert then immediately revoke.
        seed_key(&db.pool, tid, "tenant-revoked-1", &[0x22u8; 32]).await;
        sqlx::query("UPDATE tenant_keys SET revoked_at = now() WHERE tenant_id = $1")
            .bind(tid)
            .execute(&db.pool)
            .await
            .unwrap();

        let result = get_active_signer(&db.pool, tid).await.unwrap();
        assert!(result.is_none(), "revoked key → None (cluster fallback)");
    }

    /// Rotation: two active keys → newest wins. `ORDER BY created_at
    /// DESC LIMIT 1` is the rotation mechanism — insert a new key, old
    /// one stays valid for verification but isn't used for new sigs.
    #[tokio::test]
    async fn newest_active_key_wins() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "tk-rotation").await;

        // Two inserts. `created_at DEFAULT now()` — PG's now() is
        // transaction-start time, so two separate statements get
        // distinct timestamps. (Same-txn would need explicit values.)
        seed_key(&db.pool, tid, "tenant-rot-OLD", &[0x33u8; 32]).await;
        seed_key(&db.pool, tid, "tenant-rot-NEW", &[0x44u8; 32]).await;

        let signer = get_active_signer(&db.pool, tid).await.unwrap().unwrap();
        assert_eq!(
            signer.key_name(),
            "tenant-rot-NEW",
            "newest created_at should win on rotation"
        );
    }

    /// Corruption: wrong-length seed → InvariantViolation, NOT a
    /// panic from ed25519-dalek or a silently-dropped key.
    #[tokio::test]
    async fn bad_seed_length_is_invariant_violation() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tid = seed_tenant(&db.pool, "tk-corrupt").await;
        // 16 bytes — not 32. BYTEA has no length constraint.
        seed_key(&db.pool, tid, "tenant-corrupt-1", &[0x55u8; 16]).await;

        // `.err()` not `.unwrap_err()`: Signer has no Debug (holds an
        // ed25519 secret — deriving Debug would risk leaking it in logs).
        let err = get_active_signer(&db.pool, tid)
            .await
            .err()
            .expect("wrong-length seed should error");
        assert!(
            matches!(err, MetadataError::InvariantViolation(_)),
            "expected InvariantViolation, got {err:?}"
        );
    }
}

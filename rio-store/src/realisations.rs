//! CA derivation realisation storage.
//!
//! A realisation maps `(modular_drv_hash, output_name)` to
//! `(output_path, output_hash)`. Nix writes these after building a CA
//! derivation (wopRegisterDrvOutput) and reads them to check for cache
//! hits (wopQueryRealisation).
//!
//! The modular derivation hash (`hashDerivationModulo`) depends only on
//! the derivation's fixed attributes — NOT on output paths. Two CA
//! derivations with identical inputs hash the same even if their output
//! store paths differ. That's what makes realisations useful: the next
//! build with the same inputs finds this mapping and skips work.
//!
//! # Why this table exists in Phase 2c (before Phase 5)
//!
//! Phase 5 activates full CA early cutoff (per-edge tracking in the DAG).
//! But the DATA MODEL exists now so gateway wopRegisterDrvOutput/
//! wopQueryRealisation aren't stubs: modern Nix clients send these
//! opportunistically after every CA build, and dropping them silently
//! means throwing away cache-hit opportunities. Storing them costs us
//! ~200 bytes per CA output and one INSERT; the payoff is every
//! subsequent build of the same CA derivation becomes a lookup.

use sqlx::PgPool;
use tracing::{debug, instrument};

/// A CA derivation output realisation (owned, DB-facing).
///
/// Mirrors `rio_proto::types::Realisation` but with types fixed up
/// for DB storage: `[u8; 32]` not `Vec<u8>` for hashes. The gRPC layer
/// validates hash lengths at the trust boundary and converts.
#[derive(Debug, Clone)]
pub struct Realisation {
    pub drv_hash: [u8; 32],
    pub output_name: String,
    pub output_path: String,
    pub output_hash: [u8; 32],
    pub signatures: Vec<String>,
}

/// Insert a realisation. Idempotent: ON CONFLICT DO NOTHING.
///
/// CA derivations are deterministic — same `(drv_hash, output_name)`
/// always produces the same `output_path`. So a duplicate insert means
/// "yes, we already knew that", not a conflict. ON CONFLICT DO NOTHING
/// is correct, not just convenient.
///
/// Returns `true` if a row was inserted, `false` if it already existed.
/// The caller doesn't usually care — both are success — but tests can
/// use the distinction.
#[instrument(skip(pool, r), fields(drv_hash = hex::encode(r.drv_hash), output = %r.output_name))]
pub async fn insert(pool: &PgPool, r: &Realisation) -> anyhow::Result<bool> {
    let result = sqlx::query(
        r#"
        INSERT INTO realisations (drv_hash, output_name, output_path, output_hash, signatures)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (drv_hash, output_name) DO NOTHING
        "#,
    )
    .bind(r.drv_hash.as_slice())
    .bind(&r.output_name)
    .bind(&r.output_path)
    .bind(r.output_hash.as_slice())
    .bind(&r.signatures)
    .execute(pool)
    .await?;

    let inserted = result.rows_affected() > 0;
    debug!(inserted, "realisation insert");
    Ok(inserted)
}

/// Look up a realisation by (drv_hash, output_name).
///
/// `None` means this CA derivation output hasn't been built (or
/// registered) before — a cache miss, not an error. The gRPC layer
/// maps this to NOT_FOUND; the gateway maps NOT_FOUND to an empty-set
/// wire response.
#[instrument(skip(pool), fields(drv_hash = hex::encode(drv_hash), output = %output_name))]
pub async fn query(
    pool: &PgPool,
    drv_hash: &[u8; 32],
    output_name: &str,
) -> anyhow::Result<Option<Realisation>> {
    let row: Option<RealisationRow> = sqlx::query_as(
        r#"
        SELECT drv_hash, output_name, output_path, output_hash, signatures
        FROM realisations
        WHERE drv_hash = $1 AND output_name = $2
        "#,
    )
    .bind(drv_hash.as_slice())
    .bind(output_name)
    .fetch_optional(pool)
    .await?;

    // DB-egress validation: hashes must be 32 bytes. The INSERT side binds
    // [u8; 32] so this should never fail unless something wrote to the
    // table directly (psql, future migration bug). Catching it here turns
    // a silently-propagating corrupt row into a loud error at the boundary.
    row.map(|r| r.try_into_validated())
        .transpose()
        .map_err(|e| anyhow::anyhow!("malformed realisations row: {e}"))
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// A `BYTEA` column that should be 32 bytes (SHA-256) came back a different
/// length. The DB schema doesn't enforce length on BYTEA, so this catches
/// corrupt writes at the read boundary instead of panicking in a `try_into()`.
#[derive(Debug, thiserror::Error)]
#[error("{field} must be 32 bytes, got {got}")]
pub(crate) struct RealisationRowError {
    field: &'static str,
    got: usize,
}

#[derive(sqlx::FromRow)]
struct RealisationRow {
    drv_hash: Vec<u8>,
    output_name: String,
    output_path: String,
    output_hash: Vec<u8>,
    signatures: Vec<String>,
}

impl RealisationRow {
    fn try_into_validated(self) -> Result<Realisation, RealisationRowError> {
        let drv_hash: [u8; 32] =
            self.drv_hash
                .as_slice()
                .try_into()
                .map_err(|_| RealisationRowError {
                    field: "drv_hash",
                    got: self.drv_hash.len(),
                })?;
        let output_hash: [u8; 32] =
            self.output_hash
                .as_slice()
                .try_into()
                .map_err(|_| RealisationRowError {
                    field: "output_hash",
                    got: self.output_hash.len(),
                })?;
        Ok(Realisation {
            drv_hash,
            output_name: self.output_name,
            output_path: self.output_path,
            output_hash,
            signatures: self.signatures,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    fn sample() -> Realisation {
        Realisation {
            drv_hash: [0xAA; 32],
            output_name: "out".into(),
            output_path: "/nix/store/00000000000000000000000000000000-ca-output".into(),
            output_hash: [0xBB; 32],
            signatures: vec!["sig:test".into()],
        }
    }

    #[tokio::test]
    async fn insert_then_query_roundtrips() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let r = sample();

        let inserted = insert(&db.pool, &r).await?;
        assert!(inserted, "first insert should return true");

        let found = query(&db.pool, &r.drv_hash, &r.output_name).await?;
        let found = found.expect("should find just-inserted realisation");
        assert_eq!(found.drv_hash, r.drv_hash);
        assert_eq!(found.output_name, r.output_name);
        assert_eq!(found.output_path, r.output_path);
        assert_eq!(found.output_hash, r.output_hash);
        assert_eq!(found.signatures, r.signatures);
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_insert_is_idempotent() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let r = sample();

        let first = insert(&db.pool, &r).await?;
        assert!(first);
        let second = insert(&db.pool, &r).await?;
        assert!(!second, "second insert should be ON CONFLICT no-op");

        // Still queryable after the no-op — ON CONFLICT DO NOTHING didn't
        // somehow delete or corrupt the existing row.
        let found = query(&db.pool, &r.drv_hash, &r.output_name).await?;
        assert!(found.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn query_missing_returns_none() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let found = query(&db.pool, &[0xFF; 32], "out").await?;
        assert!(found.is_none());
        Ok(())
    }

    /// Different output_name → different key. "out" and "dev" for the same
    /// drv_hash are independent realisations.
    #[tokio::test]
    async fn output_name_is_part_of_key() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let r_out = sample();
        let r_dev = Realisation {
            output_name: "dev".into(),
            output_path: "/nix/store/11111111111111111111111111111111-ca-dev".into(),
            ..sample()
        };

        insert(&db.pool, &r_out).await?;
        insert(&db.pool, &r_dev).await?;

        let found_out = query(&db.pool, &r_out.drv_hash, "out").await?.unwrap();
        let found_dev = query(&db.pool, &r_dev.drv_hash, "dev").await?.unwrap();
        assert_eq!(found_out.output_path, r_out.output_path);
        assert_eq!(found_dev.output_path, r_dev.output_path);
        assert_ne!(found_out.output_path, found_dev.output_path);
        Ok(())
    }
}

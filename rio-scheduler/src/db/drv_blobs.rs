//! drv_blobs lookups — ADR-024 digest-bearing submission verification.
//!
//! Scheduler+store share PG (same migrations/ dir, see `live_pins.rs`).
//! The store's `DrvBlobService` owns writes (`PutDrvBlobs` is the only
//! writer); the scheduler reads the same rows here to answer "does
//! this drv digest exist, and under which drv_path?" at SubmitBuild
//! time. Reading the table directly instead of round-tripping the
//! store's `HasDrvs` RPC keeps the bulk-verify one PG query AND
//! returns `drv_path`, which `HasDrvs`' presence bitmap does not —
//! the submit path needs the path to derive a DAG edge for an input
//! digest that resolves to an already-known derivation.

use std::collections::HashMap;

use uuid::Uuid;

use super::SchedulerDb;

impl SchedulerDb {
    /// Resolve drv digests to their stored `drv_path`s.
    ///
    /// Tenant-scoped exactly like the store's `HasDrvs`: with
    /// `Some(tenant)`, only blobs the tenant has a
    /// `drv_blob_tenants` visibility binding for resolve (same
    /// answer for "doesn't exist" and "exists but not yours");
    /// `None` (single-tenant/dev mode) consults `drv_blobs` alone.
    ///
    /// Digests absent from the result did not resolve — the caller
    /// rejects the submission listing them (the ADR-024 stale-ack
    /// recovery contract). A PG error MUST also reject (deny on
    /// failure): accepting a digest-bearing submission without
    /// verifying its blobs would dispatch builds whose drv content
    /// may be GC'd or never uploaded.
    pub async fn resolve_drv_digests(
        &self,
        digests: &[Vec<u8>],
        tenant: Option<Uuid>,
    ) -> Result<HashMap<Vec<u8>, String>, sqlx::Error> {
        if digests.is_empty() {
            return Ok(HashMap::new());
        }
        let rows: Vec<(Vec<u8>, String)> = match tenant {
            Some(t) => {
                sqlx::query_as(
                    "SELECT d.digest, d.drv_path FROM drv_blobs d \
                       JOIN drv_blob_tenants j ON j.digest = d.digest \
                      WHERE d.digest = ANY($1::bytea[]) AND j.tenant_id = $2",
                )
                .bind(digests)
                .bind(t)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT digest, drv_path FROM drv_blobs \
                      WHERE digest = ANY($1::bytea[])",
                )
                .bind(digests)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().collect())
    }
}

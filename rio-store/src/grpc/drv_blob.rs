//! DrvBlobService (ADR-024): derivations as a first-class castore
//! blob kind.
//!
//! The stored form is the canonical `rio.drv.v1.Derivation` encoding;
//! the key is `blake3(received bytes)` — the negotiation digest of the
//! ADR-024 build plan. Every put is verified server-side via
//! [`verify_drv_blob`]: digest recompute over received bytes, proto
//! decode, structural validation, canonical re-encode byte-compare,
//! ATerm reconstruction + drv_path recompute, FOD output-path
//! recompute. Any failure rejects the whole batch — non-canonical
//! bytes are NEVER stored, so `GetDrvBlob` is byte-stable by
//! construction (served bytes == received bytes == canonical bytes).
//!
//! Tenant model mirrors the rest of the castore surface
//! (`r[store.castore.tenant-scope+3]`): storage is digest-keyed global
//! (`drv_blobs`), visibility is a `drv_blob_tenants` junction written
//! by the put. `HasDrvs`/`GetDrvBlob` answer only for blobs the
//! calling tenant has uploaded. Puts are write-through idempotent
//! (unconditional upsert; re-put refreshes the GC grace clock) — no
//! present/absent timing oracle, same discipline as chunk puts.

use std::sync::Arc;

use sha2::Digest as _;
use sqlx::PgPool;
use tonic::{Request, Response, Status};
use tracing::{instrument, warn};

use rio_proto::DrvBlobService;
use rio_proto::derivation_util::verify_drv_blob;
use rio_proto::types::{
    DrvBlob, GetDrvBlobRequest, HasBitmap, HasDrvsRequest, PutDrvBlobsRequest, PutDrvBlobsResponse,
};

use super::directory::{
    HAS_BATCH_MAX, build_bitmap, parse_digest, parse_digests, resolve_castore_tenant,
};

/// Cap on blobs per `PutDrvBlobs` call. Drv blobs are ~3.4 KB mean
/// with a fat p99 tail, so the gRPC message-size cap (not this count)
/// is the practical bound; this only stops a degenerate
/// many-empty-blobs request from turning into one giant multi-row
/// INSERT.
const PUT_DRV_BATCH_MAX: usize = 4_096;

pub struct DrvBlobServiceImpl {
    pool: PgPool,
    /// Same verifier instance as `DirectoryServiceImpl`'s — the
    /// tenant ladder (JWT, else HMAC assignment-token claim) is shared
    /// via [`resolve_castore_tenant`].
    hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
}

impl DrvBlobServiceImpl {
    pub fn new(pool: PgPool, hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>) -> Self {
        Self {
            pool,
            hmac_verifier,
        }
    }

    fn tenant<T>(&self, request: &Request<T>) -> Result<uuid::Uuid, Status> {
        resolve_castore_tenant(
            request,
            self.hmac_verifier.as_ref(),
            "DrvBlobService requires a tenant: send a JWT or an HMAC \
             assignment token",
        )
    }
}

#[tonic::async_trait]
impl DrvBlobService for DrvBlobServiceImpl {
    /// Verify-then-store batch put. Verification of EVERY blob happens
    /// before any write, so a batch with one bad blob stores nothing
    /// (the client's negotiation state stays consistent: a reject
    /// means "fix and resend the batch", never "some subset landed").
    // r[impl store.drv.blob-kind]
    // r[impl store.drv.verify-on-put]
    #[instrument(skip(self, request), fields(rpc = "PutDrvBlobs"))]
    async fn put_drv_blobs(
        &self,
        request: Request<PutDrvBlobsRequest>,
    ) -> Result<Response<PutDrvBlobsResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.tenant(&request)?;
        let blobs = request.into_inner().blobs;
        rio_common::grpc::check_bound("blobs", blobs.len(), PUT_DRV_BATCH_MAX)?;
        if blobs.is_empty() {
            return Ok(Response::new(PutDrvBlobsResponse { created: vec![] }));
        }

        // Phase 1: verify everything. Hard reject on ANY failure mode
        // of `verify_drv_blob` — the error names the blob index and
        // drv path so the client can attribute the reject, and the
        // distinct `DrvBlobError` variants keep digest-mismatch /
        // non-canonical / drv-path-mismatch separable in the message.
        let mut digests: Vec<[u8; 32]> = Vec::with_capacity(blobs.len());
        for (i, b) in blobs.iter().enumerate() {
            let claimed = parse_digest(&b.digest)?;
            let v = verify_drv_blob(&b.body, &claimed, &b.drv_path).map_err(|e| {
                Status::invalid_argument(format!(
                    "drv blob {i} ({path}): {e}",
                    path = b.drv_path.escape_default()
                ))
            })?;
            debug_assert_eq!(v.digest, claimed);
            digests.push(claimed);
        }

        // Phase 2: one transaction, sorted-ascending multi-row upserts
        // (the chunk writers' lock-order discipline — concurrent
        // overlapping batches must not circular-wait on uniqueness
        // locks). Dedup within the batch: PG rejects `ON CONFLICT`
        // affecting the same row twice in one statement.
        let mut rows: Vec<(&[u8; 32], &DrvBlob)> = digests.iter().zip(blobs.iter()).collect();
        rows.sort_unstable_by_key(|(d, _)| **d);
        rows.dedup_by_key(|(d, _)| **d);
        let blob_digests: Vec<&[u8]> = rows.iter().map(|(d, _)| d.as_slice()).collect();
        let drv_paths: Vec<&str> = rows.iter().map(|(_, b)| b.drv_path.as_str()).collect();
        let path_hashes: Vec<Vec<u8>> = rows
            .iter()
            .map(|(_, b)| sha2::Sha256::digest(b.drv_path.as_bytes()).to_vec())
            .collect();
        let bodies: Vec<&[u8]> = rows.iter().map(|(_, b)| b.body.as_slice()).collect();

        let mut tx = self.pool.begin().await.map_err(internal)?;
        // Write-through idempotent: a re-put of an existing digest
        // refreshes `created_at` (restarts the GC grace clock — the
        // client just proved it still wants the blob) instead of
        // leaving a near-expiry row behind a fresh ack. Content under
        // a digest is immutable by construction (verified blake3), so
        // the overwrite-equivalent never changes bytes.
        sqlx::query(
            r#"
            INSERT INTO drv_blobs (digest, drv_path, drv_path_hash, body)
            SELECT * FROM UNNEST($1::bytea[], $2::text[], $3::bytea[], $4::bytea[])
            ON CONFLICT (digest) DO UPDATE SET created_at = now()
            "#,
        )
        .bind(&blob_digests)
        .bind(&drv_paths)
        .bind(&path_hashes)
        .bind(&bodies)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;

        // Visibility binding. RETURNING reports which junction rows
        // are NEW — that (and only that) feeds `created`, so the
        // response never discloses another tenant's prior upload.
        let inserted: Vec<Vec<u8>> = sqlx::query_scalar(
            r#"
            INSERT INTO drv_blob_tenants (digest, tenant_id)
            SELECT d, $2 FROM UNNEST($1::bytea[]) AS u(d)
            ON CONFLICT DO NOTHING
            RETURNING digest
            "#,
        )
        .bind(&blob_digests)
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await
        .map_err(internal)?;
        tx.commit().await.map_err(internal)?;

        let new: std::collections::HashSet<[u8; 32]> = inserted
            .into_iter()
            .filter_map(|d| d.try_into().ok())
            .collect();
        Ok(Response::new(PutDrvBlobsResponse {
            created: digests.iter().map(|d| new.contains(d)).collect(),
        }))
    }

    /// Byte-stable fetch: the served `body` is exactly the verified
    /// put payload. NotFound unless the calling tenant has a
    /// visibility binding (same answer for "doesn't exist" and
    /// "exists but not yours").
    // r[impl store.drv.blob-kind]
    #[instrument(skip(self, request), fields(rpc = "GetDrvBlob"))]
    async fn get_drv_blob(
        &self,
        request: Request<GetDrvBlobRequest>,
    ) -> Result<Response<DrvBlob>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.tenant(&request)?;
        let digest = parse_digest(&request.into_inner().digest)?;
        let row: Option<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT d.drv_path, d.body FROM drv_blobs d \
               JOIN drv_blob_tenants t ON t.digest = d.digest \
              WHERE d.digest = $1 AND t.tenant_id = $2",
        )
        .bind(digest.as_slice())
        .bind(tenant)
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        let Some((drv_path, body)) = row else {
            return Err(Status::not_found("drv blob not found"));
        };
        Ok(Response::new(DrvBlob {
            digest: digest.to_vec(),
            drv_path,
            body,
        }))
    }

    /// Bulk presence bitmap over drv digests — `HasBlobs` semantics
    /// (bit i set ⇔ present AND tenant-visible), reusing the shared
    /// bitmap builder so bit order cannot drift across the four
    /// presence probes.
    // r[impl store.drv.blob-kind]
    #[instrument(skip(self, request), fields(rpc = "HasDrvs"))]
    async fn has_drvs(
        &self,
        request: Request<HasDrvsRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.tenant(&request)?;
        let digests = parse_digests(&request.into_inner().digests)?;
        metrics::histogram!("rio_store_directory_has_batch_size", "rpc" => "HasDrvs")
            .record(digests.len() as f64);
        if digests.is_empty() {
            return Ok(Response::new(HasBitmap { bitmap: vec![] }));
        }
        debug_assert!(digests.len() <= HAS_BATCH_MAX);
        let slices: Vec<&[u8]> = digests.iter().map(|d| d.as_slice()).collect();
        // The junction alone answers presence: a `drv_blob_tenants`
        // row FK-references `drv_blobs` (CASCADE on delete), so a
        // visible row implies the blob exists.
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT digest FROM drv_blob_tenants \
              WHERE digest = ANY($1::bytea[]) AND tenant_id = $2",
        )
        .bind(&slices)
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        let present: std::collections::HashSet<[u8; 32]> =
            rows.into_iter().filter_map(|d| d.try_into().ok()).collect();
        Ok(Response::new(HasBitmap {
            bitmap: build_bitmap(&digests, &present),
        }))
    }
}

fn internal(e: impl std::fmt::Display) -> Status {
    warn!(error = %e, "DrvBlobService PG error");
    Status::internal("drv blob query failed")
}

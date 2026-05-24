//! `PutPathChunked` commit transaction (ADR-022 §6.2).
//!
//! One transaction across every non-skipped output: manifest_data +
//! nar_index + castore tables + chunk refcounts + the `durable` flip +
//! signed narinfo + the `'uploading' → 'complete'` status flip. Any
//! failure rolls the whole batch back; the caller's `PlaceholderGuard`s
//! reap the placeholders and the already-uploaded S3 chunks orphan into
//! the `r[store.chunk.grace-ttl]` sweep.
//!
//! This is also where `PutPathChunked` outputs become servable by
//! `GetDirectory`/`ReadBlob`/`StatBlob`: the castore tables are written
//! here, synchronously, not by a later indexer pass — and
//! `manifests.nar_indexed` is set so the background indexer never
//! re-derives (and double-counts) what this transaction already wrote.

use sqlx::{Postgres, Transaction};
use tonic::Status;
use tracing::warn;

use crate::metadata;

use super::super::{StoreServiceImpl, putpath_metadata_status};
use super::validate::{ValidatedBegin, ValidatedOutput};

impl StoreServiceImpl {
    /// Commit every non-skipped output in one transaction.
    ///
    /// `claims[i]` is the placeholder ownership token for output `i`
    /// (`None` only for skipped outputs — every non-skipped output
    /// holds a claim by the time this runs, CA outputs included).
    // r[impl store.atomic.multi-output]
    // r[impl store.castore.tenant-scope]
    pub(super) async fn commit_chunked(
        &self,
        validated: &ValidatedBegin,
        skipped: &[bool],
        claims: &[Option<uuid::Uuid>],
        hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
        resolved_signer: Option<&(crate::signing::Signer, bool)>,
    ) -> Result<(), Status> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| rio_common::grpc::internal("PutPathChunked: begin transaction", e))?;

        for (i, out) in validated.outputs.iter().enumerate() {
            if skipped[i] {
                continue;
            }
            let claim = claims[i].expect("non-skipped output holds a placeholder claim");
            if let Err(e) = commit_one(&mut tx, validated, out, claim, resolved_signer, self).await
            {
                drop(tx);
                return Err(e);
            }
        }

        // Tenant visibility for ALL outputs, including idempotent-
        // skipped ones: the prior commit may have happened before this
        // tenant existed (or via a path that didn't write the
        // junction). `path_tenants` is keyed (store_path_hash, tenant)
        // — it grants narinfo/castore visibility, not content, so
        // writing it for a skipped output whose content we did NOT
        // verify is safe: the tenant legitimately requested this build.
        // The FK to `tenants` is guarded with an EXISTS so a deleted
        // tenant degrades to "no row" instead of failing the commit.
        if let Some(tenant) = hmac_claims.and_then(|c| c.tenant.as_deref())
            && let Ok(tenant_id) = tenant.parse::<uuid::Uuid>()
        {
            for out in &validated.outputs {
                if let Err(e) = sqlx::query(
                    "INSERT INTO path_tenants (store_path_hash, tenant_id) \
                     SELECT $1, t.tenant_id FROM tenants t WHERE t.tenant_id = $2 \
                     ON CONFLICT DO NOTHING",
                )
                .bind(&out.info.store_path_hash)
                .bind(tenant_id)
                .execute(&mut *tx)
                .await
                {
                    drop(tx);
                    return Err(rio_common::grpc::internal(
                        "PutPathChunked: path_tenants insert",
                        e,
                    ));
                }
            }
        }

        tx.commit()
            .await
            .map_err(|e| rio_common::grpc::internal("PutPathChunked: commit", e))?;
        Ok(())
    }
}

/// Commit one output inside the batch transaction.
async fn commit_one(
    tx: &mut Transaction<'_, Postgres>,
    validated: &ValidatedBegin,
    out: &ValidatedOutput,
    claim: uuid::Uuid,
    resolved_signer: Option<&(crate::signing::Signer, bool)>,
    svc: &StoreServiceImpl,
) -> Result<(), Status> {
    let hash = out.info.store_path_hash.as_slice();

    // --- manifest_data -------------------------------------------------
    // The placeholder row exists with status='uploading' and our
    // claim_id; the manifest_data row must not (the placeholder never
    // writes one). A PK conflict here means a concurrent writer slipped
    // past the claim gate — fail loudly rather than clobber.
    sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
        .bind(hash)
        .bind(&out.chunk_list_bytes)
        .execute(&mut **tx)
        .await
        .map_err(|e| putpath_metadata_status("PutPathChunked: manifest_data insert", e.into()))?;

    // --- chunk refcounts ----------------------------------------------
    // One ref per unique chunk per manifest (matching the GC sweep's
    // per-manifest decrement). Sorted per r[store.chunk.lock-order].
    // `deleted = false` resurrects a swept-but-not-yet-drained chunk;
    // `uploaded_at` is left alone — the verify walk has already either
    // written the chunk (novel) or read it back (deduped), so presence
    // is established either way and the legacy column only drives the
    // legacy upload-skip heuristic.
    // r[impl store.chunk.lock-order]
    {
        let hashes: Vec<Vec<u8>> = out.unique_chunks.iter().map(|(h, _)| h.to_vec()).collect();
        let sizes: Vec<i64> = out
            .unique_chunks
            .iter()
            .map(|(_, s)| i64::from(*s))
            .collect();
        sqlx::query(
            r#"
            INSERT INTO chunks (blake3_hash, refcount, size)
            SELECT * FROM UNNEST($1::bytea[], $2::bigint[], $3::bigint[]) AS t(hash, one, size)
            ON CONFLICT (blake3_hash) DO UPDATE
               SET refcount = chunks.refcount + 1, deleted = false
            "#,
        )
        .bind(&hashes)
        .bind(vec![1i64; hashes.len()])
        .bind(&sizes)
        .execute(&mut **tx)
        .await
        .map_err(|e| putpath_metadata_status("PutPathChunked: chunk refcounts", e.into()))?;

        // Durable: the manifest referencing these chunks becomes
        // 'complete' in this transaction.
        metadata::mark_chunks_durable(tx, &hashes)
            .await
            .map_err(|e| {
                putpath_metadata_status("PutPathChunked: mark_chunks_durable", e.into())
            })?;
    }

    // --- nar_index + castore tables ------------------------------------
    // Mirrors `metadata::set_nar_index` but runs inside the batch
    // transaction with the already-validated DAG (no NAR reassembly, no
    // nar_ls). `nar_indexed` is flipped on the manifests row below as
    // part of the status UPDATE.
    sqlx::query(
        "INSERT INTO nar_index (store_path_hash, entries, root_node) VALUES ($1, $2, $3) \
         ON CONFLICT (store_path_hash) DO NOTHING",
    )
    .bind(hash)
    .bind(&out.nar_index_entries)
    .bind(&out.root_node_encoded)
    .execute(&mut **tx)
    .await
    .map_err(|e| putpath_metadata_status("PutPathChunked: nar_index insert", e.into()))?;

    if !out.dir_digests.is_empty() {
        // r[impl store.castore.gc]
        // refcount += 1 per output-tree occurrence; the GC sweep
        // decrements symmetrically via `digests_from_index`.
        let dir_digests: Vec<Vec<u8>> = out.dir_digests.iter().map(|d| d.to_vec()).collect();
        let dir_bodies: Vec<Vec<u8>> = out
            .dir_digests
            .iter()
            .map(|d| {
                prost::Message::encode_to_vec(
                    validated
                        .directories
                        .get(d)
                        .expect("walk only records digests present in the validated map"),
                )
            })
            .collect();
        sqlx::query(
            r#"
            INSERT INTO directories (digest, body, refcount)
            SELECT u.digest, u.body, 1
              FROM UNNEST($1::bytea[], $2::bytea[]) AS u(digest, body)
            ON CONFLICT (digest) DO UPDATE
               SET refcount = directories.refcount + 1
            "#,
        )
        .bind(&dir_digests)
        .bind(&dir_bodies)
        .execute(&mut **tx)
        .await
        .map_err(|e| putpath_metadata_status("PutPathChunked: directories upsert", e.into()))?;
        sqlx::query(
            r#"
            INSERT INTO directory_paths (digest, store_path_hash)
            SELECT u.digest, $2 FROM UNNEST($1::bytea[]) AS u(digest)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&dir_digests)
        .bind(hash)
        .execute(&mut **tx)
        .await
        .map_err(|e| putpath_metadata_status("PutPathChunked: directory_paths insert", e.into()))?;
    }
    if !out.file_blobs.is_empty() {
        let mut digests: Vec<Vec<u8>> = Vec::with_capacity(out.file_blobs.len());
        let mut offsets: Vec<i64> = Vec::with_capacity(out.file_blobs.len());
        let mut sizes: Vec<i64> = Vec::with_capacity(out.file_blobs.len());
        for (d, o, s) in &out.file_blobs {
            digests.push(d.to_vec());
            offsets.push(i64::try_from(*o).unwrap_or(i64::MAX));
            sizes.push(i64::try_from(*s).unwrap_or(i64::MAX));
        }
        sqlx::query(
            r#"
            INSERT INTO file_blobs (digest, store_path_hash, nar_offset, size)
            SELECT u.digest, $2, u.nar_offset, u.size
              FROM UNNEST($1::bytea[], $3::bigint[], $4::bigint[]) AS u(digest, nar_offset, size)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&digests)
        .bind(hash)
        .bind(&offsets)
        .bind(&sizes)
        .execute(&mut **tx)
        .await
        .map_err(|e| putpath_metadata_status("PutPathChunked: file_blobs insert", e.into()))?;
    }

    // --- narinfo + status flip -----------------------------------------
    // The narinfo row is signed over the SERVER-COMPUTED nar_hash (the
    // verify walk asserted it equals the claimed one, so `out.info`'s
    // value is already the verified value). r[store.signing.fingerprint]
    let mut info = out.info.clone();
    info.registration_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Some((signer, was_tenant)) = resolved_signer {
        svc.sign_with_resolved(signer, *was_tenant, &mut info);
    }
    metadata::complete_manifest_in_conn(&mut *tx, &info, claim, None)
        .await
        .map_err(|e| putpath_metadata_status("PutPathChunked: complete_manifest", e))?;
    // The index was written synchronously above — stop the background
    // indexer from re-deriving it (and double-counting the directory
    // refcounts).
    if let Err(e) =
        sqlx::query("UPDATE manifests SET nar_indexed = TRUE WHERE store_path_hash = $1")
            .bind(hash)
            .execute(&mut **tx)
            .await
    {
        warn!(error = %e, "PutPathChunked: nar_indexed flip failed");
        return Err(putpath_metadata_status(
            "PutPathChunked: nar_indexed flip",
            e.into(),
        ));
    }
    Ok(())
}

//! Read-side queries: path info lookup, missing-path batch check, NAR
//! reassembly (`get_manifest`), and signature append.
//!
//! All read queries filter on `manifests.status = 'complete'` — placeholders
//! from in-progress or crashed uploads are invisible. `get_manifest` is the
//! ONE place the inline-vs-chunked branch lives; callers never touch
//! `manifests`/`manifest_data` directly.

use super::*;
use sqlx::PgPool;
use tracing::instrument;

// narinfo_cols! is #[macro_export] so it lands at crate root — re-import.
use crate::narinfo_cols;

/// Fetch the storage kind + content for a completed path.
///
/// This is THE one place the inline/chunked branch is implemented. Callers
/// (GetPath, cache_server) match on the result; they never query manifests
/// or manifest_data directly.
///
/// `None` means the path has no complete manifest (either never uploaded,
/// or stuck in 'uploading' from a crashed PutPath).
#[instrument(skip(pool))]
pub async fn get_manifest(pool: &PgPool, store_path: &str) -> Result<Option<ManifestKind>> {
    // Single query: join narinfo→manifests, filter status='complete',
    // pull inline_blob. manifest_data is a second query only if needed
    // (avoids pulling a potentially-large chunk_list we won't use).
    let row: Option<(Option<Vec<u8>>,)> = sqlx::query_as(
        r#"
        SELECT m.inline_blob
        FROM manifests m
        INNER JOIN narinfo n ON n.store_path_hash = m.store_path_hash
        WHERE n.store_path = $1 AND m.status = 'complete'
        "#,
    )
    .bind(store_path)
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some((Some(blob),)) => Ok(Some(ManifestKind::Inline(Bytes::from(blob)))),
        // inline_blob NULL → chunked. Fetch + deserialize manifest_data.
        Some((None,)) => {
            let data: Option<(Vec<u8>,)> = sqlx::query_as(
                r#"
                SELECT md.chunk_list
                FROM manifest_data md
                INNER JOIN narinfo n ON n.store_path_hash = md.store_path_hash
                WHERE n.store_path = $1
                "#,
            )
            .bind(store_path)
            .fetch_optional(pool)
            .await?;

            // manifest exists + inline_blob NULL but NO manifest_data row:
            // invariant violation (store.md:222 says inline_blob NULL ⇔
            // manifest_data exists). Possible causes: manual DB surgery,
            // a bug in delete_manifest_chunked_uploading ordering, or a
            // CASCADE we didn't expect. Surface it — don't silently return
            // None (that would look like "path not found", masking corruption).
            let (chunk_list,) = data.ok_or_else(|| {
                MetadataError::InvariantViolation(format!(
                    "manifest for {store_path} has NULL inline_blob but no manifest_data row"
                ))
            })?;

            let manifest =
                crate::manifest::Manifest::deserialize(&chunk_list).map_err(|source| {
                    MetadataError::CorruptManifest {
                        store_path: store_path.to_string(),
                        source,
                    }
                })?;

            // Convert Manifest → Vec<([u8;32], u32)> for the enum variant.
            // ManifestKind predates Manifest (E1 vs C1) so the representation
            // is slightly different; this is one allocation, cheap.
            let entries: Vec<([u8; 32], u32)> = manifest
                .entries
                .into_iter()
                .map(|e| (e.hash, e.size))
                .collect();

            Ok(Some(ManifestKind::Chunked(entries)))
        }
    }
}

/// Query path info for a store path.
///
/// Only returns paths with `manifests.status = 'complete'`. Placeholders
/// (status = 'uploading') and orphans (narinfo row but no manifests row)
/// are invisible.
#[instrument(skip(pool))]
pub async fn query_path_info(pool: &PgPool, store_path: &str) -> Result<Option<ValidatedPathInfo>> {
    let row: Option<NarinfoRow> = sqlx::query_as(concat!(
        "SELECT ",
        narinfo_cols!(),
        " FROM narinfo n \
         INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash \
         WHERE n.store_path = $1 AND m.status = 'complete'"
    ))
    .bind(store_path)
    .fetch_optional(pool)
    .await?;

    validate_row(row)
}

/// Batch check which store paths are missing.
///
/// "Missing" means no manifests row with `status = 'complete'`. Paths stuck
/// in 'uploading' (crashed PutPath) are missing — the client should retry.
#[instrument(skip(pool, store_paths), fields(count = store_paths.len()))]
pub async fn find_missing_paths(pool: &PgPool, store_paths: &[String]) -> Result<Vec<String>> {
    if store_paths.is_empty() {
        return Ok(Vec::new());
    }

    let complete: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT n.store_path
        FROM narinfo n
        INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash
        WHERE n.store_path = ANY($1) AND m.status = 'complete'
        "#,
    )
    .bind(store_paths)
    .fetch_all(pool)
    .await?;

    let complete_set: std::collections::HashSet<&str> =
        complete.iter().map(|(p,)| p.as_str()).collect();

    Ok(store_paths
        .iter()
        .filter(|p| !complete_set.contains(p.as_str()))
        .cloned()
        .collect())
}

/// Resolve a store path from its NAR hash.
///
/// Used by the binary cache HTTP server's `/nar/{narhash}.nar.zst` route:
/// the narinfo we serve has `URL: nar/{nixbase32(nar_hash)}.nar.zst`, so
/// when the client fetches that URL, we need to find the path by nar_hash.
///
/// Uses `idx_narinfo_nar_hash` (migration 002_store.sql). Without it, every NAR
/// fetch would seq-scan narinfo.
#[instrument(skip(pool), fields(nar_hash = hex::encode(nar_hash)))]
pub async fn path_by_nar_hash(pool: &PgPool, nar_hash: &[u8; 32]) -> Result<Option<String>> {
    // Multiple paths CAN have the same nar_hash (two fetchurl of the
    // same file → same content → same NAR). LIMIT 1 picks one
    // arbitrarily — they all reassemble to the same bytes, so it
    // doesn't matter which we serve.
    let row: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT n.store_path
        FROM narinfo n
        INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash
        WHERE n.nar_hash = $1 AND m.status = 'complete'
        LIMIT 1
        "#,
    )
    .bind(nar_hash.as_slice())
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(p,)| p))
}

/// Resolve a store path from its 32-char nixbase32 hash part.
///
/// Nix store paths look like `/nix/store/{hash-part}-{name}`. The daemon's
/// wopQueryPathFromHashPart hands us just `{hash-part}`; we need to find the
/// full path. The hash-part uniquely identifies a path (it's a truncated hash
/// of the full fingerprint), so a LIKE prefix match is correct, not just
/// convenient.
///
/// `hash_part` is expected to be exactly 32 nixbase32 chars. The caller
/// (gRPC layer) validates length and charset — we don't re-check here.
/// An unvalidated `hash_part` with `%` or `_` in it would be a LIKE-
/// injection (matches anything / anything-one-char); the gRPC validation
/// prevents that by rejecting non-nixbase32 chars upfront.
#[instrument(skip(pool))]
pub async fn query_by_hash_part(
    pool: &PgPool,
    hash_part: &str,
) -> Result<Option<ValidatedPathInfo>> {
    // LIKE pattern: the store prefix + hash + dash + anything.
    // The dash is important — without it, a hash-part "aaa...a" would also
    // match a hypothetical "aaa...ab-name" (32-char hash that happens to
    // prefix-extend). Nix store paths always have the dash separator, so
    // including it makes the match exact.
    let pattern = format!("/nix/store/{hash_part}-%");

    let row: Option<NarinfoRow> = sqlx::query_as(concat!(
        "SELECT ",
        narinfo_cols!(),
        " FROM narinfo n \
         INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash \
         WHERE n.store_path LIKE $1 AND m.status = 'complete'"
    ))
    .bind(&pattern)
    .fetch_optional(pool)
    .await?;

    validate_row(row)
}

/// Append signatures to an existing narinfo.
///
/// Does NOT deduplicate — if the client sends the same sig twice, we store
/// it twice. Nix's own AddSignatures has the same behavior; dedup is the
/// client's responsibility. The `signatures` column is TEXT[], and PG's
/// `||` (array concat) preserves order.
///
/// Returns the number of rows updated (0 = path not found, 1 = appended).
/// Caller maps 0 to NOT_FOUND.
#[instrument(skip(pool, sigs), fields(count = sigs.len()))]
pub async fn append_signatures(pool: &PgPool, store_path: &str, sigs: &[String]) -> Result<u64> {
    // WHERE ... = $1 (not LIKE): this takes a full path. Only
    // query_by_hash_part does prefix matching.
    //
    // No manifests-join here: signatures are metadata on narinfo, independent
    // of whether the NAR content is complete. In practice clients sign AFTER
    // uploading (they need the nar_hash), so the manifest is always complete
    // when this is called — but coupling this function to that assumption
    // would break `nix store sign` against a path whose upload got stuck.
    let result = sqlx::query(
        r#"
        UPDATE narinfo SET signatures = signatures || $2
        WHERE store_path = $1
        "#,
    )
    .bind(store_path)
    .bind(sigs)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

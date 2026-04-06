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
            // ManifestKind uses tuples; Manifest uses a struct — one cheap conversion.
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

    let complete = sqlx::query!(
        r#"
        SELECT n.store_path
        FROM narinfo n
        INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash
        WHERE n.store_path = ANY($1) AND m.status = 'complete'
        "#,
        store_paths,
    )
    .fetch_all(pool)
    .await?;

    let complete_set: std::collections::HashSet<&str> =
        complete.iter().map(|r| r.store_path.as_str()).collect();

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
    let row = sqlx::query!(
        r#"
        SELECT n.store_path
        FROM narinfo n
        INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash
        WHERE n.nar_hash = $1 AND m.status = 'complete'
        LIMIT 1
        "#,
        nar_hash.as_slice(),
    )
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| r.store_path))
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

/// Tenant-scoped variant of [`query_by_hash_part`]: only returns the
/// path if `path_tenants` has a row linking it to `tenant_id`.
///
/// Same LIKE prefix match, plus an inner JOIN on `path_tenants` keyed
/// by `store_path_hash` + `tenant_id = $2`. If the tenant never built
/// (or was never attributed) the path, the JOIN eliminates the row →
/// `None` → caller returns 404 (indistinguishable from "path doesn't
/// exist at all" — no existence oracle).
///
/// Caller validates `hash_part` (LIKE-injection guard, same as the
/// unfiltered variant).
#[instrument(skip(pool))]
pub async fn query_by_hash_part_for_tenant(
    pool: &PgPool,
    hash_part: &str,
    tenant_id: uuid::Uuid,
) -> Result<Option<ValidatedPathInfo>> {
    let pattern = format!("/nix/store/{hash_part}-%");

    // JOIN on path_tenants (composite PK (store_path_hash, tenant_id)
    // — migration 012). The equality on $2 filters to this tenant's
    // rows only; the PK index makes this a point lookup after the
    // narinfo LIKE resolves.
    let row: Option<NarinfoRow> = sqlx::query_as(concat!(
        "SELECT ",
        narinfo_cols!(),
        " FROM narinfo n \
         INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash \
         INNER JOIN path_tenants pt ON pt.store_path_hash = n.store_path_hash \
         WHERE n.store_path LIKE $1 AND m.status = 'complete' AND pt.tenant_id = $2"
    ))
    .bind(&pattern)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await?;

    validate_row(row)
}

/// Append signatures to an existing narinfo, deduplicating.
///
/// Deduplicates server-side via `array(SELECT DISTINCT unnest(old || new))`.
/// Previously: `signatures || $2` (plain concat, no dedup) → 1000 ×
/// AddSignatures(same_sig) bloated the array to 1000 entries, and every
/// subsequent `query_path_info` paid for the oversized row. Nix's own
/// AddSignatures doesn't dedup, so we're NOT breaking client expectations
/// — dedup is an improvement, not a behavior change clients could observe.
///
/// `RETURNING cardinality` lets us check the post-dedup count in one
/// round-trip. Over `MAX_SIGNATURES` after dedup → the client sent novel
/// garbage sigs AND we grew past the cap → reject with
/// `ResourceExhausted` (maps to gRPC `RESOURCE_EXHAUSTED` — client
/// backs off, operator alerts on the status code).
///
/// Returns rows updated (0 = path not found, 1 = appended). Caller maps
/// 0 to NOT_FOUND.
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
    //
    // fetch_optional + RETURNING: None = path not found; Some(count) =
    // post-dedup cardinality. Disambiguates not-found from at-cap.
    let row = sqlx::query!(
        r#"
        UPDATE narinfo
           SET signatures = array(
                 SELECT DISTINCT unnest(signatures || $2::text[])
               )
         WHERE store_path = $1
        RETURNING cardinality(signatures) AS "n!"
        "#,
        store_path,
        sigs,
    )
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(0), // not found — caller maps to NOT_FOUND
        Some(r) if r.n as usize > rio_common::limits::MAX_SIGNATURES => {
            // Over cap post-dedup → client sent novel sigs and we grew
            // past the limit. Reject: clearer than silently truncating.
            // The row was updated (UPDATE committed), but we signal the
            // client their sigs pushed us over — operator alert-worthy.
            Err(MetadataError::ResourceExhausted(format!(
                "signature count {} exceeds MAX_SIGNATURES ({})",
                r.n,
                rio_common::limits::MAX_SIGNATURES
            )))
        }
        Some(_) => Ok(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::{TestDb, seed_tenant};
    use sqlx::PgPool;

    /// Seed a complete path (narinfo + manifests status='complete').
    /// Returns (store_path_hash, nar_hash).
    async fn seed_complete(
        pool: &PgPool,
        path: &str,
        inline_blob: Option<&[u8]>,
    ) -> (Vec<u8>, [u8; 32]) {
        use sha2::Digest;
        let hash: Vec<u8> = sha2::Sha256::digest(path.as_bytes()).to_vec();
        let nar_hash = {
            let mut h = [0u8; 32];
            h[0] = 0xAA;
            h
        };
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, $2, $3, 100)",
        )
        .bind(&hash)
        .bind(path)
        .bind(&nar_hash[..])
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', $2)",
        )
        .bind(&hash)
        .bind(inline_blob)
        .execute(pool)
        .await
        .unwrap();
        (hash, nar_hash)
    }

    /// Empty input → empty output, no DB round-trip.
    #[tokio::test]
    async fn find_missing_paths_empty_input() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let result = find_missing_paths(&db.pool, &[]).await.unwrap();
        assert!(result.is_empty());
    }

    /// `query_by_hash_part_for_tenant`: the JOIN on path_tenants
    /// scopes to one tenant. Same hash-part, three outcomes depending
    /// on which tenant asks.
    #[tokio::test]
    async fn query_by_hash_part_tenant_scoped() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // seed_complete's hash-part is the 32 'a's after /nix/store/.
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-tenant-scoped";
        let (ph, _) = seed_complete(&db.pool, path, Some(b"blob")).await;

        let tenant_a = seed_tenant(&db.pool, "a").await;
        let tenant_b = seed_tenant(&db.pool, "b").await;

        // Attribute only to tenant A.
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(&ph)
            .bind(tenant_a)
            .execute(&db.pool)
            .await
            .unwrap();

        let hash_part = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        // Tenant A → found.
        let got = query_by_hash_part_for_tenant(&db.pool, hash_part, tenant_a)
            .await
            .unwrap();
        assert!(
            got.as_ref().is_some_and(|i| i.store_path.as_str() == path),
            "tenant A owns the path"
        );

        // Tenant B → None (path exists in narinfo but not attributed).
        let got = query_by_hash_part_for_tenant(&db.pool, hash_part, tenant_b)
            .await
            .unwrap();
        assert!(got.is_none(), "tenant B does NOT own the path → None");

        // Unfiltered variant → found (control: proves the path IS
        // there, so the None above is the JOIN filtering, not a typo
        // in the hash-part).
        let got = query_by_hash_part(&db.pool, hash_part).await.unwrap();
        assert!(got.is_some(), "unfiltered sees the path");
    }

    /// `path_by_nar_hash` — only caller is the HTTP cache server's
    /// `/nar/{hash}.nar.zst` route. Never hit by gRPC integration tests.
    #[tokio::test]
    async fn path_by_nar_hash_found_and_not_found() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-found-by-hash";
        let (_ph, nar_hash) = seed_complete(&db.pool, path, Some(b"blob")).await;

        // Found: nar_hash matches.
        let found = path_by_nar_hash(&db.pool, &nar_hash).await.unwrap();
        assert_eq!(found.as_deref(), Some(path));

        // Not found: different hash.
        let missing = path_by_nar_hash(&db.pool, &[0xFF; 32]).await.unwrap();
        assert_eq!(missing, None);
    }

    /// `get_manifest` invariant violation: manifest.inline_blob=NULL
    /// but NO manifest_data row. Store doc says these are
    /// mutually exclusive; this state indicates corruption.
    #[tokio::test]
    async fn get_manifest_invariant_violation() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-invariant-viol";
        // inline_blob=NULL but we DON'T insert manifest_data.
        seed_complete(&db.pool, path, None).await;

        let err = get_manifest(&db.pool, path).await.unwrap_err();
        assert!(
            matches!(&err, MetadataError::InvariantViolation(s) if s.contains("NULL inline_blob")),
            "expected InvariantViolation, got {err:?}"
        );
    }

    /// `get_manifest` corrupt chunk_list: manifest_data exists but
    /// deserialize fails → CorruptManifest (mapped to DataLoss in gRPC).
    #[tokio::test]
    async fn get_manifest_corrupt_chunk_list() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = "/nix/store/cccccccccccccccccccccccccccccccc-corrupt";
        let (ph, _) = seed_complete(&db.pool, path, None).await;

        // Insert garbage into manifest_data — Manifest::deserialize
        // will reject (unknown version or bad length).
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&ph)
            .bind(b"garbage bytes not a manifest".as_slice())
            .execute(&db.pool)
            .await
            .unwrap();

        let err = get_manifest(&db.pool, path).await.unwrap_err();
        assert!(
            matches!(&err, MetadataError::CorruptManifest { store_path, .. } if store_path == path),
            "expected CorruptManifest, got {err:?}"
        );
    }

    /// `get_manifest` happy paths: inline and chunked both return
    /// the right `ManifestKind` variant.
    #[tokio::test]
    async fn get_manifest_inline_and_chunked() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Inline: inline_blob set.
        let inline_path = "/nix/store/dddddddddddddddddddddddddddddddd-inline";
        seed_complete(&db.pool, inline_path, Some(b"inline content")).await;
        let kind = get_manifest(&db.pool, inline_path).await.unwrap().unwrap();
        assert!(
            matches!(kind, ManifestKind::Inline(b) if &b[..] == b"inline content"),
            "expected Inline variant"
        );

        // Chunked: inline_blob=NULL, valid manifest_data.
        let chunked_path = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-chunked";
        let (ph, _) = seed_complete(&db.pool, chunked_path, None).await;
        let manifest = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: [0x11; 32],
                size: 4096,
            }],
        }
        .serialize();
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&ph)
            .bind(&manifest)
            .execute(&db.pool)
            .await
            .unwrap();
        let kind = get_manifest(&db.pool, chunked_path).await.unwrap().unwrap();
        match kind {
            ManifestKind::Chunked(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0, [0x11; 32]);
                assert_eq!(entries[0].1, 4096);
            }
            _ => panic!("expected Chunked variant"),
        }

        // Not found: unknown path.
        let missing = get_manifest(&db.pool, "/nix/store/nonexistent")
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    /// T6: append_signatures dedups. Same sig twice → array length 1.
    #[tokio::test]
    async fn append_signatures_dedups() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = "/nix/store/ffffffffffffffffffffffffffffffff-sigdedup";
        seed_complete(&db.pool, path, Some(b"x")).await;

        // Append the same sig twice, in separate calls.
        let r1 = append_signatures(&db.pool, path, &["sig:abc".into()])
            .await
            .unwrap();
        assert_eq!(r1, 1, "first append: 1 row updated");
        let r2 = append_signatures(&db.pool, path, &["sig:abc".into()])
            .await
            .unwrap();
        assert_eq!(r2, 1, "second append: still 1 row updated (idempotent)");

        // Verify dedup: array length is 1, not 2.
        let (sigs,): (Vec<String>,) =
            sqlx::query_as("SELECT signatures FROM narinfo WHERE store_path = $1")
                .bind(path)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(sigs.len(), 1, "dedup should collapse repeated sig");
        assert_eq!(sigs[0], "sig:abc");
    }

    /// T6: append_signatures distinguishes not-found (0) from success (1).
    #[tokio::test]
    async fn append_signatures_not_found_returns_zero() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let r = append_signatures(
            &db.pool,
            "/nix/store/00000000000000000000000000000000-nope",
            &["sig:x".into()],
        )
        .await
        .unwrap();
        assert_eq!(r, 0, "unknown path → 0 rows");
    }

    /// Over-cap post-dedup → ResourceExhausted (maps to gRPC
    /// RESOURCE_EXHAUSTED so clients back off instead of treating
    /// it as an internal error).
    #[tokio::test]
    async fn append_signatures_over_cap_rejects() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = "/nix/store/gggggggggggggggggggggggggggggggg-sigcap";
        seed_complete(&db.pool, path, Some(b"x")).await;

        // Seed MAX_SIGNATURES distinct sigs first (at cap, no error).
        let at_cap: Vec<String> = (0..rio_common::limits::MAX_SIGNATURES)
            .map(|i| format!("sig:seed{i}"))
            .collect();
        let r = append_signatures(&db.pool, path, &at_cap).await.unwrap();
        assert_eq!(r, 1, "at cap is OK");

        // One MORE novel sig → post-dedup count = MAX+1 → reject.
        let err = append_signatures(&db.pool, path, &["sig:overflow".into()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, MetadataError::ResourceExhausted(_)),
            "expected ResourceExhausted, got {err:?}"
        );
    }
}

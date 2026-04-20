//! Read-side queries: path info lookup, missing-path batch check, NAR
//! reassembly (`get_manifest`), and signature append.
//!
//! All read queries filter on `manifests.status = 'complete'` — placeholders
//! from in-progress or crashed uploads are invisible. `get_manifest` is the
//! ONE place the inline-vs-chunked branch lives; callers never touch
//! `manifests`/`manifest_data` directly.

use super::*;
use rio_nix::store_path::StorePath;
use sqlx::PgPool;
use tracing::instrument;

// narinfo_cols! is #[macro_export] so it lands at crate root — re-import.
use crate::narinfo_cols;

/// `SELECT <narinfo cols>[extra] FROM narinfo n JOIN manifests m … WHERE
/// <pred> AND m.status='complete'`. Every read query that returns full
/// `NarinfoRow`s shares this shell; only the WHERE predicate (and
/// `get_manifest_batch`'s extra `m.inline_blob` column) varies. Factored
/// so the JOIN clause + status filter can't drift between the four
/// callers (I-078 made the index strategy load-bearing).
macro_rules! narinfo_complete_select {
    ($pred:literal) => {
        narinfo_complete_select!($pred, "", "")
    };
    ($pred:literal, $extra_cols:literal) => {
        narinfo_complete_select!($pred, $extra_cols, "")
    };
    ($pred:literal, $extra_cols:literal, $extra_join:literal) => {
        concat!(
            "SELECT ",
            narinfo_cols!(),
            $extra_cols,
            " FROM narinfo n \
             INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash ",
            $extra_join,
            " WHERE ",
            $pred,
            " AND m.status = 'complete'"
        )
    };
}

/// Batch hash prep: parse + SHA-256 each path into the `bytea[]` shape
/// the `= ANY($1)` queries bind. Shared by every `*_batch` reader.
fn batch_hashes(paths: &[String]) -> Result<Vec<Vec<u8>>> {
    paths
        .iter()
        .map(|p| path_hash(p).map(|h| h.to_vec()))
        .collect()
}

/// SHA-256 of the full path string — the `narinfo` PK. The gRPC layer
/// has already run `validate_store_path` (which calls `StorePath::
/// parse`), so this re-parse can't fail; the `Err` path is for direct
/// callers (tests) that may pass unvalidated strings.
fn path_hash(store_path: &str) -> Result<[u8; 32]> {
    Ok(StorePath::parse(store_path)
        .map_err(|e| MetadataError::InvariantViolation(format!("unparseable {store_path}: {e}")))?
        .sha256_digest())
}

/// Fetch the storage kind + content for a completed path.
///
/// This is THE one place the inline/chunked branch is implemented. Callers
/// (GetPath) match on the result; they never query manifests or
/// manifest_data directly.
///
/// `None` means the path has no complete manifest (either never uploaded,
/// or stuck in 'uploading' from a crashed PutPath).
#[instrument(skip(pool))]
pub async fn get_manifest(pool: &PgPool, store_path: &str) -> Result<Option<ManifestKind>> {
    let hash = path_hash(store_path)?;
    // Single LEFT JOIN: both halves of the inline/chunked branch in
    // ONE statement = ONE MVCC snapshot. Filter on the manifests PK
    // directly (no narinfo join needed — both tables key on
    // store_path_hash). I-078: the previous narinfo-join +
    // `n.store_path = $1` filter was a Seq Scan.
    //
    // Two separate `.fetch_optional(pool)` calls (the previous shape)
    // race with GC sweep: under READ COMMITTED each statement takes a
    // fresh snapshot, so sweep's `DELETE FROM narinfo` CASCADE can
    // commit between them — query 1 sees the chunked `manifests` row,
    // query 2 sees no `manifest_data` → spurious InvariantViolation
    // for what is semantically NotFound. A READ COMMITTED `BEGIN` does
    // NOT fix that (each statement re-snapshots); LEFT JOIN does.
    type ManifestRow = (Option<Vec<u8>>, Option<Vec<u8>>);
    let row: Option<ManifestRow> = sqlx::query_as(
        r#"
        SELECT m.inline_blob, md.chunk_list
        FROM manifests m
        LEFT JOIN manifest_data md USING (store_path_hash)
        WHERE m.store_path_hash = $1 AND m.status = 'complete'
        "#,
    )
    .bind(hash.as_slice())
    .fetch_optional(pool)
    .await?;

    match row {
        None => Ok(None),
        Some((Some(blob), _)) => Ok(Some(ManifestKind::Inline(Bytes::from(blob)))),
        Some((None, Some(chunk_list))) => Ok(Some(ManifestKind::Chunked(decode_chunk_list(
            store_path,
            &chunk_list,
        )?))),
        // manifest exists + inline_blob NULL but NO manifest_data row:
        // invariant violation (store.md:222 says inline_blob NULL ⇔
        // manifest_data exists). Single-snapshot read; this arm is now
        // ONLY reachable via genuine corruption (manual DB surgery or
        // a CASCADE bug) — concurrent GC cannot produce it. Surface
        // it — don't silently return None (that would look like "path
        // not found", masking corruption).
        Some((None, None)) => Err(MetadataError::InvariantViolation(format!(
            "manifest for {store_path} has NULL inline_blob but no manifest_data row"
        ))),
    }
}

/// Deserialize a `manifest_data.chunk_list` blob into the
/// `ManifestKind::Chunked` tuple shape. Shared by `get_manifest` and
/// `get_manifest_batch`.
fn decode_chunk_list(store_path: &str, chunk_list: &[u8]) -> Result<Vec<([u8; 32], u32)>> {
    let manifest = crate::manifest::Manifest::deserialize(chunk_list).map_err(|source| {
        MetadataError::CorruptManifest {
            store_path: store_path.to_string(),
            source,
        }
    })?;
    Ok(manifest
        .entries
        .into_iter()
        .map(|e| (e.hash, e.size))
        .collect())
}

/// Query path info for a store path.
///
/// Only returns paths with `manifests.status = 'complete'`. Placeholders
/// (status = 'uploading') and orphans (narinfo row but no manifests row)
/// are invisible.
///
/// Filters on `n.store_path_hash` (the PK), not `n.store_path`. I-078:
/// the previous `WHERE n.store_path = $1` had no covering index — every
/// QPI was a Seq Scan over narinfo. Under autoscaled-builder fan-out
/// (60 builders × ~100 input paths each), every PG connection sat
/// seq-scanning, surfacing as `sqlx::pool::acquire 16s`. Computing the
/// hash client-side is one SHA-256 over ~80 bytes; the PK lookup is
/// then a single index probe. `idx_narinfo_store_path` (migration 011)
/// is defense-in-depth for any other text-filter callers.
#[instrument(skip(pool))]
pub async fn query_path_info(pool: &PgPool, store_path: &str) -> Result<Option<ValidatedPathInfo>> {
    let hash = path_hash(store_path)?;
    let row: Option<NarinfoRow> =
        sqlx::query_as(narinfo_complete_select!("n.store_path_hash = $1"))
            .bind(hash.as_slice())
            .fetch_optional(pool)
            .await?;
    validate_row(row)
}

/// Batch `query_path_info`: one PG round-trip for N paths.
///
/// I-110: the builder's `compute_input_closure` BFS was calling
/// `query_path_info` once per path (~800/build). Under autoscaled
/// fan-out (246 builders × ~800 paths) every store replica's PG pool
/// saturated (`acquired_after_secs=11`). One `= ANY(hashes)` query per
/// BFS layer collapses ~800 RPCs to ~10.
///
/// Returns one `(path, Option<info>)` per input, in INPUT ORDER. `None`
/// = no complete manifest (same semantics as `query_path_info`
/// returning `None`). Filters on `store_path_hash = ANY(...)` (PK
/// probe) — same I-078 reasoning as the single-path query.
#[instrument(skip(pool, store_paths), fields(count = store_paths.len()))]
pub async fn query_path_info_batch(
    pool: &PgPool,
    store_paths: &[String],
) -> Result<Vec<(String, Option<ValidatedPathInfo>)>> {
    if store_paths.is_empty() {
        return Ok(Vec::new());
    }
    let hashes = batch_hashes(store_paths)?;

    let rows: Vec<NarinfoRow> =
        sqlx::query_as(narinfo_complete_select!("n.store_path_hash = ANY($1)"))
            .bind(&hashes)
            .fetch_all(pool)
            .await?;

    // Index by store_path then re-project in input order. validate_row
    // applies the same DB-egress check as the single-path query —
    // a malformed row fails the whole batch (corruption indicator,
    // not a per-path miss).
    let mut by_path: std::collections::HashMap<String, ValidatedPathInfo> =
        std::collections::HashMap::with_capacity(rows.len());
    for row in rows {
        let info = row.try_into_validated()?;
        by_path.insert(info.store_path.to_string(), info);
    }

    Ok(store_paths
        .iter()
        .map(|p| (p.clone(), by_path.remove(p)))
        .collect())
}

/// Batch [`get_manifest`] + [`query_path_info`]: one call → (PathInfo,
/// ManifestKind) for N paths.
///
/// I-110c: the builder's FUSE-warm stat loop drives one `GetPath` per
/// input path; each GetPath does TWO PG lookups (narinfo + manifest).
/// One `BatchGetManifest` before the loop collapses ~1600 PG hits to
/// ONE (`= ANY(hashes)` on narinfo+manifests `LEFT JOIN
/// manifest_data`).
///
/// Returns `(path, Option<(info, manifest)>)` per input, in INPUT
/// ORDER. `None` = no complete manifest. Same `status='complete'`
/// filter and PK-probe shape (I-078) as the single-path queries.
pub type ManifestBatchEntry = (String, Option<(ValidatedPathInfo, ManifestKind)>);

#[instrument(skip(pool, store_paths), fields(count = store_paths.len()))]
pub async fn get_manifest_batch(
    pool: &PgPool,
    store_paths: &[String],
) -> Result<Vec<ManifestBatchEntry>> {
    if store_paths.is_empty() {
        return Ok(Vec::new());
    }
    let hashes = batch_hashes(store_paths)?;

    // Single query: narinfo + manifests.inline_blob + manifest_data.
    // chunk_list via LEFT JOIN — one statement = one MVCC snapshot. A
    // separate manifest_data query (the previous shape) raced with GC
    // sweep: one chunked path GC'd between the two queries failed the
    // ENTIRE batch with InvariantViolation. See `get_manifest` for the
    // full READ COMMITTED reasoning.
    #[derive(sqlx::FromRow)]
    struct Row {
        #[sqlx(flatten)]
        narinfo: NarinfoRow,
        inline_blob: Option<Vec<u8>>,
        chunk_list: Option<Vec<u8>>,
    }
    let rows: Vec<Row> = sqlx::query_as(narinfo_complete_select!(
        "n.store_path_hash = ANY($1)",
        ", m.inline_blob, md.chunk_list",
        // ON, not USING: `n` and `m` both expose store_path_hash on
        // the left side after the INNER JOIN ON, so USING is ambiguous
        // (PG 42702 "appears more than once in left table").
        "LEFT JOIN manifest_data md ON md.store_path_hash = m.store_path_hash"
    ))
    .bind(&hashes)
    .fetch_all(pool)
    .await?;

    // Index by store_path. Same 4-arm match as `get_manifest`.
    let mut by_path: std::collections::HashMap<String, (ValidatedPathInfo, ManifestKind)> =
        std::collections::HashMap::with_capacity(rows.len());
    for row in rows {
        let info = row.narinfo.try_into_validated()?;
        let path = info.store_path.to_string();
        let kind = match (row.inline_blob, row.chunk_list) {
            (Some(blob), _) => ManifestKind::Inline(Bytes::from(blob)),
            (None, Some(chunk_list)) => {
                ManifestKind::Chunked(decode_chunk_list(&path, &chunk_list)?)
            }
            // Single-snapshot read; only reachable via genuine
            // corruption — same arm as `get_manifest`. Fail the batch.
            (None, None) => {
                return Err(MetadataError::InvariantViolation(format!(
                    "manifest for {path} has NULL inline_blob but no manifest_data row"
                )));
            }
        };
        by_path.insert(path, (info, kind));
    }

    Ok(store_paths
        .iter()
        .map(|p| (p.clone(), by_path.remove(p)))
        .collect())
}

/// Batch check which store paths are missing.
///
/// "Missing" means no manifests row with `status = 'complete'`. Paths stuck
/// in 'uploading' (crashed PutPath) are missing — the client should retry.
///
/// Filters on `store_path_hash = ANY(...)` (PK) rather than `store_path =
/// ANY(...)` for the same reason as `query_path_info` (I-078). With 1k
/// paths, `= ANY(hashes)` is 1k PK probes; `= ANY(texts)` was 1k seq scans.
#[instrument(skip(pool, store_paths), fields(count = store_paths.len()))]
pub async fn find_missing_paths(pool: &PgPool, store_paths: &[String]) -> Result<Vec<String>> {
    if store_paths.is_empty() {
        return Ok(Vec::new());
    }
    let hashes = batch_hashes(store_paths)?;

    let complete = sqlx::query!(
        r#"
        SELECT n.store_path
        FROM narinfo n
        INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash
        WHERE n.store_path_hash = ANY($1) AND m.status = 'complete'
        "#,
        &hashes,
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
///
/// Currently no production caller (the substituter's dedup check was
/// subsumed by `claim_placeholder`'s `AlreadyComplete` arm); kept for the
/// planned binary-cache HTTP server.
#[allow(dead_code)]
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

    let row: Option<NarinfoRow> = sqlx::query_as(narinfo_complete_select!("n.store_path LIKE $1"))
        .bind(&pattern)
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
    //
    // Explicit tx so the over-cap arm can ROLL BACK the UPDATE. The
    // previous `.fetch_optional(pool)` auto-committed before the cap
    // check — every over-cap call still grew the array by up to 100
    // novel sigs, so an untrusted caller could bloat the row
    // indefinitely (`r[store.api.add-signatures]` says "bounded by
    // MAX_SIGNATURES" — a hard bound, not an alert threshold).
    // r[impl store.api.add-signatures]
    let mut tx = pool.begin().await?;
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
    .fetch_optional(&mut *tx)
    .await?;

    match row {
        None => {
            tx.commit().await?;
            Ok(0) // not found — caller maps to NOT_FOUND
        }
        Some(r) if r.n as usize > rio_common::limits::MAX_SIGNATURES => {
            // Over cap post-dedup → client sent novel sigs and we grew
            // past the limit. ROLL BACK the UPDATE so the stored array
            // never exceeds the cap; reject so the client backs off.
            tx.rollback().await?;
            Err(MetadataError::ResourceExhausted(format!(
                "signature count {} exceeds MAX_SIGNATURES ({})",
                r.n,
                rio_common::limits::MAX_SIGNATURES
            )))
        }
        Some(_) => {
            tx.commit().await?;
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::StoreSeed;
    use rio_test_support::TestDb;
    use rio_test_support::fixtures::test_store_path;
    use sqlx::PgPool;

    /// Seed a complete path (narinfo + manifests status='complete').
    /// Returns (store_path_hash, nar_hash). `nar_hash` is a fixed
    /// `[0xAA, 0, ..]` so callers can assert on `path_by_nar_hash`.
    async fn seed_complete(
        pool: &PgPool,
        path: &str,
        inline_blob: Option<&[u8]>,
    ) -> (Vec<u8>, [u8; 32]) {
        let mut nar_hash = [0u8; 32];
        nar_hash[0] = 0xAA;
        let mut seed = StoreSeed::raw_path(path)
            .with_nar_hash(nar_hash)
            .with_nar_size(100);
        if let Some(b) = inline_blob {
            seed = seed.with_inline_blob(b);
        }
        (seed.seed(pool).await, nar_hash)
    }

    /// Empty input → empty output, no DB round-trip.
    #[tokio::test]
    async fn find_missing_paths_empty_input() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let result = find_missing_paths(&db.pool, &[]).await.unwrap();
        assert!(result.is_empty());
    }

    /// I-110: batch QPI returns one entry per input, in input order,
    /// with `None` for paths that don't exist or aren't `complete`.
    #[tokio::test]
    async fn query_path_info_batch_order_and_misses() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let p_a = test_store_path("batch-a");
        let p_b = test_store_path("batch-b");
        let p_missing = test_store_path("batch-missing");
        let p_uploading = test_store_path("batch-uploading");

        seed_complete(&db.pool, &p_a, Some(b"a")).await;
        seed_complete(&db.pool, &p_b, Some(b"b")).await;
        // status='uploading' → invisible (same filter as single-path QPI).
        StoreSeed::raw_path(&p_uploading)
            .with_manifest_status("uploading")
            .seed(&db.pool)
            .await;

        // Input order: [b, missing, a, uploading]. Output must match.
        let req = vec![
            p_b.clone(),
            p_missing.clone(),
            p_a.clone(),
            p_uploading.clone(),
        ];
        let got = query_path_info_batch(&db.pool, &req).await.unwrap();

        assert_eq!(got.len(), 4);
        assert_eq!(got[0].0, p_b);
        assert_eq!(
            got[0].1.as_ref().map(|i| i.store_path.as_str()),
            Some(p_b.as_str())
        );
        assert_eq!(got[1].0, p_missing);
        assert!(got[1].1.is_none(), "missing path → None");
        assert_eq!(got[2].0, p_a);
        assert_eq!(
            got[2].1.as_ref().map(|i| i.store_path.as_str()),
            Some(p_a.as_str())
        );
        assert_eq!(got[3].0, p_uploading);
        assert!(got[3].1.is_none(), "status=uploading → invisible (None)");

        // Empty input → empty output, no DB round-trip.
        assert!(
            query_path_info_batch(&db.pool, &[])
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// `path_by_nar_hash` — caller is `substitute.rs` (dedup check
    /// before ingesting an upstream NAR).
    #[tokio::test]
    async fn path_by_nar_hash_found_and_not_found() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = test_store_path("found-by-hash");
        let (_ph, nar_hash) = seed_complete(&db.pool, &path, Some(b"blob")).await;

        // Found: nar_hash matches.
        let found = path_by_nar_hash(&db.pool, &nar_hash).await.unwrap();
        assert_eq!(found.as_deref(), Some(path.as_str()));

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
        let path = test_store_path("invariant-viol");
        // inline_blob=NULL but we DON'T insert manifest_data.
        seed_complete(&db.pool, &path, None).await;

        let err = get_manifest(&db.pool, &path).await.unwrap_err();
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
        let path = test_store_path("corrupt");
        let (ph, _) = seed_complete(&db.pool, &path, None).await;

        // Insert garbage into manifest_data — Manifest::deserialize
        // will reject (unknown version or bad length).
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&ph)
            .bind(b"garbage bytes not a manifest".as_slice())
            .execute(&db.pool)
            .await
            .unwrap();

        let err = get_manifest(&db.pool, &path).await.unwrap_err();
        assert!(
            matches!(&err, MetadataError::CorruptManifest { store_path, .. } if *store_path == path),
            "expected CorruptManifest, got {err:?}"
        );
    }

    /// I-110c: batch manifest returns one entry per input, in input
    /// order, covering inline + chunked + missing + uploading. Mirrors
    /// `query_path_info_batch_order_and_misses` for the manifest side.
    #[tokio::test]
    async fn get_manifest_batch_order_and_kinds() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let p_inline = test_store_path("mb-inline");
        let p_chunked = test_store_path("mb-chunked");
        let p_missing = test_store_path("mb-missing");
        let p_uploading = test_store_path("mb-uploading");

        seed_complete(&db.pool, &p_inline, Some(b"inline blob")).await;
        let (ph, _) = seed_complete(&db.pool, &p_chunked, None).await;
        let manifest = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: [0x22; 32],
                size: 8192,
            }],
        }
        .serialize();
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&ph)
            .bind(&manifest)
            .execute(&db.pool)
            .await
            .unwrap();
        StoreSeed::raw_path(&p_uploading)
            .with_manifest_status("uploading")
            .seed(&db.pool)
            .await;

        // Input order: [chunked, missing, inline, uploading].
        let req = vec![
            p_chunked.clone(),
            p_missing.clone(),
            p_inline.clone(),
            p_uploading.clone(),
        ];
        let got = get_manifest_batch(&db.pool, &req).await.unwrap();

        assert_eq!(got.len(), 4);
        // chunked
        assert_eq!(got[0].0, p_chunked);
        let (info, kind) = got[0].1.as_ref().expect("chunked present");
        assert_eq!(info.store_path.as_str(), p_chunked);
        match kind {
            ManifestKind::Chunked(e) => {
                assert_eq!(e.len(), 1);
                assert_eq!(e[0], ([0x22; 32], 8192));
            }
            _ => panic!("expected Chunked"),
        }
        // missing
        assert_eq!(got[1].0, p_missing);
        assert!(got[1].1.is_none(), "missing → None");
        // inline
        assert_eq!(got[2].0, p_inline);
        let (info, kind) = got[2].1.as_ref().expect("inline present");
        assert_eq!(info.store_path.as_str(), p_inline);
        assert!(matches!(kind, ManifestKind::Inline(b) if &b[..] == b"inline blob"));
        // uploading → invisible
        assert_eq!(got[3].0, p_uploading);
        assert!(got[3].1.is_none(), "uploading → None");

        // Empty input → empty output.
        assert!(get_manifest_batch(&db.pool, &[]).await.unwrap().is_empty());
    }

    /// I-110c: chunked path with NULL inline_blob but no manifest_data
    /// row → batch surfaces the same InvariantViolation as
    /// `get_manifest_invariant_violation` (corruption indicator, not a
    /// per-path miss).
    #[tokio::test]
    async fn get_manifest_batch_invariant_violation() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = test_store_path("mb-broken");
        seed_complete(&db.pool, &path, None).await;
        let err = get_manifest_batch(&db.pool, &[path]).await.unwrap_err();
        assert!(
            matches!(err, MetadataError::InvariantViolation(_)),
            "expected InvariantViolation, got {err:?}"
        );
    }

    /// `get_manifest` happy paths: inline and chunked both return
    /// the right `ManifestKind` variant.
    #[tokio::test]
    async fn get_manifest_inline_and_chunked() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Inline: inline_blob set.
        let inline_path = test_store_path("inline");
        seed_complete(&db.pool, &inline_path, Some(b"inline content")).await;
        let kind = get_manifest(&db.pool, &inline_path).await.unwrap().unwrap();
        assert!(
            matches!(kind, ManifestKind::Inline(b) if &b[..] == b"inline content"),
            "expected Inline variant"
        );

        // Chunked: inline_blob=NULL, valid manifest_data.
        let chunked_path = test_store_path("chunked");
        let (ph, _) = seed_complete(&db.pool, &chunked_path, None).await;
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
        let kind = get_manifest(&db.pool, &chunked_path)
            .await
            .unwrap()
            .unwrap();
        match kind {
            ManifestKind::Chunked(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0, [0x11; 32]);
                assert_eq!(entries[0].1, 4096);
            }
            _ => panic!("expected Chunked variant"),
        }

        // Not found: well-formed but absent path → None (no row).
        let missing = get_manifest(
            &db.pool,
            "/nix/store/00000000000000000000000000000000-absent",
        )
        .await
        .unwrap();
        assert!(missing.is_none());

        // Malformed path: now an Err (path_hash can't compute the
        // PK). Previously the text-filter query just returned None;
        // the gRPC layer's validate_store_path already rejects these.
        assert!(
            get_manifest(&db.pool, "/nix/store/nonexistent")
                .await
                .is_err()
        );
    }

    /// T6: append_signatures dedups. Same sig twice → array length 1.
    #[tokio::test]
    async fn append_signatures_dedups() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = test_store_path("sigdedup");
        seed_complete(&db.pool, &path, Some(b"x")).await;

        // Append the same sig twice, in separate calls.
        let r1 = append_signatures(&db.pool, &path, &["sig:abc".into()])
            .await
            .unwrap();
        assert_eq!(r1, 1, "first append: 1 row updated");
        let r2 = append_signatures(&db.pool, &path, &["sig:abc".into()])
            .await
            .unwrap();
        assert_eq!(r2, 1, "second append: still 1 row updated (idempotent)");

        // Verify dedup: array length is 1, not 2.
        let (sigs,): (Vec<String>,) =
            sqlx::query_as("SELECT signatures FROM narinfo WHERE store_path = $1")
                .bind(&path)
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
        let r = append_signatures(&db.pool, &test_store_path("nope"), &["sig:x".into()])
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
        let path = test_store_path("sigcap");
        seed_complete(&db.pool, &path, Some(b"x")).await;

        // Seed MAX_SIGNATURES distinct sigs first (at cap, no error).
        let at_cap: Vec<String> = (0..rio_common::limits::MAX_SIGNATURES)
            .map(|i| format!("sig:seed{i}"))
            .collect();
        let r = append_signatures(&db.pool, &path, &at_cap).await.unwrap();
        assert_eq!(r, 1, "at cap is OK");

        // One MORE novel sig → post-dedup count = MAX+1 → reject.
        let err = append_signatures(&db.pool, &path, &["sig:overflow".into()])
            .await
            .unwrap_err();
        assert!(
            matches!(err, MetadataError::ResourceExhausted(_)),
            "expected ResourceExhausted, got {err:?}"
        );

        // r[verify store.api.add-signatures]
        // Reject MUST roll back: stored cardinality stays at MAX, not
        // MAX+1. Previously the UPDATE auto-committed before the cap
        // check (this assertion was the fails-before/passes-after).
        let n: i32 =
            sqlx::query_scalar("SELECT cardinality(signatures) FROM narinfo WHERE store_path = $1")
                .bind(&path)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            n as usize,
            rio_common::limits::MAX_SIGNATURES,
            "over-cap reject must NOT persist the over-cap array"
        );

        // Second over-cap call with 50 more novel sigs: still rejected,
        // cardinality still pinned at MAX.
        let more: Vec<String> = (0..50).map(|i| format!("sig:over{i}")).collect();
        let err2 = append_signatures(&db.pool, &path, &more).await.unwrap_err();
        assert!(matches!(err2, MetadataError::ResourceExhausted(_)));
        let n2: i32 =
            sqlx::query_scalar("SELECT cardinality(signatures) FROM narinfo WHERE store_path = $1")
                .bind(&path)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            n2 as usize,
            rio_common::limits::MAX_SIGNATURES,
            "repeated over-cap calls must NOT grow the array"
        );
    }

    // r[verify store.api.batch-manifest+2]
    /// `get_manifest` racing GC sweep's CASCADE delete: every result
    /// is `Ok(Some(..))` or `Ok(None)`; never `InvariantViolation`.
    /// Previously: two separate `.fetch_optional(pool)` calls under
    /// READ COMMITTED could observe the manifests row but not the
    /// (just-deleted) manifest_data row → spurious InvariantViolation.
    /// After the LEFT JOIN rewrite this is structurally impossible
    /// (one statement = one snapshot), so this test is deterministic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_manifest_concurrent_gc_never_invariant_violation() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = test_store_path("gc-race");
        let (ph, _) = seed_complete(&db.pool, &path, None).await;
        let manifest = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: [0x33; 32],
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

        let pool_r = db.pool.clone();
        let path_r = path.clone();
        let reader = tokio::spawn(async move {
            let mut results = Vec::with_capacity(200);
            for _ in 0..200 {
                results.push(get_manifest(&pool_r, &path_r).await);
            }
            results
        });

        let pool_d = db.pool.clone();
        let deleter = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            // Sweep's effect: DELETE narinfo CASCADE → manifests +
            // manifest_data gone atomically.
            sqlx::query("DELETE FROM narinfo WHERE store_path_hash = $1")
                .bind(&ph)
                .execute(&pool_d)
                .await
                .unwrap();
        });

        let (results, _) = tokio::join!(reader, deleter);
        for r in results.unwrap() {
            match r {
                Ok(Some(ManifestKind::Chunked(_))) | Ok(None) => {}
                other => panic!(
                    "get_manifest under concurrent GC must be Ok(Some(Chunked)) \
                     or Ok(None); got {other:?}"
                ),
            }
        }
    }

    /// Batch variant of the GC-race test: one stable inline path + one
    /// chunked path being deleted. Batch never errors; the inline path
    /// always resolves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_manifest_batch_concurrent_gc_never_invariant_violation() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let p_inline = test_store_path("gcb-inline");
        let p_chunked = test_store_path("gcb-chunked");
        seed_complete(&db.pool, &p_inline, Some(b"stable")).await;
        let (ph, _) = seed_complete(&db.pool, &p_chunked, None).await;
        let manifest = crate::manifest::Manifest {
            entries: vec![crate::manifest::ManifestEntry {
                hash: [0x44; 32],
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

        let pool_r = db.pool.clone();
        let req = vec![p_inline.clone(), p_chunked.clone()];
        let reader = tokio::spawn(async move {
            let mut results = Vec::with_capacity(200);
            for _ in 0..200 {
                results.push(get_manifest_batch(&pool_r, &req).await);
            }
            results
        });

        let pool_d = db.pool.clone();
        let deleter = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            sqlx::query("DELETE FROM narinfo WHERE store_path_hash = $1")
                .bind(&ph)
                .execute(&pool_d)
                .await
                .unwrap();
        });

        let (results, _) = tokio::join!(reader, deleter);
        for r in results.unwrap() {
            let batch = r.expect("batch must never Err under concurrent GC");
            assert_eq!(batch.len(), 2);
            // Inline path always resolves (never deleted).
            assert!(
                matches!(&batch[0].1, Some((_, ManifestKind::Inline(b))) if &b[..] == b"stable"),
                "stable inline path must always resolve"
            );
            // Chunked path: Some(Chunked) before delete, None after.
            assert!(
                matches!(&batch[1].1, Some((_, ManifestKind::Chunked(_))) | None),
                "chunked path under GC: Some(Chunked) or None, got {:?}",
                batch[1].1
            );
        }
    }

    // r[verify store.api.hash-part+2]
    /// Migration 049: `idx_narinfo_store_path_pattern` exists with
    /// opclass `text_pattern_ops`. Under non-C collation (production
    /// default `en_US.UTF-8`), the default-opclass `idx_narinfo_
    /// store_path` cannot serve `LIKE 'prefix%'` → seq-scan. CI's
    /// `--locale=C` initdb masks this, so we assert the INDEX exists
    /// (locale-independent) rather than EXPLAIN.
    #[tokio::test]
    async fn query_by_hash_part_pattern_idx_exists() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let opclass: Option<String> = sqlx::query_scalar(
            "SELECT oc.opcname \
               FROM pg_index x \
               JOIN pg_class i  ON i.oid = x.indexrelid \
               JOIN pg_opclass oc ON oc.oid = x.indclass[0] \
              WHERE i.relname = 'idx_narinfo_store_path_pattern'",
        )
        .fetch_optional(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            opclass.as_deref(),
            Some("text_pattern_ops"),
            "idx_narinfo_store_path_pattern must exist with text_pattern_ops \
             (LIKE 'prefix%' under non-C collation seq-scans without it)"
        );
    }

    /// Dev-only EXPLAIN sanity for `query_by_hash_part`. Only meaningful
    /// against a non-C-locale PG (test/prod divergence is the bug here);
    /// under CI's `--locale=C` initdb the default-opclass index ALSO
    /// serves LIKE-prefix, so this can't gate. Mirrors orphan.rs's
    /// `scan_query_uses_uploading_partial_idx` shape.
    #[ignore = "EXPLAIN plan depends on PG cost model + locale; dev-only sanity"]
    #[tokio::test]
    async fn query_by_hash_part_uses_pattern_idx() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             SELECT sha256(i::text::bytea), \
                    '/nix/store/' || lpad(to_hex(i), 32, '0') || '-seed', \
                    sha256(i::text::bytea), 0 \
             FROM generate_series(1, 10000) AS i",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status) \
             SELECT sha256(i::text::bytea), 'complete' \
             FROM generate_series(1, 10000) AS i",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE narinfo")
            .execute(&db.pool)
            .await
            .unwrap();

        let plan: Vec<(String,)> = sqlx::query_as(concat!(
            "EXPLAIN (FORMAT TEXT) ",
            narinfo_complete_select!("n.store_path LIKE $1")
        ))
        .bind("/nix/store/00000000000000000000000000000001-%")
        .fetch_all(&db.pool)
        .await
        .unwrap();

        let joined: String = plan
            .into_iter()
            .map(|(l,)| l)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("idx_narinfo_store_path_pattern"),
            "EXPLAIN should reference idx_narinfo_store_path_pattern; got:\n{joined}"
        );
        assert!(
            !joined.contains("Seq Scan on narinfo"),
            "EXPLAIN should NOT seq-scan narinfo; got:\n{joined}"
        );
    }
}

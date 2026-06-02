//! Store-side derivation modulo-hash cache (`drv_modulo_cache`, M_068).
//!
//! CppNix parity: `Store::queryPartialDerivationOutputMap` answers
//! "which output paths does this deriver own" from the store's OWN copy
//! of the derivation (`store-api.cc:396-410`, backed by
//! `drvHashes`/`pathDerivationModulo`, `derivations.cc:856-874`) — never
//! from a client's claim about it. This module is rio's persistent form
//! of that table: rows are populated best-effort when a `.drv` is
//! ingested (after the text-CA gate — `store.put.drv-text-ca` — has
//! already bound the bytes to the path) and read-through-completed at
//! proof time by the IA deriver-proof gate.
//!
//! Resolvers in `rio_nix::derivation::hash` are SYNCHRONOUS: every
//! caller pre-seeds the hash cache from already-persisted rows (an
//! owned arena of nothing — cache rows ARE the arena) before invoking
//! the walk; async I/O never runs inside a resolver. At ingestion,
//! inputs that have no cache row yet simply skip population (counted),
//! because out-of-order uploads are normal and the proof-time
//! read-through completes the chain later.
// r[impl store.ingest.drv-modulo-cache]

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{debug, warn};

/// One cached deriver row.
#[derive(Debug, Clone)]
pub(crate) struct DrvModuloRow {
    pub modulo_hash: [u8; 32],
    /// `{output_name: store_path}` for STATIC input-addressed derivers;
    /// empty for fixed-output and unknown-path (deferred) derivers.
    pub ia_output_paths: HashMap<String, String>,
    /// The deriver's own output paths are not statically derivable
    /// (floating-CA self or deferred-IA): realisations carry the truth.
    pub deferred: bool,
}

/// sha256 over the FULL `.drv` store path string — the table's key
/// (narinfo keying convention).
pub(crate) fn drv_path_hash(drv_path: &str) -> Vec<u8> {
    Sha256::digest(drv_path.as_bytes()).to_vec()
}

/// Computed (not yet persisted) row + the inputs it consumed.
pub(crate) struct ComputedModulo {
    pub row: DrvModuloRow,
}

/// Why a best-effort population was skipped.
#[derive(Debug)]
pub(crate) enum SkipReason {
    /// The bytes do not parse as a derivation. (A text-CA-valid `.drv`
    /// path can hold garbage — the upload gate binds bytes to the path,
    /// it does not parse them.)
    ParseFailed,
    /// An `inputDrvs` entry has no cache row yet (out-of-order upload)
    /// or the modulo walk could not complete over the seeded cache
    /// (cyclic or otherwise ill-formed input metadata fails closed).
    MissingInput,
}

/// Compute the modulo row for `.drv` bytes, resolving inputs ONLY from
/// the pre-seeded `input_hashes` cache (path → modulo hash). Pure and
/// synchronous; the caller owns all I/O.
pub(crate) fn compute_drv_modulo(
    bytes: &[u8],
    drv_path: &str,
    input_hashes: &HashMap<String, [u8; 32]>,
) -> Result<ComputedModulo, SkipReason> {
    use rio_nix::derivation::{Derivation, DerivationLike, input_addressed_output_paths};

    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(SkipReason::ParseFailed);
    };
    let Ok(drv) = Derivation::parse(text) else {
        return Err(SkipReason::ParseFailed);
    };

    // Cache-only resolution: a missing input fails the walk (the
    // resolver returns None), mapped to MissingInput below.
    //
    // INPUT form, not the published form: every consumer of this row's
    // hash seeds it into ANOTHER derivation's modulo walk as an
    // input-position digest (`populate_on_ingest` seeds, the
    // read-through's bottom-up pass). For floating-CA subjects the
    // published (masked) form diverges from the input form and would
    // poison every downstream IA derivation — the masked-form
    // false-result class. The masked/published hash is a realisation
    // key; nothing in this cache needs it.
    let resolve_none = |_: &str| -> Option<&Derivation> { None };
    let mut cache: HashMap<String, [u8; 32]> = input_hashes.clone();
    let modulo_hash = rio_nix::derivation::hash_derivation_modulo_input_form(
        &drv,
        drv_path,
        &resolve_none,
        &mut cache,
    )
    .map_err(|_| SkipReason::MissingInput)?;

    let unknown = drv.has_unknown_output_paths();
    let is_ca = drv.is_content_addressed();
    let deferred = unknown;
    let ia_output_paths = if !unknown && !is_ca {
        // Static input-addressed deriver: derive the per-output paths
        // the same way the trusted plane does. The walk above already
        // proved every input hash is seeded.
        let mut cache2: HashMap<String, [u8; 32]> = input_hashes.clone();
        match input_addressed_output_paths(&drv, drv_path, &resolve_none, &mut cache2) {
            Ok(map) => map
                .into_iter()
                .map(|(name, sp)| (name, sp.as_str().to_string()))
                .collect(),
            Err(_) => return Err(SkipReason::MissingInput),
        }
    } else {
        HashMap::new()
    };

    Ok(ComputedModulo {
        row: DrvModuloRow {
            modulo_hash,
            ia_output_paths,
            deferred,
        },
    })
}

/// Load cache rows for a batch of input `.drv` paths. Returns
/// `path → modulo hash` for every row found; absent paths are simply
/// missing from the map.
pub(crate) async fn load_drv_modulo_batch(
    pool: &PgPool,
    drv_paths: &[String],
) -> Result<HashMap<String, [u8; 32]>, sqlx::Error> {
    if drv_paths.is_empty() {
        return Ok(HashMap::new());
    }
    let hashes: Vec<Vec<u8>> = drv_paths.iter().map(|p| drv_path_hash(p)).collect();
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT drv_path, modulo_hash FROM drv_modulo_cache \
         WHERE drv_path_hash = ANY($1)",
    )
    .bind(&hashes)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(p, h)| <[u8; 32]>::try_from(h.as_slice()).ok().map(|h| (p, h)))
        .collect())
}

/// Load one full cached row.
pub(crate) async fn load_drv_modulo(
    pool: &PgPool,
    drv_path: &str,
) -> Result<Option<DrvModuloRow>, sqlx::Error> {
    let row: Option<(Vec<u8>, sqlx::types::JsonValue, bool)> = sqlx::query_as(
        "SELECT modulo_hash, ia_output_paths, deferred FROM drv_modulo_cache \
         WHERE drv_path_hash = $1",
    )
    .bind(drv_path_hash(drv_path))
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(h, paths, deferred)| {
        let modulo_hash = <[u8; 32]>::try_from(h.as_slice()).ok()?;
        let ia_output_paths = paths
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Some(DrvModuloRow {
            modulo_hash,
            ia_output_paths,
            deferred,
        })
    }))
}

/// Idempotent upsert. Rows are content-derived immutable facts about
/// text-CA-bound bytes, so `DO NOTHING` on conflict is exact (a
/// re-upload of the same path carries identical bytes by construction).
pub(crate) async fn upsert_drv_modulo(
    pool: &PgPool,
    drv_path: &str,
    row: &DrvModuloRow,
) -> Result<(), sqlx::Error> {
    let paths_json = sqlx::types::JsonValue::Object(
        row.ia_output_paths
            .iter()
            .map(|(k, v)| (k.clone(), sqlx::types::JsonValue::String(v.clone())))
            .collect(),
    );
    sqlx::query(
        "INSERT INTO drv_modulo_cache \
         (drv_path_hash, drv_path, modulo_hash, ia_output_paths, deferred) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (drv_path_hash) DO NOTHING",
    )
    .bind(drv_path_hash(drv_path))
    .bind(drv_path)
    .bind(row.modulo_hash.as_slice())
    .bind(&paths_json)
    .bind(row.deferred)
    .execute(pool)
    .await?;
    Ok(())
}

/// Best-effort ingestion hook: parse the just-persisted `.drv` bytes,
/// seed input hashes from existing cache rows, compute, upsert. NEVER
/// fails the upload — every outcome is a counter.
pub(crate) async fn populate_on_ingest(pool: &PgPool, drv_path: &str, drv_bytes: &[u8]) {
    // Cheap pre-parse for the input list (the compute parses again;
    // ~KBs, negligible vs the upload itself).
    let inputs: Vec<String> = match std::str::from_utf8(drv_bytes)
        .ok()
        .and_then(|t| rio_nix::derivation::Derivation::parse(t).ok())
    {
        Some(drv) => drv.input_drvs().keys().cloned().collect(),
        None => {
            metrics::counter!(
                "rio_store_drv_modulo_cache_total",
                "event" => "parse_failed"
            )
            .increment(1);
            debug!(
                drv_path,
                "drv modulo population skipped: bytes do not parse"
            );
            return;
        }
    };
    let seeds = match load_drv_modulo_batch(pool, &inputs).await {
        Ok(s) => s,
        Err(e) => {
            // A DB failure is not "the inputs are absent" — labeling it
            // `skipped_missing_input` told operators reading the metric
            // that uploads were merely out of order when the database
            // was failing (merged_bug_015 sibling site, pattern R1).
            metrics::counter!(
                "rio_store_drv_modulo_cache_total",
                "event" => "seed_load_failed"
            )
            .increment(1);
            warn!(drv_path, error = %e, "drv modulo population skipped: seed load failed");
            return;
        }
    };
    match compute_drv_modulo(drv_bytes, drv_path, &seeds) {
        Ok(computed) => {
            if let Err(e) = upsert_drv_modulo(pool, drv_path, &computed.row).await {
                warn!(drv_path, error = %e, "drv modulo upsert failed (best-effort)");
                return;
            }
            metrics::counter!(
                "rio_store_drv_modulo_cache_total",
                "event" => "populated"
            )
            .increment(1);
        }
        Err(SkipReason::ParseFailed) => {
            metrics::counter!(
                "rio_store_drv_modulo_cache_total",
                "event" => "parse_failed"
            )
            .increment(1);
        }
        Err(SkipReason::MissingInput) => {
            metrics::counter!(
                "rio_store_drv_modulo_cache_total",
                "event" => "skipped_missing_input"
            )
            .increment(1);
            debug!(
                drv_path,
                "drv modulo population skipped: input rows absent (out-of-order upload; \
                 proof-time read-through completes the chain)"
            );
        }
    }
}

/// Read-through bounds for proof-time computation
/// (`store.put.ia-deriver-proof+2`): a deriver closure needing more than
/// this many input `.drv` fetches, or deeper than this, is declared
/// unverifiable (fail-closed) rather than letting one upload walk an
/// unbounded graph on the hot path. Counter-instrumented consts, not
/// config.
pub(crate) const READ_THROUGH_MAX_FETCHES: usize = 64;
pub(crate) const READ_THROUGH_MAX_DEPTH: usize = 32;

/// Fetch a `.drv`'s raw text bytes from the store's OWN backend.
/// `.drv`s are KB-sized and therefore inline; a chunked or absent
/// manifest returns `Ok(None)` (not resident in usable form).
///
/// Error classes are PROPAGATED, never folded into the absent verdict
/// (merged_bug_015 — pattern R1: the old `.ok()??` collapsed DB
/// failures and row corruption into "unverifiable", which the proof
/// gate then served as `PERMISSION_DENIED`, telling an honest worker
/// its deriver doesn't derive its path because the store's database
/// hiccuped):
/// - `get_manifest` errors (connection, serialization, corrupt chunk
///   list) → `Err` — the caller maps to `INTERNAL`/retriable, never an
///   authorization verdict.
/// - a `status='complete'` `.drv` whose inline NAR does not parse as a
///   single regular file → `Err(InvariantViolation)`: the text-CA gate
///   (`store.put.drv-text-ca`) proved single-file shape at ingestion,
///   so failure here is row-level corruption, not absence.
async fn own_drv_bytes(
    pool: &PgPool,
    drv_path: &str,
) -> Result<Option<Vec<u8>>, super::MetadataError> {
    match super::get_manifest(pool, drv_path).await? {
        None => Ok(None),
        Some(super::ManifestKind::Inline(nar)) => rio_nix::nar::extract_single_file(&nar)
            .map(Some)
            .map_err(|e| {
                super::MetadataError::InvariantViolation(format!(
                    "complete .drv manifest for {drv_path} holds a NAR that is not a \
                     single regular file (text-CA-gated at ingestion; this is row \
                     corruption): {e}"
                ))
            }),
        Some(super::ManifestKind::Chunked(_)) => Ok(None),
    }
}

/// Proof-time read-through: return the deriver's cached row, computing
/// it (and its missing ancestors) from the store's own backend when
/// absent — bounded by [`READ_THROUGH_MAX_FETCHES`] /
/// [`READ_THROUGH_MAX_DEPTH`]. Every computed row is persisted (cache
/// warm). `None` = unverifiable within bounds.
// r[impl store.put.ia-deriver-proof+2]
pub(crate) async fn load_or_compute_drv_modulo(
    pool: &PgPool,
    drv_path: &str,
) -> Result<Option<DrvModuloRow>, super::MetadataError> {
    if let Some(row) = load_drv_modulo(pool, drv_path).await? {
        return Ok(Some(row));
    }
    // Phase 1 (I/O): pre-fetch the missing closure into an owned arena
    // — cache rows first, own backend for misses. The hash walk below
    // is synchronous over this arena; no I/O runs inside a resolver.
    let mut arena: HashMap<String, Vec<u8>> = HashMap::new();
    let mut seeds: HashMap<String, [u8; 32]> = HashMap::new();
    let mut frontier = vec![(drv_path.to_string(), 0usize)];
    let mut fetches = 0usize;
    while let Some((path, depth)) = frontier.pop() {
        if arena.contains_key(&path) || seeds.contains_key(&path) {
            continue;
        }
        if depth > 0
            && let Some(h) = load_drv_modulo_batch(pool, std::slice::from_ref(&path))
                .await?
                .remove(&path)
        {
            seeds.insert(path, h);
            continue;
        }
        if fetches >= READ_THROUGH_MAX_FETCHES || depth >= READ_THROUGH_MAX_DEPTH {
            metrics::counter!("rio_store_ia_proof_total", "result" => "unverifiable").increment(1);
            return Ok(None);
        }
        fetches += 1;
        let Some(bytes) = own_drv_bytes(pool, &path).await? else {
            metrics::counter!("rio_store_ia_proof_total", "result" => "unverifiable").increment(1);
            return Ok(None);
        };
        let inputs: Vec<String> = match std::str::from_utf8(&bytes)
            .ok()
            .and_then(|t| rio_nix::derivation::Derivation::parse(t).ok())
        {
            Some(d) => d.input_drvs().keys().cloned().collect(),
            None => {
                metrics::counter!("rio_store_ia_proof_total", "result" => "unverifiable")
                    .increment(1);
                return Ok(None);
            }
        };
        for i in inputs {
            frontier.push((i, depth + 1));
        }
        arena.insert(path, bytes);
    }
    // Phase 2 (pure): bottom-up over the arena until the target's row
    // exists. Each pass computes every node whose inputs are all
    // seeded; a pass with no progress means a cycle — fail closed.
    let mut remaining: Vec<String> = arena.keys().cloned().collect();
    while !remaining.is_empty() {
        let mut next = Vec::new();
        let mut progressed = false;
        for path in remaining {
            let bytes = &arena[&path];
            match compute_drv_modulo(bytes, &path, &seeds) {
                Ok(c) => {
                    seeds.insert(path.clone(), c.row.modulo_hash);
                    upsert_drv_modulo(pool, &path, &c.row).await?;
                    progressed = true;
                    if path == drv_path {
                        metrics::counter!(
                            "rio_store_ia_proof_total",
                            "result" => "computed_on_miss"
                        )
                        .increment(1);
                    }
                }
                Err(_) => next.push(path),
            }
        }
        if !progressed {
            metrics::counter!("rio_store_ia_proof_total", "result" => "unverifiable").increment(1);
            return Ok(None);
        }
        remaining = next;
    }
    Ok(load_drv_modulo(pool, drv_path).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE masked-form regression (vm-ca-cutoff class, store edition):
    /// a floating-CA derivation consumed as an INPUT must contribute
    /// its `mask_outputs=false` digest to the parent's walk — the
    /// cache row's `modulo_hash` IS that digest, never the published
    /// (masked) realisation key. Composes `compute_drv_modulo` exactly
    /// the way `populate_on_ingest` / the read-through do and compares
    /// against a full-resolution reference walk; before the
    /// input-form fix the cached composition diverged for floating
    /// inputs and silently mis-derived every downstream IA path.
    // r[verify store.put.ia-deriver-proof+2]
    #[test]
    fn cached_floating_input_matches_full_resolution_walk() {
        use rio_nix::derivation::{Derivation, input_addressed_output_paths};
        use std::collections::HashMap;

        let floating_path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-floaty.drv";
        // The env["out"] placeholder is what masking clears for a
        // floating subject (oracle maskOutputs masks output-name env
        // entries too) — it is what makes the published (masked) and
        // input (unmasked) forms DIVERGE; every real floating drv
        // carries it.
        let floating = r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo f > $out"],[("name","floaty"),("out","/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9")])"#;
        let parent_path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-parent.drv";
        let parent = format!(
            r#"Derive([("out","/nix/store/cccccccccccccccccccccccccccccccc-parent","","")],[("{floating_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","cp in $out"],[("name","parent")])"#
        );

        // Reference: full-resolution walk with the floating drv
        // resolvable in memory.
        let floating_drv = Derivation::parse(floating).expect("floating parses");
        let parent_drv = Derivation::parse(&parent).expect("parent parses");
        let resolve =
            |p: &str| -> Option<&Derivation> { (p == floating_path).then_some(&floating_drv) };
        let mut ref_cache: HashMap<String, [u8; 32]> = HashMap::new();
        let reference =
            input_addressed_output_paths(&parent_drv, parent_path, &resolve, &mut ref_cache)
                .expect("reference walk derives");

        // Cached composition: the floating row's stored hash seeds the
        // parent's compute, exactly as the read-through does.
        let row_f = compute_drv_modulo(floating.as_bytes(), floating_path, &HashMap::new())
            .expect("floating row computes");
        let seeds: HashMap<String, [u8; 32]> =
            [(floating_path.to_string(), row_f.row.modulo_hash)].into();
        let row_p = compute_drv_modulo(parent.as_bytes(), parent_path, &seeds)
            .expect("parent computes from cached seed");

        for (name, sp) in &reference {
            assert_eq!(
                row_p.row.ia_output_paths.get(name).map(String::as_str),
                Some(sp.as_str()),
                "cached-seed derivation diverged from the full-resolution walk \
                 for output {name} — the floating input's cached hash is not \
                 its input-position form"
            );
        }
        assert!(
            !reference.is_empty(),
            "fixture precondition: parent derives outputs"
        );
    }
}

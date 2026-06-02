//! Store-side derivation modulo-hash cache (`drv_modulo_cache`, M_068).
//!
//! CppNix parity: `Store::queryPartialDerivationOutputMap` answers
//! "which output paths does this deriver own" from the store's OWN copy
//! of the derivation (`store-api.cc:396-410`, backed by
//! `drvHashes`/`pathDerivationModulo`, `derivations.cc:856-874`) — never
//! from a client's claim about it. This module is rio's persistent form
//! of that table: rows are populated best-effort when a `.drv` is
//! ingested (after the text-CA gate — `store.put.drv-text-ca+2` — has
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
// r[impl store.ingest.drv-modulo-cache+2]

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
/// the pre-seeded `seeds` cache (path → input-form modulo hash). Pure
/// and synchronous; the caller owns all I/O.
///
/// Takes `&mut` and memoizes INTO the cache (the walk inserts every
/// input-form hash it derives, including the subject's): at read-through
/// scale the previous per-call clones were O(N²) over a closure of N
/// derivations — the dominant cost the work budget could not see
/// (bug_007 sibling cost; pattern R4 dominance).
pub(crate) fn compute_drv_modulo(
    bytes: &[u8],
    drv_path: &str,
    seeds: &mut HashMap<String, [u8; 32]>,
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
    let modulo_hash = rio_nix::derivation::hash_derivation_modulo_input_form(
        &drv,
        drv_path,
        &resolve_none,
        seeds,
    )
    .map_err(|_| SkipReason::MissingInput)?;

    let unknown = drv.has_unknown_output_paths();
    let is_ca = drv.is_content_addressed();
    let deferred = unknown;
    let ia_output_paths = if !unknown && !is_ca {
        // Static input-addressed deriver: derive the per-output paths
        // the same way the trusted plane does. The walk above already
        // proved every input hash is seeded. Sharing ONE cache between
        // the two walks is sound: `hash_modulo_walk` memoizes only
        // mask=false (input-form) entries, never the masked subject.
        match input_addressed_output_paths(&drv, drv_path, &resolve_none, seeds) {
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

/// What one best-effort population attempt did — the batch ingestion
/// loop uses this to drive its multi-pass fixpoint
/// (`store.ingest.drv-modulo-cache+2`); nothing else branches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopulateOutcome {
    /// Row upserted (or computation succeeded against an existing row).
    Populated,
    /// An input row is still absent — retryable within the same batch
    /// once siblings populate.
    MissingInput,
    /// Terminal for this attempt set: unparseable bytes, or a DB
    /// failure (counted separately).
    Skipped,
}

/// Best-effort ingestion hook: parse the just-persisted `.drv` bytes,
/// seed input hashes from existing cache rows, compute, upsert. NEVER
/// fails the upload — every outcome is a counter.
pub(crate) async fn populate_on_ingest(
    pool: &PgPool,
    drv_path: &str,
    drv_bytes: &[u8],
) -> PopulateOutcome {
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
            return PopulateOutcome::Skipped;
        }
    };
    let mut seeds = match load_drv_modulo_batch(pool, &inputs).await {
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
            return PopulateOutcome::Skipped;
        }
    };
    match compute_drv_modulo(drv_bytes, drv_path, &mut seeds) {
        Ok(computed) => {
            if let Err(e) = upsert_drv_modulo(pool, drv_path, &computed.row).await {
                warn!(drv_path, error = %e, "drv modulo upsert failed (best-effort)");
                return PopulateOutcome::Skipped;
            }
            metrics::counter!(
                "rio_store_drv_modulo_cache_total",
                "event" => "populated"
            )
            .increment(1);
            PopulateOutcome::Populated
        }
        Err(SkipReason::ParseFailed) => {
            metrics::counter!(
                "rio_store_drv_modulo_cache_total",
                "event" => "parse_failed"
            )
            .increment(1);
            PopulateOutcome::Skipped
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
                 healed by the in-batch fixpoint, the already-complete re-upload hook, \
                 or the proof-time read-through)"
            );
            PopulateOutcome::MissingInput
        }
    }
}

/// Probe-first heal for an ALREADY-COMPLETE `.drv` whose cache row is
/// missing (`store.ingest.drv-modulo-cache+2`): a no-op when the row
/// exists; otherwise reads the store's own bytes and re-fires
/// population. Re-uploads of complete derivations are the natural
/// retry signal for rows whose original population skipped (the
/// inputs arrived later) — without this, such rows were only ever
/// filled by a proof-time read-through.
pub(crate) async fn heal_if_missing(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
) {
    if !drv_path.ends_with(".drv") {
        return;
    }
    match load_drv_modulo(pool, drv_path).await {
        Ok(Some(_)) => return, // probe-first: row present, nothing to heal
        Ok(None) => {}
        Err(e) => {
            warn!(drv_path, error = %e, "modulo-cache heal probe failed (best-effort)");
            return;
        }
    }
    let mut budget = WorkBudget::new(PROOF_WALK_WORK_MAX);
    match own_drv_bytes(pool, chunks, drv_path, &mut budget).await {
        Ok(FetchedDrv::Bytes(bytes)) => {
            let _ = populate_on_ingest(pool, drv_path, &bytes).await;
        }
        Ok(_) => {}
        Err(e) => {
            warn!(drv_path, error = %e, "modulo-cache heal fetch failed (best-effort)");
        }
    }
}

/// Total work budget for one proof-time read-through walk
/// (`store.put.ia-deriver-proof+3`). UNITS, charged at call time by the
/// owning [`WorkBudget`]: 1 per cache probe (the initial row lookup and
/// one BATCHED input probe per expanded node), 1 per own-backend `.drv`
/// fetch, 1 per chunk fetched during chunked-`.drv` reassembly. Metered
/// APIs take `&mut WorkBudget`, so an unmetered awaited operation in
/// the walk is unwritable by construction.
///
/// SIZING (pattern R4 — calibrated at the measured real-world shape,
/// not toy fixtures): the measured nixpkgs `hello` closure is 1,963
/// `.drv`s at inputDrvs depth 236 (fan ≤ 96). A fully cold walk costs
/// ≤ 2 units/node (fetch + probe; leaves cost 1) ⇒ ~3,926 units, so
/// 16,384 ≈ 4.2× headroom; the merge-gated 2,048-node real-scale test
/// asserts work_used ≤ cap/2.
///
/// DOMINANCE over the deleted READ_THROUGH_MAX_FETCHES=64 /
/// READ_THROUGH_MAX_DEPTH=32 pair (R4: a replaced bound needs a written
/// dominance argument): every closure admissible under the old bounds
/// (≤ 64 fetches, depth ≤ 32) costs ≤ 64·2 + 64 = 192 units here, far
/// under 16,384 — nothing previously provable becomes unprovable. The
/// old depth bound REJECTED the measured real-world class outright
/// (236 > 32: merged_bug_002, the deploy blocker); it is deleted, not
/// retuned, because depth was never the cost being bounded — work is.
/// The old per-node costs the pair did NOT meter (one cache probe per
/// frontier entry, unbounded chunk reassembly) are charged here.
///
/// PROGRESS (monotone): every exit — including over-budget — first
/// persists every row whose input closure completed
/// ([`complete_partial_arena`]); persistence of already-discovered work
/// is deliberately exempt from the budget (bounding it would convert
/// "bounded work per attempt" into "discovered work discarded", the
/// merged_bug_002 zero-progress pathology). Retries therefore resume
/// from durable progress wherever any leaf-complete subtree fits in one
/// attempt. Accepted fail-closed residual, written here per R4: a pure
/// CHAIN deeper than ~cap/2 (≈ 8,192 nodes — 34× the measured depth
/// class) completes no subtree per attempt and stays
/// `RESOURCE_EXHAUSTED` until the cap is raised; the
/// `rio_store_ia_proof_work_units` histogram (registered with this
/// const) makes approach visible long before that.
///
/// CppNix parity note: the oracle's `hashDerivationModulo` /
/// `pathDerivationModulo` recursion (derivations.cc:856-874) is
/// UNBOUNDED (memoized in-process `drvHashes`); the budget is rio's
/// deliberate DoS deviation for an adversarial-input network service,
/// with monotone persistence restoring the oracle's convergence
/// semantics across attempts.
pub(crate) const PROOF_WALK_WORK_MAX: usize = 16_384;

/// Cap on a chunked `.drv` NAR's reassembled size. `.drv`s are text; a
/// multi-MiB one is already pathological — 64 MiB is far beyond any
/// honest derivation while bounding what one proof walk will buffer.
pub(crate) const DRV_REASSEMBLY_CAP: u64 = 64 * 1024 * 1024;

/// Typed work budget (pattern R4). All metered operations in the proof
/// walk take `&mut WorkBudget` and charge BEFORE doing the work; the
/// only constructor takes the cap, and exhaustion is a typed signal the
/// caller must route (never a silent skip).
pub(crate) struct WorkBudget {
    cap: usize,
    used: usize,
}

/// Charge refusal: the budget is exhausted.
pub(crate) struct Exhausted;

impl WorkBudget {
    pub(crate) fn new(cap: usize) -> Self {
        WorkBudget { cap, used: 0 }
    }
    /// Units consumed so far (histogram + tests).
    pub(crate) fn used(&self) -> usize {
        self.used
    }
    /// Charge `units` or refuse without consuming.
    fn charge(&mut self, units: usize) -> Result<(), Exhausted> {
        let next = self.used.saturating_add(units);
        if next > self.cap {
            return Err(Exhausted);
        }
        self.used = next;
        Ok(())
    }
}

/// Why a proof concluded ABSENT (a verdict about the closure, distinct
/// from infrastructure errors which are `Err(MetadataError)`). The gate
/// derives both the gRPC code and the client-facing message from this
/// — `PERMISSION_DENIED` is constructible only from these arms.
#[derive(Debug)]
pub(crate) enum AbsentReason {
    /// A `.drv` in the closure has no complete manifest in this store.
    NotResident { path: String },
    /// A `.drv` in the closure is resident but cannot be used.
    Unparseable { path: String, why: String },
    /// The walk exhausted its work budget; `persisted` proven rows were
    /// durably cached before returning, so a retry resumes.
    OverBudget { persisted: usize, work_used: usize },
    /// The input metadata forms a cycle: no topological order exists,
    /// so no row in the cyclic remainder is derivable. Fail-closed
    /// (`store.gc.sweep-cycle-reclaim` owns cycle reclamation).
    Cycle,
}

/// Proof-walk outcome: the deriver row, or a typed absence verdict.
pub(crate) enum ProofOutcome {
    Proven(DrvModuloRow),
    Absent(AbsentReason),
}

/// One fetched-or-refused own-backend read inside the walk.
enum FetchedDrv {
    Bytes(Vec<u8>),
    Absent,
    Unreadable(String),
    OverBudget,
}

/// Fetch a `.drv`'s raw text bytes from the store's OWN backend,
/// charging the budget (1 for the manifest fetch; 1 per chunk when
/// reassembling). Inline NARs extract directly; chunked NARs reassemble
/// through the chunk cache up to [`DRV_REASSEMBLY_CAP`].
///
/// Error classes are PROPAGATED, never folded into the absent verdict
/// (merged_bug_015 — pattern R1): `get_manifest`/chunk-backend errors →
/// `Err`; a complete manifest whose NAR fails single-file extraction →
/// `Err(InvariantViolation)` (text-CA-gated at ingestion, so this is
/// row corruption, not absence).
async fn own_drv_bytes(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
    budget: &mut WorkBudget,
) -> Result<FetchedDrv, super::MetadataError> {
    if budget.charge(1).is_err() {
        return Ok(FetchedDrv::OverBudget);
    }
    let extract = |nar: &[u8]| -> Result<Vec<u8>, super::MetadataError> {
        rio_nix::nar::extract_single_file(nar).map_err(|e| {
            super::MetadataError::InvariantViolation(format!(
                "complete .drv manifest for {drv_path} holds a NAR that is not a \
                 single regular file (text-CA-gated at ingestion; this is row \
                 corruption): {e}"
            ))
        })
    };
    match super::get_manifest(pool, drv_path).await? {
        None => Ok(FetchedDrv::Absent),
        Some(super::ManifestKind::Inline(nar)) => Ok(FetchedDrv::Bytes(extract(&nar)?)),
        Some(super::ManifestKind::Chunked(entries)) => {
            let Some(cache) = chunks else {
                return Ok(FetchedDrv::Unreadable(
                    "chunked .drv NAR but this store has no chunk backend configured".into(),
                ));
            };
            let total: u64 = entries.iter().map(|(_, sz)| u64::from(*sz)).sum();
            if total > DRV_REASSEMBLY_CAP {
                return Ok(FetchedDrv::Unreadable(format!(
                    "reassembled .drv NAR would be {total} bytes \
                     (cap {DRV_REASSEMBLY_CAP})"
                )));
            }
            let mut nar = Vec::with_capacity(total as usize);
            for (hash, _sz) in &entries {
                if budget.charge(1).is_err() {
                    return Ok(FetchedDrv::OverBudget);
                }
                let chunk = cache.get_verified(hash).await.map_err(|e| {
                    super::MetadataError::InvariantViolation(format!(
                        "chunk fetch failed reassembling .drv {drv_path}: {e}"
                    ))
                })?;
                nar.extend_from_slice(&chunk);
            }
            Ok(FetchedDrv::Bytes(extract(&nar)?))
        }
    }
}

/// Persist every arena node whose input closure is fully seeded —
/// bottom-up passes until a pass makes no progress. Returns the number
/// of rows persisted. Runs on EVERY walk exit (monotone progress, R4):
/// persistence of already-discovered work is exempt from the budget by
/// design (see [`PROOF_WALK_WORK_MAX`]).
async fn complete_partial_arena(
    pool: &PgPool,
    arena: &mut HashMap<String, (Vec<u8>, Vec<String>)>,
    seeds: &mut HashMap<String, [u8; 32]>,
) -> Result<usize, super::MetadataError> {
    let mut persisted = 0usize;
    loop {
        let ready: Vec<String> = arena
            .iter()
            .filter(|(_, (_, inputs))| inputs.iter().all(|i| seeds.contains_key(i)))
            .map(|(p, _)| p.clone())
            .collect();
        if ready.is_empty() {
            return Ok(persisted);
        }
        for path in ready {
            let (bytes, _inputs) = arena.remove(&path).expect("selected from arena");
            match compute_drv_modulo(&bytes, &path, seeds) {
                Ok(c) => {
                    seeds.insert(path.clone(), c.row.modulo_hash);
                    upsert_drv_modulo(pool, &path, &c.row).await?;
                    persisted += 1;
                }
                Err(_) => {
                    // Inputs all seeded yet the walk failed: ill-formed
                    // bytes slipped past the parse (defensive). Leave it
                    // un-persisted; the caller's verdict logic treats an
                    // underivable target as Cycle/Unparseable.
                    debug!(path, "arena node failed to compute despite seeded inputs");
                }
            }
        }
    }
}

/// Proof-time read-through (`store.put.ia-deriver-proof+3`): return the
/// deriver's cached row, computing it (and its missing ancestors) from
/// the store's own backend when absent — a single budgeted, MONOTONE
/// walk (every exit persists what it proved). One batched membership
/// probe per expanded node; frontier dedup-on-push; eager leaf-first
/// compute keeps the in-memory arena small.
// r[impl store.put.ia-deriver-proof+3]
pub(crate) async fn prove_drv_modulo(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
) -> Result<ProofOutcome, super::MetadataError> {
    let (outcome, _work) =
        prove_drv_modulo_with_cap(pool, chunks, drv_path, PROOF_WALK_WORK_MAX).await?;
    Ok(outcome)
}

/// [`prove_drv_modulo`] with an explicit cap, returning the work used —
/// the test seam for budget semantics (production always passes
/// [`PROOF_WALK_WORK_MAX`]).
pub(crate) async fn prove_drv_modulo_with_cap(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
    cap: usize,
) -> Result<(ProofOutcome, usize), super::MetadataError> {
    let mut budget = WorkBudget::new(cap);
    let result = prove_inner(pool, chunks, drv_path, &mut budget).await;
    let work = budget.used();
    metrics::histogram!("rio_store_ia_proof_work_units").record(work as f64);
    if let Ok(outcome) = &result {
        let label = match outcome {
            ProofOutcome::Proven(_) => "proven",
            ProofOutcome::Absent(AbsentReason::NotResident { .. }) => "not_resident",
            ProofOutcome::Absent(AbsentReason::Unparseable { .. }) => "unparseable",
            ProofOutcome::Absent(AbsentReason::OverBudget { .. }) => "over_budget",
            ProofOutcome::Absent(AbsentReason::Cycle) => "cycle",
        };
        metrics::counter!("rio_store_ia_proof_total", "result" => label).increment(1);
    }
    result.map(|o| (o, work))
}

async fn prove_inner(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
    budget: &mut WorkBudget,
) -> Result<ProofOutcome, super::MetadataError> {
    // Fast path: cached row (charged — it is a probe like any other).
    if budget.charge(1).is_err() {
        return Ok(ProofOutcome::Absent(AbsentReason::OverBudget {
            persisted: 0,
            work_used: budget.used(),
        }));
    }
    if let Some(row) = load_drv_modulo(pool, drv_path).await? {
        return Ok(ProofOutcome::Proven(row));
    }

    // Discovery: DFS from the target. arena = fetched-but-uncomputed
    // nodes (bytes + input list); seeds = proven input-form hashes;
    // queued = dedup-on-push.
    let mut arena: HashMap<String, (Vec<u8>, Vec<String>)> = HashMap::new();
    let mut seeds: HashMap<String, [u8; 32]> = HashMap::new();
    let mut queued: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stack: Vec<String> = vec![drv_path.to_string()];
    let mut exit: Option<AbsentReason> = None;

    while let Some(path) = stack.pop() {
        if seeds.contains_key(&path) || arena.contains_key(&path) {
            continue;
        }
        let bytes = match own_drv_bytes(pool, chunks, &path, budget).await? {
            FetchedDrv::Bytes(b) => b,
            FetchedDrv::Absent => {
                exit = Some(AbsentReason::NotResident { path });
                break;
            }
            FetchedDrv::Unreadable(why) => {
                exit = Some(AbsentReason::Unparseable { path, why });
                break;
            }
            FetchedDrv::OverBudget => {
                exit = Some(AbsentReason::OverBudget {
                    persisted: 0,
                    work_used: budget.used(),
                });
                break;
            }
        };
        let inputs: Vec<String> = match std::str::from_utf8(&bytes)
            .ok()
            .and_then(|t| rio_nix::derivation::Derivation::parse(t).ok())
        {
            Some(d) => d.input_drvs().keys().cloned().collect(),
            None => {
                exit = Some(AbsentReason::Unparseable {
                    path,
                    why: "bytes do not parse as a derivation".into(),
                });
                break;
            }
        };

        // ONE batched probe for every not-yet-seen input of this node
        // (bug_007: the per-path probe inside the old loop was an
        // unmetered query per frontier entry).
        let unseen: Vec<String> = inputs
            .iter()
            .filter(|i| !seeds.contains_key(*i) && !arena.contains_key(*i) && !queued.contains(*i))
            .cloned()
            .collect();
        if !unseen.is_empty() {
            if budget.charge(1).is_err() {
                exit = Some(AbsentReason::OverBudget {
                    persisted: 0,
                    work_used: budget.used(),
                });
                break;
            }
            let found = load_drv_modulo_batch(pool, &unseen).await?;
            for (p, h) in &found {
                seeds.insert(p.clone(), *h);
            }
            for p in unseen {
                if !seeds.contains_key(&p) {
                    queued.insert(p.clone());
                    stack.push(p);
                }
            }
        }

        // Eager leaf-first: compute immediately when every input is
        // already seeded (leaves and probe-satisfied nodes) — keeps the
        // arena small and persists progress as early as possible.
        if inputs.iter().all(|i| seeds.contains_key(i)) {
            let mut local_seeds = std::mem::take(&mut seeds);
            match compute_drv_modulo(&bytes, &path, &mut local_seeds) {
                Ok(c) => {
                    local_seeds.insert(path.clone(), c.row.modulo_hash);
                    upsert_drv_modulo(pool, &path, &c.row).await?;
                }
                Err(_) => {
                    arena.insert(path, (bytes, inputs));
                }
            }
            seeds = local_seeds;
        } else {
            arena.insert(path, (bytes, inputs));
        }
    }

    // EVERY exit persists what it proved (monotone progress).
    let persisted = complete_partial_arena(pool, &mut arena, &mut seeds).await?;

    match exit {
        Some(AbsentReason::OverBudget { work_used, .. }) => {
            Ok(ProofOutcome::Absent(AbsentReason::OverBudget {
                persisted,
                work_used,
            }))
        }
        Some(reason) => Ok(ProofOutcome::Absent(reason)),
        None => match load_drv_modulo(pool, drv_path).await? {
            Some(row) => Ok(ProofOutcome::Proven(row)),
            // Discovery completed, the arena drained as far as it
            // could, and the target is still underivable: the remainder
            // is cyclic (acyclic closures always topo-complete).
            None => Ok(ProofOutcome::Absent(AbsentReason::Cycle)),
        },
    }
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
    // r[verify store.put.ia-deriver-proof+3]
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
        let row_f = compute_drv_modulo(floating.as_bytes(), floating_path, &mut HashMap::new())
            .expect("floating row computes");
        let mut seeds: HashMap<String, [u8; 32]> =
            [(floating_path.to_string(), row_f.row.modulo_hash)].into();
        let row_p = compute_drv_modulo(parent.as_bytes(), parent_path, &mut seeds)
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

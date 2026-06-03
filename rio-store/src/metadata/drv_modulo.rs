//! Store-side derivation modulo-hash cache (`drv_modulo_cache`, M_068).
//!
//! CppNix parity: `Store::queryPartialDerivationOutputMap` answers
//! "which output paths does this deriver own" from the store's OWN copy
//! of the derivation (`store-api.cc:396-410`, backed by
//! `drvHashes`/`pathDerivationModulo`, `derivations.cc:856-874`) — never
//! from a client's claim about it. This module is rio's persistent form
//! of that table: rows are populated best-effort when a `.drv` is
//! ingested (after the text-CA gate — `store.put.drv-text-ca+3` — has
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

use rio_nix::derivation::DerivationLike as _;
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
        // ON CONFLICT: rows are content-derived immutable facts, so
        // the value columns never update — but a re-populate IS
        // residency evidence, so it clears the orphan stamp (M_073
        // lifecycle clock; the WHERE guard keeps the no-op case
        // write-free for fixpoint re-passes).
        "INSERT INTO drv_modulo_cache \
         (drv_path_hash, drv_path, modulo_hash, ia_output_paths, deferred) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (drv_path_hash) DO UPDATE SET orphaned_at = NULL \
         WHERE drv_modulo_cache.orphaned_at IS NOT NULL",
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

/// Record the TERMINAL `skipped_missing_input` event for one `.drv`
/// (round-16 bug_085; pattern R1(f) cadence): callers that retry
/// population (the batch fixpoint) call this ONCE per still-missing
/// `.drv` at fixpoint exit, never per attempt — the metric's
/// documented meaning is "this ingestion left the row unpopulated
/// pending later inputs", an event per `.drv`, not per pass.
pub(crate) fn record_missing_input(drv_path: &str) {
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
}

/// Best-effort ingestion hook: parse the just-persisted `.drv` bytes,
/// seed input hashes from existing cache rows, compute, upsert. NEVER
/// fails the upload — every outcome is a counter, with ONE exception
/// per R1(f) cadence (bug_085): the `MissingInput` outcome is
/// RETURNED, not counted, because the batch caller retries it inside
/// its fixpoint — emission per ATTEMPT inflated the counter
/// quadratically in reverse-topological batches (a 120-output batch
/// could emit ~7k increments for ~119 actual events). Every caller
/// records the terminal event via [`record_missing_input`] exactly
/// once when its retry scope is exhausted.
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
        // FOD base case (bug_083; derivations.cc:864-874): a
        // fixed-output subject's modulo hash never consults its
        // inputs, so no seeds are loaded — population cannot be
        // deferred (or seed-load-failed) by input rows the hash does
        // not need. (The compute itself already applied this cut via
        // the rio_nix walk; this makes the module's FOD handling
        // explicit end-to-end.)
        Some(drv) if drv.is_fixed_output() => Vec::new(),
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
        // NOT counted here (bug_085): the caller owns the terminal
        // decision — see [`record_missing_input`].
        Err(SkipReason::MissingInput) => PopulateOutcome::MissingInput,
    }
}

/// Probe-first heal for an ALREADY-COMPLETE `.drv` whose cache row is
/// missing (`store.ingest.drv-modulo-cache+2`): a no-op when the row
/// exists; otherwise reads the store's own bytes and re-fires
/// population. Re-uploads of complete derivations are the natural
/// retry signal for rows whose original population skipped (the
/// inputs arrived later) — without this, such rows were only ever
/// filled by a proof-time read-through.
///
/// ADMITTED, best-effort (round-16 bug_080): the spawn site fires one
/// task per AlreadyComplete `.drv` re-upload, so the heal itself is
/// the chokepoint — a fresh negative memo (this path failed to
/// populate within [`HEAL_NEGATIVE_MEMO_TTL`]) skips before any I/O;
/// the row probe runs un-permitted (probe-before-admit); a concurrent
/// heal of the SAME path skips (singleflight); fetch+populate runs
/// only under one of the [`PROOF_WALK_CONCURRENCY`] shared permits,
/// and a saturated pool skips rather than queueing (the proof-time
/// read-through, not the heal, owns correctness).
pub(crate) async fn heal_if_missing(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
) {
    if !drv_path.ends_with(".drv") {
        return;
    }
    if PROOF_ADMISSION.memo_fresh(drv_path) {
        admission_event("heal_skipped_memo");
        return;
    }
    match load_drv_modulo(pool, drv_path).await {
        Ok(Some(_)) => {
            // Probe-first: row present, nothing to heal. Drop any
            // stale memo so the map tracks only still-missing paths.
            PROOF_ADMISSION.memo_clear(drv_path);
            return;
        }
        Ok(None) => {}
        Err(e) => {
            warn!(drv_path, error = %e, "modulo-cache heal probe failed (best-effort)");
            return;
        }
    }
    let Some(_flight) = PROOF_ADMISSION.begin_heal(drv_path) else {
        admission_event("heal_skipped_inflight");
        return;
    };
    let Ok(_permit) = PROOF_ADMISSION.permits.try_acquire() else {
        // Saturated: skip WITHOUT a memo — saturation is a global
        // condition, not evidence about this path.
        admission_event("heal_skipped_saturated");
        return;
    };
    let mut budget = WorkBudget::new(PROOF_WALK_WORK_MAX, PROOF_WALK_ARENA_BYTES_MAX);
    let healed = match own_drv_bytes(pool, chunks, drv_path, &mut budget).await {
        Ok(FetchedDrv::Bytes(bytes)) => match populate_on_ingest(pool, drv_path, &bytes).await {
            PopulateOutcome::Populated => true,
            PopulateOutcome::MissingInput => {
                // The heal is its own (single-attempt) retry scope —
                // terminal here, so record (bug_085 cadence contract).
                record_missing_input(drv_path);
                false
            }
            PopulateOutcome::Skipped => false,
        },
        Ok(_) => false,
        Err(e) => {
            warn!(drv_path, error = %e, "modulo-cache heal fetch failed (best-effort)");
            false
        }
    };
    if healed {
        PROOF_ADMISSION.memo_clear(drv_path);
    } else {
        PROOF_ADMISSION.memo_record(drv_path);
    }
}

/// Total work budget for one proof-time read-through walk
/// (`store.put.ia-deriver-proof+4`). UNITS, charged at call time by the
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

/// Cap on the BYTES one proof walk may RETAIN in its arena
/// (fetched-but-uncomputed `.drv` bytes + their path/input strings),
/// charged BEFORE each retention (round-16 bug_079; pattern R4
/// SCALE-PER-DIMENSION: the work-unit budget counts *operations* and is
/// blind to padded payloads — 16,384 units of 16 MiB `.drv`s would have
/// retained ~256 GiB while "within budget").
///
/// SIZING: the measured nixpkgs `hello` closure (1,963 `.drv`s) totals
/// single-digit MiB of `.drv` text, so 256 MiB is ~30× the measured
/// real-world closure; a walk needing more is byte-flood-shaped, not
/// honest. Adversarial floor: with the W1 admission cap
/// (`rio_common::limits::MAX_DRV_NAR_BYTES` = 16 MiB per `.drv` at
/// ingestion), one walk retains at most ~16 maximal-padding nodes
/// before exhausting — typed `RESOURCE_EXHAUSTED`, and the monotone
/// exit drain still persists every leaf-complete subtree first.
///
/// DOMINANCE (what is and is not charged): the arena's
/// `(bytes, inputs)` values and their path keys are charged via
/// [`arena_charge`]; the `seeds` (32-byte hashes) and `queued`
/// (path-string) ledgers are NOT charged — both are bounded by the
/// work-unit budget (entries exist only for charged probes/fetches,
/// ≤ ~512 B each ⇒ ≤ ~8 MiB at the work cap, two orders below this
/// const); the transient chunk-reassembly buffer is bounded separately
/// by [`DRV_REASSEMBLY_CAP`] and is freed (parsed into retained bytes
/// or dropped) before the next retention.
///
/// AGGREGATE (R4): per-walk. Cold walks run only under a
/// [`PROOF_WALK_CONCURRENCY`] admission permit, so the store-process
/// aggregate of retained arenas is `PROOF_WALK_CONCURRENCY ×
/// PROOF_WALK_ARENA_BYTES_MAX` = 1 GiB.
///
/// The `rio_store_ia_proof_arena_bytes` histogram (registered with
/// this const) records per-walk charged bytes so approach is visible
/// long before exhaustion.
pub(crate) const PROOF_WALK_ARENA_BYTES_MAX: usize = 256 * 1024 * 1024;

/// Bytes one arena retention charges: the retained `.drv` bytes, the
/// path key, the input-path strings, and a fixed per-node bookkeeping
/// overhead (map entry + Vec/String headers; 64 B upper-bounds the
/// containers' inline parts on 64-bit).
fn arena_charge(path: &str, bytes: &[u8], inputs: &[String]) -> usize {
    bytes.len() + path.len() + inputs.iter().map(String::len).sum::<usize>() + 64
}

/// Concurrent budgeted-walk admission permits (round-16 bug_080;
/// pattern R4 AGGREGATE/ADMISSION). Cold proof walks and heal tasks
/// share one process-wide pool, so the AGGREGATE retained-byte bound
/// is `PROOF_WALK_CONCURRENCY × PROOF_WALK_ARENA_BYTES_MAX` = 1 GiB
/// (heal tasks retain at most one [`DRV_REASSEMBLY_CAP`] buffer each,
/// strictly below the per-walk arena cap).
///
/// SIZING: cold walks are PG/S3-bound, not CPU-bound — 4 concurrent
/// walks keep a recovering store busy without letting a cold-cache
/// stampede (every post-wipe upload misses the cache simultaneously)
/// multiply the worst-case arena by the request count. Warm traffic
/// NEVER queues here: the row probe runs before admission
/// (probe-before-admit), so a cached proof costs one indexed point
/// query and no permit.
pub(crate) const PROOF_WALK_CONCURRENCY: usize = 4;

/// How long a failed heal attempt for a path suppresses further heal
/// spawns for that path (round-16 bug_080: permanently-unpopulatable
/// `.drv`s — garbage bytes at a text-CA path, perpetually-missing
/// inputs — re-fired a fetch+parse per AlreadyComplete re-upload,
/// forever). 10 minutes per owner sign-off (plan Q3): long enough to
/// collapse re-upload stampedes to one attempt per window, short
/// enough that a path healed sideways (inputs arrive later) is retried
/// the same operator-minute; correctness never depends on the heal —
/// the proof-time read-through still completes chains on demand.
pub(crate) const HEAL_NEGATIVE_MEMO_TTL: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);

/// Negative-memo capacity. 4096 paths × ~150 B ≈ 600 KiB worst case.
/// When full after purging expired entries, new failures are NOT
/// memoized (fail-open): the cost of a lost memo is one extra
/// permit-bounded heal attempt per TTL, never unbounded work.
const HEAL_MEMO_MAX: usize = 4096;

/// Process-wide admission state for budgeted walks and heals
/// (chokepoint per R2: it lives with the walk it admits; the only
/// route to a cold walk is [`prove_drv_modulo_with_caps`] and the only
/// heal entry is [`heal_if_missing`], both of which consult this).
struct ProofAdmission {
    /// Cold-walk/heal concurrency permits ([`PROOF_WALK_CONCURRENCY`]).
    permits: tokio::sync::Semaphore,
    /// Heal singleflight: paths with a heal currently in flight.
    heal_inflight: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Heal negative memo: path → when its last heal attempt failed.
    heal_memo: std::sync::Mutex<HashMap<String, std::time::Instant>>,
}

/// Removes the path from the inflight set when the heal exits (any
/// path: success, failure, panic-unwind).
struct HealFlight<'a> {
    admission: &'a ProofAdmission,
    path: String,
}

impl Drop for HealFlight<'_> {
    fn drop(&mut self) {
        self.admission
            .heal_inflight
            .lock()
            .expect("heal_inflight mutex poisoned")
            .remove(&self.path);
    }
}

impl ProofAdmission {
    fn new() -> Self {
        ProofAdmission {
            permits: tokio::sync::Semaphore::new(PROOF_WALK_CONCURRENCY),
            heal_inflight: std::sync::Mutex::new(std::collections::HashSet::new()),
            heal_memo: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Is there a fresh negative memo for `path`?
    fn memo_fresh(&self, path: &str) -> bool {
        self.heal_memo
            .lock()
            .expect("heal_memo mutex poisoned")
            .get(path)
            .is_some_and(|t| t.elapsed() < HEAL_NEGATIVE_MEMO_TTL)
    }

    /// Record a failed heal attempt for `path`. Purges expired entries
    /// when at capacity; fails open (no memo) if still full.
    fn memo_record(&self, path: &str) {
        let mut memo = self.heal_memo.lock().expect("heal_memo mutex poisoned");
        if memo.len() >= HEAL_MEMO_MAX && !memo.contains_key(path) {
            memo.retain(|_, t| t.elapsed() < HEAL_NEGATIVE_MEMO_TTL);
            if memo.len() >= HEAL_MEMO_MAX {
                return; // fail-open; cost bounded by permits
            }
        }
        memo.insert(path.to_string(), std::time::Instant::now());
    }

    /// Drop any memo for `path` (it populated, or its row exists).
    fn memo_clear(&self, path: &str) {
        self.heal_memo
            .lock()
            .expect("heal_memo mutex poisoned")
            .remove(path);
    }

    /// Singleflight entry: claim the in-flight slot for `path`, or
    /// `None` if another heal for the same path is already running.
    fn begin_heal(&self, path: &str) -> Option<HealFlight<'_>> {
        let mut inflight = self
            .heal_inflight
            .lock()
            .expect("heal_inflight mutex poisoned");
        if !inflight.insert(path.to_string()) {
            return None;
        }
        Some(HealFlight {
            admission: self,
            path: path.to_string(),
        })
    }
}

static PROOF_ADMISSION: std::sync::LazyLock<ProofAdmission> =
    std::sync::LazyLock::new(ProofAdmission::new);

fn admission_event(event: &'static str) {
    metrics::counter!("rio_store_ia_proof_admission_total", "event" => event).increment(1);
}

/// Typed work budget (pattern R4). All metered operations in the proof
/// walk take `&mut WorkBudget` and charge BEFORE doing the work; the
/// only constructor takes the caps, and exhaustion is a typed signal
/// the caller must route (never a silent skip).
///
/// TWO LEDGERS, one type: work UNITS (ops — [`PROOF_WALK_WORK_MAX`])
/// and retained arena BYTES ([`PROOF_WALK_ARENA_BYTES_MAX`]). Both are
/// charge-only (never refunded): the byte ledger is the cumulative
/// retention charge, not a live gauge — it upper-bounds live arena
/// memory by construction, and stays an over-approximation if a future
/// refactor drains and re-retains.
pub(crate) struct WorkBudget {
    cap: usize,
    used: usize,
    arena_cap: usize,
    arena_used: usize,
}

/// Charge refusal: the budget is exhausted.
pub(crate) struct Exhausted;

impl WorkBudget {
    pub(crate) fn new(cap: usize, arena_cap: usize) -> Self {
        WorkBudget {
            cap,
            used: 0,
            arena_cap,
            arena_used: 0,
        }
    }
    /// Units consumed so far (histogram + tests).
    pub(crate) fn used(&self) -> usize {
        self.used
    }
    /// Arena bytes charged so far (histogram + tests).
    pub(crate) fn arena_used(&self) -> usize {
        self.arena_used
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
    /// Charge `bytes` of arena retention or refuse without consuming.
    /// Charged BEFORE the insert, so over-cap bytes are never retained.
    fn charge_arena(&mut self, bytes: usize) -> Result<(), Exhausted> {
        let next = self.arena_used.saturating_add(bytes);
        if next > self.arena_cap {
            return Err(Exhausted);
        }
        self.arena_used = next;
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
    /// The walk exhausted its budget (work units or arena bytes).
    /// `persisted` is the TOTAL rows this attempt made durable — eager
    /// mid-flight computes and the exit drain both route through the
    /// walk owner's sole [`ProofWalk::persist`] chokepoint, so the
    /// count equals the SQL row delta by construction (round-16
    /// merged_bug_086; pinned by the row-delta test). A retry resumes
    /// from those rows.
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
/// `Err(InvariantViolation)` (text-CA-gated at ingestion, so THAT one
/// is row corruption, not absence).
///
/// Chunk-fetch failures map PER VARIANT at the producing statement
/// (round-16 bug_027): a transient backend failure (S3 blip, timeout,
/// task panic) is `ChunkBackend` (UNAVAILABLE — retry); an
/// authoritative not-found for a manifest-referenced chunk, or a
/// content-verification failure, is `DataLoss` (DATA_LOSS — the
/// manifest's claim is broken). Neither is ever an absence verdict.
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
                let chunk = cache.get_verified(hash).await.map_err(|e| match e {
                    crate::cas::ChunkError::Backend { .. } => super::MetadataError::ChunkBackend(
                        format!("reassembling .drv {drv_path}: {e}"),
                    ),
                    crate::cas::ChunkError::NotFound(_)
                    | crate::cas::ChunkError::Corrupt { .. } => {
                        super::MetadataError::DataLoss(format!(
                            "manifest-referenced chunk failed reassembling .drv {drv_path}: {e}"
                        ))
                    }
                })?;
                nar.extend_from_slice(&chunk);
            }
            Ok(FetchedDrv::Bytes(extract(&nar)?))
        }
    }
}

/// The proof-walk OWNER (round-16 bug_084 + merged_bug_086; the MP1
/// contract-owner paydown — a remedy that introduces a contract
/// introduces, same commit, the type that owns its transitions):
///
/// - [`Self::persist`] is the SOLE row-upsert site of the walk: eager
///   mid-flight computes and the exit drain both route through it, so
///   `persisted` equals the SQL row delta BY CONSTRUCTION — a persist
///   the count doesn't see is unwritable (merged_bug_086's
///   dual-persist-site drift class dies here).
/// - [`Self::finish`] and [`Self::fail`] CONSUME the walk and BOTH run
///   the exit [`Self::drain`], and they are the only ways out of
///   [`prove_inner`]'s routing match: a `?` inside [`Self::discover`]
///   propagates to that match, which routes it through `fail` — so an
///   infrastructure `Err` can no longer skip the monotone-persistence
///   obligation (bug_084), and a NEW exit class cannot be written that
///   bypasses it (R4 EXIT CLASSES, structural form).
struct ProofWalk<'a> {
    pool: &'a PgPool,
    chunks: Option<&'a crate::cas::ChunkCache>,
    budget: &'a mut WorkBudget,
    /// Fetched-but-uncomputed nodes: path → (bytes, input list).
    arena: HashMap<String, (Vec<u8>, Vec<String>)>,
    /// Proven input-form hashes.
    seeds: HashMap<String, [u8; 32]>,
    /// Dedup-on-push frontier ledger.
    queued: std::collections::HashSet<String>,
    /// Rows made durable by THIS walk — incremented only inside
    /// [`Self::persist`].
    persisted: usize,
}

impl<'a> ProofWalk<'a> {
    fn new(
        pool: &'a PgPool,
        chunks: Option<&'a crate::cas::ChunkCache>,
        budget: &'a mut WorkBudget,
    ) -> Self {
        ProofWalk {
            pool,
            chunks,
            budget,
            arena: HashMap::new(),
            seeds: HashMap::new(),
            queued: std::collections::HashSet::new(),
            persisted: 0,
        }
    }

    /// THE persist chokepoint: upsert + seed + count, in one place.
    /// Every row this walk makes durable goes through here.
    async fn persist(
        &mut self,
        path: &str,
        row: &DrvModuloRow,
    ) -> Result<(), super::MetadataError> {
        upsert_drv_modulo(self.pool, path, row).await?;
        self.seeds.insert(path.to_string(), row.modulo_hash);
        self.persisted += 1;
        Ok(())
    }

    /// Exit drain: persist every arena node whose input closure is
    /// fully seeded — bottom-up passes until a pass makes no progress.
    /// Runs on EVERY walk exit (typed verdict via [`Self::finish`],
    /// `Err` propagation via [`Self::fail`]) — monotone progress, R4.
    /// Persistence of already-discovered work is deliberately exempt
    /// from the budget (see [`PROOF_WALK_WORK_MAX`]).
    async fn drain(&mut self) -> Result<(), super::MetadataError> {
        loop {
            let ready: Vec<String> = self
                .arena
                .iter()
                .filter(|(_, (_, inputs))| inputs.iter().all(|i| self.seeds.contains_key(i)))
                .map(|(p, _)| p.clone())
                .collect();
            if ready.is_empty() {
                return Ok(());
            }
            for path in ready {
                let (bytes, _inputs) = self.arena.remove(&path).expect("selected from arena");
                match compute_drv_modulo(&bytes, &path, &mut self.seeds) {
                    Ok(c) => {
                        self.persist(&path, &c.row).await?;
                    }
                    Err(_) => {
                        // Inputs all seeded yet the walk failed:
                        // ill-formed bytes slipped past the parse
                        // (defensive). Leave it un-persisted; the
                        // verdict logic treats an underivable target
                        // as Cycle/Unparseable.
                        debug!(path, "arena node failed to compute despite seeded inputs");
                    }
                }
            }
        }
    }

    /// Typed-verdict exit: drain, then derive the outcome. Consumes
    /// the walk.
    async fn finish(
        mut self,
        target: &str,
        exit: Option<AbsentReason>,
    ) -> Result<ProofOutcome, super::MetadataError> {
        self.drain().await?;
        match exit {
            Some(AbsentReason::OverBudget { work_used, .. }) => {
                Ok(ProofOutcome::Absent(AbsentReason::OverBudget {
                    persisted: self.persisted,
                    work_used,
                }))
            }
            Some(reason) => Ok(ProofOutcome::Absent(reason)),
            None => match load_drv_modulo(self.pool, target).await? {
                Some(row) => Ok(ProofOutcome::Proven(row)),
                // Discovery completed, the arena drained as far as it
                // could, and the target is still underivable: the
                // remainder is cyclic (acyclic closures always
                // topo-complete).
                None => Ok(ProofOutcome::Absent(AbsentReason::Cycle)),
            },
        }
    }

    /// `Err`-propagation exit: drain best-effort (a drain failure is
    /// logged and swallowed — the PRIMARY error is what the caller
    /// must see), then hand the error back. Consumes the walk.
    /// Closes round-16 bug_084: pre-owner, `?` paths dropped the
    /// arena, losing every drain-eligible subtree on infra errors.
    async fn fail(mut self, err: super::MetadataError) -> super::MetadataError {
        if let Err(e2) = self.drain().await {
            debug!(
                error = %e2,
                "proof-walk exit drain failed during error propagation \
                 (best-effort; primary error preserved)"
            );
        }
        err
    }

    /// Discovery: DFS from `target`. `Ok(Some(reason))` = typed absent
    /// exit; `Ok(None)` = clean completion (verdict derived by
    /// [`Self::finish`]); `Err` = infrastructure failure (routed
    /// through [`Self::fail`] by the caller's match).
    async fn discover(
        &mut self,
        target: &str,
    ) -> Result<Option<AbsentReason>, super::MetadataError> {
        let mut stack: Vec<String> = vec![target.to_string()];

        while let Some(path) = stack.pop() {
            if self.seeds.contains_key(&path) || self.arena.contains_key(&path) {
                continue;
            }
            let bytes = match own_drv_bytes(self.pool, self.chunks, &path, self.budget).await? {
                FetchedDrv::Bytes(b) => b,
                FetchedDrv::Absent => {
                    return Ok(Some(AbsentReason::NotResident { path }));
                }
                FetchedDrv::Unreadable(why) => {
                    return Ok(Some(AbsentReason::Unparseable { path, why }));
                }
                FetchedDrv::OverBudget => {
                    return Ok(Some(AbsentReason::OverBudget {
                        persisted: 0,
                        work_used: self.budget.used(),
                    }));
                }
            };
            let inputs: Vec<String> = match std::str::from_utf8(&bytes)
                .ok()
                .and_then(|t| rio_nix::derivation::Derivation::parse(t).ok())
            {
                // FOD base case (round-16 bug_083; oracle parity:
                // `hashDerivationModulo`, derivations.cc:864-874 — the
                // fixed-output branch returns the `fixed:out:…`
                // fingerprint WITHOUT recursing into inputDrvs, and the
                // rio_nix walk's Visit arm applies the same cut). A
                // fixed-output node's modulo hash needs nothing below
                // it, so the walk treats it as a leaf: its inputs are
                // never probed, queued, fetched, or required resident.
                // Pre-fix, a FOD whose fetch-tooling inputs were GC'd
                // (or never uploaded) failed the whole proof
                // NotResident on a node the oracle never visits.
                Some(d) if d.is_fixed_output() => Vec::new(),
                Some(d) => d.input_drvs().keys().cloned().collect(),
                None => {
                    return Ok(Some(AbsentReason::Unparseable {
                        path,
                        why: "bytes do not parse as a derivation".into(),
                    }));
                }
            };

            // ONE batched probe for every not-yet-seen input of this
            // node (bug_007: the per-path probe inside the old loop was
            // an unmetered query per frontier entry).
            let unseen: Vec<String> = inputs
                .iter()
                .filter(|i| {
                    !self.seeds.contains_key(*i)
                        && !self.arena.contains_key(*i)
                        && !self.queued.contains(*i)
                })
                .cloned()
                .collect();
            if !unseen.is_empty() {
                if self.budget.charge(1).is_err() {
                    return Ok(Some(AbsentReason::OverBudget {
                        persisted: 0,
                        work_used: self.budget.used(),
                    }));
                }
                let found = load_drv_modulo_batch(self.pool, &unseen).await?;
                for (p, h) in &found {
                    self.seeds.insert(p.clone(), *h);
                }
                for p in unseen {
                    if !self.seeds.contains_key(&p) {
                        self.queued.insert(p.clone());
                        stack.push(p);
                    }
                }
            }

            // Eager leaf-first: compute immediately when every input is
            // already seeded (leaves and probe-satisfied nodes) — keeps
            // the arena small and persists progress as early as
            // possible; the row routes through the persist chokepoint.
            if inputs.iter().all(|i| self.seeds.contains_key(i)) {
                match compute_drv_modulo(&bytes, &path, &mut self.seeds) {
                    Ok(c) => {
                        self.persist(&path, &c.row).await?;
                        continue;
                    }
                    Err(_) => {
                        // Fall through to retention (defensive: parse
                        // succeeded but the walk failed; drain retries).
                    }
                }
            }
            // Retention is charged BEFORE the insert (bug_079):
            // over-cap bytes are never resident.
            if self
                .budget
                .charge_arena(arena_charge(&path, &bytes, &inputs))
                .is_err()
            {
                return Ok(Some(AbsentReason::OverBudget {
                    persisted: 0,
                    work_used: self.budget.used(),
                }));
            }
            self.arena.insert(path, (bytes, inputs));
        }
        Ok(None)
    }
}

/// Proof-time read-through (`store.put.ia-deriver-proof+4`): return the
/// deriver's cached row, computing it (and its missing ancestors) from
/// the store's own backend when absent — a single budgeted, MONOTONE
/// walk (every exit persists what it proved). One batched membership
/// probe per expanded node; frontier dedup-on-push; eager leaf-first
/// compute keeps the in-memory arena small.
// r[impl store.put.ia-deriver-proof+4]
pub(crate) async fn prove_drv_modulo(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
) -> Result<ProofOutcome, super::MetadataError> {
    let (outcome, _work) = prove_drv_modulo_with_caps(
        pool,
        chunks,
        drv_path,
        PROOF_WALK_WORK_MAX,
        PROOF_WALK_ARENA_BYTES_MAX,
    )
    .await?;
    Ok(outcome)
}

/// [`prove_drv_modulo`] with explicit caps, returning the work used —
/// the test seam for budget semantics (production always passes
/// [`PROOF_WALK_WORK_MAX`] / [`PROOF_WALK_ARENA_BYTES_MAX`]).
pub(crate) async fn prove_drv_modulo_with_caps(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
    cap: usize,
    arena_cap: usize,
) -> Result<(ProofOutcome, usize), super::MetadataError> {
    let mut budget = WorkBudget::new(cap, arena_cap);
    let result = prove_admitted(pool, chunks, drv_path, &mut budget).await;
    let work = budget.used();
    metrics::histogram!("rio_store_ia_proof_work_units").record(work as f64);
    metrics::histogram!("rio_store_ia_proof_arena_bytes").record(budget.arena_used() as f64);
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

/// Probe-before-admit wrapper (round-16 bug_080): the warm fast path —
/// a cached row — costs one charged probe and NO permit; only a cache
/// miss (a genuinely cold, budgeted walk) acquires one of the
/// [`PROOF_WALK_CONCURRENCY`] permits, held for the walk's lifetime so
/// the process aggregate of retained arenas is permits × arena cap.
/// `prove_inner`'s own initial probe re-checks after the (possibly
/// queued) acquire — a concurrent admitted walk that proved this path
/// while we waited turns our walk into a second fast path.
async fn prove_admitted(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
    budget: &mut WorkBudget,
) -> Result<ProofOutcome, super::MetadataError> {
    if budget.charge(1).is_err() {
        return Ok(ProofOutcome::Absent(AbsentReason::OverBudget {
            persisted: 0,
            work_used: budget.used(),
        }));
    }
    if let Some(row) = load_drv_modulo(pool, drv_path).await? {
        admission_event("fast_path");
        return Ok(ProofOutcome::Proven(row));
    }
    let _permit = PROOF_ADMISSION
        .permits
        .acquire()
        .await
        .expect("proof-walk admission semaphore is never closed");
    admission_event("admitted");
    prove_inner(pool, chunks, drv_path, budget).await
}

/// EVERY exit persists what it proved (monotone progress): the routing
/// match below is the walk's ONLY exit surface — typed verdicts and
/// clean completions route through [`ProofWalk::finish`], `Err`s
/// through [`ProofWalk::fail`], and both drain. Adding an exit that
/// bypasses the obligation requires bypassing this match (round-16
/// bug_084's structural close).
async fn prove_inner(
    pool: &PgPool,
    chunks: Option<&crate::cas::ChunkCache>,
    drv_path: &str,
    budget: &mut WorkBudget,
) -> Result<ProofOutcome, super::MetadataError> {
    // Post-admission probe (charged — it is a probe like any other):
    // re-checks the cache after a possibly-queued permit acquire.
    if budget.charge(1).is_err() {
        return Ok(ProofOutcome::Absent(AbsentReason::OverBudget {
            persisted: 0,
            work_used: budget.used(),
        }));
    }
    if let Some(row) = load_drv_modulo(pool, drv_path).await? {
        return Ok(ProofOutcome::Proven(row));
    }

    let mut walk = ProofWalk::new(pool, chunks, budget);
    match walk.discover(drv_path).await {
        Ok(exit) => walk.finish(drv_path, exit).await,
        Err(e) => Err(walk.fail(e).await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Admission semantics (bug_080), unit-level: memo TTL freshness,
    /// capacity fail-open, singleflight claim/release, permit count.
    #[test]
    fn admission_memo_ttl_singleflight_and_permits() {
        let adm = ProofAdmission::new();
        let p = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv";

        // Negative memo: absent → not fresh; recorded → fresh;
        // cleared → not fresh.
        assert!(!adm.memo_fresh(p));
        adm.memo_record(p);
        assert!(adm.memo_fresh(p));
        adm.memo_clear(p);
        assert!(!adm.memo_fresh(p));

        // TTL: a backdated entry is stale.
        adm.heal_memo.lock().unwrap().insert(
            p.to_string(),
            std::time::Instant::now()
                - (HEAL_NEGATIVE_MEMO_TTL + std::time::Duration::from_secs(1)),
        );
        assert!(!adm.memo_fresh(p), "expired memo must not suppress heals");

        // Capacity: at HEAL_MEMO_MAX with all-fresh entries, a NEW path
        // fails open (not memoized) instead of evicting fresh state.
        let adm2 = ProofAdmission::new();
        for i in 0..HEAL_MEMO_MAX {
            adm2.memo_record(&format!("/nix/store/{i:032}-f.drv"));
        }
        adm2.memo_record(p);
        assert!(
            !adm2.memo_fresh(p),
            "full memo of fresh entries fails OPEN for new paths \
             (cost bounded by permits, never unbounded growth)"
        );
        // …but an EXISTING key refreshes in place even at capacity.
        let existing = format!("/nix/store/{:032}-f.drv", 0);
        adm2.memo_record(&existing);
        assert!(adm2.memo_fresh(&existing));

        // Singleflight: second claim for the same path refuses while
        // the first flight lives; releases on drop (incl. unwind).
        let flight = adm.begin_heal(p).expect("first claim wins");
        assert!(
            adm.begin_heal(p).is_none(),
            "concurrent same-path heal refused"
        );
        drop(flight);
        assert!(adm.begin_heal(p).is_some(), "slot released on drop");

        // Permit pool size is the const (aggregate = permits × arena cap).
        assert_eq!(adm.permits.available_permits(), PROOF_WALK_CONCURRENCY);
    }

    /// Heal negative-memo end-to-end (bug_080): a permanently-
    /// unpopulatable `.drv` (garbage bytes at a text-CA path) records a
    /// memo on its first heal; within the TTL the memo is fresh, so the
    /// spawn-site stampede (one task per AlreadyComplete re-upload)
    /// collapses to memo checks. A path whose row EXISTS clears its
    /// memo on probe.
    #[tokio::test]
    async fn heal_memo_records_failure_and_clears_on_present_row() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-garbage.drv";
        // Stage garbage bytes as a complete inline manifest (the
        // text-CA gate binds bytes to paths; it does not parse them).
        let node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: b"not a derivation".to_vec(),
        };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node).unwrap();
        let key = rio_nix::store_path::StorePath::parse(path)
            .unwrap()
            .sha256_digest();
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(key.as_slice())
        .bind(path)
        .bind([0u8; 32].as_slice())
        .bind(nar.len() as i64)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', $2)",
        )
        .bind(key.as_slice())
        .bind(&nar)
        .execute(&db.pool)
        .await
        .unwrap();

        assert!(!PROOF_ADMISSION.memo_fresh(path));
        heal_if_missing(&db.pool, None, path).await;
        assert!(
            PROOF_ADMISSION.memo_fresh(path),
            "failed heal must record a negative memo"
        );
        // Second heal within the TTL: the memo short-circuits before
        // any probe/fetch — observable as the row still being absent
        // and the memo unchanged (behavioral pin: no panic, no work).
        heal_if_missing(&db.pool, None, path).await;
        assert!(PROOF_ADMISSION.memo_fresh(path));

        // A path with a PRESENT row: a FRESH memo short-circuits before
        // the probe (memo check is free; the heal would no-op anyway) —
        // once the memo EXPIRES, the probe finds the row and clears the
        // entry so the map tracks only still-missing paths.
        let healthy = "/nix/store/cccccccccccccccccccccccccccccccc-ok.drv";
        upsert_drv_modulo(
            &db.pool,
            healthy,
            &DrvModuloRow {
                modulo_hash: [7u8; 32],
                ia_output_paths: HashMap::new(),
                deferred: false,
            },
        )
        .await
        .unwrap();
        PROOF_ADMISSION.memo_record(healthy);
        heal_if_missing(&db.pool, None, healthy).await;
        assert!(
            PROOF_ADMISSION.memo_fresh(healthy),
            "fresh memo short-circuits before the probe (free check first)"
        );
        PROOF_ADMISSION.heal_memo.lock().unwrap().insert(
            healthy.to_string(),
            std::time::Instant::now()
                - (HEAL_NEGATIVE_MEMO_TTL + std::time::Duration::from_secs(1)),
        );
        heal_if_missing(&db.pool, None, healthy).await;
        assert!(
            PROOF_ADMISSION
                .heal_memo
                .lock()
                .unwrap()
                .get(healthy)
                .is_none(),
            "expired memo + present row: probe must clear the entry"
        );
    }

    /// Cadence pin (bug_085; R1(f)): `populate_on_ingest` RETURNS the
    /// MissingInput outcome without counting it — the terminal
    /// `skipped_missing_input` event is the caller's to record exactly
    /// once per `.drv` when its retry scope (fixpoint pass set, single
    /// shot, heal) is exhausted. Pre-fix, the batch fixpoint emitted
    /// one increment PER PASS per still-missing `.drv` (quadratic in
    /// reverse-topological batches).
    #[tokio::test]
    async fn missing_input_outcome_is_returned_not_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let ghost = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ghost.drv";
        let consumer = format!(
            r#"Derive([("out","/nix/store/cccccccccccccccccccccccccccccccc-consumer","","")],[("{ghost}",["out"])],[],"x86_64-linux","/bin/sh",["-c","x"],[("name","consumer"),("out","/nix/store/cccccccccccccccccccccccccccccccc-consumer")])"#
        );
        let path = "/nix/store/dddddddddddddddddddddddddddddddd-consumer.drv";

        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let count_missing = |snapshot: metrics_util::debugging::Snapshot| -> u64 {
            snapshot
                .into_vec()
                .into_iter()
                .filter_map(|(key, _, _, val)| {
                    let (kind, key) = key.into_parts();
                    (kind == metrics_util::MetricKind::Counter
                        && key.name() == "rio_store_drv_modulo_cache_total"
                        && key
                            .labels()
                            .any(|l| l.key() == "event" && l.value() == "skipped_missing_input"))
                    .then_some(match val {
                        DebugValue::Counter(c) => c,
                        _ => 0,
                    })
                })
                .sum()
        };

        // Three retried attempts (the fixpoint shape): ZERO increments.
        let _g = metrics::set_default_local_recorder(&rec);
        for _ in 0..3 {
            let outcome = populate_on_ingest(&db.pool, path, consumer.as_bytes()).await;
            assert_eq!(outcome, PopulateOutcome::MissingInput);
        }
        assert_eq!(
            count_missing(snap.snapshot()),
            0,
            "per-attempt emission is the bug_085 regression"
        );
        // ONE terminal record at retry-scope exhaustion.
        record_missing_input(path);
        assert_eq!(
            count_missing(snap.snapshot()),
            1,
            "terminal event counted once"
        );
        drop(_g);
    }

    /// FOD base-case pin (bug_083; oracle derivations.cc:864-874): a
    /// fixed-output subject computes with ZERO seeds even when its
    /// `inputDrvs` is non-empty — the modulo walk never consults
    /// inputs below a FOD, so neither population nor the proof walk
    /// may demand them.
    #[test]
    fn fod_subject_computes_without_input_seeds() {
        let ghost = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ghost.drv";
        let fod = format!(
            r#"Derive([("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[("{ghost}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#
        );
        let mut empty_seeds = HashMap::new();
        let computed = compute_drv_modulo(
            fod.as_bytes(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-fixed.drv",
            &mut empty_seeds,
        )
        .expect("FOD computes without any input seeds (base case)");
        assert!(
            computed.row.ia_output_paths.is_empty() && !computed.row.deferred,
            "FOD rows are membership-only, not deferred"
        );
    }

    /// THE masked-form regression (vm-ca-cutoff class, store edition):
    /// a floating-CA derivation consumed as an INPUT must contribute
    /// its `mask_outputs=false` digest to the parent's walk — the
    /// cache row's `modulo_hash` IS that digest, never the published
    /// (masked) realisation key. Composes `compute_drv_modulo` exactly
    /// the way `populate_on_ingest` / the read-through do and compares
    /// against a full-resolution reference walk; before the
    /// input-form fix the cached composition diverged for floating
    /// inputs and silently mis-derived every downstream IA path.
    // r[verify store.put.ia-deriver-proof+4]
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

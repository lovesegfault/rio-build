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
        Ok(FetchedDrv::Bytes(bytes)) => {
            populate_on_ingest(pool, drv_path, &bytes).await == PopulateOutcome::Populated
        }
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
    /// The walk exhausted its work budget. `persisted` counts the rows
    /// the EXIT DRAIN proved and upserted (`complete_partial_arena`);
    /// rows the walk upserted eagerly mid-flight are durable too but
    /// are NOT included in this count (round-16 merged_bug_086) — so
    /// treat it as "additional rows proven at exit", not "total rows
    /// this attempt made durable". Either way the cache strictly grew
    /// and a retry resumes from it.
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

/// Persist every arena node whose input closure is fully seeded —
/// bottom-up passes until a pass makes no progress. Returns the number
/// of rows persisted. Runs on every TYPED-VERDICT walk exit (monotone
/// progress, R4); the `Err` propagation paths in `prove_inner`
/// currently bypass this drain (round-16 bug_084 — "every exit" is the
/// goal the proof-walk owner type will enforce, not the shipped
/// behavior; an infra error today loses the un-drained arena, costing
/// re-derivation on retry, never correctness). Persistence of
/// already-discovered work is exempt from the budget by
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
                    seeds = local_seeds;
                    // Retention is charged BEFORE the insert (bug_079):
                    // over-cap bytes are never resident.
                    if budget
                        .charge_arena(arena_charge(&path, &bytes, &inputs))
                        .is_err()
                    {
                        exit = Some(AbsentReason::OverBudget {
                            persisted: 0,
                            work_used: budget.used(),
                        });
                        break;
                    }
                    arena.insert(path, (bytes, inputs));
                    continue;
                }
            }
            seeds = local_seeds;
        } else {
            // Retention is charged BEFORE the insert (bug_079).
            if budget
                .charge_arena(arena_charge(&path, &bytes, &inputs))
                .is_err()
            {
                exit = Some(AbsentReason::OverBudget {
                    persisted: 0,
                    work_used: budget.used(),
                });
                break;
            }
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

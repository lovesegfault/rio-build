//! CA input resolution: rewrite `inputDrvs` placeholders to realized paths.
//!
//! Per ADR-018 Appendix B (Nix `Derivation::tryResolve`,
//! `derivations.cc:1215-1239`): a CA derivation whose inputs are
//! themselves CA carries *placeholder* strings in env/args/builder
//! where the input's output path should be. The placeholder is
//! computed from the input's `.drv` store path at ATerm-construct
//! time (before the CA input's output path is known); resolution
//! replaces each placeholder with the realized path after the input
//! completes.
//!
//! The resolved derivation is a `BasicDerivation` — `inputDrvs` is
//! empty (all inputs have been collapsed into `inputSrcs` as concrete
//! store paths).
//!
//! ## Side effect: `realisation_deps`
//!
//! Each successful `(modular_hash, output_name) → output_path` lookup
//! during resolution is recorded as a row in the `realisation_deps`
//! junction table. This is rio's **derived build trace** (ADR-018:45)
//! — a local cache of resolve-time dependency edges that the
//! scheduler computes from its own `realisations` table. It never
//! crosses the wire; `wopRegisterDrvOutput`'s `dependentRealisations`
//! field is always `{}` from current Nix (ADR-018 Finding).
// r[impl sched.ca.resolve+2]

use std::collections::{BTreeMap, HashMap, HashSet};

use rio_nix::derivation::{BasicDerivation, Derivation, DerivationError};
use rio_nix::store_path::{StorePath, StorePathError, nixbase32};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;
use tracing::{debug, instrument, warn};

/// Errors during CA input resolution.
#[derive(Debug, Error)]
pub enum ResolveError {
    /// The `drv_content` bytes weren't valid UTF-8. ATerm is ASCII;
    /// a non-UTF-8 blob means the gateway inlined garbage.
    #[error("derivation content is not valid UTF-8: {0}")]
    InvalidUtf8(#[from] std::str::Utf8Error),

    /// ATerm parse failed. Likely a truncated or corrupted inline.
    #[error("failed to parse derivation ATerm: {0}")]
    Parse(#[from] DerivationError),

    /// An `inputDrvs` path didn't parse as a `StorePath`. Nix never
    /// produces this — guard against manually-crafted derivations.
    #[error("input derivation path is not a valid store path: {0}")]
    InvalidInputDrvPath(#[from] StorePathError),

    /// A CA input's realisation wasn't found. The DAG guarantees the
    /// input is Completed before the parent dispatches (all deps must
    /// be Completed for Queued→Ready), so a missing realisation means
    /// the input completed WITHOUT registering a realisation — either
    /// `wopRegisterDrvOutput` was never called (client bug) or the
    /// realisation row was GC'd (scheduler/GC bug).
    #[error("realisation for {drv_path}!{output_name} not found (modular hash {modular_hex})")]
    RealisationMissing {
        drv_path: String,
        output_name: String,
        modular_hex: String,
    },

    /// DB error from the realisations lookup or `realisation_deps`
    /// insert. Transient (PG blip) — caller should defer and retry.
    #[error("database error during resolution: {0}")]
    Db(#[from] sqlx::Error),

    /// No `drv_content` available. Resolution requires the full
    /// ATerm to parse `inputDrvs` and perform placeholder replacement.
    /// Caller should fetch from store or defer.
    #[error("drv_content is empty — cannot resolve without ATerm")]
    NoDrvContent,
}

/// One CA input to resolve: the `.drv` store path, the modular
/// derivation hash (realisations PK), and the output names this
/// parent derivation depends on.
///
/// The modular hash must be the 32-byte SHA-256 from
/// `hash_derivation_modulo` — the same value Nix sends as the
/// `sha256:<hex>` prefix of `wopRegisterDrvOutput`'s `id` field.
/// The scheduler does NOT compute this itself; it receives it from
/// the gateway via `DerivationNode.ca_modular_hash` (computed
/// post-BFS from the full drv_cache — see
/// `rio-gateway/src/translate.rs:populate_ca_modular_hashes`).
#[derive(Debug, Clone)]
pub struct CaResolveInput {
    /// Store path of the input `.drv` file. Matches an `inputDrvs`
    /// key in the parent's ATerm exactly.
    pub drv_path: String,
    /// Modular derivation hash (`hashDerivationModulo`). The
    /// `drv_hash` half of the `realisations` composite PK.
    pub modular_hash: [u8; 32],
    /// Output names the parent depends on (from the `inputDrvs`
    /// value set). Usually just `["out"]`.
    pub output_names: Vec<String>,
}

/// One IA (input-addressed) input for resolve: the `.drv` store
/// path and the name→path mapping for its outputs.
///
/// IA outputs are deterministic — the gateway computes them at
/// submit time from the parsed `.drv` and plumbs them via
/// `DerivationNode.expected_output_paths`. No store RPC needed.
///
/// `output_names` and `output_paths` are **index-paired** (same
/// layout as `DerivationState`). The parent's `inputDrvs` entry is
/// `(drv_path, {wanted_names})`; resolve collects `output_paths[i]`
/// for each `i` where `output_names[i]` is in `wanted_names`.
#[derive(Debug, Clone)]
pub struct IaResolveInput {
    /// Store path of the input `.drv` file. Matches an `inputDrvs`
    /// key in the parent's ATerm exactly.
    pub drv_path: String,
    /// Output names. Index-paired with `output_paths`.
    pub output_names: Vec<String>,
    /// Expected output store paths. Index-paired with `output_names`.
    /// From the DAG's `DerivationState.expected_output_paths`.
    pub output_paths: Vec<String>,
}

/// A successful realisation lookup, recorded for the
/// `realisation_deps` insert side-effect.
///
/// The `(dep_modular_hash, dep_output_name)` pair is the **dependency**
/// side; the parent's own `(modular_hash, output_name)` pair (if the
/// parent is itself CA, which it must be for resolve to fire) is the
/// **dependent** side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealisationLookup {
    /// Modular hash of the input derivation (realisations PK half).
    pub dep_modular_hash: [u8; 32],
    /// Output name of the input derivation.
    pub dep_output_name: String,
    /// Realized store path (from the realisations table).
    pub realized_path: String,
}

/// Result of [`resolve_ca_inputs`]: the rewritten ATerm bytes ready
/// for dispatch, plus the realisation lookups performed (for the
/// `realisation_deps` side-effect insert — see [`insert_realisation_deps`]).
#[derive(Debug)]
pub struct ResolvedDerivation {
    /// ATerm-serialized resolved derivation. `inputDrvs` is `[]`;
    /// all CA-input output paths are now in `inputSrcs`; placeholders
    /// in env/args/builder are replaced with realized paths.
    pub drv_content: Vec<u8>,
    /// Every `(dep_modular_hash, dep_output_name) → realized_path`
    /// lookup that succeeded. Fed to [`insert_realisation_deps`].
    pub lookups: Vec<RealisationLookup>,
}

/// Compute the Nix `DownstreamPlaceholder` rendering for an input
/// derivation's output.
///
/// Per `downstream-placeholder.cc:7-21`:
///
/// 1. Clear-text: `"nix-upstream-output:" + drvPath.hashPart() + ":" + outputPathName(drvName, outputName)`
///    where `outputPathName` is just `drvName` for the `"out"` output,
///    and `drvName + "-" + outputName` otherwise.
/// 2. SHA-256 the clear-text.
/// 3. Render: `"/" + nixbase32(hash)` — a 53-char string starting
///    with `/` (`/` + 52 chars for a 32-byte nixbase32).
///
/// `drv_path` must be a valid store path (the input `.drv` file);
/// `output_name` is e.g. `"out"`, `"dev"`.
///
/// This is the string that appears LITERALLY in the parent derivation's
/// env/args wherever the input's output path would be — it's what
/// [`resolve_ca_inputs`] string-replaces.
pub fn downstream_placeholder(drv_path: &StorePath, output_name: &str) -> String {
    // drvName: the store-path name with the trailing ".drv" stripped.
    // Nix: `drvPath.name()` on a .drv path already strips the
    // extension (it's baked into DrvPath::name()). Our StorePath
    // doesn't special-case .drv, so strip manually.
    let drv_name = drv_path
        .name()
        .strip_suffix(".drv")
        .unwrap_or(drv_path.name());

    // outputPathName: `name` for "out", `name-outputName` otherwise.
    // Matches `derivation-common.cc:outputPathName`.
    let output_path_name = if output_name == "out" {
        drv_name.to_string()
    } else {
        format!("{drv_name}-{output_name}")
    };

    let cleartext = format!(
        "nix-upstream-output:{}:{}",
        drv_path.hash_part(),
        output_path_name
    );
    let hash: [u8; 32] = Sha256::digest(cleartext.as_bytes()).into();
    format!("/{}", nixbase32::encode(&hash))
}

/// After all CA-input derivations complete, query realisations for
/// their outputs and rewrite `inputDrvs` placeholder paths to
/// realized store paths. Returns the "resolved" derivation ready for
/// dispatch.
///
/// Per ADR-018 Appendix B (tryResolve @ derivations.cc:1215-1239):
///
/// For each `(inputDrv, outputNames)` in `ca_inputs`:
/// 1. Query `realisations` for `(modular_hash, output_name)` → `output_path`
/// 2. Insert `output_path` into the resolved derivation's `inputSrcs`
/// 3. Record rewrite: `DownstreamPlaceholder(input, output).render()` → `output_path`
/// 4. SIDE EFFECT: caller inserts into `realisation_deps` via
///    [`insert_realisation_deps`] — this IS rio's derived-build-trace.
///
/// Then: string-replace all placeholder renderings through
/// env/args/builder (Nix's `rewriteDerivation` is a global
/// string-replace through the whole ATerm, so we match that).
/// Finally: drop `inputDrvs` — resolved derivation is a
/// `BasicDerivation` serialized with `inputDrvs = []`.
///
/// ## IA inputs (`ia_inputs`)
///
/// Non-CA inputs (input-addressed, fixed-output with known path)
/// don't need PLACEHOLDER resolution — their output paths are
/// already literal in env/args/builder. They DO still need their
/// output paths added to `inputSrcs`: Nix's `tryResolveInput`
/// (`derivations.cc:1206-1234`) iterates **every** `inputDrv`
/// regardless of addressing mode, adding each output path to
/// `inputSrcs`. `ia_inputs` provides the name→path mapping
/// pre-collected from the DAG's `expected_output_paths` (no store
/// RPC needed — the gateway computed these at submit time).
///
/// Caller guarantees at least one of `ca_inputs` / `ia_inputs` is
/// non-empty. A floating-CA derivation with ZERO inputs doesn't
/// need resolve (nothing to collapse into `inputSrcs`); the caller
/// ([`maybe_resolve_ca`] in `dispatch.rs`) short-circuits before
/// reaching here.
///
/// ## Why this returns `lookups` instead of inserting itself
///
/// Separating the pure resolution from the DB side-effect lets the
/// caller batch inserts across multiple dispatches, wrap in a
/// transaction with the assignment insert, or skip the insert in
/// tests. The pure-function shape also makes the test (`T3`)
/// assert on the rewrite logic without a PG fixture.
///
/// [`maybe_resolve_ca`]: crate::actor::DagActor
#[instrument(skip_all, fields(n_ca_inputs = ca_inputs.len(), n_ia_inputs = ia_inputs.len()))]
pub async fn resolve_ca_inputs(
    drv_content: &[u8],
    ca_inputs: &[CaResolveInput],
    ia_inputs: &[IaResolveInput],
    pool: &PgPool,
) -> Result<ResolvedDerivation, ResolveError> {
    if drv_content.is_empty() {
        return Err(ResolveError::NoDrvContent);
    }

    // Parse the full Derivation (with inputDrvs). Needed up-front:
    // the IA-input loop below iterates `drv.input_drvs()` to decide
    // which output names the parent actually wants per IA input.
    // Also acts as a validity check — if the ATerm is malformed we
    // fail before hitting PG.
    let drv_text = std::str::from_utf8(drv_content)?;
    let drv = Derivation::parse(drv_text)?;

    // Build the rewrite map: placeholder string → realized path.
    // Also collect lookups for the realisation_deps side-effect.
    let mut rewrites: BTreeMap<String, String> = BTreeMap::new();
    let mut lookups: Vec<RealisationLookup> = Vec::new();
    let mut new_input_srcs: Vec<String> = Vec::new();

    for input in ca_inputs {
        let input_path = StorePath::parse(&input.drv_path)?;

        for output_name in &input.output_names {
            // Step 2 of ADR-018 Appendix B: query realisations.
            let realized = query_realisation(pool, &input.modular_hash, output_name)
                .await?
                .ok_or_else(|| ResolveError::RealisationMissing {
                    drv_path: input.drv_path.clone(),
                    output_name: output_name.clone(),
                    modular_hex: hex::encode(input.modular_hash),
                })?;

            // Step 3: insert into inputSrcs.
            new_input_srcs.push(realized.clone());

            // Step 4: placeholder → realized path rewrite.
            let placeholder = downstream_placeholder(&input_path, output_name);
            rewrites.insert(placeholder, realized.clone());

            // Step 5 (caller's side-effect): record the dependency
            // edge for realisation_deps.
            lookups.push(RealisationLookup {
                dep_modular_hash: input.modular_hash,
                dep_output_name: output_name.clone(),
                realized_path: realized,
            });
        }
    }

    // IA-input half: for each inputDrv NOT in `ca_inputs`, look up
    // its output paths in `ia_inputs` (pre-collected from the DAG's
    // `expected_output_paths`). These are deterministic — the
    // gateway computed them at submit time from the parsed `.drv`
    // and plumbed them via `DerivationNode.expected_output_paths`.
    // No store RPC needed.
    //
    // Nix's `tryResolveInput` (derivations.cc:1206-1234) iterates
    // every inputDrv regardless of addressing mode, adding each
    // output path to `inputSrcs`. IA outputs are concrete; CA
    // outputs come from realisations. The CA loop above handles the
    // CA half; this loop closes the IA half.
    let ca_drv_paths: HashSet<&str> = ca_inputs.iter().map(|c| c.drv_path.as_str()).collect();
    let ia_by_path: HashMap<&str, &IaResolveInput> = ia_inputs
        .iter()
        .map(|ia| (ia.drv_path.as_str(), ia))
        .collect();

    for (input_drv_path, wanted_names) in drv.input_drvs() {
        if ca_drv_paths.contains(input_drv_path.as_str()) {
            // CA — already handled above via realisation lookup.
            continue;
        }
        let Some(ia) = ia_by_path.get(input_drv_path.as_str()) else {
            // Parent references an IA input not in `ia_inputs` — the
            // submission didn't include this transitive dep in the
            // DAG slice, or it was a recovered node with
            // `expected_output_paths` not persisted. Unusual but
            // not an error: the worker's FUSE layer will
            // on-demand-fetch. Log at debug, skip (preserves
            // pre-existing behavior for this edge).
            debug!(
                input_drv_path = %input_drv_path,
                "IA input not in DAG slice — skipping inputSrcs add"
            );
            continue;
        };
        // `output_names` and `output_paths` are index-paired. Collect
        // only the paths for output names the parent actually wants
        // (the `inputDrvs` value set). Most IA derivations have a
        // single `"out"` so this is a 1:1 pass; multi-output (dev,
        // doc, lib) get filtered here.
        for (i, name) in ia.output_names.iter().enumerate() {
            if wanted_names.contains(name)
                && let Some(path) = ia.output_paths.get(i)
            {
                new_input_srcs.push(path.clone());
            }
        }
    }

    // Apply rewrites as a global string-replace through the whole
    // ATerm — matches Nix's `rewriteDerivation` (which string-
    // replaces through builder/args/env without parsing). Doing it
    // on the serialized form (not the parsed struct) guarantees we
    // catch ALL placeholder occurrences, including any in output
    // paths or env VALUES that the struct accessors wouldn't expose.
    //
    // BTreeMap iteration is sorted by placeholder string. Since
    // placeholders are 53-char hashed strings, no placeholder is a
    // substring of another — replacement order is safe.
    let mut rewritten = drv_text.to_string();
    for (placeholder, realized) in &rewrites {
        rewritten = rewritten.replace(placeholder, realized);
    }

    // Re-parse the rewritten ATerm so we can drop inputDrvs and
    // merge the realized paths into inputSrcs. The string-replace
    // above only touched placeholder occurrences in env/args/builder;
    // it did NOT touch the inputDrvs list (which is keyed by .drv
    // paths, not placeholders).
    let rewritten_drv = Derivation::parse(&rewritten)?;

    // Build the resolved Derivation. Per ADR-018 Appendix B step 3,
    // `BasicDerivation resolved{*this}` slice-copy drops `inputDrvs`
    // entirely (it's a Derivation-only field) — ALL entries, CA and
    // IA alike. `inputSrcs` ← old inputSrcs ∪ every input's output
    // path (CA realized + IA expected, both collected above).

    let n_input_srcs_added = new_input_srcs.len();

    // Serialize with inputDrvs unconditionally empty and inputSrcs
    // expanded with both CA realized paths and IA expected paths.
    let resolved_aterm = serialize_resolved(&rewritten_drv, new_input_srcs);

    debug!(
        n_rewrites = rewrites.len(),
        n_lookups = lookups.len(),
        n_input_srcs_added,
        "CA resolve complete"
    );

    Ok(ResolvedDerivation {
        drv_content: resolved_aterm.into_bytes(),
        lookups,
    })
}

/// Insert dependency edges into `realisation_deps` for a resolved
/// derivation.
///
/// `parent_modular_hash` / `parent_output_names` identify the
/// **dependent** (the derivation being resolved); `lookups` holds
/// the **dependencies** (the CA inputs whose realisations were
/// consumed). One row per (parent_output, dep_output) pair.
///
/// Idempotent: `ON CONFLICT DO NOTHING` on the 4-column composite PK.
/// Same dispatch retrying after a DB blip doesn't duplicate rows.
///
/// The FK to `realisations` means BOTH sides must already exist in
/// the `realisations` table before this insert. The dependency side
/// is guaranteed (we just queried it); the parent side is NOT yet
/// registered (it hasn't been built yet — we're at dispatch time).
/// So this insert is **deferred**: caller records `lookups` and
/// inserts at COMPLETION time, after `wopRegisterDrvOutput` lands
/// the parent's realisation.
///
/// Wired into `handle_success_completion` (completion.rs) AFTER the
/// `r[sched.ca.cutoff-compare]` / cutoff-propagate hooks — the
/// parent's realisation row lands via `wopRegisterDrvOutput` before
/// completion fires, so the FK is satisfied by the time this runs.
#[instrument(skip_all, fields(
    parent = hex::encode(parent_modular_hash),
    n_outputs = parent_output_names.len(),
    n_deps = lookups.len()
))]
pub async fn insert_realisation_deps(
    pool: &PgPool,
    parent_modular_hash: &[u8; 32],
    parent_output_names: &[String],
    lookups: &[RealisationLookup],
) -> Result<u64, sqlx::Error> {
    // Build the flat (drv_hash, output_name, dep_drv_hash,
    // dep_output_name) rows. For a parent with M outputs and N dep
    // lookups, that's M×N rows — each of the parent's outputs
    // depends on all the dep realisations that were consumed during
    // resolve. This matches Nix's model: the resolve step is
    // per-derivation, not per-output; all outputs share the same
    // dependency closure.
    //
    // UNNEST-based batch insert keeps it one round-trip. For a
    // typical 1-output parent with 3 CA inputs, that's 3 rows.
    let mut drv_hash_col: Vec<&[u8]> = Vec::new();
    let mut output_name_col: Vec<&str> = Vec::new();
    let mut dep_drv_hash_col: Vec<&[u8]> = Vec::new();
    let mut dep_output_name_col: Vec<&str> = Vec::new();

    for parent_out in parent_output_names {
        for dep in lookups {
            drv_hash_col.push(parent_modular_hash.as_slice());
            output_name_col.push(parent_out.as_str());
            dep_drv_hash_col.push(dep.dep_modular_hash.as_slice());
            dep_output_name_col.push(dep.dep_output_name.as_str());
        }
    }

    if drv_hash_col.is_empty() {
        return Ok(0);
    }

    let result = sqlx::query(
        r#"
        INSERT INTO realisation_deps
            (drv_hash, output_name, dep_drv_hash, dep_output_name)
        SELECT * FROM UNNEST($1::bytea[], $2::text[], $3::bytea[], $4::text[])
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(&drv_hash_col)
    .bind(&output_name_col)
    .bind(&dep_drv_hash_col)
    .bind(&dep_output_name_col)
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// Query the realisations table for one (modular_hash, output_name).
///
/// Returns `None` for a cache miss (realisation not registered).
/// This is the same query as `rio_store::realisations::query` but
/// local to the scheduler — the scheduler accesses `realisations`
/// directly (both crates share the PG pool and migrations), not via
/// the store gRPC. ADR-018 §3: "resolution logic belongs in the
/// scheduler."
///
/// **Schema coupling:** the store-side module is `pub(crate)` (see
/// `rio-store/src/lib.rs`), so this duplication is intentional, not a
/// visibility bug. The store uses validated `[u8;32]` types; here we
/// accept raw slices. Any migration touching the `realisations` table
/// MUST update both query sites or risk silent drift.
pub(crate) async fn query_realisation(
    pool: &PgPool,
    modular_hash: &[u8; 32],
    output_name: &str,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT output_path FROM realisations WHERE drv_hash = $1 AND output_name = $2",
    )
    .bind(modular_hash.as_slice())
    .bind(output_name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(p,)| p))
}

/// A prior realisation: some OTHER derivation that produced the same
/// output_path. Used by CA cutoff to detect "this content existed
/// before this build" — for CA, same content → same path, so finding
/// a DIFFERENT modular_hash pointing to the same path proves a prior
/// build landed it.
#[derive(Debug, Clone)]
pub struct PriorRealisation {
    /// Modular hash of the prior derivation (different from current).
    pub drv_hash: [u8; 32],
    /// Output name (usually `"out"`).
    pub output_name: String,
    /// Realized store path (same as current — CA content-addressing).
    pub output_path: String,
    /// NAR hash of the output (same as current — same content).
    pub output_hash: [u8; 32],
}

/// Find a prior realisation for `output_path` with a DIFFERENT
/// modular_hash. The CA cutoff trigger check.
///
/// For CA derivations, the output_path is a function of content: two
/// builds producing identical bytes get identical store paths. If a
/// PRIOR build (different modular_hash — different drv, same content)
/// already registered a realisation for this path, the content
/// existed before → downstream can be skipped.
///
/// Contrast with `ContentLookup(nar_hash, exclude=path)` — that was
/// the previous trigger check, and it's broken for CA: since both
/// builds produce the SAME path, the self-exclusion filters out the
/// only matching row. The realisation-based check excludes by
/// modular_hash instead (different across builds with different drv
/// envs, even if content matches).
///
/// Uses `realisations_output_idx` (migration 002). Returns the FIRST
/// match — if multiple prior builds exist, any one proves existence.
// r[impl sched.ca.cutoff-compare]
#[instrument(skip(pool, exclude_modular_hash))]
pub async fn query_prior_realisation(
    pool: &PgPool,
    output_path: &str,
    exclude_modular_hash: &[u8; 32],
) -> Result<Option<PriorRealisation>, sqlx::Error> {
    let row: Option<(Vec<u8>, String, String, Vec<u8>)> = sqlx::query_as(
        "SELECT drv_hash, output_name, output_path, output_hash \
         FROM realisations \
         WHERE output_path = $1 AND drv_hash != $2 \
         LIMIT 1",
    )
    .bind(output_path)
    .bind(exclude_modular_hash.as_slice())
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(h, name, path, oh)| {
        Some(PriorRealisation {
            drv_hash: h.as_slice().try_into().ok()?,
            output_name: name,
            output_path: path,
            output_hash: oh.as_slice().try_into().ok()?,
        })
    }))
}

/// `(modular_hash, output_name)` composite key — the realisations PK.
pub type RealisationKey = (Vec<u8>, String);

/// `(output_path, output_hash)` — the realisation's payload.
pub type RealisationOutput = (String, [u8; 32]);

/// Walk `realisation_deps` transitively from a seed set, collecting
/// all dependent realisations (the reverse direction: "who was built
/// using these as inputs?").
///
/// The CA cutoff cascade: given a trigger's PRIOR realisation (found
/// via [`query_prior_realisation`]), walk the dependency graph to
/// discover what was previously built downstream. Those prior outputs
/// are the candidate paths for skip-verification.
///
/// Uses `realisation_deps_reverse_idx` (migration 015, explicitly
/// indexed "for cutoff cascade"). Bounded at `max_nodes` to match
/// the in-mem DAG's `MAX_CASCADE_NODES` — a runaway walk on a
/// pathological dependency graph shouldn't hang the actor loop.
///
/// Returns `(drv_hash, output_name) → (output_path, output_hash)`.
/// The caller matches these against the current DAG's cascade
/// candidates by name-suffix (output_path ends with `-{pname}`).
// r[impl sched.ca.cutoff-propagate]
#[instrument(skip(pool, seeds))]
pub async fn walk_dependent_realisations(
    pool: &PgPool,
    seeds: &[RealisationKey],
    max_nodes: usize,
) -> Result<HashMap<RealisationKey, RealisationOutput>, sqlx::Error> {
    let mut found: HashMap<RealisationKey, RealisationOutput> = HashMap::new();
    let mut frontier: Vec<RealisationKey> = seeds.to_vec();
    let mut visited: HashSet<RealisationKey> = seeds.iter().cloned().collect();

    while let Some((dep_hash, dep_name)) = frontier.pop() {
        if found.len() >= max_nodes {
            break;
        }
        // Join realisation_deps (reverse) → realisations to get
        // dependent's output_path in one round-trip.
        let rows: Vec<(Vec<u8>, String, String, Vec<u8>)> = sqlx::query_as(
            "SELECT rd.drv_hash, rd.output_name, r.output_path, r.output_hash \
             FROM realisation_deps rd \
             JOIN realisations r \
               ON r.drv_hash = rd.drv_hash AND r.output_name = rd.output_name \
             WHERE rd.dep_drv_hash = $1 AND rd.dep_output_name = $2",
        )
        .bind(&dep_hash)
        .bind(&dep_name)
        .fetch_all(pool)
        .await?;
        for (h, n, path, oh) in rows {
            let key = (h, n);
            if visited.insert(key.clone()) {
                let Ok(oh32) = oh.as_slice().try_into() else {
                    warn!(
                        output_hash_len = oh.len(),
                        "corrupt realisation hash in PG row — skipping"
                    );
                    continue;
                };
                found.insert(key.clone(), (path, oh32));
                frontier.push(key);
            }
        }
    }
    Ok(found)
}

/// Insert a realisation row. Idempotent (`ON CONFLICT DO NOTHING`).
///
/// Scheduler-side mirror of `rio_store::realisations::insert` — the
/// scheduler writes directly to PG (both crates share the pool and
/// migrations; ADR-018 §3 "resolution logic belongs in the scheduler").
///
/// Called from `handle_success_completion` when a CA build finishes:
/// the worker's `CompletionReport.built_outputs` carries
/// `(output_name, output_path, output_hash)`; the scheduler has
/// `ca_modular_hash` from the DAG state. Together they form the full
/// realisation row — no extra RPC needed.
///
/// **Why the scheduler inserts** (not the worker): the gateway's
/// `wopRegisterDrvOutput` handler inserts realisations for the Nix
/// wire-protocol path (client → gateway → store). But the rio-builder
/// speaks gRPC, not wire protocol — its upload flow is `PutPath →
/// CompletionReport`, with no RegisterRealisation call. Without this
/// insert, a CA-on-CA chain built entirely by rio-builders never lands
/// a realisation row, so the next dispatch's `query_realisation`
/// returns `None` → `RealisationMissing` → dispatch-unresolved →
/// worker crashes on the empty-string output path from the floating-
/// CA input's `.drv`. Observed: 9748 retry events in ~10min before
/// this fix (InfrastructureFailure has no backoff).
///
/// `signatures` is empty: the scheduler doesn't sign (phase 5
/// concern). The gateway path populates signatures from the wire
/// JSON; a rio-builder-built realisation is unsigned until a later
/// signing pass (or never, for private deployments).
#[instrument(skip(pool), fields(drv_hash = hex::encode(modular_hash), output = %output_name))]
pub async fn insert_realisation(
    pool: &PgPool,
    modular_hash: &[u8; 32],
    output_name: &str,
    output_path: &str,
    output_hash: &[u8; 32],
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        r#"
        INSERT INTO realisations (drv_hash, output_name, output_path, output_hash, signatures)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (drv_hash, output_name) DO NOTHING
        "#,
    )
    .bind(modular_hash.as_slice())
    .bind(output_name)
    .bind(output_path)
    .bind(output_hash.as_slice())
    .bind::<&[String]>(&[])
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Serialize a resolved derivation back to ATerm with `inputDrvs`
/// unconditionally emptied and `extra_input_srcs` merged into the
/// existing `inputSrcs` set.
///
/// Per Nix's `tryResolve` (`derivations.cc:1204`), the resolved
/// derivation is a `BasicDerivation` — `inputDrvs` is NOT a
/// `BasicDerivation` field, so the slice-copy
/// `BasicDerivation resolved{*this}` drops it entirely. Both CA AND
/// IA entries are gone. ADR-018 Appendix B step 3.
fn serialize_resolved(
    drv: &Derivation,
    extra_input_srcs: impl IntoIterator<Item = String>,
) -> String {
    BasicDerivation::from_resolved(drv, extra_input_srcs).to_aterm()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    // r[verify sched.ca.resolve+2]
    /// Golden value from upstream Nix's own unit test
    /// (`nix/src/libstore-tests/downstream-placeholder.cc:16-20`):
    ///
    /// ```cpp
    /// StorePath{"g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo.drv"}, "out"
    /// → "/0c6rn30q4frawknapgwq386zq358m8r6msvywcvc89n6m5p2dgbz"
    /// ```
    ///
    /// This catches nixbase32 bit-order divergence, SHA-256 input-
    /// encoding mismatches, and `.drv`-suffix-stripping bugs that
    /// the shape tests (length/alphabet) miss. The shape tests prove
    /// "looks like a placeholder"; this proves "IS the placeholder
    /// Nix would compute".
    ///
    /// If this FAILS: the divergence is in one of
    ///   - `nixbase32::encode` byte-order (rio vs Nix)
    ///   - `hash_part()` (whether it includes store-dir prefix)
    ///   - the cleartext format string (`nix-upstream-output:{hash}:{outputPathName}`)
    ///   - `.drv` suffix-stripping on the name
    ///
    /// The test IS the debugging probe: the golden value is known-
    /// correct, so diff each component against what Nix computes.
    #[test]
    fn placeholder_golden_matches_nix_upstream() {
        let drv_path =
            StorePath::parse("/nix/store/g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo.drv").unwrap();
        let p = downstream_placeholder(&drv_path, "out");
        assert_eq!(
            p, "/0c6rn30q4frawknapgwq386zq358m8r6msvywcvc89n6m5p2dgbz",
            "must match upstream Nix golden value \
             (libstore-tests/downstream-placeholder.cc:16-20)"
        );
    }

    /// Compute a placeholder and assert the shape: `/` + 52 nixbase32
    /// chars. Matches the `"/1ril1qzj..."` example in ADR-018
    /// Appendix B.
    #[test]
    fn placeholder_shape() {
        let drv_path =
            StorePath::parse("/nix/store/00000000000000000000000000000000-foo.drv").unwrap();
        let p = downstream_placeholder(&drv_path, "out");
        assert!(p.starts_with('/'), "placeholder must start with /");
        assert_eq!(p.len(), 53, "/ + 52-char nixbase32 of SHA-256");
        // nixbase32 alphabet is all lowercase alnum minus e/o/t/u.
        for c in p[1..].chars() {
            assert!(
                "0123456789abcdfghijklmnpqrsvwxyz".contains(c),
                "{c:?} not in nixbase32 alphabet"
            );
        }
    }

    /// `outputPathName` semantics: `out` → bare drvName; anything
    /// else → `drvName-outputName`. Distinct placeholder per output.
    #[test]
    fn placeholder_output_name_matters() {
        let drv_path =
            StorePath::parse("/nix/store/00000000000000000000000000000000-multi.drv").unwrap();
        let p_out = downstream_placeholder(&drv_path, "out");
        let p_dev = downstream_placeholder(&drv_path, "dev");
        assert_ne!(p_out, p_dev, "different outputs → different placeholders");
    }

    /// The `.drv` suffix is stripped before outputPathName
    /// computation — `foo.drv` and `foo` (not a .drv) produce
    /// DIFFERENT placeholders because the hash_part differs.
    #[test]
    fn placeholder_strips_drv_suffix() {
        // Two paths with SAME hash part but name "foo.drv" vs "foo"
        // would produce the same placeholder for "out" iff the .drv
        // suffix is stripped (making outputPathName identical). But
        // since hash_part is also in the cleartext, we can't easily
        // construct that. Instead verify the stripping directly:
        let drv_path =
            StorePath::parse("/nix/store/11111111111111111111111111111111-bar.drv").unwrap();
        // The cleartext uses "bar" (stripped), not "bar.drv".
        let p = downstream_placeholder(&drv_path, "out");
        // Recompute manually with the expected cleartext shape.
        let expected_clear = format!(
            "nix-upstream-output:{}:bar",
            "11111111111111111111111111111111"
        );
        let expected_hash: [u8; 32] = Sha256::digest(expected_clear.as_bytes()).into();
        let expected = format!("/{}", nixbase32::encode(&expected_hash));
        assert_eq!(p, expected);
    }

    // r[verify sched.ca.resolve+2]
    /// CA drv with one CA inputDrv. Mock realisations table returns
    /// the realized path. Assert resolved drv has the realized path
    /// in inputSrcs, placeholder is gone from env, and the
    /// `realisation_deps` INSERT side-effect stages exactly one
    /// dependency edge.
    #[tokio::test]
    async fn resolve_rewrites_ca_input_paths() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Set up: the CA input's realisation. One output "out" with a
        // known realized path. modular_hash is an arbitrary 32-byte
        // value — in production this comes from the gateway's
        // hash_derivation_modulo call.
        let input_modular: [u8; 32] = [0x11; 32];
        let realized_path = "/nix/store/22222222222222222222222222222222-dep-out";
        sqlx::query(
            "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
             VALUES ($1, 'out', $2, $3)",
        )
        .bind(input_modular.as_slice())
        .bind(realized_path)
        .bind([0x33u8; 32].as_slice())
        .execute(&db.pool)
        .await?;

        // The input's .drv store path — what the PARENT references in
        // its inputDrvs.
        let input_drv_path = "/nix/store/44444444444444444444444444444444-dep.drv";
        let input_sp = StorePath::parse(input_drv_path)?;
        let placeholder = downstream_placeholder(&input_sp, "out");

        // Build the parent CA derivation's ATerm. It references the
        // input via inputDrvs and embeds the placeholder in its env
        // (where the input's output path would be).
        // Outputs: floating-CA ("sha256" hash_algo, empty hash,
        // empty path — Nix leaves CA output paths empty pre-build).
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{input_drv_path}",["out"])],["/nix/store/55555555555555555555555555555555-fixed-src"],"x86_64-linux","/bin/sh",["-c","build"],[("DEP","{placeholder}"),("name","parent"),("out",""),("system","x86_64-linux")])"#
        );

        let ca_inputs = vec![CaResolveInput {
            drv_path: input_drv_path.into(),
            modular_hash: input_modular,
            output_names: vec!["out".into()],
        }];

        let resolved =
            resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &[], &db.pool).await?;

        // --- Assert the rewrite ---
        let resolved_text = std::str::from_utf8(&resolved.drv_content)?;
        let resolved_drv = Derivation::parse(resolved_text)?;

        // Placeholder is gone from env.DEP; realized path is there.
        assert_eq!(
            resolved_drv.env().get("DEP").map(String::as_str),
            Some(realized_path),
            "placeholder should be replaced by realized path in env"
        );
        assert!(
            !resolved_text.contains(&placeholder),
            "placeholder should be fully gone from resolved ATerm"
        );

        // Realized path is in inputSrcs; original fixed-src still there.
        assert!(
            resolved_drv.input_srcs().contains(realized_path),
            "realized path should be in inputSrcs"
        );
        assert!(
            resolved_drv
                .input_srcs()
                .contains("/nix/store/55555555555555555555555555555555-fixed-src"),
            "original inputSrcs should be preserved"
        );

        // ALL inputDrvs are dropped — resolved derivation is a
        // BasicDerivation.
        assert!(
            resolved_drv.input_drvs().is_empty(),
            "resolved drv MUST have empty inputDrvs"
        );

        // --- Assert the realisation_deps side-effect lookup ---
        assert_eq!(resolved.lookups.len(), 1, "exactly one dependency lookup");
        assert_eq!(resolved.lookups[0].dep_modular_hash, input_modular);
        assert_eq!(resolved.lookups[0].dep_output_name, "out");
        assert_eq!(resolved.lookups[0].realized_path, realized_path);

        // --- Assert the realisation_deps INSERT works ---
        // The parent must ALSO be registered in realisations first
        // (FK constraint). Simulate the parent's wopRegisterDrvOutput.
        let parent_modular: [u8; 32] = [0xAA; 32];
        sqlx::query(
            "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
             VALUES ($1, 'out', '/nix/store/parent-out', $2)",
        )
        .bind(parent_modular.as_slice())
        .bind([0xBBu8; 32].as_slice())
        .execute(&db.pool)
        .await?;

        let n = insert_realisation_deps(
            &db.pool,
            &parent_modular,
            &["out".into()],
            &resolved.lookups,
        )
        .await?;
        assert_eq!(n, 1, "exactly one realisation_deps row inserted");

        // Verify the row landed with the right values.
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM realisation_deps
             WHERE drv_hash = $1 AND output_name = 'out'
               AND dep_drv_hash = $2 AND dep_output_name = 'out'",
        )
        .bind(parent_modular.as_slice())
        .bind(input_modular.as_slice())
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(count, 1);

        // Idempotency: re-insert is a no-op (ON CONFLICT DO NOTHING).
        let n2 = insert_realisation_deps(
            &db.pool,
            &parent_modular,
            &["out".into()],
            &resolved.lookups,
        )
        .await?;
        assert_eq!(n2, 0, "duplicate insert should be a no-op");

        Ok(())
    }

    /// Missing realisation → `RealisationMissing` error. The parent
    /// should be deferred and redispatched after the input's
    /// realisation lands.
    #[tokio::test]
    async fn resolve_missing_realisation_errors() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let input_drv_path = "/nix/store/66666666666666666666666666666666-gone.drv";
        let placeholder = downstream_placeholder(&StorePath::parse(input_drv_path)?, "out");
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{input_drv_path}",["out"])],[],"x86_64-linux","/bin/sh",[],[("DEP","{placeholder}")])"#
        );

        let ca_inputs = vec![CaResolveInput {
            drv_path: input_drv_path.into(),
            modular_hash: [0x77; 32], // Not in realisations table.
            output_names: vec!["out".into()],
        }];

        let err = resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &[], &db.pool)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ResolveError::RealisationMissing { .. }),
            "expected RealisationMissing, got {err:?}"
        );
        Ok(())
    }

    /// Empty `ca_inputs` + empty `ia_inputs` → still serializes as a
    /// `BasicDerivation` (inputDrvs stripped). Degenerate case: the
    /// caller ([`maybe_resolve_ca`]) short-circuits before reaching
    /// here when both are empty, so this documents that the function
    /// handles the edge gracefully rather than panicking.
    ///
    /// [`maybe_resolve_ca`]: crate::actor::DagActor
    #[tokio::test]
    async fn resolve_empty_inputs_still_strips_inputdrvs() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // ATerm with one IA inputDrv and no srcs. After resolve,
        // inputDrvs MUST be `[]` (BasicDerivation form) even though
        // neither ca_inputs nor ia_inputs provided anything.
        let ia_drv = "/nix/store/99999999999999999999999999999999-ia.drv";
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{ia_drv}",["out"])],[],"x86_64-linux","/bin/sh",[],[("name","leaf")])"#
        );
        let resolved = resolve_ca_inputs(parent_aterm.as_bytes(), &[], &[], &db.pool).await?;
        let reparsed = Derivation::parse(std::str::from_utf8(&resolved.drv_content)?)?;
        assert!(
            reparsed.input_drvs().is_empty(),
            "resolved drv MUST have empty inputDrvs (BasicDerivation form)"
        );
        assert!(resolved.lookups.is_empty(), "no CA lookups");
        Ok(())
    }

    // r[verify sched.ca.resolve+2]
    /// `serialize_resolved` drops ALL `inputDrvs` (CA and IA alike)
    /// and merges `inputSrcs` in sorted order — matching Nix's
    /// `BasicDerivation resolved{*this}` slice-copy semantics
    /// (`derivations.cc:1204`, ADR-018 Appendix B step 3).
    ///
    /// This test exercises `serialize_resolved` directly (not the
    /// full `resolve_ca_inputs`). The caller is responsible for
    /// passing both CA realized paths AND IA expected paths via
    /// `extra_input_srcs`; serialize_resolved merges them all.
    #[test]
    fn serialize_resolved_drops_all_inputdrvs() -> anyhow::Result<()> {
        // Parent with TWO inputDrvs: one CA, one IA. Both must be
        // dropped. Both the realized CA path AND the IA expected
        // path land in inputSrcs (caller passes both).
        let aterm = r#"Derive([("out","","sha256","")],[("/nix/store/aaa-ca.drv",["out"]),("/nix/store/bbb-ia.drv",["out"])],["/nix/store/zzz-src"],"x86_64-linux","/bin/sh",[],[("name","parent")])"#;
        let drv = Derivation::parse(aterm)?;

        // Caller-collected: realized CA path + IA expected path.
        let resolved = serialize_resolved(
            &drv,
            ["/nix/store/ccc-realized", "/nix/store/ddd-ia-out"].map(String::from),
        );

        let reparsed = Derivation::parse(&resolved)?;
        // ALL inputDrvs dropped — resolved derivation is a
        // BasicDerivation; inputDrvs is not a BasicDerivation field.
        assert!(
            reparsed.input_drvs().is_empty(),
            "resolved drv MUST have empty inputDrvs (BasicDerivation slice)"
        );
        // inputSrcs is the sorted union: ccc < ddd < zzz.
        let srcs: Vec<&str> = reparsed.input_srcs().iter().map(String::as_str).collect();
        assert_eq!(
            srcs,
            vec![
                "/nix/store/ccc-realized",
                "/nix/store/ddd-ia-out",
                "/nix/store/zzz-src"
            ]
        );
        Ok(())
    }

    /// Positive CA-only case: one CA input, no IA inputs. Placeholder
    /// replacement + realized path in `inputSrcs` + empty `inputDrvs`.
    /// This is the minimal `tryResolve` shape.
    #[test]
    fn serialize_resolved_ca_only() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","","sha256","")],[("/nix/store/aaa-ca.drv",["out"])],["/nix/store/orig-src"],"x86_64-linux","/bin/sh",[],[("name","parent")])"#;
        let drv = Derivation::parse(aterm)?;

        let resolved = serialize_resolved(&drv, ["/nix/store/realized-ca".to_string()]);

        let reparsed = Derivation::parse(&resolved)?;
        assert!(reparsed.input_drvs().is_empty());
        assert!(reparsed.input_srcs().contains("/nix/store/realized-ca"));
        assert!(reparsed.input_srcs().contains("/nix/store/orig-src"));
        Ok(())
    }

    // r[verify sched.ca.resolve+2]
    /// End-to-end CA+IA resolve: parent with one CA input and one IA
    /// input. After `resolve_ca_inputs`:
    ///
    /// - `inputDrvs = []` (P0398's BasicDerivation semantics)
    /// - `inputSrcs = {orig_srcs, realized_ca, ia_expected_path}`
    ///
    /// The IA path comes from `ia_inputs` (pre-collected from the
    /// DAG's `expected_output_paths`, no store RPC) — proves the IA
    /// lookup is wired and filters by output name correctly.
    #[tokio::test]
    async fn serialize_resolved_includes_ia_output_paths_in_inputsrcs() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // --- CA input: realisation in PG ---
        let ca_modular: [u8; 32] = [0x11; 32];
        let ca_realized = "/nix/store/22222222222222222222222222222222-ca-out";
        sqlx::query(
            "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
             VALUES ($1, 'out', $2, $3)",
        )
        .bind(ca_modular.as_slice())
        .bind(ca_realized)
        .bind([0x33u8; 32].as_slice())
        .execute(&db.pool)
        .await?;
        let ca_drv = "/nix/store/44444444444444444444444444444444-ca.drv";
        let ca_sp = StorePath::parse(ca_drv)?;
        let placeholder = downstream_placeholder(&ca_sp, "out");

        // --- IA input: just IaResolveInput, no PG needed ---
        let ia_drv = "/nix/store/88888888888888888888888888888888-ia.drv";
        let ia_out = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ia-out";
        let ia_dev = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-ia-dev";

        // --- Parent ATerm: one CA inputDrv (wants "out") + one IA
        //     inputDrv (wants "out" only, NOT "dev") ---
        let orig_src = "/nix/store/55555555555555555555555555555555-src";
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{ca_drv}",["out"]),("{ia_drv}",["out"])],["{orig_src}"],"x86_64-linux","/bin/sh",["-c","build"],[("DEP","{placeholder}"),("name","parent"),("out",""),("system","x86_64-linux")])"#
        );

        let ca_inputs = vec![CaResolveInput {
            drv_path: ca_drv.into(),
            modular_hash: ca_modular,
            output_names: vec!["out".into()],
        }];
        let ia_inputs = vec![IaResolveInput {
            drv_path: ia_drv.into(),
            // Multi-output IA — index-paired. Parent only wants "out"
            // so only ia_out should land in inputSrcs, NOT ia_dev.
            output_names: vec!["out".into(), "dev".into()],
            output_paths: vec![ia_out.into(), ia_dev.into()],
        }];

        let resolved =
            resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &ia_inputs, &db.pool).await?;

        let reparsed = Derivation::parse(std::str::from_utf8(&resolved.drv_content)?)?;

        // inputDrvs stripped (P0398).
        assert!(reparsed.input_drvs().is_empty());

        // inputSrcs = {orig, realized-ca, ia-out}. NOT ia-dev
        // (parent didn't ask for "dev").
        let srcs = reparsed.input_srcs();
        assert!(srcs.contains(orig_src), "original inputSrcs preserved");
        assert!(srcs.contains(ca_realized), "CA realized path in inputSrcs");
        assert!(
            srcs.contains(ia_out),
            "IA expected path (out) in inputSrcs — proves DAG lookup wired"
        );
        assert!(
            !srcs.contains(ia_dev),
            "IA 'dev' path NOT in inputSrcs — parent only wants 'out'"
        );

        // CA placeholder replaced.
        assert_eq!(
            reparsed.env().get("DEP").map(String::as_str),
            Some(ca_realized)
        );

        // Realisation lookups: exactly one (the CA input). IA inputs
        // don't produce lookups (no realisation query).
        assert_eq!(resolved.lookups.len(), 1);
        assert_eq!(resolved.lookups[0].dep_modular_hash, ca_modular);

        Ok(())
    }

    /// IA input not in `ia_inputs` (DAG didn't have the node) →
    /// resolve still succeeds, logs at debug, skips the `inputSrcs`
    /// add for that input. Preserves pre-existing behavior for this
    /// edge (the worker's FUSE layer on-demand-fetches regardless).
    #[tokio::test]
    async fn resolve_ia_input_not_in_dag_skips_gracefully() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Parent ATerm: one IA inputDrv. ia_inputs is EMPTY
        // (simulates DAG slice not including this dep).
        let ia_drv = "/nix/store/88888888888888888888888888888888-gone.drv";
        let orig_src = "/nix/store/55555555555555555555555555555555-src";
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{ia_drv}",["out"])],["{orig_src}"],"x86_64-linux","/bin/sh",[],[("name","parent")])"#
        );

        // No CA inputs either — but function still succeeds and
        // produces a resolved BasicDerivation form.
        let resolved = resolve_ca_inputs(parent_aterm.as_bytes(), &[], &[], &db.pool).await?;

        let reparsed = Derivation::parse(std::str::from_utf8(&resolved.drv_content)?)?;
        assert!(reparsed.input_drvs().is_empty());
        assert!(reparsed.input_srcs().contains(orig_src));
        // The IA output path is NOT in inputSrcs — DAG didn't have it.
        // Builds still work via FUSE; only resolved-drv-hash compat
        // with Nix is affected for this edge.
        assert_eq!(
            reparsed.input_srcs().len(),
            1,
            "only orig_src; IA path skipped"
        );
        assert!(resolved.lookups.is_empty());
        Ok(())
    }

    // TODO(P0311-T62): golden resolved-ATerm test against Nix's
    //   tryResolve. inputSrcs is now complete (IA output paths via
    //   DAG expected_output_paths), so a byte-for-byte compare
    //   against a Nix-generated resolved .drv is meaningful. The
    //   remaining diff would only be inputSrcs ordering, which
    //   serialize_resolved already sorts canonically. ADR-018
    //   Appendix B captures the transformation but no byte-exact
    //   fixture exists yet. Adjacent golden-fixture work: P0311-T62
    //   (downstream_placeholder golden — landed above as
    //   placeholder_golden_matches_nix_upstream).
}

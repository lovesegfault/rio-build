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
// r[impl sched.ca.resolve+3]

use std::collections::{BTreeMap, HashMap, HashSet};

use rio_nix::derivation::{BasicDerivation, Derivation, DerivationError};
use rio_nix::store_path::{StorePath, StorePathError, nixbase32, output_path_name};
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

    /// DB error from `rio_store::realisations::query`. Same
    /// retry-ability as [`Db`](Self::Db); separate variant only
    /// because rio-store's accessors return the SQLSTATE-classified
    /// [`MetadataError`](rio_store::error::MetadataError) rather than
    /// raw `sqlx::Error`.
    #[error("realisations lookup: {0}")]
    Store(#[from] rio_store::error::MetadataError),

    /// No `drv_content` available. Resolution requires the full
    /// ATerm to parse `inputDrvs` and perform placeholder replacement.
    /// Caller should fetch from store or defer.
    #[error("drv_content is empty — cannot resolve without ATerm")]
    NoDrvContent,
}

/// One CA input to resolve: the `.drv` store path and the modular
/// derivation hash (realisations PK).
///
/// The modular hash must be the 32-byte SHA-256 from
/// `hash_derivation_modulo` — the same value Nix sends as the
/// `sha256:<hex>` prefix of `wopRegisterDrvOutput`'s `id` field.
/// The scheduler does NOT compute this itself; it receives it from
/// the gateway via `DerivationNode.ca.modular_hash` (computed
/// post-BFS from the full drv_cache — see
/// `rio-gateway/src/translate.rs:populate_ca_modular_hashes`).
///
/// **No `output_names` field** — the parent's parsed `inputDrvs`
/// (from the ATerm) is the SOLE source of truth for which output
/// names to resolve. The DAG's per-child `output_names` is the
/// child's full output list, not the parent's wanted-subset; using
/// it here over-resolves (Nix-incompat resolved ATerm) and can
/// spuriously `RealisationMissing` on an output the parent never
/// references.
#[derive(Debug, Clone)]
pub struct CaResolveInput {
    /// Store path of the input `.drv` file. Matches an `inputDrvs`
    /// key in the parent's ATerm exactly.
    pub drv_path: String,
    /// Modular derivation hash (`hashDerivationModulo`). The
    /// `drv_hash` half of the `realisations` composite PK.
    pub modular_hash: [u8; 32],
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
    /// `(output_name, store_path)` for every deferred-IA output whose
    /// path was computed post-resolve via
    /// [`BasicDerivation::fill_deferred_outputs`]. Empty for
    /// floating-CA-self (its outputs stay `""` — nix-daemon computes
    /// scratch paths) and for concrete IA. Dispatch overwrites
    /// `state.expected_output_paths` from this so the HMAC
    /// `expected_outputs` claim carries the real path, not `""`.
    pub output_paths: Vec<(String, String)>,
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

    let cleartext = format!(
        "nix-upstream-output:{}:{}",
        drv_path.hash_part(),
        output_path_name(drv_name, output_name)
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

    // drv_path → modular_hash (CA) and drv_path → IaResolveInput (IA).
    // The parsed ATerm's `inputDrvs` map is the SINGLE source of
    // truth for which outputs to resolve — `ca_inputs`/`ia_inputs`
    // only supply per-child metadata (modular hash, expected paths).
    let ca_by_path: HashMap<&str, [u8; 32]> = ca_inputs
        .iter()
        .map(|c| (c.drv_path.as_str(), c.modular_hash))
        .collect();
    let ia_by_path: HashMap<&str, &IaResolveInput> = ia_inputs
        .iter()
        .map(|ia| (ia.drv_path.as_str(), ia))
        .collect();

    // Pass 1 — collect (modular_hash, output_name) pairs for ONE
    // batched realisations lookup. Nix's `tryResolveInput`
    // (derivations.cc:1206-1234) iterates the parent's `inputDrvs`
    // wanted-subset, NOT the child's full output list — driving
    // from `drv.input_drvs()` here makes over-resolve impossible.
    let mut pairs: Vec<([u8; 32], String)> = Vec::new();
    for (input_drv_path, wanted_names) in drv.input_drvs() {
        if let Some(&mh) = ca_by_path.get(input_drv_path.as_str()) {
            for name in wanted_names {
                pairs.push((mh, name.clone()));
            }
        }
    }

    // Step 2 of ADR-018 Appendix B: query realisations. ONE PG
    // round-trip — the per-pair sequential await was an I-139
    // actor-stall site (N×M serial queries inside the single-
    // threaded actor per CA dispatch).
    let realised: HashMap<([u8; 32], String), String> =
        rio_store::realisations::query_batch(pool, &pairs)
            .await?
            .into_iter()
            .map(|r| ((r.drv_hash, r.output_name), r.output_path))
            .collect();

    // Pass 2 — single walk over `inputDrvs` handles BOTH CA and IA.
    // Nix's `tryResolveInput` iterates every inputDrv regardless of
    // addressing mode, adding each output path to `inputSrcs`. IA
    // outputs are concrete; CA outputs come from realisations.
    for (input_drv_path, wanted_names) in drv.input_drvs() {
        let input_sp = StorePath::parse(input_drv_path)?;
        if let Some(&mh) = ca_by_path.get(input_drv_path.as_str()) {
            for output_name in wanted_names {
                let realized = realised
                    .get(&(mh, output_name.clone()))
                    .ok_or_else(|| ResolveError::RealisationMissing {
                        drv_path: input_drv_path.clone(),
                        output_name: output_name.clone(),
                        modular_hex: hex::encode(mh),
                    })?
                    .clone();
                // Step 3: insert into inputSrcs.
                new_input_srcs.push(realized.clone());
                // Step 4: placeholder → realized path rewrite.
                rewrites.insert(
                    downstream_placeholder(&input_sp, output_name),
                    realized.clone(),
                );
                // Step 5 (caller's side-effect): record the
                // dependency edge for realisation_deps.
                lookups.push(RealisationLookup {
                    dep_modular_hash: mh,
                    dep_output_name: output_name.clone(),
                    realized_path: realized,
                });
            }
        } else if let Some(ia) = ia_by_path.get(input_drv_path.as_str()) {
            // `output_names` and `output_paths` are index-paired.
            // Collect only the paths for output names the parent
            // actually wants (the `inputDrvs` value set). Most IA
            // derivations have a single `"out"` so this is a 1:1
            // pass; multi-output (dev, doc, lib) get filtered here.
            //
            // ALSO record a placeholder→path rewrite. For
            // concrete-IA inputs the placeholder doesn't appear in
            // the parent ATerm (env already has the literal path) so
            // the replace is a no-op. For DEFERRED-IA inputs
            // (IA-on-IA-on-CA chains — ca-chain.nix iaLevels≥2) the
            // parent's env carries the placeholder exactly like a CA
            // input would, and Nix's `tryResolveInput`
            // (`derivations.cc:1221`) performs the same
            // `rewrites.emplace` for every input, CA or not. Skip
            // empty paths (deferred child not yet completed → no
            // rewrite available); unreachable today (children always
            // completed-with-paths or absent from DAG by
            // parent-dispatch time) but defended for parity with the
            // three sibling consumers (assignment.rs, snapshot.rs,
            // translate.rs) that filter `!p.is_empty()`.
            for (i, name) in ia.output_names.iter().enumerate() {
                if wanted_names.contains(name)
                    && let Some(path) = ia.output_paths.get(i)
                    && !path.is_empty()
                {
                    new_input_srcs.push(path.clone());
                    rewrites.insert(downstream_placeholder(&input_sp, name), path.clone());
                }
            }
        } else {
            // Parent references an input not in `ca_inputs` OR
            // `ia_inputs` — the submission didn't include this
            // transitive dep in the DAG slice, or it was a recovered
            // node with `expected_output_paths` not persisted.
            // Unusual but not an error: the worker's FUSE layer will
            // on-demand-fetch. Log at debug, skip (preserves
            // pre-existing behavior for this edge).
            debug!(
                input_drv_path = %input_drv_path,
                "input not in DAG slice — skipping inputSrcs add"
            );
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
    let mut basic = BasicDerivation::from_resolved(&rewritten_drv, new_input_srcs);

    // Phase 2 — own-output path fill for deferred-IA. After inputDrvs
    // collapse, a deferred-IA's `outputs[i].path` and `env[out_name]`
    // are both still `""`; nix-daemon takes that literally → build
    // runs with `$out=""`. Nix's post-tryResolve step rehashes the
    // resolved drv (`hashDerivationModulo(resolved, mask=true)`) and
    // calls `makeOutputPath` per output. `fill_deferred_outputs` does
    // exactly that, leaving floating-CA outputs untouched (nix-daemon
    // computes their scratch paths internally from hash_algo).
    //
    // `drv_name` from env: `derivationStrict` always sets `env.name`.
    // Only consulted when an output is actually deferred-IA, so
    // floating-CA-self resolves with no `name` env still succeed.
    let drv_name = rewritten_drv
        .env()
        .get("name")
        .map(String::as_str)
        .unwrap_or_default();
    let output_paths = basic.fill_deferred_outputs(drv_name)?;

    let resolved_aterm = basic.to_aterm();

    debug!(
        n_rewrites = rewrites.len(),
        n_lookups = lookups.len(),
        n_input_srcs_added,
        n_outputs_filled = output_paths.len(),
        "CA resolve complete"
    );

    Ok(ResolvedDerivation {
        drv_content: resolved_aterm.into_bytes(),
        lookups,
        output_paths,
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
    let row: Option<(Vec<u8>, String)> = sqlx::query_as(
        "SELECT drv_hash, output_name \
         FROM realisations \
         WHERE output_path = $1 AND drv_hash != $2 \
         LIMIT 1",
    )
    .bind(output_path)
    .bind(exclude_modular_hash.as_slice())
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(h, name)| {
        Some(PriorRealisation {
            drv_hash: h.as_slice().try_into().ok()?,
            output_name: name,
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
/// candidates by exact store-path name segment (the portion after
/// the 32-char hash equals `{pname}` / `{pname}-{output}`).
// r[impl sched.ca.cutoff-propagate+2]
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
        // dependent's output_path in one round-trip. LIMIT pushes the
        // bound to PG so a single high-fanout reverse-deps query
        // (glibc, stdenv: 10⁵-10⁶ rows) doesn't `fetch_all` into an
        // unbounded Vec before the inner-loop break runs (bug_130).
        // The function is a best-effort bounded sample, not an
        // exhaustive walk; LIMIT preserves the contract.
        let rows: Vec<(Vec<u8>, String, String, Vec<u8>)> = sqlx::query_as(
            "SELECT rd.drv_hash, rd.output_name, r.output_path, r.output_hash \
             FROM realisation_deps rd \
             JOIN realisations r \
               ON r.drv_hash = rd.drv_hash AND r.output_name = rd.output_name \
             WHERE rd.dep_drv_hash = $1 AND rd.dep_output_name = $2 \
             LIMIT $3",
        )
        .bind(&dep_hash)
        .bind(&dep_name)
        .bind(max_nodes as i64)
        .fetch_all(pool)
        .await?;
        for (h, n, path, oh) in rows {
            if found.len() >= max_nodes {
                // A single high-fanout reverse-deps query (glibc,
                // stdenv) can return ≫max_nodes rows; the outer-loop
                // check alone overshoots by one full fetch. Cap here so
                // the caller's "bounded by MAX_CASCADE_NODES" linear
                // scan (completion.rs) actually holds.
                break;
            }
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
/// a realisation row, so the next dispatch's resolve-time
/// [`query_batch`](rio_store::realisations::query_batch) finds nothing
/// → `RealisationMissing` → dispatch-unresolved →
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

/// Batch [`insert_realisation`]: one UNNEST round-trip for N rows.
///
/// Same `ON CONFLICT DO NOTHING` idempotence and same empty-
/// `signatures` semantics as the single-row variant. Used by
/// `ca_cutoff_cascade` where the per-item loop was N sequential PG
/// awaits inside the single-threaded actor — same N+1 actor-stall
/// class as `persist_status_batch` / `upsert_path_tenants_for_batch`
/// (I-139, "5281 sequential PG awaits → ~20s head-of-line blocking").
// r[impl sched.db.batch-unnest]
#[instrument(skip(pool, rows), fields(n_rows = rows.len()))]
pub async fn insert_realisation_batch(
    pool: &PgPool,
    rows: &[([u8; 32], String, String, [u8; 32])],
) -> Result<u64, sqlx::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut h: Vec<&[u8]> = Vec::with_capacity(rows.len());
    let mut n: Vec<&str> = Vec::with_capacity(rows.len());
    let mut p: Vec<&str> = Vec::with_capacity(rows.len());
    let mut oh: Vec<&[u8]> = Vec::with_capacity(rows.len());
    for (mh, on, op, ohash) in rows {
        h.push(mh.as_slice());
        n.push(on.as_str());
        p.push(op.as_str());
        oh.push(ohash.as_slice());
    }
    let result = sqlx::query(
        r#"
        INSERT INTO realisations (drv_hash, output_name, output_path, output_hash, signatures)
        SELECT h, n, p, oh, '{}'::text[]
        FROM UNNEST($1::bytea[], $2::text[], $3::text[], $4::bytea[]) AS t(h, n, p, oh)
        ON CONFLICT (drv_hash, output_name) DO NOTHING
        "#,
    )
    .bind(&h)
    .bind(&n)
    .bind(&p)
    .bind(&oh)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    // r[verify sched.ca.resolve+3]
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

    // r[verify sched.ca.resolve+3]
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

    // r[verify sched.ca.resolve+3]
    /// Deferred-IA on CA input: parent output is `("out","","","")`
    /// (no `hash_algo` — Nix's `DerivationOutput::Deferred`). After
    /// `resolve_ca_inputs`, the parent's own `$out` MUST be a real
    /// store path in BOTH `outputs[0].path` and `env["out"]`, and
    /// `ResolvedDerivation.output_paths` must surface it for the
    /// dispatch-side `expected_output_paths` overwrite.
    ///
    /// Regression: pre-fix, `resolve_ca_inputs` only did phase 1
    /// (input collapse + placeholder rewrite); the resolved ATerm
    /// still had `("out","","","")` and `env["out"]=""` → builder
    /// ran with `$out=""` → `mkdir -p $out` → busybox usage error.
    #[tokio::test]
    async fn resolve_fills_deferred_ia_out() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

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

        let input_drv_path = "/nix/store/44444444444444444444444444444444-dep.drv";
        let input_sp = StorePath::parse(input_drv_path)?;
        let placeholder = downstream_placeholder(&input_sp, "out");

        // Deferred-IA parent: ("out","","","") — no hash_algo, no path.
        let parent_aterm = format!(
            r#"Derive([("out","","","")],[("{input_drv_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","build"],[("DEP","{placeholder}"),("name","parent"),("out",""),("system","x86_64-linux")])"#
        );

        let ca_inputs = vec![CaResolveInput {
            drv_path: input_drv_path.into(),
            modular_hash: input_modular,
        }];

        let resolved =
            resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &[], &db.pool).await?;

        let reparsed = Derivation::parse(std::str::from_utf8(&resolved.drv_content)?)?;

        // The fix: own output path is computed and written into both
        // outputs[0].path and env["out"].
        let out_path = reparsed.outputs()[0].path();
        assert!(
            out_path.starts_with("/nix/store/") && out_path.ends_with("-parent"),
            "deferred-IA $out must be filled post-resolve; got {out_path:?}"
        );
        assert_eq!(
            reparsed.env().get("out").map(String::as_str),
            Some(out_path),
            "env[\"out\"] must match outputs[0].path"
        );
        // Surfaced for dispatch's expected_output_paths overwrite.
        assert_eq!(
            resolved.output_paths,
            vec![("out".to_string(), out_path.to_string())]
        );
        // Phase 1 still happened: input placeholder rewritten,
        // inputDrvs collapsed.
        assert_eq!(
            reparsed.env().get("DEP").map(String::as_str),
            Some(realized_path)
        );
        assert!(reparsed.input_drvs().is_empty());
        assert!(reparsed.input_srcs().contains(realized_path));
        Ok(())
    }

    /// Floating-CA-self with a CA input: `("out","","sha256","")`.
    /// `fill_deferred_outputs` must leave it alone — nix-daemon
    /// computes the scratch path internally from `hash_algo`.
    /// `output_paths` stays empty so dispatch doesn't clobber
    /// `expected_output_paths` with a wrong value.
    #[tokio::test]
    async fn resolve_floating_ca_self_leaves_out_empty() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let input_modular: [u8; 32] = [0x11; 32];
        sqlx::query(
            "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
             VALUES ($1, 'out', '/nix/store/22222222222222222222222222222222-dep', $2)",
        )
        .bind(input_modular.as_slice())
        .bind([0x33u8; 32].as_slice())
        .execute(&db.pool)
        .await?;

        let input_drv_path = "/nix/store/44444444444444444444444444444444-dep.drv";
        let placeholder = downstream_placeholder(&StorePath::parse(input_drv_path)?, "out");
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{input_drv_path}",["out"])],[],"x86_64-linux","/bin/sh",[],[("DEP","{placeholder}"),("name","parent"),("out","")])"#
        );

        let ca_inputs = vec![CaResolveInput {
            drv_path: input_drv_path.into(),
            modular_hash: input_modular,
        }];

        let resolved =
            resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &[], &db.pool).await?;
        let reparsed = Derivation::parse(std::str::from_utf8(&resolved.drv_content)?)?;

        assert_eq!(
            reparsed.outputs()[0].path(),
            "",
            "floating-CA output path must stay empty for nix-daemon scratch handling"
        );
        assert!(
            resolved.output_paths.is_empty(),
            "floating-CA → no output_paths surfaced"
        );
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

    // r[verify sched.ca.resolve+3]
    /// `BasicDerivation::from_resolved` drops ALL `inputDrvs` (CA and
    /// IA alike) and merges `inputSrcs` in sorted order — matching
    /// Nix's `BasicDerivation resolved{*this}` slice-copy semantics
    /// (`derivations.cc:1204`, ADR-018 Appendix B step 3).
    #[test]
    fn from_resolved_drops_all_inputdrvs() -> anyhow::Result<()> {
        // Parent with TWO inputDrvs: one CA, one IA. Both must be
        // dropped. Both the realized CA path AND the IA expected
        // path land in inputSrcs (caller passes both).
        let aterm = r#"Derive([("out","","sha256","")],[("/nix/store/aaa-ca.drv",["out"]),("/nix/store/bbb-ia.drv",["out"])],["/nix/store/zzz-src"],"x86_64-linux","/bin/sh",[],[("name","parent")])"#;
        let drv = Derivation::parse(aterm)?;

        // Caller-collected: realized CA path + IA expected path.
        let resolved = BasicDerivation::from_resolved(
            &drv,
            ["/nix/store/ccc-realized", "/nix/store/ddd-ia-out"].map(String::from),
        )
        .to_aterm();

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
    fn from_resolved_ca_only() -> anyhow::Result<()> {
        let aterm = r#"Derive([("out","","sha256","")],[("/nix/store/aaa-ca.drv",["out"])],["/nix/store/orig-src"],"x86_64-linux","/bin/sh",[],[("name","parent")])"#;
        let drv = Derivation::parse(aterm)?;

        let resolved =
            BasicDerivation::from_resolved(&drv, ["/nix/store/realized-ca".to_string()]).to_aterm();

        let reparsed = Derivation::parse(&resolved)?;
        assert!(reparsed.input_drvs().is_empty());
        assert!(reparsed.input_srcs().contains("/nix/store/realized-ca"));
        assert!(reparsed.input_srcs().contains("/nix/store/orig-src"));
        Ok(())
    }

    /// bug_049: an empty `output_paths[i]` must NOT land in the
    /// resolved drv's `inputSrcs`. Pre-fix the `!path.is_empty()`
    /// guard wrapped only `rewrites.insert`, not `new_input_srcs.push`
    /// — `""` would feed into `hashDerivationModulo`. Unreachable
    /// today (children always completed-with-paths or absent from DAG
    /// by parent-dispatch time) but the asymmetry contradicted both
    /// the comment and the three sibling consumers that filter
    /// `!p.is_empty()`.
    #[tokio::test]
    async fn resolve_ia_empty_output_path_skipped() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let ia_drv = "/nix/store/88888888888888888888888888888888-ia.drv";
        let orig_src = "/nix/store/55555555555555555555555555555555-src";
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{ia_drv}",["out"])],["{orig_src}"],"x86_64-linux","/bin/sh",["-c","build"],[("name","parent"),("out",""),("system","x86_64-linux")])"#
        );
        let ia_inputs = vec![IaResolveInput {
            drv_path: ia_drv.into(),
            output_names: vec!["out".into()],
            output_paths: vec![String::new()],
        }];

        let resolved =
            resolve_ca_inputs(parent_aterm.as_bytes(), &[], &ia_inputs, &db.pool).await?;
        let reparsed = Derivation::parse(std::str::from_utf8(&resolved.drv_content)?)?;
        assert!(
            !reparsed.input_srcs().contains(""),
            "bug_049: empty IA output_path must not be pushed into inputSrcs"
        );
        assert!(reparsed.input_srcs().contains(orig_src));
        Ok(())
    }

    // r[verify sched.ca.resolve+3]
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

    // r[verify sched.ca.resolve+3]
    /// Multi-output CA child where the parent only wants `["out"]`.
    /// `ca_inputs` carries no per-edge subset (the DAG only knows the
    /// child's FULL output list); the parsed `inputDrvs` is the sole
    /// source of truth for which outputs to resolve.
    ///
    /// Regression: pre-fix the CA loop iterated `child.output_names`
    /// (`["out","dev"]`) → resolved `inputSrcs` contained `dev`'s
    /// realized path (Nix-incompat ATerm bytes) and recorded a
    /// spurious `realisation_deps` edge for `dev`.
    #[tokio::test]
    async fn resolve_ca_multi_output_filters_to_wanted_subset() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let input_modular: [u8; 32] = [0x11; 32];
        let out_path = "/nix/store/22222222222222222222222222222222-libfoo-out";
        let dev_path = "/nix/store/33333333333333333333333333333333-libfoo-dev";
        for (name, path, oh) in [("out", out_path, 0xaau8), ("dev", dev_path, 0xbb)] {
            sqlx::query(
                "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(input_modular.as_slice())
            .bind(name)
            .bind(path)
            .bind([oh; 32].as_slice())
            .execute(&db.pool)
            .await?;
        }

        let input_drv_path = "/nix/store/44444444444444444444444444444444-libfoo.drv";
        let placeholder = downstream_placeholder(&StorePath::parse(input_drv_path)?, "out");
        // Parent inputDrvs: libfoo → ["out"] only.
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{input_drv_path}",["out"])],[],"x86_64-linux","/bin/sh",[],[("DEP","{placeholder}"),("name","app")])"#
        );

        let ca_inputs = vec![CaResolveInput {
            drv_path: input_drv_path.into(),
            modular_hash: input_modular,
        }];

        let resolved =
            resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &[], &db.pool).await?;
        let reparsed = Derivation::parse(std::str::from_utf8(&resolved.drv_content)?)?;

        let srcs = reparsed.input_srcs();
        assert!(
            srcs.contains(out_path),
            "wanted 'out' realized in inputSrcs"
        );
        assert!(
            !srcs.contains(dev_path),
            "unwanted 'dev' MUST NOT be in inputSrcs — Nix tryResolve only adds wanted-subset"
        );
        assert_eq!(
            resolved.lookups.len(),
            1,
            "exactly one realisation_deps lookup (out only, no spurious dev edge)"
        );
        assert_eq!(resolved.lookups[0].dep_output_name, "out");
        Ok(())
    }

    // r[verify sched.ca.resolve+3]
    /// Multi-output CA child where `dev`'s realisation was never
    /// inserted (best-effort PG insert at completion lost it). Parent
    /// only wants `["out"]` — resolve MUST succeed.
    ///
    /// Regression: pre-fix the CA loop queried `dev` regardless and
    /// returned `RealisationMissing{output_name: "dev"}`, aborting
    /// resolve for an output the parent never references → parent
    /// dispatched unresolved → worker crashed on placeholder → infinite
    /// retry-with-backoff.
    #[tokio::test]
    async fn resolve_ca_multi_output_unwanted_missing_ok() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let input_modular: [u8; 32] = [0x11; 32];
        let out_path = "/nix/store/22222222222222222222222222222222-libfoo-out";
        // Only `out` realisation present. `dev` absent.
        sqlx::query(
            "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
             VALUES ($1, 'out', $2, $3)",
        )
        .bind(input_modular.as_slice())
        .bind(out_path)
        .bind([0xaau8; 32].as_slice())
        .execute(&db.pool)
        .await?;

        let input_drv_path = "/nix/store/44444444444444444444444444444444-libfoo.drv";
        let placeholder = downstream_placeholder(&StorePath::parse(input_drv_path)?, "out");
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{input_drv_path}",["out"])],[],"x86_64-linux","/bin/sh",[],[("DEP","{placeholder}"),("name","app")])"#
        );

        let ca_inputs = vec![CaResolveInput {
            drv_path: input_drv_path.into(),
            modular_hash: input_modular,
        }];

        let resolved = resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &[], &db.pool)
            .await
            .expect("resolve must succeed when only an UNWANTED output's realisation is missing");
        let reparsed = Derivation::parse(std::str::from_utf8(&resolved.drv_content)?)?;
        assert!(reparsed.input_srcs().contains(out_path));
        assert_eq!(
            reparsed.env().get("DEP").map(String::as_str),
            Some(out_path)
        );
        Ok(())
    }

    /// Three CA inputs, all single-output `["out"]` wanted. Asserts
    /// the batch→map→lookup wiring doesn't drop entries when
    /// `query_batch` returns multiple rows. Passes before+after the
    /// per-pair-await → batch refactor; guards correctness across the
    /// refactor (the structural single-await is verified by code
    /// inspection — only one `query_batch().await?` between ATerm
    /// parse and rewrite apply).
    #[tokio::test]
    async fn resolve_ca_multiple_inputs_single_roundtrip() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let mut ca_inputs = Vec::new();
        let mut input_drvs_aterm = String::new();
        let mut realized_paths = Vec::new();
        for i in 1u8..=3 {
            let modular: [u8; 32] = [i; 32];
            let realized = format!("/nix/store/{:032}-dep{i}-out", i);
            sqlx::query(
                "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
                 VALUES ($1, 'out', $2, $3)",
            )
            .bind(modular.as_slice())
            .bind(&realized)
            .bind([0x99u8; 32].as_slice())
            .execute(&db.pool)
            .await?;
            let drv_path = format!("/nix/store/{:032}-dep{i}.drv", i + 0x40);
            if i > 1 {
                input_drvs_aterm.push(',');
            }
            input_drvs_aterm.push_str(&format!(r#"("{drv_path}",["out"])"#));
            ca_inputs.push(CaResolveInput {
                drv_path,
                modular_hash: modular,
            });
            realized_paths.push(realized);
        }

        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[{input_drvs_aterm}],[],"x86_64-linux","/bin/sh",[],[("name","app")])"#
        );

        let resolved =
            resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &[], &db.pool).await?;
        let reparsed = Derivation::parse(std::str::from_utf8(&resolved.drv_content)?)?;

        for p in &realized_paths {
            assert!(
                reparsed.input_srcs().contains(p),
                "realized path {p} missing from inputSrcs"
            );
        }
        assert_eq!(resolved.lookups.len(), 3, "all three CA inputs recorded");
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

    // r[verify sched.ca.cutoff-propagate+2]
    /// Regression: the `found.len() >= max_nodes` check ran only at the
    /// top of each `frontier.pop()`, BEFORE the unbounded `fetch_all`.
    /// A single high-fanout reverse-deps query (glibc, stdenv) could
    /// return ≫`max_nodes` rows, all inserted into `found` before the
    /// next outer-loop check. The caller's "bounded by
    /// MAX_CASCADE_NODES" linear scan (completion.rs) then ran on the
    /// full set, outside the 2s timeout, in the actor's single-threaded
    /// loop.
    #[tokio::test]
    async fn walk_dependent_realisations_caps_single_high_fanout() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Seed: one realisation with N dependents all pointing back at it
        // via realisation_deps — a single reverse-deps query returns N.
        let seed_hash: [u8; 32] = [0u8; 32];
        insert_realisation(&db.pool, &seed_hash, "out", "/nix/store/seed", &[0xaa; 32]).await?;

        const N: u8 = 50;
        for i in 1..=N {
            let dep_hash: [u8; 32] = [i; 32];
            insert_realisation(
                &db.pool,
                &dep_hash,
                "out",
                &format!("/nix/store/dep-{i}"),
                &[0xbb; 32],
            )
            .await?;
            sqlx::query(
                "INSERT INTO realisation_deps \
                 (drv_hash, output_name, dep_drv_hash, dep_output_name) \
                 VALUES ($1, 'out', $2, 'out')",
            )
            .bind(dep_hash.as_slice())
            .bind(seed_hash.as_slice())
            .execute(&db.pool)
            .await?;
        }

        let seeds = [(seed_hash.to_vec(), "out".to_string())];

        // Regression assertion: cap respected even when ONE query
        // returns more than max_nodes.
        let out = walk_dependent_realisations(&db.pool, &seeds, 10).await?;
        assert!(
            out.len() <= 10,
            "single high-fanout query must not overshoot max_nodes; got {}",
            out.len()
        );

        // Sanity: with cap above fanout, all dependents returned.
        let out = walk_dependent_realisations(&db.pool, &seeds, 100).await?;
        assert_eq!(out.len(), usize::from(N));
        Ok(())
    }

    // r[verify sched.db.batch-unnest]
    /// `insert_realisation_batch` round-trips N rows in one UNNEST and
    /// is idempotent (`ON CONFLICT DO NOTHING`) — same contract as the
    /// single-row [`insert_realisation`].
    #[tokio::test]
    async fn insert_realisation_batch_inserts_and_is_idempotent() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let rows: Vec<([u8; 32], String, String, [u8; 32])> = (0u8..5)
            .map(|i| {
                (
                    [i; 32],
                    "out".to_string(),
                    format!("/nix/store/{}-pkg-{i}", "a".repeat(32)),
                    [0xEE; 32],
                )
            })
            .collect();

        // Empty batch is a cheap no-op.
        assert_eq!(insert_realisation_batch(&db.pool, &[]).await?, 0);

        let n = insert_realisation_batch(&db.pool, &rows).await?;
        assert_eq!(n, 5, "all five rows inserted");

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM realisations")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count, 5);

        // Same rows again → ON CONFLICT DO NOTHING.
        let n2 = insert_realisation_batch(&db.pool, &rows).await?;
        assert_eq!(n2, 0, "duplicate batch insert is a no-op");

        // Spot-check one row landed with the right values, and that
        // signatures defaulted to '{}'.
        let (path, sigs): (String, Vec<String>) = sqlx::query_as(
            "SELECT output_path, signatures FROM realisations \
             WHERE drv_hash = $1 AND output_name = 'out'",
        )
        .bind([3u8; 32].as_slice())
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(path, format!("/nix/store/{}-pkg-3", "a".repeat(32)));
        assert!(sigs.is_empty(), "batch insert leaves signatures empty");

        Ok(())
    }

    // TODO(P0311-T62): golden resolved-ATerm test against Nix's
    //   tryResolve. inputSrcs is now complete (IA output paths via
    //   DAG expected_output_paths), so a byte-for-byte compare
    //   against a Nix-generated resolved .drv is meaningful. The
    //   remaining diff would only be inputSrcs ordering, which
    //   BasicDerivation::from_resolved already sorts canonically. ADR-018
    //   Appendix B captures the transformation but no byte-exact
    //   fixture exists yet. Adjacent golden-fixture work: P0311-T62
    //   (downstream_placeholder golden — landed above as
    //   placeholder_golden_matches_nix_upstream).
}

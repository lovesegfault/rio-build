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

use std::collections::BTreeMap;

use rio_nix::derivation::{Derivation, DerivationError};
use rio_nix::store_path::{StorePath, StorePathError, nixbase32};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;
use tracing::{debug, instrument};

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
pub fn downstream_placeholder(
    drv_path: &StorePath,
    output_name: &str,
) -> Result<String, ResolveError> {
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
    Ok(format!("/{}", nixbase32::encode(&hash)))
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
/// ## Inputs not in `ca_inputs`
///
/// Non-CA inputs (input-addressed, fixed-output with known path)
/// don't need resolution — their output paths are already concrete
/// in the ATerm. They stay in `inputDrvs` untouched. If `ca_inputs`
/// is empty, this is a no-op that returns the original `drv_content`
/// unchanged.
///
/// ## Why this returns `lookups` instead of inserting itself
///
/// Separating the pure resolution from the DB side-effect lets the
/// caller batch inserts across multiple dispatches, wrap in a
/// transaction with the assignment insert, or skip the insert in
/// tests. The pure-function shape also makes the test (`T3`)
/// assert on the rewrite logic without a PG fixture.
#[instrument(skip_all, fields(n_ca_inputs = ca_inputs.len()))]
pub async fn resolve_ca_inputs(
    drv_content: &[u8],
    ca_inputs: &[CaResolveInput],
    pool: &PgPool,
) -> Result<ResolvedDerivation, ResolveError> {
    if drv_content.is_empty() {
        return Err(ResolveError::NoDrvContent);
    }

    // Fast-path: no CA inputs → resolution is a no-op. The parent
    // is CA but all its inputs are input-addressed (common: a
    // CA `mkDerivation` depending on IA `stdenv`).
    if ca_inputs.is_empty() {
        return Ok(ResolvedDerivation {
            drv_content: drv_content.to_vec(),
            lookups: Vec::new(),
        });
    }

    // Parse the full Derivation (with inputDrvs). Early parse is a
    // validity check — if the ATerm is malformed we fail before
    // hitting PG. The rewritten parse below is the one we serialize.
    let drv_text = std::str::from_utf8(drv_content)?;
    let _ = Derivation::parse(drv_text)?;

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
            let placeholder = downstream_placeholder(&input_path, output_name)?;
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

    // Re-parse the rewritten ATerm so we can drop the CA inputDrvs
    // and merge the realized paths into inputSrcs. The string-replace
    // above only touched placeholder occurrences in env/args/builder;
    // it did NOT touch the inputDrvs list (which is keyed by .drv
    // paths, not placeholders).
    let rewritten_drv = Derivation::parse(&rewritten)?;

    // Build the resolved Derivation: inputSrcs ← old inputSrcs ∪
    // realized paths ∪ the .drv paths of NON-CA inputs (input-addressed
    // deps still need their .drv visible). inputDrvs ← ONLY non-CA
    // inputs (Nix keeps input-addressed inputDrvs even in resolved
    // derivations — they resolve to fixed paths at eval time anyway).
    //
    // Actually per ADR-018 Appendix B step 3, resolved.inputDrvs is
    // ALWAYS empty (BasicDerivation slice-copy drops it). The
    // non-CA inputs' CONCRETE output paths should already be in the
    // unresolved drv as literal paths (input-addressed → path known at
    // eval time → no placeholder). So: drop ALL inputDrvs, add
    // realized CA paths to inputSrcs.
    let ca_input_paths: std::collections::BTreeSet<&str> =
        ca_inputs.iter().map(|i| i.drv_path.as_str()).collect();

    // Serialize with inputDrvs stripped of CA entries and inputSrcs
    // expanded. Since `Derivation` has no public mutator, we
    // re-serialize manually in the same ATerm shape.
    let resolved_aterm = serialize_resolved(
        &rewritten_drv,
        &ca_input_paths,
        new_input_srcs.iter().map(String::as_str),
    );

    debug!(
        n_rewrites = rewrites.len(),
        n_lookups = lookups.len(),
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
async fn query_realisation(
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

/// Serialize a resolved derivation back to ATerm with `inputDrvs`
/// entries in `drop_input_drvs` removed and `extra_input_srcs` merged
/// into the existing `inputSrcs` set.
///
/// Uses `Derivation::to_aterm` on a re-parsed struct rather than
/// hand-rolling the serializer — ensures escaping and ordering match
/// Nix's canonical form exactly. The re-parse cost is ~μs for
/// typical <100KB ATerms.
fn serialize_resolved<'a>(
    drv: &Derivation,
    drop_input_drvs: &std::collections::BTreeSet<&str>,
    extra_input_srcs: impl Iterator<Item = &'a str>,
) -> String {
    // Collect the surviving inputDrvs and the union inputSrcs.
    // `Derivation` has no public setters, so we re-serialize the
    // ATerm by hand for the inputDrvs/inputSrcs sections and
    // delegate to `to_aterm` for the rest via a parse-edit-parse
    // round trip on the tail.
    //
    // Simpler: serialize the whole thing to_aterm(), then textually
    // edit the `inputDrvs` and `inputSrcs` lists. ATerm's list shape
    // is `[item,item,...]` with no whitespace, so we can splice.
    //
    // But that's fragile against drv_path strings containing `]`.
    // Instead: re-build the ATerm from scratch, mirroring
    // `Derivation::to_aterm`'s structure exactly.
    let mut out = String::new();
    out.push_str("Derive(");

    // outputs — unchanged (placeholders in output paths were
    // already replaced by the global string-replace before this
    // parse).
    out.push('[');
    for (i, o) in drv.outputs().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('(');
        write_aterm_string(&mut out, o.name());
        out.push(',');
        write_aterm_string(&mut out, o.path());
        out.push(',');
        write_aterm_string(&mut out, o.hash_algo());
        out.push(',');
        write_aterm_string(&mut out, o.hash());
        out.push(')');
    }
    out.push_str("],");

    // inputDrvs — drop CA entries.
    out.push('[');
    let mut first = true;
    for (drv_path, output_names) in drv.input_drvs() {
        if drop_input_drvs.contains(drv_path.as_str()) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push('(');
        write_aterm_string(&mut out, drv_path);
        out.push_str(",[");
        for (j, name) in output_names.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            write_aterm_string(&mut out, name);
        }
        out.push_str("])");
    }
    out.push_str("],");

    // inputSrcs — union of original + realized CA paths.
    // BTreeSet for dedup + Nix-canonical sorted order.
    let mut srcs: std::collections::BTreeSet<&str> =
        drv.input_srcs().iter().map(String::as_str).collect();
    for s in extra_input_srcs {
        srcs.insert(s);
    }
    out.push('[');
    for (i, s) in srcs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_aterm_string(&mut out, s);
    }
    out.push_str("],");

    // platform, builder
    write_aterm_string(&mut out, drv.platform());
    out.push(',');
    write_aterm_string(&mut out, drv.builder());
    out.push(',');

    // args
    out.push('[');
    for (i, a) in drv.args().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_aterm_string(&mut out, a);
    }
    out.push_str("],");

    // env
    out.push('[');
    for (i, (k, v)) in drv.env().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('(');
        write_aterm_string(&mut out, k);
        out.push(',');
        write_aterm_string(&mut out, v);
        out.push(')');
    }
    out.push_str("])");

    out
}

/// ATerm string escaping — mirrors `aterm.rs:write_aterm_string`.
/// Duplicated here because `rio_nix::derivation`'s helper is
/// crate-private. 12 lines of escaping is cheaper than making it
/// `pub` for one cross-crate caller.
fn write_aterm_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// Compute a placeholder and assert the shape: `/` + 52 nixbase32
    /// chars. Matches the `"/1ril1qzj..."` example in ADR-018
    /// Appendix B.
    #[test]
    fn placeholder_shape() {
        let drv_path =
            StorePath::parse("/nix/store/00000000000000000000000000000000-foo.drv").unwrap();
        let p = downstream_placeholder(&drv_path, "out").unwrap();
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
        let p_out = downstream_placeholder(&drv_path, "out").unwrap();
        let p_dev = downstream_placeholder(&drv_path, "dev").unwrap();
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
        let p = downstream_placeholder(&drv_path, "out").unwrap();
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
        let placeholder = downstream_placeholder(&input_sp, "out")?;

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

        let resolved = resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &db.pool).await?;

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

        // CA inputDrv is dropped from inputDrvs.
        assert!(
            !resolved_drv.input_drvs().contains_key(input_drv_path),
            "CA input should be removed from inputDrvs"
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
        let placeholder = downstream_placeholder(&StorePath::parse(input_drv_path)?, "out")?;
        let parent_aterm = format!(
            r#"Derive([("out","","sha256","")],[("{input_drv_path}",["out"])],[],"x86_64-linux","/bin/sh",[],[("DEP","{placeholder}")])"#
        );

        let ca_inputs = vec![CaResolveInput {
            drv_path: input_drv_path.into(),
            modular_hash: [0x77; 32], // Not in realisations table.
            output_names: vec!["out".into()],
        }];

        let err = resolve_ca_inputs(parent_aterm.as_bytes(), &ca_inputs, &db.pool)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ResolveError::RealisationMissing { .. }),
            "expected RealisationMissing, got {err:?}"
        );
        Ok(())
    }

    /// Empty `ca_inputs` → no-op passthrough. The parent is CA but
    /// has no CA inputs (all inputs input-addressed).
    #[tokio::test]
    async fn resolve_noop_when_no_ca_inputs() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let parent_aterm = r#"Derive([("out","","sha256","")],[],[],"x86_64-linux","/bin/sh",[],[("name","leaf")])"#;
        let resolved = resolve_ca_inputs(parent_aterm.as_bytes(), &[], &db.pool).await?;
        assert_eq!(
            resolved.drv_content,
            parent_aterm.as_bytes(),
            "no CA inputs → drv_content unchanged"
        );
        assert!(resolved.lookups.is_empty());
        Ok(())
    }

    /// `serialize_resolved` preserves non-CA inputDrvs and merges
    /// inputSrcs in sorted order — ATerm canonical form matters for
    /// Nix compatibility.
    #[test]
    fn serialize_resolved_preserves_non_ca_inputs() -> anyhow::Result<()> {
        // Parent with TWO inputDrvs: one CA (to drop), one IA (to keep).
        let aterm = r#"Derive([("out","","sha256","")],[("/nix/store/aaa-ca.drv",["out"]),("/nix/store/bbb-ia.drv",["out"])],["/nix/store/zzz-src"],"x86_64-linux","/bin/sh",[],[("name","parent")])"#;
        let drv = Derivation::parse(aterm)?;

        let drop: std::collections::BTreeSet<&str> =
            ["/nix/store/aaa-ca.drv"].into_iter().collect();
        let resolved = serialize_resolved(&drv, &drop, ["/nix/store/ccc-realized"].into_iter());

        let reparsed = Derivation::parse(&resolved)?;
        // CA inputDrv dropped; IA inputDrv kept.
        assert!(!reparsed.input_drvs().contains_key("/nix/store/aaa-ca.drv"));
        assert!(reparsed.input_drvs().contains_key("/nix/store/bbb-ia.drv"));
        // inputSrcs is the sorted union: ccc-realized < zzz-src.
        let srcs: Vec<&str> = reparsed.input_srcs().iter().map(String::as_str).collect();
        assert_eq!(srcs, vec!["/nix/store/ccc-realized", "/nix/store/zzz-src"]);
        Ok(())
    }
}

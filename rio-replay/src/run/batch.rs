//! Greedy batch assembly with the dual cap: batches are capped on BOTH job
//! count and the estimated merged-closure drv node count, so a single
//! batch submission never carries more derivations than the gateway
//! comfortably ingests even when jobs share most of their closures. An
//! oversized single job becomes a singleton batch rather than being dropped.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// One submittable job (target drv + its dependency drv closure, from
/// the replay archive). Closure sizing is always done on the merged-batch
/// union inside [`assemble_batches`], never per job — shared dependencies
/// must only count once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingJob {
    pub job: String,
    pub drv_path: String,
    pub dep_drvs: Vec<String>,
}

/// One submission root: the target drv plus the output selection the wire
/// demand string is formatted from at the submission chokepoint
/// ([`derived_path`](Self::derived_path)). Carrying the selection ON the
/// root — instead of in a parallel vector or a projection the dispatcher
/// re-derives — makes "root submitted without its recorded selection"
/// unrepresentable: every producer that names a root decides its outputs in
/// the same expression.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchRoot {
    /// Store path of the root derivation. Same ARCHIVE provenance as
    /// `outputs` (a recorded request target or a workload unit's drv
    /// path), but unlike `outputs` it is not clamped by
    /// [`derived_path`](Self::derived_path): a corrupt path formats
    /// verbatim into the demand string and the gateway's
    /// `DerivedPath::parse` rejects THAT root (`InputRejected`) —
    /// per-root containment, never an engine-side guess at what the
    /// recording meant.
    pub drv: String,
    /// Recorded output names; `[]` and `["*"]` both mean every output.
    /// ARCHIVE-controlled (recorded request input, possibly foreign or
    /// corrupt — neither the v1 schema nor the v0 shim validates names),
    /// so the wire formatting clamps rather than trusting it; see
    /// [`derived_path`](Self::derived_path). Workload producers (the
    /// assembler's units, canary probes) have no recorded selection and
    /// leave this empty.
    pub outputs: Vec<String>,
}

impl BatchRoot {
    /// `"<drv>!out1,out2"` / `"<drv>!*"` formatting for
    /// `BuildPathsWithResults` — the engine side of the wire grammar the
    /// gateway parses with [`rio_nix::protocol::derived_path::DerivedPath::parse`].
    ///
    /// `outputs` is ARCHIVE-controlled, and the parser REJECTS shapes a raw
    /// join could emit — duplicate names (`DuplicateOutputName`), empty
    /// names (`EmptyOutputName`), more than
    /// [`MAX_OUTPUT_NAMES`](rio_nix::protocol::derived_path::MAX_OUTPUT_NAMES)
    /// names (`TooManyOutputs`) — turning the whole root into an
    /// `InputRejected` failure. It also treats `*` as "all outputs" only
    /// when `*` is the ENTIRE spec; a `*` mixed among names parses as a
    /// literal output name no derivation declares. So the recorded
    /// selection is normalized here, at the one formatting site:
    ///
    /// - `[]` and any list containing `*` format as `!*` — the recording
    ///   asked for everything, and `*` saturates exactly like the
    ///   gateway's own demand union (all ∪ X = all).
    /// - Duplicates collapse to the first occurrence: repeats are
    ///   unambiguous about intent, and the parser rejects them verbatim.
    /// - A wire-inexpressible member (empty, or containing the `,`
    ///   separator) collapses the WHOLE selection to `!*`: the recorded
    ///   selection is corrupt evidence, and the all-outputs demand — the
    ///   pre-threading posture for every root — over-asks rather than
    ///   guessing a narrower set or getting the root wire-rejected.
    /// - More than `MAX_OUTPUT_NAMES` distinct names cannot be expressed
    ///   in one demand string; same widest-demand fallback.
    ///
    /// CLAMPED, not dropped, for the same reason `recorded_request_from`
    /// clamps corrupt offsets: a request is workload — it must still
    /// submit — and `!*` is the unique fallback that never under-asks
    /// relative to the recording.
    pub fn derived_path(&self) -> String {
        use rio_nix::protocol::derived_path::MAX_OUTPUT_NAMES;
        let mut names: Vec<&str> = Vec::with_capacity(self.outputs.len());
        for name in &self.outputs {
            if name == "*" {
                return all_outputs_demand(&self.drv);
            }
            if name.is_empty() || name.contains(',') {
                return all_outputs_demand(&self.drv);
            }
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
        if names.is_empty() || names.len() > MAX_OUTPUT_NAMES {
            return all_outputs_demand(&self.drv);
        }
        format!("{}!{}", self.drv, names.join(","))
    }
}

/// The all-outputs demand string — the ONE production spelling of the
/// `` `<drv>!*` `` wire form ([`rio_nix::protocol::derived_path::DerivedPath::parse`]
/// reads it back as `OutputSpec::All`). Every saturating arm of
/// [`BatchRoot::derived_path`] and the supply prefetch arm
/// (`run/supply/exec.rs`, whose roots are plan-derived paths that carry
/// no recorded selection vocabulary at all) route through this owner,
/// so the demand grammar's widest form cannot fork spellings across
/// producers. The literal-site enumeration test below pins the
/// single-owner rule: a new production site formatting the form
/// directly fails the count until it routes here.
pub fn all_outputs_demand(drv: &str) -> String {
    format!("{drv}!*")
}

/// One assembled batch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Batch {
    pub jobs: Vec<String>,
    pub roots: Vec<BatchRoot>,
    /// The PRODUCER's a-priori merged-DAG node estimate. For batches from
    /// [`assemble_batches`] this is the exact union of {target drv} ∪ dep
    /// drvs over the batch — the quantity the packing caps act on; the
    /// timed dispatcher's audited literal constructions (which have no
    /// adjacency data — recorded request targets need not be workload
    /// units) write the roots-only floor. Packing and batches.jsonl
    /// bookkeeping ONLY: the build op's stderr drain budget is keyed at
    /// the submission chokepoint from the realized import closure
    /// (`ClientOpsSubmitter::submit_batch`), never from this field, so no
    /// producer can under-key the belt. The construction-site enumeration
    /// test below pins every literal producer.
    pub est_nodes: usize,
}

impl Batch {
    /// The bare root drv paths, in submission order — the projection the
    /// drv-closure import, the supply top-up, the positional result
    /// mapping, and the batches.jsonl record key on (results are always
    /// drv-keyed; only the wire demand string carries the output
    /// selection).
    pub fn root_drv_paths(&self) -> Vec<String> {
        self.roots.iter().map(|root| root.drv.clone()).collect()
    }
}

/// Greedy accumulation in input order, capped on both job count and the
/// merged node estimate. A single job whose own estimate exceeds `max_nodes`
/// becomes a singleton batch: it must still be submitted, just never packed
/// together with anything else. Pathological caps therefore reduce batching
/// efficiency but never drop work — `max_nodes = 0` degrades to one batch
/// per job, and `max_jobs` is clamped to at least 1.
pub fn assemble_batches(jobs: &[PendingJob], max_jobs: usize, max_nodes: usize) -> Vec<Batch> {
    let max_jobs = max_jobs.max(1);
    let mut batches = Vec::new();
    let mut current = Batch::default();
    let mut union: HashSet<&str> = HashSet::new();

    for job in jobs {
        // Nodes this job would newly add to the running batch union.
        let mut added: HashSet<&str> = HashSet::new();
        if !union.contains(job.drv_path.as_str()) {
            added.insert(job.drv_path.as_str());
        }
        for dep in &job.dep_drvs {
            if !union.contains(dep.as_str()) {
                added.insert(dep.as_str());
            }
        }
        let would_overflow_jobs = current.jobs.len() + 1 > max_jobs;
        let would_overflow_nodes = union.len() + added.len() > max_nodes;
        if !current.jobs.is_empty() && (would_overflow_jobs || would_overflow_nodes) {
            current.est_nodes = union.len();
            batches.push(std::mem::take(&mut current));
            union.clear();
            // Re-seed this job's nodes against the fresh union.
            added = std::iter::once(job.drv_path.as_str())
                .chain(job.dep_drvs.iter().map(String::as_str))
                .collect();
        }
        union.extend(added);
        current.jobs.push(job.job.clone());
        // Workload units carry no recorded per-target output selection —
        // empty formats as the all-outputs demand (`!*`).
        current.roots.push(BatchRoot {
            drv: job.drv_path.clone(),
            outputs: Vec::new(),
        });
    }
    if !current.jobs.is_empty() {
        current.est_nodes = union.len();
        batches.push(current);
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, deps: usize) -> PendingJob {
        PendingJob {
            job: name.to_string(),
            drv_path: format!("/nix/store/{:0>32}-{name}.drv", name.len()),
            dep_drvs: (0..deps)
                .map(|i| format!("/nix/store/{i:0>32}-dep-{name}-{i}.drv"))
                .collect(),
        }
    }

    #[test]
    fn caps_on_job_count() {
        let jobs: Vec<PendingJob> = (0..7).map(|i| job(&format!("j{i}"), 0)).collect();
        let batches = assemble_batches(&jobs, 3, 1_000);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].jobs.len(), 3);
        assert_eq!(batches[1].jobs.len(), 3);
        assert_eq!(batches[2].jobs.len(), 1);
        assert_eq!(batches[0].est_nodes, 3);
    }

    #[test]
    fn caps_on_merged_nodes_with_shared_deps_counted_once() {
        // Two jobs share the same 10 deps: merged estimate is 12, not 22.
        let shared: Vec<String> = (0..10)
            .map(|i| format!("/nix/store/{i:0>32}-shared-{i}.drv"))
            .collect();
        let mut a = job("a", 0);
        a.dep_drvs = shared.clone();
        let mut b = job("b", 0);
        b.dep_drvs = shared.clone();
        let batches = assemble_batches(&[a, b], 50, 15);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].est_nodes, 12);

        // Three jobs with disjoint 10-dep closures and a 25-node cap → 2+1.
        let jobs: Vec<PendingJob> = (0..3).map(|i| job(&format!("d{i}"), 10)).collect();
        let batches = assemble_batches(&jobs, 50, 25);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].jobs.len(), 2);
        assert_eq!(batches[1].jobs.len(), 1);
    }

    #[test]
    fn oversized_single_job_becomes_singleton() {
        let big = job("texlive", 9000);
        let small = job("hello", 1);
        let batches = assemble_batches(&[small.clone(), big.clone(), small.clone()], 50, 4500);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[1].jobs, vec!["texlive"]);
        assert_eq!(batches[1].est_nodes, 9001);
        // The default submit caps (50 jobs / 4500 nodes) still accept a lone
        // oversized job as its own batch.
        let m1 = assemble_batches(&[big], 50, 4500);
        assert_eq!(m1.len(), 1);
        // A zero node cap degrades to one batch per job, never dropped work.
        let zero = assemble_batches(&[job("a", 2), job("b", 0)], 50, 0);
        assert_eq!(zero.len(), 2);
    }

    #[test]
    fn deterministic_for_same_input() {
        let jobs: Vec<PendingJob> = (0..20).map(|i| job(&format!("j{i}"), i % 5)).collect();
        let a = assemble_batches(&jobs, 4, 30);
        let b = assemble_batches(&jobs, 4, 30);
        assert_eq!(a, b);
    }

    /// The assembler is a producer of the demand-string vocabulary like
    /// the timed dispatcher: its workload units carry no recorded
    /// per-target output selection, so every root it emits formats as the
    /// all-outputs demand — the absent-subset side of the wire contract.
    #[test]
    fn assembler_roots_demand_all_outputs() {
        let jobs: Vec<PendingJob> = (0..3).map(|i| job(&format!("j{i}"), 1)).collect();
        let batches = assemble_batches(&jobs, 50, 1_000);
        let roots: Vec<&BatchRoot> = batches.iter().flat_map(|b| &b.roots).collect();
        assert_eq!(roots.len(), 3);
        for root in roots {
            assert!(root.outputs.is_empty());
            assert_eq!(root.derived_path(), format!("{}!*", root.drv));
        }
    }

    /// Wire-grammar conformance of the demand-string formatter against the
    /// REAL parser the gateway runs on every received derived path
    /// (`DerivedPath::parse` — `rio-gateway/src/handler/build.rs` calls it
    /// verbatim on each `wopBuildPathsWithResults` entry, and a parse
    /// failure turns the root into an `InputRejected` result).
    ///
    /// Quantification domain: the recorded-outputs value class is
    /// ARCHIVE-controlled with no validation at the v1 schema, the v0
    /// shim, or the schedule conversion — so the corpus enumerates the
    /// honest shapes (absent / `["*"]` / explicit subset, the SR-mandated
    /// rows on both sides of the "has recorded subset" condition) AND
    /// every parser-rejection class a corrupt recording can trigger
    /// (duplicates, empty name, separator-in-name, over-cap) plus the
    /// `*`-among-names shape the parser ACCEPTS with the wrong meaning.
    ///
    /// Each corrupt row asserts clamp-vs-scale explicitly, two-directional:
    /// the formatter's output is the documented clamp AND the naive join
    /// it replaces is rejected (or mis-parsed, for the two accept-but-wrong
    /// shapes) by the same parser — proving the normalization is
    /// load-bearing, not decorative.
    #[test]
    fn derived_path_conforms_to_the_gateway_parser() {
        use rio_nix::protocol::derived_path::{
            DerivedPath, DerivedPathError, MAX_OUTPUT_NAMES, OutputSpec,
        };

        let drv = format!("/nix/store/{:0>32}-multi-1.0.drv", 7);
        let root = |outputs: &[&str]| BatchRoot {
            drv: drv.clone(),
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
        };
        // The unnormalized join `derived_path` replaces — what reaching
        // the wire raw would have produced for the same recorded list.
        let naive = |outputs: &[&str]| format!("{drv}!{}", outputs.join(","));
        let parsed_spec = |s: &str| match DerivedPath::parse(s) {
            Ok(DerivedPath::Built { outputs, .. }) => outputs,
            other => panic!("expected a Built derived path for {s:?}, got {other:?}"),
        };
        let names = |list: &[&str]| OutputSpec::Names(list.iter().map(|s| s.to_string()).collect());

        // ── Honest shapes: format exactly, parse back to the selection ──
        // Absent subset (workload producers) and the recorded all-outputs
        // spelling: the all-outputs demand.
        for all in [&[] as &[&str], &["*"]] {
            let formatted = root(all).derived_path();
            assert_eq!(formatted, format!("{drv}!*"));
            assert_eq!(parsed_spec(&formatted), OutputSpec::All);
        }
        // Recorded subsets: exact demand, recorded order preserved.
        for subset in [&["out"] as &[&str], &["out", "dev"], &["dev", "out"]] {
            let formatted = root(subset).derived_path();
            assert_eq!(formatted, format!("{drv}!{}", subset.join(",")));
            assert_eq!(parsed_spec(&formatted), names(subset));
        }

        // ── `*` among names: parser ACCEPTS the naive string but as a
        // literal output name no derivation declares (only a whole-spec
        // `*` means "all"); the formatter saturates to all-outputs, the
        // gateway's own demand-union semantics (all ∪ X = all).
        let formatted = root(&["out", "*"]).derived_path();
        assert_eq!(formatted, format!("{drv}!*"));
        assert_eq!(parsed_spec(&formatted), OutputSpec::All);
        assert_eq!(parsed_spec(&naive(&["out", "*"])), names(&["out", "*"]));

        // ── Duplicates: unambiguous intent, deduped to first occurrence;
        // the naive string is parser-REJECTED.
        let formatted = root(&["out", "out", "dev", "out"]).derived_path();
        assert_eq!(formatted, format!("{drv}!out,dev"));
        assert_eq!(parsed_spec(&formatted), names(&["out", "dev"]));
        assert!(matches!(
            DerivedPath::parse(&naive(&["out", "out"])),
            Err(DerivedPathError::DuplicateOutputName)
        ));

        // ── Wire-inexpressible members clamp the WHOLE selection to the
        // widest demand (never under-ask on corrupt evidence, never get
        // the root wire-rejected). Empty name: naive string REJECTED.
        let formatted = root(&["out", ""]).derived_path();
        assert_eq!(formatted, format!("{drv}!*"));
        assert!(matches!(
            DerivedPath::parse(&naive(&["out", ""])),
            Err(DerivedPathError::EmptyOutputName)
        ));
        // Separator inside a recorded name: the naive string PARSES but as
        // two names the recording never listed.
        let formatted = root(&["a,b"]).derived_path();
        assert_eq!(formatted, format!("{drv}!*"));
        assert_eq!(parsed_spec(&naive(&["a,b"])), names(&["a", "b"]));

        // ── Over the parser's name cap: clamped to all-outputs; one more
        // distinct name than the cap is naive-REJECTED. At the cap
        // exactly, no clamp: the full subset formats and parses.
        let over: Vec<String> = (0..=MAX_OUTPUT_NAMES).map(|i| format!("o{i}")).collect();
        let over_refs: Vec<&str> = over.iter().map(String::as_str).collect();
        let formatted = root(&over_refs).derived_path();
        assert_eq!(formatted, format!("{drv}!*"));
        assert!(matches!(
            DerivedPath::parse(&naive(&over_refs)),
            Err(DerivedPathError::TooManyOutputs(n)) if n == MAX_OUTPUT_NAMES + 1
        ));
        let at_cap_refs = &over_refs[..MAX_OUTPUT_NAMES];
        let formatted = root(at_cap_refs).derived_path();
        assert_eq!(formatted, naive(at_cap_refs));
        assert_eq!(parsed_spec(&formatted), names(at_cap_refs));

        // ── Dedupe runs BEFORE the cap: a raw list far over the cap that
        // collapses to a tiny distinct set formats EXACTLY (300 raw / 2
        // distinct here), while the 257-DISTINCT sibling above clamps —
        // the cap charges distinct names, never raw length, so a
        // recording that sloppily repeats an honest selection is not
        // widened to `!*` by its repetition.
        let raw300: Vec<&str> = (0..300)
            .map(|i| if i % 2 == 0 { "out" } else { "dev" })
            .collect();
        let formatted = root(&raw300).derived_path();
        assert_eq!(formatted, format!("{drv}!out,dev"));
        assert_eq!(parsed_spec(&formatted), names(&["out", "dev"]));
    }

    /// Standing enumeration of [`Batch`] est_nodes producers: every `.rs`
    /// file in the crate is scanned for literal `Batch` constructions —
    /// the universe is a directory walk at test time
    /// (`crate::run::crate_sources`), so a NEW production site in any
    /// existing or any new file fails this lint until it either comes from
    /// [`assemble_batches`] (whose batches never match the needle — it
    /// builds via `Batch::default`) or is audited here with its
    /// `est_nodes` justification. Each file pins both its production-zone
    /// and test-zone count (`crate::run::lint_zones`; zone semantics
    /// documented on `run::tests::assert_consumer_counts`).
    ///
    /// Why this enumeration exists: the stderr drain budget was once keyed
    /// on the producer-written `est_nodes`, and the two timed-dispatcher
    /// literal sites wrote the ROOT COUNT — under-budgeting legal one-root
    /// deep-closure submissions to the single-unit floor. The budget is
    /// now derived at the submission chokepoint from the realized import
    /// closure, so a producer cannot under-key it; this lint keeps the
    /// remaining producer obligations (honest packing/bookkeeping
    /// estimates) reviewed whenever a construction site appears.
    ///
    /// The audited production sites:
    ///  1. src/run/batch.rs — the struct declaration itself, plus the
    ///     `impl Batch` block header (the needle is a plain substring
    ///     match; the impl block constructs nothing);
    ///  2. src/run/timeline.rs — the timed dispatcher's initial-dispatch
    ///     construction (roots-only floor; no adjacency data exists for
    ///     recorded request targets);
    ///  3. src/run/timeline.rs — its confirmation-retry sibling (same
    ///     floor, same rationale).
    ///
    /// Test-zone occurrences are fixtures: submitter.rs scripted batches
    /// (incl. the under-/over-keyed chokepoint fixtures and the
    /// recorded-output-selection wire-conformance rows), submit.rs
    /// chokepoint records, mod.rs stage harnesses.
    ///
    /// Outside the audited set, named so the census stays honest about
    /// its universe: the supply prefetch arm's `prefetch_build`
    /// (`run/supply/exec.rs`) emits a per-root all-outputs demand
    /// without constructing a `Batch`, so the needle structurally
    /// cannot see it — by design, since the prefetch arm asks the
    /// target to substitute everything and carries no recorded
    /// selection. Its demand STRING is not outside the audit, though:
    /// it routes through [`all_outputs_demand`], whose single-owner
    /// rule `all_outputs_demand_is_the_only_production_spelling` pins.
    #[test]
    fn batch_construction_sites_are_enumerated() {
        // Built at runtime so this test's own strings cannot match it.
        let needle = format!("{}{}", "Batch ", "{");
        let allowed: std::collections::BTreeMap<&str, (usize, usize)> = [
            ("src/run/batch.rs", (2, 0)),
            ("src/run/timeline.rs", (2, 0)),
            ("src/run/submitter.rs", (0, 6)),
            ("src/run/submit.rs", (0, 3)),
            ("src/run/mod.rs", (0, 2)),
        ]
        .into_iter()
        .collect();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (file, text) in crate::run::crate_sources() {
            let (prod, tail) = crate::run::lint_zones(&text);
            let expected = allowed.get(file.as_str()).copied().unwrap_or((0, 0));
            assert_eq!(
                (prod.matches(&needle).count(), tail.matches(&needle).count()),
                expected,
                "{file} (production zone, test zone): a new literal Batch construction \
                 site must come from assemble_batches or be audited into this enumeration \
                 with its est_nodes justification (the stderr drain budget is keyed at the \
                 submission chokepoint from the realized import closure, but est_nodes \
                 still feeds batch packing and the batches.jsonl record)"
            );
            seen.insert(file);
        }
        for file in allowed.keys() {
            assert!(
                seen.contains(*file),
                "{file} is enumerated but no longer exists; drop or move its row"
            );
        }
    }

    /// [`all_outputs_demand`] IS the saturating arm of the audited
    /// chokepoint, byte-for-byte — the prefetch arm routing through it
    /// therefore emits exactly what the submission chokepoint emits for
    /// a selection-less root — and the REAL gateway parser reads the
    /// form back as the all-outputs selection (the same parser
    /// `derived_path_conforms_to_the_gateway_parser` pins for every
    /// other arm). Both directions: the owner's output equals the
    /// independent spelling AND parses as `OutputSpec::All`, so neither
    /// a drifted owner nor a parser regression passes silently.
    #[test]
    fn all_outputs_demand_matches_the_saturating_arm_and_the_parser() {
        use rio_nix::protocol::derived_path::{DerivedPath, OutputSpec};

        let drv = format!("/nix/store/{:0>32}-prefetch-1.0.drv", 9);
        let demand = all_outputs_demand(&drv);
        // Independent spelling of the wire form (kept literal here so a
        // rewrite of the owner cannot re-derive the expectation).
        assert_eq!(demand, format!("{drv}!*"));
        // The owner and the chokepoint's selection-less arm agree.
        let selection_less = BatchRoot {
            drv: drv.clone(),
            outputs: Vec::new(),
        };
        assert_eq!(demand, selection_less.derived_path());
        // The gateway parser reads it back as the all-outputs demand.
        match DerivedPath::parse(&demand) {
            Ok(DerivedPath::Built { outputs, .. }) => assert_eq!(outputs, OutputSpec::All),
            other => panic!("expected a Built all-outputs derived path, got {other:?}"),
        }
    }

    /// Single-owner rule for the all-outputs demand literal, as a
    /// standing enumeration (same walked two-zone universe as the
    /// `Batch` census above): the only PRODUCTION-zone spellings of the
    /// wire form's format literal in the crate are this file's — the
    /// `derived_path` doc's quoted grammar example and the
    /// [`all_outputs_demand`] body. Every other producer (the
    /// submission chokepoint's saturating arms, the supply prefetch
    /// arm) routes through the owner fn, so a NEW production site that
    /// formats the literal directly — the shape that put the prefetch
    /// demand outside the audited construction set — fails this count
    /// until it routes through the owner or is audited here.
    ///
    /// Test-zone occurrences are deliberate spellings: wire-echo
    /// fixtures (`KeyedBuildResult.derived_path` values simulating the
    /// daemon's keyed echo) and expected-demand assertions, counted per
    /// file so a swapped zone cannot keep the books balanced.
    #[test]
    fn all_outputs_demand_is_the_only_production_spelling() {
        // Built at runtime so this test's own strings cannot match it
        // (each character separately — even a quoted two-char fragment
        // would re-create the needle in this file's source): the format
        // literal's closing characters (`!`, `*`, `"`).
        let needle = format!("{}{}{}", '!', '*', '"');
        let allowed: std::collections::BTreeMap<&str, (usize, usize)> = [
            ("src/run/batch.rs", (2, 7)),
            ("src/run/collect.rs", (0, 2)),
            ("src/run/mod.rs", (0, 2)),
            ("src/run/model.rs", (0, 1)),
            ("src/run/submitter.rs", (0, 5)),
            ("src/run/timeline.rs", (0, 1)),
            ("src/run/supply/exec.rs", (0, 1)),
        ]
        .into_iter()
        .collect();
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (file, text) in crate::run::crate_sources() {
            let (prod, tail) = crate::run::lint_zones(&text);
            let expected = allowed.get(file.as_str()).copied().unwrap_or((0, 0));
            assert_eq!(
                (prod.matches(&needle).count(), tail.matches(&needle).count()),
                expected,
                "{file} (production zone, test zone): a new spelling of the all-outputs \
                 demand literal must route through batch::all_outputs_demand (the one \
                 production owner) or be audited into this enumeration with its \
                 justification"
            );
            seen.insert(file);
        }
        for file in allowed.keys() {
            assert!(
                seen.contains(*file),
                "{file} is enumerated but no longer exists; drop or move its row"
            );
        }
    }
}

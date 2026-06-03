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

/// One assembled batch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Batch {
    pub jobs: Vec<String>,
    pub root_drvs: Vec<String>,
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
        current.root_drvs.push(job.drv_path.clone());
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
    ///  1. src/run/batch.rs — the struct declaration itself;
    ///  2. src/run/timeline.rs — the timed dispatcher's initial-dispatch
    ///     construction (roots-only floor; no adjacency data exists for
    ///     recorded request targets);
    ///  3. src/run/timeline.rs — its confirmation-retry sibling (same
    ///     floor, same rationale).
    ///
    /// Test-zone occurrences are fixtures: submitter.rs scripted batches
    /// (incl. the under-/over-keyed chokepoint fixtures), submit.rs
    /// chokepoint records, mod.rs stage harnesses.
    #[test]
    fn batch_construction_sites_are_enumerated() {
        // Built at runtime so this test's own strings cannot match it.
        let needle = format!("{}{}", "Batch ", "{");
        let allowed: std::collections::BTreeMap<&str, (usize, usize)> = [
            ("src/run/batch.rs", (1, 0)),
            ("src/run/timeline.rs", (2, 0)),
            ("src/run/submitter.rs", (0, 4)),
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
}

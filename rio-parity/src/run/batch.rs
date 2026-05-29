//! Greedy batch assembly with the dual cap: batches are capped on BOTH job
//! count and the estimated merged-closure drv node count, so a single
//! `nix build` invocation never carries more derivations than the gateway
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
    /// Exact size of the union of {target drv} ∪ dep drvs over the batch —
    /// the merged-DAG node estimate the caps act on.
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
}

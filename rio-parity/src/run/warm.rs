//! Warm stage (leaf mode): batched root submissions of the warm set's
//! producing derivations against the warm tenant's store URL, so the
//! scheduler's bulk substitution pulls upstream-built dependencies into
//! rio-store before the leaf builds run — no new rio-side code is
//! involved, the warm builds are ordinary submissions. Reuses the
//! [`Submitter`] and [`ResultReader`] facades; each warm root carries a
//! node estimate of 1 because a warm batch is a roots-only merge (every
//! root is expected to be substitutable, so no dependency derivations
//! ride along).
//!
//! Per-path dispositions land in warm.jsonl: `not-found-upstream` was
//! already written by the hydra-truth sweep, `already-present` comes
//! from the plan-time validity snapshot, `no-static-producer` covers
//! paths with no producing derivation in the dep closure, and
//! `substituted` / `built-fallback` / `failed-after-retries` come from
//! reading the warm builds' per-drv observations. The stage is complete
//! when every warm-set path has a terminal disposition; a path that
//! cannot be warmed never wedges the campaign — its leaf job is still
//! attempted and measured.
//!
//! Warm batches are deliberately outside the engine's stall/queued
//! watchdog: nothing here registers warm roots with it. Each warm batch
//! is bounded by the per-batch child timeout (`batch_timeout_hours`)
//! instead, and every root receives a terminal disposition as soon as its
//! batch settles, so a slow or wedged warm batch is cut off by that
//! timeout rather than re-queued.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};

use super::batch::{Batch, PendingJob, assemble_batches};
use super::model::{
    BATCH_KIND_WARM, DISPOSITION_ALREADY_PRESENT, DISPOSITION_BUILT_FALLBACK,
    DISPOSITION_FAILED_AFTER_RETRIES, DISPOSITION_NO_STATIC_PRODUCER,
    DISPOSITION_NOT_FOUND_UPSTREAM, DISPOSITION_SUBSTITUTED, STATUS_CANCELLED, STATUS_COMPLETED,
    STATUS_DEPENDENCY_FAILED, STATUS_POISONED, STATUS_SKIPPED, WarmEntry, now_rfc3339,
};
use super::reader::ResultReader;
use super::spec::Knobs;
use super::state::{StateDir, StateFile};
use super::submit::{SubmitTracker, submit_one_batch};
use super::submitter::Submitter;

/// What the warm stage still has to do, computed by [`warm_work`].
#[derive(Debug, Default)]
pub struct WarmWork {
    /// Warm-set paths that still need a warm submission.
    pub to_warm: Vec<String>,
    /// Producing drv → the still-to-warm paths it produces (the warm batch
    /// roots).
    pub by_drv: BTreeMap<String, Vec<String>>,
}

/// Compute what still needs warming: warm-set paths with no disposition yet,
/// minus paths already valid in rio-store at the plan-time snapshot
/// (recorded `already-present` here) and paths with no static producing
/// derivation (recorded `no-static-producer` here; content-addressed /
/// floating outputs have no drv to submit). Paths the hydra-truth sweep
/// already classified `not-found-upstream` carry a disposition and are
/// skipped like any other settled path.
pub fn warm_work(
    warm_set: &[String],
    producer: &BTreeMap<String, String>,
    plan_valid_paths: &HashSet<String>,
    state: &StateDir,
) -> Result<WarmWork> {
    let existing: HashSet<String> = state
        .load_jsonl::<WarmEntry>(StateFile::Warm)?
        .into_iter()
        .map(|w| w.path)
        .collect();
    let mut work = WarmWork::default();
    for path in warm_set {
        if existing.contains(path) {
            continue;
        }
        if plan_valid_paths.contains(path) {
            state.append_jsonl(
                StateFile::Warm,
                &WarmEntry {
                    path: path.clone(),
                    drv_path: producer.get(path).cloned(),
                    disposition: DISPOSITION_ALREADY_PRESENT.into(),
                    batch_id: None,
                    observed_at: now_rfc3339(),
                },
            )?;
            continue;
        }
        let Some(drv) = producer.get(path) else {
            // Content-addressed / floating outputs have no static producer
            // mapping in the dep closure — there is no derivation to submit
            // for them, so they are recorded and excluded from warming.
            state.append_jsonl(
                StateFile::Warm,
                &WarmEntry {
                    path: path.clone(),
                    drv_path: None,
                    disposition: DISPOSITION_NO_STATIC_PRODUCER.into(),
                    batch_id: None,
                    observed_at: now_rfc3339(),
                },
            )?;
            continue;
        };
        work.to_warm.push(path.clone());
        work.by_drv
            .entry(drv.clone())
            .or_default()
            .push(path.clone());
    }
    Ok(work)
}

/// Derive the warm-tenant store URL the warm stage dials: the build-tenant
/// gateway URL with its `ssh-key` query parameter re-pointed at
/// `<ssh_key_dir>/<warm_tenant>`. Tenant selection on the gateway is by SSH
/// key, not by URL, so everything else about the URL is kept as-is.
///
/// The tenant name becomes a single path component under the key directory,
/// so it is restricted to a plain file-name alphabet (ASCII alphanumerics,
/// `-`, `_`); anything else — path separators, `..`, an empty name — is
/// rejected so a crafted tenant value can never point the key path outside
/// the directory. The key file itself is deliberately not checked for
/// existence here: this only derives the URL string.
pub fn warm_store_url(
    gateway_store_url: &str,
    ssh_key_dir: &std::path::Path,
    warm_tenant: &str,
) -> Result<String> {
    ensure!(
        !warm_tenant.is_empty()
            && warm_tenant
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "invalid warm tenant name {warm_tenant:?}: must be non-empty and contain only \
         ASCII alphanumerics, '-' or '_'"
    );
    let key = ssh_key_dir.join(warm_tenant);
    let key = key.display();
    Ok(match gateway_store_url.split_once('?') {
        Some((base, query)) => {
            let mut params: Vec<&str> = query
                .split('&')
                .filter(|p| !p.is_empty() && !p.starts_with("ssh-key="))
                .collect();
            let warm_key = format!("ssh-key={key}");
            params.push(&warm_key);
            format!("{base}?{}", params.join("&"))
        }
        None => format!("{gateway_store_url}?ssh-key={key}"),
    })
}

/// Map a warm root drv's observed scheduler status to the per-path
/// disposition; `None` for a status that is not terminal yet.
pub fn disposition_for(status: &str, exec_id_present: bool) -> Option<&'static str> {
    match status {
        // Completed without an observed execution = the scheduler
        // substituted it (or short-circuited it via CA early cutoff);
        // completed WITH an execution = substitution fell through and the
        // scheduler built it as a fallback.
        STATUS_COMPLETED | STATUS_SKIPPED => Some(if exec_id_present {
            DISPOSITION_BUILT_FALLBACK
        } else {
            DISPOSITION_SUBSTITUTED
        }),
        STATUS_POISONED | STATUS_DEPENDENCY_FAILED | STATUS_CANCELLED => {
            Some(DISPOSITION_FAILED_AFTER_RETRIES)
        }
        _ => None,
    }
}

/// Run the warm stage to completion: every path still needing warming gets a
/// terminal disposition appended to warm.jsonl, batch by batch. Roots whose
/// warm build did not reach a terminal status (or whose batch never produced
/// a build id) are recorded `failed-after-retries` rather than retried — an
/// unwarmable path never wedges the campaign; its leaf job is still
/// attempted.
#[allow(clippy::too_many_arguments)]
pub async fn run_warm(
    state: Arc<StateDir>,
    submitter: Arc<dyn Submitter>,
    reader: Arc<dyn ResultReader>,
    warm_store_url: &str,
    warm_set: &[String],
    producer: &BTreeMap<String, String>,
    plan_valid_paths: &HashSet<String>,
    knobs: &Knobs,
    batch_seq: Arc<AtomicU64>,
) -> Result<()> {
    let work = warm_work(warm_set, producer, plan_valid_paths, &state)?;
    if work.to_warm.is_empty() {
        tracing::info!("warm stage: nothing left to warm");
        log_disposition_summary(&state)?;
        return Ok(());
    }
    tracing::info!(
        paths = work.to_warm.len(),
        roots = work.by_drv.len(),
        batch_max_jobs = knobs.batch_max_jobs,
        batch_max_nodes = knobs.batch_max_nodes,
        "warm stage starting"
    );
    // One PendingJob per producing drv, node estimate 1 (roots-only merge:
    // no dependency drvs ride along, so the merged-DAG estimate is just the
    // root count). The "job" name of a warm pending job is its drv path —
    // warm roots have no workload-unit job name of their own.
    let jobs: Vec<PendingJob> = work
        .by_drv
        .keys()
        .map(|drv| PendingJob {
            job: drv.clone(),
            drv_path: drv.clone(),
            dep_drvs: vec![],
        })
        .collect();
    let batches: Vec<Batch> = assemble_batches(&jobs, knobs.batch_max_jobs, knobs.batch_max_nodes);
    // The warm stage keeps its OWN SubmitTracker: in-flight / cool-down
    // bookkeeping must not leak into the build-submit loop's tracker (or
    // vice versa). The post-settlement cool-down is inert here — warm
    // batches are submitted sequentially and never re-offered — so the
    // value below only keeps submit_one_batch's bookkeeping consistent.
    let tracker = SubmitTracker::default();
    let cooldown = Duration::from_secs(knobs.collect_poll_secs.max(1));
    // Same child-deadline clamping as the submit loop: spec validation
    // rejects a non-positive batch_timeout_hours, but a Knobs value built
    // outside a loaded spec must never become a 0-second deadline that
    // kills every child the moment it spawns.
    let timeout = Duration::from_secs((knobs.batch_timeout_hours * 3600.0).max(1.0) as u64);

    let batches_total = batches.len();
    for (batch_index, batch) in batches.into_iter().enumerate() {
        let batch_id = batch_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tracing::info!(
            batch_id,
            batch_index = batch_index + 1,
            batches_total,
            roots = batch.root_drvs.len(),
            "starting warm batch"
        );
        let record = submit_one_batch(
            &state,
            submitter.as_ref(),
            &tracker,
            warm_store_url,
            BATCH_KIND_WARM,
            batch_id,
            batch.clone(),
            timeout,
            cooldown,
        )
        .await?;
        // Per-root dispositions from the warm build's per-drv observations.
        let observations: HashMap<String, super::reader::DrvObservation> = match &record.build_id {
            Some(build_id) => reader
                .read_build(build_id, &batch.root_drvs)
                .await
                .with_context(|| format!("read back warm batch {batch_id} (build {build_id})"))?
                .into_iter()
                .map(|o| (o.drv_path.clone(), o))
                .collect(),
            None => {
                tracing::warn!(
                    batch_id,
                    roots = batch.root_drvs.len(),
                    "warm batch settled without a build id; recording its paths \
                     failed-after-retries (their leaf jobs are still attempted)"
                );
                HashMap::new()
            }
        };
        for root in &batch.root_drvs {
            let disposition = match observations.get(root) {
                Some(o) => disposition_for(&o.status, o.exec_id.is_some()).unwrap_or_else(|| {
                    tracing::warn!(
                        batch_id,
                        root,
                        status = %o.status,
                        "warm root has no terminal status after its batch settled; recording \
                         failed-after-retries"
                    );
                    DISPOSITION_FAILED_AFTER_RETRIES
                }),
                // Only reachable when the batch settled without a build id
                // (the read-back above was skipped): with a build id the
                // reader returns an observation for every requested root, so
                // a root with no derivation rows arrives as an empty-status
                // observation and is handled by the arm above. The paths stay
                // un-warmed and are recorded as failed so the stage can
                // finish — the leaf jobs depending on them are still
                // attempted.
                None => DISPOSITION_FAILED_AFTER_RETRIES,
            };
            for path in work.by_drv.get(root).into_iter().flatten() {
                state.append_jsonl(
                    StateFile::Warm,
                    &WarmEntry {
                        path: path.clone(),
                        drv_path: Some(root.clone()),
                        disposition: disposition.to_string(),
                        batch_id: Some(batch_id),
                        observed_at: now_rfc3339(),
                    },
                )?;
            }
        }
    }
    log_disposition_summary(&state)?;
    Ok(())
}

/// End-of-stage summary: count every warm.jsonl disposition (including the
/// pre-classified ones written by hydra-truth and the plan snapshot) so the
/// log shows how the whole warm set settled.
fn log_disposition_summary(state: &StateDir) -> Result<()> {
    let entries: Vec<WarmEntry> = state.load_jsonl(StateFile::Warm)?;
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in &entries {
        *counts.entry(entry.disposition.as_str()).or_default() += 1;
    }
    let count = |d: &str| counts.get(d).copied().unwrap_or(0);
    tracing::info!(
        paths = entries.len(),
        already_present = count(DISPOSITION_ALREADY_PRESENT),
        not_found_upstream = count(DISPOSITION_NOT_FOUND_UPSTREAM),
        no_static_producer = count(DISPOSITION_NO_STATIC_PRODUCER),
        substituted = count(DISPOSITION_SUBSTITUTED),
        built_fallback = count(DISPOSITION_BUILT_FALLBACK),
        failed_after_retries = count(DISPOSITION_FAILED_AFTER_RETRIES),
        "warm stage complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::model::BatchRecord;
    use crate::run::reader::DrvObservation;
    use crate::run::reader::test_support::FakeReader;
    use crate::run::submitter::BatchOutcome;
    use crate::run::submitter::test_support::FakeSubmitter;

    fn p(name: &str) -> String {
        format!("/nix/store/{}-{name}", "d".repeat(32))
    }

    fn drv(name: &str) -> String {
        format!("/nix/store/{}-{name}.drv", "e".repeat(32))
    }

    #[test]
    fn warm_work_prefilters_and_maps_producers() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        // hydra-truth already classified p3 as not-found-upstream.
        state
            .append_jsonl(
                StateFile::Warm,
                &WarmEntry {
                    path: p("p3"),
                    drv_path: None,
                    disposition: DISPOSITION_NOT_FOUND_UPSTREAM.into(),
                    batch_id: None,
                    observed_at: now_rfc3339(),
                },
            )
            .unwrap();
        let warm_set = vec![p("p1"), p("p2"), p("p3"), p("p4"), p("p5")];
        let producer: BTreeMap<String, String> = [
            (p("p1"), drv("d1")),
            (p("p2"), drv("d1")),
            (p("p4"), drv("d4")),
        ]
        .into();
        let plan_valid: HashSet<String> = [p("p2")].into();
        let work = warm_work(&warm_set, &producer, &plan_valid, &state).unwrap();
        // p1 and p4 still need warming; p2 was already present, p3 was
        // pre-classified by hydra-truth, p5 has no static producer.
        assert_eq!(work.to_warm, vec![p("p1"), p("p4")]);
        assert_eq!(work.by_drv[&drv("d1")], vec![p("p1")]);
        assert_eq!(work.by_drv[&drv("d4")], vec![p("p4")]);
        let entries: Vec<WarmEntry> = state.load_jsonl(StateFile::Warm).unwrap();
        let disposition_of = |path: String| {
            entries
                .iter()
                .find(|e| e.path == path)
                .map(|e| e.disposition.clone())
        };
        assert_eq!(
            disposition_of(p("p2")).as_deref(),
            Some(DISPOSITION_ALREADY_PRESENT)
        );
        assert_eq!(
            disposition_of(p("p5")).as_deref(),
            Some(DISPOSITION_NO_STATIC_PRODUCER)
        );
        // The paths still to warm have no disposition yet.
        assert_eq!(disposition_of(p("p1")), None);
        assert_eq!(disposition_of(p("p4")), None);
    }

    #[test]
    fn warm_store_url_repoints_the_ssh_key_at_the_warm_tenant() {
        // The launch-written gateway URL keeps every parameter except the
        // ssh-key, which moves to the warm tenant's key file.
        assert_eq!(
            warm_store_url(
                "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/etc/rio/parity-ssh/parity-leaf",
                std::path::Path::new("/etc/rio/parity-ssh"),
                "parity-warm",
            )
            .unwrap(),
            "ssh-ng://rio@rio-gateway.rio-system.svc:22?compress=true&ssh-key=/etc/rio/parity-ssh/parity-warm"
        );
        // A gateway URL with no query string still gains the warm key.
        assert_eq!(
            warm_store_url(
                "ssh-ng://rio@gw:22",
                std::path::Path::new("/keys"),
                "parity-warm"
            )
            .unwrap(),
            "ssh-ng://rio@gw:22?ssh-key=/keys/parity-warm"
        );
    }

    #[test]
    fn warm_store_url_rejects_tenant_names_outside_the_file_name_alphabet() {
        // The tenant name is joined onto the key directory as a path
        // component; traversal sequences, separators, and empty names must
        // never reach that join.
        for bad in ["../evil", "a/b", "", "warm tenant"] {
            let err = warm_store_url("ssh-ng://rio@gw:22", std::path::Path::new("/keys"), bad)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("invalid warm tenant name") && err.contains(bad),
                "tenant {bad:?} must be rejected with an error naming it: {err}"
            );
        }
    }

    #[test]
    fn disposition_mapping() {
        assert_eq!(
            disposition_for(STATUS_COMPLETED, false),
            Some(DISPOSITION_SUBSTITUTED)
        );
        assert_eq!(
            disposition_for(STATUS_COMPLETED, true),
            Some(DISPOSITION_BUILT_FALLBACK)
        );
        assert_eq!(
            disposition_for(STATUS_SKIPPED, false),
            Some(DISPOSITION_SUBSTITUTED)
        );
        assert_eq!(
            disposition_for(STATUS_POISONED, false),
            Some(DISPOSITION_FAILED_AFTER_RETRIES)
        );
        assert_eq!(
            disposition_for(STATUS_DEPENDENCY_FAILED, false),
            Some(DISPOSITION_FAILED_AFTER_RETRIES)
        );
        assert_eq!(
            disposition_for(STATUS_CANCELLED, true),
            Some(DISPOSITION_FAILED_AFTER_RETRIES)
        );
        assert_eq!(disposition_for("running", false), None);
        assert_eq!(disposition_for("", false), None);
    }

    #[tokio::test]
    async fn run_warm_writes_terminal_dispositions() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let submitter = Arc::new(FakeSubmitter::default());
        let build_id = "0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a";
        submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some(build_id.into()),
            ..BatchOutcome::default()
        }));
        let reader = Arc::new(FakeReader::default());
        reader.set(
            build_id,
            DrvObservation {
                drv_path: drv("d1"),
                status: STATUS_COMPLETED.into(),
                ..DrvObservation::default()
            },
        );
        reader.set(
            build_id,
            DrvObservation {
                drv_path: drv("d4"),
                status: STATUS_POISONED.into(),
                failed_builders: Some(vec![]),
                ..DrvObservation::default()
            },
        );
        // d6 is deliberately NOT scripted: the read-back returns an
        // empty-status observation for it (no derivation rows in the build).

        let warm_set = vec![p("p1"), p("p4"), p("p6")];
        let producer: BTreeMap<String, String> = [
            (p("p1"), drv("d1")),
            (p("p4"), drv("d4")),
            (p("p6"), drv("d6")),
        ]
        .into();
        run_warm(
            state.clone(),
            submitter.clone(),
            reader,
            "ssh-ng://rio@gw:22?ssh-key=/warm",
            &warm_set,
            &producer,
            &HashSet::new(),
            &Knobs::default(),
            Arc::new(AtomicU64::new(100)),
        )
        .await
        .unwrap();
        // The warm submission went to the warm store URL as one batch of
        // three roots; the batch record carries the warm kind and the
        // seeded id.
        {
            let submitted = submitter.submitted.lock().unwrap();
            assert_eq!(submitted.len(), 1);
            assert_eq!(submitted[0].0, "ssh-ng://rio@gw:22?ssh-key=/warm");
            assert_eq!(
                submitted[0].1.root_drvs,
                vec![drv("d1"), drv("d4"), drv("d6")]
            );
        }
        let batches: Vec<BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].kind, BATCH_KIND_WARM);
        assert_eq!(batches[0].batch_id, 100);

        let entries: Vec<WarmEntry> = state.load_jsonl(StateFile::Warm).unwrap();
        let by_path: HashMap<String, String> = entries
            .into_iter()
            .map(|e| (e.path, e.disposition))
            .collect();
        assert_eq!(by_path[&p("p1")], DISPOSITION_SUBSTITUTED);
        assert_eq!(by_path[&p("p4")], DISPOSITION_FAILED_AFTER_RETRIES);
        // The build id existed but the build recorded no rows for d6: the
        // empty-status observation still lands a terminal disposition.
        assert_eq!(by_path[&p("p6")], DISPOSITION_FAILED_AFTER_RETRIES);
        // Idempotent: a second pass finds nothing left to warm.
        let again = warm_work(&warm_set, &producer, &HashSet::new(), &state).unwrap();
        assert!(again.to_warm.is_empty());
    }

    /// An engine-side submission failure (no build id at all) and a root the
    /// scheduler ended up building (exec_id present) both still land
    /// terminal dispositions: nothing is retried and nothing wedges the
    /// stage.
    #[tokio::test]
    async fn run_warm_handles_submission_failures_and_built_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(StateDir::new(dir.path()).unwrap());
        let submitter = Arc::new(FakeSubmitter::default());
        let build_id = "0193e4a2-7c1b-7d20-9b3a-1f2e3d4c5b6a";
        // batch_max_jobs = 1 forces two batches: the FIRST (root d1) settles
        // with a build id, the SECOND (root d4) fails at submission. Scripted
        // outcomes pop from the back, so the second batch's error goes first.
        submitter
            .outcomes
            .lock()
            .unwrap()
            .push(Err(anyhow::anyhow!("ssh handshake failed")));
        submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some(build_id.into()),
            ..BatchOutcome::default()
        }));
        let reader = Arc::new(FakeReader::default());
        reader.set(
            build_id,
            DrvObservation {
                drv_path: drv("d1"),
                status: STATUS_COMPLETED.into(),
                exec_id: Some("exec-1".into()),
                ..DrvObservation::default()
            },
        );
        let warm_set = vec![p("p1"), p("p4")];
        let producer: BTreeMap<String, String> =
            [(p("p1"), drv("d1")), (p("p4"), drv("d4"))].into();
        let knobs = Knobs {
            batch_max_jobs: 1,
            ..Knobs::default()
        };
        run_warm(
            state.clone(),
            submitter.clone(),
            reader,
            "ssh-ng://rio@gw:22?ssh-key=/warm",
            &warm_set,
            &producer,
            &HashSet::new(),
            &knobs,
            Arc::new(AtomicU64::new(0)),
        )
        .await
        .unwrap();
        assert_eq!(submitter.submitted.lock().unwrap().len(), 2);
        let entries: Vec<WarmEntry> = state.load_jsonl(StateFile::Warm).unwrap();
        let by_path: HashMap<String, String> = entries
            .into_iter()
            .map(|e| (e.path, e.disposition))
            .collect();
        // d1 completed WITH an execution → the scheduler built it as a
        // fallback instead of substituting it.
        assert_eq!(by_path[&p("p1")], DISPOSITION_BUILT_FALLBACK);
        // d4's batch never got a build id → terminal failed-after-retries.
        assert_eq!(by_path[&p("p4")], DISPOSITION_FAILED_AFTER_RETRIES);
    }
}

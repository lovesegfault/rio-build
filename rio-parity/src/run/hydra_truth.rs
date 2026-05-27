//! Hydra-truth stage: per-path upstream ground truth from cache.nixos.org
//! narinfos.
//!
//! For every in-scope target output path and every warm-set path, the sweep
//! fetches the upstream narinfo (presence, `NarHash`, `NarSize`, `Deriver`)
//! with bounded concurrency and per-path retries, appending each result to
//! `hydra.jsonl` so an interrupted or resumed campaign never re-fetches a
//! path it already has. Warm-set paths that are absent upstream are
//! pre-classified `not-found-upstream` in `warm.jsonl`: the warm stage must
//! never submit them, because substituting a path that upstream does not
//! serve cannot succeed, and building it locally would mask exactly the
//! work the campaign is trying to measure.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;

use super::model::{
    DISPOSITION_NOT_FOUND_UPSTREAM, HydraEntry, HydraOutcome, WarmEntry, now_rfc3339,
};
use super::state::{StateDir, StateFile};

/// Minimal narinfo fetch surface for the sweep: `Some(raw narinfo text)` on
/// HTTP 200, `None` on 404 (path not published upstream), and `Err` for
/// anything else (treated as transient and retried by the sweep).
#[async_trait]
pub trait NarinfoSource: Send + Sync {
    async fn fetch_narinfo_text(&self, store_path: &str) -> Result<Option<String>>;
}

/// The production source is the existing cache.nixos.org client (descriptive
/// User-Agent, shared HTTP client construction); the sweep layers the disk
/// cache, bounded concurrency, and retries on top of it.
#[async_trait]
impl NarinfoSource for crate::nixcache::NixCacheClient {
    async fn fetch_narinfo_text(&self, store_path: &str) -> Result<Option<String>> {
        crate::nixcache::NixCacheClient::fetch_narinfo_text(self, store_path).await
    }
}

/// Parse one fetched narinfo into a [`HydraEntry`] for `path`.
///
/// `None` (404) is recorded as not found. A 200 body that fails to parse is
/// recorded as found with no usable NAR identity: the path demonstrably
/// exists upstream, so treating it as absent would mis-classify the job, but
/// its hash cannot be compared.
pub fn entry_from_narinfo(path: &str, text: Option<&str>) -> HydraEntry {
    match text {
        None => HydraEntry {
            path: path.to_string(),
            found: false,
            nar_hash: None,
            nar_size: None,
            deriver: None,
            fetched_at: now_rfc3339(),
        },
        Some(text) => match rio_nix::narinfo::NarInfo::parse(text) {
            Ok(ni) => HydraEntry {
                path: path.to_string(),
                found: true,
                nar_hash: Some(ni.nar_hash),
                nar_size: Some(ni.nar_size),
                deriver: ni.deriver,
                fetched_at: now_rfc3339(),
            },
            Err(e) => {
                tracing::warn!(path, error = %e, "malformed narinfo treated as found (hash unusable)");
                HydraEntry {
                    path: path.to_string(),
                    found: true,
                    nar_hash: None,
                    nar_size: None,
                    deriver: None,
                    fetched_at: now_rfc3339(),
                }
            }
        },
    }
}

/// Retry policy for transient cache.nixos.org/Fastly errors (5xx, connection
/// resets). `Backoff` has no `Default` by design — per-site constants stay
/// local (`rio-common/src/backoff.rs`). Full jitter desynchronizes the
/// concurrent fetchers so retries don't re-arrive as a synchronized burst.
const NARINFO_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_millis(500),
    mult: 2.0,
    cap: Duration::from_secs(15),
    jitter: rio_common::backoff::Jitter::Full,
};

/// Fetch one path's narinfo with bounded retries (transient errors only).
async fn fetch_one(
    source: &dyn NarinfoSource,
    path: &str,
    max_attempts: u32,
) -> Result<HydraEntry> {
    // Validate up front: a malformed store path is an eval-set bug, not a
    // transient fetch error — fail immediately instead of retrying it.
    let _ = rio_nix::store_path::StorePath::parse(path)
        .map_err(|e| anyhow::anyhow!("bad store path {path}: {e}"))?;
    let mut attempt = 0u32;
    loop {
        match source.fetch_narinfo_text(path).await {
            Ok(text) => return Ok(entry_from_narinfo(path, text.as_deref())),
            Err(e) if attempt + 1 < max_attempts => {
                let delay = NARINFO_BACKOFF.duration(attempt);
                tracing::warn!(path, attempt, error = %e, "narinfo fetch failed; retrying");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => return Err(e.context(format!("narinfo fetch for {path}"))),
        }
    }
}

/// Outcome counters for progress reporting.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepStats {
    /// Paths fetched from upstream this run.
    pub fetched: usize,
    /// Paths already present in hydra.jsonl (not re-fetched).
    pub cached: usize,
    /// Newly fetched paths with an upstream narinfo.
    pub found: usize,
    /// Newly fetched paths absent upstream (404).
    pub not_found: usize,
}

/// Run the narinfo sweep over `target_paths` ∪ `warm_paths`, appending new
/// entries to `hydra.jsonl` (the disk cache) and pre-classifying warm-set
/// paths that are absent upstream as `not-found-upstream` in `warm.jsonl`.
///
/// Each entry is appended as soon as its fetch completes, so an aborted sweep
/// resumes from wherever it got to; paths already in `hydra.jsonl` are never
/// re-fetched. A path that still fails after `max_attempts` aborts the sweep.
pub async fn run_hydra_truth(
    state: &StateDir,
    source: &dyn NarinfoSource,
    target_paths: &[String],
    warm_paths: &[String],
    concurrency: usize,
    max_attempts: u32,
) -> Result<SweepStats> {
    // Load the disk cache (resume): paths already swept are not re-fetched.
    let existing: Vec<HydraEntry> = state.load_jsonl(StateFile::Hydra)?;
    let mut by_path: HashMap<String, HydraEntry> =
        existing.into_iter().map(|e| (e.path.clone(), e)).collect();
    let already_warm_classified: HashSet<String> = state
        .load_jsonl::<WarmEntry>(StateFile::Warm)?
        .into_iter()
        .map(|w| w.path)
        .collect();

    let mut want: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for p in target_paths.iter().chain(warm_paths.iter()) {
        if !seen.insert(p.as_str()) {
            continue;
        }
        if !by_path.contains_key(p) {
            want.push(p.clone());
        }
    }
    let mut stats = SweepStats {
        cached: seen.len() - want.len(),
        ..SweepStats::default()
    };

    let concurrency = concurrency.max(1);
    let mut fetches = futures_util::stream::iter(want.into_iter().map(|path| async move {
        let entry = fetch_one(source, &path, max_attempts).await;
        (path, entry)
    }))
    .buffer_unordered(concurrency);

    while let Some((path, entry)) = fetches.next().await {
        let entry = entry?;
        stats.fetched += 1;
        if entry.found {
            stats.found += 1;
        } else {
            stats.not_found += 1;
        }
        state.append_jsonl(StateFile::Hydra, &entry)?;
        by_path.insert(path, entry);
    }

    // Warm pre-classification: a warm-set path with no upstream narinfo can
    // never be substituted, so it is recorded up front and never submitted to
    // the warm stage. Iteration is sorted so warm.jsonl appends are
    // deterministic for a given input set.
    let warm_set: BTreeSet<&String> = warm_paths.iter().collect();
    for path in warm_set {
        if already_warm_classified.contains(path) {
            continue;
        }
        if let Some(entry) = by_path.get(path)
            && !entry.found
        {
            state.append_jsonl(
                StateFile::Warm,
                &WarmEntry {
                    path: path.clone(),
                    drv_path: None,
                    disposition: DISPOSITION_NOT_FOUND_UPSTREAM.into(),
                    batch_id: None,
                    observed_at: now_rfc3339(),
                },
            )?;
        }
    }
    Ok(stats)
}

/// Hydra-side outcome for one job, derived from its declared outputs and the
/// swept narinfo cache: a job is hydra-built when every declared output has
/// an upstream narinfo; anything less is unknown (absence of a narinfo is
/// absence of evidence, not proof of failure). An exact `buildstatus` (scoped
/// campaigns that recorded it at eval time) overrides the narinfo heuristic
/// and is the only way to produce [`HydraOutcome::Failed`].
pub fn hydra_outcome_for_job(
    outputs: &BTreeMap<String, String>,
    by_path: &HashMap<String, HydraEntry>,
    buildstatus: Option<i64>,
) -> HydraOutcome {
    if let Some(status) = buildstatus {
        return if status == 0 {
            HydraOutcome::Built
        } else {
            HydraOutcome::Failed
        };
    }
    if outputs.is_empty() {
        return HydraOutcome::Unknown;
    }
    if outputs
        .values()
        .all(|p| by_path.get(p).is_some_and(|e| e.found))
    {
        HydraOutcome::Built
    } else {
        HydraOutcome::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::evalset_input::test_fixtures::fake_hash;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory narinfo source: store path → narinfo text. Counts fetches
    /// and can fail the first N attempts per path.
    #[derive(Default)]
    struct FakeSource {
        bodies: HashMap<String, String>,
        fetches: AtomicUsize,
        fail_first: Mutex<HashMap<String, u32>>,
    }

    #[async_trait]
    impl NarinfoSource for FakeSource {
        async fn fetch_narinfo_text(&self, store_path: &str) -> Result<Option<String>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            if let Some(n) = self.fail_first.lock().unwrap().get_mut(store_path)
                && *n > 0
            {
                *n -= 1;
                anyhow::bail!("transient 502");
            }
            Ok(self.bodies.get(store_path).cloned())
        }
    }

    fn store_path(name: &str) -> String {
        format!("/nix/store/{}-{name}", fake_hash(name))
    }

    fn narinfo_text(path: &str) -> String {
        format!(
            "StorePath: {path}\nURL: nar/x.nar.zst\nCompression: zstd\n\
             NarHash: sha256:{}\nNarSize: 4242\nReferences: \n\
             Deriver: {}-x.drv\nSig: cache.nixos.org-1:abc\n",
            "0".repeat(52),
            "c".repeat(32),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn sweep_caches_classifies_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let target = store_path("app");
        let warm_found = store_path("dep1");
        let warm_missing = store_path("dep2");

        let mut source = FakeSource::default();
        source.bodies.insert(target.clone(), narinfo_text(&target));
        source
            .bodies
            .insert(warm_found.clone(), narinfo_text(&warm_found));
        // warm_missing: no body → 404. target: fail once, then succeed (retry path).
        source.fail_first.lock().unwrap().insert(target.clone(), 1);

        let stats = run_hydra_truth(
            &state,
            &source,
            std::slice::from_ref(&target),
            &[warm_found.clone(), warm_missing.clone()],
            8,
            3,
        )
        .await
        .unwrap();
        assert_eq!(stats.fetched, 3);
        assert_eq!(stats.found, 2);
        assert_eq!(stats.not_found, 1);

        // hydra.jsonl has 3 entries; warm.jsonl pre-classified exactly the missing path.
        let hydra: Vec<HydraEntry> = state.load_jsonl(StateFile::Hydra).unwrap();
        assert_eq!(hydra.len(), 3);
        let warm: Vec<WarmEntry> = state.load_jsonl(StateFile::Warm).unwrap();
        assert_eq!(warm.len(), 1);
        assert_eq!(warm[0].path, warm_missing);
        assert_eq!(warm[0].disposition, DISPOSITION_NOT_FOUND_UPSTREAM);

        // Second sweep: everything served from the disk cache, zero new fetches.
        let before = source.fetches.load(Ordering::SeqCst);
        let stats2 = run_hydra_truth(
            &state,
            &source,
            std::slice::from_ref(&target),
            &[warm_found.clone(), warm_missing.clone()],
            8,
            3,
        )
        .await
        .unwrap();
        assert_eq!(stats2.fetched, 0);
        assert_eq!(stats2.cached, 3);
        assert_eq!(source.fetches.load(Ordering::SeqCst), before);
        // No duplicate warm classification.
        let warm2: Vec<WarmEntry> = state.load_jsonl(StateFile::Warm).unwrap();
        assert_eq!(warm2.len(), 1);
    }

    #[tokio::test]
    async fn malformed_store_path_fails_without_retry() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let source = FakeSource::default();
        let err = run_hydra_truth(
            &state,
            &source,
            &["not-a-store-path".to_string()],
            &[],
            4,
            3,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("bad store path"), "{err}");
        assert_eq!(
            source.fetches.load(Ordering::SeqCst),
            0,
            "a malformed path must not be fetched or retried"
        );
    }

    #[test]
    fn narinfo_parse_to_entry_and_job_outcome() {
        let p = store_path("x");
        let entry = entry_from_narinfo(&p, Some(&narinfo_text(&p)));
        assert!(entry.found);
        assert_eq!(entry.nar_size, Some(4242));
        assert!(entry.nar_hash.as_deref().unwrap().starts_with("sha256:"));
        let missing = entry_from_narinfo(&p, None);
        assert!(!missing.found);
        // A 200 body that isn't a parseable narinfo is still "found", just
        // with no usable NAR identity.
        let malformed = entry_from_narinfo(&p, Some("definitely not a narinfo"));
        assert!(malformed.found);
        assert!(malformed.nar_hash.is_none());

        let mut by_path = HashMap::new();
        by_path.insert(p.clone(), entry);
        let mut outputs = BTreeMap::new();
        outputs.insert("out".to_string(), p.clone());
        assert_eq!(
            hydra_outcome_for_job(&outputs, &by_path, None),
            HydraOutcome::Built
        );
        // A second output with no narinfo → unknown, not failed: absence of a
        // narinfo is absence of evidence.
        outputs.insert("dev".to_string(), store_path("y"));
        assert_eq!(
            hydra_outcome_for_job(&outputs, &by_path, None),
            HydraOutcome::Unknown
        );
        // No declared outputs → unknown.
        assert_eq!(
            hydra_outcome_for_job(&BTreeMap::new(), &by_path, None),
            HydraOutcome::Unknown
        );
        // Explicit buildstatus wins (scoped campaigns).
        assert_eq!(
            hydra_outcome_for_job(&outputs, &by_path, Some(0)),
            HydraOutcome::Built
        );
        assert_eq!(
            hydra_outcome_for_job(&outputs, &by_path, Some(1)),
            HydraOutcome::Failed
        );
    }
}

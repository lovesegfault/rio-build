//! Per-unit expected-outcome truth for the campaign, plus the upstream
//! coverage probe for the prefetch (warm) set.
//!
//! [`expected_outcomes_for_units`] loads each unit's expected outcome (and,
//! where the recorder captured them, expected per-output NAR identities)
//! from an open replay archive's `outcomes.jsonl` into the engine's
//! [`ExpectedOutcome`] / [`ExpectedSide`] shapes. Truth is baked in when the
//! archive is recorded, so this path performs no outbound queries.
//!
//! [`probe_warm_upstream_coverage`] is the supply stage's upstream-coverage
//! probe for the prefetch set: for every warm path it fetches the upstream
//! narinfo (presence only) with bounded concurrency and per-path retries.
//! Paths that are absent upstream are pre-classified `unavailable` in
//! `supply.jsonl`: the prefetch arm must never submit them, because
//! substituting a path that upstream does not serve cannot succeed, and
//! building it on the target would mask exactly the work the campaign is
//! trying to measure. Found paths are returned in memory and seed the
//! supply ladder's coverage set; nothing is cached on disk, so a resumed
//! campaign that has not finished its supply stage re-probes (resume costs
//! re-probing, never correctness).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;

use crate::archive::reader::ReplayArchive;
use crate::archive::schema::Capability;

use super::archive_input::ManifestEntry;
use super::model::{
    ExpectedOutcome, ExpectedOutput, ExpectedSide, SUPPLY_MECHANISM_NONE,
    SUPPLY_OUTCOME_UNAVAILABLE, SUPPLY_SOURCE_TARGET_SUBSTITUTER, SupplyEntry, now_rfc3339,
};
use super::state::{StateDir, StateFile};

/// Minimal narinfo fetch surface for the coverage probe: `Some(raw narinfo
/// text)` on HTTP 200, `None` on 404 (path not published upstream), and
/// `Err` for anything else (treated as transient and retried by the probe).
#[async_trait]
pub trait NarinfoSource: Send + Sync {
    async fn fetch_narinfo_text(&self, store_path: &str) -> Result<Option<String>>;
}

/// The production source is the existing binary-cache client (descriptive
/// User-Agent, shared HTTP client construction); the probe layers bounded
/// concurrency and retries on top of it.
#[async_trait]
impl NarinfoSource for crate::nixcache::NixCacheClient {
    async fn fetch_narinfo_text(&self, store_path: &str) -> Result<Option<String>> {
        crate::nixcache::NixCacheClient::fetch_narinfo_text(self, store_path).await
    }
}

/// Whether a fetched narinfo response says `path` is published upstream.
///
/// `None` (404) means absent. A 200 body that fails to parse still counts
/// as present: the path demonstrably exists upstream, so treating it as
/// absent would wrongly pre-classify it unavailable; the malformed body is
/// only logged.
pub fn upstream_presence(path: &str, text: Option<&str>) -> bool {
    match text {
        None => false,
        Some(text) => {
            if let Err(e) = rio_nix::narinfo::NarInfo::parse(text) {
                tracing::warn!(path, error = %e, "malformed narinfo treated as present upstream");
            }
            true
        }
    }
}

/// Retry policy for transient upstream/CDN errors (5xx, connection resets).
/// `Backoff` has no `Default` by design — per-site constants stay local
/// (`rio-common/src/backoff.rs`). Full jitter desynchronizes the concurrent
/// fetchers so retries don't re-arrive as a synchronized burst.
const NARINFO_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_millis(500),
    mult: 2.0,
    cap: Duration::from_secs(15),
    jitter: rio_common::backoff::Jitter::Full,
};

/// Fetch one path's upstream presence with bounded retries (transient
/// errors only).
async fn fetch_one(source: &dyn NarinfoSource, path: &str, max_attempts: u32) -> Result<bool> {
    // Validate up front: a malformed store path is an archive bug, not a
    // transient fetch error — fail immediately instead of retrying it.
    let _ = rio_nix::store_path::StorePath::parse(path)
        .map_err(|e| anyhow::anyhow!("bad store path {path}: {e}"))?;
    let mut attempt = 0u32;
    loop {
        match source.fetch_narinfo_text(path).await {
            Ok(text) => return Ok(upstream_presence(path, text.as_deref())),
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

/// Emit a probe-progress log line every this many completed fetches, so an
/// operator watching the logs can tell a long-but-moving probe apart from
/// one wedged in retry backoff.
const PROGRESS_LOG_EVERY: usize = 500;

/// Outcome counters for progress reporting.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepStats {
    /// Paths fetched from upstream this run.
    pub fetched: usize,
    /// Fetched paths with an upstream narinfo.
    pub found: usize,
    /// Fetched paths absent upstream (404).
    pub not_found: usize,
}

/// Result of the warm-set upstream-coverage probe.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WarmCoverage {
    /// Warm paths with an upstream narinfo: the seed coverage set for the
    /// supply stage's prefetch classification and upload ladder.
    pub found: BTreeSet<String>,
    /// Probe counters (progress logging and tests).
    pub stats: SweepStats,
}

/// Probe upstream coverage for the prefetch (warm) set, pre-classifying
/// paths that are absent upstream as `unavailable` in `supply.jsonl` and
/// returning the set of paths that are present.
///
/// A path that still fails after `max_attempts` aborts the probe. Paths
/// already carrying a supply.jsonl record (a resumed campaign) are never
/// pre-classified twice.
pub async fn probe_warm_upstream_coverage(
    state: &StateDir,
    source: &dyn NarinfoSource,
    warm_paths: &[String],
    concurrency: usize,
    max_attempts: u32,
) -> Result<WarmCoverage> {
    let already_classified: HashSet<String> = state
        .load_jsonl::<SupplyEntry>(StateFile::Supply)?
        .into_iter()
        .map(|entry| entry.path)
        .collect();

    // De-duplicate the input while keeping its order for the fetch stream.
    let mut want: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for p in warm_paths {
        if seen.insert(p.as_str()) {
            want.push(p.clone());
        }
    }
    let to_probe = want.len();
    tracing::info!(to_probe, "warm-set upstream-coverage probe starting");

    let mut coverage = WarmCoverage::default();
    let concurrency = concurrency.max(1);
    let mut fetches = futures_util::stream::iter(want.into_iter().map(|path| async move {
        let present = fetch_one(source, &path, max_attempts).await;
        (path, present)
    }))
    .buffer_unordered(concurrency);

    while let Some((path, present)) = fetches.next().await {
        let present = present?;
        coverage.stats.fetched += 1;
        if present {
            coverage.stats.found += 1;
            coverage.found.insert(path);
        } else {
            coverage.stats.not_found += 1;
        }
        if coverage.stats.fetched.is_multiple_of(PROGRESS_LOG_EVERY) {
            tracing::info!(
                fetched = coverage.stats.fetched,
                to_probe,
                found = coverage.stats.found,
                not_found = coverage.stats.not_found,
                "upstream-coverage probe progress"
            );
        }
    }

    // Prefetch pre-classification: a prefetch-set path with no upstream
    // narinfo can never be substituted, so it is recorded up front and the
    // prefetch arm never submits it. Iteration is sorted so supply.jsonl
    // appends are deterministic for a given input set.
    let warm_set: BTreeSet<&String> = warm_paths.iter().collect();
    for path in warm_set {
        if already_classified.contains(path) || coverage.found.contains(path) {
            continue;
        }
        state.append_jsonl(
            StateFile::Supply,
            &SupplyEntry {
                path: path.clone(),
                source: SUPPLY_SOURCE_TARGET_SUBSTITUTER.into(),
                mechanism: SUPPLY_MECHANISM_NONE.into(),
                outcome: SUPPLY_OUTCOME_UNAVAILABLE.into(),
                detail: Some("no upstream narinfo".into()),
                batch_id: None,
                bytes: None,
                observed_at: now_rfc3339(),
            },
        )?;
    }
    Ok(coverage)
}

/// Expected truth for one campaign unit, loaded from the replay archive:
/// the unit's expected outcome plus the record fragment (expected
/// per-output NAR identity) the classifier and the report consume.
#[derive(Debug, Clone, PartialEq)]
pub struct UnitTruth {
    pub outcome: ExpectedOutcome,
    pub side: ExpectedSide,
}

/// Truth entry for a unit with no usable outcome record: outcome unknown,
/// every declared output carried with no expected NAR identity.
fn unknown_truth(unit: &ManifestEntry) -> UnitTruth {
    UnitTruth {
        outcome: ExpectedOutcome::Unknown,
        side: ExpectedSide {
            outcome: ExpectedOutcome::Unknown.as_str().to_string(),
            outputs: unit
                .outputs
                .keys()
                .map(|name| (name.clone(), ExpectedOutput::default()))
                .collect(),
        },
    }
}

/// Load the expected outcome of every campaign unit from the open replay
/// archive's recorded truth, keyed by job name.
///
/// Truth is baked into the archive when it is recorded, so this performs
/// no outbound queries. Each unit's record is resolved by derivation alone
/// through the reader's canonical collapse over sessions — the session-less
/// record when one exists, otherwise the highest-numbered session's record
/// (see `ReplayArchive::expected_outcome_across_sessions`) — so truth a
/// recorder scoped to specific sessions is never invisible here. The
/// recorded outcome is carried through verbatim in the archive's neutral
/// vocabulary; how each value compares is the classifier's business.
///
/// The mapped [`ExpectedSide`] carries, for every output the unit declares,
/// the expected NAR identity from the record when one was recorded for
/// that output, and an empty [`ExpectedOutput`] otherwise. A `built` record
/// may carry hashes for all, some, or none of its outputs depending on
/// what the recorder could observe; a hash-less output is merely not
/// comparable and never changes the outcome.
///
/// Units with no outcome record, and every unit of an archive recorded
/// without the `expected_outcomes` capability, map to
/// [`ExpectedOutcome::Unknown`] with no expected output identities.
pub fn expected_outcomes_for_units(
    archive: &ReplayArchive,
    units: &[ManifestEntry],
) -> Result<BTreeMap<String, UnitTruth>> {
    if !Capability::ExpectedOutcomes.enabled_in(archive.capabilities()) {
        tracing::warn!(
            "archive lacks the expected_outcomes capability; treating every unit's truth as unknown"
        );
        return Ok(units
            .iter()
            .map(|unit| (unit.job.clone(), unknown_truth(unit)))
            .collect());
    }
    let mut truth = BTreeMap::new();
    for unit in units {
        // The timeless engine has one truth slot per unit and no per-request
        // identity to probe with, so this is deliberately NOT a SessionKey
        // lookup: the reader's collapse-over-sessions helper owns the rule
        // for resolving (session, drv)-scoped truth onto a session-less
        // unit. Session-aware consumers (the timed wiring) mint a
        // `SessionKey` from the recorded request instead — see the design
        // note on `archive::schema::SessionKey`.
        let entry = match archive.expected_outcome_across_sessions(&unit.drv_path) {
            None => unknown_truth(unit),
            Some(record) => UnitTruth {
                outcome: record.outcome,
                side: ExpectedSide {
                    outcome: record.outcome.as_str().to_string(),
                    outputs: unit
                        .outputs
                        .keys()
                        .map(|name| {
                            // The typed digest is carried straight through
                            // (no re-encode/re-parse): the comparator
                            // receives exactly what the archive record
                            // decoded to.
                            let output = record.outputs.get(name).map_or_else(
                                ExpectedOutput::default,
                                |hash| ExpectedOutput {
                                    narinfo_present: true,
                                    nar_hash: Some(hash.nar_hash),
                                    nar_size: Some(hash.nar_size),
                                },
                            );
                            (name.clone(), output)
                        })
                        .collect(),
                },
            },
        };
        truth.insert(unit.job.clone(), entry);
    }
    Ok(truth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::archive_input::fake_hash;
    use std::collections::HashMap;
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
    async fn probe_classifies_absent_paths_and_retries_transient_errors() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let warm_found = store_path("dep1");
        let warm_missing = store_path("dep2");

        let mut source = FakeSource::default();
        source
            .bodies
            .insert(warm_found.clone(), narinfo_text(&warm_found));
        // warm_missing: no body → 404. warm_found: fail once, then succeed
        // (retry path).
        source
            .fail_first
            .lock()
            .unwrap()
            .insert(warm_found.clone(), 1);

        let coverage = probe_warm_upstream_coverage(
            &state,
            &source,
            &[warm_found.clone(), warm_missing.clone()],
            8,
            3,
        )
        .await
        .unwrap();
        assert_eq!(coverage.stats.fetched, 2);
        assert_eq!(coverage.stats.found, 1);
        assert_eq!(coverage.stats.not_found, 1);
        assert_eq!(
            coverage.found,
            BTreeSet::from([warm_found.clone()]),
            "only the upstream-present path seeds the coverage set"
        );

        // supply.jsonl pre-classified exactly the missing path.
        let supply: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply).unwrap();
        assert_eq!(supply.len(), 1);
        assert_eq!(supply[0].path, warm_missing);
        assert_eq!(supply[0].source, SUPPLY_SOURCE_TARGET_SUBSTITUTER);
        assert_eq!(supply[0].mechanism, SUPPLY_MECHANISM_NONE);
        assert_eq!(supply[0].outcome, SUPPLY_OUTCOME_UNAVAILABLE);
        assert_eq!(supply[0].detail.as_deref(), Some("no upstream narinfo"));

        // A second probe (resumed campaign whose supply stage has not
        // finished) re-fetches — there is no disk cache — but never
        // duplicates the supply pre-classification.
        let before = source.fetches.load(Ordering::SeqCst);
        let coverage2 = probe_warm_upstream_coverage(
            &state,
            &source,
            &[warm_found.clone(), warm_missing.clone()],
            8,
            3,
        )
        .await
        .unwrap();
        assert_eq!(coverage2.stats.fetched, 2);
        assert_eq!(coverage2.found, coverage.found);
        assert!(source.fetches.load(Ordering::SeqCst) > before);
        let supply2: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply).unwrap();
        assert_eq!(supply2.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_retries_abort_the_probe_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let good = store_path("good");
        let bad = store_path("bad");

        let mut source = FakeSource::default();
        source.bodies.insert(good.clone(), narinfo_text(&good));
        // `bad` keeps failing transiently past max_attempts.
        source
            .fail_first
            .lock()
            .unwrap()
            .insert(bad.clone(), u32::MAX);

        let err = probe_warm_upstream_coverage(&state, &source, &[good.clone(), bad.clone()], 1, 3)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains(&bad),
            "abort error must name the failing path: {err}"
        );
        // An aborted probe pre-classifies nothing: absence is only recorded
        // from a complete pass, never from an upstream outage.
        let supply: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply).unwrap();
        assert!(supply.is_empty());

        // After the upstream recovers (404 now), a re-run completes and
        // records the miss.
        source.fail_first.lock().unwrap().clear();
        let coverage =
            probe_warm_upstream_coverage(&state, &source, &[good.clone(), bad.clone()], 1, 3)
                .await
                .unwrap();
        assert_eq!(coverage.stats.fetched, 2);
        assert_eq!(coverage.stats.not_found, 1);
        assert_eq!(coverage.found, BTreeSet::from([good.clone()]));
        let supply: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply).unwrap();
        assert_eq!(supply.len(), 1);
        assert_eq!(supply[0].path, bad);
    }

    #[tokio::test]
    async fn malformed_store_path_fails_without_retry() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let source = FakeSource::default();
        let err =
            probe_warm_upstream_coverage(&state, &source, &["not-a-store-path".to_string()], 4, 3)
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
    fn upstream_presence_classification() {
        let p = store_path("x");
        assert!(upstream_presence(&p, Some(&narinfo_text(&p))));
        assert!(!upstream_presence(&p, None));
        // A 200 body that isn't a parseable narinfo is still "present":
        // the path exists upstream even if its narinfo is unusable.
        assert!(upstream_presence(&p, Some("definitely not a narinfo")));
    }

    #[test]
    fn archive_outcomes_map_onto_engine_truth() {
        let tmp = tempfile::tempdir().unwrap();
        crate::run::archive_input::write_mini_archive(tmp.path());
        let archive = ReplayArchive::open(tmp.path()).unwrap();
        let units = crate::run::archive_input::load_units(&archive).unwrap();
        let truth = expected_outcomes_for_units(&archive, &units).unwrap();
        assert_eq!(truth.len(), units.len(), "every unit gets a truth entry");

        let app_a = &truth["appA.x86_64-linux"];
        assert_eq!(app_a.outcome, ExpectedOutcome::Built);
        // per-output expected NAR identity carried into ExpectedSide: the
        // typed digest matches the archive record's hex value exactly.
        let out = app_a.side.outputs.get("out").unwrap();
        assert!(out.narinfo_present);
        assert_eq!(
            out.nar_hash,
            Some(crate::narhash::NarHash::parse(&"ab".repeat(32)).unwrap())
        );
        assert!(out.nar_size.is_some());

        // A built record may carry hashes for only some outputs or none at
        // all; the outcome stays built and the hash-less outputs are simply
        // not comparable.
        let app_b = &truth["appB.x86_64-linux"];
        assert_eq!(app_b.outcome, ExpectedOutcome::Built);
        assert!(app_b.side.outputs.values().all(|o| !o.narinfo_present));
        assert!(app_b.side.outputs.values().all(|o| o.nar_hash.is_none()));

        // a unit with an `unknown` (or absent) outcome record stays Unknown
        assert_eq!(
            truth["divergentC.x86_64-linux"].outcome,
            ExpectedOutcome::Unknown
        );
        let kvm = &truth["kvmTest.x86_64-linux"];
        assert_eq!(kvm.outcome, ExpectedOutcome::Unknown);
        assert!(!kvm.side.outputs["out"].narinfo_present);
        assert!(kvm.side.outputs["out"].nar_hash.is_none());
    }

    /// Truth scoped to a recorded session must reach the campaign: an
    /// archive whose outcome records carry `session: Some(N)` (N ≠ 0, the
    /// shape the v1 schema defines and the v0 shim produces for every
    /// record) resolves through the collapse-over-sessions lookup instead
    /// of silently classifying the whole campaign against no truth.
    #[test]
    fn session_scoped_truth_reaches_campaign_units() {
        use crate::archive::schema::{
            Capabilities, ExpectedOutcome as ArchiveOutcome, OutcomeRecord, OutputHash,
            RequestRecord, RequestTarget, Substituters, UnitRecord,
        };
        use crate::archive::writer::{ArchiveWriter, ManifestSeed};
        use std::collections::BTreeMap;

        let app_drv = format!("/nix/store/{}-app-1.0.drv", fake_hash("app-drv"));
        let app_out = format!("/nix/store/{}-app-1.0", fake_hash("app-out"));

        let tmp = tempfile::tempdir().unwrap();
        let writer = ArchiveWriter::create(tmp.path()).unwrap();
        writer
            .add_drv(
                &app_drv,
                &format!(
                    r#"Derive([("out","{app_out}","","")],[],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{app_out}")])"#
                ),
            )
            .unwrap();
        writer
            .write_units(&[UnitRecord {
                drv: app_drv.clone(),
                label: Some("app.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), app_out.clone())]),
                required_features: Vec::new(),
                identity_divergent: false,
            }])
            .unwrap();
        writer
            .write_requests(&[RequestRecord {
                session: 7,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: app_drv.clone(),
                    outputs: vec!["*".to_string()],
                }],
            }])
            .unwrap();
        // The truth record is scoped to recorded session 7 — there is no
        // session-less form anywhere in this archive.
        writer
            .write_outcomes(&[OutcomeRecord {
                session: Some(7),
                drv: app_drv.clone(),
                outcome: ArchiveOutcome::Built,
                detail: None,
                duration_s: None,
                stop_offset_s: None,
                outputs: BTreeMap::from([(
                    "out".to_string(),
                    OutputHash {
                        nar_hash: crate::narhash::NarHash::parse(&"cd".repeat(32)).unwrap(),
                        nar_size: 4242,
                    },
                )]),
            }])
            .unwrap();
        let stamp: jiff::Timestamp = "2026-05-28T00:00:00Z".parse().unwrap();
        writer
            .finalize(ManifestSeed {
                created_at: stamp,
                from: stamp,
                to: stamp,
                capabilities: Capabilities {
                    timed: false,
                    expected_outcomes: true,
                    output_hashes: true,
                    embedded_store_paths: false,
                    impure_env: false,
                    dependency_closures: false,
                },
                substituters: Substituters::default(),
                fat: false,
                provenance: serde_json::Map::new(),
            })
            .unwrap();

        let archive = ReplayArchive::open(tmp.path()).unwrap();
        let units = crate::run::archive_input::load_units(&archive).unwrap();
        let truth = expected_outcomes_for_units(&archive, &units).unwrap();

        let app = &truth["app.x86_64-linux"];
        assert_eq!(
            app.outcome,
            ExpectedOutcome::Built,
            "session-scoped truth must not collapse to Unknown"
        );
        let out = app.side.outputs.get("out").unwrap();
        assert!(out.narinfo_present);
        assert_eq!(
            out.nar_hash,
            Some(crate::narhash::NarHash::parse(&"cd".repeat(32)).unwrap())
        );
    }

    /// Archive-sourced truth must reach a real comparison: the expected
    /// hash loaded from the archive's `outcomes.jsonl` (production wire
    /// form, bare hex), fed through the comparator against rio's digest,
    /// yields equal/differs — and a differing match-built is projected to
    /// output-divergence. This pins the producer→comparator path end to
    /// end, so the comparator can never again expect a spelling production
    /// truth does not supply.
    #[test]
    fn archive_truth_is_comparable_and_divergence_is_reachable() {
        use crate::run::classify::{
            NAR_DIFFERS, NAR_EQUAL, OutputHashes, compare_output, project_output_divergence,
        };
        use crate::run::model::Verdict;

        let tmp = tempfile::tempdir().unwrap();
        crate::run::archive_input::write_mini_archive(tmp.path());
        let archive = ReplayArchive::open(tmp.path()).unwrap();
        let units = crate::run::archive_input::load_units(&archive).unwrap();
        let truth = expected_outcomes_for_units(&archive, &units).unwrap();
        let expected = truth["appA.x86_64-linux"].side.outputs["out"]
            .nar_hash
            .expect("appA carries a recorded output hash");

        // rio rebuilt the path bit-identically: equal.
        assert_eq!(
            compare_output(&OutputHashes {
                rio: Some(expected),
                expected: Some(expected),
            }),
            NAR_EQUAL
        );
        // rio produced different bytes: differs, and the verdict projection
        // promotes the match-built to output-divergence.
        let differing = crate::narhash::NarHash::from_digest([0x5au8; 32]);
        let nar_verdict = compare_output(&OutputHashes {
            rio: Some(differing),
            expected: Some(expected),
        });
        assert_eq!(nar_verdict, NAR_DIFFERS);
        assert_eq!(
            project_output_divergence(Verdict::MatchBuilt, nar_verdict),
            Verdict::OutputDivergence,
        );
    }
}

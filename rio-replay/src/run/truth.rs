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
//!
//! Probe errors follow the design's §8.4 failure rules
//! (docs/dev/2026-05-28-build-replay-design.md, "Probe errors are not
//! misses"): a path whose probe still errors after the bounded retries —
//! or whose 200 body does not parse as a narinfo — is neither coverage
//! nor a miss. It is excluded from the returned coverage set (so the
//! supply ladder's archive/relay rungs, which can actually deliver it,
//! stay reachable) and is never pre-classified `unavailable` (absence is
//! a definitive upstream claim an error cannot support). One
//! persistently-erroring object degrades that single path; it never
//! aborts the probe, the engine, or the campaign — the systemic backstop
//! for a broadly-erroring upstream is the prefetch-shortfall pause gate,
//! which sees every undeliverable warm path. This is the same rule the
//! ladder's own `probe_substituter_narinfos` implements.

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
/// `Err` for anything else (retried by the probe with backoff, then
/// degrading that single path to [`ProbeOutcome::Errored`] — never
/// coverage, never a miss, never a probe abort).
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

/// Tri-state outcome of one warm-path narinfo probe.
///
/// The vocabulary and the per-variant consequences are the design's §8.4
/// failure rules (docs/dev/2026-05-28-build-replay-design.md, "Failure
/// handling and supply dispositions"): a 404 is a miss; everything else
/// that is not a usable narinfo is an error, and "Probe errors are not
/// misses … Coverage probes that error fall through to the next ladder
/// rung". The exhaustive matches over this enum are what make a fourth
/// outcome impossible to add without deciding its consequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// HTTP 200 with a parseable narinfo: confirmed upstream coverage.
    /// The path seeds the supply ladder's target-substituter rung.
    Present,
    /// HTTP 404: upstream definitively does not serve the path. The miss
    /// is recorded (`unavailable` pre-classification) so the prefetch arm
    /// never submits a path that cannot substitute.
    Absent,
    /// Neither: a transport error or non-404 HTTP status that persisted
    /// through the bounded retries, or a 200 body that does not parse as
    /// a narinfo. Coverage is unknown — the path is withheld from the
    /// coverage set so the ladder rungs that can actually deliver it
    /// (archive, relay) stay reachable, and no `unavailable` row is
    /// minted because an error supports no upstream-absence claim.
    Errored,
}

/// Classify one fetched narinfo response for `path`.
///
/// `None` (404) is [`ProbeOutcome::Absent`]. A 200 body must parse as a
/// narinfo to count as [`ProbeOutcome::Present`]: the target store
/// substitutes through the same `rio_nix` parser, so an unparseable body
/// (CDN error page, proxied S3 XML error served as 200) is coverage the
/// target structurally cannot use — counting it would withhold the path
/// from the archive/relay rungs while guaranteeing the target's own
/// substitution fails. It classifies [`ProbeOutcome::Errored`], the same
/// rule `Substituter::narinfo` applies for the ladder's probes.
pub fn classify_probe_response(path: &str, text: Option<&str>) -> ProbeOutcome {
    match text {
        None => ProbeOutcome::Absent,
        Some(text) => match rio_nix::narinfo::NarInfo::parse(text) {
            Ok(_) => ProbeOutcome::Present,
            Err(e) => {
                tracing::warn!(
                    path,
                    error = %e,
                    "unparseable narinfo body; not counted as upstream coverage (the target's \
                     substitution would fail on the same parse) and not a miss"
                );
                ProbeOutcome::Errored
            }
        },
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

/// Fetch one path's upstream presence with bounded retries.
///
/// Fetch errors (transport failures, non-404 statuses) are retried with
/// backoff; a path still erroring after `max_attempts` resolves to
/// [`ProbeOutcome::Errored`] — degrading that single path per the design's
/// §8.4 rule, never the probe. `Err` is reserved for malformed store paths:
/// an archive bug, not an upstream condition, so it is not retried and
/// (unlike any HTTP outcome) does abort the probe.
async fn fetch_one(
    source: &dyn NarinfoSource,
    path: &str,
    max_attempts: u32,
) -> Result<ProbeOutcome> {
    // Validate up front: a malformed store path is an archive bug, not a
    // transient fetch error — fail immediately instead of retrying it.
    let _ = rio_nix::store_path::StorePath::parse(path)
        .map_err(|e| anyhow::anyhow!("bad store path {path}: {e}"))?;
    let mut attempt = 0u32;
    loop {
        match source.fetch_narinfo_text(path).await {
            Ok(text) => return Ok(classify_probe_response(path, text.as_deref())),
            Err(e) if attempt + 1 < max_attempts => {
                let delay = NARINFO_BACKOFF.duration(attempt);
                tracing::warn!(path, attempt, error = %e, "narinfo fetch failed; retrying");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => {
                tracing::warn!(
                    path,
                    attempts = max_attempts,
                    error = %format!("{e:#}"),
                    "narinfo fetch still failing after bounded retries; probe errors are not \
                     misses — this path is not coverage and not pre-classified, and falls \
                     through to the supply ladder"
                );
                return Ok(ProbeOutcome::Errored);
            }
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
    /// Fetched paths with a parseable upstream narinfo.
    pub found: usize,
    /// Fetched paths absent upstream (404).
    pub not_found: usize,
    /// Fetched paths whose probe errored (persistent fetch error, or a 200
    /// body that does not parse as a narinfo): neither coverage nor a miss.
    pub errored: usize,
}

/// Result of the warm-set upstream-coverage probe.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WarmCoverage {
    /// Warm paths with a parseable upstream narinfo: the seed coverage set
    /// for the supply stage's prefetch classification and upload ladder.
    pub found: BTreeSet<String>,
    /// Warm paths whose probe errored. Excluded from [`Self::found`] AND
    /// from the `unavailable` pre-classification (§8.4: probe errors are
    /// not misses); the supply ladder re-probes them and its archive/relay
    /// rungs may still deliver them.
    pub errored: BTreeSet<String>,
    /// Probe counters (progress logging and tests).
    pub stats: SweepStats,
}

/// Probe upstream coverage for the prefetch (warm) set, pre-classifying
/// paths that are absent upstream as `unavailable` in `supply.jsonl` and
/// returning the set of paths that are present.
///
/// Per-path fetch errors that outlive the bounded retries degrade exactly
/// that path ([`WarmCoverage::errored`]); the probe always completes the
/// sweep. The only aborts are engine-local: a malformed store path (an
/// archive bug) or a state-dir write failure. Paths already carrying a
/// supply.jsonl record (a resumed campaign) are never pre-classified
/// twice.
pub async fn probe_warm_upstream_coverage(
    state: &StateDir,
    source: &dyn NarinfoSource,
    warm_paths: &[String],
    concurrency: usize,
    max_attempts: u32,
) -> Result<WarmCoverage> {
    // supply-fold: exempt — presence-only read (which paths carry ANY
    // row, for resume idempotence of the pre-classification appends); no
    // per-path truth is folded, so no SupplyFold projection applies.
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
        let outcome = fetch_one(source, &path, max_attempts).await;
        (path, outcome)
    }))
    .buffer_unordered(concurrency);

    while let Some((path, outcome)) = fetches.next().await {
        coverage.stats.fetched += 1;
        // Exhaustive over the §8.4 outcome vocabulary: adding a variant
        // forces a decision about its coverage/miss/fall-through effect.
        match outcome? {
            ProbeOutcome::Present => {
                coverage.stats.found += 1;
                coverage.found.insert(path);
            }
            ProbeOutcome::Absent => {
                coverage.stats.not_found += 1;
            }
            ProbeOutcome::Errored => {
                if coverage.errored.is_empty() {
                    // Once per probed cache (the warm probe targets one
                    // upstream), like the ladder's per-cache probe-error
                    // warning; per-path detail is logged at the fetch site.
                    tracing::warn!(
                        path = %path,
                        "narinfo probes against the upstream cache are erroring; probe errors \
                         are never misses — affected paths fall through the supply ladder"
                    );
                }
                coverage.stats.errored += 1;
                coverage.errored.insert(path);
            }
        }
        if coverage.stats.fetched.is_multiple_of(PROGRESS_LOG_EVERY) {
            tracing::info!(
                fetched = coverage.stats.fetched,
                to_probe,
                found = coverage.stats.found,
                not_found = coverage.stats.not_found,
                errored = coverage.stats.errored,
                "upstream-coverage probe progress"
            );
        }
    }
    tracing::info!(
        fetched = coverage.stats.fetched,
        found = coverage.stats.found,
        not_found = coverage.stats.not_found,
        errored = coverage.stats.errored,
        "warm-set upstream-coverage probe finished"
    );

    // Prefetch pre-classification: a prefetch-set path with no upstream
    // narinfo can never be substituted, so it is recorded up front and the
    // prefetch arm never submits it. Errored paths are skipped — recording
    // `unavailable` would convert "coverage unknown" into a definitive
    // upstream-absence claim (§8.4 forbids exactly that), and the rows it
    // minted would feed the shortfall denominators and unit dispositions
    // with fabricated misses. Iteration is sorted so supply.jsonl appends
    // are deterministic for a given input set.
    let warm_set: BTreeSet<&String> = warm_paths.iter().collect();
    for path in warm_set {
        if already_classified.contains(path)
            || coverage.found.contains(path)
            || coverage.errored.contains(path)
        {
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
/// record when one exists, otherwise the scoped record of the highest
/// informativeness rank, with ties within a rank class resolved by
/// consumed-truth content order (outcome, then recorded output hashes)
/// and the highest-numbered session deciding only between records whose
/// consumed truth is identical (see
/// `ReplayArchive::expected_outcome_across_sessions`, the rule's owner
/// doc) — so truth a recorder scoped to specific sessions is never
/// invisible here, and a built record cannot lose to a concurrent
/// session's disconnect.
/// The recorded outcome is carried through verbatim in the archive's
/// neutral vocabulary; how each value compares is the classifier's
/// business.
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

    /// Contract: design §8.4 ("Probe errors are not misses … Coverage
    /// probes that error fall through to the next ladder rung",
    /// docs/dev/2026-05-28-build-replay-design.md). A path that still
    /// errors after the bounded retries degrades — that single path is
    /// neither coverage nor a recorded miss — and the probe COMPLETES:
    /// one persistently-erroring object (deterministic 403/410/451, a
    /// blocked key) must never abort the probe, the engine, and with it
    /// the campaign Job. Once the upstream recovers to a definitive 404,
    /// a re-run records the real miss.
    #[tokio::test(start_paused = true)]
    async fn persistent_probe_errors_degrade_the_path_never_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let good = store_path("good");
        let bad = store_path("bad");

        let mut source = FakeSource::default();
        source.bodies.insert(good.clone(), narinfo_text(&good));
        // `bad` keeps erroring past max_attempts (a deterministic
        // per-object refusal looks identical to the probe).
        source
            .fail_first
            .lock()
            .unwrap()
            .insert(bad.clone(), u32::MAX);

        let coverage =
            probe_warm_upstream_coverage(&state, &source, &[good.clone(), bad.clone()], 1, 3)
                .await
                .expect("a per-path probe error must not abort the probe");
        assert_eq!(coverage.stats.fetched, 2, "the sweep completes");
        assert_eq!(coverage.found, BTreeSet::from([good.clone()]));
        assert_eq!(
            coverage.errored,
            BTreeSet::from([bad.clone()]),
            "the erroring path degrades to the errored set"
        );
        assert_eq!(coverage.stats.errored, 1);
        // Errors are not misses: no `unavailable` pre-classification for
        // the errored path — it falls through to the ladder rungs that
        // can deliver it instead of being barred from substitution.
        let supply: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply).unwrap();
        assert!(
            supply.is_empty(),
            "an errored probe must mint no supply rows: {supply:?}"
        );

        // After the upstream recovers (404 now), a re-run records the
        // definitive miss.
        source.fail_first.lock().unwrap().clear();
        let coverage =
            probe_warm_upstream_coverage(&state, &source, &[good.clone(), bad.clone()], 1, 3)
                .await
                .unwrap();
        assert_eq!(coverage.stats.fetched, 2);
        assert_eq!(coverage.stats.not_found, 1);
        assert!(coverage.errored.is_empty());
        assert_eq!(coverage.found, BTreeSet::from([good.clone()]));
        let supply: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply).unwrap();
        assert_eq!(supply.len(), 1);
        assert_eq!(supply[0].path, bad);
        assert_eq!(supply[0].outcome, SUPPLY_OUTCOME_UNAVAILABLE);
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

    /// Contract: design §8.4 — "A narinfo probe HTTP 403/`AccessDenied`
    /// is an error … never treated as 'not covered'; 404/`NoSuchKey` is a
    /// miss" — plus the parser-parity rule the ladder's
    /// `Substituter::narinfo` already enforces: a 200 body that does not
    /// parse is an ERROR, not coverage. The target store substitutes
    /// through the same `rio_nix` parser, so counting an unparseable body
    /// as present would withhold the path from the archive/relay rungs
    /// while guaranteeing the target's substitution of it fails.
    #[test]
    fn probe_response_classification_follows_design_failure_rules() {
        let p = store_path("x");
        assert_eq!(
            classify_probe_response(&p, Some(&narinfo_text(&p))),
            ProbeOutcome::Present
        );
        assert_eq!(classify_probe_response(&p, None), ProbeOutcome::Absent);
        assert_eq!(
            classify_probe_response(&p, Some("definitely not a narinfo")),
            ProbeOutcome::Errored,
            "an unparseable 200 body is not coverage and not a miss"
        );
    }

    /// Totality over the probe-outcome vocabulary, both directions per
    /// outcome. Quantification domain: every [`ProbeOutcome`] variant
    /// (the §8.4 outcome vocabulary), each driven through a real probe
    /// run — `Present` (parseable 200), `Absent` (404), and `Errored` in
    /// both producible shapes (persistent transport error; unparseable
    /// 200). For each variant the test asserts the full consequence row:
    /// coverage-set membership, errored-set membership, AND supply-row
    /// emission — so no variant can silently gain or lose a consequence.
    /// The match below is exhaustive: a new variant fails compilation
    /// here until its row is decided.
    #[tokio::test(start_paused = true)]
    async fn probe_outcome_consequences_are_total() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let present = store_path("present");
        let absent = store_path("absent");
        let err_transport = store_path("err-transport");
        let err_garbage = store_path("err-garbage");

        let mut source = FakeSource::default();
        source
            .bodies
            .insert(present.clone(), narinfo_text(&present));
        // 200 with a body the narinfo parser rejects (CDN error page).
        source.bodies.insert(
            err_garbage.clone(),
            "<html>access denied by policy</html>".to_string(),
        );
        // Persistent transport/HTTP error.
        source
            .fail_first
            .lock()
            .unwrap()
            .insert(err_transport.clone(), u32::MAX);

        let warm = [
            present.clone(),
            absent.clone(),
            err_transport.clone(),
            err_garbage.clone(),
        ];
        let coverage = probe_warm_upstream_coverage(&state, &source, &warm, 2, 2)
            .await
            .unwrap();

        let supply: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply).unwrap();
        let rows: BTreeSet<&str> = supply.iter().map(|e| e.path.as_str()).collect();
        let expected = [
            (&present, ProbeOutcome::Present),
            (&absent, ProbeOutcome::Absent),
            (&err_transport, ProbeOutcome::Errored),
            (&err_garbage, ProbeOutcome::Errored),
        ];
        for (path, outcome) in expected {
            // (covered, errored, supply-row) per variant — the full row,
            // both the must-admit and the must-block direction.
            let want = match outcome {
                ProbeOutcome::Present => (true, false, false),
                ProbeOutcome::Absent => (false, false, true),
                ProbeOutcome::Errored => (false, true, false),
            };
            assert_eq!(
                (
                    coverage.found.contains(path),
                    coverage.errored.contains(path),
                    rows.contains(path.as_str()),
                ),
                want,
                "consequence row for {outcome:?} ({path})"
            );
        }
        assert_eq!(coverage.stats.fetched, 4);
        assert_eq!(
            (
                coverage.stats.found,
                coverage.stats.not_found,
                coverage.stats.errored
            ),
            (1, 1, 2)
        );
        assert_eq!(supply.len(), 1, "exactly the miss is pre-classified");
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
                outputs: Some(BTreeMap::from([("out".to_string(), app_out.clone())])),
                required_features: Some(Vec::new()),
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

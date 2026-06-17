//! Criterion-style local baselines: `.fsbench/baselines/<name>.json`,
//! byte-identical in shape to a result file (minus the transient
//! `compare` block). Saving is gated — a baseline is a future
//! comparison's denominator, so anything that would poison every later
//! verdict (contention, failed attribution, dishonest cold) refuses
//! the save rather than warning past it.

use std::path::PathBuf;

use anyhow::Result;

use super::result::ResultV1;
use crate::sh::repo_root;

/// Why a save did not happen. `Refused` is part of the exit-code
/// vocabulary (refusals exit 2, like compare refusals); `Io` is an
/// ordinary failure (exit 1) — a full disk is not a verdict about the
/// run's eligibility.
#[derive(Debug)]
pub enum SaveError {
    Refused(String),
    Io(anyhow::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Refused(m) => write!(f, "{m}"),
            SaveError::Io(e) => write!(f, "{e:#}"),
        }
    }
}

pub fn baseline_path(name: &str) -> PathBuf {
    repo_root()
        .join(".fsbench/baselines")
        .join(format!("{name}.json"))
}

pub fn load(name: &str) -> Result<Option<ResultV1>> {
    let p = baseline_path(name);
    if !p.exists() {
        return Ok(None);
    }
    super::result::read(&p).map(Some)
}

/// Refuses ineligible runs. The error text names the disqualifier —
/// the operator's next step differs for each.
pub fn save(result: &ResultV1, name: &str) -> Result<PathBuf, SaveError> {
    let refuse = |m: String| Err(SaveError::Refused(m));
    if result.placement.attribution != "exact" {
        return refuse(format!(
            "refusing --save-baseline {name}: run is unattributed (no executor match) — \
             a baseline without placement identity can never be compared safely"
        ));
    }
    if result.placement.contended {
        return refuse(format!(
            "refusing --save-baseline {name}: run was contended \
             (max {} co-tenants on the bench node) — wait for an idle window",
            result.placement.max_co_tenants
        ));
    }
    // Only a POSITIVE honesty verdict may become a denominator:
    // Some(false) is proven dishonest, and None (scrape failed, deltas
    // invalid) is unverified — silently trusting it would let one bad
    // sampling run poison every later compare. There is deliberately
    // no override flag; --force exists only on the compare side, where
    // it marks verdicts untrusted instead of laundering them.
    match result.cluster_metrics.honest_cold {
        Some(true) => {}
        Some(false) => {
            return refuse(format!(
                "refusing --save-baseline {name}: run is dishonest-cold \
                 (Promote bytes did not cover the cold phase) — \
                 its cold numbers were not actually cold"
            ));
        }
        None => {
            return refuse(format!(
                "refusing --save-baseline {name}: cold honesty unverifiable \
                 (metric deltas invalid or scrapes failed) — an unverified run \
                 must not become the trusted denominator; re-run and check the \
                 co-tenancy watcher"
            ));
        }
    }
    let mut clean = result.clone();
    // The compare block records this run's verdicts against a PREVIOUS
    // baseline — meaningless inside the new baseline itself.
    clean.compare = None;
    let p = baseline_path(name);
    super::result::write(&clean, &p).map_err(SaveError::Io)?;
    Ok(p)
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::result::*;
    use super::*;
    use std::collections::BTreeMap;

    /// A minimal eligible result, reused by compare.rs tests.
    pub fn eligible() -> ResultV1 {
        let metric = |unit: &str, value: f64| Metric {
            unit: unit.into(),
            value: Some(value),
            ..Default::default()
        };
        let phase = |metrics: BTreeMap<String, Metric>| PhaseMetrics {
            start_epoch_ms: 0,
            end_epoch_ms: 1,
            reps: 3,
            metrics,
        };
        ResultV1 {
            schema: SCHEMA.into(),
            run_id: "r1".into(),
            seed: "s1".into(),
            created_at: "2026-06-02T00:00:00Z".into(),
            git: GitInfo {
                commit: "abc".into(),
                dirty: false,
                image_tag: "abc".into(),
            },
            cluster: ClusterInfo {
                provider: "eks".into(),
                context: "rio-jorg".into(),
            },
            placement: Placement {
                node: Some("n1".into()),
                instance_type: Some("c7a.4xlarge".into()),
                capacity_type: Some("od".into()),
                kernel: "6.12.20".into(),
                ami: None,
                attribution: "exact".into(),
                contended: false,
                max_co_tenants: 1,
                cotenancy_samples: 30,
            },
            workload: Workload {
                version: 1,
                dataset_digest: "fixture-digest".into(),
                files: 6556,
                total_bytes: 1_932_848_097,
                unique_chunk_bytes: 1_500_000_000,
                unique_chunk_bytes_storm: 1_100_000_000,
                jq_src: Some("hash-jq-1.7.1.tar.gz".into()),
                toolchain: Some("hash-gcc-wrapper-14".into()),
                closure: "python3".into(),
                phases: vec!["read_storm_warm".into()],
                reps: 3,
            },
            phases: BTreeMap::from([
                (
                    "read_storm_warm".into(),
                    phase(BTreeMap::from([
                        ("mib_s".to_string(), metric("mib_s", 1000.0)),
                        (
                            "open_ns".to_string(),
                            Metric {
                                unit: "ns".into(),
                                p50: Some(10_000.0),
                                p99: Some(50_000.0),
                                rep_spread: Some(0.05),
                                ..Default::default()
                            },
                        ),
                    ])),
                ),
                (
                    "ratios".into(),
                    phase(BTreeMap::from([(
                        "warm_read_slowdown".to_string(),
                        metric("ratio", 1.2),
                    )])),
                ),
            ]),
            cluster_metrics: ClusterMetrics {
                valid: true,
                honest_cold: Some(true),
                honesty_note: None,
                mountd: None,
                builder: None,
            },
            compare: None,
        }
    }

    /// Every disqualifier must come back as the Refused VARIANT — the
    /// exit-code mapping (refusal = 2) keys on it, so a refusal that
    /// surfaced as Io would silently exit 1 instead.
    #[test]
    fn save_refuses_disqualified_runs() {
        let assert_refused = |e: &SaveError, needle: &str| {
            assert!(
                matches!(e, SaveError::Refused(_)),
                "must be a refusal, not Io: {e}"
            );
            assert!(e.to_string().contains(needle), "got: {e}");
        };

        let mut r = eligible();
        r.placement.contended = true;
        assert_refused(&save(&r, "t").unwrap_err(), "contended");

        let mut r = eligible();
        r.placement.attribution = "unattributed".into();
        assert_refused(&save(&r, "t").unwrap_err(), "unattributed");

        let mut r = eligible();
        r.cluster_metrics.honest_cold = Some(false);
        assert_refused(&save(&r, "t").unwrap_err(), "dishonest-cold");

        // None = honesty never computed (scrape failed / deltas
        // invalid). Saving it would make an UNVERIFIED run the trusted
        // denominator of every later compare — refuse, with a message
        // distinct from the proven-dishonest case.
        let mut r = eligible();
        r.cluster_metrics.honest_cold = None;
        assert_refused(&save(&r, "t").unwrap_err(), "unverifiable");
    }
}

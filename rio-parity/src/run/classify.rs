//! The bucket classifier: ONE pure function over (Hydra outcome × rio
//! outcome × auxiliary flags), plus the per-output NAR comparison verdict
//! and the headline arithmetic. No I/O, no clocks — everything here is
//! deterministic and exhaustively testable.
//!
//! Precedence rationale: operator skips and evaluation failures are
//! dispositive on their own, so they are honored before any build evidence
//! is consulted; infrastructure failures and upstream source rot say
//! nothing about parity (the build never got a fair attempt), so they are
//! pulled out before the Hydra-keyed cross product that actually scores
//! agreement.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::model::{Bucket, FailureKind, HydraOutcome, RioOutcome, RootCauseKind};

/// Auxiliary flags resolved before classification (plan output + resolve-unknown).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AuxFlags {
    /// Job was filtered out / unsupported system or feature (reason string).
    pub skipped: Option<String>,
    /// Attr failed local evaluation (recorded as an eval-error exclusion
    /// in the archive).
    pub eval_error: bool,
    /// Job is in the plan-time not-attemptable set: in leaf mode its own
    /// outputs sit inside another in-scope job's dependency closure, so
    /// warming would have masked the build.
    pub plan_not_attemptable: bool,
    /// Every declared output was already valid in rio-store at the
    /// plan-time validity snapshot.
    pub plan_snapshot_valid: bool,
    /// Verdict from the optional resolve-unknown pass over Hydra-unknown
    /// jobs: Some(true) = Hydra built a different drv for this job (eval
    /// divergence), Some(false) = resolved and the drv matches. None when
    /// resolution has not run (the default).
    pub resolve_unknown_divergent: Option<bool>,
}

/// Classification result: the bucket plus whether the job is a cascaded
/// dependent counted under an excluded root cause (its failing dependency
/// was infra / source-rot, so the job is excluded from the headline rather
/// than charged as its own failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    pub bucket: Bucket,
    pub cascaded: bool,
}

/// THE classifier. Precedence (highest first): skipped → eval-error →
/// not-attemptable → not-attempted → completed-without-execution
/// (cached-prior / target-substituted) → infra and upstream-source-rot
/// failures including their cascaded dependents → the remaining cross
/// product, keyed by the Hydra outcome first.
pub fn classify(hydra: &HydraOutcome, rio: &RioOutcome, flags: &AuxFlags) -> Classification {
    let plain = |bucket| Classification {
        bucket,
        cascaded: false,
    };

    // 1. skipped
    if flags.skipped.is_some() {
        return plain(Bucket::Skipped);
    }
    // 2. eval-error
    if flags.eval_error {
        return plain(Bucket::EvalError);
    }
    // 3. not-attemptable (plan-time set)
    if flags.plan_not_attemptable {
        return plain(Bucket::NotAttemptable);
    }
    // 4. not-attempted
    if matches!(rio, RioOutcome::NotAttempted) {
        return plain(Bucket::NotAttempted);
    }
    // 5/6. completed-without-execution discriminator: plan-snapshot valid ⇒
    // cached-prior; (not-attemptable already handled); else
    // target-substituted.
    if let RioOutcome::Built { executed: false } = rio {
        return if flags.plan_snapshot_valid {
            plain(Bucket::CachedPrior)
        } else {
            plain(Bucket::TargetSubstituted)
        };
    }
    // 7. infra / source-rot (incl. cascaded dependents per the cascade rule).
    match rio {
        RioOutcome::TargetFailed {
            kind: FailureKind::Infra,
        } => return plain(Bucket::RioInfraFailure),
        RioOutcome::TargetFailed {
            kind: FailureKind::SourceRot,
        } => {
            return plain(Bucket::UpstreamSourceUnavailable);
        }
        RioOutcome::DependencyFailed {
            root: RootCauseKind::Infra,
            ..
        } => {
            return Classification {
                bucket: Bucket::RioInfraFailure,
                cascaded: true,
            };
        }
        RioOutcome::DependencyFailed {
            root: RootCauseKind::SourceRot,
            ..
        } => {
            return Classification {
                bucket: Bucket::UpstreamSourceUnavailable,
                cascaded: true,
            };
        }
        _ => {}
    }
    // 8. Cross product, Hydra keyed first.
    match hydra {
        HydraOutcome::Unknown => match flags.resolve_unknown_divergent {
            Some(true) => plain(Bucket::EvalDivergence),
            _ => plain(Bucket::HydraUnknown),
        },
        HydraOutcome::Failed => match rio {
            RioOutcome::Built { .. } => plain(Bucket::HydraOnlyFailure),
            // Any rio failure shape agrees with Hydra's failure.
            _ => plain(Bucket::BothFailed),
        },
        HydraOutcome::Built => match rio {
            RioOutcome::Built { .. } => plain(Bucket::MatchBuilt),
            RioOutcome::DependencyFailed {
                root: RootCauseKind::Genuine,
                ..
            } => plain(Bucket::RioDependencyFailure),
            RioOutcome::TargetFailed { .. } => plain(Bucket::RioOnlyFailure),
            // Exhaustiveness: NotAttempted and Built{executed:false} were
            // handled above; DependencyFailed infra/source-rot handled above.
            _ => plain(Bucket::RioOnlyFailure),
        },
    }
}

/// Per-output NAR comparison input.
#[derive(Debug, Clone, Default)]
pub struct OutputHashes {
    /// rio nar_hash as lowercase hex (raw SHA-256), when the path is valid.
    pub rio_hex: Option<String>,
    /// cache.nixos.org NarHash field (e.g. `sha256:<nixbase32>`), when present.
    pub hydra_narhash: Option<String>,
}

/// Per-output NAR comparison verdict strings (the `narCompare` values in
/// results.jsonl).
pub const NAR_EQUAL: &str = "equal";
pub const NAR_DIFFERS: &str = "differs";
pub const NAR_NOT_COMPARABLE: &str = "not-comparable";

/// Compare one output: comparable iff both sides have a hash; equality is on
/// the raw SHA-256 digest (cache.nixos.org NarHash is nixbase32, rio's is hex).
/// Anything that cannot be compared meaningfully — an upstream hash that is
/// not SHA-256, or a rio value that is not 64 lowercase hex characters — is
/// `not-comparable` rather than a false `differs`.
pub fn compare_output(h: &OutputHashes) -> &'static str {
    let (Some(rio_hex), Some(hydra)) = (&h.rio_hex, &h.hydra_narhash) else {
        return NAR_NOT_COMPARABLE;
    };
    let Ok(parsed) = rio_nix::hash::NixHash::parse(hydra) else {
        return NAR_NOT_COMPARABLE;
    };
    if parsed.algo() != rio_nix::hash::HashAlgo::SHA256 {
        return NAR_NOT_COMPARABLE;
    }
    if rio_hex.len() != 64
        || !rio_hex
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return NAR_NOT_COMPARABLE;
    }
    if parsed.to_hex().eq_ignore_ascii_case(rio_hex) {
        NAR_EQUAL
    } else {
        NAR_DIFFERS
    }
}

/// Job-level verdict: `differs` if any comparable output differs; `equal` if
/// ≥1 comparable and all match; else `not-comparable`.
///
/// Generic over the verdict string type so callers can pass either the
/// `&'static str` verdicts produced by [`compare_output`] or owned `String`
/// values reloaded from results.jsonl — either way this function stays the
/// single source of the any-differs / any-equal rule.
pub fn job_nar_verdict<S: AsRef<str>>(outputs: &BTreeMap<String, S>) -> &'static str {
    if outputs.values().any(|v| v.as_ref() == NAR_DIFFERS) {
        NAR_DIFFERS
    } else if outputs.values().any(|v| v.as_ref() == NAR_EQUAL) {
        NAR_EQUAL
    } else {
        NAR_NOT_COMPARABLE
    }
}

/// Headline + secondary metrics from bucket counts. The headline ratio is
/// match-built over (match-built + rio-only-failure + rio-dependency-failure);
/// every other bucket is excluded from the denominator. NAR agreement is the
/// non-gating share of compared jobs whose NAR hashes match.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Headline {
    pub numerator: usize,
    pub denominator: usize,
    pub headline_pct: Option<f64>,
    pub nar_equal: usize,
    pub nar_compared: usize,
    pub nar_agreement_pct: Option<f64>,
}

/// Compute the [`Headline`] metrics from final bucket counts (keyed by
/// [`Bucket::as_str`]) plus the job-level NAR comparison tallies.
pub fn headline(
    bucket_counts: &BTreeMap<String, usize>,
    nar_equal: usize,
    nar_compared: usize,
) -> Headline {
    let get = |b: Bucket| bucket_counts.get(b.as_str()).copied().unwrap_or(0);
    let numerator = get(Bucket::MatchBuilt);
    let denominator = numerator + get(Bucket::RioOnlyFailure) + get(Bucket::RioDependencyFailure);
    Headline {
        numerator,
        denominator,
        headline_pct: (denominator > 0).then(|| 100.0 * numerator as f64 / denominator as f64),
        nar_equal,
        nar_compared,
        nar_agreement_pct: (nar_compared > 0)
            .then(|| 100.0 * nar_equal as f64 / nar_compared as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn flags() -> AuxFlags {
        AuxFlags::default()
    }

    fn dep(root: RootCauseKind) -> RioOutcome {
        RioOutcome::DependencyFailed {
            root,
            failing_drv: "/nix/store/d.drv".into(),
        }
    }
    fn failed(kind: FailureKind) -> RioOutcome {
        RioOutcome::TargetFailed { kind }
    }

    /// Spot rows straight out of the classification table + precedence list.
    #[rstest]
    // precedence head: skipped/eval-error/not-attemptable/not-attempted
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: true },
           AuxFlags { skipped: Some("system".into()), ..flags() }, Bucket::Skipped, false)]
    #[case(HydraOutcome::Built, failed(FailureKind::Genuine),
           AuxFlags { eval_error: true, ..flags() }, Bucket::EvalError, false)]
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: false },
           AuxFlags { plan_not_attemptable: true, ..flags() }, Bucket::NotAttemptable, false)]
    #[case(
        HydraOutcome::Built,
        RioOutcome::NotAttempted,
        flags(),
        Bucket::NotAttempted,
        false
    )]
    // completed-without-execution discriminator
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: false },
           AuxFlags { plan_snapshot_valid: true, ..flags() }, Bucket::CachedPrior, false)]
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: false }, flags(), Bucket::TargetSubstituted, false)]
    // infra / source rot (and their cascades)
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::Infra),
        flags(),
        Bucket::RioInfraFailure,
        false
    )]
    #[case(
        HydraOutcome::Failed,
        failed(FailureKind::Infra),
        flags(),
        Bucket::RioInfraFailure,
        false
    )]
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::SourceRot),
        flags(),
        Bucket::UpstreamSourceUnavailable,
        false
    )]
    #[case(
        HydraOutcome::Built,
        dep(RootCauseKind::Infra),
        flags(),
        Bucket::RioInfraFailure,
        true
    )]
    #[case(
        HydraOutcome::Built,
        dep(RootCauseKind::SourceRot),
        flags(),
        Bucket::UpstreamSourceUnavailable,
        true
    )]
    // cross product, Hydra keyed first
    #[case(HydraOutcome::Unknown, RioOutcome::Built { executed: true }, flags(), Bucket::HydraUnknown, false)]
    #[case(HydraOutcome::Unknown, failed(FailureKind::Genuine),
           AuxFlags { resolve_unknown_divergent: Some(true), ..flags() }, Bucket::EvalDivergence, false)]
    #[case(HydraOutcome::Failed, RioOutcome::Built { executed: true }, flags(), Bucket::HydraOnlyFailure, false)]
    #[case(
        HydraOutcome::Failed,
        failed(FailureKind::Genuine),
        flags(),
        Bucket::BothFailed,
        false
    )]
    #[case(
        HydraOutcome::Failed,
        failed(FailureKind::Timeout),
        flags(),
        Bucket::BothFailed,
        false
    )]
    #[case(
        HydraOutcome::Failed,
        dep(RootCauseKind::Genuine),
        flags(),
        Bucket::BothFailed,
        false
    )]
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: true }, flags(), Bucket::MatchBuilt, false)]
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::Genuine),
        flags(),
        Bucket::RioOnlyFailure,
        false
    )]
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::Timeout),
        flags(),
        Bucket::RioOnlyFailure,
        false
    )]
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::ResourceCeiling),
        flags(),
        Bucket::RioOnlyFailure,
        false
    )]
    #[case(
        HydraOutcome::Built,
        dep(RootCauseKind::Genuine),
        flags(),
        Bucket::RioDependencyFailure,
        false
    )]
    fn design_rows(
        #[case] hydra: HydraOutcome,
        #[case] rio: RioOutcome,
        #[case] aux: AuxFlags,
        #[case] expected: Bucket,
        #[case] cascaded: bool,
    ) {
        let c = classify(&hydra, &rio, &aux);
        assert_eq!(
            c.bucket, expected,
            "hydra={hydra:?} rio={rio:?} aux={aux:?}"
        );
        assert_eq!(c.cascaded, cascaded);
    }

    /// The infra-poisoned shared dependency scenario: the dependency itself
    /// is infra (excluded), its dependents cascade out of the denominator,
    /// and an unrelated genuine dependency failure stays in.
    #[test]
    fn infra_poisoned_shared_dependency_cascade() {
        // Dependent of the infra-poisoned dep: excluded, counted as cascaded.
        let c = classify(&HydraOutcome::Built, &dep(RootCauseKind::Infra), &flags());
        assert_eq!((c.bucket, c.cascaded), (Bucket::RioInfraFailure, true));
        // Dependent of a genuine target failure: stays in the denominator.
        let c = classify(&HydraOutcome::Built, &dep(RootCauseKind::Genuine), &flags());
        assert_eq!(
            (c.bucket, c.cascaded),
            (Bucket::RioDependencyFailure, false)
        );
    }

    /// Exhaustive grid: every (hydra, rio, flag-combination) maps to exactly
    /// one bucket, the precedence holds (e.g. skipped beats everything), and
    /// excluded-by-plan flags never leak into headline buckets.
    #[test]
    fn exhaustive_grid_is_total_and_respects_precedence() {
        let hydras = [
            HydraOutcome::Built,
            HydraOutcome::Failed,
            HydraOutcome::Unknown,
        ];
        let rios = [
            RioOutcome::NotAttempted,
            RioOutcome::Built { executed: true },
            RioOutcome::Built { executed: false },
            failed(FailureKind::Genuine),
            failed(FailureKind::Infra),
            failed(FailureKind::Timeout),
            failed(FailureKind::ResourceCeiling),
            failed(FailureKind::SourceRot),
            dep(RootCauseKind::Genuine),
            dep(RootCauseKind::Infra),
            dep(RootCauseKind::SourceRot),
        ];
        let bools = [false, true];
        let resolve = [None, Some(false), Some(true)];
        let mut grid = 0usize;
        for hydra in &hydras {
            for rio in &rios {
                for skipped in &bools {
                    for eval_error in &bools {
                        for not_attemptable in &bools {
                            for snapshot_valid in &bools {
                                for rud in &resolve {
                                    let aux = AuxFlags {
                                        skipped: skipped.then(|| "filtered".to_string()),
                                        eval_error: *eval_error,
                                        plan_not_attemptable: *not_attemptable,
                                        plan_snapshot_valid: *snapshot_valid,
                                        resolve_unknown_divergent: *rud,
                                    };
                                    grid += 1;
                                    let c = classify(hydra, rio, &aux);
                                    let ctx = format!("hydra={hydra:?} rio={rio:?} aux={aux:?}");
                                    // Total: as_str never panics, bucket is one of ALL.
                                    assert!(Bucket::ALL.contains(&c.bucket), "{ctx}");
                                    // Precedence assertions.
                                    if aux.skipped.is_some() {
                                        assert_eq!(c.bucket, Bucket::Skipped, "{ctx}");
                                    } else if aux.eval_error {
                                        assert_eq!(c.bucket, Bucket::EvalError, "{ctx}");
                                    } else if aux.plan_not_attemptable {
                                        assert_eq!(c.bucket, Bucket::NotAttemptable, "{ctx}");
                                    } else if matches!(rio, RioOutcome::NotAttempted) {
                                        assert_eq!(c.bucket, Bucket::NotAttempted, "{ctx}");
                                    }
                                    // Headline buckets only ever come from executed builds
                                    // or genuine failures.
                                    if c.bucket == Bucket::MatchBuilt {
                                        assert!(
                                            matches!(rio, RioOutcome::Built { executed: true }),
                                            "{ctx}"
                                        );
                                    }
                                    // Cascaded is only ever set for dependency failures
                                    // with infra/source-rot roots.
                                    if c.cascaded {
                                        assert!(
                                            matches!(
                                                rio,
                                                RioOutcome::DependencyFailed {
                                                    root: RootCauseKind::Infra
                                                        | RootCauseKind::SourceRot,
                                                    ..
                                                }
                                            ),
                                            "{ctx}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(
            grid,
            3 * 11 * 2 * 2 * 2 * 2 * 3,
            "grid covered every combination"
        );
    }

    #[test]
    fn nar_comparison_and_verdicts() {
        // cache.nixos.org publishes nixbase32; rio stores raw bytes (hex here).
        // Build a matching pair via rio-nix to avoid hand-encoding base32.
        let digest = [7u8; 32];
        let nix_hash =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest.to_vec()).unwrap();
        let equal = compare_output(&OutputHashes {
            rio_hex: Some(hex::encode(digest)),
            hydra_narhash: Some(nix_hash.to_colon()),
        });
        assert_eq!(equal, NAR_EQUAL);
        let differs = compare_output(&OutputHashes {
            rio_hex: Some(hex::encode([8u8; 32])),
            hydra_narhash: Some(nix_hash.to_colon()),
        });
        assert_eq!(differs, NAR_DIFFERS);
        assert_eq!(
            compare_output(&OutputHashes {
                rio_hex: None,
                hydra_narhash: Some(nix_hash.to_colon())
            }),
            NAR_NOT_COMPARABLE
        );
        // SRI form is also accepted (NixHash::parse handles both).
        let sri = compare_output(&OutputHashes {
            rio_hex: Some(hex::encode(digest)),
            hydra_narhash: Some(nix_hash.to_sri()),
        });
        assert_eq!(sri, NAR_EQUAL);

        let mut outs = BTreeMap::new();
        outs.insert("out".to_string(), NAR_EQUAL);
        outs.insert("dev".to_string(), NAR_NOT_COMPARABLE);
        assert_eq!(job_nar_verdict(&outs), NAR_EQUAL);
        outs.insert("lib".to_string(), NAR_DIFFERS);
        assert_eq!(job_nar_verdict(&outs), NAR_DIFFERS);
        let empty: BTreeMap<String, &'static str> = BTreeMap::new();
        assert_eq!(job_nar_verdict(&empty), NAR_NOT_COMPARABLE);
        // Owned-String verdict values (the shape reloaded from results.jsonl)
        // go through the same single source of the any-differs/any-equal rule.
        let owned: BTreeMap<String, String> = outs
            .iter()
            .map(|(k, v)| (k.clone(), (*v).to_string()))
            .collect();
        assert_eq!(job_nar_verdict(&owned), NAR_DIFFERS);
    }

    /// A hash pair the engine cannot meaningfully compare must come out
    /// `not-comparable`, never a false `differs`.
    #[test]
    fn nar_comparison_guards_against_unusable_hashes() {
        // 0xab digests so the hex form contains letters (the uppercase case
        // below must actually differ from the lowercase form).
        let digest = [0xab_u8; 32];
        let rio_hex = hex::encode(digest);
        // Upstream NarHash with a non-SHA-256 algorithm.
        let sha512 =
            rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA512, vec![7u8; 64]).unwrap();
        assert_eq!(
            compare_output(&OutputHashes {
                rio_hex: Some(rio_hex.clone()),
                hydra_narhash: Some(sha512.to_colon()),
            }),
            NAR_NOT_COMPARABLE
        );
        // Rio-side value that is not a 64-char lowercase hex digest: wrong
        // length, non-hex characters, or uppercase hex.
        let sha256 = rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest.to_vec())
            .unwrap()
            .to_colon();
        for bad in [
            "0badc0ffee".to_string(),
            format!("{}zz", &rio_hex[..62]),
            rio_hex.to_ascii_uppercase(),
        ] {
            assert_eq!(
                compare_output(&OutputHashes {
                    rio_hex: Some(bad.clone()),
                    hydra_narhash: Some(sha256.clone()),
                }),
                NAR_NOT_COMPARABLE,
                "rio_hex={bad}"
            );
        }
        // The well-formed pair still compares equal.
        assert_eq!(
            compare_output(&OutputHashes {
                rio_hex: Some(rio_hex),
                hydra_narhash: Some(sha256),
            }),
            NAR_EQUAL
        );
    }

    #[test]
    fn headline_math() {
        let mut counts = BTreeMap::new();
        counts.insert(Bucket::MatchBuilt.as_str().to_string(), 90);
        counts.insert(Bucket::RioOnlyFailure.as_str().to_string(), 7);
        counts.insert(Bucket::RioDependencyFailure.as_str().to_string(), 3);
        counts.insert(Bucket::RioInfraFailure.as_str().to_string(), 5); // excluded
        counts.insert(Bucket::CachedPrior.as_str().to_string(), 11); // excluded
        let h = headline(&counts, 80, 85);
        assert_eq!(h.denominator, 100);
        assert_eq!(h.numerator, 90);
        assert!((h.headline_pct.unwrap() - 90.0).abs() < f64::EPSILON);
        assert!((h.nar_agreement_pct.unwrap() - (80.0 / 85.0 * 100.0)).abs() < 1e-9);
        let empty = headline(&BTreeMap::new(), 0, 0);
        assert_eq!(empty.headline_pct, None);
    }
}

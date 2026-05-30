//! The classifier: ONE pure function over (Hydra outcome × rio outcome ×
//! auxiliary flags) that assigns each unit a verdict or a disposition,
//! plus the per-output NAR comparison verdict and the headline arithmetic.
//! No I/O, no clocks — everything here is deterministic and exhaustively
//! testable.
//!
//! Precedence rationale: a replayed (or out-raced) recorded interruption is
//! its own final observation — the unit's recorded outcome was an
//! interruption, so no other comparison is meaningful and the timed flag is
//! honored before everything else. After that, operator filters and
//! evaluation failures are dispositive on their own, so they are honored
//! before any build evidence is consulted; infrastructure failures and
//! upstream source rot say nothing about outcome agreement (the build never
//! got a fair attempt), so they are pulled out before the Hydra-keyed cross
//! product that actually scores agreement.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::model::{
    Disposition, FailureKind, HydraOutcome, RioOutcome, RootCauseKind, UnifiedClass, Verdict,
};

/// How a recorded interruption (cancellation or client disconnect) played
/// out when the timed dispatcher replayed it for a unit. Only ever set for
/// members of timed batches; the wire form is kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimedInterruption {
    /// The interruption was reproduced: the channel was abandoned at the
    /// recorded offset and the unit did not complete, as recorded.
    Replayed,
    /// The replayed build completed before the recorded interruption offset
    /// (the target was faster than the recording).
    NotReproduced,
}

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
    /// How the timed dispatcher's interruption replay played out for this
    /// unit. None outside timed mode (and for timed units with no recorded
    /// interruption armed).
    #[serde(default)]
    pub timed_interruption: Option<TimedInterruption>,
}

/// Classification result: the unified class (a verdict or a disposition)
/// plus whether the job is a cascaded dependent counted under an excluded
/// root cause (its failing dependency was infra / source-rot, so the job is
/// excluded from the headline rather than charged as its own failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    pub class: UnifiedClass,
    pub cascaded: bool,
}

/// THE classifier. Precedence (highest first): timed-interruption flag →
/// filtered → eval-error → not-attemptable → not-attempted →
/// completed-without-execution (cached-prior / target-substituted) → infra
/// and upstream-source-rot failures including their cascaded dependents →
/// the remaining cross product, keyed by the Hydra outcome first.
pub fn classify(hydra: &HydraOutcome, rio: &RioOutcome, flags: &AuxFlags) -> Classification {
    let verdict = |v: Verdict| Classification {
        class: UnifiedClass::Verdict(v),
        cascaded: false,
    };
    let disposition = |d: Disposition| Classification {
        class: UnifiedClass::Disposition(d),
        cascaded: false,
    };

    // 0. timed interruption replay: the recorded outcome was an
    // interruption, so the replay (or the build out-racing it) is the final
    // observation — no other rule applies.
    match flags.timed_interruption {
        Some(TimedInterruption::Replayed) => return verdict(Verdict::InterruptionReplayed),
        Some(TimedInterruption::NotReproduced) => {
            return verdict(Verdict::InterruptionNotReproduced);
        }
        None => {}
    }
    // 1. filtered (operator scope filters; the skip reason is retained on
    // the record by the caller)
    if flags.skipped.is_some() {
        return disposition(Disposition::Filtered);
    }
    // 2. eval-error
    if flags.eval_error {
        return disposition(Disposition::EvalError);
    }
    // 3. not-attemptable (plan-time set)
    if flags.plan_not_attemptable {
        return disposition(Disposition::NotAttemptable);
    }
    // 4. not-attempted
    if matches!(rio, RioOutcome::NotAttempted) {
        return disposition(Disposition::NotAttempted);
    }
    // 5/6. completed-without-execution discriminator: plan-snapshot valid ⇒
    // cached-prior; (not-attemptable already handled); else
    // target-substituted.
    if let RioOutcome::Built { executed: false } = rio {
        return if flags.plan_snapshot_valid {
            disposition(Disposition::CachedPrior)
        } else {
            disposition(Disposition::TargetSubstituted)
        };
    }
    // 7. infra / source-rot (incl. cascaded dependents per the cascade rule).
    match rio {
        RioOutcome::TargetFailed {
            kind: FailureKind::Infra,
        } => return verdict(Verdict::InfraIndeterminate),
        RioOutcome::TargetFailed {
            kind: FailureKind::SourceRot,
        } => {
            return verdict(Verdict::SourceUnavailable);
        }
        RioOutcome::DependencyFailed {
            root: RootCauseKind::Infra,
            ..
        } => {
            return Classification {
                class: UnifiedClass::Verdict(Verdict::InfraIndeterminate),
                cascaded: true,
            };
        }
        RioOutcome::DependencyFailed {
            root: RootCauseKind::SourceRot,
            ..
        } => {
            return Classification {
                class: UnifiedClass::Verdict(Verdict::SourceUnavailable),
                cascaded: true,
            };
        }
        _ => {}
    }
    // 8. Cross product, Hydra keyed first.
    match hydra {
        HydraOutcome::Unknown => match flags.resolve_unknown_divergent {
            Some(true) => disposition(Disposition::IdentityDivergent),
            _ => verdict(Verdict::NoTruth),
        },
        HydraOutcome::Failed => match rio {
            RioOutcome::Built { .. } => verdict(Verdict::UnexpectedSuccess),
            // Any rio failure shape agrees with the recorded failure.
            _ => verdict(Verdict::MatchFailed),
        },
        HydraOutcome::Built => match rio {
            RioOutcome::Built { .. } => verdict(Verdict::MatchBuilt),
            RioOutcome::DependencyFailed {
                root: RootCauseKind::Genuine,
                ..
            } => verdict(Verdict::UnexpectedDependencyFailure),
            RioOutcome::TargetFailed { .. } => verdict(Verdict::UnexpectedFailure),
            // Exhaustiveness: NotAttempted and Built{executed:false} were
            // handled above; DependencyFailed infra/source-rot handled above.
            _ => verdict(Verdict::UnexpectedFailure),
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

/// The verdict-level projection of "any recorded output hash differs":
/// `match-built` with a `differs` job-level NAR verdict becomes
/// `output-divergence`; every other verdict (and a non-differing
/// `match-built`) passes through unchanged. Callers compute `nar_verdict`
/// with [`job_nar_verdict`].
pub fn project_output_divergence(verdict: Verdict, nar_verdict: &str) -> Verdict {
    if verdict == Verdict::MatchBuilt && nar_verdict == NAR_DIFFERS {
        Verdict::OutputDivergence
    } else {
        verdict
    }
}

/// Headline + secondary metrics from verdict counts. The build-outcome
/// headline ratio is (match-built + output-divergence) over (match-built +
/// output-divergence + unexpected-failure + unexpected-dependency-failure);
/// every other verdict and every disposition is excluded from the
/// denominator. NAR agreement is the non-gating share of the headline
/// numerator that stayed `match-built` (i.e. no recorded output hash
/// differed).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Headline {
    pub numerator: usize,
    pub denominator: usize,
    pub headline_pct: Option<f64>,
    pub nar_equal: usize,
    pub nar_compared: usize,
    pub nar_agreement_pct: Option<f64>,
}

/// Compute the [`Headline`] metrics from final verdict counts (keyed by
/// [`Verdict::as_str`]) plus the job-level NAR comparison tallies. The NAR
/// agreement percentage is derived from the verdict counts (match-built
/// over match-built + output-divergence); the `nar_equal`/`nar_compared`
/// tallies are carried through verbatim for the summary line.
pub fn headline(
    verdict_counts: &BTreeMap<String, usize>,
    nar_equal: usize,
    nar_compared: usize,
) -> Headline {
    let get = |v: Verdict| verdict_counts.get(v.as_str()).copied().unwrap_or(0);
    let match_built = get(Verdict::MatchBuilt);
    let output_divergence = get(Verdict::OutputDivergence);
    let numerator = match_built + output_divergence;
    let denominator =
        numerator + get(Verdict::UnexpectedFailure) + get(Verdict::UnexpectedDependencyFailure);
    Headline {
        numerator,
        denominator,
        headline_pct: (denominator > 0).then(|| 100.0 * numerator as f64 / denominator as f64),
        nar_equal,
        nar_compared,
        nar_agreement_pct: (numerator > 0).then(|| 100.0 * match_built as f64 / numerator as f64),
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
    fn v(verdict: Verdict) -> UnifiedClass {
        UnifiedClass::Verdict(verdict)
    }
    fn d(disposition: Disposition) -> UnifiedClass {
        UnifiedClass::Disposition(disposition)
    }

    /// Spot rows straight out of the classification table + precedence list.
    #[rstest]
    // timed-interruption flags outrank every other rule: any (hydra, rio)
    // pair classifies into the interruption verdict, including over the
    // filtered flag (the previous precedence head).
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: true },
           AuxFlags { timed_interruption: Some(TimedInterruption::Replayed), ..flags() },
           v(Verdict::InterruptionReplayed), false)]
    #[case(HydraOutcome::Failed, failed(FailureKind::Genuine),
           AuxFlags { timed_interruption: Some(TimedInterruption::NotReproduced), ..flags() },
           v(Verdict::InterruptionNotReproduced), false)]
    #[case(HydraOutcome::Built, RioOutcome::NotAttempted,
           AuxFlags { timed_interruption: Some(TimedInterruption::Replayed),
                      skipped: Some("system".into()), ..flags() },
           v(Verdict::InterruptionReplayed), false)]
    // precedence head: filtered/eval-error/not-attemptable/not-attempted
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: true },
           AuxFlags { skipped: Some("system".into()), ..flags() }, d(Disposition::Filtered), false)]
    #[case(HydraOutcome::Built, failed(FailureKind::Genuine),
           AuxFlags { eval_error: true, ..flags() }, d(Disposition::EvalError), false)]
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: false },
           AuxFlags { plan_not_attemptable: true, ..flags() }, d(Disposition::NotAttemptable), false)]
    #[case(
        HydraOutcome::Built,
        RioOutcome::NotAttempted,
        flags(),
        d(Disposition::NotAttempted),
        false
    )]
    // completed-without-execution discriminator
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: false },
           AuxFlags { plan_snapshot_valid: true, ..flags() }, d(Disposition::CachedPrior), false)]
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: false }, flags(), d(Disposition::TargetSubstituted), false)]
    // infra / source rot (and their cascades)
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::Infra),
        flags(),
        v(Verdict::InfraIndeterminate),
        false
    )]
    #[case(
        HydraOutcome::Failed,
        failed(FailureKind::Infra),
        flags(),
        v(Verdict::InfraIndeterminate),
        false
    )]
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::SourceRot),
        flags(),
        v(Verdict::SourceUnavailable),
        false
    )]
    #[case(
        HydraOutcome::Built,
        dep(RootCauseKind::Infra),
        flags(),
        v(Verdict::InfraIndeterminate),
        true
    )]
    #[case(
        HydraOutcome::Built,
        dep(RootCauseKind::SourceRot),
        flags(),
        v(Verdict::SourceUnavailable),
        true
    )]
    // cross product, Hydra keyed first
    #[case(HydraOutcome::Unknown, RioOutcome::Built { executed: true }, flags(), v(Verdict::NoTruth), false)]
    #[case(HydraOutcome::Unknown, failed(FailureKind::Genuine),
           AuxFlags { resolve_unknown_divergent: Some(true), ..flags() }, d(Disposition::IdentityDivergent), false)]
    #[case(HydraOutcome::Failed, RioOutcome::Built { executed: true }, flags(), v(Verdict::UnexpectedSuccess), false)]
    #[case(
        HydraOutcome::Failed,
        failed(FailureKind::Genuine),
        flags(),
        v(Verdict::MatchFailed),
        false
    )]
    #[case(
        HydraOutcome::Failed,
        failed(FailureKind::Timeout),
        flags(),
        v(Verdict::MatchFailed),
        false
    )]
    #[case(
        HydraOutcome::Failed,
        dep(RootCauseKind::Genuine),
        flags(),
        v(Verdict::MatchFailed),
        false
    )]
    #[case(HydraOutcome::Built, RioOutcome::Built { executed: true }, flags(), v(Verdict::MatchBuilt), false)]
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::Genuine),
        flags(),
        v(Verdict::UnexpectedFailure),
        false
    )]
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::Timeout),
        flags(),
        v(Verdict::UnexpectedFailure),
        false
    )]
    #[case(
        HydraOutcome::Built,
        failed(FailureKind::ResourceCeiling),
        flags(),
        v(Verdict::UnexpectedFailure),
        false
    )]
    #[case(
        HydraOutcome::Built,
        dep(RootCauseKind::Genuine),
        flags(),
        v(Verdict::UnexpectedDependencyFailure),
        false
    )]
    fn design_rows(
        #[case] hydra: HydraOutcome,
        #[case] rio: RioOutcome,
        #[case] aux: AuxFlags,
        #[case] expected: UnifiedClass,
        #[case] cascaded: bool,
    ) {
        let c = classify(&hydra, &rio, &aux);
        assert_eq!(c.class, expected, "hydra={hydra:?} rio={rio:?} aux={aux:?}");
        assert_eq!(c.cascaded, cascaded);
    }

    /// The verdict-level projection of the per-output NAR comparison: only a
    /// differing match-built becomes output-divergence; every other verdict
    /// (and a non-differing match-built) passes through unchanged.
    #[rstest]
    #[case(Verdict::MatchBuilt, NAR_DIFFERS, Verdict::OutputDivergence)]
    #[case(Verdict::MatchFailed, NAR_DIFFERS, Verdict::MatchFailed)]
    #[case(Verdict::MatchBuilt, NAR_EQUAL, Verdict::MatchBuilt)]
    #[case(Verdict::MatchBuilt, NAR_NOT_COMPARABLE, Verdict::MatchBuilt)]
    fn output_divergence_projection(
        #[case] verdict: Verdict,
        #[case] nar_verdict: &str,
        #[case] expected: Verdict,
    ) {
        assert_eq!(project_output_divergence(verdict, nar_verdict), expected);
    }

    /// The infra-poisoned shared dependency scenario: the dependency itself
    /// is infra (excluded), its dependents cascade out of the denominator,
    /// and an unrelated genuine dependency failure stays in.
    #[test]
    fn infra_poisoned_shared_dependency_cascade() {
        // Dependent of the infra-poisoned dep: excluded, counted as cascaded.
        let c = classify(&HydraOutcome::Built, &dep(RootCauseKind::Infra), &flags());
        assert_eq!(
            (c.class, c.cascaded),
            (v(Verdict::InfraIndeterminate), true)
        );
        // Dependent of a genuine target failure: stays in the denominator.
        let c = classify(&HydraOutcome::Built, &dep(RootCauseKind::Genuine), &flags());
        assert_eq!(
            (c.class, c.cascaded),
            (v(Verdict::UnexpectedDependencyFailure), false)
        );
    }

    /// Exhaustive grid: every (hydra, rio, flag-combination) maps to exactly
    /// one verdict or disposition, the precedence holds (e.g. the filtered
    /// flag beats everything below the timed flag), and excluded-by-plan
    /// flags never leak into the match-built verdict.
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
        let timed = [
            None,
            Some(TimedInterruption::Replayed),
            Some(TimedInterruption::NotReproduced),
        ];
        let mut grid = 0usize;
        for hydra in &hydras {
            for rio in &rios {
                for skipped in &bools {
                    for eval_error in &bools {
                        for not_attemptable in &bools {
                            for snapshot_valid in &bools {
                                for rud in &resolve {
                                    for ti in &timed {
                                        let aux = AuxFlags {
                                            skipped: skipped.then(|| "filtered".to_string()),
                                            eval_error: *eval_error,
                                            plan_not_attemptable: *not_attemptable,
                                            plan_snapshot_valid: *snapshot_valid,
                                            resolve_unknown_divergent: *rud,
                                            timed_interruption: *ti,
                                        };
                                        grid += 1;
                                        let c = classify(hydra, rio, &aux);
                                        let ctx =
                                            format!("hydra={hydra:?} rio={rio:?} aux={aux:?}");
                                        // Total: every combination yields exactly one class —
                                        // a verdict or a disposition, never both, never neither.
                                        assert!(
                                            c.class.verdict().is_some()
                                                != c.class.disposition().is_some(),
                                            "{ctx}"
                                        );
                                        // Precedence assertions: the timed-interruption flag
                                        // wins over everything else, then the plan-time flags.
                                        if let Some(ti) = aux.timed_interruption {
                                            let expected = match ti {
                                                TimedInterruption::Replayed => {
                                                    Verdict::InterruptionReplayed
                                                }
                                                TimedInterruption::NotReproduced => {
                                                    Verdict::InterruptionNotReproduced
                                                }
                                            };
                                            assert_eq!(c.class, v(expected), "{ctx}");
                                            assert!(!c.cascaded, "{ctx}");
                                        } else if aux.skipped.is_some() {
                                            assert_eq!(c.class, d(Disposition::Filtered), "{ctx}");
                                        } else if aux.eval_error {
                                            assert_eq!(c.class, d(Disposition::EvalError), "{ctx}");
                                        } else if aux.plan_not_attemptable {
                                            assert_eq!(
                                                c.class,
                                                d(Disposition::NotAttemptable),
                                                "{ctx}"
                                            );
                                        } else if matches!(rio, RioOutcome::NotAttempted) {
                                            assert_eq!(
                                                c.class,
                                                d(Disposition::NotAttempted),
                                                "{ctx}"
                                            );
                                        }
                                        // The match-built verdict only ever comes from builds
                                        // this campaign actually executed.
                                        if c.class == v(Verdict::MatchBuilt) {
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
        }
        assert_eq!(
            grid,
            3 * 11 * 2 * 2 * 2 * 2 * 3 * 3,
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

    /// The headline numerator/denominator are an explicit verdict list, so
    /// the timed-only interruption verdicts never enter either side.
    #[test]
    fn headline_ignores_interruption_buckets() {
        let mut counts = BTreeMap::new();
        counts.insert(Verdict::MatchBuilt.as_str().to_string(), 90);
        counts.insert(Verdict::UnexpectedFailure.as_str().to_string(), 7);
        counts.insert(Verdict::UnexpectedDependencyFailure.as_str().to_string(), 3);
        let without = headline(&counts, 80, 85);
        counts.insert(Verdict::InterruptionReplayed.as_str().to_string(), 5);
        counts.insert(Verdict::InterruptionNotReproduced.as_str().to_string(), 2);
        let with = headline(&counts, 80, 85);
        assert_eq!(with.numerator, without.numerator);
        assert_eq!(with.denominator, without.denominator);
        assert_eq!(with.headline_pct, without.headline_pct);
    }

    #[test]
    fn headline_math() {
        let mut counts = BTreeMap::new();
        counts.insert(Verdict::MatchBuilt.as_str().to_string(), 90);
        counts.insert(Verdict::OutputDivergence.as_str().to_string(), 4);
        counts.insert(Verdict::UnexpectedFailure.as_str().to_string(), 7);
        counts.insert(Verdict::UnexpectedDependencyFailure.as_str().to_string(), 3);
        counts.insert(Verdict::InfraIndeterminate.as_str().to_string(), 5); // excluded
        let h = headline(&counts, 80, 85);
        assert_eq!(h.numerator, 94);
        assert_eq!(h.denominator, 104);
        assert!((h.headline_pct.unwrap() - (94.0 / 104.0 * 100.0)).abs() < 1e-9);
        // NAR agreement comes from the verdict counts: the share of the
        // headline numerator that stayed match-built.
        assert!((h.nar_agreement_pct.unwrap() - (90.0 / 94.0 * 100.0)).abs() < 1e-9);
        // The raw comparison tallies are carried through for the summary
        // line, not recomputed.
        assert_eq!((h.nar_equal, h.nar_compared), (80, 85));
        let empty = headline(&BTreeMap::new(), 0, 0);
        assert_eq!(empty.headline_pct, None);
        assert_eq!(empty.nar_agreement_pct, None);
    }
}

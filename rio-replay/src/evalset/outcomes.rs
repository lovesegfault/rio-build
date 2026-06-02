//! Maps the recorder's truth sources — cache.nixos.org narinfo presence
//! and Hydra buildstatus codes — into the neutral expected-outcome
//! vocabulary written to `outcomes.jsonl`.
//!
//! The outcome value and the record shape are the archive schema types
//! ([`crate::archive::schema`]); this module only decides which value
//! the recorder writes for each workload unit.

use std::collections::BTreeMap;

use crate::archive::schema::{ExpectedOutcome, OutcomeRecord, OutputHash};
use crate::nixcache::NarinfoFact;

/// Map a Hydra `buildstatus` code to the neutral expected outcome.
///
/// Per-code, sourced against Hydra's own status enum — `BuildStatus` in
/// `subprojects/crates/db/src/models.rs` and the `/build/<id>` API
/// contract in `hydra-api.yaml`, both at NixOS/hydra commit `3ccd938cd`
/// (<https://github.com/NixOS/hydra/blob/3ccd938cd7d0085a2915132369eb09e35991550b/subprojects/crates/db/src/models.rs>)
/// — so each native condition lands on the vocabulary value whose
/// [`crate::archive::schema`] contract describes it, instead of
/// collapsing into `failed` ("deterministic build failure attributable
/// to the unit itself"), which most non-zero codes are not:
///
/// - `0` Success → `built`.
/// - `4` Cancelled ("canceled by the user") → `cancelled`: the source
///   attempt ended before completion; there is no deterministic
///   expectation to compare against.
/// - `2` DepFailed, `3` Aborted, `9` Unsupported (the API doc presents
///   9 as "aborted": no machine of the required system type) →
///   `indeterminate`: the unit itself was never fairly attempted, so
///   the attempt is not usable truth.
/// - `7` TimedOut, `10` LogLimitExceeded, `11` NarSizeLimitExceeded →
///   `resource-exhausted`: source-side resource limits (build-time
///   quota, log size, output size) — compared like `failed` but
///   reported separately, so source limits never hide inside the
///   deterministic-failure counts.
/// - anything else → `failed`: `1` Failed and `6` FailedWithOutput are
///   real build failures, `12` NotDeterministic fails the unit's own
///   determinism check, and unknown or retired codes follow Hydra's own
///   API contract, which presents every undocumented code as failed
///   (`* : failed`). `8` CachedFailure and `13` Resolved are step-only
///   statuses a build's `buildstatus` never carries.
///
/// Every non-zero code keeps the raw numeric value verbatim in the
/// returned detail string (`hydra buildstatus=<code>`), so the native
/// status stays visible to humans even though the engine never
/// interprets `detail`.
pub fn outcome_from_buildstatus(status: i64) -> (ExpectedOutcome, Option<String>) {
    let outcome = match status {
        0 => return (ExpectedOutcome::Built, None),
        4 => ExpectedOutcome::Cancelled,
        2 | 3 | 9 => ExpectedOutcome::Indeterminate,
        7 | 10 | 11 => ExpectedOutcome::ResourceExhausted,
        _ => ExpectedOutcome::Failed,
    };
    (outcome, Some(format!("hydra buildstatus={status}")))
}

/// Decide the expected-outcome record for one workload unit.
///
/// An exact Hydra `buildstatus` — known for the jobs whose builds the
/// recorder fetched individually — always wins over narinfo presence.
/// Without one the upstream cache decides: every declared output present
/// upstream means `built`; anything less (no declared outputs, or any
/// output the swept cache does not serve) means `unknown`, never `failed`,
/// because the absence of a cache entry is absence of evidence, not proof
/// of failure.
///
/// `built` records carry the per-output NAR identity (`nar_hash_hex`,
/// `nar_size`) of every output whose swept narinfo had a usable hash —
/// both fields present in its [`NarinfoFact`]; outputs whose narinfo was
/// missing, malformed, or hash-less are omitted from the map rather than
/// invented. The recorder is timeless and session-less: `session`,
/// `duration_s`, and `stop_offset_s` are never set.
pub fn expected_outcome_for_unit(
    drv: &str,
    outputs: &BTreeMap<String, String>,
    facts: &BTreeMap<String, NarinfoFact>,
    buildstatus: Option<i64>,
) -> OutcomeRecord {
    let (outcome, detail) = match buildstatus {
        Some(status) => outcome_from_buildstatus(status),
        None => {
            let all_outputs_upstream = !outputs.is_empty()
                && outputs
                    .values()
                    .all(|path| facts.get(path).is_some_and(|fact| fact.found));
            let outcome = if all_outputs_upstream {
                ExpectedOutcome::Built
            } else {
                ExpectedOutcome::Unknown
            };
            (outcome, None)
        }
    };

    // Per-output NAR identity travels only on `built` records (that is what
    // the archive's `output_hashes` capability asserts). An entry needs both
    // the NarHash and the NarSize; a found-but-unusable narinfo yields
    // neither an entry nor a downgrade of the outcome.
    let output_hashes: BTreeMap<String, OutputHash> = if outcome == ExpectedOutcome::Built {
        outputs
            .iter()
            .filter_map(|(name, path)| {
                let fact = facts.get(path)?;
                let nar_hash = fact.nar_hash?;
                let nar_size = fact.nar_size?;
                Some((name.clone(), OutputHash { nar_hash, nar_size }))
            })
            .collect()
    } else {
        BTreeMap::new()
    };

    OutcomeRecord {
        session: None,
        drv: drv.to_string(),
        outcome,
        detail,
        duration_s: None,
        stop_offset_s: None,
        outputs: output_hashes,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::nixcache::NarinfoFact;

    fn fact(found: bool, hash: Option<&str>, size: Option<u64>) -> NarinfoFact {
        NarinfoFact {
            found,
            nar_hash: hash.map(|h| crate::narhash::NarHash::parse(h).unwrap()),
            nar_size: size,
        }
    }

    #[test]
    fn buildstatus_mapping_is_pinned() {
        // (status, expected outcome string, detail must contain) — one
        // row per code Hydra's BuildStatus enum defines (NixOS/hydra
        // 3ccd938cd, subprojects/crates/db/src/models.rs), plus an
        // unknown code. Every non-zero code keeps the raw numeric value
        // in detail; the engine never interprets it.
        let cases = [
            (0, "built", None),                                 // Success
            (1, "failed", Some("buildstatus=1")),               // Failed
            (2, "indeterminate", Some("buildstatus=2")),        // DepFailed
            (3, "indeterminate", Some("buildstatus=3")),        // Aborted
            (4, "cancelled", Some("buildstatus=4")),            // Cancelled (by the user)
            (6, "failed", Some("buildstatus=6")),               // FailedWithOutput
            (7, "resource-exhausted", Some("buildstatus=7")),   // TimedOut
            (8, "failed", Some("buildstatus=8")),               // CachedFailure (step-only)
            (9, "indeterminate", Some("buildstatus=9")),        // Unsupported system
            (10, "resource-exhausted", Some("buildstatus=10")), // LogLimitExceeded
            (11, "resource-exhausted", Some("buildstatus=11")), // NarSizeLimitExceeded
            (12, "failed", Some("buildstatus=12")),             // NotDeterministic
            (13, "failed", Some("buildstatus=13")),             // Resolved (step-only)
            (177, "failed", Some("buildstatus=177")),           // unknown codes stay failures
        ];
        for (status, want, detail_needle) in cases {
            let (outcome, detail) = outcome_from_buildstatus(status);
            assert_eq!(outcome.as_str(), want, "buildstatus {status}");
            match detail_needle {
                None => assert!(detail.is_none()),
                Some(n) => assert!(detail.as_deref().unwrap().contains(n)),
            }
        }
    }

    #[test]
    fn buildstatus_mapping_covers_the_neutral_vocabulary() {
        // The mapping's image over the build statuses Hydra's API
        // documents for `/build/<id>` (hydra-api.yaml: 0, 1, 2, 3, 4,
        // 6, 7, 9, 10, 11) must cover every neutral-vocabulary value a
        // Hydra recording can express — a re-collapse (e.g. every
        // non-zero code → failed) cannot land without failing this
        // assertion, even if the per-code pin above were edited to
        // match.
        let documented_build_codes = [0, 1, 2, 3, 4, 6, 7, 9, 10, 11];
        let image: std::collections::BTreeSet<&str> = documented_build_codes
            .iter()
            .map(|&status| outcome_from_buildstatus(status).0.as_str())
            .collect();
        let expressible: std::collections::BTreeSet<&str> = [
            ExpectedOutcome::Built,
            ExpectedOutcome::Failed,
            ExpectedOutcome::ResourceExhausted,
            ExpectedOutcome::Cancelled,
            ExpectedOutcome::Indeterminate,
        ]
        .map(ExpectedOutcome::as_str)
        .into();
        assert_eq!(
            image, expressible,
            "every expressible neutral outcome must be reachable from some documented Hydra code"
        );
        // The two remaining vocabulary values are structurally out of a
        // buildstatus's reach, by design rather than by collapse:
        // `disconnected` describes a recording client's disconnect, and
        // this recorder is timeless and session-less (the v0 recorder
        // maps it from its CLIENT_DISCONNECT status); `unknown` is
        // produced only by the narinfo-presence path when no
        // buildstatus exists at all (`expected_outcome_for_unit`).
    }

    #[test]
    fn narinfo_presence_mapping_is_pinned() {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x-1.0".to_string(),
        );
        outputs.insert(
            "dev".to_string(),
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x-1.0-dev".to_string(),
        );
        let mut facts = BTreeMap::new();
        facts.insert(
            outputs["out"].clone(),
            fact(true, Some("aa".repeat(32).as_str()), Some(7)),
        );
        facts.insert(
            outputs["dev"].clone(),
            fact(true, Some("bb".repeat(32).as_str()), Some(9)),
        );
        // All outputs present upstream → built, with per-output hashes carried over.
        let rec = expected_outcome_for_unit(
            "/nix/store/dddddddddddddddddddddddddddddddd-x-1.0.drv",
            &outputs,
            &facts,
            None,
        );
        assert_eq!(rec.outcome.as_str(), "built");
        assert_eq!(rec.outputs.len(), 2);
        assert_eq!(rec.outputs["out"].nar_size, 7);
        // Any output missing upstream → unknown (never failed), no outputs map.
        facts.get_mut(&outputs["dev"]).unwrap().found = false;
        let rec = expected_outcome_for_unit(
            "/nix/store/dddddddddddddddddddddddddddddddd-x-1.0.drv",
            &outputs,
            &facts,
            None,
        );
        assert_eq!(rec.outcome.as_str(), "unknown");
        assert!(rec.outputs.is_empty());
        // A found-but-unusable hash keeps the outcome built but omits that output's hash entry.
        facts.get_mut(&outputs["dev"]).unwrap().found = true;
        facts.get_mut(&outputs["dev"]).unwrap().nar_hash = None;
        let rec = expected_outcome_for_unit(
            "/nix/store/dddddddddddddddddddddddddddddddd-x-1.0.drv",
            &outputs,
            &facts,
            None,
        );
        assert_eq!(rec.outcome.as_str(), "built");
        assert_eq!(rec.outputs.len(), 1);
        // An exact buildstatus always wins over narinfo presence.
        let rec = expected_outcome_for_unit(
            "/nix/store/dddddddddddddddddddddddddddddddd-x-1.0.drv",
            &outputs,
            &facts,
            Some(1),
        );
        assert_eq!(rec.outcome.as_str(), "failed");
    }

    #[test]
    fn buildstatus_built_keeps_usable_hashes_and_timeless_fields_unset() {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "out".to_string(),
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x-1.0".to_string(),
        );
        let mut facts = BTreeMap::new();
        facts.insert(
            outputs["out"].clone(),
            fact(true, Some("cc".repeat(32).as_str()), Some(11)),
        );
        let drv = "/nix/store/dddddddddddddddddddddddddddddddd-x-1.0.drv";
        // A successful buildstatus and the narinfo sweep agree on `built`;
        // the swept per-output NAR identity is still carried so these units
        // keep output-hash comparison at replay time.
        let rec = expected_outcome_for_unit(drv, &outputs, &facts, Some(0));
        assert_eq!(rec.outcome.as_str(), "built");
        assert!(rec.detail.is_none());
        assert_eq!(rec.outputs.len(), 1);
        assert_eq!(rec.outputs["out"].nar_hash.to_hex(), "cc".repeat(32));
        assert_eq!(rec.outputs["out"].nar_size, 11);
        // Recorder-written records are timeless and session-less.
        assert_eq!(rec.drv, drv);
        assert!(rec.session.is_none());
        assert!(rec.duration_s.is_none());
        assert!(rec.stop_offset_s.is_none());
    }
}

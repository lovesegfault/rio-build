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
/// The mapping is deliberately coarse: `0` (success) becomes `built` with
/// no detail; every non-zero code becomes `failed`, with the raw numeric
/// code preserved verbatim in the returned detail string
/// (`hydra buildstatus=<code>`). Finer Hydra status semantics (timed out,
/// aborted, output limit exceeded, …) are never interpreted — they stay
/// visible to humans through the detail string only.
pub fn outcome_from_buildstatus(status: i64) -> (ExpectedOutcome, Option<String>) {
    if status == 0 {
        (ExpectedOutcome::Built, None)
    } else {
        (
            ExpectedOutcome::Failed,
            Some(format!("hydra buildstatus={status}")),
        )
    }
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
        // (status, expected outcome string, detail must contain).
        // The recorder mapping is deliberately coarse: 0 → built, every
        // non-zero Hydra buildstatus → failed with the numeric code kept
        // in detail (the design's truth-baking rule); finer Hydra status
        // semantics stay visible through the detail string only.
        let cases = [
            (0, "built", None),
            (1, "failed", Some("buildstatus=1")),
            (2, "failed", Some("buildstatus=2")),
            (3, "failed", Some("buildstatus=3")),
            (4, "failed", Some("buildstatus=4")),
            (6, "failed", Some("buildstatus=6")),
            (7, "failed", Some("buildstatus=7")),
            (9, "failed", Some("buildstatus=9")),
            (10, "failed", Some("buildstatus=10")),
            (11, "failed", Some("buildstatus=11")),
            (177, "failed", Some("buildstatus=177")),
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

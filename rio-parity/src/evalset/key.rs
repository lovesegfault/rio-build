//! The reproduction-recipe key: every input that determines what the
//! recorder evaluates, hashed into the recipe digest.
//!
//! The digest covers the jobset coordinates, the requested systems, the
//! evaluation scope, and the eval-logic version (engine version, nix
//! and nix-eval-jobs versions, and the hash of the generated
//! entry-point/args expression). It is the recorder's idempotency
//! handle: it names the local output directory, is recorded in the
//! archive provenance as `recipe_digest`, and keys the by-recipe
//! pointer that lets a re-run find the archive an identical recipe
//! already produced. Any recipe or tooling change yields a new digest.

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::evalset::Scope;

/// Identity of one reproduction recipe: every input that determines
/// what gets evaluated and recorded.
/// Hashed by [`EvalSetKey::digest`]; the SHORT form of that digest
/// ([`EvalSetKey::short_digest`], first 16 hex chars) is what names the
/// local output directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSetKey {
    pub hydra_eval_id: u64,
    pub project: String,
    pub jobset: String,
    /// Systems in scope, sorted and deduplicated (callers normalize CLI
    /// input through [`EvalSetKey::normalize_systems`]) so that
    /// `--systems` argument order or repetition can never fork the
    /// digest of an otherwise identical recipe.
    pub systems: Vec<String>,
    pub scope: Scope,
    pub engine_version: String,
    pub nix_version: String,
    pub nix_eval_jobs_version: String,
    /// SHA-256 (hex) of the generated argv + selection expression — the
    /// "hash of the generated entry-point/args expression" component.
    ///
    /// Both inputs embed absolute paths under the chosen work/output
    /// directories (the unpacked source tree, selection.nix, the
    /// gc-roots dir), so the digest is reproducible only for runs using
    /// the same work and output directories — not across machines or
    /// operators.
    pub args_expr_sha256: String,
    /// Set by `--force`: salts the digest so a forced re-record is a
    /// new recipe (new by-recipe pointer, new archive) instead of being
    /// skipped as already recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forced_at: Option<String>,
}

impl EvalSetKey {
    /// Sort + dedupe a systems list into the canonical form the
    /// `systems` field requires, so two invocations that only differ in
    /// `--systems` order or repetition produce the same key digest.
    pub fn normalize_systems(mut systems: Vec<String>) -> Vec<String> {
        systems.sort();
        systems.dedup();
        systems
    }

    /// Full hex SHA-256 over the serde_json encoding of the key.
    ///
    /// That encoding is part of the identity: fields serialize in
    /// declaration order with their snake_case names, and [`Scope`]
    /// keeps its kebab-case `kind` tag with snake_case fields — the
    /// same casing the recorder's other JSON artifacts (fidelity.json,
    /// the provenance block) use. Changing any serde attribute here or
    /// on [`Scope`] changes every digest, orphaning every existing
    /// by-recipe pointer and provenance `recipe_digest`, so the golden
    /// test below pins the current encoding.
    pub fn digest(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("EvalSetKey serializes");
        hex::encode(sha2::Sha256::digest(&canonical))
    }

    /// First 16 hex chars of [`EvalSetKey::digest`] — used in the local
    /// output directory name and operator-facing identifiers.
    pub fn short_digest(&self) -> String {
        self.digest()[..16].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evalset::Scope;

    fn key() -> EvalSetKey {
        EvalSetKey {
            hydra_eval_id: 1824219,
            project: "nixos".into(),
            jobset: "unstable".into(),
            systems: vec!["x86_64-linux".into()],
            scope: Scope::Jobs {
                jobs: vec!["nixpkgs.hello.x86_64-linux".into()],
            },
            engine_version: "0.1.0".into(),
            nix_version: "nix (Nix) 2.34.7".into(),
            nix_eval_jobs_version: "nix-eval-jobs 2.34.0".into(),
            args_expr_sha256: "deadbeef".into(),
            forced_at: None,
        }
    }

    #[test]
    fn digest_is_deterministic_and_sensitive_to_every_field() {
        let a = key();
        assert_eq!(a.digest(), key().digest(), "same key ⇒ same digest");
        assert_eq!(a.short_digest().len(), 16);
        assert!(a.digest().starts_with(&a.short_digest()));

        let mut b = key();
        b.systems = vec!["aarch64-linux".into()];
        assert_ne!(a.digest(), b.digest());

        let mut c = key();
        c.scope = Scope::Constituents {
            aggregate_job: "tested".into(),
        };
        assert_ne!(a.digest(), c.digest());

        let mut d = key();
        d.nix_eval_jobs_version = "nix-eval-jobs 9.9.9".into();
        assert_ne!(a.digest(), d.digest());

        // --force salts the key with a timestamp ⇒ a NEW prefix (eval
        // sets are write-once, so a forced rebuild must never land on
        // the existing one).
        let mut e = key();
        e.forced_at = Some("2026-05-26T12:00:00Z".into());
        assert_ne!(a.digest(), e.digest());
    }

    #[test]
    fn normalized_systems_make_the_digest_order_independent() {
        // The same systems set spelled in different orders (and with a
        // duplicate) must land on the same digest once normalized —
        // otherwise CLI argument order would fork S3 prefixes.
        let mut a = key();
        a.systems = EvalSetKey::normalize_systems(vec![
            "x86_64-linux".into(),
            "aarch64-linux".into(),
            "x86_64-linux".into(),
        ]);
        let mut b = key();
        b.systems =
            EvalSetKey::normalize_systems(vec!["aarch64-linux".into(), "x86_64-linux".into()]);
        assert_eq!(
            a.systems,
            vec!["aarch64-linux".to_string(), "x86_64-linux".to_string()]
        );
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn digest_golden_value_pins_the_canonical_encoding() {
        // Pinned so an accidental serialization change (field order,
        // rename, Scope field casing) is caught — if this changes
        // intentionally, every existing S3 prefix is orphaned, so bump
        // deliberately.
        let d = key().digest();
        assert_eq!(
            d,
            "8b919129046e0f60b9142c44550a563e3a4e70e695587c3189655b755a7ac83a"
        );
        assert_eq!(key().short_digest(), d[..16].to_string());
        assert_eq!(d.len(), 64);
        assert!(d.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn scope_serialization_keeps_the_digest_encoding() {
        // The scope is embedded verbatim in the archive provenance
        // (inside `recipe` and at top level); both must use the exact
        // encoding the digest was computed over — kebab-case `kind` tag,
        // snake_case fields — so the recorded documents can never
        // disagree with the digest they are filed under.
        assert_eq!(
            serde_json::to_value(&key().scope).unwrap(),
            serde_json::json!({"kind": "jobs", "jobs": ["nixpkgs.hello.x86_64-linux"]})
        );
        // The Constituents variant's field stays snake_case too.
        assert_eq!(
            serde_json::to_value(Scope::Constituents {
                aggregate_job: "tested".into()
            })
            .unwrap(),
            serde_json::json!({"kind": "constituents", "aggregate_job": "tested"})
        );
    }
}

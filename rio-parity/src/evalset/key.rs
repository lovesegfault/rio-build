//! Eval-set identity: the key digest and the evalset.json metadata.
//!
//! An eval set is addressed as `<hydra-eval-id>/<key-digest>`: the
//! digest covers the jobset coordinates, the requested systems, the
//! evaluation scope, and the eval-logic version (engine version, nix
//! and nix-eval-jobs versions, and the hash of the generated
//! entry-point/args expression). The same evaluation built with the
//! same recipe therefore lands on the same — write-once — prefix,
//! while any recipe or tooling change yields a new digest and a new
//! prefix alongside the old one.

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::evalset::Scope;

/// Identity of one eval set: every input that determines its contents.
/// Hashed by [`EvalSetKey::digest`] into the digest that names the S3
/// prefix and the local output directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSetKey {
    pub hydra_eval_id: u64,
    pub project: String,
    pub jobset: String,
    pub systems: Vec<String>,
    pub scope: Scope,
    pub engine_version: String,
    pub nix_version: String,
    pub nix_eval_jobs_version: String,
    /// SHA-256 (hex) of the generated argv + selection expression — the
    /// "hash of the generated entry-point/args expression" component.
    pub args_expr_sha256: String,
    /// Set by `--force`: salts the digest so a forced rebuild lands in
    /// a NEW prefix instead of overwriting the write-once one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forced_at: Option<String>,
}

impl EvalSetKey {
    /// Full hex SHA-256 over the serde_json encoding of the key.
    ///
    /// That encoding is part of the identity: fields serialize in
    /// declaration order with their snake_case names, and [`Scope`]
    /// keeps its kebab-case `kind` tag with snake_case fields — the
    /// same casing the rest of the eval-set artifacts (fidelity.json,
    /// evalset.json) use; only manifest.jsonl mirrors nix-eval-jobs'
    /// camelCase. Changing any serde attribute here or on [`Scope`]
    /// changes every digest and orphans every existing S3 prefix, so
    /// the golden test below pins the current encoding.
    pub fn digest(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("EvalSetKey serializes");
        hex::encode(sha2::Sha256::digest(&canonical))
    }

    /// First 16 hex chars of [`EvalSetKey::digest`] — used in S3
    /// prefixes and Job names.
    pub fn short_digest(&self) -> String {
        self.digest()[..16].to_string()
    }
}

/// Per-set statistics recorded in evalset.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvalSetStats {
    pub in_scope_jobs: usize,
    pub manifest_records: usize,
    pub eval_errors: usize,
    pub aggregates_excluded: usize,
    pub dep_closure_records: usize,
    pub ca_outputs: usize,
    pub hydra_requests_used: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_bytes: Option<u64>,
}

/// Contents of `evalset.json`: the key and its digests, the resolved
/// nixpkgs revision/source, the jobset configuration as fetched from
/// Hydra, the exact evaluator invocation (including the derived
/// revCount/shortRev), the systems and scope, per-set statistics, and
/// the fidelity verdict — everything needed to audit how the eval set
/// was produced without re-deriving it.
#[derive(Debug, Clone, Serialize)]
pub struct EvalSetMeta {
    pub key: EvalSetKey,
    pub key_digest: String,
    pub key_short_digest: String,
    pub hydra_eval_id: u64,
    pub nixpkgs_revision: String,
    pub project: String,
    pub jobset: String,
    /// Raw jobset config JSON as fetched from Hydra.
    pub jobset_config: serde_json::Value,
    pub source_store_path: String,
    pub rev_count: u64,
    pub short_rev: String,
    pub evaluator_program: String,
    pub evaluator_argv: Vec<String>,
    pub systems: Vec<String>,
    pub scope: Scope,
    pub dry_run: bool,
    pub fidelity_divergent: bool,
    pub stats: EvalSetStats,
    pub created_at: String,
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
    fn evalset_meta_keeps_scope_encoding_in_sync_with_the_key() {
        // evalset.json embeds the scope twice (inside `key` and at top
        // level); both must use the exact encoding the digest was
        // computed over — kebab-case `kind` tag, snake_case fields — so
        // the document can never disagree with the prefix it lives
        // under.
        let key = key();
        let meta = EvalSetMeta {
            key: key.clone(),
            key_digest: key.digest(),
            key_short_digest: key.short_digest(),
            hydra_eval_id: key.hydra_eval_id,
            nixpkgs_revision: "68d8aa3d661f0e6bd5862291b5bb263b2a6595c9".into(),
            project: key.project.clone(),
            jobset: key.jobset.clone(),
            jobset_config: serde_json::json!({"enabled": 1}),
            source_store_path: "/nix/store/gay80fqbpm2wakbsyd4in44gx0cwx3h5-source".into(),
            rev_count: 975402,
            short_rev: "68d8aa3d661f".into(),
            evaluator_program: "nix-eval-jobs".into(),
            evaluator_argv: vec!["--meta".into()],
            systems: key.systems.clone(),
            scope: key.scope.clone(),
            dry_run: false,
            fidelity_divergent: false,
            stats: EvalSetStats::default(),
            created_at: "2026-05-26T12:00:00Z".into(),
        };
        let v = serde_json::to_value(&meta).unwrap();
        assert_eq!(v["scope"], v["key"]["scope"]);
        assert_eq!(
            v["scope"],
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
        // Stats keep their snake_case names and omit the absent archive
        // size instead of writing null.
        assert_eq!(v["stats"]["in_scope_jobs"], 0);
        assert!(v["stats"].get("archive_bytes").is_none());
    }
}

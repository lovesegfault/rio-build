//! Serde types for the v1 replay-archive members.
//!
//! Field names follow the archive format specification in
//! `docs/dev/2026-05-28-build-replay-design.md` ("Archive format v1"):
//! snake_case JSON, unknown fields ignored, `.jsonl` members one record
//! per line. These types are shared by the reader, the writer, and the
//! recorders; nothing outside `provenance` may carry source-specific
//! vocabulary.
//!
//! Derive policy: types without `f64` fields also derive `Eq`; `Default`
//! is derived only where an all-default value is meaningful (capability,
//! count, presence, and substituter containers), not for records whose
//! required fields have no sensible default.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `manifest.json` — archive metadata, capabilities, provenance, integrity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// `"MAJOR.MINOR"`; absence of this field identifies a v0 archive.
    pub format_version: String,
    pub created_at: jiff::Timestamp,
    /// Start of the recorded window (offsets are relative to this).
    pub from: jiff::Timestamp,
    /// End of the recorded window. Timeless recorders set `from == to`.
    pub to: jiff::Timestamp,
    pub capabilities: Capabilities,
    /// Informational; mismatches with member contents are warnings, not errors.
    pub counts: Counts,
    #[serde(default)]
    pub substituters: Substituters,
    /// Recorder's claim that the archive embeds everything required beyond
    /// what the target must build itself. Advisory.
    #[serde(default)]
    pub fat: bool,
    /// Opaque recorder metadata, carried verbatim into campaign records and
    /// reports; never interpreted by the engine.
    pub provenance: serde_json::Map<String, serde_json::Value>,
    /// Integrity table over the metadata members present in the archive
    /// (member path → digest). `manifest.json` itself is never listed.
    pub files: BTreeMap<String, MemberDigest>,
    /// Aggregate digests over bulk content and narinfo sidecars.
    pub content_digests: ContentDigests,
}

/// Capability flags: what the archive contains. All default to `false`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub timed: bool,
    #[serde(default)]
    pub expected_outcomes: bool,
    #[serde(default)]
    pub output_hashes: bool,
    #[serde(default)]
    pub embedded_store_paths: bool,
    #[serde(default)]
    pub impure_env: bool,
    #[serde(default)]
    pub dependency_closures: bool,
}

/// Which optional data an archive actually carries; the backing-data check
/// for [`Capabilities`] (shared by the reader and the writer).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemberPresence {
    pub outcomes: bool,
    pub units: bool,
    pub closures: bool,
    pub impure_env: bool,
    pub exclusions: bool,
    pub embedded_store_paths: bool,
}

impl Capabilities {
    /// Error when a flag is set but the member backing it is absent; every
    /// unbacked flag is reported in the one error message.
    /// (`timed` has no backing member; `output_hashes` requires the
    /// outcomes member because per-output hashes live there.)
    pub fn require_backing_members(&self, present: &MemberPresence) -> anyhow::Result<()> {
        let mut missing: Vec<(&str, &str)> = Vec::new();
        if self.expected_outcomes && !present.outcomes {
            missing.push(("expected_outcomes", super::OUTCOMES_MEMBER));
        }
        if self.output_hashes && !present.outcomes {
            missing.push(("output_hashes", super::OUTCOMES_MEMBER));
        }
        if self.impure_env && !present.impure_env {
            missing.push(("impure_env", super::IMPURE_ENV_MEMBER));
        }
        if self.dependency_closures && !present.closures {
            missing.push(("dependency_closures", super::CLOSURES_MEMBER));
        }
        if self.embedded_store_paths && !present.embedded_store_paths {
            missing.push(("embedded_store_paths", "an embedded non-drv store path"));
        }
        if missing.is_empty() {
            return Ok(());
        }
        let detail = missing
            .iter()
            .map(|(flag, member)| {
                format!("capability `{flag}` is set but {member} is absent from the archive")
            })
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("{detail}")
    }
}

/// Informational counts so operators and tools can size a campaign without
/// scanning members.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub workload_units: u64,
    #[serde(default)]
    pub expected_outcomes: u64,
    #[serde(default)]
    pub embedded_drvs: u64,
    #[serde(default)]
    pub embedded_store_paths: u64,
}

/// Binary caches associated with the archive. `relay`: caches the engine may
/// relay content from at campaign time (https:// or s3:// only). `target`:
/// advisory list of caches the recorder expects the target's tenants to use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Substituters {
    #[serde(default)]
    pub relay: Vec<String>,
    #[serde(default)]
    pub target: Vec<String>,
}

/// SHA-256 + size of one metadata member (and of one S3 object in the
/// upload completion marker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberDigest {
    /// Lowercase hex SHA-256 of the member's bytes.
    pub sha256: String,
    /// Byte length.
    pub size: u64,
}

/// Aggregate digests over the bulk content and narinfo sidecars; together
/// with `files` they make `archive_id` a content address over the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentDigests {
    /// Digest of the canonical embedded-derivation listing.
    pub drvs: String,
    /// Digest of the canonical embedded-store-path listing (per-path digest =
    /// SHA-256 of the uncompressed NAR serialization).
    pub embedded_store_paths: String,
    /// Digest of the canonical narinfo-sidecar listing (per-sidecar digest =
    /// SHA-256 of the sidecar file's bytes).
    pub narinfo: String,
}

/// One `requests.jsonl` record: a recorded client submission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Opaque grouping key for the recorded client connection.
    #[serde(default)]
    pub session: i64,
    /// Seconds after `manifest.from`; meaningful only when `timed`.
    /// Negative values are clamped to 0 at load.
    #[serde(default)]
    pub offset_s: f64,
    /// The derivations (and outputs) the client asked for. Non-empty.
    pub targets: Vec<RequestTarget>,
}

/// One requested derivation within a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestTarget {
    pub drv: String,
    /// `[]` and `["*"]` both mean all outputs; writers normalize to `["*"]`.
    #[serde(default)]
    pub outputs: Vec<String>,
}

/// One `units.jsonl` record: display/filter metadata for a workload unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitRecord {
    pub drv: String,
    /// Human-facing name (e.g. a Hydra job name), used by filters and reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Statically declared output paths (output name → store path).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_features: Vec<String>,
    /// Set by the recorder when its fidelity gate found this unit's
    /// derivation identity divergent from the recorded source.
    #[serde(default)]
    pub identity_divergent: bool,
}

/// One `outcomes.jsonl` record: recorded truth for one (session, unit).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// `None` applies to any request of the unit; `Some` scopes the
    /// expectation to that recorded session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<i64>,
    pub drv: String,
    pub outcome: ExpectedOutcome,
    /// Free-form, human-readable detail (native status text/code). Never
    /// interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Wall-clock duration of the source attempt, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<f64>,
    /// Offset from `manifest.from` at which the source attempt stopped;
    /// meaningful only in timed archives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_offset_s: Option<f64>,
    /// Expected per-output content for `built` outcomes.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, OutputHash>,
}

/// Expected NAR identity of one output of a `built` outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputHash {
    /// Lowercase hex SHA-256 of the uncompressed NAR.
    pub nar_hash_hex: String,
    pub nar_size: u64,
}

/// The neutral expected-outcome vocabulary. Recorders map their native
/// status codes into these values at archive-creation time; the engine
/// never sees a native code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExpectedOutcome {
    /// The source completed the unit successfully.
    Built,
    /// Deterministic build failure attributable to the unit itself.
    Failed,
    /// The unit hit a source-side resource limit (memory, disk, build-time
    /// quota); compared like `failed` but reportable separately.
    ResourceExhausted,
    /// The source attempt was cancelled before completion.
    Cancelled,
    /// The recording client disconnected before the unit finished.
    Disconnected,
    /// The source attempt ended for infrastructure reasons; not usable truth.
    Indeterminate,
    /// The recorder looked and could not determine an outcome.
    Unknown,
}

impl ExpectedOutcome {
    /// The wire string of this outcome — the kebab-case form written to
    /// `outcomes.jsonl` — for callers that need it without a serde round
    /// trip (log fields, golden assertions, report buckets).
    pub const fn as_str(self) -> &'static str {
        match self {
            ExpectedOutcome::Built => "built",
            ExpectedOutcome::Failed => "failed",
            ExpectedOutcome::ResourceExhausted => "resource-exhausted",
            ExpectedOutcome::Cancelled => "cancelled",
            ExpectedOutcome::Disconnected => "disconnected",
            ExpectedOutcome::Indeterminate => "indeterminate",
            ExpectedOutcome::Unknown => "unknown",
        }
    }
}

/// One `closures.jsonl` record: direct dependency adjacency for one
/// derivation in the union requisite closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosureRecord {
    pub drv: String,
    /// Direct input derivations (`inputDrvs` keys). May be empty.
    pub inputs: Vec<String>,
    /// Direct input sources (`inputSrcs`).
    #[serde(default)]
    pub srcs: Vec<String>,
    /// Statically declared output paths; `None` for floating
    /// content-addressed outputs.
    pub outputs: BTreeMap<String, Option<String>>,
}

/// The `impure-env.json` member: derivation store path → impure environment
/// variable names the derivation declares.
pub type ImpureEnv = BTreeMap<String, Vec<String>>;

/// One `exclusions.jsonl` record: a scope item the recorder could not turn
/// into a workload unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drv: Option<String>,
    /// Recommended values: the `EXCLUSION_REASON_*` constants. Free-form
    /// values are permitted.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Recommended `ExclusionRecord::reason` values.
pub const EXCLUSION_REASON_EVAL_ERROR: &str = "eval-error";
pub const EXCLUSION_REASON_AGGREGATE: &str = "aggregate";
pub const EXCLUSION_REASON_UNSUPPORTED: &str = "unsupported";

/// Parse a `format_version` string (`"MAJOR.MINOR"`). Returns (major, minor).
/// Unknown majors are refused; any minor of a known major is accepted.
pub fn parse_format_version(version: &str) -> anyhow::Result<(u64, u64)> {
    let (major, minor) = version.split_once('.').ok_or_else(|| {
        anyhow::anyhow!("malformed format_version {version:?} (want MAJOR.MINOR)")
    })?;
    let major: u64 = major
        .parse()
        .map_err(|_| anyhow::anyhow!("malformed format_version {version:?} (want MAJOR.MINOR)"))?;
    let minor: u64 = minor
        .parse()
        .map_err(|_| anyhow::anyhow!("malformed format_version {version:?} (want MAJOR.MINOR)"))?;
    anyhow::ensure!(
        major == super::SUPPORTED_MAJOR,
        "unsupported archive format_version {version} (supported major: {})",
        super::SUPPORTED_MAJOR
    );
    Ok((major, minor))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn expected_outcome_wire_encoding_is_kebab_case() {
        let cases = [
            (ExpectedOutcome::Built, "built"),
            (ExpectedOutcome::Failed, "failed"),
            (ExpectedOutcome::ResourceExhausted, "resource-exhausted"),
            (ExpectedOutcome::Cancelled, "cancelled"),
            (ExpectedOutcome::Disconnected, "disconnected"),
            (ExpectedOutcome::Indeterminate, "indeterminate"),
            (ExpectedOutcome::Unknown, "unknown"),
        ];
        for (variant, wire) in cases {
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                json!(wire),
                "wire encoding of {variant:?}"
            );
            let parsed: ExpectedOutcome = serde_json::from_value(json!(wire)).unwrap();
            assert_eq!(parsed, variant, "parse of {wire:?}");
            assert_eq!(variant.as_str(), wire, "as_str of {variant:?}");
        }
    }

    #[test]
    fn request_and_outcome_records_round_trip_with_defaults() {
        let request: RequestRecord =
            serde_json::from_value(json!({"targets": [{"drv": "/nix/store/x-a.drv"}]})).unwrap();
        assert_eq!(request.session, 0);
        assert_eq!(request.offset_s, 0.0);
        assert_eq!(request.targets.len(), 1);
        assert_eq!(request.targets[0].drv, "/nix/store/x-a.drv");
        assert!(request.targets[0].outputs.is_empty());

        let outcome: OutcomeRecord =
            serde_json::from_value(json!({"drv": "/nix/store/x-a.drv", "outcome": "built"}))
                .unwrap();
        assert_eq!(outcome.session, None);
        assert_eq!(outcome.detail, None);
        assert!(outcome.outputs.is_empty());

        let serialized = serde_json::to_value(&outcome).unwrap();
        let fields = serialized.as_object().unwrap();
        assert!(!fields.contains_key("session"), "got: {serialized}");
        assert!(!fields.contains_key("detail"), "got: {serialized}");
        assert!(!fields.contains_key("outputs"), "got: {serialized}");
    }

    #[test]
    fn unknown_fields_are_ignored_in_known_members() {
        let request: RequestRecord = serde_json::from_value(json!({
            "targets": [{"drv": "/nix/store/x-a.drv"}],
            "extra_recorder_field": "x",
        }))
        .unwrap();
        assert_eq!(request.targets.len(), 1);

        let manifest: Manifest = serde_json::from_value(json!({
            "format_version": "1.0",
            "created_at": "2026-05-01T00:00:00Z",
            "from": "2026-05-01T00:00:00Z",
            "to": "2026-05-01T00:00:00Z",
            "capabilities": {},
            "counts": {},
            "provenance": {},
            "files": {},
            "content_digests": {
                "drvs": "d",
                "embedded_store_paths": "e",
                "narinfo": "n",
            },
            "extra_recorder_field": "x",
        }))
        .unwrap();
        assert_eq!(manifest.format_version, "1.0");
    }

    #[test]
    fn capabilities_default_to_false_and_unknown_flags_are_ignored() {
        let absent: Capabilities = serde_json::from_value(json!({})).unwrap();
        assert_eq!(absent, Capabilities::default());
        assert!(!absent.timed);
        assert!(!absent.expected_outcomes);
        assert!(!absent.output_hashes);
        assert!(!absent.embedded_store_paths);
        assert!(!absent.impure_env);
        assert!(!absent.dependency_closures);

        let timed: Capabilities =
            serde_json::from_value(json!({"timed": true, "future_flag": true})).unwrap();
        assert!(timed.timed);
        assert!(!timed.expected_outcomes);
    }

    #[test]
    fn format_version_major_gate() {
        assert_eq!(parse_format_version("1.0").unwrap(), (1, 0));
        assert_eq!(parse_format_version("1.7").unwrap(), (1, 7));

        let err = parse_format_version("2.0").unwrap_err().to_string();
        assert!(
            err.contains("unsupported archive format_version 2.0"),
            "got: {err}"
        );

        let err = parse_format_version("garbage").unwrap_err().to_string();
        assert!(err.contains("malformed format_version"), "got: {err}");
    }

    #[test]
    fn capability_backing_member_check() {
        let cases = [
            (
                Capabilities {
                    expected_outcomes: true,
                    ..Default::default()
                },
                "expected_outcomes",
                MemberPresence {
                    outcomes: true,
                    ..Default::default()
                },
            ),
            (
                Capabilities {
                    dependency_closures: true,
                    ..Default::default()
                },
                "dependency_closures",
                MemberPresence {
                    closures: true,
                    ..Default::default()
                },
            ),
            (
                Capabilities {
                    impure_env: true,
                    ..Default::default()
                },
                "impure_env",
                MemberPresence {
                    impure_env: true,
                    ..Default::default()
                },
            ),
            (
                Capabilities {
                    embedded_store_paths: true,
                    ..Default::default()
                },
                "embedded_store_paths",
                MemberPresence {
                    embedded_store_paths: true,
                    ..Default::default()
                },
            ),
        ];
        for (capabilities, flag, satisfying) in cases {
            let err = capabilities
                .require_backing_members(&MemberPresence::default())
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("capability `{flag}` is set")),
                "got: {err}"
            );
            capabilities.require_backing_members(&satisfying).unwrap();
        }

        // Several unbacked capabilities are reported together in one error.
        let err = Capabilities {
            expected_outcomes: true,
            impure_env: true,
            dependency_closures: true,
            ..Default::default()
        }
        .require_backing_members(&MemberPresence::default())
        .unwrap_err()
        .to_string();
        for flag in ["expected_outcomes", "impure_env", "dependency_closures"] {
            assert!(
                err.contains(&format!("capability `{flag}` is set")),
                "got: {err}"
            );
        }

        Capabilities::default()
            .require_backing_members(&MemberPresence::default())
            .unwrap();
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let captured_at = jiff::Timestamp::from_second(0).unwrap();
        let mut provenance = serde_json::Map::new();
        provenance.insert("recorder".to_string(), json!("rio-parity-eval"));
        let manifest = Manifest {
            format_version: crate::archive::FORMAT_VERSION.to_string(),
            created_at: captured_at,
            from: captured_at,
            to: captured_at,
            capabilities: Capabilities {
                expected_outcomes: true,
                ..Default::default()
            },
            counts: Counts {
                requests: 2,
                workload_units: 3,
                expected_outcomes: 3,
                embedded_drvs: 5,
                embedded_store_paths: 1,
            },
            substituters: Substituters {
                relay: vec!["https://cache.nixos.org".to_string()],
                target: Vec::new(),
            },
            fat: false,
            provenance,
            files: BTreeMap::from([(
                crate::archive::REQUESTS_MEMBER.to_string(),
                MemberDigest {
                    sha256: "0".repeat(64),
                    size: 123,
                },
            )]),
            content_digests: ContentDigests {
                drvs: "1".repeat(64),
                embedded_store_paths: "2".repeat(64),
                narinfo: "3".repeat(64),
            },
        };

        let serialized = serde_json::to_value(&manifest).unwrap();
        let created_at = serialized["created_at"].as_str().unwrap();
        assert!(
            created_at.starts_with("19") || created_at.starts_with("20"),
            "got: {created_at}"
        );
        assert!(created_at.contains('T'), "got: {created_at}");

        let round_tripped: Manifest = serde_json::from_value(serialized).unwrap();
        assert_eq!(round_tripped, manifest);
    }
}

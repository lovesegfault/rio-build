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
    /// unbacked flag is reported in the one error message. The flag →
    /// backing-member relation is [`Capability::backing`]'s data, so the
    /// reader and the writer cannot check different relations.
    pub fn require_backing_members(&self, present: &MemberPresence) -> anyhow::Result<()> {
        let mut missing: Vec<(&str, &str)> = Vec::new();
        for capability in Capability::ALL {
            if !capability.enabled_in(self) {
                continue;
            }
            if let Some((member, is_present)) = capability.backing(present)
                && !is_present
            {
                missing.push((capability.flag(), member));
            }
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

/// The closed set of archive capabilities — one variant per
/// [`Capabilities`] field, carrying everything the engine and the docs
/// know about each flag as data: its wire name, whether a given manifest
/// declares it, the staged data that must back it, and the design-doc
/// table row describing what it means and what gates on it (including the
/// absent-case behavior).
///
/// This is the single surface for capability decisions. Engine gate sites
/// ask [`Capability::enabled_in`] instead of reading raw manifest booleans,
/// the reader/writer backing checks iterate [`Capability::ALL`], and the
/// design doc's capability table is rendered from
/// [`capability_table_markdown`] and pinned by a test — so two consumers
/// can no longer disagree about what an absent flag means, and adding a
/// `Capabilities` field without deciding its absent-case behavior fails
/// compilation in [`Capability::enabled_in`]'s exhaustive destructuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Timed,
    ExpectedOutcomes,
    OutputHashes,
    EmbeddedStorePaths,
    ImpureEnv,
    DependencyClosures,
}

impl Capability {
    /// Every capability, in the manifest's field order (also the design
    /// doc's table order).
    pub const ALL: [Capability; 6] = [
        Capability::Timed,
        Capability::ExpectedOutcomes,
        Capability::OutputHashes,
        Capability::EmbeddedStorePaths,
        Capability::ImpureEnv,
        Capability::DependencyClosures,
    ];

    /// The wire name: the `manifest.capabilities` field this variant
    /// mirrors.
    pub const fn flag(self) -> &'static str {
        match self {
            Capability::Timed => "timed",
            Capability::ExpectedOutcomes => "expected_outcomes",
            Capability::OutputHashes => "output_hashes",
            Capability::EmbeddedStorePaths => "embedded_store_paths",
            Capability::ImpureEnv => "impure_env",
            Capability::DependencyClosures => "dependency_closures",
        }
    }

    /// Whether `capabilities` declares this capability. The exhaustive
    /// destructuring couples the enum to the struct at compile time:
    /// adding a `Capabilities` field without a `Capability` variant (or
    /// vice versa) fails here.
    pub fn enabled_in(self, capabilities: &Capabilities) -> bool {
        let Capabilities {
            timed,
            expected_outcomes,
            output_hashes,
            embedded_store_paths,
            impure_env,
            dependency_closures,
        } = *capabilities;
        match self {
            Capability::Timed => timed,
            Capability::ExpectedOutcomes => expected_outcomes,
            Capability::OutputHashes => output_hashes,
            Capability::EmbeddedStorePaths => embedded_store_paths,
            Capability::ImpureEnv => impure_env,
            Capability::DependencyClosures => dependency_closures,
        }
    }

    /// The staged data that must back this flag when it is set, as
    /// `(description, present)` against the given member presence. `None`
    /// for `timed`, which asserts a property of `requests.jsonl` offsets
    /// rather than a member's presence; `output_hashes` is backed by the
    /// outcomes member because per-output hashes live in its records.
    pub fn backing(self, present: &MemberPresence) -> Option<(&'static str, bool)> {
        match self {
            Capability::Timed => None,
            Capability::ExpectedOutcomes => Some((super::OUTCOMES_MEMBER, present.outcomes)),
            Capability::OutputHashes => Some((super::OUTCOMES_MEMBER, present.outcomes)),
            Capability::EmbeddedStorePaths => Some((
                "an embedded non-drv store path",
                present.embedded_store_paths,
            )),
            Capability::ImpureEnv => Some((super::IMPURE_ENV_MEMBER, present.impure_env)),
            Capability::DependencyClosures => Some((super::CLOSURES_MEMBER, present.closures)),
        }
    }

    /// What the flag asserts about the archive (the design-doc table's
    /// "Meaning" column).
    pub const fn meaning(self) -> &'static str {
        match self {
            Capability::Timed => {
                "`requests.jsonl` offsets are meaningful recorded times (and `outcomes.jsonl` \
                 may carry `stop_offset_s`)."
            }
            Capability::ExpectedOutcomes => {
                "`outcomes.jsonl` is present and is authoritative truth for the workload."
            }
            Capability::OutputHashes => {
                "`built` expected outcomes carry per-output NAR hashes. The flag asserts that \
                 every `built` outcome the recorder could hash carries `outputs`; readers treat \
                 per-record absence as not-comparable, never as a format error."
            }
            Capability::EmbeddedStorePaths => {
                "`nix/store/` contains embedded non-drv store paths, each with a narinfo sidecar."
            }
            Capability::ImpureEnv => "`impure-env.json` is present.",
            Capability::DependencyClosures => {
                "`closures.jsonl` is present and covers the full union closure of the workload."
            }
        }
    }

    /// What the engine gates on the flag, including what an absent flag
    /// means (the design-doc table's "What it gates" column).
    pub const fn gates(self) -> &'static str {
        match self {
            Capability::Timed => {
                "The timed scheduling mode, cancellation/disconnect reproduction, \
                 dispatch-lateness accounting (§9 Scheduling). Timeless archives can only run \
                 in drain mode."
            }
            Capability::ExpectedOutcomes => {
                "Verdict comparison (§7 Comparison model). Without it every unit ends in a \
                 no-truth verdict; the campaign is a load/exercise run."
            }
            Capability::OutputHashes => "Output-divergence verdicts (§7 Comparison model).",
            Capability::EmbeddedStorePaths => {
                "The archive rung of the supply ladder (§8 Supply planning)."
            }
            Capability::ImpureEnv => "Impure demotion (§7, §8).",
            Capability::DependencyClosures => {
                "Plan-time closure computation (batching, overlap analysis, supply planning) \
                 without parsing every embedded ATerm (§6 The replayer). When false the engine \
                 falls back to walking the embedded `.drv` files."
            }
        }
    }
}

/// Render the archive capability table exactly as the design doc's
/// "Capabilities" section publishes it. The doc's table is a paste of this
/// output and a test pins the two together, so the published flag
/// semantics are derivable from — and cannot drift from — the enum's data.
pub fn capability_table_markdown() -> String {
    let mut table = String::from("| Flag | Meaning | What it gates |\n|---|---|---|\n");
    for capability in Capability::ALL {
        table.push_str(&format!(
            "| `{}` | {} | {} |\n",
            capability.flag(),
            capability.meaning(),
            capability.gates(),
        ));
    }
    table
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
    /// SHA-256 of the uncompressed NAR. The wire field is `nar_hash_hex` and
    /// writers emit lowercase hex per the format specification; loading goes
    /// through [`crate::narhash::NarHash`], so any spelling a recorder
    /// legitimately produces (hex, `sha256:` nixbase32/hex, SRI) decodes to
    /// the same digest, and a value no spelling explains is a loud parse
    /// error at archive open instead of a silently incomparable record.
    #[serde(rename = "nar_hash_hex")]
    pub nar_hash: crate::narhash::NarHash,
    pub nar_size: u64,
}

/// The session half of the `(session, drv)` key that scopes archive truth:
/// the one resolution of recorded session identity onto engine lookups.
///
/// # Design note — one identity, two consumers
///
/// Archive records are keyed per recorded session ([`OutcomeRecord::session`]),
/// while the engine reads them from two places with different granularity:
///
/// - **Campaign truth** (`run/truth.rs::expected_outcomes_for_units`)
///   resolves one truth slot per workload *unit*. The timeless engine has
///   no per-request identity there, so it does not probe with a session at
///   all — it goes through the reader's canonical collapse-over-sessions
///   helper (`ReplayArchive::expected_outcome_across_sessions`).
/// - **Timed scheduling** (`run/mod.rs` wiring, consumed by
///   `run/timeline.rs`) resolves per-request timing truth — and any
///   per-target job keying layered on top — at the wiring point, where the
///   recorded [`RequestRecord`] is in hand: mint the key with
///   [`SessionKey::of_request`] there and carry the resolution (or
///   [`SessionKey::recorded`]'s echo of the grouping id) downstream; never
///   re-derive it from a bare integer later.
///
/// There are exactly two ways to obtain a key — from a recorded request,
/// or the explicit session-less identity ([`SessionKey::SESSIONLESS`]).
/// There is deliberately no integer constructor: a hard-coded probe
/// session (the bug shape this type retires) is unrepresentable, and a
/// consumer that thinks it needs one actually needs either the request it
/// is acting for or the collapse helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey(Option<i64>);

impl SessionKey {
    /// The explicit session-less identity: resolves only truth recorded
    /// without a session scope (the form session-less recorders write).
    pub const SESSIONLESS: SessionKey = SessionKey(None);

    /// The session a recorded request was captured under.
    pub fn of_request(record: &RequestRecord) -> Self {
        SessionKey(Some(record.session))
    }

    /// The recorded session id this key resolves to (`None` for
    /// [`SessionKey::SESSIONLESS`]). Read-only by design: the id can be
    /// displayed, logged, or used as a grouping echo, but never turned
    /// back into a `SessionKey`.
    pub fn recorded(self) -> Option<i64> {
        self.0
    }
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

    /// [`Capability::flag`] and [`Capability::enabled_in`] agree with the
    /// serde wire form bijectively: setting exactly the JSON field named by
    /// a variant's flag enables exactly that variant.
    #[test]
    fn capability_enum_matches_the_wire_flags() {
        for capability in Capability::ALL {
            let mut value = serde_json::to_value(Capabilities::default()).unwrap();
            value[capability.flag()] = serde_json::Value::Bool(true);
            let parsed: Capabilities = serde_json::from_value(value).unwrap();
            for other in Capability::ALL {
                assert_eq!(
                    other.enabled_in(&parsed),
                    other == capability,
                    "setting `{}` must enable {:?} and nothing else",
                    capability.flag(),
                    capability,
                );
            }
        }
    }

    /// The design doc's capability table is a paste of
    /// [`capability_table_markdown`]: the rendered block must appear in the
    /// doc verbatim, so flag semantics cannot drift between code and doc.
    ///
    /// The doc lives at the workspace root, outside this crate's directory
    /// — and the sandboxed CI test build stages each crate's own directory
    /// only — so the verbatim pin runs where the doc is visible (dev-shell
    /// `cargo nextest`/`cargo test` runs, which the repo's commit loop
    /// requires) and reduces to the structural floor elsewhere.
    #[test]
    fn capability_table_pins_the_design_doc() {
        let table = capability_table_markdown();
        for capability in Capability::ALL {
            assert!(
                table.contains(&format!("| `{}` | ", capability.flag())),
                "renderer must emit one row per capability:\n{table}"
            );
        }

        let doc = crate::test_manifest_dir().join("../docs/dev/2026-05-28-build-replay-design.md");
        let Ok(text) = std::fs::read_to_string(&doc) else {
            eprintln!(
                "design doc not present in this build's source tree; verbatim table pin not \
                 checked here (dev-shell test runs check it)"
            );
            return;
        };
        assert!(
            text.contains(&table),
            "the design doc's capability table drifted from Capability's data — paste the \
             rendered table over the doc's:\n{table}"
        );
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let captured_at = jiff::Timestamp::from_second(0).unwrap();
        let mut provenance = serde_json::Map::new();
        provenance.insert("recorder".to_string(), json!("rio-replay-eval"));
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

//! v0 archives — the contract nxb-replay writes today (manifest without
//! `format_version`, `builds.jsonl` with native status codes,
//! `ssh_session_id`/`paths` request shape). The engine accepts them
//! indefinitely via an upgrade-on-open shim that maps them into the v1
//! in-memory model; v0 archives have no content-addressed identity and
//! cannot be published to the v1 S3 layout.
//!
//! The mapping rules restate the v0-compatibility table in
//! `docs/dev/2026-05-28-build-replay-design.md` ("v0 compatibility").

use std::collections::BTreeMap;

use super::FORMAT_VERSION;
use super::schema::{
    Capabilities, ContentDigests, Counts, ExpectedOutcome, Manifest, OutcomeRecord, OutputHash,
    RequestRecord, RequestTarget, Substituters,
};

/// v0 manifest.json (no `format_version` field).
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct V0Manifest {
    pub from: jiff::Timestamp,
    pub to: jiff::Timestamp,
    pub created_at: jiff::Timestamp,
    #[serde(default)]
    pub src_substituters: Vec<String>,
    #[serde(default)]
    pub target_substituters: Vec<String>,
    #[serde(default)]
    pub fat: bool,
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub drvs: u64,
    #[serde(default)]
    pub embedded_srcs: u64,
}

/// One v0 requests.jsonl record.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct V0Request {
    pub ssh_session_id: i64,
    #[serde(default)]
    pub offset_s: f64,
    /// `[drv_path, [outputs]]` pairs; `["*"]` and `[]` both mean all outputs.
    pub paths: Vec<(String, Vec<String>)>,
}

/// One v0 builds.jsonl record.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct V0BuildRecord {
    pub ssh_session_id: i64,
    pub drv_path: String,
    pub status: i32,
    #[serde(default)]
    pub status_msg: Option<String>,
    #[serde(default)]
    pub duration_s: Option<f64>,
    #[serde(default)]
    pub stop_offset_s: Option<f64>,
    #[serde(default)]
    pub outputs: std::collections::BTreeMap<String, OutputHash>,
}

/// v0 builds.jsonl member name (v1 uses outcomes.jsonl).
pub(crate) const V0_BUILDS_MEMBER: &str = "builds.jsonl";

/// Recorded v0 status codes with a dedicated neutral mapping.
pub(crate) mod v0_status {
    pub const BUILT: i32 = 0;
    pub const CANCELLED: i32 = 6;
    pub const BUILDER_ERROR: i32 = 10;
    pub const CLIENT_DISCONNECT: i32 = 13;
    pub const RESOURCE_EXHAUSTED: i32 = 16;
}

/// Map one v0 build record into a v1 outcome record (the neutral
/// vocabulary). The native code is preserved in `detail` for every
/// non-built status; `status_msg` wins when the recorder captured one.
pub(crate) fn map_build_record(record: V0BuildRecord) -> OutcomeRecord {
    let outcome = match record.status {
        v0_status::BUILT => ExpectedOutcome::Built,
        v0_status::CANCELLED => ExpectedOutcome::Cancelled,
        v0_status::BUILDER_ERROR => ExpectedOutcome::Indeterminate,
        v0_status::CLIENT_DISCONNECT => ExpectedOutcome::Disconnected,
        v0_status::RESOURCE_EXHAUSTED => ExpectedOutcome::ResourceExhausted,
        _ => ExpectedOutcome::Failed,
    };
    let detail = match record.status_msg {
        Some(msg) => Some(msg),
        None if record.status != v0_status::BUILT => Some(format!("status={}", record.status)),
        None => None,
    };
    OutcomeRecord {
        session: Some(record.ssh_session_id),
        drv: record.drv_path,
        outcome,
        detail,
        duration_s: record.duration_s,
        stop_offset_s: record.stop_offset_s,
        outputs: record.outputs,
    }
}

/// Map a v0 request into the v1 shape: `ssh_session_id` → `session`,
/// `[drv, [outputs]]` pairs → target objects, all-output spellings
/// normalized to `["*"]`.
pub(crate) fn map_request(request: V0Request) -> RequestRecord {
    RequestRecord {
        session: request.ssh_session_id,
        offset_s: request.offset_s,
        targets: request
            .paths
            .into_iter()
            .map(|(drv, outputs)| RequestTarget {
                drv,
                outputs: if outputs.is_empty() {
                    vec!["*".to_string()]
                } else {
                    outputs
                },
            })
            .collect(),
    }
}

/// Map the v0 manifest plus inferred capability/count facts into the v1
/// in-memory manifest. `workload_units`, `output_hashes_present`,
/// `has_builds`, `has_impure_env`, and `has_embedded_paths` are derived by
/// the reader from the archive contents.
pub(crate) fn map_manifest(
    v0: V0Manifest,
    workload_units: u64,
    has_builds: bool,
    output_hashes_present: bool,
    has_impure_env: bool,
    has_embedded_paths: bool,
) -> Manifest {
    Manifest {
        // The in-memory model is always v1; `ReplayArchive::format()` is what
        // reports the on-disk contract as V0.
        format_version: FORMAT_VERSION.to_string(),
        created_at: v0.created_at,
        from: v0.from,
        to: v0.to,
        // v0 archives carry no capability flags; they are inferred from
        // member presence (and are therefore consistent by construction).
        capabilities: Capabilities {
            timed: true,
            expected_outcomes: has_builds,
            output_hashes: output_hashes_present,
            embedded_store_paths: has_embedded_paths,
            impure_env: has_impure_env,
            dependency_closures: false,
        },
        counts: Counts {
            requests: v0.requests,
            workload_units,
            // Placeholder; the reader sets this to the mapped outcome count.
            expected_outcomes: 0,
            embedded_drvs: v0.drvs,
            embedded_store_paths: v0.embedded_srcs,
        },
        substituters: Substituters {
            relay: v0.src_substituters,
            target: v0.target_substituters,
        },
        fat: v0.fat,
        // v0 has no provenance object and no integrity tables; the empty
        // values mean "nothing recorded", never "recorded as empty".
        provenance: serde_json::Map::new(),
        files: BTreeMap::new(),
        content_digests: ContentDigests {
            drvs: String::new(),
            embedded_store_paths: String::new(),
            narinfo: String::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v0 build record with the given status and no recorder message.
    fn record_with_status(status: i32) -> V0BuildRecord {
        V0BuildRecord {
            ssh_session_id: 7,
            drv_path: "/nix/store/a1111111111111111111111111111111-dep.drv".to_string(),
            status,
            status_msg: None,
            duration_s: Some(1.5),
            stop_offset_s: Some(3.0),
            outputs: BTreeMap::new(),
        }
    }

    #[test]
    fn status_code_mapping_covers_the_neutral_vocabulary() {
        let cases = [
            (0, ExpectedOutcome::Built),
            (6, ExpectedOutcome::Cancelled),
            (10, ExpectedOutcome::Indeterminate),
            (13, ExpectedOutcome::Disconnected),
            (16, ExpectedOutcome::ResourceExhausted),
            (1, ExpectedOutcome::Failed),
            (4, ExpectedOutcome::Failed),
            (99, ExpectedOutcome::Failed),
        ];
        for (status, expected) in cases {
            let mapped = map_build_record(record_with_status(status));
            assert_eq!(mapped.outcome, expected, "status {status}");
            assert_eq!(mapped.session, Some(7), "status {status}");
            assert_eq!(mapped.duration_s, Some(1.5), "status {status}");
            assert_eq!(mapped.stop_offset_s, Some(3.0), "status {status}");
        }

        // The native code is preserved in detail for non-built statuses…
        let exhausted = map_build_record(record_with_status(16));
        assert_eq!(exhausted.detail, Some("status=16".into()));
        // …unless the recorder captured a message, which wins…
        let mut with_msg = record_with_status(1);
        with_msg.status_msg = Some("boom".to_string());
        assert_eq!(map_build_record(with_msg).detail, Some("boom".into()));
        // …and a built record without a message carries no detail at all.
        assert!(map_build_record(record_with_status(0)).detail.is_none());
    }

    #[test]
    fn request_mapping_normalizes_output_spellings() {
        let mapped = map_request(V0Request {
            ssh_session_id: 3,
            offset_s: 1.25,
            paths: vec![
                (
                    "/nix/store/a1111111111111111111111111111111-a.drv".to_string(),
                    vec![],
                ),
                (
                    "/nix/store/a2222222222222222222222222222222-b.drv".to_string(),
                    vec!["*".to_string()],
                ),
                (
                    "/nix/store/a3333333333333333333333333333333-c.drv".to_string(),
                    vec!["out".to_string(), "dev".to_string()],
                ),
            ],
        });
        assert_eq!(mapped.session, 3);
        assert_eq!(mapped.offset_s, 1.25);
        let outputs: Vec<&[String]> = mapped
            .targets
            .iter()
            .map(|target| target.outputs.as_slice())
            .collect();
        assert_eq!(
            outputs,
            vec![
                &["*".to_string()][..],
                &["*".to_string()][..],
                &["out".to_string(), "dev".to_string()][..],
            ]
        );
    }

    #[test]
    fn manifest_mapping_infers_capabilities_and_counts() {
        let stamp: jiff::Timestamp = "2026-05-20T12:00:00Z".parse().unwrap();
        let mapped = map_manifest(
            V0Manifest {
                from: stamp,
                to: stamp,
                created_at: stamp,
                src_substituters: vec!["https://cache.example.org".to_string()],
                target_substituters: vec![],
                fat: false,
                requests: 4,
                drvs: 4,
                embedded_srcs: 1,
            },
            4,
            true,
            true,
            true,
            true,
        );

        assert_eq!(
            mapped.capabilities,
            Capabilities {
                timed: true,
                expected_outcomes: true,
                output_hashes: true,
                embedded_store_paths: true,
                impure_env: true,
                dependency_closures: false,
            }
        );
        assert_eq!(
            mapped.counts,
            Counts {
                requests: 4,
                workload_units: 4,
                expected_outcomes: 0,
                embedded_drvs: 4,
                embedded_store_paths: 1,
            }
        );
        assert_eq!(mapped.format_version, FORMAT_VERSION);
        assert!(mapped.provenance.is_empty());
        assert!(mapped.files.is_empty());
        assert!(mapped.content_digests.drvs.is_empty());
        assert!(mapped.content_digests.embedded_store_paths.is_empty());
        assert!(mapped.content_digests.narinfo.is_empty());
        assert_eq!(
            mapped.substituters.relay,
            vec!["https://cache.example.org".to_string()]
        );
        assert!(mapped.substituters.target.is_empty());
        assert!(!mapped.fat);
    }
}

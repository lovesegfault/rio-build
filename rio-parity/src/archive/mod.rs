//! Replay archives — the source-agnostic record/replay package consumed
//! by the campaign engine and produced by recorders.
//!
//! An archive is a tree of member files with a fixed logical layout
//! (see `docs/dev/2026-05-28-build-replay-design.md`, "Archive format
//! v1"). The published at-rest form is a single DwarFS image of that
//! tree; a plain directory with the same member paths is the local
//! working form (recorder staging, fixtures, dev runs). This module
//! family hosts the schema types, the content-addressed identity, the
//! reader (directory + DwarFS backends, with a v0 upgrade-on-open
//! shim), the staging-directory writer, and the write-once S3 layout.

pub mod identity;
pub mod schema;

/// Member paths inside an archive (identical in the directory and image forms).
pub const MANIFEST_MEMBER: &str = "manifest.json";
pub const REQUESTS_MEMBER: &str = "requests.jsonl";
pub const OUTCOMES_MEMBER: &str = "outcomes.jsonl";
pub const UNITS_MEMBER: &str = "units.jsonl";
pub const CLOSURES_MEMBER: &str = "closures.jsonl";
pub const IMPURE_ENV_MEMBER: &str = "impure-env.json";
pub const EXCLUSIONS_MEMBER: &str = "exclusions.jsonl";
/// Directory of narinfo sidecars for embedded non-drv store paths.
pub const NARINFO_DIR: &str = "narinfo";
/// Directory of embedded derivations (`*.drv`) and unpacked store-path trees.
pub const STORE_DIR: &str = "nix/store";

/// The metadata members covered by `manifest.files` (the manifest itself is
/// never listed; bulk content under `nix/store/` and `narinfo/` is covered by
/// `manifest.content_digests` instead).
pub const METADATA_MEMBERS: [&str; 6] = [
    REQUESTS_MEMBER,
    OUTCOMES_MEMBER,
    UNITS_MEMBER,
    CLOSURES_MEMBER,
    IMPURE_ENV_MEMBER,
    EXCLUSIONS_MEMBER,
];

/// `format_version` written by this library.
pub const FORMAT_VERSION: &str = "1.0";
/// The one major version this reader understands. Unknown majors are
/// refused; any minor of a known major is accepted (additive evolution).
pub const SUPPORTED_MAJOR: u64 = 1;

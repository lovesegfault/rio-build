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

pub(crate) mod backend;
pub mod identity;
pub mod reader;
pub mod schema;
pub(crate) mod v0;
pub mod writer;

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

/// The hash part of a store path: the basename characters before the first
/// `-`. Accepts a full `/nix/store/...` path, a basename, or a bare hash.
pub(crate) fn hash_part(path_or_name: &str) -> &str {
    let base = path_or_name.rsplit('/').next().unwrap_or(path_or_name);
    base.split('-').next().unwrap_or(base)
}

/// Parse a `.jsonl` member: one JSON record per line, blank lines skipped,
/// errors name the member and the 1-based line number.
pub(crate) fn parse_jsonl<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    member: &str,
) -> anyhow::Result<Vec<T>> {
    use anyhow::Context as _;
    let text =
        std::str::from_utf8(bytes).with_context(|| format!("{member} is not valid UTF-8"))?;
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: T = serde_json::from_str(line)
            .with_context(|| format!("{member} line {}: malformed record", idx + 1))?;
        out.push(record);
    }
    Ok(out)
}

/// Parse one narinfo sidecar. Sidecars for paths embedded in an archive
/// legitimately lack a `URL:` line — they describe contents of the archive
/// itself, not a fetchable cache object — but `NarInfo::parse` requires the
/// field. Synthesize a placeholder (never dereferenced; embedded bytes come
/// from the archive itself) and retry. Any other failure, or a failure with
/// a `URL:` line present, is returned as-is so the caller can decide.
pub(crate) fn parse_narinfo_sidecar(
    text: &str,
    stem: &str,
) -> anyhow::Result<rio_nix::narinfo::NarInfo> {
    use rio_nix::narinfo::NarInfo;
    match NarInfo::parse(text) {
        Ok(narinfo) => Ok(narinfo),
        Err(err) => {
            let has_url = text
                .lines()
                .filter_map(|line| line.split_once(':'))
                .any(|(key, _)| key.trim() == "URL");
            if has_url {
                Err(err.into())
            } else {
                NarInfo::parse(&format!("{text}\nURL: nar/{stem}.nar\n"))
                    .map_err(anyhow::Error::from)
            }
        }
    }
}

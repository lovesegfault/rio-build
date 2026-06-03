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
pub mod s3;
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

/// AWS S3's hard limit on a single-PUT object upload: 5 GiB (the documented
/// PutObject maximum; larger objects require multipart upload). Archive
/// publication uploads the DwarFS image as exactly one `PutObject` — and
/// the publish flow's lost-conditional self-write attribution RELIES on
/// single-part PUTs (a multipart composite checksum is unattributable, see
/// `s3::ArchiveStore::remote_object_match`) — so an image above this cap
/// deterministically fails at publish time with an opaque backend error.
/// [`ensure_single_put_size`] turns that into a loud refusal at pack and
/// publish time. Lifting the cap means a multipart path with FULL_OBJECT
/// checksums and reworked self-write attribution, not deleting this check.
pub(crate) const S3_SINGLE_PUT_MAX_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Refuse `bytes` above [`S3_SINGLE_PUT_MAX_BYTES`], naming the cap, the
/// single-part constraint, and the remediation. Enforced at BOTH ends of
/// the image lifecycle: when `mkdwarfs` produces an image (so the operator
/// learns at staging time, not after the multi-hour pipeline that follows)
/// and again at publish (covering images that reached publish without
/// passing through this tree's packer).
pub(crate) fn ensure_single_put_size(bytes: u64, what: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        bytes <= S3_SINGLE_PUT_MAX_BYTES,
        "{what} is {bytes} bytes, above the {S3_SINGLE_PUT_MAX_BYTES}-byte (5 GiB) S3 \
         single-PUT cap — archive publication uploads the image as one PutObject (the \
         write-once attribution requires single-part uploads), so this image cannot be \
         published; reduce the archive's embedded scope or split the campaign",
    );
    Ok(())
}

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

/// What to do with one defective record of a per-record archive member —
/// a narinfo sidecar that fails to parse, whose filename names a different
/// store hash than its content describes, or that resolves to a store hash
/// another sidecar file already claimed; two `nix/store/` members
/// colliding on hash part; or a request record with no targets — the axis
/// on which the v0 and v1 paths deliberately differ. Every per-record
/// validity site on the open paths (and the writer's finalize, which
/// shares the sidecar enumeration) takes this policy, so adding a new
/// per-record check forces an explicit v0/v1 decision at compile time.
/// Structural corruption is NOT covered and stays loud on both paths: a
/// malformed JSONL line or member document is handled where it is parsed
/// (see [`reader::ReplayArchive::open_v0`] for why).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordPolicy {
    /// v1 (and the writer's finalize, which enumerates the same way): a
    /// hard error naming the offending record. The v1 writer enforces
    /// per-record validity at stage time (`write_requests` rejects empty
    /// targets; finalize digests the sidecar listing), so a reader-side
    /// hit means tampering or corruption worth refusing. Skipping an
    /// unparseable sidecar could never help a v1 archive anyway — a
    /// skipped sidecar would be missing from the recomputed narinfo
    /// listing digest, so open would still fail, just with a far less
    /// actionable message. A filename↔content identity mismatch, by
    /// contrast, is invisible to that digest (it hashes content store
    /// paths and bytes, independent of filenames), so this identity check
    /// is the only thing standing between a mis-assembled archive and a
    /// wrong-sidecar verification failure at NAR-serialization time,
    /// mid-campaign. Duplicate resolutions (the canonical
    /// `<hash>.narinfo` plus the mirror `<hash>-<name>.narinfo` for one
    /// path) are refused for the same reason: lookups key on the hash, so
    /// one of the two files would be silently shadowed, and the per-file
    /// listing would carry two lines for one store path.
    Strict,
    /// v0: warn and skip the record. v0 archives are digest-less,
    /// irreplaceable recordings of past production windows produced by an
    /// external recorder that never enforced these rules; one defective
    /// record must cost only itself (a bad sidecar makes one path
    /// non-uploadable, an empty request schedules nothing), never the
    /// whole recording. For duplicate resolutions in BOTH backend
    /// enumerations — sidecar files claiming one store hash
    /// (`index_narinfos`) and store members colliding on hash part
    /// (`index_store_entries`) — the first file in name order wins,
    /// deterministically: backends promise no entry order, so each
    /// enumeration sorts before its first-wins fold.
    WarnAndSkip,
}

/// The indexed `narinfo/` directory of an archive (or a writer staging
/// tree): every consumer's view of the sidecar set, derived once.
#[derive(Debug, Default)]
pub(crate) struct SidecarIndex {
    /// Store-path hash part → parsed sidecar (the lookup map). The
    /// identity cross-check guarantees the key equals the hash part of
    /// the sidecar's content `StorePath:`.
    pub(crate) by_hash: std::collections::HashMap<String, rio_nix::narinfo::NarInfo>,
    /// `(content store path, sidecar-bytes sha256)`, one entry per sidecar
    /// FILE in filename order — the exact listing `identity::listing_digest`
    /// consumes. Under [`RecordPolicy::Strict`] duplicate resolutions are
    /// refused, so this is also one entry per store path: the per-file and
    /// per-store-path views coincide by construction.
    pub(crate) digests: Vec<(String, String)>,
}

/// Index an archive's `narinfo/` sidecar directory — THE one enumeration
/// of the v1 sidecar contract, shared by `ReplayArchive::open` (which
/// recomputes the narinfo listing digest from it) and
/// `ArchiveWriter::finalize` (which validates a staging tree with it
/// before computing that digest). Both sides deriving their view from
/// this single function is what keeps the contract's two ends agreeing:
/// finalize structurally cannot bless a `narinfo/` tree that open would
/// refuse, because per-file identity, duplicate resolution, and listing
/// cardinality are decided here, once.
///
/// Per file: sidecars without a `URL:` line get one synthesized (see
/// [`parse_narinfo_sidecar`]); sidecars that fail to parse, whose filename
/// disagrees with their content's `StorePath:` about which store hash they
/// describe, or whose store hash another sidecar file already claimed are
/// handled per `policy`. Files are visited in name order so the listing —
/// and the v0 first-file-wins choice — is deterministic regardless of
/// backend iteration order.
pub(crate) fn index_narinfos(
    backend: &backend::Backend,
    policy: RecordPolicy,
) -> anyhow::Result<SidecarIndex> {
    use anyhow::anyhow;

    use crate::archive::backend::EntryKind;

    let mut index = SidecarIndex::default();
    // Hash part → the sidecar file (rel path) that claimed it, for the
    // duplicate-resolution check.
    let mut claimed_by: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut entries = backend.list_dir(NARINFO_DIR)?.unwrap_or_default();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in entries {
        if entry.kind != EntryKind::Regular {
            continue;
        }
        let Some(stem) = entry.name.strip_suffix(".narinfo") else {
            continue;
        };
        let rel = format!("{NARINFO_DIR}/{}", entry.name);
        let bytes = backend
            .read_file(&rel)?
            .ok_or_else(|| anyhow!("{rel}: listed but unreadable"))?;
        let parsed = std::str::from_utf8(&bytes)
            .map_err(anyhow::Error::from)
            .and_then(|text| parse_narinfo_sidecar(text, stem));
        let narinfo = match (parsed, policy) {
            (Ok(narinfo), _) => narinfo,
            (Err(err), RecordPolicy::Strict) => {
                return Err(err.context(format!("unparseable narinfo sidecar {rel}")));
            }
            (Err(err), RecordPolicy::WarnAndSkip) => {
                tracing::warn!("skipping unparseable narinfo sidecar {rel}: {err:#}");
                continue;
            }
        };
        // The sidecar carries its identity twice: the filename stem (the
        // locator — it keys the lookup map, and through it the v1
        // sidecar-completeness check and the supply path) and the
        // content's `StorePath:` (what every consumer of the parsed
        // sidecar trusts). Conformant producers write them in agreement
        // (`ArchiveWriter::embed_store_path` checks the text against the
        // embedded path), and the listing digest cannot catch a
        // disagreement — it hashes content identities and bytes, never
        // filenames, so e.g. swapping two sidecar files recomputes the
        // identical digest. Cross-check the two here so a mis-assembled
        // or edited tree is refused (or the sidecar dropped, per policy)
        // at index time, instead of supplying paths described by the
        // wrong sidecar and failing NAR verification mid-campaign.
        let stem_hash = hash_part(stem);
        let content_hash = hash_part(&narinfo.store_path);
        if stem_hash != content_hash {
            let mismatch = anyhow!(
                "narinfo sidecar {rel} is named for store hash {stem_hash} but its StorePath \
                 describes {} — sidecar lookups key on the filename, so this sidecar would be \
                 served for the wrong store path",
                narinfo.store_path
            );
            match policy {
                RecordPolicy::Strict => return Err(mismatch),
                RecordPolicy::WarnAndSkip => {
                    tracing::warn!("skipping mismatched narinfo sidecar: {mismatch:#}");
                    continue;
                }
            }
        }
        // One store path, one sidecar file. Both supported spellings
        // (`<hash>.narinfo` and `<hash>-<name>.narinfo`) resolve to the
        // same hash, so two files for one path would shadow each other in
        // the lookup map while still contributing two listing lines.
        if let Some(first) = claimed_by.get(stem_hash) {
            let duplicate = anyhow!(
                "narinfo sidecars {first} and {rel} both describe store hash {stem_hash}; \
                 each embedded store path must have exactly one sidecar"
            );
            match policy {
                RecordPolicy::Strict => return Err(duplicate),
                RecordPolicy::WarnAndSkip => {
                    tracing::warn!("skipping duplicate narinfo sidecar: {duplicate:#}");
                    continue;
                }
            }
        }
        claimed_by.insert(stem_hash.to_string(), rel);
        index
            .digests
            .push((narinfo.store_path.clone(), identity::sha256_hex(&bytes)));
        index.by_hash.insert(stem_hash.to_string(), narinfo);
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The single-PUT precondition admits exactly the documented AWS cap
    /// and refuses one byte more (limit / limit+1), naming the cap and the
    /// single-part constraint. The expectation derives from AWS S3's
    /// PutObject contract — 5 GiB maximum for a single-PUT upload — which
    /// the publish flow is structurally tied to (single-part PUTs are what
    /// make the write-once self-write attribution's checksums comparable).
    #[test]
    fn single_put_size_boundary() {
        assert_eq!(
            S3_SINGLE_PUT_MAX_BYTES,
            5 * 1024 * 1024 * 1024,
            "the cap is AWS S3's documented 5 GiB single-PUT maximum"
        );
        ensure_single_put_size(S3_SINGLE_PUT_MAX_BYTES, "image at the cap")
            .expect("an image at exactly the cap is publishable");
        let err = format!(
            "{:#}",
            ensure_single_put_size(S3_SINGLE_PUT_MAX_BYTES + 1, "oversized image").unwrap_err()
        );
        assert!(
            err.contains("5 GiB") && err.contains("single-PUT") && err.contains("PutObject"),
            "the refusal names the cap and the single-part constraint: {err}"
        );
    }
}

//! Staging-directory writer for v1 replay archives.
//!
//! [`ArchiveWriter`] assembles the directory form of an archive: a recorder
//! stages members one call at a time, then [`ArchiveWriter::finalize`] runs
//! the completeness rules, computes the integrity tables, and writes
//! `manifest.json` (see `docs/dev/2026-05-28-build-replay-design.md`,
//! "Archive format v1"). The directory form is a local working
//! representation only — recorder staging, fixtures, dev runs — and is never
//! the published S3 form; packing the staged tree into the published DwarFS
//! image is a separate step.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Serialize;
use sha2::Digest as _;

use super::identity;
use super::schema::{
    Capabilities, ClosureRecord, ContentDigests, Counts, ExclusionRecord, ImpureEnv, Manifest,
    MemberDigest, MemberPresence, OutcomeRecord, RequestRecord, Substituters, UnitRecord,
};

/// Staging-directory writer for v1 replay archives.
#[derive(Debug)]
pub struct ArchiveWriter {
    root: PathBuf,
}

/// Recorder-chosen manifest fields that `finalize` cannot derive from the
/// staged members.
#[derive(Debug, Clone)]
pub struct ManifestSeed {
    pub created_at: jiff::Timestamp,
    pub from: jiff::Timestamp,
    pub to: jiff::Timestamp,
    pub capabilities: Capabilities,
    pub substituters: Substituters,
    pub fat: bool,
    pub provenance: serde_json::Map<String, serde_json::Value>,
}

/// Result of `finalize`: the manifest as written plus the derived identity.
#[derive(Debug, Clone)]
pub struct FinalizedArchive {
    pub manifest: Manifest,
    /// Full 64-hex archive id (SHA-256 of the manifest.json bytes).
    pub archive_id: String,
    /// Path of the written manifest.json.
    pub manifest_path: PathBuf,
}

impl ArchiveWriter {
    /// Create the staging-directory layout under `root` (the root itself
    /// plus the `narinfo/` and `nix/store/` subdirectories).
    ///
    /// Refuses a root that already holds a finalized archive: restaging over
    /// it would silently invalidate the manifest's digests and identity.
    pub fn create(root: &Path) -> anyhow::Result<Self> {
        if root.join(super::MANIFEST_MEMBER).exists() {
            anyhow::bail!(
                "refusing to stage into {}: it already contains {} (a finalized archive)",
                root.display(),
                super::MANIFEST_MEMBER
            );
        }
        for dir in [super::NARINFO_DIR, super::STORE_DIR] {
            let dir = root.join(dir);
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// The staging root this writer writes into.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stage `requests.jsonl`. Every archive needs at least one request and
    /// every request must name at least one target; targets with empty
    /// `outputs` are normalized to `["*"]` (both spellings mean "all
    /// outputs"). A repeat call replaces the previously staged member.
    pub fn write_requests(&self, records: &[RequestRecord]) -> anyhow::Result<()> {
        anyhow::ensure!(
            !records.is_empty(),
            "{} needs at least one request record with non-empty targets",
            super::REQUESTS_MEMBER
        );
        // Validate everything before touching the member file, so a rejected
        // input cannot leave a partially staged requests.jsonl behind.
        for (idx, record) in records.iter().enumerate() {
            anyhow::ensure!(
                !record.targets.is_empty(),
                "request record {idx} (session {}) must have non-empty targets",
                record.session
            );
        }
        let path = self.root.join(super::REQUESTS_MEMBER);
        let file =
            std::fs::File::create(&path).with_context(|| format!("create {}", path.display()))?;
        let mut out = std::io::BufWriter::new(file);
        for (idx, record) in records.iter().enumerate() {
            // Normalize empty `outputs` to ["*"], cloning only the records
            // that actually need rewriting.
            let written = if record
                .targets
                .iter()
                .any(|target| target.outputs.is_empty())
            {
                let mut normalized = record.clone();
                for target in &mut normalized.targets {
                    if target.outputs.is_empty() {
                        target.outputs = vec!["*".to_string()];
                    }
                }
                write_jsonl_line(&mut out, &normalized)
            } else {
                write_jsonl_line(&mut out, record)
            };
            written.with_context(|| format!("write request record {idx} to {}", path.display()))?;
        }
        out.flush()
            .with_context(|| format!("flush {}", path.display()))
    }

    /// Stage `outcomes.jsonl` (expected outcomes — the recorded truth). A
    /// repeat call replaces the previously staged member.
    pub fn write_outcomes(&self, records: &[OutcomeRecord]) -> anyhow::Result<()> {
        write_jsonl(&self.root.join(super::OUTCOMES_MEMBER), records)
    }

    /// Stage `units.jsonl` (per-unit display and filter metadata). A repeat
    /// call replaces the previously staged member.
    pub fn write_units(&self, records: &[UnitRecord]) -> anyhow::Result<()> {
        write_jsonl(&self.root.join(super::UNITS_MEMBER), records)
    }

    /// Stage `closures.jsonl` (direct dependency adjacency). A repeat call
    /// replaces the previously staged member.
    pub fn write_closures(&self, records: &[ClosureRecord]) -> anyhow::Result<()> {
        write_jsonl(&self.root.join(super::CLOSURES_MEMBER), records)
    }

    /// Stage `impure-env.json` (derivation → impure environment variable
    /// names). A repeat call replaces the previously staged member.
    pub fn write_impure_env(&self, map: &ImpureEnv) -> anyhow::Result<()> {
        write_json_pretty(&self.root.join(super::IMPURE_ENV_MEMBER), map)
    }

    /// Stage `exclusions.jsonl`. Each record must carry at least one of
    /// `label`/`drv`, otherwise completeness accounting could not name the
    /// excluded item. A repeat call replaces the previously staged member.
    pub fn write_exclusions(&self, records: &[ExclusionRecord]) -> anyhow::Result<()> {
        for record in records {
            anyhow::ensure!(
                record.label.is_some() || record.drv.is_some(),
                "exclusion record (reason {:?}) must carry a label or drv",
                record.reason
            );
        }
        write_jsonl(&self.root.join(super::EXCLUSIONS_MEMBER), records)
    }

    /// Stage one derivation member: `nix/store/<basename>` holding the ATerm
    /// text.
    pub fn add_drv(&self, store_path: &str, aterm: &str) -> anyhow::Result<()> {
        let parsed = rio_nix::store_path::StorePath::parse(store_path)
            .with_context(|| format!("invalid store path {store_path}"))?;
        anyhow::ensure!(
            parsed.is_derivation(),
            "add_drv expects a .drv store path, got {store_path}"
        );
        let dest = self.root.join(super::STORE_DIR).join(parsed.basename());
        std::fs::write(&dest, aterm).with_context(|| format!("write {}", dest.display()))
    }

    /// Stage one embedded (non-derivation) store path: copy the tree at
    /// `source` to `nix/store/<basename>` and write its narinfo sidecar to
    /// `narinfo/<hash-part>.narinfo`. The sidecar text must describe
    /// `store_path` (its `StorePath:` field is checked here, so a mix-up
    /// fails at embed time rather than as a missing-sidecar error at
    /// finalize).
    pub fn embed_store_path(
        &self,
        store_path: &str,
        source: &Path,
        narinfo_text: &str,
    ) -> anyhow::Result<()> {
        let parsed = rio_nix::store_path::StorePath::parse(store_path)
            .with_context(|| format!("invalid store path {store_path}"))?;
        anyhow::ensure!(
            !parsed.is_derivation(),
            "embed_store_path expects a non-drv store path, got {store_path} \
             (derivations go through add_drv)"
        );
        let hash_part = super::hash_part(store_path);
        let narinfo = super::parse_narinfo_sidecar(narinfo_text, hash_part)
            .with_context(|| format!("narinfo sidecar for {store_path}"))?;
        anyhow::ensure!(
            narinfo.store_path == store_path,
            "narinfo sidecar StorePath {} does not match the embedded path {store_path}",
            narinfo.store_path
        );
        let dest = self.root.join(super::STORE_DIR).join(parsed.basename());
        copy_tree(source, &dest)
            .with_context(|| format!("embed {store_path} from {}", source.display()))?;
        let sidecar = self
            .root
            .join(super::NARINFO_DIR)
            .join(format!("{hash_part}.narinfo"));
        std::fs::write(&sidecar, narinfo_text)
            .with_context(|| format!("write {}", sidecar.display()))
    }

    /// Run the completeness rules over the staged members, compute the
    /// integrity tables, and write `manifest.json`.
    ///
    /// Completeness rules (each violation is an error):
    /// - `requests.jsonl` must be staged;
    /// - every capability flag must be backed by its member, and every
    ///   staged optional member must be claimed by its capability flag;
    /// - relay substituters must be `https://` or `s3://` URLs;
    /// - the full requisite `.drv` closure of every workload unit must be
    ///   staged under `nix/store/`;
    /// - every embedded non-drv store path must have a narinfo sidecar that
    ///   agrees with the staged tree's NAR serialization.
    ///
    /// Consumes the writer: once a root holds `manifest.json` nothing more
    /// may be staged into it (and [`ArchiveWriter::create`] refuses to
    /// reopen it).
    pub fn finalize(self, seed: ManifestSeed) -> anyhow::Result<FinalizedArchive> {
        // requests.jsonl is the one member every archive must carry.
        let requests_path = self.root.join(super::REQUESTS_MEMBER);
        anyhow::ensure!(
            requests_path.is_file(),
            "{} is required but is not staged (call write_requests before finalize)",
            super::REQUESTS_MEMBER
        );
        let requests_bytes = std::fs::read(&requests_path)
            .with_context(|| format!("read {}", requests_path.display()))?;
        let requests: Vec<RequestRecord> =
            super::parse_jsonl(&requests_bytes, super::REQUESTS_MEMBER)?;

        // Enumerate what is actually staged: metadata members, derivation
        // members, embedded store-path trees, and narinfo sidecars.
        let staged_members: Vec<&str> = super::METADATA_MEMBERS
            .into_iter()
            .filter(|member| self.root.join(member).is_file())
            .collect();
        let store_dir = self.root.join(super::STORE_DIR);
        let mut drv_members: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut embedded_trees: BTreeMap<String, PathBuf> = BTreeMap::new();
        for entry in std::fs::read_dir(&store_dir)
            .with_context(|| format!("list {}", store_dir.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                anyhow::bail!("non-UTF-8 entry under {}: {name:?}", store_dir.display());
            };
            let store_path = format!("{}{name}", rio_nix::store_path::STORE_PREFIX);
            if name.ends_with(".drv") {
                drv_members.insert(store_path, entry.path());
            } else {
                embedded_trees.insert(store_path, entry.path());
            }
        }
        let narinfo_dir = self.root.join(super::NARINFO_DIR);
        let mut sidecar_paths: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&narinfo_dir)
            .with_context(|| format!("list {}", narinfo_dir.display()))?
        {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("narinfo") {
                sidecar_paths.push(path);
            }
        }
        sidecar_paths.sort();

        // Capability flags and staged members must agree in both directions:
        // a set flag needs its backing member, and a staged optional member
        // needs its flag (a recorder that stages data it does not claim is a
        // recorder bug, not an archive variant).
        let presence = MemberPresence {
            outcomes: staged_members.contains(&super::OUTCOMES_MEMBER),
            units: staged_members.contains(&super::UNITS_MEMBER),
            closures: staged_members.contains(&super::CLOSURES_MEMBER),
            impure_env: staged_members.contains(&super::IMPURE_ENV_MEMBER),
            exclusions: staged_members.contains(&super::EXCLUSIONS_MEMBER),
            embedded_store_paths: !embedded_trees.is_empty(),
        };
        seed.capabilities.require_backing_members(&presence)?;
        let mut unclaimed: Vec<String> = Vec::new();
        for (member_present, flag_set, staged_what, flag) in [
            (
                presence.outcomes,
                seed.capabilities.expected_outcomes,
                super::OUTCOMES_MEMBER,
                "expected_outcomes",
            ),
            (
                presence.impure_env,
                seed.capabilities.impure_env,
                super::IMPURE_ENV_MEMBER,
                "impure_env",
            ),
            (
                presence.closures,
                seed.capabilities.dependency_closures,
                super::CLOSURES_MEMBER,
                "dependency_closures",
            ),
            (
                presence.embedded_store_paths,
                seed.capabilities.embedded_store_paths,
                "embedded non-drv store paths",
                "embedded_store_paths",
            ),
        ] {
            if member_present && !flag_set {
                unclaimed.push(format!(
                    "{staged_what} staged but capability `{flag}` is not set"
                ));
            }
        }
        anyhow::ensure!(unclaimed.is_empty(), "{}", unclaimed.join("; "));

        // Relay substituters are campaign-time fetch sources; refuse plain
        // http:// (the engine never relays over cleartext).
        for relay in &seed.substituters.relay {
            anyhow::ensure!(
                relay.starts_with("https://") || relay.starts_with("s3://"),
                "relay substituter {relay:?}: only https:// and s3:// are allowed"
            );
        }

        // The full requisite .drv closure of every workload unit must be
        // staged: walk input_drvs recursively from every requested target.
        let workload: BTreeSet<&str> = requests
            .iter()
            .flat_map(|record| record.targets.iter().map(|target| target.drv.as_str()))
            .collect();
        // Each pending entry carries the staged derivation that required it
        // (None for the workload targets themselves) so a completeness error
        // can name the dependent alongside the missing path.
        let mut pending: Vec<(String, Option<String>)> = workload
            .iter()
            .map(|drv| ((*drv).to_string(), None))
            .collect();
        let mut visited: BTreeSet<String> = BTreeSet::new();
        while let Some((drv_path, required_by)) = pending.pop() {
            if !visited.insert(drv_path.clone()) {
                continue;
            }
            let Some(member_path) = drv_members.get(&drv_path) else {
                let requirer = match &required_by {
                    Some(parent) => format!("required by {parent}"),
                    None => "requested by the workload".to_string(),
                };
                anyhow::bail!(
                    "derivation {drv_path} is {requirer} but missing from {}",
                    super::STORE_DIR
                );
            };
            let aterm = std::fs::read_to_string(member_path)
                .with_context(|| format!("read {}", member_path.display()))?;
            let derivation = rio_nix::derivation::Derivation::parse(&aterm)
                .with_context(|| format!("parse {drv_path}"))?;
            for input in derivation.input_drvs().keys() {
                if !visited.contains(input) {
                    pending.push((input.clone(), Some(drv_path.clone())));
                }
            }
        }

        // Every embedded non-drv path needs a sidecar, and the sidecar must
        // agree with the tree actually staged (NarHash and NarSize over the
        // uncompressed NAR serialization).
        let mut sidecar_records: BTreeMap<String, (rio_nix::narinfo::NarInfo, PathBuf)> =
            BTreeMap::new();
        for sidecar_path in sidecar_paths {
            let text = std::fs::read_to_string(&sidecar_path)
                .with_context(|| format!("read {}", sidecar_path.display()))?;
            let stem = sidecar_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            let narinfo = super::parse_narinfo_sidecar(&text, stem)
                .with_context(|| format!("parse narinfo sidecar {}", sidecar_path.display()))?;
            sidecar_records.insert(narinfo.store_path.clone(), (narinfo, sidecar_path));
        }
        let mut embedded_nar_digests: Vec<(String, String)> = Vec::new();
        for (store_path, tree) in &embedded_trees {
            let Some((narinfo, _)) = sidecar_records.get(store_path) else {
                anyhow::bail!("embedded store path {store_path} has no narinfo sidecar");
            };
            let mut hasher = Sha256Writer::default();
            let nar_size = rio_nix::nar::dump_path_streaming(tree, &mut hasher)
                .with_context(|| format!("NAR-serialize {}", tree.display()))?;
            let nar_sha256 = hasher.finish();
            let sidecar_hex = crate::nixcache::narhash_to_hex(&narinfo.nar_hash)
                .with_context(|| format!("narinfo sidecar for {store_path}"))?;
            anyhow::ensure!(
                nar_sha256 == sidecar_hex && nar_size == narinfo.nar_size,
                "narinfo sidecar for {store_path} disagrees with the embedded tree \
                 (sidecar NarHash {sidecar_hex} NarSize {}, staged tree NAR sha256 {nar_sha256} \
                 size {nar_size})",
                narinfo.nar_size
            );
            embedded_nar_digests.push((store_path.clone(), nar_sha256));
        }

        // Informational counts (operators size campaigns from these without
        // scanning members).
        let expected_outcomes = if presence.outcomes {
            let outcomes_path = self.root.join(super::OUTCOMES_MEMBER);
            let bytes = std::fs::read(&outcomes_path)
                .with_context(|| format!("read {}", outcomes_path.display()))?;
            super::parse_jsonl::<OutcomeRecord>(&bytes, super::OUTCOMES_MEMBER)?.len() as u64
        } else {
            0
        };
        let counts = Counts {
            requests: requests.len() as u64,
            workload_units: workload.len() as u64,
            expected_outcomes,
            embedded_drvs: drv_members.len() as u64,
            embedded_store_paths: embedded_trees.len() as u64,
        };

        // Per-member digests over the staged metadata members, streamed
        // through the hasher rather than read into memory; the manifest
        // itself is never listed.
        let mut files: BTreeMap<String, MemberDigest> = BTreeMap::new();
        for member in &staged_members {
            let path = self.root.join(member);
            let mut file =
                std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
            let mut hasher = Sha256Writer::default();
            let size = std::io::copy(&mut file, &mut hasher)
                .with_context(|| format!("read {}", path.display()))?;
            files.insert(
                (*member).to_string(),
                MemberDigest {
                    sha256: hasher.finish(),
                    size,
                },
            );
        }

        // Aggregate digests over the bulk content and the narinfo sidecars.
        let mut drv_digests: Vec<(String, String)> = Vec::new();
        for (store_path, member_path) in &drv_members {
            let bytes = std::fs::read(member_path)
                .with_context(|| format!("read {}", member_path.display()))?;
            drv_digests.push((store_path.clone(), identity::sha256_hex(&bytes)));
        }
        let mut narinfo_digests: Vec<(String, String)> = Vec::new();
        for (store_path, (_, sidecar_path)) in &sidecar_records {
            let bytes = std::fs::read(sidecar_path)
                .with_context(|| format!("read {}", sidecar_path.display()))?;
            narinfo_digests.push((store_path.clone(), identity::sha256_hex(&bytes)));
        }
        let content_digests = ContentDigests {
            drvs: identity::listing_digest(&drv_digests),
            embedded_store_paths: identity::listing_digest(&embedded_nar_digests),
            narinfo: identity::listing_digest(&narinfo_digests),
        };

        // Assemble and write the manifest. The archive id is computed over
        // exactly the bytes written to disk (trailing newline included), so
        // a reader recomputing it from the stored member gets the same id.
        let manifest = Manifest {
            format_version: super::FORMAT_VERSION.to_string(),
            created_at: seed.created_at,
            from: seed.from,
            to: seed.to,
            capabilities: seed.capabilities,
            counts,
            substituters: seed.substituters,
            fat: seed.fat,
            provenance: seed.provenance,
            files,
            content_digests,
        };
        let mut manifest_bytes =
            serde_json::to_vec_pretty(&manifest).context("serialize manifest.json")?;
        manifest_bytes.push(b'\n');
        // Write through a temp name and rename: manifest.json's presence is
        // what marks a root as finalized (and what `create` refuses), so a
        // half-written manifest must never be observable.
        let manifest_path = self.root.join(super::MANIFEST_MEMBER);
        let tmp_path = self.root.join(format!("{}.tmp", super::MANIFEST_MEMBER));
        std::fs::write(&tmp_path, &manifest_bytes)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &manifest_path).with_context(|| {
            format!(
                "rename {} to {}",
                tmp_path.display(),
                manifest_path.display()
            )
        })?;
        Ok(FinalizedArchive {
            archive_id: identity::archive_id_from_manifest_bytes(&manifest_bytes),
            manifest,
            manifest_path,
        })
    }
}

/// Pack a staged archive directory into a single DwarFS image with the
/// external `mkdwarfs` tool (the published, at-rest form of an archive).
/// The image's contents — not its filename or compression parameters —
/// carry the archive identity, so no determinism flags are required.
///
/// The image is first written to a temporary `<image>.tmp` sibling and only
/// renamed to `image` after `mkdwarfs` exits successfully, so a failed pack
/// can neither leave nor clobber a truncated image at the final path; an
/// existing file at `image` is replaced (atomically) on success only.
/// Blocking; call via `spawn_blocking` from async contexts.
pub fn pack_with_mkdwarfs(staging: &Path, image: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        staging.is_dir(),
        "mkdwarfs input {} is not a directory (expected a staged archive root)",
        staging.display()
    );
    let image_name = image
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("image path {} has no file name", image.display()))?;
    let mut tmp_name = image_name.to_os_string();
    tmp_name.push(".tmp");
    let tmp_image = image.with_file_name(tmp_name);
    // --log-level=warn keeps stderr down to actual problems, so the clipped
    // snippet below carries the real failure rather than progress noise.
    let out = std::process::Command::new("mkdwarfs")
        .arg("-i")
        .arg(staging)
        .arg("-o")
        .arg(&tmp_image)
        .arg("--no-progress")
        .arg("--log-level=warn")
        .arg("--force")
        .output()
        .context("spawn mkdwarfs (is the dwarfs package in the environment?)")?;
    if !out.status.success() {
        // Best effort: a failed pack should not leave its temporary output
        // behind either.
        let _ = std::fs::remove_file(&tmp_image);
        anyhow::bail!(
            "mkdwarfs -i {} -o {} failed ({}): {}",
            staging.display(),
            tmp_image.display(),
            out.status,
            crate::body_snippet(std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")),
        );
    }
    std::fs::rename(&tmp_image, image).with_context(|| {
        format!(
            "rename packed image {} to {}",
            tmp_image.display(),
            image.display()
        )
    })
}

/// Write one JSON record per line, each line terminated by `\n`, streamed
/// through a buffered writer rather than assembled in memory.
fn write_jsonl<T: Serialize>(path: &Path, records: &[T]) -> anyhow::Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut out = std::io::BufWriter::new(file);
    for record in records {
        write_jsonl_line(&mut out, record)
            .with_context(|| format!("write a record to {}", path.display()))?;
    }
    out.flush()
        .with_context(|| format!("flush {}", path.display()))
}

/// Serialize one record as a single JSONL line into `out`.
fn write_jsonl_line<T: Serialize>(out: &mut impl std::io::Write, record: &T) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *out, record)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Write a single pretty-printed JSON value with a trailing newline (every
/// member the writer emits ends with a newline, so committed fixture
/// archives stay byte-identical under the end-of-file formatting hook).
fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("serialize {}", path.display()))?;
    bytes.push(b'\n');
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))
}

/// Recursively copy a store-path tree into the staging directory:
/// directories recurse, regular files keep their permission bits (the
/// executable bit is NAR-relevant), symlinks are recreated with their
/// literal target. Anything else (FIFO, socket, device) is an error naming
/// the offending path.
fn copy_tree(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let file_type = std::fs::symlink_metadata(src)
        .with_context(|| format!("stat {}", src.display()))?
        .file_type();
    if file_type.is_dir() {
        std::fs::create_dir_all(dst).with_context(|| format!("create {}", dst.display()))?;
        for entry in std::fs::read_dir(src).with_context(|| format!("list {}", src.display()))? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else if file_type.is_file() {
        // std::fs::copy carries the permission bits over, which preserves the
        // executable bit the NAR serialization cares about.
        std::fs::copy(src, dst)
            .with_context(|| format!("copy {} to {}", src.display(), dst.display()))?;
    } else if file_type.is_symlink() {
        let target =
            std::fs::read_link(src).with_context(|| format!("readlink {}", src.display()))?;
        std::os::unix::fs::symlink(&target, dst)
            .with_context(|| format!("symlink {} -> {}", dst.display(), target.display()))?;
    } else {
        anyhow::bail!(
            "unsupported file type at {} (only directories, regular files, and symlinks can be \
             embedded)",
            src.display()
        );
    }
    Ok(())
}

/// `std::io::Write` adapter feeding a SHA-256 hasher, so embedded trees can
/// be NAR-serialized straight into the digest without buffering the whole
/// NAR in memory.
#[derive(Default)]
struct Sha256Writer {
    hasher: sha2::Sha256,
}

impl Sha256Writer {
    /// Lowercase hex digest of everything written so far.
    fn finish(self) -> String {
        hex::encode(self.hasher.finalize())
    }
}

impl std::io::Write for Sha256Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! The canonical tiny archive shared by the writer, packing, and reader
    //! tests: two derivations (`app` depending on `dep`), one embedded
    //! source tree with a narinfo sidecar, two requests, outcomes, units,
    //! closures, and one exclusion.
    //!
    //! The committed-fixture integration test
    //! (`rio-parity/tests/archive_fixture.rs`) cannot see this `#[cfg(test)]`
    //! module and deliberately keeps its own copy of these constants and
    //! builders.

    use std::collections::BTreeMap;
    use std::path::Path;

    use sha2::Digest as _;

    use super::{ArchiveWriter, FinalizedArchive, ManifestSeed};
    use crate::archive::schema::{
        Capabilities, ClosureRecord, EXCLUSION_REASON_EVAL_ERROR, ExclusionRecord, ExpectedOutcome,
        OutcomeRecord, OutputHash, RequestRecord, RequestTarget, Substituters, UnitRecord,
    };

    pub(crate) const DEP_DRV: &str = "/nix/store/d1111111111111111111111111111111-dep.drv";
    pub(crate) const APP_DRV: &str = "/nix/store/d2222222222222222222222222222222-app.drv";
    pub(crate) const SRC_PATH: &str = "/nix/store/g1111111111111111111111111111111-src";
    pub(crate) const DEP_OUT: &str = "/nix/store/f1111111111111111111111111111111-dep";
    pub(crate) const APP_OUT: &str = "/nix/store/f2222222222222222222222222222222-app";

    /// ATerm of `dep.drv`: builds `dep` from the embedded `src` path.
    pub(crate) const DEP_ATERM: &str = concat!(
        r#"Derive([("out","/nix/store/f1111111111111111111111111111111-dep","","")],[],["/nix/store/g1111111111111111111111111111111-src"],"x86_64-linux","/bin/sh",["-c","cp -r $src $out"],[("out","/nix/store/f1111111111111111111111111111111-dep"),("src","/nix/store/g1111111111111111111111111111111-src")])"#,
        "\n"
    );

    /// ATerm of `app.drv`: depends on `dep.drv`.
    pub(crate) const APP_ATERM: &str = concat!(
        r#"Derive([("out","/nix/store/f2222222222222222222222222222222-app","","")],[("/nix/store/d1111111111111111111111111111111-dep.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","/nix/store/f2222222222222222222222222222222-app")])"#,
        "\n"
    );

    /// Populate `dir` with the embedded source tree: a plain file, an
    /// executable file, and a relative symlink, so staging exercises the
    /// full copy fidelity (mode bits and link targets) that the sidecar
    /// agreement check in finalize then verifies.
    pub(crate) fn make_src_tree(dir: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("content.txt"), "hello replay v1\n").unwrap();
        let script = dir.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        std::os::unix::fs::symlink("content.txt", dir.join("latest")).unwrap();
    }

    /// Narinfo sidecar text for the tree at `dir`, derived from its NAR
    /// serialization. Deliberately carries no `URL:` line (sidecars of
    /// embedded paths need none), so finalize exercises the URL-synthesis
    /// path of the sidecar parser.
    pub(crate) fn src_sidecar_text(dir: &Path) -> String {
        let mut nar = Vec::new();
        let nar_size = rio_nix::nar::dump_path_streaming(dir, &mut nar).unwrap();
        let digest: [u8; 32] = sha2::Sha256::digest(&nar).into();
        let nar_hash = rio_nix::store_path::nixbase32::encode(&digest);
        format!(
            "StorePath: {SRC_PATH}\nNarHash: sha256:{nar_hash}\nNarSize: {nar_size}\nReferences:\nCompression: none\n"
        )
    }

    /// The fixed manifest seed of the tiny archive.
    pub(crate) fn tiny_seed() -> ManifestSeed {
        let stamp: jiff::Timestamp = "2026-05-28T00:00:00Z".parse().unwrap();
        let mut provenance = serde_json::Map::new();
        provenance.insert(
            "recorder".to_string(),
            serde_json::Value::from("fixture-generator"),
        );
        provenance.insert(
            "description".to_string(),
            serde_json::Value::from("tiny test archive"),
        );
        ManifestSeed {
            created_at: stamp,
            from: stamp,
            to: stamp,
            capabilities: Capabilities {
                timed: false,
                expected_outcomes: true,
                output_hashes: true,
                embedded_store_paths: true,
                impure_env: false,
                dependency_closures: true,
            },
            substituters: Substituters {
                relay: vec!["https://cache.example.org".to_string()],
                target: vec!["https://cache.example.org".to_string()],
            },
            fat: false,
            provenance,
        }
    }

    /// The two recorded requests of the tiny archive.
    pub(crate) fn tiny_requests() -> Vec<RequestRecord> {
        vec![
            RequestRecord {
                session: 0,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: APP_DRV.to_string(),
                    outputs: Vec::new(),
                }],
            },
            RequestRecord {
                session: 1,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: DEP_DRV.to_string(),
                    outputs: vec!["out".to_string()],
                }],
            },
        ]
    }

    /// Stage every member of the tiny archive into `writer`, building the
    /// embedded source tree at `src_tree`.
    pub(crate) fn stage_tiny_archive(writer: &ArchiveWriter, src_tree: &Path) {
        writer.add_drv(DEP_DRV, DEP_ATERM).unwrap();
        writer.add_drv(APP_DRV, APP_ATERM).unwrap();
        make_src_tree(src_tree);
        writer
            .embed_store_path(SRC_PATH, src_tree, &src_sidecar_text(src_tree))
            .unwrap();
        writer.write_requests(&tiny_requests()).unwrap();
        writer
            .write_outcomes(&[
                OutcomeRecord {
                    session: None,
                    drv: DEP_DRV.to_string(),
                    outcome: ExpectedOutcome::Built,
                    detail: None,
                    duration_s: None,
                    stop_offset_s: None,
                    outputs: BTreeMap::from([(
                        "out".to_string(),
                        OutputHash {
                            nar_hash_hex: "1".repeat(64),
                            nar_size: 120,
                        },
                    )]),
                },
                OutcomeRecord {
                    session: Some(0),
                    drv: APP_DRV.to_string(),
                    outcome: ExpectedOutcome::Failed,
                    detail: Some("status=1".to_string()),
                    duration_s: None,
                    stop_offset_s: None,
                    outputs: BTreeMap::new(),
                },
            ])
            .unwrap();
        writer
            .write_units(&[
                UnitRecord {
                    drv: DEP_DRV.to_string(),
                    label: Some("pkgs.dep.x86_64-linux".to_string()),
                    system: Some("x86_64-linux".to_string()),
                    outputs: BTreeMap::from([("out".to_string(), DEP_OUT.to_string())]),
                    required_features: Vec::new(),
                    identity_divergent: false,
                },
                UnitRecord {
                    drv: APP_DRV.to_string(),
                    label: Some("pkgs.app.x86_64-linux".to_string()),
                    system: Some("x86_64-linux".to_string()),
                    outputs: BTreeMap::from([("out".to_string(), APP_OUT.to_string())]),
                    required_features: Vec::new(),
                    identity_divergent: false,
                },
            ])
            .unwrap();
        writer
            .write_closures(&[
                ClosureRecord {
                    drv: DEP_DRV.to_string(),
                    inputs: Vec::new(),
                    srcs: vec![SRC_PATH.to_string()],
                    outputs: BTreeMap::from([("out".to_string(), Some(DEP_OUT.to_string()))]),
                },
                ClosureRecord {
                    drv: APP_DRV.to_string(),
                    inputs: vec![DEP_DRV.to_string()],
                    srcs: Vec::new(),
                    outputs: BTreeMap::from([("out".to_string(), Some(APP_OUT.to_string()))]),
                },
            ])
            .unwrap();
        writer
            .write_exclusions(&[ExclusionRecord {
                label: Some("pkgs.broken.x86_64-linux".to_string()),
                drv: None,
                reason: EXCLUSION_REASON_EVAL_ERROR.to_string(),
                detail: Some("evaluation failed".to_string()),
            }])
            .unwrap();
    }

    /// Stage and finalize the tiny archive at `root`.
    pub(crate) fn tiny_archive(root: &Path) -> FinalizedArchive {
        let writer = ArchiveWriter::create(root).unwrap();
        let src_tree = tempfile::TempDir::new().unwrap();
        stage_tiny_archive(&writer, &src_tree.path().join("src"));
        writer.finalize(tiny_seed()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::test_support::*;
    use super::*;
    use crate::archive::schema::{EXCLUSION_REASON_UNSUPPORTED, ExpectedOutcome, RequestTarget};
    use crate::archive::{
        CLOSURES_MEMBER, EXCLUSIONS_MEMBER, IMPURE_ENV_MEMBER, MANIFEST_MEMBER, NARINFO_DIR,
        OUTCOMES_MEMBER, REQUESTS_MEMBER, UNITS_MEMBER,
    };

    #[test]
    fn finalize_writes_a_manifest_with_correct_counts_and_digests() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        let finalized = tiny_archive(&root);

        let manifest_bytes = std::fs::read(root.join(MANIFEST_MEMBER)).unwrap();
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest, finalized.manifest);
        assert_eq!(manifest.format_version, "1.0");
        assert_eq!(manifest.counts.requests, 2);
        assert_eq!(manifest.counts.workload_units, 2);
        assert_eq!(manifest.counts.expected_outcomes, 2);
        assert_eq!(manifest.counts.embedded_drvs, 2);
        assert_eq!(manifest.counts.embedded_store_paths, 1);

        // Exactly the five staged metadata members are listed (no
        // impure-env.json, no manifest.json), each with the digest and size
        // of the staged bytes.
        let mut expected_members = vec![
            REQUESTS_MEMBER,
            OUTCOMES_MEMBER,
            UNITS_MEMBER,
            CLOSURES_MEMBER,
            EXCLUSIONS_MEMBER,
        ];
        expected_members.sort_unstable();
        assert_eq!(
            manifest
                .files
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            expected_members
        );
        assert!(!manifest.files.contains_key(IMPURE_ENV_MEMBER));
        for (member, digest) in &manifest.files {
            let bytes = std::fs::read(root.join(member)).unwrap();
            assert_eq!(digest.sha256, identity::sha256_hex(&bytes), "{member}");
            assert_eq!(digest.size, bytes.len() as u64, "{member}");
        }

        assert_ne!(
            manifest.content_digests.narinfo,
            identity::EMPTY_LISTING_DIGEST
        );
        assert_eq!(finalized.archive_id, identity::sha256_hex(&manifest_bytes));

        // A finalized root refuses restaging.
        let err = ArchiveWriter::create(&root).unwrap_err().to_string();
        assert!(err.contains("already contains manifest.json"), "got: {err}");

        // Identical content staged at a different root produces the same id.
        let dir2 = tempfile::TempDir::new().unwrap();
        let root2 = dir2.path().join("archive");
        let finalized2 = tiny_archive(&root2);
        assert_eq!(finalized2.archive_id, finalized.archive_id);
    }

    #[test]
    fn requests_normalize_empty_outputs_to_star() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        tiny_archive(&root);

        let requests = std::fs::read_to_string(root.join(REQUESTS_MEMBER)).unwrap();
        let app_line = requests
            .lines()
            .find(|line| line.contains(APP_DRV))
            .expect("requests.jsonl has a line for app.drv");
        assert!(app_line.contains(r#""outputs":["*"]"#), "got: {app_line}");
    }

    #[test]
    fn finalize_requires_requests() {
        let dir = tempfile::TempDir::new().unwrap();
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        writer.add_drv(DEP_DRV, DEP_ATERM).unwrap();

        let err = writer.finalize(tiny_seed()).unwrap_err().to_string();
        assert!(err.contains(REQUESTS_MEMBER), "got: {err}");
        assert!(err.contains("required"), "got: {err}");
    }

    #[test]
    fn capability_without_backing_member_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        writer.add_drv(DEP_DRV, DEP_ATERM).unwrap();
        writer.add_drv(APP_DRV, APP_ATERM).unwrap();
        writer.write_requests(&tiny_requests()).unwrap();

        let mut seed = tiny_seed();
        seed.capabilities = Capabilities {
            expected_outcomes: true,
            ..Default::default()
        };
        let err = writer.finalize(seed).unwrap_err().to_string();
        assert!(err.contains("capability `expected_outcomes`"), "got: {err}");
    }

    #[test]
    fn staged_member_without_capability_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        writer.add_drv(DEP_DRV, DEP_ATERM).unwrap();
        writer.add_drv(APP_DRV, APP_ATERM).unwrap();
        writer.write_requests(&tiny_requests()).unwrap();
        writer
            .write_outcomes(&[OutcomeRecord {
                session: None,
                drv: DEP_DRV.to_string(),
                outcome: ExpectedOutcome::Built,
                detail: None,
                duration_s: None,
                stop_offset_s: None,
                outputs: BTreeMap::new(),
            }])
            .unwrap();

        let mut seed = tiny_seed();
        seed.capabilities = Capabilities::default();
        let err = writer.finalize(seed).unwrap_err().to_string();
        assert!(err.contains("expected_outcomes"), "got: {err}");
    }

    #[test]
    fn missing_input_drv_fails_closure_completeness() {
        let dir = tempfile::TempDir::new().unwrap();
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        writer.add_drv(APP_DRV, APP_ATERM).unwrap();
        writer
            .write_requests(&[RequestRecord {
                session: 0,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: APP_DRV.to_string(),
                    outputs: Vec::new(),
                }],
            }])
            .unwrap();

        let mut seed = tiny_seed();
        seed.capabilities = Capabilities::default();
        let err = writer.finalize(seed).unwrap_err().to_string();
        assert!(err.contains("missing from nix/store"), "got: {err}");
        assert!(err.contains(DEP_DRV), "got: {err}");
        // The dependent that pulled the missing path in is named too.
        assert!(err.contains(APP_DRV), "got: {err}");
    }

    #[test]
    fn sidecar_disagreement_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        writer.add_drv(DEP_DRV, DEP_ATERM).unwrap();
        writer.add_drv(APP_DRV, APP_ATERM).unwrap();
        writer.write_requests(&tiny_requests()).unwrap();

        let src_dir = tempfile::TempDir::new().unwrap();
        let tree = src_dir.path().join("src");
        make_src_tree(&tree);
        let tampered: String = src_sidecar_text(&tree)
            .lines()
            .map(|line| {
                if line.starts_with("NarSize:") {
                    "NarSize: 9999".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        writer.embed_store_path(SRC_PATH, &tree, &tampered).unwrap();

        let mut seed = tiny_seed();
        seed.capabilities = Capabilities {
            embedded_store_paths: true,
            ..Default::default()
        };
        let err = writer.finalize(seed).unwrap_err().to_string();
        assert!(
            err.contains("disagrees with the embedded tree"),
            "got: {err}"
        );
    }

    #[test]
    fn http_relay_substituter_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        let src_dir = tempfile::TempDir::new().unwrap();
        stage_tiny_archive(&writer, &src_dir.path().join("src"));

        let mut seed = tiny_seed();
        seed.substituters.relay = vec!["http://cache.example.org".to_string()];
        let err = writer.finalize(seed).unwrap_err().to_string();
        assert!(
            err.contains("only https:// and s3:// are allowed"),
            "got: {err}"
        );
    }

    #[test]
    fn exclusions_require_label_or_drv() {
        let dir = tempfile::TempDir::new().unwrap();
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        let err = writer
            .write_exclusions(&[ExclusionRecord {
                label: None,
                drv: None,
                reason: EXCLUSION_REASON_UNSUPPORTED.to_string(),
                detail: None,
            }])
            .unwrap_err()
            .to_string();
        assert!(err.contains("label or drv"), "got: {err}");
    }

    #[test]
    fn missing_sidecar_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        writer.add_drv(DEP_DRV, DEP_ATERM).unwrap();
        writer.add_drv(APP_DRV, APP_ATERM).unwrap();
        writer.write_requests(&tiny_requests()).unwrap();
        let src_dir = tempfile::TempDir::new().unwrap();
        let tree = src_dir.path().join("src");
        make_src_tree(&tree);
        writer
            .embed_store_path(SRC_PATH, &tree, &src_sidecar_text(&tree))
            .unwrap();
        // Drop the sidecar after staging so the embedded tree is orphaned.
        std::fs::remove_file(
            dir.path()
                .join(NARINFO_DIR)
                .join(format!("{}.narinfo", crate::archive::hash_part(SRC_PATH))),
        )
        .unwrap();

        let mut seed = tiny_seed();
        seed.capabilities = Capabilities {
            embedded_store_paths: true,
            ..Default::default()
        };
        let err = writer.finalize(seed).unwrap_err().to_string();
        assert!(err.contains("has no narinfo sidecar"), "got: {err}");
        assert!(err.contains(SRC_PATH), "got: {err}");
    }

    #[test]
    fn add_drv_and_embed_store_path_reject_invalid_arguments() {
        let dir = tempfile::TempDir::new().unwrap();
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        let src_dir = tempfile::TempDir::new().unwrap();
        let tree = src_dir.path().join("src");
        make_src_tree(&tree);

        // add_drv refuses a non-derivation store path.
        let err = writer.add_drv(SRC_PATH, DEP_ATERM).unwrap_err().to_string();
        assert!(err.contains("expects a .drv store path"), "got: {err}");

        // embed_store_path refuses a derivation store path.
        let err = writer
            .embed_store_path(DEP_DRV, &tree, "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("expects a non-drv store path"), "got: {err}");

        // Both refuse paths that are not store paths at all.
        let err = writer
            .add_drv("/tmp/not-a-store-path.drv", DEP_ATERM)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid store path"), "got: {err}");
        let err = writer
            .embed_store_path("/tmp/not-a-store-path", &tree, "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid store path"), "got: {err}");

        // embed_store_path refuses a sidecar describing a different path.
        let other = "/nix/store/h1111111111111111111111111111111-other";
        let err = writer
            .embed_store_path(other, &tree, &src_sidecar_text(&tree))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not match the embedded path"),
            "got: {err}"
        );
    }

    #[test]
    fn pack_with_mkdwarfs_produces_an_image_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        tiny_archive(&root);

        // mkdwarfs's exit status is the success signal here; assert only that
        // a single non-empty image file came out. Validating the image's
        // contents is the job of whatever opens it.
        let image = dir.path().join("basic.dwarfs");
        pack_with_mkdwarfs(&root, &image).unwrap();
        let meta = std::fs::metadata(&image).unwrap();
        assert!(meta.is_file(), "expected a regular image file");
        assert!(meta.len() > 0, "image file is empty");
    }
}

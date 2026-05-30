//! Reader for replay archives: open a directory or DwarFS image, validate
//! its integrity, and expose the v1 in-memory model.
//!
//! Metadata (manifest, requests, outcomes, units, closures, impure-env,
//! exclusions, narinfo sidecars, the `nix/store/` entry index) is parsed
//! eagerly at [`ReplayArchive::open`], which also verifies the manifest's
//! `files` digests and the narinfo listing digest (see
//! `docs/dev/2026-05-28-build-replay-design.md`, "Identity, integrity, and
//! content addressing"). Derivation text and NAR payloads are read lazily;
//! embedded store paths are verified against their narinfo sidecars when
//! they are NAR-serialized ([`ReplayArchive::dump_nar`]), not at open time.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result, anyhow, ensure};
use rio_nix::narinfo::NarInfo;

use super::backend::{Backend, EntryKind, WalkEntry};
use super::schema::{
    Capabilities, ClosureRecord, Counts, ExclusionRecord, ImpureEnv, Manifest, MemberPresence,
    OutcomeRecord, RequestRecord, Substituters, UnitRecord,
};
use super::{
    CLOSURES_MEMBER, EXCLUSIONS_MEMBER, IMPURE_ENV_MEMBER, MANIFEST_MEMBER, METADATA_MEMBERS,
    NARINFO_DIR, OUTCOMES_MEMBER, REQUESTS_MEMBER, STORE_DIR, UNITS_MEMBER,
};
use super::{identity, schema, v0};

/// Which on-disk contract an opened archive was written to. The in-memory
/// model is always v1; v0 archives are upgraded on open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    V0,
    V1,
}

/// An opened replay archive: backend handle plus eagerly parsed metadata.
/// Derivation text and NAR payloads are read lazily.
///
/// `open` is synchronous (file I/O, plus decompression on the DwarFS
/// backend); async callers wrap it in `tokio::task::spawn_blocking`.
#[derive(Debug)]
pub struct ReplayArchive {
    format: ArchiveFormat,
    backend: Backend,
    manifest: Manifest,
    /// `Some` for v1 archives; `None` for v0 archives (no content address).
    archive_id: Option<String>,
    requests: Vec<RequestRecord>,
    workload_units: BTreeSet<String>,
    outcomes: HashMap<(Option<i64>, String), OutcomeRecord>,
    units: HashMap<String, UnitRecord>,
    closures: Vec<ClosureRecord>,
    impure_env: ImpureEnv,
    exclusions: Vec<ExclusionRecord>,
    /// Store-path hash part → parsed narinfo sidecar.
    narinfos: HashMap<String, NarInfo>,
    /// Store-path hash part → `nix/store/` entry (basename, kind, exec bit).
    store_entries: HashMap<String, WalkEntry>,
}

impl ReplayArchive {
    /// Open a `.dwarfs` image or an archive directory, validating the
    /// manifest's integrity tables along the way.
    pub fn open(path: &Path) -> Result<Self> {
        let backend = Backend::open(path)?;

        let manifest_bytes = backend.read_file(MANIFEST_MEMBER)?.ok_or_else(|| {
            anyhow!(
                "{}: no {MANIFEST_MEMBER} — not a replay archive?",
                path.display()
            )
        })?;

        // A manifest without `format_version` is a v0 archive (the contract
        // predating the versioned format) and goes through the
        // upgrade-on-open shim instead of the v1 validation pipeline.
        // Either way every validation failure names the archive being
        // opened, so a caller juggling several archives can tell which one
        // is bad.
        let probe: serde_json::Value = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("{}: malformed {MANIFEST_MEMBER}", path.display()))?;
        if probe.get("format_version").is_none() {
            return Self::open_v0(path, backend, &manifest_bytes)
                .with_context(|| format!("open v0 replay archive {}", path.display()));
        }
        Self::open_v1(path, backend, manifest_bytes)
            .with_context(|| format!("open replay archive {}", path.display()))
    }

    /// The v1 open path: parse the manifest, verify the integrity tables,
    /// and eagerly load the metadata members.
    fn open_v1(path: &Path, backend: Backend, manifest_bytes: Vec<u8>) -> Result<Self> {
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .with_context(|| format!("{}: malformed {MANIFEST_MEMBER}", path.display()))?;
        schema::parse_format_version(&manifest.format_version)
            .with_context(|| format!("{}: {MANIFEST_MEMBER}", path.display()))?;

        ensure_relay_substituter_schemes(&manifest.substituters)?;

        // The staged metadata members and the manifest's `files` table must
        // describe the same set, and every listed member's bytes must match
        // its recorded digest. The manifest itself is never listed.
        let mut staged: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
        for member in METADATA_MEMBERS {
            if let Some(bytes) = backend.read_file(member)? {
                staged.insert(member, bytes);
            }
        }
        for member in staged.keys() {
            ensure!(
                manifest.files.contains_key(*member),
                "{member} is staged in the archive but not listed in manifest.files"
            );
        }
        for (member, digest) in &manifest.files {
            // `files` keys are plain root-level member basenames (the v1
            // layout never nests metadata members); refuse separators and
            // traversal outright so a hostile manifest cannot point the
            // digest check at arbitrary paths.
            ensure!(
                !member.is_empty() && !member.contains('/') && !member.contains(".."),
                "manifest.files key {member:?} is not a plain root-level member name"
            );
            // Members this reader does not know about (optional members added
            // by a later v1 minor) are read and digest-checked too: the
            // `files` table is authoritative for everything it lists.
            let unknown_member_bytes;
            let bytes = match staged.get(member.as_str()) {
                Some(bytes) => bytes.as_slice(),
                None => {
                    unknown_member_bytes = backend.read_file(member)?.ok_or_else(|| {
                        anyhow!("{member} is listed in manifest.files but absent from the archive")
                    })?;
                    unknown_member_bytes.as_slice()
                }
            };
            ensure!(
                bytes.len() as u64 == digest.size,
                "{member}: size mismatch (manifest.files records {} bytes, the archive member \
                 has {})",
                digest.size,
                bytes.len()
            );
            let actual = identity::sha256_hex(bytes);
            ensure!(
                actual == digest.sha256,
                "{member}: sha256 mismatch (manifest.files records {}, the archive member \
                 hashes to {actual})",
                digest.sha256
            );
        }

        // requests.jsonl is the one member every archive must carry: without
        // it there is no workload to replay.
        let requests_bytes = staged
            .get(REQUESTS_MEMBER)
            .ok_or_else(|| anyhow!("{}: no {REQUESTS_MEMBER}", path.display()))?;
        let mut requests: Vec<RequestRecord> = super::parse_jsonl(requests_bytes, REQUESTS_MEMBER)?;
        let workload_units = normalize_requests(&mut requests)?;

        // Expected outcomes keyed by (session, drv); duplicate keys keep the
        // last record (a re-recorded outcome supersedes the earlier one).
        let mut outcomes: HashMap<(Option<i64>, String), OutcomeRecord> = HashMap::new();
        let mut outcome_records: u64 = 0;
        if let Some(bytes) = staged.get(OUTCOMES_MEMBER) {
            for record in super::parse_jsonl::<OutcomeRecord>(bytes, OUTCOMES_MEMBER)? {
                outcome_records += 1;
                outcomes.insert((record.session, record.drv.clone()), record);
            }
        }

        // Per-unit metadata; records for derivations outside the workload are
        // dropped with a warning (they cannot be scheduled, so keeping them
        // would only mislead filters and reports).
        let mut units: HashMap<String, UnitRecord> = HashMap::new();
        if let Some(bytes) = staged.get(UNITS_MEMBER) {
            for record in super::parse_jsonl::<UnitRecord>(bytes, UNITS_MEMBER)? {
                if !workload_units.contains(&record.drv) {
                    tracing::warn!(
                        drv = %record.drv,
                        "ignoring {UNITS_MEMBER} record for a derivation that is not a \
                         workload unit"
                    );
                    continue;
                }
                units.insert(record.drv.clone(), record);
            }
        }

        let closures: Vec<ClosureRecord> = match staged.get(CLOSURES_MEMBER) {
            Some(bytes) => super::parse_jsonl(bytes, CLOSURES_MEMBER)?,
            None => Vec::new(),
        };

        let impure_env: ImpureEnv = match staged.get(IMPURE_ENV_MEMBER) {
            Some(bytes) => serde_json::from_slice(bytes)
                .with_context(|| format!("malformed {IMPURE_ENV_MEMBER}"))?,
            None => ImpureEnv::new(),
        };

        let exclusions: Vec<ExclusionRecord> = match staged.get(EXCLUSIONS_MEMBER) {
            Some(bytes) => {
                let records: Vec<ExclusionRecord> = super::parse_jsonl(bytes, EXCLUSIONS_MEMBER)?;
                for record in &records {
                    // An exclusion that names nothing cannot enter the
                    // completeness accounting.
                    ensure!(
                        record.label.is_some() || record.drv.is_some(),
                        "{EXCLUSIONS_MEMBER}: exclusion record (reason {:?}) must carry a \
                         label or drv",
                        record.reason
                    );
                }
                records
            }
            None => Vec::new(),
        };

        let (narinfos, narinfo_digests) = index_narinfos(&backend, SidecarPolicy::Strict)?;
        let store_entries = index_store_entries(&backend)?;

        // The narinfo listing digest covers the sidecars' load-bearing
        // References lines; verify it at open time (sidecars are small).
        // The drvs digest is not verified here (an input-addressed .drv path
        // commits to its ATerm, so corruption surfaces at import time) and
        // embedded store paths are verified against their sidecars when they
        // are NAR-serialized for upload.
        let recomputed_narinfo_digest = identity::listing_digest(&narinfo_digests);
        ensure!(
            recomputed_narinfo_digest == manifest.content_digests.narinfo,
            "narinfo listing digest mismatch (manifest records {}, the archive's sidecars hash \
             to {recomputed_narinfo_digest})",
            manifest.content_digests.narinfo
        );

        // Capability flags must be backed by the data they claim. The
        // reverse direction (member present, flag false) is only a warning:
        // the data is loaded, but the engine will gate on the flags.
        let presence = MemberPresence {
            outcomes: staged.contains_key(OUTCOMES_MEMBER),
            units: staged.contains_key(UNITS_MEMBER),
            closures: staged.contains_key(CLOSURES_MEMBER),
            impure_env: staged.contains_key(IMPURE_ENV_MEMBER),
            exclusions: staged.contains_key(EXCLUSIONS_MEMBER),
            embedded_store_paths: store_entries
                .values()
                .any(|entry| !entry.name.ends_with(".drv")),
        };
        manifest.capabilities.require_backing_members(&presence)?;
        for (member_present, flag_set, staged_what, flag) in [
            (
                presence.outcomes,
                manifest.capabilities.expected_outcomes,
                OUTCOMES_MEMBER,
                "expected_outcomes",
            ),
            (
                presence.impure_env,
                manifest.capabilities.impure_env,
                IMPURE_ENV_MEMBER,
                "impure_env",
            ),
            (
                presence.closures,
                manifest.capabilities.dependency_closures,
                CLOSURES_MEMBER,
                "dependency_closures",
            ),
            (
                presence.embedded_store_paths,
                manifest.capabilities.embedded_store_paths,
                "embedded non-drv store paths",
                "embedded_store_paths",
            ),
        ] {
            if member_present && !flag_set {
                tracing::warn!(
                    "archive carries {staged_what} but capability `{flag}` is not set; the \
                     engine will gate on the flag"
                );
            }
        }

        // v1 requires a sidecar for every embedded non-drv store path: the
        // sidecar carries the NarHash/NarSize/References the supply path
        // depends on.
        for entry in store_entries.values() {
            if entry.name.ends_with(".drv") {
                continue;
            }
            ensure!(
                narinfos.contains_key(super::hash_part(&entry.name)),
                "embedded store path {}{} has no narinfo sidecar",
                rio_nix::store_path::STORE_PREFIX,
                entry.name
            );
        }

        // Counts are informational; disagreement is logged, never fatal.
        let recomputed_counts = Counts {
            requests: requests.len() as u64,
            workload_units: workload_units.len() as u64,
            expected_outcomes: outcome_records,
            embedded_drvs: store_entries
                .values()
                .filter(|entry| entry.name.ends_with(".drv"))
                .count() as u64,
            embedded_store_paths: store_entries
                .values()
                .filter(|entry| !entry.name.ends_with(".drv"))
                .count() as u64,
        };
        if recomputed_counts != manifest.counts {
            tracing::warn!(
                manifest_counts = ?manifest.counts,
                ?recomputed_counts,
                "manifest.counts disagrees with the archive contents (informational only)"
            );
        }

        // The archive id is the digest of the manifest member's bytes exactly
        // as stored — never of a re-serialization of the parsed manifest.
        let archive_id = identity::archive_id_from_manifest_bytes(&manifest_bytes);

        Ok(Self {
            format: ArchiveFormat::V1,
            backend,
            manifest,
            archive_id: Some(archive_id),
            requests,
            workload_units,
            outcomes,
            units,
            closures,
            impure_env,
            exclusions,
            narinfos,
            store_entries,
        })
    }

    /// The v0 upgrade-on-open path: parse the legacy nxb-replay members and
    /// map them into the v1 in-memory model (see
    /// `docs/dev/2026-05-28-build-replay-design.md`, "v0 compatibility").
    ///
    /// v0 archives carry no `files`/`content_digests` integrity tables, no
    /// capability flags, and no content-addressed identity, so the v1
    /// digest, sidecar-presence, and capability cross-checks do not apply;
    /// capabilities are inferred from member presence (and are therefore
    /// consistent by construction). The relay-substituter scheme rule does
    /// apply: a cleartext relay is just as unusable at campaign time in a v0
    /// recording as in a v1 archive. Units, closures, and exclusions do not
    /// exist in v0 and load as empty.
    fn open_v0(path: &Path, backend: Backend, manifest_bytes: &[u8]) -> Result<Self> {
        let v0_manifest: v0::V0Manifest = serde_json::from_slice(manifest_bytes)
            .with_context(|| format!("{}: malformed {MANIFEST_MEMBER}", path.display()))?;

        // requests.jsonl is required in v0 just as in v1: without it there is
        // no workload to replay.
        let requests_bytes = backend
            .read_file(REQUESTS_MEMBER)?
            .ok_or_else(|| anyhow!("{}: no {REQUESTS_MEMBER}", path.display()))?;
        let mut requests: Vec<RequestRecord> =
            super::parse_jsonl::<v0::V0Request>(&requests_bytes, REQUESTS_MEMBER)?
                .into_iter()
                .map(v0::map_request)
                .collect();
        let workload_units = normalize_requests(&mut requests)?;

        // builds.jsonl is the v0 truth member; each record maps into the
        // neutral outcome vocabulary, keyed like v1 with the last record
        // winning (a re-recorded outcome supersedes the earlier one).
        let builds_bytes = backend.read_file(v0::V0_BUILDS_MEMBER)?;
        let has_builds = builds_bytes.is_some();
        let mut outcomes: HashMap<(Option<i64>, String), OutcomeRecord> = HashMap::new();
        if let Some(bytes) = &builds_bytes {
            for record in super::parse_jsonl::<v0::V0BuildRecord>(bytes, v0::V0_BUILDS_MEMBER)? {
                let mapped = v0::map_build_record(record);
                outcomes.insert((mapped.session, mapped.drv.clone()), mapped);
            }
        }

        let impure_env_bytes = backend.read_file(IMPURE_ENV_MEMBER)?;
        let has_impure_env = impure_env_bytes.is_some();
        let impure_env: ImpureEnv = match &impure_env_bytes {
            Some(bytes) => serde_json::from_slice(bytes)
                .with_context(|| format!("malformed {IMPURE_ENV_MEMBER}"))?,
            None => ImpureEnv::new(),
        };

        // Sidecars and the store index are read exactly as for v1 (including
        // the URL-synthesis fallback); only the unparseable-sidecar policy
        // differs — see [`SidecarPolicy`]. The v1 narinfo listing digest has
        // nothing to be checked against, so the recomputed digests are
        // dropped.
        let (narinfos, _) = index_narinfos(&backend, SidecarPolicy::WarnAndSkip)?;
        let store_entries = index_store_entries(&backend)?;

        let output_hashes_present = outcomes.values().any(|record| !record.outputs.is_empty());
        let has_embedded_paths = store_entries
            .values()
            .any(|entry| !entry.name.ends_with(".drv"));

        let mut manifest = v0::map_manifest(
            v0_manifest,
            workload_units.len() as u64,
            has_builds,
            output_hashes_present,
            has_impure_env,
            has_embedded_paths,
        );
        // The recorded v0 counts are advisory; requests and expected
        // outcomes (which v0 manifests never record) are recomputed from the
        // parsed members so the exposed counts always describe what was
        // actually loaded.
        manifest.counts.requests = requests.len() as u64;
        manifest.counts.expected_outcomes = outcomes.len() as u64;

        ensure_relay_substituter_schemes(&manifest.substituters)?;

        Ok(Self {
            format: ArchiveFormat::V0,
            backend,
            manifest,
            // v0 archives have no content-addressed identity; they are
            // referenced by path and cannot be published to the v1 S3 layout.
            archive_id: None,
            requests,
            workload_units,
            outcomes,
            units: HashMap::new(),
            closures: Vec::new(),
            impure_env,
            exclusions: Vec::new(),
            narinfos,
            store_entries,
        })
    }

    /// Which on-disk contract the opened archive was written to.
    pub fn format(&self) -> ArchiveFormat {
        self.format
    }

    /// The archive manifest (v0 manifests are mapped into this model on open).
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Capability flags: what the archive contains and what the engine may
    /// gate on.
    pub fn capabilities(&self) -> &Capabilities {
        &self.manifest.capabilities
    }

    /// `Some` for v1 archives (SHA-256 of the manifest member bytes);
    /// `None` for v0 archives, which have no content-addressed identity.
    pub fn archive_id(&self) -> Option<&str> {
        self.archive_id.as_deref()
    }

    /// The short id (first 16 hex characters of [`Self::archive_id`]).
    pub fn archive_id_short(&self) -> Option<String> {
        self.archive_id.as_deref().map(identity::short_id)
    }

    /// The raw bytes of the archive's `manifest.json` member, exactly as
    /// stored — re-read from the backend, never re-serialized from the
    /// parsed [`Manifest`] (the archive id is defined over the stored
    /// bytes, and a re-serialization would not be byte-identical).
    /// Publishing an archive uploads exactly these bytes as the standalone
    /// S3 manifest object next to the image.
    ///
    /// Errors for v0 archives: their manifest predates the v1 identity
    /// contract, so they have no canonical manifest bytes to publish.
    pub fn manifest_bytes(&self) -> Result<Vec<u8>> {
        ensure!(
            self.format == ArchiveFormat::V1,
            "v0 archives have no content-addressed identity, so there are no canonical \
             manifest bytes to expose; only v1 archives can be published"
        );
        self.backend.read_file(MANIFEST_MEMBER)?.ok_or_else(|| {
            anyhow!("{MANIFEST_MEMBER} is no longer readable from an archive that was opened")
        })
    }

    /// Recorded requests, sorted by `offset_s` ascending, negative offsets
    /// clamped to 0, output lists normalized so `[]` becomes `["*"]`.
    pub fn requests(&self) -> &[RequestRecord] {
        &self.requests
    }

    /// Distinct workload-unit drv paths (the union of all targets).
    pub fn workload_units(&self) -> &BTreeSet<String> {
        &self.workload_units
    }

    /// Expected outcomes keyed by `(session, drv)`; duplicate keys keep the
    /// last record. Empty when the archive has no truth member.
    pub fn outcomes(&self) -> &HashMap<(Option<i64>, String), OutcomeRecord> {
        &self.outcomes
    }

    /// Lookup order: exact `(Some(session), drv)`, then `(None, drv)`.
    pub fn expected_outcome(&self, session: i64, drv: &str) -> Option<&OutcomeRecord> {
        self.outcomes
            .get(&(Some(session), drv.to_string()))
            .or_else(|| self.outcomes.get(&(None, drv.to_string())))
    }

    /// Per-unit metadata keyed by drv path (empty when units.jsonl absent).
    pub fn units(&self) -> &HashMap<String, UnitRecord> {
        &self.units
    }

    /// Direct dependency adjacency records (empty when closures.jsonl absent).
    pub fn closures(&self) -> &[ClosureRecord] {
        &self.closures
    }

    /// drv path → impure environment variable names. Empty when the member
    /// is absent.
    pub fn impure_env(&self) -> &ImpureEnv {
        &self.impure_env
    }

    /// Scope items the recorder could not turn into workload units (empty
    /// when exclusions.jsonl absent).
    pub fn exclusions(&self) -> &[ExclusionRecord] {
        &self.exclusions
    }

    /// Narinfo sidecar by store-path hash part (full path or basename also
    /// accepted).
    pub fn narinfo(&self, hash_part_or_path: &str) -> Option<&NarInfo> {
        self.narinfos.get(super::hash_part(hash_part_or_path))
    }

    /// True if the archive embeds this store path's contents (a non-drv tree).
    pub fn has_embedded(&self, store_path: &str) -> bool {
        self.store_entries
            .get(super::hash_part(store_path))
            .is_some_and(|entry| !entry.name.ends_with(".drv"))
    }

    /// Embedded non-drv store paths (full /nix/store/... paths, sorted).
    pub fn embedded_store_paths(&self) -> Vec<String> {
        self.collect_store_paths(|name| !name.ends_with(".drv"))
    }

    /// Embedded derivation store paths (full /nix/store/... paths, sorted).
    pub fn embedded_drvs(&self) -> Vec<String> {
        self.collect_store_paths(|name| name.ends_with(".drv"))
    }

    /// Full store paths of the `nix/store/` entries whose basename satisfies
    /// `keep`, sorted.
    fn collect_store_paths(&self, keep: impl Fn(&str) -> bool) -> Vec<String> {
        let mut paths: Vec<String> = self
            .store_entries
            .values()
            .filter(|entry| keep(&entry.name))
            .map(|entry| format!("{}{}", rio_nix::store_path::STORE_PREFIX, entry.name))
            .collect();
        paths.sort();
        paths
    }

    /// Read a `.drv` member's ATerm text (full store path or basename).
    pub fn read_drv(&self, drv_path: &str) -> Result<String> {
        let entry = self
            .store_entries
            .get(super::hash_part(drv_path))
            .ok_or_else(|| anyhow!("derivation {drv_path} is not present in the archive"))?;
        ensure!(
            entry.name.ends_with(".drv"),
            "{drv_path} resolves to {} in the archive, which is not a .drv",
            entry.name
        );
        let rel = format!("{STORE_DIR}/{}", entry.name);
        let bytes = self
            .backend
            .read_file(&rel)?
            .ok_or_else(|| anyhow!("{rel}: listed in the archive index but unreadable"))?;
        String::from_utf8(bytes).with_context(|| format!("{rel}: derivation text is not UTF-8"))
    }

    /// NAR-serialize an embedded store path and verify it against its
    /// narinfo sidecar's NarHash/NarSize. Blocking; see `open`.
    ///
    /// On the DwarFS backend all content reads serialize on an internal
    /// lock, so callers should not expect parallel-dump throughput.
    pub fn dump_nar(&self, store_path: &str) -> Result<Vec<u8>> {
        // TODO: add a streaming variant (NAR bytes written into an
        // `impl Write` while hashing) before the supply path starts uploading
        // large embedded store paths; buffering the whole NAR in memory is
        // fine for the fixture-sized paths handled today but not for
        // multi-gigabyte toolchain closures.
        let entry = self
            .store_entries
            .get(super::hash_part(store_path))
            .ok_or_else(|| anyhow!("store path {store_path} is not embedded in the archive"))?;
        let rel = format!("{STORE_DIR}/{}", entry.name);
        let nar = match &self.backend {
            Backend::Dir { root } => {
                let mut nar = Vec::new();
                rio_nix::nar::dump_path_streaming(&root.join(&rel), &mut nar)
                    .with_context(|| format!("NAR-serialize {rel}"))?;
                nar
            }
            Backend::Dwarfs(_) => {
                let node = self
                    .backend
                    .nar_node(&rel, entry)
                    .with_context(|| format!("NAR-serialize {rel} from the DwarFS image"))?;
                let mut nar = Vec::new();
                rio_nix::nar::serialize(&mut nar, &node)
                    .with_context(|| format!("NAR-serialize {rel} from the DwarFS image"))?;
                nar
            }
        };

        // Use-time integrity: the produced NAR must agree with the sidecar
        // that describes it. v0 archives may legitimately lack a sidecar, in
        // which case the NAR is returned unverified; for v1 a missing sidecar
        // cannot happen if open succeeded.
        match self.narinfo(store_path) {
            Some(narinfo) => {
                let sidecar_hex = crate::nixcache::narhash_to_hex(&narinfo.nar_hash)
                    .with_context(|| format!("narinfo sidecar for {store_path}"))?;
                let nar_hex = identity::sha256_hex(&nar);
                ensure!(
                    nar_hex == sidecar_hex && nar.len() as u64 == narinfo.nar_size,
                    "{store_path}: the archived tree does not match its narinfo sidecar \
                     (sidecar NarHash {sidecar_hex} NarSize {}, archive NAR sha256 {nar_hex} \
                     size {})",
                    narinfo.nar_size,
                    nar.len()
                );
            }
            None => ensure!(
                self.format == ArchiveFormat::V0,
                "{store_path}: embedded store path has no narinfo sidecar to verify against"
            ),
        }
        Ok(nar)
    }
}

/// Relay substituters are campaign-time fetch sources; refuse plain http://
/// (the engine never relays over cleartext). Applies to both archive
/// formats — for v0 the list comes from the mapped `src_substituters`.
fn ensure_relay_substituter_schemes(substituters: &Substituters) -> Result<()> {
    for relay in &substituters.relay {
        ensure!(
            relay.starts_with("https://") || relay.starts_with("s3://"),
            "relay substituter {relay:?}: only https:// and s3:// are allowed"
        );
    }
    Ok(())
}

/// What to do with a narinfo sidecar that fails to parse, the one place the
/// v0 and v1 open paths deliberately differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidecarPolicy {
    /// v1: a hard open error naming the offending file. Skipping could never
    /// help a v1 archive anyway — a skipped sidecar would be missing from
    /// the recomputed narinfo listing digest, so open would still fail, just
    /// with a far less actionable message.
    Strict,
    /// v0: warn and skip. v0 archives are digest-less, irreplaceable
    /// recordings of past production windows; one bad sidecar only makes
    /// that path non-uploadable from the archive and must not refuse the
    /// whole recording.
    WarnAndSkip,
}

/// `(store path, sidecar sha256)` listing entries, in the shape
/// [`identity::listing_digest`] consumes.
type SidecarDigestListing = Vec<(String, String)>;

/// Index the `narinfo/` sidecar directory: store-path hash part → parsed
/// sidecar, plus the `(store path, sidecar sha256)` listing the v1 integrity
/// check recomputes its narinfo digest from. Sidecars without a `URL:` line
/// get one synthesized (see [`super::parse_narinfo_sidecar`]); sidecars that
/// still fail to parse are handled per `policy`.
fn index_narinfos(
    backend: &Backend,
    policy: SidecarPolicy,
) -> Result<(HashMap<String, NarInfo>, SidecarDigestListing)> {
    let mut narinfos: HashMap<String, NarInfo> = HashMap::new();
    let mut narinfo_digests: SidecarDigestListing = Vec::new();
    for entry in backend.list_dir(NARINFO_DIR)?.unwrap_or_default() {
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
            .and_then(|text| super::parse_narinfo_sidecar(text, stem));
        let narinfo = match (parsed, policy) {
            (Ok(narinfo), _) => narinfo,
            (Err(err), SidecarPolicy::Strict) => {
                return Err(err.context(format!("unparseable narinfo sidecar {rel}")));
            }
            (Err(err), SidecarPolicy::WarnAndSkip) => {
                tracing::warn!("skipping unparseable narinfo sidecar {rel}: {err:#}");
                continue;
            }
        };
        narinfo_digests.push((narinfo.store_path.clone(), identity::sha256_hex(&bytes)));
        // Key by the hash part so `<hash>-<name>.narinfo` sidecar naming
        // resolves the same as the canonical `<hash>.narinfo`.
        narinfos.insert(super::hash_part(stem).to_string(), narinfo);
    }
    Ok((narinfos, narinfo_digests))
}

/// Index `nix/store/`: hash part → entry. Drives the store-path keyed
/// lookups; contents stay in the backend until asked for.
fn index_store_entries(backend: &Backend) -> Result<HashMap<String, WalkEntry>> {
    let mut store_entries: HashMap<String, WalkEntry> = HashMap::new();
    for entry in backend.list_dir(STORE_DIR)?.unwrap_or_default() {
        store_entries.insert(super::hash_part(&entry.name).to_string(), entry);
    }
    Ok(store_entries)
}

/// Request post-processing shared by both open paths: every request must
/// name at least one target, negative offsets are clamped to 0 (clock skew
/// at capture time can produce slightly negative values; rejecting the whole
/// archive for that would be overkill), `[]` output lists are normalized to
/// `["*"]` (both spellings mean "all outputs"), and the records are sorted
/// by offset ascending (recorded lines are not guaranteed globally ordered —
/// per-session buffers get flushed independently — and the replay timeline
/// wants ascending offsets). Returns the distinct workload-unit drv paths.
fn normalize_requests(requests: &mut [RequestRecord]) -> Result<BTreeSet<String>> {
    for record in requests.iter_mut() {
        ensure!(
            !record.targets.is_empty(),
            "{REQUESTS_MEMBER}: request record (session {}) must have non-empty targets",
            record.session
        );
        record.offset_s = record.offset_s.max(0.0);
        for target in &mut record.targets {
            if target.outputs.is_empty() {
                target.outputs = vec!["*".to_string()];
            }
        }
    }
    requests.sort_by(|a, b| a.offset_s.total_cmp(&b.offset_s));
    Ok(requests
        .iter()
        .flat_map(|record| record.targets.iter().map(|target| target.drv.clone()))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::archive::writer::pack_with_mkdwarfs;
    use crate::archive::writer::test_support::{APP_DRV, DEP_DRV, SRC_PATH, tiny_archive};
    use crate::archive::{FORMAT_VERSION, hash_part};

    /// Stage and finalize the tiny archive in `dir`, returning the staged
    /// root and the writer's view of the identity.
    fn staged_tiny_archive(dir: &Path) -> (PathBuf, crate::archive::writer::FinalizedArchive) {
        let root = dir.join("archive");
        let finalized = tiny_archive(&root);
        (root, finalized)
    }

    /// Rewrite one textual occurrence inside a staged member file.
    fn rewrite_member(root: &Path, member: &str, from: &str, to: &str) {
        let path = root.join(member);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(from), "{member} does not contain {from:?}");
        std::fs::write(&path, text.replace(from, to)).unwrap();
    }

    /// Re-point `manifest.files[member]` at the member's current on-disk
    /// bytes. The manifest itself is not digest-protected, so tests can edit
    /// a member and then refresh its entry to get past the integrity gate
    /// and reach the record-level validation behind it.
    fn refresh_files_entry(root: &Path, member: &str) {
        let bytes = std::fs::read(root.join(member)).unwrap();
        let manifest_path = root.join(MANIFEST_MEMBER);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["files"][member] = serde_json::json!({
            "sha256": identity::sha256_hex(&bytes),
            "size": bytes.len(),
        });
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn opens_a_v1_directory_archive_and_exposes_the_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, finalized) = staged_tiny_archive(dir.path());

        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(archive.format(), ArchiveFormat::V1);
        assert_eq!(archive.archive_id(), Some(finalized.archive_id.as_str()));
        assert_eq!(
            archive.archive_id_short(),
            Some(finalized.archive_id[..16].to_string())
        );
        assert!(archive.capabilities().expected_outcomes);

        assert_eq!(archive.requests().len(), 2);
        assert!(
            archive
                .requests()
                .windows(2)
                .all(|pair| pair[0].offset_s <= pair[1].offset_s),
            "requests are sorted by offset"
        );
        assert_eq!(
            archive.workload_units().iter().collect::<Vec<_>>(),
            vec![DEP_DRV, APP_DRV],
        );

        // Session 7 recorded nothing for dep.drv: the lookup falls back to
        // the session-less truth record.
        let dep = archive.expected_outcome(7, DEP_DRV).unwrap();
        assert_eq!(dep.outcome, crate::archive::schema::ExpectedOutcome::Built);
        // Session 0's app.drv expectation is session-scoped.
        let app = archive.expected_outcome(0, APP_DRV).unwrap();
        assert_eq!(app.outcome, crate::archive::schema::ExpectedOutcome::Failed);
        assert_eq!(app.session, Some(0));

        assert_eq!(archive.units().len(), 2);
        assert!(
            archive.units().values().all(|unit| unit.label.is_some()),
            "got: {:?}",
            archive.units()
        );
        assert_eq!(archive.closures().len(), 2);
        assert_eq!(archive.exclusions().len(), 1);
        assert!(archive.impure_env().is_empty());

        assert!(archive.has_embedded(SRC_PATH));
        assert_eq!(archive.embedded_store_paths(), vec![SRC_PATH.to_string()]);
        assert_eq!(
            archive.embedded_drvs(),
            vec![DEP_DRV.to_string(), APP_DRV.to_string()]
        );

        let aterm = archive.read_drv(DEP_DRV).unwrap();
        let derivation = rio_nix::derivation::Derivation::parse(&aterm).unwrap();
        assert_eq!(derivation.outputs().len(), 1);

        let nar = archive.dump_nar(SRC_PATH).unwrap();
        let sidecar = archive.narinfo(SRC_PATH).unwrap();
        assert_eq!(nar.len() as u64, sidecar.nar_size);
    }

    #[test]
    fn image_and_directory_forms_have_the_same_identity_and_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());
        let image = dir.path().join("archive.dwarfs");
        pack_with_mkdwarfs(&root, &image).unwrap();

        let from_dir = ReplayArchive::open(&root).unwrap();
        let from_image = ReplayArchive::open(&image).unwrap();

        // Container independence: the identity and the parsed model are the
        // same whichever form the same archive takes.
        assert_eq!(from_image.format(), ArchiveFormat::V1);
        assert_eq!(from_dir.archive_id(), from_image.archive_id());
        assert_eq!(from_dir.requests().len(), from_image.requests().len());
        assert_eq!(from_dir.outcomes().len(), from_image.outcomes().len());
        assert_eq!(from_dir.units().len(), from_image.units().len());
        assert_eq!(from_dir.closures().len(), from_image.closures().len());
        assert_eq!(
            from_dir.dump_nar(SRC_PATH).unwrap(),
            from_image.dump_nar(SRC_PATH).unwrap()
        );
    }

    #[test]
    fn unknown_format_major_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        let pinned = format!("\"format_version\": \"{FORMAT_VERSION}\"");
        rewrite_member(
            &root,
            MANIFEST_MEMBER,
            &pinned,
            "\"format_version\": \"2.0\"",
        );
        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("unsupported archive format_version 2.0"),
            "got: {err}"
        );

        // Any minor of the supported major is accepted (additive evolution);
        // the manifest is never listed in `files`, so rewriting it cannot
        // trip the member digest checks.
        rewrite_member(
            &root,
            MANIFEST_MEMBER,
            "\"format_version\": \"2.0\"",
            "\"format_version\": \"1.9\"",
        );
        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(archive.manifest().format_version, "1.9");
    }

    #[test]
    fn metadata_member_corruption_is_detected() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        let path = root.join(REQUESTS_MEMBER);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("sha256 mismatch") || err.contains("size mismatch"),
            "got: {err}"
        );
        assert!(err.contains(REQUESTS_MEMBER), "got: {err}");
    }

    #[test]
    fn unlisted_metadata_member_is_detected() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // Bytes that differ from the digests recorded in `files` are refused.
        let units_path = root.join(UNITS_MEMBER);
        let mut units_text = std::fs::read_to_string(&units_path).unwrap();
        units_text
            .push_str("{\"drv\":\"/nix/store/d3333333333333333333333333333333-extra.drv\"}\n");
        std::fs::write(&units_path, &units_text).unwrap();
        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("sha256 mismatch") || err.contains("size mismatch"),
            "got: {err}"
        );
        assert!(err.contains(UNITS_MEMBER), "got: {err}");

        // A staged metadata member the manifest does not list is refused.
        std::fs::write(root.join(IMPURE_ENV_MEMBER), "{}\n").unwrap();
        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(err.contains("not listed in manifest.files"), "got: {err}");
        std::fs::remove_file(root.join(IMPURE_ENV_MEMBER)).unwrap();

        // A listed member that is gone from the archive is refused.
        std::fs::remove_file(units_path).unwrap();
        std::fs::remove_file(root.join(EXCLUSIONS_MEMBER)).unwrap();
        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("listed in manifest.files but absent"),
            "got: {err}"
        );
    }

    #[test]
    fn narinfo_listing_digest_mismatch_is_detected() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        let sidecar = root
            .join(NARINFO_DIR)
            .join(format!("{}.narinfo", hash_part(SRC_PATH)));
        let mut text = std::fs::read_to_string(&sidecar).unwrap();
        text.push_str("Sig: example.org:0000000000000000000000000000000000000000000000000000\n");
        std::fs::write(&sidecar, text).unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("narinfo listing digest mismatch"),
            "got: {err}"
        );
    }

    #[test]
    fn capability_without_member_is_rejected_on_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // The capabilities object is not covered by `files`, so a manifest
        // edit here reaches the capability check rather than a digest error.
        rewrite_member(
            &root,
            MANIFEST_MEMBER,
            "\"impure_env\": false",
            "\"impure_env\": true",
        );
        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(err.contains("capability `impure_env`"), "got: {err}");
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());
        std::fs::remove_file(root.join(MANIFEST_MEMBER)).unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(err.contains("no manifest.json"), "got: {err}");
        assert!(err.contains("not a replay archive"), "got: {err}");
    }

    #[test]
    fn unknown_extra_members_are_ignored() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, finalized) = staged_tiny_archive(dir.path());

        // Recorders may keep their own QA artifacts next to the spec'd
        // members; they are outside the identity and ignored on open.
        std::fs::write(root.join("fidelity.json"), "{\"checked\": true}\n").unwrap();
        std::fs::create_dir_all(root.join("notes")).unwrap();
        std::fs::write(root.join("notes/readme.txt"), "extra member\n").unwrap();

        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(archive.archive_id(), Some(finalized.archive_id.as_str()));
    }

    #[test]
    fn dump_nar_detects_sidecar_disagreement() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // Embedded trees are not hashed at open time (that is dump-time
        // integrity), so the open succeeds and the dump fails.
        let embedded = root
            .join(STORE_DIR)
            .join(SRC_PATH.rsplit('/').next().unwrap());
        std::fs::write(embedded.join("content.txt"), "tampered after finalize\n").unwrap();

        let archive = ReplayArchive::open(&root).unwrap();
        let err = format!("{:#}", archive.dump_nar(SRC_PATH).unwrap_err());
        assert!(
            err.contains("does not match its narinfo sidecar"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_optional_member_listed_in_files_is_digest_checked() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // A later v1 minor may add an optional member this reader does not
        // know about; listing it in manifest.files must not break open.
        std::fs::write(root.join("fidelity.json"), "{\"checked\": true}\n").unwrap();
        refresh_files_entry(&root, "fidelity.json");
        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(archive.format(), ArchiveFormat::V1);

        // ... but the listed digest is still enforced for it.
        std::fs::write(root.join("fidelity.json"), "{\"checked\": false}\n").unwrap();
        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("sha256 mismatch") || err.contains("size mismatch"),
            "got: {err}"
        );
        assert!(err.contains("fidelity.json"), "got: {err}");
    }

    #[test]
    fn files_keys_with_path_separators_are_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        let manifest_path = root.join(MANIFEST_MEMBER);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["files"]["notes/readme.txt"] = serde_json::json!({
            "sha256": "0".repeat(64),
            "size": 1,
        });
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("not a plain root-level member name"),
            "got: {err}"
        );
    }

    #[test]
    fn unparseable_narinfo_sidecar_is_an_open_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        let sidecar_name = format!("{}.narinfo", hash_part(SRC_PATH));
        let sidecar = root.join(NARINFO_DIR).join(&sidecar_name);
        let text = std::fs::read_to_string(&sidecar).unwrap();
        let broken = text.replace("NarSize: ", "NarSize: not-a-number-");
        assert_ne!(broken, text, "sidecar has a NarSize line to break");
        std::fs::write(&sidecar, broken).unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(err.contains("unparseable narinfo sidecar"), "got: {err}");
        assert!(err.contains(&sidecar_name), "got: {err}");
    }

    #[test]
    fn cleartext_relay_substituter_is_rejected_on_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // The substituter lists live in the manifest, which is not covered by
        // `files`, so the edit reaches the scheme check rather than a digest
        // error.
        rewrite_member(
            &root,
            MANIFEST_MEMBER,
            "https://cache.example.org",
            "http://cache.example.org",
        );
        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("only https:// and s3:// are allowed"),
            "got: {err}"
        );
    }

    #[test]
    fn empty_request_targets_are_rejected_on_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        let path = root.join(REQUESTS_MEMBER);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"session\":2,\"offset_s\":0.0,\"targets\":[]}\n");
        std::fs::write(&path, text).unwrap();
        refresh_files_entry(&root, REQUESTS_MEMBER);

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(err.contains("non-empty targets"), "got: {err}");
    }

    #[test]
    fn duplicate_outcome_records_keep_the_last() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // Re-record the session-less dep.drv truth as `failed`; the later
        // record supersedes the Built one staged by the writer.
        let path = root.join(OUTCOMES_MEMBER);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!(
            "{{\"drv\":\"{DEP_DRV}\",\"outcome\":\"failed\"}}\n"
        ));
        std::fs::write(&path, text).unwrap();
        refresh_files_entry(&root, OUTCOMES_MEMBER);

        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(archive.outcomes().len(), 2, "duplicate keys collapse");
        assert_eq!(
            archive.expected_outcome(7, DEP_DRV).unwrap().outcome,
            crate::archive::schema::ExpectedOutcome::Failed
        );
    }

    // ----- v0 upgrade-on-open shim, against the committed nxb-replay fixture -----

    /// Store paths of the committed v0 fixture (byte-exact copy of the
    /// origin/xtask-replay `replay/basic` fixture; never edited).
    const V0_DEP_DRV: &str = "/nix/store/a1111111111111111111111111111111-dep.drv";
    const V0_APP_DRV: &str = "/nix/store/a2222222222222222222222222222222-app.drv";
    const V0_IMPURE_DRV: &str = "/nix/store/a3333333333333333333333333333333-impure.drv";
    const V0_CACHED_DRV: &str = "/nix/store/a4444444444444444444444444444444-cached.drv";
    const V0_SRC_PATH: &str = "/nix/store/b1111111111111111111111111111111-src.txt";

    /// The committed v0 fixture directory.
    fn v0_fixture_dir() -> PathBuf {
        crate::test_manifest_dir().join("tests/fixtures/archive/v0-basic")
    }

    /// Recursive copy of the committed v0 fixture into `dst` so tests can
    /// delete or edit individual files without touching the committed copy.
    fn copy_v0_fixture_to(dst: &Path) {
        fn copy_tree(src: &Path, dst: &Path) {
            std::fs::create_dir_all(dst).unwrap();
            for entry in std::fs::read_dir(src).unwrap() {
                let entry = entry.unwrap();
                let to = dst.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    copy_tree(&entry.path(), &to);
                } else {
                    std::fs::copy(entry.path(), &to).unwrap();
                }
            }
        }
        copy_tree(&v0_fixture_dir(), dst);
    }

    #[test]
    fn opens_the_v0_fixture_through_the_upgrade_shim() {
        let archive = ReplayArchive::open(&v0_fixture_dir()).unwrap();
        assert_eq!(archive.format(), ArchiveFormat::V0);
        assert!(archive.archive_id().is_none());
        assert!(archive.archive_id_short().is_none());
        // No identity ⇒ no canonical manifest bytes either: v0 archives
        // cannot be published, and the refusal says so.
        let err = format!("{:#}", archive.manifest_bytes().unwrap_err());
        assert!(err.contains("v0"), "got: {err}");

        let manifest = archive.manifest();
        assert_eq!(
            manifest.substituters.relay,
            vec!["https://cache.example.org".to_string()]
        );
        assert!(!manifest.fat);
        assert_eq!(
            manifest.counts,
            Counts {
                requests: 4,
                workload_units: 4,
                expected_outcomes: 3,
                embedded_drvs: 4,
                embedded_store_paths: 1,
            }
        );

        // Capabilities are inferred from member presence: builds.jsonl,
        // output hashes, an embedded tree, and impure-env.json are all
        // there; closures never exist in v0.
        let capabilities = archive.capabilities();
        assert!(capabilities.timed);
        assert!(capabilities.expected_outcomes);
        assert!(capabilities.output_hashes);
        assert!(capabilities.embedded_store_paths);
        assert!(capabilities.impure_env);
        assert!(!capabilities.dependency_closures);

        // Sorted by offset regardless of file order; sessions follow.
        let offsets: Vec<f64> = archive.requests().iter().map(|r| r.offset_s).collect();
        assert_eq!(offsets, vec![0.25, 2.0, 5.5, 9.0]);
        let sessions: Vec<i64> = archive.requests().iter().map(|r| r.session).collect();
        assert_eq!(sessions, vec![10, 13, 11, 12]);
        // The offset-9.0 request's second target was recorded with `[]`
        // (all outputs); the v1 model spells that `["*"]`.
        let last = &archive.requests()[3];
        assert_eq!(last.targets[1].drv, V0_DEP_DRV);
        assert_eq!(last.targets[1].outputs, vec!["*".to_string()]);

        // Truth records arrive through the native-status mapping.
        let dep = archive.expected_outcome(10, V0_DEP_DRV).unwrap();
        assert_eq!(dep.outcome, schema::ExpectedOutcome::Built);
        assert_eq!(dep.outputs["out"].nar_size, 120);
        let app = archive.expected_outcome(11, V0_APP_DRV).unwrap();
        assert_eq!(app.outcome, schema::ExpectedOutcome::Failed);
        // detail keeps the native code alongside the recorder's message.
        assert_eq!(
            app.detail.as_deref(),
            Some("status=1: builder failed with exit code 1")
        );
        let impure = archive.expected_outcome(12, V0_IMPURE_DRV).unwrap();
        assert_eq!(impure.outcome, schema::ExpectedOutcome::Disconnected);
        assert_eq!(impure.stop_offset_s, Some(11.0));
        // The cached drv was a cache hit at record time: no truth record.
        assert!(archive.expected_outcome(12, V0_CACHED_DRV).is_none());

        assert_eq!(archive.impure_env().len(), 1);
        assert!(archive.units().is_empty());
        assert!(archive.closures().is_empty());
        assert!(archive.exclusions().is_empty());

        // Embedded content is reachable exactly as for v1.
        assert!(archive.has_embedded(V0_SRC_PATH));
        let aterm = archive.read_drv(V0_DEP_DRV).unwrap();
        let derivation = rio_nix::derivation::Derivation::parse(&aterm).unwrap();
        assert_eq!(derivation.outputs().len(), 1);

        let nar = archive.dump_nar(V0_SRC_PATH).unwrap();
        let sidecar = archive.narinfo(V0_SRC_PATH).unwrap();
        assert_eq!(nar.len() as u64, sidecar.nar_size);
        assert_eq!(
            identity::sha256_hex(&nar),
            crate::nixcache::narhash_to_hex(&sidecar.nar_hash).unwrap()
        );

        // The URL-less c111… sidecar is accepted via the URL-synthesis
        // fallback, same as v1.
        assert!(
            archive
                .narinfo("c1111111111111111111111111111111")
                .is_some()
        );
    }

    #[test]
    fn opens_the_v0_fixture_dwarfs_image() {
        // The committed image predates the `c111…` sidecar, so assert only
        // content it actually carries.
        let image = crate::test_manifest_dir().join("tests/fixtures/archive/v0-basic.dwarfs");
        let archive = ReplayArchive::open(&image).unwrap();
        assert_eq!(archive.format(), ArchiveFormat::V0);
        assert_eq!(archive.requests().len(), 4);
        assert!(archive.has_embedded(V0_SRC_PATH));

        let nar = archive.dump_nar(V0_SRC_PATH).unwrap();
        let sidecar = archive.narinfo(V0_SRC_PATH).unwrap();
        assert_eq!(nar.len() as u64, sidecar.nar_size);
        assert_eq!(
            identity::sha256_hex(&nar),
            crate::nixcache::narhash_to_hex(&sidecar.nar_hash).unwrap()
        );
    }

    #[test]
    fn v0_missing_optional_files_are_tolerated() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);
        std::fs::remove_file(root.join(crate::archive::v0::V0_BUILDS_MEMBER)).unwrap();
        std::fs::remove_file(root.join(IMPURE_ENV_MEMBER)).unwrap();

        let archive = ReplayArchive::open(&root).unwrap();
        assert!(archive.outcomes().is_empty());
        assert!(archive.impure_env().is_empty());
        assert!(!archive.capabilities().expected_outcomes);
        assert!(!archive.capabilities().impure_env);
        assert_eq!(archive.requests().len(), 4);
    }

    #[test]
    fn v0_malformed_request_line_reports_member_and_line() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);
        let path = root.join(REQUESTS_MEMBER);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"ssh_session_id\":99,\n");
        std::fs::write(&path, text).unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(err.contains(REQUESTS_MEMBER), "got: {err}");
        assert!(err.contains("line 5"), "got: {err}");
    }

    #[test]
    fn v0_negative_offsets_clamp_to_zero() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);
        let path = root.join(REQUESTS_MEMBER);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(
            "{\"ssh_session_id\":14,\"offset_s\":-3.5,\"paths\":[[\"/nix/store/a1111111111111111111111111111111-dep.drv\",[\"out\"]]]}\n",
        );
        std::fs::write(&path, text).unwrap();

        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(archive.requests().len(), 5);
        // Clamped to 0.0 it sorts before the 0.25 request.
        assert_eq!(archive.requests()[0].session, 14);
        assert_eq!(archive.requests()[0].offset_s, 0.0);
    }

    #[test]
    fn v0_unparseable_narinfo_sidecar_is_skipped_not_fatal() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);

        // Break one sidecar. The same corruption in a v1 archive is a hard
        // open error (`unparseable_narinfo_sidecar_is_an_open_error`); the
        // v0 shim only skips the affected path because the recording cannot
        // be regenerated.
        let sidecar = root
            .join(NARINFO_DIR)
            .join("c1111111111111111111111111111111.narinfo");
        let text = std::fs::read_to_string(&sidecar).unwrap();
        let broken = text.replace("NarSize: ", "NarSize: not-a-number-");
        assert_ne!(broken, text, "sidecar has a NarSize line to break");
        std::fs::write(&sidecar, broken).unwrap();

        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(archive.requests().len(), 4);
        assert!(
            archive
                .narinfo("c1111111111111111111111111111111")
                .is_none(),
            "the broken sidecar is skipped"
        );
        assert!(
            archive.narinfo(V0_SRC_PATH).is_some(),
            "the intact sidecar still loads"
        );
    }

    #[test]
    fn v0_cleartext_relay_substituter_is_rejected_on_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);

        // The relay-scheme rule applies to v0 archives too: rewrite the
        // recorded src_substituters entry to plain http://.
        let manifest_path = root.join(MANIFEST_MEMBER);
        let text = std::fs::read_to_string(&manifest_path).unwrap();
        let rewritten = text.replace("https://cache.example.org", "http://cache.example.org");
        assert_ne!(rewritten, text, "manifest has a relay entry to rewrite");
        std::fs::write(&manifest_path, rewritten).unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("only https:// and s3:// are allowed"),
            "got: {err}"
        );
    }

    #[test]
    fn v0_embedded_path_without_sidecar_dumps_unverified() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);
        std::fs::remove_file(
            root.join(NARINFO_DIR)
                .join("b1111111111111111111111111111111.narinfo"),
        )
        .unwrap();

        // v0 has no sidecar-presence rule, so the archive still opens…
        let archive = ReplayArchive::open(&root).unwrap();
        assert!(archive.narinfo(V0_SRC_PATH).is_none());
        // …and the embedded tree still NAR-serializes — unverified, because
        // there is no sidecar to check it against (v1 refuses this at open).
        let nar = archive.dump_nar(V0_SRC_PATH).unwrap();
        assert!(!nar.is_empty());
    }
}

//! Reader for replay archives: open a directory or DwarFS image, validate
//! its integrity, and expose the v1 in-memory model.
//!
//! Metadata (manifest, requests, outcomes, units, closures, impure-env,
//! exclusions, narinfo sidecars, the `nix/store/` entry index) is parsed
//! eagerly at [`ReplayArchive::open`], which also verifies the manifest's
//! `files` digests, the narinfo listing digest, and the embedded-derivation
//! listing digest (see `docs/dev/2026-05-28-build-replay-design.md`,
//! "Identity, integrity, and content addressing"). Derivation text and NAR
//! payloads are read lazily; embedded store paths are verified against
//! their narinfo sidecars when they are NAR-serialized
//! ([`ReplayArchive::dump_nar`]), not at open time.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result, anyhow, ensure};
use rio_nix::narinfo::NarInfo;

use super::backend::{Backend, WalkEntry};
use super::schema::{
    Capabilities, Capability, ClosureRecord, Counts, ExclusionRecord, ExpectedOutcome, ImpureEnv,
    Manifest, MemberPresence, OutcomeRecord, OutputHash, RequestRecord, SessionKey, Substituters,
    UnitRecord,
};
use super::{
    CLOSURES_MEMBER, EXCLUSIONS_MEMBER, IMPURE_ENV_MEMBER, MANIFEST_MEMBER, METADATA_MEMBERS,
    OUTCOMES_MEMBER, REQUESTS_MEMBER, STORE_DIR, UNITS_MEMBER,
};
use super::{RecordPolicy, identity, schema, v0};

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
    /// Truth records, drv → per-session map. The inner `BTreeMap` keeps the
    /// session-less record (`None`) first and scoped sessions ascending,
    /// which is exactly the order the typed lookups walk.
    outcomes: HashMap<String, BTreeMap<Option<i64>, OutcomeRecord>>,
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

        // Conformant v1 archives cannot carry a screen-rejected relay entry
        // (the writer refuses them at finalize), so for archives produced by
        // a foreign recorder the entry is warned about here and judged per
        // entry at use — open never fails on a list entry, the same
        // write-once-stays-openable rule the bootstrap classification
        // documents.
        warn_unusable_relay_substituters(&manifest.substituters);

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
        let workload_units = normalize_requests(&mut requests, RecordPolicy::Strict)?;

        // Expected outcomes keyed by (session, drv); duplicate keys keep the
        // last record (a re-recorded outcome supersedes the earlier one).
        let mut outcomes: HashMap<String, BTreeMap<Option<i64>, OutcomeRecord>> = HashMap::new();
        let mut outcome_records: u64 = 0;
        if let Some(bytes) = staged.get(OUTCOMES_MEMBER) {
            for record in super::parse_jsonl::<OutcomeRecord>(bytes, OUTCOMES_MEMBER)? {
                outcome_records += 1;
                outcomes
                    .entry(record.drv.clone())
                    .or_default()
                    .insert(record.session, record);
            }
        }
        // Capability gate, enforced where the data enters: per-output NAR
        // hashes are usable only under the `output_hashes` flag (the
        // recorder's claim that it vouches for them — design doc
        // "Capabilities" table: the flag gates output-divergence verdicts).
        // Withholding them HERE means no consumer can mint divergence
        // verdicts from unvouched hashes by forgetting to ask; first-party
        // producers are flag-consistent, so this only bites foreign v1
        // archives, which get the warning.
        if !manifest.capabilities.output_hashes {
            let mut withheld = 0usize;
            for record in outcomes
                .values_mut()
                .flat_map(|by_session| by_session.values_mut())
            {
                if !record.outputs.is_empty() {
                    record.outputs.clear();
                    withheld += 1;
                }
            }
            if withheld > 0 {
                tracing::warn!(
                    records = withheld,
                    "outcome records carry per-output hashes but capability `output_hashes` \
                     is not set; the hashes are withheld — output-divergence verdicts gate \
                     on the flag"
                );
            }
        }

        // Per-unit metadata; records for derivations outside the workload are
        // dropped with a warning (they cannot be scheduled, so keeping them
        // would only mislead filters and reports). Duplicate records for one
        // derivation keep the last — the same supersession rule as duplicate
        // outcome keys — but loudly: aliased source jobs (two attr paths
        // evaluating to one derivation, a routine Hydra shape) arrive as two
        // records here, and the shadowed label leaves the campaign's
        // reporting entirely.
        // TODO: the shadowed label is absent from results AND exclusions, so
        // scope-item completeness accounting cannot name it; making the
        // accounting total needs the recorder to emit an alias exclusion
        // record for every shadowed label at archive-creation time.
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
                if let Some(shadowed) = units.insert(record.drv.clone(), record) {
                    let kept = &units[&shadowed.drv];
                    tracing::warn!(
                        drv = %shadowed.drv,
                        kept_label = ?kept.label,
                        shadowed_label = ?shadowed.label,
                        "duplicate {UNITS_MEMBER} records for one workload derivation; keeping \
                         the later record — the shadowed label will not appear in campaign \
                         results or exclusions"
                    );
                }
            }
        }

        let closures: Vec<ClosureRecord> = match staged.get(CLOSURES_MEMBER) {
            Some(bytes) => super::parse_jsonl(bytes, CLOSURES_MEMBER)?,
            None => Vec::new(),
        };

        // Same gate shape for the impure-env map: the member is parsed (a
        // malformed member is a malformed archive whatever the flags say)
        // but exposed to consumers only under the `impure_env` flag, so
        // the impure-demotion decision (workload_set) cannot fire on data
        // the recorder did not claim. The member-present/flag-false case
        // is warned about in the capability loop below.
        let impure_env: ImpureEnv = match staged.get(IMPURE_ENV_MEMBER) {
            Some(bytes) => {
                let parsed: ImpureEnv = serde_json::from_slice(bytes)
                    .with_context(|| format!("malformed {IMPURE_ENV_MEMBER}"))?;
                if manifest.capabilities.impure_env {
                    parsed
                } else {
                    ImpureEnv::new()
                }
            }
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

        // THE shared sidecar enumeration (`super::index_narinfos`) — the
        // same function the writer's finalize validates a staging tree
        // with, so the two ends of the sidecar contract cannot drift.
        let sidecars = super::index_narinfos(&backend, RecordPolicy::Strict)?;
        let store_entries = index_store_entries(&backend, RecordPolicy::Strict)?;

        // Open-time content-digest verification, one decision surface per
        // field (`ContentDigests::verify_at_open`):
        // - narinfo: the sidecar listing covers the load-bearing
        //   References/NarHash lines; sidecars are small, so the listing is
        //   recomputed here.
        // - drvs: the embedded-derivation listing is recomputed here too —
        //   .drv members are small ATerm text (the same argument as the
        //   sidecars), and NOTHING downstream re-checks them: the import
        //   path reads the bytes raw, derives the upload path-info FROM
        //   them, and registers them under the recorded store path, so a
        //   corrupted-but-parseable ATerm would replay a semantically
        //   different derivation with the divergence charged to the wrong
        //   side. (An input-addressed .drv path does commit to its ATerm,
        //   but no import-time check ever recomputes that commitment.)
        //   Recomputation also catches membership damage — a .drv member
        //   removed or added after finalize changes the listing.
        // - embedded store paths: deliberately NOT recomputed at open
        //   (that would NAR-serialize every embedded tree); each path is
        //   verified against its sidecar when its bytes are produced for
        //   upload (dump_nar).
        let mut drv_digests: Vec<(String, String)> = Vec::new();
        for entry in store_entries.values() {
            if !entry.name.ends_with(".drv") {
                continue;
            }
            let rel = format!("{STORE_DIR}/{}", entry.name);
            let bytes = backend
                .read_file(&rel)?
                .ok_or_else(|| anyhow!("{rel}: listed in the archive index but unreadable"))?;
            drv_digests.push((
                format!("{}{}", rio_nix::store_path::STORE_PREFIX, entry.name),
                identity::sha256_hex(&bytes),
            ));
        }
        manifest.content_digests.verify_at_open(
            &identity::listing_digest(&drv_digests),
            &identity::listing_digest(&sidecars.digests),
        )?;

        // Capability flags must be backed by the data they claim. The
        // reverse direction (member present, flag false) is only a warning
        // — but the data is then GATED, not used: impure-env and embedded
        // store paths are withheld by this reader (above/`has_embedded`),
        // outcome hashes are stripped above, and the remaining members'
        // consumers gate on the flag themselves
        // (`run/truth.rs::expected_outcomes_for_units` for outcomes,
        // `run/supply.rs::walk_closure` / `run/archive_input.rs::
        // load_closures` for closures). An archive must stay openable no
        // matter what an honest-but-conservative recorder declined to
        // claim.
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
                    "archive carries {staged_what} but capability `{flag}` is not set; every \
                     consumer of the claim this flag vouches for gates on it, so the data is \
                     withheld from those uses (a member shared by several claims — e.g. \
                     outcomes.jsonl under `timed` — is still usable under the claims the \
                     archive does make)"
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
                sidecars.by_hash.contains_key(super::hash_part(&entry.name)),
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
            narinfos: sidecars.by_hash,
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
    /// consistent by construction). The recorded `src_substituters` list
    /// maps to the relay list verbatim — including entries the admission
    /// screen rejects (the original v0 consumer accepted plain http
    /// caches). Such entries classify `Unusable` per entry at the campaign
    /// bootstrap and are skipped at use; open only warns about them,
    /// because a v0 recording of a past production window is irreplaceable
    /// and must never become unreadable over a list entry. Units,
    /// closures, and exclusions do not exist in v0 and load as empty.
    ///
    /// The same never-refuse-over-one-record posture governs every
    /// per-record validity check on this path via
    /// [`RecordPolicy::WarnAndSkip`]: defective narinfo sidecars,
    /// store members colliding on hash part, and empty-target request
    /// records (all of which the v1 writer would have refused to stage,
    /// and v0's external recorder never screened) cost only themselves. Structural corruption stays a loud open
    /// error, deliberately: a malformed JSONL line or member document is
    /// refused with member + line, exactly as the original v0 consumer
    /// refused it — no recording that ever worked becomes unreadable —
    /// and warn-skipping a torn truth (`builds.jsonl`) record would
    /// silently flip parity verdicts from "expected outcome" to "no
    /// recorded truth", trading a loud, nameable refusal for wrong
    /// answers.
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
        let workload_units = normalize_requests(&mut requests, RecordPolicy::WarnAndSkip)?;

        // builds.jsonl is the v0 truth member; each record maps into the
        // neutral outcome vocabulary, keyed like v1 with the last record
        // winning (a re-recorded outcome supersedes the earlier one).
        let builds_bytes = backend.read_file(v0::V0_BUILDS_MEMBER)?;
        let has_builds = builds_bytes.is_some();
        let mut outcomes: HashMap<String, BTreeMap<Option<i64>, OutcomeRecord>> = HashMap::new();
        if let Some(bytes) = &builds_bytes {
            for record in super::parse_jsonl::<v0::V0BuildRecord>(bytes, v0::V0_BUILDS_MEMBER)? {
                let mapped = v0::map_build_record(record);
                outcomes
                    .entry(mapped.drv.clone())
                    .or_default()
                    .insert(mapped.session, mapped);
            }
        }

        let impure_env_bytes = backend.read_file(IMPURE_ENV_MEMBER)?;
        let has_impure_env = impure_env_bytes.is_some();
        let impure_env: ImpureEnv = match &impure_env_bytes {
            Some(bytes) => serde_json::from_slice(bytes)
                .with_context(|| format!("malformed {IMPURE_ENV_MEMBER}"))?,
            None => ImpureEnv::new(),
        };

        // Sidecars and the store index are read exactly as for v1 (the
        // shared `super::index_narinfos`, including the URL-synthesis
        // fallback); only the defective-record policy differs — see
        // [`RecordPolicy`]. The v1 narinfo listing digest has
        // nothing to be checked against, so the recomputed digests are
        // dropped.
        let sidecars = super::index_narinfos(&backend, RecordPolicy::WarnAndSkip)?;
        let store_entries = index_store_entries(&backend, RecordPolicy::WarnAndSkip)?;

        let output_hashes_present = outcomes
            .values()
            .flat_map(|by_session| by_session.values())
            .any(|record| !record.outputs.is_empty());
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
        manifest.counts.expected_outcomes = outcomes
            .values()
            .map(|by_session| by_session.len() as u64)
            .sum();

        warn_unusable_relay_substituters(&manifest.substituters);

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
            narinfos: sidecars.by_hash,
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

    /// Every expected-outcome record (order unspecified; one record per
    /// distinct `(session, drv)` key, duplicates keep the last). Empty when
    /// the archive has no truth member.
    pub fn outcome_records(&self) -> impl Iterator<Item = &OutcomeRecord> {
        self.outcomes
            .values()
            .flat_map(|by_session| by_session.values())
    }

    /// Session-resolved truth lookup. For a key minted from a recorded
    /// request, the order is exact `(session, drv)`, then the session-less
    /// `(null, drv)` record; [`SessionKey::SESSIONLESS`] resolves only the
    /// session-less record.
    pub fn expected_outcome(&self, session: SessionKey, drv: &str) -> Option<&OutcomeRecord> {
        let by_session = self.outcomes.get(drv)?;
        match session.recorded() {
            Some(session) => by_session
                .get(&Some(session))
                .or_else(|| by_session.get(&None)),
            None => by_session.get(&None),
        }
    }

    /// THE canonical collapse over sessions, for consumers that resolve
    /// truth by derivation alone (the timeless engine has one truth slot
    /// per workload unit, whatever sessions requested it).
    ///
    /// Rule: the session-less record when one exists (the format gives it
    /// authority — it explicitly applies to any request of the unit);
    /// otherwise the scoped record of the highest informativeness rank
    /// (`informativeness_rank` below — the design doc's expected-outcome
    /// vocabulary carries the same table), with ties WITHIN a rank class
    /// resolved by record content (the `consumed_truth` ordering:
    /// outcome, then recorded output hashes) and the highest-numbered
    /// session deciding only between records whose consumed truth is
    /// identical.
    ///
    /// Session ids must never decide consumed truth — between rank
    /// classes or within one: they are opaque grouping keys whose
    /// allocation order carries no truth ordering (the committed v0
    /// production fixture allocates them out of capture order), and for
    /// overlapping sessions even capture order says nothing — the earlier
    /// session's build can complete after the later session's disconnect.
    /// A `built` record discarded in favor of a concurrent `disconnected`
    /// one would silently drop a truly-built unit from the parity
    /// denominator — and two `built` records with conflicting hashes
    /// resolved by session id would flip the unit's NAR-comparison
    /// baseline — with the choice flipping under session relabeling.
    ///
    /// Scoped records whose consumed truth disagrees with the chosen
    /// record's are logged here, and the per-unit conflict count is
    /// surfaced in the report's comparability block
    /// ([`ReplayArchive::truth_collapse_conflicts`]) — the collapse is
    /// losing information the timeless engine cannot represent, and the
    /// operator must be able to see how much.
    pub fn expected_outcome_across_sessions(&self, drv: &str) -> Option<&OutcomeRecord> {
        let by_session = self.outcomes.get(drv)?;
        if let Some(record) = by_session.get(&None) {
            return Some(record);
        }
        let record = collapse_scoped(by_session)?;
        let disagreeing: Vec<i64> = by_session
            .iter()
            .filter(|(_, other)| consumed_truth(other) != consumed_truth(record))
            .filter_map(|(session, _)| *session)
            .collect();
        if !disagreeing.is_empty() {
            tracing::warn!(
                drv,
                chosen_session = record.session.unwrap_or_default(),
                chosen_outcome = record.outcome.as_str(),
                ?disagreeing,
                "collapsing session-scoped truth over sessions whose records disagree on the \
                 consumed truth (outcome or recorded output hashes); the unit counts as a \
                 truth-collapse conflict in the comparability block"
            );
        }
        Some(record)
    }

    /// Workload units whose session-scoped truth records disagree on the
    /// CONSUMED truth — the outcome enum or the recorded output hashes
    /// (`consumed_truth`) — with no session-less record to supersede
    /// them: exactly the units whose one truth slot the scoped collapse
    /// had to resolve by rank or content order, discarding recorded
    /// information. Surfaced in the report's comparability block so
    /// collapse-resolved truth is visible next to the headline it shapes,
    /// not only in engine logs. Comparing the projection (not just the
    /// outcome enum) is what makes a hash-conflicted pair of `built`
    /// records count: those conflicts silently flip the unit's
    /// NAR-comparison baseline, the one collapse axis a bare outcome
    /// comparison cannot see.
    ///
    /// Gated on the `expected_outcomes` capability INSIDE the owner, like
    /// every other outcomes-as-truth consumer (`Capability::gates()`:
    /// the flag claims "verdict comparison"): `None` when the claim is
    /// withdrawn — the metric's documented meaning is "the units whose
    /// one truth slot the cross-session collapse RESOLVED", and under a
    /// withdrawn claim no truth is resolved at all (every unit's truth is
    /// Unknown), so any count would assert a measurement that never
    /// happened. Stamping `None` keeps the comparability field honest:
    /// not measured, rather than a fabricated `Some` over staged records
    /// the recorder declined to vouch for.
    ///
    /// A unit whose scoped records disagree UNDER a session-less record
    /// does not count: the format gives the session-less form authority
    /// over every request of the unit, so that resolution is supersession
    /// by contract, not an information-losing pick.
    pub fn truth_collapse_conflicts(&self) -> Option<usize> {
        if !Capability::ExpectedOutcomes.enabled_in(self.capabilities()) {
            return None;
        }
        Some(
            self.workload_units
                .iter()
                .filter(|drv| {
                    let Some(by_session) = self.outcomes.get(*drv) else {
                        return false;
                    };
                    if by_session.contains_key(&None) {
                        return false;
                    }
                    let mut truths = by_session.values().map(consumed_truth);
                    let first = truths.next();
                    truths.any(|truth| Some(truth) != first)
                })
                .count(),
        )
    }

    /// Per-unit metadata keyed by drv path (empty when units.jsonl absent).
    /// Duplicate records for one derivation kept the last at open time —
    /// the supersession rule shared with duplicate outcome keys — with a
    /// warning naming the shadowed label.
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

    /// True if the archive embeds this store path's contents (a non-drv
    /// tree) AND declares the `embedded_store_paths` capability — the flag
    /// gates the archive rung of the supply ladder (design doc
    /// "Capabilities" table), and the gate is enforced here so no supply
    /// consumer can plan uploads from trees the recorder did not claim.
    /// First-party producers are flag-consistent (the writer refuses
    /// unclaimed members; v0 infers flags from presence), so the
    /// withholding only bites foreign v1 archives, which get an open-time
    /// warning.
    pub fn has_embedded(&self, store_path: &str) -> bool {
        Capability::EmbeddedStorePaths.enabled_in(&self.manifest.capabilities)
            && self
                .store_entries
                .get(super::hash_part(store_path))
                .is_some_and(|entry| !entry.name.ends_with(".drv"))
    }

    /// Embedded non-drv store paths (full /nix/store/... paths, sorted).
    /// Empty when the archive does not declare `embedded_store_paths` —
    /// same gate as [`Self::has_embedded`].
    pub fn embedded_store_paths(&self) -> Vec<String> {
        if !Capability::EmbeddedStorePaths.enabled_in(&self.manifest.capabilities) {
            return Vec::new();
        }
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
                let sidecar_hex = crate::narhash::NarHash::parse(&narinfo.nar_hash)
                    .with_context(|| format!("narinfo sidecar for {store_path}"))?
                    .to_hex();
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

/// Informativeness rank of one truth record for the timeless collapse —
/// higher ranks carry strictly more usable information for a timeless
/// outcome comparison and are kept over lower ones. The table (mirrored in
/// the design doc's expected-outcome vocabulary, where the rank order
/// itself is open to dispute as policy):
///
/// | rank | records |
/// |---|---|
/// | 4 | `built` with recorded output hashes (drives NAR comparison) |
/// | 3 | `built` without hashes (a deterministic success claim) |
/// | 2 | `failed`, `resource-exhausted` (deterministic failure claims) |
/// | 1 | `cancelled`, `disconnected`, `indeterminate` (interruption/infra: no claim about the build) |
/// | 0 | `unknown` (the recorder looked and could not decide) |
///
/// Exhaustive on purpose: a new outcome variant refuses to compile until
/// its rank — whether the timeless collapse may discard it — is decided
/// here.
fn informativeness_rank(record: &OutcomeRecord) -> u8 {
    match record.outcome {
        ExpectedOutcome::Built => {
            if record.outputs.is_empty() {
                3
            } else {
                4
            }
        }
        ExpectedOutcome::Failed | ExpectedOutcome::ResourceExhausted => 2,
        ExpectedOutcome::Cancelled
        | ExpectedOutcome::Disconnected
        | ExpectedOutcome::Indeterminate => 1,
        ExpectedOutcome::Unknown => 0,
    }
}

/// The consumed-truth projection of one outcome record: exactly the
/// fields the timeless collapse's consumer reads off the chosen record
/// (`run::truth::expected_outcomes_for_units` copies `outcome` and
/// `outputs` into the comparison baseline; nothing else of a collapsed
/// record is consumed). This projection is the ONE unit of equality for
/// the collapse tiebreak, the disagreement warn, and the
/// `truthCollapseConflicts` count, so the disclosure granularity
/// structurally cannot lag the truth granularity again (round 3 added
/// outcome-level disclosure while rank-4 hash payloads stayed
/// session-decided and undisclosed).
///
/// Every field is destructured, so a NEW `OutcomeRecord` field refuses
/// to compile until it is classified truth-bearing (joins the
/// projection) or not:
/// - `session`: the relabeling axis itself — an opaque grouping label,
///   never truth.
/// - `drv`: the unit's identity, constant across one collapse input by
///   construction (the outcomes map is keyed by it).
/// - `detail`: free-form human-readable text, "Never interpreted" per
///   the schema.
/// - `duration_s` / `stop_offset_s`: per-attempt timing measurements,
///   expected to vary across sessions; consumed only by the
///   session-aware timed lookup (`expected_outcome` via `SessionKey`),
///   never off a collapsed record.
/// - `outcome` + `outputs`: the consumed truth.
///
/// The projection's `Ord` (outcome wire string, then the outputs map —
/// `OutputHash` orders by digest bytes then size) is the within-rank
/// tiebreak: arbitrary but total and derived from record CONTENT, so
/// the pick cannot flip under session relabeling.
fn consumed_truth(record: &OutcomeRecord) -> (&'static str, &BTreeMap<String, OutputHash>) {
    let OutcomeRecord {
        session: _,
        drv: _,
        outcome,
        detail: _,
        duration_s: _,
        stop_offset_s: _,
        outputs,
    } = record;
    (outcome.as_str(), outputs)
}

/// The scoped half of the collapse rule: the record of the highest
/// [`informativeness_rank`]; ties within a rank class resolve by the
/// records' consumed-truth content ([`consumed_truth`]'s ordering), and
/// the highest session decides only between records whose consumed
/// truth is identical (`BTreeMap` iterates sessions ascending and
/// `max_by` keeps the LAST maximum). Callers resolve the session-less
/// record first; this function only ever sees scoped disagreement.
///
/// The CONSUMED truth of the choice — not merely its rank class — is
/// therefore invariant under any relabeling of the session axis:
/// session ids only ever pick between records the consumer cannot tell
/// apart. Two rank-4 `built` records with conflicting hashes resolve to
/// the content-greater record under either labeling; content order is
/// arbitrary-but-total tiebreak policy, not a truth judgment, so the
/// conflict is never silent — it is disclosed through the warn above
/// and counted by [`ReplayArchive::truth_collapse_conflicts`].
fn collapse_scoped(by_session: &BTreeMap<Option<i64>, OutcomeRecord>) -> Option<&OutcomeRecord> {
    by_session.values().max_by(|a, b| {
        informativeness_rank(a)
            .cmp(&informativeness_rank(b))
            .then_with(|| consumed_truth(a).cmp(&consumed_truth(b)))
    })
}

/// Surface screen-rejected relay entries at open time, without judging
/// them: judgment over archive list entries belongs to
/// [`crate::nixcache::classify_substituter`] — total, per entry, at the
/// campaign bootstrap — so an archive stays openable no matter what its
/// recorded lists carry. Consumers skip `Unusable` entries; an error
/// surfaces only at a point of use with no usable alternative. The warning
/// here just gives list/inspect/dry-run callers (which never reach the
/// bootstrap classification) the same early signal.
fn warn_unusable_relay_substituters(substituters: &Substituters) {
    for entry in &substituters.relay {
        if let crate::nixcache::ArchiveSubstituterUrl::Unusable { url, reason } =
            crate::nixcache::classify_substituter(entry)
        {
            tracing::warn!(
                url = %url,
                reason = %reason,
                "archive relay substituter entry is unusable; campaign-time consumers \
                 will skip it"
            );
        }
    }
}

/// Index `nix/store/`: hash part → entry. Drives the store-path keyed
/// lookups; contents stay in the backend until asked for.
///
/// Two members colliding on hash part cannot both be served — this map
/// and every lookup through it key on the hash part — and real store
/// paths derive the hash from the name, so a collision proves the
/// archive was mis-assembled. It is handled per `policy`, exactly like
/// a defective narinfo sidecar: refused at open for v1, where silent
/// last-wins indexing would otherwise serve whichever member the
/// backend listed last (a vanished supply rung, or the wrong ATerm,
/// surfacing mid-campaign); warn-and-keep-the-first-in-name-order for
/// v0, whose irreplaceable recordings must stay openable — the shadowed
/// member was unreachable through this map either way, the skip just
/// makes it loud.
fn index_store_entries(
    backend: &Backend,
    policy: RecordPolicy,
) -> Result<HashMap<String, WalkEntry>> {
    index_store_listing(backend.list_dir(STORE_DIR)?.unwrap_or_default(), policy)
}

/// The fold behind [`index_store_entries`], over the raw backend
/// listing. Entries are sorted by name BEFORE the fold — the same
/// determinism close as the sibling enumeration `index_narinfos`
/// (archive/mod.rs): backends promise no entry order (fs::read_dir and
/// DwarFS image order are filesystem-state accidents; `backend.rs`'s
/// parity test sorts before comparing for exactly that reason), so an
/// unsorted fold would let the v0 first-wins choice — which colliding
/// member serves `read_drv`/`dump_nar`/`has_embedded` — and the Strict
/// error's named pair re-roll across copies and forms of the same
/// recording. Split from the backend call so the listing-order
/// invariance test can inject permutations directly.
fn index_store_listing(
    mut entries: Vec<WalkEntry>,
    policy: RecordPolicy,
) -> Result<HashMap<String, WalkEntry>> {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let mut store_entries: HashMap<String, WalkEntry> = HashMap::new();
    for entry in entries {
        match store_entries.entry(super::hash_part(&entry.name).to_string()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(entry);
            }
            std::collections::hash_map::Entry::Occupied(slot) => {
                let collision = anyhow!(
                    "{STORE_DIR}/ members {} and {} collide on store hash part {} — lookups key \
                     on the hash part, so one member would silently shadow the other (the \
                     archive was mis-assembled)",
                    slot.get().name,
                    entry.name,
                    slot.key(),
                );
                match policy {
                    RecordPolicy::Strict => return Err(collision),
                    RecordPolicy::WarnAndSkip => {
                        tracing::warn!("skipping colliding store member: {collision:#}");
                    }
                }
            }
        }
    }
    Ok(store_entries)
}

/// Request post-processing shared by both open paths: every request must
/// name at least one target (per `policy` — the v1 writer stages only
/// non-empty targets, so a v1 reader hit is tamper detection; v0's
/// producer never enforced the rule, and an empty request schedules
/// nothing, so the record is dropped with a warning), negative offsets
/// are clamped to 0 (clock skew at capture time can produce slightly
/// negative values; rejecting the whole archive for that would be
/// overkill), `[]` output lists are normalized to `["*"]` (both spellings
/// mean "all outputs"), and the records are sorted by offset ascending
/// (recorded lines are not guaranteed globally ordered — per-session
/// buffers get flushed independently — and the replay timeline wants
/// ascending offsets). Returns the distinct workload-unit drv paths.
fn normalize_requests(
    requests: &mut Vec<RequestRecord>,
    policy: RecordPolicy,
) -> Result<BTreeSet<String>> {
    match policy {
        RecordPolicy::Strict => {
            for record in requests.iter() {
                ensure!(
                    !record.targets.is_empty(),
                    "{REQUESTS_MEMBER}: request record (session {}) must have non-empty targets",
                    record.session
                );
            }
        }
        RecordPolicy::WarnAndSkip => {
            requests.retain(|record| {
                if record.targets.is_empty() {
                    tracing::warn!(
                        session = record.session,
                        "skipping {REQUESTS_MEMBER} record with no targets (an empty request \
                         schedules nothing)"
                    );
                    return false;
                }
                true
            });
        }
    }
    for record in requests.iter_mut() {
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
    use crate::archive::{FORMAT_VERSION, NARINFO_DIR, hash_part};

    /// Stage and finalize the tiny archive in `dir`, returning the staged
    /// root and the writer's view of the identity.
    fn staged_tiny_archive(dir: &Path) -> (PathBuf, crate::archive::writer::FinalizedArchive) {
        let root = dir.join("archive");
        let finalized = tiny_archive(&root);
        (root, finalized)
    }

    /// The session key of the archive's recorded request on `session` —
    /// how every session-scoped probe is minted (a `SessionKey` is only
    /// constructible from a recorded request, so tests resolve sessions
    /// the way the engine does: from the requests that exist).
    fn key_of_session(archive: &ReplayArchive, session: i64) -> SessionKey {
        SessionKey::of_request(
            archive
                .requests()
                .iter()
                .find(|record| record.session == session)
                .unwrap_or_else(|| panic!("fixture records a request on session {session}")),
        )
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

    /// A write-once archive whose substituter lists carry entries the
    /// engine's admission screen rejects must still OPEN: neither list is
    /// scheme-checked at open (the lists are recorder input, and the only
    /// producer-side rule is the v1 writer's finalize refusal of
    /// non-https/s3 relay entries), and rejection is the campaign
    /// bootstrap's per-entry classification — which skips to the next
    /// usable entry instead of refusing the archive.
    #[test]
    fn archive_with_unusable_substituter_entries_opens_and_classifies() {
        use crate::archive::writer::test_support::{stage_tiny_archive, tiny_seed};

        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        let writer = crate::archive::writer::ArchiveWriter::create(&root).unwrap();
        let src_tree = tempfile::TempDir::new().unwrap();
        stage_tiny_archive(&writer, &src_tree.path().join("src"));
        let mut seed = tiny_seed();
        // Production nix.conf shapes the screen rejects (internal http,
        // ssh) ahead of a perfectly usable https entry; the relay keeps an
        // s3 entry (format-valid) before the https one.
        seed.substituters.target = vec![
            "http://internal-cache:8080".to_string(),
            "ssh://build-cache.internal".to_string(),
        ];
        seed.substituters.relay = vec![
            "s3://team-bucket".to_string(),
            "https://cache.nixos.org".to_string(),
        ];
        writer.finalize(seed).unwrap();

        let archive = ReplayArchive::open(&root).expect("rejected entries must not block open");
        let classified =
            crate::nixcache::ClassifiedSubstituters::classify(&archive.manifest().substituters);
        assert!(
            classified.target.iter().all(|entry| matches!(
                entry,
                crate::nixcache::ArchiveSubstituterUrl::Unusable { .. }
            )),
            "both target entries fail the screen"
        );
        let probe = classified
            .first_probeable()
            .expect("the https relay entry is selected past the unusable/s3 ones");
        assert_eq!(probe.base().as_str(), "https://cache.nixos.org/");
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

        // Session 1's request asked for dep.drv but no scoped record
        // exists for it: the lookup falls back to the session-less truth
        // record.
        let dep = archive
            .expected_outcome(key_of_session(&archive, 1), DEP_DRV)
            .unwrap();
        assert_eq!(dep.outcome, crate::archive::schema::ExpectedOutcome::Built);
        // The explicit session-less identity resolves the same record.
        assert_eq!(
            archive
                .expected_outcome(SessionKey::SESSIONLESS, DEP_DRV)
                .unwrap()
                .outcome,
            dep.outcome
        );
        // Session 0's app.drv expectation is session-scoped.
        let app = archive
            .expected_outcome(key_of_session(&archive, 0), APP_DRV)
            .unwrap();
        assert_eq!(app.outcome, crate::archive::schema::ExpectedOutcome::Failed);
        assert_eq!(app.session, Some(0));
        // ... so the session-less identity does not see it.
        assert!(
            archive
                .expected_outcome(SessionKey::SESSIONLESS, APP_DRV)
                .is_none()
        );

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
        assert_eq!(
            from_dir.outcome_records().count(),
            from_image.outcome_records().count()
        );
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

    /// The embedded-derivation listing digest is recomputed at open, so a
    /// `.drv` member whose bytes changed after finalize refuses the
    /// archive — the check the import path depends on, because nothing
    /// downstream re-derives the .drv path↔ATerm commitment: the importer
    /// reads the embedded bytes raw and registers them under the recorded
    /// store path (`run/drv_import.rs`), so a corrupted-but-parseable
    /// ATerm would otherwise replay a semantically different derivation
    /// charged to the wrong side. Contract: design doc "Identity,
    /// integrity, and content addressing" — `content_digests.drvs` is
    /// checked when the archive is opened. The tampered ATerm here stays
    /// parseable on purpose (a changed builder argument): the layers that
    /// do exist (UTF-8 check, ATerm parse) cannot catch it, only the
    /// digest can.
    #[test]
    fn drv_listing_digest_mismatch_is_detected() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // nix/store members are not covered by manifest.files (only
        // metadata members are), so this edit reaches exactly the drvs
        // listing digest — there is no other line of defense at open.
        let member = format!("{STORE_DIR}/{}", DEP_DRV.rsplit('/').next().unwrap());
        rewrite_member(&root, &member, "cp -r $src $out", "cp -r $src $oux");

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("embedded-derivation listing digest mismatch"),
            "got: {err}"
        );
    }

    /// Membership damage is caught by the same recomputation: a `.drv`
    /// member REMOVED after finalize (post-finalize member loss, the
    /// absence half of the integrity story) and a member ADDED (a
    /// mis-assembled archive) both change the recomputed listing.
    #[test]
    fn drv_member_absence_and_addition_are_open_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());
        let dep_member = root
            .join(STORE_DIR)
            .join(DEP_DRV.rsplit('/').next().unwrap());

        // Removed member: the recomputed listing is missing a line.
        let dep_bytes = std::fs::read(&dep_member).unwrap();
        std::fs::remove_file(&dep_member).unwrap();
        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("embedded-derivation listing digest mismatch"),
            "got: {err}"
        );

        // Restored + an extra member finalize never saw: an extra line.
        std::fs::write(&dep_member, &dep_bytes).unwrap();
        std::fs::write(
            root.join(STORE_DIR)
                .join("e9999999999999999999999999999999-extra.drv"),
            "Derive([],[],[],\"x86_64-linux\",\"/bin/sh\",[],[])\n",
        )
        .unwrap();
        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains("embedded-derivation listing digest mismatch"),
            "got: {err}"
        );
    }

    /// Stage every member the capability vocabulary can claim and claim
    /// all six flags: the tiny archive plus an impure-env entry for `app`
    /// and adjacency records carrying a sentinel input source the ATerms
    /// do NOT declare (so the adjacency-vs-ATerm construction choice is
    /// observable).
    fn full_capability_archive(root: &Path) {
        use crate::archive::schema::{Capabilities, ClosureRecord};
        use crate::archive::writer::ArchiveWriter;
        use crate::archive::writer::test_support::{
            APP_DRV, APP_OUT, DEP_DRV, DEP_OUT, SRC_PATH, stage_tiny_archive, tiny_seed,
        };
        use std::collections::BTreeMap;

        let writer = ArchiveWriter::create(root).unwrap();
        let src_tree = tempfile::TempDir::new().unwrap();
        stage_tiny_archive(&writer, &src_tree.path().join("src"));
        writer
            .write_impure_env(&BTreeMap::from([(
                APP_DRV.to_string(),
                vec!["NIX_FOO".to_string()],
            )]))
            .unwrap();
        writer
            .write_closures(&[
                ClosureRecord {
                    drv: DEP_DRV.to_string(),
                    inputs: Vec::new(),
                    srcs: vec![SRC_PATH.to_string(), SENTINEL_SRC.to_string()],
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
        let mut seed = tiny_seed();
        seed.capabilities = Capabilities {
            timed: true,
            expected_outcomes: true,
            output_hashes: true,
            embedded_store_paths: true,
            impure_env: true,
            dependency_closures: true,
        };
        writer.finalize(seed).unwrap();
    }

    /// Input source present only in the adjacency records, never in the
    /// ATerms — the observable difference between the two closure
    /// constructions.
    const SENTINEL_SRC: &str = "/nix/store/h1111111111111111111111111111111-sentinel-src";

    /// Flip one capability flag to false in a finalized archive's
    /// manifest. The manifest member is not covered by `files`, so the
    /// edited archive opens — this is exactly the foreign-producer shape
    /// the open-time leniency exists for: data staged, claim withdrawn,
    /// open warns and admits, the engine gates.
    fn set_capability_false(root: &Path, flag: &str) {
        let manifest_path = root.join(MANIFEST_MEMBER);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["capabilities"][flag] = serde_json::Value::Bool(false);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    /// Behavioral flip test over the WHOLE capability vocabulary
    /// (quantification domain: `Capability::ALL` — the same closed enum
    /// the published gates table renders from): for each flag, open the
    /// same fully-staged archive bytes with the flag claimed and with it
    /// withdrawn, and assert the behavior the `gates()` table documents
    /// observably changes. The match is exhaustive on purpose: a new
    /// `Capability` variant refuses to compile here until its gate delta
    /// is asserted, so the table can no longer truthfully render a gate
    /// no code has.
    ///
    /// Both directions per flag (must-admit AND must-withhold): the
    /// flag-true baseline proves the data flows, the flag-false twin
    /// proves the gate holds — so the discriminator is the flag, not the
    /// staged data. Contract per arm: the `Capability::gates()` row
    /// (pinned verbatim into the design doc's Capabilities table).
    #[test]
    fn capability_flags_gate_their_documented_engine_behavior() {
        use crate::archive::schema::Capability;
        use crate::archive::writer::test_support::{APP_DRV, SRC_PATH};
        use crate::run::{archive_input, supply, truth};

        let dep_job = "pkgs.dep.x86_64-linux";
        for capability in Capability::ALL {
            let base_dir = tempfile::TempDir::new().unwrap();
            full_capability_archive(base_dir.path());
            let baseline = ReplayArchive::open(base_dir.path()).unwrap();

            let gated_dir = tempfile::TempDir::new().unwrap();
            full_capability_archive(gated_dir.path());
            set_capability_false(gated_dir.path(), capability.flag());
            let gated = ReplayArchive::open(gated_dir.path())
                .unwrap_or_else(|e| panic!("flag-false archives stay openable ({e:#})"));

            match capability {
                // Gates the timed scheduling mode: the engine's legality
                // check refuses a flag-less archive.
                Capability::Timed => {
                    crate::run::ensure_timed_capability(&baseline).unwrap();
                    let err = crate::run::ensure_timed_capability(&gated)
                        .unwrap_err()
                        .to_string();
                    assert!(err.contains("timed capability"), "{err}");
                }
                // Gates verdict comparison: without the flag every unit's
                // truth is Unknown (a load/exercise run).
                Capability::ExpectedOutcomes => {
                    let units = archive_input::load_units(&baseline).unwrap();
                    let with = truth::expected_outcomes_for_units(&baseline, &units).unwrap();
                    assert_eq!(
                        with[dep_job].outcome,
                        crate::run::model::ExpectedOutcome::Built
                    );
                    let units = archive_input::load_units(&gated).unwrap();
                    let without = truth::expected_outcomes_for_units(&gated, &units).unwrap();
                    assert!(
                        without
                            .values()
                            .all(|t| t.outcome == crate::run::model::ExpectedOutcome::Unknown),
                        "{without:?}"
                    );
                    // Same gate, sibling consumer: the truth-collapse
                    // conflict count is an outcomes-as-truth measurement
                    // too. Seed a real cross-session disagreement (two
                    // scoped records, no session-less supersede) so the
                    // baseline measures a NONZERO count and the withheld
                    // side is distinguishable from "measured, none found":
                    // claimed → Some(>0), withdrawn → None (the campaign
                    // record stamps "not measured" instead of asserting a
                    // collapse that never ran).
                    let conflict_dir = tempfile::TempDir::new().unwrap();
                    let (conflict_root, _) = staged_tiny_archive(conflict_dir.path());
                    let path = conflict_root.join(OUTCOMES_MEMBER);
                    let mut text = std::fs::read_to_string(&path).unwrap();
                    text.push_str(&format!(
                        "{{\"session\":5,\"drv\":\"{APP_DRV}\",\"outcome\":\"built\"}}\n"
                    ));
                    std::fs::write(&path, text).unwrap();
                    refresh_files_entry(&conflict_root, OUTCOMES_MEMBER);
                    let claimed = ReplayArchive::open(&conflict_root).unwrap();
                    assert_eq!(
                        claimed.truth_collapse_conflicts(),
                        Some(1),
                        "the disagreement is measured under the claim"
                    );
                    set_capability_false(&conflict_root, capability.flag());
                    let withheld = ReplayArchive::open(&conflict_root).unwrap();
                    assert_eq!(
                        withheld.truth_collapse_conflicts(),
                        None,
                        "the withdrawn claim withholds the measurement at the reader"
                    );
                }
                // Gates output-divergence verdicts: per-output hashes are
                // withheld when unclaimed, so the comparator sees
                // not-comparable instead of minting divergence from data
                // the recorder did not vouch for. The outcome itself still
                // flows (expected_outcomes stays claimed).
                Capability::OutputHashes => {
                    let units = archive_input::load_units(&baseline).unwrap();
                    let with = truth::expected_outcomes_for_units(&baseline, &units).unwrap();
                    assert!(with[dep_job].side.outputs["out"].nar_hash.is_some());
                    let units = archive_input::load_units(&gated).unwrap();
                    let without = truth::expected_outcomes_for_units(&gated, &units).unwrap();
                    assert_eq!(
                        without[dep_job].outcome,
                        crate::run::model::ExpectedOutcome::Built,
                        "the outcome is vouched for; only the hashes are not"
                    );
                    assert!(without[dep_job].side.outputs["out"].nar_hash.is_none());
                    assert!(
                        gated
                            .outcome_records()
                            .all(|record| record.outputs.is_empty()),
                        "unclaimed hashes are withheld at the reader"
                    );
                }
                // Gates the archive rung of the supply ladder: unclaimed
                // embedded trees are not offered as upload sources.
                Capability::EmbeddedStorePaths => {
                    assert!(baseline.has_embedded(SRC_PATH));
                    assert_eq!(baseline.embedded_store_paths(), vec![SRC_PATH.to_string()]);
                    assert!(!gated.has_embedded(SRC_PATH));
                    assert!(gated.embedded_store_paths().is_empty());
                }
                // Gates impure demotion: units leave the measured workload
                // only when the recorder claimed the impure-env data.
                Capability::ImpureEnv => {
                    let with = supply::workload_set(&baseline);
                    assert_eq!(
                        with.demoted_impure.iter().collect::<Vec<_>>(),
                        vec![APP_DRV]
                    );
                    let without = supply::workload_set(&gated);
                    assert!(without.demoted_impure.is_empty());
                    assert!(without.drvs.contains(APP_DRV));
                    assert!(gated.impure_env().is_empty());
                }
                // Gates plan-time closure construction: adjacency records
                // are used only under the flag (the sentinel source exists
                // only there); otherwise the embedded ATerms are walked.
                Capability::DependencyClosures => {
                    let units = archive_input::load_units(&baseline).unwrap();
                    let with = archive_input::load_closures(&baseline, &units).unwrap();
                    let dep_entry = with.iter().find(|e| e.job == dep_job).unwrap();
                    assert!(dep_entry.srcs.contains(&SENTINEL_SRC.to_string()));
                    let units = archive_input::load_units(&gated).unwrap();
                    let without = archive_input::load_closures(&gated, &units).unwrap();
                    let dep_entry = without.iter().find(|e| e.job == dep_job).unwrap();
                    assert!(!dep_entry.srcs.contains(&SENTINEL_SRC.to_string()));
                    assert!(dep_entry.srcs.contains(&SRC_PATH.to_string()));
                }
            }
        }
    }

    /// Standing enumeration of EVERY production consumer of the reader's
    /// outcome-derived surface (`outcome_records` / `expected_outcome` /
    /// `expected_outcome_across_sessions` / `truth_collapse_conflicts`),
    /// each named with the capability gate that licenses it.
    /// Quantification domain: call-shaped occurrences in every
    /// `rio-replay/src/**/*.rs` production region (each file truncated at
    /// its first `#[cfg(test)]` line — the bounded-io scan's rule).
    ///
    /// outcomes.jsonl is the one member whose claims split across THREE
    /// capability flags (expected_outcomes / output_hashes / timed), so
    /// it is exempt from the reader's structural withholding and relies
    /// on per-consumer gates — the discipline that decays exactly when a
    /// new consumer lands outside any battery. This test is the decay
    /// stop: a new call site fails the count until it is enumerated here
    /// WITH its gate, and the per-flag flip battery
    /// (`capability_flags_gate_their_documented_engine_behavior`) is
    /// where its behavioral delta then gets pinned.
    #[test]
    fn outcome_surface_consumers_are_enumerated_with_their_gates() {
        // (file, call needle, expected production call sites, gate) —
        // needles are dot-prefixed so definitions don't count, and built
        // by concatenation so this test's own source cannot match.
        let surface = |name: &str| format!(".{name}(");
        let expected: &[(&str, String, usize, &str)] = &[
            (
                "run/truth.rs",
                surface("expected_outcome_across_sessions"),
                1,
                "ExpectedOutcomes: expected_outcomes_for_units returns all-Unknown before \
                 this call when the flag is withdrawn",
            ),
            (
                "run/mod.rs",
                surface("expected_outcome"),
                1,
                "Timed: timing_index is reachable only inside the ScheduleMode::Timed arm \
                 after ensure_timed_capability",
            ),
            (
                "run/mod.rs",
                surface("outcome_records"),
                1,
                "Timed: the interruption arming scan runs only inside the \
                 ScheduleMode::Timed arm after ensure_timed_capability",
            ),
            (
                "run/mod.rs",
                surface("truth_collapse_conflicts"),
                1,
                "ExpectedOutcomes: the reader withholds the count (None) when the claim \
                 is withdrawn; the stamp passes the Option through",
            ),
        ];
        let needles: Vec<String> = vec![
            surface("outcome_records"),
            surface("expected_outcome"),
            surface("expected_outcome_across_sessions"),
            surface("truth_collapse_conflicts"),
        ];

        fn walk(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, files);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    files.push(path);
                }
            }
        }
        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src_root, &mut files);
        assert!(files.len() > 10, "the source walk must visit the crate");

        let mut found: std::collections::BTreeMap<(String, String), usize> = Default::default();
        for path in &files {
            let rel = path
                .strip_prefix(&src_root)
                .unwrap()
                .display()
                .to_string()
                .replace('\\', "/");
            // The reader is the surface's owner: its own file defines the
            // methods and tests them; consumers live elsewhere.
            if rel == "archive/reader.rs" {
                continue;
            }
            let src = std::fs::read_to_string(path).unwrap();
            let prod = src
                .split("#[cfg(test)]")
                .next()
                .expect("split always yields at least one piece");
            for needle in &needles {
                // The needles are mutually exclusive by construction: the
                // trailing `(` keeps `.expected_outcome(` from matching
                // `.expected_outcome_across_sessions(`.
                let count = prod.matches(needle.as_str()).count();
                if count > 0 {
                    *found.entry((rel.clone(), needle.clone())).or_default() += count;
                }
            }
        }

        let expected_map: std::collections::BTreeMap<(String, String), usize> = expected
            .iter()
            .map(|(file, needle, count, _gate)| (((*file).to_string(), needle.clone()), *count))
            .collect();
        assert_eq!(
            found, expected_map,
            "outcome-derived consumers changed: every production call site of the reader's \
             outcome surface must be enumerated here with the capability gate that licenses \
             it (and its behavioral delta pinned in the flip battery)"
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

    /// A sidecar whose filename and content disagree about which store path
    /// it describes is refused at open. The fixture is two writer-produced
    /// sidecars with their FILES swapped — the realistic mis-assembly shape
    /// (a foreign recorder, or a shuffled/edited directory-form archive)
    /// that every other v1 open-time check is provably blind to: the
    /// completeness loop only tests filename presence, and the narinfo
    /// listing digest hashes (content store path, bytes) pairs, which a
    /// swap leaves identical. Without the identity cross-check this archive
    /// opens cleanly and fails hours later, when `dump_nar` verifies an
    /// embedded tree against the other path's sidecar.
    #[test]
    fn mismatched_sidecar_identity_is_an_open_error() {
        use crate::archive::writer::test_support::{sidecar_text, stage_tiny_archive, tiny_seed};

        const EXTRA_PATH: &str = "/nix/store/h2222222222222222222222222222222-extra";

        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        let writer = crate::archive::writer::ArchiveWriter::create(&root).unwrap();
        let trees = tempfile::TempDir::new().unwrap();
        stage_tiny_archive(&writer, &trees.path().join("src"));
        // A second embedded path, so the archive has two sidecars to swap.
        let extra_tree = trees.path().join("extra");
        std::fs::create_dir_all(&extra_tree).unwrap();
        std::fs::write(extra_tree.join("data.txt"), "second embedded tree\n").unwrap();
        writer
            .embed_store_path(
                EXTRA_PATH,
                &extra_tree,
                &sidecar_text(EXTRA_PATH, &extra_tree),
            )
            .unwrap();
        writer.finalize(tiny_seed()).unwrap();

        // The writer-conformant archive opens: the cross-check cannot fire
        // on agreeing identities.
        ReplayArchive::open(&root).expect("conformant sidecars must not trip the identity check");

        // Swap the two sidecar files: each filename now locates the OTHER
        // path's description.
        let src_sidecar = root
            .join(NARINFO_DIR)
            .join(format!("{}.narinfo", hash_part(SRC_PATH)));
        let extra_sidecar = root
            .join(NARINFO_DIR)
            .join(format!("{}.narinfo", hash_part(EXTRA_PATH)));
        let src_bytes = std::fs::read(&src_sidecar).unwrap();
        let extra_bytes = std::fs::read(&extra_sidecar).unwrap();
        std::fs::write(&src_sidecar, &extra_bytes).unwrap();
        std::fs::write(&extra_sidecar, &src_bytes).unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(err.contains("is named for store hash"), "got: {err}");
        // Whichever swapped sidecar is indexed first, the error names both
        // identities: its filename hash and the other path it describes.
        assert!(
            err.contains(hash_part(SRC_PATH)) && err.contains(hash_part(EXTRA_PATH)),
            "got: {err}"
        );
    }

    /// Two `nix/store/` members colliding on store hash part cannot both
    /// be served — every lookup keys on the hash part — so a v1 archive
    /// carrying them is refused at open, like the narinfo identity
    /// mismatch above: real store paths derive the hash from the name,
    /// so a collision proves mis-assembly, and last-wins indexing would
    /// silently serve whichever member the backend listed last (a
    /// vanished supply rung or the wrong ATerm, surfacing mid-campaign).
    #[test]
    fn colliding_store_members_are_an_open_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        let collider = format!("{}-src2", hash_part(SRC_PATH));
        std::fs::write(root.join(STORE_DIR).join(&collider), b"foreign member\n").unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(err.contains("collide on store hash part"), "got: {err}");
        // The refusal names both members and the shared hash part.
        assert!(
            err.contains("-src") && err.contains(&collider) && err.contains(hash_part(SRC_PATH)),
            "got: {err}"
        );
    }

    /// Two sidecar files resolving to one store path (the canonical
    /// `<hash>.narinfo` plus the mirror `<hash>-<name>.narinfo`) are
    /// refused at open with an error naming both files. The writer's
    /// finalize refuses the same shape pre-pack (shared enumeration); this
    /// pins the post-finalize-damage path, where the named refusal is what
    /// stands between an operator and a bare "listing digest mismatch".
    #[test]
    fn duplicate_sidecar_spellings_are_an_open_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        let canonical = format!("{}.narinfo", hash_part(SRC_PATH));
        let mirror = format!("{}-src.narinfo", hash_part(SRC_PATH));
        std::fs::copy(
            root.join(NARINFO_DIR).join(&canonical),
            root.join(NARINFO_DIR).join(&mirror),
        )
        .unwrap();

        let err = format!("{:#}", ReplayArchive::open(&root).unwrap_err());
        assert!(
            err.contains(&canonical) && err.contains(&mirror),
            "the refusal must name both sidecar files: {err}"
        );
        assert!(err.contains("exactly one sidecar"), "got: {err}");
    }

    /// A v1 archive whose RELAY list carries an entry the admission screen
    /// rejects still opens. Our writer cannot produce such an archive (the
    /// finalize check refuses it), so the fixture edits the manifest after
    /// finalize — exactly the shape a foreign v1 recorder could publish.
    /// The published artifact is write-once: judgment over its list
    /// entries belongs to the bootstrap's per-entry classification, which
    /// skips the entry, never to open, which would brick the archive.
    #[test]
    fn cleartext_relay_substituter_opens_and_classifies_unusable() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // The substituter lists live in the manifest, which is not covered
        // by `files`, so the edit mimics a foreign recorder rather than
        // tripping a digest error. The tiny seed spells the same URL in
        // BOTH lists, so this rewrites the relay entry — the formerly
        // open-gated population — alongside the never-gated target one.
        rewrite_member(
            &root,
            MANIFEST_MEMBER,
            "https://cache.example.org",
            "http://cache.example.org",
        );
        let archive = ReplayArchive::open(&root).expect("a write-once archive must stay openable");
        assert_eq!(
            archive.manifest().substituters.relay,
            vec!["http://cache.example.org".to_string()]
        );
        let classified =
            crate::nixcache::ClassifiedSubstituters::classify(&archive.manifest().substituters);
        assert!(
            matches!(
                &classified.relay[0],
                crate::nixcache::ArchiveSubstituterUrl::Unusable { .. }
            ),
            "the cleartext relay entry classifies Unusable per entry"
        );
        // With every entry in both lists rejected, nothing is probeable —
        // and that is a point-of-use concern (the warm-set probe), not an
        // open error.
        assert!(classified.first_probeable().is_none());
    }

    /// v1 requests must carry non-empty `targets` (design doc §4.5,
    /// requests.jsonl table): `ArchiveWriter::write_requests` refuses to
    /// stage a violating record, so meeting one at open time means the
    /// archive was tampered with or corrupted — refuse loudly. The v0
    /// path deliberately diverges
    /// (`v0_empty_targets_request_is_skipped_not_fatal`): its producer
    /// never enforced the rule and the recordings are irreplaceable.
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
        assert_eq!(
            archive.outcome_records().count(),
            2,
            "duplicate keys collapse"
        );
        assert_eq!(
            archive
                .expected_outcome(key_of_session(&archive, 1), DEP_DRV)
                .unwrap()
                .outcome,
            crate::archive::schema::ExpectedOutcome::Failed
        );
    }

    /// One job key naming two distinct workload derivations is refused at
    /// the engine's ingest boundary with an error listing the colliders:
    /// every downstream structure (contexts, truth, the pending/terminal
    /// pools, latest-record-per-job results) is a job-keyed last-wins map,
    /// so admitting the collision would silently drop one unit from
    /// scheduling and accounting. The unedited archive — distinct labels
    /// for distinct derivations — must keep loading.
    #[test]
    fn load_units_refuses_one_job_key_naming_two_derivations() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // Must-admit: distinct labels load one entry per workload drv.
        let archive = ReplayArchive::open(&root).unwrap();
        let entries = crate::run::archive_input::load_units(&archive).unwrap();
        assert_eq!(entries.len(), 2);

        // A foreign or windowed recorder labels two derivations with one
        // job name (e.g. the same attr at two source revisions).
        rewrite_member(
            &root,
            UNITS_MEMBER,
            "pkgs.app.x86_64-linux",
            "pkgs.dep.x86_64-linux",
        );
        refresh_files_entry(&root, UNITS_MEMBER);

        let archive = ReplayArchive::open(&root).unwrap();
        let err = format!(
            "{:#}",
            crate::run::archive_input::load_units(&archive).unwrap_err()
        );
        assert!(err.contains("one job key"), "got: {err}");
        assert!(
            err.contains("pkgs.dep.x86_64-linux") && err.contains(DEP_DRV) && err.contains(APP_DRV),
            "the error must name the colliding job key and every derivation it claims: {err}"
        );
    }

    /// Duplicate units.jsonl records for ONE derivation (aliased source
    /// jobs: two attr paths evaluating to the same drv) keep the last
    /// record — the supersession rule duplicate outcome keys already use —
    /// and the workload itself is unaffected: still one entry per
    /// derivation, under the surviving label.
    #[test]
    fn duplicate_drv_unit_records_keep_the_last() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        let path = root.join(UNITS_MEMBER);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!(
            "{{\"drv\":\"{APP_DRV}\",\"label\":\"pkgs.appAlias.x86_64-linux\"}}\n"
        ));
        std::fs::write(&path, text).unwrap();
        refresh_files_entry(&root, UNITS_MEMBER);

        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(archive.units().len(), 2, "one record per derivation");
        assert_eq!(
            archive.units()[APP_DRV].label.as_deref(),
            Some("pkgs.appAlias.x86_64-linux"),
            "the later record supersedes"
        );
        let entries = crate::run::archive_input::load_units(&archive).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry.job == "pkgs.appAlias.x86_64-linux"),
            "{entries:?}"
        );
    }

    #[test]
    fn collapse_over_sessions_prefers_sessionless_then_informativeness_rank() {
        let dir = tempfile::TempDir::new().unwrap();
        let (root, _) = staged_tiny_archive(dir.path());

        // dep.drv has only the session-less record: the collapse picks it.
        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(
            archive
                .expected_outcome_across_sessions(DEP_DRV)
                .unwrap()
                .outcome,
            crate::archive::schema::ExpectedOutcome::Built
        );
        // app.drv has only the (session 0)-scoped record: a by-drv consumer
        // still sees it (the bug shape this helper retires was a constant
        // probe that could only reach session 0 by accident — scoped truth
        // on any other session vanished).
        assert_eq!(
            archive
                .expected_outcome_across_sessions(APP_DRV)
                .unwrap()
                .outcome,
            crate::archive::schema::ExpectedOutcome::Failed
        );
        assert!(
            archive
                .expected_outcome_across_sessions("/nix/store/x-unrecorded.drv")
                .is_none()
        );
        // Nothing disagrees yet: no conflicts to disclose.
        assert_eq!(archive.truth_collapse_conflicts(), Some(0));

        // Add a later session's `built` record for app.drv: with no
        // session-less form, the collapse keeps the most informative scoped
        // record per the design doc's rank table (`built` outranks
        // `failed`) — the session number is incidental here.
        let path = root.join(OUTCOMES_MEMBER);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!(
            "{{\"session\":5,\"drv\":\"{APP_DRV}\",\"outcome\":\"built\"}}\n"
        ));
        std::fs::write(&path, text).unwrap();
        refresh_files_entry(&root, OUTCOMES_MEMBER);

        let archive = ReplayArchive::open(&root).unwrap();
        let collapsed = archive.expected_outcome_across_sessions(APP_DRV).unwrap();
        assert_eq!(collapsed.session, Some(5));
        assert_eq!(
            collapsed.outcome,
            crate::archive::schema::ExpectedOutcome::Built
        );
        // The disagreement is now countable for the comparability block:
        // app.drv's scoped records disagree with no session-less supersede;
        // dep.drv (session-less authority) never counts.
        assert_eq!(archive.truth_collapse_conflicts(), Some(1));
    }

    /// The bug shape the rank exists for, pinned at the archive level in
    /// BOTH session orders: a built record (with output hashes) recorded by
    /// one session and a concurrent session's disconnect must collapse to
    /// the built record whichever session id is higher. Under the previous
    /// highest-session rule one of these two archives demoted a
    /// demonstrably-built unit to truth-indeterminate.
    #[test]
    fn built_truth_survives_a_concurrent_disconnect_in_either_session_order() {
        for (built_session, disconnected_session) in [(3i64, 9i64), (9, 3)] {
            let dir = tempfile::TempDir::new().unwrap();
            let (root, _) = staged_tiny_archive(dir.path());

            // Replace app.drv's truth with the two-session disagreement.
            let path = root.join(OUTCOMES_MEMBER);
            let text = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<String> = text
                .lines()
                .filter(|line| !line.contains(APP_DRV))
                .map(String::from)
                .collect();
            lines.push(format!(
                "{{\"session\":{built_session},\"drv\":\"{APP_DRV}\",\"outcome\":\"built\",\
                 \"outputs\":{{\"out\":{{\"nar_hash_hex\":\"{}\",\"nar_size\":7}}}}}}",
                "2".repeat(64)
            ));
            lines.push(format!(
                "{{\"session\":{disconnected_session},\"drv\":\"{APP_DRV}\",\
                 \"outcome\":\"disconnected\"}}"
            ));
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            refresh_files_entry(&root, OUTCOMES_MEMBER);

            let archive = ReplayArchive::open(&root).unwrap();
            let collapsed = archive.expected_outcome_across_sessions(APP_DRV).unwrap();
            assert_eq!(
                collapsed.outcome,
                crate::archive::schema::ExpectedOutcome::Built,
                "built truth must win for sessions ({built_session}, {disconnected_session})"
            );
            assert_eq!(collapsed.session, Some(built_session));
            assert!(
                !collapsed.outputs.is_empty(),
                "the comparison-driving hashes ride along"
            );
            assert_eq!(archive.truth_collapse_conflicts(), Some(1));
        }
    }

    /// Within-rank-4 conflict at the archive level, in BOTH labelings:
    /// two session-scoped `built` records carry CONFLICTING output
    /// hashes — the unit's NAR-comparison baseline. The chosen consumed
    /// truth (the hashes `expected_outcomes_for_units` copies into
    /// `ExpectedSide`) must be identical whichever session id carries
    /// which hash — content order decides, never session allocation —
    /// and the conflict must be DISCLOSED: `truthCollapseConflicts`
    /// counts the outputs axis even though the outcome enum agrees.
    /// Pre-fix both failed: the higher session's hash won (the choice
    /// flipped under relabeling) and the count compared only the
    /// outcome enum (zero disclosed conflicts).
    #[test]
    fn conflicting_rank4_hashes_resolve_by_content_and_are_disclosed() {
        let mut chosen_hashes = Vec::new();
        for (low_payload_session, high_payload_session) in [(3i64, 9i64), (9, 3)] {
            let dir = tempfile::TempDir::new().unwrap();
            let (root, _) = staged_tiny_archive(dir.path());

            let path = root.join(OUTCOMES_MEMBER);
            let text = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<String> = text
                .lines()
                .filter(|line| !line.contains(APP_DRV))
                .map(String::from)
                .collect();
            lines.push(format!(
                "{{\"session\":{low_payload_session},\"drv\":\"{APP_DRV}\",\"outcome\":\"built\",\
                 \"outputs\":{{\"out\":{{\"nar_hash_hex\":\"{}\",\"nar_size\":7}}}}}}",
                "2".repeat(64)
            ));
            lines.push(format!(
                "{{\"session\":{high_payload_session},\"drv\":\"{APP_DRV}\",\"outcome\":\"built\",\
                 \"outputs\":{{\"out\":{{\"nar_hash_hex\":\"{}\",\"nar_size\":7}}}}}}",
                "7".repeat(64)
            ));
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            refresh_files_entry(&root, OUTCOMES_MEMBER);

            let archive = ReplayArchive::open(&root).unwrap();
            let collapsed = archive.expected_outcome_across_sessions(APP_DRV).unwrap();
            assert_eq!(
                collapsed.outcome,
                crate::archive::schema::ExpectedOutcome::Built
            );
            chosen_hashes.push(collapsed.outputs.clone());
            assert_eq!(
                archive.truth_collapse_conflicts(),
                Some(1),
                "an outcome-equal hash conflict must be disclosed, not silently resolved"
            );
        }
        chosen_hashes.dedup();
        assert_eq!(
            chosen_hashes.len(),
            1,
            "the NAR-comparison baseline must not flip under session relabeling: \
             {chosen_hashes:?}"
        );
        assert_eq!(
            chosen_hashes[0]["out"].nar_hash,
            crate::narhash::NarHash::parse(&"7".repeat(64)).unwrap(),
            "content order picks the greater digest under either labeling"
        );
    }

    /// The conflict count's payload axis at the unit level, both
    /// directions (counted vs not): records identical on the consumed
    /// truth — including identical hashes recorded by different
    /// sessions, and outcome-equal records where only non-truth fields
    /// (detail, timing) differ — are NOT conflicts; records differing in
    /// nar size alone ARE (size is part of the recorded NAR identity the
    /// comparator consumes). A session-less record supersedes by
    /// contract, so even a hash conflict under one stays uncounted.
    #[test]
    fn truth_collapse_conflicts_counts_the_consumed_truth_axes() {
        let cases: &[(&str, Vec<String>, usize)] = &[
            (
                "identical hashes across sessions",
                vec![
                    outcome_line(3, "built", Some(("2", 7))),
                    outcome_line(9, "built", Some(("2", 7))),
                ],
                0,
            ),
            (
                "non-truth fields differ (detail/timing)",
                vec![
                    format!(
                        "{{\"session\":3,\"drv\":\"{APP_DRV}\",\"outcome\":\"failed\",\
                         \"detail\":\"exit 2\",\"duration_s\":10.0}}"
                    ),
                    format!(
                        "{{\"session\":9,\"drv\":\"{APP_DRV}\",\"outcome\":\"failed\",\
                         \"detail\":\"exit code 2 (rebuilt)\",\"duration_s\":99.5}}"
                    ),
                ],
                0,
            ),
            (
                "nar size differs, digest equal",
                vec![
                    outcome_line(3, "built", Some(("2", 7))),
                    outcome_line(9, "built", Some(("2", 8))),
                ],
                1,
            ),
            (
                "hash conflict under a session-less supersede",
                vec![
                    outcome_line(3, "built", Some(("2", 7))),
                    outcome_line(9, "built", Some(("7", 7))),
                    format!(
                        "{{\"drv\":\"{APP_DRV}\",\"outcome\":\"built\",\
                         \"outputs\":{{\"out\":{{\"nar_hash_hex\":\"{}\",\"nar_size\":7}}}}}}",
                        "2".repeat(64)
                    ),
                ],
                0,
            ),
        ];
        for (label, extra_lines, want) in cases {
            let dir = tempfile::TempDir::new().unwrap();
            let (root, _) = staged_tiny_archive(dir.path());
            let path = root.join(OUTCOMES_MEMBER);
            let text = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<String> = text
                .lines()
                .filter(|line| !line.contains(APP_DRV))
                .map(String::from)
                .collect();
            lines.extend(extra_lines.iter().cloned());
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            refresh_files_entry(&root, OUTCOMES_MEMBER);

            let archive = ReplayArchive::open(&root).unwrap();
            assert_eq!(archive.truth_collapse_conflicts(), Some(*want), "{label}");
        }
    }

    /// One scoped outcome line for `APP_DRV`; `outputs` from the
    /// `(digest nibble, nar_size)` pair when present.
    fn outcome_line(session: i64, outcome: &str, outputs: Option<(&str, u64)>) -> String {
        match outputs {
            None => {
                format!("{{\"session\":{session},\"drv\":\"{APP_DRV}\",\"outcome\":\"{outcome}\"}}")
            }
            Some((nibble, nar_size)) => format!(
                "{{\"session\":{session},\"drv\":\"{APP_DRV}\",\"outcome\":\"{outcome}\",\
                 \"outputs\":{{\"out\":{{\"nar_hash_hex\":\"{}\",\"nar_size\":{nar_size}}}}}}}",
                nibble.repeat(64)
            ),
        }
    }

    /// One synthetic scoped record per (outcome, hashes) shape, for the
    /// rank-policy tests below. Constant `'3'` payload — shapes that need
    /// the within-rank payload axis use [`scoped_record_with_payload`].
    fn scoped_record(session: i64, outcome: ExpectedOutcome, with_hashes: bool) -> OutcomeRecord {
        scoped_record_with_payload(session, outcome, with_hashes, '3')
    }

    /// [`scoped_record`] with the hash payload nibble chosen by the
    /// caller: rank-4 records with DIFFERENT payloads are how the
    /// within-rank conflict axis is exercised — a constant payload is
    /// exactly what made the round-3 relabeling lattice structurally
    /// blind to session-decided hash flips.
    fn scoped_record_with_payload(
        session: i64,
        outcome: ExpectedOutcome,
        with_hashes: bool,
        payload: char,
    ) -> OutcomeRecord {
        OutcomeRecord {
            session: Some(session),
            drv: "/nix/store/r-rank.drv".to_string(),
            outcome,
            detail: None,
            duration_s: None,
            stop_offset_s: None,
            outputs: if with_hashes {
                BTreeMap::from([(
                    "out".to_string(),
                    crate::archive::schema::OutputHash {
                        nar_hash: crate::narhash::NarHash::parse(&payload.to_string().repeat(64))
                            .unwrap(),
                        nar_size: 7,
                    },
                )])
            } else {
                BTreeMap::new()
            },
        }
    }

    /// Every distinct record shape the rank table ranks: (outcome, carries
    /// output hashes). Hashes only differentiate `built` (the format
    /// defines `outputs` for built outcomes).
    fn rank_table_shapes() -> Vec<(ExpectedOutcome, bool)> {
        let mut shapes = Vec::new();
        for outcome in ExpectedOutcome::ALL {
            if outcome == ExpectedOutcome::Built {
                shapes.push((outcome, true));
            }
            shapes.push((outcome, false));
        }
        shapes
    }

    /// The rank assignment IS the design doc's table
    /// (docs/dev/2026-05-28-build-replay-design.md, "The informativeness
    /// rank"): built+hashes 4, built 3, failed/resource-exhausted 2,
    /// cancelled/disconnected/indeterminate 1, unknown 0. The expected
    /// rows are data here so a rank change must touch BOTH the doc table
    /// and this pin, and iterating `ExpectedOutcome::ALL` forces a row for
    /// every vocabulary entry.
    #[test]
    fn collapse_rank_table_matches_the_design_doc() {
        use ExpectedOutcome::*;
        let documented: &[(ExpectedOutcome, bool, u8)] = &[
            (Built, true, 4),
            (Built, false, 3),
            (Failed, false, 2),
            (ResourceExhausted, false, 2),
            (Cancelled, false, 1),
            (Disconnected, false, 1),
            (Indeterminate, false, 1),
            (Unknown, false, 0),
        ];
        assert_eq!(
            documented.len(),
            rank_table_shapes().len(),
            "every record shape needs a documented rank row"
        );
        for (outcome, with_hashes, rank) in documented {
            assert_eq!(
                informativeness_rank(&scoped_record(1, *outcome, *with_hashes)),
                *rank,
                "{outcome:?} with_hashes={with_hashes}"
            );
        }
        for outcome in ExpectedOutcome::ALL {
            assert!(
                documented.iter().any(|(o, _, _)| *o == outcome),
                "{outcome:?} has no documented rank row"
            );
        }
    }

    /// The consumed-truth invariance property over the full shape
    /// lattice, stated over what the consumer actually reads (the
    /// (outcome, outputs) projection, [`consumed_truth`]) — not over the
    /// classifier's internal rank class, which is the granularity error
    /// that let rank-4 hash flips ride session ids undetected: for every
    /// pair of record shapes × payload assignment and BOTH labelings of
    /// the two session ids, the collapse picks the maximum rank, and the
    /// chosen CONSUMED truth is a pure function of the record contents
    /// (rank, then content order), never of the session labeling.
    /// Session ids decide only between records whose consumed truth is
    /// identical. The payload axis runs hash nibbles ('3','7') and
    /// ('7','3') across the pair, so within-rank-4 conflicting-hash
    /// cells are exercised in both content directions. A three-record
    /// equal-rank multiset is checked under every permutation:
    /// reordering equal-rank records across sessions never changes the
    /// chosen consumed truth.
    #[test]
    fn collapse_consumed_truth_is_invariant_under_session_relabeling() {
        let shapes = rank_table_shapes();
        for &(outcome_a, hashes_a) in &shapes {
            for &(outcome_b, hashes_b) in &shapes {
                for (payload_a, payload_b) in [('3', '3'), ('3', '7'), ('7', '3')] {
                    let rank_a = informativeness_rank(&scoped_record_with_payload(
                        0, outcome_a, hashes_a, payload_a,
                    ));
                    let rank_b = informativeness_rank(&scoped_record_with_payload(
                        0, outcome_b, hashes_b, payload_b,
                    ));
                    let mut chosen_truths = Vec::new();
                    for (session_a, session_b) in [(3i64, 9i64), (9, 3)] {
                        let by_session: BTreeMap<Option<i64>, OutcomeRecord> = BTreeMap::from([
                            (
                                Some(session_a),
                                scoped_record_with_payload(
                                    session_a, outcome_a, hashes_a, payload_a,
                                ),
                            ),
                            (
                                Some(session_b),
                                scoped_record_with_payload(
                                    session_b, outcome_b, hashes_b, payload_b,
                                ),
                            ),
                        ]);
                        let chosen = collapse_scoped(&by_session).unwrap();
                        let label = format!(
                            "{outcome_a:?}/{hashes_a}/{payload_a} @{session_a} vs \
                             {outcome_b:?}/{hashes_b}/{payload_b} @{session_b}"
                        );
                        assert_eq!(informativeness_rank(chosen), rank_a.max(rank_b), "{label}");
                        // The content-derived expectation: the max of the two
                        // records under (rank, consumed-truth) order — a pure
                        // function of contents, independent of the labeling.
                        let record_a = &by_session[&Some(session_a)];
                        let record_b = &by_session[&Some(session_b)];
                        let want = if (rank_a, consumed_truth(record_a))
                            >= (rank_b, consumed_truth(record_b))
                        {
                            consumed_truth(record_a)
                        } else {
                            consumed_truth(record_b)
                        };
                        assert_eq!(
                            consumed_truth(chosen),
                            want,
                            "consumed truth must resolve by content, not labeling: {label}"
                        );
                        // Session ids decide only between records the
                        // consumer cannot tell apart.
                        if consumed_truth(record_a) == consumed_truth(record_b) {
                            assert_eq!(chosen.session, Some(session_a.max(session_b)), "{label}");
                        }
                        chosen_truths
                            .push((consumed_truth(chosen).0, consumed_truth(chosen).1.clone()));
                    }
                    chosen_truths.dedup();
                    assert_eq!(
                        chosen_truths.len(),
                        1,
                        "one consumed truth across both labelings: \
                         {outcome_a:?}/{hashes_a}/{payload_a} vs {outcome_b:?}/{hashes_b}/{payload_b}"
                    );
                }
            }
        }

        // Equal-rank multiset: failed / resource-exhausted (both rank 2)
        // plus a disconnected (rank 1), permuted over three sessions every
        // way — the chosen class is always the shared maximum rank, and
        // the chosen consumed truth is the content-max of that class
        // (resource-exhausted > failed in wire-string order), regardless
        // of which session carries which record.
        let multiset = [
            (ExpectedOutcome::Failed, false),
            (ExpectedOutcome::ResourceExhausted, false),
            (ExpectedOutcome::Disconnected, false),
        ];
        let sessions = [2i64, 5, 8];
        let permutations: [[usize; 3]; 6] = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        for permutation in permutations {
            let by_session: BTreeMap<Option<i64>, OutcomeRecord> = permutation
                .iter()
                .zip(sessions)
                .map(|(&shape_index, session)| {
                    let (outcome, hashes) = multiset[shape_index];
                    (Some(session), scoped_record(session, outcome, hashes))
                })
                .collect();
            let chosen = collapse_scoped(&by_session).unwrap();
            assert_eq!(informativeness_rank(chosen), 2, "{permutation:?}");
            assert_eq!(
                chosen.outcome,
                ExpectedOutcome::ResourceExhausted,
                "the content-max of the top rank class wins under every permutation: \
                 {permutation:?}"
            );
        }
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
        let dep = archive
            .expected_outcome(key_of_session(&archive, 10), V0_DEP_DRV)
            .unwrap();
        assert_eq!(dep.outcome, schema::ExpectedOutcome::Built);
        assert_eq!(dep.outputs["out"].nar_size, 120);
        let app = archive
            .expected_outcome(key_of_session(&archive, 11), V0_APP_DRV)
            .unwrap();
        assert_eq!(app.outcome, schema::ExpectedOutcome::Failed);
        // detail keeps the native code alongside the recorder's message.
        assert_eq!(
            app.detail.as_deref(),
            Some("status=1: builder failed with exit code 1")
        );
        let impure = archive
            .expected_outcome(key_of_session(&archive, 12), V0_IMPURE_DRV)
            .unwrap();
        assert_eq!(impure.outcome, schema::ExpectedOutcome::Disconnected);
        assert_eq!(impure.stop_offset_s, Some(11.0));
        // The cached drv was a cache hit at record time: no truth record.
        assert!(
            archive
                .expected_outcome(key_of_session(&archive, 12), V0_CACHED_DRV)
                .is_none()
        );

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
            crate::narhash::NarHash::parse(&sidecar.nar_hash)
                .unwrap()
                .to_hex()
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
            crate::narhash::NarHash::parse(&sidecar.nar_hash)
                .unwrap()
                .to_hex()
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
        assert_eq!(archive.outcome_records().count(), 0);
        assert!(archive.impure_env().is_empty());
        assert!(!archive.capabilities().expected_outcomes);
        assert!(!archive.capabilities().impure_env);
        assert_eq!(archive.requests().len(), 4);
    }

    /// Structural corruption stays a loud v0 open error, deliberately:
    /// the original v0 consumer hard-failed a malformed JSONL line with
    /// the same member + line message, so no recording that ever worked
    /// becomes unreadable through this refusal — and warn-skipping torn
    /// records (especially of the truth member) would silently change
    /// what the archive claims happened. Per-record SEMANTIC junk is the
    /// lenient axis (see [`RecordPolicy`] and
    /// `v0_empty_targets_request_is_skipped_not_fatal`).
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

    /// A v0 request record with an empty `paths` list is dropped with a
    /// warning instead of refusing the open. The v0 contract never
    /// required non-empty targets — the design doc's v0-compatibility
    /// table maps `paths` by field rename only, and the original v0
    /// consumer accepted such records — the non-emptiness rule is v1's,
    /// enforced by `ArchiveWriter::write_requests` at stage time and
    /// re-checked by the v1 reader as tamper detection
    /// (`empty_request_targets_are_rejected_on_open`). An empty request
    /// schedules nothing, so dropping it is semantically exact, while
    /// refusing would make an irreplaceable recording unopenable on
    /// every surface.
    #[test]
    fn v0_empty_targets_request_is_skipped_not_fatal() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);
        let path = root.join(REQUESTS_MEMBER);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"ssh_session_id\":15,\"offset_s\":1.0,\"paths\":[]}\n");
        std::fs::write(&path, text).unwrap();

        let archive = ReplayArchive::open(&root).unwrap();
        // The vacuous record costs only itself: the four real requests
        // load, sort, and count exactly as without it.
        assert_eq!(archive.requests().len(), 4);
        assert!(archive.requests().iter().all(|record| record.session != 15));
        assert_eq!(archive.manifest().counts.requests, 4);
        assert_eq!(archive.manifest().counts.workload_units, 4);
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

    /// A v0 sidecar whose filename and content identities disagree is
    /// skipped with a warning, like any other defective v0 sidecar: the
    /// recording is irreplaceable, so the affected path merely loses its
    /// description. Crucially it is dropped, not indexed under the
    /// filename — keying it by the (wrong) filename would serve one store
    /// path's NarHash/References as another's.
    #[test]
    fn v0_mismatched_sidecar_identity_is_skipped_not_fatal() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);

        // Rename the dep-output sidecar to a stem describing a different
        // store hash; its content still says c111… . The same mismatch in
        // a v1 archive is a hard open error
        // (`mismatched_sidecar_identity_is_an_open_error`).
        let mismatched_stem = "d1111111111111111111111111111111";
        std::fs::rename(
            root.join(NARINFO_DIR)
                .join("c1111111111111111111111111111111.narinfo"),
            root.join(NARINFO_DIR)
                .join(format!("{mismatched_stem}.narinfo")),
        )
        .unwrap();

        let archive = ReplayArchive::open(&root).unwrap();
        assert_eq!(archive.requests().len(), 4);
        assert!(
            archive
                .narinfo("c1111111111111111111111111111111")
                .is_none(),
            "the mismatched sidecar is not indexed under its content identity"
        );
        assert!(
            archive.narinfo(mismatched_stem).is_none(),
            "…and not under its filename identity either: that lookup would \
             return the wrong path's description"
        );
        assert!(
            archive.narinfo(V0_SRC_PATH).is_some(),
            "the intact sidecar still loads"
        );
    }

    /// The same store hash-part collision that hard-fails a v1 open
    /// (`colliding_store_members_are_an_open_error`) warns and keeps one
    /// member in a v0 archive: the recording is irreplaceable and the
    /// shadowed member was unreachable under hash-part keying anyway —
    /// what changes is that the collapse is loud and bounded to one
    /// entry per hash part instead of silently last-wins.
    ///
    /// WHICH member survives is part of the contract: the first in name
    /// order, per the `RecordPolicy::WarnAndSkip` rule shared with the
    /// sibling enumeration `index_narinfos` (archive/mod.rs). Both
    /// directions through the real open path: one collider sorts BEFORE
    /// the original member (it must win) and one AFTER (it must lose),
    /// with creation order deliberately disagreeing with name order so
    /// a creation-ordered enumeration cannot pass by accident. The
    /// listing-order axis itself is pinned by
    /// `store_member_winner_is_invariant_to_listing_order`, which
    /// injects permutations directly (raw fs enumeration order is not
    /// controllable from a test).
    #[test]
    fn v0_colliding_store_members_warn_and_keep_first_in_name_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);

        let hash = hash_part(V0_SRC_PATH);
        let first_by_name = format!("{hash}-a-collider.txt");
        let last_by_name = format!("{hash}-src2.txt");
        std::fs::write(
            root.join(STORE_DIR).join(&last_by_name),
            b"foreign member\n",
        )
        .unwrap();
        std::fs::write(
            root.join(STORE_DIR).join(&first_by_name),
            b"first in name order\n",
        )
        .unwrap();

        let archive =
            ReplayArchive::open(&root).expect("an irreplaceable v0 recording must stay openable");
        let with_hash: Vec<String> = archive
            .embedded_store_paths()
            .into_iter()
            .filter(|path| hash_part(path) == hash)
            .collect();
        assert_eq!(
            with_hash,
            vec![format!(
                "{}{first_by_name}",
                rio_nix::store_path::STORE_PREFIX
            )],
            "exactly one member per hash part survives, and it is the first in name order"
        );
    }

    /// Listing-order invariance of the store-member fold, with the
    /// order axis injected directly (`index_store_listing` exists so a
    /// test can do this — raw fs/DwarFS enumeration order is a
    /// filesystem-state accident no test can script). Quantification
    /// domain: every rotation of the listing plus its reversal, over a
    /// member set with one three-way collision and one non-colliding
    /// member. Under WarnAndSkip every permutation must produce the
    /// SAME index — the lex-min collider wins, the non-collider is
    /// untouched; under Strict every permutation must refuse with the
    /// error naming the SAME pair (the two lex-min members), so even
    /// the refusal message cannot re-roll across copies of one
    /// archive. Pre-sort, the reversed permutation alone makes the
    /// last-created member win and this test fail deterministically.
    #[test]
    fn store_member_winner_is_invariant_to_listing_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);
        let hash = hash_part(V0_SRC_PATH);
        for name in [
            format!("{hash}-a-collider.txt"),
            format!("{hash}-zz-collider.txt"),
        ] {
            std::fs::write(root.join(STORE_DIR).join(name), b"collider\n").unwrap();
        }

        let backend = Backend::open(&root).unwrap();
        let listing = backend.list_dir(STORE_DIR).unwrap().unwrap_or_default();
        assert!(
            listing.len() >= 4,
            "fixture + colliders give a 3-way collision plus non-colliding members"
        );

        let mut permutations: Vec<Vec<WalkEntry>> = Vec::new();
        for rotation in 0..listing.len() {
            let mut perm = listing.clone();
            perm.rotate_left(rotation);
            permutations.push(perm.clone());
            perm.reverse();
            permutations.push(perm);
        }

        let expected_winner = format!("{hash}-a-collider.txt");
        let mut warn_indexes = Vec::new();
        let mut strict_errors = Vec::new();
        for perm in permutations {
            let index = index_store_listing(perm.clone(), RecordPolicy::WarnAndSkip).unwrap();
            assert_eq!(
                index.get(hash).map(|entry| entry.name.as_str()),
                Some(expected_winner.as_str()),
                "the lex-min collider must win under every listing order"
            );
            let mut as_pairs: Vec<(String, String)> = index
                .iter()
                .map(|(k, v)| (k.clone(), v.name.clone()))
                .collect();
            as_pairs.sort();
            warn_indexes.push(as_pairs);
            let err = format!(
                "{:#}",
                index_store_listing(perm, RecordPolicy::Strict).unwrap_err()
            );
            strict_errors.push(err);
        }
        warn_indexes.dedup();
        assert_eq!(
            warn_indexes.len(),
            1,
            "one index for every permutation, not one per listing order"
        );
        strict_errors.dedup();
        assert_eq!(
            strict_errors.len(),
            1,
            "the Strict refusal must name the same colliding pair under every \
             listing order: {strict_errors:?}"
        );
        assert!(
            strict_errors[0].contains(&expected_winner),
            "the refusal names the lex-min member first: {}",
            strict_errors[0]
        );
    }

    /// A v0 recording whose `src_substituters` (the mapped relay list)
    /// carries an entry the admission screen rejects still OPENS. The
    /// original v0 consumer accepted plain-http internal caches, so real
    /// recordings of past production windows can carry one — and they are
    /// irreplaceable: open performs no fetches, judgment over list entries
    /// is the campaign bootstrap's per-entry classification, and an error
    /// surfaces only at a point of use with no usable alternative.
    #[test]
    fn v0_cleartext_relay_substituter_opens_and_classifies_unusable() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("archive");
        copy_v0_fixture_to(&root);

        // The recorded src_substituters entry becomes plain http — the
        // population the retired open-time scheme gate used to brick.
        let manifest_path = root.join(MANIFEST_MEMBER);
        let text = std::fs::read_to_string(&manifest_path).unwrap();
        let rewritten = text.replace("https://cache.example.org", "http://cache.example.org");
        assert_ne!(rewritten, text, "manifest has a relay entry to rewrite");
        std::fs::write(&manifest_path, rewritten).unwrap();

        let archive =
            ReplayArchive::open(&root).expect("an irreplaceable v0 recording must stay openable");
        // The recording is fully usable, not merely opened.
        assert_eq!(archive.requests().len(), 4);
        assert!(archive.narinfo(V0_SRC_PATH).is_some());
        // The entry survives verbatim and is judged at the bootstrap's
        // classification chokepoint: Unusable, skipped — and with no
        // probeable entry left, only a campaign shape that needs the probe
        // fails, at that point of use.
        assert_eq!(
            archive.manifest().substituters.relay,
            vec!["http://cache.example.org".to_string()]
        );
        let classified =
            crate::nixcache::ClassifiedSubstituters::classify(&archive.manifest().substituters);
        assert!(
            matches!(
                &classified.relay[0],
                crate::nixcache::ArchiveSubstituterUrl::Unusable { .. }
            ),
            "the cleartext relay entry classifies Unusable per entry"
        );
        assert!(classified.first_probeable().is_none());
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

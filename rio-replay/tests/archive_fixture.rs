//! The committed v1 archive fixture: generation, drift detection, and the
//! identity golden. The fixture under tests/fixtures/archive/v1-basic is
//! regenerated only by the #[ignore] generator below; the non-ignored
//! tests pin its contents and identity.
//!
//! Everything here goes through the public `rio_replay::archive` API only,
//! which doubles as proof that the API is sufficient for an external
//! recorder to assemble a valid archive.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rio_replay::archive::reader::ReplayArchive;
use rio_replay::archive::schema::{
    Capabilities, ClosureRecord, EXCLUSION_REASON_EVAL_ERROR, ExclusionRecord, ExpectedOutcome,
    OutcomeRecord, OutputHash, RequestRecord, RequestTarget, Substituters, UnitRecord,
};
use rio_replay::archive::writer::{
    ArchiveWriter, FinalizedArchive, ManifestSeed, pack_with_mkdwarfs,
};
use rio_replay::archive::{MANIFEST_MEMBER, NARINFO_DIR, identity};

/// The pinned identity of the committed fixture: the SHA-256 of its
/// `manifest.json` bytes. Run-once-and-pin discipline (same as the
/// rio-migrations checksum freeze): if this assertion starts failing, the
/// archive format or the writer's serialization changed — regenerate the
/// fixture with `regenerate_v1_basic_fixture`, review the diff deliberately,
/// and only then update the pin.
const V1_BASIC_ARCHIVE_ID: &str =
    "9fbf07bcc90bd64d3f29ea098ea7412a74c05208985afd48b4b6c57d225f0dee";

/// Store paths of the fixture archive. Deliberately restated here instead of
/// reusing `crate::archive::writer::test_support` (which carries the same
/// constants and tree builder): this file is the executable specification of
/// the committed fixture, and integration tests cannot see `#[cfg(test)]`
/// items anyway.
const DEP_DRV: &str = "/nix/store/d1111111111111111111111111111111-dep.drv";
const APP_DRV: &str = "/nix/store/d2222222222222222222222222222222-app.drv";
const OOM_DRV: &str = "/nix/store/d3333333333333333333333333333333-oom.drv";
const SRC_PATH: &str = "/nix/store/g1111111111111111111111111111111-src";
const DEP_OUT: &str = "/nix/store/f1111111111111111111111111111111-dep";
const APP_OUT: &str = "/nix/store/f2222222222222222222222222222222-app";
const OOM_OUT: &str = "/nix/store/f3333333333333333333333333333333-oom";

/// ATerm of `dep.drv`: builds `dep` from the embedded `src` path.
const DEP_ATERM: &str = concat!(
    r#"Derive([("out","/nix/store/f1111111111111111111111111111111-dep","","")],[],["/nix/store/g1111111111111111111111111111111-src"],"x86_64-linux","/bin/sh",["-c","cp -r $src $out"],[("out","/nix/store/f1111111111111111111111111111111-dep"),("src","/nix/store/g1111111111111111111111111111111-src")])"#,
    "\n"
);

/// ATerm of `app.drv`: depends on `dep.drv`.
const APP_ATERM: &str = concat!(
    r#"Derive([("out","/nix/store/f2222222222222222222222222222222-app","","")],[("/nix/store/d1111111111111111111111111111111-dep.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","/nix/store/f2222222222222222222222222222222-app")])"#,
    "\n"
);

/// ATerm of `oom.drv`: a standalone unit whose recorded outcome pins the
/// `resource-exhausted` vocabulary value end-to-end through the fixture.
const OOM_ATERM: &str = concat!(
    r#"Derive([("out","/nix/store/f3333333333333333333333333333333-oom","","")],[],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","/nix/store/f3333333333333333333333333333333-oom")])"#,
    "\n"
);

/// The committed fixture directory (resolved at runtime: under nextest's
/// `--workspace-remap` the compile-time manifest dir is a per-crate build
/// sandbox that no longer exists when the test binary runs).
fn fixture_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest"),
    )
    .join("tests/fixtures/archive/v1-basic")
}

/// Populate `dir` with the embedded source tree: a plain file, an executable
/// file, and a relative symlink, so the committed fixture pins NAR fidelity
/// for all three entry kinds.
fn make_src_tree(dir: &Path) {
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
/// serialization. No `URL:` line — sidecars of embedded paths need none.
fn src_sidecar_text(dir: &Path) -> String {
    use sha2::Digest as _;

    let mut nar = Vec::new();
    let nar_size = rio_nix::nar::dump_path_streaming(dir, &mut nar).unwrap();
    let digest: [u8; 32] = sha2::Sha256::digest(&nar).into();
    let nar_hash = rio_nix::store_path::nixbase32::encode(&digest);
    format!(
        "StorePath: {SRC_PATH}\nNarHash: sha256:{nar_hash}\nNarSize: {nar_size}\nReferences:\nCompression: none\n"
    )
}

/// Stage the fixture archive into `root`. Deterministic: fixed timestamps,
/// fixed content, BTreeMap ordering throughout, so regenerating the fixture
/// from an unchanged writer is byte-identical.
fn generate_v1_basic(root: &Path) -> FinalizedArchive {
    let writer = ArchiveWriter::create(root).unwrap();

    writer.add_drv(DEP_DRV, DEP_ATERM).unwrap();
    writer.add_drv(APP_DRV, APP_ATERM).unwrap();
    writer.add_drv(OOM_DRV, OOM_ATERM).unwrap();

    let src_dir = tempfile::TempDir::new().unwrap();
    let src_tree = src_dir.path().join("src");
    make_src_tree(&src_tree);
    writer
        .embed_store_path(SRC_PATH, &src_tree, &src_sidecar_text(&src_tree))
        .unwrap();

    writer
        .write_requests(&[
            RequestRecord {
                session: 0,
                offset_s: 0.0,
                targets: vec![
                    RequestTarget {
                        drv: APP_DRV.to_string(),
                        outputs: Vec::new(),
                    },
                    RequestTarget {
                        drv: OOM_DRV.to_string(),
                        outputs: vec!["*".to_string()],
                    },
                ],
            },
            RequestRecord {
                session: 1,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: DEP_DRV.to_string(),
                    outputs: vec!["out".to_string()],
                }],
            },
        ])
        .unwrap();

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
            OutcomeRecord {
                session: None,
                drv: OOM_DRV.to_string(),
                outcome: ExpectedOutcome::ResourceExhausted,
                detail: Some("status=16".to_string()),
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
            UnitRecord {
                drv: OOM_DRV.to_string(),
                label: Some("pkgs.oom.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), OOM_OUT.to_string())]),
                required_features: vec!["big-parallel".to_string()],
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
            ClosureRecord {
                drv: OOM_DRV.to_string(),
                inputs: Vec::new(),
                srcs: Vec::new(),
                outputs: BTreeMap::from([("out".to_string(), Some(OOM_OUT.to_string()))]),
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
    writer
        .finalize(ManifestSeed {
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
        })
        .unwrap()
}

/// What one fixture entry is, for byte-level comparison: directories by
/// presence, symlinks by their literal target, regular files by their bytes
/// plus the executable bit (a lost 100755 mode must fail the comparison and
/// name the file — the exec bit is part of the NAR serialization).
#[derive(Debug, PartialEq, Eq)]
enum EntryFingerprint {
    Dir,
    Symlink(PathBuf),
    File { bytes: Vec<u8>, exec: bool },
}

/// Recursively fingerprint every entry under `root`, keyed by relative path.
fn collect_tree(root: &Path) -> BTreeMap<String, EntryFingerprint> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, EntryFingerprint>) {
        use std::os::unix::fs::PermissionsExt as _;

        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .expect("fixture paths are UTF-8")
                .to_string();
            // DirEntry::file_type does not follow symlinks, so a symlink is
            // fingerprinted by its target rather than its referent's bytes.
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                out.insert(rel, EntryFingerprint::Dir);
                walk(root, &path, out);
            } else if file_type.is_symlink() {
                out.insert(
                    rel,
                    EntryFingerprint::Symlink(std::fs::read_link(&path).unwrap()),
                );
            } else {
                let exec = entry.metadata().unwrap().permissions().mode() & 0o100 != 0;
                out.insert(
                    rel,
                    EntryFingerprint::File {
                        bytes: std::fs::read(&path).unwrap(),
                        exec,
                    },
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// Regenerate the committed fixture in place and print its identity. Run
/// manually when the archive format or the writer changes deliberately:
///
/// ```text
/// nix develop -c cargo nextest run -p rio-replay --run-ignored all \
///   --no-capture -E 'test(regenerate_v1_basic_fixture)'
/// ```
///
/// then review the fixture diff and update `V1_BASIC_ARCHIVE_ID` to the
/// printed id.
#[test]
#[ignore = "regenerates the committed fixture in place; run explicitly and review the diff"]
fn regenerate_v1_basic_fixture() {
    let dir = fixture_dir();
    std::fs::remove_dir_all(&dir).ok();
    let finalized = generate_v1_basic(&dir);
    println!(
        "regenerated {} with archive_id {}",
        dir.display(),
        finalized.archive_id
    );
}

/// The committed fixture must be byte-identical to what the generator above
/// produces, member by member: any writer serialization drift (or a stray
/// hand edit to the fixture) fails here instead of silently shipping a
/// fixture the writer can no longer reproduce.
#[test]
fn committed_fixture_matches_the_generator() {
    let fixture = fixture_dir();
    assert!(
        fixture.is_dir(),
        "committed fixture missing at {}; run the regenerate_v1_basic_fixture test",
        fixture.display()
    );

    let staging = tempfile::TempDir::new().unwrap();
    let regenerated_root = staging.path().join("v1-basic");
    generate_v1_basic(&regenerated_root);

    let committed = collect_tree(&fixture);
    let regenerated = collect_tree(&regenerated_root);
    assert_eq!(
        committed.keys().collect::<Vec<_>>(),
        regenerated.keys().collect::<Vec<_>>(),
        "the committed fixture and the generator disagree about which members exist"
    );
    for (rel, expected) in &regenerated {
        assert_eq!(
            &committed[rel], expected,
            "committed fixture member {rel} differs from the generator's output; regenerate \
             with the regenerate_v1_basic_fixture test and review the diff"
        );
    }
}

/// The identity golden: the committed fixture's archive id is pinned, the
/// manifest's content digests are recomputable from the members the reader
/// exposes, and changing a covered member changes the id.
#[test]
fn committed_fixture_identity_golden() {
    let archive = ReplayArchive::open(&fixture_dir()).unwrap();
    assert_eq!(
        archive.archive_id(),
        Some(V1_BASIC_ARCHIVE_ID),
        "the committed fixture's identity changed; if the format/writer change is deliberate, \
         regenerate the fixture, review the diff, and update V1_BASIC_ARCHIVE_ID"
    );
    assert_eq!(
        archive.archive_id_short().as_deref(),
        Some(&V1_BASIC_ARCHIVE_ID[..16])
    );

    // The committed fixture pins the `resource-exhausted` wire value
    // end-to-end: a vocabulary rename would fail here on the committed bytes.
    assert_eq!(
        archive.expected_outcome(0, OOM_DRV).unwrap().outcome,
        ExpectedOutcome::ResourceExhausted
    );

    // Each content_digests field must equal the canonical listing digest
    // recomputed from the members the opened archive exposes.
    let manifest = archive.manifest();

    let drv_entries: Vec<(String, String)> = archive
        .embedded_drvs()
        .into_iter()
        .map(|drv| {
            let aterm = archive.read_drv(&drv).unwrap();
            (drv, identity::sha256_hex(aterm.as_bytes()))
        })
        .collect();
    assert_eq!(
        identity::listing_digest(&drv_entries),
        manifest.content_digests.drvs
    );

    let nar_entries: Vec<(String, String)> = archive
        .embedded_store_paths()
        .into_iter()
        .map(|path| {
            let nar = archive.dump_nar(&path).unwrap();
            (path, identity::sha256_hex(&nar))
        })
        .collect();
    assert_eq!(
        identity::listing_digest(&nar_entries),
        manifest.content_digests.embedded_store_paths
    );

    let mut narinfo_entries: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir(fixture_dir().join(NARINFO_DIR)).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        let store_path = text
            .lines()
            .find_map(|line| line.strip_prefix("StorePath:"))
            .expect("sidecar carries a StorePath line")
            .trim()
            .to_string();
        narinfo_entries.push((store_path, identity::sha256_hex(&bytes)));
    }
    assert_eq!(
        identity::listing_digest(&narinfo_entries),
        manifest.content_digests.narinfo
    );

    // The id is the digest of the manifest bytes, and the manifest embeds a
    // digest for every covered member — so changing any covered member
    // (here: simulated by flipping its recorded digest) changes the id.
    let manifest_bytes = std::fs::read(fixture_dir().join(MANIFEST_MEMBER)).unwrap();
    assert_eq!(
        identity::archive_id_from_manifest_bytes(&manifest_bytes),
        V1_BASIC_ARCHIVE_ID
    );
    let manifest_text = String::from_utf8(manifest_bytes).unwrap();
    let recorded = &manifest.content_digests.drvs;
    let flipped_first = if recorded.starts_with('0') { "1" } else { "0" };
    let tampered = manifest_text.replacen(
        recorded.as_str(),
        &format!("{flipped_first}{}", &recorded[1..]),
        1,
    );
    assert_ne!(tampered, manifest_text, "the drvs digest was rewritten");
    assert_ne!(
        identity::archive_id_from_manifest_bytes(tampered.as_bytes()),
        V1_BASIC_ARCHIVE_ID,
        "a changed member digest must change the archive id"
    );
}

/// Container independence for the committed fixture: packed into a DwarFS
/// image, it opens with the same identity and the same model counts as the
/// directory form.
#[test]
fn committed_fixture_round_trips_through_a_dwarfs_image() {
    let dir = tempfile::TempDir::new().unwrap();
    let image = dir.path().join("v1-basic.dwarfs");
    pack_with_mkdwarfs(&fixture_dir(), &image).unwrap();

    let from_dir = ReplayArchive::open(&fixture_dir()).unwrap();
    let from_image = ReplayArchive::open(&image).unwrap();

    assert_eq!(from_image.archive_id(), Some(V1_BASIC_ARCHIVE_ID));
    assert_eq!(from_dir.archive_id(), from_image.archive_id());
    assert_eq!(from_dir.requests().len(), from_image.requests().len());
    assert_eq!(from_dir.outcomes().len(), from_image.outcomes().len());
    assert_eq!(from_dir.units().len(), from_image.units().len());
    assert_eq!(from_dir.closures().len(), from_image.closures().len());
}

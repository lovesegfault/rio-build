//! Integration tests for the eval-store core and its FFI surface.
//!
//! The "nix side" of every cross-check is simulated with an independent
//! re-computation through rio-nix's public API — agreement exercises the
//! success path, a deliberately wrong path exercises the hard-error path.

use std::collections::BTreeMap;
use std::io::Cursor;

use rio_evalstore::store::{EntryKind, PathStat, ProvidedInfo};
use rio_evalstore::{CaMethod, DumpMethod, EvalStore, EvalStoreError};
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::nar::{self, NarEntry, NarNode};
use rio_nix::store_path::StorePath;
use sha2::{Digest, Sha256};

fn sample_tree() -> NarNode {
    NarNode::Directory {
        entries: vec![
            NarEntry {
                name: "bin".into(),
                node: NarNode::Directory {
                    entries: vec![NarEntry {
                        name: "tool".into(),
                        node: NarNode::Regular {
                            executable: true,
                            contents: b"#!/bin/sh\necho hi\n".to_vec(),
                        },
                    }],
                },
            },
            NarEntry {
                name: "data.txt".into(),
                node: NarNode::Regular {
                    executable: false,
                    contents: b"payload\n".to_vec(),
                },
            },
            NarEntry {
                name: "link".into(),
                node: NarNode::Symlink {
                    target: "data.txt".into(),
                },
            },
        ],
    }
}

fn nar_bytes(node: &NarNode) -> Vec<u8> {
    let mut out = Vec::new();
    nar::serialize(&mut out, node).expect("serialize");
    out
}

/// The "nix side" of the addToStoreFromDump cross-check: recompute the
/// path from the hashes the store hands back.
fn nix_path_recursive(name: &str, hashes: &rio_evalstore::store::AddHashes) -> String {
    let h = NixHash::new(HashAlgo::SHA256, hex::decode(&hashes.nar_sha256).unwrap()).unwrap();
    StorePath::make_fixed_output(name, &h, true, &[])
        .unwrap()
        .to_string()
}

fn open_store(dir: &tempfile::TempDir) -> EvalStore {
    EvalStore::open(Some(dir.path().join("cas").to_str().unwrap())).expect("open")
}

#[test]
fn add_from_dump_roundtrip_and_readback() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let tree = sample_tree();
    let dump = nar_bytes(&tree);

    let result = store.add_from_dump(
        "sample",
        DumpMethod::NixArchive,
        CaMethod::NixArchive,
        &[],
        &mut Cursor::new(&dump),
        &mut |h| Ok(nix_path_recursive("sample", h)),
    )?;

    // Path agrees with an independent recomputation.
    let nar_hash = NixHash::new(HashAlgo::SHA256, Sha256::digest(&dump).to_vec())?;
    let expected = StorePath::make_fixed_output("sample", &nar_hash, true, &[])?;
    assert_eq!(result.path, expected.as_str());
    assert_eq!(result.nar_size, dump.len() as u64);

    let basename = expected.basename();
    assert!(store.is_valid_path(basename));

    // Path info round-trips.
    let info = store.query_path_info(basename)?.expect("indexed");
    assert_eq!(info.nar_size, dump.len() as u64);
    assert_eq!(info.nar_hash, hex::encode(Sha256::digest(&dump)));
    assert_eq!(info.ca.as_deref().map(|c| &c[..8]), Some("fixed:r:"));

    // NAR regeneration is byte-identical (framing from the dir blobs).
    let mut regen = Vec::new();
    store.nar_from_path(basename, &mut regen)?;
    assert_eq!(regen, dump, "regenerated NAR must be byte-identical");

    // Read-back ops.
    let root = store.lstat(basename, "")?.expect("root exists");
    assert_eq!(root, PathStat::Directory);
    let dirents = store.read_directory(basename, "")?;
    assert_eq!(
        dirents,
        vec![
            (b"bin".to_vec(), EntryKind::Directory),
            (b"data.txt".to_vec(), EntryKind::Regular),
            (b"link".to_vec(), EntryKind::Symlink),
        ]
    );
    let mut content = Vec::new();
    store.read_file(basename, "bin/tool", &mut content)?;
    assert_eq!(content, b"#!/bin/sh\necho hi\n");
    assert_eq!(store.read_link(basename, "link")?, b"data.txt");
    assert_eq!(store.lstat(basename, "missing")?, None);
    assert_eq!(
        store.lstat(basename, "bin/tool")?,
        Some(PathStat::Regular {
            size: 18,
            executable: true
        })
    );

    // queryPathFromHashPart finds it.
    let hash_part = &basename[..32];
    assert_eq!(
        store.query_path_from_hash_part(hash_part)?,
        Some(expected.to_string())
    );
    Ok(())
}

/// Everything survives a close + reopen: the pack store is flushed on
/// drop, and a fresh process serves the same bytes (streamed content
/// comes from FETCHED records, not from any origin tree).
#[test]
fn streamed_ingest_persists_across_reopen() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let dump = nar_bytes(&sample_tree());
    let path = {
        let store = open_store(&dir);
        store
            .add_from_dump(
                "sample",
                DumpMethod::NixArchive,
                CaMethod::NixArchive,
                &[],
                &mut Cursor::new(&dump),
                &mut |h| Ok(nix_path_recursive("sample", h)),
            )?
            .path
    };
    let basename = path.strip_prefix("/nix/store/").unwrap();

    let store = open_store(&dir);
    assert!(store.is_valid_path(basename), "path lost across reopen");
    let mut regen = Vec::new();
    store.nar_from_path(basename, &mut regen)?;
    assert_eq!(regen, dump);
    let mut content = Vec::new();
    store.read_file(basename, "data.txt", &mut content)?;
    assert_eq!(content, b"payload\n");
    Ok(())
}

#[test]
fn cross_check_mismatch_is_hard_error_and_does_not_register() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let dump = nar_bytes(&sample_tree());

    let wrong = "/nix/store/00000000000000000000000000000000-sample";
    let err = store
        .add_from_dump(
            "sample",
            DumpMethod::NixArchive,
            CaMethod::NixArchive,
            &[],
            &mut Cursor::new(&dump),
            &mut |_| Ok(wrong.to_string()),
        )
        .expect_err("mismatch must fail");
    let msg = err.to_string();
    assert!(
        msg.contains(wrong) && msg.contains("cross-check FAILED"),
        "error must print both paths: {msg}"
    );
    match err {
        EvalStoreError::PathMismatch {
            rust_path,
            nix_path,
            ..
        } => {
            assert_eq!(nix_path, wrong);
            assert_ne!(rust_path, nix_path);
            // Nothing registered under either path.
            assert!(!store.is_valid_path(&rust_path["/nix/store/".len()..]));
        }
        other => panic!("expected PathMismatch, got {other:?}"),
    }
    Ok(())
}

#[test]
fn text_method_matches_make_text() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let reference = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep".to_string();
    let contents = format!("refers to {reference}\n").into_bytes();

    let expected = StorePath::make_text(
        "builder.sh",
        &NixHash::new(HashAlgo::SHA256, Sha256::digest(&contents).to_vec())?,
        &[StorePath::parse(&reference)?],
    )?;

    let result = store.add_from_dump(
        "builder.sh",
        DumpMethod::Flat,
        CaMethod::Text,
        std::slice::from_ref(&reference),
        &mut Cursor::new(&contents),
        &mut |h| {
            let hash = NixHash::new(HashAlgo::SHA256, hex::decode(&h.content_sha256).unwrap())?;
            Ok(
                StorePath::make_text("builder.sh", &hash, &[StorePath::parse(&reference)?])?
                    .to_string(),
            )
        },
    )?;
    assert_eq!(result.path, expected.as_str());

    let info = store
        .query_path_info(expected.basename())?
        .expect("indexed");
    assert_eq!(info.references, vec![reference]);
    assert_eq!(info.ca.as_deref().map(|c| &c[..5]), Some("text:"));

    // Flat dump read-back: root is the file itself.
    let mut got = Vec::new();
    store.read_file(expected.basename(), "", &mut got)?;
    assert_eq!(got, contents);
    Ok(())
}

#[test]
fn flat_method_matches_make_fixed_output() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let contents = b"flat contents".to_vec();
    let expected = StorePath::make_fixed_output(
        "flat-file",
        &NixHash::new(HashAlgo::SHA256, Sha256::digest(&contents).to_vec())?,
        false,
        &[],
    )?;
    let result = store.add_from_dump(
        "flat-file",
        DumpMethod::Flat,
        CaMethod::Flat,
        &[],
        &mut Cursor::new(&contents),
        &mut |h| {
            let hash = NixHash::new(HashAlgo::SHA256, hex::decode(&h.content_sha256).unwrap())?;
            Ok(StorePath::make_fixed_output("flat-file", &hash, false, &[])?.to_string())
        },
    )?;
    assert_eq!(result.path, expected.as_str());
    Ok(())
}

/// The ATerm fixture used by write_derivation tests, with one input src
/// and one input drv so the reference set is non-trivial.
const DRV_ATERM: &str = r#"Derive([("out","/nix/store/abc-leaf","","")],[("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep.drv",["out"])],["/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-src"],"x86_64-linux","/bin/sh",["-c","echo hello"],[("name","leaf"),("out","/nix/store/abc-leaf"),("system","x86_64-linux")])"#;

fn expected_drv_path(name: &str, aterm: &str) -> StorePath {
    let refs = vec![
        StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-dep.drv").unwrap(),
        StorePath::parse("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-src").unwrap(),
    ];
    StorePath::make_text(
        name,
        &NixHash::new(HashAlgo::SHA256, Sha256::digest(aterm.as_bytes()).to_vec()).unwrap(),
        &refs,
    )
    .unwrap()
}

/// Drvs are memory-only (ADR-024): served from the in-process map for
/// the lifetime of the store, never written to the pack store, gone
/// after close.
#[test]
fn write_derivation_is_memory_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let nix_path = expected_drv_path("leaf.drv", DRV_ATERM);

    let path = store.write_derivation("leaf.drv", DRV_ATERM.as_bytes(), nix_path.as_str())?;
    assert_eq!(path, nix_path.as_str());

    let basename = nix_path.basename();
    assert!(store.is_valid_path(basename));

    // The path's content is the original ATerm bytes (readDerivation).
    let mut aterm_back = Vec::new();
    store.read_file(basename, "", &mut aterm_back)?;
    assert_eq!(aterm_back, DRV_ATERM.as_bytes());

    // lstat + NAR regeneration agree with a plain regular-file node.
    assert_eq!(
        store.lstat(basename, "")?,
        Some(PathStat::Regular {
            size: DRV_ATERM.len() as u64,
            executable: false
        })
    );
    let mut regen = Vec::new();
    store.nar_from_path(basename, &mut regen)?;
    let expected_nar = nar_bytes(&NarNode::Regular {
        executable: false,
        contents: DRV_ATERM.as_bytes().to_vec(),
    });
    assert_eq!(regen, expected_nar);

    // Info carries text CA + both references.
    let info = store.query_path_info(basename)?.expect("known");
    assert_eq!(info.references.len(), 2);
    assert_eq!(info.ca.as_deref().map(|c| &c[..5]), Some("text:"));

    // No drv write reached the pack store: zero records were written by
    // this whole test...
    for op in ["dirblob_write", "fetched_write", "meta_write"] {
        assert_eq!(store.stats().count(op), 0, "{op} after write_derivation");
    }
    drop(store);
    // ...and a fresh process does not know the path (memory-only).
    let reopened = open_store(&dir);
    assert!(
        !reopened.is_valid_path(basename),
        "drv path must die with the process"
    );
    // Belt and braces: no ATerm bytes anywhere in the CAS directory.
    for entry in walk_files(&dir.path().join("cas")) {
        let data = std::fs::read(&entry)?;
        assert!(
            !data.windows(7).any(|w| w == b"Derive("),
            "drv bytes leaked into {entry:?}"
        );
    }
    Ok(())
}

#[test]
fn write_derivation_mismatch_prints_both_paths() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let wrong = "/nix/store/00000000000000000000000000000000-leaf.drv";
    let err = store
        .write_derivation("leaf.drv", DRV_ATERM.as_bytes(), wrong)
        .expect_err("must fail");
    let rust_path = expected_drv_path("leaf.drv", DRV_ATERM);
    let msg = err.to_string();
    assert!(msg.contains(wrong), "missing nix path: {msg}");
    assert!(msg.contains(rust_path.as_str()), "missing rust path: {msg}");
    Ok(())
}

#[test]
fn add_nar_verifies_claimed_hash() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let dump = nar_bytes(&sample_tree());
    let nar_hash = hex::encode(Sha256::digest(&dump));
    let path = "/nix/store/cccccccccccccccccccccccccccccccc-prebuilt".to_string();

    let info = ProvidedInfo {
        path: path.clone(),
        nar_hash: nar_hash.clone(),
        nar_size: dump.len() as u64,
        references: vec![],
        ca: None,
    };
    store.add_nar(&info, &mut Cursor::new(&dump))?;
    assert!(store.is_valid_path("cccccccccccccccccccccccccccccccc-prebuilt"));

    // Wrong claimed hash → hard error, not registered.
    let bad = ProvidedInfo {
        path: "/nix/store/dddddddddddddddddddddddddddddddd-bad".to_string(),
        nar_hash: "ff".repeat(32),
        nar_size: dump.len() as u64,
        references: vec![],
        ca: None,
    };
    let err = store
        .add_nar(&bad, &mut Cursor::new(&dump))
        .expect_err("must fail");
    assert!(err.to_string().contains("NAR hash cross-check FAILED"));
    assert!(!store.is_valid_path("dddddddddddddddddddddddddddddddd-bad"));
    Ok(())
}

/// Basenames arrive over FFI; anything that isn't a store-path basename
/// must behave like an absent object — never reach a filesystem join.
#[test]
fn traversal_basenames_behave_as_absent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    for bad in ["../../../etc/passwd", "..", "x/../y"] {
        assert!(!store.is_valid_path(bad), "is_valid_path({bad:?})");
        assert!(
            store.query_path_info(bad)?.is_none(),
            "query_path_info({bad:?})"
        );
        assert_eq!(store.lstat(bad, "")?, None, "lstat({bad:?})");
        let mut sink = Vec::new();
        let err = store.read_file(bad, "", &mut sink).expect_err("read_file");
        assert!(matches!(err, EvalStoreError::ForeignPath(_)), "got: {err}");
        let err = store
            .nar_from_path(bad, &mut sink)
            .expect_err("nar_from_path");
        assert!(matches!(err, EvalStoreError::ForeignPath(_)), "got: {err}");
    }
    Ok(())
}

#[test]
fn foreign_path_errors_name_m1() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let mut sink = Vec::new();
    let err = store
        .nar_from_path("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-foreign", &mut sink)
        .expect_err("foreign path must error");
    assert!(matches!(err, EvalStoreError::ForeignPath(_)));
    assert!(err.to_string().contains("M1"), "must name M1: {err}");
    // lstat on a foreign path is a clean miss, not an error (whole-store
    // accessor semantics).
    assert_eq!(
        store.lstat("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-foreign", "")?,
        None
    );
    Ok(())
}

/// Warm re-ingest of identical content writes zero new pack records —
/// content addressing dedups files, directory blobs, and the path-meta
/// record alike.
#[test]
fn repeat_ingest_writes_no_new_records() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let dump = nar_bytes(&sample_tree());
    let ingest = || {
        store.add_from_dump(
            "sample",
            DumpMethod::NixArchive,
            CaMethod::NixArchive,
            &[],
            &mut Cursor::new(&dump),
            &mut |h| Ok(nix_path_recursive("sample", h)),
        )
    };
    ingest()?;
    let writes = |op: &str| store.stats().count(op);
    let before = (
        writes("fetched_write"),
        writes("dirblob_write"),
        writes("meta_write"),
    );
    assert!(before.0 > 0 && before.1 > 0 && before.2 > 0, "{before:?}");
    ingest()?;
    let after = (
        writes("fetched_write"),
        writes("dirblob_write"),
        writes("meta_write"),
    );
    assert_eq!(before, after, "warm re-ingest must dedup every record");
    assert!(
        writes("fetched_dedup") > 0 && writes("dirblob_dedup") > 0,
        "dedup counters must show the warm hits"
    );
    Ok(())
}

fn walk_files(dir: &std::path::Path) -> impl Iterator<Item = std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                stack.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    files.into_iter()
}

/// Reads must bump the path's root last-use clock — the LRU the pack
/// store GC evicts by. Touches are batched in-process and persisted at
/// flush (ADR-024: batched index records, not per-read appends).
#[test]
fn access_touches_root_lru_clock() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let cas = dir.path().join("cas");
    let dump = nar_bytes(&sample_tree());
    let basename = {
        let store = open_store(&dir);
        let result = store.add_from_dump(
            "sample",
            DumpMethod::NixArchive,
            CaMethod::NixArchive,
            &[],
            &mut Cursor::new(&dump),
            &mut |h| Ok(nix_path_recursive("sample", h)),
        )?;
        result.path.strip_prefix("/nix/store/").unwrap().to_string()
    };

    // Backdate the root's clock directly in the pack store.
    {
        let mut pack = rio_packstore::PackStore::open(&cas, rio_packstore::Options::default())?;
        let digests = pack.root_digests(&basename).expect("root exists");
        pack.add_root_at(&basename, &digests, 1_000)?;
        pack.flush()?;
    }

    // An accessor read through a fresh store + flush-on-drop...
    {
        let store = open_store(&dir);
        let _ = store.lstat(&basename, "data.txt")?;
    }

    // ...must have advanced the clock past the backdated value.
    let pack = rio_packstore::PackStore::open(&cas, rio_packstore::Options::default())?;
    let last_use = pack.root_last_use(&basename).expect("root exists");
    assert!(
        last_use > 1_000,
        "read must touch the LRU clock: {last_use}"
    );
    Ok(())
}

/// Root digest-list order is a write-side convention, never a readback
/// contract: the pack store re-encodes root lists on GC/touch, so a
/// reordered (or future deduped) list must still resolve the path-meta
/// record — readback finds it by record kind, not position.
#[test]
fn readback_survives_reordered_root_digest_list() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let cas = dir.path().join("cas");
    let dump = nar_bytes(&sample_tree());
    let basename = {
        let store = open_store(&dir);
        let result = store.add_from_dump(
            "sample",
            DumpMethod::NixArchive,
            CaMethod::NixArchive,
            &[],
            &mut Cursor::new(&dump),
            &mut |h| Ok(nix_path_recursive("sample", h)),
        )?;
        result.path.strip_prefix("/nix/store/").unwrap().to_string()
    };

    // Adversarial rewrite: re-record the root with its digest list
    // reversed (meta record now LAST).
    {
        let mut pack = rio_packstore::PackStore::open(&cas, rio_packstore::Options::default())?;
        let mut digests = pack.root_digests(&basename).expect("root exists");
        digests.reverse();
        pack.add_root(&basename, &digests)?;
        pack.flush()?;
    }

    let store = open_store(&dir);
    assert!(store.is_valid_path(&basename));
    let info = store
        .query_path_info(&basename)?
        .expect("meta must resolve from a reordered root list");
    assert_eq!(info.nar_size, dump.len() as u64);
    let mut regen = Vec::new();
    store.nar_from_path(&basename, &mut regen)?;
    assert_eq!(regen, dump);
    Ok(())
}

/// Set a file's mtime into the past so its fingerprint record is trusted
/// (the racy-fingerprint rule distrusts records made within the coarse-
/// clock slack of the file's mtime).
fn backdate(path: &std::path::Path) -> anyhow::Result<()> {
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    // Read-only open: works for directories too (we own the files).
    let f = std::fs::OpenOptions::new().read(true).open(path)?;
    f.set_times(std::fs::FileTimes::new().set_modified(old))?;
    Ok(())
}

#[test]
fn fingerprint_hit_and_invalidation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);

    // Ingest a file so the CAS entry exists.
    let contents = b"source file".to_vec();
    let result = store.add_from_dump(
        "src.txt",
        DumpMethod::Flat,
        CaMethod::NixArchive,
        &[],
        &mut Cursor::new(&contents),
        &mut |h| Ok(nix_path_recursive("src.txt", h)),
    )?;

    let fs_path = dir.path().join("src.txt");
    std::fs::write(&fs_path, &contents)?;
    backdate(&fs_path)?;
    let key = EvalStore::method_key("src.txt", CaMethod::NixArchive, &[]);
    let fs_path_str = fs_path.to_str().unwrap();

    assert_eq!(store.fingerprint_lookup(fs_path_str, &key)?, None);
    store.fingerprint_record(fs_path_str, &key, &result.path)?;
    assert_eq!(
        store.fingerprint_lookup(fs_path_str, &key)?,
        Some(result.path.clone()),
        "unchanged file must hit"
    );

    // A different method must not hit.
    let other_method = EvalStore::method_key("src.txt", CaMethod::Flat, &[]);
    assert_eq!(store.fingerprint_lookup(fs_path_str, &other_method)?, None);

    // A different store-path name must not hit: the same file added as
    // `builtins.path { path = ./x; name = "other"; }` mints a different
    // store path, and a stale hit would silently return the wrong one.
    let other_name = EvalStore::method_key("other", CaMethod::NixArchive, &[]);
    assert_eq!(store.fingerprint_lookup(fs_path_str, &other_name)?, None);

    // Rewriting the file (new inode and/or mtime) invalidates.
    std::fs::remove_file(&fs_path)?;
    std::fs::write(&fs_path, b"changed")?;
    assert_eq!(
        store.fingerprint_lookup(fs_path_str, &key)?,
        None,
        "changed file must miss"
    );
    Ok(())
}

/// Racy-fingerprint rule (ADR-024): a record whose file mtime is within
/// the coarse-clock slack of the record's write time could mask a
/// same-size in-place rewrite, so lookup must distrust it (re-hash)
/// instead of serving a possibly-stale store path.
#[test]
fn fingerprint_distrusts_record_within_mtime_slack() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let contents = b"freshly written".to_vec();
    let result = store.add_from_dump(
        "fresh.txt",
        DumpMethod::Flat,
        CaMethod::NixArchive,
        &[],
        &mut Cursor::new(&contents),
        &mut |h| Ok(nix_path_recursive("fresh.txt", h)),
    )?;

    let fs_path = dir.path().join("fresh.txt");
    std::fs::write(&fs_path, &contents)?;
    let key = EvalStore::method_key("fresh.txt", CaMethod::NixArchive, &[]);
    let fs_path_str = fs_path.to_str().unwrap();

    // Record immediately after writing: mtime ≈ record time → distrusted.
    store.fingerprint_record(fs_path_str, &key, &result.path)?;
    assert_eq!(
        store.fingerprint_lookup(fs_path_str, &key)?,
        None,
        "record within the mtime slack must be distrusted"
    );

    // Once the mtime is safely older than the record, it hits.
    backdate(&fs_path)?;
    store.fingerprint_record(fs_path_str, &key, &result.path)?;
    assert_eq!(
        store.fingerprint_lookup(fs_path_str, &key)?,
        Some(result.path),
        "backdated file must hit"
    );
    Ok(())
}

/// Fingerprint records survive a store close: the single-file table is
/// the persistent asset a second invocation skips hashing with.
#[test]
fn fingerprints_persist_across_reopen() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let contents = b"persisted".to_vec();
    let fs_path = dir.path().join("p.txt");
    std::fs::write(&fs_path, &contents)?;
    backdate(&fs_path)?;
    let key = EvalStore::method_key("p.txt", CaMethod::NixArchive, &[]);
    let fs_path_str = fs_path.to_str().unwrap();

    let path = {
        let store = open_store(&dir);
        let result = store.add_from_dump(
            "p.txt",
            DumpMethod::Flat,
            CaMethod::NixArchive,
            &[],
            &mut Cursor::new(&contents),
            &mut |h| Ok(nix_path_recursive("p.txt", h)),
        )?;
        store.fingerprint_record(fs_path_str, &key, &result.path)?;
        result.path
    };

    let store = open_store(&dir);
    assert_eq!(
        store.fingerprint_lookup(fs_path_str, &key)?,
        Some(path),
        "record lost across reopen"
    );
    assert_eq!(store.stats().count("fingerprint_hit"), 1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Local source trees (add_source_tree): the not-a-mirror rule.
// ---------------------------------------------------------------------------

/// Build a small on-disk source tree and return its root.
fn write_source_tree(root: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(root.join("bin"))?;
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(root.join("bin/tool"), b"#!/bin/sh\necho hi\n")?;
    std::fs::set_permissions(
        root.join("bin/tool"),
        std::fs::Permissions::from_mode(0o755),
    )?;
    std::fs::write(root.join("src/lib.rs"), b"pub fn answer() -> u32 { 42 }\n")?;
    std::fs::write(root.join("README"), b"docs\n")?;
    std::os::unix::fs::symlink("bin/tool", root.join("latest"))?;
    Ok(())
}

/// The nix-side path computation for add_source_tree cross-checks.
fn nix_path_for_tree(
    name: &'static str,
) -> impl FnMut(&rio_evalstore::store::AddHashes) -> Result<String, EvalStoreError> {
    move |h| Ok(nix_path_recursive(name, h))
}

#[test]
fn add_source_tree_serves_reads_from_the_origin() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let tree = dir.path().join("tree");
    write_source_tree(&tree)?;

    let result = store.add_source_tree(
        tree.to_str().unwrap(),
        "tree",
        &[],
        &mut nix_path_for_tree("tree"),
    )?;

    // Identity parity with rio-nix's own dump of the same tree.
    let mut dump = Vec::new();
    nar::dump_path_streaming(&tree, &mut dump)?;
    assert_eq!(result.nar_sha256, hex::encode(Sha256::digest(&dump)));
    let nar_hash = NixHash::new(HashAlgo::SHA256, Sha256::digest(&dump).to_vec())?;
    let expected = StorePath::make_fixed_output("tree", &nar_hash, true, &[])?;
    assert_eq!(result.path, expected.as_str());
    let basename = expected.basename();

    // Read-back walks the dir blobs; file contents come from the origin.
    assert_eq!(store.lstat(basename, "")?, Some(PathStat::Directory));
    let mut content = Vec::new();
    store.read_file(basename, "src/lib.rs", &mut content)?;
    assert_eq!(content, b"pub fn answer() -> u32 { 42 }\n");
    assert_eq!(store.read_link(basename, "latest")?, b"bin/tool");

    // NAR regeneration (origin splice) is byte-identical to the dump.
    let mut regen = Vec::new();
    store.nar_from_path(basename, &mut regen)?;
    assert_eq!(regen, dump);
    Ok(())
}

/// Not-a-mirror: local tree file CONTENT must not be copied into the
/// CAS; chunk lists (FILE_CHUNK_META) must.
#[test]
fn add_source_tree_stores_no_file_content() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let tree = dir.path().join("tree");
    write_source_tree(&tree)?;
    {
        let store = open_store(&dir);
        store.add_source_tree(
            tree.to_str().unwrap(),
            "tree",
            &[],
            &mut nix_path_for_tree("tree"),
        )?;
        assert_eq!(store.stats().count("fetched_write"), 0);
        assert!(store.stats().count("chunkmeta_write") > 0);
    }

    // Inspect the packs directly: no record keyed by any file's blake3
    // (content would be), but the chunk-meta record for each file (one
    // whole-file chunk at this size: digest ‖ digest ‖ 0u64 ‖ len) IS
    // present.
    let pack =
        rio_packstore::PackStore::open(dir.path().join("cas"), rio_packstore::Options::default())?;
    for rel in ["bin/tool", "src/lib.rs", "README"] {
        let data = std::fs::read(tree.join(rel))?;
        let file_digest = rio_packstore::Digest::of(&data);
        assert!(
            !pack.contains(&file_digest),
            "{rel}: file content leaked into the CAS"
        );
        let mut payload = Vec::new();
        payload.extend_from_slice(&file_digest.0);
        payload.extend_from_slice(&file_digest.0);
        payload.extend_from_slice(&0u64.to_le_bytes());
        payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
        assert!(
            pack.contains(&rio_packstore::Digest::of(&payload)),
            "{rel}: chunk-meta record missing"
        );
    }
    Ok(())
}

/// The not-a-mirror rule's read side: a mutated origin file fails the
/// digest verify with a named error instead of serving stale or wrong
/// bytes.
#[test]
fn mutated_origin_is_a_named_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let tree = dir.path().join("tree");
    write_source_tree(&tree)?;
    let result = store.add_source_tree(
        tree.to_str().unwrap(),
        "tree",
        &[],
        &mut nix_path_for_tree("tree"),
    )?;
    let basename = result.path.strip_prefix("/nix/store/").unwrap();

    std::fs::write(tree.join("README"), b"mutated!\n")?;
    let mut sink = Vec::new();
    let err = store
        .read_file(basename, "README", &mut sink)
        .expect_err("mutated origin must fail");
    assert!(
        matches!(err, EvalStoreError::OriginChanged { .. }),
        "got: {err}"
    );

    // A deleted origin is its own named error.
    std::fs::remove_file(tree.join("README"))?;
    let err = store
        .read_file(basename, "README", &mut sink)
        .expect_err("deleted origin must fail");
    assert!(
        matches!(err, EvalStoreError::OriginUnreadable { .. }),
        "got: {err}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// The 92×-pathology gate: warm reads decode each directory blob at most
// once and fingerprint hits re-hash nothing.
// ---------------------------------------------------------------------------

/// Recursively walk every member of a store path through the public
/// read surface (the ops nix's accessor issues during eval).
fn full_walk(store: &EvalStore, basename: &str, rel: &str) -> anyhow::Result<()> {
    for (name, kind) in store.read_directory(basename, rel)? {
        let name = String::from_utf8(name)?;
        let child = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        assert!(store.lstat(basename, &child)?.is_some());
        match kind {
            EntryKind::Directory => full_walk(store, basename, &child)?,
            EntryKind::Regular => {
                let mut sink = Vec::new();
                store.read_file(basename, &child, &mut sink)?;
            }
            EntryKind::Symlink => {
                store.read_link(basename, &child)?;
            }
        }
    }
    Ok(())
}

/// Structural proof that the warm path cannot re-create the measured
/// 92× pathology: after first touch, repeated full walks of an
/// ingested tree perform ZERO additional directory-blob decodes (pure
/// cache hits), and a fingerprint hit performs zero re-hashing /
/// re-ingest work.
#[test]
fn warm_walks_decode_each_directory_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let tree = dir.path().join("tree");
    write_source_tree(&tree)?;
    // Three distinct directories: root, bin, src.
    let result = store.add_source_tree(
        tree.to_str().unwrap(),
        "tree",
        &[],
        &mut nix_path_for_tree("tree"),
    )?;
    let basename = result.path.strip_prefix("/nix/store/").unwrap().to_string();

    full_walk(&store, &basename, "")?;
    let decodes_after_first = store.stats().count("dir_decode");
    assert_eq!(
        decodes_after_first, 3,
        "first walk decodes each distinct directory exactly once"
    );

    for _ in 0..10 {
        full_walk(&store, &basename, "")?;
        let mut nar = Vec::new();
        store.nar_from_path(&basename, &mut nar)?;
    }
    assert_eq!(
        store.stats().count("dir_decode"),
        decodes_after_first,
        "warm walks must be pure cache hits — zero decodes beyond first touch"
    );
    assert!(store.stats().count("dir_cache_hit") > 0);

    // Fingerprint hit path: zero re-hash, zero re-ingest, zero writes.
    backdate(&tree)?;
    let key = EvalStore::method_key("tree", CaMethod::NixArchive, &[]);
    store.fingerprint_record(tree.to_str().unwrap(), &key, &result.path)?;
    let writes_before = (
        store.stats().count("dirblob_write"),
        store.stats().count("chunkmeta_write"),
        store.stats().count("meta_write"),
        store.stats().count("add_source_tree"),
    );
    assert_eq!(
        store.fingerprint_lookup(tree.to_str().unwrap(), &key)?,
        Some(result.path.clone()),
        "fingerprint must hit"
    );
    assert_eq!(store.stats().count("fingerprint_hit"), 1);
    let writes_after = (
        store.stats().count("dirblob_write"),
        store.stats().count("chunkmeta_write"),
        store.stats().count("meta_write"),
        store.stats().count("add_source_tree"),
    );
    assert_eq!(
        writes_before, writes_after,
        "a fingerprint hit must not ingest, hash, or write anything"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// FFI smoke test — through the extern "C" surface end to end.
// ---------------------------------------------------------------------------

mod ffi_smoke {
    use super::*;
    use rio_evalstore::ffi::*;
    use std::ffi::{CStr, CString, c_char, c_int, c_void};

    struct ReadState {
        data: Vec<u8>,
        pos: usize,
    }

    unsafe extern "C" fn read_cb(
        ctx: *mut c_void,
        buf: *mut u8,
        cap: usize,
        n_read: *mut usize,
    ) -> c_int {
        let state = unsafe { &mut *ctx.cast::<ReadState>() };
        let n = cap.min(state.data.len() - state.pos);
        unsafe {
            std::ptr::copy_nonoverlapping(state.data.as_ptr().add(state.pos), buf, n);
            *n_read = n;
        }
        state.pos += n;
        0
    }

    unsafe extern "C" fn write_cb(ctx: *mut c_void, data: *const u8, len: usize) -> c_int {
        let out = unsafe { &mut *ctx.cast::<Vec<u8>>() };
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(data, len) });
        0
    }

    /// `ctx` is the store-path name as a NUL-terminated C string;
    /// computes the recursive fixed-output path from `nar_sha256`.
    unsafe extern "C" fn path_cb(
        ctx: *mut c_void,
        hashes_json: *const c_char,
        out_path: *mut c_char,
        out_cap: usize,
    ) -> c_int {
        let name = unsafe { CStr::from_ptr(ctx.cast::<c_char>()) }
            .to_str()
            .unwrap();
        let json = unsafe { CStr::from_ptr(hashes_json) }.to_str().unwrap();
        let hashes: BTreeMap<String, serde_json::Value> = serde_json::from_str(json).unwrap();
        let nar_hex = hashes["nar_sha256"].as_str().unwrap();
        let h = NixHash::new(HashAlgo::SHA256, hex::decode(nar_hex).unwrap()).unwrap();
        let path = StorePath::make_fixed_output(name, &h, true, &[]).unwrap();
        let cpath = CString::new(path.as_str()).unwrap();
        let bytes = cpath.as_bytes_with_nul();
        if bytes.len() > out_cap {
            return 1;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out_path, bytes.len());
        }
        0
    }

    fn take_string(p: *mut c_char) -> Option<String> {
        if p.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        unsafe { rio_string_free(p) };
        Some(s)
    }

    #[test]
    fn end_to_end_through_ffi() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cas = CString::new(dir.path().join("cas").to_str().unwrap())?;

        let mut store: *mut EvalStore = std::ptr::null_mut();
        let mut err: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            unsafe { rio_store_open(cas.as_ptr(), &mut store, &mut err) },
            RIO_OK,
            "{:?}",
            take_string(err)
        );

        // Ingest a NAR dump.
        let dump = nar_bytes(&sample_tree());
        let mut read_state = ReadState {
            data: dump.clone(),
            pos: 0,
        };
        let name = CString::new("ffi-sample")?;
        let refs = CString::new("[]")?;
        let mut out_json: *mut c_char = std::ptr::null_mut();
        let rc = unsafe {
            rio_add_from_dump(
                store,
                name.as_ptr(),
                1, // NixArchive dump
                1, // NixArchive CA
                refs.as_ptr(),
                read_cb,
                (&raw mut read_state).cast::<c_void>(),
                path_cb,
                name.as_ptr().cast_mut().cast::<c_void>(),
                &mut out_json,
                &mut err,
            )
        };
        assert_eq!(rc, RIO_OK, "{:?}", take_string(err));
        let result: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&take_string(out_json).expect("result json"))?;
        let path = result["path"].as_str().unwrap().to_string();
        let basename = path.strip_prefix("/nix/store/").unwrap().to_string();
        let cbase = CString::new(basename)?;

        // isValidPath through FFI.
        let mut valid: c_int = 0;
        assert_eq!(
            unsafe { rio_is_valid_path(store, cbase.as_ptr(), &mut valid, &mut err) },
            RIO_OK
        );
        assert_eq!(valid, 1);

        // queryPathInfo through FFI.
        let mut info_json: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            unsafe { rio_query_path_info(store, cbase.as_ptr(), &mut info_json, &mut err) },
            RIO_OK
        );
        let info: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&take_string(info_json).expect("info"))?;
        assert_eq!(info["nar_size"].as_u64(), Some(dump.len() as u64));

        // readDirectory through FFI: flat buffer — u32 count, then per
        // entry u8 kind, u32 name_len, raw name bytes (little-endian).
        let rel = CString::new("")?;
        let mut dir_buf: *mut u8 = std::ptr::null_mut();
        let mut dir_len: usize = 0;
        assert_eq!(
            unsafe {
                rio_read_directory(
                    store,
                    cbase.as_ptr(),
                    rel.as_ptr(),
                    &mut dir_buf,
                    &mut dir_len,
                    &mut err,
                )
            },
            RIO_OK
        );
        let buf = unsafe { std::slice::from_raw_parts(dir_buf, dir_len) };
        let mut dirents: BTreeMap<String, u8> = BTreeMap::new();
        let mut pos = 0usize;
        let rd32 = |buf: &[u8], pos: &mut usize| {
            let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            v
        };
        let count = rd32(buf, &mut pos);
        for _ in 0..count {
            let kind = buf[pos];
            pos += 1;
            let name_len = rd32(buf, &mut pos) as usize;
            let name = String::from_utf8(buf[pos..pos + name_len].to_vec())?;
            pos += name_len;
            dirents.insert(name, kind);
        }
        assert_eq!(pos, dir_len, "buffer fully consumed");
        unsafe { rio_bytes_free(dir_buf, dir_len) };
        assert_eq!(dirents["bin"], RIO_NODE_DIRECTORY);
        assert_eq!(dirents["data.txt"], RIO_NODE_REGULAR);
        assert_eq!(dirents["link"], RIO_NODE_SYMLINK);

        // lstat through FFI: flat out-struct, kind 0 = missing.
        let mut st = RioStat {
            kind: 99,
            executable: 99,
            size: 99,
        };
        let rel_file = CString::new("data.txt")?;
        assert_eq!(
            unsafe { rio_lstat(store, cbase.as_ptr(), rel_file.as_ptr(), &mut st, &mut err) },
            RIO_OK
        );
        assert_eq!(st.kind, RIO_NODE_REGULAR);
        assert_eq!(st.executable, 0);
        assert_eq!(st.size, b"payload\n".len() as u64);
        let rel_missing = CString::new("no-such-entry")?;
        assert_eq!(
            unsafe {
                rio_lstat(
                    store,
                    cbase.as_ptr(),
                    rel_missing.as_ptr(),
                    &mut st,
                    &mut err,
                )
            },
            RIO_OK
        );
        assert_eq!(st.kind, RIO_NODE_MISSING);

        // NAR regeneration through FFI is byte-identical.
        let mut regen: Vec<u8> = Vec::new();
        assert_eq!(
            unsafe {
                rio_nar_from_path(
                    store,
                    cbase.as_ptr(),
                    write_cb,
                    (&raw mut regen).cast::<c_void>(),
                    &mut err,
                )
            },
            RIO_OK
        );
        assert_eq!(regen, dump);

        // Foreign path through FFI → RIO_UNSUPPORTED with an M1 message.
        let foreign = CString::new("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-foreign")?;
        let mut sink: Vec<u8> = Vec::new();
        let rc = unsafe {
            rio_nar_from_path(
                store,
                foreign.as_ptr(),
                write_cb,
                (&raw mut sink).cast::<c_void>(),
                &mut err,
            )
        };
        assert_eq!(rc, RIO_UNSUPPORTED);
        let msg = take_string(err).expect("error message");
        assert!(msg.contains("M1"), "{msg}");

        unsafe { rio_store_free(store) };
        Ok(())
    }

    /// `rio_add_source_tree` end to end: ingests a local tree through
    /// the two-plane pipeline, returns the cross-checked path as JSON,
    /// and reads file bytes back from the origin.
    #[test]
    fn add_source_tree_through_ffi() -> anyhow::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir()?;
        let cas = CString::new(dir.path().join("cas").to_str().unwrap())?;
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(&tree)?;
        std::fs::write(tree.join("data.txt"), b"tree payload\n")?;
        // Non-UTF-8 entry name: only local-tree ingest can produce one
        // (the NAR dump path goes through rio-nix's UTF-8 NarNode), and
        // the flat readDirectory buffer must hand it back byte-exact.
        let weird_name: &[u8] = b"w\xff\xfeird";
        std::fs::write(
            tree.join(std::ffi::OsStr::from_bytes(weird_name)),
            b"bytes\n",
        )?;

        let mut store: *mut EvalStore = std::ptr::null_mut();
        let mut err: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            unsafe { rio_store_open(cas.as_ptr(), &mut store, &mut err) },
            RIO_OK,
            "{:?}",
            take_string(err)
        );

        let fs_path = CString::new(tree.to_str().unwrap())?;
        let name = CString::new("ffi-tree")?;
        let refs = CString::new("[]")?;
        let mut out_json: *mut c_char = std::ptr::null_mut();
        let rc = unsafe {
            rio_add_source_tree(
                store,
                fs_path.as_ptr(),
                name.as_ptr(),
                refs.as_ptr(),
                path_cb,
                name.as_ptr().cast_mut().cast::<c_void>(),
                &mut out_json,
                &mut err,
            )
        };
        assert_eq!(rc, RIO_OK, "{:?}", take_string(err));
        let result: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&take_string(out_json).expect("result json"))?;
        let path = result["path"].as_str().unwrap().to_string();

        // Independent recomputation from rio-nix's own dump. The fs
        // dumper requires UTF-8 names, so hash the canonical token
        // stream assembled by hand for this two-entry tree instead.
        let mut dump = Vec::new();
        {
            use rio_nix::nar::frame;
            let w = &mut dump;
            frame::magic(w)?;
            frame::node_open(w)?;
            frame::directory_open(w)?;
            for (name, contents) in [
                (&b"data.txt"[..], &b"tree payload\n"[..]),
                (weird_name, &b"bytes\n"[..]),
            ] {
                frame::entry_open(w, name)?;
                frame::node_open(w)?;
                frame::regular_header(w, false, contents.len() as u64)?;
                w.extend_from_slice(contents);
                frame::contents_padding(w, contents.len() as u64)?;
                frame::node_close(w)?;
                frame::entry_close(w)?;
            }
            frame::node_close(w)?;
        }
        let h = NixHash::new(HashAlgo::SHA256, Sha256::digest(&dump).to_vec())?;
        assert_eq!(
            path,
            StorePath::make_fixed_output("ffi-tree", &h, true, &[])?.as_str()
        );

        // Read-back through FFI serves bytes from the origin tree.
        let basename = path.strip_prefix("/nix/store/").unwrap();
        let cbase = CString::new(basename)?;
        let rel = CString::new("data.txt")?;
        let mut content: Vec<u8> = Vec::new();
        assert_eq!(
            unsafe {
                rio_read_file(
                    store,
                    cbase.as_ptr(),
                    rel.as_ptr(),
                    write_cb,
                    (&raw mut content).cast::<c_void>(),
                    &mut err,
                )
            },
            RIO_OK
        );
        assert_eq!(content, b"tree payload\n");

        // The non-UTF-8 name round-trips through the flat readDirectory
        // buffer as raw bytes (no lossy mangling, no refusal).
        let rel_root = CString::new("")?;
        let mut dir_buf: *mut u8 = std::ptr::null_mut();
        let mut dir_len: usize = 0;
        assert_eq!(
            unsafe {
                rio_read_directory(
                    store,
                    cbase.as_ptr(),
                    rel_root.as_ptr(),
                    &mut dir_buf,
                    &mut dir_len,
                    &mut err,
                )
            },
            RIO_OK,
            "{:?}",
            take_string(err)
        );
        let buf = unsafe { std::slice::from_raw_parts(dir_buf, dir_len) };
        let mut names: Vec<Vec<u8>> = Vec::new();
        let mut pos = 0usize;
        let count = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap());
        pos += 4;
        for _ in 0..count {
            assert_eq!(buf[pos], RIO_NODE_REGULAR);
            pos += 1;
            let name_len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;
            names.push(buf[pos..pos + name_len].to_vec());
            pos += name_len;
        }
        assert_eq!(pos, dir_len, "buffer fully consumed");
        unsafe { rio_bytes_free(dir_buf, dir_len) };
        assert_eq!(names, vec![b"data.txt".to_vec(), weird_name.to_vec()]);

        unsafe { rio_store_free(store) };
        Ok(())
    }
}

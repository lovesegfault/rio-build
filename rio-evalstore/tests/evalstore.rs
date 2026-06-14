//! Integration tests for the eval-store core and its FFI surface.
//!
//! The "nix side" of every cross-check is simulated with an independent
//! re-computation through rio-nix's public API — agreement exercises the
//! success path, a deliberately wrong path exercises the hard-error path.

use std::collections::BTreeMap;
use std::io::Cursor;

use rio_evalstore::store::ProvidedInfo;
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

    // NAR regeneration is byte-identical (framing from DAG).
    let mut regen = Vec::new();
    store.nar_from_path(basename, &mut regen)?;
    assert_eq!(regen, dump, "regenerated NAR must be byte-identical");

    // Read-back ops.
    let root = store.lstat(basename, "")?.expect("root exists");
    assert!(matches!(
        root,
        rio_evalstore::cas::DagNode::Directory { .. }
    ));
    let dirents = store.read_directory(basename, "")?;
    assert_eq!(
        dirents.keys().collect::<Vec<_>>(),
        vec!["bin", "data.txt", "link"]
    );
    let mut content = Vec::new();
    store.read_file(basename, "bin/tool", &mut content)?;
    assert_eq!(content, b"#!/bin/sh\necho hi\n");
    assert_eq!(store.read_link(basename, "link")?, "data.txt");
    assert_eq!(store.lstat(basename, "missing")?, None);

    // queryPathFromHashPart finds it.
    let hash_part = &basename[..32];
    assert_eq!(
        store.query_path_from_hash_part(hash_part)?,
        Some(expected.to_string())
    );
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

#[test]
fn write_derivation_cross_checks_and_stores_json() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let nix_path = expected_drv_path("leaf.drv", DRV_ATERM);
    let drv_json = br#"{"name":"leaf","version":4}"#;

    let path = store.write_derivation(
        "leaf.drv",
        DRV_ATERM.as_bytes(),
        drv_json,
        nix_path.as_str(),
    )?;
    assert_eq!(path, nix_path.as_str());

    let basename = nix_path.basename();
    assert!(store.is_valid_path(basename));

    // The drv JSON blob is captured (the canonical stored form).
    assert_eq!(
        store.read_drv_json(basename)?.as_deref(),
        Some(&drv_json[..])
    );

    // The path's content is the original ATerm bytes.
    let mut aterm_back = Vec::new();
    store.read_file(basename, "", &mut aterm_back)?;
    assert_eq!(aterm_back, DRV_ATERM.as_bytes());

    // Info carries text CA + both references.
    let info = store.query_path_info(basename)?.expect("indexed");
    assert_eq!(info.references.len(), 2);
    assert!(info.drv_json_blob.is_some());
    Ok(())
}

#[test]
fn write_derivation_mismatch_prints_both_paths() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let wrong = "/nix/store/00000000000000000000000000000000-leaf.drv";
    let err = store
        .write_derivation("leaf.drv", DRV_ATERM.as_bytes(), b"{}", wrong)
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

#[test]
fn repeat_ingest_writes_no_new_blobs() -> anyhow::Result<()> {
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
    let count_blobs = |root: &std::path::Path| -> usize { walkdir_count(&root.join("blobs")) };
    let cas_root = dir.path().join("cas");
    let before = count_blobs(&cas_root);
    ingest()?;
    assert_eq!(
        count_blobs(&cas_root),
        before,
        "warm re-ingest must dedup all blobs"
    );
    Ok(())
}

fn walkdir_count(dir: &std::path::Path) -> usize {
    walk_files(dir).count()
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

/// Ingest is the integrity boundary: the blob key is derived by hashing
/// the ingested bytes, so what enters the CAS is correct by construction.
/// Local reads trust the disk (ADR-024 integrity model) — this pins the
/// key/content agreement at the ingest edge.
#[test]
fn ingest_derives_blob_keys_from_content() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    store.add_from_dump(
        "sample",
        DumpMethod::NixArchive,
        CaMethod::NixArchive,
        &[],
        &mut Cursor::new(nar_bytes(&sample_tree())),
        &mut |h| Ok(nix_path_recursive("sample", h)),
    )?;

    // Every stored blob's filename equals the BLAKE3 of its contents.
    for blob in walk_files(&dir.path().join("cas").join("blobs")) {
        let name = blob.file_name().unwrap().to_str().unwrap().to_owned();
        let actual = blake3::hash(&std::fs::read(&blob)?).to_hex();
        assert_eq!(name, actual.as_str(), "blob key must match content hash");
    }
    Ok(())
}

/// Accesses must bump the path entry's mtime — the explicit LRU clock the
/// future CAS GC sweep evicts by (ADR-024). Kernel atime is not trusted.
#[test]
fn access_touches_entry_lru_clock() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);
    let dump = nar_bytes(&sample_tree());
    let result = store.add_from_dump(
        "sample",
        DumpMethod::NixArchive,
        CaMethod::NixArchive,
        &[],
        &mut Cursor::new(&dump),
        &mut |h| Ok(nix_path_recursive("sample", h)),
    )?;
    let basename = result.path.strip_prefix("/nix/store/").unwrap();
    let entry = dir
        .path()
        .join("cas")
        .join("index")
        .join(format!("{basename}.json"));

    // Backdate the entry, then access through each read surface and
    // assert the clock advanced again.
    let backdate = || -> anyhow::Result<std::time::SystemTime> {
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let f = std::fs::OpenOptions::new().write(true).open(&entry)?;
        f.set_times(std::fs::FileTimes::new().set_modified(old))?;
        Ok(old)
    };
    let mtime = || std::fs::metadata(&entry).unwrap().modified().unwrap();

    let old = backdate()?;
    assert!(store.is_valid_path(basename));
    assert!(mtime() > old, "isValidPath hit must touch the entry");

    let old = backdate()?;
    let _ = store.lstat(basename, "data.txt")?;
    assert!(mtime() > old, "accessor read must touch the entry");

    let old = backdate()?;
    let mut sink = Vec::new();
    store.nar_from_path(basename, &mut sink)?;
    assert!(mtime() > old, "narFromPath must touch the entry");

    // Blobs stay timestamp-free from the store's perspective: reads do
    // not touch them (reachability, not recency, will decide blobs).
    Ok(())
}

/// Set a file's mtime into the past so its fingerprint record is trusted
/// (the racy-fingerprint rule distrusts records made within the coarse-
/// clock slack of the file's mtime).
fn backdate(path: &std::path::Path) -> anyhow::Result<()> {
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.set_times(std::fs::FileTimes::new().set_modified(old))?;
    Ok(())
}

#[test]
fn fingerprint_hit_and_invalidation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = open_store(&dir);

    // Ingest a file so the index entry exists.
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

    unsafe extern "C" fn path_cb(
        _ctx: *mut c_void,
        hashes_json: *const c_char,
        out_path: *mut c_char,
        out_cap: usize,
    ) -> c_int {
        let json = unsafe { CStr::from_ptr(hashes_json) }.to_str().unwrap();
        let hashes: BTreeMap<String, serde_json::Value> = serde_json::from_str(json).unwrap();
        let nar_hex = hashes["nar_sha256"].as_str().unwrap();
        let h = NixHash::new(HashAlgo::SHA256, hex::decode(nar_hex).unwrap()).unwrap();
        let path = StorePath::make_fixed_output("ffi-sample", &h, true, &[]).unwrap();
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
                std::ptr::null_mut(),
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
}

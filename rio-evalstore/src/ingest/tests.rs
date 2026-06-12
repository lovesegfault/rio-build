//! Ingest pipeline tests: NAR sha256 parity against rio-nix's writer,
//! chunk-list parity against single-threaded FastCDC, determinism across
//! thread/budget settings, oversized-file admission, and clean error
//! joins.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fastcdc::v2020::FastCDC;
use rio_common::limits::{FASTCDC_AVG_BYTES, FASTCDC_MAX_BYTES, FASTCDC_MIN_BYTES};
use rio_nix::nar::frame;
use sha2::{Digest, Sha256};

use super::{IngestConfig, IngestError, IngestFile, IngestNode, IngestResult, ingest_tree};

/// Deterministic filler (xorshift64*) — incompressible enough to exercise
/// content-defined boundaries without a dev-dependency.
fn pseudo_random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut v = Vec::with_capacity(len + 8);
    while v.len() < len {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        v.extend_from_slice(&s.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    v.truncate(len);
    v
}

/// The canonical NAR bytes per rio-nix's writer (the workspace oracle).
fn rio_nix_nar(root: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    rio_nix::nar::dump_path_streaming(root, &mut buf).expect("oracle dump succeeds");
    buf
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn ingest_default(root: &Path) -> IngestResult {
    ingest_tree(root, &IngestConfig::default()).expect("ingest succeeds")
}

/// Assert NAR identity parity between the pipeline and the rio-nix writer.
fn assert_nar_parity(root: &Path, result: &IngestResult) {
    let nar = rio_nix_nar(root);
    assert_eq!(result.nar_size, nar.len() as u64, "nar_size mismatch");
    assert_eq!(result.nar_sha256, sha256(&nar), "nar_sha256 mismatch");
}

/// Mixed fixture: nested dirs, empty dir, empty file, executable,
/// multi-chunk file, relative + absolute symlinks.
fn build_fixture(root: &Path) {
    fs::create_dir(root).unwrap();
    fs::write(root.join("a.txt"), b"hello world").unwrap();
    fs::write(root.join("empty"), b"").unwrap();
    fs::write(root.join("exec.sh"), b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(root.join("exec.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir(root.join("emptydir")).unwrap();
    fs::create_dir_all(root.join("nested/deep")).unwrap();
    fs::write(
        root.join("nested/deep/big.bin"),
        pseudo_random_bytes(1, 3 * FASTCDC_MAX_BYTES + 12_345),
    )
    .unwrap();
    fs::write(root.join("nested/small"), b"x").unwrap();
    std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
    std::os::unix::fs::symlink("/absolute/nowhere", root.join("abslink")).unwrap();
}

/// Collect (path, file) pairs from a result tree, in entry order.
fn collect_files(node: &IngestNode, at: PathBuf, out: &mut Vec<(PathBuf, IngestFile)>) {
    match node {
        IngestNode::File(f) => out.push((at, f.clone())),
        IngestNode::Symlink(_) => {}
        IngestNode::Dir(d) => {
            for e in &d.entries {
                let name = std::ffi::OsStr::from_bytes(&e.name);
                collect_files(&e.node, at.join(name), out);
            }
        }
    }
}

/// One-shot single-threaded FastCDC + blake3 over a whole file — the
/// oracle the parallel chunk plane must reproduce exactly.
fn oneshot_chunks(data: &[u8]) -> Vec<super::IngestChunk> {
    if data.is_empty() {
        return Vec::new();
    }
    FastCDC::new(
        data,
        FASTCDC_MIN_BYTES,
        FASTCDC_AVG_BYTES,
        FASTCDC_MAX_BYTES,
    )
    .map(|c| super::IngestChunk {
        digest: *blake3::hash(&data[c.offset..c.offset + c.length]).as_bytes(),
        offset: c.offset as u64,
        len: c.length as u32,
    })
    .collect()
}

// NAR sha256 parity: the spine's framing + content interleave must hash
// to exactly what rio-nix's writer produces for the same tree.
#[test]
fn nar_sha256_matches_rio_nix_writer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    build_fixture(&root);
    let result = ingest_default(&root);
    assert_nar_parity(&root, &result);
}

// Every root shape NAR supports: single file, executable single file,
// single symlink, empty directory.
#[test]
fn root_shapes_match_rio_nix_writer() {
    let dir = tempfile::tempdir().unwrap();

    let file_root = dir.path().join("file");
    fs::write(&file_root, pseudo_random_bytes(2, FASTCDC_MAX_BYTES + 1)).unwrap();
    let r = ingest_default(&file_root);
    assert_nar_parity(&file_root, &r);
    assert!(matches!(&r.root, IngestNode::File(f) if !f.executable));

    let exec_root = dir.path().join("exec");
    fs::write(&exec_root, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&exec_root, fs::Permissions::from_mode(0o755)).unwrap();
    let r = ingest_default(&exec_root);
    assert_nar_parity(&exec_root, &r);
    assert!(matches!(&r.root, IngestNode::File(f) if f.executable));

    let link_root = dir.path().join("link");
    std::os::unix::fs::symlink("anywhere/else", &link_root).unwrap();
    let r = ingest_default(&link_root);
    assert_nar_parity(&link_root, &r);
    assert!(matches!(&r.root, IngestNode::Symlink(s) if s.target == b"anywhere/else"));

    let empty_root = dir.path().join("emptydir");
    fs::create_dir(&empty_root).unwrap();
    let r = ingest_default(&empty_root);
    assert_nar_parity(&empty_root, &r);
    assert!(matches!(&r.root, IngestNode::Dir(d) if d.entries.is_empty()));
}

// Chunk plane parity: per-file chunk runs, offsets, and digests must
// equal a direct single-threaded FastCDC+blake3 of each file's contents.
#[test]
fn chunk_lists_match_single_threaded_fastcdc() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    build_fixture(&root);
    let result = ingest_default(&root);

    let mut files = Vec::new();
    collect_files(&result.root, PathBuf::new(), &mut files);
    assert_eq!(files.len(), 5, "fixture has five regular files");

    for (rel, file) in files {
        let data = fs::read(root.join(&rel)).unwrap();
        assert_eq!(file.size, data.len() as u64, "{rel:?}: size");
        assert_eq!(
            file.digest,
            *blake3::hash(&data).as_bytes(),
            "{rel:?}: whole-file blake3"
        );
        assert_eq!(
            file.chunks,
            oneshot_chunks(&data),
            "{rel:?}: chunk run diverged from one-shot FastCDC"
        );
        // The run tiles the file exactly.
        let mut expect_offset = 0u64;
        for c in &file.chunks {
            assert_eq!(c.offset, expect_offset, "{rel:?}: chunk offsets contiguous");
            expect_offset += u64::from(c.len);
        }
        assert_eq!(expect_offset, file.size, "{rel:?}: chunk lens sum to size");
    }
}

// Determinism: byte-identical results across runs and across R/W/budget
// settings — thread interleaving must never leak into the output.
#[test]
fn deterministic_across_thread_and_budget_configs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    build_fixture(&root);

    let configs = [
        IngestConfig {
            reader_threads: 1,
            chunk_workers: 1,
            ..IngestConfig::default()
        },
        IngestConfig::default(), // R=8, W=2, 32 MiB
        // Tiny budget: every fixture file above 64 KiB takes the
        // oversized-admission path, with full reader contention.
        IngestConfig {
            reader_threads: 8,
            chunk_workers: 2,
            byte_budget: 64 * 1024,
            ..IngestConfig::default()
        },
    ];
    let baseline = ingest_tree(&root, &configs[0]).expect("ingest succeeds");
    assert_nar_parity(&root, &baseline);
    for cfg in &configs {
        for _ in 0..3 {
            let r = ingest_tree(&root, cfg).expect("ingest succeeds");
            assert_eq!(r, baseline, "non-deterministic result under {cfg:?}");
        }
    }
}

// A file larger than the whole byte budget must be admitted (alone) and
// the ingest must complete — the canonical 32 MiB default with a >32 MiB
// file, plus siblings to keep all readers busy.
#[test]
fn oversized_file_ingests_without_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("huge.bin"),
        pseudo_random_bytes(3, 33 * 1024 * 1024),
    )
    .unwrap();
    for i in 0..20 {
        fs::write(
            root.join(format!("small-{i:02}")),
            pseudo_random_bytes(i, 4096),
        )
        .unwrap();
    }
    let result = ingest_default(&root);
    assert_nar_parity(&root, &result);
}

// EACCES mid-walk: the whole ingest fails with the named error and all
// threads join (the test returning at all proves no hang/leak).
// CAP_DAC_OVERRIDE makes mode 000 readable for root, so skip there —
// `unsupported_file_type_fails_cleanly` covers the error-join path
// unconditionally.
#[test]
fn unreadable_file_fails_cleanly_with_named_error() {
    // SAFETY: geteuid has no preconditions and touches no memory.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    build_fixture(&root);
    let bad = root.join("nested/locked");
    fs::write(&bad, b"can't touch this").unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();

    let err = ingest_tree(&root, &IngestConfig::default()).unwrap_err();
    match err {
        IngestError::ReadFile { path, .. } => assert_eq!(path, bad),
        other => panic!("expected ReadFile, got {other:?}"),
    }
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).unwrap();
}

// Unreadable directory: readdir fails mid-discovery with the named error.
#[test]
fn unreadable_dir_fails_cleanly_with_named_error() {
    // SAFETY: geteuid has no preconditions and touches no memory.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    build_fixture(&root);
    let bad = root.join("nested/sealed");
    fs::create_dir(&bad).unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o000)).unwrap();

    let err = ingest_tree(&root, &IngestConfig::default()).unwrap_err();
    match err {
        IngestError::ReadDir { path, .. } => assert_eq!(path, bad),
        other => panic!("expected ReadDir, got {other:?}"),
    }
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
}

// A socket (any non-regular/dir/symlink) is rejected with a named error,
// mid-tree, with many sibling files keeping every pipeline thread busy —
// the error-path join test that works regardless of euid.
#[test]
fn unsupported_file_type_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir_all(root.join("sub")).unwrap();
    for i in 0..50 {
        fs::write(root.join(format!("f{i:03}")), pseudo_random_bytes(i, 8192)).unwrap();
    }
    let sock = root.join("sub/socket");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();

    let err = ingest_tree(&root, &IngestConfig::default()).unwrap_err();
    match err {
        IngestError::UnsupportedFileType { path } => assert_eq!(path, sock),
        other => panic!("expected UnsupportedFileType, got {other:?}"),
    }
}

// Non-UTF-8 entry names and symlink targets: NAR is a byte format and
// nix accepts them; rio-nix's tree writer requires UTF-8, so the oracle
// here is the canonical token stream hand-assembled from the same
// `frame` emitters the spine uses, in byte-lex name order.
#[test]
fn non_utf8_names_and_targets() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir(&root).unwrap();
    let weird_name: &[u8] = b"b\xff\xfe-name";
    let weird_target: &[u8] = b"t\xfa\xfbrget";
    fs::write(root.join("a"), b"plain").unwrap();
    fs::write(root.join(std::ffi::OsStr::from_bytes(weird_name)), b"odd").unwrap();
    std::os::unix::fs::symlink(
        std::ffi::OsStr::from_bytes(weird_target),
        root.join(std::ffi::OsStr::from_bytes(b"c\xfflink")),
    )
    .unwrap();

    let result = ingest_default(&root);

    // Expected NAR: dir with entries sorted byte-lex: "a" < "b\xff..." < "c\xff...".
    let mut nar = Vec::new();
    let w = &mut nar;
    frame::magic(w).unwrap();
    frame::node_open(w).unwrap();
    frame::directory_open(w).unwrap();
    for (name, body) in [
        (&b"a"[..], Some(&b"plain"[..])),
        (weird_name, Some(&b"odd"[..])),
        (&b"c\xfflink"[..], None),
    ] {
        frame::entry_open(w, name).unwrap();
        frame::node_open(w).unwrap();
        match body {
            Some(contents) => {
                frame::regular_header(w, false, contents.len() as u64).unwrap();
                w.extend_from_slice(contents);
                frame::contents_padding(w, contents.len() as u64).unwrap();
            }
            None => frame::symlink(w, weird_target).unwrap(),
        }
        frame::node_close(w).unwrap();
        frame::entry_close(w).unwrap();
    }
    frame::node_close(w).unwrap();

    assert_eq!(result.nar_size, nar.len() as u64);
    assert_eq!(result.nar_sha256, sha256(&nar));

    // Names and targets survive as raw bytes in the tree.
    let IngestNode::Dir(d) = &result.root else {
        panic!("dir root expected");
    };
    let names: Vec<&[u8]> = d.entries.iter().map(|e| e.name.as_slice()).collect();
    assert_eq!(names, vec![&b"a"[..], weird_name, &b"c\xfflink"[..]]);
    assert!(
        matches!(&d.entries[2].node, IngestNode::Symlink(s) if s.target == weird_target),
        "symlink target bytes preserved"
    );
}

// A file whose read length disagrees with the discovery lstat fails the
// whole ingest with SizeChanged — a mutating source tree must never emit
// a NAR whose length prefix disagrees with its contents. procfs gives a
// deterministic stat/read mismatch: /proc/version lstats as 0 bytes but
// reads non-empty, exercising the same check a mid-ingest mutation hits.
#[test]
fn size_change_between_stat_and_read_fails_cleanly() {
    let target = Path::new("/proc/version");
    let err = ingest_tree(target, &IngestConfig::default()).unwrap_err();
    match err {
        IngestError::SizeChanged {
            path,
            expected,
            actual,
        } => {
            assert_eq!(path, target);
            assert_eq!(expected, 0, "procfs stat size");
            assert!(actual > 0, "/proc/version reads non-empty");
        }
        other => panic!("expected SizeChanged, got {other:?}"),
    }
}

// Steal-lock degraded-mode regression (P1 profiling finding): when the
// byte budget saturates with out-of-NAR-order buffers — here forced by
// dir "zz" (NAR-last, but listed first because its readdir is 16 entries
// while "aa"'s is 3000) — the old pipeline parked every reader on the
// budget and the spine then stole and read the ENTIRE "aa" subtree
// serially at QD1 (~35s on cold ext4 for nixpkgs). The fix keeps readers
// alive and parked centrally, reserves a budget slice while listings are
// in flight, and lets any freed byte fund the spine-nearest pending
// file. Structural assertion (project policy: count ops, not wall
// clock): the reader pool must perform the bulk of the file reads.
#[test]
fn budget_saturation_keeps_readers_reading() {
    const AA_FILES: usize = 3000;
    const ZZ_FILES: usize = 16;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir_all(root.join("aa")).unwrap();
    fs::create_dir(root.join("zz")).unwrap();
    for i in 0..AA_FILES {
        fs::write(
            root.join(format!("aa/f{i:04}")),
            pseudo_random_bytes(i as u64, 4096),
        )
        .unwrap();
    }
    for i in 0..ZZ_FILES {
        // 16 × 64 KiB = the whole 1 MiB budget: without the listing-gated
        // head reserve these park out of NAR order and starve the budget.
        fs::write(
            root.join(format!("zz/g{i:02}")),
            pseudo_random_bytes(1000 + i as u64, 64 * 1024),
        )
        .unwrap();
    }
    let cfg = IngestConfig {
        reader_threads: 8,
        chunk_workers: 2,
        byte_budget: 1024 * 1024,
        test_delays: super::TestDelays {
            // Cold-device read latency: makes spine steals expensive and
            // the regime stable enough to measure.
            read: Some(std::time::Duration::from_micros(200)),
            // Spine slower than the 8-way reader pool's per-file rate, so
            // a healthy pipeline keeps readers ahead of the spine.
            spine: Some(std::time::Duration::from_micros(100)),
        },
    };
    let (result, stats) = super::pipeline::run(&root, &cfg).expect("ingest succeeds");
    assert_nar_parity(&root, &result);

    let total = stats.reader_file_reads + stats.spine_file_reads;
    println!(
        "reader_file_reads={} spine_file_reads={}",
        stats.reader_file_reads, stats.spine_file_reads
    );
    assert_eq!(total, (AA_FILES + ZZ_FILES) as u64, "every file read once");
    assert!(
        stats.reader_file_reads * 2 >= total,
        "steal-lock degraded mode: readers performed only {}/{total} file \
         reads (spine stole {}) — the budget-saturation regime serialized \
         the ingest at QD1",
        stats.reader_file_reads,
        stats.spine_file_reads,
    );
}

// Zero-byte files: empty chunk run, blake3-of-empty digest, and the NAR
// still hashes to parity (covered structurally; parity via fixture).
#[test]
fn zero_byte_file_has_empty_chunk_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("empty-file");
    fs::write(&root, b"").unwrap();
    let result = ingest_default(&root);
    assert_nar_parity(&root, &result);
    let IngestNode::File(f) = &result.root else {
        panic!("file root expected");
    };
    assert_eq!(f.size, 0);
    assert!(f.chunks.is_empty());
    assert_eq!(f.digest, *blake3::hash(b"").as_bytes());
}

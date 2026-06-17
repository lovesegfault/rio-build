//! Multi-process crash-safety tests: real concurrent writer processes
//! against one store directory.
//!
//! Child pattern: the parent re-spawns this same test binary with
//! `<child test name> --exact` and an env-var payload. Without the env
//! var the child tests no-op, so a normal test run is unaffected.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use rio_packstore::{Digest, Kind, Options, PackStore};

const DIR_ENV: &str = "RIO_PACKSTORE_TEST_DIR";
const ID_ENV: &str = "RIO_PACKSTORE_TEST_ID";

const WRITER_ROOTS: usize = 1000;
/// Flush cadence: many interleaved index rewrites per child is exactly
/// the window where a naive write-new+rename loses the other writer's
/// entries (the demonstrated 1000/1000 loss case).
const FLUSH_EVERY: usize = 50;

fn spawn_child(test_name: &str, dir: &Path, id: u32) -> Child {
    Command::new(std::env::current_exe().expect("test binary path"))
        .args([test_name, "--exact", "--nocapture"])
        .env(DIR_ENV, dir)
        .env(ID_ENV, id.to_string())
        .spawn()
        .expect("spawn child test process")
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn writer_payload(id: u32, i: usize) -> Vec<u8> {
    // ~140B mean metadata-blob sizes, unique per (writer, i).
    let mut out = vec![0u8; 96 + (i * 13) % 128];
    let mut xof = blake3::Hasher::new()
        .update(format!("writer-{id}-blob-{i}").as_bytes())
        .finalize_xof();
    xof.fill(&mut out);
    out
}

fn root_name(id: u32, i: usize) -> String {
    format!("/rio/store/w{id}-{i:04}")
}

// ── Child bodies (no-ops without the env var) ────────────────────────

/// Child: write 1000 blobs + roots with interleaved flushes.
#[test]
fn child_concurrent_writer() {
    let Ok(dir) = std::env::var(DIR_ENV) else {
        return;
    };
    let id: u32 = std::env::var(ID_ENV).unwrap().parse().unwrap();
    let mut store = PackStore::open(&dir, Options::default()).unwrap();
    for i in 0..WRITER_ROOTS {
        let payload = writer_payload(id, i);
        let digest = store.put(Kind::DIRECTORY, &payload).unwrap();
        store.add_root(&root_name(id, i), &[digest]).unwrap();
        if i % FLUSH_EVERY == 0 {
            store.flush().unwrap();
        }
    }
    store.flush().unwrap();
}

/// Child: open the store, write + flush one rooted blob, then HOLD the
/// segment (shared writer flock) until the parent signals release.
#[test]
fn child_segment_holder() {
    let Ok(dir) = std::env::var(DIR_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let mut store = PackStore::open(&dir, Options::default()).unwrap();
    let digest = store.put(Kind::FETCHED, b"held-by-live-writer").unwrap();
    store.add_root("/rio/store/held", &[digest]).unwrap();
    store.flush().unwrap();
    fs::write(dir.join("child-ready"), b"").unwrap();
    let release = dir.join("child-release");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !release.exists() {
        assert!(
            Instant::now() < deadline,
            "parent never released holder child"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // store (and its segment flock) lives until here.
    drop(store);
}

// ── Parent tests ─────────────────────────────────────────────────────

/// Two concurrent writer processes, interleaved flushes: the index
/// merge (load + merge + rename under the exclusive flock) must lose
/// NOTHING — a plain write-new+rename loses one writer's entries
/// essentially every time at this cadence.
#[test]
fn two_process_writers_lose_no_entries() {
    let tmp = tempfile::tempdir().unwrap();
    let children: Vec<Child> = (0..2)
        .map(|id| spawn_child("child_concurrent_writer", tmp.path(), id))
        .collect();
    for mut child in children {
        let status = child.wait().expect("child exits");
        assert!(status.success(), "writer child failed: {status:?}");
    }

    let store = PackStore::open(tmp.path(), Options::default()).unwrap();
    let mut missing_roots = 0usize;
    let mut missing_blobs = 0usize;
    for id in 0..2u32 {
        for i in 0..WRITER_ROOTS {
            let expected = writer_payload(id, i);
            match store.root_digests(&root_name(id, i)) {
                Some(digests) => assert_eq!(digests, vec![Digest::of(&expected)]),
                None => missing_roots += 1,
            }
            match store.get(&Digest::of(&expected)).unwrap() {
                Some(bytes) => assert_eq!(&bytes[..], &expected[..]),
                None => missing_blobs += 1,
            }
        }
    }
    assert_eq!(
        (missing_roots, missing_blobs),
        (0, 0),
        "concurrent flushes lost entries: {missing_roots} roots, {missing_blobs} blobs of {}",
        2 * WRITER_ROOTS,
    );
}

/// GC must skip a segment whose writer is alive (shared flock held),
/// while still repacking the dead segments around it.
#[test]
fn gc_skips_live_writers_segment() {
    let tmp = tempfile::tempdir().unwrap();

    // Dead garbage: three writerless segments of unrooted blobs.
    let mut dead = Vec::new();
    for i in 0..3 {
        let mut store = PackStore::open(tmp.path(), Options::default()).unwrap();
        let payload = format!("dead-garbage-{i}").into_bytes();
        dead.push(store.put(Kind::FETCHED, &payload).unwrap());
        store.flush().unwrap();
    }

    let mut child = spawn_child("child_segment_holder", tmp.path(), 0);
    wait_for(&tmp.path().join("child-ready"), "holder child ready");

    // Force GC. The holder's segment must be skipped (live shared
    // flock); the three dead segments must be repacked away.
    let opts = Options {
        max_segments: 0,
        grace: Duration::ZERO,
        ..Options::default()
    };
    let store = PackStore::open(tmp.path(), opts).unwrap();
    let stats = store.last_gc_stats().expect("forced trigger ran GC");
    assert!(
        stats.packs_skipped_live_writer >= 1,
        "live writer's segment must be skipped, stats: {stats:?}"
    );
    assert_eq!(
        stats.packs_repacked, 3,
        "dead segments repacked, stats: {stats:?}"
    );
    for d in &dead {
        assert_eq!(store.get(d).unwrap(), None, "unrooted garbage collected");
    }
    // The held blob survives: rooted, and its segment untouched.
    let held = Digest::of(b"held-by-live-writer");
    assert_eq!(
        store.get(&held).unwrap().as_deref(),
        Some(&b"held-by-live-writer"[..])
    );
    assert!(store.root_digests("/rio/store/held").is_some());
    drop(store);

    // Release the child; once its flock is gone the segment is fair
    // game and a later GC consolidates it without losing the blob.
    fs::write(tmp.path().join("child-release"), b"").unwrap();
    let status = child.wait().expect("holder child exits");
    assert!(status.success(), "holder child failed: {status:?}");

    let opts = Options {
        max_segments: 0,
        grace: Duration::ZERO,
        ..Options::default()
    };
    let store = PackStore::open(tmp.path(), opts).unwrap();
    let stats = store.last_gc_stats().expect("trigger fired again");
    assert_eq!(stats.packs_skipped_live_writer, 0, "no live writers remain");
    assert_eq!(
        store.get(&held).unwrap().as_deref(),
        Some(&b"held-by-live-writer"[..])
    );
}

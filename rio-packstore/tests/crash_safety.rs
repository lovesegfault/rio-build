//! Single-process crash-safety and GC tests.
//!
//! These exercise the ADR-024 pack-store invariants with real files
//! and realistic record sizes (140B-mean metadata blobs plus MB-scale
//! fetched content) — no mocks, no in-memory fakes.

use std::fs;
use std::path::Path;
use std::time::Duration;

use rio_packstore::{Digest, Kind, Options, PackStore, RECORD_HEADER_LEN};

/// Deterministic pseudo-random bytes (blake3 XOF) — incompressible,
/// content-distinct per seed, no rand dependency.
fn blob(seed: &str, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut xof = blake3::Hasher::new().update(seed.as_bytes()).finalize_xof();
    xof.fill(&mut out);
    out
}

/// Options that never trigger GC — for tests that want plain writes.
fn quiet() -> Options {
    Options {
        grace: Duration::from_secs(6 * 3600),
        ..Options::default()
    }
}

/// Options that force GC at next open: any segment triggers, nothing
/// is in the grace window.
fn gc_now() -> Options {
    Options {
        max_segments: 0,
        grace: Duration::ZERO,
        ..Options::default()
    }
}

fn pack_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<_> = fs::read_dir(dir.join("packs"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "pack"))
        .collect();
    out.sort();
    out
}

#[test]
fn put_get_roundtrip_realistic_sizes() {
    let tmp = tempfile::tempdir().unwrap();

    // 140B-mean metadata blobs + MB-scale fetched content.
    let mut expected = Vec::new();
    {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        for i in 0..200 {
            let bytes = blob(&format!("meta-{i}"), 64 + (i * 7) % 512);
            let digest = store.put(Kind::DIRECTORY, &bytes).unwrap();
            assert_eq!(digest, Digest::of(&bytes));
            expected.push((digest, bytes));
        }
        for (i, len) in [(0usize, 2 * 1024 * 1024), (1, 5 * 1024 * 1024)] {
            let bytes = blob(&format!("fetched-{i}"), len);
            let digest = store.put(Kind::FETCHED, &bytes).unwrap();
            expected.push((digest, bytes));
        }
        // Same-process readback, pre-flush.
        for (digest, bytes) in &expected {
            assert_eq!(
                store.get(digest).unwrap().as_deref(),
                Some(bytes.as_slice())
            );
            assert!(store.contains(digest));
        }
        store.flush().unwrap();
    }

    // Fresh open reads through the index.
    let store = PackStore::open(tmp.path(), quiet()).unwrap();
    for (digest, bytes) in &expected {
        assert_eq!(
            store.get(digest).unwrap().as_deref(),
            Some(bytes.as_slice())
        );
    }
    assert_eq!(store.get(&Digest::of(b"never stored")).unwrap(), None);
}

#[test]
fn put_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
    let bytes = blob("dup", 140);
    let d1 = store.put(Kind::DIRECTORY, &bytes).unwrap();
    store.flush().unwrap();
    let len_after_first: u64 = pack_files(tmp.path())
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .sum();
    let d2 = store.put(Kind::DIRECTORY, &bytes).unwrap();
    store.flush().unwrap();
    let len_after_second: u64 = pack_files(tmp.path())
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .sum();
    assert_eq!(d1, d2);
    assert_eq!(
        len_after_first, len_after_second,
        "second put must write nothing"
    );
}

#[test]
fn store_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("store");
    let mut store = PackStore::open(&root, quiet()).unwrap();
    store
        .put(Kind::FETCHED, b"private flake input bytes")
        .unwrap();
    store.flush().unwrap();

    // The store holds fetched private source trees: everything we
    // create must be owner-only (umask can only tighten further).
    let mut checked = 0;
    for path in [root.clone(), root.join("packs")] {
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{path:?} group/other bits set: {mode:o}");
        checked += 1;
    }
    for entry in fs::read_dir(&root)
        .unwrap()
        .chain(fs::read_dir(root.join("packs")).unwrap())
    {
        let path = entry.unwrap().path();
        if path.is_dir() {
            continue;
        }
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{path:?} group/other bits set: {mode:o}");
        checked += 1;
    }
    // dirs + gc.lock + index.bin + one segment, at minimum.
    assert!(
        checked >= 5,
        "expected to check at least 5 paths, saw {checked}"
    );
}

#[test]
fn reserved_kinds_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
    assert!(store.put(Kind(0xF0), b"x").is_err());
    assert!(store.put(Kind(0xFF), b"x").is_err());
}

#[test]
fn index_loss_rebuilds_from_packs() {
    let tmp = tempfile::tempdir().unwrap();
    let mut digests = Vec::new();
    {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        for i in 0..50 {
            let bytes = blob(&format!("b{i}"), 140);
            digests.push((store.put(Kind::DIRECTORY, &bytes).unwrap(), bytes));
        }
        let roots: Vec<Digest> = digests.iter().map(|(d, _)| *d).collect();
        store.add_root("/rio/store/aaa-root", &roots).unwrap();
        store.flush().unwrap();
    }

    // The index is a cache of the packs, never the truth: deleting it
    // must lose nothing — blobs AND the root table come back.
    fs::remove_file(tmp.path().join("index.bin")).unwrap();
    let store = PackStore::open(tmp.path(), quiet()).unwrap();
    for (digest, bytes) in &digests {
        assert_eq!(
            store.get(digest).unwrap().as_deref(),
            Some(bytes.as_slice())
        );
    }
    let pinned = store
        .root_digests("/rio/store/aaa-root")
        .expect("root rebuilt from pack records");
    assert_eq!(pinned.len(), digests.len());
}

#[test]
fn torn_tail_recovered_without_data_loss() {
    let tmp = tempfile::tempdir().unwrap();
    let mut digests = Vec::new();
    {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        for i in 0..10 {
            let bytes = blob(&format!("kept-{i}"), 140);
            digests.push((store.put(Kind::DIRECTORY, &bytes).unwrap(), bytes));
        }
        let roots: Vec<Digest> = digests.iter().map(|(d, _)| *d).collect();
        store.add_root("/rio/store/bbb-root", &roots).unwrap();
        store.flush().unwrap();
    }

    // Simulate a crash mid-append: a record header + partial payload
    // lands at the tail of the (now writerless) segment.
    let packs = pack_files(tmp.path());
    assert_eq!(packs.len(), 1);
    let seg = &packs[0];
    let clean_len = fs::metadata(seg).unwrap().len();
    let torn_payload = blob("torn", 4096);
    let torn_digest = Digest::of(&torn_payload);
    let mut torn = Vec::new();
    torn.extend_from_slice(b"RPK1");
    torn.push(0);
    torn.push(0);
    torn.extend_from_slice(&(torn_payload.len() as u32).to_le_bytes());
    torn.extend_from_slice(&torn_digest.0);
    torn.extend_from_slice(&torn_payload[..torn_payload.len() / 2]); // crash here
    let mut data = fs::read(seg).unwrap();
    data.extend_from_slice(&torn);
    fs::write(seg, &data).unwrap();

    // Crash also lost the index (worst case): rebuild must keep every
    // complete record and drop only the torn one — and a plain reopen
    // (no GC lock) must NOT truncate the file.
    fs::remove_file(tmp.path().join("index.bin")).unwrap();
    {
        let store = PackStore::open(tmp.path(), quiet()).unwrap();
        for (digest, bytes) in &digests {
            assert_eq!(
                store.get(digest).unwrap().as_deref(),
                Some(bytes.as_slice())
            );
        }
        assert_eq!(store.get(&torn_digest).unwrap(), None);
        assert_eq!(
            fs::metadata(seg).unwrap().len(),
            clean_len + torn.len() as u64,
            "read path must never truncate"
        );
    }

    // GC (holding the exclusive lock AND the segment's writer flock)
    // is the only thing allowed to drop the torn tail physically.
    let store = PackStore::open(tmp.path(), gc_now()).unwrap();
    let stats = store.last_gc_stats().expect("forced trigger ran GC");
    assert!(stats.packs_repacked >= 1);
    assert_eq!(stats.torn_tails_dropped, 1);
    assert!(!seg.exists(), "torn segment repacked away");
    for (digest, bytes) in &digests {
        assert_eq!(
            store.get(digest).unwrap().as_deref(),
            Some(bytes.as_slice())
        );
    }
    assert_eq!(store.get(&torn_digest).unwrap(), None);
}

#[test]
fn mid_pack_corruption_drops_one_record_not_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let payloads: Vec<Vec<u8>> = (0..3).map(|i| blob(&format!("rec-{i}"), 200)).collect();
    {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        for p in &payloads {
            store.put(Kind::DIRECTORY, p).unwrap();
        }
        store.flush().unwrap();
    }

    // Records are appended sequentially; smash record 2's length field.
    let packs = pack_files(tmp.path());
    assert_eq!(packs.len(), 1);
    let mut data = fs::read(&packs[0]).unwrap();
    let rec2_start = RECORD_HEADER_LEN + payloads[0].len();
    data[rec2_start + 6..rec2_start + 10].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&packs[0], &data).unwrap();

    fs::remove_file(tmp.path().join("index.bin")).unwrap();
    let store = PackStore::open(tmp.path(), quiet()).unwrap();
    // The hole swallows record 2 only; the scanner resyncs to record 3.
    assert_eq!(
        store.get(&Digest::of(&payloads[0])).unwrap().as_deref(),
        Some(payloads[0].as_slice())
    );
    assert_eq!(store.get(&Digest::of(&payloads[1])).unwrap(), None);
    assert_eq!(
        store.get(&Digest::of(&payloads[2])).unwrap().as_deref(),
        Some(payloads[2].as_slice())
    );
}

#[test]
fn gc_evicts_lru_roots_within_size_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let old_bytes = blob("old-only", 100 * 1024);
    let new_bytes = blob("new-only", 100 * 1024);
    let shared_bytes = blob("shared", 100 * 1024);
    let (old_d, new_d, shared_d);
    {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        old_d = store.put(Kind::FETCHED, &old_bytes).unwrap();
        new_d = store.put(Kind::FETCHED, &new_bytes).unwrap();
        shared_d = store.put(Kind::FETCHED, &shared_bytes).unwrap();
        // LRU clocks far apart; both far outside any grace window.
        store
            .add_root_at("/rio/store/old-root", &[old_d, shared_d], 1_000)
            .unwrap();
        store
            .add_root_at("/rio/store/new-root", &[new_d, shared_d], 2_000_000)
            .unwrap();
        store.flush().unwrap();
    }

    // Live = 300KiB; cap at 250KiB forces exactly the LRU root out.
    let opts = Options {
        size_cap_bytes: Some(250 * 1024),
        grace: Duration::ZERO,
        ..Options::default()
    };
    let store = PackStore::open(tmp.path(), opts).unwrap();
    let stats = store.last_gc_stats().expect("size-cap trigger ran GC");
    assert_eq!(stats.roots_evicted, 1);

    assert!(
        store.root_digests("/rio/store/old-root").is_none(),
        "LRU root evicted"
    );
    assert!(
        store.root_digests("/rio/store/new-root").is_some(),
        "recent root survives"
    );
    // Eviction unit is the root: the blob only the old root pinned is
    // gone after repack, the shared blob survives.
    assert_eq!(store.get(&old_d).unwrap(), None);
    assert_eq!(
        store.get(&shared_d).unwrap().as_deref(),
        Some(shared_bytes.as_slice())
    );
    assert_eq!(
        store.get(&new_d).unwrap().as_deref(),
        Some(new_bytes.as_slice())
    );
}

#[test]
fn grace_window_protects_recent_roots_and_packs() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes = blob("in-flight", 100 * 1024);
    let digest;
    {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        digest = store.put(Kind::FETCHED, &bytes).unwrap();
        store.add_root("/rio/store/fresh-root", &[digest]).unwrap();
        store.flush().unwrap();
    }

    // Cap of 1 byte demands eviction, but the root was just used and
    // the pack was just written: the 6h in-flight grace protects both.
    let opts = Options {
        size_cap_bytes: Some(1),
        max_segments: 0,
        grace: Duration::from_secs(6 * 3600),
        ..Options::default()
    };
    let store = PackStore::open(tmp.path(), opts).unwrap();
    let stats = store.last_gc_stats().expect("trigger fired");
    assert_eq!(stats.roots_evicted, 0, "grace window blocks eviction");
    assert_eq!(stats.packs_repacked, 0, "young pack not repacked");
    assert_eq!(
        store.get(&digest).unwrap().as_deref(),
        Some(bytes.as_slice())
    );
}

#[test]
fn unrooted_blobs_die_outside_grace() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes = blob("never-rooted", 64 * 1024);
    let digest;
    {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        digest = store.put(Kind::FETCHED, &bytes).unwrap();
        store.flush().unwrap();
    }
    let store = PackStore::open(tmp.path(), gc_now()).unwrap();
    let stats = store.last_gc_stats().expect("trigger fired");
    assert!(stats.records_dropped >= 1);
    assert_eq!(store.get(&digest).unwrap(), None, "unrooted blob collected");
}

#[test]
fn segment_count_trigger_consolidates() {
    let tmp = tempfile::tempdir().unwrap();
    let mut digests = Vec::new();
    // 6 opens → 6 writer segments, each with a rooted blob.
    for i in 0u64..6 {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        let bytes = blob(&format!("seg-{i}"), 4096);
        let d = store.put(Kind::DIRECTORY, &bytes).unwrap();
        store
            .add_root_at(&format!("/rio/store/root-{i}"), &[d], 1_000 + i)
            .unwrap();
        store.flush().unwrap();
        digests.push((d, bytes));
    }
    assert_eq!(pack_files(tmp.path()).len(), 6);

    let opts = Options {
        max_segments: 2,
        grace: Duration::ZERO,
        ..Options::default()
    };
    let store = PackStore::open(tmp.path(), opts).unwrap();
    let stats = store.last_gc_stats().expect("segment-count trigger fired");
    assert_eq!(stats.packs_repacked, 6);
    assert_eq!(
        pack_files(tmp.path()).len(),
        1,
        "consolidated into one pack"
    );
    for (d, bytes) in &digests {
        assert_eq!(store.get(d).unwrap().as_deref(), Some(bytes.as_slice()));
    }
    // Root table survives consolidation (re-emitted into the
    // consolidated pack): prove it by dropping the index again.
    drop(store);
    fs::remove_file(tmp.path().join("index.bin")).unwrap();
    let store = PackStore::open(tmp.path(), quiet()).unwrap();
    for i in 0..6 {
        assert!(
            store
                .root_digests(&format!("/rio/store/root-{i}"))
                .is_some()
        );
    }
}

#[test]
fn repack_one_segment_at_a_time_headroom() {
    let tmp = tempfile::tempdir().unwrap();
    const SEG_PAYLOAD: usize = 1024 * 1024;
    let mut live = Vec::new();
    // 8 segments, each ~2MiB: one rooted (live) and one unrooted
    // (dead) 1MiB blob.
    for i in 0..8 {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        let live_bytes = blob(&format!("live-{i}"), SEG_PAYLOAD);
        let dead_bytes = blob(&format!("dead-{i}"), SEG_PAYLOAD);
        let d = store.put(Kind::FETCHED, &live_bytes).unwrap();
        store.put(Kind::FETCHED, &dead_bytes).unwrap();
        store
            .add_root_at(&format!("/rio/store/hr-{i}"), &[d], 1_000)
            .unwrap();
        store.flush().unwrap();
        live.push((d, live_bytes));
    }
    let packs = pack_files(tmp.path());
    assert_eq!(packs.len(), 8);
    let total_before: u64 = packs.iter().map(|p| fs::metadata(p).unwrap().len()).sum();
    let max_segment: u64 = packs
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .max()
        .unwrap();

    let store = PackStore::open(tmp.path(), gc_now()).unwrap();
    let stats = store.last_gc_stats().expect("trigger fired");
    assert_eq!(stats.packs_repacked, 8);

    // The one-segment-at-a-time invariant: transient disk is one
    // segment of headroom over the starting footprint — never the
    // ~2× live bytes an all-at-once repack costs.
    let headroom = max_segment + 64 * 1024; // record/root overhead slack
    assert!(
        stats.peak_pack_bytes <= total_before + headroom,
        "peak {} exceeded start {} + one segment {}",
        stats.peak_pack_bytes,
        total_before,
        headroom,
    );

    // And the dead half is actually reclaimed.
    let total_after: u64 = pack_files(tmp.path())
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .sum();
    assert!(
        total_after < total_before * 6 / 10,
        "dead bytes reclaimed: {total_after}"
    );
    for (d, bytes) in &live {
        assert_eq!(store.get(d).unwrap().as_deref(), Some(bytes.as_slice()));
    }
}

#[test]
fn root_only_segment_repack_keeps_roots_in_packs() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes = blob("warm-blob", 140);
    let digest;
    {
        // Writer 1 stores the blob.
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        digest = store.put(Kind::DIRECTORY, &bytes).unwrap();
        store.flush().unwrap();
    }
    let blob_pack = pack_files(tmp.path()).pop().unwrap();
    {
        // Writer 2 (a warm eval): the blob dedups, so its segment holds
        // ONLY the ROOT record.
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        store.add_root("/rio/store/warm-root", &[digest]).unwrap();
        store.flush().unwrap();
    }
    let root_pack = pack_files(tmp.path())
        .into_iter()
        .find(|p| p != &blob_pack)
        .expect("writer 2 created its own segment");

    // Age the roots-only segment past the grace window; the blob's
    // segment stays young (grace-protected, skipped by repack).
    fs::File::open(&root_pack)
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    let opts = Options {
        max_segments: 0,
        grace: Duration::from_secs(3600),
        ..Options::default()
    };
    let store = PackStore::open(tmp.path(), opts).unwrap();
    let stats = store.last_gc_stats().expect("segment-count trigger fired");
    assert_eq!(stats.packs_repacked, 1, "only the aged segment repacked");
    drop(store);

    // The repacked segment held the only on-disk ROOT record. Packs
    // alone must rebuild the root table — losing the index must not
    // lose the root (and with it, eventually, the blob).
    fs::remove_file(tmp.path().join("index.bin")).unwrap();
    let store = PackStore::open(tmp.path(), quiet()).unwrap();
    assert_eq!(
        store.root_digests("/rio/store/warm-root"),
        Some(vec![digest]),
        "root must survive repack of its roots-only segment + index loss"
    );
    assert_eq!(
        store.get(&digest).unwrap().as_deref(),
        Some(bytes.as_slice())
    );
}

#[test]
fn repack_collapses_duplicate_records_across_segments() {
    let tmp = tempfile::tempdir().unwrap();
    let bytes = blob("dup-across-writers", 100 * 1024);
    let digest;
    {
        // Two handles open concurrently: neither sees the other's
        // unflushed write, so both append their own copy of the digest
        // (exactly what two racing eval processes do).
        let mut s1 = PackStore::open(tmp.path(), quiet()).unwrap();
        let mut s2 = PackStore::open(tmp.path(), quiet()).unwrap();
        digest = s1.put(Kind::FETCHED, &bytes).unwrap();
        assert_eq!(s2.put(Kind::FETCHED, &bytes).unwrap(), digest);
        s1.add_root_at("/rio/store/dup-root", &[digest], 1_000)
            .unwrap();
        s1.flush().unwrap();
        s2.flush().unwrap();
    }
    assert_eq!(pack_files(tmp.path()).len(), 2);

    let store = PackStore::open(tmp.path(), gc_now()).unwrap();
    let stats = store.last_gc_stats().expect("trigger fired");
    assert_eq!(stats.packs_repacked, 2);
    // One copy survives consolidation; the duplicate is skipped — NOT
    // copied again (duplicates would otherwise persist through every
    // future GC, a permanent space leak).
    assert_eq!(
        stats.records_copied, 1,
        "duplicate record must not be re-copied"
    );
    let total_after: u64 = pack_files(tmp.path())
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .sum();
    assert!(
        total_after < 2 * bytes.len() as u64,
        "consolidated pack holds one copy, not two: {total_after}"
    );
    assert_eq!(
        store.get(&digest).unwrap().as_deref(),
        Some(bytes.as_slice())
    );
}

#[test]
fn approx_dead_counter_triggers_repack() {
    let tmp = tempfile::tempdir().unwrap();
    let kept = blob("kept", 10 * 1024);
    let bulky = blob("bulky-evictee", 200 * 1024);
    let (kept_d, bulky_d);
    {
        let mut store = PackStore::open(tmp.path(), quiet()).unwrap();
        kept_d = store.put(Kind::FETCHED, &kept).unwrap();
        bulky_d = store.put(Kind::FETCHED, &bulky).unwrap();
        store
            .add_root_at("/rio/store/kept", &[kept_d], 2_000_000)
            .unwrap();
        store
            .add_root_at("/rio/store/bulky", &[bulky_d], 1_000)
            .unwrap();
        store.flush().unwrap();
    }

    // Pass 1: size cap evicts the bulky LRU root; approx_dead grows by
    // the evicted root's referenced bytes and the repack reclaims them
    // (counter resets to zero afterwards — asserted indirectly: a
    // second open with only the dead-ratio trigger must NOT fire GC).
    let opts = Options {
        size_cap_bytes: Some(64 * 1024),
        grace: Duration::ZERO,
        ..Options::default()
    };
    let store = PackStore::open(tmp.path(), opts).unwrap();
    let stats = store.last_gc_stats().expect("size-cap trigger fired");
    assert_eq!(stats.roots_evicted, 1);
    assert!(stats.packs_repacked >= 1);
    assert_eq!(store.get(&bulky_d).unwrap(), None);
    assert_eq!(
        store.get(&kept_d).unwrap().as_deref(),
        Some(kept.as_slice())
    );
    drop(store);

    let store = PackStore::open(
        tmp.path(),
        Options {
            grace: Duration::ZERO,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(
        store.last_gc_stats().is_none(),
        "approx_dead reset after repack — no spurious GC"
    );
}

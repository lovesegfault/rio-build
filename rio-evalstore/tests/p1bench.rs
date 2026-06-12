//! P1 acceptance bench harness (ADR-024 "Staging" P1 + "Measurement plan").
//!
//! Every test here is `#[ignore]`d: they need a real source tree on real
//! hardware and print measurements instead of asserting gates (the gate
//! verdict depends on the device class the host provides — see the ADR's
//! "NVMe gate by specification" note). Run explicitly, release profile,
//! e.g.:
//!
//! ```text
//! P1B_TREE=/path/to/nixpkgs-worktree P1B_CAS=/path/to/scratch-cas \
//!   cargo test -p rio-evalstore --release --test p1bench -- \
//!   --ignored --nocapture p1_warm_trace
//! ```
//!
//! Env knobs:
//! - `P1B_TREE`: source tree to ingest/replay (nixpkgs checkout, `.git`
//!   excluded by the caller — the harness walks everything it is given).
//! - `P1B_CAS`: CAS directory (created on first use; delete for a cold run).
//! - `P1B_DEVDIR`: directory on the device under test (device benches).
//! - `P1B_READERS` / `P1B_WORKERS`: override `IngestConfig` R / W.
//!
//! The warm-trace replay reproduces the dagbench M5 trace shape exactly:
//! 1,511 lstat + 772 readDir + 278 isValidPath = 2,561 ops, targets
//! sampled with the same SplitMix64 stream and seed (0xDA6B_E4C4) and the
//! same Fisher-Yates shuffle as dagbench `replay::gen_trace`, over the
//! tree given in `P1B_TREE` (the original protoperf op arguments were not
//! logged; sampling-synthesis is the documented M5 method).

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rio_evalstore::EvalStore;
use rio_evalstore::ingest::{self, IngestConfig};
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::StorePath;
use sha2::Digest as _;

fn env_path(key: &str) -> PathBuf {
    PathBuf::from(std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set")))
}

fn ingest_config() -> IngestConfig {
    let mut cfg = IngestConfig::default();
    if let Ok(r) = std::env::var("P1B_READERS") {
        cfg.reader_threads = r.parse().expect("P1B_READERS");
    }
    if let Ok(w) = std::env::var("P1B_WORKERS") {
        cfg.chunk_workers = w.parse().expect("P1B_WORKERS");
    }
    cfg
}

/// The "nix side" of the add_source_tree cross-check: recompute the path
/// from the hashes the store hands back (same rule as nix's
/// `makeFixedOutputPath`, recursive sha256).
fn nix_path_for(name: &str, hashes: &rio_evalstore::store::AddHashes) -> String {
    let h = NixHash::new(HashAlgo::SHA256, hex::decode(&hashes.nar_sha256).unwrap()).unwrap();
    StorePath::make_fixed_output(name, &h, true, &[])
        .unwrap()
        .to_string()
}

/// dagbench `model::SplitMix64`, bit-for-bit.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

enum Op {
    Lstat(String),
    ReadDir(String),
    IsValidPath,
}

const N_LSTAT: usize = 1511;
const N_READDIR: usize = 772;
const N_IVP: usize = 278;
const TRACE_SEED: u64 = 0xDA6B_E4C4;
const WARM_TRACE_BUDGET_MS: f64 = 92.0;

/// Collect leaf (file/symlink) and directory rel-paths in NAR order —
/// the same traversal `dagbench::replay::gen_trace` runs over its model.
fn collect_paths(root: &Path) -> (Vec<String>, Vec<String>) {
    let mut leaves = Vec::new();
    let mut dirs = Vec::new();
    let mut cur = String::new();
    walk(root, &mut cur, &mut leaves, &mut dirs);
    return (leaves, dirs);

    fn walk(dir: &Path, cur: &mut String, leaves: &mut Vec<String>, dirs: &mut Vec<String>) {
        if !cur.is_empty() {
            dirs.push(cur.clone());
        }
        let mut entries: Vec<(Vec<u8>, fs::FileType, PathBuf)> = fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().into_encoded_bytes(),
                    e.file_type().unwrap(),
                    e.path(),
                )
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, ftype, path) in entries {
            // Trace paths are joined with '/': skip the rare non-UTF-8 name.
            let Ok(name) = String::from_utf8(name) else {
                continue;
            };
            let len0 = cur.len();
            if !cur.is_empty() {
                cur.push('/');
            }
            cur.push_str(&name);
            if ftype.is_dir() {
                walk(&path, cur, leaves, dirs);
            } else {
                leaves.push(cur.clone());
            }
            cur.truncate(len0);
        }
    }
}

fn gen_trace(root: &Path) -> Vec<Op> {
    let (leaves, dirs) = collect_paths(root);
    let mut rng = SplitMix64(TRACE_SEED);
    let mut ops = Vec::with_capacity(N_LSTAT + N_READDIR + N_IVP);
    for _ in 0..N_LSTAT {
        ops.push(Op::Lstat(
            leaves[(rng.next() % leaves.len() as u64) as usize].clone(),
        ));
    }
    for _ in 0..N_READDIR {
        ops.push(Op::ReadDir(
            dirs[(rng.next() % dirs.len() as u64) as usize].clone(),
        ));
    }
    for _ in 0..N_IVP {
        ops.push(Op::IsValidPath);
    }
    for i in (1..ops.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        ops.swap(i, j);
    }
    ops
}

/// Ingest `P1B_TREE` into `P1B_CAS` if absent, returning the store-path
/// basename. Idempotent: a second call re-ingests but writes no new pack
/// records (CAS dedup), so the replay benches can call it as setup.
fn ensure_ingested(store: &EvalStore, tree: &Path) -> String {
    let result = store
        .add_source_tree(tree.to_str().unwrap(), "p1bench-tree", &[], &mut |h| {
            Ok(nix_path_for("p1bench-tree", h))
        })
        .expect("add_source_tree");
    store.flush().expect("flush");
    result
        .path
        .rsplit('/')
        .next()
        .expect("basename")
        .to_string()
}

fn replay(store: &EvalStore, basename: &str, ops: &[Op]) -> (Duration, u64) {
    let mut acc = 0u64;
    let t = Instant::now();
    for op in ops {
        match op {
            Op::Lstat(rel) => {
                let st = store.lstat(basename, rel).expect("lstat");
                acc += st.is_some() as u64;
            }
            Op::ReadDir(rel) => {
                let entries = store.read_directory(basename, rel).expect("read_directory");
                acc += entries.len() as u64;
            }
            Op::IsValidPath => {
                acc += store.is_valid_path(basename) as u64;
            }
        }
    }
    (t.elapsed(), acc)
}

/// Gate 1: warm trace ≤92ms on the small-mixed op shape (dagbench M5).
#[test]
#[ignore = "manual P1 acceptance bench; needs P1B_TREE/P1B_CAS"]
fn p1_warm_trace() {
    let tree = env_path("P1B_TREE");
    let cas = env_path("P1B_CAS");
    let ops = gen_trace(&tree);
    assert_eq!(ops.len(), N_LSTAT + N_READDIR + N_IVP);

    // Setup store (cold ingest if the CAS is fresh), then drop it so the
    // replay store opens with an empty in-process decoded-dir cache —
    // "warm" means pack bytes in the page cache, process cache cold.
    let setup = EvalStore::open(Some(cas.to_str().unwrap())).expect("open");
    let basename = ensure_ingested(&setup, &tree);
    drop(setup);

    let t_open = Instant::now();
    let store = EvalStore::open(Some(cas.to_str().unwrap())).expect("open");
    let open_ms = t_open.elapsed().as_secs_f64() * 1e3;

    let (pass1, acc1) = replay(&store, &basename, &ops);
    let (pass2, acc2) = replay(&store, &basename, &ops);
    let p1_ms = pass1.as_secs_f64() * 1e3;
    let decodes = store.stats().count("dir_decode");
    println!(
        "P1BENCH warm_trace ops={} open_ms={open_ms:.2} pass1_ms={p1_ms:.2} \
         pass2_ms={:.3} dirblob_decodes={decodes} acc1={acc1} acc2={acc2} \
         budget_ms={WARM_TRACE_BUDGET_MS} gate={}",
        ops.len(),
        pass2.as_secs_f64() * 1e3,
        if p1_ms <= WARM_TRACE_BUDGET_MS {
            "PASS"
        } else {
            "FAIL"
        },
    );
}

/// Evict every regular file of `tree` from the page cache via
/// `posix_fadvise(POSIX_FADV_DONTNEED)` — the unprivileged stand-in for
/// `drop_caches` when /proc is read-only. Drops file DATA only; dentries
/// and inodes stay cached, which is exactly the simulation's
/// "cold-data, warm-metadata" regime (its primary gate scenario).
fn evict_tree(root: &Path) -> usize {
    use std::os::fd::AsRawFd as _;
    let (leaves, _) = collect_paths(root);
    let mut n = 0;
    for rel in &leaves {
        let p = root.join(rel);
        if !fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let f = fs::File::open(&p).expect("open for evict");
        let rc = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        assert_eq!(rc, 0, "fadvise failed for {p:?}");
        n += 1;
    }
    n
}

/// Gate 3: cold source-tree ingest. Pipeline-only (`ingest_tree`) and
/// end-to-end (`add_source_tree` incl. pack commit + flush) timings; the
/// caller controls cache state (drop_caches / `P1B_EVICT=1` / fresh copy)
/// and device.
#[test]
#[ignore = "manual P1 acceptance bench; needs P1B_TREE/P1B_CAS"]
fn p1_cold_ingest() {
    let tree = env_path("P1B_TREE");
    let cas = env_path("P1B_CAS");
    let cfg = ingest_config();

    let evicted = if std::env::var("P1B_EVICT").as_deref() == Ok("1") {
        evict_tree(&tree)
    } else {
        0
    };

    let t = Instant::now();
    let result = ingest::ingest_tree(&tree, &cfg).expect("ingest_tree");
    let pipeline_s = t.elapsed().as_secs_f64();

    // Re-evict so the end-to-end run sees the same cache state as the
    // pipeline-only run instead of the bytes the first pass just pulled in.
    if evicted > 0 {
        evict_tree(&tree);
    }
    let store = EvalStore::open(Some(cas.to_str().unwrap())).expect("open");
    let t = Instant::now();
    let basename = ensure_ingested(&store, &tree);
    let full_s = t.elapsed().as_secs_f64();

    println!(
        "P1BENCH cold_ingest readers={} workers={} evicted_files={evicted} \
         nar_size={} pipeline_s={pipeline_s:.3} add_source_tree_s={full_s:.3} \
         basename={basename} gate_budget_s=2.0",
        cfg.reader_threads, cfg.chunk_workers, result.nar_size,
    );
}

const QD1_SAMPLES: usize = 2000;
const READ_SIZE: usize = 4096;
const DEV_FILE_BYTES: u64 = 2 << 30;

fn open_direct(path: &Path) -> fs::File {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .expect("open O_DIRECT (device bench needs a real filesystem, not tmpfs)")
}

fn pread_direct(file: &fs::File, buf: &mut [u8], offset: u64) {
    use std::os::unix::fs::FileExt as _;
    file.read_exact_at(buf, offset).expect("pread");
}

/// 4096-aligned read buffer for O_DIRECT.
#[repr(align(4096))]
struct AlignedBuf([u8; READ_SIZE]);

fn aligned_buf() -> Box<AlignedBuf> {
    Box::new(AlignedBuf([0u8; READ_SIZE]))
}

fn ensure_dev_file(dir: &Path) -> PathBuf {
    let path = dir.join("p1bench-devfile.bin");
    if fs::metadata(&path).is_ok_and(|m| m.len() == DEV_FILE_BYTES) {
        return path;
    }
    // Incompressible-ish deterministic content so no filesystem trickery
    // (hole punching, dedup) can elide device reads.
    let mut f = fs::File::create(&path).expect("create dev file");
    let mut rng = SplitMix64(0x1234_5678);
    let mut block = vec![0u8; 1 << 20];
    let mut written = 0u64;
    while written < DEV_FILE_BYTES {
        for chunk in block.chunks_exact_mut(8) {
            chunk.copy_from_slice(&rng.next().to_le_bytes());
        }
        std::io::Write::write_all(&mut f, &block).expect("write");
        written += block.len() as u64;
    }
    f.sync_all().expect("fsync");
    path
}

/// Constant (a): cold random-read latency + IOPS at QD1 and QD8 on the
/// device behind `P1B_DEVDIR` (O_DIRECT — page cache bypassed, so no
/// drop_caches needed). Decides R and classifies the device (NVMe vs
/// EBS-class) for the ingest gate's environment rule.
#[test]
#[ignore = "manual P1 acceptance bench; needs P1B_DEVDIR"]
fn p1_device_random_read() {
    let dir = env_path("P1B_DEVDIR");
    let path = ensure_dev_file(&dir);
    let file = open_direct(&path);
    let blocks = DEV_FILE_BYTES / READ_SIZE as u64;

    // QD1: per-read latency distribution.
    let mut rng = SplitMix64(0xABCD);
    let mut buf = aligned_buf();
    let mut lat_us: Vec<f64> = Vec::with_capacity(QD1_SAMPLES);
    for _ in 0..QD1_SAMPLES {
        let offset = (rng.next() % blocks) * READ_SIZE as u64;
        let t = Instant::now();
        pread_direct(&file, &mut buf.0, offset);
        lat_us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    lat_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = lat_us.iter().sum::<f64>() / lat_us.len() as f64;
    let pct = |p: f64| lat_us[((lat_us.len() as f64 * p) as usize).min(lat_us.len() - 1)];
    println!(
        "P1BENCH dev_qd1 samples={QD1_SAMPLES} mean_us={mean:.1} p50_us={:.1} \
         p99_us={:.1} iops={:.0}",
        pct(0.50),
        pct(0.99),
        1e6 / mean,
    );

    // QD8 / QD32: aggregate IOPS over a fixed window, one file handle per
    // thread (mirrors R parallel readers, each with its own fd).
    for qd in [8usize, 32] {
        let window = Duration::from_secs(3);
        let total: u64 = std::thread::scope(|s| {
            let handles: Vec<_> = (0..qd)
                .map(|i| {
                    let path = &path;
                    s.spawn(move || {
                        let file = open_direct(path);
                        let mut rng = SplitMix64(0x517E ^ i as u64);
                        let mut buf = aligned_buf();
                        let mut n = 0u64;
                        let t = Instant::now();
                        while t.elapsed() < window {
                            let offset = (rng.next() % blocks) * READ_SIZE as u64;
                            pread_direct(&file, &mut buf.0, offset);
                            n += 1;
                        }
                        n
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        println!(
            "P1BENCH dev_qd{qd} window_s=3 iops={:.0}",
            total as f64 / window.as_secs_f64(),
        );
    }
}

/// Constant (b): warm open+read+close over real ≤4KiB tree files (the
/// 91%-of-files case). Sets the warm-path floor R parallel readers race.
#[test]
#[ignore = "manual P1 acceptance bench; needs P1B_TREE"]
fn p1_warm_open_read_close() {
    let tree = env_path("P1B_TREE");
    let (leaves, _) = collect_paths(&tree);
    let mut small: Vec<PathBuf> = leaves
        .iter()
        .map(|rel| tree.join(rel))
        .filter(|p| {
            fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.len() <= READ_SIZE as u64)
        })
        .collect();
    small.truncate(20_000);

    let mut buf = vec![0u8; READ_SIZE];
    // Warm pass: pull everything into the page cache.
    for p in &small {
        let mut f = fs::File::open(p).expect("open");
        let _ = f.read(&mut buf).expect("read");
    }
    // Timed pass.
    let t = Instant::now();
    let mut bytes = 0u64;
    for p in &small {
        let mut f = fs::File::open(p).expect("open");
        bytes += f.read(&mut buf).expect("read") as u64;
    }
    let elapsed = t.elapsed();
    println!(
        "P1BENCH warm_orc files={} bytes={bytes} total_ms={:.1} per_file_us={:.2}",
        small.len(),
        elapsed.as_secs_f64() * 1e3,
        elapsed.as_secs_f64() * 1e6 / small.len() as f64,
    );
}

/// Constant (c): single-thread FastCDC scan rate, blake3 and sha256
/// throughput (the spine ceiling), plus blake3-per-CDC-chunk (the real
/// plane-2 unit of work).
#[test]
#[ignore = "manual P1 acceptance bench; CPU-only"]
fn p1_hash_rates() {
    use rio_common::limits::{FASTCDC_AVG_BYTES, FASTCDC_MAX_BYTES, FASTCDC_MIN_BYTES};

    const BUF: usize = 256 << 20;
    let mut data = vec![0u8; BUF];
    let mut rng = SplitMix64(0xFEED);
    for chunk in data.chunks_exact_mut(8) {
        chunk.copy_from_slice(&rng.next().to_le_bytes());
    }
    let gbps = |elapsed: Duration| BUF as f64 / elapsed.as_secs_f64() / 1e9;

    let t = Instant::now();
    let mut sha = sha2::Sha256::new();
    sha.update(&data);
    let digest = sha.finalize();
    println!(
        "P1BENCH hash sha256_gbps={:.2} tag={:02x}",
        gbps(t.elapsed()),
        digest[0],
    );

    let t = Instant::now();
    let b3 = blake3::hash(&data);
    println!(
        "P1BENCH hash blake3_gbps={:.2} tag={:02x}",
        gbps(t.elapsed()),
        b3.as_bytes()[0],
    );

    let t = Instant::now();
    let chunks: Vec<fastcdc::v2020::Chunk> = fastcdc::v2020::FastCDC::new(
        &data,
        FASTCDC_MIN_BYTES,
        FASTCDC_AVG_BYTES,
        FASTCDC_MAX_BYTES,
    )
    .collect();
    println!(
        "P1BENCH hash fastcdc_gbps={:.2} chunks={}",
        gbps(t.elapsed()),
        chunks.len(),
    );

    // Plane-2 composite: CDC scan + blake3 per chunk + whole-file blake3,
    // i.e. what one chunk worker does per byte.
    let t = Instant::now();
    let mut acc = 0u8;
    for c in &chunks {
        acc ^= blake3::hash(&data[c.offset..c.offset + c.length]).as_bytes()[0];
    }
    acc ^= blake3::hash(&data).as_bytes()[0];
    println!(
        "P1BENCH hash plane2_composite_gbps={:.2} tag={acc:02x}",
        BUF as f64 / (t.elapsed().as_secs_f64()) / 1e9,
    );
}

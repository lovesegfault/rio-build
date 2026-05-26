//! Mark-scan cost measurement for the lazy chunk collector (design §5a go/no-go).
//!
//! Seeds N synthetic chunked paths (`narinfo` + `manifests` + `manifest_data`
//! with realistic `chunk_list` blobs) and then runs the collector's mark phase
//! exactly as the shadow/live collector will: stream `manifest_data.chunk_list`
//! joined to `manifests` in keyset pages on ONE connection, parse each blob
//! with [`super::try_parse_unique_chunk_hashes`], and accumulate the live set
//! into a `TEMP TABLE live_chunks(blake3_hash BYTEA PRIMARY KEY)` via batched
//! `INSERT … ON CONFLICT DO NOTHING`.
//!
//! The measurement reports wall-clock for the mark phase, rows scanned,
//! references parsed, mark-set size (distinct hashes), and the temp-table
//! size — the numbers the go/no-go threshold (full scan at the ~1.5 M-path
//! scale, linear-or-better growth, temp-table-bounded memory) is judged
//! against. Measured figures are volatile and belong in the introducing
//! commit message and the invariant map, never in this file.
//!
//! `#[ignore]`d: the measurement-scale run takes minutes by design. Run it
//! explicitly, e.g.:
//!
//! ```text
//! MARK_SCAN_BENCH_PATHS=1500000 \
//!   cargo nextest run -p rio-store -E 'test(mark_scan_bench)' --run-ignored all
//! ```
//!
//! Env knobs (all optional):
//! - `MARK_SCAN_BENCH_PATHS`        — number of chunked paths to seed (default 2000, smoke scale)
//! - `MARK_SCAN_BENCH_SEED_WORKERS` — parallel seeding tasks (default 12; seeding only — the
//!   mark scan itself is always one connection, like the collector)
//!
//! The per-path entry-count mix approximates the inventory's volume shape:
//! median a few dozen entries, a long tail up to ~160k entries (the
//! 10 GB-NAR class at ~64 KiB average chunk size, ~5.6 MB serialized), drawn
//! deterministically from a fixed seed so re-runs are comparable. Chunk
//! hashes are uniformly distributed (worst-case temp-table btree locality);
//! a fraction of entries is drawn from a shared pool so the mark set sees
//! realistic cross-manifest dedup.

use std::time::Instant;

use sqlx::PgPool;

use crate::manifest::{Manifest, ManifestEntry};

/// Deterministic splitmix64 — no external RNG dependency, uniform output.
struct SplitMix(u64);

impl SplitMix {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[lo, hi]` (inclusive). `hi >= lo`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }
}

/// 32 uniformly-distributed bytes from a (domain, a, b) key.
fn synth_hash(domain: u64, a: u64, b: u64) -> [u8; 32] {
    let mut rng = SplitMix::new(
        domain
            .wrapping_mul(0xA076_1D64_78BD_642F)
            .wrapping_add(a.wrapping_mul(0xE703_7ED1_A0B4_28DB))
            .wrapping_add(b),
    );
    let mut out = [0u8; 32];
    for word in out.chunks_exact_mut(8) {
        word.copy_from_slice(&rng.next_u64().to_le_bytes());
    }
    out
}

/// Per-path entry count, drawn from the recorded mix (weights /10000):
/// 50% 8–40, 25% 41–96, 15% 97–256, 9% 257–1280, 0.9% 1281–9600,
/// 0.09% 9601–65536, 0.01% 65537–163840. Median ~24 (≈1.5 MB NAR at
/// 64 KiB average chunk), tail capped at the 10 GB-NAR class.
fn entry_count(rng: &mut SplitMix) -> usize {
    let bucket = rng.range(0, 9999);
    let (lo, hi) = match bucket {
        0..=4999 => (8, 40),
        5000..=7499 => (41, 96),
        7500..=8999 => (97, 256),
        9000..=9899 => (257, 1280),
        9900..=9989 => (1281, 9600),
        9990..=9998 => (9601, 65536),
        _ => (65537, 163_840),
    };
    rng.range(lo, hi) as usize
}

/// Probability (×/1000) that an entry's hash comes from the shared pool
/// rather than being unique to its (path, entry) — models cross-manifest
/// dedup so `ON CONFLICT DO NOTHING` does real work.
const SHARED_PER_MILLE: u64 = 600;

/// Build one path's serialized `chunk_list`.
fn build_chunk_list(path_idx: u64, pool_size: u64) -> Vec<u8> {
    let mut rng = SplitMix::new(0xBE5C_0000_0000_0000 ^ path_idx);
    let n = entry_count(&mut rng);
    let mut entries = Vec::with_capacity(n);
    for entry_idx in 0..n {
        let hash = if rng.range(0, 999) < SHARED_PER_MILLE {
            synth_hash(1, rng.range(0, pool_size - 1), 0)
        } else {
            synth_hash(2, path_idx, entry_idx as u64)
        };
        entries.push(ManifestEntry {
            hash,
            size: 64 * 1024,
        });
    }
    Manifest { entries }.serialize()
}

/// Seed `[start, end)` paths in batches: one narinfo + manifests +
/// manifest_data INSERT per batch. Returns (total_entries, blob_bytes);
/// `total_entries` counts manifest entries before any dedup.
async fn seed_range(pool: PgPool, start: u64, end: u64, pool_size: u64) -> (u64, u64) {
    const BATCH: u64 = 400;
    let mut refs_total = 0u64;
    let mut blob_bytes = 0u64;
    let mut next = start;
    while next < end {
        let batch_end = (next + BATCH).min(end);
        let count = (batch_end - next) as usize;
        let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(count);
        let mut paths: Vec<String> = Vec::with_capacity(count);
        let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(count);
        for path_idx in next..batch_end {
            let blob = build_chunk_list(path_idx, pool_size);
            refs_total += ((blob.len() - 1) / 36) as u64;
            blob_bytes += blob.len() as u64;
            hashes.push(synth_hash(0, path_idx, 0).to_vec());
            paths.push(format!(
                "/nix/store/{path_idx:032x}-mark-scan-bench-{path_idx}"
            ));
            blobs.push(blob);
        }
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             SELECT t.h, t.p, t.h, 0 FROM unnest($1::bytea[], $2::text[]) AS t(h, p)",
        )
        .bind(&hashes)
        .bind(&paths)
        .execute(&pool)
        .await
        .expect("seed narinfo batch");
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status) \
             SELECT h, 'complete' FROM unnest($1::bytea[]) AS t(h)",
        )
        .bind(&hashes)
        .execute(&pool)
        .await
        .expect("seed manifests batch");
        sqlx::query(
            "INSERT INTO manifest_data (store_path_hash, chunk_list) \
             SELECT * FROM unnest($1::bytea[], $2::bytea[]) AS t(h, c)",
        )
        .bind(&hashes)
        .bind(&blobs)
        .execute(&pool)
        .await
        .expect("seed manifest_data batch");
        next = batch_end;
    }
    (refs_total, blob_bytes)
}

/// The mark phase exactly as the collector will run it: one connection,
/// keyset pages over `manifest_data JOIN manifests`, fallible parse per
/// row, batched `ON CONFLICT DO NOTHING` inserts into the temp table.
/// Returns (rows_scanned, refs_parsed, mark_set_size, temp_table_bytes,
/// wall_clock).
async fn run_mark_scan(pool: &PgPool) -> (u64, u64, i64, i64, std::time::Duration) {
    /// Manifest rows fetched per page.
    const PAGE_ROWS: i64 = 1000;
    /// Parsed hashes buffered before an insert flush.
    const INSERT_BUF: usize = 50_000;

    let mut conn = pool.acquire().await.expect("acquire mark connection");
    sqlx::query("CREATE TEMP TABLE live_chunks (blake3_hash BYTEA PRIMARY KEY)")
        .execute(&mut *conn)
        .await
        .expect("create temp table");

    let started = Instant::now();
    let mut cursor: Vec<u8> = Vec::new();
    let mut rows_scanned = 0u64;
    let mut refs_parsed = 0u64;
    let mut buf: Vec<Vec<u8>> = Vec::with_capacity(INSERT_BUF + 4096);
    loop {
        let page: Vec<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT md.store_path_hash, md.chunk_list \
               FROM manifest_data md \
               JOIN manifests m USING (store_path_hash) \
              WHERE md.store_path_hash > $1 \
              ORDER BY md.store_path_hash \
              LIMIT $2",
        )
        .bind(&cursor)
        .bind(PAGE_ROWS)
        .fetch_all(&mut *conn)
        .await
        .expect("mark page fetch");
        if page.is_empty() {
            break;
        }
        cursor = page.last().expect("non-empty page").0.clone();
        for (_hash, chunk_list) in &page {
            rows_scanned += 1;
            let hashes = super::try_parse_unique_chunk_hashes(chunk_list)
                .expect("bench seeds only well-formed manifests");
            refs_parsed += hashes.len() as u64;
            buf.extend(hashes.iter().map(|h| h.to_vec()));
        }
        if buf.len() >= INSERT_BUF {
            sqlx::query(
                "INSERT INTO live_chunks (blake3_hash) \
                 SELECT unnest($1::bytea[]) ON CONFLICT DO NOTHING",
            )
            .bind(&buf)
            .execute(&mut *conn)
            .await
            .expect("mark insert flush");
            buf.clear();
        }
    }
    if !buf.is_empty() {
        sqlx::query(
            "INSERT INTO live_chunks (blake3_hash) \
             SELECT unnest($1::bytea[]) ON CONFLICT DO NOTHING",
        )
        .bind(&buf)
        .execute(&mut *conn)
        .await
        .expect("mark insert final flush");
        buf.clear();
    }
    let elapsed = started.elapsed();

    let mark_set_size: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_chunks")
        .fetch_one(&mut *conn)
        .await
        .expect("mark set count");
    let temp_table_bytes: i64 = sqlx::query_scalar("SELECT pg_total_relation_size('live_chunks')")
        .fetch_one(&mut *conn)
        .await
        .expect("temp table size");
    sqlx::query("DROP TABLE live_chunks")
        .execute(&mut *conn)
        .await
        .expect("drop temp table");

    (
        rows_scanned,
        refs_parsed,
        mark_set_size,
        temp_table_bytes,
        elapsed,
    )
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The measurement. Smoke scale by default; see the module doc for the
/// measurement-scale invocation.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "mark-scan cost measurement: minutes-long at measurement scale, run explicitly"]
async fn mark_scan_bench() {
    let n_paths = env_u64("MARK_SCAN_BENCH_PATHS", 2_000);
    let seed_workers = env_u64("MARK_SCAN_BENCH_SEED_WORKERS", 12).max(1);
    // Shared-pool size scales with the population so the dedup factor
    // stays roughly constant across scale points.
    let pool_size = (n_paths * 4).clamp(4_096, 8_000_000);

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;

    // ---- Seed (parallel; fixture setup, not part of the measurement) ----
    let seed_started = Instant::now();
    // db.pool caps at 5 connections; reopen() extra pools so the seed
    // workers are not connection-starved.
    let mut pools = vec![db.pool.clone()];
    while (pools.len() * 5) < seed_workers as usize {
        pools.push(db.reopen().await);
    }
    let chunk = n_paths.div_ceil(seed_workers);
    let mut tasks = Vec::new();
    for w in 0..seed_workers {
        let start = w * chunk;
        let end = ((w + 1) * chunk).min(n_paths);
        if start >= end {
            break;
        }
        let pool = pools[(w as usize) % pools.len()].clone();
        tasks.push(tokio::spawn(seed_range(pool, start, end, pool_size)));
    }
    let mut entries_seeded = 0u64;
    let mut blob_bytes = 0u64;
    for t in tasks {
        let (r, b) = t.await.expect("seed task");
        entries_seeded += r;
        blob_bytes += b;
    }
    let seed_elapsed = seed_started.elapsed();
    // Planner stats so the join strategy matches a steady-state table.
    sqlx::query("ANALYZE manifest_data")
        .execute(&db.pool)
        .await
        .expect("analyze manifest_data");
    sqlx::query("ANALYZE manifests")
        .execute(&db.pool)
        .await
        .expect("analyze manifests");
    sqlx::query("ANALYZE narinfo")
        .execute(&db.pool)
        .await
        .expect("analyze narinfo");
    let manifest_data_bytes: i64 =
        sqlx::query_scalar("SELECT pg_total_relation_size('manifest_data')")
            .fetch_one(&db.pool)
            .await
            .expect("manifest_data size");

    // ---- Mark scan (the measurement) ----
    let (rows_scanned, refs_parsed, mark_set_size, temp_table_bytes, elapsed) =
        run_mark_scan(&db.pool).await;

    println!(
        "mark_scan_bench: paths={n_paths} seed_workers={seed_workers} shared_pool={pool_size}\n\
         mark_scan_bench: seeded entries={entries_seeded} blob_bytes={blob_bytes} \
         manifest_data_total_bytes={manifest_data_bytes} seed_secs={:.1}\n\
         mark_scan_bench: SCAN rows_scanned={rows_scanned} refs_parsed={refs_parsed} \
         mark_set_size={mark_set_size} temp_table_bytes={temp_table_bytes} \
         scan_secs={:.3}",
        seed_elapsed.as_secs_f64(),
        elapsed.as_secs_f64(),
    );

    // Sanity, not the verdict: the verdict (against the §5a threshold) is
    // recorded in the invariant map from the printed figures.
    assert_eq!(rows_scanned, n_paths, "every seeded manifest is scanned");
    assert!(
        refs_parsed > 0 && refs_parsed <= entries_seeded,
        "per-manifest dedup'd references are bounded by the seeded entries"
    );
    assert!(
        mark_set_size > 0 && (mark_set_size as u64) <= refs_parsed,
        "mark set is non-empty and no larger than the parsed references"
    );
}

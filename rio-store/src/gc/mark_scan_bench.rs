//! Mark-scan cost measurement for the lazy chunk collector (design §5a go/no-go).
//!
//! Seeds N synthetic chunked paths (`narinfo` + `manifests` + `manifest_data`
//! with realistic `chunk_list` blobs) and then measures the collector's mark
//! phase — deriving the live-chunk set from the durable manifests at scan
//! time, no new write-path state, the live set rebuilt per cycle into a
//! session temp table — in several formulations of that same architecture:
//!
//! - `mark_scan_bench` — the design §4.1 prescribed shape: stream
//!   `manifest_data.chunk_list` joined to `manifests` in keyset pages on ONE
//!   connection, parse each blob with [`super::try_parse_unique_chunk_hashes`],
//!   and accumulate the live set into a `TEMP TABLE live_chunks(blake3_hash
//!   BYTEA PRIMARY KEY)` via batched `INSERT … ON CONFLICT DO NOTHING`.
//! - `mark_scan_bench_copy_groupby` — same scan and client-side fallible
//!   parse, but references stream into an unindexed temp table via
//!   `COPY … FROM STDIN (FORMAT binary)` and are deduplicated once at the
//!   end by a single set-based `GROUP BY` into `live_chunks` (no per-row
//!   `ON CONFLICT` probe, no btree maintained during the scan).
//! - `mark_scan_bench_server_side` — no client round-trip: a fail-closed
//!   validation pass (version byte, 36-byte entry alignment, `MAX_CHUNKS`)
//!   followed by one `CREATE TEMP TABLE live_chunks AS SELECT DISTINCT
//!   <hash slice>` that expands every `chunk_list` inside the server
//!   (`generate_series` + `substring` over a once-per-row detoasted copy).
//! - `mark_scan_bench_collect_phase` — the design §5b re-entry gate (c)
//!   measurement: seeds a `chunks` table to match the manifest fixture
//!   (one row per distinct referenced hash, plus an unreferenced
//!   would-collect population — see [`UNREF_PER_MILLE_OF_LIVE`]), then runs
//!   the full collect cycle the adopted design prescribes — server-side
//!   mark, a one-time unique index + ANALYZE on the mark product, and the
//!   batched soft-delete anti-join (`NOT EXISTS` against `live_chunks`,
//!   `GREATEST(created_at, last_referenced_at)` grace term, keyset cursor +
//!   LIMIT batches, loop-until-short, one statement per batch) — timing
//!   mark, prepare, and collect separately plus combined. The collect loop
//!   is capped at a per-cycle victim budget (the design §4.1 step 3 v4
//!   capped collect — see [`COLLECT_CAP_DEFAULT`]); a backlog larger than
//!   the cap stops the cycle at the cap with the keyset cursor reported,
//!   which is the gate-(c) v4 verdict shape. The candidate-scan and
//!   soft-delete terms are timed separately so the sparse full-pass scan
//!   cost stays visible next to the victim-write cost. Also carries the
//!   EXPLAIN plan-shape guard (re-entry gate (b)) over the expansion and
//!   the per-batch anti-join statement.
//!
//! Each test reports wall-clock (per phase and total), rows scanned,
//! references parsed/expanded, mark-set size (distinct hashes), table sizes,
//! and the database-wide temp-file spill delta — the numbers the go/no-go
//! threshold (full scan at the ~1.5 M-path scale, linear-or-better growth,
//! bounded memory) is judged against. Measured figures are volatile and
//! belong in the introducing commit message and the campaign archive
//! (`docs/spec/models/refcount-records.md` §3, the measurement-gate chain —
//! the recording destination for any future measurement round), never in
//! this file.
//!
//! `#[ignore]`d: the measurement-scale runs take minutes by design. Run one
//! explicitly, e.g.:
//!
//! ```text
//! MARK_SCAN_BENCH_PATHS=1500000 \
//!   cargo nextest run -p rio-store -E 'test(mark_scan_bench_server_side)' \
//!   --run-ignored all --release
//! ```
//!
//! Env knobs (all optional):
//! - `MARK_SCAN_BENCH_PATHS`        — number of chunked paths to seed (default 2000, smoke scale)
//! - `MARK_SCAN_BENCH_SEED_WORKERS` — parallel seeding tasks (default 12; seeding only — the
//!   mark scan itself is always one connection, like the collector)
//! - `MARK_SCAN_BENCH_WORK_MEM`     — session `work_mem` for the set-based formulations
//!   (default 4GB; bounds the dedup aggregate, which spills to temp files past it; also used
//!   as `maintenance_work_mem` for the collect bench's one-time `live_chunks` index build)
//! - `MARK_SCAN_BENCH_COPY_ROWS`    — references per `COPY` statement in the copy+GROUP BY
//!   formulation (default 1,000,000)
//! - `MARK_SCAN_BENCH_COLLECT_BATCH` — per-batch LIMIT for the collect-phase soft-delete loop
//!   (default 10,000)
//! - `MARK_SCAN_BENCH_COLLECT_CAP`  — per-cycle victim cap for the collect-phase loop
//!   (default [`COLLECT_CAP_DEFAULT`]; smaller values exercise the cap stop on smoke fixtures)
//!
//! At or below 20 k paths the alternative formulations also run the
//! prescribed shape on the same fixture and assert both produce a mark set
//! of the same cardinality; at every scale they assert a known manifest's
//! hashes are all present in the produced live set (decisive against
//! slicing/encoding misalignment).
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

// The server-side mark statements are the SHIPPED collector statements
// (gc::collect), re-imported under the bench's historical names so the
// EXPLAIN plan-shape guard (re-entry gate (b)), the small-scale
// equivalence checks, and the chunks-fixture seeding all exercise the
// production SQL rather than a copy that could drift from it.
use super::collect::{
    COLLECT_BATCH_SELECT_SQL, COLLECT_BATCH_UPDATE_SQL, MARK_EXPANSION_SQL as SERVER_MARK_SQL,
    mark_validation_sql as server_validate_sql,
};

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

/// Session `work_mem` for the set-based formulations. Alphanumeric-only
/// guard keeps the value safe to splice into `SET work_mem = '…'`.
fn env_work_mem() -> String {
    let v = std::env::var("MARK_SCAN_BENCH_WORK_MEM").unwrap_or_else(|_| "4GB".to_string());
    assert!(
        !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric()),
        "MARK_SCAN_BENCH_WORK_MEM must be a plain PostgreSQL memory quantity, e.g. 4GB"
    );
    v
}

/// At or below this path count the alternative formulations also run the
/// prescribed shape on the same fixture and compare mark-set cardinality.
/// Above it the prescribed shape is too slow to re-run casually and its
/// figures are already on record.
const EQUIVALENCE_MAX_PATHS: u64 = 20_000;

/// Deterministic membership sample: the (capped) hash list of path 0's
/// manifest. Every alternative formulation must mark all of them —
/// cardinality alone could mask a uniform slicing offset, this cannot.
fn sample_hashes(pool_size: u64) -> Vec<[u8; 32]> {
    let blob = build_chunk_list(0, pool_size);
    let mut hashes =
        super::try_parse_unique_chunk_hashes(&blob).expect("synthetic manifest is well-formed");
    hashes.truncate(64);
    hashes
}

/// Fixture-seeding outcome (never part of a measured phase).
struct SeededVolume {
    entries_seeded: u64,
    blob_bytes: u64,
    manifest_data_bytes: i64,
    seed_secs: f64,
}

/// Seed the synthetic narinfo/manifests/manifest_data volume with
/// `seed_workers` parallel tasks and refresh planner stats. Fixture setup
/// shared by every formulation; never part of a measured phase.
async fn seed_fixture(
    db: &rio_test_support::TestDb,
    n_paths: u64,
    seed_workers: u64,
    pool_size: u64,
) -> SeededVolume {
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
    let seed_secs = seed_started.elapsed().as_secs_f64();
    // Planner stats so the scan/join strategy matches a steady-state table.
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
    SeededVolume {
        entries_seeded,
        blob_bytes,
        manifest_data_bytes,
        seed_secs,
    }
}

/// Cumulative temp-file bytes written database-wide (`pg_stat_database`),
/// read before/after a formulation to report its spill volume. Spill is
/// the *bounded-memory* mechanism for the set-based aggregates — past
/// `work_mem` they partition to temp files instead of growing resident.
async fn db_temp_bytes(conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>) -> i64 {
    sqlx::query_scalar("SELECT temp_bytes FROM pg_stat_database WHERE datname = current_database()")
        .fetch_one(&mut **conn)
        .await
        .expect("pg_stat_database temp_bytes")
}

/// `EXPLAIN` (no execution) of one statement, for the measurement record.
async fn explain_lines(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    sql: &str,
) -> Vec<String> {
    // AssertSqlSafe: wraps one of this module's fixed statements, test-only.
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT text) {sql}")))
        .fetch_all(&mut **conn)
        .await
        .expect("EXPLAIN")
}

/// Reduce an `EXPLAIN` plan to one line: top node + planned worker count.
fn summarize_plan(lines: &[String]) -> String {
    let top = lines.first().map(String::as_str).unwrap_or("").trim();
    let workers = lines
        .iter()
        .find_map(|l| l.trim().strip_prefix("Workers Planned: "))
        .unwrap_or("0");
    format!("{top} [workers_planned={workers}]")
}

/// Every hash of one known manifest must be present in the produced
/// `live_chunks`. Runs on the mark connection before the temp tables are
/// dropped; never part of a measured phase.
async fn assert_sample_marked(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    sample: &[[u8; 32]],
) {
    let sample_vecs: Vec<Vec<u8>> = sample.iter().map(|h| h.to_vec()).collect();
    let present: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM live_chunks WHERE blake3_hash = ANY($1)")
            .bind(&sample_vecs)
            .fetch_one(&mut **conn)
            .await
            .expect("sample membership probe");
    assert_eq!(
        present,
        sample.len() as i64,
        "every sampled reference from a known manifest is in the live set"
    );
}

/// `COPY … (FORMAT binary)` per-statement header: 11-byte signature,
/// 4-byte flags (0), 4-byte header-extension length (0).
const COPY_BINARY_HEADER: &[u8] = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0";

/// One binary COPY tuple for a single BYTEA(32) column: field count
/// (i16 = 1) + field length (i32 = 32) + 32 hash bytes.
const COPY_TUPLE_BYTES: usize = 2 + 4 + 32;

/// The copy+GROUP BY formulation's dedup statement. `mark_refs` is
/// deliberately not ANALYZEd first: with no column stats the planner uses
/// its default distinct estimate and picks the hash aggregate (the
/// intended set-based dedup); sampled stats on a near-all-distinct bytea
/// column tend to push it to an external sort of the whole reference
/// stream instead.
const DEDUP_SQL: &str =
    "CREATE TEMP TABLE live_chunks AS SELECT blake3_hash FROM mark_refs GROUP BY blake3_hash";

/// One complete `COPY mark_refs FROM STDIN (FORMAT binary)` statement
/// carrying `rows` pre-encoded tuples.
async fn copy_refs(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    tuples: &[u8],
    rows: usize,
) {
    let raw: &mut sqlx::PgConnection = &mut *conn;
    let mut copy = raw
        .copy_in_raw("COPY mark_refs (blake3_hash) FROM STDIN (FORMAT binary)")
        .await
        .expect("begin COPY");
    copy.send(COPY_BINARY_HEADER).await.expect("COPY header");
    copy.send(tuples).await.expect("COPY tuples");
    // Binary-format trailer: field count -1.
    copy.send(&(-1i16).to_be_bytes()[..])
        .await
        .expect("COPY trailer");
    let copied = copy.finish().await.expect("finish COPY");
    assert_eq!(copied, rows as u64, "COPY row count matches encoded tuples");
}

/// Outcome of one alternative-formulation mark run. `total` is the
/// wall-clock from the first mark statement to `live_chunks` being
/// complete — the figure the §5a threshold is judged against; the two
/// phases are formulation-specific (documented at each runner).
struct AltMarkStats {
    rows_scanned: u64,
    refs_fed: u64,
    mark_set_size: i64,
    refs_table_bytes: i64,
    live_table_bytes: i64,
    phase1: std::time::Duration,
    phase2: std::time::Duration,
    total: std::time::Duration,
    spill_delta_bytes: i64,
    plan_summary: String,
}

/// Candidate "copy + GROUP BY": the same one-connection keyset scan and
/// fallible client-side parse as [`run_mark_scan`], but the parsed
/// references stream into an unindexed session temp table with
/// `COPY … FROM STDIN (FORMAT binary)` and are deduplicated once at the
/// end by a single set-based `GROUP BY` into `live_chunks` — no per-row
/// `ON CONFLICT` probe, no btree maintained during the scan.
///
/// Phase 1 = scan + parse + COPY; phase 2 = the `GROUP BY`
/// materialization. Client memory stays bounded by one page of manifests
/// plus one COPY buffer; server memory by `work_mem` (the aggregate
/// spills to temp files past it) plus the session temp tables.
async fn run_mark_scan_copy_groupby(
    pool: &PgPool,
    work_mem: &str,
    copy_batch_rows: usize,
    print_plan: bool,
    sample: &[[u8; 32]],
) -> AltMarkStats {
    /// Manifest rows fetched per page (same as the prescribed shape).
    const PAGE_ROWS: i64 = 1000;

    let mut conn = pool.acquire().await.expect("acquire mark connection");
    // AssertSqlSafe: SET can't take bind parameters; the value is
    // alphanumeric-guarded by env_work_mem. Test-only.
    sqlx::query(sqlx::AssertSqlSafe(format!("SET work_mem = '{work_mem}'")))
        .execute(&mut *conn)
        .await
        .expect("set work_mem");
    // A TEMP table is already session-local and never WAL-logged (the
    // UNLOGGED property is implied); no PK, no index — dedup happens
    // once, set-based, in phase 2.
    sqlx::query("CREATE TEMP TABLE mark_refs (blake3_hash BYTEA)")
        .execute(&mut *conn)
        .await
        .expect("create mark_refs");
    let spill_before = db_temp_bytes(&mut conn).await;

    let started = Instant::now();
    let mut cursor: Vec<u8> = Vec::new();
    let mut rows_scanned = 0u64;
    let mut refs_fed = 0u64;
    // Pre-encoded COPY tuple stream for the in-flight batch (the
    // per-statement header/trailer are added at flush time).
    let mut tuples: Vec<u8> = Vec::with_capacity(copy_batch_rows * COPY_TUPLE_BYTES + 64);
    let mut tuple_rows = 0usize;
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
            refs_fed += hashes.len() as u64;
            for h in &hashes {
                tuples.extend_from_slice(&1i16.to_be_bytes());
                tuples.extend_from_slice(&32i32.to_be_bytes());
                tuples.extend_from_slice(h);
            }
            tuple_rows += hashes.len();
        }
        if tuple_rows >= copy_batch_rows {
            copy_refs(&mut conn, &tuples, tuple_rows).await;
            tuples.clear();
            tuple_rows = 0;
        }
    }
    if tuple_rows > 0 {
        copy_refs(&mut conn, &tuples, tuple_rows).await;
        tuples.clear();
    }
    let phase1 = started.elapsed();

    let plan = explain_lines(&mut conn, DEDUP_SQL).await;
    if print_plan {
        for line in &plan {
            println!("mark_scan_bench plan| {line}");
        }
    }
    sqlx::query(DEDUP_SQL)
        .execute(&mut *conn)
        .await
        .expect("dedup GROUP BY");
    let total = started.elapsed();
    let phase2 = total - phase1;

    let spill_delta_bytes = db_temp_bytes(&mut conn).await - spill_before;
    let mark_set_size: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_chunks")
        .fetch_one(&mut *conn)
        .await
        .expect("mark set count");
    let refs_table_bytes: i64 = sqlx::query_scalar("SELECT pg_total_relation_size('mark_refs')")
        .fetch_one(&mut *conn)
        .await
        .expect("mark_refs size");
    let live_table_bytes: i64 = sqlx::query_scalar("SELECT pg_total_relation_size('live_chunks')")
        .fetch_one(&mut *conn)
        .await
        .expect("live_chunks size");
    assert_sample_marked(&mut conn, sample).await;
    sqlx::query("DROP TABLE live_chunks, mark_refs")
        .execute(&mut *conn)
        .await
        .expect("drop temp tables");

    AltMarkStats {
        rows_scanned,
        refs_fed,
        mark_set_size,
        refs_table_bytes,
        live_table_bytes,
        phase1,
        phase2,
        total,
        spill_delta_bytes,
        plan_summary: summarize_plan(&plan),
    }
}

/// Candidate "server-side expansion": no client round-trip — a
/// fail-closed validation pass over the joined manifests (version byte,
/// 36-byte entry alignment, `MAX_CHUNKS` bound; any violation aborts,
/// preserving the §4.4 corrupt-manifest polarity), then the single
/// [`SERVER_MARK_SQL`] expansion + dedup statement.
///
/// Phase 1 = the validation pass; phase 2 = the expansion + dedup
/// statement. The client never sees a `chunk_list`; server memory is
/// bounded by `work_mem` (the aggregate spills to temp files past it)
/// plus the session temp table.
async fn run_mark_scan_server_side(
    pool: &PgPool,
    work_mem: &str,
    print_plan: bool,
    sample: &[[u8; 32]],
) -> AltMarkStats {
    let mut conn = pool.acquire().await.expect("acquire mark connection");
    // AssertSqlSafe: SET can't take bind parameters; the value is
    // alphanumeric-guarded by env_work_mem. Test-only.
    sqlx::query(sqlx::AssertSqlSafe(format!("SET work_mem = '{work_mem}'")))
        .execute(&mut *conn)
        .await
        .expect("set work_mem");
    let spill_before = db_temp_bytes(&mut conn).await;

    // Fail-closed validation pass; see server_validate_sql.
    let validate_sql = server_validate_sql(false);

    let plan = explain_lines(&mut conn, SERVER_MARK_SQL).await;
    if print_plan {
        for line in &plan {
            println!("mark_scan_bench plan| {line}");
        }
    }

    let started = Instant::now();
    // AssertSqlSafe: interpolates only the MAX_CHUNKS integer const, test-only.
    let malformed: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(validate_sql))
        .fetch_one(&mut *conn)
        .await
        .expect("validation pass");
    assert_eq!(
        malformed, 0,
        "fail-closed: the fixture seeds only well-formed manifests"
    );
    let phase1 = started.elapsed();
    sqlx::query(SERVER_MARK_SQL)
        .execute(&mut *conn)
        .await
        .expect("server-side mark");
    let total = started.elapsed();
    let phase2 = total - phase1;

    let spill_delta_bytes = db_temp_bytes(&mut conn).await - spill_before;
    let mark_set_size: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_chunks")
        .fetch_one(&mut *conn)
        .await
        .expect("mark set count");
    let live_table_bytes: i64 = sqlx::query_scalar("SELECT pg_total_relation_size('live_chunks')")
        .fetch_one(&mut *conn)
        .await
        .expect("live_chunks size");
    // Fixture totals for the record (what the statement walked); not part
    // of the formulation itself.
    let (rows_scanned, refs_fed): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM((octet_length(md.chunk_list) - 1) / 36), 0)::BIGINT \
           FROM manifest_data md \
           JOIN manifests m USING (store_path_hash)",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("fixture totals");
    assert_sample_marked(&mut conn, sample).await;
    sqlx::query("DROP TABLE live_chunks")
        .execute(&mut *conn)
        .await
        .expect("drop temp table");

    AltMarkStats {
        rows_scanned: rows_scanned as u64,
        refs_fed: refs_fed as u64,
        mark_set_size,
        refs_table_bytes: 0,
        live_table_bytes,
        phase1,
        phase2,
        total,
        spill_delta_bytes,
        plan_summary: summarize_plan(&plan),
    }
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
    let seeded = seed_fixture(&db, n_paths, seed_workers, pool_size).await;

    // ---- Mark scan (the measurement) ----
    let (rows_scanned, refs_parsed, mark_set_size, temp_table_bytes, elapsed) =
        run_mark_scan(&db.pool).await;

    println!(
        "mark_scan_bench: paths={n_paths} seed_workers={seed_workers} shared_pool={pool_size}\n\
         mark_scan_bench: seeded entries={} blob_bytes={} \
         manifest_data_total_bytes={} seed_secs={:.1}\n\
         mark_scan_bench: SCAN rows_scanned={rows_scanned} refs_parsed={refs_parsed} \
         mark_set_size={mark_set_size} temp_table_bytes={temp_table_bytes} \
         scan_secs={:.3}",
        seeded.entries_seeded,
        seeded.blob_bytes,
        seeded.manifest_data_bytes,
        seeded.seed_secs,
        elapsed.as_secs_f64(),
    );

    // Sanity, not the verdict: the verdict (against the §5a threshold) is
    // recorded in refcount-records.md (the measurement-gate chain) from
    // the printed figures.
    assert_eq!(rows_scanned, n_paths, "every seeded manifest is scanned");
    assert!(
        refs_parsed > 0 && refs_parsed <= seeded.entries_seeded,
        "per-manifest dedup'd references are bounded by the seeded entries"
    );
    assert!(
        mark_set_size > 0 && (mark_set_size as u64) <= refs_parsed,
        "mark set is non-empty and no larger than the parsed references"
    );
}

/// Alternative formulation 1: client parse + binary `COPY` into an
/// unindexed temp table + one set-based `GROUP BY`. Smoke scale by
/// default; see the module doc for the measurement-scale invocation.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "mark-scan cost measurement: minutes-long at measurement scale, run explicitly"]
async fn mark_scan_bench_copy_groupby() {
    let n_paths = env_u64("MARK_SCAN_BENCH_PATHS", 2_000);
    let seed_workers = env_u64("MARK_SCAN_BENCH_SEED_WORKERS", 12).max(1);
    let work_mem = env_work_mem();
    let copy_batch_rows = env_u64("MARK_SCAN_BENCH_COPY_ROWS", 1_000_000).max(1) as usize;
    let pool_size = (n_paths * 4).clamp(4_096, 8_000_000);

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    let seeded = seed_fixture(&db, n_paths, seed_workers, pool_size).await;
    let sample = sample_hashes(pool_size);

    let stats = run_mark_scan_copy_groupby(
        &db.pool,
        &work_mem,
        copy_batch_rows,
        n_paths <= EQUIVALENCE_MAX_PATHS,
        &sample,
    )
    .await;

    println!(
        "mark_scan_bench[copy_groupby]: paths={n_paths} seed_workers={seed_workers} \
         shared_pool={pool_size} work_mem={work_mem} copy_rows={copy_batch_rows}\n\
         mark_scan_bench[copy_groupby]: seeded entries={} blob_bytes={} \
         manifest_data_total_bytes={} seed_secs={:.1}\n\
         mark_scan_bench[copy_groupby]: SCAN rows_scanned={} refs_copied={} mark_set_size={} \
         refs_table_bytes={} live_table_bytes={} spill_delta_bytes={} \
         copy_secs={:.3} dedup_secs={:.3} scan_secs={:.3}\n\
         mark_scan_bench[copy_groupby]: dedup_plan={}",
        seeded.entries_seeded,
        seeded.blob_bytes,
        seeded.manifest_data_bytes,
        seeded.seed_secs,
        stats.rows_scanned,
        stats.refs_fed,
        stats.mark_set_size,
        stats.refs_table_bytes,
        stats.live_table_bytes,
        stats.spill_delta_bytes,
        stats.phase1.as_secs_f64(),
        stats.phase2.as_secs_f64(),
        stats.total.as_secs_f64(),
        stats.plan_summary,
    );

    assert_eq!(
        stats.rows_scanned, n_paths,
        "every seeded manifest is scanned"
    );
    assert!(
        stats.refs_fed > 0 && stats.refs_fed <= seeded.entries_seeded,
        "per-manifest dedup'd references are bounded by the seeded entries"
    );
    assert!(
        stats.mark_set_size > 0 && (stats.mark_set_size as u64) <= stats.refs_fed,
        "mark set is non-empty and no larger than the copied references"
    );

    // Cross-formulation equivalence at small scale: the prescribed shape
    // and the set-based dedup must agree on the live set's cardinality.
    if n_paths <= EQUIVALENCE_MAX_PATHS {
        let (_, baseline_refs, baseline_mark_set, _, _) = run_mark_scan(&db.pool).await;
        assert_eq!(
            baseline_refs, stats.refs_fed,
            "both formulations parse the same reference stream"
        );
        assert_eq!(
            baseline_mark_set, stats.mark_set_size,
            "both formulations derive the same live-set cardinality"
        );
    }
}

/// Alternative formulation 2: fully server-side expansion + dedup, no
/// client round-trip. Smoke scale by default; see the module doc for the
/// measurement-scale invocation.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "mark-scan cost measurement: minutes-long at measurement scale, run explicitly"]
async fn mark_scan_bench_server_side() {
    let n_paths = env_u64("MARK_SCAN_BENCH_PATHS", 2_000);
    let seed_workers = env_u64("MARK_SCAN_BENCH_SEED_WORKERS", 12).max(1);
    let work_mem = env_work_mem();
    let pool_size = (n_paths * 4).clamp(4_096, 8_000_000);

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    let seeded = seed_fixture(&db, n_paths, seed_workers, pool_size).await;
    let sample = sample_hashes(pool_size);

    let stats = run_mark_scan_server_side(
        &db.pool,
        &work_mem,
        n_paths <= EQUIVALENCE_MAX_PATHS,
        &sample,
    )
    .await;

    println!(
        "mark_scan_bench[server_side]: paths={n_paths} seed_workers={seed_workers} \
         shared_pool={pool_size} work_mem={work_mem}\n\
         mark_scan_bench[server_side]: seeded entries={} blob_bytes={} \
         manifest_data_total_bytes={} seed_secs={:.1}\n\
         mark_scan_bench[server_side]: SCAN rows_scanned={} refs_expanded={} mark_set_size={} \
         live_table_bytes={} spill_delta_bytes={} \
         validate_secs={:.3} aggregate_secs={:.3} scan_secs={:.3}\n\
         mark_scan_bench[server_side]: mark_plan={}",
        seeded.entries_seeded,
        seeded.blob_bytes,
        seeded.manifest_data_bytes,
        seeded.seed_secs,
        stats.rows_scanned,
        stats.refs_fed,
        stats.mark_set_size,
        stats.live_table_bytes,
        stats.spill_delta_bytes,
        stats.phase1.as_secs_f64(),
        stats.phase2.as_secs_f64(),
        stats.total.as_secs_f64(),
        stats.plan_summary,
    );

    assert_eq!(
        stats.rows_scanned, n_paths,
        "every seeded manifest is scanned"
    );
    assert!(
        stats.refs_fed > 0 && stats.refs_fed <= seeded.entries_seeded,
        "expanded references are bounded by the seeded entries"
    );
    assert!(
        stats.mark_set_size > 0 && (stats.mark_set_size as u64) <= stats.refs_fed,
        "mark set is non-empty and no larger than the expanded references"
    );

    // Cross-formulation equivalence at small scale: the prescribed
    // client-side parse and the server-side expansion must agree on the
    // live set's cardinality.
    if n_paths <= EQUIVALENCE_MAX_PATHS {
        let (_, _, baseline_mark_set, _, _) = run_mark_scan(&db.pool).await;
        assert_eq!(
            baseline_mark_set, stats.mark_set_size,
            "both formulations derive the same live-set cardinality"
        );
    }
}

// ===========================================================================
// Collect-phase measurement (design §5b re-entry gate (c))
// ===========================================================================

/// Unreferenced (would-collect) chunk rows seeded per 1000 live rows:
/// 100 ⇒ 10 % of the mark set. Rationale: in steady state the
/// would-collect population of one cycle is the garbage produced since
/// the previous cycle (path deletions plus crashed uploads); 10 % of
/// the live set models a store turning over a tenth of its references
/// between cycles — generous enough that the soft-delete UPDATE volume
/// is clearly visible in the measurement, small enough that the scan
/// side (every existing chunk row probed against the mark set once)
/// stays the dominant term, as it will be in production.
const UNREF_PER_MILLE_OF_LIVE: i64 = 100;

/// Of the unreferenced population, one part in this divisor is seeded
/// younger than grace (created_at at the fixture's end) and another
/// part carries a fresh `last_referenced_at` touch on an old
/// created_at — 5 % each. Both sub-populations MUST survive the
/// collect, so the grace term and the GREATEST branch over the 068
/// touch column are exercised non-vacuously; the remaining 90 % is the
/// expected collect victim set. The protected rows are inserted LAST,
/// with their timestamps anchored at that moment, because the bulk
/// fixture inserts take minutes at the design-point scale — anchoring
/// them earlier would silently age them past grace before the cycle
/// snapshot is taken.
const UNREF_PROTECTED_DIVISOR: i64 = 20;

/// Default LIMIT per collect batch (`MARK_SCAN_BENCH_COLLECT_BATCH`
/// overrides). Large enough to amortize per-statement overhead over a
/// ~12 M-victim run, small enough that one batch's row locks and undo
/// stay bounded (the production arm batches for the same reason the
/// orphan-chunk sweep does).
const COLLECT_BATCH_DEFAULT: u64 = 10_000;

/// Default per-cycle victim cap for the collect loop
/// (`MARK_SCAN_BENCH_COLLECT_CAP` overrides). This is the design value
/// of the collector's `COLLECT_CYCLE_VICTIM_CAP` (design §4.1 step 3
/// v4; derivation recorded in refcount-records.md, T-1a.1b entry and
/// sign-off item 8): the gate-(c) capped-cycle confirmation runs at
/// exactly this value, so the bench defaults to it rather than to the
/// shipped const's `cfg(test)` override (which is sized for structural
/// tests, not for the design-point measurement). A backlog larger than
/// the cap stops the cycle after `cap / batch` batches with the keyset
/// cursor carried to the next cycle; smoke runs can pass a smaller cap
/// to exercise the stop/resume shape without a 500k-victim fixture.
const COLLECT_CAP_DEFAULT: u64 = 500_000;

/// Below this fixture scale the EXPLAIN plan-shape guard only prints
/// (small tables legitimately plan differently); at or above it the
/// guard asserts. The 150 k and 1.5 M measurement points both enforce.
const PLAN_SHAPE_ENFORCE_MIN_PATHS: u64 = 100_000;

/// The candidate-scan statement with literals spliced in place of the
/// bind parameters, for `EXPLAIN` only (EXPLAIN cannot carry binds and
/// a generic plan would not reflect the custom plan the loop gets).
fn collect_batch_explain_sql(cutoff: &str, batch_limit: i64) -> String {
    COLLECT_BATCH_SELECT_SQL
        .replace("$1", "'\\x'::bytea")
        .replace("$2::timestamptz", &format!("'{cutoff}'::timestamptz"))
        .replace("$3", &batch_limit.to_string())
}

/// Structural EXPLAIN plan-shape check (design §5b gate (b)): every
/// `required` substring must appear in the plan and no `forbidden`
/// substring may. Prints a note instead of panicking when `enforce` is
/// false (small fixtures plan differently and are not the guarded
/// regime).
fn check_plan_shape(
    name: &str,
    lines: &[String],
    required: &[&str],
    forbidden: &[&str],
    enforce: bool,
) {
    let text = lines.join("\n");
    let mut violations: Vec<String> = Vec::new();
    for r in required {
        if !text.contains(r) {
            violations.push(format!("missing required plan node {r:?}"));
        }
    }
    for f in forbidden {
        if text.contains(f) {
            violations.push(format!("contains forbidden plan node {f:?}"));
        }
    }
    if violations.is_empty() {
        println!("mark_scan_bench plan-shape[{name}]: OK");
        return;
    }
    let report = format!(
        "plan-shape guard [{name}]: {}\nplan:\n{text}",
        violations.join("; ")
    );
    assert!(!enforce, "{report}");
    println!("mark_scan_bench plan-shape[{name}] (not enforced at this scale): {report}");
}

/// What `seed_chunks_fixture` produced (fixture setup, never measured).
struct ChunkFixtureStats {
    live_rows: i64,
    unref_rows: i64,
    expected_collectable: i64,
    grace_protected: i64,
    touch_protected: i64,
    chunks_table_bytes: i64,
    seed_secs: f64,
}

/// Seed the `chunks` table to match the manifest fixture: one row per
/// distinct referenced hash (week-old created_at and uploaded_at,
/// `last_referenced_at` NULL — the column's steady state for rows
/// never re-referenced since insert), plus the unreferenced
/// would-collect population per [`UNREF_PER_MILLE_OF_LIVE`] /
/// [`UNREF_PROTECTED_DIVISOR`]. The referenced set is derived by the
/// same server-side expansion the mark statement uses (its SELECT body
/// is re-used verbatim) so the fixture cannot drift from the measured
/// statement; rows are inserted in hash order so the PK btree builds
/// by rightmost-page appends instead of random descent.
async fn seed_chunks_fixture(pool: &PgPool, work_mem: &str) -> ChunkFixtureStats {
    let mut conn = pool.acquire().await.expect("acquire chunk-seed connection");
    // AssertSqlSafe: SET can't take bind parameters; the value is
    // alphanumeric-guarded by env_work_mem. Test-only.
    sqlx::query(sqlx::AssertSqlSafe(format!("SET work_mem = '{work_mem}'")))
        .execute(&mut *conn)
        .await
        .expect("set work_mem");

    let started = Instant::now();
    let select_body = SERVER_MARK_SQL
        .strip_prefix("CREATE TEMP TABLE live_chunks AS ")
        .expect("SERVER_MARK_SQL starts with the CTAS prefix");
    // AssertSqlSafe: splices only this module's fixed statement body.
    let insert_live = format!(
        "INSERT INTO chunks (blake3_hash, size, created_at, uploaded_at, deleted) \
         SELECT s.blake3_hash, 65536, now() - interval '7 days', \
                now() - interval '7 days', FALSE \
           FROM ({select_body}) AS s \
          ORDER BY s.blake3_hash"
    );
    sqlx::query(sqlx::AssertSqlSafe(insert_live))
        .execute(&mut *conn)
        .await
        .expect("seed referenced chunks");
    let live_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chunks")
        .fetch_one(&mut *conn)
        .await
        .expect("live chunk count");

    // Unreferenced population: deterministic md5-derived 32-byte hashes
    // (domain-separated from the splitmix-derived manifest hashes), 90 %
    // old + untouched (the expected victims), 5 % younger than grace,
    // 5 % old but freshly touched via last_referenced_at. The
    // collectable bulk goes in first; the two protected sub-populations
    // go in LAST with their timestamps anchored at that moment — see
    // UNREF_PROTECTED_DIVISOR.
    let unref_rows = (live_rows * UNREF_PER_MILLE_OF_LIVE / 1000).max(20);
    let grace_protected = unref_rows / UNREF_PROTECTED_DIVISOR;
    let touch_protected = unref_rows / UNREF_PROTECTED_DIVISOR;
    let collectable = unref_rows - grace_protected - touch_protected;
    sqlx::query(
        "INSERT INTO chunks (blake3_hash, size, created_at, uploaded_at, deleted) \
         SELECT decode(md5('unref-' || g.i) || md5('unref-tail-' || g.i), 'hex'), 65536, \
                now() - interval '7 days', NULL, FALSE \
           FROM generate_series(1, $1::bigint) AS g(i) \
          ORDER BY 1",
    )
    .bind(collectable)
    .execute(&mut *conn)
    .await
    .expect("seed collectable unreferenced chunks");

    sqlx::query("ANALYZE chunks")
        .execute(&mut *conn)
        .await
        .expect("analyze chunks");
    let chunks_table_bytes: i64 = sqlx::query_scalar("SELECT pg_total_relation_size('chunks')")
        .fetch_one(&mut *conn)
        .await
        .expect("chunks table size");

    // Protected sub-populations, inserted last so their "recent"
    // timestamps really are recent relative to the cycle snapshot the
    // measured phase takes a few statements from now.
    sqlx::query(
        "INSERT INTO chunks (blake3_hash, size, created_at, uploaded_at, deleted) \
         SELECT decode(md5('unref-grace-' || g.i) || md5('unref-grace-tail-' || g.i), 'hex'), \
                65536, now(), NULL, FALSE \
           FROM generate_series(1, $1::bigint) AS g(i) \
          ORDER BY 1",
    )
    .bind(grace_protected)
    .execute(&mut *conn)
    .await
    .expect("seed grace-protected chunks");
    sqlx::query(
        "INSERT INTO chunks (blake3_hash, size, created_at, uploaded_at, deleted, \
                             last_referenced_at) \
         SELECT decode(md5('unref-touch-' || g.i) || md5('unref-touch-tail-' || g.i), 'hex'), \
                65536, now() - interval '7 days', NULL, FALSE, now() \
           FROM generate_series(1, $1::bigint) AS g(i) \
          ORDER BY 1",
    )
    .bind(touch_protected)
    .execute(&mut *conn)
    .await
    .expect("seed touch-protected chunks");
    let seed_secs = started.elapsed().as_secs_f64();

    ChunkFixtureStats {
        live_rows,
        unref_rows,
        expected_collectable: collectable,
        grace_protected,
        touch_protected,
        chunks_table_bytes,
        seed_secs,
    }
}

/// Outcome of one collect-phase run. `combined` is mark + prepare +
/// collect — the lock-held window the §5b gate-(c) verdict is judged
/// against; bookkeeping between the phases (sample probes, EXPLAIN,
/// counts) is excluded. `collect_scan` / `collect_delete` split the
/// collect term into the anti-join candidate-scan cost and the
/// soft-delete (UPDATE + commit) cost, so a sparse full-pass cycle
/// (scan-bound, few victims) is distinguishable from the victim-write
/// cost the cap bounds.
struct CollectPhaseStats {
    mark_validate: std::time::Duration,
    mark_expand: std::time::Duration,
    prepare: std::time::Duration,
    collect: std::time::Duration,
    collect_scan: std::time::Duration,
    collect_delete: std::time::Duration,
    combined: std::time::Duration,
    batches: u64,
    rows_collected: u64,
    cap_reached: bool,
    cursor_at_stop: Option<Vec<u8>>,
    mark_set_size: i64,
    live_table_bytes: i64,
    spill_delta_bytes: i64,
    expand_plan_summary: String,
    collect_plan_summary: String,
}

/// The full collect cycle as the adopted design prescribes it, on one
/// connection: fail-closed validation pass, server-side set-based
/// expansion into `live_chunks`, a one-time unique index + ANALYZE on
/// the mark product (what makes the per-batch anti-join an index probe
/// instead of a per-batch hash/sort of the whole mark set), then the
/// batched keyset soft-delete loop, capped at `victim_cap` victims per
/// cycle (the §4.1 step 3 v4 capped collect; the cursor at the stop
/// point is reported so a follow-up cycle could resume from it).
async fn run_collect_phase(
    pool: &PgPool,
    work_mem: &str,
    batch_limit: u64,
    victim_cap: u64,
    sample: &[[u8; 32]],
    enforce_plan_shape: bool,
) -> CollectPhaseStats {
    let mut conn = pool.acquire().await.expect("acquire collect connection");
    // AssertSqlSafe: SET can't take bind parameters; the value is
    // alphanumeric-guarded by env_work_mem. Test-only. work_mem bounds
    // the expansion's dedup; maintenance_work_mem bounds the one-time
    // index build's sort.
    sqlx::query(sqlx::AssertSqlSafe(format!("SET work_mem = '{work_mem}'")))
        .execute(&mut *conn)
        .await
        .expect("set work_mem");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET maintenance_work_mem = '{work_mem}'"
    )))
    .execute(&mut *conn)
    .await
    .expect("set maintenance_work_mem");
    let spill_before = db_temp_bytes(&mut conn).await;

    // Cycle snapshot: the grace cutoff is anchored at cycle start (the
    // production predicate compares against cycle_started_at − grace,
    // never against a per-batch now()). Same bigint bind pattern for
    // make_interval as orphan.rs / sweep.rs.
    let cutoff: String = sqlx::query_scalar("SELECT (now() - make_interval(secs => $1))::text")
        .bind(super::sweep::CHUNK_GRACE_SECS)
        .fetch_one(&mut *conn)
        .await
        .expect("grace cutoff");

    // Plans are captured before execution (EXPLAIN of the CTAS must run
    // while live_chunks does not exist yet); untimed.
    let expand_plan = explain_lines(&mut conn, SERVER_MARK_SQL).await;
    for line in &expand_plan {
        println!("mark_scan_bench[collect_phase] expand plan| {line}");
    }
    check_plan_shape(
        "mark-expansion",
        &expand_plan,
        &["generate_series"],
        &[],
        enforce_plan_shape,
    );
    assert!(
        expand_plan
            .iter()
            .any(|l| l.contains("HashAggregate") || l.contains("Unique") || l.contains("Group")),
        "mark expansion must deduplicate set-based (HashAggregate/Unique), got:\n{}",
        expand_plan.join("\n")
    );

    // ---- Mark (timed) ----
    let t0 = Instant::now();
    let malformed: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(server_validate_sql(false)))
        .fetch_one(&mut *conn)
        .await
        .expect("validation pass");
    assert_eq!(
        malformed, 0,
        "fail-closed: the fixture seeds only well-formed manifests"
    );
    let t1 = Instant::now();
    sqlx::query(SERVER_MARK_SQL)
        .execute(&mut *conn)
        .await
        .expect("server-side mark");
    let t2 = Instant::now();

    // Untimed bookkeeping between phases.
    assert_sample_marked(&mut conn, sample).await;
    let mark_set_size: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_chunks")
        .fetch_one(&mut *conn)
        .await
        .expect("mark set count");
    let live_table_bytes: i64 = sqlx::query_scalar("SELECT pg_total_relation_size('live_chunks')")
        .fetch_one(&mut *conn)
        .await
        .expect("live_chunks size");

    // ---- Prepare (timed): index + stats on the mark product ----
    let t3 = Instant::now();
    sqlx::query("CREATE UNIQUE INDEX live_chunks_hash_idx ON live_chunks (blake3_hash)")
        .execute(&mut *conn)
        .await
        .expect("index live_chunks");
    sqlx::query("ANALYZE live_chunks")
        .execute(&mut *conn)
        .await
        .expect("analyze live_chunks");
    let t4 = Instant::now();

    // Per-batch plan, captured against the pristine pre-collect state
    // the first batches will see; untimed.
    let collect_plan = explain_lines(
        &mut conn,
        &collect_batch_explain_sql(&cutoff, batch_limit as i64),
    )
    .await;
    for line in &collect_plan {
        println!("mark_scan_bench[collect_phase] collect plan| {line}");
    }
    check_plan_shape(
        "collect-anti-join",
        &collect_plan,
        &["chunks_pkey", "live_chunks_hash_idx"],
        &["Seq Scan on live_chunks", "Seq Scan on chunks", "Sort"],
        enforce_plan_shape,
    );

    // ---- Collect (timed): keyset-batched soft-delete loop ----
    // Per batch, one transaction: candidate scan (already hash-ordered
    // by the keyset SELECT), then the sorted `= ANY` soft-delete — the
    // retired orphan-chunk sweep's batch shape with the predicate swapped; the
    // soft-delete re-checks `deleted = FALSE` and the grace term in its
    // own WHERE per the T-1a.8 consequence (see
    // COLLECT_BATCH_UPDATE_SQL). The loop ends on the first short
    // candidate scan (pass complete) or when `victim_cap` victims have
    // been collected (cap stop — the v4 capped collect; the per-batch
    // LIMIT is clamped to the remaining budget so the cycle never
    // overshoots the cap). The candidate scan and the soft-delete
    // halves are accumulated separately.
    let t5 = Instant::now();
    let mut cursor: Vec<u8> = Vec::new();
    let mut batches = 0u64;
    let mut rows_collected = 0u64;
    let mut cap_reached = false;
    let mut scan_elapsed = std::time::Duration::ZERO;
    let mut delete_elapsed = std::time::Duration::ZERO;
    loop {
        let remaining = victim_cap.saturating_sub(rows_collected);
        if remaining == 0 {
            cap_reached = true;
            break;
        }
        let this_limit = batch_limit.min(remaining);
        let mut tx = sqlx::Connection::begin(&mut *conn)
            .await
            .expect("begin collect batch tx");
        let scan_started = Instant::now();
        let candidates: Vec<Vec<u8>> = sqlx::query_scalar(COLLECT_BATCH_SELECT_SQL.as_str())
            .bind(&cursor)
            .bind(&cutoff)
            .bind(this_limit as i64)
            .fetch_all(&mut *tx)
            .await
            .expect("collect candidate scan");
        scan_elapsed += scan_started.elapsed();
        if candidates.is_empty() {
            tx.commit().await.expect("commit empty collect batch");
            break;
        }
        batches += 1;
        let short = candidates.len() < this_limit as usize;
        let delete_started = Instant::now();
        let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as(COLLECT_BATCH_UPDATE_SQL.as_str())
            .bind(&candidates)
            .bind(&cutoff)
            .fetch_all(&mut *tx)
            .await
            .expect("collect soft-delete batch");
        rows_collected += rows.len() as u64;
        tx.commit().await.expect("commit collect batch");
        delete_elapsed += delete_started.elapsed();
        cursor = candidates
            .last()
            .expect("non-empty batch has a last hash")
            .clone();
        if short {
            break;
        }
    }
    let t6 = Instant::now();

    let spill_delta_bytes = db_temp_bytes(&mut conn).await - spill_before;
    sqlx::query("DROP TABLE live_chunks")
        .execute(&mut *conn)
        .await
        .expect("drop temp table");

    CollectPhaseStats {
        mark_validate: t1 - t0,
        mark_expand: t2 - t1,
        prepare: t4 - t3,
        collect: t6 - t5,
        collect_scan: scan_elapsed,
        collect_delete: delete_elapsed,
        combined: (t2 - t0) + (t4 - t3) + (t6 - t5),
        batches,
        rows_collected,
        cap_reached,
        cursor_at_stop: if cursor.is_empty() {
            None
        } else {
            Some(cursor)
        },
        mark_set_size,
        live_table_bytes,
        spill_delta_bytes,
        expand_plan_summary: summarize_plan(&expand_plan),
        collect_plan_summary: summarize_plan(&collect_plan),
    }
}

/// The collect-phase measurement (re-entry gate (c)). Smoke scale by
/// default; see the module doc for the measurement-scale invocation.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "collect-phase cost measurement: minutes-long at measurement scale, run explicitly"]
async fn mark_scan_bench_collect_phase() {
    let n_paths = env_u64("MARK_SCAN_BENCH_PATHS", 2_000);
    let seed_workers = env_u64("MARK_SCAN_BENCH_SEED_WORKERS", 12).max(1);
    let work_mem = env_work_mem();
    let batch_limit = env_u64("MARK_SCAN_BENCH_COLLECT_BATCH", COLLECT_BATCH_DEFAULT).max(1);
    let victim_cap = env_u64("MARK_SCAN_BENCH_COLLECT_CAP", COLLECT_CAP_DEFAULT).max(1);
    let pool_size = (n_paths * 4).clamp(4_096, 8_000_000);

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    let seeded = seed_fixture(&db, n_paths, seed_workers, pool_size).await;
    let sample = sample_hashes(pool_size);
    let chunks_fixture = seed_chunks_fixture(&db.pool, &work_mem).await;

    let stats = run_collect_phase(
        &db.pool,
        &work_mem,
        batch_limit,
        victim_cap,
        &sample,
        n_paths >= PLAN_SHAPE_ENFORCE_MIN_PATHS,
    )
    .await;

    println!(
        "mark_scan_bench[collect_phase]: paths={n_paths} seed_workers={seed_workers} \
         shared_pool={pool_size} work_mem={work_mem} batch_limit={batch_limit} \
         victim_cap={victim_cap}\n\
         mark_scan_bench[collect_phase]: seeded entries={} blob_bytes={} \
         manifest_data_total_bytes={} seed_secs={:.1}\n\
         mark_scan_bench[collect_phase]: CHUNKS live_rows={} unref_rows={} \
         expected_collectable={} grace_protected={} touch_protected={} \
         chunks_table_bytes={} chunk_seed_secs={:.1}\n\
         mark_scan_bench[collect_phase]: MARK validate_secs={:.3} expand_secs={:.3} \
         mark_set_size={} live_table_bytes={}\n\
         mark_scan_bench[collect_phase]: PREPARE index_analyze_secs={:.3}\n\
         mark_scan_bench[collect_phase]: COLLECT batches={} rows_soft_deleted={} \
         cap_reached={} collect_secs={:.3} candidate_scan_secs={:.3} soft_delete_secs={:.3} \
         cursor_at_stop={} spill_delta_bytes={}\n\
         mark_scan_bench[collect_phase]: COMBINED mark+prepare+collect_secs={:.3}\n\
         mark_scan_bench[collect_phase]: expand_plan={}\n\
         mark_scan_bench[collect_phase]: collect_plan={}",
        seeded.entries_seeded,
        seeded.blob_bytes,
        seeded.manifest_data_bytes,
        seeded.seed_secs,
        chunks_fixture.live_rows,
        chunks_fixture.unref_rows,
        chunks_fixture.expected_collectable,
        chunks_fixture.grace_protected,
        chunks_fixture.touch_protected,
        chunks_fixture.chunks_table_bytes,
        chunks_fixture.seed_secs,
        stats.mark_validate.as_secs_f64(),
        stats.mark_expand.as_secs_f64(),
        stats.mark_set_size,
        stats.live_table_bytes,
        stats.prepare.as_secs_f64(),
        stats.batches,
        stats.rows_collected,
        stats.cap_reached,
        stats.collect.as_secs_f64(),
        stats.collect_scan.as_secs_f64(),
        stats.collect_delete.as_secs_f64(),
        stats
            .cursor_at_stop
            .as_deref()
            .map(hex::encode)
            .unwrap_or_else(|| "none".to_string()),
        stats.spill_delta_bytes,
        stats.combined.as_secs_f64(),
        stats.expand_plan_summary,
        stats.collect_plan_summary,
    );

    // Mark-side sanity (same as the mark-only formulations).
    assert_eq!(
        stats.mark_set_size, chunks_fixture.live_rows,
        "the mark set and the seeded referenced population are the same set"
    );

    // Collect-side correctness. Uncapped pass (victims fit in the cap):
    // exactly the old, untouched, unreferenced population is
    // soft-deleted. Capped pass (the gate-(c) v4 backlog case): exactly
    // `victim_cap` victims are soft-deleted across exactly
    // ceil(cap / batch) full batches and the cursor is reported. In
    // both cases nothing outside the old-untouched-unreferenced
    // population is ever soft-deleted (asserted structurally below).
    let (deleted_rows, surviving_rows): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*) FILTER (WHERE deleted), COUNT(*) FILTER (WHERE NOT deleted) FROM chunks",
    )
    .fetch_one(&db.pool)
    .await
    .expect("post-collect chunk counts");
    let total_rows = chunks_fixture.live_rows + chunks_fixture.unref_rows;
    if stats.cap_reached {
        assert!(
            (chunks_fixture.expected_collectable as u64) > victim_cap,
            "cap_reached implies the backlog exceeded the cap"
        );
        assert_eq!(
            stats.rows_collected, victim_cap,
            "a capped cycle soft-deletes exactly the per-cycle victim cap"
        );
        assert_eq!(
            stats.batches,
            victim_cap.div_ceil(batch_limit),
            "a capped cycle stops after exactly ceil(cap / batch) batches"
        );
        assert!(
            stats.cursor_at_stop.is_some(),
            "a capped cycle reports the keyset cursor it stopped at"
        );
        assert_eq!(
            deleted_rows as u64, victim_cap,
            "soft-deleted rows match the cap"
        );
        assert_eq!(
            surviving_rows as u64,
            total_rows as u64 - victim_cap,
            "everything past the cap is left for subsequent cycles"
        );
    } else {
        assert_eq!(
            stats.rows_collected, chunks_fixture.expected_collectable as u64,
            "collect soft-deletes exactly the old unreferenced population"
        );
        assert_eq!(
            deleted_rows, chunks_fixture.expected_collectable,
            "soft-deleted rows match the expected victim set"
        );
        assert_eq!(
            surviving_rows,
            chunks_fixture.live_rows
                + chunks_fixture.grace_protected
                + chunks_fixture.touch_protected,
            "referenced, younger-than-grace, and freshly-touched rows all survive"
        );
    }

    // Victim-set soundness, capped or not: a soft-deleted row is never
    // S3-confirmed (the referenced fixture rows all carry uploaded_at),
    // never freshly touched (the 068 column), and never younger than
    // grace — i.e. victims ⊆ the old, untouched, unreferenced seed.
    let bad_victims: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chunks \
          WHERE deleted \
            AND (uploaded_at IS NOT NULL \
                 OR last_referenced_at IS NOT NULL \
                 OR created_at > now() - make_interval(secs => $1))",
    )
    .bind(super::sweep::CHUNK_GRACE_SECS)
    .fetch_one(&db.pool)
    .await
    .expect("victim soundness probe");
    assert_eq!(
        bad_victims, 0,
        "no referenced, grace-protected, or touch-protected row is ever soft-deleted"
    );
    let sample_deleted: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE deleted AND blake3_hash = ANY($1)")
            .bind(sample.iter().map(|h| h.to_vec()).collect::<Vec<Vec<u8>>>())
            .fetch_one(&db.pool)
            .await
            .expect("sample survival probe");
    assert_eq!(
        sample_deleted, 0,
        "no chunk of the sampled known manifest is collected"
    );
}

/// The live arm's soft-delete statement re-evaluates the row-local
/// collect predicate — `deleted = FALSE` AND the
/// `GREATEST(created_at, last_referenced_at) < cutoff` grace term — in
/// its own WHERE clause (the T-1a.8 consequence). A candidate that a
/// concurrent upgrade re-references (touches) between the candidate
/// scan and the soft-delete must survive the soft-delete; an untouched
/// candidate must still be collected (the conjunct filters, it does not
/// neuter the statement). Dropping either conjunct re-opens the §4.6(i)
/// mark-stale data-loss window, so the structural pin fails loudly if
/// the shipped statement regresses to a hash-only or deleted-only
/// WHERE.
#[tokio::test]
async fn collect_batch_update_rechecks_collect_predicate() {
    // Structural pin: the statement the live arm ships must carry both
    // row-local conjuncts in its own WHERE.
    assert!(
        COLLECT_BATCH_UPDATE_SQL.contains("deleted = FALSE")
            && COLLECT_BATCH_UPDATE_SQL
                .contains("GREATEST(created_at, last_referenced_at) < $2::timestamptz"),
        "COLLECT_BATCH_UPDATE_SQL must re-check the collect predicate's row-local conjuncts \
         (deleted = FALSE AND GREATEST(created_at, last_referenced_at) < cutoff) in its own \
         WHERE clause; got: {}",
        COLLECT_BATCH_UPDATE_SQL.as_str()
    );
    // merged_bug_026 extension: every chunks-mutating statement's
    // OUTER qual must carry every row-local eligibility conjunct —
    // the reap DELETE included (its EPQ re-check set).
    let reap_outer = super::collect::REAP_BATCH_DELETE_SQL
        .rsplit_once("LIMIT $2)")
        .expect("reap statement shape")
        .1;
    for conjunct in [
        "chunks.deleted",
        "chunks.deleted_at < now() - make_interval(secs => $1)",
    ] {
        assert!(
            reap_outer.contains(conjunct),
            "REAP_BATCH_DELETE_SQL outer qual lost {conjunct:?}: {reap_outer}"
        );
    }

    let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
    // Two old, unreferenced, past-grace candidates.
    let touched = crate::test_helpers::ChunkSeed::new(0xE1)
        .age_secs(3600)
        .seed(&db.pool)
        .await;
    let untouched = crate::test_helpers::ChunkSeed::new(0xE2)
        .age_secs(3600)
        .seed(&db.pool)
        .await;

    let mut conn = db.pool.acquire().await.expect("acquire");
    let cutoff: String = sqlx::query_scalar("SELECT (now() - make_interval(secs => $1))::text")
        .bind(super::sweep::CHUNK_GRACE_SECS)
        .fetch_one(&mut *conn)
        .await
        .expect("grace cutoff");
    // Empty mark set on this session: both candidates are unmarked.
    sqlx::query("CREATE TEMP TABLE live_chunks (blake3_hash BYTEA PRIMARY KEY)")
        .execute(&mut *conn)
        .await
        .expect("temp live_chunks");

    let candidates: Vec<Vec<u8>> = sqlx::query_scalar(COLLECT_BATCH_SELECT_SQL.as_str())
        .bind(Vec::<u8>::new())
        .bind(&cutoff)
        .bind(10i64)
        .fetch_all(&mut *conn)
        .await
        .expect("candidate scan");
    assert_eq!(candidates.len(), 2, "both seeded chunks are candidates");

    // A concurrent upgrade transaction commits between the candidate
    // scan and the soft-delete: it re-references (touches) one of the
    // candidates — the §4.6(i) interleaving the grace re-check closes.
    sqlx::query("UPDATE chunks SET last_referenced_at = now() WHERE blake3_hash = $1")
        .bind(&touched[..])
        .execute(&db.pool)
        .await
        .expect("concurrent touch");

    let collected: Vec<(Vec<u8>, i64)> = sqlx::query_as(COLLECT_BATCH_UPDATE_SQL.as_str())
        .bind(&candidates)
        .bind(&cutoff)
        .fetch_all(&mut *conn)
        .await
        .expect("soft-delete batch");
    assert_eq!(
        collected.len(),
        1,
        "exactly the untouched candidate is soft-deleted"
    );
    assert_eq!(collected[0].0, untouched.to_vec());

    let (touched_deleted, untouched_deleted): (bool, bool) = sqlx::query_as(
        "SELECT \
           (SELECT deleted FROM chunks WHERE blake3_hash = $1), \
           (SELECT deleted FROM chunks WHERE blake3_hash = $2)",
    )
    .bind(&touched[..])
    .bind(&untouched[..])
    .fetch_one(&db.pool)
    .await
    .expect("post-state probe");
    assert!(
        !touched_deleted,
        "a candidate re-referenced (touched) after the scan must survive the soft-delete"
    );
    assert!(untouched_deleted, "the untouched candidate is collected");
}

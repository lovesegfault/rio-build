//! `xtask measure` — ADR-022 P0543 measurement tooling (V11/V12 +
//! closure size).
//!
//! Three offline measurements that feed the castore-FUSE sizing tables
//! and the `STREAM_THRESHOLD` config default (P0575). **None of these
//! are gates** — the closure-size and NAR-size ceilings were removed
//! from the exit criteria (no device table; `nar_ls` streams
//! unconditionally), and V12 is tuning input for a human-picked
//! default. All three subcommands merge their results into
//! `.stress-test/metrics/v11-v12.json` (gitignored — measurement
//! artifact, not source) so partial runs accumulate.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::Value;
use tracing::{info, warn};

use crate::sh::{self, repo_root};

/// Merged output for all three subcommands, relative to the repo root.
const OUT_REL: &str = ".stress-test/metrics/v11-v12.json";

/// Modeled fetch bandwidth: 1 Gbps = 125 MB/s. The plan's own
/// calibration point for the initial default ("8 MiB ≈ 60-120 ms
/// whole-file at 1 Gbps") uses this figure — it is the sustained
/// per-flow floor we size for, not the instance's burst ceiling.
const BANDWIDTH_BYTES_PER_SEC: f64 = 125_000_000.0;

/// Fixed cost of the streaming-open path before any range is readable:
/// `open()` returns after the first chunk lands. Spike `15a9db79`
/// measured 10.3 ms for a 256 MiB file with a 10 ms/chunk backend
/// (ADR-022 §2.10); the design text rounds it to "~10 ms".
const FIRST_CHUNK_LATENCY_SECS: f64 = 0.010;

/// FastCDC parameters for the V11 chunk-reuse scan. These mirror
/// `rio-store/src/chunker.rs` (`CHUNK_MIN`/`CHUNK_AVG`/`CHUNK_MAX`,
/// the source of truth) — pulling rio-store's full server dep tree
/// into xtask for three constants is not worth it for an offline
/// measurement tool, so unlike rio-builder's mirror (pinned by its
/// `chunker_constants_match_rio_store` test) this one is documented
/// only. If the store's chunking parameters change, re-run V11 with
/// these updated to match or the reuse number describes a chunker
/// that no longer exists.
const CHUNK_MIN: usize = 16 * 1024;
const CHUNK_AVG: usize = 64 * 1024;
const CHUNK_MAX: usize = 256 * 1024;

/// The access-probe traces from `42aa81b2`: page-cache fill TSVs
/// captured by `nix/tests/lib/spike_access_probe.sh`, one per
/// (file, consumer) pair, plus the probed file's size (from
/// `RESULTS.md` — the TSVs only carry offsets). Kept in lockstep with
/// `nix/tests/lib/spike-access-data/RESULTS.md`; `v12()` fails loudly
/// if a listed TSV is missing rather than silently shrinking the
/// sample.
const ACCESS_TRACES: &[(&str, u64)] = &[
    // libLLVM.so.21.1 — link-time, ld.so load, `opt --version`, `opt -O2`.
    ("llvm-link.tsv", 188_217_688),
    ("llvm-ldso.tsv", 188_217_688),
    ("llvm-opt-version.tsv", 188_217_688),
    ("llvm-opt-O2.tsv", 188_217_688),
    // libicudata.so.76.1 — ld.so load.
    ("icu-ldso.tsv", 31_859_656),
    // libv8.a — 1-sym link (head member), 1-sym link (transitive tail),
    // `ar t`.
    ("v8a-ld1sym.tsv", 152_810_402),
    ("v8a-ld1sym-tail.tsv", 152_810_402),
    ("v8a-art.tsv", 152_810_402),
    // libclangTidyBugproneModule.a — `ar t`.
    ("ctidy-art.tsv", 110_406_710),
];

#[derive(clap::Args)]
pub struct MeasureArgs {
    #[command(subcommand)]
    cmd: MeasureCmd,
}

#[derive(clap::Subcommand)]
enum MeasureCmd {
    /// V11: intra-closure chunk-reuse %. Realizes the closure locally,
    /// FastCDC-chunks every NAR with the store's parameters, and
    /// reports how many chunk bytes are duplicates of a chunk seen
    /// earlier in the same closure. Downloads + hashes the full
    /// closure — minutes to hours for `nixpkgs#chromium`.
    V11 {
        /// Flake installable whose runtime closure is scanned.
        #[arg(long, default_value = "nixpkgs#chromium")]
        installable: String,
    },
    /// V12: STREAM_THRESHOLD tuning. Pure local data analysis over the
    /// nix-index `top1000.csv` dataset and the `42aa81b2` access-probe
    /// traces — computes the file size at which whole-file fetch
    /// latency exceeds the p50 first-range-touched latency of the
    /// streaming-open path.
    V12 {
        /// Path to the nix-index `top1000.csv` external dataset (1000
        /// largest files in nixpkgs). Defaults to the location the ADR
        /// records for it: `~/src/nix-index/main/top1000.csv`.
        #[arg(long)]
        top1000: Option<PathBuf>,
    },
    /// Closure path counts: `nix path-info -r nixpkgs#chromium | wc -l`
    /// for x86_64-linux and aarch64-linux. Queries cache.nixos.org
    /// narinfo references — no NAR download. Informational only (the
    /// `< 65535` device-table gate was removed).
    ClosurePaths,
}

pub async fn run(args: MeasureArgs) -> Result<()> {
    let section = match args.cmd {
        MeasureCmd::V11 { installable } => ("v11", v11(&installable).await?),
        MeasureCmd::V12 { top1000 } => ("v12", v12(top1000.as_deref())?),
        MeasureCmd::ClosurePaths => ("closure_paths", closure_paths().await?),
    };
    let path = repo_root().join(OUT_REL);
    merge_into_output(&path, section.0, section.1)?;
    info!("wrote {}", path.display());
    Ok(())
}

/// Read-modify-write the output file: replace one top-level section,
/// keep the others. Running `measure v12` must not clobber a previous
/// `measure v11` result — the file is the union of whichever
/// measurements have been taken so far.
fn merge_into_output(path: &Path, key: &str, value: Value) -> Result<()> {
    fs::create_dir_all(path.parent().expect("output path has a parent"))?;
    let mut doc: serde_json::Map<String, Value> = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("{} exists but is not a JSON object", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(e) => return Err(e).context(format!("read {}", path.display())),
    };
    doc.insert(key.to_owned(), value);
    doc.insert(
        "generated_by".to_owned(),
        Value::String("cargo xtask measure (P0543)".to_owned()),
    );
    fs::write(path, format!("{}\n", serde_json::to_string_pretty(&doc)?))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// V12 — STREAM_THRESHOLD tuning
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct V12Report {
    /// The headline number: the file size at which whole-file fetch
    /// latency exceeds the p50 first-range-touched latency of the
    /// streaming-open path. Files larger than this are served faster
    /// (to first useful byte) by streaming; files smaller are served
    /// at least as fast by a whole-file fetch, which also skips the
    /// chunk-cache machinery.
    v12_stream_threshold_bytes: u64,
    /// The crossover ignoring the streaming path's fixed first-chunk
    /// cost — i.e. the pure-bandwidth answer, p50(first-range bytes).
    /// Reported for transparency: the headline number is dominated by
    /// `first_chunk_latency_secs * bandwidth`, not by this term.
    crossover_bytes_pure_bandwidth: u64,
    /// `STREAM_THRESHOLD`'s initial default from the plan (8 MiB) and
    /// what a whole-file fetch of a file that size costs under the
    /// same bandwidth model. The default is deliberately above the
    /// latency crossover: between the two, whole-file fetch is slower
    /// to first byte but stays within an acceptable open() budget and
    /// avoids running the per-chunk fetch/verify/promote machinery.
    initial_default_bytes: u64,
    initial_default_whole_file_ms: f64,
    /// Model parameters, so the number is reproducible.
    model: V12Model,
    /// Per-trace breakdown of the `42aa81b2` access-probe data.
    access_traces: Vec<TraceStat>,
    /// p50 of `first_range_bytes` over the traces.
    p50_first_range_bytes: u64,
    /// p50 first-range-touched latency under the streaming path.
    p50_first_range_latency_ms: f64,
    /// Size distribution of the nix-index top-1000 dataset. Context
    /// for the threshold: every one of the 1000 largest files in
    /// nixpkgs is far above any plausible threshold, so the threshold
    /// only decides the fate of mid-size files.
    top1000: Top1000Stats,
}

#[derive(Serialize)]
struct V12Model {
    bandwidth_bytes_per_sec: f64,
    first_chunk_latency_secs: f64,
    /// `whole_file_latency(S) = S / bandwidth`;
    /// `first_range_latency(trace) = first_chunk_latency +
    /// first_range_bytes / bandwidth`; the threshold is the S at which
    /// the former exceeds the p50 of the latter.
    formula: &'static str,
}

#[derive(Serialize)]
struct TraceStat {
    trace: String,
    file_size: u64,
    /// Length of the first contiguous byte range the consumer touches
    /// (events coalesced the same way `spike_access_analyze.py` does:
    /// sort by offset, merge adjacent/overlapping). Every probed
    /// consumer starts at offset 0 (ELF header / archive index), so
    /// the spatially-first range is also the temporally-first.
    first_range_bytes: u64,
    /// Total bytes touched and coalesced range count — must agree with
    /// the corresponding RESULTS.md row; a mismatch means the TSV and
    /// the table have drifted.
    touched_bytes: u64,
    ranges: usize,
    /// Latency until this consumer's first touched range is readable
    /// under the streaming path.
    first_range_latency_ms: f64,
    /// Latency until anything is readable under a whole-file fetch.
    whole_file_latency_ms: f64,
}

#[derive(Serialize)]
struct Top1000Stats {
    source: String,
    files: usize,
    min_bytes: u64,
    p50_bytes: u64,
    max_bytes: u64,
    over_64mib: usize,
    over_256mib: usize,
    over_1gib: usize,
    /// Whole-file fetch latency of the median top-1000 file — the
    /// open() stall streaming-open exists to avoid.
    p50_whole_file_latency_ms: f64,
}

fn v12(top1000: Option<&Path>) -> Result<Value> {
    // -- access-probe traces -------------------------------------------------
    let data_dir = repo_root().join("nix/tests/lib/spike-access-data");
    let mut traces = Vec::with_capacity(ACCESS_TRACES.len());
    for &(name, file_size) in ACCESS_TRACES {
        let ranges = coalesce_trace(&data_dir.join(name))?;
        let first = ranges.first().copied().unwrap_or((0, 0));
        ensure!(
            first.0 == 0,
            "{name}: first touched range starts at {} — every probed consumer reads the \
             ELF header / archive index at offset 0 first, so a nonzero start means the \
             trace is truncated or the coalescing is wrong",
            first.0
        );
        let first_range_bytes = first.1 - first.0;
        traces.push(TraceStat {
            trace: name.to_owned(),
            file_size,
            first_range_bytes,
            touched_bytes: ranges.iter().map(|(s, e)| e - s).sum(),
            ranges: ranges.len(),
            first_range_latency_ms: 1e3
                * (FIRST_CHUNK_LATENCY_SECS + first_range_bytes as f64 / BANDWIDTH_BYTES_PER_SEC),
            whole_file_latency_ms: 1e3 * file_size as f64 / BANDWIDTH_BYTES_PER_SEC,
        });
    }
    let first_ranges: Vec<u64> = traces.iter().map(|t| t.first_range_bytes).collect();
    let (p50_first_range_bytes, threshold) = stream_threshold(&first_ranges);
    let p50_first_range_latency_secs =
        FIRST_CHUNK_LATENCY_SECS + p50_first_range_bytes as f64 / BANDWIDTH_BYTES_PER_SEC;

    // -- top1000.csv ---------------------------------------------------------
    let csv_path = match top1000 {
        Some(p) => p.to_path_buf(),
        None => default_top1000_path()?,
    };
    let mut sizes = parse_top1000(&csv_path)
        .with_context(|| format!("ingest nix-index dataset {}", csv_path.display()))?;
    sizes.sort_unstable();
    let p50_top = sizes[sizes.len() / 2];
    let top1000 = Top1000Stats {
        source: csv_path.display().to_string(),
        files: sizes.len(),
        min_bytes: sizes[0],
        p50_bytes: p50_top,
        max_bytes: *sizes.last().expect("parse_top1000 rejects empty"),
        over_64mib: sizes.iter().filter(|&&s| s > 64 << 20).count(),
        over_256mib: sizes.iter().filter(|&&s| s > 256 << 20).count(),
        over_1gib: sizes.iter().filter(|&&s| s > 1 << 30).count(),
        p50_whole_file_latency_ms: 1e3 * p50_top as f64 / BANDWIDTH_BYTES_PER_SEC,
    };

    let initial_default_bytes = 8 << 20;
    let report = V12Report {
        v12_stream_threshold_bytes: threshold,
        crossover_bytes_pure_bandwidth: p50_first_range_bytes,
        initial_default_bytes,
        initial_default_whole_file_ms: 1e3 * initial_default_bytes as f64 / BANDWIDTH_BYTES_PER_SEC,
        model: V12Model {
            bandwidth_bytes_per_sec: BANDWIDTH_BYTES_PER_SEC,
            first_chunk_latency_secs: FIRST_CHUNK_LATENCY_SECS,
            formula: "threshold = (first_chunk_latency + p50(first_range_bytes)/bandwidth) \
                      * bandwidth",
        },
        access_traces: traces,
        p50_first_range_bytes,
        p50_first_range_latency_ms: 1e3 * p50_first_range_latency_secs,
        top1000,
    };

    println!(
        "v12_stream_threshold_bytes = {} ({:.2} MiB)",
        report.v12_stream_threshold_bytes,
        report.v12_stream_threshold_bytes as f64 / (1 << 20) as f64
    );
    println!(
        "  p50 first-range = {} bytes -> {:.2} ms streamed; whole-file crosses that at {} bytes",
        report.p50_first_range_bytes,
        report.p50_first_range_latency_ms,
        report.v12_stream_threshold_bytes
    );
    println!(
        "  initial default 8 MiB = {:.0} ms whole-file at 1 Gbps; top-1000 p50 {} MiB = {:.0} ms",
        report.initial_default_whole_file_ms,
        report.top1000.p50_bytes >> 20,
        report.top1000.p50_whole_file_latency_ms
    );
    Ok(serde_json::to_value(report)?)
}

/// The V12 answer: given the first-touched-range sizes observed by the
/// access probe, return `(p50 first-range bytes, threshold bytes)`.
///
/// The threshold is the file size at which whole-file fetch latency
/// (`S / BW`) exceeds the p50 first-range-touched latency of the
/// streaming path (`first_chunk_latency + first_range / BW`):
///
/// ```text
/// S / BW > C + p50(R) / BW   ⇔   S > C·BW + p50(R)
/// ```
///
/// Below the threshold a whole-file fetch delivers the consumer's
/// first touched range at least as fast as streaming would *and*
/// skips the per-chunk fetch/verify/promote machinery; above it,
/// streaming wins on time-to-first-useful-byte and the win grows
/// linearly with file size.
fn stream_threshold(first_range_bytes: &[u64]) -> (u64, u64) {
    let mut sorted = first_range_bytes.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[sorted.len() / 2];
    let latency = FIRST_CHUNK_LATENCY_SECS + p50 as f64 / BANDWIDTH_BYTES_PER_SEC;
    (p50, (latency * BANDWIDTH_BYTES_PER_SEC).ceil() as u64)
}

/// `~/src/nix-index/main/top1000.csv` — the location ADR-022 records
/// for the external nix-index dataset (it is not vendored into this
/// repo). `--top1000` overrides.
fn default_top1000_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context(
        "HOME is unset and --top1000 was not given; pass --top1000 <path> to the \
         nix-index top1000.csv dataset",
    )?;
    Ok(PathBuf::from(home).join("src/nix-index/main/top1000.csv"))
}

/// Parse the nix-index `top1000.csv` (`attr,path,subpath,size` with
/// quoted string fields). Only the trailing size column is needed;
/// splitting on the last comma sidesteps quoting/embedded-comma rules.
fn parse_top1000(path: &Path) -> Result<Vec<u64>> {
    let text = fs::read_to_string(path).with_context(|| {
        format!(
            "read {} — the nix-index top-1000 dataset is external to this repo; \
             pass --top1000 <path> if it lives elsewhere",
            path.display()
        )
    })?;
    let mut sizes = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if i == 0 && line.starts_with("attr,") {
            continue; // header
        }
        if line.is_empty() {
            continue;
        }
        let (_, size) = line
            .rsplit_once(',')
            .with_context(|| format!("{}:{}: no comma", path.display(), i + 1))?;
        sizes.push(
            size.trim()
                .parse::<u64>()
                .with_context(|| format!("{}:{}: size column `{size}`", path.display(), i + 1))?,
        );
    }
    ensure!(!sizes.is_empty(), "{}: no data rows", path.display());
    Ok(sizes)
}

/// Read a `spike_access_probe.sh` TSV (`ofs<TAB>len` per page-cache
/// folio add) and coalesce into disjoint `[start, end)` ranges — the
/// same sort-then-merge `spike_access_analyze.py` uses, so the range
/// counts here match the RESULTS.md table.
fn coalesce_trace(path: &Path) -> Result<Vec<(u64, u64)>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read trace {}", path.display()))?;
    let mut events = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let (ofs, len) = line
            .split_once('\t')
            .with_context(|| format!("{}:{}: expected `ofs<TAB>len`", path.display(), i + 1))?;
        let ofs: u64 = ofs.trim().parse()?;
        let len: u64 = len.trim().parse()?;
        events.push((ofs, ofs + len));
    }
    ensure!(!events.is_empty(), "{}: empty trace", path.display());
    events.sort_unstable();
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    for (start, end) in events {
        match ranges.last_mut() {
            Some((_, cur_end)) if start <= *cur_end => *cur_end = (*cur_end).max(end),
            _ => ranges.push((start, end)),
        }
    }
    Ok(ranges)
}

// ---------------------------------------------------------------------------
// V11 — intra-closure chunk reuse
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct V11Report {
    installable: String,
    store_paths: usize,
    /// Total NAR bytes chunked (= sum of all chunk lengths).
    nar_bytes: u64,
    total_chunks: usize,
    unique_chunks: usize,
    /// Bytes attributable to the first occurrence of each distinct
    /// chunk — what the CAS actually stores for this closure.
    unique_chunk_bytes: u64,
    /// `1 - unique_chunk_bytes / nar_bytes`: the fraction of NAR bytes
    /// the CAS does NOT store again because an identical chunk already
    /// exists elsewhere in the same closure.
    reuse_pct_by_bytes: f64,
    reuse_pct_by_count: f64,
    chunker: ChunkerParams,
}

#[derive(Serialize)]
struct ChunkerParams {
    min: usize,
    avg: usize,
    max: usize,
    hash: &'static str,
}

async fn v11(installable: &str) -> Result<Value> {
    let sh = sh::shell()?;
    info!("realizing {installable} locally (downloads the full closure)");
    sh::run(sh::cmd!(sh, "nix build --no-link {installable}"))
        .await
        .with_context(|| format!("realize {installable}"))?;
    let paths = sh::run_read(sh::cmd!(sh, "nix path-info -r {installable}")).await?;
    let paths: Vec<&str> = paths.lines().filter(|l| !l.is_empty()).collect();
    ensure!(!paths.is_empty(), "empty closure for {installable}");
    info!("chunking {} store paths", paths.len());

    // Distinct chunk digest -> length. BTreeMap (not HashMap) so the
    // iteration below is deterministic if this ever grows a "dump the
    // duplicate set" mode.
    let mut seen: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    let (mut nar_bytes, mut total_chunks, mut dup_bytes) = (0u64, 0usize, 0u64);
    for (i, path) in paths.iter().enumerate() {
        // Raw std::process::Command (not sh::run_read): `nix store
        // dump-path` writes the binary NAR to stdout, and the sh::*
        // wrappers are String-typed.
        let out = std::process::Command::new("nix")
            .args(["store", "dump-path", path])
            .output()
            .with_context(|| format!("nix store dump-path {path}"))?;
        if !out.status.success() {
            bail!(
                "nix store dump-path {path}: {}: {}",
                out.status,
                str::from_utf8(&out.stderr)
                    .unwrap_or("<non-utf8 stderr>")
                    .trim()
            );
        }
        for chunk in fastcdc::v2020::FastCDC::new(&out.stdout, CHUNK_MIN, CHUNK_AVG, CHUNK_MAX) {
            let data = &out.stdout[chunk.offset..chunk.offset + chunk.length];
            let digest = *blake3::hash(data).as_bytes();
            nar_bytes += data.len() as u64;
            total_chunks += 1;
            if seen.insert(digest, data.len() as u64).is_some() {
                dup_bytes += data.len() as u64;
            }
        }
        if (i + 1) % 100 == 0 {
            info!("  {}/{} paths chunked", i + 1, paths.len());
        }
    }

    let unique_chunk_bytes: u64 = seen.values().sum();
    let report = V11Report {
        installable: installable.to_owned(),
        store_paths: paths.len(),
        nar_bytes,
        total_chunks,
        unique_chunks: seen.len(),
        unique_chunk_bytes,
        reuse_pct_by_bytes: 100.0 * dup_bytes as f64 / nar_bytes.max(1) as f64,
        reuse_pct_by_count: 100.0 * (total_chunks - seen.len()) as f64
            / (total_chunks.max(1)) as f64,
        chunker: ChunkerParams {
            min: CHUNK_MIN,
            avg: CHUNK_AVG,
            max: CHUNK_MAX,
            hash: "blake3",
        },
    };
    println!(
        "v11: {} paths, {} chunks ({} unique) over {} NAR bytes — {:.2}% of bytes are \
         intra-closure duplicates",
        report.store_paths,
        report.total_chunks,
        report.unique_chunks,
        report.nar_bytes,
        report.reuse_pct_by_bytes
    );
    Ok(serde_json::to_value(report)?)
}

// ---------------------------------------------------------------------------
// closure-paths — store path counts for both arches
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ClosurePathsReport {
    /// `nix flake metadata nixpkgs` resolved revision, so the counts
    /// are labeled with what they measured.
    nixpkgs_rev: Option<String>,
    /// Per-arch closure path count, or the failure reason. Both gates
    /// on these numbers were removed from P0543's exit criteria —
    /// informational only.
    x86_64_linux: Value,
    aarch64_linux: Value,
}

async fn closure_paths() -> Result<Value> {
    let sh = sh::shell()?;
    let nixpkgs_rev = sh::try_read(sh::cmd!(
        sh,
        "nix flake metadata nixpkgs --json --no-write-lock-file"
    ))
    .ok()
    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    .and_then(|v| {
        v.pointer("/locked/rev")
            .and_then(|r| r.as_str().map(String::from))
    });

    let mut counts = Vec::with_capacity(2);
    for installable in [
        "nixpkgs#legacyPackages.x86_64-linux.chromium",
        "nixpkgs#legacyPackages.aarch64-linux.chromium",
    ] {
        // `--store https://cache.nixos.org` walks narinfo references on
        // the binary cache instead of requiring a local copy of a
        // multi-GB closure; `--eval-store auto` keeps the .drv writes
        // local (an HTTP binary cache is read-only).
        info!("counting closure of {installable} via cache.nixos.org");
        let count = sh::run_read(sh::cmd!(
            sh,
            "nix path-info -r --eval-store auto --store https://cache.nixos.org {installable}"
        ))
        .await
        .map(|out| Value::from(out.lines().filter(|l| !l.is_empty()).count()));
        counts.push(match count {
            Ok(n) => {
                println!("closure_paths {installable} = {n}");
                n
            }
            Err(e) => {
                // Not measured ≠ failed run: the count is informational
                // (its gate was removed) and chromium may simply not be
                // cached for the registry's current nixpkgs revision.
                warn!("{installable}: {e:#}");
                serde_json::json!({ "not_yet_measured": format!("{e:#}") })
            }
        });
    }
    let report = ClosurePathsReport {
        nixpkgs_rev,
        aarch64_linux: counts.pop().expect("two installables pushed above"),
        x86_64_linux: counts.pop().expect("two installables pushed above"),
    };
    Ok(serde_json::to_value(report)?)
}

// All tests use synthetic inputs in tempdirs — the nextest-xtask check
// runs in a sandbox where `repo_root()` resolves to the (nonexistent)
// build path and `nix/tests/lib/spike-access-data/` is not staged, so
// nothing here may read committed repo files. The binding to the real
// committed traces is exercised by running `cargo xtask measure v12`
// from a checkout.
#[cfg(test)]
mod tests {
    use super::*;

    /// Coalescing must match `spike_access_analyze.py`'s sort-then-merge:
    /// adjacent and overlapping folio adds fuse, out-of-order events are
    /// handled, and a gap of even one byte starts a new range.
    #[test]
    fn coalesce_merges_adjacent_and_overlapping() {
        let dir = tempfile::tempdir().unwrap();
        let tsv = dir.path().join("t.tsv");
        // Events deliberately out of order; 0..4096, 4096..8192 are
        // adjacent; 6000..10000 overlaps; 16384..32768 is disjoint.
        std::fs::write(&tsv, "4096\t4096\n0\t4096\n6000\t4000\n16384\t16384\n").unwrap();
        assert_eq!(
            coalesce_trace(&tsv).unwrap(),
            vec![(0, 10_000), (16_384, 32_768)]
        );
    }

    /// The threshold formula, pinned at the value the committed
    /// `42aa81b2` traces produce (p50 first-range = the 16 KiB ELF
    /// header probe — 5 of the 9 traces are mmap'd `.so` loads):
    /// `(0.010 + 16384/125e6) * 125e6 = 1_266_384`. A change to the
    /// bandwidth or first-chunk-latency model constants shows up here
    /// as a diff, not as a silently different tuning recommendation.
    #[test]
    fn stream_threshold_formula_is_pinned() {
        // Median of an odd-length set is the middle element.
        let (p50, threshold) = stream_threshold(&[10_223_616, 16_384, 16_384]);
        assert_eq!(p50, 16_384);
        assert_eq!(threshold, 1_266_384);
        // The threshold floor is first_chunk_latency * bandwidth even
        // for a degenerate 0-byte first range — a file that whole-file
        // fetches faster than the streaming path's fixed cost should
        // never stream.
        assert_eq!(stream_threshold(&[0]), (0, 1_250_000));
    }

    /// top1000.csv ingestion: header skipped, embedded commas in the
    /// quoted subpath column don't break the size parse, and the
    /// distribution buckets land where they should.
    #[test]
    fn parse_top1000_handles_quoted_commas() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("top1000.csv");
        std::fs::write(
            &csv,
            "attr,path,subpath,size\n\
             \"a\",\"/nix/store/x\",\"/lib/a.so\",123000000\n\
             \"b\",\"/nix/store/y\",\"/lib/b, with comma.a\",300000000\n\
             \"c\",\"/nix/store/z\",\"/lib/c.a\",2000000000\n",
        )
        .unwrap();
        let sizes = parse_top1000(&csv).unwrap();
        assert_eq!(sizes, vec![123_000_000, 300_000_000, 2_000_000_000]);
    }

    /// `merge_into_output` is read-modify-write: a v12 run must not
    /// clobber an earlier v11 section, and a fresh run creates the
    /// parent directory.
    #[test]
    fn merge_preserves_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metrics/out.json");
        merge_into_output(&path, "v11", serde_json::json!({"store_paths": 3})).unwrap();
        merge_into_output(&path, "v12", serde_json::json!({"x": 1})).unwrap();
        let back: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back["v11"]["store_paths"], 3);
        assert_eq!(back["v12"]["x"], 1);
    }
}

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use hdrhistogram::Histogram;
use walkdir::WalkDir;

/// Benchmark results for a single concurrency level.
#[derive(Debug, serde::Serialize)]
pub struct BenchResult {
    pub concurrency: usize,
    pub direct_p50_us: u64,
    pub direct_p99_us: u64,
    pub direct_max_us: u64,
    pub fuse_p50_us: u64,
    pub fuse_p99_us: u64,
    pub fuse_max_us: u64,
    pub ratio_p50: f64,
    pub ratio_p99: f64,
    pub file_count: usize,
    pub total_bytes: u64,
}

/// Collect all regular files under a directory.
fn collect_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut skipped = 0u64;

    for entry in WalkDir::new(dir) {
        match entry {
            Ok(e) => {
                if e.file_type().is_file() {
                    files.push(e.into_path());
                }
            }
            Err(e) => {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "skipping unreadable entry during file collection"
                );
                skipped += 1;
            }
        }
    }

    if skipped > 0 {
        tracing::warn!(
            dir = %dir.display(),
            skipped,
            "some directory entries were unreadable during file collection"
        );
    }

    files
}

/// Read all files sequentially, recording per-read latency into a histogram.
fn read_files_sequential(files: &[std::path::PathBuf]) -> anyhow::Result<(Histogram<u64>, u64)> {
    let mut hist = Histogram::<u64>::new(3)?;
    let mut total_bytes = 0u64;

    for path in files {
        let start = Instant::now();
        let data = fs::read(path)?;
        let elapsed = start.elapsed();
        total_bytes += data.len() as u64;
        hist.record(elapsed.as_micros() as u64)?;
    }

    Ok((hist, total_bytes))
}

/// Read all files with `concurrency` threads, recording per-read latency.
fn read_files_concurrent(
    files: &[std::path::PathBuf],
    concurrency: usize,
) -> anyhow::Result<(Histogram<u64>, u64)> {
    let files = Arc::new(files.to_vec());
    let chunk_size = files.len().div_ceil(concurrency);

    let handles: Vec<_> = (0..concurrency)
        .map(|i| {
            let files = Arc::clone(&files);
            std::thread::spawn(move || -> anyhow::Result<(Histogram<u64>, u64)> {
                let mut hist = Histogram::<u64>::new(3)?;
                let mut total_bytes = 0u64;
                let start_idx = i * chunk_size;
                let end_idx = (start_idx + chunk_size).min(files.len());

                for path in &files[start_idx..end_idx] {
                    let start = Instant::now();
                    let data = fs::read(path)?;
                    let elapsed = start.elapsed();
                    total_bytes += data.len() as u64;
                    hist.record(elapsed.as_micros() as u64)?;
                }

                Ok((hist, total_bytes))
            })
        })
        .collect();

    let mut combined_hist = Histogram::<u64>::new(3)?;
    let mut total_bytes = 0u64;

    for handle in handles {
        let (hist, bytes) = handle.join().map_err(|panic_payload| {
            let msg = panic_payload
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| panic_payload.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            anyhow::anyhow!("thread panicked: {msg}")
        })??;
        combined_hist.add(&hist)?;
        total_bytes += bytes;
    }

    Ok((combined_hist, total_bytes))
}

/// Run the FUSE vs direct read benchmark at a given concurrency level.
fn bench_at_concurrency(
    direct_files: &[std::path::PathBuf],
    fuse_files: &[std::path::PathBuf],
    concurrency: usize,
) -> anyhow::Result<BenchResult> {
    let (direct_hist, total_bytes) = if concurrency == 1 {
        read_files_sequential(direct_files)?
    } else {
        read_files_concurrent(direct_files, concurrency)?
    };

    let (fuse_hist, _) = if concurrency == 1 {
        read_files_sequential(fuse_files)?
    } else {
        read_files_concurrent(fuse_files, concurrency)?
    };

    let direct_p50 = direct_hist.value_at_quantile(0.50);
    let direct_p99 = direct_hist.value_at_quantile(0.99);
    let direct_max = direct_hist.max();

    let fuse_p50 = fuse_hist.value_at_quantile(0.50);
    let fuse_p99 = fuse_hist.value_at_quantile(0.99);
    let fuse_max = fuse_hist.max();

    let ratio_p50 = if direct_p50 > 0 {
        fuse_p50 as f64 / direct_p50 as f64
    } else {
        0.0
    };
    let ratio_p99 = if direct_p99 > 0 {
        fuse_p99 as f64 / direct_p99 as f64
    } else {
        0.0
    };

    Ok(BenchResult {
        concurrency,
        direct_p50_us: direct_p50,
        direct_p99_us: direct_p99,
        direct_max_us: direct_max,
        fuse_p50_us: fuse_p50,
        fuse_p99_us: fuse_p99,
        fuse_max_us: fuse_max,
        ratio_p50,
        ratio_p99,
        file_count: direct_files.len(),
        total_bytes,
    })
}

/// Print benchmark results as a formatted table.
fn print_results(results: &[BenchResult]) {
    println!(
        "\n{:<12} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>8} {:>8}",
        "Concurrency",
        "Dir p50",
        "Dir p99",
        "Dir max",
        "FUSE p50",
        "FUSE p99",
        "FUSE max",
        "p50 x",
        "p99 x"
    );
    println!("{}", "-".repeat(100));

    for r in results {
        println!(
            "{:<12} {:>9}us {:>9}us {:>9}us {:>9}us {:>9}us {:>9}us {:>7.1}x {:>7.1}x",
            r.concurrency,
            r.direct_p50_us,
            r.direct_p99_us,
            r.direct_max_us,
            r.fuse_p50_us,
            r.fuse_p99_us,
            r.fuse_max_us,
            r.ratio_p50,
            r.ratio_p99,
        );
    }

    println!();

    // Go/no-go assessment for raw read-path overhead only. The overall
    // architecture decision (see docs/src/phases/phase1a.md) considers
    // mitigations for lookup/open overhead that this benchmark does not measure.
    if let Some(worst_p99) = results.iter().map(|r| r.ratio_p99).reduce(f64::max) {
        if worst_p99 <= 2.0 {
            println!("PASS: FUSE overhead within target (< 2x direct reads)");
        } else if worst_p99 <= 5.0 {
            println!(
                "WARNING: FUSE overhead exceeds target ({:.1}x) but within fail gate (5x)",
                worst_p99
            );
        } else {
            println!(
                "FAIL: FUSE overhead exceeds fail gate ({:.1}x > 5x) — activate fallback plan",
                worst_p99
            );
        }
    }
}

/// CLI entry point for the benchmark subcommand.
pub fn run_benchmark(
    backing_dir: &Path,
    mount_point: &Path,
    max_concurrency: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        backing_dir.is_dir(),
        "backing directory does not exist: {}",
        backing_dir.display()
    );
    anyhow::ensure!(
        mount_point.is_dir(),
        "mount point does not exist (is FUSE mounted?): {}",
        mount_point.display()
    );

    let direct_files = collect_files(backing_dir);
    anyhow::ensure!(
        !direct_files.is_empty(),
        "no files found in backing directory"
    );

    // Build corresponding FUSE paths by replacing the backing_dir prefix with mount_point
    let fuse_files: Vec<PathBuf> = direct_files
        .iter()
        .map(|p| {
            let rel = p.strip_prefix(backing_dir).with_context(|| {
                format!(
                    "file {} is not under {}",
                    p.display(),
                    backing_dir.display()
                )
            })?;
            Ok(mount_point.join(rel))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    tracing::info!(
        file_count = direct_files.len(),
        max_concurrency,
        "starting FUSE read latency benchmark"
    );

    let mut concurrency_levels: Vec<usize> =
        (0..=max_concurrency.ilog2()).map(|i| 1 << i).collect();
    if *concurrency_levels.last().unwrap() != max_concurrency {
        concurrency_levels.push(max_concurrency);
    }

    let mut results = Vec::new();

    for &c in &concurrency_levels {
        tracing::info!(concurrency = c, "benchmarking");
        let result = bench_at_concurrency(&direct_files, &fuse_files, c)?;
        results.push(result);
    }

    print_results(&results);

    // Write JSON results
    let json = serde_json::to_string_pretty(&results)?;
    println!("\n--- JSON results ---\n{json}");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collect_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::write(dir.path().join("b.txt"), "world").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/c.txt"), "nested").unwrap();

        let files = collect_files(dir.path());
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn test_read_files_sequential() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            fs::write(dir.path().join(format!("{i}.txt")), format!("data-{i}")).unwrap();
        }

        let files = collect_files(dir.path());
        let (hist, total_bytes) = read_files_sequential(&files).unwrap();

        assert_eq!(hist.len(), 10);
        assert!(total_bytes > 0);
    }

    #[test]
    fn test_read_files_concurrent() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            fs::write(dir.path().join(format!("{i}.txt")), format!("data-{i}")).unwrap();
        }

        let files = collect_files(dir.path());
        let (hist, total_bytes) = read_files_concurrent(&files, 4).unwrap();

        assert_eq!(hist.len(), 20);
        assert!(total_bytes > 0);
    }
}

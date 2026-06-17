//! Read-path checks: byte integrity on the cold (streaming) and warm
//! (passthrough) paths, ranged reads, EOF behavior, and concurrency.

use std::fs;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::time::Duration;

use anyhow::{Context, ensure};

use super::{Ctx, Outcome, first_divergence, wait_for};

/// generic/075 + generic/091 (read-only adaptation of fsx): bytes read
/// through the mount must equal the locally regenerated oracle on both
/// the cold path (first open → ReadBlob / streaming fill) and the warm
/// path (digest promoted to the shared cache → passthrough), at odd
/// offsets, and at/past EOF. Guards `Opener::open`,
/// `fetch_and_promote`, `FillState::read_at`, and the passthrough fd
/// end to end — the off-by-one classes fsx hunts live in the streaming
/// window math.
///
/// Must run before any other check reads the big blob, otherwise the
/// "cold" leg silently tests the warm path twice.
pub fn generic_075_091_read_integrity(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let big = &ctx.manifest.big_file;
    let big_path = ctx.on_mount(&big.path);
    let oracle = ctx.manifest.oracle_bytes();
    let oracle_b3 = blake3::hash(&oracle);

    ensure!(
        fs::symlink_metadata(&big_path)?.len() == big.size,
        "{} size mismatch before reading",
        big.path
    );

    // Cold read: the first open of this digest — the streaming-fill
    // path (the blob is larger than the harness's stream threshold).
    let cold = fs::read(&big_path).context("cold read of the big blob")?;
    ensure!(
        cold.len() as u64 == big.size,
        "cold read returned {} bytes, expected {}",
        cold.len(),
        big.size
    );
    ensure!(
        blake3::hash(&cold) == oracle_b3,
        "big blob bytes corrupted on the cold (streaming) read; first divergence at offset {:?}",
        first_divergence(&oracle, &cold)
    );

    // The fill promotes into the shared node cache, keyed by the file
    // digest. Wait for it so the re-read below is the warm passthrough
    // path, not a second streaming fill.
    if let Some(cache_dir) = &ctx.cache_dir {
        let hex = oracle_b3.to_hex();
        let cache_file = cache_dir.join(&hex.as_str()[..2]).join(hex.as_str());
        wait_for(
            "big blob digest to appear in the shared cache",
            Duration::from_secs(120),
            || cache_file.exists(),
        )?;
        let warm = fs::read(&big_path).context("warm read of the big blob")?;
        ensure!(
            blake3::hash(&warm) == oracle_b3,
            "big blob bytes corrupted on the warm (passthrough) read; first divergence at offset {:?}",
            first_divergence(&oracle, &warm)
        );
    }

    // Whole-file reads of the explicit small files (below the stream
    // threshold → the whole-file ReadBlob path).
    for f in &ctx.manifest.files {
        let body = fs::read(ctx.on_mount(&f.path))?;
        ensure!(
            body == f.content.as_bytes(),
            "{} content mismatch through the mount",
            f.path
        );
    }

    // Odd-offset ranged reads (pread) against the oracle: start, an
    // unaligned middle window crossing page boundaries, and a window
    // ending exactly at EOF.
    let file = fs::File::open(&big_path)?;
    let size = big.size;
    for (skip, count) in [(0, 17), (4093, 8200), (size - 13, 13)] {
        let mut buf = vec![0u8; count as usize];
        file.read_exact_at(&mut buf, skip)
            .with_context(|| format!("pread skip={skip} count={count}"))?;
        ensure!(
            buf == oracle[skip as usize..(skip + count) as usize],
            "ranged read skip={skip} count={count} differs from the oracle"
        );
    }

    // EOF behavior: a read starting at EOF returns 0 bytes; a read
    // straddling EOF returns exactly the bytes that exist.
    let mut buf = [0u8; 64];
    let at_eof = file.read_at(&mut buf, size)?;
    ensure!(
        at_eof == 0,
        "read past EOF returned {at_eof} bytes, expected 0"
    );
    let straddling = read_fully_at(&file, &mut buf, size - 3)?;
    ensure!(
        straddling == 3,
        "short read at EOF returned {straddling} bytes, expected 3"
    );
    ensure!(
        buf[..3] == oracle[oracle.len() - 3..],
        "short read at EOF returned wrong bytes"
    );
    Ok(Outcome::Pass)
}

/// generic/095 + generic/310 + generic/113 (read-only adaptation):
/// concurrent whole-file readers agree with the oracle, repeated
/// open/close cycles of one file keep working (the per-digest
/// passthrough backing id must be reused, not re-registered — the
/// kernel EBUSYs a re-registration), and readdir racing read on the
/// same directory neither errors nor wedges. Only generic/113's sync
/// open/close cycling is ported here; its io_uring/AIO readers are the
/// deferred P3 extension and are NOT exercised. Guards the per-digest
/// singleflight (`fills`), shared `FillState` joins, and `Opener`'s
/// concurrent map updates under fuser's thread pool.
pub fn generic_095_113_310_concurrency(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let big_path = ctx.on_mount(&ctx.manifest.big_file.path);
    let oracle_b3 = blake3::hash(&ctx.manifest.oracle_bytes());

    // 8 parallel whole-file readers (generic/095).
    std::thread::scope(|s| -> anyhow::Result<()> {
        let mut handles = Vec::new();
        for i in 0..8 {
            let big_path = &big_path;
            handles.push((
                i,
                s.spawn(move || -> anyhow::Result<blake3::Hash> {
                    Ok(blake3::hash(&fs::read(big_path)?))
                }),
            ));
        }
        for (i, h) in handles {
            let hash = h
                .join()
                .map_err(|_| anyhow::anyhow!("reader thread {i} panicked"))??;
            ensure!(
                hash == oracle_b3,
                "concurrent reader {i} saw different bytes than the oracle"
            );
        }
        Ok(())
    })?;

    // Repeated open/close cycles of one (already promoted) file
    // (generic/113, sync leg).
    let exec = ctx
        .manifest
        .files
        .iter()
        .find(|f| f.executable)
        .context("manifest has no executable file")?;
    let exec_path = ctx.on_mount(&exec.path);
    for cycle in 0..30 {
        let mut body = Vec::new();
        fs::File::open(&exec_path)
            .with_context(|| format!("open/close cycle {cycle}"))?
            .read_to_end(&mut body)?;
        ensure!(
            body == exec.content.as_bytes(),
            "open/close cycle {cycle} read wrong bytes"
        );
    }

    // readdir vs read racing on the same directory (generic/310).
    let dir = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let probe_file = dir.join("f7");
    std::thread::scope(|s| -> anyhow::Result<()> {
        let lister = s.spawn(|| -> anyhow::Result<()> {
            for _ in 0..40 {
                let n = fs::read_dir(&dir)?.count();
                ensure!(
                    n == ctx.manifest.seq_dir.count as usize,
                    "readdir under read contention saw {n} entries"
                );
            }
            Ok(())
        });
        let reader = s.spawn(|| -> anyhow::Result<()> {
            for _ in 0..40 {
                let body = fs::read_to_string(&probe_file)?;
                ensure!(body == "7\n", "read under readdir contention got {body:?}");
            }
            Ok(())
        });
        lister
            .join()
            .map_err(|_| anyhow::anyhow!("lister thread panicked"))??;
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("reader thread panicked"))??;
        Ok(())
    })?;
    Ok(Outcome::Pass)
}

// ─── helpers ───────────────────────────────────────────────────────────

/// pread until EOF or the buffer is full; returns the byte count.
fn read_fully_at(file: &fs::File, buf: &mut [u8], mut offset: u64) -> anyhow::Result<usize> {
    let mut total = 0;
    loop {
        let n = file.read_at(&mut buf[total..], offset)?;
        if n == 0 || total + n == buf.len() {
            return Ok(total + n);
        }
        total += n;
        offset += n as u64;
    }
}

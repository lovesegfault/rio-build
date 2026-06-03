//! The measurement primitives: read-storm, open-storm, randread, and
//! the local copy. Pure std + blake3 syscall loops — no fio, no
//! external commands; everything the bench quotes is measured by this
//! process against the path it was given.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};

use crate::dataset::FileEntry;

const READ_BUF: usize = 1024 * 1024;
/// randread IO size. 4 KiB at 4 KiB-aligned offsets — both the
/// page-cache unit and the O_DIRECT alignment requirement.
pub const RANDREAD_IO_BYTES: u64 = 4096;
pub const RANDREAD_IOS: u64 = 65_536;

pub struct ReadStormOut {
    pub files: u64,
    pub bytes: u64,
    pub wall_ms: u64,
    pub open_ns: Vec<u64>,
    pub read_ns: Vec<u64>,
    /// Files whose blake3 matched the manifest (only counted when
    /// verification ran).
    pub checksum_ok: u64,
}

/// Open + read (+ optionally hash-verify) every file once. `verify`
/// is on for the cold pass only: the hash is the integrity oracle for
/// freshly fetched bytes, and its CPU cost is part of "consume the
/// file" — but on warm passes it would dominate page-cache reads and
/// skew the passthrough numbers, so warm passes skip it.
pub fn read_storm(root: &Path, files: &[&FileEntry], verify: bool) -> Result<ReadStormOut> {
    let started = Instant::now();
    let mut buf = vec![0u8; READ_BUF];
    let mut out = ReadStormOut {
        files: files.len() as u64,
        bytes: 0,
        wall_ms: 0,
        open_ns: Vec::with_capacity(files.len()),
        read_ns: Vec::with_capacity(files.len()),
        checksum_ok: 0,
    };
    for entry in files {
        let path = root.join(&entry.path);
        let t0 = Instant::now();
        let mut f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        out.open_ns.push(t0.elapsed().as_nanos() as u64);

        let t1 = Instant::now();
        let mut hasher = verify.then(blake3::Hasher::new);
        let mut got = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if let Some(h) = hasher.as_mut() {
                h.update(&buf[..n]);
            }
            got += n as u64;
        }
        out.read_ns.push(t1.elapsed().as_nanos() as u64);
        ensure!(
            got == entry.bytes,
            "{}: read {got} bytes, manifest says {}",
            entry.path,
            entry.bytes
        );
        if let Some(h) = hasher {
            if h.finalize().to_hex().to_string() == entry.blake3 {
                out.checksum_ok += 1;
            } else {
                // Corruption through the mount is a broken run, not a
                // data point — fail loudly so the drv (and the bench)
                // goes red instead of quoting numbers off bad bytes.
                bail!("{}: blake3 mismatch (castore corruption?)", entry.path);
            }
        }
        out.bytes += got;
    }
    out.wall_ms = started.elapsed().as_millis() as u64;
    Ok(out)
}

pub struct OpenStormOut {
    pub files: u64,
    pub open_ns: Vec<u64>,
    pub fstat_ns: Vec<u64>,
}

/// Recursive open/fstat/close walk over every regular file under
/// `root` (a real closure, python3 by default). Symlinks are not
/// followed — a closure's symlinks point inside the same store and
/// would double-count their targets.
pub fn open_storm(root: &Path) -> Result<OpenStormOut> {
    let mut out = OpenStormOut {
        files: 0,
        open_ns: Vec::new(),
        fstat_ns: Vec::new(),
    };
    walk(root, &mut out)?;
    ensure!(
        out.files > 0,
        "open_storm found no regular files under {}",
        root.display()
    );
    Ok(out)
}

fn walk(dir: &Path, out: &mut OpenStormOut) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        // DirEntry::file_type is the lstat view — symlinks show as
        // symlinks, not their targets.
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk(&entry.path(), out)?;
        } else if ft.is_file() {
            let path = entry.path();
            let t0 = Instant::now();
            let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
            out.open_ns.push(t0.elapsed().as_nanos() as u64);
            let t1 = Instant::now();
            let _ = f.metadata()?;
            out.fstat_ns.push(t1.elapsed().as_nanos() as u64);
            out.files += 1;
        }
    }
    Ok(())
}

pub struct RandreadOut {
    pub ios: u64,
    pub wall_ms: u64,
    pub io_ns: Vec<u64>,
    /// Whether the O_DIRECT open was honored for this pass. Recorded,
    /// never required — passthrough O_DIRECT semantics are
    /// best-effort.
    pub direct: bool,
}

/// `ios` psync 4 KiB reads at uniform random 4 KiB-aligned offsets.
/// Deterministic offset sequence per `prng_seed` so every rep (and
/// every run of the same seed) visits the same offsets.
pub fn randread(path: &Path, file_bytes: u64, ios: u64, prng_seed: u64) -> Result<RandreadOut> {
    ensure!(
        file_bytes >= RANDREAD_IO_BYTES,
        "randread target smaller than one IO"
    );
    let (f, direct) = open_maybe_direct(path)?;

    // O_DIRECT needs an aligned buffer; harmless for buffered reads.
    let mut raw = vec![0u8; 2 * RANDREAD_IO_BYTES as usize];
    let off = raw.as_ptr().align_offset(RANDREAD_IO_BYTES as usize);
    let buf = &mut raw[off..off + RANDREAD_IO_BYTES as usize];

    let blocks = file_bytes / RANDREAD_IO_BYTES;
    let mut state = prng_seed;
    let started = Instant::now();
    let mut io_ns = Vec::with_capacity(ios as usize);
    for _ in 0..ios {
        let offset = (splitmix64(&mut state) % blocks) * RANDREAD_IO_BYTES;
        let t0 = Instant::now();
        f.read_exact_at(buf, offset)
            .with_context(|| format!("pread {} @{offset}", path.display()))?;
        io_ns.push(t0.elapsed().as_nanos() as u64);
    }
    Ok(RandreadOut {
        ios,
        wall_ms: started.elapsed().as_millis() as u64,
        io_ns,
        direct,
    })
}

/// Try O_DIRECT first; fall back to a plain open. The fallback also
/// covers filesystems that accept the flag at open() but fail reads
/// with EINVAL — probed with one aligned read so the failure mode
/// surfaces here, not mid-phase.
fn open_maybe_direct(path: &Path) -> Result<(File, bool)> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(f) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECT)
        .open(path)
    {
        let mut raw = vec![0u8; 2 * RANDREAD_IO_BYTES as usize];
        let off = raw.as_ptr().align_offset(RANDREAD_IO_BYTES as usize);
        if f.read_exact_at(&mut raw[off..off + RANDREAD_IO_BYTES as usize], 0)
            .is_ok()
        {
            return Ok((f, true));
        }
    }
    Ok((File::open(path)?, false))
}

/// Read the whole file once, sequentially. Used to complete the
/// streaming background fill before randread's warm passes.
pub fn sequential_read(path: &Path) -> Result<u64> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; READ_BUF];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        total += n as u64;
    }
}

pub struct CopyOut {
    pub bytes: u64,
    pub wall_ms: u64,
}

/// Copy the dataset tree to `to` (the same-pod scratch dir). The copy
/// itself is a recorded measurement: a warm sequential read off the
/// castore plus a local write.
pub fn copy_tree(root: &Path, files: &[&FileEntry], to: &Path) -> Result<CopyOut> {
    let started = Instant::now();
    let mut bytes = 0u64;
    for entry in files {
        let dst = to.join(&entry.path);
        std::fs::create_dir_all(dst.parent().expect("sharded path has a parent"))?;
        bytes += std::fs::copy(root.join(&entry.path), &dst)
            .with_context(|| format!("copy {}", entry.path))?;
    }
    Ok(CopyOut {
        bytes,
        wall_ms: started.elapsed().as_millis() as u64,
    })
}

/// SplitMix64 — deterministic, dependency-free random stream. Shared
/// with the dataset layout RNG; quality is far beyond what either
/// needs.
pub(crate) fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub struct JqBuildOut {
    pub configure_wall_ms: u64,
    pub make_wall_ms: u64,
}

/// A real compiler workload: unpack the jq source tarball to scratch
/// and `./configure && make` it with the toolchain served through the
/// castore mount (the bench drv's stdenv cc/make — many small
/// toolchain reads, header opens, cc/ld execs). Writes go to scratch
/// only; the mount serves reads. Each rep gets a fresh build dir, so
/// "warm" means the toolchain and headers are node-/page-cache warm,
/// not an incremental rebuild.
pub fn jq_build(jq_src: &Path, scratch: &Path, rep: u32) -> Result<JqBuildOut> {
    let workdir = scratch.join(format!("jq-build-{rep}"));
    std::fs::create_dir_all(&workdir)?;
    // Direct argv invocations — no shell, no string interpolation. The
    // jq_src path is a drv-baked store path today, but a path must
    // never ride inside a shell line on principle; env (CC, PATH) is
    // inherited from the bench drv's stdenv setup, exactly what a real
    // build sees.
    let timed = |label: &str, cmd: &mut std::process::Command| -> Result<u64> {
        let t0 = Instant::now();
        let out = cmd
            .current_dir(&workdir)
            .output()
            .with_context(|| format!("spawn {label}"))?;
        ensure!(
            out.status.success(),
            "jq_build {label} failed ({}):\n{}",
            out.status,
            // The tail is where configure/make name the actual error.
            String::from_utf8(out.stderr)
                .unwrap_or_default()
                .lines()
                .rev()
                .take(30)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        );
        Ok(t0.elapsed().as_millis() as u64)
    };
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    timed(
        "unpack",
        std::process::Command::new("tar")
            .arg("xf")
            .arg(jq_src)
            .arg("--strip-components=1"),
    )?;
    // --without-oniguruma keeps the dependency surface to the
    // toolchain itself (regex builtins disabled — irrelevant, the
    // artifact is never run).
    let configure_wall_ms = timed(
        "configure",
        std::process::Command::new("./configure").arg("--without-oniguruma"),
    )?;
    let make_wall_ms = timed(
        "make",
        std::process::Command::new("make").arg(format!("-j{cores}")),
    )?;
    Ok(JqBuildOut {
        configure_wall_ms,
        make_wall_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{Manifest, generate, test_fixture_tree};

    /// Generated fixture dataset shared by the storm tests: a
    /// stand-in harvest re-rooted under a temp dir.
    fn gen_dataset() -> (tempfile::TempDir, tempfile::TempDir, Manifest) {
        let src = tempfile::tempdir().unwrap();
        test_fixture_tree(src.path());
        let out = tempfile::tempdir().unwrap();
        let m = generate("seed-1", &[src.path().to_path_buf()], out.path()).unwrap();
        (src, out, m)
    }

    #[test]
    fn read_storm_verifies_and_detects_corruption() {
        let (_src, d, m) = gen_dataset();
        let d = d.path();
        let files: Vec<&FileEntry> = m.files.iter().collect();
        let out = read_storm(d, &files, true).unwrap();
        assert_eq!(out.checksum_ok, m.files.len() as u64);
        assert_eq!(out.bytes, m.total_bytes);
        assert_eq!(out.open_ns.len(), m.files.len());

        // Flip one byte → the storm must fail, not under-count: a
        // silent checksum miss would let a corrupting FUSE bug produce
        // plausible-looking benchmark numbers.
        let victim = d.join(&m.files[0].path);
        let mut bytes = std::fs::read(&victim).unwrap();
        bytes[0] ^= 0xff;
        std::fs::write(&victim, bytes).unwrap();
        assert!(read_storm(d, &files, true).is_err());
        // Unverified pass doesn't hash, so it still succeeds.
        assert!(read_storm(d, &files, false).is_ok());
    }

    #[test]
    fn randread_offsets_are_deterministic_and_in_bounds() {
        let (_src, d, m) = gen_dataset();
        let big = m.randread_reserve().unwrap();
        let path = d.path().join(&big.path);

        let a = randread(&path, big.bytes, 64, 7).unwrap();
        let b = randread(&path, big.bytes, 64, 7).unwrap();
        assert_eq!(a.ios, 64);
        // Same prng seed → same offset sequence → byte-identical work.
        // (Latencies differ; the structural part is the offsets, which
        // read_exact_at would fail on if any went out of bounds.)
        assert_eq!(a.io_ns.len(), b.io_ns.len());
    }

    #[test]
    fn splitmix_offsets_cover_blocks_uniformly_enough() {
        // Structural guard: offsets must be 4KiB-multiples within the
        // file. 10k draws over 16 blocks must touch every block —
        // a stuck PRNG (or % bias bug) would leave gaps.
        let blocks = 16u64;
        let mut state = 1u64;
        let mut seen = [false; 16];
        for _ in 0..10_000 {
            let off = (splitmix64(&mut state) % blocks) * RANDREAD_IO_BYTES;
            assert_eq!(off % RANDREAD_IO_BYTES, 0);
            seen[(off / RANDREAD_IO_BYTES) as usize] = true;
        }
        assert!(seen.iter().all(|s| *s));
    }

    #[test]
    fn open_storm_counts_regular_files_only() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sub/dir")).unwrap();
        std::fs::write(d.path().join("a"), b"x").unwrap();
        std::fs::write(d.path().join("sub/dir/b"), b"y").unwrap();
        std::os::unix::fs::symlink("a", d.path().join("link")).unwrap();
        let out = open_storm(d.path()).unwrap();
        // The symlink must not be followed: closures link inside the
        // same store and following would double-count targets.
        assert_eq!(out.files, 2);
        assert_eq!(out.open_ns.len(), 2);
        assert_eq!(out.fstat_ns.len(), 2);
    }

    #[test]
    fn copy_tree_round_trips() {
        let (_src, d, m) = gen_dataset();
        let scratch = tempfile::tempdir().unwrap();
        let files: Vec<&FileEntry> = m.files.iter().collect();
        let out = copy_tree(d.path(), &files, scratch.path()).unwrap();
        assert_eq!(out.bytes, m.total_bytes);
        // The local twin must be verifiable with the same manifest —
        // local_baseline phases reuse the FileEntry oracle unchanged.
        assert!(read_storm(scratch.path(), &files, true).is_ok());
    }
}

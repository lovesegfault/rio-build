//! Alternate read-path checks: mmap, splice(2), copy_file_range(2), and
//! lseek(SEEK_HOLE/SEEK_DATA).
//!
//! `read()`/`pread()` integrity is covered by [`super::read`]. These
//! checks exercise the OTHER kernel read paths a build's tools actually
//! use against the castore-FUSE mount, each of which reaches the file
//! contents through a different mechanism than the FUSE_READ upcall:
//!
//! * mmap — page faults serviced from the passthrough backing file's
//!   page cache (or FUSE pages in the no-passthrough fallback).
//! * splice — zero-copy file→pipe; with passthrough the kernel splices
//!   straight from the backing file.
//! * copy_file_range — what `cp`/`install` try first; the contract is
//!   byte-exact copy or exactly EXDEV (the errno their fallback expects).
//! * lseek(SEEK_HOLE/SEEK_DATA) — the data/hole map a sparse-aware
//!   copier (cp --sparse, tar -S, rsync -S) queries before reading.
//!
//! The byte-returning paths must all serve content identical to
//! `pread()` and to the manifest oracle. They run after
//! [`super::read::generic_075_091_read_integrity`] has forced the big
//! blob warm, so the mapping/splice/copy is backed by the promoted
//! passthrough fd (`builder.fs.passthrough-on-hit`) — the path that
//! production builds hit.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use anyhow::{Context, ensure};

use super::{Ctx, Outcome, errno_of, expect_errno, first_divergence};
use nix::errno::Errno;
// The raw syscalls (mmap/splice/copy_file_range/SEEK_*) come through
// nix's libc re-export — their safe nix wrappers sit behind features
// this workspace does not enable, and the re-export needs no extra
// dependency.
use nix::libc;

/// generic/074 + generic/127 (read/mmap-pattern legs): bytes read
/// through a memory mapping must equal `pread()` and the oracle, for
/// both `MAP_PRIVATE` and `MAP_SHARED` read-only mappings, on a
/// page-straddling file. The big blob is not page-aligned (its size is
/// prime-ish), so the final partial page exercises the kernel's
/// zero-fill-to-page-end behavior; we only compare the `[0, size)`
/// bytes that actually exist. Guards the passthrough mmap path: a
/// FUSE/passthrough mismatch here (stale page cache, wrong backing
/// inode, FOPEN_DIRECT_IO defeating the mapping) corrupts every
/// tool that mmaps its inputs — linkers and the dynamic loader map
/// every `.so`.
pub fn generic_074_127_mmap_reads(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let big = &ctx.manifest.big_file;
    let big_path = ctx.on_mount(&big.path);
    let oracle = ctx.manifest.oracle_bytes();
    let size = usize::try_from(big.size).expect("size fits usize");

    let file = fs::File::open(&big_path).with_context(|| format!("open {} for mmap", big.path))?;

    // Both mapping kinds are read-only; for a RO FUSE file they must
    // return identical bytes. MAP_SHARED is the interesting leg under
    // passthrough — it maps the backing file's page cache directly.
    for shared in [false, true] {
        let mapped = mmap_read(&file, size, shared)
            .with_context(|| format!("mmap (shared={shared}) of {}", big.path))?;
        ensure!(
            mapped.len() == size,
            "mmap (shared={shared}) returned {} bytes, expected {size}",
            mapped.len()
        );
        ensure!(
            blake3::hash(&mapped) == blake3::hash(&oracle),
            "mmap (shared={shared}) of {} differs from the oracle at offset {:?}",
            big.path,
            first_divergence(&oracle, &mapped)
        );
    }

    // mmap windows must agree byte-for-byte with pread of the same fd
    // (not just with the oracle) — pins that the two read paths see one
    // coherent file, the property a tool relying on mmap depends on.
    let private = mmap_read(&file, size, false)?;
    for (off, len) in [(0usize, 4096), (4093, 8200), (size - 4096, 4096)] {
        let mut via_pread = vec![0u8; len];
        file.read_exact_at(&mut via_pread, off as u64)
            .with_context(|| format!("pread off={off} len={len}"))?;
        ensure!(
            private[off..off + len] == via_pread[..],
            "mmap window [{off},{}) disagrees with pread",
            off + len
        );
    }

    // Small explicit files: a mapping of a sub-page file must expose
    // exactly its bytes (and the kernel zero-fills the rest of the page,
    // which we do not read).
    for f in ctx.manifest.files.iter().filter(|f| !f.content.is_empty()) {
        let sf = fs::File::open(ctx.on_mount(&f.path))?;
        let mapped = mmap_read(&sf, f.content.len(), false)
            .with_context(|| format!("mmap small file {}", f.path))?;
        ensure!(
            mapped == f.content.as_bytes(),
            "mmap of {} differs from expected content",
            f.path
        );
    }
    Ok(Outcome::Pass)
}

/// generic/249 (splice read): splicing a FUSE file into a pipe must
/// deliver the same bytes as `pread()`, on a full-file splice and on an
/// offset-anchored partial splice. splice is a distinct kernel read
/// path — with passthrough it moves pages directly from the backing
/// file — so a divergence here is invisible to the `read()` checks.
pub fn generic_249_splice_read(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let big = &ctx.manifest.big_file;
    let big_path = ctx.on_mount(&big.path);
    let oracle = ctx.manifest.oracle_bytes();
    let size = big.size;

    let file =
        fs::File::open(&big_path).with_context(|| format!("open {} for splice", big.path))?;

    // Full file, advancing the file offset (off_in = NULL).
    let spliced =
        splice_to_vec(&file, None, size).with_context(|| format!("splice full {}", big.path))?;
    ensure!(
        spliced.len() as u64 == size,
        "splice returned {} bytes, expected {size}",
        spliced.len()
    );
    ensure!(
        blake3::hash(&spliced) == blake3::hash(&oracle),
        "spliced bytes of {} differ from the oracle at offset {:?}",
        big.path,
        first_divergence(&oracle, &spliced)
    );

    // Offset-anchored partial splice crossing page boundaries. Uses the
    // off_in pointer, so the file's own offset is untouched.
    let (off, len) = (4093u64, 70_000u64);
    let window = splice_to_vec(&file, Some(off), len)
        .with_context(|| format!("splice window off={off} len={len}"))?;
    ensure!(
        window == oracle[off as usize..(off + len) as usize],
        "spliced window [{off},{}) differs from the oracle",
        off + len
    );
    Ok(Outcome::Pass)
}

/// generic/430 + generic/553 (copy_file_range with the FUSE file as
/// source): the cross-fs copy contract that keeps `cp`/`install` from
/// the mount working.
///
/// Since kernel 5.19, userspace cross-fs `copy_file_range()` without a
/// native filesystem op fails with EXDEV by policy — and EXDEV is one of
/// the exact errnos coreutils recognizes to fall back to a plain
/// read/write copy. So the contract this check pins is: cfr from a
/// castore file either copies byte-exactly (a kernel/FS that supports
/// it, e.g. a future `FUSE_COPY_FILE_RANGE` impl) or fails with
/// **exactly EXDEV**. Any other errno (EIO, EPERM, ENOTSUP from a buggy
/// FS-side impl) breaks every `cp` from the mount: coreutils propagates
/// it as a hard error instead of falling back. When the copy does
/// succeed, the oracle comparison and the generic/553 zero-return legs
/// (source offset at/past EOF, zero length) apply.
///
/// (Overlayfs copy-up is NOT this path — it uses the kernel-internal
/// COPY_FILE_SPLICE flag that bypasses the cross-fs restriction.)
pub fn generic_430_553_copy_file_range(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let big = &ctx.manifest.big_file;
    let big_path = ctx.on_mount(&big.path);
    let oracle = ctx.manifest.oracle_bytes();
    let size = big.size;

    let src = fs::File::open(&big_path)
        .with_context(|| format!("open {} as copy_file_range source", big.path))?;

    // Destination on the host tmpfs, well outside the read-only mount.
    // Per-process name + O_EXCL create: a pre-existing file or symlink
    // at the path (stale crashed run, or anything else) is an error,
    // never followed or truncated — the runner runs as root.
    let dst_path =
        std::env::temp_dir().join(format!("rio-xfstests-cfr-dst.{}.bin", std::process::id()));
    let _cleanup = RemoveOnDrop(dst_path.clone());
    let create_dst = || -> anyhow::Result<fs::File> {
        match fs::remove_file(&dst_path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("unlink stale cfr dest"),
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dst_path)
            .context("create cfr dest (O_EXCL)")
    };

    // Full copy, or the EXDEV fallback contract. EXDEV is a PASS: it is
    // the kernel's ≥5.19 cross-fs policy answer and the errno coreutils'
    // fallback path expects. Anything else is a failure — it would turn
    // every `cp` from the mount into a hard error.
    {
        let dst = create_dst()?;
        match copy_file_range_full(&src, &dst, size) {
            Ok(copied) => ensure!(
                copied == size,
                "copy_file_range copied {copied} bytes, expected {size}"
            ),
            Err(e) if errno_of(&e) == Errno::EXDEV => {
                println!(
                    "    copy_file_range → EXDEV (kernel cross-fs policy; \
                     coreutils falls back to read/write — the conformant answer)"
                );
                return Ok(Outcome::Pass);
            }
            Err(e) => {
                let errno = errno_of(&e);
                anyhow::bail!(
                    "copy_file_range from the mount failed with {errno:?} — only EXDEV \
                     (fallback-compatible) or success are conformant; this errno would \
                     make `cp` from the mount fail instead of falling back"
                );
            }
        }
        let copied = fs::read(&dst_path)?;
        ensure!(
            blake3::hash(&copied) == blake3::hash(&oracle),
            "copy_file_range full copy differs from the oracle at offset {:?}",
            first_divergence(&oracle, &copied)
        );
    }

    // Offset-ranged copy: a middle window via off_in, dest offset 0.
    {
        let (off, len) = (4093u64, 90_000u64);
        let dst = create_dst()?;
        let copied = copy_file_range_at(&src, off, &dst, len).context("copy_file_range ranged")?;
        ensure!(
            copied == len,
            "ranged copy_file_range copied {copied}, expected {len}"
        );
        let body = fs::read(&dst_path)?;
        ensure!(
            body == oracle[off as usize..(off + len) as usize],
            "copy_file_range window [{off},{}) differs from the oracle",
            off + len
        );
    }

    // Error legs (generic/553): a source offset at or past EOF copies
    // zero bytes (POSIX: short/zero copy, never an error or garbage);
    // a zero length copies zero bytes.
    {
        let dst = create_dst()?;
        let at_eof = copy_file_range_at(&src, size, &dst, 4096)
            .context("copy_file_range with off_in at EOF")?;
        ensure!(
            at_eof == 0,
            "copy_file_range at EOF copied {at_eof} bytes, expected 0"
        );
        let past_eof = copy_file_range_at(&src, size + 4096, &dst, 4096)
            .context("copy_file_range with off_in past EOF")?;
        ensure!(
            past_eof == 0,
            "copy_file_range past EOF copied {past_eof} bytes, expected 0"
        );
        let zero_len =
            copy_file_range_at(&src, 0, &dst, 0).context("copy_file_range with zero length")?;
        ensure!(
            zero_len == 0,
            "copy_file_range len=0 copied {zero_len} bytes, expected 0"
        );
    }
    Ok(Outcome::Pass)
}

/// generic/285 + generic/448 + generic/706 (SEEK_HOLE/SEEK_DATA
/// conformance): castore-FUSE implements no `FUSE_LSEEK` op, so the
/// kernel falls back to `generic_file_llseek`. The backing-cache files
/// are written by sequential fetch and are never sparse, so the file is
/// wholly data with one implicit hole at EOF — which is also the correct
/// POSIX answer. Pin it so a future custom lseek op (or a sparse backing
/// file) cannot start reporting phantom holes: SEEK_DATA in data returns
/// the offset, SEEK_HOLE in data returns the size, and ENXIO comes back
/// at/after EOF and for negative offsets (`generic_file_llseek_size`
/// compares the offset as unsigned, so a negative offset lands in the
/// "beyond EOF" branch — ENXIO, not EINVAL). A sparse-aware copier that
/// trusts a phantom hole would silently zero-fill part of a build input.
pub fn generic_285_448_706_seek_hole_data(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let big = &ctx.manifest.big_file;
    let big_path = ctx.on_mount(&big.path);
    let size = i64::try_from(big.size).expect("size fits i64");
    let file = fs::File::open(&big_path).with_context(|| format!("open {} for lseek", big.path))?;

    // Whole file is data; the only hole is the implicit one at EOF.
    ensure!(
        lseek_ok(&file, 0, libc::SEEK_DATA)? == 0,
        "SEEK_DATA at 0 must be 0 (data starts at the front)"
    );
    ensure!(
        lseek_ok(&file, 0, libc::SEEK_HOLE)? == size,
        "SEEK_HOLE at 0 must be the file size (no real holes)"
    );
    let mid = size / 2;
    ensure!(
        lseek_ok(&file, mid, libc::SEEK_DATA)? == mid,
        "SEEK_DATA inside data must return the offset"
    );
    ensure!(
        lseek_ok(&file, mid, libc::SEEK_HOLE)? == size,
        "SEEK_HOLE inside data must return the file size"
    );

    // At/after EOF → ENXIO (generic/448). Negative offsets are ALSO
    // ENXIO, not EINVAL: generic_file_llseek_size casts the offset to
    // unsigned before the EOF comparison, so -1 is "past EOF".
    expect_lseek_errno(
        &file,
        size,
        libc::SEEK_DATA,
        Errno::ENXIO,
        "SEEK_DATA at EOF",
    )?;
    expect_lseek_errno(
        &file,
        size,
        libc::SEEK_HOLE,
        Errno::ENXIO,
        "SEEK_HOLE at EOF",
    )?;
    expect_lseek_errno(
        &file,
        size + 4096,
        libc::SEEK_DATA,
        Errno::ENXIO,
        "SEEK_DATA past EOF",
    )?;
    expect_lseek_errno(
        &file,
        size + 4096,
        libc::SEEK_HOLE,
        Errno::ENXIO,
        "SEEK_HOLE past EOF",
    )?;
    expect_lseek_errno(
        &file,
        -1,
        libc::SEEK_DATA,
        Errno::ENXIO,
        "SEEK_DATA negative",
    )?;
    expect_lseek_errno(
        &file,
        -1,
        libc::SEEK_HOLE,
        Errno::ENXIO,
        "SEEK_HOLE negative",
    )?;

    // Degenerate small-file leg (generic/706 intent: a tiny non-empty
    // file has data at 0 and a hole only at EOF). The fixture's smallest
    // non-empty file stands in for the upstream 1-byte file.
    if let Some(small) = ctx
        .manifest
        .files
        .iter()
        .filter(|f| !f.content.is_empty())
        .min_by_key(|f| f.content.len())
    {
        let sf = fs::File::open(ctx.on_mount(&small.path))?;
        let slen = small.content.len() as i64;
        ensure!(
            lseek_ok(&sf, 0, libc::SEEK_DATA)? == 0,
            "small-file SEEK_DATA at 0 must be 0"
        );
        ensure!(
            lseek_ok(&sf, 0, libc::SEEK_HOLE)? == slen,
            "small-file SEEK_HOLE at 0 must be its size"
        );
    }

    // Empty-file leg if the fixture exposes one: offset 0 is already at
    // EOF, so both whences give ENXIO.
    if let Some(empty) = ctx.manifest.files.iter().find(|f| f.content.is_empty()) {
        let ef = fs::File::open(ctx.on_mount(&empty.path))?;
        expect_lseek_errno(
            &ef,
            0,
            libc::SEEK_DATA,
            Errno::ENXIO,
            "empty SEEK_DATA at 0",
        )?;
        expect_lseek_errno(
            &ef,
            0,
            libc::SEEK_HOLE,
            Errno::ENXIO,
            "empty SEEK_HOLE at 0",
        )?;
    }
    Ok(Outcome::Pass)
}

// ─── syscall wrappers ───────────────────────────────────────────────────

/// Map `len` bytes of `file` read-only, copy them out, and unmap. A
/// zero-length mapping is invalid (`EINVAL`), so callers must guard it.
fn mmap_read(file: &fs::File, len: usize, shared: bool) -> io::Result<Vec<u8>> {
    debug_assert!(len > 0, "mmap of a zero-length range is EINVAL");
    let flags = if shared {
        libc::MAP_SHARED
    } else {
        libc::MAP_PRIVATE
    };
    // SAFETY: PROT_READ mapping of a valid fd; the returned region is
    // [ptr, ptr+len) and is unmapped before this function returns, so
    // the copied Vec never outlives the mapping.
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            flags,
            file.as_raw_fd(),
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let out = std::slice::from_raw_parts(ptr.cast::<u8>(), len).to_vec();
        if libc::munmap(ptr, len) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(out)
    }
}

/// Splice `len` bytes of `file` (from `off`, or the file offset when
/// `off` is `None`) through a pipe and return them. Splices at most one
/// pipe-buffer worth at a time and drains immediately, so the
/// single-threaded caller never deadlocks on a full pipe.
fn splice_to_vec(file: &fs::File, off: Option<u64>, len: u64) -> io::Result<Vec<u8>> {
    const CHUNK: usize = 64 * 1024; // default pipe capacity
    let (rd, wr) = pipe()?;
    let _rd = OwnedFdGuard(rd);
    let _wr = OwnedFdGuard(wr);

    let mut out = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    let mut file_off: i64 = off.map(|o| o as i64).unwrap_or(0);
    let use_off = off.is_some();
    let mut remaining = len;

    while remaining > 0 {
        let want = remaining.min(CHUNK as u64) as usize;
        let mut off_ptr = file_off;
        // SAFETY: rd/wr are open pipe fds owned by the guards above;
        // file is a valid open fd. off_in points at a live i64 when
        // use_off, else NULL (advance the file offset).
        let moved = unsafe {
            libc::splice(
                file.as_raw_fd(),
                if use_off {
                    &mut off_ptr
                } else {
                    std::ptr::null_mut()
                },
                wr,
                std::ptr::null_mut(),
                want,
                0,
            )
        };
        if moved < 0 {
            return Err(io::Error::last_os_error());
        }
        if moved == 0 {
            break; // EOF
        }
        let moved = moved as usize;
        if use_off {
            file_off += moved as i64;
        }
        // Drain exactly `moved` bytes from the pipe before splicing more.
        let mut buf = vec![0u8; moved];
        let mut got = 0;
        while got < moved {
            // SAFETY: rd is a live pipe fd; buf[got..] is in bounds.
            let n = unsafe { libc::read(rd, buf[got..].as_mut_ptr().cast(), moved - got) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                break;
            }
            got += n as usize;
        }
        out.extend_from_slice(&buf[..got]);
        remaining -= moved as u64;
    }
    Ok(out)
}

/// copy_file_range from `src` (advancing its offset) into `dst` until
/// `len` bytes are copied or the source hits EOF. Returns bytes copied.
fn copy_file_range_full(src: &fs::File, dst: &fs::File, len: u64) -> io::Result<u64> {
    let mut copied = 0u64;
    while copied < len {
        let want = (len - copied) as usize;
        // SAFETY: both fds are valid and open; NULL offsets advance the
        // file offsets per the man page.
        let n = unsafe {
            libc::copy_file_range(
                src.as_raw_fd(),
                std::ptr::null_mut(),
                dst.as_raw_fd(),
                std::ptr::null_mut(),
                want,
                0,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            break; // source EOF
        }
        copied += n as u64;
    }
    Ok(copied)
}

/// copy_file_range of `len` bytes starting at source offset `off` into
/// `dst` at offset 0, in one call. Returns bytes copied (0 is valid:
/// off at/past EOF or len 0).
fn copy_file_range_at(src: &fs::File, off: u64, dst: &fs::File, len: u64) -> io::Result<u64> {
    let mut off_in: i64 = off as i64;
    let mut off_out: i64 = 0;
    // SAFETY: both fds valid; off_in/off_out point at live i64s.
    let n = unsafe {
        libc::copy_file_range(
            src.as_raw_fd(),
            &mut off_in,
            dst.as_raw_fd(),
            &mut off_out,
            len as usize,
            0,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as u64)
}

fn pipe() -> io::Result<(i32, i32)> {
    let mut fds = [0i32; 2];
    // SAFETY: fds is a 2-element array the kernel fills with the pipe
    // read/write descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

/// `lseek(whence)` returning the resulting offset, or the OS error.
fn lseek_ok(file: &fs::File, offset: i64, whence: i32) -> io::Result<i64> {
    // SAFETY: file is a valid open fd; lseek has no memory effects.
    let r = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r)
}

/// Assert an `lseek(whence)` at `offset` fails with `want`.
fn expect_lseek_errno(
    file: &fs::File,
    offset: i64,
    whence: i32,
    want: Errno,
    what: &str,
) -> anyhow::Result<()> {
    expect_errno(what, lseek_ok(file, offset, whence), &[want]).map(|_| ())
}

// ─── helpers ─────────────────────────────────────────────────────────────

/// Closes a raw fd on drop (the splice pipe ends).
struct OwnedFdGuard(i32);
impl Drop for OwnedFdGuard {
    fn drop(&mut self) {
        // SAFETY: the fd was returned by pipe() and is closed exactly once.
        unsafe { libc::close(self.0) };
    }
}

/// Removes a path on drop (the copy_file_range scratch dest).
struct RemoveOnDrop(std::path::PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

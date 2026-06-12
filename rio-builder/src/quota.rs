//! Read kubelet's XFS/ext4 project-quota usage for the per-build emptyDir.
//!
//! kubelet assigns a project ID to each emptyDir when the node filesystem
//! is mounted with `-o prjquota` (NixOS AMIs do this for the gp3-root
//! pool). The kernel then tracks `dqb_curspace` per project — the
//! CURRENT allocated bytes for the build's overlay upper dir at the
//! instant of the call (NOT a kernel-tracked high-water mark; `struct
//! dqblk` has no HWM field). The cgroup poll loop max-tracks across
//! samples to derive the peak. Unlike a `du` walk, this counts
//! unlinked-but-still-open files.
//!
//! `current_bytes()` reads the project ID via `FS_IOC_FSGETXATTR` on the
//! emptyDir, then `quotactl_fd(Q_GETQUOTA, PRJQUOTA, projid)` for the
//! current usage. `quotactl_fd` (Linux 5.14+) takes an open fd instead
//! of a block-device path, so no `/proc/mounts` grovel.
//!
//! Returns `Ok(None)` when the filesystem has no project quota assigned
//! (tmpfs, or a node without `-o prjquota`) — the caller falls back to
//! the existing `statvfs` sample.

use std::{fs::File, io, os::fd::AsRawFd, path::Path};

use nix::libc;

/// R17 (live_057-a): the ε in the quota-exhaustion predicate
/// `used ≥ hard_limit − DISK_FULL_QUOTA_SLACK_BYTES`. Nonzero because
/// the ENOSPC-refused write never lands: the post-failure
/// `dqb_curspace` sample sits BELOW the hard limit by up to the
/// refused write's size, so an exact `used ≥ limit` test misses real
/// exhaustions. VIOLABLE, hypothesis 64 MiB (the live_057 incident
/// pods' quota/statfs evidence was not retained, so the derivation
/// duty falls to the recorded fallback: a refused write is at most a
/// few output blobs — 64 MiB over-covers typical single-file refusals
/// while staying ≪ the 25 GiB ladder rung, so a false positive
/// requires a build that PARKED within 64 MiB of its quota and failed
/// for an unrelated reason — measure at the first classified
/// production sample and re-derive). Raising it trades false
/// negatives (missed sizing signals → retry-poison) for false
/// positives (a spurious classification inflates every affected
/// pname's disk request by one doubling).
pub const DISK_FULL_QUOTA_SLACK_BYTES: u64 = 64 << 20;

/// R17 (live_057-a): the statvfs floor below which disk exhaustion
/// attributes to the NODE, not the build's quota — when the node
/// filesystem itself is nearly full, the build's failure is the
/// node's problem (the result keeps its non-quota infra lane and the
/// scheduler re-places; S2's witnessed-eviction window is the
/// pod-level backstop per §1.6.4-15). VIOLABLE, hypothesis 2 GiB
/// (fallback derivation, same evidence note as the slack const:
/// kubelet's default imagefs/nodefs eviction thresholds sit at
/// 10-15% — 2 GiB is comfortably inside the band where kubelet
/// eviction, not prjquota, is the operative constraint on the
/// gp3-root pool's smallest nodes; measure and re-derive at the
/// first node-attributed sample).
pub const DISK_FULL_NODE_HEADROOM_BYTES: u64 = 2 << 30;

/// One project-quota sample: current usage + the hard limit
/// (`dqb_bhardlimit`, converted from 1 KiB quota blocks to bytes;
/// `None` = no limit set on the project).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaStatus {
    /// `dqb_curspace` — current allocated bytes.
    pub used_bytes: u64,
    /// `dqb_bhardlimit × 1024`, or `None` when the project has no
    /// hard limit (0 in the kernel record).
    pub hard_limit_bytes: Option<u64>,
}

/// The worker-side disk-exhaustion classification predicate
/// (live_057-a, the CgroupOom twin's truth table): a failed build is
/// quota-attributed iff the overlay's project quota is at/over its
/// hard limit (within [`DISK_FULL_QUOTA_SLACK_BYTES`] — the refused
/// write never lands) AND the node filesystem is not itself exhausted
/// (≥ [`DISK_FULL_NODE_HEADROOM_BYTES`] free — otherwise the node,
/// not the build's sizing, is the cause and the non-quota infra lane
/// keeps the report).
pub fn classify_quota_exhaustion(quota: QuotaStatus, node_free_bytes: u64) -> bool {
    let Some(limit) = quota.hard_limit_bytes else {
        return false;
    };
    limit > 0
        && quota.used_bytes >= limit.saturating_sub(DISK_FULL_QUOTA_SLACK_BYTES)
        && node_free_bytes >= DISK_FULL_NODE_HEADROOM_BYTES
}

/// `_IOR('X', 31, struct fsxattr)` — same value on x86_64 and aarch64
/// (`sizeof(struct fsxattr) == 28` → `0x801c581f`). The `nix` crate
/// doesn't bind this ioctl; hard-code rather than pull `ioctl_read!`
/// macro machinery for one call site.
const FS_IOC_FSGETXATTR: libc::c_ulong = 0x801c_581f;

/// Project quota type (`PRJQUOTA` in `<linux/quota.h>`). `libc` exports
/// `USRQUOTA`/`GRPQUOTA` but not this one.
const PRJQUOTA: libc::c_int = 2;

/// `<linux/fs.h>` `struct fsxattr`. Only `fsx_projid` is read; the rest
/// is padding for the ioctl ABI.
#[repr(C)]
#[derive(Default)]
struct Fsxattr {
    fsx_xflags: u32,
    fsx_extsize: u32,
    fsx_nextents: u32,
    fsx_projid: u32,
    fsx_cowextsize: u32,
    _pad: [u8; 8],
}

/// `QCMD(cmd, type)` from `<sys/quota.h>`: `(cmd << SUBCMDSHIFT) | type`.
fn qcmd(cmd: libc::c_int, typ: libc::c_int) -> libc::c_int {
    (cmd << 8) | (typ & 0xff)
}

/// Returns kubelet's project-quota usage for the emptyDir at `dir`, or
/// `None` if the filesystem has no project quota assigned (gp3-root pool
/// without `-o prjquota`, tmpfs, or any ioctl/syscall failure).
///
/// `io::Result` only for the initial `open()` — every other failure mode
/// is `Ok(None)` so the cgroup poll loop's `?`-free max-track stays simple.
// r[impl sched.sla.disk-scalar]
pub fn current_bytes(dir: &Path) -> io::Result<Option<u64>> {
    let f = File::open(dir)?;
    let mut x = Fsxattr::default();
    // SAFETY: FS_IOC_FSGETXATTR writes exactly sizeof(Fsxattr) bytes to
    // the pointer. `x` is repr(C), Default-zeroed, lives on our stack.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FSGETXATTR, &mut x as *mut _) } < 0 {
        // ENOTTY (tmpfs) / EOPNOTSUPP (fs without xattr) → no quota.
        return Ok(None);
    }
    if x.fsx_projid == 0 {
        // projid 0 = no project assigned (kubelet didn't set one, or
        // the node fs lacks -o prjquota).
        return Ok(None);
    }
    // SAFETY: dqblk is POD; zeroed is a valid bit-pattern. Kernel writes
    // it on Q_GETQUOTA success.
    let mut dq: libc::dqblk = unsafe { std::mem::zeroed() };
    let cmd = qcmd(libc::Q_GETQUOTA, PRJQUOTA);
    // SAFETY: SYS_quotactl_fd(int fd, int cmd, qid_t id, void *addr).
    // All four args are passed as c_long per the raw-syscall ABI.
    let r = unsafe {
        libc::syscall(
            libc::SYS_quotactl_fd,
            f.as_raw_fd() as libc::c_long,
            cmd as libc::c_long,
            x.fsx_projid as libc::c_long,
            &mut dq as *mut _ as libc::c_long,
        )
    };
    if r < 0 {
        // ENOSYS (kernel <5.14), ESRCH (projid has no quota record),
        // EINVAL (quota not enabled on this mount) → no signal.
        return Ok(None);
    }
    Ok(Some(dq.dqb_curspace))
}

/// The limit-read face (live_057-a): one sample of usage AND hard
/// limit for the project quota at `dir`. Same degradation contract as
/// [`current_bytes`] — `Ok(None)` when no project quota is assigned
/// (tmpfs, no `-o prjquota`, kernel <5.14, no quota record).
pub fn status(dir: &Path) -> io::Result<Option<QuotaStatus>> {
    let f = File::open(dir)?;
    let mut x = Fsxattr::default();
    // SAFETY: as in `current_bytes` — FS_IOC_FSGETXATTR writes exactly
    // sizeof(Fsxattr) bytes; `x` is repr(C), zeroed, on our stack.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FSGETXATTR, &mut x as *mut _) } < 0 {
        return Ok(None);
    }
    if x.fsx_projid == 0 {
        return Ok(None);
    }
    // SAFETY: dqblk is POD; zeroed is valid. Kernel fills on success.
    let mut dq: libc::dqblk = unsafe { std::mem::zeroed() };
    let cmd = qcmd(libc::Q_GETQUOTA, PRJQUOTA);
    // SAFETY: as in `current_bytes`.
    let r = unsafe {
        libc::syscall(
            libc::SYS_quotactl_fd,
            f.as_raw_fd() as libc::c_long,
            cmd as libc::c_long,
            x.fsx_projid as libc::c_long,
            &mut dq as *mut _ as libc::c_long,
        )
    };
    if r < 0 {
        return Ok(None);
    }
    Ok(Some(QuotaStatus {
        used_bytes: dq.dqb_curspace,
        // dqb_bhardlimit is in 1 KiB quota blocks; 0 = no limit.
        hard_limit_bytes: (dq.dqb_bhardlimit > 0).then(|| dq.dqb_bhardlimit.saturating_mul(1024)),
    }))
}

/// Free bytes on the filesystem holding `dir` (statvfs;
/// `f_bavail × f_frsize` — the unprivileged view, matching what a
/// build's writes can actually use). `None` on any failure.
///
/// merged_bug_074: under enforced prjquota with `PROJINHERIT`, a
/// statvfs taken INSIDE the project view is CLAMPED by the kernel to
/// the project (`f_bavail = limit − used`, `f_blocks ≈ limit`) — a
/// sample from the quota'd dir itself can never witness the NODE's
/// headroom. Callers needing the node view use
/// [`node_free_bytes_decoupled`].
pub fn node_free_bytes(dir: &Path) -> Option<u64> {
    statvfs_of(dir).map(|sv| sv.f_bavail.saturating_mul(sv.f_frsize))
}

/// The project id assigned to `dir`, or `None` when unreadable /
/// unassigned (tmpfs, fs without xattr, projid 0). The shared
/// `FS_IOC_FSGETXATTR` face of [`current_bytes`]/[`status`], exposed
/// for the decoupled-ancestor walk.
pub fn project_id(dir: &Path) -> Option<u32> {
    let f = File::open(dir).ok()?;
    let mut x = Fsxattr::default();
    // SAFETY: as in `current_bytes` — the ioctl writes exactly
    // sizeof(Fsxattr) bytes; `x` is repr(C), zeroed, on our stack.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FSGETXATTR, &mut x as *mut _) } < 0 {
        return None;
    }
    (x.fsx_projid != 0).then_some(x.fsx_projid)
}

/// Clamp detection (merged_bug_074, the in-tree arm): does this
/// statvfs look like the PROJECT view rather than the filesystem
/// view? Under XFS prjquota the kernel reports
/// `f_blocks ≈ hard_limit / f_frsize` for project-owned inodes, so a
/// total-capacity within one block of the hard limit is the clamp
/// signature. Pure over the sampled quantities (unit-testable; the
/// kernel-coupled end-to-end witness is the prjquota VM probe).
/// `false` when the limit is 0 (no enforcement → nothing to clamp
/// to) or `f_frsize` is 0 (degenerate statvfs).
pub fn statvfs_clamped(f_blocks: u64, f_frsize: u64, hard_limit_bytes: u64) -> bool {
    if hard_limit_bytes == 0 || f_frsize == 0 {
        return false;
    }
    let total = f_blocks.saturating_mul(f_frsize);
    total.abs_diff(hard_limit_bytes) <= f_frsize
}

// r[impl builder.disk.satisfiable-letter]
/// merged_bug_074 — the DECOUPLED node-headroom sample: free bytes of
/// the filesystem holding `dir`, taken from a vantage the project
/// clamp cannot reach. Walks same-device ancestors of `dir` and
/// returns the first statvfs that is neither project-owned
/// ([`project_id`] = `None`) nor clamp-shaped
/// ([`statvfs_clamped`] against `hard_limit_bytes`); `PROJINHERIT`
/// marks the quota'd subtree, so the first unowned ancestor (the
/// kubelet volume parent, or the mount root) reports the true node
/// view. `None` when no decoupled vantage exists on the device (every
/// ancestor project-owned or clamp-shaped, or statvfs fails) — the
/// caller's non-attribution lane, never a fabricated headroom.
pub fn node_free_bytes_decoupled(dir: &Path, hard_limit_bytes: Option<u64>) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(dir).ok()?.dev();
    let mut cur = dir.to_path_buf();
    // Bounded walk: a store path is never deeper than this, and the
    // loop must terminate even on a pathological mount layout.
    for _ in 0..64 {
        let Some(parent) = cur.parent() else {
            break; // filesystem root reached
        };
        let parent = parent.to_path_buf();
        let Ok(meta) = std::fs::metadata(&parent) else {
            break;
        };
        if meta.dev() != dev {
            break; // crossed a mount boundary — different filesystem
        }
        cur = parent;
        if project_id(&cur).is_some() {
            continue; // still inside a project (PROJINHERIT subtree)
        }
        let Some(sv) = statvfs_of(&cur) else {
            continue;
        };
        if let Some(limit) = hard_limit_bytes
            && statvfs_clamped(sv.f_blocks, sv.f_frsize, limit)
        {
            // Belt over the projid test: some configurations clamp
            // without exposing the inherited projid at this level.
            continue;
        }
        return Some(sv.f_bavail.saturating_mul(sv.f_frsize));
    }
    None
}

/// USED bytes on the filesystem holding `dir`
/// (`(f_blocks − f_bfree) × f_frsize`) — the H9″ fuse-cache occupancy
/// instrument (live_057-d): on a dedicated cache mount this is the
/// cache's occupancy exactly; on a shared mount it is the
/// filesystem-level upper bound, which is precisely the eviction-
/// pressure signal the measured-RULED fuse-addend trigger reads.
pub fn fs_used_bytes(dir: &Path) -> Option<u64> {
    statvfs_of(dir).map(|sv| {
        sv.f_blocks
            .saturating_sub(sv.f_bfree)
            .saturating_mul(sv.f_frsize)
    })
}

/// `(f_blocks, f_frsize)` of the filesystem view at `dir` — the raw
/// inputs of [`statvfs_clamped`], exposed for the prjquota VM probe
/// (`quota_probe`) so the clamp detector runs over the same numbers
/// in-VM as in-process. `None` on statvfs failure.
pub fn fs_capacity_bytes(dir: &Path) -> Option<(u64, u64)> {
    statvfs_of(dir).map(|sv| (sv.f_blocks, sv.f_frsize))
}

fn statvfs_of(dir: &Path) -> Option<libc::statvfs> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs writes exactly one struct statvfs on success;
    // zeroed is a valid bit-pattern for the out-param.
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut sv) } == 0).then_some(sv)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W10-CM (the unit-level matrix cells, live_057-a; re-keyed at
    /// merged_bug_074): the classification predicate's truth table
    /// over KERNEL-POSSIBLE input pairs. Input provenance per the
    /// satisfiable-letter contract — `used` is the during-build peak
    /// (project view; the 1Hz monitor max-track folded with the
    /// post-daemon one-shot), `node_free` is the DECOUPLED ancestor
    /// sample. Under the RETIRED same-dir sampling the positive cells
    /// were kernel-impossible (the clamp forced
    /// `node_free = limit − used ≤ slack < headroom` exactly when the
    /// quota conjunct held — the dead-letter pairing the prjquota VM
    /// probe demonstrates live); under the decoupled contract every
    /// cell below is reachable, so none are deleted. Includes the two
    /// negative corners the §1.6.4-15 order-pin's premise rides on.
    #[test]
    fn quota_exhaustion_classification_cells() {
        let gib = 1u64 << 30;
        let q = |used: u64, limit: u64| QuotaStatus {
            used_bytes: used,
            hard_limit_bytes: Some(limit),
        };
        // Positive: at the limit, node healthy.
        assert!(classify_quota_exhaustion(q(25 * gib, 25 * gib), 50 * gib));
        // Positive: within the slack of the limit (the refused write
        // never lands — dqb_curspace sits below the hard limit).
        assert!(classify_quota_exhaustion(
            q(25 * gib - DISK_FULL_QUOTA_SLACK_BYTES, 25 * gib),
            50 * gib
        ));
        // NEGATIVE (the false-positive corner): one byte below the
        // slack band — a spurious classification would inflate every
        // affected pname's disk request by a doubling.
        assert!(!classify_quota_exhaustion(
            q(25 * gib - DISK_FULL_QUOTA_SLACK_BYTES - 1, 25 * gib),
            50 * gib
        ));
        // NEGATIVE (the attribution corner): quota at limit but the
        // NODE fs is itself exhausted — the node's exhaustion is not
        // the build's sizing signal; the non-quota infra lane keeps
        // the report.
        assert!(!classify_quota_exhaustion(
            q(25 * gib, 25 * gib),
            DISK_FULL_NODE_HEADROOM_BYTES - 1
        ));
        // No hard limit / zero limit → never quota-attributed.
        assert!(!classify_quota_exhaustion(
            QuotaStatus {
                used_bytes: 25 * gib,
                hard_limit_bytes: None
            },
            50 * gib
        ));
    }

    /// The statvfs faces degrade to `None`/sane values, never panic
    /// (the H9″ instrument and the node-headroom read share the
    /// helper).
    #[test]
    fn statvfs_faces_are_total() {
        assert!(node_free_bytes(std::path::Path::new("/definitely/not/a/path")).is_none());
        assert!(fs_used_bytes(std::path::Path::new("/definitely/not/a/path")).is_none());
        // /tmp exists: both faces answer.
        assert!(node_free_bytes(std::path::Path::new("/tmp")).is_some());
        assert!(fs_used_bytes(std::path::Path::new("/tmp")).is_some());
    }

    /// merged_bug_074 — the clamp-detection arm's pure cells: a
    /// statvfs whose total capacity sits within one block of the
    /// project hard limit is the PROJECT view, not the node view.
    /// (The kernel-coupled end-to-end witness is the prjquota VM
    /// probe; these cells pin the detector's algebra.)
    #[test]
    fn statvfs_clamp_detection_cells() {
        let gib = 1u64 << 30;
        let frsize = 4096u64;
        let blocks_for = |bytes: u64| bytes / frsize;
        // Exact clamp: f_blocks * frsize == hard limit.
        assert!(statvfs_clamped(blocks_for(25 * gib), frsize, 25 * gib));
        // Off-by-one-block (rounding at the kernel's conversion).
        assert!(statvfs_clamped(blocks_for(25 * gib) - 1, frsize, 25 * gib));
        // A real node view: total far above the limit.
        assert!(!statvfs_clamped(blocks_for(500 * gib), frsize, 25 * gib));
        // No enforcement → nothing to clamp to.
        assert!(!statvfs_clamped(blocks_for(25 * gib), frsize, 0));
        // Degenerate statvfs.
        assert!(!statvfs_clamped(blocks_for(25 * gib), 0, 25 * gib));
    }

    /// The decoupled node-headroom walk degrades totally: a
    /// nonexistent dir is `None`; a real unquota'd dir (its ancestors
    /// carry no project) answers with the filesystem view; the walk
    /// never panics and never crosses a mount into a different
    /// filesystem's numbers (asserted via the device pin inside the
    /// walk — the /tmp case exercises the same-device path).
    #[test]
    fn node_free_decoupled_degrades_totally() {
        assert!(
            node_free_bytes_decoupled(std::path::Path::new("/definitely/not/a/path"), None)
                .is_none()
        );
        // /tmp's parent chain has no project quotas on dev nodes —
        // the first ancestor answers with the plain statvfs view.
        let got = node_free_bytes_decoupled(std::path::Path::new("/tmp"), None);
        // On a tmpfs root mount the walk stops at the mount boundary
        // and may answer None; on a shared root it answers Some.
        // Either way: no panic, and Some values are plausible bytes.
        if let Some(free) = got {
            assert!(free > 0 || node_free_bytes(std::path::Path::new("/tmp")) == Some(0));
        }
    }

    /// tmpfs has no project quotas → `Ok(None)`, not `Err`. Verifies
    /// the ENOTTY-on-ioctl path degrades gracefully (the cgroup poll
    /// loop must not crash on a node without `-o prjquota`).
    // r[verify sched.sla.disk-scalar]
    #[test]
    fn returns_none_on_tmpfs() {
        let r = current_bytes(std::path::Path::new("/tmp"));
        assert!(matches!(r, Ok(None)), "got {r:?}");
    }
}

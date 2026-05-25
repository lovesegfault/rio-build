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

/// `_IOR('X', 31, struct fsxattr)` — same value on x86_64 and aarch64
/// (`sizeof(struct fsxattr) == 28` → `0x801c581f`). The `nix` crate
/// doesn't bind this ioctl; hard-code rather than pull `ioctl_read!`
/// macro machinery for one call site.
const FS_IOC_FSGETXATTR: libc::c_ulong = 0x801c_581f;

/// `_IOW('X', 32, struct fsxattr)` — the set-side counterpart.
const FS_IOC_FSSETXATTR: libc::c_ulong = 0x401c_5820;

/// `<uapi/linux/fs.h>` `FS_XFLAG_PROJINHERIT`: new children inherit the
/// directory's project ID, so every file a build writes under its
/// staging dir is accounted against the build's quota.
const FS_XFLAG_PROJINHERIT: u32 = 0x0000_0200;

/// `<sys/quota.h>` `QIF_LIMITS` (`QIF_BLIMITS | QIF_ILIMITS`): which
/// `dqblk` fields `Q_SETQUOTA` should apply.
const QIF_LIMITS: u32 = 1 | 4;

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

/// Assign XFS project `projid` to the directory behind `dirfd` (with
/// `PROJINHERIT` so children are accounted against it) and set a hard
/// block limit of `limit_bytes`. `limit_bytes == 0` clears the limit
/// (used at teardown so dead projids don't accumulate quota records).
///
/// Unlike [`current_bytes`], failures here are **errors**, not `None`:
/// rio-mountd calls this to enforce a security bound
/// (`r[builder.mountd.staging-quota]`) and silently running unquota'd
/// would defeat it. The caller decides whether the filesystem is
/// required to support project quotas.
///
/// Uses the generic `Q_SETQUOTA`/`struct dqblk` interface rather than
/// the XFS-specific `Q_XSETQLIM`/`fs_disk_quota`: XFS wires the VFS
/// quota ops, `libc` already ships the `dqblk` layout, and the generic
/// path also covers ext4-with-`prjquota`. `dqb_bhardlimit` is in
/// 1024-byte blocks per `<sys/quota.h>`.
pub fn apply_project_quota(dirfd: &impl AsRawFd, projid: u32, limit_bytes: u64) -> io::Result<()> {
    // ── 1. Tag the directory with the project id + inherit flag.
    let mut x = Fsxattr::default();
    // SAFETY: FS_IOC_FSGETXATTR writes exactly sizeof(Fsxattr) bytes to
    // the pointer. `x` is repr(C), Default-zeroed, lives on our stack.
    if unsafe { libc::ioctl(dirfd.as_raw_fd(), FS_IOC_FSGETXATTR, &mut x as *mut _) } < 0 {
        return Err(io::Error::last_os_error());
    }
    x.fsx_projid = projid;
    x.fsx_xflags |= FS_XFLAG_PROJINHERIT;
    // SAFETY: FS_IOC_FSSETXATTR reads exactly sizeof(Fsxattr) bytes.
    if unsafe { libc::ioctl(dirfd.as_raw_fd(), FS_IOC_FSSETXATTR, &x as *const _) } < 0 {
        return Err(io::Error::last_os_error());
    }

    // ── 2. Hard block limit on the project.
    // SAFETY: dqblk is POD; zeroed is a valid bit-pattern.
    let mut dq: libc::dqblk = unsafe { std::mem::zeroed() };
    dq.dqb_bhardlimit = limit_bytes.div_ceil(1024);
    dq.dqb_bsoftlimit = dq.dqb_bhardlimit;
    dq.dqb_valid = QIF_LIMITS;
    let cmd = qcmd(libc::Q_SETQUOTA, PRJQUOTA);
    // SAFETY: SYS_quotactl_fd(int fd, int cmd, qid_t id, void *addr).
    let r = unsafe {
        libc::syscall(
            libc::SYS_quotactl_fd,
            dirfd.as_raw_fd() as libc::c_long,
            cmd as libc::c_long,
            projid as libc::c_long,
            &mut dq as *mut _ as libc::c_long,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tmpfs has no project quotas → `Ok(None)`, not `Err`. Verifies
    /// the ENOTTY-on-ioctl path degrades gracefully (the cgroup poll
    /// loop must not crash on a node without `-o prjquota`).
    // r[verify sched.sla.disk-scalar]
    #[test]
    fn returns_none_on_tmpfs() {
        let r = current_bytes(std::path::Path::new("/tmp"));
        assert!(matches!(r, Ok(None)), "got {r:?}");
    }

    /// The set side must FAIL (not silently no-op) on a filesystem
    /// without project-quota support — rio-mountd treats that as a
    /// fatal Mount error because the staging quota is a security bound.
    #[test]
    fn apply_fails_loudly_without_prjquota() {
        let dir = tempfile::tempdir().unwrap();
        let f = File::open(dir.path()).unwrap();
        let r = apply_project_quota(&f, 4242, 1 << 20);
        assert!(r.is_err(), "tmpfs must not accept a project quota: {r:?}");
    }
}

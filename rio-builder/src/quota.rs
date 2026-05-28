//! Project-quota usage for the per-build emptyDir: read kubelet's
//! assignment when one exists, mint our own when kubelet declines.
//!
//! PRECONDITION (live060-b — this module's producer is DEAD without
//! it, and for an entire deployment era nothing said so): the
//! filesystem hosting kubelet's emptyDir volumes must be mounted with
//! `-o prjquota` AND a project ID must be assigned to the emptyDir.
//! The mount-option half is node provisioning (the live060-a work —
//! xfs `-o prjquota` at /var/lib/kubelet). The projid half has TWO
//! assigners with complementary jurisdictions:
//!
//! - **kubelet** (`LocalStorageCapacityIsolationFSQuotaMonitoring`):
//!   assigns a projid per emptyDir of a USER-NAMESPACED pod
//!   (`hostUsers: false`). Quota assignment is userns-conditioned at
//!   the deployed minor (kubelet 1.36 refuses `SupportsQuotas` for
//!   host-user pods).
//! - **the builder itself** ([`ensure_project_quota`], live_063):
//!   under `hostUsers: true` — the production builder pools' actual
//!   posture (I-186 FUSE passthrough, pinned until P0560) — kubelet
//!   never assigns, so `setup_overlay` self-assigns a projid to the
//!   emptyDir root from the builder-owned range. The kernel permits
//!   `FS_IOC_FSSETXATTR` projid changes only from the init user
//!   namespace, which is exactly the `hostUsers: true` posture — the
//!   two assigners partition the posture space with no gap.
//!
//! THE FOUR DECLINE MODES (every one degrades to `Ok(None)` reads and
//! `peak_disk_bytes: None` completions — counted and warned once per
//! pod at the completion seam via
//! `rio_builder_quota_evidence_absent_total`, never fatal):
//!
//! 1. **Filesystem**: no `-o prjquota` mount (stock ext4 root, tmpfs).
//!    The live060 silence: 159/160 builder nodes EBS-only ext4,
//!    2022/2022 completions evidence-free.
//! 2. **Kernel**: no `quotactl_fd` (Linux < 5.14), or quotas not
//!    enabled on the mount.
//! 3. **Kubelet half**: feature gate off, `/etc/projects`+`/etc/projid`
//!    registry missing, or the FHS shim absent — kubelet silently
//!    declines for pods it WOULD otherwise cover.
//! 4. **Posture** (live_063): the pod runs `hostUsers: true`, kubelet
//!    refuses quota assignment for host-user pods, and modes 1–3 are
//!    all healthy — the wave-12 fleet shape: 56/56 nodes provisioned,
//!    0/1912 completions with `Some`. Resolved by the builder-minted
//!    path above; on a `hostUsers: false` pod the mint observes
//!    kubelet's projid and stands down (kubelet precedence).
//!
//! When the precondition holds, the kernel tracks `dqb_curspace` per
//! project — the
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

/// `_IOW('X', 32, struct fsxattr)` → `0x401c5820`. The write twin of
/// [`FS_IOC_FSGETXATTR`]; same hard-code rationale.
const FS_IOC_FSSETXATTR: libc::c_ulong = 0x401c_5820;

/// `FS_XFLAG_PROJINHERIT` (`<linux/fs.h>`): new children of a
/// directory carrying this flag inherit its project ID — one
/// assignment at the emptyDir root covers every per-build path
/// created after it, kernel-side, with no per-file work.
const FS_XFLAG_PROJINHERIT: u32 = 0x0000_0200;

/// `<sys/quota.h>` `QIF_LIMITS` (`QIF_BLIMITS | QIF_ILIMITS`): which
/// `dqblk` fields `Q_SETQUOTA` should apply.
const QIF_LIMITS: u32 = 1 | 4;

/// kubelet's project-ID allocator base (`volume/util/fsquota`:
/// `firstQuota = 1048576`). Kubelet assigns projids upward from here
/// and records them in `/etc/projid`; the builder-owned range below
/// is collision-free against it BY CONSTRUCTION.
const KUBELET_PROJID_BASE: u32 = 1_048_576;

/// R17 (live_063): the builder-minted projid range is the half-open
/// `[BUILDER_PROJID_BASE, BUILDER_PROJID_BASE + BUILDER_PROJID_RANGE)`
/// = `[2^19, 2^20)`, ending EXACTLY at `KUBELET_PROJID_BASE` — the
/// two allocators partition the id space statically (no registry
/// round-trip, no shared lock; the compile-time assert below pins the
/// adjacency). Within the range, candidates derive from the emptyDir
/// root's inode number (`candidate_projid`) and a free-record probe
/// disambiguates the residual congruence class — see
/// [`ensure_project_quota`]'s collision note.
pub const BUILDER_PROJID_BASE: u32 = 1 << 19;
/// See [`BUILDER_PROJID_BASE`].
pub const BUILDER_PROJID_RANGE: u32 = 1 << 19;

// The static range-discipline witness: the builder range ends exactly
// at kubelet's base. A drift in either constant is a compile error,
// not a runtime collision.
const _: () = assert!(BUILDER_PROJID_BASE + BUILDER_PROJID_RANGE == KUBELET_PROJID_BASE);

/// rio-mountd's project-ID range floor. mountd staging dirs claim ids
/// in `[MOUNTD_PROJID_BASE, MOUNTD_PROJID_CEILING)` — disjoint from
/// the builder-minted `[BUILDER_PROJID_BASE, …)` and kubelet's
/// `[KUBELET_PROJID_BASE, …)` so the three allocators partition the id
/// space statically when `/var/rio` is a bind of the kubelet XFS
/// (rio-nvme nodes; PROJID-04).
pub const MOUNTD_PROJID_BASE: u32 = 1;
/// See [`MOUNTD_PROJID_BASE`].
pub const MOUNTD_PROJID_CEILING: u32 = BUILDER_PROJID_BASE;
const _: () = assert!(MOUNTD_PROJID_CEILING <= BUILDER_PROJID_BASE);

/// R17 (live_063): bound on the linear free-record probe. VIOLABLE,
/// hypothesis 64: a collision needs two live emptyDirs on one node
/// whose root inodes are congruent mod 2^19 — with O(100) builder
/// pods per node the expected occupancy of any one congruence class
/// is ≪ 1, so 64 linear steps over-cover while bounding the syscall
/// budget of a pathological mount. Exhaustion degrades to no-mint
/// (`Unavailable`), never an error.
const PROJID_PROBE_LIMIT: u32 = 64;

/// A project ID acquired (or observed) on the emptyDir root — the
/// typed handle of the R32 acquisition; constructed only by
/// [`ensure_project_quota`]'s observe/mint arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectId(u32);

impl ProjectId {
    /// The raw id, for logging and the VM probe's kv grammar.
    pub fn get(self) -> u32 {
        self.0
    }
}

// r[impl builder.disk.enforcement-posture]
/// D-2 (R26 + R30 producer-reachability) — the project-quota
/// ENFORCEMENT posture, observed from the kernel's own limit record.
/// The DiskFull lane's first conjunct (`hard_limit_bytes` is a real
/// enforcing limit) depends on an external system's posture; this
/// letter surfaces that posture as a typed observable so the lane's
/// dormancy is a fact in the telemetry, not an inference from absence.
///
/// Produced by the actual detection (the `Q_GETQUOTA` limit-read after
/// [`ensure_project_quota`] has observed-or-minted the projid), never a
/// config assumption. Emitted once per pod via the
/// `rio_builder_quota_enforcement` gauge (label `mode` = [`label`]) and
/// a startup-tier log line at the overlay setup seam.
///
/// [`label`]: QuotaEnforcement::label
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaEnforcement {
    /// A real (sub-sentinel, nonzero) hard limit is set: the kernel
    /// enforces ENOSPC at the limit and [`classify_quota_exhaustion`]
    /// CAN fire. Not the deployed posture today; the readback that
    /// proves the future enforcing flip when the owner schedules it.
    Enforcing,
    /// kubelet's `AssignQuota` `-1` non-enforcing sentinel
    /// (`dqb_bhardlimit` reads back as `u64::MAX` through the
    /// 1 KiB-block saturating conversion): usage tracking for kubelet
    /// eviction only, never kernel enforcement at the deployed minors.
    /// The DiskFull lane is dormant — the `hostUsers: false` fleet
    /// shape under kubelet-assigned projids.
    NonEnforcing,
    /// A projid is set with NO hard limit (`dqb_bhardlimit == 0`):
    /// the builder's own monitoring-only mint (the deliberate non-goal
    /// of [`ensure_project_quota`] under `hostUsers: true`). The
    /// DiskFull lane is dormant — the `hostUsers: true` fleet shape.
    NoLimit,
    /// No project assigned, or the quota record is unreadable (decline
    /// modes 1–3). The DiskFull lane is dormant; sizing evidence is
    /// absent too (the live060-b producer is dead).
    Unavailable,
}

impl QuotaEnforcement {
    /// Derive the posture from one [`status`] read (the limit-read
    /// face), taken AFTER the projid is in place.
    pub fn classify(status: Option<QuotaStatus>) -> Self {
        match status {
            None => Self::Unavailable,
            Some(QuotaStatus {
                hard_limit_bytes: None,
                ..
            }) => Self::NoLimit,
            // The kubelet -1 sentinel: dqb_bhardlimit = u64::MAX
            // saturates to u64::MAX through the ×1024 conversion. No
            // real limit on a kubelet local disk approaches this.
            Some(QuotaStatus {
                hard_limit_bytes: Some(u64::MAX),
                ..
            }) => Self::NonEnforcing,
            Some(QuotaStatus {
                hard_limit_bytes: Some(_),
                ..
            }) => Self::Enforcing,
        }
    }

    /// The `mode` label value on `rio_builder_quota_enforcement`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Enforcing => "enforcing",
            Self::NonEnforcing => "non_enforcing",
            Self::NoLimit => "no_limit",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The closed outcome alphabet of [`ensure_project_quota`] (R14 — no
/// wildcard consumers; the overlay setup logs each arm distinctly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjQuota {
    /// A projid was already assigned (kubelet's, on a
    /// `hostUsers: false` pod — or our own from an earlier build in
    /// the same pod). Observed, honored, untouched.
    Existing(ProjectId),
    /// No projid was assigned and the builder minted one from the
    /// builder-owned range (the live_063 `hostUsers: true` path).
    Minted(ProjectId),
    /// No project quota is obtainable here (decline modes 1–3, a
    /// non-init user namespace, or probe exhaustion). The reader
    /// degrades to `None` exactly as before — never fatal.
    Unavailable,
}

impl ProjQuota {
    /// The assigned id, if any.
    pub fn project_id(self) -> Option<ProjectId> {
        match self {
            ProjQuota::Existing(id) | ProjQuota::Minted(id) => Some(id),
            ProjQuota::Unavailable => None,
        }
    }
}

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

/// `ioctl(FS_IOC_FSGETXATTR)`: read the project id + xflags of the
/// directory behind `fd`.
fn fsgetxattr(fd: libc::c_int) -> io::Result<Fsxattr> {
    let mut x = Fsxattr::default();
    // SAFETY: FS_IOC_FSGETXATTR writes exactly sizeof(Fsxattr) bytes to
    // the pointer. `x` is repr(C), Default-zeroed, lives on our stack.
    if unsafe { libc::ioctl(fd, FS_IOC_FSGETXATTR, &mut x as *mut _) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(x)
}

/// `quotactl_fd(2)` (Linux 5.14+): one quota command against the
/// filesystem holding `fd`. Whether `dq` is read or written depends on
/// `cmd`.
fn quotactl_fd(fd: libc::c_int, cmd: libc::c_int, id: u32, dq: &mut libc::dqblk) -> io::Result<()> {
    // SAFETY: SYS_quotactl_fd(int fd, int cmd, qid_t id, void *addr).
    // All four args are passed as c_long per the raw-syscall ABI.
    let r = unsafe {
        libc::syscall(
            libc::SYS_quotactl_fd,
            fd as libc::c_long,
            cmd as libc::c_long,
            id as libc::c_long,
            dq as *mut _ as libc::c_long,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    let Ok(x) = fsgetxattr(f.as_raw_fd()) else {
        // ENOTTY (tmpfs) / EOPNOTSUPP (fs without xattr) → no quota.
        return Ok(None);
    };
    if x.fsx_projid == 0 {
        // projid 0 = no project assigned (kubelet didn't set one, or
        // the node fs lacks -o prjquota).
        return Ok(None);
    }
    // SAFETY: dqblk is POD; zeroed is a valid bit-pattern. Kernel writes
    // it on Q_GETQUOTA success.
    let mut dq: libc::dqblk = unsafe { std::mem::zeroed() };
    let cmd = qcmd(libc::Q_GETQUOTA, PRJQUOTA);
    if quotactl_fd(f.as_raw_fd(), cmd, x.fsx_projid, &mut dq).is_err() {
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

/// The candidate id for probe step `attempt`: `BASE + ((ino + attempt)
/// mod RANGE)`. Pure — the unit cells pin the algebra (always in the
/// builder-owned range, wraps, distinct per attempt until the range is
/// exhausted); the kernel-coupled acquisition is the VM witness's job.
fn candidate_projid(ino: u64, attempt: u32) -> u32 {
    let range = u64::from(BUILDER_PROJID_RANGE);
    let slot = (ino % range + u64::from(attempt)) % range;
    // `slot < RANGE ≤ u32::MAX − BASE` by the compile-time range
    // assert, so the cast and the add are exact.
    BUILDER_PROJID_BASE + slot as u32
}

/// One free-record probe verdict (closed; no wildcard arms at the
/// consumer in [`ensure_project_quota`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeVerdict {
    /// No quota record, or a dead record (zero usage, zero inodes,
    /// zero limits) — safe to claim: project accounting counts live
    /// blocks only, so a previous tenant's reclaimed id carries
    /// nothing forward.
    Free,
    /// Live usage, live inodes, or a configured limit — another
    /// emptyDir on this filesystem owns the id; probe the next slot.
    Taken,
    /// The filesystem cannot answer (`EINVAL`/`ENOSYS`/… — quotas not
    /// enabled, kernel too old): mode 1/2 territory, the whole mint
    /// declines.
    Unsupported,
}

/// `Q_GETQUOTA` on `candidate` through the directory's fd.
fn probe_candidate(f: &File, candidate: u32) -> ProbeVerdict {
    // SAFETY: dqblk is POD; zeroed is valid. Kernel fills on success.
    let mut dq: libc::dqblk = unsafe { std::mem::zeroed() };
    let cmd = qcmd(libc::Q_GETQUOTA, PRJQUOTA);
    // SAFETY: as in `current_bytes` — four scalar args per the raw
    // syscall ABI; the out-pointer lives on our stack.
    let r = unsafe {
        libc::syscall(
            libc::SYS_quotactl_fd,
            f.as_raw_fd() as libc::c_long,
            cmd as libc::c_long,
            candidate as libc::c_long,
            &mut dq as *mut _ as libc::c_long,
        )
    };
    if r < 0 {
        // ESRCH/ENOENT = no record for this id → free. Anything else
        // (EINVAL: quota not enabled on the mount; ENOSYS: kernel
        // <5.14; EPERM; …) → the mint cannot proceed here at all.
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) | Some(libc::ENOENT) => ProbeVerdict::Free,
            _ => ProbeVerdict::Unsupported,
        };
    }
    let dead = dq.dqb_curspace == 0
        && dq.dqb_curinodes == 0
        && dq.dqb_bhardlimit == 0
        && dq.dqb_bsoftlimit == 0;
    if dead {
        ProbeVerdict::Free
    } else {
        ProbeVerdict::Taken
    }
}

// r[impl sched.sla.disk-scalar]
/// live_063 (decline mode #4): observe-or-mint the emptyDir root's
/// project ID — the R32 acquisition face of the overlay obligation.
/// Called by `setup_overlay` BEFORE the first per-build directory is
/// created under `base_dir`, so `FS_XFLAG_PROJINHERIT` carries the id
/// down the build's whole subtree and the existing readers
/// ([`current_bytes`] in the 1 Hz poll, [`status`] at the completion
/// seam — both consulting `base_dir`) work unchanged.
///
/// The acquisition lifecycle (R32, recorded):
/// - **acquire**: this call — idempotent; an already-assigned projid
///   (kubelet's under `hostUsers: false`, or ours from a previous
///   build in the same pod) is observed and honored
///   ([`ProjQuota::Existing`]), so the two assigners never fight.
/// - **carry**: `PROJINHERIT` — every path created below the root
///   inherits the id kernel-side; per-build dirs need no per-file
///   work and teardown needs no un-assignment.
/// - **account**: the kernel tracks `dqb_curspace` over LIVE blocks;
///   build teardown (`remove_dir_all`) returns the blocks and the
///   id's usage decays to ~0 of its own accord. The root keeps the
///   id for the pod's lifetime — kubelet-equivalent granularity (one
///   id per emptyDir, sequential builds share it exactly as they
///   would under kubelet's assignment).
/// - **no limit is minted**: monitoring-only, matching the acceptance
///   (`peak_disk_bytes` Some). Enforcement under `hostUsers: true`
///   (an ENOSPC at the sizing limit instead of kubelet's du-walk
///   eviction) is a deliberate non-goal of this path: it would change
///   the production failure mode and needs the sizeLimit plumbed into
///   the pod — [`status`] keeps reporting `hard_limit_bytes: None`
///   and [`classify_quota_exhaustion`] keeps returning `false`, both
///   exactly as on the pre-fix fleet.
///
/// Collision discipline: candidates live in the builder-owned
/// `[2^19, 2^20)` (statically disjoint from kubelet's `1048576+`
/// allocator — the `const` assert above), keyed on the root's inode
/// number and disambiguated by a bounded free-record probe. The
/// residual race (two probes passing the same dead record in the same
/// microsecond window, which additionally requires inode congruence
/// mod 2^19 across pods) pools the two builds' accounting; the
/// polarity is sizing-CONSERVATIVE (an inflated peak over-requests
/// disk — never a lost eviction signal), and the window is priced as
/// accepted residual rather than guarded by a cross-pod registry the
/// pod filesystem namespace cannot host.
///
/// Total: every failure (non-prjquota fs, kernel <5.14, non-init
/// userns — the kernel rejects projid changes outside it, which is
/// why this path CAN run under `hostUsers: true` and CANNOT under
/// `hostUsers: false`, where kubelet covers instead — probe
/// exhaustion, verify mismatch) degrades to
/// [`ProjQuota::Unavailable`]; the build proceeds and the reader
/// stays `None`, the same never-fatal contract as every other decline
/// mode.
pub fn ensure_project_quota(dir: &Path) -> ProjQuota {
    use std::os::unix::fs::MetadataExt;
    let Ok(f) = File::open(dir) else {
        return ProjQuota::Unavailable;
    };
    let mut x = Fsxattr::default();
    // SAFETY: as in `current_bytes` — the ioctl writes exactly
    // sizeof(Fsxattr) bytes; `x` is repr(C), zeroed, on our stack.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FSGETXATTR, &mut x as *mut _) } < 0 {
        // ENOTTY (tmpfs) / EOPNOTSUPP — mode 1: nothing to mint on.
        return ProjQuota::Unavailable;
    }
    if x.fsx_projid != 0 {
        // kubelet (or an earlier build in this pod) already assigned:
        // observe and stand down.
        return ProjQuota::Existing(ProjectId(x.fsx_projid));
    }
    let Ok(meta) = f.metadata() else {
        return ProjQuota::Unavailable;
    };
    let ino = meta.ino();
    let mut chosen = None;
    for attempt in 0..PROJID_PROBE_LIMIT {
        let candidate = candidate_projid(ino, attempt);
        match probe_candidate(&f, candidate) {
            ProbeVerdict::Free => {
                chosen = Some(candidate);
                break;
            }
            ProbeVerdict::Taken => continue,
            ProbeVerdict::Unsupported => return ProjQuota::Unavailable,
        }
    }
    let Some(candidate) = chosen else {
        // Probe exhausted — 64 occupied congruence slots on one
        // filesystem. Degrade rather than guess.
        return ProjQuota::Unavailable;
    };
    // Read-modify-write: keep every other xattr field/flag as GET
    // returned them (the kubelet fsquota applier does the same), set
    // the id, and raise PROJINHERIT so the subtree inherits.
    x.fsx_projid = candidate;
    x.fsx_xflags |= FS_XFLAG_PROJINHERIT;
    // SAFETY: FS_IOC_FSSETXATTR reads exactly sizeof(Fsxattr) bytes
    // from the pointer; `x` is repr(C), initialized, on our stack.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FSSETXATTR, &x as *const _) } < 0 {
        // EINVAL: non-init userns (the kernel's projid jurisdiction
        // rule) or xflags the fs rejects; EPERM: capability missing.
        // All decline modes, never errors.
        return ProjQuota::Unavailable;
    }
    // Verify-readback: the acquisition is claimed only on the
    // kernel's own word (R16 — the witness is the re-read, not the
    // ioctl's return alone).
    let mut check = Fsxattr::default();
    // SAFETY: as above.
    if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FSGETXATTR, &mut check as *mut _) } < 0 {
        return ProjQuota::Unavailable;
    }
    if check.fsx_projid == candidate && check.fsx_xflags & FS_XFLAG_PROJINHERIT != 0 {
        ProjQuota::Minted(ProjectId(candidate))
    } else {
        ProjQuota::Unavailable
    }
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

// r[impl builder.disk.satisfiable-letter+2]
/// merged_bug_074 — the DECOUPLED node-headroom sample: free bytes of
/// the filesystem holding `dir`, taken from a vantage the project
/// clamp cannot reach. Walks same-device ancestors of `dir` and
/// returns the first statvfs that is neither project-owned
/// ([`project_id`] = `None`) nor clamp-shaped
/// ([`statvfs_clamped`] against `hard_limit_bytes`); `PROJINHERIT`
/// marks the quota'd subtree, so the first unowned ancestor reports
/// the true node view.
///
/// merged_bug_012 (R31'-d(iii) — the production-topology face): in the
/// builder pod `dir` is the overlays-emptyDir mountPoint itself
/// (`/var/rio/overlays`); its parent `/var/rio` is container-rootfs
/// overlayfs (a different device), so the ancestor walk dead-ends at
/// the FIRST iteration and the host-namespace ancestor (the kubelet
/// volume's parent on the node disk) is unreachable from the pod's
/// mount namespace. The `sibling` fallback names a same-device sibling
/// mount — in production the fuse-cache emptyDir (`/var/rio/cache`),
/// backed by the same kubelet local-disk filesystem, outside `dir`'s
/// `PROJINHERIT` subtree — and is consulted only when the ancestor
/// walk yields nothing. The sibling's OWN project (kubelet's, with the
/// non-enforcing sentinel limit; or none under `hostUsers: true`)
/// leaves its statvfs unclamped, so it reports the node view.
///
/// `None` when no decoupled vantage exists on the device (every
/// ancestor project-owned or clamp-shaped AND no same-device sibling,
/// or statvfs fails) — the caller's non-attribution lane, never a
/// fabricated headroom.
pub fn node_free_bytes_decoupled(
    dir: &Path,
    hard_limit_bytes: Option<u64>,
    sibling: Option<&Path>,
) -> Option<u64> {
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
    // In-pod fallback (merged_bug_012): the ancestor walk yielded
    // nothing — `dir` is a mount root in the container namespace. A
    // same-device sibling mount on the same node filesystem, outside
    // `dir`'s project subtree, is the remaining decoupled vantage.
    let sibling = sibling?;
    if std::fs::metadata(sibling).ok()?.dev() != dev {
        return None; // not the same node filesystem — never fabricate
    }
    let sv = statvfs_of(sibling)?;
    // The sibling carries its OWN project (a different one — kubelet's
    // per-emptyDir id, or none under hostUsers:true). Its statvfs is
    // unclamped when that project has no enforcing limit.
    let sib_limit = status(sibling)
        .ok()
        .flatten()
        .and_then(|q| q.hard_limit_bytes);
    if let Some(l) = sib_limit
        && statvfs_clamped(sv.f_blocks, sv.f_frsize, l)
    {
        return None; // the sibling is clamped too — no vantage
    }
    Some(sv.f_bavail.saturating_mul(sv.f_frsize))
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
    let mut x = fsgetxattr(dirfd.as_raw_fd())?;
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
    quotactl_fd(dirfd.as_raw_fd(), cmd, projid, &mut dq)
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
            node_free_bytes_decoupled(std::path::Path::new("/definitely/not/a/path"), None, None)
                .is_none()
        );
        // /tmp's parent chain has no project quotas on dev nodes —
        // the first ancestor answers with the plain statvfs view.
        let got = node_free_bytes_decoupled(std::path::Path::new("/tmp"), None, None);
        // On a tmpfs root mount the walk stops at the mount boundary
        // and may answer None; on a shared root it answers Some.
        // Either way: no panic, and Some values are plausible bytes.
        if let Some(free) = got {
            assert!(free > 0 || node_free_bytes(std::path::Path::new("/tmp")) == Some(0));
        }
    }

    /// W14-C1 (merged_bug_012, R31'-d(iii)) — the production-topology
    /// witness. In the builder pod `overlay_base_dir` is the overlays-
    /// emptyDir mountPoint: its parent `/var/rio` is container-rootfs
    /// overlayfs (a different device), so the ancestor walk dead-ends
    /// at the FIRST iteration and `node_free` is structurally `None`
    /// in-pod — the second independent blocker on the DiskFull lane,
    /// behind the non-enforcing kubelet quota posture.
    ///
    /// Fixture: `/dev/shm` is a tmpfs mount root on every Linux test
    /// host (parent `/dev` is devtmpfs — a different device). The
    /// premise is asserted, not assumed; a host where it does not hold
    /// fails the witness rather than vacuously passing.
    ///
    /// PRE-FIX RED: with no sibling, a mount-root dir yields `None` —
    /// the conjunct is dead in the production topology. POST-FIX: a
    /// same-device sibling mount (the fuse-cache emptyDir in
    /// production; here a tempdir under the same tmpfs) answers with
    /// the node filesystem's free bytes, so the conjunct CAN be true
    /// in-pod.
    // r[verify builder.disk.satisfiable-letter+2]
    #[test]
    fn node_free_decoupled_in_pod_topology_consults_the_sibling() {
        use std::os::unix::fs::MetadataExt;
        let mount_root = std::path::Path::new("/dev/shm");
        let dev = std::fs::metadata(mount_root)
            .expect("/dev/shm exists on every Linux test host")
            .dev();
        let parent_dev = std::fs::metadata("/dev").unwrap().dev();
        assert_ne!(
            dev, parent_dev,
            "fixture premise: /dev/shm is a mount root (parent on a \
             different device) — the production in-pod topology"
        );
        // The PRE-FIX behavior, kept as the no-sibling face: the
        // ancestor walk on a mount-root dir dead-ends; no fabricated
        // headroom (the non-attribution lane).
        assert_eq!(
            node_free_bytes_decoupled(mount_root, None, None),
            None,
            "no sibling: a mount-root dir has no same-device ancestor"
        );
        // POST-FIX: a same-device sibling (the fuse-cache emptyDir in
        // production) is a valid decoupled vantage — same node
        // filesystem, outside `dir`'s project subtree.
        let sibling = tempfile::Builder::new()
            .prefix("rio-quota-sibling-")
            .tempdir_in(mount_root)
            .unwrap();
        let got = node_free_bytes_decoupled(mount_root, None, Some(sibling.path()));
        assert!(
            got.is_some(),
            "the same-device sibling vantage must answer in the in-pod \
             topology (merged_bug_012): got {got:?}"
        );
        // W14-C2 (host vantage preserved — the fallback fires only
        // when the ancestor walk yields nothing): on a deep
        // non-mount-root path the FIRST ancestor answers, the sibling
        // is never consulted, and a wrong-device sibling is harmless.
        let deep = sibling.path().join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        let via_ancestor = node_free_bytes_decoupled(&deep, None, None);
        assert!(via_ancestor.is_some(), "host topology: ancestor answers");
        assert_eq!(
            node_free_bytes_decoupled(&deep, None, Some(std::path::Path::new("/proc"))),
            via_ancestor,
            "the sibling is consulted only when the ancestor walk is empty"
        );
        // A different-device sibling is rejected (never a fabricated
        // cross-filesystem headroom).
        assert_eq!(
            node_free_bytes_decoupled(mount_root, None, Some(std::path::Path::new("/proc"))),
            None,
            "a sibling on a different device is not a vantage for `dir`'s \
             node filesystem"
        );
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

    /// live_063 — the candidate algebra cells (the pure half of the
    /// collision discipline; the kernel-coupled acquisition is the
    /// kubelet-projquota VM witness's provisioned × hostUsers:true
    /// cell). Every candidate lands in the builder-owned half-open
    /// range for every (ino, attempt) — including the u64 extremes —
    /// so no candidate can ever collide with kubelet's `1048576+`
    /// allocator; attempts walk distinct slots and wrap.
    #[test]
    fn builder_projid_candidates_stay_in_the_owned_range() {
        let hi = BUILDER_PROJID_BASE + BUILDER_PROJID_RANGE;
        for ino in [0u64, 1, 524_287, 524_288, u64::MAX - 1, u64::MAX] {
            for attempt in [0u32, 1, 63, BUILDER_PROJID_RANGE - 1, BUILDER_PROJID_RANGE] {
                let c = candidate_projid(ino, attempt);
                assert!(
                    (BUILDER_PROJID_BASE..hi).contains(&c),
                    "candidate {c} for (ino={ino}, attempt={attempt}) escaped \
                     [{BUILDER_PROJID_BASE}, {hi})"
                );
            }
        }
        // The range's far edge is exactly kubelet's base (the
        // compile-time assert's runtime echo, kept so a test reader
        // sees the adjacency without chasing the const).
        assert_eq!(hi, KUBELET_PROJID_BASE);
        // Attempts are a linear walk: distinct until the range wraps.
        let ino = 987_654_321u64;
        assert_ne!(candidate_projid(ino, 0), candidate_projid(ino, 1));
        assert_eq!(
            candidate_projid(ino, 0),
            candidate_projid(ino, BUILDER_PROJID_RANGE),
            "attempt walk must wrap at the range size"
        );
    }

    /// live_063 — the mint declines or observes per the host fs:
    /// nonexistent-path yields `Unavailable` unconditionally; `/tmp`
    /// yields `Unavailable` on tmpfs/ext4 (decline mode one) but
    /// `Existing(kubelet_id)` on rio builder pods where `/tmp` is
    /// XFS-prjquota emptyDir. The branch makes both arms an
    /// in-process assertion (sh-010: was env-dependent fail on rio
    /// self-host; the `Existing` arm was previously only
    /// alphabet-covered at `projquota_id_projection_cells`).
    #[test]
    fn ensure_declines_or_observes_per_host_fs() {
        assert_eq!(
            ensure_project_quota(std::path::Path::new("/definitely/not/a/path")),
            ProjQuota::Unavailable
        );
        let tmp = std::path::Path::new("/tmp");
        match project_id(tmp) {
            Some(id) => assert_eq!(
                ensure_project_quota(tmp),
                ProjQuota::Existing(ProjectId(id)),
                "prjquota present: ensure must observe the assigned id, not mint"
            ),
            None => assert_eq!(
                ensure_project_quota(tmp),
                ProjQuota::Unavailable,
                "no prjquota: ensure must decline, never panic"
            ),
        }
    }

    /// W14-C3/C4 (D-2, R26 + R30 producer-reachability) — the
    /// quota-enforcement posture is a typed, observable letter.
    /// PRE-FIX nothing surfaces the dormancy: the kubelet `-1` sentinel
    /// and the builder's monitoring-only no-limit mint both leave the
    /// DiskFull lane silently dormant. The presence-asserting test
    /// (this fn) is the RED. POST-FIX the closed alphabet partitions
    /// the limit-read result, and the gauge label is the alphabet.
    // r[verify builder.disk.enforcement-posture]
    #[test]
    fn quota_enforcement_posture_cells() {
        // W14-C3: the dormant fleet shapes — both readable, both
        // non-enforcing, distinguished by source.
        assert_eq!(
            QuotaEnforcement::classify(Some(QuotaStatus {
                used_bytes: 0,
                hard_limit_bytes: None,
            })),
            QuotaEnforcement::NoLimit,
            "the builder's monitoring-only mint (dqb_bhardlimit == 0)"
        );
        assert_eq!(
            QuotaEnforcement::classify(Some(QuotaStatus {
                used_bytes: 0,
                hard_limit_bytes: Some(u64::MAX),
            })),
            QuotaEnforcement::NonEnforcing,
            "kubelet's AssignQuota -1 sentinel (saturates to u64::MAX \
             through the 1KiB-block conversion)"
        );
        // W14-C4: the enforcing face — when a real (sub-sentinel)
        // hard limit IS read back, the letter says so. The witness for
        // the future posture flip: when the owner schedules enforcing
        // quotas, this letter is the readback that proves it.
        assert_eq!(
            QuotaEnforcement::classify(Some(QuotaStatus {
                used_bytes: 0,
                hard_limit_bytes: Some(25 << 30),
            })),
            QuotaEnforcement::Enforcing,
        );
        // The decline modes (no project / unreadable).
        assert_eq!(
            QuotaEnforcement::classify(None),
            QuotaEnforcement::Unavailable,
        );
        // The label alphabet is closed and the gauge name is reachable
        // from describe_metrics() (the (lllll) suite asserts the
        // registration; this pins the label values the suite cannot).
        for v in [
            QuotaEnforcement::Enforcing,
            QuotaEnforcement::NonEnforcing,
            QuotaEnforcement::NoLimit,
            QuotaEnforcement::Unavailable,
        ] {
            assert!(!v.label().is_empty());
        }
    }

    /// The `ProjQuota` alphabet is closed and its id projection total:
    /// both assigned arms expose the id, the decline arm exposes none.
    #[test]
    fn projquota_id_projection_cells() {
        let id = ProjectId(BUILDER_PROJID_BASE + 7);
        assert_eq!(ProjQuota::Existing(id).project_id(), Some(id));
        assert_eq!(ProjQuota::Minted(id).project_id(), Some(id));
        assert_eq!(ProjQuota::Unavailable.project_id(), None);
        assert_eq!(id.get(), BUILDER_PROJID_BASE + 7);
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

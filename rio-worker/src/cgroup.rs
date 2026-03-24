//! Per-build cgroup v2 resource tracking.
//!
//! Solves the whole-tree measurement problem: `read_vmhwm_bytes(daemon.id())`
//! measured nix-daemon's RSS (~10MB) because nix-daemon FORKS the
//! builder and waitpid()s — the builder's memory never appeared in
// r[impl worker.cgroup.sibling-layout]
// r[impl worker.cgroup.memory-peak]
//! daemon's `/proc`. cgroup `memory.peak` captures the WHOLE TREE
//! (daemon + builder + every compiler sub-process), which is what
//! size-class memory-bump actually needs.
//!
//! Bonus: `cpu.stat usage_usec` gives us cumulative tree-wide CPU
//! time, enabling `peak_cpu_cores` with the same mechanism.
//!
//! # cgroup v2 is a hard requirement
//!
//! `own_cgroup()` returns `Err` if `/sys/fs/cgroup` isn't cgroup2fs.
//! `enable_subtree_controllers()` returns `Err` if delegation isn't
//! set up (systemd `Delegate=yes` on the worker unit — the NixOS module
//! configures this). The worker's main.rs propagates both with `?` — startup
//! fails. No degraded mode: if a worker pod is Running, cgroup
//! tracking is live.
//!
//! This is intentional. A worker that silently falls back to the
//! phase2c VmHWM bug (measuring 10MB for every build) poisons
//! build_history with garbage that takes ~10 EMA cycles to wash out
//! even after the bug is fixed. Better to fail loud at startup.
//!
//! # Layout
//!
//! ```text
//! /sys/fs/cgroup/<worker-slice>/          ← own_cgroup() finds this
//!   cgroup.subtree_control                ← enable_subtree_controllers writes +memory +cpu
//!   <drv-hash>/                           ← BuildCgroup::create makes this per build
//!     cgroup.procs                        ← add_process writes daemon PID here
//!     memory.peak                         ← kernel-tracked tree peak, read at build end
//!     cpu.stat                            ← usage_usec, polled 1Hz for peak-cores
//! ```
//!
//! The `<drv-hash>` subdirectory name: derivation hashes are
//! nixbase32-encoded (alphabet `[0-9a-df-np-sv-z]`) so they're valid
//! cgroup directory names. No escaping needed. Max path component
//! length on cgroup2fs is 255 bytes; drv hashes are 32 chars.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// cgroup v2 mount point. Fixed by convention on every modern Linux
/// (systemd mounts it here unconditionally). We don't scan
/// `/proc/mounts` — if this isn't cgroup2fs, we fail startup anyway.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// RAII per-build cgroup. `Drop` removes the directory.
///
/// `rmdir` fails (`EBUSY`) if processes are still in the cgroup. The
/// executor calls `daemon.kill().await` + `daemon.wait().await` BEFORE
/// dropping this, so the tree should be empty. If rmdir still fails
/// (zombie grandchild we don't know about), we log and leak — the
/// kernel cleans up when the processes eventually die. Leaked
/// cgroups are empty directories under `/sys/fs/cgroup`; harmless,
/// but they accumulate. A pod restart clears them (fresh cgroup).
#[derive(Debug)]
pub struct BuildCgroup {
    path: PathBuf,
}

impl BuildCgroup {
    /// Create a sub-cgroup under `parent` named `name`.
    ///
    /// `parent` should be `own_cgroup()`. `name` should be the
    /// derivation hash (valid cgroup name chars — nixbase32 alphabet).
    ///
    /// `mkdir` can fail:
    /// - `EEXIST`: stale cgroup from a crashed build. We handle this
    ///   by removing it first (best-effort — if it has processes,
    ///   remove fails and so does our mkdir; the error is clear).
    /// - `EACCES`: delegation not configured. Should have failed at
    ///   `enable_subtree_controllers` — this is a can't-happen path
    ///   if main.rs called that first.
    pub fn create(parent: &Path, name: &str) -> io::Result<Self> {
        let path = parent.join(name);

        // Stale cgroup from a prior crash: same drv_hash, worker
        // restarted before cleanup. The directory exists but is
        // (probably) empty — no processes in it (the processes died
        // with the prior worker). rmdir succeeds; our mkdir succeeds.
        //
        // If it's NOT empty (very weird — crashed worker left a live
        // grandchild we don't own?): rmdir fails EBUSY, our mkdir
        // fails EEXIST, and we bubble a clear error. Operator
        // investigates. Don't try to be clever with SIGKILL-ing
        // unknown PIDs.
        if path.exists() {
            // Best-effort; mkdir below surfaces the real error.
            let _ = fs::remove_dir(&path);
        }

        fs::create_dir(&path)?;
        Ok(Self { path })
    }

    /// Move a process into this cgroup. Its entire future descendant
    /// tree is accounted here (cgroup inheritance on fork).
    ///
    /// Write `{pid}\n` to `cgroup.procs`. The kernel moves the
    /// process atomically. Fails (`EINVAL`) if the PID doesn't exist
    /// — which means the daemon died between spawn and this call.
    /// Executor treats that as a build failure anyway (no daemon =
    /// no build), so bubbling this error is correct.
    ///
    /// **Ordering matters:** call this AFTER `spawn_daemon` returns
    /// but BEFORE `run_daemon_build` — so the daemon is in the cgroup
    /// BEFORE it forks the builder. If we added it after the fork,
    /// the builder would be in the parent cgroup and we'd measure
    /// only daemon RSS (back to the phase2c bug).
    pub fn add_process(&self, pid: u32) -> io::Result<()> {
        // OpenOptions write (not append): cgroup.procs is a
        // pseudo-file; each write() syscall moves one PID. append
        // vs write-from-0 doesn't matter (the file doesn't have
        // "content" to overwrite). Use write for clarity.
        fs::write(self.path.join("cgroup.procs"), format!("{pid}\n"))
    }

    /// Kernel-tracked peak memory of the whole tree, in bytes.
    ///
    /// `memory.peak` is cumulative-max over the cgroup's lifetime —
    /// the kernel updates it on every allocation. One read at
    /// build-end gives the true peak, no polling.
    ///
    /// `None` on read/parse failure. The file CAN be missing
    /// (`memory` controller not enabled on this level) — but
    /// `enable_subtree_controllers` should have caught that. This
    /// is a belt-and-suspenders None.
    ///
    /// Format: single line, decimal bytes, no suffix. `"12345\n"`.
    pub fn memory_peak(&self) -> Option<u64> {
        read_single_u64(&self.path.join("memory.peak"))
    }

    /// Cumulative CPU time of the whole tree, in microseconds.
    ///
    /// `cpu.stat` has multiple lines; `usage_usec` is user+system
    /// combined across the tree. The caller polls this periodically
    /// and computes `delta / elapsed` for instantaneous cores-
    /// equivalent.
    ///
    /// `None` on read/parse failure (same caveat as memory_peak).
    ///
    /// Path to this cgroup. Exposed so the CPU polling task can
    /// clone it (the task outlives the borrowed `&BuildCgroup`
    /// across the `run_daemon_build` await).
    pub fn path(&self) -> &Path {
        &self.path
    }

    // r[impl worker.cancel.cgroup-kill]
    /// SIGKILL every process in this cgroup tree (including
    /// descendants in sub-cgroups). The kernel's `cgroup.kill`
    /// pseudo-file: write "1" → kernel sends SIGKILL to every PID
    /// in `cgroup.procs` recursively.
    ///
    /// This is the Cancel mechanism. The daemon + builder + every
    /// child dies immediately (SIGKILL can't be caught). nix-daemon
    /// has no chance to do its normal cleanup (which is FINE — we're
    /// abandoning this build entirely, the overlayfs Drop handles
    /// mount cleanup, and a crashed daemon's sandbox doesn't need
    /// orderly teardown).
    ///
    /// After this, `run_daemon_build` sees stdout EOF → returns
    /// Err(DaemonDied or similar). The executor's error path sends
    /// CompletionReport; the caller (spawn_build_task) checks the
    /// cancel flag to decide between InfrastructureFailure and
    /// Cancelled status.
    ///
    /// `io::Result`: can fail if the cgroup was already removed
    /// (race with Drop) or the kernel is too old for cgroup.kill
    /// (Linux 5.14+). The caller logs and the build lingers —
    /// worst case is the 2h daemon_timeout still applies.
    pub fn kill(&self) -> io::Result<()> {
        fs::write(self.path.join("cgroup.kill"), "1")
    }
}

/// Cancel a build by cgroup path (lookup via the cancel registry).
///
/// Static free function instead of a `BuildCgroup` method because the
/// cancel registry stores PathBufs (not BuildCgroup — those are owned
/// by the executor and dropped at the end of execute_build; the cancel
/// handler runs in the main event loop concurrently).
///
/// Same semantics as [`BuildCgroup::kill`]: SIGKILLs the whole tree.
/// `io::Result` — cgroup may have been removed (build finished between
/// the registry lookup and this call). Caller logs and continues.
pub fn kill_cgroup(path: &Path) -> io::Result<()> {
    fs::write(path.join("cgroup.kill"), "1")
}

impl Drop for BuildCgroup {
    fn drop(&mut self) {
        // Best-effort. EBUSY if processes remain; we log and leak.
        // The leaked cgroup is an empty (or nearly-empty) pseudo-
        // directory under /sys/fs/cgroup — harmless but untidy.
        // Pod restart clears the whole subtree.
        if let Err(e) = fs::remove_dir(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "cgroup rmdir failed (leaked; pod restart clears)"
            );
        }
    }
}

/// Find the delegated cgroup ROOT for per-build sub-cgroups.
///
/// This is the PARENT of the worker process's own cgroup. Why parent:
///
/// With systemd `DelegateSubgroup=builds`, the worker process runs in
/// `.../rio-worker.service/builds/`. `/proc/self/cgroup` points there.
/// But cgroup v2's no-internal-processes rule means `.../builds/`
/// cannot have BOTH the worker process AND sub-cgroups with enabled
/// controllers. So per-build cgroups go in `.../rio-worker.service/`
/// (the PARENT) as SIBLINGS of `builds/`. The service cgroup is
/// empty (DelegateSubgroup moved the worker out), so enabling
/// controllers on it succeeds.
///
/// Layout:
/// ```text
/// .../rio-worker.service/         ← delegated_root() returns THIS
///   cgroup.subtree_control        ← enable_subtree_controllers writes here (empty cgroup, no EBUSY)
///   builds/                       ← DelegateSubgroup; /proc/self/cgroup points here
///     cgroup.procs                ← worker PID
///   <drv-hash>/                   ← BuildCgroup::create per build (SIBLING of builds/)
///     cgroup.procs                ← nix-daemon PID
///     memory.peak, cpu.stat       ← resource tracking
/// ```
///
/// In K8s pods WITHOUT cgroup namespacing: same shape as systemd.
/// kubelet puts the container in a sub-cgroup of the pod cgroup;
/// `/proc/self/cgroup` points to the container cgroup; parent is
/// the pod cgroup (empty hierarchy node). Per-build cgroups
/// become siblings of the container.
///
/// In K8s pods WITH cgroup namespacing (containerd default):
/// `/proc/self/cgroup` shows `0::/` — the namespace root. We're
/// PID 1 in the CGROUP NS root. There IS no parent from our
/// perspective. Handling: replicate what `DelegateSubgroup=` does
/// — move ourselves into a `/leaf/` sub-cgroup, leaving the ns
/// root empty. Then the ns root IS the delegated root.
///
/// ```text
/// (container's namespace view)
/// /sys/fs/cgroup/              ← ns root, RETURNED as delegated_root
///   cgroup.subtree_control     ← enable_subtree_controllers works (empty now)
///   leaf/                      ← we created + moved ourselves here
///     cgroup.procs             ← worker PID (was in ns root)
///   <drv-hash>/                ← BuildCgroup::create per build
/// ```
///
/// Fails if:
/// - `/proc/self/cgroup` is cgroup v1 format (multiple lines)
/// - the parent path doesn't exist or isn't a cgroup
/// - we're at ns root AND either the rw remount fails (missing
///   CAP_SYS_ADMIN / seccomp-blocked mount(2) / locked flags under
///   userns) or the subsequent mkdir/move-self fails
///
/// All mean cgroup v2 isn't properly set up. Hard error — startup fails.
pub fn delegated_root() -> io::Result<PathBuf> {
    let content = fs::read_to_string("/proc/self/cgroup")?;
    let own = parse_own_cgroup(&content)?;

    // ---- cgroup namespace root: /proc/self/cgroup = "0::/" ----
    // own == /sys/fs/cgroup exactly. Containerd (k3s, kind, EKS
    // with cgroup-driver=systemd) namespaces cgroups — the
    // container sees ONLY its own subtree, rooted at
    // /sys/fs/cgroup. We're PID 1 IN that root. .parent() would
    // be /sys/fs (not a cgroup).
    //
    // Replicate systemd DelegateSubgroup=: move ourselves into a
    // leaf sub-cgroup. The ns root becomes empty → subtree
    // controllers can be enabled → per-build cgroups become
    // siblings of leaf/.
    //
    // `own == Path::new(CGROUP_ROOT)` — Path comparison handles
    // trailing slashes correctly. Don't string-compare.
    if own == Path::new(CGROUP_ROOT) {
        // Containerd mounts /sys/fs/cgroup read-only for non-
        // privileged pods, even with CAP_SYS_ADMIN. Remount rw
        // so we can create sub-cgroups and write subtree_control.
        // CAP_SYS_ADMIN + RuntimeDefault seccomp permits mount(2).
        //
        // MS_REMOUNT | MS_BIND modifies only the per-mount-point
        // flags — clears MS_RDONLY while preserving the superblock's
        // nosuid/nodev/noexec. Safe on an already-rw mount (clears
        // nothing it shouldn't). Plain MS_REMOUNT would strip those
        // flags too (harmless for cgroup2fs, but less precise) and
        // fails to clear RO if a runtime ever sets it via the
        // bind-remount path instead of superblock.
        //
        // Under privileged=true (k3s/kind escape hatch) this path
        // is unreachable — containerd mounts rw already. Under the
        // production default (privileged=false + device plugin +
        // hostUsers:false, ADR-012), the container's cgroup mount
        // may be RO and this remount is load-bearing.
        // r[impl worker.cgroup.ns-root-remount]
        nix::mount::mount(
            None::<&str>,
            CGROUP_ROOT,
            None::<&str>,
            nix::mount::MsFlags::MS_REMOUNT | nix::mount::MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| {
            io::Error::other(format!(
                "cannot remount {CGROUP_ROOT} rw: {e} \
                 (needs CAP_SYS_ADMIN and a seccomp profile permitting \
                 mount(2); also check AppArmor/SELinux if EACCES)"
            ))
        })?;

        let leaf = own.join("leaf");
        // EEXIST: prior worker crashed after mkdir before
        // move-procs. Treat as already-created (we're about to
        // move ourselves into it anyway). Any other error —
        // no perms — bubbles.
        if let Err(e) = fs::create_dir(&leaf)
            && e.kind() != io::ErrorKind::AlreadyExists
        {
            // EACCES after a successful rw remount means the ns-root
            // directory is owned by an unmapped UID (nobody:nobody
            // inside a userns). runc chowns the cgroup to the userns
            // root ONLY when the OCI spec lists /sys/fs/cgroup as rw,
            // which containerd passes only when cgroup_writable=true
            // is set on the runtime. The remount above fixes the
            // mount flag but can't fix the inode ownership — and
            // CAP_DAC_OVERRIDE doesn't apply to unmapped UIDs.
            let hint = if e.kind() == io::ErrorKind::PermissionDenied {
                " — if running with hostUsers:false, the pod cgroup is \
                 likely owned by an unmapped UID (nobody). runc only \
                 chowns it when containerd sets cgroup_writable=true on \
                 the runc runtime (v2.1+); without that, set \
                 hostUsers:true on the WorkerPool spec"
            } else {
                ""
            };
            return Err(io::Error::other(format!(
                "cannot create leaf cgroup {} in namespace root: {e}{hint}",
                leaf.display()
            )));
        }
        // Move ALL processes from the ns root into leaf/. In
        // practice "all" = us (PID 1 in container) + maybe a
        // few kernel threads that containerd didn't move. Write
        // each PID one at a time (cgroup.procs takes one PID
        // per write, EINVAL on multiple).
        //
        // Writing our own PID is enough for the no-internal-
        // processes rule (kernel threads don't count). But if
        // there ARE user threads in the root for some reason
        // (init shim?), moving them too is harmless.
        let own_pid = std::process::id();
        fs::write(leaf.join("cgroup.procs"), own_pid.to_string()).map_err(|e| {
            io::Error::other(format!(
                "cannot move self (pid {own_pid}) into leaf cgroup {}: {e}",
                leaf.display()
            ))
        })?;

        tracing::info!(
            leaf = %leaf.display(),
            "cgroup namespace root detected; moved self into leaf sub-cgroup"
        );
        // The ns root is now our delegated root. It's empty
        // (we just moved out), so enable_subtree_controllers
        // will succeed.
        return Ok(own);
    }

    // ---- Non-namespaced: systemd or pod without cgroupns ----
    // Parent. If we're in the fs root cgroup (`/sys/fs/cgroup`
    // itself), handled above. If somehow .parent() is None (can't
    // happen for /sys/fs/cgroup/..., but Path API is Option):
    let parent = own.parent().ok_or_else(|| {
        io::Error::other(
            "worker is in the root cgroup (no parent) — \
             rio-worker expects to run under systemd with Delegate=yes + \
             DelegateSubgroup=, or inside a container. \
             Running as a bare process is not supported.",
        )
    })?;

    // The parent() of /sys/fs/cgroup is /sys/fs — but the
    // equality check above catches that case before here.
    // This check is for "parent cgroup exists but isn't a
    // cgroup" — weird hybrid hierarchies.
    if !parent.join("cgroup.controllers").exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "parent of own cgroup ({}) is not a cgroup \
                 (missing cgroup.controllers). Worker's own cgroup: {}. \
                 Likely running in the root cgroup — configure systemd \
                 Delegate=yes + DelegateSubgroup= on the rio-worker unit.",
                parent.display(),
                own.display()
            ),
        ));
    }

    Ok(parent.to_path_buf())
}

/// Find the worker process's own cgroup (where `/proc/self/cgroup`
/// points). Most callers want [`delegated_root`] instead — this is
/// exposed for diagnostics/tests.
///
/// `/proc/self/cgroup` on cgroup v2 is a single line `0::/<path>`.
/// Join with `/sys/fs/cgroup` to get the filesystem path.
pub fn own_cgroup() -> io::Result<PathBuf> {
    let content = fs::read_to_string("/proc/self/cgroup")?;
    let path = parse_own_cgroup(&content)?;

    // Existence check: catches "cgroup v2 mounted but the path in
    // /proc/self/cgroup doesn't match /sys/fs/cgroup" (possible
    // under nested-container cgroupns weirdness). Better a clear
    // error here than a cryptic ENOENT later in create().
    if !path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "own cgroup path {} doesn't exist under {} — \
                 cgroup namespace mismatch or /sys/fs/cgroup not mounted",
                path.display(),
                CGROUP_ROOT
            ),
        ));
    }

    Ok(path)
}

/// Enable `+memory +cpu` on the parent's `cgroup.subtree_control`.
///
/// cgroup v2's "no internal processes" rule: a cgroup can have
/// EITHER processes OR sub-cgroups with enabled controllers, not
/// both. So we need the WORKER's cgroup to have controllers
/// enabled in subtree_control, then put the DAEMON in a SUB-
/// cgroup. The worker itself stays in its own cgroup; builds go
/// one level down.
///
/// systemd's `Delegate=yes` handles the "worker can write to its
/// own subtree_control" part — it grants ownership of the cgroup
/// subtree to the service's UID. Without it, this write fails
/// EACCES. The NixOS module (nix/modules/worker.nix) configures it.
///
/// Idempotent: writing `+memory +cpu` when they're already enabled
/// is a no-op (kernel returns success). Safe to call on every
/// startup.
///
/// **The systemd wrinkle:** if the worker's cgroup has the worker
/// process in `cgroup.procs`, enabling subtree controllers fails
/// with EBUSY (the no-internal-processes rule). systemd's
/// `DelegateSubgroup=builds` (254+) solves this by putting the
/// worker in a `/builds` sub-cgroup within its slice, leaving the
/// slice itself empty. If that's not available, the worker must
/// create a `/leaf` sub-cgroup and move ITSELF into it first —
/// that's more work than we want in the worker, so we require
/// DelegateSubgroup (or equivalent). The EBUSY error message here
/// is explicit about what's wrong.
pub fn enable_subtree_controllers(parent: &Path) -> io::Result<()> {
    let subtree = parent.join("cgroup.subtree_control");
    match fs::write(&subtree, "+memory +cpu\n") {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
            // More helpful message than bare EBUSY.
            Err(io::Error::other(format!(
                "cannot enable subtree controllers on {}: EBUSY \
                 (the worker process is in this cgroup — cgroup v2's \
                 no-internal-processes rule). Configure systemd \
                 DelegateSubgroup= on the worker unit so the worker \
                 runs in a sub-cgroup, leaving the parent empty.",
                subtree.display()
            )))
        }
        Err(e) => Err(e),
    }
}

// ----- parsers (pure fns for testability) ---------------------------------

/// Parse `/proc/self/cgroup` for cgroup v2. Expects exactly one line
/// `0::/<path>`. cgroup v1 has multiple lines with subsystem names
/// (`3:cpu:/path`) — we reject that explicitly with a clear message.
fn parse_own_cgroup(content: &str) -> io::Result<PathBuf> {
    let content = content.trim_end();
    let mut lines = content.lines();
    let first = lines.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "/proc/self/cgroup is empty (cgroup not mounted?)",
        )
    })?;

    // cgroup v2: exactly one line starting "0::". More lines = hybrid
    // or pure v1. We require pure v2.
    if lines.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "multiple lines in /proc/self/cgroup — this is cgroup v1 or \
             hybrid hierarchy. rio-worker requires pure cgroup v2 \
             (systemd.unified_cgroup_hierarchy=1 on the kernel cmdline).",
        ));
    }

    // "0::/kubepods.slice/.../rio-worker-xyz" → strip "0::"
    let rel = first.strip_prefix("0::").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected /proc/self/cgroup format: {first:?} — expected '0::/...'"),
        )
    })?;

    // Join with CGROUP_ROOT. rel starts with "/" (it's an absolute
    // path within the cgroup hierarchy). `Path::join` on an absolute
    // rhs DISCARDS the base — so strip the leading "/" first.
    // This is a common footgun (Path::join("/sys", "/foo") = "/foo").
    let rel = rel.strip_prefix('/').unwrap_or(rel);
    Ok(PathBuf::from(CGROUP_ROOT).join(rel))
}

/// Parse `cpu.stat` for the `usage_usec` line. First whitespace-
/// separated token after the key.
///
/// `pub(crate)`: the executor's CPU poll task reads `cpu.stat`
/// directly by path (cloned from `BuildCgroup::path()`) and calls
/// this to parse. It can't hold `&BuildCgroup` across the
/// `run_daemon_build` await; exposing the parser is the simplest fix.
pub(crate) fn parse_cpu_stat_usage_usec(content: &str) -> Option<u64> {
    content
        .lines()
        .find(|l| l.starts_with("usage_usec "))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

/// Read a single-line decimal u64 file. cgroup v2 `memory.peak`,
/// `memory.current`, etc. all use this format: `"12345\n"`.
fn read_single_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn cpu_fraction(delta_usec: u64, wall_usec: u64) -> f64 {
    if wall_usec > 0 {
        delta_usec as f64 / wall_usec as f64
    } else {
        0.0
    }
}

/// `max = None` (cgroup `memory.max = "max"`) → 0.0 per obs spec.
fn mem_fraction(current: u64, max: Option<u64>) -> f64 {
    match max {
        Some(m) if m > 0 => current as f64 / m as f64,
        _ => 0.0,
    }
}

/// Snapshot of the worker's whole-tree resource usage, published by
/// [`utilization_reporter_loop`] and read by the heartbeat loop.
///
/// `cpu_fraction` is cores-equivalent (1.0 = one core fully busy; >1.0
/// on multi-core). `memory_max_bytes` is the cgroup `memory.max` limit
/// — `None` means unbounded ("max"), in which case the proto
/// `memory_total_bytes` is sent as 0 (the scheduler treats 0 as
/// "unknown ceiling"; it won't compute a fraction from 0).
///
/// Disk fields: statvfs on `overlay_base_dir`. This is where per-build
/// overlay upper dirs accumulate — the relevant quota for "can this
/// worker accept another build." Not the FUSE cache dir (that's LRU-
/// bounded separately; see `r[worker.fuse.cache-lru]`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceSnapshot {
    pub cpu_fraction: f64,
    pub memory_current_bytes: u64,
    pub memory_max_bytes: Option<u64>,
    pub disk_used_bytes: u64,
    pub disk_total_bytes: u64,
}

impl ResourceSnapshot {
    /// Convert to the proto `ResourceUsage`. `running_builds` /
    /// `available_build_slots` are zero here — the heartbeat caller
    /// overwrites them (it has `max_builds` and the running set; the
    /// cgroup sampler doesn't).
    pub fn to_proto(self) -> rio_proto::types::ResourceUsage {
        rio_proto::types::ResourceUsage {
            cpu_fraction: self.cpu_fraction,
            memory_used_bytes: self.memory_current_bytes,
            memory_total_bytes: self.memory_max_bytes.unwrap_or(0),
            disk_used_bytes: self.disk_used_bytes,
            disk_total_bytes: self.disk_total_bytes,
            running_builds: 0,
            available_build_slots: 0,
        }
    }
}

/// Shared handle: heartbeat loop reads, sampler writes. `std::sync::RwLock`
/// — no async needed, reads are non-blocking and the critical section is
/// a `Copy` struct store. Poisoned lock (panic inside sampler while
/// holding write) → `into_inner()` recovers; the snapshot is Copy so a
/// torn write can't happen.
pub type ResourceSnapshotHandle = std::sync::Arc<std::sync::RwLock<ResourceSnapshot>>;

/// statvfs on the overlay base dir. `f_blocks * f_frsize` = total;
/// `(f_blocks - f_bavail) * f_frsize` = used. `f_bavail` (not
/// `f_bfree`) because bfree includes root-reserved blocks — the
/// worker runs unprivileged, so bavail is the real ceiling.
///
/// ENOENT (overlay_base_dir not yet created on a fresh worker) →
/// (0, 0). The scheduler reads both-zero as "unknown" and doesn't
/// penalize placement.
fn sample_disk(overlay_base: &Path) -> (u64, u64) {
    match nix::sys::statvfs::statvfs(overlay_base) {
        Ok(s) => {
            // fsblkcnt_t / c_ulong → u64. On Linux x86_64 both are
            // already u64 (clippy flags the cast as unnecessary), but
            // they're typedef'd to 32-bit on some targets. Keep the
            // casts for portability; silence the lint locally.
            #[allow(clippy::unnecessary_cast)]
            let frsize = s.fragment_size() as u64;
            #[allow(clippy::unnecessary_cast)]
            let blocks = s.blocks() as u64;
            #[allow(clippy::unnecessary_cast)]
            let bavail = s.blocks_available() as u64;
            let total = blocks.saturating_mul(frsize);
            let used = blocks.saturating_sub(bavail).saturating_mul(frsize);
            (used, total)
        }
        Err(_) => (0, 0),
    }
}

/// Background task that polls the worker's parent cgroup for CPU and
/// memory utilization, emitting `rio_worker_cpu_fraction` and
/// `rio_worker_memory_fraction` gauges every 10s, and publishes a
/// [`ResourceSnapshot`] for the heartbeat loop's `ResourceUsage`.
///
/// `root` is the PARENT cgroup (what `delegated_root()` returns) —
/// this captures the whole worker's tree: rio-worker process + all
/// per-build sub-cgroups + all nix-daemon subprocesses.
///
/// CPU fraction: delta `cpu.stat usage_usec` / interval µs. 1.0 = one
/// core fully utilized; >1.0 on multi-core. Directly comparable to
/// cgroup `cpu.max` limits.
///
/// Memory fraction: `memory.current` / `memory.max`. If `memory.max`
/// is `"max"` (unbounded — no cgroup memory limit), the fraction is
/// reported as 0.0 (gauge stays at default; can't compute saturation
/// without a limit).
///
/// Spawned from `main.rs` after `delegated_root()` returns. If the
/// cgroup files become unreadable mid-run (rare — would indicate the
/// cgroup was removed out from under us), the gauge simply stops
/// updating; no crash.
// r[impl obs.metric.worker-util]
pub async fn utilization_reporter_loop(
    root: PathBuf,
    overlay_base: PathBuf,
    snapshot: ResourceSnapshotHandle,
) {
    // 10s: matches HEARTBEAT_INTERVAL. The heartbeat reads the shared
    // snapshot; a 15s poll would mean every third heartbeat sees stale
    // data. 10s keeps Prometheus gauges and ListWorkers in lockstep.
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
    let cpu_stat_path = root.join("cpu.stat");
    let mem_current_path = root.join("memory.current");
    let mem_max_path = root.join("memory.max");

    let mut last_usage_usec = fs::read_to_string(&cpu_stat_path)
        .ok()
        .and_then(|c| parse_cpu_stat_usage_usec(&c));
    let mut last_instant = std::time::Instant::now();

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        let now_usage = fs::read_to_string(&cpu_stat_path)
            .ok()
            .and_then(|c| parse_cpu_stat_usage_usec(&c));
        let now_instant = std::time::Instant::now();

        let mem_current = read_single_u64(&mem_current_path);
        let mem_max = fs::read_to_string(&mem_max_path).ok().and_then(|s| {
            let s = s.trim();
            if s == "max" {
                None
            } else {
                s.parse::<u64>().ok()
            }
        });

        let (disk_used, disk_total) = sample_disk(&overlay_base);

        // CPU: only set the gauge if we have BOTH a previous sample and
        // a new reading. Snapshot cpu_fraction is 0.0 on the first tick
        // (no delta yet) — same as today's ResourceUsage::default().
        let cpu_frac = if let (Some(last_usage), Some(now_usage)) = (last_usage_usec, now_usage) {
            let cpu_delta = now_usage.saturating_sub(last_usage);
            let wall_delta = now_instant.duration_since(last_instant).as_micros() as u64;
            let frac = cpu_fraction(cpu_delta, wall_delta);
            metrics::gauge!("rio_worker_cpu_fraction").set(frac);
            frac
        } else {
            0.0
        };
        if let Some(nu) = now_usage {
            last_usage_usec = Some(nu);
            last_instant = now_instant;
        }

        // Memory: always set if memory.current is readable (explicit 0.0
        // on unbounded max per obs spec — avoids stale-gauge persistence).
        if let Some(current) = mem_current {
            metrics::gauge!("rio_worker_memory_fraction").set(mem_fraction(current, mem_max));
        }

        // Publish snapshot. Heartbeat reads this; it's always one poll
        // behind reality (up to 10s stale), which is fine for placement.
        // unwrap_or_else(into_inner): a poisoned RwLock here means the
        // sampler itself panicked previously — recover and keep writing.
        *snapshot.write().unwrap_or_else(|e| e.into_inner()) = ResourceSnapshot {
            cpu_fraction: cpu_frac,
            memory_current_bytes: mem_current.unwrap_or(0),
            memory_max_bytes: mem_max,
            disk_used_bytes: disk_used,
            disk_total_bytes: disk_total,
        };
    }
}

// Need libc for EBUSY. Worker already has `nix` dep but libc is lighter.
use nix::libc;

// r[verify worker.cgroup.sibling-layout]
// r[verify worker.cgroup.memory-peak]
#[cfg(test)]
mod tests {
    use super::*;

    // ---- parsers (pure, no filesystem) ------------------------------------

    #[test]
    fn parse_own_cgroup_v2_single_line() {
        let content = "0::/kubepods.slice/kubepods-burstable.slice/pod-abc/worker\n";
        let path = parse_own_cgroup(content).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/sys/fs/cgroup/kubepods.slice/kubepods-burstable.slice/pod-abc/worker"),
            "strips '0::' AND the leading '/' (Path::join footgun)"
        );
    }

    #[test]
    fn parse_own_cgroup_v1_rejected() {
        // cgroup v1: multiple lines with subsystem names. Pure v2
        // has exactly one line "0::...". Hybrid has both.
        let content = "12:cpu,cpuacct:/foo\n11:memory:/foo\n0::/foo\n";
        let err = parse_own_cgroup(content).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(
            err.to_string().contains("cgroup v1"),
            "error message explains the problem: {err}"
        );
    }

    #[test]
    fn parse_own_cgroup_root() {
        // Running in the root cgroup (unusual but valid — e.g.,
        // the worker launched outside systemd).
        let content = "0::/\n";
        let path = parse_own_cgroup(content).unwrap();
        assert_eq!(path, PathBuf::from("/sys/fs/cgroup"));
    }

    #[test]
    fn parse_own_cgroup_empty_rejected() {
        let err = parse_own_cgroup("").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn parse_own_cgroup_malformed_rejected() {
        // Missing "0::" prefix — some other format we don't understand.
        let err = parse_own_cgroup("weird:/foo\n").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("0::/"));
    }

    #[test]
    fn parse_cpu_stat_usage_usec_happy() {
        let content = "usage_usec 123456789\n\
                       user_usec 100000000\n\
                       system_usec 23456789\n\
                       nr_periods 0\n";
        assert_eq!(parse_cpu_stat_usage_usec(content), Some(123_456_789));
    }

    #[test]
    fn parse_cpu_stat_usage_usec_missing() {
        // cpu controller not enabled → file exists but no usage line.
        // (In practice enable_subtree_controllers would have failed
        // first, but defensive.)
        let content = "nr_periods 0\nnr_throttled 0\n";
        assert_eq!(parse_cpu_stat_usage_usec(content), None);
    }

    #[test]
    fn parse_cpu_stat_key_prefix_not_fooled() {
        // "usage_usec_foo" should NOT match "usage_usec " (space-
        // delimited). starts_with("usage_usec ") with trailing space
        // catches this.
        let content = "usage_usec_something_else 999\nusage_usec 42\n";
        assert_eq!(
            parse_cpu_stat_usage_usec(content),
            Some(42),
            "space-delimited key match, not prefix match"
        );
    }

    // r[verify obs.metric.worker-util]
    #[test]
    fn cpu_fraction_half_core() {
        // 500ms cpu usage over 1s wall → 0.5 (half a core).
        assert_eq!(cpu_fraction(500_000, 1_000_000), 0.5);
    }

    #[test]
    fn cpu_fraction_zero_wall_guard() {
        // Div-by-zero guard: zero wall → 0.0 (not NaN/inf).
        assert_eq!(cpu_fraction(500, 0), 0.0);
    }

    #[test]
    fn mem_fraction_half() {
        assert_eq!(mem_fraction(512, Some(1024)), 0.5);
    }

    #[test]
    fn mem_fraction_unbounded_is_zero() {
        // memory.max = "max" (None) → 0.0 per obs spec.
        assert_eq!(mem_fraction(999_999, None), 0.0);
    }

    #[test]
    fn mem_fraction_zero_max_guard() {
        // Div-by-zero guard: max=0 → 0.0 (not NaN/inf).
        assert_eq!(mem_fraction(999, Some(0)), 0.0);
    }

    // ---- filesystem (real /proc, real cgroup — Linux-only) ----------------

    /// Sanity: our own /proc/self/cgroup parses on this system.
    ///
    /// This is the "does the real kernel format match what we expect"
    /// canary. If the kernel changes the format (unlikely — it's ABI),
    /// this fails before any build attempt.
    ///
    /// Requires cgroup v2. VM tests + CI runners have it. If running
    /// locally on a cgroup v1 system: this test will fail, which is
    /// CORRECT — the worker wouldn't work there either.
    #[test]
    #[cfg(target_os = "linux")]
    fn own_cgroup_parses_on_this_system() {
        match own_cgroup() {
            Ok(path) => {
                assert!(
                    path.starts_with(CGROUP_ROOT),
                    "should be under {}: {}",
                    CGROUP_ROOT,
                    path.display()
                );
                assert!(path.exists(), "our own cgroup should exist");
                // Verify cgroup.controllers exists (proves it's
                // actually a cgroup, not a random directory).
                assert!(
                    path.join("cgroup.controllers").exists(),
                    "missing cgroup.controllers — {} is not a cgroup?",
                    path.display()
                );
            }
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                // cgroup v1 system. The worker wouldn't start here
                // in production either. Don't fail the test suite
                // on developer machines with old kernels — panic
                // with a clear message so it's obvious what happened.
                panic!(
                    "this system is cgroup v1 (not supported by rio-worker). \
                     To run these tests locally, boot with \
                     systemd.unified_cgroup_hierarchy=1. Error: {e}"
                );
            }
            Err(e) => panic!("own_cgroup failed unexpectedly: {e}"),
        }
    }

    /// memory.peak parsing against a synthetic file (not a real
    /// cgroup — that needs delegation which we can't assume in CI).
    #[test]
    fn read_single_u64_from_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.peak");
        fs::write(&path, "123456789\n").unwrap();
        assert_eq!(read_single_u64(&path), Some(123_456_789));

        // Trailing garbage → parse fail → None
        fs::write(&path, "123 kB\n").unwrap();
        assert_eq!(
            read_single_u64(&path),
            None,
            "not 'VmHWM: 123 kB' format — memory.peak is raw bytes"
        );

        // Missing file → None
        assert_eq!(read_single_u64(&dir.path().join("nope")), None);
    }
}

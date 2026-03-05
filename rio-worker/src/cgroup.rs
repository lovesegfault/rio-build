//! Per-build cgroup v2 resource tracking.
//!
//! Fixes the phase2c VmHWM bug: `read_vmhwm_bytes(daemon.id())`
//! measured nix-daemon's RSS (~10MB) because nix-daemon FORKS the
//! builder and waitpid()s — the builder's memory never appeared in
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
//! set up (systemd `Delegate=yes` on the worker unit — H1 configures
//! this). The worker's main.rs propagates both with `?` — startup
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
    /// Format (cpu.stat excerpt):
    /// ```text
    /// usage_usec 123456789
    /// user_usec 100000000
    /// system_usec 23456789
    /// ...
    /// ```
    pub fn cpu_usage_usec(&self) -> Option<u64> {
        let content = fs::read_to_string(self.path.join("cpu.stat")).ok()?;
        parse_cpu_stat_usage_usec(&content)
    }

    /// Path to this cgroup. Exposed so the CPU polling task can
    /// clone it (the task outlives the borrowed `&BuildCgroup`
    /// across the `run_daemon_build` await).
    pub fn path(&self) -> &Path {
        &self.path
    }
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

/// Find the worker process's own cgroup.
///
/// `/proc/self/cgroup` on cgroup v2 is a single line `0::/<path>`.
/// Join with `/sys/fs/cgroup` to get the filesystem path.
///
/// Fails if:
/// - the file format is cgroup v1 (multiple lines with subsystems)
/// - `/sys/fs/cgroup/<path>` doesn't exist (mount issue)
///
/// Both mean cgroup v2 isn't properly set up. Hard error — the
/// worker main.rs propagates with `?` and startup fails.
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
/// EACCES. H1 configures it in the NixOS module.
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
/// `run_daemon_build` await, so `BuildCgroup::cpu_usage_usec()`
/// doesn't work for it. Exposing the parser is the simplest fix.
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

// Need libc for EBUSY. Worker already has `nix` dep but libc is lighter.
use nix::libc;

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

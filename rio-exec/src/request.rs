//! The execution request: a complete, concrete description of a process
//! and the sandbox it runs in.
//!
//! Everything in an [`ExecutionRequest`] is fully resolved by the
//! caller — no placeholders, no lazy lookups, no defaults filled in by
//! the executor. [`ExecutionRequest::validate`] is the boundary check
//! that rejects requests the sandbox builder cannot act on safely.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::ExecError;

/// A fully-resolved, build-system-agnostic execution request.
///
/// Everything is concrete: no placeholders, no lazy lookups. The
/// executor adds nothing to the environment, synthesizes no argv
/// entries, and infers no mounts.
#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    /// The path passed to `execve(2)`, resolved inside the sandbox's
    /// filesystem view. argv is [`args`](Self::args) verbatim — the
    /// executor does not synthesize `argv[0]` from this.
    pub program: PathBuf,
    /// The COMPLETE argv, including `args[0]`. Callers that want the
    /// conventional `argv[0] = basename(program)` set it themselves.
    pub args: Vec<OsString>,
    /// The COMPLETE environment. The executor adds nothing.
    pub env: Vec<(OsString, OsString)>,
    /// Working directory inside the sandbox. The child `chdir(2)`s here
    /// after `pivot_root`, before exec, so it must fall under one of
    /// [`mounts`](Self::mounts) — anything else lands in the empty
    /// chroot skeleton.
    pub cwd: PathBuf,
    /// All bind mounts, writable and read-only. The executor applies
    /// them sorted by target path, parents before children, so nesting
    /// is order-independent for callers (a writable parent directory
    /// with read-only entries bind-mounted inside it is the expected
    /// shape). Duplicate targets are an
    /// [`InvalidRequest`](ExecError::InvalidRequest) error.
    pub mounts: Vec<Mount>,
    /// Character devices to bind from the host into the sandbox's
    /// `/dev` beyond the standard set (e.g. `/dev/kvm`). The caller
    /// derives this from whatever capability negotiation it does; the
    /// executor only checks the device exists on the host.
    pub extra_devices: Vec<PathBuf>,
    /// Small files written into the sandbox before exec. Each path must
    /// fall under a writable mount (the executor writes them through
    /// the mount's host-side source); ownership is the sandbox uid/gid.
    pub inline_files: Vec<InlineFile>,
    /// Paths (sandbox-absolute) the caller wants reported on after
    /// exit. The executor `lstat(2)`s each through the host-side view
    /// of the writable mount it falls under and reports existence +
    /// metadata. It does NOT judge whether a missing path is an error —
    /// that is build-system policy.
    pub declared_outputs: Vec<PathBuf>,
    /// How the child's stdout/stderr are captured.
    pub capture: OutputCapture,
    /// Namespace, identity, and hardening parameters.
    pub isolation: Isolation,
    /// Wall-clock, silence, log-volume, and cgroup limits.
    pub limits: Limits,
}

/// A single bind mount in the sandbox's filesystem view.
#[derive(Debug, Clone)]
pub struct Mount {
    /// Host-side path to bind from.
    pub source: PathBuf,
    /// Sandbox-absolute path to bind to.
    pub target: PathBuf,
    /// Read-write when true; bind-then-remount-read-only when false.
    ///
    /// The read-only remount covers the top of the bind only: a source
    /// that itself contains other mount points would carry them into
    /// the sandbox still writable. Callers must therefore not pass
    /// sources containing submounts as read-only mounts.
    pub writable: bool,
    /// Skip this mount silently if `source` does not exist on the host.
    pub optional: bool,
}

/// A small file materialized into the sandbox before exec.
///
/// `Debug` elides the contents (they can carry caller secrets such as
/// credential files; only the length is printed).
#[derive(Clone)]
pub struct InlineFile {
    /// Sandbox-absolute path to create. Must fall under a writable
    /// mount's target.
    pub path: PathBuf,
    /// Raw file contents.
    pub contents: Vec<u8>,
    /// Permission bits (e.g. `0o600`).
    pub mode: u32,
}

impl fmt::Debug for InlineFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineFile")
            .field("path", &self.path)
            .field("contents", &format_args!("<{} bytes>", self.contents.len()))
            .field("mode", &format_args!("{:#o}", self.mode))
            .finish()
    }
}

/// How the child's output is captured. Per-build-system policy: some
/// callers want their processes to observe `isatty(1) == isatty(2) ==
/// true` and a single merged raw-mode stream; others require separate
/// stdout/stderr capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputCapture {
    /// One pty slave `dup2`'d onto fds 1 and 2. Lines arrive as
    /// [`LogStream::Merged`](crate::LogStream::Merged).
    MergedPty,
    /// Two pipes. Lines arrive tagged
    /// [`Stdout`](crate::LogStream::Stdout) /
    /// [`Stderr`](crate::LogStream::Stderr).
    SeparatePipes,
}

/// Namespace, identity, and hardening parameters for the sandbox.
#[derive(Debug, Clone)]
pub struct Isolation {
    /// When false the sandbox gets its own (empty but for loopback)
    /// network namespace. When true `CLONE_NEWNET` is omitted and the
    /// process shares the executor's network; the plan then also binds
    /// the host's `/etc/resolv.conf`, `/etc/services`, and `/etc/hosts`
    /// (and synthesizes an `nsswitch.conf`) so name resolution works.
    /// Trust-store material (the CA bundle) is NOT inferred — that bind
    /// stays with the caller.
    pub network: bool,
    /// uid the process runs as inside the sandbox. Without a user
    /// namespace this is a host uid — a singleton identity: two
    /// concurrent executions under the same uid could observe and
    /// signal each other's processes. The executor itself performs
    /// **no** concurrency control; serializing executions is the
    /// caller's responsibility (rio-builder's `BuildSlot` enforces one
    /// build per pod — see the executor module docs' caller contract).
    pub uid: u32,
    /// gid the process runs as inside the sandbox.
    pub gid: u32,
    /// Architecture personality applied before exec.
    pub personality: Personality,
    /// Hostname (and domainname) inside the UTS namespace.
    pub hostname: String,
    /// Install the multi-ABI seccomp filter that denies setuid/setgid
    /// mode bits and xattr manipulation. Named for the policy rather
    /// than the mechanism because that is the only policy it installs.
    pub deny_setuid_and_xattrs: bool,
}

/// Architecture personality applied to the child before exec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Personality {
    /// No `personality(2)` change.
    Native,
    /// `PER_LINUX32` (fatal if it fails) so `uname -m` reports the
    /// 32-bit machine, plus `ADDR_NO_RANDOMIZE` on a best-effort basis
    /// (some seccomp profiles block it; the executor logs and
    /// continues).
    Linux32,
}

/// Wall-clock, silence, log-volume, and cgroup limits.
///
/// The executor *enforces* these (they are execution mechanics) but
/// does not decide what exceeding them *means* — the corresponding
/// [`ExitOutcome`](crate::ExitOutcome) variants carry the fact to the
/// caller, which owns the policy.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Absolute wall-clock deadline for the whole execution.
    pub timeout: Option<Duration>,
    /// Kill the process tree if no output is captured for this long.
    /// Reset on every captured line.
    pub max_silent: Option<Duration>,
    /// Abort with
    /// [`LogLimitExceeded`](crate::ExitOutcome::LogLimitExceeded) after
    /// this many captured bytes.
    pub max_log_bytes: Option<u64>,
    /// Existing cgroup directory to attach the child to before it
    /// proceeds past fork. The executor does not create or destroy the
    /// cgroup; it only writes the child pid into `cgroup.procs`.
    pub cgroup: Option<PathBuf>,
}

impl ExecutionRequest {
    /// Validate the request at the executor boundary.
    ///
    /// Rejects anything the sandbox builder cannot act on safely:
    /// relative or lexically-unclean paths (the mount and output
    /// machinery prefix-matches sandbox paths against mount targets,
    /// which is only sound on clean absolute paths), duplicate mount
    /// targets, files or outputs that fall under no writable mount,
    /// an empty argv, and strings that cannot become C strings.
    ///
    /// Every rejection names the offending path or field.
    pub fn validate(&self) -> Result<(), ExecError> {
        let invalid = |msg: String| Err(ExecError::InvalidRequest(msg));

        // Rule 1+2: absoluteness and lexical cleanliness.
        //
        // `program`, mount sources, and extra devices only need to be
        // absolute: program is resolved by execve inside the chroot
        // and never prefix-matched; sources and devices are host paths
        // the caller already resolved (a `..` in a host path is legal)
        // but a *relative* one would silently resolve against the
        // executor's cwd at mount(2) time. `cwd`, mount targets,
        // inline-file paths, and declared outputs are all
        // prefix-matched against mount targets below, so they must
        // also be lexically clean — `/a/../b`.starts_with(`/a`) is
        // true component-wise while the path actually escapes `/a`.
        if !self.program.is_absolute() {
            return invalid(format!(
                "program path must be absolute: {}",
                self.program.display()
            ));
        }
        if !is_clean_absolute(&self.cwd) {
            return invalid(format!(
                "cwd must be an absolute, lexically clean path (no `..`, `.`, or empty \
                 components): {}",
                self.cwd.display()
            ));
        }
        for m in &self.mounts {
            if !is_clean_absolute(&m.target) {
                return invalid(format!(
                    "mount target must be an absolute, lexically clean path (no `..`, `.`, or \
                     empty components): {}",
                    m.target.display()
                ));
            }
            if !m.source.is_absolute() {
                return invalid(format!(
                    "mount source must be absolute: {}",
                    m.source.display()
                ));
            }
        }
        for d in &self.extra_devices {
            if !d.is_absolute() {
                return invalid(format!(
                    "extra device path must be absolute: {}",
                    d.display()
                ));
            }
        }
        for f in &self.inline_files {
            if !is_clean_absolute(&f.path) {
                return invalid(format!(
                    "inline file path must be an absolute, lexically clean path (no `..`, `.`, \
                     or empty components): {}",
                    f.path.display()
                ));
            }
        }
        for o in &self.declared_outputs {
            if !is_clean_absolute(o) {
                return invalid(format!(
                    "declared output must be an absolute, lexically clean path (no `..`, `.`, \
                     or empty components): {}",
                    o.display()
                ));
            }
        }

        // Rule 3: no duplicate mount targets. PathBuf's Eq compares
        // components, so `/a//b` and `/a/b` would collide here — but
        // both are already rejected by the cleanliness check above, so
        // a plain pairwise comparison over the (small) mount list is
        // exact.
        for (i, m) in self.mounts.iter().enumerate() {
            if self.mounts[..i].iter().any(|p| p.target == m.target) {
                return invalid(format!("duplicate mount target: {}", m.target.display()));
            }
        }

        // Rule 5: argv[0] is required.
        if self.args.is_empty() {
            return invalid("args must be non-empty (argv[0] is required)".to_string());
        }

        // Rule 6: everything that becomes a C string at execve must
        // not contain NUL, and the hostname must be present.
        if self.isolation.hostname.is_empty() {
            return invalid("isolation.hostname must be non-empty".to_string());
        }
        if self.isolation.hostname.as_bytes().contains(&0) {
            return invalid("isolation.hostname must not contain NUL bytes".to_string());
        }
        if has_nul(self.program.as_os_str()) {
            return invalid(format!(
                "program path must not contain NUL bytes: {}",
                self.program.display()
            ));
        }
        for (i, a) in self.args.iter().enumerate() {
            if has_nul(a) {
                return invalid(format!("args[{i}] must not contain NUL bytes"));
            }
        }
        for (k, v) in &self.env {
            if has_nul(k) || has_nul(v) {
                return invalid(format!(
                    "env entry must not contain NUL bytes: {}",
                    k.display()
                ));
            }
            // execve takes the environment as `KEY=VALUE` C strings:
            // an empty key or a `=` inside the key cannot round-trip.
            if k.is_empty() {
                return invalid("env key must not be empty".to_string());
            }
            if k.as_bytes().contains(&b'=') {
                return invalid(format!("env key must not contain `=`: {}", k.display()));
            }
        }

        // Rule 4: inline files and declared outputs must be reachable
        // through a writable mount — the executor writes the former
        // and lstats the latter through the mount's host-side source.
        for f in &self.inline_files {
            if self.writable_mount_for(&f.path).is_none() {
                return invalid(format!(
                    "inline file path is not under any writable mount target: {}",
                    f.path.display()
                ));
            }
        }
        for o in &self.declared_outputs {
            if self.writable_mount_for(o).is_none() {
                return invalid(format!(
                    "declared output is not under any writable mount target: {}",
                    o.display()
                ));
            }
        }

        // Rule 7: cwd must resolve to *something* after pivot_root.
        // The chroot skeleton contains only mount points, so a cwd
        // outside every mount is an empty directory at best and ENOENT
        // at worst.
        if !self.mounts.iter().any(|m| self.cwd.starts_with(&m.target)) {
            return invalid(format!(
                "cwd is not under any mount target: {}",
                self.cwd.display()
            ));
        }

        Ok(())
    }

    /// The most specific (longest-prefix) writable mount whose target
    /// contains `path`, if any.
    ///
    /// Used by validation and by the sandbox builder to translate a
    /// sandbox-absolute path into the host-side location it can be
    /// written to (inline files) or read from (declared outputs).
    pub(crate) fn writable_mount_for(&self, path: &Path) -> Option<&Mount> {
        self.mounts
            .iter()
            .filter(|m| m.writable && path.starts_with(&m.target))
            .max_by_key(|m| m.target.components().count())
    }
}

/// True when `p` is absolute and lexically clean: no `..` components,
/// no `.` components, no empty components (`/a//b`, trailing `/`).
///
/// Purely lexical — never touches the filesystem, because sandbox-side
/// paths generally do not exist on the host. `Path::components()`
/// silently normalizes `//`, `/./`, and a trailing `/` away, so empty
/// and `.` components have to be caught on the raw bytes before the
/// component walk. `..` components survive normalization and are caught
/// by the component walk itself.
fn is_clean_absolute(p: &Path) -> bool {
    if !p.is_absolute() {
        return false;
    }
    let bytes = p.as_os_str().as_bytes();
    // Empty components: `//` anywhere, or a trailing `/` (other than
    // the root path itself).
    if bytes.windows(2).any(|w| w == b"//") {
        return false;
    }
    if bytes.len() > 1 && bytes.ends_with(b"/") {
        return false;
    }
    // `.` components: `/./` in the middle or `/.` at the end. (A
    // leading `.` in a file name — `/work/.config` — is not a `.`
    // component and stays allowed.)
    if bytes.windows(3).any(|w| w == b"/./") || bytes.ends_with(b"/.") {
        return false;
    }
    p.components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// True when the OS string contains a NUL byte anywhere (it could not
/// be passed to `execve(2)` as a C string).
fn has_nul(s: &OsStr) -> bool {
    s.as_bytes().contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative request that passes validation. Each rejection
    /// test below mutates exactly one aspect of it.
    fn minimal_valid_request() -> ExecutionRequest {
        ExecutionRequest {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                OsString::from("sh"),
                OsString::from("-c"),
                OsString::from("true"),
            ],
            env: vec![(OsString::from("PATH"), OsString::from("/path-not-set"))],
            cwd: PathBuf::from("/work/build"),
            mounts: vec![
                Mount {
                    source: PathBuf::from("/host/scratch/build"),
                    target: PathBuf::from("/work/build"),
                    writable: true,
                    optional: false,
                },
                Mount {
                    source: PathBuf::from("/host/scratch/outputs"),
                    target: PathBuf::from("/work/outputs"),
                    writable: true,
                    optional: false,
                },
                Mount {
                    source: PathBuf::from("/host/inputs/tool"),
                    target: PathBuf::from("/work/outputs/tool"),
                    writable: false,
                    optional: false,
                },
                Mount {
                    source: PathBuf::from("/host/sh"),
                    target: PathBuf::from("/bin/sh"),
                    writable: false,
                    optional: false,
                },
            ],
            extra_devices: vec![],
            inline_files: vec![InlineFile {
                path: PathBuf::from("/work/build/.config"),
                contents: b"key=value\n".to_vec(),
                mode: 0o600,
            }],
            declared_outputs: vec![PathBuf::from("/work/outputs/result")],
            capture: OutputCapture::MergedPty,
            isolation: Isolation {
                network: false,
                uid: 1000,
                gid: 100,
                personality: Personality::Native,
                hostname: String::from("localhost"),
                deny_setuid_and_xattrs: true,
            },
            limits: Limits {
                timeout: Some(Duration::from_secs(3600)),
                max_silent: Some(Duration::from_secs(600)),
                max_log_bytes: Some(64 * 1024 * 1024),
                cgroup: None,
            },
        }
    }

    /// Assert that validation fails and the error message contains
    /// `needle` (the offending path or field).
    #[track_caller]
    fn assert_rejected(req: &ExecutionRequest, needle: &str) {
        let err = req.validate().expect_err("request should be rejected");
        let ExecError::InvalidRequest(msg) = err else {
            panic!("validation failures must be InvalidRequest, got {err:?}");
        };
        assert!(
            msg.contains(needle),
            "error message should name the offending path: expected {needle:?} in {msg:?}"
        );
    }

    #[test]
    fn accepts_representative_request() {
        minimal_valid_request().validate().expect("should validate");
    }

    // Rule 1: absoluteness.

    #[test]
    fn rejects_relative_program() {
        let mut req = minimal_valid_request();
        req.program = PathBuf::from("bin/sh");
        assert_rejected(&req, "bin/sh");
    }

    #[test]
    fn rejects_relative_mount_target() {
        let mut req = minimal_valid_request();
        req.mounts[0].target = PathBuf::from("work/build");
        assert_rejected(&req, "work/build");
    }

    #[test]
    fn rejects_relative_mount_source() {
        let mut req = minimal_valid_request();
        req.mounts[0].source = PathBuf::from("host/scratch/build");
        assert_rejected(&req, "host/scratch/build");
    }

    #[test]
    fn rejects_relative_extra_device() {
        let mut req = minimal_valid_request();
        req.extra_devices.push(PathBuf::from("dev/kvm"));
        assert_rejected(&req, "dev/kvm");
    }

    // Rule 2: lexical cleanliness.

    #[test]
    fn rejects_dotdot_in_mount_target() {
        let mut req = minimal_valid_request();
        req.mounts[0].target = PathBuf::from("/work/../escape");
        assert_rejected(&req, "/work/../escape");
    }

    #[test]
    fn rejects_curdir_in_inline_file_path() {
        let mut req = minimal_valid_request();
        req.inline_files[0].path = PathBuf::from("/work/build/./x");
        assert_rejected(&req, "/work/build/./x");
    }

    #[test]
    fn rejects_empty_component_in_declared_output() {
        let mut req = minimal_valid_request();
        req.declared_outputs[0] = PathBuf::from("/work/outputs//result");
        assert_rejected(&req, "/work/outputs//result");
    }

    #[test]
    fn rejects_trailing_slash_in_mount_target() {
        let mut req = minimal_valid_request();
        req.mounts[0].target = PathBuf::from("/work/build/");
        assert_rejected(&req, "/work/build/");
    }

    #[test]
    fn rejects_trailing_curdir_in_mount_target() {
        let mut req = minimal_valid_request();
        req.mounts[0].target = PathBuf::from("/work/build/.");
        assert_rejected(&req, "/work/build/.");
    }

    #[test]
    fn rejects_dotdot_cwd_that_would_pass_prefix_match() {
        // `/work/build/../../etc` starts_with `/work/build` component-
        // wise, so the cleanliness check is what keeps the rule-7
        // prefix match sound.
        let mut req = minimal_valid_request();
        req.cwd = PathBuf::from("/work/build/../../etc");
        assert_rejected(&req, "/work/build/../../etc");
    }

    // Rule 3: duplicate mount targets.

    #[test]
    fn rejects_duplicate_mount_targets() {
        let mut req = minimal_valid_request();
        let dup = req.mounts[0].clone();
        req.mounts.push(dup);
        assert_rejected(&req, "/work/build");
    }

    // Rule 4: inline files / declared outputs must be under a writable
    // mount.

    #[test]
    fn rejects_inline_file_outside_any_mount() {
        let mut req = minimal_valid_request();
        req.inline_files[0].path = PathBuf::from("/elsewhere/file");
        assert_rejected(&req, "/elsewhere/file");
    }

    #[test]
    fn rejects_declared_output_under_readonly_mount_only() {
        let mut req = minimal_valid_request();
        // `/bin/sh` is a mount target, but a read-only one.
        req.declared_outputs.push(PathBuf::from("/bin/sh"));
        assert_rejected(&req, "/bin/sh");
    }

    // Rule 5: argv must be non-empty.

    #[test]
    fn rejects_empty_args() {
        let mut req = minimal_valid_request();
        req.args.clear();
        assert_rejected(&req, "argv[0]");
    }

    // Rule 6: NUL-freedom and hostname presence.

    #[test]
    fn rejects_empty_hostname() {
        let mut req = minimal_valid_request();
        req.isolation.hostname = String::new();
        assert_rejected(&req, "hostname");
    }

    #[test]
    fn rejects_nul_in_program() {
        let mut req = minimal_valid_request();
        req.program = PathBuf::from("/bin/s\0h");
        assert_rejected(&req, "NUL");
    }

    #[test]
    fn rejects_nul_in_arg() {
        let mut req = minimal_valid_request();
        req.args.push(OsString::from("a\0b"));
        assert_rejected(&req, "args[3]");
    }

    #[test]
    fn rejects_nul_in_env_value() {
        let mut req = minimal_valid_request();
        req.env
            .push((OsString::from("EVIL"), OsString::from("a\0b")));
        assert_rejected(&req, "EVIL");
    }

    #[test]
    fn rejects_empty_env_key() {
        let mut req = minimal_valid_request();
        req.env.push((OsString::new(), OsString::from("value")));
        assert_rejected(&req, "env key must not be empty");
    }

    #[test]
    fn rejects_equals_in_env_key() {
        let mut req = minimal_valid_request();
        req.env
            .push((OsString::from("KEY=BAD"), OsString::from("value")));
        assert_rejected(&req, "KEY=BAD");
    }

    // Rule 7: cwd must be under some mount.

    #[test]
    fn rejects_cwd_outside_all_mounts() {
        let mut req = minimal_valid_request();
        req.cwd = PathBuf::from("/nowhere");
        assert_rejected(&req, "/nowhere");
    }

    // Helper behavior.

    #[test]
    fn writable_mount_for_picks_longest_prefix() {
        let mut req = minimal_valid_request();
        // Add a more specific writable mount nested under /work/outputs.
        req.mounts.push(Mount {
            source: PathBuf::from("/host/scratch/outputs/nested"),
            target: PathBuf::from("/work/outputs/nested"),
            writable: true,
            optional: false,
        });
        let m = req
            .writable_mount_for(Path::new("/work/outputs/nested/file"))
            .expect("should find a writable mount");
        assert_eq!(m.target, PathBuf::from("/work/outputs/nested"));

        let m = req
            .writable_mount_for(Path::new("/work/outputs/other"))
            .expect("should find the parent mount");
        assert_eq!(m.target, PathBuf::from("/work/outputs"));
    }

    #[test]
    fn writable_mount_for_ignores_readonly_mounts() {
        let req = minimal_valid_request();
        assert!(req.writable_mount_for(Path::new("/bin/sh")).is_none());
    }

    #[test]
    fn inline_file_debug_elides_contents() {
        let f = InlineFile {
            path: PathBuf::from("/work/build/.netrc"),
            contents: b"machine example password hunter2".to_vec(),
            mode: 0o600,
        };
        let dbg = format!("{f:?}");
        assert!(
            !dbg.contains("hunter2"),
            "Debug must not leak contents: {dbg}"
        );
        assert!(
            dbg.contains("32 bytes"),
            "Debug should show the length: {dbg}"
        );
    }
}

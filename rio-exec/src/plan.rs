//! The sandbox plan: a fully-resolved, ordered description of every
//! operation that constructs a sandbox.
//!
//! [`SandboxPlan::compile`] is a **pure function** — no syscalls, no
//! filesystem access — that turns an [`ExecutionRequest`] plus a
//! [`HostLayout`] into concrete ordered operation lists. All of the
//! ordering and content decisions that make sandboxes subtly wrong live
//! here, where they can be unit-tested without privileges:
//!
//! - bind mounts sorted parents-before-children so a writable parent
//!   applied after its read-only children would not shadow them;
//! - the synthesized `/etc` files;
//! - the `/dev` population;
//! - the parent-directory chains every mount target and inline file
//!   needs;
//! - everything the post-fork child will touch, pre-converted to
//!   `CString`s so the child never allocates.
//!
//! The plan is consumed by two interpreters: [`skeleton::build`]
//! (host-side, pre-fork, ordinary error handling) and the child
//! sequence in [`child`] (post-fork, async-signal-safe).
//!
//! [`skeleton::build`]: crate::skeleton::build
//! [`child`]: crate::child

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use libc::sock_filter;

use crate::request::{ExecutionRequest, Personality};
use crate::{ExecError, seccomp};

/// Host-side locations the executor chooses for a single execution.
///
/// Kept separate from the request because the caller describes *what*
/// to run and the executor decides *where on the host* to stage it.
#[derive(Debug, Clone)]
pub struct HostLayout {
    /// Directory the chroot skeleton is built in. Becomes the sandbox's
    /// `/` after `pivot_root`. Must be on a filesystem the executor can
    /// create directories and files in; should be empty or a previous
    /// attempt's skeleton (the builder is idempotent).
    pub chroot_dir: PathBuf,
}

/// The name of the directory (directly under the chroot root) that the
/// old root is pivoted onto, unmounted from, and removed. Present in
/// the skeleton so the child does not have to `mkdir` it.
pub(crate) const PIVOT_OLD_ROOT: &str = ".real-root";

/// [`PIVOT_OLD_ROOT`] as the relative C string `pivot_root(2)` takes
/// (the child's cwd is the chroot root when it pivots). A unit test
/// asserts the two spellings match.
pub(crate) const PIVOT_OLD_ROOT_C: &CStr = c".real-root";

/// A fully-resolved, ordered description of one sandbox.
///
/// Everything the post-fork child touches is precomputed here —
/// pre-joined paths as `CString`s, the assembled seccomp program, the
/// execve triple — because the child must not allocate.
#[derive(Debug)]
pub(crate) struct SandboxPlan {
    /// Where the chroot skeleton lives on the host.
    pub chroot_dir: PathBuf,
    /// Directories to create under `chroot_dir`, parents before
    /// children, as `(chroot-relative path, mode)`.
    pub dirs: Vec<(PathBuf, u32)>,
    /// Symlinks to create under `chroot_dir`, as
    /// `(chroot-relative path, link target)`. Created host-side by the
    /// skeleton builder — a symlink is plain data, it does not need the
    /// mount namespace to exist.
    pub symlinks: Vec<(PathBuf, PathBuf)>,
    /// Files to write before fork: the synthesized `/etc` files (under
    /// `chroot_dir`) and the request's inline files (under their
    /// writable mount's host source).
    pub files: Vec<PlannedFile>,
    /// Bind mounts in application order (parents before children).
    pub binds: Vec<PlannedBind>,
    /// Non-bind mounts (`proc`, `devpts`, `/dev/shm` tmpfs), applied
    /// after every bind.
    pub special_mounts: Vec<SpecialMount>,
    /// Everything the child does that is not a mount.
    pub child: ChildPlan,
    /// The execve triple, pre-converted to `CString`s.
    pub exec: ExecPlan,
}

/// A file written to the host before fork.
#[derive(Clone)]
pub(crate) struct PlannedFile {
    /// Absolute host-side path to write.
    pub host_path: PathBuf,
    /// Raw contents.
    pub contents: Vec<u8>,
    /// Permission bits.
    pub mode: u32,
    /// Chown to the sandbox uid/gid after writing. True for the
    /// request's inline files (the sandboxed process must be able to
    /// read its own `0o600` credential files); false for the
    /// synthesized `/etc` files (world-readable, owner irrelevant).
    pub chown_to_sandbox_user: bool,
}

impl std::fmt::Debug for PlannedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Same rationale as `InlineFile`'s manual Debug: the contents
        // can be caller secrets (netrc, tokens).
        f.debug_struct("PlannedFile")
            .field("host_path", &self.host_path)
            .field("contents", &format_args!("<{} bytes>", self.contents.len()))
            .field("mode", &format_args!("{:#o}", self.mode))
            .field("chown_to_sandbox_user", &self.chown_to_sandbox_user)
            .finish()
    }
}

/// A single bind mount, resolved and ordered.
#[derive(Debug, Clone)]
pub(crate) struct PlannedBind {
    /// Host path to bind from (for stat, error messages, tests).
    pub source: PathBuf,
    /// Sandbox-absolute target (for error messages, tests).
    pub target: PathBuf,
    /// `source` as a C string for the child's `mount(2)`.
    pub source_c: CString,
    /// `{chroot_dir}{target}` as a C string for the child's `mount(2)`
    /// — binds are applied before `pivot_root`, so the mount target is
    /// the host-side path into the skeleton.
    pub target_in_chroot_c: CString,
    /// Bind then remount read-only.
    pub read_only: bool,
    /// Skip silently if `source` does not exist (resolved by the
    /// skeleton builder, which performs the only `stat`).
    pub optional: bool,
    /// Set by the skeleton builder when the bind must not be applied:
    /// an `optional` bind whose source was absent, or a nested bind
    /// whose source root is a symlink (the enclosing mount's source
    /// already carries the symlink, and `mount(2)` would resolve it
    /// against the host namespace). The child ignores skipped binds.
    pub skipped: bool,
    /// True when this bind's target is strictly inside another planned
    /// bind's target. The skeleton builder does NOT create targets for
    /// nested binds — after the enclosing bind is applied, the target
    /// path resolves into the enclosing mount's *source*, so the target
    /// must already exist there. (For the intended caller the enclosing
    /// writable mount's source is a directory tree that already
    /// contains every nested target.) A missing nested target surfaces
    /// as `ENOENT` from the child's `mount(2)`, attributed to this
    /// bind's index.
    pub nested: bool,
}

/// A non-bind mount applied after the binds.
#[derive(Debug, Clone)]
pub(crate) struct SpecialMount {
    /// Filesystem type (`proc`, `tmpfs`, `devpts`).
    pub fstype: &'static CStr,
    /// Sandbox-absolute target (for error messages, tests).
    pub target: PathBuf,
    /// `{chroot_dir}{target}` as a C string.
    pub target_in_chroot_c: CString,
    /// `MS_*` flags.
    pub flags: libc::c_ulong,
    /// Mount options string.
    pub data: &'static CStr,
}

/// Everything the child does that is not a filesystem operation,
/// precomputed so the child performs no allocation and no decisions.
pub(crate) struct ChildPlan {
    /// `CLONE_NEW*` bits for `unshare(2)`, run by the intermediate
    /// process.
    pub unshare_flags: libc::c_int,
    /// Hostname for `sethostname(2)` (raw bytes; the syscall takes a
    /// pointer + length, not a C string).
    pub hostname: Vec<u8>,
    /// Bring up the loopback interface in the new network namespace
    /// (only meaningful when `CLONE_NEWNET` is in
    /// [`unshare_flags`](Self::unshare_flags); best-effort).
    pub bring_up_loopback: bool,
    /// uid/gid to drop to immediately before exec.
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    /// Architecture personality to apply before exec.
    pub personality: Personality,
    /// The assembled seccomp program, or `None` when the request did
    /// not ask for the purity filter. Assembled here (it allocates) so
    /// the child only points the kernel at the finished slice.
    pub seccomp_program: Option<Vec<sock_filter>>,
    /// The chroot directory as a C string (the child binds it onto
    /// itself, `chdir`s into it, and `pivot_root`s on `"."`).
    pub chroot_dir_c: CString,
    /// The post-pivot working directory (sandbox-absolute).
    pub cwd_c: CString,
    /// `/{PIVOT_OLD_ROOT}` — the post-pivot path of the old root, for
    /// `umount2` + `rmdir`.
    pub pivot_old_root_abs_c: CString,
}

impl std::fmt::Debug for ChildPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildPlan")
            .field("unshare_flags", &format_args!("{:#x}", self.unshare_flags))
            // The hostname originates from `Isolation::hostname: String`,
            // so it is always valid UTF-8; the fallback only defends a
            // future non-String source.
            .field(
                "hostname",
                &std::str::from_utf8(&self.hostname).unwrap_or("<non-utf8>"),
            )
            .field("bring_up_loopback", &self.bring_up_loopback)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("personality", &self.personality)
            .field(
                "seccomp_program",
                &self.seccomp_program.as_ref().map(|p| p.len()),
            )
            .field("chroot_dir_c", &self.chroot_dir_c)
            .field("cwd_c", &self.cwd_c)
            .finish_non_exhaustive()
    }
}

/// The execve triple with every string pre-converted to a NUL-terminated
/// C string. The pointer arrays `execve(2)` needs are built by
/// [`ExecPlan::ptr_arrays`] in the parent (it allocates) and handed to
/// the child.
#[derive(Debug)]
pub(crate) struct ExecPlan {
    /// The path passed to `execve(2)`.
    pub program: CString,
    /// argv, verbatim from the request.
    pub argv: Vec<CString>,
    /// envp as `KEY=VALUE` strings.
    pub envp: Vec<CString>,
}

/// NUL-terminated pointer arrays into an [`ExecPlan`]'s strings, in the
/// shape `execve(2)` wants. Borrows the plan: the pointers are only
/// valid while the plan is alive and unmoved.
pub(crate) struct ExecPtrs<'a> {
    pub argv: Vec<*const libc::c_char>,
    pub envp: Vec<*const libc::c_char>,
    _plan: std::marker::PhantomData<&'a ExecPlan>,
}

impl ExecPlan {
    /// Build the NUL-terminated pointer arrays for `execve(2)`.
    ///
    /// Allocates — must be called by the parent before forking, with
    /// the result passed to the child by reference.
    pub(crate) fn ptr_arrays(&self) -> ExecPtrs<'_> {
        let argv = self
            .argv
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        let envp = self
            .envp
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        ExecPtrs {
            argv,
            envp,
            _plan: std::marker::PhantomData,
        }
    }
}

/// The character devices bound from the host into every sandbox's
/// `/dev`. Bound (not `mknod`ed) because the executor may not hold
/// `CAP_MKNOD`; a bind of a device node carries the device through.
const STANDARD_DEVICES: &[&str] = &["null", "zero", "full", "random", "urandom", "tty"];

/// The `/dev` symlinks present in every sandbox. Targets are link
/// *contents* — they need not resolve at creation time.
const DEV_SYMLINKS: &[(&str, &str)] = &[
    ("dev/fd", "/proc/self/fd"),
    ("dev/stdin", "/proc/self/fd/0"),
    ("dev/stdout", "/proc/self/fd/1"),
    ("dev/stderr", "/proc/self/fd/2"),
    ("dev/ptmx", "/dev/pts/ptmx"),
];

impl SandboxPlan {
    /// Resolve a request into the ordered operation lists that
    /// construct its sandbox.
    ///
    /// Pure: no syscalls, no filesystem access. Calls
    /// [`ExecutionRequest::validate`] first; everything after that is
    /// resolution, not validation.
    pub(crate) fn compile(
        req: &ExecutionRequest,
        layout: &HostLayout,
    ) -> Result<SandboxPlan, ExecError> {
        req.validate()?;
        if !layout.chroot_dir.is_absolute() {
            return Err(ExecError::InvalidRequest(format!(
                "chroot_dir must be absolute: {}",
                layout.chroot_dir.display()
            )));
        }

        // -------------------------------------------------------------
        // Binds: the request's mounts + the /dev device nodes + (with
        // network) the host's resolver files, all in one list sorted
        // parents-before-children.
        // -------------------------------------------------------------
        let mut binds: Vec<PlannedBind> = Vec::new();
        for m in &req.mounts {
            binds.push(plan_bind(
                layout,
                m.source.clone(),
                m.target.clone(),
                !m.writable,
                m.optional,
            )?);
        }
        for name in STANDARD_DEVICES {
            let p = PathBuf::from(format!("/dev/{name}"));
            // The host device is also the sandbox path. Required: a
            // Linux host without /dev/null is broken beyond this
            // crate's concern, and silently omitting it would produce
            // confusing downstream failures.
            binds.push(plan_bind(layout, p.clone(), p, false, false)?);
        }
        for dev in &req.extra_devices {
            let name = dev.file_name().ok_or_else(|| {
                ExecError::InvalidRequest(format!(
                    "extra device path has no file name: {}",
                    dev.display()
                ))
            })?;
            let target = Path::new("/dev").join(name);
            // Not optional: the caller explicitly asked for this
            // device; a host that lacks it should fail loudly rather
            // than run the process without it.
            binds.push(plan_bind(layout, dev.clone(), target, false, false)?);
        }
        if req.isolation.network {
            // The process shares the executor's network namespace, so
            // give it the executor's resolver configuration. Optional:
            // a host without /etc/services still resolves names. The
            // CA-bundle question is policy and stays with the caller.
            for f in ["/etc/resolv.conf", "/etc/services", "/etc/hosts"] {
                let p = PathBuf::from(f);
                binds.push(plan_bind(layout, p.clone(), p, true, true)?);
            }
        }

        // Parents before children: sort by component count of the
        // target (a parent has strictly fewer components than anything
        // nested inside it), then lexicographically so the order is
        // deterministic for equal depths. A mount applied over a
        // directory hides everything previously mounted beneath it, so
        // applying a writable parent after its read-only children would
        // silently produce an empty parent.
        binds.sort_by(|a, b| {
            a.target
                .components()
                .count()
                .cmp(&b.target.components().count())
                .then_with(|| a.target.cmp(&b.target))
        });

        // Mark nested binds (their target lives inside another bind's
        // target) — the skeleton builder must not create host-side
        // placeholder targets for these, because after the enclosing
        // bind is applied the path resolves into the enclosing mount's
        // source instead.
        let targets: Vec<PathBuf> = binds.iter().map(|b| b.target.clone()).collect();
        for b in &mut binds {
            b.nested = targets
                .iter()
                .any(|t| t != &b.target && b.target.starts_with(t));
        }

        // -------------------------------------------------------------
        // Directories: the fixed skeleton + the parent chain of every
        // non-nested bind target, inline file, declared output, and the
        // cwd. BTreeMap orders parents before children (PathBuf's Ord
        // is component-wise, and a parent is a strict prefix of its
        // children) and dedupes; explicit skeleton modes win over the
        // 0o755 default for derived parent chains.
        // -------------------------------------------------------------
        let mut dirs: BTreeMap<PathBuf, u32> = BTreeMap::new();
        let add_chain = |dirs: &mut BTreeMap<PathBuf, u32>, p: &Path| {
            for anc in p.ancestors().skip(1) {
                if anc == Path::new("/") {
                    continue;
                }
                dirs.entry(rel(anc)).or_insert(0o755);
            }
        };
        for b in &binds {
            if !b.nested {
                add_chain(&mut dirs, &b.target);
            }
        }
        for f in &req.inline_files {
            add_chain(&mut dirs, &f.path);
        }
        for o in &req.declared_outputs {
            add_chain(&mut dirs, o);
        }
        // The cwd itself (not just its parents) must exist somewhere:
        // the child chdirs into it after pivot_root. When it falls
        // under a mount the real directory comes from the mount
        // source and the skeleton placeholder is shadowed; the
        // placeholder costs nothing and keeps the failure mode for a
        // cwd that the mount source does NOT provide at the chdir
        // step (attributable) rather than at exec.
        add_chain(&mut dirs, &req.cwd);
        if req.cwd != Path::new("/") {
            dirs.entry(rel(&req.cwd)).or_insert(0o755);
        }
        // Fixed skeleton. Inserted last so these modes override the
        // 0o755 the chains above may have inserted.
        for (p, mode) in [
            ("tmp", 0o1777),
            // /etc is created 0o755 so the synthesized files can be
            // written into it, then chmod'd to 0o555 by the skeleton
            // builder's final step.
            ("etc", 0o755),
            ("dev", 0o755),
            ("dev/shm", 0o755),
            ("dev/pts", 0o755),
            ("proc", 0o555),
            (PIVOT_OLD_ROOT, 0o700),
        ] {
            dirs.insert(PathBuf::from(p), mode);
        }
        let dirs: Vec<(PathBuf, u32)> = dirs.into_iter().collect();

        // -------------------------------------------------------------
        // Symlinks: the /dev convenience links. Plain host-side file
        // creation; no namespace dependency.
        // -------------------------------------------------------------
        let symlinks = DEV_SYMLINKS
            .iter()
            .map(|(p, t)| (PathBuf::from(p), PathBuf::from(t)))
            .collect();

        // -------------------------------------------------------------
        // Files: the synthesized /etc files (into the chroot skeleton)
        // + the request's inline files (into their writable mount's
        // host source).
        // -------------------------------------------------------------
        let mut files: Vec<PlannedFile> = Vec::new();
        let uid = req.isolation.uid;
        let gid = req.isolation.gid;
        let cwd_str = req.cwd.display();
        // The build user/group are named `nixbld`, matching the entries
        // CppNix synthesizes inside its sandbox: the name is observable
        // via `whoami` / `id -un` / `id -gn` / getpwuid and gets baked
        // into some outputs ("built by" banners, perl's Config.pm), so
        // it is part of the de-facto sandbox ABI rather than a free
        // choice.
        files.push(PlannedFile {
            host_path: layout.chroot_dir.join("etc/passwd"),
            contents: format!(
                "root:x:0:0:root:{cwd_str}:/noshell\n\
                 nixbld:x:{uid}:{gid}:Nix build user:{cwd_str}:/noshell\n\
                 nobody:x:65534:65534:nobody:/:/noshell\n"
            )
            .into_bytes(),
            mode: 0o444,
            chown_to_sandbox_user: false,
        });
        files.push(PlannedFile {
            host_path: layout.chroot_dir.join("etc/group"),
            contents: format!(
                "root:x:0:\n\
                 nixbld:!:{gid}:\n\
                 nogroup:x:65534:\n"
            )
            .into_bytes(),
            mode: 0o444,
            chown_to_sandbox_user: false,
        });
        if req.isolation.network {
            // Shared network namespace: name resolution goes through
            // the bind-mounted host resolv.conf; nsswitch makes glibc
            // actually consult it.
            files.push(PlannedFile {
                host_path: layout.chroot_dir.join("etc/nsswitch.conf"),
                contents: b"hosts: files dns\nservices: files\n".to_vec(),
                mode: 0o444,
                chown_to_sandbox_user: false,
            });
        } else {
            // Private network namespace: the only name that resolves is
            // localhost, in both address families.
            files.push(PlannedFile {
                host_path: layout.chroot_dir.join("etc/hosts"),
                contents: b"127.0.0.1 localhost\n::1 localhost\n".to_vec(),
                mode: 0o444,
                chown_to_sandbox_user: false,
            });
        }
        for f in &req.inline_files {
            // validate() guarantees a writable mount exists for every
            // inline file.
            let m = req
                .writable_mount_for(&f.path)
                .expect("validate() accepted an inline file with no writable mount");
            let rel_to_mount = f
                .path
                .strip_prefix(&m.target)
                .expect("writable_mount_for returned a non-prefix mount");
            files.push(PlannedFile {
                host_path: m.source.join(rel_to_mount),
                contents: f.contents.clone(),
                mode: f.mode,
                chown_to_sandbox_user: true,
            });
        }

        // -------------------------------------------------------------
        // Special mounts, applied after every bind: a fresh /proc (the
        // child is pid 1 of the new PID namespace by the time it
        // mounts this, so it sees only the sandbox's processes), a
        // tmpfs on /dev/shm, and a private devpts instance.
        // -------------------------------------------------------------
        let special_mounts = vec![
            SpecialMount {
                fstype: c"proc",
                target: PathBuf::from("/proc"),
                target_in_chroot_c: chroot_path_c(layout, Path::new("/proc"))?,
                flags: libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                data: c"",
            },
            SpecialMount {
                fstype: c"tmpfs",
                target: PathBuf::from("/dev/shm"),
                target_in_chroot_c: chroot_path_c(layout, Path::new("/dev/shm"))?,
                flags: libc::MS_NOSUID | libc::MS_NODEV,
                data: c"mode=1777",
            },
            SpecialMount {
                fstype: c"devpts",
                target: PathBuf::from("/dev/pts"),
                target_in_chroot_c: chroot_path_c(layout, Path::new("/dev/pts"))?,
                flags: libc::MS_NOSUID | libc::MS_NOEXEC,
                // `newinstance` so the sandbox cannot see or grab the
                // host's ptys; `ptmxmode=0666` so an unprivileged
                // process can `posix_openpt` (without it /dev/pts/ptmx
                // is 0000 and every pty-allocating test suite fails
                // EACCES); mode=0620 for the slaves. `gid=5` (the
                // conventional `tty` group) is deliberately omitted —
                // the sandbox's group database has no gid 5 and the
                // process runs as a single uid/gid anyway.
                data: c"newinstance,ptmxmode=0666,mode=0620",
            },
        ];

        // -------------------------------------------------------------
        // The child plan.
        // -------------------------------------------------------------
        let mut unshare_flags = libc::CLONE_NEWNS
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWCGROUP;
        if !req.isolation.network {
            unshare_flags |= libc::CLONE_NEWNET;
        }
        let child = ChildPlan {
            unshare_flags,
            hostname: req.isolation.hostname.clone().into_bytes(),
            bring_up_loopback: !req.isolation.network,
            uid,
            gid,
            personality: req.isolation.personality,
            seccomp_program: req
                .isolation
                .deny_setuid_and_xattrs
                .then(seccomp::build_filter),
            chroot_dir_c: path_c(&layout.chroot_dir)?,
            cwd_c: path_c(&req.cwd)?,
            pivot_old_root_abs_c: path_c(Path::new(&format!("/{PIVOT_OLD_ROOT}")))?,
        };

        // -------------------------------------------------------------
        // The execve triple.
        // -------------------------------------------------------------
        let exec = ExecPlan {
            program: path_c(&req.program)?,
            argv: req
                .args
                .iter()
                .map(|a| CString::new(a.as_bytes()))
                .collect::<Result<_, _>>()
                .map_err(|_| {
                    // Unreachable: validate() rejects NUL in args.
                    ExecError::InvalidRequest("argv entry contains NUL".into())
                })?,
            envp: req
                .env
                .iter()
                .map(|(k, v)| {
                    let mut kv = Vec::with_capacity(k.len() + 1 + v.len());
                    kv.extend_from_slice(k.as_bytes());
                    kv.push(b'=');
                    kv.extend_from_slice(v.as_bytes());
                    CString::new(kv)
                })
                .collect::<Result<_, _>>()
                .map_err(|_| {
                    // Unreachable: validate() rejects NUL in env.
                    ExecError::InvalidRequest("env entry contains NUL".into())
                })?,
        };

        Ok(SandboxPlan {
            chroot_dir: layout.chroot_dir.clone(),
            dirs,
            symlinks,
            files,
            binds,
            special_mounts,
            child,
            exec,
        })
    }
}

/// Build one [`PlannedBind`] with its pre-joined C strings.
fn plan_bind(
    layout: &HostLayout,
    source: PathBuf,
    target: PathBuf,
    read_only: bool,
    optional: bool,
) -> Result<PlannedBind, ExecError> {
    Ok(PlannedBind {
        source_c: path_c(&source)?,
        target_in_chroot_c: chroot_path_c(layout, &target)?,
        source,
        target,
        read_only,
        optional,
        skipped: false,
        nested: false,
    })
}

/// `path` relative to `/` (strip the leading root component).
fn rel(path: &Path) -> PathBuf {
    path.strip_prefix("/").unwrap_or(path).to_path_buf()
}

/// `{chroot_dir}{sandbox_path}` as a C string.
fn chroot_path_c(layout: &HostLayout, sandbox_path: &Path) -> Result<CString, ExecError> {
    path_c(&layout.chroot_dir.join(rel(sandbox_path)))
}

/// A path as a C string, rejecting interior NULs.
fn path_c(p: &Path) -> Result<CString, ExecError> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| {
        ExecError::InvalidRequest(format!("path contains a NUL byte: {}", p.display()))
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::Duration;

    use super::*;
    use crate::request::{InlineFile, Isolation, Limits, Mount, OutputCapture};

    fn layout() -> HostLayout {
        HostLayout {
            chroot_dir: PathBuf::from("/host/chroot"),
        }
    }

    /// A request with nested mounts in deliberately shuffled order: the
    /// deepest target first, the writable parents last. Compilation
    /// must invert that.
    fn nested_request() -> ExecutionRequest {
        ExecutionRequest {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("sh")],
            env: vec![(OsString::from("PATH"), OsString::from("/path-not-set"))],
            cwd: PathBuf::from("/build"),
            mounts: vec![
                Mount {
                    source: PathBuf::from("/host/inputs/a/nested"),
                    target: PathBuf::from("/work/inputs/a/nested"),
                    writable: false,
                    optional: false,
                },
                Mount {
                    source: PathBuf::from("/host/inputs/a"),
                    target: PathBuf::from("/work/inputs/a"),
                    writable: false,
                    optional: false,
                },
                Mount {
                    source: PathBuf::from("/host/build"),
                    target: PathBuf::from("/build"),
                    writable: true,
                    optional: false,
                },
                Mount {
                    source: PathBuf::from("/host/work"),
                    target: PathBuf::from("/work"),
                    writable: true,
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
                path: PathBuf::from("/build/.netrc"),
                contents: b"machine m login l password p\n".to_vec(),
                mode: 0o600,
            }],
            declared_outputs: vec![PathBuf::from("/work/out/result")],
            capture: OutputCapture::MergedPty,
            isolation: Isolation {
                network: false,
                uid: 1000,
                gid: 100,
                personality: crate::request::Personality::Native,
                hostname: String::from("localhost"),
                deny_setuid_and_xattrs: true,
            },
            limits: Limits {
                timeout: Some(Duration::from_secs(60)),
                max_silent: None,
                max_log_bytes: None,
                cgroup: None,
            },
        }
    }

    fn compile(req: &ExecutionRequest) -> SandboxPlan {
        SandboxPlan::compile(req, &layout()).expect("plan should compile")
    }

    /// The position of a bind by target, panicking if absent.
    fn bind_pos(plan: &SandboxPlan, target: &str) -> usize {
        plan.binds
            .iter()
            .position(|b| b.target == Path::new(target))
            .unwrap_or_else(|| panic!("no bind for {target}"))
    }

    #[test]
    fn binds_are_ordered_parents_before_children() {
        let plan = compile(&nested_request());
        assert!(bind_pos(&plan, "/work") < bind_pos(&plan, "/work/inputs/a"));
        assert!(bind_pos(&plan, "/work/inputs/a") < bind_pos(&plan, "/work/inputs/a/nested"));
        // Equal depths are ordered lexicographically (determinism, not
        // correctness).
        assert!(bind_pos(&plan, "/bin/sh") < bind_pos(&plan, "/dev/null"));
    }

    #[test]
    fn compilation_is_deterministic() {
        let req = nested_request();
        let a = compile(&req);
        let b = compile(&req);
        let targets =
            |p: &SandboxPlan| p.binds.iter().map(|b| b.target.clone()).collect::<Vec<_>>();
        assert_eq!(targets(&a), targets(&b));
        let dirs = |p: &SandboxPlan| p.dirs.clone();
        assert_eq!(dirs(&a), dirs(&b));
    }

    #[test]
    fn nested_binds_are_marked_and_unnested_are_not() {
        let plan = compile(&nested_request());
        assert!(plan.binds[bind_pos(&plan, "/work/inputs/a")].nested);
        assert!(plan.binds[bind_pos(&plan, "/work/inputs/a/nested")].nested);
        assert!(!plan.binds[bind_pos(&plan, "/work")].nested);
        assert!(!plan.binds[bind_pos(&plan, "/bin/sh")].nested);
        assert!(!plan.binds[bind_pos(&plan, "/dev/null")].nested);
    }

    #[test]
    fn standard_devices_are_planned_as_required_binds() {
        let plan = compile(&nested_request());
        for dev in ["null", "zero", "full", "random", "urandom", "tty"] {
            let b = &plan.binds[bind_pos(&plan, &format!("/dev/{dev}"))];
            assert!(!b.optional, "/dev/{dev} must not be optional");
            assert!(!b.read_only, "/dev/{dev} is bound read-write");
            assert_eq!(b.source, Path::new(&format!("/dev/{dev}")));
        }
    }

    #[test]
    fn extra_devices_are_bound_under_dev_by_basename() {
        let mut req = nested_request();
        req.extra_devices.push(PathBuf::from("/dev/kvm"));
        let plan = compile(&req);
        let b = &plan.binds[bind_pos(&plan, "/dev/kvm")];
        assert_eq!(b.source, Path::new("/dev/kvm"));
        assert!(!b.optional);
    }

    #[test]
    fn read_only_flag_follows_writability() {
        let plan = compile(&nested_request());
        assert!(plan.binds[bind_pos(&plan, "/work/inputs/a")].read_only);
        assert!(!plan.binds[bind_pos(&plan, "/work")].read_only);
    }

    #[test]
    fn etc_passwd_and_group_name_the_sandbox_identity() {
        let plan = compile(&nested_request());
        let passwd = plan
            .files
            .iter()
            .find(|f| f.host_path.ends_with("etc/passwd"))
            .expect("passwd planned");
        let text = String::from_utf8(passwd.contents.clone()).expect("passwd is utf8");
        assert!(text.contains("nixbld:x:1000:100:Nix build user:/build:/noshell\n"));
        assert!(text.contains("root:x:0:0:"));
        assert!(text.contains("nobody:x:65534:65534:"));
        assert_eq!(passwd.mode, 0o444);
        assert!(!passwd.chown_to_sandbox_user);

        let group = plan
            .files
            .iter()
            .find(|f| f.host_path.ends_with("etc/group"))
            .expect("group planned");
        let text = String::from_utf8(group.contents.clone()).expect("group is utf8");
        assert!(text.contains("nixbld:!:100:\n"));
        assert!(text.contains("nogroup:x:65534:\n"));
    }

    #[test]
    fn isolated_network_gets_static_hosts_and_no_resolver_binds() {
        let plan = compile(&nested_request());
        let hosts = plan
            .files
            .iter()
            .find(|f| f.host_path.ends_with("etc/hosts"))
            .expect("hosts planned for isolated network");
        assert_eq!(
            hosts.contents,
            b"127.0.0.1 localhost\n::1 localhost\n".to_vec()
        );
        assert!(
            !plan
                .binds
                .iter()
                .any(|b| b.target == Path::new("/etc/resolv.conf")),
            "no resolver bind without network access"
        );
        assert!(
            !plan
                .files
                .iter()
                .any(|f| f.host_path.ends_with("nsswitch.conf")),
            "no nsswitch without network access"
        );
    }

    #[test]
    fn shared_network_gets_resolver_binds_and_nsswitch() {
        let mut req = nested_request();
        req.isolation.network = true;
        let plan = compile(&req);
        for f in ["/etc/resolv.conf", "/etc/services", "/etc/hosts"] {
            let b = &plan.binds[bind_pos(&plan, f)];
            assert!(b.optional, "{f} must be optional (the host may lack it)");
            assert!(b.read_only, "{f} must be read-only");
        }
        assert!(
            plan.files
                .iter()
                .any(|f| f.host_path.ends_with("nsswitch.conf")),
            "nsswitch.conf planned with network access"
        );
        assert!(
            !plan
                .files
                .iter()
                .any(|f| f.host_path.ends_with("etc/hosts")),
            "no static hosts file when the host's is bind-mounted"
        );
    }

    #[test]
    fn inline_files_resolve_to_their_writable_mounts_host_source() {
        let plan = compile(&nested_request());
        let netrc = plan
            .files
            .iter()
            .find(|f| f.host_path.ends_with(".netrc"))
            .expect("inline file planned");
        // /build/.netrc under the /build -> /host/build mount.
        assert_eq!(netrc.host_path, Path::new("/host/build/.netrc"));
        assert_eq!(netrc.mode, 0o600);
        assert!(netrc.chown_to_sandbox_user);
    }

    #[test]
    fn dev_symlinks_are_planned() {
        let plan = compile(&nested_request());
        let links: Vec<(&str, &str)> = plan
            .symlinks
            .iter()
            .map(|(p, t)| (p.to_str().expect("utf8"), t.to_str().expect("utf8")))
            .collect();
        assert_eq!(
            links,
            vec![
                ("dev/fd", "/proc/self/fd"),
                ("dev/stdin", "/proc/self/fd/0"),
                ("dev/stdout", "/proc/self/fd/1"),
                ("dev/stderr", "/proc/self/fd/2"),
                ("dev/ptmx", "/dev/pts/ptmx"),
            ]
        );
    }

    #[test]
    fn dirs_contain_parent_chains_and_fixed_skeleton() {
        let plan = compile(&nested_request());
        let dirs: Vec<&Path> = plan.dirs.iter().map(|(p, _)| p.as_path()).collect();
        // Parent chain of the deepest non-nested bind target and of
        // the declared output.
        assert!(dirs.contains(&Path::new("work")));
        assert!(dirs.contains(&Path::new("work/out")));
        assert!(dirs.contains(&Path::new("bin")));
        // The cwd itself.
        assert!(dirs.contains(&Path::new("build")));
        // The fixed skeleton.
        for d in [
            "tmp",
            "etc",
            "dev",
            "dev/shm",
            "dev/pts",
            "proc",
            ".real-root",
        ] {
            assert!(
                dirs.contains(&Path::new(d)),
                "missing fixed skeleton dir {d}"
            );
        }
        // No parent chain for nested bind targets (their placeholders
        // would be shadowed).
        assert!(!dirs.contains(&Path::new("work/inputs/a")));
        // Parents come before children.
        let pos = |d: &str| {
            dirs.iter()
                .position(|p| *p == Path::new(d))
                .unwrap_or_else(|| panic!("missing {d}"))
        };
        assert!(pos("dev") < pos("dev/shm"));
        assert!(pos("work") < pos("work/out"));
        // Modes.
        let mode = |d: &str| {
            plan.dirs
                .iter()
                .find(|(p, _)| p == Path::new(d))
                .map(|(_, m)| *m)
                .unwrap_or_else(|| panic!("missing {d}"))
        };
        assert_eq!(mode("tmp"), 0o1777);
        assert_eq!(mode("proc"), 0o555);
        assert_eq!(mode("etc"), 0o755, "etc is created writable, locked later");
    }

    #[test]
    fn special_mounts_cover_proc_shm_and_pts_in_that_order() {
        let plan = compile(&nested_request());
        let targets: Vec<&Path> = plan
            .special_mounts
            .iter()
            .map(|s| s.target.as_path())
            .collect();
        assert_eq!(
            targets,
            vec![
                Path::new("/proc"),
                Path::new("/dev/shm"),
                Path::new("/dev/pts")
            ]
        );
        let pts = &plan.special_mounts[2];
        assert_eq!(pts.fstype, c"devpts");
        assert!(
            pts.data.to_str().expect("utf8").contains("ptmxmode=0666"),
            "ptmx must be openable by the unprivileged sandbox user"
        );
        assert!(pts.data.to_str().expect("utf8").contains("newinstance"));
    }

    #[test]
    fn unshare_flags_track_network_isolation() {
        let isolated = compile(&nested_request());
        assert_ne!(isolated.child.unshare_flags & libc::CLONE_NEWNET, 0);
        assert!(isolated.child.bring_up_loopback);

        let mut req = nested_request();
        req.isolation.network = true;
        let shared = compile(&req);
        assert_eq!(shared.child.unshare_flags & libc::CLONE_NEWNET, 0);
        assert!(!shared.child.bring_up_loopback);
        // The non-network namespaces are always present.
        for flag in [
            libc::CLONE_NEWNS,
            libc::CLONE_NEWPID,
            libc::CLONE_NEWIPC,
            libc::CLONE_NEWUTS,
            libc::CLONE_NEWCGROUP,
        ] {
            assert_ne!(shared.child.unshare_flags & flag, 0);
        }
    }

    #[test]
    fn seccomp_program_is_assembled_iff_requested() {
        let with = compile(&nested_request());
        assert!(with.child.seccomp_program.is_some());
        let mut req = nested_request();
        req.isolation.deny_setuid_and_xattrs = false;
        let without = compile(&req);
        assert!(without.child.seccomp_program.is_none());
    }

    #[test]
    fn exec_plan_round_trips_argv_and_env() {
        let plan = compile(&nested_request());
        assert_eq!(plan.exec.program.to_str(), Ok("/bin/sh"));
        assert_eq!(plan.exec.argv, vec![CString::new("sh").expect("cstr")]);
        assert_eq!(
            plan.exec.envp,
            vec![CString::new("PATH=/path-not-set").expect("cstr")]
        );
        let ptrs = plan.exec.ptr_arrays();
        assert_eq!(ptrs.argv.len(), 2, "argv is NULL-terminated");
        assert!(ptrs.argv[1].is_null());
        assert_eq!(ptrs.envp.len(), 2, "envp is NULL-terminated");
        assert!(ptrs.envp[1].is_null());
    }

    #[test]
    fn compile_rejects_what_validate_rejects() {
        let mut req = nested_request();
        req.args.clear();
        let err = SandboxPlan::compile(&req, &layout()).expect_err("invalid request");
        let ExecError::InvalidRequest(msg) = err else {
            panic!("compile rejections must be InvalidRequest, got {err:?}");
        };
        assert!(msg.contains("argv[0]"));
    }

    #[test]
    fn compile_rejects_relative_chroot_dir() {
        let req = nested_request();
        let err = SandboxPlan::compile(
            &req,
            &HostLayout {
                chroot_dir: PathBuf::from("relative/chroot"),
            },
        )
        .expect_err("relative chroot dir");
        let ExecError::InvalidRequest(msg) = err else {
            panic!("compile rejections must be InvalidRequest, got {err:?}");
        };
        assert!(msg.contains("relative/chroot"));
    }

    #[test]
    fn pivot_old_root_spellings_match() {
        assert_eq!(
            PIVOT_OLD_ROOT_C.to_str().expect("utf8"),
            PIVOT_OLD_ROOT,
            "the str (skeleton) and CStr (child) spellings of the pivot \
             directory must stay identical"
        );
    }

    #[test]
    fn planned_file_debug_elides_contents() {
        let plan = compile(&nested_request());
        let netrc = plan
            .files
            .iter()
            .find(|f| f.host_path.ends_with(".netrc"))
            .expect("inline file planned");
        let dbg = format!("{netrc:?}");
        assert!(
            !dbg.contains("password"),
            "Debug must not leak contents: {dbg}"
        );
    }

    #[test]
    fn bind_c_strings_are_prejoined_into_the_chroot() {
        let plan = compile(&nested_request());
        let b = &plan.binds[bind_pos(&plan, "/work")];
        assert_eq!(b.target_in_chroot_c.to_str(), Ok("/host/chroot/work"));
        assert_eq!(b.source_c.to_str(), Ok("/host/work"));
    }
}

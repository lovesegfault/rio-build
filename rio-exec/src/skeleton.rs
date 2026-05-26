//! Host-side sandbox preparation: materialize a [`SandboxPlan`]'s
//! directory tree, files, and symlinks before forking.
//!
//! Runs in the parent with ordinary error handling and allocation. The
//! only filesystem *reads* the whole executor performs on mount sources
//! happen here (the per-bind `stat` that resolves optional binds and
//! file-vs-directory bind targets); the post-fork child only performs
//! the mounts.
//!
//! # Idempotency
//!
//! `build` over an existing skeleton converges instead of failing: a
//! directory that already exists is re-`chmod`ed to its planned mode, a
//! file is re-written, a symlink is re-created. A retried execution can
//! therefore reuse the previous attempt's chroot directory without a
//! cleanup pass in between.
//!
//! # Ownership
//!
//! Inline files are `chown`ed to the sandbox uid/gid so the process can
//! read its own `0o600` credential files after the privilege drop. When
//! the executor already *is* the sandbox uid (unprivileged tests) that
//! chown is a no-op the kernel always permits; when the executor is
//! root (production) it is a real ownership transfer. The chroot root
//! itself is set to mode `0o750` and, when the executor is root, group
//! `gid` — readable and traversable by the sandboxed process, writable
//! only by root. An unprivileged executor cannot give a directory away
//! to root, so there it stays owned by the executor, which is the same
//! trust domain.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _, chown};
use std::path::Path;

use crate::plan::SandboxPlan;

/// Materialize the plan's chroot skeleton, files, symlinks, and bind
/// targets on the host, and resolve each optional bind's presence.
///
/// Mutates the plan: optional binds whose source is absent are marked
/// [`skipped`](crate::plan::PlannedBind::skipped) so the child does not
/// attempt them.
pub(crate) fn build(plan: &mut SandboxPlan) -> io::Result<()> {
    let root = plan.chroot_dir.clone();

    // The chroot root: 0o750, group = sandbox gid when we have the
    // privilege to set it. Created first so everything else nests
    // inside it.
    ensure_dir(&root, 0o750)?;
    if is_root() {
        chown(&root, Some(0), Some(plan.child.gid))
            .map_err(|e| annotate(e, format_args!("chown chroot root {}", root.display())))?;
    }

    // Directories, parents before children (the plan's order).
    for (rel, mode) in &plan.dirs {
        ensure_dir(&root.join(rel), *mode)?;
    }

    // Symlinks. Recreated unconditionally: a stale link from a previous
    // attempt with different contents must not survive.
    for (rel, target) in &plan.symlinks {
        let link = root.join(rel);
        match fs::symlink_metadata(&link) {
            Ok(_) => fs::remove_file(&link)
                .map_err(|e| annotate(e, format_args!("remove stale {}", link.display())))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(annotate(e, format_args!("lstat {}", link.display()))),
        }
        std::os::unix::fs::symlink(target, &link).map_err(|e| {
            annotate(
                e,
                format_args!("symlink {} -> {}", link.display(), target.display()),
            )
        })?;
    }

    // Bind targets. Only for non-nested binds: a nested bind's target
    // resolves into its enclosing mount's *source* once that mount is
    // applied, so a placeholder created here would be shadowed and the
    // real target must already exist in the enclosing source (see
    // `PlannedBind::nested`). The lstat (and, for symlink roots, the
    // following stat) on the source is the single point that resolves
    // optional binds, file-vs-directory targets, and symlink-rooted
    // sources.
    for bind in &mut plan.binds {
        // lstat, not stat: a source whose root is itself a symlink must
        // be detected as such, not resolved against the *host*
        // namespace.
        let meta = match fs::symlink_metadata(&bind.source) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound && bind.optional => {
                bind.skipped = true;
                continue;
            }
            Err(e) => {
                return Err(annotate(
                    e,
                    format_args!("stat bind source {}", bind.source.display()),
                ));
            }
        };
        // A source whose root is itself a symlink cannot be carried in
        // as one by `mount(2)` — the kernel resolves the source path
        // against the *host* namespace. The discriminator below is the
        // planned mount topology, never what the host happens to
        // resolve, so the same request produces the same
        // sandbox-visible result on every machine:
        //
        // - Nested binds (artifacts living inside an enclosing mount's
        //   source — the store-path-input shape): the enclosing
        //   mount's source already contains the symlink itself, so the
        //   sandbox sees it as-is, valid or dangling alike, exactly
        //   like CppNix's `doBind` symlink handling. Re-binding it
        //   here would resolve it against the host (or fail outright
        //   when the target only exists inside the sandbox), so the
        //   bind is skipped: no placeholder, no mount.
        // - Non-nested binds (explicitly planned host paths such as
        //   the sandbox shell, whose `sh -> busybox` target lives next
        //   to it on the host): keep the established behavior — the
        //   followed stat decides optional/file-vs-directory and the
        //   bind mount resolves the same way.
        if meta.file_type().is_symlink() {
            if bind.nested {
                bind.skipped = true;
                continue;
            }
            let resolved = match fs::metadata(&bind.source) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound && bind.optional => {
                    bind.skipped = true;
                    continue;
                }
                Err(e) => {
                    return Err(annotate(
                        e,
                        format_args!("stat bind source {}", bind.source.display()),
                    ));
                }
            };
            let target = root.join(
                bind.target
                    .strip_prefix("/")
                    .expect("bind targets are validated absolute"),
            );
            if resolved.is_dir() {
                ensure_dir(&target, 0o755)?;
            } else {
                ensure_file(&target, &[], 0o444)?;
            }
            continue;
        }
        if bind.nested {
            continue;
        }
        let target = root.join(
            bind.target
                .strip_prefix("/")
                .expect("bind targets are validated absolute"),
        );
        if meta.is_dir() {
            ensure_dir(&target, 0o755)?;
        } else {
            // A zero-length placeholder for a file (or device-node)
            // bind target. Contents never matter — the bind covers it.
            ensure_file(&target, &[], 0o444)?;
        }
    }

    // Files: the synthesized /etc files and the request's inline files.
    // Written after the directories exist and before /etc is locked
    // down.
    for f in &plan.files {
        ensure_file(&f.host_path, &f.contents, f.mode)?;
        if f.chown_to_sandbox_user {
            chown(&f.host_path, Some(plan.child.uid), Some(plan.child.gid))
                .map_err(|e| annotate(e, format_args!("chown {}", f.host_path.display())))?;
        }
    }

    // /etc is read-only from here on. Last, so the writes above (and a
    // re-run's writes, which re-chmod it open via ensure_dir) succeed.
    let etc = root.join("etc");
    fs::set_permissions(&etc, fs::Permissions::from_mode(0o555))
        .map_err(|e| annotate(e, format_args!("chmod {}", etc.display())))?;

    Ok(())
}

/// `mkdir(path, mode)`, converging on an existing directory by
/// re-applying the mode. The explicit `set_permissions` on both paths
/// also defeats the umask, which `mkdir(2)` applies to its mode
/// argument.
fn ensure_dir(path: &Path, mode: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(path)
                .map_err(|e| annotate(e, format_args!("lstat {}", path.display())))?;
            if !meta.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!(
                        "skeleton path exists and is not a directory: {}",
                        path.display()
                    ),
                ));
            }
        }
        Err(e) => return Err(annotate(e, format_args!("mkdir {}", path.display()))),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| annotate(e, format_args!("chmod {}", path.display())))
}

/// Write `contents` to a freshly-created `path` with `mode`, replacing
/// whatever non-directory entry was there from a previous attempt.
///
/// `O_CREAT | O_EXCL` (`create_new`) is the symlink defence: it refuses
/// to follow an existing symlink (even a dangling one), so this can
/// never write *through* a link planted at the path — the stale entry
/// is unlinked and a fresh regular file is created in its place. The
/// remove-then-create shape (rather than open-and-truncate) is also
/// what makes a re-run converge over the previous run's read-only
/// placeholders without a chmod dance.
fn ensure_file(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write as _;
    let create = || {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)
    };
    let mut f = match create() {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(path)
                .map_err(|e| annotate(e, format_args!("remove stale {}", path.display())))?;
            // A second AlreadyExists here means something is racing the
            // skeleton builder for this path; surface it rather than
            // looping.
            create().map_err(|e| annotate(e, format_args!("recreate {}", path.display())))?
        }
        Err(e) => return Err(annotate(e, format_args!("create {}", path.display()))),
    };
    f.write_all(contents)
        .map_err(|e| annotate(e, format_args!("write {}", path.display())))?;
    // The umask applies to the open(2) mode argument; set the planned
    // mode explicitly.
    f.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|e| annotate(e, format_args!("chmod {}", path.display())))
}

/// Whether the executor can perform real ownership transfers.
fn is_root() -> bool {
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Attach the failing path/operation to an io::Error so skeleton
/// failures are attributable without a backtrace.
fn annotate(e: io::Error, what: std::fmt::Arguments<'_>) -> io::Error {
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::plan::{HostLayout, SandboxPlan};
    use crate::request::{
        ExecutionRequest, InlineFile, Isolation, Limits, Mount, OutputCapture, Personality,
    };

    /// A request whose mount sources all live under `src_root` so the
    /// skeleton builder's stats hit real files. The caller decides
    /// which sources to actually create.
    fn request(src_root: &Path) -> ExecutionRequest {
        ExecutionRequest {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("sh")],
            env: vec![],
            cwd: PathBuf::from("/build"),
            mounts: vec![
                Mount {
                    source: src_root.join("build"),
                    target: PathBuf::from("/build"),
                    writable: true,
                    optional: false,
                },
                Mount {
                    source: src_root.join("tool"),
                    target: PathBuf::from("/opt/tool"),
                    writable: false,
                    optional: false,
                },
                Mount {
                    source: src_root.join("maybe"),
                    target: PathBuf::from("/opt/maybe"),
                    writable: false,
                    optional: true,
                },
            ],
            extra_devices: vec![],
            inline_files: vec![InlineFile {
                path: PathBuf::from("/build/.netrc"),
                contents: b"machine m\n".to_vec(),
                mode: 0o600,
            }],
            declared_outputs: vec![],
            capture: OutputCapture::MergedPty,
            isolation: Isolation {
                network: false,
                // The executor's own identity so the chown calls are
                // no-ops the kernel always permits; production uses a
                // dedicated uid from a root executor.
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
                personality: Personality::Native,
                hostname: String::from("localhost"),
                deny_setuid_and_xattrs: false,
            },
            limits: Limits {
                timeout: Some(Duration::from_secs(60)),
                max_silent: None,
                max_log_bytes: None,
                cgroup: None,
            },
        }
    }

    /// Compile and build a plan in a fresh tempdir, creating the given
    /// mount sources first. Returns the tempdir (keep it alive), the
    /// chroot dir, and the built plan.
    fn build_in_tempdir(create_sources: &[&str]) -> (tempfile::TempDir, PathBuf, SandboxPlan) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_root = tmp.path().join("sources");
        fs::create_dir_all(src_root.join("build")).expect("mkdir build source");
        for s in create_sources {
            let p = src_root.join(s);
            if *s == "tool" {
                // A regular-file mount source.
                fs::write(&p, b"#!/bin/sh\n").expect("write tool source");
            } else {
                fs::create_dir_all(&p).expect("mkdir source");
            }
        }
        let chroot = tmp.path().join("chroot");
        let req = request(&src_root);
        let mut plan = SandboxPlan::compile(
            &req,
            &HostLayout {
                chroot_dir: chroot.clone(),
            },
        )
        .expect("plan compiles");
        build(&mut plan).expect("skeleton builds");
        (tmp, chroot, plan)
    }

    fn mode_of(p: &Path) -> u32 {
        fs::symlink_metadata(p).expect("lstat").permissions().mode() & 0o7777
    }

    #[test]
    fn builds_the_directory_tree_with_planned_modes() {
        let (_tmp, chroot, _plan) = build_in_tempdir(&["tool"]);
        assert!(chroot.join("tmp").is_dir());
        assert_eq!(mode_of(&chroot.join("tmp")), 0o1777);
        assert!(chroot.join("proc").is_dir());
        assert_eq!(mode_of(&chroot.join("proc")), 0o555);
        assert!(chroot.join("dev/pts").is_dir());
        assert!(chroot.join("dev/shm").is_dir());
        assert!(chroot.join(".real-root").is_dir());
        assert!(chroot.join("build").is_dir(), "cwd placeholder");
        assert_eq!(mode_of(&chroot) & 0o777, 0o750, "chroot root is 0750");
    }

    #[test]
    fn writes_the_etc_files_and_locks_etc() {
        let (_tmp, chroot, _plan) = build_in_tempdir(&["tool"]);
        let passwd = fs::read_to_string(chroot.join("etc/passwd")).expect("passwd");
        assert!(passwd.contains("nixbld:x:"));
        let hosts = fs::read_to_string(chroot.join("etc/hosts")).expect("hosts");
        assert!(hosts.contains("127.0.0.1 localhost"));
        assert_eq!(mode_of(&chroot.join("etc")), 0o555, "etc locked last");
        assert_eq!(mode_of(&chroot.join("etc/passwd")), 0o444);
    }

    #[test]
    fn writes_inline_files_into_the_mount_source() {
        let (tmp, _chroot, _plan) = build_in_tempdir(&["tool"]);
        let netrc = tmp.path().join("sources/build/.netrc");
        assert_eq!(fs::read(&netrc).expect("netrc"), b"machine m\n");
        assert_eq!(mode_of(&netrc), 0o600);
    }

    #[test]
    fn creates_dev_symlinks() {
        let (_tmp, chroot, _plan) = build_in_tempdir(&["tool"]);
        assert_eq!(
            fs::read_link(chroot.join("dev/fd")).expect("dev/fd"),
            Path::new("/proc/self/fd")
        );
        assert_eq!(
            fs::read_link(chroot.join("dev/ptmx")).expect("dev/ptmx"),
            Path::new("/dev/pts/ptmx")
        );
    }

    #[test]
    fn creates_file_targets_for_file_sources_and_dir_targets_for_dirs() {
        let (_tmp, chroot, _plan) = build_in_tempdir(&["tool"]);
        // /opt/tool's source is a regular file -> placeholder file.
        assert!(chroot.join("opt/tool").is_file());
        // /dev/null's source is a (character device) file -> placeholder
        // file, not a directory.
        assert!(chroot.join("dev/null").is_file());
        // /build's source is a directory -> placeholder dir.
        assert!(chroot.join("build").is_dir());
    }

    #[test]
    fn optional_bind_with_missing_source_is_skipped() {
        let (_tmp, chroot, plan) = build_in_tempdir(&["tool"]);
        let maybe = plan
            .binds
            .iter()
            .find(|b| b.target == Path::new("/opt/maybe"))
            .expect("optional bind planned");
        assert!(maybe.skipped, "missing optional source must be skipped");
        assert!(
            !chroot.join("opt/maybe").exists(),
            "no placeholder for a skipped bind"
        );
        let tool = plan
            .binds
            .iter()
            .find(|b| b.target == Path::new("/opt/tool"))
            .expect("required bind planned");
        assert!(!tool.skipped);
    }

    #[test]
    fn nested_symlink_source_is_left_to_the_enclosing_mount() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_root = tmp.path().join("sources");
        fs::create_dir_all(src_root.join("build")).expect("mkdir build source");
        fs::write(src_root.join("tool"), b"#!/bin/sh\n").expect("write tool source");
        // A store-path-input shape: the bind's target sits inside the
        // enclosing writable /build mount, and its source root is a
        // symlink to a sandbox-absolute path that does not exist on the
        // host (linkFarm / `ln -s ${dep} $out`). The enclosing mount's
        // source carries the symlink; the bind itself must do nothing.
        let dep = "/nix/store/00000000000000000000000000000000-dep";
        std::os::unix::fs::symlink(dep, src_root.join("build/link")).expect("symlink source");
        let mut req = request(&src_root);
        req.mounts.push(Mount {
            source: src_root.join("build/link"),
            target: PathBuf::from("/build/link"),
            writable: false,
            optional: false,
        });
        let chroot = tmp.path().join("chroot");
        let mut plan = SandboxPlan::compile(
            &req,
            &HostLayout {
                chroot_dir: chroot.clone(),
            },
        )
        .expect("plan compiles");
        build(&mut plan).expect("skeleton builds despite the host-unresolvable symlink");
        let bind = plan
            .binds
            .iter()
            .find(|b| b.target == Path::new("/build/link"))
            .expect("bind planned");
        assert!(bind.nested, "the layout makes this a nested bind");
        assert!(
            bind.skipped,
            "nested symlink sources are not re-bound over the enclosing mount"
        );
        assert!(
            !chroot.join("build/link").exists()
                && fs::symlink_metadata(chroot.join("build/link")).is_err(),
            "no placeholder is created for it in the skeleton"
        );
        // The enclosing mount's source still carries the symlink the
        // sandbox will actually see.
        assert_eq!(
            fs::read_link(src_root.join("build/link")).expect("symlink intact"),
            Path::new(dep)
        );
    }

    #[test]
    fn resolvable_symlink_source_keeps_the_bind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_root = tmp.path().join("sources");
        fs::create_dir_all(src_root.join("build")).expect("mkdir build source");
        fs::write(src_root.join("tool"), b"#!/bin/sh\n").expect("write tool source");
        // `sh -> tool` next to its target, like a busybox-style sandbox
        // shell: the host resolves it, so the established resolve+bind
        // behavior must be preserved for it.
        std::os::unix::fs::symlink(src_root.join("tool"), src_root.join("sh")).expect("symlink");
        let mut req = request(&src_root);
        req.mounts.push(Mount {
            source: src_root.join("sh"),
            target: PathBuf::from("/opt/sh"),
            writable: false,
            optional: false,
        });
        let chroot = tmp.path().join("chroot");
        let mut plan = SandboxPlan::compile(
            &req,
            &HostLayout {
                chroot_dir: chroot.clone(),
            },
        )
        .expect("plan compiles");
        build(&mut plan).expect("skeleton builds");
        let bind = plan
            .binds
            .iter()
            .find(|b| b.target == Path::new("/opt/sh"))
            .expect("bind planned");
        assert!(
            !bind.skipped,
            "resolvable symlink sources still get bind-mounted"
        );
        assert!(
            chroot.join("opt/sh").is_file(),
            "placeholder is a regular file from the resolved target"
        );
    }

    #[test]
    fn required_bind_with_missing_source_is_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_root = tmp.path().join("sources");
        fs::create_dir_all(src_root.join("build")).expect("mkdir");
        // "tool" deliberately not created.
        let req = request(&src_root);
        let mut plan = SandboxPlan::compile(
            &req,
            &HostLayout {
                chroot_dir: tmp.path().join("chroot"),
            },
        )
        .expect("plan compiles");
        let err = build(&mut plan).expect_err("missing required source");
        assert!(
            err.to_string().contains("tool"),
            "error names the missing source: {err}"
        );
    }

    #[test]
    fn build_is_idempotent() {
        let (tmp, chroot, _plan) = build_in_tempdir(&["tool"]);
        // Re-compile and re-build over the same tree (a retry reuses
        // the chroot dir). /etc was locked to 0o555 by the first run;
        // the second run must converge, not fail.
        let req = request(&tmp.path().join("sources"));
        let mut plan = SandboxPlan::compile(
            &req,
            &HostLayout {
                chroot_dir: chroot.clone(),
            },
        )
        .expect("plan recompiles");
        build(&mut plan).expect("second build converges");
        assert_eq!(mode_of(&chroot.join("etc")), 0o555);
        let passwd = fs::read_to_string(chroot.join("etc/passwd")).expect("passwd");
        assert!(passwd.contains("nixbld:x:"));
    }

    #[test]
    fn replaces_a_planted_symlink_without_writing_through_it() {
        let (tmp, chroot, _plan) = build_in_tempdir(&["tool"]);
        // Replace a synthesized file with a symlink pointing outside
        // the skeleton, then re-build: the create_new open never
        // follows an existing link, so the link is unlinked and
        // replaced with a fresh regular file and the link's target is
        // never written to.
        let etc = chroot.join("etc");
        fs::set_permissions(&etc, fs::Permissions::from_mode(0o755)).expect("unlock etc");
        let target = tmp.path().join("victim");
        fs::write(&target, b"untouched").expect("victim");
        fs::remove_file(etc.join("passwd")).expect("rm passwd");
        std::os::unix::fs::symlink(&target, etc.join("passwd")).expect("symlink");
        let req = request(&tmp.path().join("sources"));
        let mut plan = SandboxPlan::compile(
            &req,
            &HostLayout {
                chroot_dir: chroot.clone(),
            },
        )
        .expect("plan recompiles");
        build(&mut plan).expect("rebuild converges over the planted link");
        assert_eq!(
            fs::read(&target).expect("victim"),
            b"untouched",
            "the symlink's target must never be written through"
        );
        let meta = fs::symlink_metadata(etc.join("passwd")).expect("passwd");
        assert!(
            meta.is_file() && !meta.is_symlink(),
            "the planted link is replaced by a regular file"
        );
        let passwd = fs::read_to_string(etc.join("passwd")).expect("passwd");
        assert!(passwd.contains("nixbld:x:"));
    }
}

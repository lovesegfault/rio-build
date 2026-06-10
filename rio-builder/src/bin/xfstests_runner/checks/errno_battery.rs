//! Error-path checks: permission enforcement for the unprivileged
//! build uid, EEXIST precedence, the missing-name errno contract, and
//! the root-credential probes that reach the FUSE daemon's own write
//! handlers (the generic/050+294 "read-only filesystem" intent).
//!
//! Two distinct legs, asserting two distinct enforcement layers:
//!
//! * **Unprivileged leg** (`PrivDrop` to the build uid): the castore
//!   mount is read-only at the VFS level, so mnt_want_write rejects
//!   every mutation with EROFS before the mode bits or the FUSE
//!   daemon are consulted — the same answers a ro-tmpfs gives.
//!   `access(W_OK)` is denied EACCES instead: the ro flag lands
//!   per-mount, and DAC over the root-owned 0444/0555 attrs
//!   (`default_permissions`) runs before faccessat's readonly-mount
//!   check, which only DAC-passing (root) callers reach.
//!
//! * **Root leg**: root holds CAP_DAC_OVERRIDE; on an MS_RDONLY mount
//!   the VFS still answers EROFS for mutations, and any op that does
//!   reach the FUSE daemon is denied EROFS by its write-op handlers
//!   (`r[builder.fs.write-ops-erofs]`). The probe accepts EROFS or the
//!   historically-documented fuser default (PLAN.md F-C/F-D) so a
//!   divergence cannot drift silently.

use std::fs;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use nix::errno::Errno;
use nix::fcntl::AT_FDCWD;
use nix::sys::stat::{UtimensatFlags, utimensat};
use nix::sys::time::TimeSpec;
use nix::unistd::{AccessFlags, eaccess};

use super::{Ctx, Outcome, PrivDrop, cpath, errno_of, expect_errno, open_raw, wait_for};
use crate::manifest::FileSpec;

/// generic/126: the executable bit is the only mode bit castore
/// preserves and it decides whether builds can exec their inputs.
/// Exec of the executable twin succeeds, exec of the byte-identical
/// non-executable twin fails with EACCES, and access(2) answers
/// honestly from the served modes (the predecessor JIT-FUSE answered
/// the access upcall ignoring the mask — old finding F-1; with
/// default_permissions there is no upcall to get wrong, and this pins
/// that).
pub fn generic_126_exec_access(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let exec = ctx
        .manifest
        .files
        .iter()
        .find(|f| f.executable)
        .context("manifest has no executable file")?;
    let twin = ctx
        .manifest
        .files
        .iter()
        .find(|f| !f.executable && f.content == exec.content)
        .context("manifest has no non-executable twin of the executable file")?;
    let plain = plain_unique_file(ctx)?;

    // Exec as the build uid (fork+exec with setuid, no shell).
    let out = std::process::Command::new(ctx.on_mount(&exec.path))
        .uid(ctx.probe_uid)
        .gid(ctx.probe_gid)
        .output()
        .with_context(|| format!("exec {} as the build uid", exec.path))?;
    ensure!(
        out.status.success(),
        "exec of {} as the build uid failed: {:?}",
        exec.path,
        out
    );
    ensure!(
        out.stdout == b"tool-ok\n",
        "exec of {} printed \"{}\", expected \"tool-ok\\n\"",
        exec.path,
        out.stdout.escape_ascii()
    );

    let twin_exec = std::process::Command::new(ctx.on_mount(&twin.path))
        .uid(ctx.probe_uid)
        .gid(ctx.probe_gid)
        .output();
    match twin_exec {
        Ok(out) => bail!(
            "exec of the non-executable {} unexpectedly ran: {out:?}",
            twin.path
        ),
        Err(e) => ensure!(
            errno_of(&e) == Errno::EACCES,
            "exec of the non-executable {} failed with {:?}, expected EACCES",
            twin.path,
            errno_of(&e)
        ),
    }

    // access(2) honesty with the build uid's effective ids.
    {
        let _guard = PrivDrop::to(ctx.probe_uid, ctx.probe_gid)?;
        let probes: [(&str, &str, AccessFlags, Option<Errno>); 5] = [
            (
                "X_OK on the executable",
                &exec.path,
                AccessFlags::X_OK,
                None,
            ),
            (
                "X_OK on the non-executable twin",
                &twin.path,
                AccessFlags::X_OK,
                Some(Errno::EACCES),
            ),
            (
                "R_OK on a read-only input",
                &plain.path,
                AccessFlags::R_OK,
                None,
            ),
            // The ro flag lands per-mount (MNT_READONLY), not on the
            // FUSE superblock, so for an UNPRIVILEGED caller the
            // 0444/0555 DAC denial (EACCES, inside inode_permission)
            // fires before faccessat's __mnt_is_readonly check ever
            // runs. Only a caller that passes DAC — root via
            // CAP_DAC_OVERRIDE — reaches the mnt check and sees EROFS;
            // that surface is pinned by mount_readonly_honesty's root
            // leg. Either way W_OK is DENIED at the access check, which
            // is what `test -w` probers need.
            (
                "W_OK on a read-only input",
                &plain.path,
                AccessFlags::W_OK,
                Some(Errno::EACCES),
            ),
            (
                "W_OK on an input directory",
                &ctx.manifest.seq_dir.path,
                AccessFlags::W_OK,
                Some(Errno::EACCES),
            ),
        ];
        for (what, rel, flags, expect) in probes {
            let res = eaccess(&ctx.on_mount(rel), flags);
            match expect {
                None => ensure!(res.is_ok(), "{what} ({rel}): denied with {res:?}"),
                Some(errno) => ensure!(
                    res == Err(errno),
                    "{what} ({rel}): got {res:?}, expected {errno:?}"
                ),
            }
        }
    }
    Ok(Outcome::Pass)
}

/// generic/050 + generic/123 (adapted): every mutation attempted by
/// the build uid fails with EROFS — the read-only mount makes the
/// kernel's mnt_want_write deny writes before the mode bits (which
/// would say EACCES/EPERM) are even consulted — and the tree is
/// byte-identical afterwards. generic/123's four operations
/// (overwrite, append, delete, move of a root-created file) are
/// exactly the O_TRUNC, O_APPEND, unlink, and rename probes below, so
/// it folds in here rather than as a separate check. A regression lets
/// builds scribble on (or appear to scribble on) shared inputs.
pub fn generic_050_write_protection_unprivileged(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let plain = plain_unique_file(ctx)?;
    let exec = ctx
        .manifest
        .files
        .iter()
        .find(|f| f.executable)
        .context("manifest has no executable file")?;
    let small = ctx.on_mount(&plain.path);
    let big = ctx.on_mount(&ctx.manifest.big_file.path);
    let tool = ctx.on_mount(&exec.path);
    let new_file = ctx.dep_root.join("uxx-new-file");
    let new_dir = ctx.dep_root.join("uxx-new-dir");
    let new_symlink = ctx.dep_root.join("uxx-new-symlink");
    let renamed = small.with_file_name("uxx-renamed");
    let ts = TimeSpec::new(1234567, 0);

    let top_level_before = fs::read_dir(&ctx.dep_root)?.count();

    {
        let _guard = PrivDrop::to(ctx.probe_uid, ctx.probe_gid)?;
        type Probe<'a> = (&'a str, Errno, Box<dyn Fn() -> io::Result<()> + 'a>);
        let probes: Vec<Probe> = vec![
            (
                "open(O_WRONLY|O_TRUNC) on a read-only input",
                Errno::EROFS,
                Box::new(|| {
                    fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(&small)
                        .map(drop)
                }),
            ),
            (
                "open(O_WRONLY|O_APPEND) on a read-only input",
                Errno::EROFS,
                Box::new(|| fs::OpenOptions::new().append(true).open(&small).map(drop)),
            ),
            (
                "create a new file",
                Errno::EROFS,
                Box::new(|| {
                    fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&new_file)
                        .map(drop)
                }),
            ),
            (
                "mkdir a new directory",
                Errno::EROFS,
                Box::new(|| fs::create_dir(&new_dir)),
            ),
            (
                "unlink a read-only input",
                Errno::EROFS,
                Box::new(|| fs::remove_file(&small)),
            ),
            (
                "rename a read-only input",
                Errno::EROFS,
                Box::new(|| fs::rename(&small, &renamed)),
            ),
            (
                "create a symlink",
                Errno::EROFS,
                Box::new(|| std::os::unix::fs::symlink("foo", &new_symlink)),
            ),
            (
                "truncate a read-only input",
                Errno::EROFS,
                Box::new(|| nix::unistd::truncate(&big, 1).map_err(io::Error::from)),
            ),
            (
                "chmod a read-only input",
                Errno::EROFS,
                Box::new(|| fs::set_permissions(&tool, fs::Permissions::from_mode(0o700))),
            ),
            (
                "utimensat with explicit times",
                Errno::EROFS,
                Box::new(|| {
                    utimensat(AT_FDCWD, &small, &ts, &ts, UtimensatFlags::FollowSymlink)
                        .map_err(io::Error::from)
                }),
            ),
        ];
        for (what, expected, probe) in &probes {
            expect_errno(what, probe(), &[*expected])?;
        }
    }

    // Nothing changed: content, modes, and the top-level entry set are
    // exactly what they were.
    ensure!(
        fs::read(&small)? == plain.content.as_bytes(),
        "tree changed: {} content differs after the denied mutations",
        plain.path
    );
    ensure!(
        fs::symlink_metadata(&tool)?.mode() & 0o7777 == 0o555,
        "tree changed: {} mode differs after the denied chmod",
        exec.path
    );
    let top_level_after = fs::read_dir(&ctx.dep_root)?.count();
    ensure!(
        top_level_after == top_level_before,
        "tree changed: dep root has {top_level_after} entries, had {top_level_before}"
    );
    ensure!(
        !new_file.exists() && !new_dir.exists() && !new_symlink.exists() && !renamed.exists(),
        "tree changed: a denied creation left an entry behind"
    );
    Ok(Outcome::Pass)
}

/// generic/294: creating a name that already exists reports EEXIST —
/// the dentry resolves through lookup before the write-permission
/// failure, so EEXIST (not EACCES) is the errno tools key on to decide
/// "already there" vs "forbidden".
pub fn generic_294_eexist_unprivileged(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let plain = plain_unique_file(ctx)?;
    let existing_dir = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let existing_file = ctx.on_mount(&plain.path);

    let _guard = PrivDrop::to(ctx.probe_uid, ctx.probe_gid)?;
    expect_errno(
        "mkdir over an existing directory",
        fs::create_dir(&existing_dir),
        &[Errno::EEXIST],
    )?;
    expect_errno(
        "symlink over an existing file",
        std::os::unix::fs::symlink("whatever", &existing_file),
        &[Errno::EEXIST],
    )?;
    expect_errno(
        "exclusive create over an existing file",
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&existing_file)
            .map(drop),
        &[Errno::EEXIST],
    )?;
    Ok(Outcome::Pass)
}

/// generic/050 + generic/294 (root leg): operations that pass the
/// kernel's default_permissions check (root holds CAP_DAC_OVERRIDE)
/// reach the FUSE daemon itself. POSIX/xfstests expect every mutation
/// of a read-only filesystem to fail with EROFS; the MS_RDONLY mount
/// answers that in the VFS, and anything that still reaches the FUSE
/// daemon is denied EROFS by its write-op handlers
/// (`r[builder.fs.write-ops-erofs]`). Asserted as `EROFS or the
/// historically-documented actual` (PLAN.md F-D: fuser's
/// ENOSYS/EPERM defaults from before those layers existed) so any
/// divergence is pinned and printed as FINDING lines for the VM log.
pub fn generic_294_erofs_battery_root(ctx: &Ctx) -> anyhow::Result<Outcome> {
    ensure!(
        nix::unistd::geteuid().is_root(),
        "the root-leg battery must run as root (CAP_DAC_OVERRIDE is the point)"
    );
    let plain = plain_unique_file(ctx)?;
    let exec = ctx
        .manifest
        .files
        .iter()
        .find(|f| f.executable)
        .context("manifest has no executable file")?;
    let small = ctx.on_mount(&plain.path);
    let big = ctx.on_mount(&ctx.manifest.big_file.path);
    let tool = ctx.on_mount(&exec.path);
    let seq_dir = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let new_file = ctx.dep_root.join("rxx-new-file");
    let new_dir = ctx.dep_root.join("rxx-new-dir");
    let new_symlink = ctx.dep_root.join("rxx-new-symlink");
    let new_hardlink = ctx.dep_root.join("rxx-new-hardlink");
    let renamed = small.with_file_name("rxx-renamed");
    let ts = TimeSpec::new(1234567, 0);

    let top_level_before = fs::read_dir(&ctx.dep_root)?.count();

    type Probe<'a> = (&'a str, Errno, Box<dyn Fn() -> io::Result<()> + 'a>);
    let probes: Vec<Probe> = vec![
        (
            "unlink",
            Errno::ENOSYS,
            Box::new(|| fs::remove_file(&small)),
        ),
        (
            "mkdir",
            Errno::ENOSYS,
            Box::new(|| fs::create_dir(&new_dir)),
        ),
        (
            "rmdir",
            Errno::ENOSYS,
            Box::new(|| fs::remove_dir(&seq_dir)),
        ),
        (
            "create (O_CREAT|O_EXCL)",
            Errno::ENOSYS,
            Box::new(|| {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&new_file)
                    .map(drop)
            }),
        ),
        (
            "rename",
            Errno::ENOSYS,
            Box::new(|| fs::rename(&small, &renamed)),
        ),
        (
            "symlink",
            Errno::EPERM,
            Box::new(|| std::os::unix::fs::symlink("foo", &new_symlink)),
        ),
        (
            "hard link",
            Errno::EPERM,
            Box::new(|| fs::hard_link(&tool, &new_hardlink)),
        ),
        (
            "chmod",
            Errno::ENOSYS,
            Box::new(|| fs::set_permissions(&tool, fs::Permissions::from_mode(0o700))),
        ),
        (
            "truncate",
            Errno::ENOSYS,
            Box::new(|| nix::unistd::truncate(&big, 1).map_err(io::Error::from)),
        ),
        (
            "utimensat",
            Errno::ENOSYS,
            Box::new(|| {
                utimensat(AT_FDCWD, &small, &ts, &ts, UtimensatFlags::FollowSymlink)
                    .map_err(io::Error::from)
            }),
        ),
    ];

    for (op, documented, probe) in &probes {
        let actual = expect_errno(
            &format!("{op} as root"),
            probe(),
            &[Errno::EROFS, *documented],
        )?;
        if actual == Errno::EROFS {
            println!("    {op} as root -> EROFS (POSIX-conformant)");
        } else {
            println!("    FINDING F-D: {op} as root -> {actual:?} (POSIX/xfstests expect EROFS)");
        }
    }

    // No probe mutated anything.
    ensure!(
        fs::read(&small)? == plain.content.as_bytes(),
        "tree changed: {} content differs after the root-leg probes",
        plain.path
    );
    ensure!(
        fs::symlink_metadata(&tool)?.mode() & 0o7777 == 0o555,
        "tree changed: {} mode differs after the root-leg chmod",
        exec.path
    );
    ensure!(
        fs::symlink_metadata(&big)?.len() == ctx.manifest.big_file.size,
        "tree changed: {} size differs after the root-leg truncate",
        ctx.manifest.big_file.path
    );
    let top_level_after = fs::read_dir(&ctx.dep_root)?.count();
    ensure!(
        top_level_after == top_level_before,
        "tree changed: dep root has {top_level_after} entries, had {top_level_before}"
    );
    Ok(Outcome::Pass)
}

/// generic/007 (nametest): names outside the prefetched closure answer
/// ENOENT — never EIO, never a stall on a store fetch (the closure is
/// the allowlist). The misses are negative-cached with an infinite
/// TTL, so hammering the same missing name must stay ENOENT and must
/// not poison resolution of real entries.
pub fn generic_007_enoent_never_eio(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let plain = plain_unique_file(ctx)?;
    let missing: Vec<PathBuf> = vec![
        ctx.dep_root.join("no-such-entry"),
        ctx.on_mount(&format!(
            "{}/f{}",
            ctx.manifest.seq_dir.path,
            ctx.manifest.seq_dir.count + 1
        )),
        ctx.dep_root.join("names/missing"),
        // A plausible store-path basename that is not part of the
        // mounted closure, probed at the mount root.
        ctx.mount
            .join("00000000000000000000000000000000-rio-xfstests-not-an-input"),
    ];
    for path in &missing {
        let res = fs::symlink_metadata(path);
        let Err(e) = res else {
            bail!(
                "stat of missing path {} unexpectedly succeeded",
                path.display()
            );
        };
        let errno = errno_of(&e);
        ensure!(
            errno != Errno::EIO,
            "EIO leaked for missing path {}",
            path.display()
        );
        ensure!(
            errno == Errno::ENOENT,
            "stat of missing path {} gave {errno:?}, expected ENOENT",
            path.display()
        );
    }

    // Negative-dentry cache: 50 repeats stay ENOENT...
    for _ in 0..50 {
        expect_errno(
            "repeated stat of a cached-negative name",
            fs::symlink_metadata(&missing[0]).map(drop),
            &[Errno::ENOENT],
        )?;
    }
    // ...and real content still resolves afterwards.
    ensure!(
        fs::read(ctx.on_mount(&plain.path))? == plain.content.as_bytes(),
        "negative caching poisoned resolution of {}",
        plain.path
    );
    Ok(Outcome::Pass)
}

/// statfs sanity: statvfs on the mount succeeds and reports a sane
/// NAME_MAX. FINDING F-A (PLAN.md): the castore-FUSE replies the
/// conventional empty statfs (all-zero block/file totals) —
/// harmless for builds that only read inputs, but tools that pre-check
/// free space on an input path see 0. Asserted as `0 or >0` so a real
/// statfs implementation keeps this green; the actual totals are
/// printed for the VM log.
pub fn statfs_zero_totals(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let vfs = nix::sys::statvfs::statvfs(&ctx.dep_root).context("statvfs on the mount")?;
    ensure!(
        vfs.name_max() == 255,
        "statfs NAME_MAX is {}, expected 255",
        vfs.name_max()
    );
    if vfs.blocks() == 0 {
        println!(
            "    FINDING F-A: statfs reports 0 total blocks (the castore-FUSE empty statfs reply)"
        );
    } else {
        println!(
            "    statfs reports {} blocks of {} bytes",
            vfs.blocks(),
            vfs.fragment_size()
        );
    }
    Ok(Outcome::Pass)
}

/// generic/050 (root write leg) — the write-through probe.
///
/// POSIX: open(O_WRONLY) on a read-only filesystem fails with EROFS,
/// so write-through is impossible. On a castore-FUSE whose `open()`
/// handler ignored the access mode, root's open(O_WRONLY) would (for
/// a cache-hit file) get a FOPEN_PASSTHROUGH reply with a backing
/// id. The kernel then opens the backing cache file with the
/// caller's flags under the BACKING_OPEN broker's credentials
/// (rio-mountd, root) — `backing_file_open` performs no DAC check —
/// and write(2) goes straight into the node-shared cache file.
///
/// FINDING F-C (PLAN.md): a root-equivalent process on the builder
/// node can silently corrupt the shared content-addressed cache
/// through any castore mount; the corruption is then served to every
/// build on the node that reads that digest. Build processes cannot
/// reach this (they fail at default_permissions), so this is a
/// host-trust boundary note, not a build-escape — but xfstests
/// generic/050 would flag it, and so does this check.
///
/// The probe asserts whichever of the two known behaviors holds
/// (EROFS-style denial = fixed, or write-through = documented finding),
/// fails on anything else, and repairs the cache file afterwards so
/// the rest of the suite and the shared cache see the original bytes.
pub fn write_through_passthrough_root(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let Some(cache_dir) = &ctx.cache_dir else {
        // Without the cache dir we could not repair a successful
        // write-through, so don't attempt one.
        return Ok(Outcome::Skip("no --cache-dir given (cannot repair)"));
    };
    ensure!(
        nix::unistd::geteuid().is_root(),
        "the write-through probe must run as root"
    );

    let plain = plain_unique_file(ctx)?;
    let target = ctx.on_mount(&plain.path);
    let original = fs::read(&target)?;
    ensure!(
        original == plain.content.as_bytes(),
        "{} does not have its expected content before the probe",
        plain.path
    );

    // The probe is about the passthrough write path, so the digest must
    // be in the shared cache (earlier checks read this file; promotion
    // is asynchronous).
    let digest_hex = blake3::hash(&original).to_hex();
    let cache_file = cache_dir
        .join(&digest_hex.as_str()[..2])
        .join(digest_hex.as_str());
    wait_for(
        "probe target digest to appear in the shared cache",
        Duration::from_secs(120),
        || cache_file.exists(),
    )?;

    let probe_bytes = b"RIO-XFSTESTS-WRITE-THROUGH-PROBE";
    let open_res = fs::OpenOptions::new().write(true).open(&target);
    let file = match open_res {
        Err(e) => {
            let errno = errno_of(&e);
            ensure!(
                matches!(
                    errno,
                    Errno::EROFS | Errno::EACCES | Errno::EPERM | Errno::EIO
                ),
                "open(O_WRONLY) as root failed with unexpected errno {errno:?}"
            );
            println!(
                "    open(O_WRONLY) as root refused with {errno:?} — write-through not possible \
                 (finding F-C no longer applies)"
            );
            return Ok(Outcome::Pass);
        }
        Ok(f) => {
            println!(
                "    FINDING F-C: open(O_WRONLY) as root SUCCEEDED on the castore mount \
                 (POSIX expects EROFS)"
            );
            f
        }
    };

    let write_res = file.write_at(probe_bytes, 0);
    drop(file);
    let after = fs::read(&target)?;
    let mutated = after != original;

    let outcome = match (&write_res, mutated) {
        (Err(e), false) => {
            println!(
                "    write(2) through the root O_WRONLY fd failed with {:?}; content unchanged \
                 (write-through blocked at the write stage)",
                errno_of(e)
            );
            Ok(Outcome::Pass)
        }
        (Ok(n), true) => {
            println!(
                "    FINDING F-C: write(2) as root wrote {n} bytes THROUGH the mount into the \
                 shared backing cache — content served by the FUSE changed"
            );
            ensure!(
                fs::read(&cache_file)?.starts_with(probe_bytes),
                "content through the mount changed but the cache file does not hold the probe \
                 bytes — unknown write path, investigate"
            );
            Ok(Outcome::Pass)
        }
        (Ok(n), false) => bail!(
            "write(2) as root claimed to write {n} bytes but content through the mount is \
             unchanged — unknown write path, investigate"
        ),
        (Err(e), true) => bail!(
            "write(2) as root failed with {:?} but content through the mount changed",
            errno_of(e)
        ),
    };

    // Repair: restore the original bytes in the backing cache file so
    // the shared cache (and anything else reading this digest) is
    // unharmed. Root writes the cache file directly on the host fs.
    if mutated {
        let repair = fs::OpenOptions::new()
            .write(true)
            .open(&cache_file)
            .context("open cache file for repair")?;
        repair
            .write_all_at(&original, 0)
            .context("rewrite original bytes")?;
        repair
            .set_len(original.len() as u64)
            .context("truncate cache file back to the original size")?;
        drop(repair);
        let healed = fs::read(&target)?;
        ensure!(
            healed == original,
            "cache repair did not restore the original content through the mount"
        );
        println!("    (backing cache file repaired to the original bytes)");
    }
    outcome
}

/// Read-only honesty of the mount itself (the xfstests `_require`
/// ro-mount intent): a mount that serves an immutable tree must SAY
/// so. Three surfaces, each consumed by real tooling:
///
/// * `statvfs().f_flag` carries ST_RDONLY — rsync/install pre-check it;
/// * the mount's options in /proc/self/mounts say `ro` — mount(8),
///   findmnt, and container runtimes read it;
/// * `faccessat2(W_OK)` as root fails with EROFS — on an MS_RDONLY
///   mount the kernel answers before any permission logic. A mount
///   that is secretly rw passes W_OK (root holds CAP_DAC_OVERRIDE) and
///   only refuses at open/write time, so tools that probe-then-write
///   fail late with confusing errors.
///
/// RED on the pre-fix castore-FUSE: the mount was not MS_RDONLY (write
/// protection was default_permissions + the daemon's EROFS table), so
/// all three surfaces claimed writability.
pub fn mount_readonly_honesty(ctx: &Ctx) -> anyhow::Result<Outcome> {
    use nix::libc;

    let mount_c = cpath(&ctx.mount);

    // statvfs: ST_RDONLY advertised.
    // SAFETY: zeroed statvfs is a valid out-buffer; rc checked.
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(mount_c.as_ptr(), &mut vfs) };
    ensure!(
        rc == 0,
        "statvfs({}) failed: {}",
        ctx.mount.display(),
        io::Error::last_os_error()
    );
    ensure!(
        vfs.f_flag & libc::ST_RDONLY != 0,
        "statvfs f_flag {:#x} lacks ST_RDONLY — the mount does not advertise itself \
         read-only, so writability pre-checks (rsync, install) pass and fail late",
        vfs.f_flag
    );

    // /proc/self/mounts: the options field must say ro.
    let mounts = fs::read_to_string("/proc/self/mounts").context("read /proc/self/mounts")?;
    let mount_str = ctx.mount.to_str().context("mount path is not UTF-8")?;
    let line = mounts
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some(mount_str))
        .with_context(|| format!("{mount_str} not in /proc/self/mounts"))?;
    let options = line
        .split_whitespace()
        .nth(3)
        .context("malformed /proc/self/mounts line")?;
    ensure!(
        options.split(',').any(|o| o == "ro"),
        "mount options are \"{options}\" — expected an `ro` mount, got rw"
    );

    // faccessat2(W_OK) as root: EROFS from the MS_RDONLY check, not a
    // CAP_DAC_OVERRIDE pass.
    ensure!(
        nix::unistd::geteuid().is_root(),
        "the W_OK honesty leg must run as root (CAP_DAC_OVERRIDE is the point)"
    );
    let probe = plain_unique_file(ctx)?;
    let file_c = cpath(&ctx.on_mount(&probe.path));
    // SAFETY: valid C path; no out-pointers.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            libc::AT_FDCWD,
            file_c.as_ptr(),
            libc::W_OK,
            0,
        )
    };
    let errno = Errno::last();
    ensure!(
        rc == -1 && errno == Errno::EROFS,
        "faccessat2({}, W_OK) as root returned {rc} ({errno:?}) — on a read-only mount the \
         kernel must answer EROFS; a pass here means the mount is secretly rw and write \
         refusal only happens at open time",
        probe.path
    );
    Ok(Outcome::Pass)
}

/// generic/006 (name-limit leg): the errno contract at the NAME_MAX /
/// PATH_MAX boundaries. A 256-byte component must be ENAMETOOLONG —
/// per-component NAME_MAX enforcement is the FILESYSTEM's job (the
/// kernel only rejects past FUSE_NAME_MAX=1024), and the daemon's
/// lookup handler gates components past its advertised NAME_MAX
/// (finding F-F, fixed). A legal-length missing name is plain ENOENT,
/// and a path longer than PATH_MAX is ENAMETOOLONG (enforced by
/// getname before any lookup). Tools that build deep paths (tar,
/// install -D) branch on exactly these errnos.
pub fn generic_006_name_limits(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let names_parent = ctx.on_mount("names");

    // One past NAME_MAX: POSIX says ENAMETOOLONG, strictly. ENOENT
    // here is a regression of the F-F fix (the lookup handler must
    // reject over-long names before the negative-entry path).
    let too_long = names_parent.join("n".repeat(256));
    expect_errno(
        "lstat of a 256-byte name",
        fs::symlink_metadata(&too_long),
        &[Errno::ENAMETOOLONG],
    )?;

    // Exactly NAME_MAX but nonexistent: a legal name that is simply
    // absent — ENOENT (the negative-entry contract, not a length error).
    let absent_max = names_parent.join("z".repeat(255));
    expect_errno(
        "lstat of an absent NAME_MAX name",
        fs::symlink_metadata(&absent_max),
        &[Errno::ENOENT],
    )?;

    // Total path beyond PATH_MAX (4096): ENAMETOOLONG.
    let mut deep = ctx.dep_root.clone();
    while deep.as_os_str().len() <= 4096 {
        deep.push("d");
    }
    expect_errno(
        "lstat of a > PATH_MAX path",
        fs::symlink_metadata(&deep),
        &[Errno::ENAMETOOLONG],
    )?;
    Ok(Outcome::Pass)
}

/// open(2) flag contracts on a read-only tree (generic/004's O_TMPFILE
/// refusal + generic/763's zero-byte-write leg + the open(2) flag
/// errnos a build's tooling branches on):
///
/// * `O_DIRECTORY` on a regular file → ENOTDIR
/// * `O_NOFOLLOW` on a symlink → ELOOP
/// * `O_PATH|O_NOFOLLOW` on a symlink → an fd to the LINK itself
///   (fstat reports S_IFLNK and size == strlen(target))
/// * `O_TMPFILE` on an input dir → refused cleanly (EOPNOTSUPP from a
///   FUSE without the tmpfile op, or EROFS/EACCES — never a created
///   inode)
/// * `write()` through an O_RDONLY fd → EBADF, even for zero bytes
pub fn open_flag_contracts(ctx: &Ctx) -> anyhow::Result<Outcome> {
    use std::os::fd::AsRawFd;

    use nix::libc;

    let plain = plain_unique_file(ctx)?;
    let plain_path = ctx.on_mount(&plain.path);
    let symlink = ctx
        .manifest
        .symlinks
        .first()
        .context("manifest has no symlinks")?;
    let symlink_path = ctx.on_mount(&symlink.path);

    expect_errno(
        "open(O_DIRECTORY) on a regular file",
        open_raw(&plain_path, libc::O_RDONLY | libc::O_DIRECTORY),
        &[Errno::ENOTDIR],
    )?;
    expect_errno(
        "open(O_NOFOLLOW) on a symlink",
        open_raw(&symlink_path, libc::O_RDONLY | libc::O_NOFOLLOW),
        &[Errno::ELOOP],
    )?;

    // O_PATH|O_NOFOLLOW yields a handle to the symlink itself.
    let link_fd = open_raw(&symlink_path, libc::O_PATH | libc::O_NOFOLLOW)
        .context("open(O_PATH|O_NOFOLLOW) on a symlink")?;
    let st = nix::sys::stat::fstat(&link_fd)?;
    ensure!(
        st.st_mode & libc::S_IFMT == libc::S_IFLNK,
        "O_PATH|O_NOFOLLOW fd of {} is not a symlink (mode {:o})",
        symlink.path,
        st.st_mode
    );
    ensure!(
        st.st_size as u64 == symlink.target.len() as u64,
        "O_PATH symlink fd size {} != target length {}",
        st.st_size,
        symlink.target.len()
    );

    // O_PATH on a file: stat through the fd matches lstat by identity.
    let path_fd = open_raw(&plain_path, libc::O_PATH).context("open(O_PATH) on a file")?;
    let via_fd = nix::sys::stat::fstat(&path_fd)?;
    let via_lstat = fs::symlink_metadata(&plain_path)?;
    ensure!(
        via_fd.st_ino == via_lstat.ino() && via_fd.st_dev == via_lstat.dev(),
        "O_PATH fstat identity (dev={}, ino={}) != lstat (dev={}, ino={})",
        via_fd.st_dev,
        via_fd.st_ino,
        via_lstat.dev(),
        via_lstat.ino()
    );

    // O_TMPFILE on an input dir must be refused, never create an inode.
    let dir_path = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let before = fs::read_dir(&dir_path)?.count();
    let tmpfile = expect_errno(
        "open(O_TMPFILE|O_WRONLY) on an input dir",
        open_raw(&dir_path, libc::O_TMPFILE | libc::O_WRONLY),
        &[Errno::EOPNOTSUPP, Errno::EROFS, Errno::EACCES],
    )?;
    println!("    O_TMPFILE refused with {tmpfile:?}");
    ensure!(
        fs::read_dir(&dir_path)?.count() == before,
        "the refused O_TMPFILE changed the directory's entry count"
    );

    // write(2) through an O_RDONLY fd: EBADF — including the zero-byte
    // write (vfs_write checks FMODE_WRITE before looking at the count).
    let ro = fs::File::open(&plain_path)?;
    for len in [0usize, 1] {
        let buf = [0u8; 1];
        // SAFETY: valid fd and buffer; len <= buf.len().
        let n = unsafe { libc::write(ro.as_raw_fd(), buf.as_ptr().cast(), len) };
        let errno = Errno::last();
        ensure!(
            n == -1 && errno == Errno::EBADF,
            "write of {len} bytes through an O_RDONLY fd returned {n}/{errno:?}, expected \
             -1/EBADF"
        );
    }
    Ok(Outcome::Pass)
}

// ─── helpers ───────────────────────────────────────────────────────────

/// The probe target for write/permission checks: a non-executable,
/// non-empty file whose content is unique in the fixture (so a
/// corrupted shared digest cannot alias another file's checks).
fn plain_unique_file(ctx: &Ctx) -> anyhow::Result<&FileSpec> {
    ctx.manifest
        .files
        .iter()
        .find(|f| {
            !f.executable
                && !f.content.is_empty()
                && ctx
                    .manifest
                    .files
                    .iter()
                    .filter(|other| other.content == f.content)
                    .count()
                    == 1
        })
        .context("manifest has no unique-content plain file for the write probes")
}

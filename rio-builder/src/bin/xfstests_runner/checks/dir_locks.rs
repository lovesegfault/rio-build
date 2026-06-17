//! Directory-stream cookies, POSIX/flock locks, and second-uid DAC —
//! the kernel surfaces `std::fs::read_dir` and the permission/errno
//! batteries cannot reach.
//!
//! * `rewinddir`/`seekdir`/`telldir` round-trip through glibc's
//!   directory cookies, which the kernel hands to and resumes from the
//!   FUSE READDIR offset bookkeeping (`tree::InoMap::readdir`). The
//!   userspace tier already proves in-range offset resume exhaustively;
//!   these add the glibc cookie path and out-of-range/garbage offsets
//!   against the live mount, where a bad offset can surface as an
//!   EIO/loop instead of a panic-free empty listing.
//! * POSIX record locks and flock on a read-only input: a build's
//!   configure scripts and any sqlite-backed input take read locks, so
//!   they must be granted and tracked kernel-locally — the castore
//!   daemon implements no lock ops, so anything other than local
//!   kernel handling would corrupt a shared input silently.
//! * DAC enforcement for a SECOND unprivileged uid (not the uid that
//!   mounted the FUSE): pins that `allow_other` + `default_permissions`
//!   apply the served root-owned modes to arbitrary uids, not just the
//!   builder's.

use std::fs;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::{Context, ensure};
use nix::errno::Errno;
use nix::libc;
use nix::unistd::{AccessFlags, eaccess};

use super::{Ctx, Outcome, PrivDrop, RawDir, expect_errno, open_raw, readable_plain_file};

/// generic/471: rewinddir resets an open directory stream to the start —
/// reading it again yields the identical complete listing, and a rewind
/// after a partial read restarts from the first entry. The castore FUSE
/// serves cached dirent pages (`FOPEN_CACHE_DIR`); an offset-0 reset
/// that skipped or duplicated entries would make a second `ls` of an
/// input directory disagree with the first.
pub fn generic_471_rewinddir(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let dir = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let count = ctx.manifest.seq_dir.count;
    let expected: std::collections::BTreeSet<Vec<u8>> =
        (1..=count).map(|i| format!("f{i}").into_bytes()).collect();

    let d = RawDir::open(&dir)?;
    let first = d.names()?;
    ensure!(
        first == expected,
        "initial listing of {} is wrong ({} entries)",
        ctx.manifest.seq_dir.path,
        first.len()
    );
    d.rewind();
    let after_full = d.names()?;
    ensure!(
        after_full == expected,
        "rewinddir after a full read did not re-yield the identical listing"
    );

    // Rewind after a PARTIAL read must restart from the beginning, not
    // continue from where the partial read stopped.
    let d2 = RawDir::open(&dir)?;
    for _ in 0..count / 2 {
        d2.next_entry()?;
    }
    d2.rewind();
    let after_partial = d2.names()?;
    ensure!(
        after_partial == expected,
        "rewinddir after a partial read did not restart from the first entry \
         ({} entries seen)",
        after_partial.len()
    );
    Ok(Outcome::Pass)
}

/// generic/676: seekdir to a cookie from telldir resumes at exactly that
/// entry, and seekdir to a garbage/out-of-range cookie produces a sane
/// (possibly empty) listing — never EIO, never a panic, never an
/// unterminated stream. `InoMap::readdir` trusts the kernel-supplied
/// resume offset verbatim, so an offset it cannot map must degrade to a
/// bounded/empty reply rather than erroring or looping (the regression
/// class behind the historical udf seekdir crash).
pub fn generic_676_seekdir(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let dir = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let count = ctx.manifest.seq_dir.count;

    // Record the cookie BEFORE each entry, the way an application using
    // telldir/seekdir does.
    let d = RawDir::open(&dir)?;
    let mut entries: Vec<(libc::c_long, Vec<u8>)> = Vec::new();
    loop {
        let cookie = d.tell();
        match d.next_entry()? {
            None => break,
            Some((name, _)) => entries.push((cookie, name)),
        }
    }
    ensure!(
        entries.len() >= usize::try_from(count).expect("count fits usize"),
        "stream yielded {} entries, expected at least {count}",
        entries.len()
    );

    // Seeking to a recorded cookie re-yields that same entry.
    let probe_indices = [0, entries.len() / 3, entries.len() / 2, entries.len() - 1];
    for &idx in &probe_indices {
        let (cookie, ref want) = entries[idx];
        d.seek(cookie);
        let (got, _) = d.next_entry()?.with_context(|| {
            format!("seekdir to a valid cookie ({cookie}) then readdir hit EOF")
        })?;
        ensure!(
            &got == want,
            "seekdir to cookie {cookie} yielded {:?}, expected {:?}",
            got.escape_ascii().to_string(),
            want.escape_ascii().to_string()
        );
    }

    // Garbage / out-of-range cookies: the stream must stay well-behaved.
    // A bounded drain proves it neither errors (next_entry fails on a
    // readdir errno) nor runs forever.
    let cap = usize::try_from(count).expect("count fits usize") + 8;
    for bogus in [
        libc::c_long::MAX,
        0x7fff_ffff,
        -1,
        1_000_000,
        libc::c_long::MIN,
    ] {
        d.seek(bogus);
        let mut seen = 0usize;
        while d.next_entry()?.is_some() {
            seen += 1;
            ensure!(
                seen <= cap,
                "seekdir to a garbage cookie ({bogus}) produced an unterminated stream"
            );
        }
    }
    Ok(Outcome::Pass)
}

/// generic/088: with `default_permissions` the kernel enforces the
/// served root-owned 0444/0555 modes for ANY unprivileged uid, not only
/// the uid that mounted the FUSE. Drops to a second unprivileged
/// identity (distinct from the build uid the other batteries probe) and
/// asserts the same view + same denial wall: reads and exec of an input
/// succeed, `access(W_OK)` answers the DAC EACCES, and every actual
/// mutation is EROFS-denied by the read-only mount. A regression where
/// only the mount-owner uid is enforced would let a build's helper
/// processes (which can run under other uids) bypass input protection.
pub fn generic_088_second_uid_dac(ctx: &Ctx) -> anyhow::Result<Outcome> {
    ensure!(
        nix::unistd::geteuid().is_root(),
        "the second-uid DAC battery must start as root (it drops privilege itself)"
    );
    ensure!(
        ctx.second_uid != 0 && ctx.second_uid != ctx.probe_uid,
        "the second uid ({}) must be unprivileged and differ from the build uid ({}) — \
         testing a uid the rest of the suite already covers proves nothing",
        ctx.second_uid,
        ctx.probe_uid
    );

    let readable = readable_plain_file(ctx)?;
    let exec = ctx
        .manifest
        .files
        .iter()
        .find(|f| f.executable)
        .context("manifest has no executable file")?;
    let read_path = ctx.on_mount(&readable.path);
    let new_file = ctx.dep_root.join("u088-new-file");

    {
        let _guard = PrivDrop::to(ctx.second_uid, ctx.second_gid)?;

        // Reads of a world-readable input succeed and return the bytes.
        let body = fs::read(&read_path)
            .with_context(|| format!("read {} as the second uid", readable.path))?;
        ensure!(
            body == readable.content.as_bytes(),
            "{} content differs when read as the second uid",
            readable.path
        );

        // access(2) reflects the served modes for the second uid.
        let probes: [(&str, &str, AccessFlags, Option<Errno>); 4] = [
            (
                "R_OK on a 0444 input",
                &readable.path,
                AccessFlags::R_OK,
                None,
            ),
            ("X_OK on a 0555 input", &exec.path, AccessFlags::X_OK, None),
            (
                "W_OK on a 0444 input",
                &readable.path,
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
                None => ensure!(
                    res.is_ok(),
                    "{what} ({rel}) as the second uid: denied with {res:?}"
                ),
                Some(errno) => ensure!(
                    res == Err(errno),
                    "{what} ({rel}) as the second uid: got {res:?}, expected {errno:?}"
                ),
            }
        }

        // Mutations are denied exactly as for the build uid: any
        // write-mode open hits mnt_want_write's EROFS on the read-only
        // mount before DAC is even consulted (POSIX-prescribed, same
        // as ro-tmpfs).
        expect_errno(
            "create a file as the second uid",
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&new_file)
                .map(drop),
            &[Errno::EROFS],
        )?;
        expect_errno(
            "open(O_WRONLY|O_TRUNC) an input as the second uid",
            fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&read_path)
                .map(drop),
            &[Errno::EROFS],
        )?;
    }

    ensure!(
        !new_file.exists(),
        "tree changed: the denied create as the second uid left an entry behind"
    );
    Ok(Outcome::Pass)
}

/// generic/131: POSIX record locks and flock on a read-only input.
///
/// A read lock taken through an O_RDONLY fd must be GRANTED and tracked
/// by the kernel locally — the castore daemon implements no lock ops, so
/// the kernel must be the lock manager. The structural proof is
/// cross-process: a child's F_GETLK must observe the parent's read lock
/// as a conflict (if the FUSE delegated locking to the lock-less daemon,
/// the child would see nothing and a sqlite/dotlock-using build sharing
/// an input across processes would corrupt silently). A write lock
/// requested through a read-only fd is rejected by the kernel with
/// EBADF before the fs is involved. flock is best-effort: fuser does not
/// advertise FUSE_FLOCK_LOCKS, so ENOSYS is acceptable; if a grant IS
/// returned it must be enforced (a conflicting exclusive lock refused).
pub fn generic_131_read_locks(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let target = readable_plain_file(ctx)?;
    let path = ctx.on_mount(&target.path);

    let f = fs::File::open(&path).with_context(|| format!("open {} O_RDONLY", target.path))?;
    let fd = f.as_raw_fd();

    // F_RDLCK through an O_RDONLY fd: granted.
    let rd = make_flock(libc::F_RDLCK);
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETLK, &rd) } == 0,
        "F_SETLK F_RDLCK on a read-only fd failed: {}",
        io::Error::last_os_error()
    );

    // F_GETLK answers (the lock manager is live, not ENOSYS/EIO); our own
    // lock never self-conflicts, so a whole-file query is grantable.
    let mut query = make_flock(libc::F_WRLCK);
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_GETLK, &mut query) } == 0,
        "F_GETLK failed: {}",
        io::Error::last_os_error()
    );
    ensure!(
        query.l_type == as_short(libc::F_UNLCK),
        "F_GETLK reported a conflict from our own process (l_type={})",
        query.l_type
    );

    // Cross-process: a child must see the parent's read lock.
    let parent_pid = std::process::id() as libc::pid_t;
    // SAFETY: the runner is single-threaded and runs no async runtime;
    // the child only calls async-signal-safe libc and _exit.
    let child = unsafe { libc::fork() };
    ensure!(child >= 0, "fork failed: {}", io::Error::last_os_error());
    if child == 0 {
        let code = child_observes_parent_lock(&path, parent_pid);
        unsafe { libc::_exit(code) };
    }
    let mut status: libc::c_int = 0;
    ensure!(
        unsafe { libc::waitpid(child, &mut status, 0) } == child,
        "waitpid on the F_GETLK child failed"
    );
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };
    ensure!(
        code == 0,
        "child F_GETLK did not observe the parent's read lock (code {code}); the kernel is \
         not tracking POSIX locks locally for the castore mount"
    );

    // Release.
    let un = make_flock(libc::F_UNLCK);
    unsafe { libc::fcntl(fd, libc::F_SETLK, &un) };

    // A write lock through a read-only fd: rejected by the kernel (EBADF),
    // never reaching the fs.
    let f2 = fs::File::open(&path)?;
    let wr = make_flock(libc::F_WRLCK);
    let r = unsafe { libc::fcntl(f2.as_raw_fd(), libc::F_SETLK, &wr) };
    let e = Errno::last();
    ensure!(
        r == -1 && e == Errno::EBADF,
        "F_SETLK F_WRLCK on a read-only fd returned {r}/{e:?}, expected -1/EBADF"
    );

    // flock leg.
    let sh = unsafe { libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB) };
    if sh == 0 {
        let other = fs::File::open(&path)?;
        let ofd = other.as_raw_fd();
        let ex = unsafe { libc::flock(ofd, libc::LOCK_EX | libc::LOCK_NB) };
        let ex_errno = Errno::last();
        ensure!(
            ex == -1 && ex_errno == Errno::EAGAIN,
            "flock LOCK_EX on a second fd returned {ex}/{ex_errno:?} while a shared lock was \
             held, expected -1/EAGAIN — the shared lock is not enforced"
        );
        ensure!(
            unsafe { libc::flock(ofd, libc::LOCK_SH | libc::LOCK_NB) } == 0,
            "flock LOCK_SH on a second fd was refused despite shared-lock compatibility"
        );
        unsafe { libc::flock(ofd, libc::LOCK_UN) };
        unsafe { libc::flock(fd, libc::LOCK_UN) };
        println!("    flock: shared locks granted and enforced kernel-locally");
    } else {
        let e = Errno::last();
        ensure!(
            matches!(e, Errno::ENOSYS | Errno::EOPNOTSUPP),
            "flock LOCK_SH on a read-only input failed with {e:?}, expected success or \
             ENOSYS/EOPNOTSUPP"
        );
        println!(
            "    flock: not advertised by the daemon ({e:?}); POSIX record locks are the \
             supported path"
        );
    }
    Ok(Outcome::Pass)
}

/// generic/478 + generic/571 (read legs): OFD locks and leases on
/// read-only input files. OFD (open-file-description) locks are what
/// modern concurrent tooling uses instead of process-keyed POSIX
/// locks; like the generic/131 records they must be granted on an
/// O_RDONLY fd and tracked by the kernel (the daemon has no lock ops),
/// and a second fd's F_OFD_GETLK must see the conflict with the OFD
/// marker pid -1. A write OFD lock through a read-only fd is EBADF. A
/// read lease (F_SETLEASE) on an input must be grantable and visible
/// via F_GETLEASE — leases are pure VFS state, so anything else means
/// the kernel thinks the file is open for conflicting writes.
pub fn generic_478_571_ofd_locks_lease(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let target = readable_plain_file(ctx)?;
    let path = ctx.on_mount(&target.path);

    let f1 = fs::File::open(&path).with_context(|| format!("open {} O_RDONLY", target.path))?;

    // OFD read lock on a read-only fd: granted. l_pid must be 0 on set
    // (the kernel rejects nonzero with EINVAL).
    let rd = make_flock(libc::F_RDLCK);
    ensure!(
        unsafe { libc::fcntl(f1.as_raw_fd(), libc::F_OFD_SETLK, &rd) } == 0,
        "F_OFD_SETLK F_RDLCK on a read-only fd failed: {}",
        io::Error::last_os_error()
    );

    // A second open file description must observe the conflict, with
    // the OFD marker pid (-1) as the holder.
    let f2 = fs::File::open(&path)?;
    let mut query = make_flock(libc::F_WRLCK);
    ensure!(
        unsafe { libc::fcntl(f2.as_raw_fd(), libc::F_OFD_GETLK, &mut query) } == 0,
        "F_OFD_GETLK failed: {}",
        io::Error::last_os_error()
    );
    ensure!(
        query.l_type == as_short(libc::F_RDLCK),
        "F_OFD_GETLK on a second fd reported l_type={}, expected the read-lock conflict — \
         OFD locks are not tracked kernel-locally",
        query.l_type
    );
    ensure!(
        query.l_pid == -1,
        "F_OFD_GETLK conflict reported l_pid={}, expected the OFD marker -1",
        query.l_pid
    );

    // A write OFD lock through a read-only fd: EBADF from the kernel.
    let wr = make_flock(libc::F_WRLCK);
    let r = unsafe { libc::fcntl(f2.as_raw_fd(), libc::F_OFD_SETLK, &wr) };
    let e = Errno::last();
    ensure!(
        r == -1 && e == Errno::EBADF,
        "F_OFD_SETLK F_WRLCK on a read-only fd returned {r}/{e:?}, expected -1/EBADF"
    );

    // Release the read lock.
    let un = make_flock(libc::F_UNLCK);
    unsafe { libc::fcntl(f1.as_raw_fd(), libc::F_OFD_SETLK, &un) };
    drop(f2);

    // Read lease: grantable on a file nobody holds open for writing
    // (nothing on this mount can be), and reported back by F_GETLEASE.
    ensure!(
        unsafe { libc::fcntl(f1.as_raw_fd(), libc::F_SETLEASE, libc::F_RDLCK) } == 0,
        "F_SETLEASE F_RDLCK on a read-only input failed: {} — a read lease on an immutable \
         file must be grantable",
        io::Error::last_os_error()
    );
    let lease = unsafe { libc::fcntl(f1.as_raw_fd(), libc::F_GETLEASE) };
    ensure!(
        lease == libc::F_RDLCK,
        "F_GETLEASE reported {lease}, expected F_RDLCK"
    );
    ensure!(
        unsafe { libc::fcntl(f1.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK) } == 0,
        "releasing the read lease failed: {}",
        io::Error::last_os_error()
    );
    Ok(Outcome::Pass)
}

/// generic/637 (small-getdents leg): a directory must enumerate
/// completely through a fresh fd even when every getdents64 call can
/// only return a handful of entries — the 200-entry dir through a
/// 64-byte buffer takes ~100 syscalls, each resuming from the kernel
/// offset cookie. The lookalike-names dir runs with a 512-byte buffer
/// (its NAME_MAX entry needs ~280 bytes). Duplicated or skipped
/// entries on resume are the corruption class; the immutable-fs
/// visibility half of upstream 637 is moot.
pub fn generic_637_small_getdents(ctx: &Ctx) -> anyhow::Result<Outcome> {
    // 200-entry dir, tiny buffer.
    let seq = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let names = getdents_names(&seq, 64)?;
    let expected: std::collections::BTreeSet<Vec<u8>> = (1..=ctx.manifest.seq_dir.count)
        .map(|i| format!("f{i}").into_bytes())
        .collect();
    assert_complete(&seq, &names, &expected)?;

    // Lookalike-names dir (holds a NAME_MAX entry), small buffer.
    let names_dir = ctx.on_mount("names");
    let listed = getdents_names(&names_dir, 512)?;
    let expected: std::collections::BTreeSet<Vec<u8>> = ctx
        .manifest
        .files_under("names/")
        .map(|f| f.path.as_bytes()["names/".len()..].to_vec())
        .collect();
    assert_complete(&names_dir, &listed, &expected)?;
    Ok(Outcome::Pass)
}

// ─── helpers ───────────────────────────────────────────────────────────

/// Raw getdents64 enumeration of `dir` through a `bufsize`-byte buffer,
/// returning every entry name (dots excluded).
fn getdents_names(dir: &Path, bufsize: usize) -> anyhow::Result<Vec<Vec<u8>>> {
    let fd = open_raw(dir, libc::O_RDONLY | libc::O_DIRECTORY)
        .with_context(|| format!("open({}, O_DIRECTORY)", dir.display()))?;

    let mut buf = vec![0u8; bufsize];
    let mut names = Vec::new();
    loop {
        // SAFETY: fd is a live directory fd; buf is bufsize bytes.
        let n = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                fd.as_raw_fd(),
                buf.as_mut_ptr(),
                bufsize,
            )
        };
        ensure!(
            n >= 0,
            "getdents64({}, buf={bufsize}) failed: {} — a small buffer must yield a short \
             batch, not an error",
            dir.display(),
            io::Error::last_os_error()
        );
        if n == 0 {
            return Ok(names);
        }
        let mut off = 0usize;
        while off < n as usize {
            // SAFETY: the kernel wrote a valid dirent64 record at off;
            // d_name is NUL-terminated within the record.
            let (name, reclen) = unsafe {
                let d = buf.as_ptr().add(off).cast::<libc::dirent64>();
                (
                    std::ffi::CStr::from_ptr(std::ptr::addr_of!((*d).d_name).cast())
                        .to_bytes()
                        .to_vec(),
                    (*d).d_reclen as usize,
                )
            };
            ensure!(reclen > 0, "getdents64 returned a zero-length record");
            if name != b"." && name != b".." {
                names.push(name);
            }
            off += reclen;
        }
    }
}

/// The enumeration must be exactly `expected`: no missing entries, no
/// duplicates, no strays.
fn assert_complete(
    dir: &Path,
    listed: &[Vec<u8>],
    expected: &std::collections::BTreeSet<Vec<u8>>,
) -> anyhow::Result<()> {
    let unique: std::collections::BTreeSet<Vec<u8>> = listed.iter().cloned().collect();
    ensure!(
        unique.len() == listed.len(),
        "{}: small-buffer getdents returned {} entries with duplicates ({} unique) — the \
         offset resume re-served an entry",
        dir.display(),
        listed.len(),
        unique.len()
    );
    ensure!(
        &unique == expected,
        "{}: small-buffer getdents listed {} entries, expected {} (missing: {:?})",
        dir.display(),
        unique.len(),
        expected.len(),
        expected.difference(&unique).take(3).collect::<Vec<_>>()
    );
    Ok(())
}

/// A whole-file `flock` of the given type, anchored at offset 0.
fn make_flock(l_type: libc::c_int) -> libc::flock {
    // SAFETY: libc::flock is plain-old-data; an all-zero value is a valid
    // (whole-file, SEEK_SET) lock request before we set the type.
    let mut fl: libc::flock = unsafe { std::mem::zeroed() };
    fl.l_type = as_short(l_type);
    fl.l_whence = as_short(libc::SEEK_SET);
    fl.l_start = 0;
    fl.l_len = 0;
    fl
}

fn as_short(v: libc::c_int) -> libc::c_short {
    v as libc::c_short
}

/// Child half of the cross-process lock proof: open the same file and
/// F_GETLK a whole-file write lock; the parent's read lock must show up
/// as the conflicting holder. Returns 0 on the expected conflict, a
/// distinct non-zero code otherwise (decoded in the failure message).
fn child_observes_parent_lock(path: &Path, parent_pid: libc::pid_t) -> libc::c_int {
    let Ok(cf) = fs::File::open(path) else {
        return 10;
    };
    let mut query = make_flock(libc::F_WRLCK);
    if unsafe { libc::fcntl(cf.as_raw_fd(), libc::F_GETLK, &mut query) } != 0 {
        return 11;
    }
    if query.l_type != as_short(libc::F_RDLCK) {
        return 12; // no conflict observed — lock not tracked locally
    }
    if query.l_pid != parent_pid {
        return 13; // conflict reported by an unexpected holder
    }
    0
}

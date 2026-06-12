//! flock(2) helpers.
//!
//! Lock roles (ADR-024 pack store):
//!
//! - Every writer holds a SHARED flock on its own segment for the
//!   segment's lifetime. GC/repack skips any pack whose flock it
//!   cannot take exclusively — a live writer's segment is never
//!   repacked or unlinked out from under it.
//! - One exclusive advisory lock (the `gc.lock` file) serializes GC
//!   and every index rewrite. Index rewrites are load + merge + rename
//!   under this lock; GC try-locks it at open and skips GC entirely if
//!   another process holds it.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

fn flock(file: &File, op: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: flock on a valid open fd; no memory is touched.
        let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Shared lock, blocking. Writers take this on their own segment.
pub(crate) fn lock_shared(file: &File) -> io::Result<()> {
    flock(file, libc::LOCK_SH)
}

/// Exclusive lock, blocking. Used for the index-rewrite critical
/// section on `gc.lock`.
pub(crate) fn lock_exclusive(file: &File) -> io::Result<()> {
    flock(file, libc::LOCK_EX)
}

/// Exclusive lock, non-blocking. Returns false if someone else holds
/// the lock — the caller must skip, never wait: GC is opportunistic
/// and a segment with a live writer is simply not repacked.
pub(crate) fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    // SAFETY: flock on a valid open fd; no memory is touched.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK || code == libc::EINTR => Ok(false),
        _ => Err(err),
    }
}

/// Drop the lock early (locks also die with the fd).
pub(crate) fn unlock(file: &File) -> io::Result<()> {
    flock(file, libc::LOCK_UN)
}

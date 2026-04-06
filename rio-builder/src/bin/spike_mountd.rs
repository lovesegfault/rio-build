//! Spike: privileged mount helper for the composefs-style stack (P0541).
//!
//! Subcommands:
//!   fuse <mount> <socket>
//!     Open /dev/fuse, mount it at <mount>, send the fd to <socket>, exit.
//!   stack <objects-mount> <erofs-img> <meta-mount> <merged-mount> <socket>
//!     fuse-mount + losetup+erofs-mount + overlay(metacopy,redirect_dir),
//!     send the /dev/fuse fd, exit. Q1+Q2 in one shot.
//!   fsmount-erofs <erofs-img> <socket>
//!     fsopen("erofs")/fsconfig/fsmount → detached mount fd; send THAT fd.
//!   recv-mount <socket> <target>
//!     Receive a detached mount fd, attempt move_mount into <target>.
//!     Intended to run unprivileged inside its own mountns (Q6 receiver).
//!
//! NOT production code. Prototype of `rio-mountd` per ADR-022 §C / V2 P0567.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;

use anyhow::{Context, bail};
use nix::cmsg_space;
use nix::libc;
use nix::mount::{MsFlags, mount};
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};

fn send_fd(socket: &str, fd: RawFd) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket).context("connect to fd socket")?;
    let fds = [fd];
    let cmsg = [ControlMessage::ScmRights(&fds)];
    let iov = [IoSlice::new(b"F")];
    sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)
        .context("sendmsg SCM_RIGHTS")?;
    Ok(())
}

fn recv_fd(socket: &str) -> anyhow::Result<OwnedFd> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    eprintln!("spike_mountd: listening for fd on {socket}");
    let (stream, _) = listener.accept()?;
    let mut cmsg = cmsg_space!([RawFd; 1]);
    let mut buf = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let msg = recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg),
        MsgFlags::empty(),
    )?;
    for c in msg.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = c
            && let Some(fd) = fds.into_iter().next()
        {
            // SAFETY: fd just delivered via SCM_RIGHTS; sole owner.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
    }
    bail!("no SCM_RIGHTS received")
}

/// Open /dev/fuse and mount it at `mnt`. Returns the open fd (caller sends it).
fn mount_fuse(mnt: &str) -> anyhow::Result<OwnedFd> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .context("open /dev/fuse")?;
    let fd = f.as_raw_fd();
    let uid = nix::unistd::geteuid();
    let gid = nix::unistd::getegid();
    // rootmode=40000 (octal S_IFDIR). allow_other so non-root readers work.
    let data = format!("fd={fd},rootmode=40000,user_id={uid},group_id={gid},allow_other,ro");
    mount(
        Some("spike-digest"),
        mnt,
        Some("fuse"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some(data.as_str()),
    )
    .with_context(|| format!("mount fuse at {mnt}"))?;
    eprintln!("spike_mountd: fuse mounted at {mnt} (fd={fd})");
    Ok(f.into())
}

fn losetup_show(img: &str) -> anyhow::Result<String> {
    let out = Command::new("losetup")
        .args(["-f", "--show", "-r", img])
        .output()
        .context("spawn losetup")?;
    if !out.status.success() {
        bail!(
            "losetup failed: {}",
            std::str::from_utf8(&out.stderr)?.trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

fn cmd_fuse(mnt: &str, socket: &str) -> anyhow::Result<()> {
    let fd = mount_fuse(mnt)?;
    send_fd(socket, fd.as_raw_fd())?;
    eprintln!("spike_mountd: fd sent, exiting");
    Ok(())
}

fn cmd_stack(
    objects: &str,
    erofs_img: &str,
    meta: &str,
    merged: &str,
    socket: &str,
) -> anyhow::Result<()> {
    let fuse_fd = mount_fuse(objects)?;
    // Hand off the /dev/fuse fd BEFORE the overlay mount: overlayfs probes
    // each lower's root (FUSE_GETATTR on ino 1) at mount(2) time. If the fd
    // is still only held by us — and we're not serving it — that probe
    // deadlocks. The receiver does Session::from_fd → handshake → serves;
    // any probe queued before then sits in /dev/fuse until it does.
    send_fd(socket, fuse_fd.as_raw_fd())?;
    drop(fuse_fd);
    eprintln!("spike_mountd: fd sent (server now owns /dev/fuse)");

    let loopdev = losetup_show(erofs_img)?;
    mount(
        Some(loopdev.as_str()),
        meta,
        Some("erofs"),
        MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .with_context(|| format!("mount erofs {loopdev} at {meta}"))?;
    eprintln!("spike_mountd: erofs mounted at {meta} via {loopdev}");

    let opts = format!("ro,lowerdir={meta}::{objects},metacopy=on,redirect_dir=on");
    mount(
        Some("overlay"),
        merged,
        Some("overlay"),
        MsFlags::empty(),
        Some(opts.as_str()),
    )
    .with_context(|| format!("mount overlay at {merged}"))?;
    eprintln!("spike_mountd: overlay mounted at {merged}; exiting (all 3 mounts up)");
    Ok(())
}

// ── Q6: new mount API (fsopen/fsconfig/fsmount/move_mount) ──────────
// nix-crate doesn't bind these; use raw syscalls. x86_64-only for the
// spike (the production node arch); aarch64 numbers happen to match.

const SYS_MOVE_MOUNT: libc::c_long = 429;
const SYS_FSOPEN: libc::c_long = 430;
const SYS_FSCONFIG: libc::c_long = 431;
const SYS_FSMOUNT: libc::c_long = 432;

const FSCONFIG_SET_FLAG: u32 = 0;
const FSCONFIG_SET_STRING: u32 = 1;
const FSCONFIG_CMD_CREATE: u32 = 6;
const FSMOUNT_CLOEXEC: u32 = 1;
const MOUNT_ATTR_RDONLY: u32 = 1;
const MOVE_MOUNT_F_EMPTY_PATH: u32 = 4;

unsafe fn sys(num: libc::c_long, a: usize, b: usize, c: usize, d: usize, e: usize) -> isize {
    // SAFETY: caller guarantees args are valid for the syscall.
    unsafe { libc::syscall(num, a, b, c, d, e) as isize }
}

fn errno_result(r: isize, what: &str) -> anyhow::Result<i32> {
    if r < 0 {
        Err(std::io::Error::last_os_error()).with_context(|| what.to_owned())
    } else {
        Ok(r as i32)
    }
}

fn cmd_fsmount_erofs(img: &str, socket: &str) -> anyhow::Result<()> {
    let loopdev = losetup_show(img)?;
    let fstype = CString::new("erofs")?;
    let key_src = CString::new("source")?;
    let val_src = CString::new(loopdev.as_str())?;

    // SAFETY: all pointers are valid CStrings; syscall numbers are correct
    // for x86_64/aarch64 Linux ≥5.2.
    let fsfd = unsafe { sys(SYS_FSOPEN, fstype.as_ptr() as usize, 0, 0, 0, 0) };
    let fsfd = errno_result(fsfd, "fsopen erofs")?;
    let r = unsafe {
        sys(
            SYS_FSCONFIG,
            fsfd as usize,
            FSCONFIG_SET_STRING as usize,
            key_src.as_ptr() as usize,
            val_src.as_ptr() as usize,
            0,
        )
    };
    errno_result(r, "fsconfig source")?;
    // RO at superblock creation: without this, erofs opens the bdev RW
    // and a `-r` loop device → EACCES. MOUNT_ATTR_RDONLY on fsmount is
    // per-mount, not per-superblock — it doesn't affect bdev open mode.
    let key_ro = CString::new("ro")?;
    let r = unsafe {
        sys(
            SYS_FSCONFIG,
            fsfd as usize,
            FSCONFIG_SET_FLAG as usize,
            key_ro.as_ptr() as usize,
            0,
            0,
        )
    };
    errno_result(r, "fsconfig ro")?;
    let r = unsafe {
        sys(
            SYS_FSCONFIG,
            fsfd as usize,
            FSCONFIG_CMD_CREATE as usize,
            0,
            0,
            0,
        )
    };
    errno_result(r, "fsconfig CMD_CREATE")?;
    let mfd = unsafe {
        sys(
            SYS_FSMOUNT,
            fsfd as usize,
            FSMOUNT_CLOEXEC as usize,
            MOUNT_ATTR_RDONLY as usize,
            0,
            0,
        )
    };
    let mfd = errno_result(mfd, "fsmount")?;
    eprintln!("spike_mountd: detached erofs mount fd={mfd}");
    send_fd(socket, mfd)?;
    // SAFETY: take ownership so close-on-drop happens after send (kernel
    // dup'd via SCM_RIGHTS already).
    let _own = unsafe { OwnedFd::from_raw_fd(mfd) };
    let _fsown = unsafe { OwnedFd::from_raw_fd(fsfd) };
    eprintln!("spike_mountd: detached-mount fd sent, exiting");
    Ok(())
}

fn cmd_recv_mount(socket: &str, target: &str) -> anyhow::Result<()> {
    let fd = recv_fd(socket)?;
    let empty = CString::new("")?;
    let tgt = CString::new(target)?;
    eprintln!(
        "spike_mountd: recv-mount got fd={}, euid={}, attempting move_mount → {target}",
        fd.as_raw_fd(),
        nix::unistd::geteuid()
    );
    let r = unsafe {
        sys(
            SYS_MOVE_MOUNT,
            fd.as_raw_fd() as usize,
            empty.as_ptr() as usize,
            libc::AT_FDCWD as usize,
            tgt.as_ptr() as usize,
            MOVE_MOUNT_F_EMPTY_PATH as usize,
        )
    };
    errno_result(r, "move_mount")?;
    eprintln!("spike_mountd: move_mount OK at {target}");
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["fuse", mnt, socket] => cmd_fuse(mnt, socket),
        ["stack", objects, img, meta, merged, socket] => {
            cmd_stack(objects, img, meta, merged, socket)
        }
        ["fsmount-erofs", img, socket] => cmd_fsmount_erofs(img, socket),
        ["recv-mount", socket, target] => cmd_recv_mount(socket, target),
        _ => bail!(
            "usage: spike_mountd fuse <mnt> <sock> | stack <obj> <img> <meta> <merged> <sock> \
             | fsmount-erofs <img> <sock> | recv-mount <sock> <target>"
        ),
    }
}

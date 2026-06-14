//! The coordinator's end of the worker channel: typed frame I/O over
//! one `socketpair(AF_UNIX, SOCK_STREAM)` plus the eval-parent spawn.
//!
//! The coordinator exec's the eval parent with one end of the
//! socketpair as **fd 3** (`EVAL_CHANNEL_FD`), CLOEXEC cleared.
//! The exec boundary is the point (ADR-024): tokio/tonic/rustls stay
//! out of every process that forks; the eval parent controls its own
//! threads. Tests skip the spawn entirely and hand
//! [`EvalChannel::from_std`] one end of a `UnixStream::pair`.

use std::path::Path;
use std::process::Stdio;

use rio_proto::evaljob::{CoordinatorFrame, WorkerFrame, coordinator_frame, worker_frame};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::framing;

/// Fd number the eval parent inherits its channel end on.
pub const EVAL_CHANNEL_FD: i32 = 3;

/// Read half: upstream `WorkerFrame`s from the eval parent.
pub struct EvalReader {
    inner: OwnedReadHalf,
}

/// Write half: downstream `CoordinatorFrame`s to the eval parent.
pub struct EvalWriter {
    inner: OwnedWriteHalf,
}

impl EvalReader {
    /// Next upstream message. `Ok(None)` = peer closed cleanly.
    /// Frames with an empty oneof (unknown future message kinds) are
    /// skipped — forward compatibility on a local channel.
    pub async fn recv(&mut self) -> std::io::Result<Option<worker_frame::Msg>> {
        loop {
            match framing::read_frame::<_, WorkerFrame>(&mut self.inner).await? {
                None => return Ok(None),
                Some(WorkerFrame { msg: Some(m) }) => return Ok(Some(m)),
                Some(WorkerFrame { msg: None }) => continue,
            }
        }
    }
}

impl EvalWriter {
    pub async fn send(&mut self, msg: coordinator_frame::Msg) -> std::io::Result<()> {
        framing::write_frame(&mut self.inner, &CoordinatorFrame { msg: Some(msg) }).await
    }
}

/// One coordinator↔eval-parent channel, split for concurrent use.
pub struct EvalChannel {
    pub reader: EvalReader,
    pub writer: EvalWriter,
}

impl EvalChannel {
    /// Wrap an already-connected std stream (the test-harness path,
    /// and the spawn path below after the child holds its end).
    pub fn from_std(stream: std::os::unix::net::UnixStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        let stream = UnixStream::from_std(stream)?;
        let (r, w) = stream.into_split();
        Ok(Self {
            reader: EvalReader { inner: r },
            writer: EvalWriter { inner: w },
        })
    }
}

/// Spawn the eval parent with its channel end on fd 3 and return the
/// coordinator's end plus the child handle. With `pipe_stderr`, the
/// child's stderr is captured (the caller forwards it through the
/// renderer so it doesn't land inside the TTY ephemeral region).
pub fn spawn_eval_parent(
    program: &Path,
    args: &[String],
    pipe_stderr: bool,
) -> anyhow::Result<(EvalChannel, tokio::process::Child)> {
    use std::os::fd::{AsRawFd, IntoRawFd};

    let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        // Eval diagnostics pass through to the user's terminal — or
        // through the renderer when the live region is up.
        .stdout(Stdio::inherit())
        .stderr(if pipe_stderr {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
    let theirs_fd = theirs.into_raw_fd();
    // SAFETY: pre_exec runs post-fork pre-exec in the child; dup2 and
    // fcntl are async-signal-safe. `ours` is not inherited (CLOEXEC by
    // default on UnixStream::pair). dup2 to a DIFFERENT fd clears
    // CLOEXEC on the duplicate; dup2(fd, fd) is a no-op that leaves
    // CLOEXEC set and the channel would close on exec — clear the flag
    // directly when the socket already landed on fd 3.
    unsafe {
        cmd.pre_exec(move || {
            if theirs_fd == EVAL_CHANNEL_FD {
                let flags = libc::fcntl(EVAL_CHANNEL_FD, libc::F_GETFD);
                if flags < 0
                    || libc::fcntl(EVAL_CHANNEL_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(theirs_fd, EVAL_CHANNEL_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    // Parent side: close the child's end (it was dup'd into the child).
    // SAFETY: theirs_fd is owned by this function after into_raw_fd.
    unsafe { libc::close(theirs_fd) };
    debug_assert!(ours.as_raw_fd() >= 0);
    Ok((EvalChannel::from_std(ours)?, child))
}

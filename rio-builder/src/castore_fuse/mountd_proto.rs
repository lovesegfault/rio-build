//! UDS wire protocol between the unprivileged builder and `rio-mountd`.
//!
//! One `SOCK_SEQPACKET` Unix socket per build. Every datagram is one
//! postcard-encoded [`Request`] or [`Reply`]; file descriptors travel
//! exclusively in the datagram's `SCM_RIGHTS` ancillary data, never in
//! the serialized body. `SOCK_SEQPACKET` (not `SOCK_STREAM` +
//! length-prefix) because stream sockets associate ancillary data with
//! a byte position, not a frame — with pipelined frames a reader whose
//! `recvmsg` boundaries don't line up with the writer's `sendmsg`
//! boundaries can attach an fd to the wrong request. Message-boundary
//! preservation makes one `recvmsg` == one frame == its fds, and
//! `MSG_TRUNC` gives oversize-frame rejection for free.
//!
//! Requests carry a `seq` echoed in the reply so the daemon can answer
//! out of order: `BackingOpen`/`BackingClose` are answered inline
//! (sub-ms), `Promote`/`PromoteChunks` run on `spawn_blocking` and
//! reply when the copy+hash finishes. The client (`mountd_client`, std
//! threads — no tokio) correlates via a `HashMap<u32,
//! std::sync::mpsc::SyncSender>` of capacity-1 reply slots that its
//! reader thread fills.
// r[impl builder.mountd.backing-broker]

use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use nix::cmsg_space;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use serde::{Deserialize, Serialize};

/// Hard ceiling on a serialized frame, enforced on both sides: the
/// sender refuses to encode anything larger, the receiver's `recvmsg`
/// buffer is exactly this size so an oversize datagram arrives with
/// `MSG_TRUNC` set and is rejected before deserialization. The largest
/// legitimate frame is `PromoteChunks` with [`PROMOTE_CHUNKS_MAX`]
/// digests ≈ 64 × 32 B + overhead ≈ 2.1 KiB.
pub const MAX_FRAME_BYTES: usize = 4096;

/// Server-enforced ceiling on `PromoteChunks.chunk_digests.len()`.
/// Documented as a contract in ADR-022 §6; the daemon rejects larger
/// batches with [`ErrKind::BatchTooLarge`] rather than trusting the
/// client to honor the doc.
pub const PROMOTE_CHUNKS_MAX: usize = 64;

/// A request frame. `seq` is chosen by the client and echoed verbatim
/// in the matching [`Reply`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub seq: u32,
    pub req: Req,
}

/// A reply frame. `seq` matches the [`Request`] it answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub seq: u32,
    pub resp: Resp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Req {
    /// Claim `build_id` and create the build's staging dir
    /// (`staging/{build_id}`, owned by the connection's peer uid, with
    /// the kernel project quota applied). The frame's `SCM_RIGHTS`
    /// carries the **builder's own `/dev/fuse` fd**, which the daemon
    /// keeps for the connection's lifetime as the target of later
    /// `BackingOpen`/`BackingClose` ioctls. Must be the first request
    /// on a connection; exactly one per connection lifetime.
    ///
    /// The daemon neither opens `/dev/fuse` nor mounts anything (P0560
    /// option (b)): the kernel requires a fuse mount to come from the
    /// same user namespace that opened the device, and a mount made
    /// inside the unprivileged DaemonSet container could never
    /// propagate into builder pods — so the builder opens the device
    /// and `mount(2)`s it on a mountpoint it owns inside its own mount
    /// namespace, where the build can actually see it.
    ///
    /// `token` is the scheduler-minted Mount-admission credential
    /// (ADR-022 §P0559, `WorkAssignment.mountd_token`): an HMAC-signed
    /// `MountdClaims{aud, build_id, …}` the daemon verifies offline
    /// when the connection's peer gid is not the allowed gid (the
    /// `hostUsers: false` executor-pod case). `None` = no token (the
    /// gid-admitted standalone/systemd path, or a keyless deployment).
    ///
    /// **Wire evolution / rollout skew.** The field was appended to an
    /// existing postcard-encoded variant, which is safe in both skew
    /// directions because the two ends are tolerant in complementary
    /// ways:
    /// - *new client → old daemon:* `postcard::from_bytes` ignores
    ///   trailing bytes, so a token-bearing Mount decodes as the legacy
    ///   `Mount{build_id}` and the daemon proceeds exactly as before
    ///   (such a connection necessarily passed the old daemon's gid
    ///   gate, so dropping the token loses nothing).
    /// - *old client → new daemon:* the legacy frame ends after
    ///   `build_id`, so decoding the new shape hits end-of-input; the
    ///   daemon's [`decode_request_compat`] then falls back to the
    ///   legacy shape and maps it to `token: None` (the live-build
    ///   reconnect-across-a-DS-upgrade case — those clients are
    ///   gid-admitted or get rejected by token mode, never mis-parsed).
    ///
    /// Both directions are pinned by the `mount_wire_*_skew` tests
    /// below.
    Mount {
        build_id: String,
        token: Option<String>,
    },
    /// Register the fd in this frame's `SCM_RIGHTS` cmsg as a FUSE
    /// passthrough backing file: the daemon issues
    /// `ioctl(kept_fuse_fd, FUSE_DEV_IOC_BACKING_OPEN)` and replies the
    /// connection-scoped `backing_id`. The fd itself never appears in
    /// the body.
    BackingOpen,
    /// Release a `backing_id` from a prior [`Req::BackingOpen`].
    BackingClose { backing_id: u32 },
    /// Verify-copy `staging/{build_id}/{hex(digest)}` into the shared
    /// backing cache at `cache/{ab}/{hex(digest)}`. The daemon re-hashes
    /// during the copy and rejects on mismatch — this is the integrity
    /// boundary for the cache.
    Promote { digest: [u8; 32] },
    /// Batch form of [`Req::Promote`] for FastCDC chunks staged under
    /// `staging/{build_id}/chunks/{hex}`, promoted into
    /// `chunks/{ab}/{hex}`. At most [`PROMOTE_CHUNKS_MAX`] per batch.
    PromoteChunks { chunk_digests: Vec<[u8; 32]> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resp {
    /// [`Req::Mount`] succeeded: the staging dir exists with
    /// `staging_quota_bytes` of kernel-enforced XFS project quota, and
    /// the daemon kept the builder's `/dev/fuse` fd for backing-open
    /// brokering. Carries no fds and reports no host-side mountpoint:
    /// the daemon stopped mounting at the P0560 option-(b) change.
    Mounted { staging_quota_bytes: u64 },
    /// [`Req::BackingOpen`] succeeded.
    BackingId(u32),
    /// Unit success: [`Req::BackingClose`] completed, or
    /// [`Req::Promote`] / [`Req::PromoteChunks`] verified and published
    /// the whole batch.
    Ok,
    /// The request failed. See [`ErrKind::is_build_fatal`] for how the
    /// builder must classify it.
    Err(ErrKind),
}

/// Typed request failures.
///
/// The builder maps every variant except [`ErrKind::Retryable`] and
/// [`ErrKind::RaceTimeout`] to a **build failure** (not an
/// infrastructure retry): re-fetching and re-promoting the same bytes
/// would fail the same way, so retrying loops forever. `RaceTimeout`
/// is the exception — the concurrent promote that won the placeholder
/// finishes eventually, after which a retry succeeds (or short-circuits
/// on the already-published entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrKind {
    /// Transient daemon-side failure (I/O error, oversize frame,
    /// request before `Mount`, …). Safe to retry as an infrastructure
    /// failure.
    Retryable(String),
    /// `Promote` re-hash did not match the claimed digest.
    DigestMismatch,
    /// The staging entry is not a regular file (symlink, FIFO, …).
    NotRegular,
    /// The staging entry exceeds the per-promote size ceiling.
    TooLarge,
    /// Another `Promote` of the same digest holds the `.promoting`
    /// placeholder and did not finish within the wait window. Retryable
    /// — the winner is still copying, not failing.
    RaceTimeout,
    /// `Mount.build_id` does not match `^[A-Za-z0-9_-]{1,64}$`.
    BadBuildId,
    /// A second `Mount` on a connection that already mounted.
    AlreadyMounted,
    /// Another live connection already owns this `build_id`.
    DuplicateBuildId,
    /// `PromoteChunks` batch exceeds [`PROMOTE_CHUNKS_MAX`].
    BatchTooLarge,
    /// `Mount` did not carry a usable `/dev/fuse` fd in its
    /// `SCM_RIGHTS` (missing, or not the fuse character device).
    /// Build-fatal: the same client would send the same fd again, so
    /// retrying cannot succeed — fix the builder, not the request.
    BadFuseFd,
    /// The connection is not authorized to `Mount`: the daemon runs in
    /// token mode, the peer's gid is not the allowed gid, and the
    /// request carried no token that verifies for the requested
    /// `build_id` (missing, malformed, wrong key, expired, wrong
    /// audience, or minted for a different build). Deliberately one
    /// variant for every cause — the daemon logs the specific reason,
    /// the wire reply does not leak it. The daemon closes the
    /// connection after replying. Build-fatal: re-presenting the same
    /// credential cannot succeed.
    ///
    /// Appended after [`ErrKind::BadFuseFd`] so every pre-existing
    /// variant keeps its postcard discriminant; only a daemon that
    /// understands tokens ever emits it.
    Unauthorized,
}

impl ErrKind {
    /// `true` for errors the builder must surface as a build failure;
    /// `false` for the variants where waiting and retrying can succeed
    /// ([`ErrKind::Retryable`], [`ErrKind::RaceTimeout`]).
    pub fn is_build_fatal(&self) -> bool {
        !matches!(self, ErrKind::Retryable(_) | ErrKind::RaceTimeout)
    }
}

impl std::fmt::Display for ErrKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrKind::Retryable(msg) => write!(f, "retryable: {msg}"),
            ErrKind::DigestMismatch => write!(f, "staged content does not match claimed digest"),
            ErrKind::NotRegular => write!(f, "staged entry is not a regular file"),
            ErrKind::TooLarge => write!(f, "staged entry exceeds the promote size ceiling"),
            ErrKind::RaceTimeout => write!(f, "concurrent promote of the same digest timed out"),
            ErrKind::BadBuildId => write!(f, "build_id is not [A-Za-z0-9_-]{{1,64}}"),
            ErrKind::AlreadyMounted => write!(f, "connection already mounted"),
            ErrKind::DuplicateBuildId => write!(f, "build_id is owned by another connection"),
            ErrKind::BatchTooLarge => write!(f, "PromoteChunks batch exceeds the maximum"),
            ErrKind::BadFuseFd => write!(
                f,
                "Mount did not carry a usable /dev/fuse fd (missing from SCM_RIGHTS, or not the \
                 fuse character device)"
            ),
            ErrKind::Unauthorized => write!(
                f,
                "connection not authorized to Mount (peer gid is not the allowed gid and no \
                 valid mountd token was presented)"
            ),
        }
    }
}

/// Frame-level decode/encode errors (not protocol errors — those are
/// [`ErrKind`]).
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame exceeds MAX_FRAME_BYTES ({0} > {MAX_FRAME_BYTES})")]
    Oversize(usize),
    #[error("frame truncated by the kernel (MSG_TRUNC) — peer sent an oversize datagram")]
    Truncated,
    #[error("postcard: {0}")]
    Codec(#[from] postcard::Error),
    #[error("peer closed the connection")]
    Eof,
    #[error("socket: {0}")]
    Io(#[from] std::io::Error),
}

/// Serialize a frame, enforcing [`MAX_FRAME_BYTES`] before it hits the
/// socket.
pub fn encode<T: Serialize>(frame: &T) -> Result<Vec<u8>, FrameError> {
    let bytes = postcard::to_stdvec(frame)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(FrameError::Oversize(bytes.len()));
    }
    Ok(bytes)
}

/// Deserialize a frame received from the socket.
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, FrameError> {
    Ok(postcard::from_bytes(bytes)?)
}

/// Pre-token request shapes, kept only so [`decode_request_compat`] can
/// accept frames from a not-yet-upgraded client (a build that was
/// running when the rio-mountd DaemonSet rolled re-issues the legacy
/// `Mount{build_id}` on its reconnect). Field order and variant order
/// mirror the originals exactly — postcard encodes by position.
mod compat {
    use serde::Deserialize;

    #[derive(Deserialize)]
    pub(super) struct LegacyRequest {
        pub seq: u32,
        pub req: LegacyReq,
    }

    #[derive(Deserialize)]
    pub(super) enum LegacyReq {
        Mount { build_id: String },
        BackingOpen,
        BackingClose { backing_id: u32 },
        Promote { digest: [u8; 32] },
        PromoteChunks { chunk_digests: Vec<[u8; 32]> },
    }

    impl From<LegacyRequest> for super::Request {
        fn from(legacy: LegacyRequest) -> Self {
            let req = match legacy.req {
                LegacyReq::Mount { build_id } => super::Req::Mount {
                    build_id,
                    token: None,
                },
                LegacyReq::BackingOpen => super::Req::BackingOpen,
                LegacyReq::BackingClose { backing_id } => super::Req::BackingClose { backing_id },
                LegacyReq::Promote { digest } => super::Req::Promote { digest },
                LegacyReq::PromoteChunks { chunk_digests } => {
                    super::Req::PromoteChunks { chunk_digests }
                }
            };
            super::Request {
                seq: legacy.seq,
                req,
            }
        }
    }
}

/// Daemon-side request decode that tolerates the pre-token `Mount`
/// frame shape: try the current [`Request`] first, and on failure fall
/// back to the legacy shape (mapped to `token: None`). Only the daemon
/// uses this — replies never changed shape, so the client keeps the
/// strict [`decode`]. A frame that fails BOTH decodes is genuinely
/// malformed and surfaces the current-shape error.
pub fn decode_request_compat(bytes: &[u8]) -> Result<Request, FrameError> {
    match postcard::from_bytes::<Request>(bytes) {
        Ok(req) => Ok(req),
        Err(current_err) => match postcard::from_bytes::<compat::LegacyRequest>(bytes) {
            Ok(legacy) => Ok(legacy.into()),
            Err(_) => Err(current_err.into()),
        },
    }
}

/// One received datagram: the frame bytes and any fds that arrived in
/// its `SCM_RIGHTS` ancillary data.
pub struct RecvFrame {
    pub bytes: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

/// Send one frame as a single `sendmsg` datagram with `fds` attached
/// via `SCM_RIGHTS`. Non-blocking semantics follow the socket: returns
/// `EWOULDBLOCK` through the `Io` variant if the send buffer is full
/// (the async caller retries on writability).
pub fn send_frame(sock: RawFd, bytes: &[u8], fds: &[RawFd]) -> Result<usize, FrameError> {
    debug_assert!(bytes.len() <= MAX_FRAME_BYTES);
    let iov = [IoSlice::new(bytes)];
    let cmsg_storage;
    let cmsgs: &[ControlMessage<'_>] = if fds.is_empty() {
        &[]
    } else {
        cmsg_storage = [ControlMessage::ScmRights(fds)];
        &cmsg_storage
    };
    let n = sendmsg::<()>(sock, &iov, cmsgs, MsgFlags::empty(), None)
        .map_err(|e| FrameError::Io(e.into()))?;
    Ok(n)
}

/// Receive one datagram. Returns [`FrameError::Eof`] on orderly
/// shutdown (zero-length read with no data — `SOCK_SEQPACKET` reports
/// peer close this way), [`FrameError::Truncated`] if the peer sent
/// more than [`MAX_FRAME_BYTES`]. Any fds in the ancillary data are
/// returned as `OwnedFd` so they cannot leak even if the caller
/// rejects the frame.
pub fn recv_frame(sock: RawFd) -> Result<RecvFrame, FrameError> {
    let mut buf = vec![0u8; MAX_FRAME_BYTES];
    // One fd per frame is the protocol contract (BackingOpen carries
    // exactly one); reserve a little extra so a misbehaving peer's
    // multi-fd cmsg is still received (and dropped) instead of
    // overflowing into MSG_CTRUNC and leaking kernel-side fds.
    let mut cmsg = cmsg_space!([RawFd; 4]);
    let mut iov = [IoSliceMut::new(&mut buf)];
    let msg = recvmsg::<()>(sock, &mut iov, Some(&mut cmsg), MsgFlags::empty())
        .map_err(|e| FrameError::Io(e.into()))?;

    let mut fds = Vec::new();
    for c in msg.cmsgs().map_err(|e| FrameError::Io(e.into()))? {
        if let ControlMessageOwned::ScmRights(raw) = c {
            for fd in raw {
                // SAFETY: fd was just installed in this process by the
                // kernel via SCM_RIGHTS; we are its sole owner.
                fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }

    // MSG_CTRUNC: the peer attached more fds than the protocol allows
    // for. The kernel releases the references that did not fit in the
    // control buffer (no leak), but a peer doing this is violating the
    // protocol — reject the frame rather than acting on a silently
    // partial fd set.
    if msg
        .flags
        .intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC)
    {
        return Err(FrameError::Truncated);
    }
    let n = msg.bytes;
    if n == 0 && fds.is_empty() {
        return Err(FrameError::Eof);
    }
    buf.truncate(n);
    Ok(RecvFrame { bytes: buf, fds })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    fn roundtrip<T>(v: &T) -> T
    where
        T: Serialize + for<'a> Deserialize<'a> + std::fmt::Debug + PartialEq,
    {
        let bytes = encode(v).expect("encode");
        decode(&bytes).expect("decode")
    }

    #[test]
    fn request_roundtrip_all_variants() {
        for req in [
            Req::Mount {
                build_id: "b-1234_ABC".into(),
                token: None,
            },
            Req::Mount {
                build_id: "b-1234_ABC".into(),
                token: Some("claims-b64.sig-b64".into()),
            },
            Req::BackingOpen,
            Req::BackingClose { backing_id: 7 },
            Req::Promote { digest: [0xAB; 32] },
            Req::PromoteChunks {
                chunk_digests: vec![[1u8; 32], [2u8; 32]],
            },
        ] {
            let frame = Request { seq: 42, req };
            assert_eq!(roundtrip(&frame), frame);
        }
    }

    /// Old client → new daemon: a pre-token `Mount{build_id}` frame
    /// (exactly what a build that survived a DaemonSet upgrade re-sends
    /// on reconnect) must decode via the compat path as `token: None`,
    /// and every other legacy request must decode identically through
    /// it. A genuinely malformed frame still errors.
    #[test]
    fn mount_wire_old_client_new_daemon_skew() {
        // Hand-rolled legacy encoder: same field/variant order as the
        // pre-token enum, so the bytes are exactly what an old client
        // emits.
        #[derive(serde::Serialize)]
        struct OldRequest {
            seq: u32,
            req: OldReq,
        }
        #[derive(serde::Serialize)]
        enum OldReq {
            Mount {
                build_id: String,
            },
            #[allow(dead_code)]
            BackingOpen,
            BackingClose {
                backing_id: u32,
            },
            #[allow(dead_code)]
            Promote {
                digest: [u8; 32],
            },
            #[allow(dead_code)]
            PromoteChunks {
                chunk_digests: Vec<[u8; 32]>,
            },
        }

        let legacy_mount = postcard::to_stdvec(&OldRequest {
            seq: 9,
            req: OldReq::Mount {
                build_id: "b-legacy".into(),
            },
        })
        .unwrap();
        // The new shape alone cannot parse it (the Option byte is
        // missing)…
        assert!(decode::<Request>(&legacy_mount).is_err());
        // …but the daemon's compat decode maps it to token: None.
        let req = decode_request_compat(&legacy_mount).expect("legacy Mount must decode");
        assert_eq!(
            req,
            Request {
                seq: 9,
                req: Req::Mount {
                    build_id: "b-legacy".into(),
                    token: None
                }
            }
        );

        // Non-Mount legacy frames are byte-identical in both shapes and
        // pass straight through.
        let legacy_close = postcard::to_stdvec(&OldRequest {
            seq: 10,
            req: OldReq::BackingClose { backing_id: 3 },
        })
        .unwrap();
        assert_eq!(
            decode_request_compat(&legacy_close).unwrap(),
            Request {
                seq: 10,
                req: Req::BackingClose { backing_id: 3 }
            }
        );

        // Garbage is still rejected.
        assert!(decode_request_compat(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).is_err());
    }

    /// New client → old daemon: a token-bearing Mount must still decode
    /// under the PRE-token request shape (postcard ignores the trailing
    /// token bytes), so an old daemon treats it as a plain
    /// `Mount{build_id}` instead of failing the build. This pins the
    /// postcard property the rollout-skew story relies on.
    #[test]
    fn mount_wire_new_client_old_daemon_skew() {
        // Mirror of the pre-token shape, as the OLD DAEMON would decode it.
        #[derive(Debug, PartialEq, serde::Deserialize)]
        struct OldRequest {
            seq: u32,
            req: OldReq,
        }
        #[derive(Debug, PartialEq, serde::Deserialize)]
        enum OldReq {
            Mount {
                build_id: String,
            },
            #[allow(dead_code)]
            BackingOpen,
            #[allow(dead_code)]
            BackingClose {
                backing_id: u32,
            },
            #[allow(dead_code)]
            Promote {
                digest: [u8; 32],
            },
            #[allow(dead_code)]
            PromoteChunks {
                chunk_digests: Vec<[u8; 32]>,
            },
        }

        let new_frame = encode(&Request {
            seq: 4,
            req: Req::Mount {
                build_id: "b-new".into(),
                token: Some("claims.sig".into()),
            },
        })
        .unwrap();
        let old: OldRequest =
            postcard::from_bytes(&new_frame).expect("old daemon must still parse the build_id");
        assert_eq!(old.seq, 4);
        assert_eq!(
            old.req,
            OldReq::Mount {
                build_id: "b-new".into()
            }
        );
    }

    #[test]
    fn reply_roundtrip_all_variants() {
        for resp in [
            Resp::Mounted {
                staging_quota_bytes: 10 << 30,
            },
            Resp::BackingId(3),
            Resp::Ok,
            Resp::Err(ErrKind::DigestMismatch),
            Resp::Err(ErrKind::Retryable("disk on fire".into())),
        ] {
            let frame = Reply { seq: 7, resp };
            assert_eq!(roundtrip(&frame), frame);
        }
    }

    #[test]
    fn max_batch_fits_in_frame() {
        // The protocol promises PROMOTE_CHUNKS_MAX digests fit under
        // MAX_FRAME_BYTES; if someone bumps one constant without the
        // other, encode() starts failing at runtime. Pin it here.
        let frame = Request {
            seq: u32::MAX,
            req: Req::PromoteChunks {
                chunk_digests: vec![[0xFF; 32]; PROMOTE_CHUNKS_MAX],
            },
        };
        let bytes = encode(&frame).expect("max batch must encode");
        assert!(
            bytes.len() <= MAX_FRAME_BYTES,
            "max PromoteChunks frame is {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn oversize_frame_rejected_before_send() {
        // A Retryable payload large enough to blow the frame budget.
        let frame = Reply {
            seq: 0,
            resp: Resp::Err(ErrKind::Retryable("x".repeat(MAX_FRAME_BYTES))),
        };
        assert!(matches!(encode(&frame), Err(FrameError::Oversize(_))));
    }

    #[test]
    fn build_fatal_classification() {
        // Waiting can fix these two; retrying must not loop forever on
        // the rest.
        assert!(!ErrKind::Retryable("x".into()).is_build_fatal());
        assert!(!ErrKind::RaceTimeout.is_build_fatal());
        for fatal in [
            ErrKind::DigestMismatch,
            ErrKind::NotRegular,
            ErrKind::TooLarge,
            ErrKind::BadBuildId,
            ErrKind::AlreadyMounted,
            ErrKind::DuplicateBuildId,
            ErrKind::BatchTooLarge,
            ErrKind::BadFuseFd,
            ErrKind::Unauthorized,
        ] {
            assert!(fatal.is_build_fatal(), "{fatal:?} must be build-fatal");
        }
    }

    /// End-to-end over a real socketpair: frame bytes survive, the fd
    /// arrives as ancillary data, and an oversize datagram is reported
    /// as Truncated rather than silently clipped.
    #[test]
    fn seqpacket_frame_and_fd_roundtrip() {
        use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
        use std::io::{Read, Seek, Write};

        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        )
        .expect("socketpair");

        // A request with no fd.
        let req = Request {
            seq: 1,
            req: Req::BackingClose { backing_id: 9 },
        };
        let bytes = encode(&req).unwrap();
        send_frame(a.as_raw_fd(), &bytes, &[]).unwrap();
        let got = recv_frame(b.as_raw_fd()).unwrap();
        assert!(got.fds.is_empty());
        assert_eq!(decode::<Request>(&got.bytes).unwrap(), req);

        // A request carrying an fd: write through the received fd and
        // observe the bytes through the original — proves it is the
        // same open file description, not a copy of the path.
        let mut tmp = tempfile::tempfile().unwrap();
        let req = Request {
            seq: 2,
            req: Req::BackingOpen,
        };
        let bytes = encode(&req).unwrap();
        send_frame(a.as_raw_fd(), &bytes, &[tmp.as_raw_fd()]).unwrap();
        let got = recv_frame(b.as_raw_fd()).unwrap();
        assert_eq!(got.fds.len(), 1);
        let mut received = std::fs::File::from(got.fds.into_iter().next().unwrap());
        received.write_all(b"via scm_rights").unwrap();
        tmp.rewind().unwrap();
        let mut back = String::new();
        tmp.read_to_string(&mut back).unwrap();
        assert_eq!(back, "via scm_rights");

        // Oversize datagram → MSG_TRUNC → Truncated. Sent with raw
        // sendmsg: `send_frame` itself refuses to send oversize frames
        // (debug_assert), but the receiver must still survive a peer
        // that bypasses the library and writes to the socket directly.
        let huge = vec![0u8; MAX_FRAME_BYTES + 1];
        let iov = [IoSlice::new(&huge)];
        sendmsg::<()>(a.as_raw_fd(), &iov, &[], MsgFlags::empty(), None).unwrap();
        assert!(matches!(
            recv_frame(b.as_raw_fd()),
            Err(FrameError::Truncated)
        ));

        // Peer close → Eof.
        drop(a);
        assert!(matches!(recv_frame(b.as_raw_fd()), Err(FrameError::Eof)));
    }
}

//! Minimal raw io_uring layer for the fuse-over-io_uring transport.
//!
//! Why raw syscalls instead of the `io-uring` crate: fuse ring
//! registration (`FUSE_IO_URING_CMD_REGISTER`) requires an
//! `IORING_OP_URING_CMD` SQE with `sqe->addr` pointing at a 2-element
//! iovec array **and `sqe->len == 2`** (`fuse_uring_get_iovec_from_sqe`
//! rejects anything else with EINVAL). The crate's `UringCmd80` opcode
//! builder exposes `addr` but not `len`, and its `Entry128` has no
//! escape hatch — so the crate cannot express the command at all. The
//! subset we need (setup, two mmaps, SQE128 push, enter, CQE drain) is
//! ~200 lines against a frozen uAPI; a dependency would not make it
//! smaller.
//!
//! Concurrency model: two usage patterns, one per ring role.
//!
//! - **Queue rings** (one per FUSE queue) have a single owner thread
//!   that stages SQEs with [`Ring::push`] and flushes them with
//!   [`Ring::submit`] / [`Ring::submit_and_wait`]; only that thread
//!   may drain the CQ. Nothing else ever touches a queue ring's SQ —
//!   slow workers wake the owner via `IORING_OP_MSG_RING` *into* the
//!   ring instead.
//! - **The waker ring** is shared by every slow worker through
//!   [`Ring::push_submit_drain`], which holds the SQ mutex across
//!   push + enter + CQ drain so concurrent wakes serialize and the CQ
//!   has exactly one consumer at a time.
//!
//! Mixing the patterns on one ring (e.g. `submit_and_wait` racing
//! `push_submit_drain`) would race the CQ head — don't.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use nix::libc;

// ─── io_uring uAPI constants (include/uapi/linux/io_uring.h) ────────────

const IORING_SETUP_CQSIZE: u32 = 1 << 3;
const IORING_SETUP_CLAMP: u32 = 1 << 4;
const IORING_SETUP_SQE128: u32 = 1 << 10;

const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;

const IORING_ENTER_GETEVENTS: u32 = 1;

const IORING_OP_MSG_RING: u8 = 40;
const IORING_OP_URING_CMD: u8 = 46;

/// Slot size with `IORING_SETUP_SQE128`.
const SQE128_SZ: usize = 128;
/// `sizeof(struct io_uring_cqe)` without `IORING_SETUP_CQE32` (the
/// fuse uring path only requires big SQEs, not big CQEs).
const CQE_SZ: usize = 16;

// ─── uAPI structs ────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqringOffsets,
    cq_off: CqringOffsets,
}

/// One 128-byte SQE, laid out for `IORING_OP_URING_CMD`: `cmd_op`
/// occupies the `off` union at byte 8 and the 80-byte command area
/// starts at byte 48 (`addr3`/`__pad2` + the SQE128 extension).
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Sqe128 {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    cmd_op: u32,
    _pad1: u32,
    addr: u64,
    len: u32,
    uring_cmd_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    splice_fd_in: i32,
    cmd: [u8; 80],
}

impl Sqe128 {
    /// An `IORING_OP_URING_CMD` SQE against `fd` with the fuse command
    /// layout: `addr`/`len` describe the headers+payload iovec pair
    /// (only read by REGISTER; COMMIT_AND_FETCH reuses the buffers
    /// captured at registration), `cmd` is the
    /// `struct fuse_uring_cmd_req`.
    pub(super) fn uring_cmd(
        fd: i32,
        cmd_op: u32,
        iovec_addr: u64,
        iovec_len: u32,
        user_data: u64,
        cmd: [u8; 80],
    ) -> Self {
        Self {
            opcode: IORING_OP_URING_CMD,
            flags: 0,
            ioprio: 0,
            fd,
            cmd_op,
            _pad1: 0,
            addr: iovec_addr,
            len: iovec_len,
            uring_cmd_flags: 0,
            user_data,
            buf_index: 0,
            personality: 0,
            splice_fd_in: 0,
            cmd,
        }
    }

    /// An `IORING_OP_MSG_RING` SQE: post a CQE carrying
    /// (`data`, `res`) into the ring behind `target_fd` — the engine's
    /// slow-worker → queue-thread wake. The source CQE (on the ring
    /// this SQE is submitted to) carries `user_data` and `res == 0` on
    /// success.
    ///
    /// `data` lands in the uAPI `off` field (bytes 8..16), which this
    /// struct splits as `cmd_op`/`_pad1`; the low/high u32 split below
    /// assumes little-endian, which every supported target is (the
    /// layout unit test pins the full u64).
    pub(super) fn msg_ring(target_fd: i32, data: u64, res: u32, user_data: u64) -> Self {
        Self {
            opcode: IORING_OP_MSG_RING,
            flags: 0,
            ioprio: 0,
            fd: target_fd,
            cmd_op: data as u32,
            _pad1: (data >> 32) as u32,
            addr: 0, // IORING_MSG_DATA
            len: res,
            uring_cmd_flags: 0, // msg_ring_flags
            user_data,
            buf_index: 0,
            personality: 0,
            splice_fd_in: 0,
            cmd: [0; 80],
        }
    }
}

/// One completion.
#[derive(Debug, Clone, Copy)]
pub(super) struct Cqe {
    pub user_data: u64,
    pub res: i32,
}

// ─── syscall wrappers ────────────────────────────────────────────────────

fn io_uring_setup(entries: u32, params: &mut IoUringParams) -> io::Result<OwnedFd> {
    // SAFETY: `params` is a valid, writable io_uring_params; the
    // kernel returns a fresh fd we immediately take ownership of.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            entries,
            params as *mut IoUringParams,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a non-negative return from io_uring_setup is a new fd
    // owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(ret as i32) })
}

fn io_uring_enter(fd: i32, to_submit: u32, min_complete: u32, flags: u32) -> io::Result<u32> {
    loop {
        // SAFETY: plain syscall on an open io_uring fd; no userspace
        // pointers are passed (sig = NULL).
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_uring_enter,
                fd,
                to_submit,
                min_complete,
                flags,
                std::ptr::null::<libc::sigset_t>(),
                0usize,
            )
        };
        if ret >= 0 {
            return Ok(ret as u32);
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(err);
    }
}

// ─── mmap regions ────────────────────────────────────────────────────────

struct MmapRegion {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the region is plain shared memory; all concurrent access
// goes through the atomics / mutex discipline documented on `Ring`.
unsafe impl Send for MmapRegion {}
// SAFETY: see above.
unsafe impl Sync for MmapRegion {}

impl MmapRegion {
    fn map(fd: &OwnedFd, len: usize, offset: i64) -> io::Result<Self> {
        // SAFETY: standard io_uring ring mapping; the kernel validates
        // len/offset against the ring geometry.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd.as_raw_fd(),
                offset,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: NonNull::new(ptr.cast()).expect("mmap returned non-null"),
            len,
        })
    }

    fn at(&self, off: u32) -> *mut u8 {
        debug_assert!((off as usize) < self.len);
        // SAFETY: offsets come from the kernel's io_uring_params and
        // are within the mapped length (debug-asserted).
        unsafe { self.ptr.as_ptr().add(off as usize) }
    }

    fn atomic_u32(&self, off: u32) -> &AtomicU32 {
        // SAFETY: the kernel-provided offset points at a 4-aligned u32
        // shared with the kernel; AtomicU32 has the same layout.
        unsafe { &*self.at(off).cast::<AtomicU32>() }
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from a successful mmap of that length.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

// ─── the ring ────────────────────────────────────────────────────────────

/// Submission-side state, mutex-guarded so any worker thread can
/// commit replies directly.
struct Sq {
    /// Local copy of the tail (the kernel only reads it).
    tail: u32,
}

pub(super) struct Ring {
    fd: OwnedFd,
    sq_ring: MmapRegion,
    /// Separate CQ mapping on pre-`IORING_FEAT_SINGLE_MMAP` kernels;
    /// `None` when the SQ mapping covers both.
    cq_ring: Option<MmapRegion>,
    sqes: MmapRegion,
    sq: Mutex<Sq>,
    sq_mask: u32,
    sq_off: SqringOffsets,
    cq_mask: u32,
    cq_off: CqringOffsets,
    sq_entries: u32,
}

impl Ring {
    /// Create an `IORING_SETUP_SQE128` ring with at least `sq_entries`
    /// submission slots and `cq_entries` completion slots (both
    /// kernel-clamped).
    pub(super) fn new(sq_entries: u32, cq_entries: u32) -> io::Result<Self> {
        let mut params = IoUringParams {
            flags: IORING_SETUP_SQE128 | IORING_SETUP_CQSIZE | IORING_SETUP_CLAMP,
            cq_entries,
            ..Default::default()
        };
        let fd = io_uring_setup(sq_entries.max(1), &mut params)?;

        let sq_sz = params.sq_off.array as usize + params.sq_entries as usize * 4;
        let cq_sz = params.cq_off.cqes as usize + params.cq_entries as usize * CQE_SZ;
        let single = params.features & IORING_FEAT_SINGLE_MMAP != 0;
        let sq_ring = MmapRegion::map(
            &fd,
            if single { sq_sz.max(cq_sz) } else { sq_sz },
            IORING_OFF_SQ_RING,
        )?;
        let cq_ring = if single {
            None
        } else {
            Some(MmapRegion::map(&fd, cq_sz, IORING_OFF_CQ_RING)?)
        };
        let sqes = MmapRegion::map(&fd, params.sq_entries as usize * SQE128_SZ, IORING_OFF_SQES)?;

        // SQ ring mask/entries are constants after setup.
        let sq_mask = sq_ring
            .atomic_u32(params.sq_off.ring_mask)
            .load(Ordering::Relaxed);
        let cq_region = cq_ring.as_ref().unwrap_or(&sq_ring);
        let cq_mask = cq_region
            .atomic_u32(params.cq_off.ring_mask)
            .load(Ordering::Relaxed);
        let tail = sq_ring
            .atomic_u32(params.sq_off.tail)
            .load(Ordering::Relaxed);

        Ok(Self {
            fd,
            sq_ring,
            cq_ring,
            sqes,
            sq: Mutex::new(Sq { tail }),
            sq_mask,
            sq_off: params.sq_off,
            cq_mask,
            cq_off: params.cq_off,
            sq_entries: params.sq_entries,
        })
    }

    pub(super) fn sq_capacity(&self) -> u32 {
        self.sq_entries
    }

    /// The ring's fd, for `IORING_OP_MSG_RING` targeting. The fd stays
    /// open for the `Ring`'s lifetime (the engine keeps every ring in
    /// `Shared`, so a worker-held raw fd cannot dangle).
    pub(super) fn raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }

    fn cq_region(&self) -> &MmapRegion {
        self.cq_ring.as_ref().unwrap_or(&self.sq_ring)
    }

    /// Write one SQE into the SQ under the lock. No syscall — the SQE
    /// is staged until the next `io_uring_enter`. Fails with
    /// `WouldBlock` if the SQ is full (the engine sizes each ring so a
    /// full SQ means a logic bug, not a transient state: each
    /// in-flight fuse request owns exactly one ring slot).
    fn push_locked(&self, sq: &mut Sq, sqe: &Sqe128) -> io::Result<()> {
        let head = self
            .sq_ring
            .atomic_u32(self.sq_off.head)
            .load(Ordering::Acquire);
        if sq.tail.wrapping_sub(head) >= self.sq_entries {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let idx = sq.tail & self.sq_mask;
        // SAFETY: idx < sq_entries, each slot is SQE128_SZ bytes in
        // the sqes mapping, and the slot is free (head check above);
        // Sqe128 is repr(C), 128 bytes, plain data.
        unsafe {
            let slot = self.sqes.ptr.as_ptr().add(idx as usize * SQE128_SZ);
            std::ptr::copy_nonoverlapping((sqe as *const Sqe128).cast::<u8>(), slot, SQE128_SZ);
            // Classic indirection array: array[idx] = idx.
            let array = self
                .sq_ring
                .at(self.sq_off.array)
                .cast::<u32>()
                .add(idx as usize);
            array.write(idx);
        }
        sq.tail = sq.tail.wrapping_add(1);
        self.sq_ring
            .atomic_u32(self.sq_off.tail)
            .store(sq.tail, Ordering::Release);
        Ok(())
    }

    /// Stage one SQE without submitting (queue-thread pattern: batch
    /// the commits a CQE batch produced, then flush them all with the
    /// single `io_uring_enter` of the next [`Ring::submit_and_wait`]).
    pub(super) fn push(&self, sqe: &Sqe128) -> io::Result<()> {
        let mut sq = self.sq.lock().expect("sq mutex poisoned");
        self.push_locked(&mut sq, sqe)
    }

    /// Pending (staged but not kernel-consumed) SQE count.
    fn pending(&self) -> u32 {
        let sq = self.sq.lock().expect("sq mutex poisoned");
        let head = self
            .sq_ring
            .atomic_u32(self.sq_off.head)
            .load(Ordering::Acquire);
        sq.tail.wrapping_sub(head)
    }

    /// Submit every staged SQE without waiting for completions. On
    /// `Err` the consumed-state of the staged SQEs is unknown — the
    /// engine treats that as fatal for the queue (poison + park).
    pub(super) fn submit(&self) -> io::Result<u32> {
        let pending = self.pending();
        io_uring_enter(self.fd.as_raw_fd(), pending, 0, 0)
    }

    /// Submit every staged SQE and block until at least one CQE is
    /// available, then drain all of them. One syscall covers both the
    /// flush and the wait — this is the queue thread's steady state
    /// (libfuse's `io_uring_submit_and_wait` shape).
    ///
    /// Single-consumer contract: only the ring's owner thread may call
    /// this (the CQ head update below is not synchronized).
    pub(super) fn submit_and_wait(&self, out: &mut Vec<Cqe>) -> io::Result<()> {
        out.clear();
        loop {
            // Re-read the staged count every round: the SQ head tracks
            // kernel consumption, so this is 0 on a retry after a
            // spurious empty drain (no double submit) and non-zero
            // again only if the kernel left SQEs behind (rare partial
            // submission), which then get retried instead of stranded.
            let to_submit = self.pending();
            io_uring_enter(self.fd.as_raw_fd(), to_submit, 1, IORING_ENTER_GETEVENTS)?;
            if self.drain_cq(out) > 0 {
                return Ok(());
            }
        }
    }

    /// Scan the currently visible CQEs for a failure (`res < 0`)
    /// without consuming anything — the registration gate's check. The
    /// CQ head only advances in `drain_cq`, so the owner thread's
    /// later [`Ring::submit_and_wait`] still sees (and accounts for)
    /// every CQE peeked here.
    pub(super) fn peek_failure(&self) -> Option<Cqe> {
        let region = self.cq_region();
        let tail = region.atomic_u32(self.cq_off.tail).load(Ordering::Acquire);
        let mut i = region.atomic_u32(self.cq_off.head).load(Ordering::Relaxed);
        while i != tail {
            let idx = i & self.cq_mask;
            // SAFETY: same slot layout/publication argument as
            // `drain_cq` below.
            let (user_data, res) = unsafe {
                let p = region.at(self.cq_off.cqes).add(idx as usize * CQE_SZ);
                (p.cast::<u64>().read(), p.add(8).cast::<i32>().read())
            };
            if res < 0 {
                return Some(Cqe { user_data, res });
            }
            i = i.wrapping_add(1);
        }
        None
    }

    /// Drain every available CQE into `out` (append); returns the
    /// count. Callers serialize per the concurrency model in the
    /// module docs.
    fn drain_cq(&self, out: &mut Vec<Cqe>) -> usize {
        let region = self.cq_region();
        let head_at = region.atomic_u32(self.cq_off.head);
        let tail = region.atomic_u32(self.cq_off.tail).load(Ordering::Acquire);
        let head = head_at.load(Ordering::Relaxed);
        if head == tail {
            return 0;
        }
        // Wrapping iteration: head/tail are free-running u32 counters,
        // so `head..tail` would silently yield nothing once tail wraps
        // past u32::MAX.
        let mut n = 0;
        let mut i = head;
        while i != tail {
            let idx = i & self.cq_mask;
            // SAFETY: cqes offset + idx*CQE_SZ is inside the CQ
            // mapping; the slot is published by the Acquire tail read
            // above. The first 12 bytes of io_uring_cqe are user_data
            // (u64) + res (i32).
            let (user_data, res) = unsafe {
                let p = region.at(self.cq_off.cqes).add(idx as usize * CQE_SZ);
                (p.cast::<u64>().read(), p.add(8).cast::<i32>().read())
            };
            out.push(Cqe { user_data, res });
            n += 1;
            i = i.wrapping_add(1);
        }
        head_at.store(tail, Ordering::Release);
        n
    }

    /// Push one SQE, submit it, and drain whatever CQEs are available
    /// (appended to `out`) — the shared-waker pattern: the SQ lock is
    /// held across all three steps, so concurrent callers serialize
    /// and the CQ has exactly one consumer at a time.
    ///
    /// Exactly-once contract: on `Err` the SQE is **not** left in the
    /// SQ (the tail is rolled back unless the kernel consumed the slot
    /// despite the error). A failed-but-still-queued SQE that a later
    /// submission carries would fire a stale wake or poison. The lock
    /// is held across `io_uring_enter` to make the rollback race-free;
    /// without `IORING_SETUP_SQPOLL` the kernel only reads the SQ
    /// inside that call, so a held lock means nobody else can consume
    /// or repush the slot meanwhile.
    pub(super) fn push_submit_drain(&self, sqe: &Sqe128, out: &mut Vec<Cqe>) -> io::Result<()> {
        let mut sq = self.sq.lock().expect("sq mutex poisoned");
        self.push_locked(&mut sq, sqe)?;
        let head_at = self.sq_ring.atomic_u32(self.sq_off.head);
        let head = head_at.load(Ordering::Acquire);
        match io_uring_enter(self.fd.as_raw_fd(), sq.tail.wrapping_sub(head), 0, 0) {
            Ok(_) => {
                self.drain_cq(out);
                Ok(())
            }
            // The kernel may consume SQEs and still error (e.g. a
            // failure after partial consumption): head == tail means
            // our SQE went through, so the command is in flight.
            Err(_) if head_at.load(Ordering::Acquire) == sq.tail => {
                self.drain_cq(out);
                Ok(())
            }
            Err(e) => {
                sq.tail = sq.tail.wrapping_sub(1);
                self.sq_ring
                    .atomic_u32(self.sq_off.tail)
                    .store(sq.tail, Ordering::Release);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-rolled SQE128 layout must put every field at the uAPI
    /// offset (`struct io_uring_sqe` + the 80-byte command extension);
    /// a drift here makes the kernel read a garbage iovec pointer.
    #[test]
    fn sqe128_layout_matches_uapi() {
        assert_eq!(std::mem::size_of::<Sqe128>(), 128);
        let sqe = Sqe128::uring_cmd(7, 2, 0x1111_2222_3333_4444, 2, 99, [0xAB; 80]);
        // SAFETY: Sqe128 is repr(C) plain data; viewing it as bytes is
        // exactly what push_locked does.
        let b: [u8; 128] = unsafe { std::mem::transmute(sqe) };
        assert_eq!(b[0], IORING_OP_URING_CMD, "opcode at 0");
        assert_eq!(
            i32::from_ne_bytes(b[4..8].try_into().unwrap()),
            7,
            "fd at 4"
        );
        assert_eq!(
            u32::from_ne_bytes(b[8..12].try_into().unwrap()),
            2,
            "cmd_op at 8"
        );
        assert_eq!(
            u64::from_ne_bytes(b[16..24].try_into().unwrap()),
            0x1111_2222_3333_4444,
            "addr at 16"
        );
        assert_eq!(
            u32::from_ne_bytes(b[24..28].try_into().unwrap()),
            2,
            "len at 24"
        );
        assert_eq!(
            u64::from_ne_bytes(b[32..40].try_into().unwrap()),
            99,
            "user_data at 32"
        );
        assert_eq!(&b[48..128], &[0xAB; 80], "command area at 48");
    }

    /// Ring creation + a no-op enter must work on a uring-capable
    /// kernel; on kernels/sandboxes where io_uring is unavailable the
    /// error must be a clean io::Error (that is the mount-time probe
    /// path), never a panic.
    #[test]
    fn ring_setup_probes_cleanly() {
        match Ring::new(8, 16) {
            Ok(ring) => {
                assert!(ring.sq_capacity() >= 8);
                // GETEVENTS with min_complete=0 returns immediately.
                io_uring_enter(ring.fd.as_raw_fd(), 0, 0, IORING_ENTER_GETEVENTS).unwrap();
            }
            Err(e) => {
                // EPERM/ENOSYS (seccomp, io_uring_disabled sysctl):
                // exactly what `prepare()` turns into a mount error.
                assert!(e.raw_os_error().is_some(), "unexpected error shape: {e}");
            }
        }
    }

    /// The `IORING_OP_MSG_RING` SQE layout: `data` must land in the
    /// uAPI `off` field (bytes 8..16) as one little-endian u64 — a
    /// drifted split would post a garbage user_data into the target
    /// ring, which the queue thread would treat as an entry index.
    #[test]
    fn msg_ring_layout_matches_uapi() {
        let sqe = Sqe128::msg_ring(7, 0x1111_2222_3333_4444, 0x77, 9);
        // SAFETY: Sqe128 is repr(C) plain data, exactly 128 bytes.
        let b: [u8; 128] = unsafe { std::mem::transmute(sqe) };
        assert_eq!(b[0], IORING_OP_MSG_RING, "opcode at 0");
        assert_eq!(
            i32::from_ne_bytes(b[4..8].try_into().unwrap()),
            7,
            "target ring fd at 4"
        );
        assert_eq!(
            u64::from_ne_bytes(b[8..16].try_into().unwrap()),
            0x1111_2222_3333_4444,
            "data (target user_data) fills off at 8..16"
        );
        assert_eq!(
            u64::from_ne_bytes(b[16..24].try_into().unwrap()),
            0,
            "addr at 16 must be 0 (IORING_MSG_DATA)"
        );
        assert_eq!(
            u32::from_ne_bytes(b[24..28].try_into().unwrap()),
            0x77,
            "res (target cqe res) at 24"
        );
        assert_eq!(
            u64::from_ne_bytes(b[32..40].try_into().unwrap()),
            9,
            "source user_data at 32"
        );
    }

    /// Full stage → flush → complete round trip through the same
    /// `push`/`submit_and_wait` paths the queue threads use, with NOP
    /// SQEs (a malformed tail/array/SQE write would surface as a hang
    /// or a missing/garbled CQE here — exactly the class of bug that
    /// wedges a real mount, where it is much harder to observe).
    /// Skips silently where io_uring itself is unavailable.
    #[test]
    fn nop_round_trip_completes() {
        let Ok(ring) = Ring::new(8, 16) else {
            eprintln!("io_uring unavailable; skipping round-trip test");
            return;
        };
        // A NOP is an all-zero SQE except user_data; reuse the
        // uring_cmd constructor and overwrite the opcode so the test
        // goes through the byte-identical push path.
        for i in 0..3u64 {
            let mut sqe = Sqe128::uring_cmd(-1, 0, 0, 0, 0xAA00 + i, [0; 80]);
            sqe.opcode = 0; // IORING_OP_NOP
            sqe.fd = -1;
            ring.push(&sqe).unwrap();
        }
        let mut got = Vec::new();
        let mut cqes = Vec::new();
        while got.len() < 3 {
            // The first call submits all three staged NOPs and waits;
            // later iterations (if the CQEs arrive split) submit 0.
            ring.submit_and_wait(&mut cqes).unwrap();
            for c in &cqes {
                assert_eq!(c.res, 0, "NOP must complete with res=0");
                got.push(c.user_data);
            }
        }
        got.sort_unstable();
        assert_eq!(got, vec![0xAA00, 0xAA01, 0xAA02]);
    }

    /// The engine's slow-worker → queue-thread wake: an
    /// `IORING_OP_MSG_RING` submitted on one ring must post a CQE with
    /// the chosen (user_data, res) into the target ring and wake its
    /// `submit_and_wait`. A broken wake would strand every slow-op
    /// commit in the real engine.
    #[test]
    fn msg_ring_wakes_target_ring() {
        let (Ok(waker), Ok(target)) = (Ring::new(2, 8), Ring::new(4, 8)) else {
            eprintln!("io_uring unavailable; skipping msg_ring test");
            return;
        };
        let mut out = Vec::new();
        waker
            .push_submit_drain(
                &Sqe128::msg_ring(target.raw_fd(), 0xFEED, 0x77, 1),
                &mut out,
            )
            .unwrap();
        // The source CQE completes inline: res 0 on success.
        assert_eq!(out.len(), 1, "source CQE must drain inline");
        assert_eq!(out[0].user_data, 1);
        assert_eq!(
            out[0].res, 0,
            "MSG_RING must succeed (landed in 5.18; fuse-over-io_uring needs 6.14)"
        );

        let mut cqes = Vec::new();
        target.submit_and_wait(&mut cqes).unwrap();
        assert_eq!(cqes.len(), 1);
        assert_eq!(cqes[0].user_data, 0xFEED, "target CQE carries the data");
        assert_eq!(cqes[0].res, 0x77, "target CQE carries the res");
    }
}

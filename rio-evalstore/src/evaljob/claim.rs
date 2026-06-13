//! Advisory memfd claim table (ADR-024 "sharing by fork order"): a
//! fixed-size `MAP_SHARED` table of `key → (pid, timestamp)` claims
//! created by the eval parent BEFORE forking, so two live workers
//! don't ingest the same big source tree at the same time.
//!
//! Strictly an optimization — correctness never depends on it
//! (`r[bc.evalparent.claim-advisory]`): the CAS's single-writer
//! segments and idempotent index already make concurrent ingest safe;
//! a lost or stale claim costs duplicate CPU/IO, never wrong content.
//! A claim is stale (ignorable) once its pid is dead or it is older
//! than [`STALE_AFTER`] — without the staleness rule the dedup would
//! invert into a stall on worker crash.
//!
//! Keys are `blake3(origin fs path)`: the content digest isn't known
//! until the ingest finishes, but two workers racing on the same tree
//! race on the same origin path.
//!
//! TODO: a waiting loser currently re-ingests after the winner
//! releases — cross-process reuse of the winner's metadata needs a
//! pack-index refresh (the winner's records live in its own segment),
//! deferred until the pack store grows a reload path. The table still
//! removes the simultaneous duplicate work, which is the expensive
//! case (two cold nixpkgs ingests racing on one disk).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Claims older than this are stale regardless of pid liveness — the
/// largest plausible single-tree ingest (ADR-024: ~60s headroom over
/// the ~38s nixpkgs-scale worst case).
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// Slot count. Open-addressed with linear probing; claims are
/// short-lived (one per in-flight tree ingest), so collisions are
/// rare and a full table simply degrades to "no claim" — advisory.
const SLOTS: usize = 1024;

const STATE_FREE: u32 = 0;
const STATE_BUSY: u32 = 1; // transient: key being written
const STATE_CLAIMED: u32 = 2;

#[repr(C)]
struct Slot {
    state: AtomicU32,
    pid: AtomicU32,
    ts_secs: AtomicU64,
    key: [AtomicU64; 4], // 32-byte key as 4 LE words
}

/// Outcome of a claim attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// We hold the claim; call [`ClaimTable::release`] when done.
    Won,
    /// A live, fresh claim by `pid` exists.
    Lost { pid: u32 },
}

/// Shared-memory claim table. Created on a `memfd` by the parent and
/// inherited by every fork worker (`MAP_SHARED` — all generations see
/// one table). Lock-free: slot transitions are single CAS operations.
pub struct ClaimTable {
    map: *mut Slot,
    _fd: OwnedFd,
}

// SAFETY: all slot access is through atomics on shared memory.
unsafe impl Send for ClaimTable {}
unsafe impl Sync for ClaimTable {}

impl ClaimTable {
    /// Create the table on an anonymous memfd. Call once in the
    /// parent, before any fork.
    pub fn create() -> io::Result<ClaimTable> {
        let len = SLOTS * std::mem::size_of::<Slot>();
        // SAFETY: plain syscalls; fd ownership transferred to OwnedFd.
        let fd = unsafe {
            let raw = libc::memfd_create(c"rio-eval-claims".as_ptr(), libc::MFD_CLOEXEC);
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            OwnedFd::from_raw_fd(raw)
        };
        // SAFETY: fd is valid; ftruncate + mmap of the sized region.
        let map = unsafe {
            if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) < 0 {
                return Err(io::Error::last_os_error());
            }
            let p = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            );
            if p == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            p.cast::<Slot>()
        };
        // memfd pages start zeroed = all slots STATE_FREE.
        Ok(ClaimTable { map, _fd: fd })
    }

    fn slot(&self, i: usize) -> &Slot {
        // SAFETY: i < SLOTS; the mapping lives as long as self.
        unsafe { &*self.map.add(i % SLOTS) }
    }

    fn key_words(key: &[u8; 32]) -> [u64; 4] {
        let mut w = [0u64; 4];
        for (i, chunk) in key.chunks_exact(8).enumerate() {
            w[i] = u64::from_le_bytes(chunk.try_into().expect("8-byte chunk"));
        }
        w
    }

    fn slot_key_matches(slot: &Slot, words: &[u64; 4]) -> bool {
        (0..4).all(|i| slot.key[i].load(Ordering::Acquire) == words[i])
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn pid_alive(pid: u32) -> bool {
        // SAFETY: kill with signal 0 only probes existence.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    /// Stale rule: dead pid OR older than [`STALE_AFTER`]
    /// (`r[bc.evalparent.claim-advisory]`).
    fn is_stale(slot: &Slot) -> bool {
        let pid = slot.pid.load(Ordering::Acquire);
        let ts = slot.ts_secs.load(Ordering::Acquire);
        Self::now_secs().saturating_sub(ts) > STALE_AFTER.as_secs() || !Self::pid_alive(pid)
    }

    /// Try to claim `key` for the calling process.
    pub fn claim(&self, key: &[u8; 32]) -> ClaimOutcome {
        let words = Self::key_words(key);
        let start = (words[0] as usize) % SLOTS;
        let mut free_candidate: Option<usize> = None;
        for probe in 0..SLOTS {
            let i = (start + probe) % SLOTS;
            let slot = self.slot(i);
            match slot.state.load(Ordering::Acquire) {
                STATE_FREE => {
                    if free_candidate.is_none() {
                        free_candidate = Some(i);
                    }
                    // A free slot ends the probe chain for this key.
                    break;
                }
                STATE_CLAIMED if Self::slot_key_matches(slot, &words) => {
                    if Self::is_stale(slot) {
                        // Take over the stale claim in place.
                        slot.pid.store(std::process::id(), Ordering::Release);
                        slot.ts_secs.store(Self::now_secs(), Ordering::Release);
                        return ClaimOutcome::Won;
                    }
                    return ClaimOutcome::Lost {
                        pid: slot.pid.load(Ordering::Acquire),
                    };
                }
                _ => continue,
            }
        }
        let Some(i) = free_candidate else {
            // Table full: advisory means "behave as if unclaimed".
            return ClaimOutcome::Won;
        };
        let slot = self.slot(i);
        if slot
            .state
            .compare_exchange(STATE_FREE, STATE_BUSY, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Lost the slot race; treat as unclaimed (advisory).
            return ClaimOutcome::Won;
        }
        for (j, w) in words.iter().enumerate() {
            slot.key[j].store(*w, Ordering::Release);
        }
        slot.pid.store(std::process::id(), Ordering::Release);
        slot.ts_secs.store(Self::now_secs(), Ordering::Release);
        slot.state.store(STATE_CLAIMED, Ordering::Release);
        ClaimOutcome::Won
    }

    /// Release a claim this process holds. Releasing a claim another
    /// process took over (stale takeover) is a no-op.
    pub fn release(&self, key: &[u8; 32]) {
        let words = Self::key_words(key);
        let start = (words[0] as usize) % SLOTS;
        for probe in 0..SLOTS {
            let i = (start + probe) % SLOTS;
            let slot = self.slot(i);
            match slot.state.load(Ordering::Acquire) {
                STATE_FREE => return,
                STATE_CLAIMED
                    if Self::slot_key_matches(slot, &words)
                        && slot.pid.load(Ordering::Acquire) == std::process::id() =>
                {
                    slot.state.store(STATE_FREE, Ordering::Release);
                    return;
                }
                _ => continue,
            }
        }
    }

    /// Block (bounded by the stale rule) until no live claim by
    /// another process exists for `key`. Returns immediately when the
    /// key is unclaimed or claimed by us.
    pub fn wait_unclaimed(&self, key: &[u8; 32]) {
        let deadline = std::time::Instant::now() + STALE_AFTER;
        loop {
            match self.claim_state(key) {
                Some(pid) if pid != std::process::id() => {
                    if std::time::Instant::now() >= deadline {
                        return; // stale by time — proceed regardless
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => return,
            }
        }
    }

    /// Live (non-stale) claimant pid for `key`, if any.
    fn claim_state(&self, key: &[u8; 32]) -> Option<u32> {
        let words = Self::key_words(key);
        let start = (words[0] as usize) % SLOTS;
        for probe in 0..SLOTS {
            let i = (start + probe) % SLOTS;
            let slot = self.slot(i);
            match slot.state.load(Ordering::Acquire) {
                STATE_FREE => return None,
                STATE_CLAIMED if Self::slot_key_matches(slot, &words) => {
                    if Self::is_stale(slot) {
                        return None;
                    }
                    return Some(slot.pid.load(Ordering::Acquire));
                }
                _ => continue,
            }
        }
        None
    }
}

impl Drop for ClaimTable {
    fn drop(&mut self) {
        let len = SLOTS * std::mem::size_of::<Slot>();
        // SAFETY: map/len are the exact mmap arguments.
        unsafe { libc::munmap(self.map.cast(), len) };
    }
}

/// RAII claim over one key: blocks until any other live claim clears,
/// then claims; releases on drop. The ingest call sites hold this for
/// the duration of one tree ingest.
pub struct ClaimGuard<'a> {
    table: &'a ClaimTable,
    key: [u8; 32],
}

impl<'a> ClaimGuard<'a> {
    pub fn acquire(table: &'a ClaimTable, key: [u8; 32]) -> ClaimGuard<'a> {
        loop {
            match table.claim(&key) {
                ClaimOutcome::Won => return ClaimGuard { table, key },
                ClaimOutcome::Lost { pid } if pid == std::process::id() => {
                    // Re-entrant ingest of the same tree in one
                    // process cannot happen (the store lock serializes
                    // it); treat as held.
                    return ClaimGuard { table, key };
                }
                ClaimOutcome::Lost { .. } => table.wait_unclaimed(&key),
            }
        }
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        self.table.release(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(tag: u8) -> [u8; 32] {
        [tag; 32]
    }

    // r[verify bc.evalparent.claim-advisory]
    #[test]
    fn claim_release_and_takeover() {
        let t = ClaimTable::create().unwrap();
        assert_eq!(t.claim(&key(1)), ClaimOutcome::Won);
        // Same process re-claiming its own live claim loses (the
        // caller is expected to know what it holds; the table only
        // answers "is someone else on this").
        match t.claim(&key(1)) {
            ClaimOutcome::Lost { pid } => assert_eq!(pid, std::process::id()),
            other => panic!("expected Lost, got {other:?}"),
        }
        t.release(&key(1));
        assert_eq!(t.claim(&key(1)), ClaimOutcome::Won);

        // Distinct keys don't interfere.
        assert_eq!(t.claim(&key(2)), ClaimOutcome::Won);
        t.release(&key(1));
        t.release(&key(2));
    }

    /// Dead-pid claims are stale and taken over — a crashed worker
    /// must not stall siblings (`r[bc.evalparent.claim-advisory]`).
    // r[verify bc.evalparent.claim-advisory]
    #[test]
    fn dead_pid_claim_is_stale() {
        let t = ClaimTable::create().unwrap();
        assert_eq!(t.claim(&key(7)), ClaimOutcome::Won);
        // Forge a dead claimant: pid 0xFFFFFFF0 cannot exist
        // (> kernel pid_max).
        let words = ClaimTable::key_words(&key(7));
        let start = (words[0] as usize) % SLOTS;
        for probe in 0..SLOTS {
            let slot = t.slot(start + probe);
            if slot.state.load(Ordering::Acquire) == STATE_CLAIMED
                && ClaimTable::slot_key_matches(slot, &words)
            {
                slot.pid.store(0xFFFF_FFF0, Ordering::Release);
                break;
            }
        }
        assert_eq!(t.claim(&key(7)), ClaimOutcome::Won, "dead pid = stale");
        // And wait_unclaimed returns immediately on a stale claim.
        t.wait_unclaimed(&key(7));
    }

    // r[verify bc.evalparent.claim-advisory]
    #[test]
    fn old_timestamp_claim_is_stale() {
        let t = ClaimTable::create().unwrap();
        assert_eq!(t.claim(&key(9)), ClaimOutcome::Won);
        let words = ClaimTable::key_words(&key(9));
        let start = (words[0] as usize) % SLOTS;
        for probe in 0..SLOTS {
            let slot = t.slot(start + probe);
            if slot.state.load(Ordering::Acquire) == STATE_CLAIMED
                && ClaimTable::slot_key_matches(slot, &words)
            {
                let old = ClaimTable::now_secs() - STALE_AFTER.as_secs() - 5;
                slot.ts_secs.store(old, Ordering::Release);
                break;
            }
        }
        // Live pid but past the 60s bound: stale.
        assert_eq!(t.claim(&key(9)), ClaimOutcome::Won);
    }
}

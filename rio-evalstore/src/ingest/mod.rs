//! Parallel source-tree ingest: one read per file, two hash planes.
//!
//! ADR-024 ("Ingest is single-read, two hash planes"): eval blocks on the
//! NAR sha256 — the store path enters string context — so ingest sits on
//! the eval critical path. Each file is read exactly once; its bytes feed
//!
//! 1. the sequential **NAR-sha256 spine** in NAR traversal order (identity
//!    — this is what nix blocks on), and
//! 2. a parallel **FastCDC + blake3 chunking plane** (storage keys for the
//!    CAS, never seen by nix).
//!
//! The threading model is fixed by measurement + discrete-event simulation
//! over a real 52,855-file nixpkgs walk (dagbench INGEST-THREADING):
//! R blocking reader threads pulling from ONE shared deque of work items —
//! both unread directories (readdir) and files (open+read). Discovery
//! parallelism is the load-bearing property: true-cold ingest is
//! readdir-bound, and a single read-ahead issuer floors ~4× worse than
//! parallel readers regardless of window size. One sha256 spine consumes
//! file buffers strictly in NAR order; W chunk workers run FastCDC+blake3;
//! a single byte budget (owned by the work-deque mutex — see the
//! pipeline module docs) is simultaneously the prefetch window and the
//! tee bound. An oversized file (> budget) is admitted alone when the
//! budget is fully free.
//!
//! **Fork safety (hard rule):** zero threads exist at construction. All
//! pipeline threads are spawned inside [`ingest_tree`] via
//! [`std::thread::scope`] and joined before it returns — nothing outlives
//! the call, so a host that forks without exec (nix-eval-jobs workers)
//! never loses or deadlocks a pipeline thread.
//!
//! NAR framing is NOT re-implemented here: the spine emits the canonical
//! token stream through [`rio_nix::nar::frame`], the single shared
//! definition of the per-node token sequences (same emitters as
//! rio-builder's fused output walk and rio-store's NAR regeneration).

mod pipeline;
#[cfg(test)]
mod tests;

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Ingest pipeline tuning. Defaults are the measured recommendation from
/// the INGEST-THREADING simulation; P1 re-measures in Rust and may retune.
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// Blocking reader threads serving the shared work deque (readdir for
    /// directories, open+read for files). 8 saturates laptop-class NVMe;
    /// more is idle threads beyond device concurrency.
    pub reader_threads: usize,
    /// FastCDC+blake3 chunk workers. Simulated utilization never exceeded
    /// 0.27 at W=4 — plane 2 finishes inside the read shadow at W=2.
    pub chunk_workers: usize,
    /// Byte budget: combined prefetch window and tee bound.
    /// Sized so the largest common single file passes without streaming
    /// (32 MiB ≥ nixpkgs' 16.6 MiB hackage-packages.nix); a larger file is
    /// admitted alone when the budget is fully free. A quarter of the
    /// budget is reserved while any directory listing is in flight (see
    /// the pipeline module docs: the head reserve is what prevents the
    /// steal-lock degraded mode the P1 profiling campaign measured).
    pub byte_budget: u64,
    /// Test-only latency injection (cold-device / slow-spine simulation
    /// for the steal-regime regression test).
    #[cfg(test)]
    pub(crate) test_delays: TestDelays,
}

/// Test-only injected latencies: `read` is added after every file read
/// (readers and spine steals alike — a cold device is slow for both);
/// `spine` is added after the spine consumes each file's contents.
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TestDelays {
    pub read: Option<std::time::Duration>,
    pub spine: Option<std::time::Duration>,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            reader_threads: 8,
            chunk_workers: 2,
            byte_budget: 32 * 1024 * 1024,
            #[cfg(test)]
            test_delays: TestDelays::default(),
        }
    }
}

/// Per-run pipeline counters: who performed the file reads. The spine
/// reading more than a small fraction is the signature of the steal-lock
/// degraded mode (every steal is a QD1 serial read on the eval critical
/// path), so tests and benches assert on the split structurally instead
/// of on wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestRunStats {
    /// Files read by the parallel reader pool.
    pub reader_file_reads: u64,
    /// Files read inline by the spine via the steal escape hatch.
    pub spine_file_reads: u64,
}

/// One ingested source tree: the nix-facing identity (NAR sha256/size)
/// plus the CAS-facing content tree. Plain data — the per-directory blob
/// layer composes these into castore blobs at the rewire step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestResult {
    /// SHA-256 of the canonical NAR serialization of the tree.
    pub nar_sha256: [u8; 32],
    /// Total NAR byte count.
    pub nar_size: u64,
    /// The ingested tree with per-file digests and chunk lists.
    pub root: IngestNode,
}

/// One node of the ingested tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestNode {
    /// A regular file.
    File(IngestFile),
    /// A symlink (target read via readlink, never followed).
    Symlink(IngestSymlink),
    /// A directory with byte-lexicographically sorted entries.
    Dir(IngestDir),
}

/// Plane-2 output for one regular file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestFile {
    /// blake3 of the full file contents.
    pub digest: [u8; 32],
    /// Content length in bytes.
    pub size: u64,
    /// Any execute bit set on the file mode.
    pub executable: bool,
    /// FastCDC chunk run covering the contents exactly, in order.
    /// Empty for a zero-byte file.
    pub chunks: Vec<IngestChunk>,
}

/// One content-defined chunk of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestChunk {
    /// blake3 of the chunk bytes (the CAS storage key).
    pub digest: [u8; 32],
    /// Byte offset of the chunk within its file.
    pub offset: u64,
    /// Chunk length; bounded by `FASTCDC_MAX_BYTES`, so u32 suffices.
    pub len: u32,
}

/// A symlink node. The target is raw bytes — NAR (and nix) permit
/// non-UTF-8 targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestSymlink {
    /// readlink(2) result, uninterpreted.
    pub target: Vec<u8>,
}

/// A directory node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestDir {
    /// Entries sorted byte-lexicographically by name (NAR order).
    pub entries: Vec<IngestEntry>,
}

/// One directory entry. Names are raw bytes — NAR entry names are
/// length-prefixed byte strings, and source trees may carry non-UTF-8
/// names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestEntry {
    /// Entry basename (no separators).
    pub name: Vec<u8>,
    /// The node behind the entry.
    pub node: IngestNode,
}

/// Ingest failures. Every variant names the path that failed; the first
/// failure aborts the whole ingest (all threads join, partial results are
/// discarded).
#[derive(Debug, Error)]
pub enum IngestError {
    /// `lstat` of a tree entry failed.
    #[error("failed to stat {path:?}")]
    Stat {
        /// The path that failed.
        path: PathBuf,
        /// Underlying errno.
        #[source]
        source: io::Error,
    },
    /// `readdir` of a directory failed.
    #[error("failed to read directory {path:?}")]
    ReadDir {
        /// The directory that failed.
        path: PathBuf,
        /// Underlying errno.
        #[source]
        source: io::Error,
    },
    /// `open`/`read` of a regular file failed.
    #[error("failed to read file {path:?}")]
    ReadFile {
        /// The file that failed.
        path: PathBuf,
        /// Underlying errno.
        #[source]
        source: io::Error,
    },
    /// `readlink` of a symlink failed.
    #[error("failed to read symlink target of {path:?}")]
    ReadLink {
        /// The symlink that failed.
        path: PathBuf,
        /// Underlying errno.
        #[source]
        source: io::Error,
    },
    /// Entry is not a regular file, directory, or symlink (FIFO, socket,
    /// device node). NAR cannot represent it; matches nix `dumpPath`.
    #[error("{path:?} has an unsupported file type (not regular/directory/symlink)")]
    UnsupportedFileType {
        /// The offending path.
        path: PathBuf,
    },
    /// A file's content length changed between the discovery `lstat` and
    /// the read. The NAR length prefix is written from the stat size, so
    /// a mutating source tree would corrupt the archive — fail loud
    /// instead (same stance as rio-nix's dump truncation check).
    #[error("file {path:?} changed size during ingest: stat said {expected}, read {actual} bytes")]
    SizeChanged {
        /// The mutating file.
        path: PathBuf,
        /// Length from the discovery lstat.
        expected: u64,
        /// Bytes actually read.
        actual: u64,
    },
    /// A directory has at least [`rio_nix::nar::MAX_DIRECTORY_ENTRIES`]
    /// entries — the same producer-side bound as rio-nix's dump (a NAR
    /// every consumer rejects must not be emitted).
    #[error("{path:?} has {count} entries, exceeding the maximum of {max}",
            max = rio_nix::nar::MAX_DIRECTORY_ENTRIES)]
    TooManyEntries {
        /// The over-full directory.
        path: PathBuf,
        /// Its entry count.
        count: usize,
    },
    /// Directory nesting exceeds [`rio_nix::nar::MAX_NAR_DEPTH`] — the
    /// same producer-side bound as rio-nix's dump (a NAR every consumer
    /// rejects must not be emitted), and what keeps the spine's recursion
    /// stack bounded.
    #[error("{path:?}: directory nesting depth {depth} exceeds {max}",
            max = rio_nix::nar::MAX_NAR_DEPTH)]
    TooDeep {
        /// The too-deep directory.
        path: PathBuf,
        /// Its depth below the ingest root.
        depth: usize,
    },
}

/// Ingest the source tree rooted at `root`.
///
/// Spawns the reader/chunk-worker pool lazily (this call is the first and
/// only place threads exist), runs the NAR-sha256 spine on the calling
/// thread, and joins everything — success or error — before returning.
pub fn ingest_tree(root: &Path, config: &IngestConfig) -> Result<IngestResult, IngestError> {
    ingest_tree_with_stats(root, config).map(|(result, _)| result)
}

/// [`ingest_tree`] plus the per-run reader/spine read split — the
/// structural surface the steal-regime regression test and the P1 bench
/// harness assert on.
pub fn ingest_tree_with_stats(
    root: &Path,
    config: &IngestConfig,
) -> Result<(IngestResult, IngestRunStats), IngestError> {
    pipeline::run(root, config)
}

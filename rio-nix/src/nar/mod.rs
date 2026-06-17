//! NAR (Nix ARchive) format reader and writer.
//!
//! NAR is a deterministic archive format for Nix store paths. Structure:
//!
//! ```text
//! str("nix-archive-1")        # magic
//! ( type regular              # regular file
//!   [executable ""]           # optional: present iff executable
//!   contents <bytes> )
//! ( type directory            # directory
//!   entry ( name <str> node <node> ) ...
//! )
//! ( type symlink              # symlink
//!   target <str> )
//! ```
//!
//! All strings use the same `u64(len) + data + pad-to-8` encoding as the
//! Nix wire protocol.
//!
//! Module layout:
//! - `sync_wire` — synchronous `std::io` sibling of [`crate::protocol::wire`]
//! - `reader` — [`parse`] (NAR bytes → [`NarNode`] tree)
//! - `writer` — [`serialize`] ([`NarNode`] tree → NAR bytes)
//! - `fs` — streaming filesystem ↔ NAR ([`dump_path_streaming`],
//!   [`restore_path_streaming`])
//! - `frame` — bare framing emitters for external walkers (ADR-022 §6)

use std::io;

use thiserror::Error;

pub mod frame;
mod fs;
mod ls;
mod reader;
mod sync_wire;
mod writer;

#[cfg(test)]
mod tests;

pub use fs::{WalkObserver, dump_path_observed, dump_path_streaming, restore_path_streaming};
#[cfg(any(test, feature = "test-oracle"))]
#[doc(hidden)]
pub use fs::{dump_path, extract_to_path};
pub use ls::{NarEntryKind, NarLsEntry, nar_ls};
pub use reader::{extract_single_file, parse};
pub use writer::serialize;

/// NAR magic header string.
pub(super) const NAR_MAGIC: &str = "nix-archive-1";

/// Maximum allowed file content size for in-memory parsing. The parser
/// eagerly allocates `vec![0u8; len]` before reading, so a tiny malicious
/// input claiming a multi-GiB length would OOM the process before
/// `read_exact` has a chance to fail. Large NARs should be streamed,
/// not parsed into memory — this limit is intentionally conservative.
pub(super) const MAX_CONTENT_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum allowed NAR entry name length.
///
/// Public because the castore validation boundary (rio-store's
/// Directory-tree walk, rio-proto's `validate_directory`) MUST enforce
/// the same bound: a tree the NAR reader would reject must never be
/// committable, or the store serves a path whose regenerated NAR its
/// own reader (and Nix's) refuses to import. Same rationale for every
/// other `pub` limit below.
pub const MAX_NAME_LEN: u64 = 256;

/// Maximum allowed symlink target length.
pub const MAX_TARGET_LEN: u64 = 4096;

/// Maximum NAR directory nesting depth. Nix's PATH_MAX is 4096; with the
/// 1-char minimum entry name plus separator that caps legitimate nesting
/// at ~2048, but no real store path approaches 256 components.
/// Each level costs ~300 bytes of stack; 256 * 300 = 77 KiB, well within
/// the 2 MiB thread stack.
pub const MAX_NAR_DEPTH: usize = 256;

/// Maximum number of entries in a single directory (DoS prevention for
/// unbounded allocation).
pub const MAX_DIRECTORY_ENTRIES: usize = 1_048_576;

/// Maximum total number of entries (files + directories + symlinks)
/// across one whole archive.
///
/// [`MAX_DIRECTORY_ENTRIES`] bounds one directory; nothing else bounds
/// the tree as a whole, and the store sizes its index materialization
/// and its GetPath framing-regeneration walk for this many nodes. The
/// value matches the per-directory cap: the largest known real store
/// paths (large source FODs) run 300–500k entries, so 1M total leaves
/// ~2–3× headroom while keeping the worst-case `nar_index` entry list
/// and the regeneration walk bounded.
pub const MAX_NAR_ENTRIES: usize = 1_048_576;

/// Maximum cumulative bytes of materialized per-entry index data
/// (`/`-joined entry paths plus symlink targets) for one archive.
///
/// The per-axis limits compose badly: at [`MAX_NAR_DEPTH`] × 256-byte
/// names a single leaf's joined path is ~64 KiB while its NAR framing
/// is ~200 bytes, so a few-hundred-MB NAR can legally expand to tens of
/// GB of index entries (~350× amplification) in every ingest path. This
/// cap bounds the expansion before it is allocated. 128 MiB keeps the
/// encoded `nar_index` (paths + targets + a bounded per-entry overhead
/// of fixed fields) under the 256 MiB default gRPC message ceiling it
/// must later be served through — rio-store compile-asserts that fit
/// next to its `nar_index` encoder — and gives ~2–3× headroom over the
/// largest known real store paths (300–500k entries × ~100 B of path).
/// `RIO_GRPC_MAX_MESSAGE_SIZE` can lower the serving ceiling below the
/// default at runtime; staying under such a reduced ceiling is the
/// operator's concern, not this constant's.
pub const MAX_NAR_INDEX_BYTES: u64 = 128 * 1024 * 1024;

/// Errors from NAR operations.
#[derive(Debug, Error)]
pub enum NarError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid NAR magic: expected {NAR_MAGIC:?}, got {0:?}")]
    InvalidMagic(String),

    #[error("expected token {expected:?}, got {got:?}")]
    UnexpectedToken { expected: String, got: String },

    #[error("unknown node type: {0:?}")]
    UnknownNodeType(String),

    #[error("content size {0} exceeds maximum {MAX_CONTENT_SIZE}")]
    ContentTooLarge(u64),

    #[error("directory entry count {0} exceeds maximum {MAX_DIRECTORY_ENTRIES}")]
    TooManyEntries(usize),

    #[error("total archive entry count {0} exceeds maximum {MAX_NAR_ENTRIES}")]
    TooManyTotalEntries(usize),

    #[error(
        "cumulative entry path/target bytes {0} exceed maximum {MAX_NAR_INDEX_BYTES} \
         (archive expands to an index larger than the store can persist or serve)"
    )]
    IndexBytesTooLarge(u64),

    #[error("non-zero padding byte after {0}-byte field")]
    NonZeroPadding(usize),

    #[error("name length {0} exceeds maximum {MAX_NAME_LEN}")]
    NameTooLong(u64),

    #[error("target length {0} exceeds maximum {MAX_TARGET_LEN}")]
    TargetTooLong(u64),

    #[error("directory entries not in sorted order: {prev:?} >= {cur:?}")]
    UnsortedEntries { prev: String, cur: String },

    #[error("invalid NAR entry name: {name:?}")]
    InvalidEntryName { name: String },

    #[error("directory nesting depth {0} exceeds maximum {MAX_NAR_DEPTH}")]
    NestingTooDeep(usize),

    #[error("file {0:?} has an unsupported type (not regular/symlink/directory)")]
    UnsupportedFileType(std::path::PathBuf),

    #[error("not a single-file NAR")]
    NotSingleFile,

    #[error("invalid UTF-8 in {context} at byte offset {offset}")]
    InvalidUtf8 {
        context: &'static str,
        offset: usize,
        #[source]
        source: std::string::FromUtf8Error,
    },
}

pub type Result<T> = std::result::Result<T, NarError>;

/// A parsed NAR node (recursive tree structure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarNode {
    /// A regular file.
    Regular {
        /// Whether the file is executable.
        executable: bool,
        /// File contents.
        contents: Vec<u8>,
    },
    /// A directory.
    Directory {
        /// Entries sorted by name.
        entries: Vec<NarEntry>,
    },
    /// A symbolic link.
    Symlink {
        /// Link target path.
        target: String,
    },
}

/// A directory entry in a NAR archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarEntry {
    /// Entry name (filename, no path separators).
    pub name: String,
    /// The node this entry refers to.
    pub node: NarNode,
}

// r[impl builder.nar.entry-name-safety]
/// Path-traversal guard for NAR directory entry names: reject names that
/// `Path::join` would interpret as upward (`..`) or absolute (`/...`),
/// plus NUL (filesystem-invalid) and `.`/empty (self-reference). Matches
/// Nix C++ `archive.cc parseDump`. Shared by [`parse`] and
/// [`restore_path_streaming`] — both write the name into a host filesystem
/// path, so both need the same safety boundary.
pub(super) fn validate_entry_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(NarError::InvalidEntryName {
            name: name.to_string(),
        });
    }
    Ok(())
}

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

use std::io;

use thiserror::Error;

mod fs;
mod reader;
mod sync_wire;
mod writer;

#[cfg(test)]
mod tests;

#[cfg(any(test, feature = "test-oracle"))]
#[doc(hidden)]
pub use fs::{dump_path, extract_to_path};
pub use fs::{dump_path_streaming, restore_path_streaming};
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
pub(super) const MAX_NAME_LEN: u64 = 256;

/// Maximum allowed symlink target length.
pub(super) const MAX_TARGET_LEN: u64 = 4096;

/// Maximum NAR directory nesting depth. Nix's PATH_MAX is 4096; with the
/// 1-char minimum entry name plus separator that caps legitimate nesting
/// at ~2048, but no real store path approaches 256 components.
/// Each level costs ~300 bytes of stack; 256 * 300 = 77 KiB, well within
/// the 2 MiB thread stack.
pub(super) const MAX_NAR_DEPTH: usize = 256;

/// Maximum number of directory entries (DoS prevention for unbounded allocation).
pub(super) const MAX_DIRECTORY_ENTRIES: usize = 1_048_576;

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

//! NAR framing emitters for external walkers.
//!
//! ADR-022 §6 splits NAR (re)construction across two crates that walk
//! *different* sources but must emit byte-identical framing:
//!
//! - **rio-builder's fused output walk** walks the overlay upper
//!   *filesystem*, emitting framing + file contents into a SHA-256 +
//!   refscan tee while simultaneously FastCDC-chunking each file.
//! - **rio-store's verify task and `GetPath` v2** walk a castore
//!   *Directory tree*, emitting the same framing with file contents
//!   spliced in from CAS chunks.
//!
//! Neither can use [`dump_path_streaming`](super::dump_path_streaming)
//! directly (it owns the source-of-bytes decision and exposes no entry
//! boundaries), and a hand-rolled copy of the token sequence in either
//! crate would eventually drift from the canonical encoder. This module
//! is the single definition of the per-node token sequences; callers
//! own the tree traversal and the content bytes.
//!
//! The token grammar (Nix `dumpPath`, `libutil/archive.cc`):
//!
//! ```text
//! nar      = "nix-archive-1" node
//! node     = "(" "type" body ")"
//! body     = "regular" ["executable" ""] "contents" <len><bytes><pad>
//!          | "symlink" "target" <target>
//!          | "directory" entry*
//! entry    = "entry" "(" "name" <name> "node" node ")"
//! ```
//!
//! Every `<…>` is a length-prefixed, zero-padded-to-8 byte string.
//! Callers MUST emit the tokens in grammar order; these helpers do not
//! validate sequencing. The byte-equivalence test against
//! [`dump_path`](super::dump_path) in `nar/tests.rs` is the drift
//! tripwire.

use std::io::{self, Write};

use crate::protocol::wire::{ZERO_PAD, padding_len};

use super::NAR_MAGIC;
use super::sync_wire::{write_bytes, write_str, write_u64};

/// `"nix-archive-1"` — the stream prefix, exactly once per NAR.
pub fn magic(w: &mut impl Write) -> io::Result<()> {
    write_str(w, NAR_MAGIC)
}

/// `"(" "type"` — opens a node. Followed by exactly one of
/// [`regular_header`], [`symlink`], or [`directory_open`], then the
/// node's body, then [`node_close`].
pub fn node_open(w: &mut impl Write) -> io::Result<()> {
    write_str(w, "(")?;
    write_str(w, "type")
}

/// `")"` — closes a node opened by [`node_open`].
pub fn node_close(w: &mut impl Write) -> io::Result<()> {
    write_str(w, ")")
}

/// `"symlink" "target" <target>` — the whole body of a symlink node.
pub fn symlink(w: &mut impl Write, target: &[u8]) -> io::Result<()> {
    write_str(w, "symlink")?;
    write_str(w, "target")?;
    write_bytes(w, target)
}

/// `"directory"` — opens a directory body. Followed by zero or more
/// [`entry_open`]…[`entry_close`] pairs in byte-lexicographic name
/// order, then [`node_close`].
pub fn directory_open(w: &mut impl Write) -> io::Result<()> {
    write_str(w, "directory")
}

/// `"entry" "(" "name" <name> "node"` — opens one directory entry.
/// Followed by the child's [`node_open`]…[`node_close`], then
/// [`entry_close`].
pub fn entry_open(w: &mut impl Write, name: &[u8]) -> io::Result<()> {
    write_str(w, "entry")?;
    write_str(w, "(")?;
    write_str(w, "name")?;
    write_bytes(w, name)?;
    write_str(w, "node")
}

/// `")"` — closes an entry opened by [`entry_open`].
pub fn entry_close(w: &mut impl Write) -> io::Result<()> {
    write_str(w, ")")
}

/// `"regular" ["executable" ""] "contents" <len>` — the header of a
/// regular-file body, up to and including the content length prefix.
/// The caller then writes exactly `len` content bytes followed by
/// [`contents_padding`]`(len)`, then [`node_close`].
pub fn regular_header(w: &mut impl Write, executable: bool, len: u64) -> io::Result<()> {
    write_str(w, "regular")?;
    if executable {
        write_str(w, "executable")?;
        write_str(w, "")?;
    }
    write_str(w, "contents")?;
    write_u64(w, len)
}

/// Zero-padding to the next 8-byte boundary after `len` content bytes.
/// Emit after the last content byte of a regular file, before
/// [`node_close`].
pub fn contents_padding(w: &mut impl Write, len: u64) -> io::Result<()> {
    let pad = padding_len(len as usize);
    if pad > 0 {
        w.write_all(&ZERO_PAD[..pad])?;
    }
    Ok(())
}

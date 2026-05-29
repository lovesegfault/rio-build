//! Streaming NAR index — `nar_ls`.
//!
//! Single forward pass over a NAR byte stream emitting one
//! [`NarLsEntry`] per node, with no `Seek` and bounded memory
//! regardless of total NAR or per-file size. Regular-file bytes are
//! streamed through a per-file [`blake3::Hasher`] in 64 KiB blocks
//! (touched once, never buffered whole), so a 1.88 GiB `libxul.so`
//! costs 64 KiB of working set.
//!
//! Sibling of [`super::reader::parse`], which builds the in-memory
//! [`NarNode`](super::NarNode) tree (and therefore buffers every
//! regular file). Callers that need both the tree and the index call
//! `parse()` once and walk the tree; `nar_ls` is for the
//! index-without-tree path (`r[store.index.putpath-eager]`).

use std::io::{self, Read};

use super::sync_wire::{consume_padding, read_name_bytes, read_target_bytes, read_u64};
use super::{
    MAX_DIRECTORY_ENTRIES, MAX_NAR_DEPTH, MAX_NAR_ENTRIES, MAX_NAR_INDEX_BYTES, NAR_MAGIC,
    NarError, Result,
};

/// Lossless `String` for error display of raw NAR bytes — non-ASCII
/// becomes `\xNN`. The codebase-wide `from_utf8_lossy` ban exists
/// because U+FFFD silently drops bytes; this keeps them visible.
fn escape(bytes: &[u8]) -> String {
    bytes.escape_ascii().to_string()
}

/// Streaming hash block size. 64 KiB matches `blake3`'s SIMD-friendly
/// chunk multiple and keeps the working set independent of file size.
const STREAM_BLOCK: usize = 64 * 1024;

/// Node kind for [`NarLsEntry`].
///
/// Mirrors `rio.types.NarEntryKind` but lives in `rio-nix` (which
/// `rio-proto` depends on, not vice versa). Conversion happens at the
/// `rio-store` layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarEntryKind {
    Regular,
    Directory,
    Symlink,
}

/// One file/dir/symlink within a NAR.
///
/// `path` and `target` are raw NAR bytes, not `String`: NAR component
/// names are arbitrary non-NUL/non-`/` byte sequences and the canonical
/// sort order is byte-lexicographic (Nix `parseDump` compares
/// `std::string`, i.e. `memcmp`). Callers that need a `&str` decide for
/// themselves whether non-UTF8 is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarLsEntry {
    /// `/`-joined component names from the NAR root. Root is `""`.
    pub path: Vec<u8>,
    pub kind: NarEntryKind,
    /// Regular-file content length. 0 for directories and symlinks.
    pub size: u64,
    /// Regular files only.
    pub executable: bool,
    /// Byte offset of regular-file content within the canonical NAR
    /// encoding, after the `"contents"` length-prefix u64. 0 for
    /// directories and symlinks.
    ///
    /// r[impl store.index.nar-ls-offset]
    pub nar_offset: u64,
    /// Symlinks only.
    pub target: Vec<u8>,
    /// `blake3(content)` for regular files. Zeroes for dirs/symlinks;
    /// `nar_index::to_proto_entry` maps non-Regular kinds to an empty
    /// `bytes` field on the wire.
    ///
    /// TODO: consider `Option<[u8; 32]>` so the absent case is
    /// unrepresentable rather than a zero sentinel.
    ///
    /// r[impl store.index.file-digest]
    pub file_digest: [u8; 32],
}

/// Tracks bytes consumed so [`NarLsEntry::nar_offset`] can be recorded
/// without `Seek`.
///
/// r[impl store.index.nar-ls-streaming]
struct CountingReader<R> {
    inner: R,
    pos: u64,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// Accumulator for [`nar_ls`] output plus the whole-archive bounds of
/// the ingest-limits contract (`r[store.ingest.tree-bounds]`).
///
/// The per-axis limits (depth, per-directory entries, name length) do
/// not bound the archive as a whole, and the joined `path` per entry
/// makes the materialized index O(entries × depth × name length) — up
/// to ~350× larger than the NAR that encodes it for adversarial shapes.
/// Rejecting at the push, before the cumulative allocation grows, is
/// what keeps every ingest path's memory and the persisted/served
/// `nar_index` size bounded.
struct LsOut {
    entries: Vec<NarLsEntry>,
    /// Cumulative `path.len() + target.len()` over all pushed entries —
    /// the variable-size part of the materialized index.
    index_bytes: u64,
}

impl LsOut {
    // r[impl store.ingest.tree-bounds+2]
    fn push(&mut self, entry: NarLsEntry) -> Result<()> {
        if self.entries.len() >= MAX_NAR_ENTRIES {
            return Err(NarError::TooManyTotalEntries(self.entries.len() + 1));
        }
        self.index_bytes += entry.path.len() as u64 + entry.target.len() as u64;
        if self.index_bytes > MAX_NAR_INDEX_BYTES {
            return Err(NarError::IndexBytesTooLarge(self.index_bytes));
        }
        self.entries.push(entry);
        Ok(())
    }
}

/// Index a NAR stream.
///
/// Entries are emitted in encounter (= DFS pre-order = NAR canonical)
/// order. The root entry has `path == b""`.
pub fn nar_ls<R: Read>(r: R) -> Result<Vec<NarLsEntry>> {
    let mut cr = CountingReader { inner: r, pos: 0 };
    let magic = read_target_bytes(&mut cr)?;
    if magic != NAR_MAGIC.as_bytes() {
        return Err(NarError::InvalidMagic(escape(&magic)));
    }

    let mut out = LsOut {
        entries: Vec::new(),
        index_bytes: 0,
    };
    ls_node(&mut cr, &mut Vec::new(), 0, &mut out)?;
    Ok(out.entries)
}

fn ls_node<R: Read>(
    cr: &mut CountingReader<R>,
    path: &mut Vec<u8>,
    depth: usize,
    out: &mut LsOut,
) -> Result<()> {
    if depth > MAX_NAR_DEPTH {
        return Err(NarError::NestingTooDeep(depth));
    }
    expect_token(cr, b"(")?;
    expect_token(cr, b"type")?;

    let node_type = read_target_bytes(cr)?;
    match node_type.as_slice() {
        b"regular" => ls_regular(cr, path, out),
        b"directory" => ls_directory(cr, path, depth, out),
        b"symlink" => ls_symlink(cr, path, out),
        _ => Err(NarError::UnknownNodeType(escape(&node_type))),
    }
}

fn ls_regular<R: Read>(cr: &mut CountingReader<R>, path: &[u8], out: &mut LsOut) -> Result<()> {
    let token = read_target_bytes(cr)?;
    let executable = match token.as_slice() {
        b"executable" => {
            // Marker value MUST be empty (Nix C++ `parseDump` checks
            // `s != ""`).
            expect_token(cr, b"")?;
            expect_token(cr, b"contents")?;
            true
        }
        b"contents" => false,
        _ => {
            return Err(NarError::UnexpectedToken {
                expected: "\"executable\" or \"contents\"".to_string(),
                got: escape(&token),
            });
        }
    };

    // No upper bound here: nar_ls is called on streaming reassembly
    // where the whole NAR is already chunked; bounding parse() at
    // MAX_CONTENT_SIZE protects RAM for the buffer-whole path, but this
    // path holds 64 KiB regardless of `size`.
    let size = read_u64(cr).map_err(NarError::Io)?;
    let nar_offset = cr.pos;
    let file_digest = stream_blake3(cr, size)?;
    // padding_len() only depends on `len % 8`, so a 0–7 modulus is
    // exact even when `size` doesn't fit in `usize` (32-bit targets).
    consume_padding(cr, (size % 8) as usize)?;
    expect_token(cr, b")")?;

    out.push(NarLsEntry {
        path: path.to_vec(),
        kind: NarEntryKind::Regular,
        size,
        executable,
        nar_offset,
        target: Vec::new(),
        file_digest,
    })
}

fn ls_directory<R: Read>(
    cr: &mut CountingReader<R>,
    path: &mut Vec<u8>,
    depth: usize,
    out: &mut LsOut,
) -> Result<()> {
    // Emit the directory entry BEFORE its children so the result is in
    // NAR encounter (DFS pre-order) order — same as `nix nar ls`.
    out.push(NarLsEntry {
        path: path.clone(),
        kind: NarEntryKind::Directory,
        size: 0,
        executable: false,
        nar_offset: 0,
        target: Vec::new(),
        file_digest: [0; 32],
    })?;

    let mut count = 0usize;
    let mut prev_name: Option<Vec<u8>> = None;

    loop {
        if count >= MAX_DIRECTORY_ENTRIES {
            return Err(NarError::TooManyEntries(count));
        }

        let token = read_target_bytes(cr)?;
        match token.as_slice() {
            b")" => return Ok(()),
            b"entry" => {
                expect_token(cr, b"(")?;
                expect_token(cr, b"name")?;

                let name = read_name_bytes(cr)?;
                validate_entry_name_bytes(&name)?;

                // Byte-lexicographic order, same as Nix `parseDump`.
                if let Some(ref prev) = prev_name
                    && name.as_slice() <= prev.as_slice()
                {
                    return Err(NarError::UnsortedEntries {
                        prev: escape(prev),
                        cur: escape(&name),
                    });
                }

                expect_token(cr, b"node")?;

                let saved_len = path.len();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(&name);
                ls_node(cr, path, depth + 1, out)?;
                path.truncate(saved_len);

                prev_name = Some(name);
                expect_token(cr, b")")?;
                count += 1;
            }
            _ => {
                return Err(NarError::UnexpectedToken {
                    expected: "\"entry\" or \")\"".to_string(),
                    got: escape(&token),
                });
            }
        }
    }
}

fn ls_symlink<R: Read>(cr: &mut CountingReader<R>, path: &[u8], out: &mut LsOut) -> Result<()> {
    expect_token(cr, b"target")?;
    let target = read_target_bytes(cr)?;
    expect_token(cr, b")")?;

    out.push(NarLsEntry {
        path: path.to_vec(),
        kind: NarEntryKind::Symlink,
        size: 0,
        executable: false,
        nar_offset: 0,
        target,
        file_digest: [0; 32],
    })
}

/// Stream `len` content bytes through a fresh `blake3::Hasher` without
/// buffering. The trailing pad bytes are NOT consumed here (the caller
/// validates them via `consume_padding` so a non-zero pad is an error,
/// not a silent skip).
fn stream_blake3<R: Read>(cr: &mut CountingReader<R>, len: u64) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut remaining = len;
    let mut buf = [0u8; STREAM_BLOCK];
    while remaining > 0 {
        let take = remaining.min(STREAM_BLOCK as u64) as usize;
        cr.read_exact(&mut buf[..take]).map_err(NarError::Io)?;
        hasher.update(&buf[..take]);
        remaining -= take as u64;
    }
    Ok(*hasher.finalize().as_bytes())
}

/// `expect_str` for raw-byte tokens. Same wire shape (length-prefixed,
/// `MAX_TARGET_LEN`-bounded), no UTF-8 requirement.
fn expect_token<R: Read>(cr: &mut CountingReader<R>, expected: &[u8]) -> Result<()> {
    let got = read_target_bytes(cr)?;
    if got != expected {
        return Err(NarError::UnexpectedToken {
            expected: escape(expected),
            got: escape(&got),
        });
    }
    Ok(())
}

/// Byte-level [`validate_entry_name`](super::validate_entry_name).
fn validate_entry_name_bytes(name: &[u8]) -> Result<()> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(NarError::InvalidEntryName { name: escape(name) });
    }
    Ok(())
}

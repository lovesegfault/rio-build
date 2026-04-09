//! Filesystem ↔ NAR streaming.
//!
//! Production paths are [`dump_path_streaming`] (fs → NAR sink) and
//! [`restore_path_streaming`] (NAR source → fs). Both work in 256 KiB
//! chunks; peak heap is O(chunk) regardless of NAR size.
//!
//! [`dump_path`] / [`extract_to_path`] are eager in-memory oracles kept
//! under `cfg(test-oracle)` for fuzz/property testing.

use std::io::{self, Read, Write};

use crate::protocol::wire::{ZERO_PAD, padding_len};

use super::sync_wire::{
    consume_padding, expect_str, read_name_bytes, read_string, read_target_bytes, read_u64,
    write_str, write_u64,
};
use super::{
    MAX_DIRECTORY_ENTRIES, MAX_NAR_DEPTH, NAR_MAGIC, NarError, Result, validate_entry_name,
};
#[cfg(any(test, feature = "test-oracle"))]
use super::{NarEntry, NarNode, writer::serialize};

/// Chunk size for streaming file content reads/writes. Matches
/// `NAR_CHUNK_SIZE` in rio-proto (256 KiB) so the worker's channel sink can
/// forward most reads as single gRPC chunks without re-buffering.
const STREAM_CHUNK: usize = 256 * 1024;

/// Eager in-memory NAR dump. Test/fuzz oracle only — production uses
/// [`dump_path_streaming`].
#[cfg(any(test, feature = "test-oracle"))]
#[doc(hidden)]
pub fn dump_path(path: &std::path::Path) -> Result<Vec<u8>> {
    let node = node_from_path(path)?;
    let mut buf = Vec::new();
    serialize(&mut buf, &node)?;
    Ok(buf)
}

/// Serialize a filesystem path directly to a `Write` sink, reading file
/// contents in 256 KiB chunks without buffering the full tree in memory.
///
/// **Byte-identical output to the eager `dump_path` oracle** — only the memory profile
/// differs. Directory structure (names, order, symlinks) is still traversed
/// in-memory (negligible: ~bytes per entry); only file CONTENTS are streamed.
///
/// Returns total bytes written (= the NAR's on-wire size, what `nar_size`
/// should be set to).
///
/// # Read-during-write detection
///
/// If a file shrinks between the `symlink_metadata()` call that reads its
/// length and the `read()` loop that copies its contents, the NAR would be
/// corrupted: the wire format is `u64:len | len bytes | padding`, so a
/// short read leaves garbage in the byte positions the reader expects to
/// be content. We detect this (read returns 0 before `remaining` hits 0)
/// and fail with a clear error. The overlay upper dir is frozen post-build,
/// so in practice this only catches filesystem bugs or a misconfigured
/// worker that mounts a still-mutating path.
pub fn dump_path_streaming(path: &std::path::Path, w: &mut impl Write) -> Result<u64> {
    let mut counter = CountingWriter::new(w);
    write_str(&mut counter, NAR_MAGIC)?;
    stream_node(&mut counter, path)?;
    Ok(counter.written)
}

/// Wraps an `impl Write` and counts bytes written. Used to return the NAR's
/// total byte size without a second pass.
struct CountingWriter<W> {
    inner: W,
    written: u64,
}

impl<W: Write> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Streaming analogue of `serialize_node(node_from_path(path))`. Walks
/// the filesystem and writes NAR framing directly, reading file contents
/// in 256 KiB chunks.
fn stream_node(w: &mut impl Write, path: &std::path::Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    write_str(w, "(")?;
    write_str(w, "type")?;

    if metadata.is_symlink() {
        let target = std::fs::read_link(path)?;
        let target = target.into_os_string().into_string().map_err(|os_str| {
            NarError::Io(io::Error::other(format!(
                "symlink target is not valid UTF-8: {os_str:?}"
            )))
        })?;
        write_str(w, "symlink")?;
        write_str(w, "target")?;
        write_str(w, &target)?;
    } else if metadata.is_dir() {
        write_str(w, "directory")?;
        // Collect + sort entries for deterministic output (same as
        // node_from_path — NAR requires sorted entries).
        let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().into_string().map_err(|os_str| {
                NarError::Io(io::Error::other(format!(
                    "directory entry name is not valid UTF-8: {os_str:?}"
                )))
            })?;
            write_str(w, "entry")?;
            write_str(w, "(")?;
            write_str(w, "name")?;
            write_str(w, &name)?;
            write_str(w, "node")?;
            stream_node(w, &entry.path())?;
            write_str(w, ")")?;
        }
    } else {
        use std::os::unix::fs::PermissionsExt;
        let executable = metadata.permissions().mode() & 0o111 != 0;
        let len = metadata.len();

        write_str(w, "regular")?;
        if executable {
            write_str(w, "executable")?;
            write_str(w, "")?;
        }
        write_str(w, "contents")?;

        // THE POINT: length prefix first, then stream contents in chunks.
        // `write_bytes` would need the whole thing in a slice; we unfold it.
        write_u64(w, len)?;
        let mut f = std::fs::File::open(path)?;
        let mut buf = vec![0u8; STREAM_CHUNK];
        let mut remaining = len;
        while remaining > 0 {
            let to_read = (STREAM_CHUNK as u64).min(remaining) as usize;
            let n = f.read(&mut buf[..to_read])?;
            if n == 0 {
                // Short read — file shrank between symlink_metadata and now.
                // See function docs. The NAR is already corrupt at this
                // point (we wrote `len` as the length prefix, but can't
                // provide that many bytes). Fail loud.
                return Err(NarError::Io(io::Error::other(format!(
                    "file {path:?} truncated during dump: expected {len} bytes, \
                     short read at {} ({} remaining). Is the overlay upper \
                     being mutated?",
                    len - remaining,
                    remaining
                ))));
            }
            w.write_all(&buf[..n])?;
            remaining -= n as u64;
        }
        // NAR padding to 8-byte boundary (same as write_bytes).
        let pad = padding_len(len as usize);
        if pad > 0 {
            w.write_all(&ZERO_PAD[..pad])?;
        }
    }

    write_str(w, ")")?;
    Ok(())
}

/// Extract a NAR archive directly to a filesystem path, reading file
/// contents in 256 KiB chunks without buffering the full tree in memory.
///
/// Mirror of [`dump_path_streaming`]: walks the NAR token stream and
/// writes each entry to disk as it's encountered. Regular-file contents
/// are `io::copy`'d in fixed-size pieces — peak heap is O(chunk size),
/// not O(NAR size). **Semantically equivalent to
/// `extract_to_path(&parse(r)?, dest)`** without the intermediate
/// [`NarNode`] tree.
///
/// Unlike [`parse`](super::parse), there is no per-file `MAX_CONTENT_SIZE`
/// cap — the caller bounds total size upstream (e.g., `MAX_NAR_SIZE` on the
/// gRPC stream). This is the path the builder's FUSE fetch uses for
/// GB-scale inputs (I-180: a 1.8 GB LLVM source NAR previously held
/// ~3.6 GB peak — `Vec<u8>` of NAR bytes + the parsed `NarNode` tree).
///
/// `dest` must NOT exist; `restore_node` creates it (file, dir, or
/// symlink) per the root node's type. On error, a partially-written tree
/// may remain at `dest` — the caller is responsible for cleanup.
pub fn restore_path_streaming(r: &mut impl Read, dest: &std::path::Path) -> Result<()> {
    let magic = read_string(r)?;
    if magic != NAR_MAGIC {
        return Err(NarError::InvalidMagic(magic));
    }
    restore_node(r, dest, 0)
}

/// Streaming analogue of `extract_to_path(parse_node(r))`. Reads NAR
/// framing tokens and writes the corresponding filesystem object at
/// `dest`, copying regular-file contents in 256 KiB chunks.
fn restore_node(r: &mut impl Read, dest: &std::path::Path, depth: usize) -> Result<()> {
    if depth > MAX_NAR_DEPTH {
        return Err(NarError::NestingTooDeep(depth));
    }
    expect_str(r, "(")?;
    expect_str(r, "type")?;

    let node_type = read_string(r)?;
    match node_type.as_str() {
        "regular" => {
            // Peek: either "executable" or "contents" (same as parse_regular).
            let token = read_string(r)?;
            let executable = match token.as_str() {
                "executable" => {
                    expect_str(r, "")?;
                    expect_str(r, "contents")?;
                    true
                }
                "contents" => false,
                _ => {
                    return Err(NarError::UnexpectedToken {
                        expected: "\"executable\" or \"contents\"".to_string(),
                        got: token,
                    });
                }
            };

            // THE POINT: read the u64 length prefix, then stream `len`
            // bytes from `r` straight to disk in fixed-size chunks. No
            // `vec![0; len]` — peak alloc is one STREAM_CHUNK buffer.
            // No MAX_CONTENT_SIZE check (caller bounds total upstream).
            let len = read_u64(r)?;
            let mut f = std::fs::File::create(dest)?;
            let mut buf = vec![0u8; STREAM_CHUNK];
            let mut remaining = len;
            while remaining > 0 {
                let to_read = (STREAM_CHUNK as u64).min(remaining) as usize;
                // read() may return short — loop until we've copied
                // exactly `len` bytes or hit EOF (truncated NAR).
                let n = r.read(&mut buf[..to_read])?;
                if n == 0 {
                    return Err(NarError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "NAR truncated mid-file: expected {len} bytes for {dest:?}, \
                             EOF at {} ({} remaining)",
                            len - remaining,
                            remaining
                        ),
                    )));
                }
                f.write_all(&buf[..n])?;
                remaining -= n as u64;
            }
            // Consume NAR padding to 8-byte boundary (validated zero).
            consume_padding(r, len as usize)?;
            drop(f);
            if executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
            }
            expect_str(r, ")")?;
        }
        "directory" => {
            std::fs::create_dir(dest)?;
            let mut prev_name: Option<String> = None;
            let mut count = 0usize;
            loop {
                if count >= MAX_DIRECTORY_ENTRIES {
                    return Err(NarError::TooManyEntries(count));
                }
                let token = read_string(r)?;
                match token.as_str() {
                    ")" => break,
                    "entry" => {
                        count += 1;
                        expect_str(r, "(")?;
                        expect_str(r, "name")?;
                        let name_bytes = read_name_bytes(r)?;
                        let name =
                            String::from_utf8(name_bytes).map_err(|e| NarError::InvalidUtf8 {
                                context: "entry name",
                                offset: e.utf8_error().valid_up_to(),
                                source: e,
                            })?;
                        // Same path-traversal + sort-order guards as
                        // `parse_directory` — restore writes straight
                        // to the host FS, so this is the safety boundary.
                        validate_entry_name(&name)?;
                        if let Some(ref prev) = prev_name
                            && name <= *prev
                        {
                            return Err(NarError::UnsortedEntries {
                                prev: prev.clone(),
                                cur: name,
                            });
                        }
                        expect_str(r, "node")?;
                        restore_node(r, &dest.join(&name), depth + 1)?;
                        expect_str(r, ")")?;
                        prev_name = Some(name);
                    }
                    _ => {
                        return Err(NarError::UnexpectedToken {
                            expected: "\"entry\" or \")\"".to_string(),
                            got: token,
                        });
                    }
                }
            }
        }
        "symlink" => {
            expect_str(r, "target")?;
            let target_bytes = read_target_bytes(r)?;
            let target = String::from_utf8(target_bytes).map_err(|e| NarError::InvalidUtf8 {
                context: "symlink target",
                offset: e.utf8_error().valid_up_to(),
                source: e,
            })?;
            std::os::unix::fs::symlink(&target, dest)?;
            expect_str(r, ")")?;
        }
        _ => return Err(NarError::UnknownNodeType(node_type)),
    }
    Ok(())
}

/// Build a [`NarNode`] tree from a filesystem path.
#[cfg(any(test, feature = "test-oracle"))]
fn node_from_path(path: &std::path::Path) -> Result<NarNode> {
    let metadata = std::fs::symlink_metadata(path)?;

    if metadata.is_symlink() {
        let target = std::fs::read_link(path)?;
        let target = target.into_os_string().into_string().map_err(|os_str| {
            NarError::Io(io::Error::other(format!(
                "symlink target is not valid UTF-8: {os_str:?}"
            )))
        })?;
        Ok(NarNode::Symlink { target })
    } else if metadata.is_dir() {
        let mut entries: Vec<NarEntry> = Vec::new();
        let mut dir_entries: Vec<_> = std::fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        dir_entries.sort_by_key(|e| e.file_name());

        for entry in dir_entries {
            let name = entry.file_name().into_string().map_err(|os_str| {
                NarError::Io(io::Error::other(format!(
                    "directory entry name is not valid UTF-8: {os_str:?}"
                )))
            })?;
            let child_path = entry.path();
            let node = node_from_path(&child_path)?;
            entries.push(NarEntry { name, node });
        }

        Ok(NarNode::Directory { entries })
    } else {
        use std::os::unix::fs::PermissionsExt;
        let executable = metadata.permissions().mode() & 0o111 != 0;
        let contents = std::fs::read(path)?;
        Ok(NarNode::Regular {
            executable,
            contents,
        })
    }
}

/// Eager in-memory NAR extract. Test/fuzz oracle only — production uses
/// [`restore_path_streaming`].
#[cfg(any(test, feature = "test-oracle"))]
#[doc(hidden)]
pub fn extract_to_path(node: &NarNode, path: &std::path::Path) -> Result<()> {
    match node {
        NarNode::Regular {
            executable,
            contents,
        } => {
            std::fs::write(path, contents)?;
            if *executable {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o755);
                std::fs::set_permissions(path, perms)?;
            }
        }
        NarNode::Directory { entries } => {
            std::fs::create_dir_all(path)?;
            for entry in entries {
                let child_path = path.join(&entry.name);
                extract_to_path(&entry.node, &child_path)?;
            }
        }
        NarNode::Symlink { target } => {
            std::os::unix::fs::symlink(target, path)?;
        }
    }
    Ok(())
}

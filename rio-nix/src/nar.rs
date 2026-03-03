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

use std::io::{self, Read, Write};

use thiserror::Error;

/// NAR magic header string.
const NAR_MAGIC: &str = "nix-archive-1";

/// Maximum allowed file content size for in-memory parsing. The parser
/// eagerly allocates `vec![0u8; len]` before reading, so a tiny malicious
/// input claiming a multi-GiB length would OOM the process before
/// `read_exact` has a chance to fail. Large NARs should be streamed,
/// not parsed into memory — this limit is intentionally conservative.
const MAX_CONTENT_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum allowed NAR entry name length.
const MAX_NAME_LEN: u64 = 256;

/// Maximum allowed symlink target length.
const MAX_TARGET_LEN: u64 = 4096;

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

    #[error("name length {0} exceeds maximum {MAX_NAME_LEN}")]
    NameTooLong(u64),

    #[error("target length {0} exceeds maximum {MAX_TARGET_LEN}")]
    TargetTooLong(u64),

    #[error("directory entries not in sorted order: {prev:?} >= {cur:?}")]
    UnsortedEntries { prev: String, cur: String },

    #[error("not a single-file NAR")]
    NotSingleFile,
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
// ---------------------------------------------------------------------------
// Synchronous string encoding (same as wire format but using std::io)
// ---------------------------------------------------------------------------

const PADDING: usize = 8;

fn padding_len(len: usize) -> usize {
    let rem = len % PADDING;
    if rem == 0 { 0 } else { PADDING - rem }
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn write_u64(w: &mut impl Write, val: u64) -> io::Result<()> {
    w.write_all(&val.to_le_bytes())
}

/// Read length-prefixed padded bytes from the wire.
fn read_padded_bytes(r: &mut impl Read, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let pad = padding_len(len);
    if pad > 0 {
        let mut pad_buf = [0u8; 8];
        r.read_exact(&mut pad_buf[..pad])?;
    }
    Ok(buf)
}

fn read_bytes_bounded(r: &mut impl Read, max_len: u64) -> Result<Vec<u8>> {
    let len = read_u64(r)?;
    if len > max_len {
        return Err(NarError::ContentTooLarge(len));
    }
    read_padded_bytes(r, len as usize)
}

fn read_name_bytes(r: &mut impl Read) -> Result<Vec<u8>> {
    let len = read_u64(r)?;
    if len > MAX_NAME_LEN {
        return Err(NarError::NameTooLong(len));
    }
    read_padded_bytes(r, len as usize)
}

fn read_target_bytes(r: &mut impl Read) -> Result<Vec<u8>> {
    let len = read_u64(r)?;
    if len > MAX_TARGET_LEN {
        return Err(NarError::TargetTooLong(len));
    }
    read_padded_bytes(r, len as usize)
}

fn read_string(r: &mut impl Read) -> Result<String> {
    let bytes = read_target_bytes(r)?;
    String::from_utf8(bytes).map_err(|_| NarError::UnexpectedToken {
        expected: "valid UTF-8 string".to_string(),
        got: "<invalid UTF-8>".to_string(),
    })
}

fn write_bytes(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    write_u64(w, data.len() as u64)?;
    w.write_all(data)?;
    let pad = padding_len(data.len());
    if pad > 0 {
        w.write_all(&[0u8; 8][..pad])?;
    }
    Ok(())
}

fn write_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    write_bytes(w, s.as_bytes())
}

fn expect_str(r: &mut impl Read, expected: &str) -> Result<()> {
    let got = read_string(r)?;
    if got != expected {
        return Err(NarError::UnexpectedToken {
            expected: expected.to_string(),
            got,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// NAR reader
// ---------------------------------------------------------------------------

/// Parse a NAR archive from a byte reader.
///
/// Returns the root [`NarNode`] representing the archived path.
pub fn parse(r: &mut impl Read) -> Result<NarNode> {
    // Read magic
    let magic = read_string(r)?;
    if magic != NAR_MAGIC {
        return Err(NarError::InvalidMagic(magic));
    }

    parse_node(r)
}

/// Parse a single NAR node.
///
/// Each sub-parser is responsible for consuming its own closing ")".
/// This is necessary because `parse_directory` reads tokens in a loop
/// and must consume ")" to detect end-of-directory.
fn parse_node(r: &mut impl Read) -> Result<NarNode> {
    expect_str(r, "(")?;
    expect_str(r, "type")?;

    let node_type = read_string(r)?;
    match node_type.as_str() {
        "regular" => parse_regular(r),
        "directory" => parse_directory(r),
        "symlink" => parse_symlink(r),
        _ => Err(NarError::UnknownNodeType(node_type)),
    }
}

fn parse_regular(r: &mut impl Read) -> Result<NarNode> {
    // Peek at next token: either "executable" or "contents"
    let token = read_string(r)?;
    let (executable, contents) = match token.as_str() {
        "executable" => {
            // Read empty string marker
            let _empty = read_string(r)?;
            expect_str(r, "contents")?;
            let contents = read_bytes_bounded(r, MAX_CONTENT_SIZE)?;
            (true, contents)
        }
        "contents" => {
            let contents = read_bytes_bounded(r, MAX_CONTENT_SIZE)?;
            (false, contents)
        }
        ")" => {
            // Empty file with no "contents" field: ( type regular )
            return Ok(NarNode::Regular {
                executable: false,
                contents: vec![],
            });
        }
        _ => {
            return Err(NarError::UnexpectedToken {
                expected: "\"executable\" or \"contents\" or \")\"".to_string(),
                got: token,
            });
        }
    };

    expect_str(r, ")")?;
    Ok(NarNode::Regular {
        executable,
        contents,
    })
}

/// Maximum number of directory entries (DoS prevention for unbounded allocation).
const MAX_DIRECTORY_ENTRIES: usize = 1_048_576;

fn parse_directory(r: &mut impl Read) -> Result<NarNode> {
    let mut entries = Vec::new();
    let mut prev_name: Option<String> = None;

    loop {
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(NarError::ContentTooLarge(entries.len() as u64));
        }

        // Peek: either "entry" or ")"
        let token = read_string(r)?;
        match token.as_str() {
            ")" => {
                // Closing ")" consumed — directory complete.
                return Ok(NarNode::Directory { entries });
            }
            "entry" => {
                expect_str(r, "(")?;
                expect_str(r, "name")?;

                let name_bytes = read_name_bytes(r)?;
                let name =
                    String::from_utf8(name_bytes).map_err(|_| NarError::UnexpectedToken {
                        expected: "valid UTF-8 name".to_string(),
                        got: "<invalid UTF-8>".to_string(),
                    })?;

                // Enforce sorted order
                if let Some(ref prev) = prev_name
                    && name <= *prev
                {
                    return Err(NarError::UnsortedEntries {
                        prev: prev.clone(),
                        cur: name,
                    });
                }
                prev_name = Some(name.clone());

                expect_str(r, "node")?;
                let node = parse_node(r)?;
                expect_str(r, ")")?; // close the entry's parens

                entries.push(NarEntry { name, node });
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

fn parse_symlink(r: &mut impl Read) -> Result<NarNode> {
    expect_str(r, "target")?;
    let target_bytes = read_target_bytes(r)?;
    let target = String::from_utf8(target_bytes).map_err(|_| NarError::UnexpectedToken {
        expected: "valid UTF-8 target".to_string(),
        got: "<invalid UTF-8>".to_string(),
    })?;
    expect_str(r, ")")?;
    Ok(NarNode::Symlink { target })
}

// ---------------------------------------------------------------------------
// NAR writer
// ---------------------------------------------------------------------------

/// Serialize a [`NarNode`] tree to NAR format.
pub fn serialize(w: &mut impl Write, node: &NarNode) -> Result<()> {
    write_str(w, NAR_MAGIC)?;
    serialize_node(w, node)
}

fn serialize_node(w: &mut impl Write, node: &NarNode) -> Result<()> {
    write_str(w, "(")?;
    write_str(w, "type")?;

    match node {
        NarNode::Regular {
            executable,
            contents,
        } => {
            write_str(w, "regular")?;
            if *executable {
                write_str(w, "executable")?;
                write_str(w, "")?;
            }
            write_str(w, "contents")?;
            write_bytes(w, contents)?;
            write_str(w, ")")?;
        }
        NarNode::Directory { entries } => {
            write_str(w, "directory")?;
            for entry in entries {
                write_str(w, "entry")?;
                write_str(w, "(")?;
                write_str(w, "name")?;
                write_str(w, &entry.name)?;
                write_str(w, "node")?;
                serialize_node(w, &entry.node)?;
                write_str(w, ")")?;
            }
            write_str(w, ")")?;
        }
        NarNode::Symlink { target } => {
            write_str(w, "symlink")?;
            write_str(w, "target")?;
            write_str(w, target)?;
            write_str(w, ")")?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Filesystem operations
// ---------------------------------------------------------------------------

/// Serialize a filesystem path to NAR bytes (equivalent to `nix-store --dump`).
///
/// Loads every file's contents into memory while building the [`NarNode`]
/// tree. For a 4 GiB output, that's 4 GiB for the tree + 4 GiB for the
/// serialized Vec = 8 GiB peak. Use [`dump_path_streaming`] when writing
/// directly to a sink (e.g., the worker's upload pipeline) to avoid this.
pub fn dump_path(path: &std::path::Path) -> Result<Vec<u8>> {
    let node = node_from_path(path)?;
    let mut buf = Vec::new();
    serialize(&mut buf, &node)?;
    Ok(buf)
}

/// Serialize a filesystem path directly to a `Write` sink, reading file
/// contents in 256 KiB chunks without buffering the full tree in memory.
///
/// **Byte-identical output to [`dump_path`]** — only the memory profile
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
    /// Chunk size for file content reads. Matches `NAR_CHUNK_SIZE` in
    /// rio-proto (256 KiB) so the worker's channel sink can forward
    /// most reads as single gRPC chunks without re-buffering.
    const STREAM_CHUNK: usize = 256 * 1024;

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
            w.write_all(&[0u8; 8][..pad])?;
        }
    }

    write_str(w, ")")?;
    Ok(())
}

/// Build a [`NarNode`] tree from a filesystem path.
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

/// Extract a NAR archive to a filesystem path (equivalent to `nix-store --restore`).
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

/// Extract the content of a single regular file from a NAR.
///
/// Returns `Err(NarError::NotSingleFile)` if the root NAR node is not a
/// regular file (i.e., it's a directory or symlink).
/// This is the common case for `.drv` files uploaded via `wopAddToStoreNar`.
pub fn extract_single_file(nar_data: &[u8]) -> Result<Vec<u8>> {
    let node = parse(&mut io::Cursor::new(nar_data))?;
    match node {
        NarNode::Regular { contents, .. } => Ok(contents),
        _ => Err(NarError::NotSingleFile),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_regular_file() -> anyhow::Result<()> {
        let node = NarNode::Regular {
            executable: false,
            contents: b"hello world\n".to_vec(),
        };

        let mut buf = Vec::new();
        serialize(&mut buf, &node)?;

        let parsed = parse(&mut Cursor::new(&buf))?;
        assert_eq!(parsed, node);
        Ok(())
    }

    #[test]
    fn roundtrip_executable_file() -> anyhow::Result<()> {
        let node = NarNode::Regular {
            executable: true,
            contents: b"#!/bin/sh\necho hello\n".to_vec(),
        };

        let mut buf = Vec::new();
        serialize(&mut buf, &node)?;

        let parsed = parse(&mut Cursor::new(&buf))?;
        assert_eq!(parsed, node);
        Ok(())
    }

    #[test]
    fn roundtrip_symlink() -> anyhow::Result<()> {
        let node = NarNode::Symlink {
            target: "file.txt".to_string(),
        };

        let mut buf = Vec::new();
        serialize(&mut buf, &node)?;

        let parsed = parse(&mut Cursor::new(&buf))?;
        assert_eq!(parsed, node);
        Ok(())
    }

    #[test]
    fn roundtrip_empty_directory() -> anyhow::Result<()> {
        let node = NarNode::Directory { entries: vec![] };

        let mut buf = Vec::new();
        serialize(&mut buf, &node)?;

        let parsed = parse(&mut Cursor::new(&buf))?;
        assert_eq!(parsed, node);
        Ok(())
    }

    #[test]
    fn roundtrip_directory_with_entries() -> anyhow::Result<()> {
        let node = NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "a_file.txt".to_string(),
                    node: NarNode::Regular {
                        executable: false,
                        contents: b"content a".to_vec(),
                    },
                },
                NarEntry {
                    name: "b_link".to_string(),
                    node: NarNode::Symlink {
                        target: "a_file.txt".to_string(),
                    },
                },
                NarEntry {
                    name: "c_dir".to_string(),
                    node: NarNode::Directory {
                        entries: vec![NarEntry {
                            name: "nested.txt".to_string(),
                            node: NarNode::Regular {
                                executable: false,
                                contents: b"nested content".to_vec(),
                            },
                        }],
                    },
                },
            ],
        };

        let mut buf = Vec::new();
        serialize(&mut buf, &node)?;

        let parsed = parse(&mut Cursor::new(&buf))?;
        assert_eq!(parsed, node);
        Ok(())
    }

    #[test]
    fn roundtrip_empty_file() -> anyhow::Result<()> {
        let node = NarNode::Regular {
            executable: false,
            contents: vec![],
        };

        let mut buf = Vec::new();
        serialize(&mut buf, &node)?;

        let parsed = parse(&mut Cursor::new(&buf))?;
        assert_eq!(parsed, node);
        Ok(())
    }

    #[test]
    fn parse_empty_regular_no_contents() -> anyhow::Result<()> {
        // NAR allows empty regular files without a "contents" field:
        // nix-archive-1 ( type regular )
        let mut buf = Vec::new();
        write_str(&mut buf, "nix-archive-1")?;
        write_str(&mut buf, "(")?;
        write_str(&mut buf, "type")?;
        write_str(&mut buf, "regular")?;
        write_str(&mut buf, ")")?;

        let parsed = parse(&mut Cursor::new(&buf))?;
        assert_eq!(
            parsed,
            NarNode::Regular {
                executable: false,
                contents: vec![],
            }
        );
        Ok(())
    }

    #[test]
    fn extract_single_file_works() -> anyhow::Result<()> {
        let node = NarNode::Regular {
            executable: false,
            contents: b"drv content here".to_vec(),
        };

        let mut buf = Vec::new();
        serialize(&mut buf, &node)?;

        let content = extract_single_file(&buf)?;
        assert_eq!(content, b"drv content here");
        Ok(())
    }

    #[test]
    fn extract_single_file_rejects_directory() -> anyhow::Result<()> {
        let node = NarNode::Directory { entries: vec![] };
        let mut buf = Vec::new();
        serialize(&mut buf, &node)?;

        let result = extract_single_file(&buf);
        assert!(matches!(result, Err(NarError::NotSingleFile)));
        Ok(())
    }

    #[test]
    fn rejects_invalid_magic() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        write_str(&mut buf, "not-nar-magic")?;
        let result = parse(&mut Cursor::new(&buf));
        assert!(matches!(result, Err(NarError::InvalidMagic(_))));
        Ok(())
    }

    #[test]
    fn rejects_unknown_node_type() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        write_str(&mut buf, NAR_MAGIC)?;
        write_str(&mut buf, "(")?;
        write_str(&mut buf, "type")?;
        write_str(&mut buf, "fifo")?;

        let result = parse(&mut Cursor::new(&buf));
        assert!(matches!(result, Err(NarError::UnknownNodeType(ref t)) if t == "fifo"));
        Ok(())
    }

    #[test]
    fn rejects_unsorted_directory_entries() -> anyhow::Result<()> {
        // Construct NAR bytes with directory entries in reverse order ("z" before "a")
        let mut buf = Vec::new();
        write_str(&mut buf, NAR_MAGIC)?;
        write_str(&mut buf, "(")?;
        write_str(&mut buf, "type")?;
        write_str(&mut buf, "directory")?;

        // First entry: "z_file"
        write_str(&mut buf, "entry")?;
        write_str(&mut buf, "(")?;
        write_str(&mut buf, "name")?;
        write_str(&mut buf, "z_file")?;
        write_str(&mut buf, "node")?;
        write_str(&mut buf, "(")?;
        write_str(&mut buf, "type")?;
        write_str(&mut buf, "regular")?;
        write_str(&mut buf, "contents")?;
        write_bytes(&mut buf, b"z content")?;
        write_str(&mut buf, ")")?; // close node
        write_str(&mut buf, ")")?; // close entry

        // Second entry: "a_file" (out of order!)
        write_str(&mut buf, "entry")?;
        write_str(&mut buf, "(")?;
        write_str(&mut buf, "name")?;
        write_str(&mut buf, "a_file")?;
        write_str(&mut buf, "node")?;
        write_str(&mut buf, "(")?;
        write_str(&mut buf, "type")?;
        write_str(&mut buf, "regular")?;
        write_str(&mut buf, "contents")?;
        write_bytes(&mut buf, b"a content")?;
        write_str(&mut buf, ")")?; // close node
        write_str(&mut buf, ")")?; // close entry

        write_str(&mut buf, ")")?; // close directory

        let result = parse(&mut Cursor::new(&buf));
        assert!(
            matches!(result, Err(NarError::UnsortedEntries { ref prev, ref cur })
                     if prev == "z_file" && cur == "a_file"),
            "expected UnsortedEntries error, got: {result:?}"
        );
        Ok(())
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// Strategy that generates arbitrary `NarNode` trees.
        ///
        /// Base cases: regular files and symlinks.
        /// Recursive case: directories with 0..5 entries, each with a unique
        /// sorted name and a recursive child node.
        fn arb_nar_node() -> impl Strategy<Value = NarNode> {
            let leaf = prop_oneof![
                // Regular file: arbitrary executable flag + small content
                (
                    any::<bool>(),
                    proptest::collection::vec(any::<u8>(), 0..256)
                )
                    .prop_map(|(executable, contents)| NarNode::Regular {
                        executable,
                        contents,
                    }),
                // Symlink: short target path
                "[a-z]{1,20}".prop_map(|target| NarNode::Symlink { target }),
            ];

            leaf.prop_recursive(
                4,  // max depth
                64, // max total nodes
                5,  // items per collection
                |inner| {
                    proptest::collection::vec(("[a-z]{1,10}", inner), 0..5).prop_map(
                        |mut entries| {
                            // Sort by name and deduplicate to satisfy the parser invariant.
                            entries.sort_by(|a, b| a.0.cmp(&b.0));
                            entries.dedup_by(|a, b| a.0 == b.0);

                            let entries = entries
                                .into_iter()
                                .map(|(name, node)| NarEntry { name, node })
                                .collect();

                            NarNode::Directory { entries }
                        },
                    )
                },
            )
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]
            #[test]
            fn nar_roundtrip(node in arb_nar_node()) {
                let mut buf = Vec::new();
                serialize(&mut buf, &node)?;

                let parsed = parse(&mut Cursor::new(&buf))?;
                prop_assert_eq!(parsed, node);
            }
        }
    }

    /// Compare our NAR output against `nix-store --dump` for a single file.
    #[test]
    fn golden_single_file() -> anyhow::Result<()> {
        let drv_path = "/nix/store/3543bymzsssf34hrlchksl28apr3gfyc-simple-test.drv";

        // Check if path exists (test may run without this specific path)
        if !std::path::Path::new(drv_path).exists() {
            eprintln!("skipping golden_single_file: {drv_path} not found");
            return Ok(());
        }

        let our_nar = dump_path(std::path::Path::new(drv_path))?;

        let nix_output = std::process::Command::new("nix-store")
            .args(["--dump", drv_path])
            .output();

        let nix_output = match nix_output {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!("skipping golden_single_file: nix-store not available");
                return Ok(());
            }
        };

        assert_eq!(
            our_nar, nix_output.stdout,
            "NAR output differs from nix-store --dump"
        );
        Ok(())
    }

    /// Compare our NAR output against `nix-store --dump` for a directory.
    #[test]
    fn golden_directory() -> anyhow::Result<()> {
        let tmpdir = tempfile::TempDir::new()?;
        let root = tmpdir.path();

        // Create a directory structure
        std::fs::create_dir(root.join("subdir"))?;
        std::fs::write(root.join("a_file.txt"), "hello world\n")?;
        std::fs::write(root.join("subdir/nested.txt"), "nested\n")?;
        std::os::unix::fs::symlink("a_file.txt", root.join("b_link"))?;

        // Make a file executable
        let script_path = root.join("c_script.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho hi\n")?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
        }

        let our_nar = dump_path(root)?;

        let nix_output = std::process::Command::new("nix-store")
            .args(["--dump", &root.to_string_lossy()])
            .output();

        let nix_output = match nix_output {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!("skipping golden_directory: nix-store not available");
                return Ok(());
            }
        };

        if our_nar != nix_output.stdout {
            // Find first difference for debugging
            let min_len = our_nar.len().min(nix_output.stdout.len());
            for i in 0..min_len {
                if our_nar[i] != nix_output.stdout[i] {
                    panic!(
                        "NAR differs at byte {i}: ours={:#04x} nix={:#04x}\n\
                         ours len={} nix len={}",
                        our_nar[i],
                        nix_output.stdout[i],
                        our_nar.len(),
                        nix_output.stdout.len()
                    );
                }
            }
            if our_nar.len() != nix_output.stdout.len() {
                panic!(
                    "NAR length differs: ours={} nix={}",
                    our_nar.len(),
                    nix_output.stdout.len()
                );
            }
        }
        Ok(())
    }

    /// Roundtrip via filesystem: dump → parse → extract → dump again.
    #[test]
    fn filesystem_roundtrip() -> anyhow::Result<()> {
        let src_dir = tempfile::TempDir::new()?;
        let src = src_dir.path();

        std::fs::create_dir(src.join("sub"))?;
        std::fs::write(src.join("file.txt"), "content\n")?;
        std::fs::write(src.join("sub/inner.txt"), "inner\n")?;
        std::os::unix::fs::symlink("file.txt", src.join("link"))?;

        // Dump → NAR bytes
        let nar1 = dump_path(src)?;

        // Parse NAR
        let node = parse(&mut Cursor::new(&nar1))?;

        // Extract to new directory
        let dst_dir = tempfile::TempDir::new()?;
        let dst = dst_dir.path().join("extracted");
        extract_to_path(&node, &dst)?;

        // Dump again
        let nar2 = dump_path(&dst)?;

        assert_eq!(nar1, nar2, "NAR roundtrip not byte-identical");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // dump_path_streaming byte-identity to dump_path
    // -----------------------------------------------------------------------

    /// THE correctness invariant for dump_path_streaming: byte-identical
    /// output to dump_path. If this ever diverges, every uploaded NAR is
    /// corrupt — the store would see a different SHA-256 than a
    /// `nix-store --dump` of the same path, and cache hits would never
    /// materialize correctly.
    #[test]
    fn streaming_byte_identical_to_eager() -> anyhow::Result<()> {
        let src_dir = tempfile::TempDir::new()?;
        let src = src_dir.path();

        // Cover all three NarNode types.
        std::fs::create_dir(src.join("sub"))?;
        std::fs::write(src.join("file.txt"), "hello streaming\n")?;
        std::fs::write(src.join("sub/inner.txt"), b"nested content")?;
        std::os::unix::fs::symlink("file.txt", src.join("link"))?;
        // Empty file — edge case for the chunk loop (0 iterations).
        std::fs::write(src.join("empty"), b"")?;
        // Executable bit.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(src.join("script.sh"), "#!/bin/sh\necho hi\n")?;
            std::fs::set_permissions(
                src.join("script.sh"),
                std::fs::Permissions::from_mode(0o755),
            )?;
        }

        let eager = dump_path(src)?;
        let mut streamed = Vec::new();
        let written = dump_path_streaming(src, &mut streamed)?;

        assert_eq!(
            eager, streamed,
            "dump_path_streaming MUST be byte-identical to dump_path"
        );
        assert_eq!(
            written,
            eager.len() as u64,
            "returned byte count should match actual bytes written"
        );
        Ok(())
    }

    /// Same invariant over a larger file (> STREAM_CHUNK = 256 KiB) to
    /// exercise the multi-iteration chunk loop.
    #[test]
    fn streaming_byte_identical_large_file() -> anyhow::Result<()> {
        let src_dir = tempfile::TempDir::new()?;
        let src = src_dir.path();

        // 600 KiB — forces at least 3 chunk-loop iterations.
        let big: Vec<u8> = (0..600 * 1024).map(|i| (i % 256) as u8).collect();
        std::fs::write(src.join("big.bin"), &big)?;

        let eager = dump_path(src)?;
        let mut streamed = Vec::new();
        let written = dump_path_streaming(src, &mut streamed)?;

        assert_eq!(eager, streamed, "large file byte-identity");
        assert_eq!(written, eager.len() as u64);
        Ok(())
    }

    /// Single regular file (not a directory) — dump of the file itself.
    #[test]
    fn streaming_byte_identical_single_file() -> anyhow::Result<()> {
        let src_dir = tempfile::TempDir::new()?;
        let f = src_dir.path().join("single");
        std::fs::write(&f, b"just one file")?;

        let eager = dump_path(&f)?;
        let mut streamed = Vec::new();
        dump_path_streaming(&f, &mut streamed)?;
        assert_eq!(eager, streamed);
        Ok(())
    }
}

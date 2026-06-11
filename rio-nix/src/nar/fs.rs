//! Filesystem ↔ NAR streaming.
//!
//! Production paths are [`dump_path_streaming`] (fs → NAR sink) and
//! [`restore_path_streaming`] (NAR source → fs). Both work in 256 KiB
//! chunks; peak heap is O(chunk) regardless of NAR size.
//!
//! `dump_path` / `extract_to_path` are eager in-memory oracles kept
//! under `cfg(test-oracle)` for fuzz/property testing.

use std::io::{self, Read, Write};

use super::frame;
use super::sync_wire::{
    consume_padding, expect_str, read_name_bytes, read_string, read_target_bytes, read_u64,
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

/// Canonical Nix store-path mtime: one second past the Epoch. Matches
/// `mtimeStore` in Nix's `libstore/posix-fs-canonicalise.cc`. Not 0 (some
/// tools treat 0 as "no timestamp") and never the wall-clock — store paths
/// are immutable and content-addressed; their metadata is fully determined
/// by the NAR.
const CANON_MTIME_SECS: u64 = 1;

/// The canonical Nix store-path `atime`/`mtime` ([`CANON_MTIME_SECS`])
/// as a [`std::fs::FileTimes`]. Mirrors the timestamp half of Nix's
/// `canonicalisePathMetaData` so a restored store path is byte-for-byte
/// what `nix-store --import` would have produced.
///
/// Applied via [`std::fs::File::set_times`] (`futimens(2)`) through the
/// fd each call site already holds — NEVER by re-opening the path.
/// Re-resolving `dest` after creating it is a TOCTOU window
/// (CVE-2021-31566 class): a concurrent attacker with write access to
/// the destination tree can swap in a symlink between create and
/// chmod/utimens and retarget the metadata write.
///
/// Not applied to symlinks: `std` has no `lutimes`/`utimensat(NOFOLLOW)`
/// wrapper, the `set-source-date-epoch-to-latest.sh` `postUnpackHook`
/// (the consumer this fix exists for) only scans `find -type f`, and the
/// FUSE attribute layer (`stat_to_attr`) hardcodes canonical times for
/// every node type regardless of on-disk state, so a non-canonical
/// on-disk symlink mtime is never observable from inside a build.
///
/// **Ordering:** for directories the times MUST be set *after* all
/// children are written. Creating/renaming a child entry updates the
/// parent's `mtime`; setting a child's *own* `mtime` does not.
/// `restore_node`'s post-recursion call site satisfies this.
fn canonical_times() -> std::fs::FileTimes {
    use std::time::{Duration, SystemTime};
    let canon = SystemTime::UNIX_EPOCH + Duration::from_secs(CANON_MTIME_SECS);
    std::fs::FileTimes::new()
        .set_accessed(canon)
        .set_modified(canon)
}

/// Open the child directory `name` relative to the held `parent` fd,
/// refusing to follow a symlink in the final component (`O_NOFOLLOW` →
/// `ELOOP`/`ENOTDIR`). Used by `restore_node` right after `mkdirat`: the
/// returned fd is the anchor for every operation on the directory's
/// children AND the post-children canonical-mtime write, so nothing
/// inside the directory ever re-resolves a path (see [`canonical_times`]
/// for the TOCTOU rationale).
///
/// `O_RDONLY` suffices — `futimens(2)` only requires the fd's owner to
/// match (or `CAP_FOWNER`), not write permission, and `O_RDONLY` on a
/// directory is valid on Linux.
fn open_subdir_nofollow(
    parent: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::CStr,
) -> io::Result<std::os::fd::OwnedFd> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    openat(
        parent,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)
}

/// Eager in-memory NAR dump. Test/fuzz oracle only — production uses
/// [`dump_path_streaming`].
#[cfg(any(test, feature = "test-oracle"))]
#[doc(hidden)]
pub fn dump_path(path: &std::path::Path) -> Result<Vec<u8>> {
    let node = node_from_path(path, 0)?;
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
    dump_path_observed(path, w, &mut ())
}

/// Tree-structure callbacks for [`dump_path_observed`].
///
/// The observer sees the walk's *structure* (entry names, kinds, file
/// content bytes) while the `Write` sink sees the resulting NAR *byte
/// stream* (framing + contents interleaved). ADR-022 §6.1's fused
/// output walk hangs FastCDC chunking, per-file BLAKE3, and castore
/// `Directory` construction off these hooks so each output byte is
/// read from disk exactly once.
///
/// All methods default to no-ops; `()` implements the trait so
/// [`dump_path_streaming`] stays observer-free. An `Err` from any hook
/// aborts the walk.
///
/// `name` is the entry's basename as raw bytes (empty for the NAR
/// root, which has no name). Calls arrive in canonical NAR walk order:
/// a directory's `enter_dir` precedes its children (byte-lex sorted by
/// name), and `leave_dir` follows the last child.
pub trait WalkObserver {
    /// A directory node begins. Its children follow, then [`Self::leave_dir`].
    fn enter_dir(&mut self, name: &[u8]) -> io::Result<()> {
        let _ = name;
        Ok(())
    }
    /// The most recently entered directory has no more children.
    fn leave_dir(&mut self) -> io::Result<()> {
        Ok(())
    }
    /// A symlink node.
    fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()> {
        let _ = (name, target);
        Ok(())
    }
    /// A regular file node begins. Exactly `size` bytes of content
    /// follow via [`Self::file_data`], then [`Self::file_end`].
    fn file_begin(&mut self, name: &[u8], executable: bool, size: u64) -> io::Result<()> {
        let _ = (name, executable, size);
        Ok(())
    }
    /// One read's worth of the current file's content bytes (≤ 256 KiB).
    fn file_data(&mut self, data: &[u8]) -> io::Result<()> {
        let _ = data;
        Ok(())
    }
    /// The current file's content is complete.
    fn file_end(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// No-op observer: [`dump_path_streaming`] without the structure hooks.
impl WalkObserver for () {}

/// [`dump_path_streaming`] with a [`WalkObserver`] receiving the tree
/// structure and file contents alongside the NAR byte stream.
pub fn dump_path_observed(
    path: &std::path::Path,
    w: &mut impl Write,
    obs: &mut impl WalkObserver,
) -> Result<u64> {
    let mut counter = CountingWriter::new(w);
    frame::magic(&mut counter)?;
    stream_node(&mut counter, path, 0, b"", obs)?;
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
///
/// Enforces the same `MAX_NAR_DEPTH` / `MAX_DIRECTORY_ENTRIES` limits as
/// `restore_node` and rejects non-regular/symlink/directory file types
/// (matching Nix `dump()`'s "unsupported type" error) — the producer must
/// emit only what the consumer accepts, and untrusted derivation output
/// must not be able to hang the worker (e.g. `mkfifo $out/pipe` would
/// otherwise block in `open(O_RDONLY)` forever).
// TODO: the whole-archive caps the ingest boundary enforces
// (`MAX_NAR_ENTRIES`, `MAX_NAR_INDEX_BYTES` — `r[store.ingest.tree-bounds]`)
// are not enforced here, only the per-axis ones above. Adding them means
// threading an entry counter and a joined-path byte budget (plus the
// prefix length) through this recursion; the store rejects an over-cap
// upload at PutPath* anyway, so today the asymmetry only costs a builder
// the wasted upload before the rejection. Add the two counters if failing
// before bytes hit the wire becomes worth the extra plumbing.
fn stream_node(
    w: &mut impl Write,
    path: &std::path::Path,
    depth: usize,
    name: &[u8],
    obs: &mut impl WalkObserver,
) -> Result<()> {
    if depth > MAX_NAR_DEPTH {
        return Err(NarError::NestingTooDeep(depth));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    frame::node_open(w)?;

    if metadata.is_symlink() {
        let target = std::fs::read_link(path)?;
        let target = target.into_os_string().into_string().map_err(|os_str| {
            NarError::Io(io::Error::other(format!(
                "symlink target is not valid UTF-8: {os_str:?}"
            )))
        })?;
        frame::symlink(w, target.as_bytes())?;
        obs.symlink(name, target.as_bytes())?;
    } else if metadata.is_dir() {
        frame::directory_open(w)?;
        obs.enter_dir(name)?;
        // Collect + sort entries for deterministic output (same as
        // node_from_path — NAR requires sorted entries).
        let mut entries: Vec<_> = std::fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(NarError::TooManyEntries(entries.len()));
        }
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().into_string().map_err(|os_str| {
                NarError::Io(io::Error::other(format!(
                    "directory entry name is not valid UTF-8: {os_str:?}"
                )))
            })?;
            frame::entry_open(w, name.as_bytes())?;
            stream_node(w, &entry.path(), depth + 1, name.as_bytes(), obs)?;
            frame::entry_close(w)?;
        }
        obs.leave_dir()?;
    } else if metadata.file_type().is_file() {
        use std::os::unix::fs::PermissionsExt;
        let executable = metadata.permissions().mode() & 0o111 != 0;
        let len = metadata.len();

        // THE POINT: the header ends with the length prefix, then contents
        // stream in chunks. `write_bytes` would need the whole thing in a
        // slice; we unfold it.
        frame::regular_header(w, executable, len)?;
        obs.file_begin(name, executable, len)?;
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
            obs.file_data(&buf[..n])?;
            remaining -= n as u64;
        }
        // NAR padding to 8-byte boundary (same as write_bytes).
        frame::contents_padding(w, len)?;
        obs.file_end()?;
    } else {
        // FIFO/socket/device. A FIFO would hang File::open(O_RDONLY)
        // forever; Nix throws "unsupported type" here. Builders are
        // untrusted — fail fast.
        return Err(NarError::UnsupportedFileType(path.to_path_buf()));
    }

    frame::node_close(w)?;
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
/// `NarNode` tree.
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
///
/// # Traversal is dirfd-relative
///
/// The parent directory of `dest` (caller-controlled: a tempdir or
/// builder-owned spool dir) is resolved exactly ONCE, here. Everything
/// below is `openat`/`mkdirat`/`symlinkat` relative to held directory
/// fds — no operation in the restore walk ever re-resolves a multi-
/// component path. A concurrent attacker with write access to the
/// destination tree who swaps an already-created directory component
/// for a symlink (CVE-2021-31566 class, intermediate-component variant)
/// retargets nothing: later writes go through the fd taken before the
/// swap, which still references the original directory.
pub fn restore_path_streaming(r: &mut impl Read, dest: &std::path::Path) -> Result<()> {
    let magic = read_string(r)?;
    if magic != NAR_MAGIC {
        return Err(NarError::InvalidMagic(magic));
    }
    let name = dest.file_name().ok_or_else(|| {
        NarError::Io(io::Error::other(format!(
            "restore destination {} has no final path component",
            dest.display()
        )))
    })?;
    // All names are passed to the *at syscalls as `&CStr`: nix's
    // `NixPath` impls for `str`/`OsStr` copy into a PATH_MAX stack
    // buffer per call, which at MAX_NAR_DEPTH-deep recursion overflows
    // a test-thread stack; the `CStr` impl passes the pointer through.
    let name = {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::CString::new(name.as_bytes()).map_err(|_| {
            NarError::Io(io::Error::other(format!(
                "restore destination {} contains NUL",
                dest.display()
            )))
        })?
    };
    let parent_path = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    let parent = nix::fcntl::open(
        parent_path,
        nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_DIRECTORY | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|e| NarError::Io(e.into()))?;
    restore_node(r, std::os::fd::AsFd::as_fd(&parent), &name, dest, 0)
}

/// Streaming analogue of `extract_to_path(parse_node(r))`. Reads NAR
/// framing tokens and writes the filesystem object `name` relative to
/// the held `parent` directory fd, copying regular-file contents in
/// 256 KiB chunks.
///
/// `name` is always a single path component: for the root it is
/// `dest.file_name()` (caller-controlled), for directory entries it has
/// passed [`validate_entry_name`] (no `/`, no `..`, no NUL — enforced
/// below before recursion, same guard as the `parse_directory` ingest
/// path). Combined with the *at-relative syscalls this means no write
/// ever resolves more than one (symlink-refused) component.
///
/// `path` is the full destination path, used for error messages only —
/// never passed to a syscall.
///
/// Restored regular files and directories carry the canonical Nix
/// store-path `mtime` (1 second past Epoch) — the timestamp half of
/// Nix's `canonicalisePathMetaData` — so `find -newer`/`make`/
/// `set-source-date-epoch-to-latest.sh` see input store paths exactly
/// as a stock Nix daemon would present them.
// r[impl builder.nar.canonical-mtime]
fn restore_node(
    r: &mut impl Read,
    parent: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::CStr,
    path: &std::path::Path,
    depth: usize,
) -> Result<()> {
    use std::os::fd::AsFd;

    use nix::sys::stat::Mode;

    if depth > MAX_NAR_DEPTH {
        return Err(NarError::NestingTooDeep(depth));
    }
    expect_str(r, "(")?;
    expect_str(r, "type")?;

    let node_type = read_string(r)?;
    match node_type.as_str() {
        // The regular/symlink arms live in their own functions: debug
        // builds don't reuse stack slots across match arms, so keeping
        // them inline put EVERY arm's locals in the recursive frame —
        // at MAX_NAR_DEPTH that overflowed a default test-thread stack.
        // Only the directory arm recurses; only its locals may ride the
        // recursion.
        "regular" => restore_regular(r, parent, name, path)?,
        "directory" => {
            nix::sys::stat::mkdirat(parent, name, Mode::from_bits_truncate(0o777))
                .map_err(|e| NarError::Io(e.into()))?;
            // Take the directory fd NOW, immediately after mkdirat, and
            // anchor every child operation AND the post-children
            // canonical mtime on it. Path-based child creation would
            // re-resolve the whole prefix per child — TOCTOU window
            // (see `canonical_times` and the function docs above).
            // O_NOFOLLOW turns a symlink swapped in since the mkdirat
            // into ELOOP/ENOTDIR instead of a retargeted fd.
            let dir = open_subdir_nofollow(parent, name)?;
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
                        // After this check the name is a single normal
                        // component (no `/`, `..`, NUL), which is what
                        // lets the *at calls below treat it as exactly
                        // one directory entry.
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
                        let cname = std::ffi::CString::new(name.as_bytes())
                            .expect("validate_entry_name rejects NUL");
                        restore_node(r, dir.as_fd(), &cname, &path.join(&name), depth + 1)?;
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
            // AFTER all children: every `restore_node(child)` above
            // created an entry in this directory, refreshing its mtime
            // each time. (A child setting its *own* mtime does NOT
            // refresh the parent's — only namespace operations do.)
            std::fs::File::from(dir).set_times(canonical_times())?;
        }
        "symlink" => restore_symlink(r, parent, name, path)?,
        _ => return Err(NarError::UnknownNodeType(node_type)),
    }
    Ok(())
}

/// `regular` arm of [`restore_node`], from the post-`type` token to the
/// closing `)`. Separate function so its locals (chunk handling, fmt
/// machinery) stay off the directory-recursion stack — see the match in
/// `restore_node`.
fn restore_regular(
    r: &mut impl Read,
    parent: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::CStr,
    path: &std::path::Path,
) -> Result<()> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;

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
    // `O_CREAT|O_EXCL` relative to the held parent fd — never
    // follows a planted symlink at the final component, and the
    // single-component `name` means no intermediate component
    // exists to be swapped either. A pre-existing entry of any
    // kind is an error. This fd is what the content write AND
    // all metadata below are applied through.
    let fd = openat(
        parent,
        name,
        OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_WRONLY | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o666),
    )
    .map_err(|e| NarError::Io(e.into()))?;
    let mut f = std::fs::File::from(fd);
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
                    "NAR truncated mid-file: expected {len} bytes for {path:?}, \
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
    // Metadata through the HELD fd (`fchmod`/`futimens`), never
    // by path — re-opening the path here would be the TOCTOU
    // window described at `canonical_times`.
    if executable {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    // Canonical mtime LAST — after `write_all` (which bumps
    // mtime) and the chmod (which only bumps ctime per POSIX,
    // but ordering it before the mtime set keeps the sequence
    // obviously-correct on a non-POSIX FS).
    f.set_times(canonical_times())?;
    expect_str(r, ")")?;
    Ok(())
}

/// `symlink` arm of [`restore_node`] — see [`restore_regular`] for why
/// it is a separate function.
fn restore_symlink(
    r: &mut impl Read,
    parent: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::CStr,
    path: &std::path::Path,
) -> Result<()> {
    expect_str(r, "target")?;
    let target_bytes = read_target_bytes(r)?;
    let target = String::from_utf8(target_bytes).map_err(|e| NarError::InvalidUtf8 {
        context: "symlink target",
        offset: e.utf8_error().valid_up_to(),
        source: e,
    })?;
    // Target → CString explicitly (same per-frame stack-buffer
    // avoidance as `name`; NUL in the target is filesystem-
    // invalid, matching the old `std::os::unix::fs::symlink`
    // behavior of rejecting it at conversion).
    let ctarget = std::ffi::CString::new(target.as_bytes()).map_err(|_| {
        NarError::Io(io::Error::other(format!(
            "symlink target for {path:?} contains NUL"
        )))
    })?;
    nix::unistd::symlinkat(ctarget.as_c_str(), parent, name).map_err(|e| NarError::Io(e.into()))?;
    // No mtime canonicalization for symlinks: `canonical_times`
    // documents why this is correct (no `std` API, the consumer
    // hooks ignore symlinks, FUSE serve-side covers it).
    expect_str(r, ")")?;
    Ok(())
}

/// Build a [`NarNode`] tree from a filesystem path.
#[cfg(any(test, feature = "test-oracle"))]
fn node_from_path(path: &std::path::Path, depth: usize) -> Result<NarNode> {
    if depth > MAX_NAR_DEPTH {
        return Err(NarError::NestingTooDeep(depth));
    }
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
        if dir_entries.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(NarError::TooManyEntries(dir_entries.len()));
        }
        dir_entries.sort_by_key(|e| e.file_name());

        for entry in dir_entries {
            let name = entry.file_name().into_string().map_err(|os_str| {
                NarError::Io(io::Error::other(format!(
                    "directory entry name is not valid UTF-8: {os_str:?}"
                )))
            })?;
            let child_path = entry.path();
            let node = node_from_path(&child_path, depth + 1)?;
            entries.push(NarEntry { name, node });
        }

        Ok(NarNode::Directory { entries })
    } else if metadata.file_type().is_file() {
        use std::os::unix::fs::PermissionsExt;
        let executable = metadata.permissions().mode() & 0o111 != 0;
        let contents = std::fs::read(path)?;
        Ok(NarNode::Regular {
            executable,
            contents,
        })
    } else {
        Err(NarError::UnsupportedFileType(path.to_path_buf()))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `stream_node` must reject anything that isn't a regular file,
    /// symlink, or directory. The motivating case is a FIFO (a derivation
    /// running `mkfifo $out/pipe`): pre-fix, the `else` branch would call
    /// `File::open(fifo)` which blocks forever in `open(O_RDONLY)`. We use
    /// a Unix-domain socket here (no extra dep needed; pre-fix it errored
    /// in `File::open` with ENXIO instead of returning the typed variant)
    /// to assert the file-type guard is in place.
    #[test]
    fn dump_streaming_rejects_unsupported_file_type() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("sock");
        let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

        let mut sink = Vec::new();
        let r = dump_path_streaming(dir.path(), &mut sink);
        assert!(
            matches!(r, Err(NarError::UnsupportedFileType(_))),
            "got {r:?}"
        );
        // Test-oracle parity.
        let r = dump_path(dir.path());
        assert!(
            matches!(r, Err(NarError::UnsupportedFileType(_))),
            "got {r:?}"
        );
    }

    /// Producer enforces the same `MAX_NAR_DEPTH` limit as the consumer
    /// (`restore_node`); a NAR the consumer is guaranteed to reject must
    /// not be emitted in the first place.
    #[test]
    fn dump_streaming_rejects_deep_nesting() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = dir.path().to_path_buf();
        // MAX_NAR_DEPTH+2 levels under the root → depth check trips.
        for _ in 0..(MAX_NAR_DEPTH + 2) {
            p.push("a");
        }
        std::fs::create_dir_all(&p).unwrap();

        let mut sink = Vec::new();
        let r = dump_path_streaming(dir.path(), &mut sink);
        assert!(matches!(r, Err(NarError::NestingTooDeep(_))), "got {r:?}");
        let r = dump_path(dir.path());
        assert!(matches!(r, Err(NarError::NestingTooDeep(_))), "got {r:?}");
    }

    /// At exactly `MAX_NAR_DEPTH` the producer must succeed and the
    /// consumer must accept the result — the streaming and oracle outputs
    /// must be byte-identical (regression guard that the `is_file()` gate
    /// doesn't reject regular files).
    #[test]
    fn dump_streaming_at_depth_limit_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = dir.path().to_path_buf();
        // root is depth 0; MAX_NAR_DEPTH-1 nested dirs → deepest dir at
        // depth MAX_NAR_DEPTH-1; file inside at depth MAX_NAR_DEPTH (the
        // limit, inclusive).
        for _ in 0..(MAX_NAR_DEPTH - 1) {
            p.push("a");
        }
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("file"), b"hello").unwrap();

        let mut streamed = Vec::new();
        let n = dump_path_streaming(dir.path(), &mut streamed).unwrap();
        assert_eq!(n as usize, streamed.len());
        let eager = dump_path(dir.path()).unwrap();
        assert_eq!(streamed, eager, "streaming and oracle must agree");

        // Consumer accepts.
        let dest = tempfile::tempdir().unwrap();
        let dest_root = dest.path().join("out");
        restore_path_streaming(&mut streamed.as_slice(), &dest_root).unwrap();
    }

    /// The directory-fd open that anchors child writes and the
    /// post-children mtime application must refuse a symlink in the
    /// final component (ELOOP), not follow it — this is what closes the
    /// mkdirat→openat TOCTOU window (CVE-2021-31566 class) when an
    /// attacker swaps the just-created directory for a symlink
    /// mid-restore.
    #[test]
    fn open_subdir_nofollow_rejects_symlinked_dir() {
        use std::os::fd::AsFd;

        let tmp = tempfile::tempdir().unwrap();
        let parent = std::fs::File::open(tmp.path()).unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();

        let err = open_subdir_nofollow(parent.as_fd(), c"link").unwrap_err();
        // O_NOFOLLOW on a symlink is ELOOP; combined with O_DIRECTORY,
        // Linux reports ENOTDIR ("not a directory" wins). Accept either —
        // the invariant is that the open FAILS instead of following.
        assert!(
            matches!(
                err.raw_os_error(),
                Some(nix::libc::ELOOP) | Some(nix::libc::ENOTDIR)
            ),
            "got {err:?}"
        );
        // The real directory still opens fine.
        open_subdir_nofollow(parent.as_fd(), c"real").unwrap();
    }
}

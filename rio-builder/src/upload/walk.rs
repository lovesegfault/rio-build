//! Fused output walk (ADR-022 §6.1, P0586).
//!
//! One pass over an output's directory tree drives every per-output
//! computation the chunked upload needs:
//!
//! - the canonical NAR byte stream → SHA-256 (`nar_hash`) and the
//!   Boyer-Moore reference scan (the sink half);
//! - per-regular-file FastCDC chunk boundaries + per-chunk BLAKE3
//!   digests (the `chunk_manifest`), per-file whole-content BLAKE3
//!   (`FileEntry.digest`), and the content-addressed `Directory` DAG
//!   (the [`WalkObserver`] half).
//!
//! Every consumer hangs off the same [`dump_path_observed`] walk, so
//! each output byte is read exactly once to *compute* the upload; only
//! the chunks the store reports as missing are read a second time when
//! they go on the wire ([`ChunkSource`] records where to find them).
// r[impl builder.upload.fused-walk]
// r[impl builder.upload.chunked-manifest]

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fastcdc::v2020::FastCDC;
use sha2::{Digest, Sha256};

use rio_common::grpc::GRPC_STREAM_TIMEOUT;
use rio_common::limits::{FASTCDC_AVG_BYTES, FASTCDC_MAX_BYTES, FASTCDC_MIN_BYTES};
use rio_nix::nar::{self, WalkObserver};
use rio_nix::refscan::{CandidateSet, RefScanSink};
use rio_proto::castore::{Directory, DirectoryEntry, FileEntry, RootNode, SymlinkEntry, root_node};
use rio_proto::castore_util::directory_digest;

use super::common::await_dump_bounded;

/// One `chunk_manifest` entry: a per-file FastCDC chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkRef {
    /// BLAKE3 of the chunk's content bytes.
    pub digest: [u8; 32],
    /// Chunk length. Bounded by [`FASTCDC_MAX_BYTES`] (256 KiB), so
    /// u32 matches the manifest wire/storage width.
    pub size: u32,
}

/// Where to re-read one chunk's bytes from disk for the upload stream.
/// First occurrence wins when the same chunk appears in several files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkSource {
    /// Path of the containing file, relative to the output root.
    /// Empty when the output root *is* the file.
    pub rel_path: PathBuf,
    /// Byte offset of the chunk within that file.
    pub offset: u64,
    /// Chunk length (same value as the manifest entry's `size`).
    pub size: u32,
}

impl ChunkSource {
    /// Path of the containing file relative to the upper store
    /// (`{basename}` or `{basename}/{rel_path}`), for re-opening
    /// beneath a held upper-store dirfd — chunk re-reads never resolve
    /// an absolute path through the attacker-written tree.
    /// `Path::join("")` leaves a trailing separator that turns a
    /// single-file output root into `ENOTDIR` on open, so the empty
    /// rel_path (root *is* the file) is special-cased.
    pub fn upper_rel_path(&self, basename: &str) -> PathBuf {
        if self.rel_path.as_os_str().is_empty() {
            PathBuf::from(basename)
        } else {
            Path::new(basename).join(&self.rel_path)
        }
    }
}

/// Everything the fused walk learns about one output: enough to build
/// its `ChunkedOutput` Begin entry and to re-read any novel chunk's
/// bytes for the upload stream.
pub(crate) struct WalkedOutput {
    /// SHA-256 of the canonical NAR serialization.
    pub nar_hash: [u8; 32],
    /// NAR byte count.
    pub nar_size: u64,
    /// Resolved + sorted full store paths found by the reference scan.
    pub references: Vec<String>,
    /// Discriminated root of the castore Directory DAG.
    pub root_node: RootNode,
    /// Distinct `Directory` bodies reachable from `root_node`, keyed by
    /// their canonical digest.
    pub directories: HashMap<[u8; 32], Directory>,
    /// Per-file chunks of every regular file in canonical NAR walk
    /// order. A file appearing N times in the tree contributes its run
    /// N times.
    pub chunk_manifest: Vec<ChunkRef>,
    /// Chunk digest → where to re-read its bytes from disk.
    pub chunk_sources: HashMap<[u8; 32], ChunkSource>,
}

/// Walk one output rooted at `output_root` (`{upper_store}/{basename}`).
///
/// Runs the blocking filesystem walk on `spawn_blocking`, bounded by
/// the same hung-disk-read deadline as the legacy dump
/// ([`await_dump_bounded`]).
pub(crate) async fn walk_output(
    output_root: &Path,
    candidates: &Arc<CandidateSet>,
) -> Result<WalkedOutput, tonic::Status> {
    let root = output_root.to_path_buf();
    let cands = Arc::clone(candidates);
    let walked = await_dump_bounded(
        "fused-walk",
        GRPC_STREAM_TIMEOUT,
        tokio::task::spawn_blocking(move || walk_output_blocking(&root, &cands)),
    )
    .await?;
    walked.map_err(|e| {
        tonic::Status::internal(format!(
            "NAR serialization failed for {}: {e}",
            output_root.display()
        ))
    })
}

/// The synchronous walk body. Separated from [`walk_output`] so tests
/// can drive it without a tokio runtime.
fn walk_output_blocking(
    output_root: &Path,
    candidates: &CandidateSet,
) -> Result<WalkedOutput, nar::NarError> {
    let mut sink = TeeSink {
        sha: Sha256::new(),
        refs: RefScanSink::new(candidates.hashes()),
    };
    let mut obs = FusedObserver::default();
    let nar_size = nar::dump_path_observed(output_root, &mut sink, &mut obs)?;

    let root_node =
        RootNode {
            node: Some(obs.root.expect(
                "dump_path_observed always closes exactly one root node before returning Ok",
            )),
        };
    Ok(WalkedOutput {
        nar_hash: sink.sha.finalize().into(),
        nar_size,
        references: candidates.resolve(&sink.refs.into_found()),
        root_node,
        directories: obs.directories,
        chunk_manifest: obs.chunk_manifest,
        chunk_sources: obs.chunk_sources,
    })
}

/// `Write` sink for the NAR byte stream: every byte (framing and file
/// contents) feeds both the SHA-256 accumulator and the reference
/// scanner. The scanner must see framing too — a store-path reference
/// can appear in a symlink target or a directory entry name.
struct TeeSink {
    sha: Sha256,
    refs: RefScanSink,
}

impl Write for TeeSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sha.update(buf);
        self.refs.write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An in-progress `Directory` body: populated while the walk is inside
/// the directory, finalized (encoded + digested) on `leave_dir`.
struct DirFrame {
    /// Entry basename in the parent (empty for the NAR root).
    name: Vec<u8>,
    body: Directory,
    /// Recursive descendant count: immediate children plus the sum of
    /// every child directory's own count (`DirectoryEntry.size`).
    descendants: u64,
}

/// Per-regular-file state between `file_begin` and `file_end`.
struct FileState {
    name: Vec<u8>,
    executable: bool,
    /// Whole-content BLAKE3 → `FileEntry.digest`.
    hasher: blake3::Hasher,
    /// Total content bytes seen so far → `FileEntry.size`.
    written: u64,
    /// Content bytes not yet assigned to a chunk. `buf[0]` is at file
    /// offset `written - buf.len()`.
    buf: Vec<u8>,
    /// Path of this file relative to the output root.
    rel_path: PathBuf,
}

/// The [`WalkObserver`] that accumulates the Directory DAG and the
/// per-file chunk manifest while [`TeeSink`] consumes the byte stream.
#[derive(Default)]
struct FusedObserver {
    /// Open directories, innermost last.
    dir_stack: Vec<DirFrame>,
    /// Path components of the open directories (excluding the nameless
    /// root), joined to form each file's `rel_path`.
    rel_dir: PathBuf,
    directories: HashMap<[u8; 32], Directory>,
    chunk_manifest: Vec<ChunkRef>,
    chunk_sources: HashMap<[u8; 32], ChunkSource>,
    file: Option<FileState>,
    /// Set when the walk closes the root node (the last callback).
    root: Option<root_node::Node>,
}

impl FusedObserver {
    /// Emit one chunk: BLAKE3 it, append to the manifest, record its
    /// disk location (first occurrence wins).
    fn emit_chunk(
        manifest: &mut Vec<ChunkRef>,
        sources: &mut HashMap<[u8; 32], ChunkSource>,
        rel_path: &Path,
        file_offset: u64,
        bytes: &[u8],
    ) {
        let digest = *blake3::hash(bytes).as_bytes();
        let size =
            u32::try_from(bytes.len()).expect("chunk length is bounded by FASTCDC_MAX_BYTES");
        manifest.push(ChunkRef { digest, size });
        sources.entry(digest).or_insert_with(|| ChunkSource {
            rel_path: rel_path.to_path_buf(),
            offset: file_offset,
            size,
        });
    }

    /// Cut as many chunks as the buffered content permits.
    ///
    /// FastCDC's boundary decision for the *first* chunk of a slice is
    /// independent of the slice's total length as long as that length
    /// exceeds `max_size` (`cut_gear` clamps `remaining` to `max_size`
    /// before scanning, so a longer tail changes nothing). It DOES
    /// depend on the length when the slice is shorter — the
    /// `remaining <= min_size` early-return and the
    /// `center = min(avg, remaining)` clamp both kick in near the end
    /// of the data. So:
    ///
    /// - mid-file (`at_end == false`): only cut while more than
    ///   `FASTCDC_MAX_BYTES` is buffered, and take only the first
    ///   chunk of each run — its boundary is final;
    /// - at `file_end` (`at_end == true`): the remaining buffer IS the
    ///   true tail, so every boundary FastCDC finds in it is exactly
    ///   what a whole-file run would have produced for the same
    ///   suffix.
    ///
    /// `incremental_chunking_matches_oneshot` proves the equivalence.
    fn drain_chunks(&mut self, at_end: bool) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        // Offset of buf[0] within the file (recomputed on every call,
        // so the compaction below doesn't need to maintain it).
        let base = file.written - file.buf.len() as u64;
        let mut cursor = 0usize;

        if at_end {
            for c in FastCDC::new(
                &file.buf,
                FASTCDC_MIN_BYTES,
                FASTCDC_AVG_BYTES,
                FASTCDC_MAX_BYTES,
            ) {
                Self::emit_chunk(
                    &mut self.chunk_manifest,
                    &mut self.chunk_sources,
                    &file.rel_path,
                    base + c.offset as u64,
                    &file.buf[c.offset..c.offset + c.length],
                );
            }
            file.buf.clear();
            return;
        }

        while file.buf.len() - cursor > FASTCDC_MAX_BYTES {
            let first = FastCDC::new(
                &file.buf[cursor..],
                FASTCDC_MIN_BYTES,
                FASTCDC_AVG_BYTES,
                FASTCDC_MAX_BYTES,
            )
            .next()
            .expect("a non-empty slice yields at least one chunk");
            Self::emit_chunk(
                &mut self.chunk_manifest,
                &mut self.chunk_sources,
                &file.rel_path,
                base + cursor as u64,
                &file.buf[cursor..cursor + first.length],
            );
            cursor += first.length;
        }
        // One compaction per file_data push instead of one per chunk.
        if cursor > 0 {
            file.buf.drain(..cursor);
        }
    }

    /// Route a finished child entry into its parent's body, or record
    /// it as the root node when there is no parent.
    fn attach_to_parent(&mut self, child: ChildEntry) {
        match self.dir_stack.last_mut() {
            Some(parent) => {
                parent.descendants += 1 + child.own_descendants();
                match child {
                    ChildEntry::Dir(e) => parent.body.directories.push(e),
                    ChildEntry::File(e) => parent.body.files.push(e),
                    ChildEntry::Symlink(e) => parent.body.symlinks.push(e),
                }
            }
            None => {
                debug_assert!(self.root.is_none(), "exactly one root node per walk");
                self.root = Some(match child {
                    ChildEntry::Dir(e) => root_node::Node::DirDigest(e.digest),
                    ChildEntry::File(e) => root_node::Node::File(e),
                    ChildEntry::Symlink(e) => root_node::Node::Symlink(e),
                });
            }
        }
    }
}

/// A finished child node, about to be attached to its parent (or to
/// become the root).
enum ChildEntry {
    Dir(DirectoryEntry),
    File(FileEntry),
    Symlink(SymlinkEntry),
}

impl ChildEntry {
    /// How many descendants this child contributes to its parent's
    /// recursive count beyond the `1` for the child itself.
    fn own_descendants(&self) -> u64 {
        match self {
            ChildEntry::Dir(e) => e.size,
            ChildEntry::File(_) | ChildEntry::Symlink(_) => 0,
        }
    }
}

impl WalkObserver for FusedObserver {
    fn enter_dir(&mut self, name: &[u8]) -> io::Result<()> {
        if !name.is_empty() {
            self.rel_dir.push(os_component(name));
        }
        self.dir_stack.push(DirFrame {
            name: name.to_vec(),
            body: Directory::default(),
            descendants: 0,
        });
        Ok(())
    }

    fn leave_dir(&mut self) -> io::Result<()> {
        let frame = self
            .dir_stack
            .pop()
            .expect("leave_dir is only called for an entered directory");
        if !frame.name.is_empty() {
            self.rel_dir.pop();
        }
        let digest = directory_digest(&frame.body);
        // Identical subtrees dedup to one body; or_insert keeps the
        // first (they are byte-identical by construction).
        self.directories.entry(digest).or_insert(frame.body);
        self.attach_to_parent(ChildEntry::Dir(DirectoryEntry {
            name: frame.name,
            digest: digest.to_vec(),
            size: frame.descendants,
        }));
        Ok(())
    }

    fn symlink(&mut self, name: &[u8], target: &[u8]) -> io::Result<()> {
        self.attach_to_parent(ChildEntry::Symlink(SymlinkEntry {
            name: name.to_vec(),
            target: target.to_vec(),
        }));
        Ok(())
    }

    fn file_begin(&mut self, name: &[u8], executable: bool, _size: u64) -> io::Result<()> {
        // The declared size is fs metadata; FileEntry.size is derived
        // from the bytes actually streamed (the walk already fails on
        // a mid-dump truncation, so they can only agree).
        self.file = Some(FileState {
            name: name.to_vec(),
            executable,
            hasher: blake3::Hasher::new(),
            written: 0,
            buf: Vec::new(),
            rel_path: self.rel_dir.join(os_component(name)),
        });
        Ok(())
    }

    fn file_data(&mut self, data: &[u8]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .expect("file_data is only called between file_begin and file_end");
        file.hasher.update(data);
        file.written += data.len() as u64;
        file.buf.extend_from_slice(data);
        self.drain_chunks(false);
        Ok(())
    }

    fn file_end(&mut self) -> io::Result<()> {
        self.drain_chunks(true);
        let file = self
            .file
            .take()
            .expect("file_end is only called after file_begin");
        self.attach_to_parent(ChildEntry::File(FileEntry {
            name: file.name,
            digest: file.hasher.finalize().as_bytes().to_vec(),
            size: file.written,
            executable: file.executable,
        }));
        Ok(())
    }
}

/// Entry basename → a `Path` component. NAR entry names reaching the
/// observer are validated UTF-8 (the walk rejects non-UTF-8 names),
/// but going through `OsStr` keeps this infallible either way.
fn os_component(name: &[u8]) -> &std::ffi::OsStr {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(name)
}

// r[verify builder.upload.fused-walk]
// r[verify builder.upload.chunked-manifest]
#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::fixtures::pseudo_random_bytes;
    use std::fs;

    fn no_candidates() -> Arc<CandidateSet> {
        Arc::new(CandidateSet::from_paths(std::iter::empty::<&str>()))
    }

    /// Drive the blocking walk directly (no tokio runtime needed).
    fn walk(root: &Path, candidates: &CandidateSet) -> WalkedOutput {
        walk_output_blocking(root, candidates).expect("fixture walk succeeds")
    }

    /// Re-read a chunk's bytes via its `ChunkSource`.
    fn read_chunk(output_root: &Path, src: &ChunkSource) -> Vec<u8> {
        use std::io::{Read, Seek, SeekFrom};
        let abs = if src.rel_path.as_os_str().is_empty() {
            output_root.to_path_buf()
        } else {
            output_root.join(&src.rel_path)
        };
        let mut f = fs::File::open(abs).expect("source file exists");
        f.seek(SeekFrom::Start(src.offset)).expect("seek");
        let mut buf = vec![0u8; src.size as usize];
        f.read_exact(&mut buf).expect("chunk bytes present");
        buf
    }

    /// The whole-file chunk list FastCDC produces in one shot — the
    /// oracle the incremental path must reproduce.
    fn oneshot_chunks(data: &[u8]) -> Vec<ChunkRef> {
        if data.is_empty() {
            return Vec::new();
        }
        FastCDC::new(
            data,
            FASTCDC_MIN_BYTES,
            FASTCDC_AVG_BYTES,
            FASTCDC_MAX_BYTES,
        )
        .map(|c| ChunkRef {
            digest: *blake3::hash(&data[c.offset..c.offset + c.length]).as_bytes(),
            size: c.length as u32,
        })
        .collect()
    }

    /// nar_hash/nar_size match the eager `dump_path` oracle, and the
    /// reference scan finds exactly the candidate paths embedded in
    /// the output contents.
    #[test]
    fn hash_size_and_references_match_dump_path_oracle() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("out");
        fs::create_dir(&root)?;
        let dep_a = "/nix/store/7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a-glibc-2.38";
        let dep_b = "/nix/store/v5sv61sszx301i0x6xysaqzla09nksnd-unused";
        fs::write(root.join("bin"), format!("RPATH={dep_a}/lib\n"))?;
        // A reference appearing ONLY in a symlink target (NAR framing,
        // not file contents) must still be found.
        std::os::unix::fs::symlink(format!("{dep_a}/lib/libc.so"), root.join("link"))?;

        let candidates = Arc::new(CandidateSet::from_paths([dep_a, dep_b]));
        let out = walk(&root, &candidates);

        let nar = nar::dump_path(&root)?;
        assert_eq!(out.nar_size, nar.len() as u64);
        assert_eq!(out.nar_hash, <[u8; 32]>::from(Sha256::digest(&nar)));
        assert_eq!(out.references, vec![dep_a.to_string()], "dep_b not present");
        Ok(())
    }

    /// The Directory DAG matches a hand-constructed expectation —
    /// same shape as `rio_store::castore::tests::golden_directory_encoding`
    /// so the builder-side and store-side constructions provably agree.
    #[test]
    fn directory_dag_matches_hand_built_bodies() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("out");
        fs::create_dir(&root)?;
        fs::write(root.join("a"), b"abc")?;
        fs::create_dir(root.join("b"))?;
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(root.join("b/d"), b"12345")?;
            fs::set_permissions(root.join("b/d"), fs::Permissions::from_mode(0o755))?;
        }
        std::os::unix::fs::symlink("a", root.join("c"))?;

        let out = walk(&root, &no_candidates());

        let inner = Directory {
            directories: vec![],
            files: vec![FileEntry {
                name: b"d".to_vec(),
                digest: blake3::hash(b"12345").as_bytes().to_vec(),
                size: 5,
                executable: true,
            }],
            symlinks: vec![],
        };
        let inner_digest = directory_digest(&inner);
        let outer = Directory {
            directories: vec![DirectoryEntry {
                name: b"b".to_vec(),
                digest: inner_digest.to_vec(),
                size: 1,
            }],
            files: vec![FileEntry {
                name: b"a".to_vec(),
                digest: blake3::hash(b"abc").as_bytes().to_vec(),
                size: 3,
                executable: false,
            }],
            symlinks: vec![SymlinkEntry {
                name: b"c".to_vec(),
                target: b"a".to_vec(),
            }],
        };
        let outer_digest = directory_digest(&outer);

        assert_eq!(out.directories.len(), 2);
        assert_eq!(out.directories.get(&inner_digest), Some(&inner));
        assert_eq!(out.directories.get(&outer_digest), Some(&outer));
        assert_eq!(
            out.root_node.node,
            Some(root_node::Node::DirDigest(outer_digest.to_vec()))
        );
        Ok(())
    }

    /// `DirectoryEntry.size` is the recursive descendant count.
    #[test]
    fn directory_entry_size_is_recursive() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("out");
        // out/a/b/c → size(b)=1, size(a)=2, root entry for a has size 2.
        fs::create_dir_all(root.join("a/b"))?;
        fs::write(root.join("a/b/c"), b"x")?;

        let out = walk(&root, &no_candidates());
        let root_digest = match &out.root_node.node {
            Some(root_node::Node::DirDigest(d)) => {
                <[u8; 32]>::try_from(d.as_slice()).expect("32 bytes")
            }
            other => panic!("expected a directory root, got {other:?}"),
        };
        let root_body = &out.directories[&root_digest];
        assert_eq!(root_body.directories[0].name, b"a");
        assert_eq!(root_body.directories[0].size, 2);
        Ok(())
    }

    /// Every file's chunk run reassembles to the file contents, every
    /// chunk's blake3 matches its digest, and the run's sizes sum to
    /// the file size. Covers empty files, multi-chunk files, and
    /// duplicate file contents.
    #[test]
    fn chunk_manifest_reassembles_every_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("out");
        fs::create_dir(&root)?;
        // > FASTCDC_MAX_BYTES → multiple chunks.
        let big = pseudo_random_bytes(7, 3 * FASTCDC_MAX_BYTES + 12_345);
        fs::write(root.join("big"), &big)?;
        fs::write(root.join("empty"), b"")?;
        fs::write(root.join("small"), b"tiny contents")?;
        // Byte-identical to `small` under a different name.
        fs::write(root.join("twin"), b"tiny contents")?;

        let out = walk(&root, &no_candidates());

        // Walk order is byte-lex: big, empty, small, twin. Consume the
        // manifest greedily per file.
        let mut cursor = 0usize;
        for (name, contents) in [
            ("big", big.as_slice()),
            ("empty", &b""[..]),
            ("small", &b"tiny contents"[..]),
            ("twin", &b"tiny contents"[..]),
        ] {
            let mut reassembled = Vec::new();
            while reassembled.len() < contents.len() {
                let c = &out.chunk_manifest[cursor];
                let src = &out.chunk_sources[&c.digest];
                let bytes = read_chunk(&root, src);
                assert_eq!(bytes.len(), c.size as usize, "{name}: size mismatch");
                assert_eq!(
                    *blake3::hash(&bytes).as_bytes(),
                    c.digest,
                    "{name}: chunk digest mismatch"
                );
                reassembled.extend_from_slice(&bytes);
                cursor += 1;
            }
            assert_eq!(reassembled, contents, "{name}: reassembly mismatch");
        }
        assert_eq!(cursor, out.chunk_manifest.len(), "no trailing chunks");

        // `small` and `twin` share digests → one source entry per
        // distinct digest, two manifest runs.
        let small_chunks = oneshot_chunks(b"tiny contents");
        assert_eq!(small_chunks.len(), 1);
        assert_eq!(
            out.chunk_manifest
                .iter()
                .filter(|c| c.digest == small_chunks[0].digest)
                .count(),
            2,
            "duplicate file contributes its run twice"
        );
        // The first occurrence (`small`) wins the source slot.
        assert_eq!(
            out.chunk_sources[&small_chunks[0].digest].rel_path,
            PathBuf::from("small")
        );
        Ok(())
    }

    /// A single-regular-file output root: `Node::File`, no
    /// directories, empty entry name, chunks readable from the root
    /// itself.
    #[test]
    fn single_file_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let f = dir.path().join("out-file");
        let contents = pseudo_random_bytes(3, FASTCDC_MAX_BYTES + 1);
        fs::write(&f, &contents)?;

        let out = walk(&f, &no_candidates());
        assert!(out.directories.is_empty());
        match &out.root_node.node {
            Some(root_node::Node::File(fe)) => {
                assert!(fe.name.is_empty());
                assert_eq!(fe.size, contents.len() as u64);
                assert_eq!(fe.digest, blake3::hash(&contents).as_bytes().to_vec());
                assert!(!fe.executable);
            }
            other => panic!("expected a file root, got {other:?}"),
        }
        assert!(
            out.chunk_manifest.len() >= 2,
            "one byte over MAX → ≥2 chunks"
        );
        let total: u64 = out.chunk_manifest.iter().map(|c| u64::from(c.size)).sum();
        assert_eq!(total, contents.len() as u64);
        // rel_path is empty → joins back to the root itself.
        let first = &out.chunk_sources[&out.chunk_manifest[0].digest];
        assert_eq!(first.rel_path, PathBuf::new());
        assert_eq!(read_chunk(&f, first).len(), first.size as usize);
        Ok(())
    }

    /// A single-symlink output root: `Node::Symlink`, nothing else.
    #[test]
    fn single_symlink_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let l = dir.path().join("out-link");
        std::os::unix::fs::symlink("/nowhere/in/particular", &l)?;

        let out = walk(&l, &no_candidates());
        assert!(out.directories.is_empty());
        assert!(out.chunk_manifest.is_empty());
        assert_eq!(
            out.root_node.node,
            Some(root_node::Node::Symlink(SymlinkEntry {
                name: Vec::new(),
                target: b"/nowhere/in/particular".to_vec(),
            }))
        );
        Ok(())
    }

    /// Empty directories produce a body with no entries and a
    /// well-defined digest; identical empty dirs dedup to one body.
    #[test]
    fn empty_directories_dedup() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("out");
        fs::create_dir_all(root.join("x"))?;
        fs::create_dir_all(root.join("y"))?;

        let out = walk(&root, &no_candidates());
        // root + (x == y) = 2 distinct bodies.
        assert_eq!(out.directories.len(), 2);
        let empty_digest = directory_digest(&Directory::default());
        assert!(out.directories.contains_key(&empty_digest));
        Ok(())
    }

    /// The incremental push-based chunker produces byte-for-byte the
    /// same chunk list as one-shot FastCDC over the whole file, for
    /// every (file size, push size) combination. This is the property
    /// that makes builder-side chunks dedup against anything.
    #[test]
    fn incremental_chunking_matches_oneshot() {
        const MIN: usize = FASTCDC_MIN_BYTES;
        const MAX: usize = FASTCDC_MAX_BYTES;
        let sizes = [
            0,
            1,
            MIN - 1,
            MIN,
            MAX,
            MAX + 1,
            2 * MAX,
            2 * MAX + 7,
            1024 * 1024,
            3 * 1024 * 1024 + 12_345,
        ];
        let pushes = [1024, 64 * 1024, 256 * 1024, usize::MAX];

        for (i, &size) in sizes.iter().enumerate() {
            let data = pseudo_random_bytes(i as u64, size);
            let expected = oneshot_chunks(&data);
            for &push in &pushes {
                let mut obs = FusedObserver::default();
                obs.file_begin(b"f", false, size as u64).unwrap();
                for piece in data.chunks(push.min(data.len().max(1))) {
                    obs.file_data(piece).unwrap();
                }
                obs.file_end().unwrap();
                assert_eq!(
                    obs.chunk_manifest, expected,
                    "size={size} push={push}: incremental chunking diverged from one-shot"
                );
                let total: u64 = obs.chunk_manifest.iter().map(|c| u64::from(c.size)).sum();
                assert_eq!(
                    total, size as u64,
                    "size={size}: chunk sizes must sum to the file size"
                );
            }
        }
    }
}

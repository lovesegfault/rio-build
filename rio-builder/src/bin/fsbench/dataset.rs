//! The dataset: a re-rooted REAL closure.
//!
//! `gen` walks one or more harvest roots (the bench drv passes a
//! pinned nixpkgs package — real ELFs, interface files, docs: real
//! chunking, real compressibility, real size distribution) in sorted
//! order and copies every regular file once into the dataset under a
//! seeded directory layout, preserving symlinks and exec bits. The
//! seed controls ONLY the tree layout — file contents are the
//! harvest's, pinned by flake.lock; "cold" therefore means the bench
//! node's local cache is empty, which the honesty gate verifies
//! rather than assumes.
//!
//! Real content has natural cross-file duplication, which breaks
//! logical-bytes honesty arithmetic — so gen chunks every file with
//! the SAME FastCDC parameters the store uses
//! (`rio_common::limits::FASTCDC_*`, exactly what the builder's fused
//! upload walk applies) and stamps the deduplicated unique-chunk byte
//! counts into the manifest. The honesty gate references those, not
//! logical bytes.

use std::collections::HashSet;
use std::fs;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use fastcdc::v2020::FastCDC;
use rio_common::limits::{FASTCDC_AVG_BYTES, FASTCDC_MAX_BYTES, FASTCDC_MIN_BYTES};
use serde::{Deserialize, Serialize};

/// Bumped when the dataset construction changes. Part of the baseline
/// identity key — results from different workload versions are never
/// comparable.
pub const WORKLOAD_VERSION: u32 = 1;

/// The castore streaming threshold (rio-builder config.rs default).
/// Gen fails loudly unless the harvest has at least two files above
/// it: the largest becomes the randread reserve, and the read storm
/// needs at least one more to exercise `miss_stream`.
pub const STREAM_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// Written to `<dataset>/manifest.json` by `gen`; the only oracle
/// `run` has for what the dataset contains and what each file must
/// hash to.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub workload_version: u32,
    pub seed: String,
    pub total_bytes: u64,
    /// Deduplicated FastCDC chunk bytes (store parameters) over the
    /// WHOLE dataset — the whole-run honesty reference.
    pub unique_chunk_bytes: u64,
    /// Same, over the read-storm subset (everything except the
    /// reserve) — the cold-window honesty reference.
    pub unique_chunk_bytes_storm: u64,
    /// blake3 over the sorted (path, kind, digest/target, exec) tree
    /// listing — the dataset half of the baseline identity key.
    pub dataset_digest: String,
    /// The single largest file: randread target, excluded from the
    /// read storm (cold randread is read-during-fill and needs a
    /// never-opened file).
    pub reserve: String,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FileEntry {
    /// Relative to the dataset root.
    pub path: String,
    pub bytes: u64,
    /// blake3 of the file contents, recorded at gen time —
    /// read_storm_cold verifies every file against this.
    pub blake3: String,
}

impl Manifest {
    pub fn load(dataset: &Path) -> Result<Self> {
        let p = dataset.join("manifest.json");
        let body = fs::read_to_string(&p)
            .with_context(|| format!("read dataset manifest {}", p.display()))?;
        let m: Manifest = serde_json::from_str(&body).context("parse dataset manifest")?;
        ensure!(
            m.workload_version == WORKLOAD_VERSION,
            "dataset workload_version {} != binary workload_version {WORKLOAD_VERSION}",
            m.workload_version
        );
        Ok(m)
    }

    pub fn randread_reserve(&self) -> Option<&FileEntry> {
        self.files.iter().find(|f| f.path == self.reserve)
    }

    /// Everything read_storm walks: all files except the reserve.
    pub fn read_storm_files(&self) -> Vec<&FileEntry> {
        self.files
            .iter()
            .filter(|f| f.path != self.reserve)
            .collect()
    }
}

/// Generate the dataset by re-rooting `harvest_roots` under `out`.
/// Deterministic given the seed and the roots' contents (pinned by
/// flake.lock in the real bench).
pub fn generate(seed: &str, harvest_roots: &[PathBuf], out: &Path) -> Result<Manifest> {
    ensure!(
        seed.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "seed must be alphanumeric-or-dash (it lands in store-path names): {seed:?}"
    );
    ensure!(
        !harvest_roots.is_empty(),
        "at least one --harvest root required"
    );
    fs::create_dir_all(out)?;

    // Sorted source walk → deterministic order regardless of readdir.
    let mut sources = Vec::new();
    for root in harvest_roots {
        walk_sorted(root, &mut sources)?;
    }

    let mut layout = LayoutRng::new(seed);
    let mut files = Vec::new();
    let mut chunks_per_file: Vec<ChunkList> = Vec::new();
    // Tree listing for dataset_digest: (dataset path, kind, digest or
    // symlink target, exec bit), in walk order (sorted by
    // construction).
    let mut listing = blake3::Hasher::new();
    let mut total = 0u64;

    for src in &sources {
        match src {
            Source::File { abs, exec } => {
                let rel = layout.place();
                let dst = out.join(&rel);
                fs::create_dir_all(dst.parent().expect("placed path has a parent"))?;
                let (bytes, digest, chunks) = copy_hash_chunk(abs, &dst)?;
                if *exec {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))?;
                }
                listing.update(rel.as_bytes());
                listing.update(if *exec { b"|x|" } else { b"|f|" });
                listing.update(digest.as_bytes());
                listing.update(b"\n");
                total += bytes;
                chunks_per_file.push(chunks);
                files.push(FileEntry {
                    path: rel,
                    bytes,
                    blake3: digest,
                });
            }
            Source::Symlink { target } => {
                let rel = layout.place();
                let dst = out.join(&rel);
                fs::create_dir_all(dst.parent().expect("placed path has a parent"))?;
                std::os::unix::fs::symlink(target, &dst)?;
                listing.update(rel.as_bytes());
                listing.update(b"|l|");
                listing.update(target.as_os_str().as_encoded_bytes());
                listing.update(b"\n");
            }
        }
    }
    ensure!(
        !files.is_empty(),
        "harvest roots contained no regular files"
    );

    // Reserve = the single largest file (walk order breaks ties
    // deterministically). The storm needs at least one MORE
    // stream-side file; if the pinned harvest ever stops providing
    // two, fail at gen time rather than silently dropping miss_stream
    // coverage.
    let reserve_idx = (0..files.len())
        .max_by_key(|&i| (files[i].bytes, files.len() - i))
        .expect("non-empty");
    let reserve = files[reserve_idx].path.clone();
    let stream_in_storm = files
        .iter()
        .enumerate()
        .filter(|(i, f)| *i != reserve_idx && f.bytes > STREAM_THRESHOLD_BYTES)
        .count();
    ensure!(
        files[reserve_idx].bytes > STREAM_THRESHOLD_BYTES && stream_in_storm >= 1,
        "harvest must contain at least two files above the {STREAM_THRESHOLD_BYTES}-byte \
         stream threshold (reserve + one for the read storm); largest = {} bytes, \
         stream-side files in storm = {stream_in_storm} — pick a bigger package set",
        files[reserve_idx].bytes
    );

    // Unique-chunk accounting with the store's own chunker — the
    // honesty references. Whole dataset and the storm subset.
    let unique = |skip: Option<usize>| -> u64 {
        let mut seen: HashSet<[u8; 32]> = HashSet::new();
        let mut bytes = 0u64;
        for (i, chunks) in chunks_per_file.iter().enumerate() {
            if Some(i) == skip {
                continue;
            }
            for (digest, len) in chunks {
                if seen.insert(*digest) {
                    bytes += u64::from(*len);
                }
            }
        }
        bytes
    };
    let unique_chunk_bytes = unique(None);
    let unique_chunk_bytes_storm = unique(Some(reserve_idx));

    let manifest = Manifest {
        workload_version: WORKLOAD_VERSION,
        seed: seed.to_string(),
        total_bytes: total,
        unique_chunk_bytes,
        unique_chunk_bytes_storm,
        dataset_digest: listing.finalize().to_hex().to_string(),
        reserve,
        files,
    };
    fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

enum Source {
    File { abs: PathBuf, exec: bool },
    Symlink { target: PathBuf },
}

/// Recursive sorted walk: regular files and symlinks, lstat view
/// (symlinks are recreated, never followed — store links point into
/// the same closure and following would duplicate their targets).
fn walk_sorted(dir: &Path, out: &mut Vec<Source>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let ft = entry.file_type()?;
        let path = entry.path();
        if ft.is_dir() {
            walk_sorted(&path, out)?;
        } else if ft.is_symlink() {
            out.push(Source::Symlink {
                target: fs::read_link(&path)?,
            });
        } else if ft.is_file() {
            use std::os::unix::fs::PermissionsExt;
            let exec = entry.metadata()?.permissions().mode() & 0o111 != 0;
            out.push(Source::File { abs: path, exec });
        }
    }
    Ok(())
}

/// Stream-copy `src` → `dst`, computing the whole-file blake3 and the
/// store-parameter FastCDC chunk list in one pass. The incremental
/// cut discipline mirrors the builder's fused walk (cut leading
/// chunks while more than FASTCDC_MAX_BYTES remains; one-shot the
/// tail), which `upload::walk` proves equivalent to one-shot
/// chunking — memory stays bounded for the multi-hundred-MB closure
/// files instead of buffering them whole.
/// `(blake3 digest, length)` per FastCDC chunk of one file.
type ChunkList = Vec<([u8; 32], u32)>;

fn copy_hash_chunk(src: &Path, dst: &Path) -> Result<(u64, String, ChunkList)> {
    let mut r = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mut w = BufWriter::new(fs::File::create(dst)?);
    let mut hasher = blake3::Hasher::new();
    let mut chunks = Vec::new();
    let mut buf = Vec::new();
    let mut io = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut io)?;
        if n == 0 {
            break;
        }
        w.write_all(&io[..n])?;
        hasher.update(&io[..n]);
        total += n as u64;
        buf.extend_from_slice(&io[..n]);
        let mut cursor = 0usize;
        while buf.len() - cursor > FASTCDC_MAX_BYTES {
            let first = chunker(&buf[cursor..])
                .next()
                .expect("a non-empty slice yields at least one chunk");
            chunks.push((
                *blake3::hash(&buf[cursor..cursor + first.length]).as_bytes(),
                first.length as u32,
            ));
            cursor += first.length;
        }
        if cursor > 0 {
            buf.drain(..cursor);
        }
    }
    for c in chunker(&buf) {
        chunks.push((
            *blake3::hash(&buf[c.offset..c.offset + c.length]).as_bytes(),
            c.length as u32,
        ));
    }
    w.flush()?;
    Ok((total, hasher.finalize().to_hex().to_string(), chunks))
}

/// The store's chunker, exactly: same algorithm, same parameters as
/// the builder's fused upload walk and rio-store's `cas` chunker.
fn chunker(data: &[u8]) -> FastCDC<'_> {
    FastCDC::new(
        data,
        FASTCDC_MIN_BYTES,
        FASTCDC_AVG_BYTES,
        FASTCDC_MAX_BYTES,
    )
}

/// Seeded layout: nested two-level directories with bounded fan-out.
/// Deterministic per seed; independent of file contents.
struct LayoutRng {
    state: u64,
    counter: u64,
}

impl LayoutRng {
    fn new(seed: &str) -> Self {
        Self {
            state: u64::from_le_bytes(
                blake3::hash(seed.as_bytes()).as_bytes()[..8]
                    .try_into()
                    .expect("blake3 output is 32 bytes"),
            ),
            counter: 0,
        }
    }

    fn place(&mut self) -> String {
        let a = crate::phases::splitmix64(&mut self.state) % 32;
        let b = crate::phases::splitmix64(&mut self.state) % 16;
        let i = self.counter;
        self.counter += 1;
        format!("d{a:02x}/s{b:02x}/f{i:06}")
    }
}

/// Shared test fixture: a small stand-in harvest with the features gen
/// must handle — two stream-side files (reserve + one storm file), a
/// duplicated-content pair (unique-chunk accounting must collapse it),
/// an exec bit, and a symlink.
#[cfg(test)]
pub(crate) fn test_fixture_tree(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(root.join("lib")).unwrap();
    fs::create_dir_all(root.join("bin")).unwrap();
    let mut state = 7u64;
    let mut blob = |bytes: usize| {
        let mut v = Vec::with_capacity(bytes + 8);
        while v.len() < bytes {
            v.extend_from_slice(&crate::phases::splitmix64(&mut state).to_le_bytes());
        }
        v.truncate(bytes);
        v
    };
    fs::write(root.join("lib/big-a.so"), blob(9 * 1024 * 1024)).unwrap();
    fs::write(root.join("lib/big-b.so"), blob(8 * 1024 * 1024 + 4096)).unwrap();
    fs::write(root.join("lib/dup-1.txt"), b"identical small content").unwrap();
    fs::write(root.join("lib/dup-2.txt"), b"identical small content").unwrap();
    fs::write(root.join("doc.txt"), blob(3000)).unwrap();
    fs::write(root.join("bin/tool"), blob(64 * 1024)).unwrap();
    fs::set_permissions(root.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("bin/tool", root.join("run")).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen_fixture(seed: &str) -> (tempfile::TempDir, tempfile::TempDir, Manifest) {
        let src = tempfile::tempdir().unwrap();
        test_fixture_tree(src.path());
        let out = tempfile::tempdir().unwrap();
        let m = generate(seed, &[src.path().to_path_buf()], out.path()).unwrap();
        (src, out, m)
    }

    #[test]
    fn deterministic_per_seed_layout_only() {
        let (_s1, _o1, a) = gen_fixture("seed-1");
        let (_s2, _o2, b) = gen_fixture("seed-1");
        let (_s3, _o3, c) = gen_fixture("seed-2");

        // Same seed + same source → identical manifests (digest,
        // unique counts, every path and file digest).
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );

        // Different seed → different LAYOUT (paths, hence
        // dataset_digest) but identical CONTENT accounting: the bytes
        // are the harvest's, the seed only places them.
        assert_ne!(a.dataset_digest, c.dataset_digest);
        assert_eq!(a.total_bytes, c.total_bytes);
        assert_eq!(a.unique_chunk_bytes, c.unique_chunk_bytes);
        let mut da: Vec<&str> = a.files.iter().map(|f| f.blake3.as_str()).collect();
        let mut dc: Vec<&str> = c.files.iter().map(|f| f.blake3.as_str()).collect();
        da.sort_unstable();
        dc.sort_unstable();
        assert_eq!(da, dc, "content digests are seed-independent");
    }

    #[test]
    fn manifest_digests_match_written_bytes() {
        let (_s, out, m) = gen_fixture("seed-1");
        for f in &m.files {
            let bytes = fs::read(out.path().join(&f.path)).unwrap();
            assert_eq!(bytes.len() as u64, f.bytes);
            assert_eq!(blake3::hash(&bytes).to_hex().to_string(), f.blake3);
        }
    }

    #[test]
    fn unique_chunk_bytes_matches_independent_recount() {
        // Recount with one-shot FastCDC over the WRITTEN tree — the
        // streaming chunker in copy_hash_chunk must agree, and the
        // duplicated-content pair must be counted once.
        let (_s, out, m) = gen_fixture("seed-1");
        let recount = |skip: Option<&str>| -> u64 {
            let mut seen = HashSet::new();
            let mut bytes = 0u64;
            for f in &m.files {
                if Some(f.path.as_str()) == skip {
                    continue;
                }
                let data = fs::read(out.path().join(&f.path)).unwrap();
                for c in chunker(&data) {
                    if seen.insert(*blake3::hash(&data[c.offset..c.offset + c.length]).as_bytes()) {
                        bytes += c.length as u64;
                    }
                }
            }
            bytes
        };
        assert_eq!(m.unique_chunk_bytes, recount(None));
        assert_eq!(m.unique_chunk_bytes_storm, recount(Some(&m.reserve)));
        // The dup pair proves dedupe is live: unique < logical total.
        assert!(m.unique_chunk_bytes < m.total_bytes);
    }

    #[test]
    fn reserve_is_largest_and_storm_excludes_it() {
        let (_s, _o, m) = gen_fixture("seed-1");
        let reserve = m.randread_reserve().expect("reserve resolves");
        assert_eq!(reserve.bytes, 9 * 1024 * 1024, "largest fixture file");
        assert!(reserve.bytes > STREAM_THRESHOLD_BYTES);
        assert!(m.read_storm_files().iter().all(|f| f.path != m.reserve));
        assert_eq!(m.read_storm_files().len(), m.files.len() - 1);
        // The storm still has a stream-side file (big-b).
        assert!(
            m.read_storm_files()
                .iter()
                .any(|f| f.bytes > STREAM_THRESHOLD_BYTES)
        );
    }

    #[test]
    fn gen_fails_loudly_without_two_stream_side_files() {
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("small"), b"tiny").unwrap();
        fs::write(src.path().join("one-big"), vec![7u8; 9 * 1024 * 1024]).unwrap();
        let out = tempfile::tempdir().unwrap();
        let err = generate("s", &[src.path().to_path_buf()], out.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("stream threshold"), "got: {err}");
    }
}

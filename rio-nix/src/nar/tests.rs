use std::io::{Cursor, Read};

use rstest::rstest;

use super::sync_wire::{write_bytes, write_str, write_u64};
use super::*;

fn reg(executable: bool, contents: &[u8]) -> NarNode {
    NarNode::Regular {
        executable,
        contents: contents.to_vec(),
    }
}

fn entry(name: &str, node: NarNode) -> NarEntry {
    NarEntry {
        name: name.to_string(),
        node,
    }
}

/// serialize → parse is identity for every NarNode shape.
#[rstest]
#[case::regular_file(reg(false, b"hello world\n"))]
#[case::executable_file(reg(true, b"#!/bin/sh\necho hello\n"))]
#[case::symlink(NarNode::Symlink { target: "file.txt".to_string() })]
#[case::empty_directory(NarNode::Directory { entries: vec![] })]
#[case::empty_file(reg(false, b""))]
#[case::directory_with_entries(NarNode::Directory {
    entries: vec![
        entry("a_file.txt", reg(false, b"content a")),
        entry("b_link", NarNode::Symlink { target: "a_file.txt".to_string() }),
        entry("c_dir", NarNode::Directory {
            entries: vec![entry("nested.txt", reg(false, b"nested content"))],
        }),
    ],
})]
fn roundtrip(#[case] node: NarNode) -> anyhow::Result<()> {
    let mut buf = Vec::new();
    serialize(&mut buf, &node)?;
    let parsed = parse(&mut Cursor::new(&buf))?;
    assert_eq!(parsed, node);
    Ok(())
}

/// The NAR grammar requires a "contents" token even for empty regular
/// files. `( type regular )` is not a valid production — reject it.
#[test]
fn reject_regular_without_contents() {
    // Hand-roll: nix-archive-1 ( type regular )
    let mut buf = Vec::new();
    write_str(&mut buf, NAR_MAGIC).unwrap();
    write_str(&mut buf, "(").unwrap();
    write_str(&mut buf, "type").unwrap();
    write_str(&mut buf, "regular").unwrap();
    write_str(&mut buf, ")").unwrap();

    let result = parse(&mut Cursor::new(&buf));
    assert!(
        matches!(result, Err(NarError::UnexpectedToken { .. })),
        "expected UnexpectedToken for contents-less regular, got {result:?}"
    );
}

/// A malicious NAR with ~300 levels of nested directories must be rejected
/// with NestingTooDeep, not crash with a stack overflow.
#[test]
fn reject_deeply_nested_nar() {
    // Build inside-out: each level wraps the previous in a one-entry dir.
    let mut node = NarNode::Regular {
        executable: false,
        contents: vec![],
    };
    for _ in 0..(MAX_NAR_DEPTH + 10) {
        node = NarNode::Directory {
            entries: vec![NarEntry {
                name: "a".to_string(),
                node,
            }],
        };
    }

    // serialize() recurses too — at 266 levels it's fine on a default
    // 2 MiB test stack. If this ever blows the test stack, switch to
    // hand-rolling bytes with write_str.
    let mut buf = Vec::new();
    serialize(&mut buf, &node).expect("serialize succeeds");

    let result = parse(&mut Cursor::new(&buf));
    assert!(
        matches!(result, Err(NarError::NestingTooDeep(d)) if d > MAX_NAR_DEPTH),
        "expected NestingTooDeep, got {result:?}"
    );
}

/// A NAR at exactly MAX_NAR_DEPTH must still parse.
#[test]
fn accept_nar_at_depth_limit() -> anyhow::Result<()> {
    let mut node = NarNode::Regular {
        executable: false,
        contents: b"leaf".to_vec(),
    };
    for _ in 0..MAX_NAR_DEPTH {
        node = NarNode::Directory {
            entries: vec![NarEntry {
                name: "a".to_string(),
                node,
            }],
        };
    }
    let mut buf = Vec::new();
    serialize(&mut buf, &node)?;
    let parsed = parse(&mut Cursor::new(&buf))?;
    assert_eq!(parsed, node);
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
    pub(super) fn arb_nar_node() -> impl Strategy<Value = NarNode> {
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
                proptest::collection::vec(("[a-z]{1,10}", inner), 0..5).prop_map(|mut entries| {
                    // Sort by name and deduplicate to satisfy the parser invariant.
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    entries.dedup_by(|a, b| a.0 == b.0);

                    let entries = entries
                        .into_iter()
                        .map(|(name, node)| NarEntry { name, node })
                        .collect();

                    NarNode::Directory { entries }
                })
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

    proptest! {
        // Fewer cases than the in-memory roundtrip — each case does
        // real filesystem I/O (tempdir create/write/read/remove).
        #![proptest_config(ProptestConfig::with_cases(256))]
        /// `serialize → restore_path_streaming → dump_path` is
        /// byte-identical for arbitrary NAR trees. This is the
        /// streaming-restore equivalent of `nar_roundtrip`.
        #[test]
        fn restore_streaming_roundtrip_prop(node in arb_nar_node()) {
            let mut buf = Vec::new();
            serialize(&mut buf, &node)?;

            let dst_dir = tempfile::TempDir::new().unwrap();
            let dst = dst_dir.path().join("r");
            restore_path_streaming(&mut Cursor::new(&buf), &dst)?;

            let redumped = dump_path(&dst)?;
            prop_assert_eq!(buf, redumped);
        }
    }
}

/// Compare our NAR output against `nix-store --dump` for a single file.
#[test]
#[tracing_test::traced_test]
fn golden_single_file() -> anyhow::Result<()> {
    let drv_path = "/nix/store/3543bymzsssf34hrlchksl28apr3gfyc-simple-test.drv";

    // Check if path exists (test may run without this specific path)
    if !std::path::Path::new(drv_path).exists() {
        tracing::info!("skipping: {drv_path} not found");
        return Ok(());
    }

    let our_nar = dump_path(std::path::Path::new(drv_path))?;

    let nix_output = std::process::Command::new("nix-store")
        .args(["--dump", drv_path])
        .output();

    let nix_output = match nix_output {
        Ok(o) if o.status.success() => o,
        _ => {
            tracing::info!("skipping: nix-store not available");
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
#[tracing_test::traced_test]
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

    #[allow(clippy::disallowed_methods)] // tempdir path, test-only
    let nix_output = std::process::Command::new("nix-store")
        .args(["--dump", &root.to_string_lossy()])
        .output();

    let nix_output = match nix_output {
        Ok(o) if o.status.success() => o,
        _ => {
            tracing::info!("skipping: nix-store not available");
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

// -----------------------------------------------------------------------
// restore_path_streaming round-trip with dump_path_streaming
// -----------------------------------------------------------------------

/// THE correctness invariant for restore_path_streaming:
/// `dump → restore → dump` is byte-identical. If this diverges, the
/// builder's FUSE fetch path materializes corrupt store paths.
// r[verify builder.fuse.fetch-bounded-memory]
#[test]
fn restore_streaming_roundtrip() -> anyhow::Result<()> {
    let src_dir = tempfile::TempDir::new()?;
    let src = src_dir.path().join("root");
    std::fs::create_dir(&src)?;

    // All three node types + edge cases (empty file, executable, nested).
    std::fs::create_dir(src.join("sub"))?;
    std::fs::write(src.join("file.txt"), "hello restore\n")?;
    std::fs::write(src.join("sub/inner.txt"), b"nested content")?;
    std::os::unix::fs::symlink("file.txt", src.join("link"))?;
    std::fs::write(src.join("empty"), b"")?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(src.join("script.sh"), "#!/bin/sh\necho hi\n")?;
        std::fs::set_permissions(
            src.join("script.sh"),
            std::fs::Permissions::from_mode(0o755),
        )?;
    }

    let mut nar = Vec::new();
    dump_path_streaming(&src, &mut nar)?;

    let dst_dir = tempfile::TempDir::new()?;
    let dst = dst_dir.path().join("restored");
    restore_path_streaming(&mut Cursor::new(&nar), &dst)?;

    let mut nar2 = Vec::new();
    dump_path_streaming(&dst, &mut nar2)?;
    assert_eq!(
        nar, nar2,
        "dump → restore_path_streaming → dump must be byte-identical"
    );

    // Spot-check the executable bit survived.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dst.join("script.sh"))?
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "executable bit lost on restore");
    Ok(())
}

/// `restore_path_streaming` has NO per-file `MAX_CONTENT_SIZE` cap
/// (unlike `parse`). A single regular file larger than 256 MiB must
/// extract successfully — this is the I-180 fix for GB-scale inputs
/// like vendored-tarball store paths.
///
/// Builds the NAR via a chained reader (header + `io::repeat(0)` +
/// trailer) so the test itself stays bounded-memory; only the
/// restored file occupies disk.
#[test]
fn restore_streaming_large_file_over_256mib() -> anyhow::Result<()> {
    // 256 MiB + 1 KiB — just past `parse`'s MAX_CONTENT_SIZE.
    const LEN: u64 = 256 * 1024 * 1024 + 1024;

    // Build NAR framing around a `len`-byte zero-filled regular file
    // WITHOUT materializing `len` bytes in memory: header tokens +
    // u64 len, then `io::repeat(0).take(len)` for content, then
    // padding (LEN % 8 == 0, so none) + closing ")".
    let mut head = Vec::new();
    for t in &[NAR_MAGIC, "(", "type", "regular", "contents"] {
        write_str(&mut head, t).unwrap();
    }
    write_u64(&mut head, LEN).unwrap();
    let mut tail = Vec::new();
    write_str(&mut tail, ")").unwrap();

    let mut r = Cursor::new(head)
        .chain(io::repeat(0u8).take(LEN))
        .chain(Cursor::new(tail));

    let dst_dir = tempfile::TempDir::new()?;
    let dst = dst_dir.path().join("big");
    restore_path_streaming(&mut r, &dst)?;

    let meta = std::fs::metadata(&dst)?;
    assert_eq!(meta.len(), LEN, "restored file size mismatch");

    // Sanity: `parse` on the SAME logical NAR would have rejected
    // this with ContentTooLarge — that's the gap restore closes.
    // (We don't actually run parse on a 256 MiB Vec here; the
    // bound check fires on the u64 read before allocation, so a
    // header-only Cursor suffices.)
    let mut head_only = Vec::new();
    for t in &[NAR_MAGIC, "(", "type", "regular", "contents"] {
        write_str(&mut head_only, t).unwrap();
    }
    write_u64(&mut head_only, LEN).unwrap();
    let err = parse(&mut Cursor::new(&head_only)).unwrap_err();
    assert!(
        matches!(err, NarError::ContentTooLarge(n) if n == LEN),
        "parse should reject {LEN}-byte file with ContentTooLarge, got {err:?}"
    );
    Ok(())
}

/// Restored regular files and directories MUST have the canonical Nix
/// store-path mtime (1 second past Epoch). NAR carries no timestamps; on
/// extraction Nix's `restorePath()` finishes with
/// `canonicalisePathMetaData()` which sets `mtime=1`. If `restore_node`
/// leaves `mtime=now` (the `File::create()`/`create_dir()` default), the
/// FUSE-served chroot store presents non-canonical metadata and any build
/// that reads input mtimes mis-behaves — most visibly, nixpkgs'
/// `set-source-date-epoch-to-latest.sh` `postUnpackHook` finds a
/// "newest" source file with `mtime≈now`, sets `SOURCE_DATE_EPOCH` to
/// it, and a `tar --mtime=@$SOURCE_DATE_EPOCH`-producing FOD
/// (`fetchPnpmDeps`, `fetchYarnDeps`, …) becomes non-deterministic.
///
/// Symlink mtime is intentionally NOT asserted here: there is no `std`
/// API to set a symlink's own mtime without a new dependency, and the
/// FUSE serve layer (`stat_to_attr`) hardcodes canonical times
/// regardless of on-disk state. Nothing in `set-source-date-epoch-to-
/// latest.sh` reads symlink mtime (`find -type f`).
// r[verify builder.nar.canonical-mtime]
#[test]
fn restore_streaming_canonicalizes_mtime() -> anyhow::Result<()> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    // The canonical Nix store-path mtime: 1 second past Epoch. Not 0
    // (some tools treat 0 as "no timestamp") and not "now" (breaks
    // reproducibility). Matches `mtimeStore` in Nix's
    // `posix-fs-canonicalise.cc`.
    const CANON_MTIME: u64 = 1;

    let src_dir = tempfile::TempDir::new()?;
    let src = src_dir.path().join("root");
    std::fs::create_dir(&src)?;
    std::fs::create_dir(src.join("sub"))?;
    std::fs::write(src.join("file.txt"), "x")?;
    std::fs::write(src.join("sub/inner.txt"), "y")?;
    std::os::unix::fs::symlink("file.txt", src.join("link"))?;

    let mut nar = Vec::new();
    dump_path_streaming(&src, &mut nar)?;

    let dst_dir = tempfile::TempDir::new()?;
    let dst = dst_dir.path().join("restored");
    restore_path_streaming(&mut Cursor::new(&nar), &dst)?;

    let want = SystemTime::UNIX_EPOCH + Duration::from_secs(CANON_MTIME);
    // Walk every restored regular file and directory, including the
    // root, and assert mtime is the canonical value. Use
    // `symlink_metadata` so symlinks are skipped (`is_file()` /
    // `is_dir()` are both false on a symlink's own metadata).
    let mut checked = 0usize;
    for path in walkdir(&dst) {
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_file() || meta.is_dir() {
            let got = meta.modified()?;
            assert_eq!(
                got.duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(u64::MAX),
                CANON_MTIME,
                "non-canonical mtime on {path:?}: got {got:?}, want {want:?}. \
                 NAR restore must mirror Nix's canonicalisePathMetaData (mtime=1)."
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 4,
        "expected to check ≥4 nodes (root dir, sub dir, 2 files), got {checked}"
    );
    Ok(())
}

/// Tiny walk helper for tests — yields `root` itself and every
/// descendant path. Symlinks are NOT followed (so a symlink to a
/// directory is yielded as a single node, not traversed into).
fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![root.to_path_buf()];
    if root.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        for entry in std::fs::read_dir(root).unwrap() {
            out.extend(walkdir(&entry.unwrap().path()));
        }
    }
    out
}

/// A pre-existing symlink at `dest` must make restore FAIL, not be
/// followed (CVE-2021-31566 class). `restore_path_streaming`'s contract
/// is "`dest` must NOT exist"; before the fd-based metadata fix,
/// `File::create(dest)` followed a planted symlink and clobbered the
/// target — content AND the path-based `set_permissions`/mtime writes
/// all landed on the victim. With `File::create_new` the open is
/// `O_EXCL` and never follows; the victim stays untouched.
#[test]
fn restore_streaming_refuses_symlink_at_dest_file() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new()?;
    let victim = tmp.path().join("victim.txt");
    std::fs::write(&victim, "secret")?;
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600))?;
    let dest = tmp.path().join("dest");
    std::os::unix::fs::symlink(&victim, &dest)?;

    // Executable regular-file NAR: exercises both the content write and
    // the 0o755 chmod — neither may reach `victim` through the symlink.
    let mut nar = Vec::new();
    serialize(&mut nar, &reg(true, b"pwned"))?;

    let res = restore_path_streaming(&mut Cursor::new(&nar), &dest);
    assert!(
        res.is_err(),
        "restore onto a pre-existing symlink must fail, got {res:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&victim)?,
        "secret",
        "symlink at dest was followed: victim content clobbered"
    );
    assert_eq!(
        std::fs::metadata(&victim)?.permissions().mode() & 0o777,
        0o600,
        "symlink at dest was followed: victim permissions changed"
    );
    Ok(())
}

/// Directory variant of the symlink-at-dest guard: a planted symlink to
/// a victim directory must fail the restore (`mkdir` returns `EEXIST`
/// on a symlink regardless of its target) — nothing may be created
/// inside the victim and its metadata must stay untouched.
#[test]
fn restore_streaming_refuses_symlink_at_dest_dir() -> anyhow::Result<()> {
    let tmp = tempfile::TempDir::new()?;
    let victim = tmp.path().join("victim-dir");
    std::fs::create_dir(&victim)?;
    let victim_mtime = std::fs::metadata(&victim)?.modified()?;
    let dest = tmp.path().join("dest");
    std::os::unix::fs::symlink(&victim, &dest)?;

    let mut nar = Vec::new();
    serialize(
        &mut nar,
        &NarNode::Directory {
            entries: vec![entry("planted.txt", reg(false, b"pwned"))],
        },
    )?;

    let res = restore_path_streaming(&mut Cursor::new(&nar), &dest);
    assert!(
        res.is_err(),
        "restore onto a symlinked directory must fail, got {res:?}"
    );
    assert!(
        std::fs::read_dir(&victim)?.next().is_none(),
        "symlink at dest was followed: entry created inside victim dir"
    );
    assert_eq!(
        std::fs::metadata(&victim)?.modified()?,
        victim_mtime,
        "symlink at dest was followed: victim dir mtime canonicalized"
    );
    Ok(())
}

/// Same path-traversal guard as `parse`: `..`, `/`, NUL, empty, `.`
/// in entry names are rejected BEFORE any filesystem write under
/// `dest` for that name.
// r[verify builder.nar.entry-name-safety]
#[test]
fn restore_streaming_rejects_bad_entry_names() {
    for bad in [&b".."[..], b"etc/passwd", b"/etc/passwd", b"foo\0bar", b""] {
        let nar = nar_with_entry_name(bad);
        let dst_dir = tempfile::TempDir::new().unwrap();
        let dst = dst_dir.path().join("out");
        let err = restore_path_streaming(&mut Cursor::new(&nar), &dst).unwrap_err();
        assert!(
            matches!(err, NarError::InvalidEntryName { .. }),
            "expected InvalidEntryName for {bad:?}, got {err:?}"
        );
    }
}

/// Truncated NAR (EOF mid-file-contents) → typed UnexpectedEof, not
/// a hung read or a short file silently written.
#[test]
fn restore_streaming_truncated_file_fails() {
    let mut nar = Vec::new();
    for t in &[NAR_MAGIC, "(", "type", "regular", "contents"] {
        write_str(&mut nar, t).unwrap();
    }
    write_u64(&mut nar, 100).unwrap();
    nar.extend_from_slice(&[0u8; 40]); // only 40 of 100 bytes

    let dst_dir = tempfile::TempDir::new().unwrap();
    let dst = dst_dir.path().join("out");
    let err = restore_path_streaming(&mut Cursor::new(&nar), &dst).unwrap_err();
    assert!(
        matches!(&err, NarError::Io(e) if e.kind() == io::ErrorKind::UnexpectedEof),
        "expected UnexpectedEof, got {err:?}"
    );
}

// ------------------------------------------------------------------
// Parser safety-bound tests: each MAX_* limit must return a typed
// error BEFORE allocating the oversized buffer. These tests write
// only the length prefix (not the actual oversized payload) — the
// check fires on the u64 read, well before read_padded_bytes.
// ------------------------------------------------------------------

/// Helper: build a NAR byte sequence from string tokens + an
/// oversized-length suffix. Used for bounds tests — tokens are
/// written normally, then a raw u64 > limit is appended.
fn nar_bytes_with_oversized_len(tokens: &[&str], oversized_len: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    for t in tokens {
        write_str(&mut buf, t).unwrap();
    }
    write_u64(&mut buf, oversized_len).unwrap();
    buf
}

#[test]
fn parse_content_too_large_rejected() {
    // ( type regular contents <len=MAX+1> — error before body read.
    let buf = nar_bytes_with_oversized_len(
        &[NAR_MAGIC, "(", "type", "regular", "contents"],
        MAX_CONTENT_SIZE + 1,
    );
    let err = parse(&mut Cursor::new(&buf)).unwrap_err();
    assert!(
        matches!(err, NarError::ContentTooLarge(n) if n == MAX_CONTENT_SIZE + 1),
        "expected ContentTooLarge, got {err:?}"
    );
}

#[test]
fn parse_name_too_long_rejected() {
    // ( type directory entry ( name <len=MAX+1>
    let buf = nar_bytes_with_oversized_len(
        &[NAR_MAGIC, "(", "type", "directory", "entry", "(", "name"],
        MAX_NAME_LEN + 1,
    );
    let err = parse(&mut Cursor::new(&buf)).unwrap_err();
    assert!(
        matches!(err, NarError::NameTooLong(n) if n == MAX_NAME_LEN + 1),
        "expected NameTooLong, got {err:?}"
    );
}

#[test]
fn parse_symlink_target_too_long_rejected() {
    // ( type symlink target <len=MAX+1>
    let buf = nar_bytes_with_oversized_len(
        &[NAR_MAGIC, "(", "type", "symlink", "target"],
        MAX_TARGET_LEN + 1,
    );
    let err = parse(&mut Cursor::new(&buf)).unwrap_err();
    assert!(
        matches!(err, NarError::TargetTooLong(n) if n == MAX_TARGET_LEN + 1),
        "expected TargetTooLong, got {err:?}"
    );
}

#[test]
fn parse_non_utf8_token_rejected() {
    // The first token after magic is expect_str("(") — inject
    // non-UTF-8 bytes where "(" should be. read_string's
    // from_utf8 → UnexpectedToken.
    let mut buf = Vec::new();
    write_str(&mut buf, NAR_MAGIC).unwrap();
    write_u64(&mut buf, 3).unwrap();
    buf.extend_from_slice(&[0xff, 0xfe, 0xfd]); // invalid UTF-8
    buf.extend_from_slice(&[0u8; 5]); // pad to 8

    let err = parse(&mut Cursor::new(&buf)).unwrap_err();
    assert!(
        matches!(
            &err,
            NarError::InvalidUtf8 {
                context: "token",
                offset: 0,
                ..
            }
        ),
        "expected InvalidUtf8 token error, got {err:?}"
    );
}

#[test]
fn parse_rejects_nonzero_padding() {
    // Hand-craft: magic (13b → 3 pad), then "(" (1b → 7 pad with junk).
    let mut buf = Vec::new();
    write_str(&mut buf, NAR_MAGIC).unwrap();
    // u64(1) + b"(" + 7 pad bytes with one non-zero
    buf.extend_from_slice(&1u64.to_le_bytes());
    buf.push(b'(');
    buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 1]);
    let result = parse(&mut Cursor::new(&buf));
    assert!(matches!(result, Err(NarError::NonZeroPadding(1))));
}

#[test]
fn parse_rejects_nonempty_executable_marker() {
    let mut buf = Vec::new();
    for tok in [NAR_MAGIC, "(", "type", "regular", "executable"] {
        write_str(&mut buf, tok).unwrap();
    }
    // The spec says the executable marker carries an empty string;
    // a non-empty value is a token-stream desync.
    write_str(&mut buf, "junk").unwrap();
    write_str(&mut buf, "contents").unwrap();
    write_bytes(&mut buf, b"hi").unwrap();
    write_str(&mut buf, ")").unwrap();
    let result = parse(&mut Cursor::new(&buf));
    assert!(matches!(
        result,
        Err(NarError::UnexpectedToken { expected, got })
            if expected.is_empty() && got == "junk"
    ));
}

#[test]
fn parse_regular_unexpected_token_rejected() {
    // ( type regular <garbage-token> — not executable/contents/)
    let mut buf = Vec::new();
    for t in &[NAR_MAGIC, "(", "type", "regular", "garbage"] {
        write_str(&mut buf, t).unwrap();
    }
    let err = parse(&mut Cursor::new(&buf)).unwrap_err();
    assert!(
        matches!(&err, NarError::UnexpectedToken { got, .. } if got == "garbage"),
        "expected UnexpectedToken, got {err:?}"
    );
}

#[test]
fn parse_directory_unexpected_token_rejected() {
    // ( type directory <garbage-token> — not entry/)
    let mut buf = Vec::new();
    for t in &[NAR_MAGIC, "(", "type", "directory", "nonsense"] {
        write_str(&mut buf, t).unwrap();
    }
    let err = parse(&mut Cursor::new(&buf)).unwrap_err();
    assert!(
        matches!(&err, NarError::UnexpectedToken { got, .. } if got == "nonsense"),
        "expected UnexpectedToken, got {err:?}"
    );
}

// ------------------------------------------------------------------
// r[verify builder.nar.entry-name-safety]
// Path-traversal guard: parse_directory rejects dangerous entry
// names before any filesystem call. Each test hand-crafts a NAR
// directory with a single bad entry name and asserts InvalidEntryName.
// ------------------------------------------------------------------

/// Build NAR bytes for a directory with one entry of the given
/// name (as raw bytes — lets tests inject NUL). The entry's node
/// is a trivial regular file. The name is the only thing that
/// varies between the rejection tests.
fn nar_with_entry_name(name: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    for t in &[NAR_MAGIC, "(", "type", "directory", "entry", "(", "name"] {
        write_str(&mut buf, t).unwrap();
    }
    write_bytes(&mut buf, name).unwrap();
    for t in &["node", "(", "type", "regular", "contents"] {
        write_str(&mut buf, t).unwrap();
    }
    write_bytes(&mut buf, b"x").unwrap();
    for t in &[")", ")", ")"] {
        write_str(&mut buf, t).unwrap();
    }
    buf
}

#[rstest]
#[case::dotdot(b"..")]
#[case::slash(b"etc/passwd")]
#[case::absolute(b"/etc/passwd")]
#[case::nul(b"foo\0bar")]
#[case::empty(b"")]
#[case::dot(b".")]
fn test_parse_rejects_unsafe_entry_name(#[case] name: &[u8]) {
    let buf = nar_with_entry_name(name);
    let err = parse(&mut Cursor::new(&buf)).unwrap_err();
    // All test names here are valid UTF-8 (including NUL); the
    // separate non-UTF-8 case asserts InvalidUtf8, not InvalidEntryName.
    let expected = std::str::from_utf8(name).unwrap();
    assert!(
        matches!(&err, NarError::InvalidEntryName { name: n } if n == expected),
        "expected InvalidEntryName for {expected:?}, got {err:?}"
    );
}

/// Safe names round-trip through extract_to_path unchanged.
#[test]
fn test_extract_safe_names_round_trip() -> anyhow::Result<()> {
    let node = NarNode::Directory {
        entries: vec![
            NarEntry {
                name: "a.b.c".to_string(),
                node: NarNode::Regular {
                    executable: false,
                    contents: b"dots ok".to_vec(),
                },
            },
            NarEntry {
                name: "bar-baz".to_string(),
                node: NarNode::Regular {
                    executable: false,
                    contents: b"dash ok".to_vec(),
                },
            },
            NarEntry {
                name: "foo".to_string(),
                node: NarNode::Regular {
                    executable: false,
                    contents: b"plain ok".to_vec(),
                },
            },
        ],
    };

    let mut buf = Vec::new();
    serialize(&mut buf, &node)?;
    let parsed = parse(&mut Cursor::new(&buf))?;
    assert_eq!(parsed, node);

    let dst = tempfile::TempDir::new()?;
    let root = dst.path().join("extracted");
    extract_to_path(&parsed, &root)?;

    assert_eq!(std::fs::read(root.join("foo"))?, b"plain ok");
    assert_eq!(std::fs::read(root.join("bar-baz"))?, b"dash ok");
    assert_eq!(std::fs::read(root.join("a.b.c"))?, b"dots ok");
    Ok(())
}

#[test]
fn parse_entry_name_non_utf8_rejected() {
    // ( type directory entry ( name <non-utf8-bytes>
    let mut buf = Vec::new();
    for t in &[NAR_MAGIC, "(", "type", "directory", "entry", "(", "name"] {
        write_str(&mut buf, t).unwrap();
    }
    write_u64(&mut buf, 2).unwrap();
    buf.extend_from_slice(&[0xff, 0xfe]); // invalid UTF-8
    buf.extend_from_slice(&[0u8; 6]); // pad to 8

    let err = parse(&mut Cursor::new(&buf)).unwrap_err();
    assert!(
        matches!(
            &err,
            NarError::InvalidUtf8 {
                context: "entry name",
                offset: 0,
                ..
            }
        ),
        "expected InvalidUtf8 entry name error, got {err:?}"
    );
}

#[test]
fn parse_symlink_target_non_utf8_rejected() {
    // ( type symlink target <non-utf8-bytes>
    let mut buf = Vec::new();
    for t in &[NAR_MAGIC, "(", "type", "symlink", "target"] {
        write_str(&mut buf, t).unwrap();
    }
    write_u64(&mut buf, 2).unwrap();
    buf.extend_from_slice(&[0xff, 0xfe]);
    buf.extend_from_slice(&[0u8; 6]);

    let err = parse(&mut Cursor::new(&buf)).unwrap_err();
    assert!(
        matches!(
            &err,
            NarError::InvalidUtf8 {
                context: "symlink target",
                offset: 0,
                ..
            }
        ),
        "expected InvalidUtf8 symlink target error, got {err:?}"
    );
}

// ------------------------------------------------------------------
// dump_path error paths: non-UTF-8 filesystem names/targets.
// Linux allows arbitrary bytes in filenames — NAR format requires
// UTF-8 strings. These must fail loud, not silently mangle.
// ------------------------------------------------------------------

#[test]
fn dump_path_non_utf8_dir_entry_name_rejected() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::TempDir::new().unwrap();

    // Create a file with a non-UTF-8 name inside the dir.
    let bad_name = std::ffi::OsStr::from_bytes(&[0xff, 0xfe, b'x']);
    let bad_path = dir.path().join(bad_name);
    std::fs::write(&bad_path, b"content").unwrap();

    // Both eager and streaming must reject.
    let err = dump_path(dir.path()).unwrap_err();
    assert!(
        matches!(&err, NarError::Io(e) if e.to_string().contains("not valid UTF-8")),
        "dump_path: expected UTF-8 error, got {err:?}"
    );

    let mut sink = Vec::new();
    let err = dump_path_streaming(dir.path(), &mut sink).unwrap_err();
    assert!(
        matches!(&err, NarError::Io(e) if e.to_string().contains("not valid UTF-8")),
        "dump_path_streaming: expected UTF-8 error, got {err:?}"
    );
}

#[test]
fn dump_path_non_utf8_symlink_target_rejected() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::TempDir::new().unwrap();
    let link_path = dir.path().join("badlink");

    // Symlink to a non-UTF-8 target.
    let bad_target = std::ffi::OsStr::from_bytes(&[0xff, 0xfe, b'/', b't']);
    std::os::unix::fs::symlink(bad_target, &link_path).unwrap();

    let err = dump_path(&link_path).unwrap_err();
    assert!(
        matches!(&err, NarError::Io(e) if e.to_string().contains("not valid UTF-8")),
        "dump_path: expected UTF-8 error, got {err:?}"
    );

    let mut sink = Vec::new();
    let err = dump_path_streaming(&link_path, &mut sink).unwrap_err();
    assert!(
        matches!(&err, NarError::Io(e) if e.to_string().contains("not valid UTF-8")),
        "dump_path_streaming: expected UTF-8 error, got {err:?}"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// nar_ls (P0546) — streaming index
// ───────────────────────────────────────────────────────────────────────────

mod ls_tests {
    use super::*;
    use proptest::prelude::*;
    use std::io;

    /// Walk a `NarNode` tree collecting the same `(path, kind)` set
    /// `nar_ls` should emit, plus a `path → contents` map for the
    /// regular files. Used as the proptest oracle.
    fn collect_files(node: &NarNode, path: &mut Vec<u8>, out: &mut Vec<(Vec<u8>, Vec<u8>)>) {
        match node {
            NarNode::Regular { contents, .. } => {
                out.push((path.clone(), contents.clone()));
            }
            NarNode::Symlink { .. } => {}
            NarNode::Directory { entries } => {
                for e in entries {
                    let saved = path.len();
                    if !path.is_empty() {
                        path.push(b'/');
                    }
                    path.extend_from_slice(e.name.as_bytes());
                    collect_files(&e.node, path, out);
                    path.truncate(saved);
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]
        // r[verify store.index.nar-ls-offset]
        // r[verify store.index.file-digest]
        #[test]
        fn nar_ls_offset_and_digest(node in super::proptests::arb_nar_node()) {
            let mut buf = Vec::new();
            serialize(&mut buf, &node)?;

            let entries = nar_ls(Cursor::new(&buf))?;

            let mut want_files = Vec::new();
            collect_files(&node, &mut Vec::new(), &mut want_files);
            let mut want: std::collections::HashMap<Vec<u8>, Vec<u8>> =
                want_files.into_iter().collect();

            for e in &entries {
                if e.kind != NarEntryKind::Regular {
                    prop_assert_eq!(e.size, 0);
                    prop_assert_eq!(e.nar_offset, 0);
                    prop_assert_eq!(e.file_digest, [0u8; 32]);
                    continue;
                }
                let content = want.remove(&e.path).expect("unknown regular path");
                prop_assert_eq!(e.size as usize, content.len());
                // nar_offset → slice of the original NAR == content
                let slice = &buf[e.nar_offset as usize..e.nar_offset as usize + content.len()];
                prop_assert_eq!(slice, &content[..]);
                // file_digest == blake3(content)
                prop_assert_eq!(e.file_digest, *blake3::hash(&content).as_bytes());
            }
            prop_assert!(want.is_empty(), "nar_ls missed regular files: {:?}", want.keys());
        }
    }

    // `nar_ls` accepts a strict superset of what `parse()` accepts —
    // it additionally takes (a) regular-file content past
    // `MAX_CONTENT_SIZE` (streamed, never buffered) and (b) non-UTF-8
    // entry names / symlink targets (raw `Vec<u8>` API; `parse()`'s
    // `String`-based `NarNode` cannot represent them). Random byte
    // vectors essentially never form a valid NAR (24-byte magic
    // prefix), so this proptest only exercises the both-reject path —
    // the agree-on-valid-input direction is `nar_ls_offset_and_digest`
    // above; the deliberate divergences are pinned by hand below and
    // exercised continuously by the `nar_ls` fuzz target.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]
        #[test]
        fn nar_ls_superset_of_parse(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
            let parse_ok = parse(&mut Cursor::new(&bytes)).is_ok();
            let ls_ok = nar_ls(Cursor::new(&bytes)).is_ok();
            prop_assert!(!parse_ok || ls_ok,
                "parse accepted but nar_ls rejected: {:?}", bytes);
        }
    }

    /// `parse()` rejects non-UTF-8 entry names and symlink targets
    /// because its `String`-based `NarNode` cannot represent them.
    /// `nar_ls()` is byte-faithful (`Vec<u8>` path/target) — `dump_path`
    /// over a Unix filesystem can legitimately produce arbitrary-byte
    /// names. Pins the deliberate divergence the `nar_ls` fuzz target
    /// special-cases as an `(Err(InvalidUtf8), Ok(_))` arm.
    #[test]
    fn nar_ls_accepts_non_utf8_names_parse_rejects() {
        // Hand-encode: dir { entry "\xFF" → symlink "\xFE" }.
        // serialize() can't produce this — NarEntry.name is String.
        let mut buf = Vec::new();
        write_str(&mut buf, NAR_MAGIC).unwrap();
        write_str(&mut buf, "(").unwrap();
        write_str(&mut buf, "type").unwrap();
        write_str(&mut buf, "directory").unwrap();
        write_str(&mut buf, "entry").unwrap();
        write_str(&mut buf, "(").unwrap();
        write_str(&mut buf, "name").unwrap();
        write_bytes(&mut buf, &[0xFF]).unwrap();
        write_str(&mut buf, "node").unwrap();
        write_str(&mut buf, "(").unwrap();
        write_str(&mut buf, "type").unwrap();
        write_str(&mut buf, "symlink").unwrap();
        write_str(&mut buf, "target").unwrap();
        write_bytes(&mut buf, &[0xFE]).unwrap();
        write_str(&mut buf, ")").unwrap();
        write_str(&mut buf, ")").unwrap();
        write_str(&mut buf, ")").unwrap();

        assert!(matches!(
            parse(&mut Cursor::new(&buf)),
            Err(NarError::InvalidUtf8 { .. })
        ));

        let entries = nar_ls(Cursor::new(&buf)).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, NarEntryKind::Directory);
        assert_eq!(entries[1].path, vec![0xFF]);
        assert_eq!(entries[1].kind, NarEntryKind::Symlink);
        assert_eq!(entries[1].target, vec![0xFE]);
    }

    /// A `Read` that delivers `len` repeated bytes then EOF, never
    /// allocating. Used to prove `nar_ls` holds bounded memory: the
    /// content here is far past `MAX_CONTENT_SIZE`, so a `parse()`-style
    /// buffer-whole impl would either OOM or reject it.
    struct RepeatReader {
        byte: u8,
        remaining: u64,
    }
    impl Read for RepeatReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = (self.remaining.min(buf.len() as u64)) as usize;
            buf[..n].fill(self.byte);
            self.remaining -= n as u64;
            Ok(n)
        }
    }

    /// Hand-written NAR header for a single regular file of arbitrary
    /// size, content supplied by a chained reader. `serialize()` would
    /// need the bytes in a `Vec`.
    fn synthetic_regular_header(content_len: u64) -> Vec<u8> {
        let mut h = Vec::new();
        write_str(&mut h, NAR_MAGIC).unwrap();
        write_str(&mut h, "(").unwrap();
        write_str(&mut h, "type").unwrap();
        write_str(&mut h, "regular").unwrap();
        write_str(&mut h, "contents").unwrap();
        write_u64(&mut h, content_len).unwrap();
        h
    }

    fn synthetic_regular_trailer(content_len: u64) -> Vec<u8> {
        let mut t = Vec::new();
        let pad = crate::protocol::wire::padding_len(content_len as usize);
        t.extend_from_slice(&vec![0u8; pad]);
        write_str(&mut t, ")").unwrap();
        t
    }

    // r[verify store.index.nar-ls-streaming]
    #[test]
    fn nar_ls_bounded_memory_past_max_content_size() {
        // 4× MAX_CONTENT_SIZE — parse() would reject it (ContentTooLarge);
        // nar_ls streams it in 64 KiB blocks. RepeatReader allocates
        // nothing, so this test going OOM means nar_ls buffers.
        let len = MAX_CONTENT_SIZE * 4;
        let header = synthetic_regular_header(len);
        let nar_offset = header.len() as u64;
        let trailer = synthetic_regular_trailer(len);

        let r = Cursor::new(header)
            .chain(RepeatReader {
                byte: 0xAB,
                remaining: len,
            })
            .chain(Cursor::new(trailer));

        let entries = nar_ls(r).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, NarEntryKind::Regular);
        assert_eq!(entries[0].size, len);
        assert_eq!(entries[0].nar_offset, nar_offset);

        // Compute expected blake3 incrementally (same block size).
        let mut h = blake3::Hasher::new();
        let block = vec![0xABu8; 64 * 1024];
        let mut rem = len;
        while rem > 0 {
            let take = rem.min(block.len() as u64) as usize;
            h.update(&block[..take]);
            rem -= take as u64;
        }
        assert_eq!(entries[0].file_digest, *h.finalize().as_bytes());
    }

    /// Hand-encoded — `serialize()` can't emit these names.
    #[rstest]
    #[case::empty(b"")]
    #[case::dot(b".")]
    #[case::dotdot(b"..")]
    #[case::slash(b"a/b")]
    #[case::nul(b"a\0b")]
    fn nar_ls_rejects_invalid_entry_names(#[case] name: &[u8]) {
        let mut buf = Vec::new();
        write_str(&mut buf, NAR_MAGIC).unwrap();
        write_str(&mut buf, "(").unwrap();
        write_str(&mut buf, "type").unwrap();
        write_str(&mut buf, "directory").unwrap();
        write_str(&mut buf, "entry").unwrap();
        write_str(&mut buf, "(").unwrap();
        write_str(&mut buf, "name").unwrap();
        write_bytes(&mut buf, name).unwrap();
        write_str(&mut buf, "node").unwrap();
        write_str(&mut buf, "(").unwrap();
        write_str(&mut buf, "type").unwrap();
        write_str(&mut buf, "symlink").unwrap();
        write_str(&mut buf, "target").unwrap();
        write_str(&mut buf, "x").unwrap();
        write_str(&mut buf, ")").unwrap();
        write_str(&mut buf, ")").unwrap();
        write_str(&mut buf, ")").unwrap();

        assert!(matches!(
            nar_ls(Cursor::new(&buf)),
            Err(NarError::InvalidEntryName { .. })
        ));
    }

    /// Mirrors `reject_deeply_nested_nar` for `nar_ls`.
    #[test]
    fn nar_ls_rejects_too_deep_nesting() {
        let mut node = reg(false, b"");
        for _ in 0..(MAX_NAR_DEPTH + 1) {
            node = NarNode::Directory {
                entries: vec![entry("a", node)],
            };
        }
        let mut buf = Vec::new();
        serialize(&mut buf, &node).unwrap();
        assert!(matches!(
            nar_ls(Cursor::new(&buf)),
            Err(NarError::NestingTooDeep(_))
        ));
    }

    #[test]
    fn nar_ls_accepts_at_depth_limit() {
        let mut node = reg(false, b"");
        for _ in 0..MAX_NAR_DEPTH {
            node = NarNode::Directory {
                entries: vec![entry("a", node)],
            };
        }
        let mut buf = Vec::new();
        serialize(&mut buf, &node).unwrap();
        let entries = nar_ls(Cursor::new(&buf)).unwrap();
        assert_eq!(entries.len(), MAX_NAR_DEPTH + 1); // 256 dirs + 1 leaf
    }

    /// `nar_ls` emits in NAR encounter order (DFS pre-order) so the
    /// caller's bottom-up dir_digest pass (P0572) can iterate in
    /// reverse without re-sorting.
    #[test]
    fn nar_ls_encounter_order() {
        let tree = NarNode::Directory {
            entries: vec![
                NarEntry {
                    name: "a".into(),
                    node: NarNode::Directory {
                        entries: vec![NarEntry {
                            name: "x".into(),
                            node: reg(false, b"x"),
                        }],
                    },
                },
                NarEntry {
                    name: "b".into(),
                    node: NarNode::Symlink {
                        target: "a/x".into(),
                    },
                },
            ],
        };
        let mut buf = Vec::new();
        serialize(&mut buf, &tree).unwrap();
        let entries = nar_ls(Cursor::new(&buf)).unwrap();
        let paths: Vec<&[u8]> = entries.iter().map(|e| e.path.as_slice()).collect();
        assert_eq!(paths, vec![&b""[..], b"a", b"a/x", b"b"]);
    }

    // ── Malformed-input rejection ──────────────────────────────────────
    // `nar_ls` reimplements the token walk (it streams instead of
    // buffering), so the `parse()` rejection tests above do not cover
    // its error returns.

    /// Encode a token sequence after the NAR magic. `serialize()` can
    /// only emit well-formed NARs; every rejection case needs raw
    /// tokens.
    fn nar_tokens(tokens: &[&str]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_str(&mut buf, NAR_MAGIC).unwrap();
        for t in tokens {
            write_str(&mut buf, t).unwrap();
        }
        buf
    }

    #[test]
    fn nar_ls_rejects_invalid_magic() {
        let mut buf = Vec::new();
        write_str(&mut buf, "not-nar-magic").unwrap();
        assert!(matches!(
            nar_ls(Cursor::new(&buf)),
            Err(NarError::InvalidMagic(_))
        ));
    }

    #[test]
    fn nar_ls_rejects_unknown_node_type() {
        let buf = nar_tokens(&["(", "type", "fifo"]);
        assert!(matches!(
            nar_ls(Cursor::new(&buf)),
            Err(NarError::UnknownNodeType(ref t)) if t == "fifo"
        ));
    }

    /// `expect_token` mismatch — the other UnexpectedToken cases below
    /// hit explicit match arms, not the generic helper.
    #[test]
    fn nar_ls_rejects_missing_open_paren() {
        let buf = nar_tokens(&["garbage"]);
        assert!(matches!(
            nar_ls(Cursor::new(&buf)),
            Err(NarError::UnexpectedToken { ref got, .. }) if got == "garbage"
        ));
    }

    #[test]
    fn nar_ls_rejects_regular_without_contents() {
        let buf = nar_tokens(&["(", "type", "regular", "garbage"]);
        assert!(matches!(
            nar_ls(Cursor::new(&buf)),
            Err(NarError::UnexpectedToken { ref got, .. }) if got == "garbage"
        ));
    }

    #[test]
    fn nar_ls_rejects_unknown_directory_token() {
        let buf = nar_tokens(&["(", "type", "directory", "nonsense"]);
        assert!(matches!(
            nar_ls(Cursor::new(&buf)),
            Err(NarError::UnexpectedToken { ref got, .. }) if got == "nonsense"
        ));
    }

    #[test]
    fn nar_ls_rejects_unsorted_entries() {
        #[rustfmt::skip]
        let buf = nar_tokens(&[
            "(", "type", "directory",
            "entry", "(", "name", "z", "node",
                "(", "type", "symlink", "target", "t", ")",
            ")",
            "entry", "(", "name", "a", "node",
                "(", "type", "symlink", "target", "t", ")",
            ")",
            ")",
        ]);
        assert!(matches!(
            nar_ls(Cursor::new(&buf)),
            Err(NarError::UnsortedEntries { ref prev, ref cur }) if prev == "z" && cur == "a"
        ));
    }

    // ── Whole-archive bounds (ingest-limits contract) ──────────────────
    // Per-axis limits (per-directory entries, depth, name length) do
    // not bound the archive as a whole; the store sizes its index
    // materialization and its GetPath regeneration walk for these
    // totals, so `nar_ls` must reject anything past them instead of
    // materializing it.

    /// `Read` over lazily-generated byte segments — synthesizes NARs
    /// with ~1M entries without materializing the archive bytes.
    struct SegmentReader<I: Iterator<Item = Vec<u8>>> {
        segments: I,
        current: Vec<u8>,
        pos: usize,
    }

    impl<I: Iterator<Item = Vec<u8>>> Read for SegmentReader<I> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                if self.pos < self.current.len() {
                    let n = (self.current.len() - self.pos).min(buf.len());
                    buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
                    self.pos += n;
                    return Ok(n);
                }
                match self.segments.next() {
                    Some(s) => {
                        self.current = s;
                        self.pos = 0;
                    }
                    None => return Ok(0),
                }
            }
        }
    }

    /// One `entry(name -> symlink target)` token run.
    fn symlink_entry_segment(name: &[u8], target: &[u8]) -> Vec<u8> {
        let mut s = Vec::new();
        for t in ["entry", "(", "name"] {
            write_str(&mut s, t).unwrap();
        }
        write_bytes(&mut s, name).unwrap();
        for t in ["node", "(", "type", "symlink", "target"] {
            write_str(&mut s, t).unwrap();
        }
        write_bytes(&mut s, target).unwrap();
        for t in [")", ")"] {
            write_str(&mut s, t).unwrap();
        }
        s
    }

    fn tokens_segment(tokens: &[&str]) -> Vec<u8> {
        let mut s = Vec::new();
        for t in tokens {
            write_str(&mut s, t).unwrap();
        }
        s
    }

    /// `nar_ls` over lazily-generated segments.
    fn nar_ls_segments(segments: impl Iterator<Item = Vec<u8>>) -> Result<Vec<NarLsEntry>> {
        nar_ls(SegmentReader {
            segments,
            current: Vec::new(),
            pos: 0,
        })
    }

    /// The `nix-archive-1 ( type directory` opener shared by the
    /// segment-built fixtures.
    fn directory_root_header() -> Vec<u8> {
        let mut h = Vec::new();
        write_str(&mut h, NAR_MAGIC).unwrap();
        for t in ["(", "type", "directory"] {
            write_str(&mut h, t).unwrap();
        }
        h
    }

    /// Lazy NAR segments for a root directory holding two
    /// subdirectories `a` and `b` with `a_count` / `b_count` symlink
    /// entries (target `"t"`) respectively. Total archive entries =
    /// 3 (root + the two subdirs) + `a_count` + `b_count`. Splitting
    /// across two subdirectories keeps both under the per-directory
    /// cap, so only the whole-archive total is exercised.
    fn two_subdir_nar_segments(a_count: usize, b_count: usize) -> impl Iterator<Item = Vec<u8>> {
        let subdir_open = |name: &str| {
            tokens_segment(&["entry", "(", "name", name, "node", "(", "type", "directory"])
        };
        // close the inner directory node, then the entry.
        let subdir_close = || tokens_segment(&[")", ")"]);
        let entries =
            |n: usize| (0..n).map(|i| symlink_entry_segment(format!("{i:07}").as_bytes(), b"t"));

        std::iter::once(directory_root_header())
            .chain(std::iter::once(subdir_open("a")))
            .chain(entries(a_count))
            .chain(std::iter::once(subdir_close()))
            .chain(std::iter::once(subdir_open("b")))
            .chain(entries(b_count))
            .chain(std::iter::once(subdir_close()))
            .chain(std::iter::once(tokens_segment(&[")"])))
    }

    /// Whole-archive entry count is bounded. A NAR of millions of
    /// directories/symlinks stays under MAX_NAR_SIZE and under every
    /// per-directory/depth cap, but the store's GetPath regeneration
    /// walk and index materialization are sized for [`MAX_NAR_ENTRIES`]
    /// — without the total cap such a path commits as 'complete' yet
    /// can never be served (bug_011). Two subdirectories each hold half
    /// the entries, so the per-directory cap is NOT what fires.
    // r[verify store.ingest.tree-bounds+2]
    #[test]
    fn nar_ls_rejects_more_total_entries_than_max() {
        // Total entries = root + 2 subdirs + 2 × per_subdir symlinks
        //               = MAX_NAR_ENTRIES + 3.
        let per_subdir = MAX_NAR_ENTRIES / 2;
        let result = nar_ls_segments(two_subdir_nar_segments(per_subdir, per_subdir));
        // Don't debug-print the Ok case — it would be a million entries.
        let outcome = result.as_ref().map(Vec::len);
        assert!(
            matches!(result, Err(NarError::TooManyTotalEntries(_))),
            "expected TooManyTotalEntries, got {outcome:?}"
        );
    }

    /// Exactly [`MAX_NAR_ENTRIES`] total entries is accepted: the cap
    /// is "more than", not "at least", so the largest archive the store
    /// sizes its index materialization and regeneration walk for must
    /// still parse. Same lazy two-subdirectory fixture as the
    /// over-the-cap test above, three entries fewer.
    // r[verify store.ingest.tree-bounds+2]
    #[test]
    fn nar_ls_accepts_exactly_max_total_entries() {
        // root + 2 subdirs + symlinks = MAX_NAR_ENTRIES exactly.
        let symlinks = MAX_NAR_ENTRIES - 3;
        let entries = nar_ls_segments(two_subdir_nar_segments(
            symlinks / 2,
            symlinks - symlinks / 2,
        ))
        .expect("an archive with exactly MAX_NAR_ENTRIES entries must parse");
        assert_eq!(entries.len(), MAX_NAR_ENTRIES);
    }

    /// Cumulative materialized index bytes (joined paths + symlink
    /// targets) are bounded. With 255-byte names at maximum nesting a
    /// single leaf's joined path is ~64 KiB while its NAR framing is
    /// ~200 bytes — a sub-MB NAR legally expands to hundreds of MB (and
    /// a few-hundred-MB NAR to tens of GB) of index entries in every
    /// ingest path (bug_012). The cap rejects the expansion instead of
    /// allocating it.
    // r[verify store.ingest.tree-bounds+2]
    #[test]
    fn nar_ls_rejects_cumulative_index_bytes_over_max() {
        // Spine: MAX_NAR_DEPTH - 1 nested dirs with 255-byte names, so
        // the leaves sit exactly at the depth limit (depth itself is
        // legal — only the cumulative expansion is not).
        let spine_name = "d".repeat(255);
        // Each leaf charges its joined path (the spine components plus
        // separators plus its own 4-char name) and its 1-byte target.
        // Size the leaf count from the constant so the leaves alone
        // exceed the cap even if MAX_NAR_INDEX_BYTES is ever raised —
        // the spine directories' own path bytes are extra margin.
        let per_leaf = ((MAX_NAR_DEPTH - 1) * (spine_name.len() + 1) + 4 + 1) as u64;
        let leaf_count = (MAX_NAR_INDEX_BYTES / per_leaf + 1) as usize;
        let leaves = NarNode::Directory {
            entries: (0..leaf_count)
                .map(|i| {
                    entry(
                        &format!("{i:04}"),
                        NarNode::Symlink {
                            target: "t".to_string(),
                        },
                    )
                })
                .collect(),
        };
        let mut node = leaves;
        for _ in 0..(MAX_NAR_DEPTH - 1) {
            node = NarNode::Directory {
                entries: vec![entry(&spine_name, node)],
            };
        }
        let mut nar = Vec::new();
        serialize(&mut nar, &node).unwrap();
        assert!(
            (nar.len() as u64) < 2 * 1024 * 1024,
            "the input must stay small — the point is the ~350× expansion, \
             got {} bytes",
            nar.len()
        );

        let result = nar_ls(Cursor::new(&nar));
        // Don't debug-print the Ok case — it would be ~128 MiB of paths.
        let outcome = result.as_ref().map(Vec::len);
        assert!(
            matches!(result, Err(NarError::IndexBytesTooLarge(_))),
            "expected IndexBytesTooLarge, got {outcome:?}"
        );
    }

    /// Cumulative index bytes of exactly [`MAX_NAR_INDEX_BYTES`] are
    /// accepted: the cap is "more than", not "at least". The fixture is
    /// shaped so the sum lands exactly on the cap — the root directory
    /// charges 0 bytes (empty path, no target) and each of its symlink
    /// children charges name + target = 4096 bytes, a divisor of the
    /// cap. Lazy segments keep the ~134 MB input from being
    /// materialized; the parsed output, whose path/target bytes ARE the
    /// quantity being measured, is the irreducible allocation.
    // r[verify store.ingest.tree-bounds+2]
    #[test]
    fn nar_ls_accepts_cumulative_index_bytes_at_max() {
        const NAME_LEN: usize = 256; // = MAX_NAME_LEN
        const TARGET_LEN: usize = 4096 - NAME_LEN;
        const PER_ENTRY: u64 = (NAME_LEN + TARGET_LEN) as u64;
        assert_eq!(
            MAX_NAR_INDEX_BYTES % PER_ENTRY,
            0,
            "fixture invariant: the per-entry charge must divide the cap exactly"
        );
        let n = (MAX_NAR_INDEX_BYTES / PER_ENTRY) as usize;
        let target = vec![b't'; TARGET_LEN];
        let symlinks = (0..n).map(move |i| {
            // NAME_LEN-byte zero-padded decimal names sort byte-lexicographically.
            let mut name = vec![b'0'; NAME_LEN];
            let digits = i.to_string();
            name[NAME_LEN - digits.len()..].copy_from_slice(digits.as_bytes());
            symlink_entry_segment(&name, &target)
        });
        let segments = std::iter::once(directory_root_header())
            .chain(symlinks)
            .chain(std::iter::once(tokens_segment(&[")"])));

        let entries = nar_ls_segments(segments)
            .expect("an archive whose index bytes sit exactly at the cap must parse");
        assert_eq!(entries.len(), n + 1, "the symlinks plus the root directory");
    }
}

// -----------------------------------------------------------------------
// WalkObserver: ADR-022 §6 external-walker surface
// -----------------------------------------------------------------------

/// Records every observer callback as a flat event list.
#[derive(Default)]
struct RecordingObserver {
    events: Vec<String>,
    /// Concatenation of every `file_data` slice for the current file.
    current: Vec<u8>,
}

impl WalkObserver for RecordingObserver {
    fn enter_dir(&mut self, name: &[u8]) -> std::io::Result<()> {
        self.events
            .push(format!("dir+ {}", str::from_utf8(name).unwrap()));
        Ok(())
    }
    fn leave_dir(&mut self) -> std::io::Result<()> {
        self.events.push("dir-".into());
        Ok(())
    }
    fn symlink(&mut self, name: &[u8], target: &[u8]) -> std::io::Result<()> {
        self.events.push(format!(
            "sym {} -> {}",
            str::from_utf8(name).unwrap(),
            str::from_utf8(target).unwrap()
        ));
        Ok(())
    }
    fn file_begin(&mut self, name: &[u8], executable: bool, size: u64) -> std::io::Result<()> {
        self.events.push(format!(
            "file+ {} x={executable} size={size}",
            str::from_utf8(name).unwrap()
        ));
        self.current.clear();
        Ok(())
    }
    fn file_data(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.current.extend_from_slice(data);
        Ok(())
    }
    fn file_end(&mut self) -> std::io::Result<()> {
        self.events.push(format!(
            "file- {:?}",
            str::from_utf8(&self.current).unwrap()
        ));
        Ok(())
    }
}

/// The observer sees the tree structure in canonical NAR walk order
/// (byte-lex by name, dirs entered before their children), file
/// contents arrive complete and in order, and the NAR byte stream is
/// identical to the observer-free dump.
#[test]
fn dump_path_observed_reports_structure_and_matches_plain_dump() -> anyhow::Result<()> {
    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("root");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(root.join("b-sub"))?;
    std::fs::write(root.join("b-sub/inner"), b"nested")?;
    std::fs::write(root.join("a-file"), b"hello")?;
    std::os::unix::fs::symlink("a-file", root.join("c-link"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(root.join("d-exec"), b"#!/bin/sh\n")?;
        std::fs::set_permissions(root.join("d-exec"), std::fs::Permissions::from_mode(0o755))?;
    }

    let mut plain = Vec::new();
    dump_path_streaming(&root, &mut plain)?;

    let mut observed = Vec::new();
    let mut obs = RecordingObserver::default();
    let n = dump_path_observed(&root, &mut observed, &mut obs)?;

    assert_eq!(plain, observed, "observer must not perturb the byte stream");
    assert_eq!(n, plain.len() as u64);
    assert_eq!(
        obs.events,
        vec![
            "dir+ ".to_string(), // root has no name
            "file+ a-file x=false size=5".to_string(),
            "file- \"hello\"".to_string(),
            "dir+ b-sub".to_string(),
            "file+ inner x=false size=6".to_string(),
            "file- \"nested\"".to_string(),
            "dir-".to_string(),
            "sym c-link -> a-file".to_string(),
            "file+ d-exec x=true size=10".to_string(),
            "file- \"#!/bin/sh\\n\"".to_string(),
            "dir-".to_string(),
        ]
    );
    Ok(())
}

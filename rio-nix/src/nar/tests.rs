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

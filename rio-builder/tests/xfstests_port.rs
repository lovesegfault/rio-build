//! xfstests ports — userspace tier (see `tests/xfstests_port/PLAN.md`).
//!
//! Ports from xfstests `tests/generic/` whose intent reduces to the
//! userspace contract of the castore-FUSE metadata table
//! (`castore_fuse::tree::InoMap`): readdir offset/resume bookkeeping,
//! byte-exact name lookup, kind/size/inode derivation. The kernel-visible
//! halves of the same tests (multi-batch FUSE_READDIR, ELOOP traversal,
//! access(2)/exec enforcement, passthrough reads) live in the
//! `vm-castore-xfstests` NixOS test — a real mount needs `/dev/fuse`,
//! mount privileges, and FUSE_PASSTHROUGH, none of which exist in the
//! test sandbox.

use fuser::{FileType, INodeNo};
use prost::Message;
use rio_builder::castore_fuse::tree::{InoMap, Node};
use rio_proto::castore::{Directory, DirectoryEntry, FileEntry, RootNode, SymlinkEntry, root_node};

fn dir_digest_of(d: &Directory) -> [u8; 32] {
    *blake3::hash(&d.encode_to_vec()).as_bytes()
}

fn file_entry(name: &[u8], size: u64, executable: bool) -> FileEntry {
    // Content digest derived from the name so every fixture file gets a
    // distinct digest (and therefore a distinct content-addressed
    // inode) without hand-maintaining 200 constants.
    FileEntry {
        name: name.to_vec(),
        digest: blake3::hash(name).as_bytes().to_vec(),
        size,
        executable,
    }
}

fn dir_root(store_path: &str, dir: &Directory) -> (String, RootNode) {
    (
        store_path.to_owned(),
        RootNode {
            node: Some(root_node::Node::DirDigest(dir_digest_of(dir).to_vec())),
        },
    )
}

/// Walk `path` segment by segment from the FUSE root, the same way the
/// kernel resolves a path through repeated `lookup` calls.
fn lookup_path(map: &InoMap, path: &[&[u8]]) -> Option<(u64, fuser::FileAttr)> {
    let mut cur = INodeNo::ROOT.0;
    let mut last = None;
    for seg in path {
        let hit = map.lookup(cur, seg)?;
        cur = hit.0;
        last = Some(hit);
    }
    last
}

/// generic/257 (t_dir_offset2): readdir offsets must be unique and
/// resumable. The kernel re-calls readdir with the last consumed
/// offset whenever its buffer fills mid-listing (a 200-entry directory
/// needs several FUSE_READDIR round-trips), so resuming from EVERY
/// entry's `next_offset` must yield exactly the remaining suffix —
/// a duplicated or skipped entry here is exactly the corruption class
/// t_dir_offset2 hunts. Also pins d_ino == st_ino for non-dot entries:
/// each readdir entry must carry the same content-derived inode that a
/// later `lookup`/`getattr` reports (the predecessor JIT-FUSE diverged
/// here — old finding F-2 — and tools correlating `ls -i` with
/// `stat %i` broke).
#[test]
fn generic_257_readdir_resume_exhaustive() {
    let files: Vec<FileEntry> = (1..=200)
        .map(|i| file_entry(format!("f{i}").as_bytes(), i, false))
        .collect();
    let big = Directory {
        directories: vec![],
        files,
        symlinks: vec![],
    };
    let roots = vec![dir_root("/nix/store/aaa-xfstests-dir200", &big)];
    let map = InoMap::from_parts(&roots, vec![big.clone()]).expect("build tree");

    let (dir_ino, _) = map
        .lookup(INodeNo::ROOT.0, b"aaa-xfstests-dir200")
        .expect("root resolves");

    let collect = |offset: u64| -> Vec<(Vec<u8>, u64, u64)> {
        map.readdir(dir_ino, offset)
            .expect("is a dir")
            .map(|e| (e.name.to_vec(), e.ino, e.next_offset))
            .collect()
    };

    let full = collect(0);
    // 200 children + "." + "..", each name exactly once.
    assert_eq!(full.len(), 202, "full enumeration is complete");
    let mut names: Vec<Vec<u8>> = full.iter().map(|(n, _, _)| n.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 202, "no duplicate names in the enumeration");

    // next_offset values must be strictly increasing and unique — the
    // kernel uses them verbatim as resume cookies.
    for window in full.windows(2) {
        assert!(
            window[1].2 > window[0].2,
            "next_offset not strictly increasing: {} then {}",
            window[0].2,
            window[1].2
        );
    }

    // Resuming from every entry's next_offset yields exactly the
    // remaining suffix (no dups, no skips), and a resume past the end
    // is an empty listing, not an error.
    for (i, (_, _, next_offset)) in full.iter().enumerate() {
        let resumed = collect(*next_offset);
        assert_eq!(
            resumed,
            full[i + 1..],
            "resume from offset {next_offset} (entry {i}) does not match the suffix"
        );
    }
    assert_eq!(
        collect(full.last().expect("non-empty").2),
        vec![],
        "resume past the last entry must be empty"
    );

    // d_ino == st_ino for every real entry. Dot entries are excluded:
    // readdir synthesizes them rather than resolving them via lookup,
    // and their inos are pinned by tree.rs's
    // readdir_dotdot_reports_parent_ino.
    for (name, d_ino, _) in &full {
        if name == b"." || name == b".." {
            continue;
        }
        let (st_ino, attr) = map.lookup(dir_ino, name).expect("entry resolves by lookup");
        assert_eq!(
            *d_ino, st_ino,
            "readdir ino for {name:?} differs from lookup ino"
        );
        assert_eq!(attr.ino, INodeNo(*d_ino));
        assert_ne!(*d_ino, 0, "entry {name:?} has ino 0");
    }
}

/// generic/453: lookalike and arbitrary-byte filenames must stay
/// distinct. `lookup` is a byte-exact scan of the Directory body —
/// any normalization (NFC/NFD folding) or truncation (NAME_MAX) would
/// resolve one entry as another and serve the wrong content to a
/// build.
#[test]
fn generic_453_byte_exact_names() {
    let nfc = "caf\u{00e9}".as_bytes(); // c3 a9
    let nfd = "cafe\u{0301}".as_bytes(); // 65 cc 81
    let space = b"a b".as_slice();
    let long = vec![b'n'; 255];
    let almost = vec![b'n'; 254];

    let dir = Directory {
        directories: vec![],
        files: vec![
            file_entry(nfc, 11, false),
            file_entry(nfd, 12, false),
            file_entry(space, 13, false),
            file_entry(&long, 14, false),
            file_entry(&almost, 15, false),
        ],
        symlinks: vec![],
    };
    let roots = vec![dir_root("/nix/store/bbb-xfstests-names", &dir)];
    let map = InoMap::from_parts(&roots, vec![dir.clone()]).expect("build tree");
    let (dir_ino, _) = map
        .lookup(INodeNo::ROOT.0, b"bbb-xfstests-names")
        .expect("root resolves");

    // Every name resolves to ITS entry (sizes disambiguate), and the
    // lookalikes get distinct inodes.
    let cases: &[(&[u8], u64)] = &[
        (nfc, 11),
        (nfd, 12),
        (space, 13),
        (&long, 14),
        (&almost, 15),
    ];
    let mut inos = Vec::new();
    for (name, size) in cases {
        let (ino, attr) = map
            .lookup(dir_ino, name)
            .unwrap_or_else(|| panic!("lookup of {name:?} failed"));
        assert_eq!(attr.size, *size, "wrong entry resolved for {name:?}");
        inos.push(ino);
    }
    inos.sort_unstable();
    inos.dedup();
    assert_eq!(
        inos.len(),
        cases.len(),
        "lookalike names collapsed onto one inode"
    );

    // A 256-byte probe (one past NAME_MAX) and a case-folded variant
    // must miss — None becomes the cached negative entry, never a
    // wrong-entry hit.
    let too_long = vec![b'n'; 256];
    assert!(map.lookup(dir_ino, &too_long).is_none());
    assert!(map.lookup(dir_ino, b"CAF\xc3\x89").is_none());

    // readdir enumerates all five (plus the dot entries), byte-exact.
    let listed: Vec<Vec<u8>> = map
        .readdir(dir_ino, 0)
        .expect("is a dir")
        .map(|e| e.name.to_vec())
        .filter(|n| n != b"." && n != b"..")
        .collect();
    assert_eq!(listed.len(), 5, "readdir dropped a lookalike entry");
    for (name, _) in cases {
        assert!(
            listed.iter().any(|n| n == name),
            "readdir is missing {name:?}"
        );
    }
}

/// generic/360: a symlink with a very long target must report
/// size == strlen(target) and round-trip the target bytes exactly.
/// Tools size their readlink(2) buffer from st_size — a short size
/// truncates the target and sends a build to the wrong path.
#[test]
fn generic_360_symlink_size_matches_target_len() {
    let long_target = vec![b'x'; 900];
    let dir = Directory {
        directories: vec![],
        files: vec![],
        symlinks: vec![
            SymlinkEntry {
                name: b"longtarget".to_vec(),
                target: long_target.clone(),
            },
            SymlinkEntry {
                name: b"rel".to_vec(),
                target: b"../data/small.txt".to_vec(),
            },
        ],
    };
    let roots = vec![dir_root("/nix/store/ccc-xfstests-links", &dir)];
    let map = InoMap::from_parts(&roots, vec![dir]).expect("build tree");

    let (ino, attr) =
        lookup_path(&map, &[b"ccc-xfstests-links", b"longtarget"]).expect("symlink resolves");
    assert_eq!(attr.kind, FileType::Symlink);
    assert_eq!(
        attr.size, 900,
        "symlink size must equal the target length (readlink buffer sizing)"
    );
    assert_eq!(
        attr.perm, 0o777,
        "symlink mode is the Linux-immutable 0o777"
    );
    assert_eq!(
        map.node(ino),
        Some(&Node::Symlink {
            target: long_target
        }),
        "target bytes must round-trip exactly"
    );

    let (_, rel) = lookup_path(&map, &[b"ccc-xfstests-links", b"rel"]).expect("symlink resolves");
    assert_eq!(rel.size, b"../data/small.txt".len() as u64);
}

/// generic/401: the file kind reported by readdir (d_type) must match
/// the kind getattr reports for the same entry, for every node kind
/// and at both tree levels (closure roots and nested Directory
/// children). `find -type`, glob ordering, and overlayfs copy-up
/// decisions all trust d_type without a follow-up stat.
#[test]
fn generic_401_readdir_kinds_match_getattr() {
    let leaf = Directory {
        directories: vec![],
        files: vec![file_entry(b"data", 3, false)],
        symlinks: vec![],
    };
    let pkg = Directory {
        directories: vec![DirectoryEntry {
            name: b"sub".to_vec(),
            digest: dir_digest_of(&leaf).to_vec(),
            size: 1,
        }],
        files: vec![file_entry(b"tool", 7, true)],
        symlinks: vec![SymlinkEntry {
            name: b"alias".to_vec(),
            target: b"tool".to_vec(),
        }],
    };
    let roots = vec![
        dir_root("/nix/store/ddd-pkg", &pkg),
        (
            "/nix/store/eee-one.patch".to_owned(),
            RootNode {
                node: Some(root_node::Node::File(file_entry(b"", 11, false))),
            },
        ),
        (
            "/nix/store/fff-link".to_owned(),
            RootNode {
                node: Some(root_node::Node::Symlink(SymlinkEntry {
                    name: vec![],
                    target: b"ddd-pkg".to_vec(),
                })),
            },
        ),
    ];
    let map = InoMap::from_parts(&roots, vec![leaf, pkg]).expect("build tree");

    let kinds_of = |ino: u64| -> Vec<(Vec<u8>, FileType)> {
        map.readdir(ino, 0)
            .expect("is a dir")
            .filter(|e| e.name != b"." && e.name != b"..")
            .map(|e| (e.name.to_vec(), e.kind))
            .collect()
    };

    // Closure roots: dir, regular file, symlink — as enumerated by
    // readdir(ROOT) and as reported by lookup+getattr.
    for (name, want) in [
        (b"ddd-pkg".as_slice(), FileType::Directory),
        (b"eee-one.patch", FileType::RegularFile),
        (b"fff-link", FileType::Symlink),
    ] {
        let listed = kinds_of(INodeNo::ROOT.0);
        let (_, d_type) = listed
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("root readdir is missing {name:?}"));
        let (_, attr) = map.lookup(INodeNo::ROOT.0, name).expect("root resolves");
        assert_eq!(*d_type, want, "readdir kind for root entry {name:?}");
        assert_eq!(attr.kind, want, "getattr kind for root entry {name:?}");
    }

    // Nested Directory children: the same agreement one level down.
    let (pkg_ino, _) = map.lookup(INodeNo::ROOT.0, b"ddd-pkg").expect("resolves");
    let listed = kinds_of(pkg_ino);
    for (name, want) in [
        (b"sub".as_slice(), FileType::Directory),
        (b"tool", FileType::RegularFile),
        (b"alias", FileType::Symlink),
    ] {
        let (_, d_type) = listed
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("pkg readdir is missing {name:?}"));
        let (_, attr) = map.lookup(pkg_ino, name).expect("child resolves");
        assert_eq!(*d_type, want, "readdir kind for {name:?}");
        assert_eq!(attr.kind, want, "getattr kind for {name:?}");
    }
}

/// generic/002 + generic/614 (adapted): link counts and block counts.
/// Castore reports honest nlink: alias count for files/symlinks (1
/// for a single-path node), 2 + subdirectory count for directories;
/// st_blocks must be ceil(size/512) so `du`/space estimates on inputs
/// stay sane. Asserting the choice here means a silent change shows up
/// as a test diff instead of a production surprise.
// r[verify builder.fs.castore-nlink]
#[test]
fn generic_002_614_nlink_and_blocks() {
    let dir = Directory {
        directories: vec![],
        files: vec![
            file_entry(b"empty", 0, false),
            file_entry(b"one", 1, false),
            file_entry(b"block-1", 511, false),
            file_entry(b"block-exact", 512, false),
            file_entry(b"block-2", 513, false),
            file_entry(b"odd", 1_300_003, false),
        ],
        symlinks: vec![SymlinkEntry {
            name: b"link".to_vec(),
            target: b"one".to_vec(),
        }],
    };
    let roots = vec![dir_root("/nix/store/ggg-xfstests-blocks", &dir)];
    let map = InoMap::from_parts(&roots, vec![dir]).expect("build tree");
    let (dir_ino, dir_attr) = map
        .lookup(INodeNo::ROOT.0, b"ggg-xfstests-blocks")
        .expect("root resolves");

    assert_eq!(
        dir_attr.nlink, 2,
        "leaf directory nlink is 2 (no subdirectories)"
    );

    for (name, size, want_blocks) in [
        (b"empty".as_slice(), 0, 0),
        (b"one", 1, 1),
        (b"block-1", 511, 1),
        (b"block-exact", 512, 1),
        (b"block-2", 513, 2),
        (b"odd", 1_300_003, 2540),
    ] {
        let (_, attr) = map.lookup(dir_ino, name).expect("child resolves");
        assert_eq!(attr.size, size, "size of {name:?}");
        assert_eq!(
            attr.blocks, want_blocks,
            "st_blocks of {name:?} must be ceil(size/512)"
        );
        assert_eq!(attr.nlink, 1, "file nlink of {name:?}");
        assert_eq!(attr.blksize, 4096, "advertised IO block size of {name:?}");
    }

    let (_, link) = map.lookup(dir_ino, b"link").expect("symlink resolves");
    assert_eq!(link.nlink, 1, "symlink nlink");
}

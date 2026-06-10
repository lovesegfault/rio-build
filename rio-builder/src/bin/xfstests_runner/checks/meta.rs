//! Metadata checks: mount layout, readdir, inode identity, file kinds,
//! link counts, symlinks, byte-exact names.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirEntryExt, MetadataExt};
use std::path::Path;

use anyhow::{Context, bail, ensure};
use nix::errno::Errno;

use super::{Ctx, Outcome, errno_of};
use crate::manifest::SymlinkResolution;

/// The mount under test must actually be the castore-FUSE: fstype
/// `fuse.rio-castore` in /proc/self/mounts, serving exactly the dep's
/// store-path basename at its root. Guards against the whole suite
/// silently asserting against a plain directory (e.g. the mountpoint
/// after a FUSE crash, or a backing dir) — every later check would
/// "pass" against the wrong filesystem.
pub fn mount_castore_fstype(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let mounts = fs::read_to_string("/proc/self/mounts").context("read /proc/self/mounts")?;
    let mount_str = ctx.mount.to_str().context("mount path is not UTF-8")?;
    let line = mounts
        .lines()
        .find(|l| l.split_whitespace().nth(1) == Some(mount_str))
        .with_context(|| format!("{mount_str} is not a mountpoint in /proc/self/mounts"))?;
    let fstype = line
        .split_whitespace()
        .nth(2)
        .context("malformed /proc/self/mounts line")?;
    ensure!(
        fstype == ctx.manifest.fstype,
        "mount {} has fstype {fstype}, expected {}",
        mount_str,
        ctx.manifest.fstype
    );

    // The mount root serves exactly the dep (serve-castore was given one
    // store path).
    let names: Vec<_> = fs::read_dir(&ctx.mount)
        .context("read_dir(mount root)")?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<Result<_, _>>()?;
    ensure!(
        names.len() == 1,
        "mount root must list exactly the dep, got {names:?}"
    );
    let dep_name = ctx
        .dep_root
        .file_name()
        .context("dep_root has no basename")?;
    ensure!(
        names[0] == dep_name,
        "mount root lists {:?}, expected {:?}",
        names[0],
        dep_name
    );
    Ok(Outcome::Pass)
}

/// Overlay-lowerdir leg of generic/257 + generic/453: a real dispatched
/// build on the worker listed the fixture's 200-entry dir and its
/// lookalike names through overlay-over-castore (cold dcache) and wrote
/// the counts into its output. The runner only validates that output —
/// the overlay stack runs on a different machine and cannot be probed
/// from here. Guards readdir through the production overlay stack
/// (multifile.nix only proves 5 entries / one READDIR batch).
pub fn overlay_readdir_consumer(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let Some(path) = &ctx.consumer_output else {
        return Ok(Outcome::Skip("no --consumer-output given"));
    };
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read consumer output {}", path.display()))?;
    let lines: Vec<&str> = raw.lines().map(str::trim).collect();
    ensure!(
        lines.len() >= 3,
        "consumer output must have 3 lines (dep path, dir200 count, names count), got {lines:?}"
    );
    ensure!(
        lines[0].starts_with("/nix/store/") && lines[0].ends_with(&ctx.manifest.root_suffix),
        "consumer line 1 is not the dep store path: {:?}",
        lines[0]
    );
    ensure!(
        lines[1] == ctx.manifest.seq_dir.count.to_string(),
        "overlay readdir of {} saw {} entries, expected {}",
        ctx.manifest.seq_dir.path,
        lines[1],
        ctx.manifest.seq_dir.count
    );
    let names_count = ctx.manifest.files_under("names/").count();
    ensure!(
        lines[2] == names_count.to_string(),
        "overlay readdir of names/ saw {} entries, expected {names_count}",
        lines[2]
    );
    Ok(Outcome::Pass)
}

/// generic/257 (t_dir_offset2): a 200-entry directory forces several
/// FUSE_READDIR(PLUS) round-trips; the listing must be complete, stable
/// across repeated enumerations, duplicate-free, and every entry's
/// d_ino must equal st_ino (and never 0). Guards `InoMap::readdir`'s
/// hand-rolled offset bookkeeping — duplicated/skipped entries on
/// resume is exactly its failure mode.
pub fn generic_257_readdir_multibatch(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let dir = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let count = ctx.manifest.seq_dir.count;

    let listing = |label: &str| -> anyhow::Result<BTreeMap<String, u64>> {
        let mut seen = BTreeMap::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("read_dir ({label})"))? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|os| anyhow::anyhow!("non-UTF-8 readdir entry {os:?} ({label})"))?;
            let prev = seen.insert(name.clone(), entry.ino());
            ensure!(prev.is_none(), "duplicate readdir entry {name} ({label})");
        }
        Ok(seen)
    };

    let first = listing("first enumeration")?;
    let second = listing("second enumeration")?;

    let expected: BTreeSet<String> = (1..=count).map(|i| format!("f{i}")).collect();
    let got: BTreeSet<String> = first.keys().cloned().collect();
    ensure!(
        got == expected,
        "dir listing wrong: {} entries (missing: {:?}, extra: {:?})",
        first.len(),
        expected.difference(&got).take(5).collect::<Vec<_>>(),
        got.difference(&expected).take(5).collect::<Vec<_>>()
    );
    ensure!(
        first == second,
        "directory listing not stable across enumerations"
    );

    // d_ino == st_ino for every entry (the predecessor JIT-FUSE invented
    // hash-based ephemeral readdir inos distinct from lookup's — old
    // finding F-2; castore readdir must not regress to that).
    for (name, d_ino) in &first {
        let st = fs::symlink_metadata(dir.join(name)).with_context(|| format!("lstat {name}"))?;
        ensure!(*d_ino != 0, "d_ino of {name} is 0");
        ensure!(
            *d_ino == st.ino(),
            "d_ino of {name} ({d_ino}) != st_ino ({})",
            st.ino()
        );
    }

    // Spot-check content derivation: f<i> holds "<i>\n" per the
    // fixture's build script.
    let last = format!("f{count}");
    for name in ["f1", "f57", last.as_str()] {
        let body = fs::read_to_string(dir.join(name))?;
        let i = &name[1..];
        ensure!(
            body == format!("{i}\n"),
            "content of {name} is {body:?}, expected \"{i}\\n\""
        );
    }
    Ok(Outcome::Pass)
}

/// Castore-specific identity contract: file/symlink inodes are
/// content-derived, directory inodes are path-derived. Identical bytes
/// (same exec bit) in different directories share one inode (with an
/// honest st_nlink alias count); the same bytes with a different exec
/// bit are distinct inodes; directories with identical contents at
/// different paths are DISTINCT inodes — a shared directory inode is a
/// hardlinked directory, which POSIX forbids and which desyncs
/// fts-based walkers (find/du/rm -r abort with fabricated ENOENT under
/// concurrency). Guards the ino derivation in `tree::InoMap`.
pub fn inode_identity_content_addressed(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let ino = |rel: &str| -> anyhow::Result<u64> {
        Ok(fs::symlink_metadata(ctx.on_mount(rel))
            .with_context(|| format!("lstat {rel}"))?
            .ino())
    };

    // Files: same (content, exec) class → one inode, and st_nlink
    // reports at least the alias count visible in the fixture (the
    // same content class may have more aliases elsewhere in the tree).
    let mut dedup_groups = 0;
    for ((_, _), members) in ctx.manifest.content_groups() {
        if members.len() < 2 {
            continue;
        }
        dedup_groups += 1;
        let inos: Vec<u64> = members
            .iter()
            .map(|f| ino(&f.path))
            .collect::<anyhow::Result<_>>()?;
        ensure!(
            inos.windows(2).all(|w| w[0] == w[1]),
            "files with identical content+exec have distinct inodes: {:?} -> {inos:?}",
            members.iter().map(|f| &f.path).collect::<Vec<_>>()
        );
        let nlink = fs::symlink_metadata(ctx.on_mount(&members[0].path))?.nlink();
        ensure!(
            nlink >= members.len() as u64,
            "deduped file {} has nlink {nlink}, expected >= its {} aliases",
            members[0].path,
            members.len()
        );
    }
    ensure!(
        dedup_groups > 0,
        "fixture has no duplicate-content files; the dedup contract is not covered"
    );

    // Same content, different exec bit → distinct inodes.
    let mut by_content: BTreeMap<&str, Vec<(bool, &str)>> = BTreeMap::new();
    for f in &ctx.manifest.files {
        by_content
            .entry(f.content.as_str())
            .or_default()
            .push((f.executable, &f.path));
    }
    let mut exec_splits = 0;
    for (_, entries) in by_content {
        let exec = entries.iter().find(|(x, _)| *x);
        let non_exec = entries.iter().find(|(x, _)| !*x);
        let (Some((_, exec_path)), Some((_, non_exec_path))) = (exec, non_exec) else {
            continue;
        };
        exec_splits += 1;
        let exec_ino = ino(exec_path)?;
        let non_exec_ino = ino(non_exec_path)?;
        ensure!(
            exec_ino != non_exec_ino,
            "same bytes with a different exec bit must be distinct inodes \
             ({exec_path} and {non_exec_path} are both inode {exec_ino})"
        );
    }
    ensure!(
        exec_splits > 0,
        "fixture has no exec/non-exec twin; the exec-bit identity split is not covered"
    );

    // Directories with identical (name, content, exec) child sets share
    // one decoded Directory body, but each PATH must be its own inode —
    // equal inos here would be hardlinked directories.
    let mut dir_shapes: BTreeMap<Vec<(String, String, bool)>, Vec<&str>> = BTreeMap::new();
    for dir in &ctx.manifest.dirs {
        let prefix = format!("{dir}/");
        let mut children: Vec<(String, String, bool)> = ctx
            .manifest
            .files_under(&prefix)
            .filter(|f| !f.path[prefix.len()..].contains('/'))
            .map(|f| {
                (
                    f.path[prefix.len()..].to_owned(),
                    f.content.clone(),
                    f.executable,
                )
            })
            .collect();
        // Only meaningful for leaf dirs fully described by explicit
        // files (no symlink/subdir/seq/big members).
        let has_other_members = ctx
            .manifest
            .symlinks
            .iter()
            .any(|s| s.path.starts_with(&prefix))
            || ctx.manifest.dirs.iter().any(|d| d.starts_with(&prefix))
            || ctx.manifest.seq_dir.path.starts_with(&prefix)
            || ctx.manifest.big_file.path.starts_with(&prefix);
        if children.is_empty() || has_other_members {
            continue;
        }
        children.sort();
        dir_shapes.entry(children).or_default().push(dir);
    }
    for (_, dirs) in dir_shapes {
        if dirs.len() < 2 {
            continue;
        }
        let mut inos: Vec<u64> = dirs.iter().map(|d| ino(d)).collect::<anyhow::Result<_>>()?;
        let count = inos.len();
        inos.sort_unstable();
        inos.dedup();
        ensure!(
            inos.len() == count,
            "directories with identical contents share an inode (hardlinked-dir \
             semantics): {dirs:?} -> {inos:?}"
        );
    }
    Ok(Outcome::Pass)
}

/// generic/401: the file kind reported by stat and by readdir's d_type
/// must agree and be correct for every node kind, and the served modes
/// must be the canonical store-path metadata (root-owned 0444/0555,
/// epoch+1 mtime). Wrong kinds break `find -type`, glob ordering, and
/// overlay copy-up decisions over the lower; wrong modes/mtimes make
/// SOURCE_DATE_EPOCH and tar-producing FODs non-deterministic.
pub fn generic_401_file_kinds(ctx: &Ctx) -> anyhow::Result<Outcome> {
    // Kinds + canonical metadata via lstat.
    for dir in &ctx.manifest.dirs {
        let st = fs::symlink_metadata(ctx.on_mount(dir))?;
        ensure!(st.is_dir(), "{dir} is not a directory");
        ensure!(
            st.mode() & 0o7777 == 0o555,
            "dir {dir} mode {:o}, expected 555",
            st.mode() & 0o7777
        );
    }
    for f in &ctx.manifest.files {
        let st = fs::symlink_metadata(ctx.on_mount(&f.path))?;
        ensure!(st.is_file(), "{} is not a regular file", f.path);
        ensure!(
            st.size() == f.content.len() as u64,
            "{} size {} != expected {}",
            f.path,
            st.size(),
            f.content.len()
        );
        let want_mode = if f.executable { 0o555 } else { 0o444 };
        ensure!(
            st.mode() & 0o7777 == want_mode,
            "{} mode {:o}, expected {want_mode:o}",
            f.path,
            st.mode() & 0o7777
        );
        ensure!(
            st.uid() == 0 && st.gid() == 0,
            "{} not root-owned ({}:{})",
            f.path,
            st.uid(),
            st.gid()
        );
        ensure!(
            st.mtime() == 1,
            "{} mtime {} != canonical store mtime 1",
            f.path,
            st.mtime()
        );
    }
    for s in &ctx.manifest.symlinks {
        let st = fs::symlink_metadata(ctx.on_mount(&s.path))?;
        ensure!(st.file_type().is_symlink(), "{} is not a symlink", s.path);
    }

    // d_type agreement: std's DirEntry::file_type() uses getdents
    // d_type when the filesystem provides it.
    if let Some(links_dir) = symlinks_parent(ctx) {
        let n_symlinks = fs::read_dir(ctx.on_mount(&links_dir))?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
            .count();
        let expected = ctx
            .manifest
            .symlinks
            .iter()
            .filter(|s| s.path.starts_with(&format!("{links_dir}/")))
            .count();
        ensure!(
            n_symlinks == expected,
            "d_type saw {n_symlinks} symlinks under {links_dir}, expected {expected}"
        );
    }
    let expected_dirs = top_level_dir_count(ctx);
    let n_dirs = fs::read_dir(&ctx.dep_root)?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .count();
    ensure!(
        n_dirs == expected_dirs,
        "d_type saw {n_dirs} top-level dirs, expected {expected_dirs}"
    );
    Ok(Outcome::Pass)
}

/// generic/002 (adapted) + the structural completeness check: castore
/// reports nlink=1 for every node — there is no backing tree to mirror
/// (the btrfs-style choice, finding F-B in PLAN.md). The practical
/// failure mode of bogus nlink is `find`/`fts` pruning subtrees via
/// the nlink-2 heuristic, so a full walk must enumerate every node the
/// manifest describes.
pub fn generic_002_nlink_walk(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let plain_file = ctx
        .manifest
        .files
        .iter()
        .find(|f| !f.executable)
        .map(|f| f.path.as_str())
        .context("manifest has no plain file for the nlink spot check")?;
    for rel in [ctx.manifest.seq_dir.path.as_str(), plain_file] {
        let st = fs::symlink_metadata(ctx.on_mount(rel))?;
        ensure!(
            st.nlink() == 1,
            "{rel} nlink {} != the documented castore choice of 1 (finding F-B)",
            st.nlink()
        );
    }

    let mut count: u64 = 0;
    walk(&ctx.dep_root, &mut count)?;
    ensure!(
        count == ctx.manifest.expected_node_count(),
        "walk enumerated {count} nodes, manifest expects {}",
        ctx.manifest.expected_node_count()
    );
    Ok(Outcome::Pass)
}

/// generic/005: dereferencing a symlink loop gives ELOOP, a dangling
/// symlink gives ENOENT, while readlink still reports the target bytes
/// for both. The kernel walks loops over the FUSE's readlink replies —
/// a wrong reply turns ELOOP into build-visible misbehavior (configure
/// scripts treat ELOOP and ENOENT very differently).
pub fn generic_005_symlink_errnos(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let mut loops = 0;
    let mut danglings = 0;
    for spec in &ctx.manifest.symlinks {
        let path = ctx.on_mount(&spec.path);
        let resolution = ctx.manifest.resolve_symlink(spec);
        let opened = fs::File::open(&path);
        match resolution {
            SymlinkResolution::Loop => {
                loops += 1;
                let Err(e) = opened else {
                    bail!(
                        "{}: deref of a symlink loop unexpectedly succeeded",
                        spec.path
                    );
                };
                ensure!(
                    errno_of(&e) == Errno::ELOOP,
                    "{}: deref of a symlink loop gave {:?}, expected ELOOP",
                    spec.path,
                    errno_of(&e)
                );
            }
            SymlinkResolution::Dangling => {
                danglings += 1;
                let Err(e) = opened else {
                    bail!(
                        "{}: deref of a dangling symlink unexpectedly succeeded",
                        spec.path
                    );
                };
                ensure!(
                    errno_of(&e) == Errno::ENOENT,
                    "{}: deref of a dangling symlink gave {:?}, expected ENOENT",
                    spec.path,
                    errno_of(&e)
                );
            }
            SymlinkResolution::File(f) => {
                let mut body = Vec::new();
                opened
                    .with_context(|| format!("{}: deref open failed", spec.path))?
                    .read_to_end(&mut body)?;
                ensure!(
                    body == f.content.as_bytes(),
                    "{}: deref content mismatch (resolves to {})",
                    spec.path,
                    f.path
                );
            }
        }
        // readlink works regardless of whether the target resolves.
        let target = fs::read_link(&path)?;
        ensure!(
            target.as_os_str().as_bytes() == spec.target.as_bytes(),
            "{}: readlink gave {:?}, expected {:?}",
            spec.path,
            target,
            spec.target
        );
    }
    // The fixture must contain both error classes or the check is
    // vacuous.
    ensure!(
        loops > 0 && danglings > 0,
        "fixture lacks loop ({loops}) or dangling ({danglings}) symlinks; generic/005 not covered"
    );
    Ok(Outcome::Pass)
}

/// generic/360: symlink targets round-trip byte-exactly and lstat size
/// equals strlen(target) — including the 900-byte target. Tools size
/// their readlink(2) buffer from st_size; a short size truncates the
/// target and sends a build to the wrong path.
pub fn generic_360_symlink_targets(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let mut max_target_len = 0;
    for spec in &ctx.manifest.symlinks {
        let path = ctx.on_mount(&spec.path);
        let st = fs::symlink_metadata(&path)?;
        ensure!(
            st.size() == spec.target.len() as u64,
            "{}: lstat size {} != target length {}",
            spec.path,
            st.size(),
            spec.target.len()
        );
        let target = fs::read_link(&path)?;
        ensure!(
            target.as_os_str().as_bytes() == spec.target.as_bytes(),
            "{}: readlink target mismatch",
            spec.path
        );
        max_target_len = max_target_len.max(spec.target.len());
    }
    // The fixture must actually contain a long-target symlink, else
    // this check silently stops covering generic/360.
    ensure!(
        max_target_len >= 900,
        "fixture has no long-target symlink (max target len {max_target_len}); generic/360 not covered"
    );
    Ok(Outcome::Pass)
}

/// generic/453: lookalike and unusual filenames (NFC vs NFD unicode,
/// embedded space, NAME_MAX-length) resolve byte-exactly to their own
/// content and all appear in readdir. `InoMap::lookup` is a byte-exact
/// scan — any normalization or truncation would serve the wrong
/// content to a build.
pub fn generic_453_byte_exact_names(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let names_files: Vec<_> = ctx.manifest.files_under("names/").collect();
    ensure!(
        !names_files.is_empty(),
        "fixture has no names/ entries; generic/453 not covered"
    );

    for f in &names_files {
        let body = fs::read(ctx.on_mount(&f.path)).with_context(|| format!("read {:?}", f.path))?;
        ensure!(
            body == f.content.as_bytes(),
            "{:?}: content \"{}\", expected {:?} — a lookalike name resolved to the wrong entry",
            f.path,
            body.escape_ascii(),
            f.content
        );
    }

    // readdir lists exactly the expected byte sequences.
    let listed: BTreeSet<Vec<u8>> = fs::read_dir(ctx.on_mount("names"))?
        .map(|e| e.map(|e| e.file_name().as_bytes().to_vec()))
        .collect::<Result<_, _>>()?;
    let expected: BTreeSet<Vec<u8>> = names_files
        .iter()
        .map(|f| f.path.as_bytes()["names/".len()..].to_vec())
        .collect();
    ensure!(
        listed == expected,
        "names/ readdir mismatch: listed {} entries, expected {}",
        listed.len(),
        expected.len()
    );

    // The NAME_MAX (255-byte) name must be present — it is the edge the
    // test exists for.
    ensure!(
        expected.iter().any(|n| n.len() == 255),
        "fixture has no NAME_MAX-length name; generic/453 edge not covered"
    );
    Ok(Outcome::Pass)
}

// ─── helpers ───────────────────────────────────────────────────────────

fn walk(dir: &Path, count: &mut u64) -> anyhow::Result<()> {
    *count += 1; // the directory itself
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            walk(&entry.path(), count)?;
        } else {
            *count += 1;
        }
    }
    Ok(())
}

/// Number of manifest dirs that sit directly under the dep root.
fn top_level_dir_count(ctx: &Ctx) -> usize {
    ctx.manifest
        .dirs
        .iter()
        .filter(|d| !d.contains('/'))
        .count()
}

/// The single directory containing every symlink (the fixture keeps
/// them in one place); `None` if they are scattered.
fn symlinks_parent(ctx: &Ctx) -> Option<String> {
    let parents: BTreeSet<&str> = ctx
        .manifest
        .symlinks
        .iter()
        .filter_map(|s| s.path.rfind('/').map(|i| &s.path[..i]))
        .collect();
    if parents.len() == 1 {
        parents.into_iter().next().map(str::to_owned)
    } else {
        None
    }
}

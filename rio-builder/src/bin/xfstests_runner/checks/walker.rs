//! Tree-walk identity checks: per-path directory inodes, honest link
//! counts, `.`/`..` identity, and walks that survive concurrent access
//! to content-identical paths.
//!
//! Motivation: castore used to dedup content-identical DIRECTORIES onto
//! one inode (`tree::dir_ino` was keyed by dir digest alone). POSIX
//! forbids hardlinked-directory semantics, and the kernel enforces one
//! dentry per directory inode by *re-parenting* the dentry on every
//! lookup of
//! a different alias (`d_splice_alias`). GNU find's fts walks verify
//! their ascent by (dev,ino) and manufacture ENOENT when a concurrent
//! reader moves the dentry mid-walk — which escaped to the data plane.
//! The checks here pin the POSIX-correct contract:
//!
//! * every directory path has its own (dev,ino) — files may still
//!   dedup, directories must not;
//! * st_nlink equals the number of paths an inode is reachable at, so
//!   `tar`/`du`/`cp` (which dedup on (dev,ino,nlink)) archive the tree
//!   correctly;
//! * readdir's `.`/`..` entries carry the self/parent inode;
//! * fd-relative `..` ascent, getcwd, and full-tree walks are stable
//!   while another thread looks up content-identical aliases.
//!
//! On the pre-fix castore-FUSE the identity checks FAIL by design —
//! they are the regression tests for the escaped bug. They go green
//! with the per-path directory-inode fix (parent_ino + name derivation;
//! files keep content-deduped inos with honest nlink).

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail, ensure};
use nix::fcntl::{OFlag, open, openat};
use nix::libc;
use nix::sys::stat::{Mode, fstat};
use nix::unistd::{chdir, fchdir, getcwd};

use super::{Ctx, Outcome, RawDir, count_nodes};

/// (st_dev, st_ino) — the identity tools compare across paths.
type FileId = (u64, u64);

fn id_of(meta: &fs::Metadata) -> FileId {
    (meta.dev(), meta.ino())
}

/// POSIX directory-inode uniqueness: a full walk of the tree must find
/// every directory path on its OWN (dev,ino), even though the fixture
/// contains content-identical directories (which the store dedups to
/// one Directory body). Two dir paths on one inode are hardlinked-dir
/// semantics — the kernel then ping-pongs the single dentry between
/// the paths and every (dev,ino)-verifying walker (GNU fts, getcwd
/// consistency checks) is fair game.
pub fn posix_dir_inode_uniqueness(ctx: &Ctx) -> anyhow::Result<Outcome> {
    ensure!(
        !ctx.manifest.alias_dir_groups().is_empty(),
        "fixture has no content-identical directories; directory-identity dedup is not covered"
    );

    let mut seen: BTreeMap<FileId, PathBuf> = BTreeMap::new();
    let mut dirs = 0u64;
    visit_dirs(&ctx.dep_root, &mut |dir| {
        dirs += 1;
        let st = fs::symlink_metadata(dir).with_context(|| format!("lstat {}", dir.display()))?;
        if let Some(prev) = seen.insert(id_of(&st), dir.to_owned()) {
            bail!(
                "two distinct directory paths share one inode — hardlinked-directory \
                 semantics, which POSIX forbids: {} and {} are both (dev={}, ino={})",
                prev.display(),
                dir.display(),
                st.dev(),
                st.ino()
            );
        }
        Ok(())
    })?;

    // The walk must have visited every directory the manifest declares
    // (root + dirs), or the uniqueness assertion ran on a subset.
    let expected = 1 + ctx.manifest.dirs.len() as u64;
    ensure!(
        dirs == expected,
        "walk visited {dirs} directories, manifest declares {expected}"
    );
    Ok(Outcome::Pass)
}

/// Hardlink honesty: group every regular-file path by observed
/// (dev,ino); each member's st_nlink must equal the group size. `tar`,
/// `du`, and `cp -a` treat st_ino equality plus nlink>1 as "same file,
/// archive once" and trust nlink for early-exit bookkeeping — a shared
/// inode claiming nlink=1 makes them silently mis-handle the tree.
/// Both honest designs pass: deduped inos with real link counts, or
/// per-path inos with nlink=1.
pub fn hardlink_nlink_honesty(ctx: &Ctx) -> anyhow::Result<Outcome> {
    ensure!(
        ctx.manifest.content_groups().values().any(|g| g.len() >= 2),
        "fixture has no duplicate-content files; nlink honesty is not covered"
    );

    let mut paths: Vec<String> = ctx.manifest.files.iter().map(|f| f.path.clone()).collect();
    paths.push(ctx.manifest.big_file.path.clone());
    for i in 1..=ctx.manifest.seq_dir.count {
        paths.push(format!("{}/f{i}", ctx.manifest.seq_dir.path));
    }

    let mut by_id: BTreeMap<FileId, Vec<(String, u64)>> = BTreeMap::new();
    for p in paths {
        let st = fs::symlink_metadata(ctx.on_mount(&p)).with_context(|| format!("lstat {p}"))?;
        ensure!(st.is_file(), "{p} is not a regular file");
        by_id.entry(id_of(&st)).or_default().push((p, st.nlink()));
    }
    for ((dev, ino), members) in &by_id {
        let want = members.len() as u64;
        for (path, nlink) in members {
            ensure!(
                *nlink == want,
                "{path}: st_nlink is {nlink} but inode (dev={dev}, ino={ino}) is reachable \
                 at {want} path(s) ({:?}) — tar/du/cp dedup on (dev,ino,nlink) and will \
                 mis-archive this tree",
                members.iter().map(|(p, _)| p).collect::<Vec<_>>()
            );
        }
    }
    Ok(Outcome::Pass)
}

/// `.`/`..` identity, for every directory: the getdents dot entries
/// must carry the directory's own and its parent's inode, and
/// fd-relative openat(fd, ".")/openat(fd, "..") must resolve to the
/// same identities. `find -depth`, fts ascent verification, and shell
/// `cd ..` all consume one of these surfaces. (The dep root's parent is
/// the mount root, FUSE_ROOT_ID.)
pub fn dot_dotdot_identity(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let mut targets: Vec<(PathBuf, PathBuf)> = vec![(ctx.dep_root.clone(), ctx.mount.clone())];
    for d in &ctx.manifest.dirs {
        let parent = match d.rfind('/') {
            Some(i) => ctx.on_mount(&d[..i]),
            None => ctx.dep_root.clone(),
        };
        targets.push((ctx.on_mount(d), parent));
    }

    for (dir, parent) in &targets {
        let self_st = fs::symlink_metadata(dir)?;
        let parent_st = fs::symlink_metadata(parent)?;

        // getdents view: the daemon's own d_ino for the dot entries.
        let entries = RawDir::open(dir)?.entries()?;
        let d_ino_of = |name: &[u8]| -> anyhow::Result<u64> {
            entries
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, ino)| *ino)
                .with_context(|| {
                    format!(
                        "readdir of {} lists no {:?} entry",
                        dir.display(),
                        name.escape_ascii().to_string()
                    )
                })
        };
        let dot = d_ino_of(b".")?;
        ensure!(
            dot == self_st.ino(),
            "{}: readdir '.' d_ino {dot} != st_ino {}",
            dir.display(),
            self_st.ino()
        );
        let dotdot = d_ino_of(b"..")?;
        ensure!(
            dotdot == parent_st.ino(),
            "{}: readdir '..' d_ino {dotdot} != parent {} ino {} — '..' must identify the \
             parent directory (POSIX); a self-pointing '..' breaks every nlink/ino-verifying \
             tree walker",
            dir.display(),
            parent.display(),
            parent_st.ino()
        );

        // fd-relative view: the kernel's dcache resolution of "." and
        // "..".
        let fd = open_dir(dir)?;
        let via_dot = fstat(openat(
            &fd,
            ".",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            Mode::empty(),
        )?)?;
        ensure!(
            (via_dot.st_dev, via_dot.st_ino) == id_of(&self_st),
            "{}: openat(fd, \".\") resolved to (dev={}, ino={}), expected self",
            dir.display(),
            via_dot.st_dev,
            via_dot.st_ino
        );
        let via_dotdot = fstat(openat(
            &fd,
            "..",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            Mode::empty(),
        )?)?;
        ensure!(
            (via_dotdot.st_dev, via_dotdot.st_ino) == id_of(&parent_st),
            "{}: openat(fd, \"..\") resolved to (dev={}, ino={}), expected the parent {} \
             (dev={}, ino={})",
            dir.display(),
            via_dotdot.st_dev,
            via_dotdot.st_ino,
            parent.display(),
            parent_st.dev(),
            parent_st.ino()
        );
    }
    Ok(Outcome::Pass)
}

/// generic/028 (read-only adaptation): getcwd must keep reporting the
/// path we chdir'd to. Upstream races getcwd against rename churn; the
/// castore has no renames, but a lookup of a content-identical ALIAS
/// directory is the analogous dentry churn — it must not retroactively
/// change our cwd's path. (On the pre-fix FUSE the kernel re-parents
/// the shared dentry and getcwd flips to the alias's path.)
pub fn generic_028_getcwd_stability(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let (alias, target) = cross_parent_alias_pair(ctx)?;
    let alias_abs = ctx.on_mount(&alias);
    let target_abs = ctx.on_mount(&target);
    let target_parent = target_abs.parent().context("alias dir has no parent")?;

    let _restore = CwdGuard::save()?;

    // Place the dentry under target's parent, then make it our cwd.
    fs::symlink_metadata(&target_abs)?;
    chdir(&target_abs).with_context(|| format!("chdir({})", target_abs.display()))?;
    let got = getcwd()?;
    ensure!(
        got == target_abs,
        "getcwd after chdir({}) reported {}",
        target_abs.display(),
        got.display()
    );

    // A lookup of the content-identical alias must not move us.
    fs::symlink_metadata(&alias_abs)?;
    let got = getcwd()?;
    ensure!(
        got == target_abs,
        "getcwd flipped to {} after a lookup of the content-identical alias {} — the \
         kernel re-parented the shared directory dentry under our feet (generic/028's \
         corruption class, via the aliased-directory inode bug)",
        got.display(),
        alias_abs.display()
    );

    // Ascend: `cd ..` must land in target's parent.
    chdir("..")?;
    let here = fs::symlink_metadata(".")?;
    let want = fs::symlink_metadata(target_parent)?;
    ensure!(
        id_of(&here) == id_of(&want),
        "chdir(\"..\") from {} arrived at (dev={}, ino={}), expected the parent {} \
         (dev={}, ino={})",
        target_abs.display(),
        here.dev(),
        here.ino(),
        target_parent.display(),
        want.dev(),
        want.ino()
    );
    Ok(Outcome::Pass)
}

/// generic/011 (read-only adaptation): many concurrent walkers over one
/// tree. Eight threads each repeatedly walk the whole tree by path
/// (fs::read_dir) and list the 200-entry dir; every walk must
/// enumerate the exact manifest node count with no errors. Lookup and
/// readdir contend on fuser's thread pool and — through the aliased
/// fixture dirs — on the kernel's dentry machinery.
pub fn generic_011_dirstress(ctx: &Ctx) -> anyhow::Result<Outcome> {
    const WALKERS: usize = 8;
    const ROUNDS: usize = 4;
    let expected = ctx.manifest.expected_node_count();
    let seq_dir = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let seq_count = usize::try_from(ctx.manifest.seq_dir.count).expect("count fits usize");

    std::thread::scope(|s| {
        let handles: Vec<_> = (0..WALKERS)
            .map(|w| {
                let seq_dir = &seq_dir;
                s.spawn(move || -> anyhow::Result<()> {
                    for round in 0..ROUNDS {
                        let mut count = 0u64;
                        count_nodes(&ctx.dep_root, &mut count)
                            .with_context(|| format!("walker {w} round {round}"))?;
                        ensure!(
                            count == expected,
                            "walker {w} round {round}: enumerated {count} nodes, expected \
                             {expected}"
                        );
                        let listed = fs::read_dir(seq_dir)?.count();
                        ensure!(
                            listed == seq_count,
                            "walker {w} round {round}: seq dir listed {listed} entries, \
                             expected {seq_count}"
                        );
                    }
                    Ok(())
                })
            })
            .collect();
        for h in handles {
            h.join().expect("dirstress walker thread panicked")?;
        }
        anyhow::Ok(())
    })?;
    Ok(Outcome::Pass)
}

/// The regression test for the escaped fts/ENOENT bug: an fts-style
/// fd-relative walk (ascend via openat(child, "..") and verify the
/// parent by (dev,ino), exactly gnulib fts's safe-changedir check)
/// must complete the full tree, repeatedly, while another thread loops
/// lookups over the content-identical alias paths. Two legs:
///
/// 1. Deterministic: hold an fd on `nest/p2/shared`, look up its alias
///    `nest/p1/shared`, ascend — the parent must still be p2. On the
///    pre-fix FUSE the alias lookup re-parents the single shared
///    dentry and the ascent lands in p1.
/// 2. Racing: 25 full fts walks against a tight alias-stat loop; the
///    walk must never see a wrong parent and must always enumerate the
///    complete tree.
pub fn fts_walk_concurrent_aliases(ctx: &Ctx) -> anyhow::Result<Outcome> {
    // Leg 1 — deterministic re-parent probe.
    let (alias, target) = cross_parent_alias_pair(ctx)?;
    {
        let target_abs = ctx.on_mount(&target);
        let target_fd = open_dir(&target_abs)?;
        let parent_abs = target_abs.parent().context("alias dir has no parent")?;
        let parent_st = fs::symlink_metadata(parent_abs)?;
        // The aliased lookup that re-parents the shared dentry on a
        // buggy fs.
        fs::symlink_metadata(ctx.on_mount(&alias))?;
        let up = fstat(openat(
            &target_fd,
            "..",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY,
            Mode::empty(),
        )?)?;
        ensure!(
            (up.st_dev, up.st_ino) == id_of(&parent_st),
            "ascending '..' out of {target} arrived at (dev={}, ino={}) instead of its \
             parent {} (dev={}, ino={}) after a lookup of the content-identical alias \
             {alias} — the kernel re-parented the shared dentry mid-walk; GNU fts \
             manufactures ENOENT exactly here and aborts the walk",
            up.st_dev,
            up.st_ino,
            parent_abs.display(),
            parent_st.dev(),
            parent_st.ino()
        );
    }

    // Leg 2 — full fts walks racing an alias-lookup loop.
    const WALK_ROUNDS: usize = 25;
    let mut alias_paths: Vec<PathBuf> = ctx
        .manifest
        .alias_dir_groups()
        .iter()
        .flatten()
        .map(|d| ctx.on_mount(d))
        .collect();
    for members in ctx.manifest.content_groups().values() {
        if members.len() >= 2 {
            alias_paths.extend(members.iter().map(|f| ctx.on_mount(&f.path)));
        }
    }
    let expected = ctx.manifest.expected_node_count();
    let stop = AtomicBool::new(false);

    std::thread::scope(|s| {
        let racer = s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                for p in &alias_paths {
                    let _ = fs::symlink_metadata(p);
                }
            }
        });
        let walked = (|| -> anyhow::Result<()> {
            for round in 0..WALK_ROUNDS {
                let root_fd = open_dir(&ctx.dep_root)?;
                let root_st = fstat(&root_fd)?;
                let count = fts_walk(&root_fd, (root_st.st_dev, root_st.st_ino), &ctx.dep_root)
                    .with_context(|| format!("fts walk round {round} (of {WALK_ROUNDS})"))?;
                ensure!(
                    count == expected,
                    "fts walk round {round}: enumerated {count} nodes, expected {expected}"
                );
            }
            Ok(())
        })();
        stop.store(true, Ordering::Relaxed);
        racer.join().expect("alias-racer thread panicked");
        walked
    })?;
    Ok(Outcome::Pass)
}

/// generic/013 (fsstress, read-only op profile) + generic/241 intent:
/// a deterministic, seeded mix of every read-side operation —
/// lstat/stat, open+pread (content-verified against the manifest),
/// readdir, readlink, lseek, eaccess — hammered from 4 threads over
/// the whole tree. Soaks `Opener`'s maps, the per-digest backing
/// refcounts, and fuser's thread pool the way a chaotic parallel build
/// would; every single op must succeed with the expected result. Runs
/// after the read-integrity checks, so big-blob reads hit the warm
/// passthrough path.
pub fn generic_013_fsstress_readonly(ctx: &Ctx) -> anyhow::Result<Outcome> {
    const THREADS: u64 = 4;
    const OPS_PER_THREAD: u32 = 1500;

    let files: Vec<&crate::manifest::FileSpec> = ctx.manifest.files.iter().collect();
    ensure!(!files.is_empty(), "manifest has no files for the soak");
    let dirs: Vec<PathBuf> = std::iter::once(ctx.dep_root.clone())
        .chain(ctx.manifest.dirs.iter().map(|d| ctx.on_mount(d)))
        .collect();
    let symlinks = &ctx.manifest.symlinks;
    let big = &ctx.manifest.big_file;
    let oracle = ctx.manifest.oracle_bytes();

    std::thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let files = &files;
                let dirs = &dirs;
                let oracle = &oracle;
                s.spawn(move || -> anyhow::Result<()> {
                    // Per-thread deterministic LCG (Numerical Recipes
                    // constants) so failures replay exactly.
                    let mut state: u64 = 0x9e3779b97f4a7c15 ^ (t + 1);
                    let mut next = move |bound: usize| -> usize {
                        state = state
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        ((state >> 33) as usize) % bound.max(1)
                    };
                    for op in 0..OPS_PER_THREAD {
                        let label = |what: &str| format!("soak thread {t} op {op}: {what}");
                        match next(6) {
                            0 => {
                                let f = files[next(files.len())];
                                let st = fs::symlink_metadata(ctx.on_mount(&f.path))
                                    .with_context(|| label(&format!("lstat {}", f.path)))?;
                                ensure!(
                                    st.size() == f.content.len() as u64,
                                    "{}",
                                    label(&format!("size mismatch on {}", f.path))
                                );
                            }
                            1 => {
                                let f = files[next(files.len())];
                                let body = fs::read(ctx.on_mount(&f.path))
                                    .with_context(|| label(&format!("read {}", f.path)))?;
                                ensure!(
                                    body == f.content.as_bytes(),
                                    "{}",
                                    label(&format!("content mismatch on {}", f.path))
                                );
                            }
                            2 => {
                                let d = &dirs[next(dirs.len())];
                                let n = fs::read_dir(d)
                                    .with_context(|| label(&format!("read_dir {}", d.display())))?
                                    .count();
                                ensure!(
                                    n > 0,
                                    "{}",
                                    label(&format!("{} listed empty", d.display()))
                                );
                            }
                            3 => {
                                let l = &symlinks[next(symlinks.len())];
                                let target = fs::read_link(ctx.on_mount(&l.path))
                                    .with_context(|| label(&format!("readlink {}", l.path)))?;
                                ensure!(
                                    target.as_os_str().as_bytes() == l.target.as_bytes(),
                                    "{}",
                                    label(&format!("readlink mismatch on {}", l.path))
                                );
                            }
                            4 => {
                                // Warm pread window of the big blob.
                                let f = fs::File::open(ctx.on_mount(&big.path))
                                    .with_context(|| label("open big"))?;
                                let off = next(oracle.len() - 4096);
                                let mut buf = vec![0u8; 4096];
                                f.read_exact_at(&mut buf, off as u64)
                                    .with_context(|| label(&format!("pread big @ {off}")))?;
                                ensure!(
                                    buf == oracle[off..off + 4096],
                                    "{}",
                                    label(&format!("big-blob window @ {off} diverges"))
                                );
                            }
                            _ => {
                                let f = files[next(files.len())];
                                nix::unistd::eaccess(
                                    &ctx.on_mount(&f.path),
                                    nix::unistd::AccessFlags::R_OK,
                                )
                                .with_context(|| label(&format!("eaccess R_OK {}", f.path)))?;
                            }
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        for h in handles {
            h.join().expect("fsstress soak thread panicked")?;
        }
        anyhow::Ok(())
    })?;
    Ok(Outcome::Pass)
}

/// generic/003 + generic/192 (read-only adaptation): atime on an
/// immutable store tree is the canonical epoch+1s and NEVER moves —
/// not after reads, not after readdir. A drifting atime would make
/// builds non-deterministic (SOURCE_DATE_EPOCH tooling, tar timestamps)
/// and would mean the FUSE invented a writable attribute on a
/// read-only tree. Upstream's persistence-across-remount half is moot:
/// the mount is per-build and ephemeral.
pub fn generic_003_192_atime_stable(ctx: &Ctx) -> anyhow::Result<Outcome> {
    let f = ctx
        .manifest
        .files
        .iter()
        .find(|f| !f.content.is_empty())
        .context("manifest has no non-empty file")?;
    let path = ctx.on_mount(&f.path);

    let before = fs::symlink_metadata(&path)?;
    ensure!(
        before.atime() == 1,
        "{}: atime is {}s, expected the canonical store time of 1s",
        f.path,
        before.atime()
    );
    let _ = fs::read(&path)?;
    let after = fs::symlink_metadata(&path)?;
    ensure!(
        after.atime() == 1 && after.atime_nsec() == before.atime_nsec(),
        "{}: atime moved to {}s/{}ns after a read — attributes on the immutable tree must \
         never change",
        f.path,
        after.atime(),
        after.atime_nsec()
    );

    let dir = ctx.on_mount(&ctx.manifest.seq_dir.path);
    let d_before = fs::symlink_metadata(&dir)?;
    let _ = fs::read_dir(&dir)?.count();
    let d_after = fs::symlink_metadata(&dir)?;
    ensure!(
        d_after.atime() == d_before.atime(),
        "{}: directory atime moved after readdir",
        ctx.manifest.seq_dir.path
    );
    Ok(Outcome::Pass)
}

// ─── helpers ───────────────────────────────────────────────────────────

/// Recursive fd-relative walk with gnulib-fts-style ascent
/// verification: every child directory is entered through openat from
/// the parent fd, and after walking it, `openat(child_fd, "..")` must
/// fstat back to the parent's (dev,ino). Returns the number of nodes
/// (the dir itself + all descendants).
fn fts_walk(fd: &OwnedFd, self_id: (u64, u64), path: &Path) -> anyhow::Result<u64> {
    let mut count = 1u64;
    for (name, _) in RawDir::from_fd(fd.as_fd())?.entries()? {
        if name == b"." || name == b".." {
            continue;
        }
        let c_name = CString::new(name.clone()).context("entry name has interior NUL")?;
        let child_path = path.join(std::ffi::OsStr::from_bytes(&name));
        let st = nix::sys::stat::fstatat(
            fd,
            c_name.as_c_str(),
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .with_context(|| format!("fstatat {}", child_path.display()))?;
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            count += 1;
            continue;
        }
        let child_fd = openat(
            fd,
            c_name.as_c_str(),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("openat {}", child_path.display()))?;
        let child_st = fstat(&child_fd)?;
        count += fts_walk(&child_fd, (child_st.st_dev, child_st.st_ino), &child_path)?;
        // The fts safe-changedir analogue: ascend and verify.
        let up = fstat(openat(
            &child_fd,
            "..",
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )?)
        .with_context(|| format!("fstat .. of {}", child_path.display()))?;
        ensure!(
            (up.st_dev, up.st_ino) == self_id,
            "ascending '..' out of {} arrived at (dev={}, ino={}), expected its parent {} \
             (dev={}, ino={}) — a concurrent alias lookup re-parented the dentry (the GNU \
             fts ENOENT class)",
            child_path.display(),
            up.st_dev,
            up.st_ino,
            path.display(),
            self_id.0,
            self_id.1
        );
    }
    Ok(count)
}

/// First pair of content-identical directories under DIFFERENT
/// parents — the shape whose dentry the kernel re-parents across
/// subtrees on a digest-keyed FUSE.
fn cross_parent_alias_pair(ctx: &Ctx) -> anyhow::Result<(String, String)> {
    let parent_of = |rel: &str| rel.rfind('/').map(|i| rel[..i].to_owned());
    for group in ctx.manifest.alias_dir_groups() {
        for (i, a) in group.iter().enumerate() {
            for b in &group[i + 1..] {
                if parent_of(a) != parent_of(b) {
                    return Ok(((*a).to_owned(), (*b).to_owned()));
                }
            }
        }
    }
    bail!(
        "fixture has no content-identical directories under different parents; the dentry \
         re-parenting class is not covered"
    )
}

/// Depth-first visit of `dir` and every directory below it.
fn visit_dirs(
    dir: &Path,
    visit: &mut impl FnMut(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    visit(dir)?;
    for entry in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            visit_dirs(&entry.path(), visit)?;
        }
    }
    Ok(())
}

fn open_dir(path: &Path) -> anyhow::Result<OwnedFd> {
    open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("open({}, O_DIRECTORY)", path.display()))
}

/// Restores the saved working directory on drop (panic-safe), so a
/// failing getcwd check cannot leave later checks running from inside
/// the mount.
struct CwdGuard(OwnedFd);

impl CwdGuard {
    fn save() -> anyhow::Result<Self> {
        Ok(CwdGuard(open_dir(Path::new("."))?))
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        fchdir(&self.0).expect("restore working directory");
    }
}

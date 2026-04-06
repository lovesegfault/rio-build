//! FUSE `Filesystem` trait implementation for `NixStoreFs`.
//!
//! # Slow-path reachability (the five constraints)
//!
//! Each of `getattr`/`open`/`readlink`/`readdir` has a slow-path fallback:
//! fast-path `File::open`/`read_link`/etc ENOENTs → `ensure_cached` → retry.
//! These fallbacks are **structurally unreachable** in normal operation
//! because `lookup()` eagerly materializes the entire NAR on first
//! root-child access. By the time any other op fires for an inode under
//! that root, the file is on disk. Reaching the slow path for a test
//! requires fault-injecting around five interacting constraints:
//!
//! 1. **`ensure_cached` checks the SQLite index, not disk.** `rm -rf` the
//!    cache file with the index row intact → `ensure_cached` returns
//!    `Ok(path-that-doesn't-exist)`, slow path's `&& let Err(errno)` is
//!    false, second `File::open` still ENOENTs → "failed after
//!    ensure_cached" branch. (The §2.10 self-heal makes this work again
//!    in prod; tests bypass by deleting the *child* file, not the
//!    store-path root — the root stat in `ensure_cached` passes, the
//!    child stat here fails.)
//!
//! 2. **`ATTR_TTL = 3600s`** — kernel won't re-ask FUSE `getattr` within a
//!    short test. Attrs cached from a prior `ls -la`. BUT `open`/
//!    `readlink`/`readdir` don't consult the attr cache: kernel resolves
//!    path via cached dentry → calls FUSE op by inode. **`getattr`'s slow
//!    path stays dark within ATTR_TTL.** Mark COVERAGE-unreachable.
//!
//! 3. **`lookup`'s `ensure_cached` is root-only** (`parent == ROOT` gate).
//!    Delete a subdir → fresh `lookup(parent_ino, "subdir")` returns plain
//!    ENOENT without trying `ensure_cached`. So `echo 3 > drop_caches`
//!    BREAKS the test — must rely on cached dentries.
//!
//! 4. **`readdir` doesn't populate dcache; `ls -la`'s stat-per-entry
//!    does.** A file only seen in plain `ls` (no `-la`) has no dentry;
//!    the kernel re-looks-up on access → constraint 3 applies → plain
//!    ENOENT, not slow path. `ls -la` the parent first.
//!
//! 5. **Intervening worker restart clears the dentry cache.**
//!    `MountOption::AutoUnmount` + systemd `Restart=on-failure` → fresh
//!    mount → kernel drops ALL dentries. If an earlier subtest SIGKILLed
//!    the target worker, re-seed dentries (`ls -la`) *inside* the
//!    fault-inject subtest, immediately before the `rm`.
//!
//! **Tell for partial success:** journalctl shows a subset of expected
//! "failed after ensure_cached" warns; the ones that DID fire have low
//! inode numbers (fresh counter post-restart); the missing ones are for
//! files no build accesses by name after the restart.
//!
//! Pattern (VM test `testScript`):
//! ```text
//! ls -la /var/rio/fuse-store/<busybox>/   # seed dentries (c. 4, 5)
//! rm /var/rio/cache/<busybox>/bin/sh      # child of root, index untouched (c. 1)
//! # DO NOT drop_caches (c. 3)
//! cat /var/rio/fuse-store/<busybox>/bin/sh     → open slow path
//! # getattr: unreachable within ATTR_TTL (c. 2)
//! ```

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::sync::atomic::Ordering;

use fuser::{
    AccessFlags, Errno, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyIoctl,
    ReplyOpen, ReplyXattr, Request,
};

use super::NixStoreFs;
use super::cache::JitClass;
use super::fetch::jit_fetch_timeout;
use super::lookup::{ATTR_TTL, BLOCK_SIZE, stat_to_attr};
use super::read::{io_error_to_errno, read_file_range};

impl Filesystem for NixStoreFs {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> Result<(), io::Error> {
        // I-080: without FUSE_PARALLEL_DIROPS, fs/fuse/dir.c::fuse_lookup
        // takes fi->mutex on the PARENT inode (fuse_lock_inode) — every
        // root-level lookup serializes kernel-side. Under JIT fetch the
        // daemon's sandbox-setup lstat()s of /var/rio/fuse-store/{hash}-
        // name would queue uninterruptibly behind the 1 holder waiting on
        // userspace (block_on(gRPC)). n_threads=4 + fetch_sem=3 are
        // defeated. Our lookup/readdir are already concurrency-safe
        // (InodeMap RwLock + Cache singleflight + FetchSemaphore), so
        // letting the kernel issue concurrent dirops just moves the
        // bound from a kernel mutex to fetch_sem — observable, bounded
        // at fuse_threads-1, doesn't pile up uninterruptible waiters.
        //
        // Unconditional (not gated on passthrough): the serialization is
        // wrong regardless. It only became a hard stall after 0f54bd21
        // because that's when the FUSE INIT flag set actually changed —
        // before, fc->parallel_dirops AND fc->passthrough were both
        // false; now passthrough=true changes kernel-side request
        // accounting (fc->num_background tracking on /dev/fuse) enough
        // that the previously-tolerable serial warm becomes a stall.
        if let Err(unsupported) = config.add_capabilities(fuser::InitFlags::FUSE_PARALLEL_DIROPS) {
            tracing::warn!(
                ?unsupported,
                "kernel lacks FUSE_PARALLEL_DIROPS; root-level lookups will serialize"
            );
        }
        if self.passthrough {
            // BOTH calls required: add_capabilities puts FUSE_PASSTHROUGH
            // in config.requested (the INIT reply flags); set_max_stack_
            // depth populates the reply's depth field, but the kernel
            // ignores it unless the flag is also set. Without the flag,
            // fc->passthrough=false → FUSE_DEV_IOC_BACKING_OPEN → EPERM
            // (the original I-061 finding).
            //
            // depth=1: I-060's chroot-store puts an overlay on top of
            // FUSE (lowerdir=fuse-store). The kernel sets FUSE's
            // sb->s_stack_depth = max_stack_depth; overlay-on-top is
            // s_stack_depth+1. FILESYSTEM_MAX_STACK_DEPTH is 2, so
            // depth=1 is the maximum that lets the overlay mount
            // (depth=2 → "overlayfs: maximum fs stacking depth
            // exceeded" at the per-build mount).
            if let Err(unsupported) = config.add_capabilities(fuser::InitFlags::FUSE_PASSTHROUGH) {
                tracing::warn!(?unsupported, "kernel lacks FUSE_PASSTHROUGH; disabling");
                self.passthrough = false;
            } else if let Err(max) = config.set_max_stack_depth(1) {
                tracing::warn!(max, "kernel rejected max_stack_depth=1; disabling");
                self.passthrough = false;
            } else {
                tracing::info!("FUSE passthrough enabled (max_stack_depth=1)");
            }
        }
        Ok(())
    }

    // COVERAGE: destroy() only fires on clean umount — main()
    // drops BackgroundSession on the normal-return path, which
    // unmounts, which triggers this. VM tests `systemctl stop` for
    // graceful shutdown so it SHOULD run, but empirically stays 0:
    // fuser's background thread is detached (no join), so the
    // umount→destroy path races atexit's profraw flush. The
    // passthrough-failures rollup is best-effort anyway.
    fn destroy(&mut self) {
        let failures = self.passthrough_failures.load(Ordering::Relaxed);
        if failures > 0 {
            tracing::warn!(
                count = failures,
                "passthrough open_backing failed for some files"
            );
        }
    }

    // COVERAGE: forget() fires when the kernel evicts inodes
    // under memory pressure. VM tests allocate ≥6GB and touch a
    // handful of store paths — no pressure, no eviction. Would
    // need a smaller-VM-RAM fixture + a cache-filling stress
    // build to hit. The ino-map cleanup is defensive (leaks
    // without it, but the mount is ephemeral anyway).
    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let mut map = self.inodes_write();
        if map.forget(ino.0, nlookup) {
            tracing::trace!(ino = ino.0, "forgot inode");
        }
    }

    // r[impl builder.fuse.lookup-caches+2]
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.real_path(parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let child_path = parent_path.join(name);

        // Check local cache first. Distinguish NotFound (fall through to
        // remote query) from other I/O errors (EACCES on corrupt perms,
        // EIO on disk failure): treating all errors as cache miss triggers
        // a re-fetch on every lookup, amplifying network + masking root cause.
        match child_path.symlink_metadata() {
            Ok(meta) => {
                let ino = self.get_or_create_inode_for_lookup(child_path);
                let attr = stat_to_attr(ino, &meta);
                reply.entry(&ATTR_TTL, &attr, Generation(0));
                metrics::counter!("rio_builder_fuse_cache_hits_total").increment(1);
                return;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Fall through to remote query below.
            }
            Err(e) => {
                tracing::warn!(
                    path = %child_path.display(),
                    error = %e,
                    "lookup: local symlink_metadata failed with non-ENOENT error"
                );
                reply.error(io_error_to_errno(&e));
                return;
            }
        }

        // For top-level entries (direct children of mount point), JIT-fetch
        // iff the name is a registered build input.
        if parent.0 == INodeNo::ROOT.0 {
            // Non-UTF-8 store basenames are invalid (nix enforces UTF-8);
            // reject with ENOENT rather than lossy-decode into a wrong path.
            let Some(name_str) = name.to_str() else {
                reply.error(Errno::ENOENT);
                return;
            };
            // r[impl builder.fuse.jit-lookup]
            // Pure allowlist: classify against the per-build registered
            // input set. The set IS the declared closure — exact
            // membership is sufficient and stronger than shape-matching.
            // Only names in the set trigger a store fetch; EVERYTHING
            // else (daemon `.lock`/`.chroot`/`.check` probes, output-
            // path pre-checks, `.links`, `eee…ee` invalid-hash probes)
            // gets fast ENOENT WITHOUT contacting the store. Replaces
            // the I-115 suffix denylist + len<34 heuristic — the ~35k
            // pointless GetPath/stress-run drops to zero.
            //
            // Hermeticity bonus: builds cannot read store paths outside
            // their declared inputs (ENOENT, same as on a clean machine
            // — except for paths an earlier build on this STS pod left
            // in cache_dir, which the local-disk fast path above
            // returned before reaching here).
            match self.cache.jit_classify(name_str) {
                JitClass::NotInput => {
                    metrics::counter!(
                        "rio_builder_fuse_jit_lookup_total",
                        "outcome" => "reject"
                    )
                    .increment(1);
                    // Fall through to final ENOENT reply (no store contact).
                }
                JitClass::KnownInput { nar_size } => {
                    // Materialize on lookup (not deferred to getattr/open/
                    // readdir): the kernel caches the lookup attr for
                    // ATTR_TTL and NEVER calls getattr. A synthetic
                    // "exists, details later" attr would mean
                    // `lookup(busybox_ino, "bin")` hits an empty cache_dir
                    // → ENOENT → build fails. Fetching here ensures the
                    // whole tree is on disk before any child lookup.
                    //
                    // I-178: size-aware timeout — the daemon's `lstat`
                    // blocks in `request_wait_answer` for up to this
                    // duration on a cold input, so a flat 60 s aborts a
                    // 1.9 GB input mid-stream.
                    let timeout = jit_fetch_timeout(self.fetch_timeout, nar_size);
                    match self.ensure_cached_with_timeout(name_str, timeout) {
                        Ok(local_path) => match local_path.symlink_metadata() {
                            Ok(meta) => {
                                let ino = self.get_or_create_inode_for_lookup(child_path);
                                let attr = stat_to_attr(ino, &meta);
                                metrics::counter!(
                                    "rio_builder_fuse_jit_lookup_total",
                                    "outcome" => "fetch"
                                )
                                .increment(1);
                                reply.entry(&ATTR_TTL, &attr, Generation(0));
                                return;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    path = %local_path.display(),
                                    error = %e,
                                    "ensure_cached succeeded but stat failed"
                                );
                                reply.error(io_error_to_errno(&e));
                                return;
                            }
                        },
                        Err(errno) => {
                            // I-043 redesign load-bearing change: NEVER return
                            // ENOENT for a registered input. Overlayfs
                            // `ovl_lookup` propagates a lower's non-ENOENT
                            // error to the caller WITHOUT caching a negative
                            // dentry; ENOENT would be cached and the daemon's
                            // retry would never re-ask FUSE → "build input
                            // does not exist" → MiscFailure → poison.
                            //
                            // I-179: `wait_for_fetcher` now returns EIO
                            // directly on guard-drop-with-cache-empty; this
                            // remap is defense-in-depth (catches any future
                            // ENOENT leak from `ensure_cached`). The legacy
                            // `NotArmed` arm preserves its ENOENT semantics
                            // unchanged.
                            // I-189: log errno symbolically (`EIO`/`ENOENT`/
                            // `EAGAIN`) not the integer. The underlying gRPC
                            // status was already error!-logged in
                            // fetch_extract_insert; correlate by store_path.
                            tracing::error!(
                                store_path = name_str,
                                errno = ?errno,
                                nar_size,
                                timeout_secs = timeout.as_secs(),
                                "JIT fetch of known input failed → EIO \
                                 (overlay must not negative-cache; see \
                                 preceding error! for underlying cause)"
                            );
                            metrics::counter!(
                                "rio_builder_fuse_jit_lookup_total",
                                "outcome" => "eio"
                            )
                            .increment(1);
                            reply.error(Errno::EIO);
                            return;
                        }
                    }
                }
                JitClass::NotArmed => {
                    // JIT not armed: tests, `RIO_BUILDER_JIT_FETCH=0`, or
                    // the pre-register window. LEGACY behavior — gRPC any
                    // store-path-shaped name. Keep the original len/dot
                    // heuristic + I-115 suffix denylist here (the JIT-
                    // armed arms above use neither — pure allowlist).
                    if name_str.len() < 34
                        || name_str.starts_with('.')
                        || name_str.ends_with(".chroot")
                        || name_str.ends_with(".lock")
                        || name_str.ends_with(".check")
                    {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    match self.ensure_cached(name_str) {
                        Ok(local_path) => match local_path.symlink_metadata() {
                            Ok(meta) => {
                                let ino = self.get_or_create_inode_for_lookup(child_path);
                                let attr = stat_to_attr(ino, &meta);
                                reply.entry(&ATTR_TTL, &attr, Generation(0));
                                return;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    path = %local_path.display(),
                                    error = %e,
                                    "ensure_cached succeeded but stat failed"
                                );
                                reply.error(io_error_to_errno(&e));
                                return;
                            }
                        },
                        Err(errno) if i32::from(errno) == i32::from(Errno::ENOENT) => {
                            // Not in remote store — fall through to final ENOENT reply.
                        }
                        Err(errno) => {
                            // Transport/server/extract error — surface it, don't mask as ENOENT.
                            reply.error(errno);
                            return;
                        }
                    }
                }
            }
        }

        // ENOENT is normal for probing. Non-UTF-8 names can't match
        // the ASCII ".Trash" literals anyway — to_str None → log it.
        let name_str = name.to_str().unwrap_or("");
        if name_str != ".Trash" && name_str != ".Trash-0" {
            tracing::trace!(parent = parent.0, name = ?name, "lookup: not found");
        }
        reply.error(Errno::ENOENT);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match path.symlink_metadata() {
            Ok(meta) => {
                let attr = stat_to_attr(ino.0, &meta);
                reply.attr(&ATTR_TTL, &attr);
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(
                        ino = ino.0,
                        path = %path.display(),
                        error = %e,
                        "getattr failed"
                    );
                }
                // COVERAGE: unreachable within ATTR_TTL (3600s). Constraint 2
                // in the module doc — the kernel caches attrs from lookup()'s
                // reply.entry and never re-asks getattr for that inode within
                // a VM test's runtime. open/readlink/readdir don't consult the
                // attr cache (they route by cached dentry → FUSE op by inode)
                // so THEIR slow paths are reachable; this one is not. Kept as
                // belt-and-suspenders for the post-TTL case and ATTR_TTL=0
                // debug builds.
                if let Some(basename) = self.store_basename_for_inode(ino.0) {
                    match self.ensure_cached(&basename) {
                        Ok(_) => match path.symlink_metadata() {
                            Ok(meta) => {
                                let attr = stat_to_attr(ino.0, &meta);
                                reply.attr(&ATTR_TTL, &attr);
                            }
                            Err(fresh) => {
                                // Cache says it materialized but the stat
                                // STILL fails — reply with the fresh error,
                                // not the pre-cache `e` (which is stale and
                                // misleading; typically ENOENT when the real
                                // post-cache failure might be EACCES/EIO).
                                reply.error(io_error_to_errno(&fresh));
                            }
                        },
                        Err(errno) => {
                            reply.error(errno);
                        }
                    }
                    return;
                }
                reply.error(io_error_to_errno(&e));
            }
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // Fast path: if the symlink exists on disk, no need to touch the cache.
        // lookup() already materialized the whole store-path tree on first
        // access, so this is the common case.
        match fs::read_link(&path) {
            Ok(target) => {
                reply.data(target.as_os_str().as_bytes());
                return;
            }
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                tracing::warn!(
                    ino = ino.0,
                    path = %path.display(),
                    error = %e,
                    "readlink failed"
                );
                reply.error(io_error_to_errno(&e));
                return;
            }
            Err(_) => {} // ENOENT — fall through to ensure_cached
        }

        // Slow path: materialize then retry.
        if let Some(basename) = self.store_basename_for_inode(ino.0)
            && let Err(errno) = self.ensure_cached(&basename)
        {
            reply.error(errno);
            return;
        }

        match fs::read_link(&path) {
            Ok(target) => reply.data(target.as_os_str().as_bytes()),
            Err(e) => {
                tracing::warn!(
                    ino = ino.0,
                    path = %path.display(),
                    error = %e,
                    "readlink failed after ensure_cached"
                );
                reply.error(io_error_to_errno(&e));
            }
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // Fast path: try to open directly. lookup() already materialized the
        // store-path tree, so this is the common case and skips a gratuitous
        // ensure_cached() SQLite write.
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Slow path: materialize then retry.
                if let Some(basename) = self.store_basename_for_inode(ino.0)
                    && let Err(errno) = self.ensure_cached(&basename)
                {
                    reply.error(errno);
                    return;
                }
                match File::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(
                            ino = ino.0,
                            path = %path.display(),
                            error = %e,
                            "open failed after ensure_cached"
                        );
                        reply.error(io_error_to_errno(&e));
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    ino = ino.0,
                    path = %path.display(),
                    error = %e,
                    "open failed"
                );
                reply.error(io_error_to_errno(&e));
                return;
            }
        };

        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);

        // FUSE_DEV_IOC_BACKING_OPEN EPERMs on directory fds. open() on
        // the FUSE root (ino=2) succeeds in File::open above but then
        // open_backing fails, logging a spurious warning on every mount.
        // Skip passthrough for non-regular files; the standard read()
        // path handles them fine. metadata() failure is treated
        // conservatively as "not a regular file" — fallback is cheap.
        let is_regular_file = file.metadata().is_ok_and(|m| m.is_file());

        if self.passthrough && is_regular_file {
            // BackingId is per-INODE, not per-fh: the kernel rejects a second
            // passthrough open of the same inode with a different fuse_backing
            // (-EBUSY → caller sees EIO). See BackingState doc.
            match self
                .backing_state_write()
                .get_or_open(ino.0, fh, &file, &reply)
            {
                Ok(backing_id) => {
                    reply.opened_passthrough(FileHandle(fh), FopenFlags::empty(), &backing_id);
                    // The backing read path doesn't need `file` (kernel has its
                    // own fd via the BackingId); drop ours.
                }
                Err(e) => {
                    let count = self.passthrough_failures.fetch_add(1, Ordering::Relaxed);
                    if count == 0 {
                        tracing::warn!(
                            ino = ino.0,
                            error = %e,
                            "passthrough open_backing failed, falling back to standard read"
                        );
                    }
                    // Cache for standard read() path.
                    self.open_files_write().insert(fh, file);
                    reply.opened(FileHandle(fh), FopenFlags::empty());
                }
            }
        } else {
            // Non-passthrough: cache the open file for read().
            self.open_files_write().insert(fh, file);
            reply.opened(FileHandle(fh), FopenFlags::empty());
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        // Use the file cached by open(). pread (read_at) is stateless,
        // so concurrent reads on the same fh are safe.
        let files = self.open_files_read();
        let Some(file) = files.get(&fh.0) else {
            // fh not in open_files — passthrough is handling it, or open()
            // was never called (shouldn't happen). Fall back to path-based.
            drop(files);
            let Some(path) = self.real_path(ino.0) else {
                reply.error(Errno::ENOENT);
                return;
            };
            match File::open(&path).and_then(|f| read_file_range(&f, offset, size as usize)) {
                Ok(data) => reply.data(&data),
                Err(e) => {
                    tracing::warn!(ino = ino.0, error = %e, "read fallback failed");
                    reply.error(io_error_to_errno(&e));
                }
            }
            return;
        };

        match read_file_range(file, offset, size as usize) {
            Ok(data) => {
                // Userspace read succeeded. When passthrough is ON, the
                // kernel handles most reads directly and this callback
                // rarely fires (only for files where open_backing failed).
                // When passthrough is OFF (RIO_FUSE_PASSTHROUGH=false),
                // every read comes through here. Near-zero vs nonzero
                // is the signal that the non-passthrough path ran.
                metrics::counter!("rio_builder_fuse_fallback_reads_total").increment(1);
                reply.data(&data);
            }
            Err(e) => {
                tracing::warn!(
                    ino = ino.0,
                    offset,
                    size,
                    error = %e,
                    "read failed"
                );
                reply.error(io_error_to_errno(&e));
            }
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        // Drop this fh's strong ref on the BackingId; the kernel-side close
        // (FUSE_DEV_IOC_BACKING_CLOSE via BackingId::Drop) fires when the
        // last fh for the inode releases. Non-passthrough fh: drop the
        // cached File.
        self.backing_state_write().release(fh.0);
        self.open_files_write().remove(&fh.0);
        reply.ok();
    }

    // Coverage note: NOT reached via the per-build overlay. `ls ${dep}/`
    // inside a build sandbox (overlay lower = this FUSE mount) returns the
    // correct full listing — verified by scheduling.nix overlay-readdir-
    // correctness (5-file dep, cold dcache, count=5 asserted) — but ops.rs
    // readdir stays at 0 hits. overlayfs ovl_iterate() on a pure-lower dir
    // gets the listing without a FUSE_READDIR round-trip to userspace
    // (mechanism unconfirmed; not a correctness issue). Exercised directly
    // by scheduling.nix fuse-direct: `ls /var/rio/fuse-store/` on the mount
    // point with NO overlay in the path.
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(dir_path) = self.real_path(ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // Fast path: try read_dir directly. lookup() materialized the tree.
        let entries = match fs::read_dir(&dir_path) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Slow path: materialize then retry.
                if ino.0 != INodeNo::ROOT.0
                    && let Some(basename) = self.store_basename_for_inode(ino.0)
                    && let Err(errno) = self.ensure_cached(&basename)
                {
                    reply.error(errno);
                    return;
                }
                match fs::read_dir(&dir_path) {
                    Ok(rd) => rd,
                    Err(e) => {
                        tracing::warn!(
                            ino = ino.0,
                            path = %dir_path.display(),
                            error = %e,
                            "readdir failed after ensure_cached"
                        );
                        reply.error(io_error_to_errno(&e));
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    ino = ino.0,
                    path = %dir_path.display(),
                    error = %e,
                    "readdir failed"
                );
                reply.error(io_error_to_errno(&e));
                return;
            }
        };

        // Stream entries directly to reply.add() with an offset counter,
        // breaking early when the kernel's buffer fills. Avoids collecting
        // ALL entries into a Vec + .skip(offset) — that would be O(n) alloc
        // per resume call.
        let mut idx: u64 = 0;
        if offset < 1 && reply.add(ino, 1, FileType::Directory, ".") {
            reply.ok();
            return;
        }
        idx = idx.max(1);
        if offset < 2 {
            let parent_ino = self.inodes_read().parent_inode(&dir_path);
            if reply.add(INodeNo(parent_ino), 2, FileType::Directory, "..") {
                reply.ok();
                return;
            }
        }
        idx = idx.max(2);

        for result in entries {
            let entry = match result {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(ino = ino.0, error = %e, "skipping unreadable dir entry");
                    continue;
                }
            };
            idx += 1;
            if idx <= offset {
                continue; // skip entries before resume point
            }

            let name = entry.file_name();
            let child_path = dir_path.join(&name);

            let kind = match entry.file_type() {
                Ok(ft) if ft.is_dir() => FileType::Directory,
                Ok(ft) if ft.is_symlink() => FileType::Symlink,
                Ok(_) => FileType::RegularFile,
                Err(e) => {
                    tracing::warn!(
                        path = %child_path.display(),
                        error = %e,
                        "skipping entry with unknown file type"
                    );
                    continue;
                }
            };

            let child_ino = self.get_or_ephemeral_inode(&child_path);
            if reply.add(INodeNo(child_ino), idx, kind, &name) {
                break; // buffer full — kernel will re-call with offset
            }
        }
        reply.ok();
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: AccessFlags, reply: fuser::ReplyEmpty) {
        if self.real_path(ino.0).is_some() {
            reply.ok();
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, BLOCK_SIZE, 255, 0);
    }

    // ── No-data stubs ─────────────────────────────────────────────────
    // fuser's defaults return ENOSYS + log at WARN. NAR-unpacked content
    // has no xattrs and no chattr flags, and the FS is read-only — these
    // implementations ARE the truthful answer, not placeholders. ENOSYS
    // (the default) and ENODATA/ENOTTY (here) are handled identically by
    // overlayfs and nix-daemon's canonicalisePathMetaData; the explicit
    // errnos just stop the per-call WARN spam (113× ioctl per build with
    // chroot-store's FS_IOC_GETFLAGS probe).

    fn getxattr(&self, _: &Request, _: INodeNo, _: &std::ffi::OsStr, _: u32, reply: ReplyXattr) {
        reply.error(Errno::ENODATA);
    }

    fn listxattr(&self, _: &Request, _: INodeNo, _: u32, reply: ReplyXattr) {
        reply.size(0);
    }

    fn flush(&self, _: &Request, _: INodeNo, _: FileHandle, _: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    #[allow(clippy::too_many_arguments)]
    fn ioctl(
        &self,
        _: &Request,
        _: INodeNo,
        _: FileHandle,
        _: fuser::IoctlFlags,
        _: u32,
        _: &[u8],
        _: u32,
        reply: ReplyIoctl,
    ) {
        reply.error(Errno::ENOTTY);
    }
}

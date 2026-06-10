# xfstests → castore-FUSE port plan

Target: the **castore-FUSE** (`rio-builder/src/castore_fuse/`, ADR-022 §2)
— the per-build, read-only, content-addressed FUSE mount served through
rio-mountd (`Mount{build_id}` fd handoff, mounted at
`/var/rio/castore/<build-id>` with `allow_other,default_permissions`,
`MS_NODEV|MS_NOSUID`). Metadata (`lookup`/`getattr`/`readdir`/`readlink`)
is answered from the in-heap Directory-DAG (`tree::InoMap`, infinite
TTLs, content-derived inode numbers); `open()` brokers a passthrough fd
to the node-SSD backing cache (`open.rs`), falling back to
`FOPEN_KEEP_CACHE` userspace reads during streaming fills or when
passthrough is disabled. The executor stacks each build's overlay on the
mount as its only lowerdir.

Surveyed: `tests/generic/` of the xfstests mirror
(`~/git/xfstests-dev`, github.com/kdave/xfstests, 793 generic tests).
This plan supersedes the earlier selection that targeted the deleted
JIT-FUSE (`rio-builder/src/fuse/`, `/var/rio/fuse-store`); the test
*selection* largely carries over, the port strategies and oracles do
not.

## Old-FUSE assumptions dropped

* **Oracle**: the old ports compared the mount against
  `/var/rio/cache/<store-path>/…` (the per-path NAR-extract tree). The
  castore backing cache is keyed by *file digest*, not store path —
  there is no per-path extracted tree to diff against. New oracle: the
  fixture derivation's known constants (contents, sizes, modes, names,
  symlink targets are all fixed by the build script), a locally
  regenerated copy of the deterministic blob for ranged-read
  comparisons, and the content-addressed identities themselves
  (digest → inode → bytes).
* **Mount location / harness**: assertions ran against a worker's
  process-level `/var/rio/fuse-store` populated by JIT fetches during a
  scheduled build. The castore mount is per-build and assembled by
  `castore_fuse::session::mount_and_serve`; the VM harness drives the
  production assembly directly via `spike_mountd_client serve-castore`
  on the client VM (the `vm-castore-fuse` pattern), plus one dispatched
  consumer build for the overlay-lowerdir leg.
* **EROFS (generic/050)**: the old mount was `MountOption::RO`, so every
  mutation was EROFS. The castore mount now carries MS_RDONLY as well:
  the kernel's mnt_want_write answers EROFS for every mutation before
  the root-owned 0444/0555 mode bits or the daemon are consulted, and
  the daemon still answers EROFS for any write op that reaches it
  (`builder.fs.open-read-only`, `builder.fs.write-ops-erofs`). The
  unprivileged leg asserts "mutations by the build uid fail (EROFS)
  and the tree is unchanged"; the root-credential legs assert the same
  EROFS contract (see Findings).
* **access(2) mask divergence (old finding F-1)**: the old FUSE answered
  `access()` itself and ignored the mask. The castore mount sets
  `default_permissions`, so the kernel evaluates access(2) from the
  served modes and never upcalls — the divergence has no analogue.
  Asserted as correct behavior (P1 #7).
* **readdir d_ino ≠ st_ino (old finding F-2)**: the old readdir invented
  hash-based ephemeral inos distinct from lookup's. Castore readdir
  derives the same content-addressed ino as lookup, so d_ino == st_ino
  is asserted (P1 #1/#13). Exception (documented design choice in
  `tree.rs`): `.`/`..` entries point at the directory itself — a
  content-addressed dir has no unique parent — so dot entries are
  excluded from the d_ino assertions.
* **statfs zeros (old finding F-3)**: still present — `CastoreFs` does
  not implement `statfs`, so fuser's default replies all-zero
  block/file counts. Logged as a finding (P1 #14 asserts only that
  statfs/`df` succeed); see Findings below.
* **nlink mirroring (generic/002)**: the old port compared dir nlink
  against the backing tree. Castore has no backing tree and reports
  nlink=1 for every node (the btrfs-style "no subdir link counts"
  choice). Adapted to nlink==1 plus a structural `find` completeness
  check (the practical failure mode of bogus nlink is `find`/`fts`
  pruning subdirectories).

## Harness

Everything is Rust calling into Rust; the VM's Python testScript holds
no assertion logic (shell probes proved unreliable — GNU `rm` prompts
on write-protected files instead of failing — and cannot pin errnos).

* **Runner** — `rio-builder/src/bin/xfstests_runner/` (a `[[bin]]`
  with the same NOT-production status as `spike_mountd_client`). Every
  kernel-visible port is a Rust check function named after its origin
  (`generic_257_*`, …) that asserts through direct syscalls against
  the live FUSE mountpoint (`--mount`), with the expected-content JSON
  manifest (`--manifest`) as the only oracle. Per-check PASS/FAIL/SKIP
  lines + FINDING lines for documented POSIX divergences; exit code =
  number of failures. `--list` enumerates checks, `--filter` runs a
  subset (debugging only — full-suite order is load-bearing for the
  cold-vs-warm read distinction).
* **VM** — `nix/tests/scenarios/castore-fuse-xfstests.nix`, wired as
  `vm-castore-xfstests` (standalone fixture + the `vm-castore-fuse`
  client module: rio-mountd, XFS-prjquota staging loopback, builder
  uid/gid). The testScript is a thin harness: it builds the fixture
  derivation `nix/tests/lib/derivations/xfstests-tree.nix` in-VM
  through the gateway (NAR ingest → castore Directory DAG + blobs),
  assembles the `serve-castore` mount with a scenario-minted tenant +
  assignment token, runs `xfstests_runner` against
  `/var/rio/castore/xfs`, and tears down. The scenario also generates
  the runner's manifest (`pkgs.writeText`) from the same literals as
  the fixture build script. A consumer build dispatched to the worker
  lists the 200-entry dir through the production overlay-over-castore
  stack (the overlay-lowerdir leg); its output file is handed to the
  runner's `overlay_readdir_consumer` check.
  Run: `/nixbuild .#checks.x86_64-linux.vm-castore-xfstests`.
* **Rust unit tier** — `rio-builder/tests/xfstests_port.rs`: ports
  whose intent reduces to the userspace contract of `tree::InoMap`
  (readdir offset/resume bookkeeping, byte-exact name lookup,
  kind/size/ino derivation) run as integration tests against
  `InoMap::from_parts`, no mount needed. The runner's manifest/oracle
  module has its own unit tests (oracle byte-stream regeneration,
  symlink-resolution classification, walk-count arithmetic).
  Run: `nix develop -c cargo nextest run -p rio-builder -E 'binary(xfstests_port) or binary(xfstests_runner)'`.

A real mount needs `/dev/fuse`, mount privileges, and a kernel with
FUSE_PASSTHROUGH — none of which exist in the dev shell or the Nix
build sandbox — so every kernel-visible behavior goes to the runner
inside the VM tier.

## Tiers

* **P1** — directly guards a real failure mode of this implementation;
  ported in this batch (15 entries: 13 VM subtests + 2 Rust test groups).
* **P2** — relevant, port next; needs a small addition first (extra
  package in the client VM, a getdents/statx/mmap helper, or an
  unprivileged-user variant) noted in the strategy column.
* **P3** — partial relevance or low expected yield; port after P2 or
  keep as a documented exclusion if the listed caveat holds.

## Ranked selection (50)

| # | xfstests | Exercises | Relevance to castore-FUSE | Port strategy | Tier | Status |
|---|----------|-----------|---------------------------|---------------|------|--------|
| 1 | generic/257 | readdir d_off uniqueness + resumability (t_dir_offset2) | `InoMap::readdir` hand-rolls offset bookkeeping (`enumerate().skip(offset)`); duplicated/skipped entries on multi-batch listings is exactly its failure mode. 200-entry dir forces several FUSE_READDIR(PLUS) round-trips; the consumer build re-checks the same dir through the overlay lowerdir. d_ino == st_ino asserted (entries carry the content-derived ino). | VM `castore-fuse-xfstests.nix` (direct + overlay leg); Rust exhaustive offset-resume test | P1 | ported |
| 2 | generic/401 | d_type / file kind reported by readdir+stat | Wrong kinds break `find -type`, symlink handling, and overlay copy-up decisions over the lower. Kind classification lives in `InoMap::attr` and `readdir`'s per-entry `FileType`. | VM (stat %F + `find -type` over getdents d_type); Rust readdir-kind test | P1 | ported |
| 3 | generic/002 | inode link counts | Castore reports nlink=1 for everything (no backing tree). The practical risk is `find`/`fts` nlink-based subdir pruning miscounting. | VM: nlink==1 + structural `find` completeness count; Rust attr check | P1 | ported (adapted) |
| 4 | generic/005 | symlink traversal limits (ELOOP) | Kernel walks loops over `readlink` replies from the in-heap tree; a wrong reply turns ELOOP into build-visible misbehavior. Dangling-symlink deref = ENOENT while readlink still works. | VM | P1 | ported |
| 5 | generic/360 | symlink with very long target | `readlink` must return the full target and `getattr` must report size==strlen(target) (`Node::Symlink` size derivation); store trees are full of long relative symlinks. | VM; Rust (attr size leg) | P1 | ported |
| 6 | generic/453 | lookalike/arbitrary-byte filenames stay distinct | `lookup` is a byte-exact scan of the Directory body's three lists — no normalization/truncation allowed. NFC vs NFD names, a space name, and a 255-byte (NAME_MAX) name must resolve, list, and stay distinct through NAR ingest → castore → FUSE. | VM; Rust byte-exact lookup test | P1 | ported |
| 7 | generic/126 | file permission bits / exec enforcement | The executable bit decides whether builds can exec their inputs; it is the only mode bit castore preserves (0444 vs 0555). Exec of a non-executable file must fail; with `default_permissions` the kernel must answer access(2) honestly from the served modes (the old F-1 divergence must NOT exist here). | VM (exec + access-mask probes as the unprivileged builder uid) | P1 | ported |
| 8 | generic/050 | write protection of the input mount | The mount carries MS_RDONLY (plus root-owned 0444/0555 + `default_permissions`). Every mutation by the build uid must fail EROFS and leave the tree unchanged — a regression here lets builds scribble on (or appear to scribble on) shared inputs. | VM (EROFS, content unchanged) | P1 | ported (adapted) |
| 9 | generic/294 | EEXIST (not the permission error) when creating existing entries | VFS resolves the existing dentry through `lookup` before the write-permission failure; "rejected with the right errno" check. | VM | P1 | ported |
| 10 | generic/007 | errno sanity for missing names (nametest) | Names outside the prefetched closure are answered with a *cached negative entry* — must surface as ENOENT, never EIO, never a stall on a store fetch (the closure is the allowlist). Repeated probes of the same missing name must keep being ENOENT (negative-dentry cache). | VM | P1 | ported |
| 11 | generic/075 | fsx — read integrity | Read-only adaptation: whole-file digest through the mount equals the locally regenerated oracle, on both the cold path (ReadBlob/streaming fill) and the warm path (shared-cache passthrough). Guards `Opener::open`/`fetch_and_promote`/`FillState::read_at` end to end. | VM | P1 | ported |
| 12 | generic/091 | fsx — sub-block / odd-offset reads | Ranged reads at non-aligned offsets and at/past EOF (short read, empty read) — the off-by-one classes in the streaming `read_at` window and the passthrough fd. Odd-sized (1300003 B) blob crosses chunk and page boundaries. | VM (folded with #11) | P1 | ported |
| 13 | generic/095 | concurrent mixed I/O on the same files | 8 parallel whole-file readers must agree with the oracle: guards the per-digest singleflight (`fills`), shared `FillState` joins, and concurrent passthrough opens. Plus the castore-specific check that identical bytes share one inode and the exec-bit splits it (content-addressed identity). | VM | P1 | ported |
| 14 | generic/113 | aio-stress | Folded with #13 as repeated open/close cycles of one file — guards the per-digest `BackingRef` refcount reuse (the kernel EBUSYs a re-registration of a different backing id for the same inode). True io_uring/AIO readers are the P3 extension. | VM | P1 | ported (sync legs) |
| 15 | generic/310 | concurrent readdir vs read on one directory | Races between `readdir` and `open/read` on the same subtree must not error or wedge (InoMap is immutable; the contention is in `Opener`'s maps and fuser's thread pool). | VM (folded) | P1 | ported |
| 16 | generic/020 | get/list extended attributes | Castore deliberately answers getxattr→ENODATA, listxattr→empty (overlay probes `user.overlay.*` on every lower inode; the EIO trap is documented on `XattrListReply`). Port pins those errnos from the kernel side; setfattr must fail. | Rust `xattr_statx::generic_020_062_097_xattr_read_legs` (native `lgetxattr`/`llistxattr` — no `pkgs.attr` needed) | P2 | ported (read legs: getxattr→ENODATA, listxattr→empty, across file/dir/symlink) |
| 17 | generic/062 | getfattr/setfattr across object types (file/dir/symlink) | Same as #16 across node kinds, incl. trusted/system namespaces returning ENODATA (not EPERM/ENOTSUP surprises) — exactly what `ovl_copy_xattr` sees during copy-up. | Rust, with #16 (probes `user.*` + `trusted.overlay.*` on each kind) | P2 | ported (folded into #16) |
| 18 | generic/097 | basic EA set/get/list/remove | Read-side legs only (list empty, get ENODATA); write legs are failure checks. | Rust, with #16 | P2 | ported (read legs; write legs are covered by the EROFS battery) |
| 19 | generic/423 | statx field correctness | statx mask handling, stx_{mode,size,nlink,ino,blocks,btime} consistency with stat; FUSE answers via the same getattr/readdirplus attrs. | Rust `xattr_statx::generic_423_statx_field_correctness` (native `statx(2)` via libc — no python); generic/532 (attributes ⊆ attributes_mask) folded in as `generic_532_statx_attributes_mask_sanity` | P2 | ported (statx vs lstat agreement on mode/size/ino/nlink/blocks/mtime across all node kinds; mask completeness; 532 subset invariant) |
| 20 | generic/285 | SEEK_DATA/SEEK_HOLE sanity | No lseek implementation → ENOSYS → kernel default treats the whole file as data; that minimum must still be POSIX-conformant (SEEK_HOLE→size, SEEK_DATA<size→offset). Read-only subset. | VM (xfs_io is already in the client via xfsprogs) | P2 |ported (`io_paths::generic_285_448_706_seek_hole_data`); no-holes adaptation SEEK_HOLE to size |
| 21 | generic/448 | SEEK_HOLE/DATA with negative offsets | Error-path leg of #20 (EINVAL/ENXIO), cheap to fold in. | VM (with #20) | P2 |ported (with #20; ENXIO/EINVAL error legs) |
| 22 | generic/706 | SEEK_DATA on a 1-byte file returns 0 | Degenerate-size leg of #20. | VM (with #20) | P2 |ported (with #20; smallest non-empty file stands in for the 1-byte case) |
| 23 | generic/471 | rewinddir POSIX semantics | rewinddir on an open fd re-yields the full identical listing (offset-0 reset against FOPEN_CACHE_DIR'd dirent pages); a rewind after a partial read restarts at the first entry. The readdir helper is the runner's own `libc` dir stream (`dir_locks.rs`), not a shell/python helper. | VM (`dir_locks::generic_471_rewinddir`) | P2 | ported |
| 24 | generic/676 | seekdir to valid and invalid positions | `InoMap::readdir` trusts the kernel-provided resume offset; arbitrary seekdir offsets must produce a sane (possibly empty) listing, never EIO/panic/duplicates. The Rust exhaustive-offset test covers in-range offsets; this adds the glibc telldir/seekdir cookie path and out-of-range/garbage offsets against the live mount. | VM (`dir_locks::generic_676_seekdir`) | P2 | ported |
| 25 | generic/011 | dirstress | Read-only adaptation: many concurrent processes walking/listing the same tree (FUSE_PARALLEL_DIROPS is negotiated). Complements #13 on the lookup/readdir side. | VM (`walker::generic_011_dirstress`, 8 threads × 4 full walks + seq-dir listings; the aliased fixture dirs make the walks contend on the kernel dentry machinery) | P2 | ported |
| 26 | generic/074 | fstest (read/mmap patterns) | mmap-read == read() comparison through the mount; with passthrough the mapping is backed by the cache file, in KEEP_CACHE fallback by FUSE pages — both must serve identical bytes. | VM; python3 (mmap) on the client | P2 |ported (`io_paths::generic_074_127_mmap_reads`) |
| 27 | generic/127 | fsx incl. memory-mapped reads | Larger mmap-read coverage once #26's helper exists; also worth one run with `RIO_DISABLE_PASSTHROUGH=1` (the escape hatch changes the read path entirely). | VM (with #26) | P2 |ported (with #26; MAP_PRIVATE+MAP_SHARED legs) |
| 28 | generic/028 | path resolution / getcwd correctness | cd deep into the mount, getcwd, resolve `..`. Upstream races getcwd against rename churn; the read-only analogue of that churn is a lookup of a content-identical ALIAS directory, which on the digest-keyed FUSE re-parents the cwd's dentry and flips getcwd to the alias path. | VM (`walker::generic_028_getcwd_stability`; **expected-red on the pre-fix FUSE**, finding F-E) | P2 | ported |
| 29 | generic/088 | permission checks for unprivileged users (DAC) | With `default_permissions` the kernel enforces the served 0444/0555 root-owned modes for arbitrary uids — probes R/W/X as a second unprivileged uid (1001/991, distinct from the build uid the other batteries use) to pin allow_other + DAC interplay. | VM (`dir_locks::generic_088_second_uid_dac`, second-uid PrivDrop) | P2 | ported |
| 30 | generic/249 | splice(2) read | splice/sendfile from a FUSE-backed file is a distinct kernel read path; with passthrough it should hit the backing file directly — bytes must match on both passthrough and KEEP_CACHE handles. | VM | P2 |ported (`io_paths::generic_249_splice_read`) |
| 31 | generic/430 | copy_file_range basic copies (FUSE as source) | Since kernel 5.19, userspace cross-fs cfr without a native FS op is EXDEV by policy — and EXDEV is the errno coreutils' fallback path expects, so the contract is "byte-exact copy or exactly EXDEV, nothing else". Any other errno turns `cp` from the mount into a hard error. Overlay copy-up is NOT this path (it uses the kernel-internal COPY_FILE_SPLICE flag; covered e2e by vm-castore-e2e). | VM (`io_paths::generic_430_553_copy_file_range`, native libc cfr) | P2 |ported (EXDEV-contract assertion; oracle + generic/553 EOF legs activate if a future kernel/FS allows the copy) |
| 32 | generic/131 | POSIX/fcntl lock smoke on read-only files | Read locks via fcntl/flock on a RO FUSE file must be granted and tracked kernel-locally (the daemon implements no lock ops). Cross-process F_GETLK proves the kernel is the lock manager; F_WRLCK through an RO fd is EBADF; flock is best-effort (ENOSYS accepted, a grant must be enforced). Configure scripts and sqlite-using builds take read locks on inputs. | VM (`dir_locks::generic_131_read_locks`) | P2 | ported |
| 33 | generic/614 | st_blocks consistency | Non-empty files report st_blocks == ceil(size/512) (`make_attr`); `du` on the mount stays sane (it is how builds estimate input size). | VM (cheap; fold into the meta subtest); Rust attr check | P2 | partially ported (Rust) |
| 34 | generic/436 | further SEEK_DATA/SEEK_HOLE sanity | Only adds value beyond #20 if backing-cache files become sparse (they are written by sequential fetch, so currently never). | VM | P3 | — |
| 35 | generic/445 | another SEEK_DATA/SEEK_HOLE pattern | Same caveat as #34. | VM | P3 | — |
| 36 | generic/490 | SEEK_DATA inside large holes | Same caveat as #34. | VM | P3 | — |
| 37 | generic/539 | SEEK_HOLE finds a punched hole | No hole punching on this fs; keep only as "no false holes reported". | VM | P3 | — |
| 38 | generic/263 | fsx O_DIRECT vs buffered | O_DIRECT open/read through FUSE passthrough either works or fails cleanly with EINVAL — never silent corruption. | VM (`io_paths::generic_263_odirect_read`; aligned warm-window reads vs oracle, or a clean EINVAL) | P3 | ported |
| 39 | generic/564 | copy_file_range error conditions | Error-path companion to #31 (bad fds, overlapping ranges, RO destination). | VM (folded into `generic_430_553`: O_RDONLY-dest → EBADF, directory source → EISDIR — the generic/434 RO-fs legs) | P3 | partially ported |
| 40 | generic/565 | copy_file_range across devices | Cross-device leg of #31 (mount → tmpfs). | VM | P3 | — |
| 41 | generic/339 | large-directory entry ordering | Through this FUSE it reduces to "big readdir is complete", covered by #1 at 200 entries. Revisit with a chromium-scale (~3k-entry) Directory if real closures get that wide (tree.rs notes the linear scan budget). | VM | P3 | — |
| 42 | generic/637 | dir content visibility across fresh fds (small getdents buffer) | The fs is immutable so the visibility half is moot; the small-getdents-buffer half is a useful stressor for offset resume (pairs with #24). | VM (`dir_locks::generic_637_small_getdents`: raw SYS_getdents64, 64-byte buffer over the 200-entry dir, 512-byte over the NAME_MAX names) | P3 | ported |
| 43 | generic/467 | open_by_handle_at variants | fuse DOES export encode_fh, so the honest contract is: name_to_handle_at fails EOPNOTSUPP **or** succeeds; a successful handle re-opened via open_by_handle_at resolves to the same (dev,ino) or fails cleanly ESTALE (no export lookup in the daemon). | VM (`io_paths::generic_467_open_by_handle`; also the 426/477/756/777 refusal contract) | P3 | ported |
| 44 | generic/477 | open_by_handle after cycle mount | Same as #43; remount-equivalents are covered by the teardown/cache-hit phases of vm-castore-fuse. | VM (covered by the #43 probe) | P3 | covered |
| 45 | generic/422 | stat blocks under delayed allocation | No delalloc on this fs; only the st_blocks sanity already in #33. | VM | P3 | — |
| 46 | generic/003 | atime/relatime/nodiratime semantics | Attrs are immutable epoch+1 with infinite TTLs → atime never changes; a single "atime stable across reads" assertion is the whole port. | VM (`walker::generic_003_192_atime_stable`) | P3 | ported |
| 47 | generic/192 | atime update + persistence | Same as #46 (persistence does not apply — the mount is per-build and ephemeral). | VM (folded into #46) | P3 | ported |
| 48 | generic/013 | fsstress | Read-only op profile (stat/readdir/readlink/open/read/eaccess); soaks `Opener` map growth + backing-id churn under load. Native Rust with a seeded per-thread LCG — no fsstress binary needed, failures replay deterministically. | VM (`walker::generic_013_fsstress_readonly`, 4 threads × 1500 verified ops) | P3 | ported |
| 49 | generic/241 | parallel dbench | Replace dbench with a parallel tree-walk + checksum loop; covered by #25 (dirstress walks) + #48 (content-verified op soak); the production-shaped load test is vm-castore-e2e's parallel builds. | VM (folded) | P3 | folded |
| 50 | generic/067 | mount/umount corner cases | Mount lifecycle is owned by rio-mountd (MNT_DETACH on UDS close, fusectl abort on session drop); the interesting legs are already exercised by vm-castore-fuse's teardown/missing-path subtests and vm-mountd. | VM | P3 | — |

### Explicitly out of scope

Write-path tests (rw/punch/collapse/zero/clone/dedupe/prealloc),
mkfs/fsck/scrub, quota, freeze/thaw, dm-flakey/error-injection,
log-recovery/fsync-power-fail, encryption/verity/idmapped-mount/ACL
groups: the mount serves an immutable content-addressed closure with no
xattrs/ACLs/special files and writes land in the overlay upper, so these
have no meaningful adaptation beyond the write-protection checks in #8/#9.
The xfstests `overlay/*` group is not ported here either: rio's
overlay-over-castore stack is exercised end-to-end by `vm-castore-e2e`
(copy-up, whiteouts, output capture), and this plan's overlay legs only
cover what the *lower* must provide (readdir/d_type/xattr answers).

## Findings (this port batch)

* **F-A — `statfs` reports all-zero totals** (analogue of old F-3):
  `CastoreFs` does not implement `statfs`, so fuser's default reply
  (0 blocks / 0 files, bsize 512, namelen 255) is what `df` and
  `statvfs(3)` see on the mount. Harmless for builds that only read
  inputs, but tools that pre-check free space on an input path see 0.
  The VM port asserts only that statfs/`df` succeed and namelen is
  sane, and logs the zero totals informationally. If this matters, a
  truthful passthrough of the backing filesystem's statvfs (or static
  sane totals) would be more conventional.
* **F-B — directory nlink is 1 (SUPERSEDED)** (generic/002
  adaptation): the original everything-is-nlink-1 choice was legal
  (btrfs does the same) but understated hardlink aliases and broke
  the `st_nlink-2 == subdir count` convention. The per-path inode fix
  replaced it with honest nlink (`builder.fs.castore-nlink`): files
  and symlinks report their path-alias count, directories report
  2 + subdirectory count. `generic_002_nlink_walk` now asserts the
  honest values; `walker::hardlink_nlink_honesty` covers the deduped
  aliases.
* **F-C — root write-through into the shared backing cache (FIXED)**
  (generic/050 root leg, `write_through_passthrough_root`): against
  the pre-EROFS castore-FUSE, `CastoreFs::open` ignored the requested
  access mode, so root's `open(O_WRONLY)` on a cache-hit file was not
  refused; the kernel then opened the backing cache file with the
  caller's flags under the BACKING_OPEN broker's credentials
  (rio-mountd, root) — `backing_file_open` → `dentry_open` performs
  no DAC check — and `write(2)` landed in the node-shared cache file,
  served back to every build that reads that digest. Build processes
  could not reach this (they fail at default_permissions before the
  FUSE is consulted); any root-equivalent process on a builder node
  could. Fixed by exactly the options this finding proposed:
  `CastoreFs::open` rejects non-read access modes with EROFS
  (`builder.fs.open-read-only`) and rio-mountd re-opens every
  BackingOpen fd `O_RDONLY` before registering it
  (`builder.mountd.backing-readonly`). The probe pins the fixed
  behavior — root's write-mode open must be refused; a regression to
  write-through re-surfaces as a FINDING line, fails on anything
  outside the two known behaviors, and repairs the cache file
  afterwards.
* **F-D — write ops returned ENOSYS/EPERM instead of EROFS (FIXED)**
  (generic/050+294 root leg, `generic_294_erofs_battery_root`): with
  CAP_DAC_OVERRIDE the kernel permission check passes and the
  operation reaches the FUSE daemon. Pre-fix, the daemon had no write
  handlers, so fuser's defaults leaked through — ENOSYS (unlink/mkdir/
  rmdir/create/rename/setattr) or EPERM (symlink/link); ENOSYS in
  particular is not a legal errno for unlink(2)/mkdir(2). Fixed by the
  macro-generated write-path deny table: every mutation op now answers
  EROFS (`builder.fs.write-ops-erofs`). The probe asserts `EROFS or
  the historical fuser default` per op and prints which branch held,
  so the POSIX-conformant behavior is pinned and a regression back to
  the fuser defaults is visible as FINDING lines in the VM log.
* **No analogue found** for the old access(2)-mask and d_ino≠st_ino
  divergences (see "Old-FUSE assumptions dropped"); both correct
  behaviors are now asserted so they cannot regress silently.
* **F-E — digest-keyed directory inodes are hardlinked-dir semantics
  (ESCAPED to the data plane).** `tree::dir_ino = h(dir_digest)` makes
  content-identical directories share one inode across paths. POSIX
  forbids hardlinked directories; the kernel enforces one dentry per
  directory inode by *re-parenting* it on every lookup of a different
  alias (`d_splice_alias`), so a concurrent reader makes the dentry
  ping-pong between paths. GNU find's fts verifies its ascent by
  (dev,ino) and manufactures ENOENT when the check fires mid-walk —
  observed in production. Two earlier batch choices were downstream of
  the same design and are REVERSED in the walker batch:
  - the `inode_identity_content_addressed` directory leg (asserted
    shared dir inodes) is removed; `walker::posix_dir_inode_uniqueness`
    asserts the inverse, per-path contract;
  - the dot-entry exception ("`.`/`..` self-point, kernel resolves
    `..` through the dcache") is replaced by
    `walker::dot_dotdot_identity`, which requires honest d_ino on the
    dots.
  **FIXED** by the per-path directory-inode fix (parent_ino + name
  derivation; files keep content-deduped inos with honest st_nlink).
  The walker identity checks and the three unit tests in
  `xfstests_port.rs` (now un-ignored) are the regression tests for
  the escape. File-level dedup (`file_ino`) is NOT implicated and
  stays asserted.
* **F-F — over-long names answer ENOENT, not ENAMETOOLONG** (found by
  the generic/006 port's first VM run): per-component NAME_MAX
  enforcement is the filesystem's job — the kernel only rejects names
  past FUSE_NAME_MAX (1024) — and the castore lookup treats ANY
  unknown name as outside the closure, so a 256-byte component
  surfaces as a cached negative entry (ENOENT) instead of
  ENAMETOOLONG. Minor POSIX divergence: tools distinguishing "name too
  long" from "missing" (pathchk, some tar/install -D error paths) get
  the wrong class. **FIXED**: the lookup handler now gates components
  past the advertised NAME_MAX with ENAMETOOLONG before the
  negative-entry path, and the probe asserts ENAMETOOLONG strictly.

## Status / next steps

Ported in this batch: the 15 P1 rows above — 18 syscall-level checks
in `xfstests_runner` (run against the live mount by
vm-castore-xfstests) plus the Rust integration ports in
`rio-builder/tests/xfstests_port.rs` (#1 offset/resume + d_ino,
#2 kinds, #3/#33 attr legs, #5 size leg, #6 byte-exact names). Beyond
the original selection, the runner adds the root-credential legs
(`generic_294_erofs_battery_root`, `write_through_passthrough_root`)
that surfaced findings F-C and F-D against the pre-EROFS castore-FUSE;
both are fixed (see Findings) and the legs now pin the conformant
behavior.

P2 batch (this round): rows 16–24 and 26–32 are ported — the xattr
read trio (`xattr_statx.rs`), statx field/mask correctness, the
mmap/splice/copy_file_range alternate read paths and SEEK_HOLE/DATA
conformance (`io_paths.rs`), rewinddir/seekdir cookie handling, the
second-uid DAC probe and read-lock smoke (`dir_locks.rs`). All probes
are native Rust syscalls (libc/nix) — no extra VM packages.

Security batch: generic/680 (Dirty Pipe, CVE-2022-0847) is ported as
`write_attack::generic_680_dirty_pipe` — a page-cache write attack
that reaches the F-C blast radius through a path the EROFS guards
cannot see; the module doc has the full analysis. generic/123 needed
no new check: its four operations are exactly the unprivileged
`generic_050` probes, so it is folded into that check's provenance.

Label corrections: `statfs_zero_totals` is castore-specific (no
upstream statfs-totals test exists; generic/361 is "remount on I/O
errors"); `generic_002` now asserts honest nlink per
`builder.fs.castore-nlink` (the old nlink==1 inverse, F-B, is
superseded); `generic_095_113_310` carries only generic/113's
sync legs (AIO/io_uring readers remain the deferred P3 extension).

Walker batch (directory identity — the F-E escape): six new VM checks
in `checks/walker.rs` plus the fixture's `nest/` aliased-directory
subtree (content-identical `shared/` under two non-identical parents —
the exact shape whose dentry the kernel re-parents across subtrees):

| check | origin | expected on pre-fix FUSE |
|---|---|---|
| `posix_dir_inode_uniqueness` | castore-specific (POSIX: no hardlinked dirs) | **RED** (dup-a/dup-b and nest/*/shared share inodes) |
| `hardlink_nlink_honesty` | generic/002+100 intent (tar/du (dev,ino,nlink)) | **RED** (deduped files share an ino but claim nlink=1) |
| `dot_dotdot_identity` | POSIX dir sanity | **RED** (`..` d_ino self-points) |
| `generic_028_getcwd_stability` | generic/028 (read-only adaptation) | **RED** (alias lookup re-parents the cwd dentry) |
| `generic_011_dirstress` | generic/011 (read-only adaptation) | green expected; MAY flake red via transient EBUSY/ESTALE from `d_splice_alias` unalias failures — also the bug |
| `fts_walk_concurrent_aliases` | the escaped-bug regression (gnulib fts ascent) | **RED** (deterministic leg: held fd + alias lookup + `..` ascent) |

Unit tier: `posix_dir_inodes_are_per_path`,
`readdir_dotdot_points_at_parent`,
`deduped_file_nlink_counts_its_paths` in `xfstests_port.rs` — they
were `#[ignore]`d while the tree was pre-fix so the nextest gate
stayed green, and are un-ignored now that the per-path fix is in.
`manifest::alias_dir_groups` (the vacuity guard +
alias-pair source for the walker checks) has its own green unit test.

Errno/contract batch (the triage table's PORT/PARTIAL rows): nine more
checks —

* `mount_readonly_honesty`: the mount must SAY it is read-only —
  statvfs ST_RDONLY, `ro` in /proc/self/mounts, and root's
  `faccessat2(W_OK)` answered EROFS by the MS_RDONLY check instead of
  passing via CAP_DAC_OVERRIDE. Green since the MS_RDONLY mount flag
  landed in `castore_fuse::mountd`; pre-flag the mount was rw with
  write refusal deferred to open time and all three surfaces lied.
* `generic_006_name_limits`: ENAMETOOLONG at the 256-byte component
  and >PATH_MAX path boundaries, ENOENT (not a length error) for an
  absent NAME_MAX name. First VM run surfaced finding F-F (over-long
  component answered ENOENT) — fixed since the lookup handler gates
  components past NAME_MAX, and the probe requires ENAMETOOLONG
  strictly.
* `open_flag_contracts` (generic/004 + generic/763 intent):
  O_DIRECTORY→ENOTDIR, O_NOFOLLOW→ELOOP, O_PATH identity legs,
  O_TMPFILE refused cleanly, write(2) through an O_RDONLY fd is EBADF
  even for zero bytes.
* `generic_478_571_ofd_locks_lease`: OFD read locks granted on RO fds
  and tracked kernel-locally (conflict visible with marker pid -1),
  OFD write lock through an RO fd is EBADF, read lease grantable and
  reported by F_GETLEASE.
* `generic_637_small_getdents`: complete enumeration through 64-byte
  getdents64 buffers (offset-cookie resume under maximal batching
  pressure).
* `generic_263_odirect_read`, `generic_467_open_by_handle`, the
  generic/434 cfr error legs (in `generic_430_553`), and the
  generic/528 btime leg (in `generic_423`).
* generic/005 depth leg: the fixture's 41-link `links/chain*` makes
  the kernel's MAXSYMLINKS=40 observable — `chain0` must ELOOP though
  the chain is finite, `chain1` must resolve. `resolve_symlink` counts
  hops so the oracle classifies both correctly.

Soak batch: `generic_013_fsstress_readonly` (read-only fsstress — 4
threads × 1500 seeded, content-verified ops; replaces the upstream
binary) and `generic_003_192_atime_stable` (atime is canonical 1s and
never moves).

The per-path inode fix has landed: the three unit-tier tests are
un-ignored and the expected-red list is history — the walker checks,
`mount_readonly_honesty`, and the strict generic/006 leg are all
expected green. Still open: whether F-A warrants a real `statfs`
implementation (note `mount_readonly_honesty` already forces
ST_RDONLY through statvfs, which the same statfs op would serve).

## Exhaustive upstream triage (tests/generic/, 793 tests)

Surveyed at HEAD of the kernel.org xfstests-dev mirror (2026-06; 793
numbered tests under `tests/generic/` — the historical `tests/shared/`
group no longer exists upstream, its tests were folded into generic/).
Every test is classified for the castore-FUSE read-only mount:

* **PORTED** — re-expressed as a runner check / unit test (see the
  ranked-selection and batch tables above for the probe mapping).
* **PORT** — portable as-is in read-only terms; probe named in the row.
* **PARTIAL** — a write-heavy test with one portable read-only
  assertion; only that slice is ported, the row says which.
* **N/A** — requires writes, mount reconfiguration, or features the
  immutable castore tree cannot express. The one-line reason is the
  exclusion rationale; "pinned by the EROFS battery" means the
  write-refusal half of the upstream test IS asserted (generic/050+294
  ports), only its data-path half is meaningless here.

Counts: 35 PORTED rows (pre-triage batches), 8 PORT, 9 PARTIAL,
741 N/A.

| generic/ | class | disposition |
|---|---|---|
| 001 | N/A | write I/O |
| 002 | PORTED | meta::generic_002_nlink_walk (adapted; F-B superseded by honest nlink) |
| 003 | PARTIAL | atime/relatime matrix is mount-option testing; port only 'atime is canonical epoch+1 and stable across reads' → walker::generic_003_192_atime_stable (batch 3) |
| 004 | PARTIAL | O_TMPFILE linking is a write test; port the refusal contract: O_TMPFILE open on an input dir must fail cleanly (EOPNOTSUPP/EROFS) → errno_battery::open_flag_contracts (batch 2) |
| 005 | PORTED | meta::generic_005_symlink_errnos |
| 006 | PARTIAL | permname creates/deletes files with every byte value; the byte-exact lookup half is the generic/453 port; the name-length errno half → errno_battery::generic_006_name_limits (batch 2) |
| 007 | PORTED | errno_battery::generic_007_enoent_never_eio |
| 008 | N/A | fallocate |
| 009 | N/A | fallocate |
| 010 | N/A | dbtest is a write workload |
| 011 | PORT | read-only adaptation: many concurrent walkers/listers over one tree → walker::generic_011_dirstress (batch 1) |
| 012 | N/A | fallocate |
| 013 | PARTIAL | fsstress with a read-only op profile (stat/readdir/readlink/open/read/seek) → walker::generic_013_fsstress_readonly (batch 3); write ops N/A |
| 014 | N/A | write I/O |
| 015 | N/A | ENOSPC paths |
| 016 | N/A | fallocate |
| 017 | N/A | fallocate |
| 018 | N/A | defrag |
| 019 | N/A | crash/fuzz test |
| 020 | PORTED | xattr_statx::generic_020_062_097_xattr_read_legs |
| 021 | N/A | fallocate |
| 022 | N/A | fallocate |
| 023 | N/A | renameat2 is a write op; rename-refused-with-EROFS is pinned by errno_battery::generic_294_erofs_battery_root |
| 024 | N/A | renameat2 RENAME_NOREPLACE (write); EROFS pinned by the root battery |
| 025 | N/A | renameat2 RENAME_EXCHANGE (write); EROFS pinned by the root battery |
| 026 | N/A | ACLs (castore serves none) |
| 027 | N/A | ENOSPC paths |
| 028 | PORT | getcwd correctness under dentry churn → walker::generic_028_getcwd_stability (batch 1; aliased-dir re-parenting makes this the escaped-bug regression surface) |
| 029 | N/A | write I/O |
| 030 | N/A | write I/O |
| 031 | N/A | write I/O |
| 032 | N/A | write I/O |
| 033 | N/A | write I/O |
| 034 | N/A | log recovery |
| 035 | N/A | overwriting rename is a write op; EROFS pinned by the root battery |
| 036 | N/A | write I/O |
| 037 | N/A | xattr/chattr write |
| 038 | N/A | fallocate |
| 039 | N/A | log recovery |
| 040 | N/A | log recovery |
| 041 | N/A | log recovery |
| 042 | N/A | write I/O |
| 043 | N/A | fs shutdown ioctl |
| 044 | N/A | fs shutdown ioctl |
| 045 | N/A | fs shutdown ioctl |
| 046 | N/A | fs shutdown ioctl |
| 047 | N/A | write I/O |
| 048 | N/A | write I/O |
| 049 | N/A | write I/O |
| 050 | PORTED | errno_battery::generic_050_* (+root legs) |
| 051 | N/A | fs shutdown ioctl |
| 052 | N/A | fs shutdown ioctl |
| 053 | N/A | ACLs (castore serves none) |
| 054 | N/A | fs shutdown ioctl |
| 055 | N/A | quota |
| 056 | N/A | log recovery |
| 057 | N/A | log recovery |
| 058 | N/A | fallocate |
| 059 | N/A | hole punch |
| 060 | N/A | fallocate |
| 061 | N/A | fallocate |
| 062 | PORTED | xattr_statx (folded) |
| 063 | N/A | fallocate |
| 064 | N/A | fallocate |
| 065 | N/A | log recovery |
| 066 | N/A | log recovery |
| 067 | N/A | mount/umount corners are rio-mountd lifecycle, covered by vm-castore-fuse teardown + vm-mountd |
| 068 | N/A | freeze/thaw |
| 069 | N/A | write I/O |
| 070 | N/A | xattr/chattr write |
| 071 | N/A | fallocate |
| 072 | N/A | collapse-range |
| 073 | N/A | log recovery |
| 074 | PORTED | io_paths::generic_074_127_mmap_reads |
| 075 | PORTED | read::generic_075_091_read_integrity |
| 076 | N/A | write I/O |
| 077 | N/A | ENOSPC paths |
| 078 | N/A | renameat2 RENAME_WHITEOUT (write) |
| 079 | N/A | needs scratch mkfs (mutates fs) |
| 080 | N/A | mtime-after-mmap-write; no writes reach the mount (EROFS) and mtime is pinned canonical by generic_401 |
| 081 | N/A | device-mapper |
| 082 | N/A | quota |
| 083 | N/A | write I/O |
| 084 | N/A | hardlink to unlinked file (write) |
| 085 | N/A | freeze/thaw |
| 086 | N/A | fallocate |
| 087 | N/A | chown matrix (setattr); EROFS pinned by the root battery |
| 088 | PORTED | dir_locks::generic_088_second_uid_dac |
| 089 | N/A | rename/mtab emulation (write) |
| 090 | N/A | log recovery |
| 091 | PORTED | read::generic_075_091_read_integrity |
| 092 | N/A | fallocate |
| 093 | N/A | xattr/chattr write |
| 094 | N/A | fallocate |
| 095 | PORTED | read::generic_095_113_310_concurrency |
| 096 | N/A | fallocate |
| 097 | PORTED | xattr_statx (read legs) |
| 098 | N/A | needs scratch mkfs (mutates fs) |
| 099 | N/A | ACLs (castore serves none) |
| 100 | N/A | untars ONTO the fs (write); tar FROM the mount is exactly the (dev,ino,nlink) honesty the batch-1 walker probes pin |
| 101 | N/A | log recovery |
| 102 | N/A | write I/O |
| 103 | N/A | fallocate |
| 104 | N/A | log recovery |
| 105 | N/A | ACLs (castore serves none) |
| 106 | N/A | log recovery |
| 107 | N/A | log recovery |
| 108 | N/A | write I/O |
| 109 | N/A | rename d_type update (write) |
| 110 | N/A | reflink/clone |
| 111 | N/A | reflink/clone |
| 112 | N/A | write I/O |
| 113 | PORTED | read::generic_095_113_310_concurrency (sync legs) |
| 114 | N/A | write I/O |
| 115 | N/A | reflink/clone |
| 116 | N/A | reflink/clone |
| 117 | N/A | xattr/chattr write |
| 118 | N/A | reflink/clone |
| 119 | N/A | reflink/clone |
| 120 | N/A | needs scratch mkfs (mutates fs) |
| 121 | N/A | reflink/clone |
| 122 | N/A | reflink/clone |
| 123 | PORTED | folded into generic_050 provenance |
| 124 | N/A | needs scratch mkfs (mutates fs) |
| 125 | N/A | O_DIRECT + truncate write paths |
| 126 | PORTED | errno_battery::generic_126_exec_access |
| 127 | PORTED | io_paths (with 074) |
| 128 | N/A | needs scratch mkfs (mutates fs) |
| 129 | N/A | write I/O |
| 130 | N/A | needs scratch mkfs (mutates fs) |
| 131 | PORTED | dir_locks::generic_131_read_locks |
| 132 | N/A | needs scratch mkfs (mutates fs) |
| 133 | N/A | write I/O |
| 134 | N/A | reflink/clone |
| 135 | N/A | needs scratch mkfs (mutates fs) |
| 136 | N/A | reflink/clone |
| 137 | N/A | fallocate |
| 138 | N/A | reflink/clone |
| 139 | N/A | reflink/clone |
| 140 | N/A | reflink/clone |
| 141 | N/A | write I/O |
| 142 | N/A | reflink/clone |
| 143 | N/A | reflink/clone |
| 144 | N/A | fallocate |
| 145 | N/A | fallocate |
| 146 | N/A | hole punch |
| 147 | N/A | insert-range |
| 148 | N/A | reflink/clone |
| 149 | N/A | zero-range |
| 150 | N/A | reflink/clone |
| 151 | N/A | reflink/clone |
| 152 | N/A | hole punch |
| 153 | N/A | collapse-range |
| 154 | N/A | reflink/clone |
| 155 | N/A | zero-range |
| 156 | N/A | reflink/clone |
| 157 | N/A | reflink/clone |
| 158 | N/A | reflink/clone |
| 159 | N/A | reflink/clone |
| 160 | N/A | reflink/clone |
| 161 | N/A | reflink/clone |
| 162 | N/A | reflink/clone |
| 163 | N/A | reflink/clone |
| 164 | N/A | reflink/clone |
| 165 | N/A | reflink/clone |
| 166 | N/A | reflink/clone |
| 167 | N/A | reflink/clone |
| 168 | N/A | reflink/clone |
| 169 | N/A | write I/O |
| 170 | N/A | reflink/clone |
| 171 | N/A | reflink/clone |
| 172 | N/A | reflink/clone |
| 173 | N/A | reflink/clone |
| 174 | N/A | reflink/clone |
| 175 | N/A | reflink/clone |
| 176 | N/A | hole punch |
| 177 | N/A | fallocate |
| 178 | N/A | hole punch |
| 179 | N/A | hole punch |
| 180 | N/A | zero-range |
| 181 | N/A | reflink/clone |
| 182 | N/A | reflink/clone |
| 183 | N/A | reflink/clone |
| 184 | N/A | mknod (write); EROFS pinned by the root battery |
| 185 | N/A | reflink/clone |
| 186 | N/A | fallocate |
| 187 | N/A | fallocate |
| 188 | N/A | fallocate |
| 189 | N/A | fallocate |
| 190 | N/A | fallocate |
| 191 | N/A | fallocate |
| 192 | PARTIAL | atime persistence across unmount N/A (per-build ephemeral mount); stable-atime leg folded into walker::generic_003_192_atime_stable (batch 3) |
| 193 | N/A | →setattr permission matrix (write); every setattr is EROFS, pinned by the root battery |
| 194 | N/A | fallocate |
| 195 | N/A | fallocate |
| 196 | N/A | fallocate |
| 197 | N/A | fallocate |
| 198 | N/A | AIO write paths |
| 199 | N/A | fallocate |
| 200 | N/A | fallocate |
| 201 | N/A | CoW dirty pages + unlink (write) |
| 202 | N/A | reflink/clone |
| 203 | N/A | reflink/clone |
| 204 | N/A | write I/O |
| 205 | N/A | reflink/clone |
| 206 | N/A | reflink/clone |
| 207 | N/A | AIO write paths |
| 208 | N/A | AIO write paths |
| 209 | N/A | AIO write paths |
| 210 | N/A | AIO write paths |
| 211 | N/A | write I/O |
| 212 | N/A | AIO write paths |
| 213 | N/A | write I/O |
| 214 | N/A | write I/O |
| 215 | N/A | c/mtime after mapped writes (write) |
| 216 | N/A | fallocate |
| 217 | N/A | fallocate |
| 218 | N/A | fallocate |
| 219 | N/A | quota |
| 220 | N/A | fallocate |
| 221 | N/A | futimens ctime (write); utimensat-refused is in the EROFS battery |
| 222 | N/A | fallocate |
| 223 | N/A | fallocate |
| 224 | N/A | needs scratch mkfs (mutates fs) |
| 225 | N/A | FIEMAP (no extents to report) |
| 226 | N/A | ENOSPC paths |
| 227 | N/A | fallocate |
| 228 | N/A | write I/O |
| 229 | N/A | fallocate |
| 230 | N/A | quota |
| 231 | N/A | quota |
| 232 | N/A | quota |
| 233 | N/A | quota |
| 234 | N/A | quota |
| 235 | N/A | quota |
| 236 | N/A | link(2) ctime (write) |
| 237 | N/A | ACLs (castore serves none) |
| 238 | N/A | fallocate |
| 239 | N/A | write I/O |
| 240 | N/A | write I/O |
| 241 | PARTIAL | dbench replaced by a concurrent walk+read soak; folded into walker::generic_011_dirstress + generic_013_fsstress_readonly |
| 242 | N/A | reflink/clone |
| 243 | N/A | reflink/clone |
| 244 | N/A | quota |
| 245 | N/A | rename onto non-empty dir (write) |
| 246 | N/A | write I/O |
| 247 | N/A | write I/O |
| 248 | N/A | write I/O |
| 249 | PORTED | io_paths::generic_249_splice_read |
| 250 | N/A | write I/O |
| 251 | N/A | needs scratch mkfs (mutates fs) |
| 252 | N/A | write I/O |
| 253 | N/A | reflink/clone |
| 254 | N/A | hole punch |
| 255 | N/A | fallocate |
| 256 | N/A | hole punch |
| 257 | PORTED | meta::generic_257_readdir_multibatch (+overlay leg, Rust unit) |
| 258 | N/A | pre-epoch timestamps cannot exist here: every attr is canonical epoch+1 (pinned by generic_401) |
| 259 | N/A | zero-range |
| 260 | N/A | needs scratch mkfs (mutates fs) |
| 261 | N/A | collapse-range |
| 262 | N/A | insert-range |
| 263 | PORT | read-only adaptation of the O_DIRECT fsx: O_DIRECT open/read either serves exact bytes or fails cleanly EINVAL → io_paths::generic_263_odirect_read (batch 2) |
| 264 | N/A | reflink/clone |
| 265 | N/A | reflink/clone |
| 266 | N/A | reflink/clone |
| 267 | N/A | reflink/clone |
| 268 | N/A | reflink/clone |
| 269 | N/A | write I/O |
| 270 | N/A | write I/O |
| 271 | N/A | reflink/clone |
| 272 | N/A | reflink/clone |
| 273 | N/A | write I/O |
| 274 | N/A | write I/O |
| 275 | N/A | write I/O |
| 276 | N/A | reflink/clone |
| 277 | N/A | needs scratch mkfs (mutates fs) |
| 278 | N/A | reflink/clone |
| 279 | N/A | reflink/clone |
| 280 | N/A | quota |
| 281 | N/A | reflink/clone |
| 282 | N/A | reflink/clone |
| 283 | N/A | reflink/clone |
| 284 | N/A | fallocate |
| 285 | PORTED | io_paths::generic_285_448_706_seek_hole_data |
| 286 | N/A | SEEK_DATA/HOLE copy needs holes; backing files are never sparse and the no-false-holes contract is pinned by the generic/285 port |
| 287 | N/A | fallocate |
| 288 | N/A | needs scratch mkfs (mutates fs) |
| 289 | N/A | fallocate |
| 290 | N/A | fallocate |
| 291 | N/A | fallocate |
| 292 | N/A | fallocate |
| 293 | N/A | fallocate |
| 294 | PORTED | errno_battery::generic_294_* |
| 295 | N/A | fallocate |
| 296 | N/A | reflink/clone |
| 297 | N/A | reflink/clone |
| 298 | N/A | reflink/clone |
| 299 | N/A | write I/O |
| 300 | N/A | hole punch |
| 301 | N/A | reflink/clone |
| 302 | N/A | reflink/clone |
| 303 | N/A | reflink/clone |
| 304 | N/A | reflink/clone |
| 305 | N/A | reflink/clone |
| 306 | N/A | RW open of device nodes: castore trees cannot contain device nodes (NAR excludes them) |
| 307 | N/A | ACLs (castore serves none) |
| 308 | N/A | write at max logical offset |
| 309 | N/A | dir mtime on rename (write) |
| 310 | PORTED | read::generic_095_113_310_concurrency |
| 311 | N/A | fallocate |
| 312 | N/A | fallocate |
| 313 | N/A | truncate ctime (write); truncate-refused pinned by the EROFS battery |
| 314 | N/A | SGID inheritance on mkdir (write) |
| 315 | N/A | write I/O |
| 316 | N/A | hole punch |
| 317 | N/A | needs scratch mkfs (mutates fs) |
| 318 | N/A | xattr/chattr write |
| 319 | N/A | ACLs (castore serves none) |
| 320 | N/A | write I/O |
| 321 | N/A | log recovery |
| 322 | N/A | log recovery |
| 323 | N/A | AIO write paths |
| 324 | N/A | fallocate |
| 325 | N/A | log recovery |
| 326 | N/A | reflink/clone |
| 327 | N/A | reflink/clone |
| 328 | N/A | reflink/clone |
| 329 | N/A | reflink/clone |
| 330 | N/A | reflink/clone |
| 331 | N/A | reflink/clone |
| 332 | N/A | reflink/clone |
| 333 | N/A | reflink/clone |
| 334 | N/A | reflink/clone |
| 335 | N/A | log recovery |
| 336 | N/A | log recovery |
| 337 | N/A | xattr/chattr write |
| 338 | N/A | write I/O |
| 339 | N/A | dir hash ordering reduces to 'big readdir is complete' here; covered by the generic/257 port (PLAN row 41 documents the ~3k-entry revisit trigger) |
| 340 | N/A | needs scratch mkfs (mutates fs) |
| 341 | N/A | log recovery |
| 342 | N/A | log recovery |
| 343 | N/A | log recovery |
| 344 | N/A | needs scratch mkfs (mutates fs) |
| 345 | N/A | needs scratch mkfs (mutates fs) |
| 346 | N/A | write I/O |
| 347 | N/A | write I/O |
| 348 | N/A | needs scratch mkfs (mutates fs) |
| 349 | N/A | write I/O |
| 350 | N/A | write I/O |
| 351 | N/A | write I/O |
| 352 | N/A | reflink/clone |
| 353 | N/A | reflink/clone |
| 354 | N/A | needs scratch mkfs (mutates fs) |
| 355 | N/A | suid/sgid clear on write |
| 356 | N/A | reflink/clone |
| 357 | N/A | reflink/clone |
| 358 | N/A | reflink/clone |
| 359 | N/A | reflink/clone |
| 360 | PORTED | meta::generic_360_symlink_targets |
| 361 | N/A | needs scratch mkfs (mutates fs) |
| 362 | N/A | O_DIRECT append write fault |
| 363 | N/A | write I/O |
| 364 | N/A | concurrent O_DIRECT writes |
| 365 | N/A | needs scratch mkfs (mutates fs) |
| 366 | N/A | write I/O |
| 367 | N/A | extsize hint ioctl (write) |
| 368 | N/A | fscrypt |
| 369 | N/A | fscrypt |
| 370 | N/A | reflink/clone |
| 371 | N/A | fallocate |
| 372 | N/A | fallocate |
| 373 | N/A | reflink/clone |
| 374 | N/A | reflink/clone |
| 375 | N/A | ACLs (castore serves none) |
| 376 | N/A | log recovery |
| 377 | N/A | xattr/chattr write |
| 378 | N/A | hardlink creation perms (write) |
| 379 | N/A | quota |
| 380 | N/A | quota |
| 381 | N/A | quota |
| 382 | N/A | quota |
| 383 | N/A | quota |
| 384 | N/A | quota |
| 385 | N/A | quota |
| 386 | N/A | quota |
| 387 | N/A | reflink/clone |
| 388 | N/A | fs shutdown ioctl |
| 389 | N/A | ACLs (castore serves none) |
| 390 | N/A | freeze/thaw |
| 391 | N/A | write I/O |
| 392 | N/A | hole punch |
| 393 | N/A | write I/O |
| 394 | N/A | RLIMIT_FSIZE on write |
| 395 | N/A | fscrypt |
| 396 | N/A | fscrypt |
| 397 | N/A | fscrypt |
| 398 | N/A | fscrypt |
| 399 | N/A | fscrypt |
| 400 | N/A | quota |
| 401 | PORTED | meta::generic_401_file_kinds |
| 402 | N/A | write I/O |
| 403 | N/A | xattr/chattr write |
| 404 | N/A | fallocate |
| 405 | N/A | dm-thin |
| 406 | N/A | needs scratch mkfs (mutates fs) |
| 407 | N/A | reflink/clone |
| 408 | N/A | reflink/clone |
| 409 | N/A | mount-option matrix |
| 410 | N/A | mount-option matrix |
| 411 | N/A | mount-option matrix |
| 412 | N/A | needs scratch mkfs (mutates fs) |
| 413 | N/A | fallocate |
| 414 | N/A | fallocate |
| 415 | N/A | hole punch |
| 416 | N/A | ENOSPC paths |
| 417 | N/A | orphan processing on RO→RW transition; no RW transition exists for a castore mount |
| 418 | N/A | write I/O |
| 419 | N/A | fscrypt |
| 420 | N/A | hole punch |
| 421 | N/A | crash/fuzz test |
| 422 | N/A | fallocate |
| 423 | PORTED | xattr_statx::generic_423_statx_field_correctness |
| 424 | N/A | statx attrs settable via chattr (write); the read-side attributes_mask sanity is the generic/532 port |
| 425 | N/A | xattr/chattr write |
| 426 | PORT | open_by_handle family: name_to_handle_at on a FUSE without export support must fail EOPNOTSUPP cleanly → io_paths::generic_467_open_by_handle (one probe covers 426/467/477/756/777; batch 2) |
| 427 | N/A | write I/O |
| 428 | N/A | DAX mmap write regression |
| 429 | N/A | fscrypt |
| 430 | PORTED | io_paths::generic_430_553_copy_file_range |
| 431 | N/A | cfr data copies: the EXDEV-or-byte-exact contract is the generic/430 port; oracle legs activate if a future kernel allows the copy |
| 432 | N/A | cfr data swap (write to a mount dest is refused); contract covered by the 430 port |
| 433 | N/A | as 432 |
| 434 | PARTIAL | cfr error matrix: the RO-fs legs (dest on the mount → EBADF, bad fd combos) extend io_paths::generic_430_553_copy_file_range (batch 2) |
| 435 | N/A | fscrypt |
| 436 | N/A | write I/O |
| 437 | N/A | DAX/mmap write race |
| 438 | N/A | mmap write corruption |
| 439 | N/A | hole punch |
| 440 | N/A | fscrypt |
| 441 | N/A | error injection |
| 442 | N/A | error injection |
| 443 | N/A | write I/O |
| 444 | N/A | ACLs (castore serves none) |
| 445 | N/A | write I/O |
| 446 | N/A | write I/O |
| 447 | N/A | hole punch |
| 448 | PORTED | io_paths (with 285) |
| 449 | N/A | ENOSPC paths |
| 450 | N/A | write I/O |
| 451 | N/A | write I/O |
| 452 | N/A | needs scratch mkfs (mutates fs) |
| 453 | PORTED | meta::generic_453_byte_exact_names |
| 454 | N/A | xattr/chattr write |
| 455 | N/A | log recovery |
| 456 | N/A | fallocate |
| 457 | N/A | reflink/clone |
| 458 | N/A | zero-range |
| 459 | N/A | freeze/thaw |
| 460 | N/A | write I/O |
| 461 | N/A | fs shutdown ioctl |
| 462 | N/A | DAX gup write race |
| 463 | N/A | reflink/clone |
| 464 | N/A | write I/O |
| 465 | N/A | write I/O |
| 466 | N/A | write I/O |
| 467 | PORT | → io_paths::generic_467_open_by_handle (batch 2) |
| 468 | N/A | fallocate |
| 469 | N/A | fallocate |
| 470 | N/A | needs scratch mkfs (mutates fs) |
| 471 | PORTED | dir_locks::generic_471_rewinddir |
| 472 | N/A | swapfile |
| 473 | N/A | FIEMAP (no extents to report) |
| 474 | N/A | fs shutdown ioctl |
| 475 | N/A | fs shutdown ioctl |
| 476 | N/A | write I/O |
| 477 | N/A | open_by_handle after cycle mount: handle refusal covered by the 467 probe; remount-equivalents covered by vm-castore-fuse teardown/cache-hit phases |
| 478 | PORT | OFD lock read legs: F_OFD_SETLK(RDLCK) on an O_RDONLY fd granted + F_OFD_GETLK advice correct → dir_locks::generic_478_571_ofd_locks_lease (batch 2) |
| 479 | N/A | mknod/symlink fsync log recovery |
| 480 | N/A | log recovery |
| 481 | N/A | log recovery |
| 482 | N/A | crash recovery |
| 483 | N/A | fallocate |
| 484 | N/A | error injection |
| 485 | N/A | fallocate |
| 486 | N/A | xattr/chattr write |
| 487 | N/A | error injection |
| 488 | N/A | needs scratch mkfs (mutates fs) |
| 489 | N/A | log recovery |
| 490 | N/A | SEEK_DATA in large holes: no holes here; covered by the 285 port |
| 491 | N/A | freeze/thaw |
| 492 | N/A | needs scratch mkfs (mutates fs) |
| 493 | N/A | dedupe ioctl |
| 494 | N/A | hole punch |
| 495 | N/A | swapfile |
| 496 | N/A | fallocate |
| 497 | N/A | collapse-range |
| 498 | N/A | log recovery |
| 499 | N/A | write I/O |
| 500 | N/A | dm-thin |
| 501 | N/A | reflink/clone |
| 502 | N/A | log recovery |
| 503 | N/A | fallocate |
| 504 | N/A | F_GETLK l_pid translation: already asserted cross-process (child checks l_pid == parent) in the generic/131 port |
| 505 | N/A | fs shutdown ioctl |
| 506 | N/A | quota |
| 507 | N/A | fs shutdown ioctl |
| 508 | N/A | fs shutdown ioctl |
| 509 | N/A | log recovery |
| 510 | N/A | log recovery |
| 511 | N/A | write I/O |
| 512 | N/A | fallocate |
| 513 | N/A | reflink/clone |
| 514 | N/A | reflink/clone |
| 515 | N/A | fallocate |
| 516 | N/A | reflink/clone |
| 517 | N/A | reflink/clone |
| 518 | N/A | reflink/clone |
| 519 | N/A | FIEMAP (no extents to report) |
| 520 | N/A | log recovery |
| 521 | N/A | long-soak directio fsx (write) |
| 522 | N/A | long-soak buffered fsx (write) |
| 523 | N/A | xattr/chattr write |
| 524 | N/A | needs scratch mkfs (mutates fs) |
| 525 | N/A | write I/O |
| 526 | N/A | log recovery |
| 527 | N/A | log recovery |
| 528 | PARTIAL | statx btime plausibility: on castore btime is canonical epoch+1; assert it when the kernel grants STATX_BTIME → fold into xattr_statx::generic_423 (batch 2) |
| 529 | N/A | xattr/chattr write |
| 530 | N/A | O_TMPFILE crash recovery |
| 531 | N/A | O_TMPFILE close stress (write) |
| 532 | PORTED | xattr_statx::generic_532_statx_attributes_mask_sanity |
| 533 | N/A | xattr/chattr write |
| 534 | N/A | log recovery |
| 535 | N/A | log recovery |
| 536 | N/A | write I/O |
| 537 | N/A | needs scratch mkfs (mutates fs) |
| 538 | N/A | AIO write paths |
| 539 | N/A | punched-hole SEEK_HOLE; no holes, covered by the 285 port |
| 540 | N/A | fallocate |
| 541 | N/A | fallocate |
| 542 | N/A | fallocate |
| 543 | N/A | fallocate |
| 544 | N/A | reflink across inode numbers (write) |
| 545 | N/A | chattr flags |
| 546 | N/A | fallocate |
| 547 | N/A | log recovery |
| 548 | N/A | fscrypt |
| 549 | N/A | fscrypt |
| 550 | N/A | fscrypt |
| 551 | N/A | AIO write paths |
| 552 | N/A | log recovery |
| 553 | PORTED | io_paths (with 430) |
| 554 | N/A | swapfile |
| 555 | N/A | FS_XFLAG_IMMUTABLE/APPEND set (write ioctl) |
| 556 | N/A | casefold feature; castore lookup is byte-exact by design (generic/453 port pins it) |
| 557 | N/A | log recovery |
| 558 | N/A | ENOSPC paths |
| 559 | N/A | dedupe ioctl |
| 560 | N/A | dedupe ioctl |
| 561 | N/A | dedupe ioctl |
| 562 | N/A | hole punch |
| 563 | N/A | loop device |
| 564 | N/A | loop device |
| 565 | N/A | needs scratch mkfs (mutates fs) |
| 566 | N/A | quota |
| 567 | N/A | write I/O |
| 568 | N/A | write I/O |
| 569 | N/A | write I/O |
| 570 | N/A | write I/O |
| 571 | PORT | fcntl advisory lock + F_SETLEASE read legs on O_RDONLY fds → dir_locks::generic_478_571_ofd_locks_lease (batch 2) |
| 572 | N/A | fsverity |
| 573 | N/A | fsverity |
| 574 | N/A | fsverity |
| 575 | N/A | fsverity |
| 576 | N/A | fscrypt |
| 577 | N/A | fsverity |
| 578 | N/A | write I/O |
| 579 | N/A | fsverity |
| 580 | N/A | fscrypt |
| 581 | N/A | fscrypt |
| 582 | N/A | fscrypt |
| 583 | N/A | fscrypt |
| 584 | N/A | fscrypt |
| 585 | N/A | rename (write) |
| 586 | N/A | write I/O |
| 587 | N/A | write I/O |
| 588 | N/A | reflink/clone |
| 589 | N/A | mount-option matrix |
| 590 | N/A | fallocate |
| 591 | N/A | write I/O |
| 592 | N/A | fscrypt |
| 593 | N/A | fscrypt |
| 594 | N/A | quota |
| 595 | N/A | fscrypt |
| 596 | N/A | needs scratch mkfs (mutates fs) |
| 597 | N/A | protected_symlinks sysctl needs sticky world-writable dirs; cannot exist on the 0555 tree |
| 598 | N/A | protected_regular/fifos sysctl; same reason |
| 599 | N/A | fs shutdown ioctl |
| 600 | N/A | quota |
| 601 | N/A | quota |
| 602 | N/A | fscrypt |
| 603 | N/A | quota |
| 604 | N/A | mount-option matrix |
| 605 | N/A | fallocate |
| 606 | N/A | xattr/chattr write |
| 607 | N/A | xattr/chattr write |
| 608 | N/A | xattr/chattr write |
| 609 | N/A | write I/O |
| 610 | N/A | fallocate |
| 611 | N/A | xattr/chattr write |
| 612 | N/A | reflink/clone |
| 613 | N/A | fscrypt |
| 614 | PORTED | unit tier (st_blocks) + meta legs |
| 615 | N/A | write I/O |
| 616 | N/A | write I/O |
| 617 | N/A | write I/O |
| 618 | N/A | xattr/chattr write |
| 619 | N/A | write I/O |
| 620 | N/A | mount-option matrix |
| 621 | N/A | fscrypt |
| 622 | N/A | fs shutdown ioctl |
| 623 | N/A | fs shutdown ioctl |
| 624 | N/A | fsverity |
| 625 | N/A | fsverity |
| 626 | N/A | ENOSPC paths |
| 627 | N/A | write I/O |
| 628 | N/A | write I/O |
| 629 | N/A | write I/O |
| 630 | N/A | write I/O |
| 631 | N/A | write I/O |
| 632 | N/A | mount-option matrix |
| 633 | N/A | idmapped mounts; not a castore configuration |
| 634 | N/A | needs scratch mkfs (mutates fs) |
| 635 | N/A | fs shutdown ioctl |
| 636 | N/A | swapfile |
| 637 | PORT | dir completeness through a tiny getdents buffer on a fresh fd → dir_locks::generic_637_small_getdents (batch 2); the visibility half is moot (immutable fs) |
| 638 | N/A | write I/O |
| 639 | N/A | write I/O |
| 640 | N/A | log recovery |
| 641 | N/A | collapse-range |
| 642 | N/A | xattr/chattr write |
| 643 | N/A | swapfile |
| 644 | N/A | mount-option matrix |
| 645 | N/A | mount-option matrix |
| 646 | N/A | fs shutdown ioctl |
| 647 | N/A | page faults during read+write mix (write) |
| 648 | N/A | fs shutdown ioctl |
| 649 | N/A | hole punch |
| 650 | N/A | write I/O |
| 651 | N/A | reflink/clone |
| 652 | N/A | fallocate |
| 653 | N/A | fallocate |
| 654 | N/A | fallocate |
| 655 | N/A | fallocate |
| 656 | N/A | xattr/chattr write |
| 657 | N/A | reflink/clone |
| 658 | N/A | fallocate |
| 659 | N/A | fallocate |
| 660 | N/A | fallocate |
| 661 | N/A | fallocate |
| 662 | N/A | fallocate |
| 663 | N/A | fallocate |
| 664 | N/A | fallocate |
| 665 | N/A | fallocate |
| 666 | N/A | fallocate |
| 667 | N/A | fallocate |
| 668 | N/A | fallocate |
| 669 | N/A | fallocate |
| 670 | N/A | reflink/clone |
| 671 | N/A | reflink/clone |
| 672 | N/A | reflink/clone |
| 673 | N/A | reflink/clone |
| 674 | N/A | reflink/clone |
| 675 | N/A | reflink/clone |
| 676 | PORTED | dir_locks::generic_676_seekdir |
| 677 | N/A | fallocate |
| 678 | N/A | io_uring write paths |
| 679 | N/A | fallocate |
| 680 | PORTED | write_attack::generic_680_dirty_pipe |
| 681 | N/A | quota |
| 682 | N/A | quota |
| 683 | N/A | fallocate |
| 684 | N/A | hole punch |
| 685 | N/A | zero-range |
| 686 | N/A | insert-range |
| 687 | N/A | collapse-range |
| 688 | N/A | fallocate |
| 689 | N/A | idmapped mounts |
| 690 | N/A | log recovery |
| 691 | N/A | quota |
| 692 | N/A | fsverity |
| 693 | N/A | fscrypt |
| 694 | N/A | i_blocks >4GiB needs a 4GiB fixture blob (prohibitive build cost); the ceil(size/512) contract is unit-tested (generic/614 row) |
| 695 | N/A | hole punch |
| 696 | N/A | write I/O |
| 697 | N/A | write I/O |
| 698 | N/A | xattr/chattr write |
| 699 | N/A | xattr/chattr write |
| 700 | N/A | rename (write) |
| 701 | N/A | as 694, plus truncate |
| 702 | N/A | reflink/clone |
| 703 | N/A | fallocate |
| 704 | N/A | scsi_debug O_DIRECT write |
| 705 | N/A | fs shutdown ioctl |
| 706 | PORTED | io_paths (with 285) |
| 707 | N/A | needs scratch mkfs (mutates fs) |
| 708 | N/A | iomap direct_io partial writes |
| 709 | N/A | quota |
| 710 | N/A | quota |
| 711 | N/A | swapext ioctl |
| 712 | N/A | exchangerange ctime (write) |
| 713 | N/A | exchangerange (write) |
| 714 | N/A | exchangerange (write) |
| 715 | N/A | exchangerange (write) |
| 716 | N/A | exchangerange (write) |
| 717 | N/A | needs scratch mkfs (mutates fs) |
| 718 | N/A | exchangerange (write) |
| 719 | N/A | exchangerange (write) |
| 720 | N/A | exchangerange stress (write) |
| 721 | N/A | exchangerange (write) |
| 722 | N/A | needs scratch mkfs (mutates fs) |
| 723 | N/A | needs scratch mkfs (mutates fs) |
| 724 | N/A | needs scratch mkfs (mutates fs) |
| 725 | N/A | needs scratch mkfs (mutates fs) |
| 726 | N/A | needs scratch mkfs (mutates fs) |
| 727 | N/A | needs scratch mkfs (mutates fs) |
| 728 | N/A | xattr/chattr write |
| 729 | N/A | read/write fault mix (write) |
| 730 | N/A | scsi_debug shutdown |
| 731 | N/A | scsi_debug write |
| 732 | N/A | rename (write) |
| 733 | N/A | hole punch |
| 734 | N/A | reflink/clone |
| 735 | N/A | fallocate |
| 736 | N/A | readdir-vs-rename infinite-loop: rename is impossible here; bounded-drain termination is pinned by the generic/676 garbage-cookie leg |
| 737 | N/A | fs shutdown ioctl |
| 738 | N/A | freeze/thaw |
| 739 | N/A | fscrypt |
| 740 | N/A | mkfs detection |
| 741 | N/A | needs scratch mkfs (mutates fs) |
| 742 | N/A | FIEMAP (no extents to report) |
| 743 | N/A | write I/O |
| 744 | N/A | reflink/clone |
| 745 | N/A | log recovery |
| 746 | N/A | FIEMAP (no extents to report) |
| 747 | N/A | needs scratch mkfs (mutates fs) |
| 748 | N/A | crash/fuzz test |
| 749 | N/A | fallocate |
| 750 | N/A | write I/O |
| 751 | N/A | needs scratch mkfs (mutates fs) |
| 752 | N/A | exchangerange swapfile |
| 753 | N/A | fs shutdown ioctl |
| 754 | N/A | needs scratch mkfs (mutates fs) |
| 755 | N/A | unlink ctime (write) |
| 756 | N/A | handle staleness needs unlink; refusal covered by the 467 probe |
| 757 | N/A | log recovery |
| 758 | N/A | write I/O |
| 759 | N/A | write I/O |
| 760 | N/A | write I/O |
| 761 | N/A | needs scratch mkfs (mutates fs) |
| 762 | N/A | fallocate |
| 763 | PARTIAL | zero-byte write contract: write(fd, buf, 0) on an O_RDONLY fd is EBADF → fold into errno_battery::open_flag_contracts (batch 2) |
| 764 | N/A | log recovery |
| 765 | N/A | write I/O |
| 766 | N/A | fs shutdown ioctl |
| 767 | N/A | write I/O |
| 768 | N/A | write I/O |
| 769 | N/A | write I/O |
| 770 | N/A | write I/O |
| 771 | N/A | log recovery |
| 772 | N/A | file_setattr syscall (write) |
| 773 | N/A | write I/O |
| 774 | N/A | write I/O |
| 775 | N/A | write I/O |
| 776 | N/A | write I/O |
| 777 | N/A | connectable handles; refusal covered by the 467 probe |
| 778 | N/A | atomic writes + shutdown |
| 779 | N/A | log recovery |
| 780 | N/A | file_setattr on special files; castore serves no special files |
| 781 | N/A | zoned block device smoke |
| 782 | N/A | log recovery |
| 783 | N/A | casefold feature |
| 784 | N/A | log recovery |
| 785 | N/A | log recovery |
| 786 | N/A | directory delegations need F_SETDELEG kernel gating; the kernel-local read-lease class is covered by the 571 lease leg |
| 787 | N/A | file delegations; as 786 |
| 788 | N/A | fsverity |
| 789 | N/A | log recovery |
| 790 | N/A | log recovery |
| 791 | N/A | error injection |
| 792 | N/A | log recovery |
| 793 | N/A | zoned GC stress (write) |

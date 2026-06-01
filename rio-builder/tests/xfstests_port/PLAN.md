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
  mutation was EROFS. The castore mount is *not* MS_RDONLY — write
  protection for the build uid comes from `default_permissions` +
  root-owned 0444/0555 attrs, and the daemon answers EROFS for any
  write-mode open or mutation op that gets past the kernel check
  (`builder.fs.open-read-only`, `builder.fs.write-ops-erofs`). Adapted
  to "mutations by the build uid fail (EACCES/EPERM) and the tree is
  unchanged" for the unprivileged leg; the root-credential legs assert
  the EROFS contract (see Findings).
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
| 8 | generic/050 | write protection of the input mount | The mount is not MS_RDONLY; protection = root-owned 0444/0555 + `default_permissions`. Every mutation by the build uid must fail and leave the tree unchanged — a regression here lets builds scribble on (or appear to scribble on) shared inputs. | VM (EACCES/EPERM adaptation, content unchanged) | P1 | ported (adapted) |
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
| 25 | generic/011 | dirstress | Read-only adaptation: many concurrent processes walking/listing the same tree (FUSE_PARALLEL_DIROPS is negotiated). Complements #13 on the lookup/readdir side. | VM | P2 | — |
| 26 | generic/074 | fstest (read/mmap patterns) | mmap-read == read() comparison through the mount; with passthrough the mapping is backed by the cache file, in KEEP_CACHE fallback by FUSE pages — both must serve identical bytes. | VM; python3 (mmap) on the client | P2 |ported (`io_paths::generic_074_127_mmap_reads`) |
| 27 | generic/127 | fsx incl. memory-mapped reads | Larger mmap-read coverage once #26's helper exists; also worth one run with `RIO_DISABLE_PASSTHROUGH=1` (the escape hatch changes the read path entirely). | VM (with #26) | P2 |ported (with #26; MAP_PRIVATE+MAP_SHARED legs) |
| 28 | generic/028 | path resolution / getcwd correctness | cd deep into the mount, `pwd -P`, resolve `..` — `..` of a content-addressed dir resolves through the dcache (the readdir dot entries deliberately self-point); a regression here breaks relative-path builds. | VM | P2 | — |
| 29 | generic/088 | permission checks for unprivileged users (DAC) | With `default_permissions` the kernel enforces the served 0444/0555 root-owned modes for arbitrary uids — probes R/W/X as a second unprivileged uid (1001/991, distinct from the build uid the other batteries use) to pin allow_other + DAC interplay. | VM (`dir_locks::generic_088_second_uid_dac`, second-uid PrivDrop) | P2 | ported |
| 30 | generic/249 | splice(2) read | splice/sendfile from a FUSE-backed file is a distinct kernel read path; with passthrough it should hit the backing file directly — bytes must match on both passthrough and KEEP_CACHE handles. | VM | P2 |ported (`io_paths::generic_249_splice_read`) |
| 31 | generic/430 | copy_file_range basic copies (FUSE as source) | Since kernel 5.19, userspace cross-fs cfr without a native FS op is EXDEV by policy — and EXDEV is the errno coreutils' fallback path expects, so the contract is "byte-exact copy or exactly EXDEV, nothing else". Any other errno turns `cp` from the mount into a hard error. Overlay copy-up is NOT this path (it uses the kernel-internal COPY_FILE_SPLICE flag; covered e2e by vm-castore-e2e). | VM (`io_paths::generic_430_553_copy_file_range`, native libc cfr) | P2 |ported (EXDEV-contract assertion; oracle + generic/553 EOF legs activate if a future kernel/FS allows the copy) |
| 32 | generic/131 | POSIX/fcntl lock smoke on read-only files | Read locks via fcntl/flock on a RO FUSE file must be granted and tracked kernel-locally (the daemon implements no lock ops). Cross-process F_GETLK proves the kernel is the lock manager; F_WRLCK through an RO fd is EBADF; flock is best-effort (ENOSYS accepted, a grant must be enforced). Configure scripts and sqlite-using builds take read locks on inputs. | VM (`dir_locks::generic_131_read_locks`) | P2 | ported |
| 33 | generic/614 | st_blocks consistency | Non-empty files report st_blocks == ceil(size/512) (`make_attr`); `du` on the mount stays sane (it is how builds estimate input size). | VM (cheap; fold into the meta subtest); Rust attr check | P2 | partially ported (Rust) |
| 34 | generic/436 | further SEEK_DATA/SEEK_HOLE sanity | Only adds value beyond #20 if backing-cache files become sparse (they are written by sequential fetch, so currently never). | VM | P3 | — |
| 35 | generic/445 | another SEEK_DATA/SEEK_HOLE pattern | Same caveat as #34. | VM | P3 | — |
| 36 | generic/490 | SEEK_DATA inside large holes | Same caveat as #34. | VM | P3 | — |
| 37 | generic/539 | SEEK_HOLE finds a punched hole | No hole punching on this fs; keep only as "no false holes reported". | VM | P3 | — |
| 38 | generic/263 | fsx O_DIRECT vs buffered | O_DIRECT open/read through FUSE passthrough either works or fails cleanly with EINVAL — never silent corruption. | VM | P3 | — |
| 39 | generic/564 | copy_file_range error conditions | Error-path companion to #31 (bad fds, overlapping ranges, RO destination). | VM | P3 | — |
| 40 | generic/565 | copy_file_range across devices | Cross-device leg of #31 (mount → tmpfs). | VM | P3 | — |
| 41 | generic/339 | large-directory entry ordering | Through this FUSE it reduces to "big readdir is complete", covered by #1 at 200 entries. Revisit with a chromium-scale (~3k-entry) Directory if real closures get that wide (tree.rs notes the linear scan budget). | VM | P3 | — |
| 42 | generic/637 | dir content visibility across fresh fds (small getdents buffer) | The fs is immutable so the visibility half is moot; the small-getdents-buffer half is a useful stressor for offset resume once a helper exists (pairs with #24). | VM + helper | P3 | — |
| 43 | generic/467 | open_by_handle_at variants | No FUSE export support → name_to_handle_at must fail EOPNOTSUPP cleanly, nothing more. | VM | P3 | — |
| 44 | generic/477 | open_by_handle after cycle mount | Same as #43; remount-equivalents are covered by the teardown/cache-hit phases of vm-castore-fuse. | VM | P3 | — |
| 45 | generic/422 | stat blocks under delayed allocation | No delalloc on this fs; only the st_blocks sanity already in #33. | VM | P3 | — |
| 46 | generic/003 | atime/relatime/nodiratime semantics | Attrs are immutable epoch+1 with infinite TTLs → atime never changes; a single "atime stable across reads" assertion is the whole port. | VM | P3 | — |
| 47 | generic/192 | atime update + persistence | Same as #46 (persistence does not apply — the mount is per-build and ephemeral). | VM | P3 | — |
| 48 | generic/013 | fsstress | Only meaningful with a read-only op profile (stat/readdir/readlink/open/read); useful soak for `Opener` map growth + backing-id churn under load. Needs the fsstress binary packaged. | VM | P3 | — |
| 49 | generic/241 | parallel dbench | Replace dbench with a parallel tree-walk + checksum loop; mostly covered by #13/#25, and the production-shaped load test is vm-castore-e2e's parallel builds. | VM | P3 | — |
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
* **F-B — directory nlink is 1** (generic/002 adaptation): legal
  (btrfs does the same) and `find` copes, but tools that trust
  `st_nlink-2 == subdir count` will miscount. Recorded as a design
  note, asserted as nlink==1 so a silent change is noticed.
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
errors"); `generic_002` asserts the inverse of upstream (nlink==1,
no hardlinks — F-B); `generic_095_113_310` carries only generic/113's
sync legs (AIO/io_uring readers remain the deferred P3 extension).

Next: #25 (generic/011 dirstress, read-only adaptation) and #28
(generic/028 getcwd/`..` resolution), then the P3 tier. After that,
decide whether F-A warrants a real `statfs` implementation and convert
the informational probe into an assertion.

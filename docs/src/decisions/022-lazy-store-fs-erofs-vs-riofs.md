# ADR-022: Lazy /nix/store filesystem — castore FUSE

Status: **Accepted.** Three earlier candidates (EROFS+fscache, custom `riofs` kmod, EROFS+composefs-style) were evaluated and set aside; see §3.

**Scope:** the builder-side `/nix/store` filesystem and the rio-store metadata that supports it. The canonical design reference is the [Design Overview](./022-design-overview.md); sequencing is in the [Implementation Plan](./022-implementation-plan.md).

---

## 0. Deployment context

Builder nodes run a NixOS-based AMI. Kernel configuration is first-party: `boot.kernelPatches[].extraStructuredConfig` in the AMI flake sets `OVERLAY_FS=y FUSE_FS=y FUSE_PASSTHROUGH=y` and the node module asserts kernel ≥6.9 at boot. No custom Kconfig symbols are required — all are stock-on in distro defconfigs; the patch block is for `=y` over `=m` only.

Device exposure: `/dev/fuse` reaches the builder via `rio-mountd` fd-handoff (§2.5); `/dev/kvm` reaches kvm-pool builds via `hostPath` CharDevice + `extra-sandbox-paths`. No device-plugin DaemonSet.

---

## 1. Problem statement

The pre-ADR-022 builder presents `/nix/store` via a FUSE filesystem (`rio-builder/src/fuse/`) that JIT-fetches whole store paths on first access. This is correct but has three structural costs:

1. **Fetch is path-granular.** `lookup` of a top-level basename materializes the entire store-path tree before returning. Touching one 4 KB header in a 1 GB output fetches 1 GB.
2. **Warm-but-partial files still upcall.** FUSE passthrough binds one backing fd at `open()`. A 200 MB `libLLVM.so` with 4 MB of hot `.rodata` either upcalls on every read until the whole file is materialized, or blocks `open()` for ~1.3 s fetching it whole. The kernel page cache cannot serve a warm range of a partially-fetched file without crossing to userspace.
3. **No cross-path content dedup.** Two store paths containing the same `.so` are two FUSE inodes, two fetches, two SSD copies, two page-cache copies.

The goal: cold fetch is file-granular (not path-granular); warm reads of any range hit page cache with zero crossings; identical files across store paths are one fetch and one page-cache entry.

---

## 2. Design — castore FUSE

The build's `/nix/store` is a single overlayfs over a **content-addressed FUSE** that serves the closure's [castore](https://snix.dev/docs/components/castore/data-model/) Directory DAG: `lookup`/`getattr`/`readdir`/`readlink` are answered from an in-memory tree; `open()` fetches the file by `file_digest` into a node-SSD cache and replies passthrough. This is the [snix-store](https://git.snix.dev/snix/snix/src/branch/canon/snix/store) filesystem model with rio's chunk backend underneath and `rio-mountd` brokering the privileged ioctls.

The tree is immutable for the mount's lifetime, so the FUSE handler advertises infinite cache TTLs and the kernel's dcache/icache absorb all repeat metadata access. Warm `read()` is page-cache via passthrough; the handler is upcalled only for cold `lookup` and cold `open()`.

### 2.1 Mount stack

r[builder.fs.castore-stack]

The build's `/nix/store` is a **single read-write overlayfs** with an SSD upper (build outputs land here) over one read-only lower: the castore-FUSE mount. Mount string: `overlay -o userxattr,upperdir=<ssd>/nix/store,workdir=<ssd>/work,lowerdir=<castore_mnt>`; the merged dir is bind-mounted at `/nix/store` inside the build's mount namespace. Upper/work dirs are local SSD (`r[builder.overlay.upper-not-overlayfs]`); outputs and the synthesized `db.sqlite` live under the upper root. This is the same overlay shape as the pre-ADR-022 mount — only the lower's granularity changed.

| Syscall | Resolved by | FUSE upcalls |
|---|---|---|
| `stat`/`getattr` (input) | overlay → FUSE `lookup`/`getattr` | **1 cold / 0 thereafter** (`Duration::MAX` ttl) |
| `readdir` (input) | overlay → FUSE `readdirplus` | **1 cold / 0 thereafter** (`FOPEN_CACHE_DIR`); populates dcache for children |
| `readlink` (input) | overlay → FUSE `readlink` | **1 cold / 0 thereafter** (`FUSE_CACHE_SYMLINKS`) |
| `open` input (cold) | overlay → FUSE `open` → backing-cache fetch | **1 open** |
| `read`/`mmap` input (cache hit / post-fill) | `FOPEN_PASSTHROUGH` → backing fd | **0** |
| `read`/`mmap` input (large file mid-fill) | FUSE `read` (~128 KiB/req) | O(touched / 128 KiB), once per file per node |
| `lookup` ENOENT (configure-probe) | overlay negative dcache (I-043) | **1 cold / 0 thereafter** |
| **write / create output** | **overlay upper (SSD)** | 0 |
| modify input → copy-up | overlay full-data-copies from FUSE lower into upper | as cold open+read |

§1's constraint — a 200 MB partially-hot `.so` either upcalls every read or blocks open — **is addressed by streaming open (§2.8) during fill, then passthrough (§2.6) thereafter.** The first open returns after the first chunk; uncached `read()` ranges upcall once during the background fill. Once the file is in the node-SSD cache, every subsequent open replies `FOPEN_PASSTHROUGH` and reads go kernel → ext4 with **zero FUSE involvement** — including after page-cache eviction.

### 2.2 Tree source — Directory DAG

r[builder.fs.castore-dag-source]

At mount time the builder holds the closure's set of input store paths and each path's `root_node` (from the scheduler-supplied `WorkAssignment.input_roots`, `r[sched.dispatch.input-roots]`). It prefetches the full Directory DAG via **`GetDirectory(root_digest, recursive=true) → stream<Directory>`** (`r[store.castore.directory-rpc]`). Chromium-scale (8 221 dirs, 23 218 files) is on the order of 5 MiB in heap.

The synthetic root (`FUSE_ROOT_ID`, `/nix/store`) is the only node not in the DAG — its children are the closure's store-path basenames, each mapping to that path's `root_digest` (or `file_digest`/symlink-target if the store path itself is a regular file or symlink). `readdir` of the root enumerates the closure; `lookup(ROOT, basename)` returns the path's content-derived inode (§2.3).

`NarIndex` is not fetched at mount — the Directory DAG carries everything `lookup`/`getattr`/`readdir`/`readlink` need (`{name, digest, size, executable}` per `FileEntry`; `{name, target}` per `SymlinkEntry`). At `open()`, chunk coordinates are resolved **server-side**: ≤ `STREAM_THRESHOLD` files call `ReadBlob(file_digest)` (`r[store.castore.blob-read]`) which streams bytes directly; > threshold call `StatBlob(file_digest, send_chunks=true)` (`r[store.castore.blob-stat]`) for the `ChunkMeta[]` list, then check the node chunk-cache and `GetChunks` misses. Both RPCs key on `file_digest` alone — no client-side resolver, no per-path `NarIndex`/`ChunkList` prefetch.

### 2.3 Inode model — content-addressed

r[builder.fs.castore-inode-digest]

Inode numbers are derived from content, not path: `ino(FileEntry) = h(file_digest ‖ executable)`; `ino(DirectoryEntry) = h(dir_digest)`; `ino(SymlinkEntry) = h("l" ‖ target)`, where `h` is the low 63 bits of blake3 with bit 63 set (so `ino ≥ 2`, never colliding with `FUSE_ROOT_ID = 1`). Two regular files anywhere in the closure with the same bytes **and the same executable bit** get the **same FUSE inode** — the kernel's icache holds one `struct inode`, one `open()` upcall fetches it once, and one `BACKING_OPEN` binds it to one page-cache. Two directories with identical `dir_digest` get the same inode and the **same dcache subtree** — `lookup(ino, name)` is answered from one `Directory` body regardless of which store path led there.

The executable bit is part of the inode key because `i_mode` is per-inode in VFS — two paths sharing an inode share `st_mode`, so same-bytes/different-exec must be distinct inodes. This costs nothing for fetch or page-cache dedup: both inodes' `open()` resolve to the same `cache/ab/<file_digest>` backing file (the cache is keyed by `file_digest` alone), so one fetch and one page-cache entry serve both. Only the kernel `struct inode` is duplicated.

The handler's state is `HashMap<u64, Node>` where `Node ∈ { File{file_digest, size, executable}, Dir{dir_digest}, Symlink{target} }`, populated during the §2.2 prefetch. `lookup(parent_ino, name)` reads the parent's `Directory` body (already in heap), finds the child by name, returns `ReplyEntry{ ino: h(child), attr, ttl: Duration::MAX }`. `getattr(ino)` is a map lookup. Collisions on the 63-bit hash are not handled — `2^-63` per pair over a ~35k-node tree is negligible, and a collision manifests as one file's content served for another's path, caught by the build's own output-hash check.

### 2.4 Kernel caching configuration

r[builder.fs.castore-cache-config]

The tree is immutable for the mount's lifetime, so every cache TTL is infinite and every cache-enable flag is set. This is [snix's exact configuration](https://git.snix.dev/snix/snix/raw/branch/canon/snix/castore/src/fs/mod.rs):

| Knob | Value | Effect |
|---|---|---|
| `ReplyEntry`/`ReplyAttr` ttl | `Duration::MAX` | Kernel never re-validates a positive dentry or attr. Kernel saturates the value via `timespec64_to_jiffies → MAX_SEC_IN_JIFFIES`; no overflow. |
| `init` capabilities | `FUSE_DO_READDIRPLUS \| FUSE_READDIRPLUS_AUTO \| FUSE_PARALLEL_DIROPS \| FUSE_CACHE_SYMLINKS` | `readdirplus` pre-populates dcache so a subsequent `stat` of every entry is 0-upcall; `PARALLEL_DIROPS` removes the per-inode mutex on concurrent lookups; `CACHE_SYMLINKS` makes `readlink` once-ever per target. |
| `opendir` reply flags | `FOPEN_CACHE_DIR \| FOPEN_KEEP_CACHE` | Kernel caches dirent pages; second `readdir` of the same dir is 0-upcall. |
| Negative reply | `reply.entry(&Duration::MAX, &FileAttr{ino: 0, ..}, 0)` | Caches the negative at the FUSE layer — `fuse_lookup_name` treats `nodeid=0` as "*same as -ENOENT, but with valid timeout*" ([`dir.c`](https://github.com/torvalds/linux/blob/master/fs/fuse/dir.c)); revalidate's `!inode → invalid` is inside the timeout-expired/`LOOKUP_EXCL\|REVAL\|RENAME_TARGET` branch, so with `ttl=MAX` and a plain `stat()` it stays valid. Under §2.1's overlay this is moot — overlay's own dcache caches lower-miss for either reply form (I-043, [`vm-spike-fuse-negdentry`](../../nix/tests/scenarios/spike-fuse-negdentry.nix)). The handler uses `ino=0` for bare-mount correctness. |

overlayfs delegates dentry revalidation to the lower's `d_op->d_revalidate` ([`ovl_revalidate_real`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/super.c)), so FUSE's `entry_timeout` is what overlay consults — infinite TTL means overlay's dentries never re-ask FUSE. Slab cost for ~35k cached `dentry`+`inode` is ~28 MB; shrinkable under memory pressure (re-upcalls if evicted, harmless on multi-GB builder pods).

### 2.5 Mount sequence and privilege boundary

Privilege is split: a node-level **`rio-mountd`** (CAP_SYS_ADMIN, DaemonSet) handles the operations the unprivileged builder cannot: open `/dev/fuse`, broker `FUSE_DEV_IOC_BACKING_OPEN`/`_CLOSE` (init-ns `CAP_SYS_ADMIN` — [`backing.c:91-93`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c), kernel `TODO: relax once backing files are visible to lsof`), and own the verified-write to the shared node caches. Per build connection on the UDS:

1. `fuse_fd = open("/dev/fuse")`; `dup` it (mountd keeps one, sends the other) → `mount("fuse", castore_mnt, …, "fd=N,allow_other,default_permissions,…")` where `castore_mnt = /var/rio/castore/{build_id}` → `SCM_RIGHTS` the fd to the builder
2. `mkdir /var/rio/staging/{build_id}` with the requesting builder's uid/gid (from `SO_PEERCRED`). The UDS socket is mode 0660, group `rio-builder`; mountd verifies `SO_PEERCRED.gid == rio-builder` and rejects others.
3. Serve `seq`-tagged requests for the build's lifetime (tokio per-conn task; replies echo `seq` so out-of-order `Promote` correlates): **`BackingOpen{fd}`** — `ioctl(kept_fuse_fd, FUSE_DEV_IOC_BACKING_OPEN, {fd}) → backing_id` (mountd does not inspect the fd — the ioctl rejects depth>0 backing, and `backing_id` is conn-scoped). **`BackingClose{id}`**. **`PromoteChunks{[chunk_digest]}`** (batched ≤64) — verify-copy each `staging/chunks/<hex>` → `/var/rio/chunks/ab/<hex>`. **`Promote{digest}`** (on `spawn_blocking` + `Semaphore(num_cpus)`) — verify-copy the assembled file from staging into cache (§2.6).

The builder, inside its unprivileged userns (after receiving the fd from step 1):

a. `fuser::Session::from_fd(fuse_fd)` → spawn `castore_fuse::serve` (§2.6) — **must be answering before (b)**
b. `mount("overlay", merged, "overlay", 0, "userxattr,upperdir=<ssd>/nix/store,workdir=<ssd>/work,lowerdir=<castore_mnt>")` — then bind `merged` at `/nix/store` inside the build's mount namespace

Teardown — builder closes the UDS (or its pod exits):

- `rio-mountd` does `umount2(castore_mnt, MNT_DETACH)` + `rmdir(castore_mnt)` + `rm -rf staging/{build_id}` + `close(kept_fuse_fd)`; the builder's mount-ns death takes the overlay with it.

r[builder.mountd.orphan-scan]

Crash-safety: `rio-mountd` start-up scans `/var/rio/castore/*` and `/var/rio/staging/*` for orphans from a prior crash and removes them.

r[builder.fs.fd-handoff-ordering]

**Ordering is load-bearing:** the `/dev/fuse` fd MUST be received and the castore-FUSE server MUST be answering before builder step (b). overlayfs probes the lower's root at `mount(2)`; with no one serving `/dev/fuse`, that probe deadlocks.

### 2.6 `open()` — backing cache, passthrough, streaming

r[builder.fs.digest-fuse-open]

`open(ino)` resolves `ino → file_digest` (the §2.3 map) and is a **broker, not a server**: its job is to ensure the file exists in the node-SSD backing cache and hand the kernel a passthrough fd to it.

r[builder.fs.passthrough-on-hit]

The handler negotiates `FUSE_PASSTHROUGH` at `init` (with `max_stack_depth = 1`; the backing cache is a non-stacking fs). On `open(ino → file_digest)`:

1. **Cache hit** (`/var/rio/cache/ab/<digest>` exists, mountd-owned): open it `O_RDONLY`, send the fd to `rio-mountd` → receive `backing_id` (mountd does the `FUSE_DEV_IOC_BACKING_OPEN` ioctl, §2.5); reply `FOPEN_PASSTHROUGH | backing_id`. All subsequent `read`/`mmap` go kernel → backing file via `fuse_passthrough_read_iter`; the handler sees no further upcalls for this fd. `release` sends `BackingClose{id}` to mountd.
2. **Cache miss, `size ≤ STREAM_THRESHOLD`**: `ReadBlob(file_digest)` streams bytes into `/var/rio/staging/{build_id}/<digest>.partial`, whole-file verify, send `Promote{digest}` → mountd verify-copies into cache → as (1).
3. **Cache miss, `size > STREAM_THRESHOLD`**: streaming-open (§2.8) — `StatBlob(file_digest, send_chunks=true)` for the chunk list, reply `FOPEN_KEEP_CACHE`, serve `read` from staging `.partial` during background fill. The fill task sources each chunk from `/var/rio/chunks/` first, falling back to `GetChunks` for misses (firing `PromoteChunk` for each). On completion: whole-file verify, `Promote{digest}`. Next `open` hits (1). Concurrent builds race on `PromoteChunk` and share progress at chunk granularity — no leader/follower coordination needed.
4. Within-build `.partial` orphan: `flock(LOCK_NB)` not held → unlink + retry.

The FUSE `read` op exists only for case (3)'s streaming window; in steady state every open is passthrough.

r[builder.fs.shared-backing-cache]

The FUSE **mount point** is per-build (`/var/rio/castore/{build_id}/`, §2.5) for cross-pod isolation. The **backing cache** (`/var/rio/cache/ab/<digest>`) is shared node-SSD, **owned by `rio-mountd` and read-only to builder pods** (mode 0755/0444). Builders cannot write the cache directly — a sandbox-escaped build writing poisoned bytes there would otherwise be passthrough-read by the next build unverified (the same lateral-movement surface §2.8 rejects for a cluster-wide cache).

r[builder.mountd.promote-verified]

Instead, the builder stages fetched files in a **per-build staging dir** (`/var/rio/staging/{build_id}/`, builder-writable, created by mountd at `Mount` time) and mountd verify-copies into the cache on `Promote`. mountd opens the staging file `O_RDONLY|O_NOFOLLOW`, rejects non-regular or oversized, creates `cache/ab/<digest>.promoting` (mountd-owned, `O_EXCL|O_WRONLY`, 0444), **stream-copies while hashing**, verifies `blake3 == digest`, renames `.promoting → final`. The copy is the integrity boundary: the cache file is a distinct inode mountd created and hashed.

r[builder.fs.node-chunk-cache]

**Cross-build fetch dedup is chunk-granular** for files > `STREAM_THRESHOLD`. mountd also owns `/var/rio/chunks/ab/<chunk_blake3>` (read-only to builders). The streaming fill task `open()`s the chunk cache entry first (ENOENT → miss); for each miss it writes the verified chunk into `.partial` at offset *and* into `staging/chunks/<hex>`, batching digests into `PromoteChunks{[..]}` (mountd verify-copies into the chunk cache). Assembly proceeds from the build's own staging — `PromoteChunks` is purely for *other* builds' benefit and never blocks this build's fill. Concurrent builds race on the chunk-cache rename; the loser reads the winner's chunk — **no builder fetches a chunk another build on this node already verified.** Chunks are independently content-addressed, so mountd's `blake3(bytes) == chunk_digest` check is context-free (unlike file ranges, which would need the chunk-map). `BACKING_OPEN`'s `d_is_reg` gate ([`backing.c:105-108`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c)) and this context-free-verify constraint together pin the design to "regular files whose names are their hashes" — block-device alternatives (ublk, dm-verity) fail the ioctl. Files ≤ threshold skip the chunk cache (whole-file fetch is fast enough; dup-fetch cost negligible).

### 2.7 Integrity — per-file blake3 in handler

r[builder.fs.file-digest-integrity]

Per-file integrity lives in the FUSE `open()` handler: **verify each chunk's blake3 against its content-address on arrival** (chunks are blake3-addressed in the CAS layer; `rio-store/src/chunker.rs`); never serve a byte from an unverified chunk. For files ≤ the streaming threshold (§2.8), the whole-file blake3 against `file_digest` additionally runs before `open()` returns. For files > threshold, per-chunk verification covers the streaming window and the whole-file `file_digest` check runs at fill-complete, gating the `.partial → /var/rio/cache/ab/<digest>` rename. The digest is the cache filename — the check is structural. fs-verity is not used: FUSE's fs-verity support ([`9fe2a036`](https://git.kernel.org/linus/9fe2a036a23ceeac402c4fde8ec37c02ab25f133), 6.10) is ioctl-forwarding only and does not populate `inode->i_verity_info`, so neither overlayfs's `ovl_validate_verity()` nor any other in-kernel consumer can use it; and the daemon answering the ioctl is the daemon serving the bytes — no stronger than blake3-in-handler.

### 2.8 Failure modes and streaming open

| Failure | Kernel behavior | rio handling |
|---|---|---|
| **FUSE handler crash** | overlayfs `lookup`/`open()` on the lower → `ENOTCONN`. **Passthrough-opened files keep working** — `fuse_passthrough_read_iter` ([passthrough.c:28-51](https://github.com/torvalds/linux/blob/master/fs/fuse/passthrough.c)) reads `ff->passthrough` directly with no `fc->connected` check; `fuse_abort_conn` ([dev.c:2451-2522](https://github.com/torvalds/linux/blob/master/fs/fuse/dev.c)) never touches `ff->passthrough` or `fc->backing_files_map`. Streaming-mode opens lose their `read` server. | Abort is terminal — the build is failed-infra and re-queued; the next pod issues a fresh `Mount{}` and gets a new `fuse_conn`. The IDR slot for the dead connection's backing-ids leaks until unmount (bounded; reaped at `fuse_conn_put`). **No D-state**, no `restore` dance. |
| **FUSE handler hung mid-fetch** | `open()` blocks in `S` (interruptible — `request_wait_answer` [dev.c:552/568](https://github.com/torvalds/linux/blob/master/fs/fuse/dev.c) `wait_event_interruptible`/`_killable`). | Per-spawn `tokio::timeout` returns `EIO` to the open; build fails loudly. |
| **Lookup ENOENT** | overlay caches negative dentry; subsequent probes 0-upcall. | Handler returns ENOENT only for names outside the closure's declared-input tree — correct (JIT fetch imperative). |
| **Build completes / pod exits** | Builder mount-ns death drops overlay; `castore_mnt` FUSE mount persists in init-ns. | `rio-mountd` detaches it on UDS close (§2.5 teardown); start-up scan reaps orphans from a prior crash. |

r[builder.fs.streaming-open-threshold]

**Streaming open is the during-fill mode for large files.** The 1000 largest files in nixpkgs are *all* >64 MiB (median 179 MiB, 7 files >1 GiB; `top1000.csv`), and access-pattern measurement (`42aa81b2`) shows consumers touch 0.3-33% of them — whole-file fetch over-fetches 64-99.7%. For files > `STREAM_THRESHOLD` (default 8 MiB) on cache miss, `open()` cannot reply passthrough (no complete backing fd yet) so it replies `FOPEN_KEEP_CACHE`, spawns a background fill task, and returns after the first chunk (~10 ms). `read(off,len)` upcalls once per uncached page during fill (priority-bumping the requested range); `mmap(MAP_PRIVATE)` page-faults route through the same `read` path, covering linkers. ~80 LoC; spike-proven (`15a9db79`) — `KEEP_CACHE` does not suppress cold-page upcalls, only invalidation, so no mode-flip is needed within the streaming window. Once the fill completes and renames into `/var/rio/cache/`, the next `open()` of that digest takes the passthrough path (§2.6 case 1) — the streaming mode is one-shot per file per node.

**Considered and rejected for the partial-file case:** allowlist-bounded prefetch of giants at mount (violates the JIT-fetch imperative — fetches inputs the build may never touch); FSx-Lustre-backed cluster-wide `objects` cache (violates the builder air-gap — shared writable FS between untrusted builders is a cache-poisoning + lateral-movement surface).

### 2.9 Kconfig (NixOS)

```nix
boot.kernelPatches = [{
  name = "overlay-castore-fuse";
  patch = null;
  extraStructuredConfig = with lib.kernel; {
    OVERLAY_FS       = yes;       # nixpkgs default =m
    FUSE_FS          = yes;
    FUSE_PASSTHROUGH = yes;
  };
}];
```

Requires kernel **≥6.9** (`FUSE_PASSTHROUGH`, [`7dc4e97a4f9a`](https://git.kernel.org/linus/7dc4e97a4f9a)). The NixOS-node module asserts version + config at boot.

r[builder.fs.passthrough-stack-depth]

**Stacking depth:** with `max_stack_depth = 1` at FUSE `init`, the FUSE superblock has `s_stack_depth = 1` ([`inode.c:1439-1444`](https://github.com/torvalds/linux/blob/master/fs/fuse/inode.c) — the in-kernel comment explicitly names overlay-as-upper as the supported case). Overlay over `{FUSE(1)}` is depth 2; `FILESYSTEM_MAX_STACK_DEPTH = 2` and overlayfs checks `>` ([`super.c:1207-1208`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/super.c)), so 2 passes. `BACKING_OPEN` rejects backing files whose `s_stack_depth >= max_stack_depth` ([`backing.c:110-112`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c)) — with `=1`, the backing cache must be depth-0 (ext4/xfs hostPath), not another overlay or FUSE. `FUSE_PASSTHROUGH` and `FUSE_WRITEBACK_CACHE` are mutually exclusive (inode.c:1440); the castore-FUSE is read-only and does not request writeback. The privilege-boundary spike VM test asserts the overlay mount succeeds at depth 2, an unprivileged `BACKING_OPEN` fails `EPERM`, and a mountd-brokered one succeeds with reads reaching the ext4 backing.

### 2.10 Spike evidence

The streaming-open, access-pattern, and privilege-boundary spikes apply unchanged to this design (the FUSE `open`/`read`/passthrough mechanics are identical; only the metadata-serving layer differs). The composefs-stack metadata spikes (`composefs-spike.nix`, `-scale.nix`) measured the §3 EROFS alternative, not §2.

| Commit | Test | Finding |
|---|---|---|
| `15a9db79` | [`composefs-spike-stream.nix`](../../nix/tests/scenarios/composefs-spike-stream.nix) | Streaming-open (§2.8): `FOPEN_KEEP_CACHE` set at `open()` does not suppress cold-page upcalls (2049 reads on first `dd` of 256 MiB), only prevents invalidation — second `dd` 0 upcalls. **No mode-flip needed.** `mmap(MAP_PRIVATE)` page-faults route through FUSE `read`. `open()` 256 MiB with 10 ms/chunk backend → 10.3 ms (vs 2560 ms whole-file). |
| `42aa81b2` | [`spike-access-data/RESULTS.md`](../../nix/tests/lib/spike-access-data/RESULTS.md) | Access patterns: real consumers touch **0.3-33%** of giant `.so`/`.a` (link-against-libLLVM 2.79% bimodal head+tail; `opt --version` 32.77% scattered/266 ranges; `libicudata` 0.28%). `ld.so` uses no `MAP_POPULATE`/`fadvise`. |
| `af8db499` | [`composefs-spike-priv.nix`](../../nix/tests/scenarios/composefs-spike-priv.nix) | Privilege boundary (§2.5): fd-handoff, stack-survives-mounter-exit, unpriv-userns-inherits, `userxattr` unpriv overlay, teardown-under-load (no D-state) all PASS on kernel 6.18.20. The `fsmount`-erofs subtests are §3-only. |
| `9492019c` | [`kvm-hostpath-spike.nix`](../../nix/tests/scenarios/kvm-hostpath-spike.nix) | `/dev/kvm` via `extra-sandbox-paths`: `ioctl(KVM_GET_API_VERSION)=12` from inside Nix sandbox; smarter-device-manager not required. |

[snix-store](https://git.snix.dev/snix/snix/src/branch/canon/snix/store) is the production validation of the §2 model — it serves a castore Directory DAG via FUSE with the §2.4 caching configuration and is in active use.

---

## 3. Alternatives considered

Three other approaches were evaluated in depth and set aside. None is retained as a fallback.

**EROFS + fscache on-demand.** Per-store-path EROFS bootstrap blobs in S3, merged at build-start into one mount; `cachefilesd`-style userspace daemon answers `/dev/cachefiles` on-demand `READ{cookie,off,len}` upcalls by reverse-mapping `(cookie,off) → nar_offset → chunk-range`. ~2 700 owned LoC, all userspace. Ruled out: ~70 ms mount latency (one eager `OPEN` per device-slot at mount, ×357 paths), no kernel-side per-file dedup (cachefiles key is `(cookie,range)` not content), per-path S3 artifact + GC tracking, and a `(cookie,off)→nar_offset` reverse-map that exists only to bridge fscache's range-addressing to rio's chunk-addressing.

**Custom `riofs` kernel module.** ~2 800 LoC in-tree-style C: `read_folio` posts `{chunk_digest}` to a `/dev/riofs` ring, userspace fetches and `write()`s back. Elegant runtime (no merge, no rio-store change, native chunk addressing, optional kernel-side digest cache), but ~800 LoC of genuinely-novel folio-lock/completion kernel code with a 2-3 min VM dev loop and zero upstream review/fuzz coverage. The latency wins were ≤2% of cold-miss (network-bound) and the recurring VFS-API-churn + we-are-the-only-debuggers cost is permanent.

**EROFS metadata layer (composefs-style).** A per-closure EROFS image carrying inodes/dirents/sizes with `user.overlay.redirect=/ab/<blake3>` xattrs, stacked under overlayfs as `lowerdir=<erofs>::<digest-fuse>` (data-only lower, kernel ≥6.16). Spiked at chromium scale (`composefs-spike-scale.nix`, `15a9db79`): <10 ms mount, `find -type f` over 23k files in 60 ms with **0** FUSE upcalls, 5.3 MiB image encoded in 70 ms. The one thing this buys over §2 is **zero cold-metadata crossings** — `stat`/`readdir` are kernel-native from the first call, where §2 pays one upcall per dirent to populate the dcache. Not worth the cost: snix-store achieves all of §1's goals without it and is production-validated; §2's once-per-dirent cost has not been shown to matter in build wall-clock; and it adds an encoder (libcomposefs FFI + a 25-line patch), an S3 artifact type, a `losetup`/`fsopen` privileged path in mountd, and a kernel ≥6.16 floor. If cold-metadata cost is ever found to matter, the image is a derived encoding of the §2.2 Directory DAG (`root_digest → walk → mkfs.erofs --tar=headerball`), so adding it would be additive.

§2 matches all three alternatives on the warm path; achieves structural per-file dedup that fscache and `riofs` cannot without extra machinery; is ~1 400 owned LoC with zero kernel code, no patched C dependencies, and a smaller privileged surface than the pre-ADR-022 FUSE setup; and relaxes the kernel floor from ≥6.16 to ≥6.9.

---

## 4. Decision

**castore FUSE (§2)**, with no maintained fallback. Warm `read()` is passthrough (0 upcalls); per-file and per-subtree dedup are structural via content-addressed inodes; cold metadata is once-per-dirent then dcache-absorbed. The known cost — whole-file cold `open()` of giants — is resolved by streaming-open (§2.8), which ships unconditionally.

---

## 5. Sources

Primary:
- [snix castore data model](https://snix.dev/docs/components/castore/data-model/) + [`snix/castore/src/fs/mod.rs`](https://git.snix.dev/snix/snix/raw/branch/canon/snix/castore/src/fs/mod.rs) — the §2 model and its FUSE caching configuration
- [`snix/castore/protos/castore.proto`](https://git.snix.dev/snix/snix/raw/branch/canon/snix/castore/protos/castore.proto) — `Directory`/`FileEntry`/`DirectoryEntry`/`SymlinkEntry` (vendored as `rio-proto/proto/castore.proto`)
- [`fs/fuse/dir.c` `fuse_dentry_revalidate`](https://github.com/torvalds/linux/blob/master/fs/fuse/dir.c) — negative-dentry behavior
- [`fs/overlayfs/super.c` `ovl_revalidate_real`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/super.c) — overlay delegates revalidation to lower
- [`fs/fuse/backing.c`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c), [`fs/fuse/passthrough.c`](https://github.com/torvalds/linux/blob/master/fs/fuse/passthrough.c) — `BACKING_OPEN` cap check + `read_iter` no-connected-check
- [`7dc4e97a4f9a`](https://git.kernel.org/linus/7dc4e97a4f9a) — `FUSE_PASSTHROUGH` (≥6.9)
- [`9fe2a036`](https://git.kernel.org/linus/9fe2a036a23ceeac402c4fde8ec37c02ab25f133) — FUSE fs-verity ioctl-forwarding (does not populate `i_verity_info`)
- §3 EROFS alternative: [`containers/composefs`](https://github.com/containers/composefs), [`erofs/erofs-utils`](https://github.com/erofs/erofs-utils), [`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9) (data-only-lower under `userxattr`, ≥6.16), [`Documentation/filesystems/overlayfs.rst` §Data-only lower layers](https://docs.kernel.org/filesystems/overlayfs.html)
- Spikes (consolidated on `adr-022`): core/scale/stream `15a9db79` ([`composefs-spike.nix`](../../nix/tests/scenarios/composefs-spike.nix), [`-scale.nix`](../../nix/tests/scenarios/composefs-spike-scale.nix), [`-stream.nix`](../../nix/tests/scenarios/composefs-spike-stream.nix), [`spike_digest_fuse.rs`](../../rio-builder/src/bin/spike_digest_fuse.rs), [`spike_stream_fuse.rs`](../../rio-builder/src/bin/spike_stream_fuse.rs)); privilege boundary `af8db499` ([`-priv.nix`](../../nix/tests/scenarios/composefs-spike-priv.nix), [`spike_mountd.rs`](../../rio-builder/src/bin/spike_mountd.rs)); access patterns `42aa81b2` ([`spike-access-data/RESULTS.md`](../../nix/tests/lib/spike-access-data/RESULTS.md)); kvm hostPath `9492019c` ([`kvm-hostpath-spike.nix`](../../nix/tests/scenarios/kvm-hostpath-spike.nix))
- `~/src/nix-index/main/top1000.csv` — 1000 largest files in nixpkgs (streaming-open sizing)

Our code:
- `rio-store/src/grpc/put_path.rs`, `cas.rs`, `chunker.rs`, `manifest.rs`
- `rio-proto/proto/types.proto`
- `rio-builder/src/fuse/{mod,ops,fetch}.rs`, `overlay.rs`

---

## 6. Extension: chunked output upload (`PutPathChunked`)

The §2 stack optimizes the **read** side — inputs reach the build via a content-addressed FUSE lower. Outputs still leave the build via the pre-ADR-022 `PutPath`: two disk passes over the overlay upper (refscan, then NAR-stream), full NAR over gRPC, store-side **whole-NAR `Vec<u8>` buffer** ([`put_path.rs:431/485`](../../rio-store/src/grpc/put_path.rs)) before FastCDC. Output bytes are traversed ≥4× and the RAM buffer is the standing 40 GiB-RSS hazard the `nar_bytes_budget` semaphore exists to fence.

`PutPathChunked` moves chunking to the builder and reduces the wire to missing chunks plus a manifest. The justification holds at zero dedup: the store-side RAM buffer disappears, builder-side disk reads drop 2×→1×, and FastCDC CPU moves off the shared rio-store replicas onto per-build ephemeral cores. Dedup is upside, not the gate.

r[store.put.chunked]

```text
builder ──gRPC──▶ rio-store ──verify blake3──▶ S3 PutObject (S3-standard only)
   │  (only egress)     │  (per-chunk, on arrival;
   │                    │   no whole-NAR buffer)
   └── air-gapped: never reaches S3, S3 Express, or any network endpoint other than rio-store
```

**This does not widen the builder trust boundary or the cache-tier surface.** The builder's only network egress remains rio-store gRPC — the air-gap invariant is unchanged. S3 standard and S3 Express are reached **exclusively by rio-store**, exactly as in `PutPath` today; the difference is internal to rio-store: it S3-writes each verified chunk on arrival instead of accumulating the full NAR in a `Vec<u8>` first. rio-mountd is uninvolved; the per-AZ Express cache stays unreachable from the builder — chunks reach it only via rio-store's serve-side `TieredChunkBackend.get` read-through, not on `put` ([Design Overview §9](./022-design-overview.md)). The §2.8 rejection of a builder-writable shared FS stands unchanged.

### 6.1 Builder-side fused walk

r[builder.upload.fused-walk]

After the build exits, `upload_all_outputs` walks each output's directory tree **once**, in canonical NAR entry order (`r[builder.nar.entry-name-safety]`). Per regular file, a single read drives four sinks in lockstep: FastCDC rolling-hash boundary detection (same `16/64/256 KiB` params as `rio-store/src/chunker.rs`, `r[store.cas.fastcdc]`) emitting `(offset, len, blake3)` per chunk; a whole-file blake3 accumulator yielding `file_digest`; the Boyer-Moore reference scanner over raw bytes (`r[builder.upload.references-scanned]`); and the SHA-256 accumulator over **NAR-framed** bytes (the walk emits NAR framing — `nar::Encoder` headers/padding — into the SHA-256 sink only; no NAR byte stream is materialized). At end-of-walk the builder holds `{chunk_manifest, file_digests, root_dir_digest, refs, nar_hash, nar_size}` having read each output byte exactly once. This closes `TODO(P0433)` (refs forced into a separate pre-pass) and `TODO(P0434)` (manifest-first upload).

r[builder.upload.chunked-manifest]

The walk yields, per output: `{store_path, nar_hash, nar_size, refs, root_node, chunk_manifest}` and a set of `Directory` bodies. `chunk_manifest` is the ordered list `[(chunk_digest, len)]` in canonical-NAR-walk order over `root_node`; for each regular file in the tree the contiguous run of `len` values sums to that `FileEntry.size`. The `Directory` bodies are content-addressed (`r[store.index.dir-digest]`) and deduplicated across outputs. Together these are sufficient for rio-store to reconstruct each output's NAR byte stream from CAS chunks without builder participation.

**Input-reuse shortcut (builder-local, no protocol change).** Before chunking an output regular file, the walk may consult a `(size, file_digest)` table of the build's declared inputs — already in heap from the §2.2 Directory DAG prefetch. On a size match, a content `cmp` against the castore-FUSE lower file confirms identity and the input's `file_digest` and chunk list are reused without hashing. This is the [ostree `devino_to_csum_cache`](https://ostreedev.github.io/ostree/reference/ostree-OstreeRepo.html) pattern adapted to a content-addressed lower; it pays off for `cp`-heavy and fixed-output builds.

### 6.2 Wire protocol

r[store.chunk.has-chunks-durable]

`HasChunks([chunk_digest]) → bitmap` is the chunk-granular sibling of the file-level `HasBlobs` (P0573, `r[store.castore.blob-read]`). A bit is set **only if the chunk is S3-durable** — i.e., referenced by at least one *complete* manifest, not merely refcount ≥1. The distinction is I-201 (stranded-chunk race): a SIGKILL between refcount-bump and S3 `PutObject`, combined with a concurrent uploader's presence-skip, permanently strands the digest. Under durable-presence semantics two builders racing on the same novel chunk both see `false`, both upload, and the second S3 `PutObject` is an idempotent overwrite of identical content. The builder MAY first probe `HasBlobs([file_digest])` to short-circuit whole files before chunking them.

r[store.put.chunked-wire]

`PutPathChunked` is a client-stream carrying all of a derivation's outputs in one RPC, satisfying `r[store.atomic.multi-output]` and `r[builder.upload.batch+2]`:

```text
Begin{ hmac_token, deriver,
       outputs:     repeated { store_path, nar_hash, nar_size, refs, root_node, chunk_manifest },
       directories: repeated Directory,       // bodies for every dir_digest reachable from any root_node, deduped
       novel:       repeated chunk_digest }   // exactly the digests the builder will send as Chunk frames
… Chunk{digest, bytes}  // zero or more, each digest ∈ Begin.novel
```

`root_node` is the snix `Node` oneof (`DirectoryNode{dir_digest}` / `FileNode` / `SymlinkNode`) — the same shape `WorkAssignment.input_roots` carries. `chunk_manifest` is the §6.1 ordered `[(digest, len)]` list per output. `novel` is the `HasChunks`-false subset over the union of all outputs' chunk digests; the server keys its receive map on this set, not on its own re-query, so a chunk that became durable (or was GC'd) between the builder's `HasChunks` call and `Begin` cannot wedge the verify driver.

r[store.put.chunked-bounds]

`Begin` is validated **before** any placeholder claim, S3 write, or verify-driver spawn; violations return `INVALID_ARGUMENT`. The HMAC assignment token is verified (`r[store.hmac.san-bypass]`) and `hash_part(Begin.deriver) == claims.drv_hash` is enforced. Per output: `store_path ∈ claims.expected_outputs` (non-CA), `nar_size ≤ MAX_NAR_SIZE`, `len(refs) ≤ MAX_REFERENCES`, `len(chunk_manifest) ≤ MAX_CHUNKS`, and per regular file in the output's tree the contiguous `chunk_manifest` run sums to `FileEntry.size`. `len(Begin.directories) ≤ MAX_DIR_NODES`. Every `Directory` body's digest is recomputed server-side as `blake3(canonical-encode(body))`; every `dir_digest` reachable from any `root_node` MUST be present in `Begin.directories` under the recomputed digest — `root_node` is therefore attested, not claimed. `novel ⊆ ∪ outputs[i].chunk_manifest.digest` and `Σ_outputs Σ chunk_manifest.len ≤ len(outputs) × MAX_NAR_SIZE`. The deduped-fetch byte total (`Σ len` over manifest entries with `digest ∉ novel`) is acquired against the existing `nar_bytes_budget` semaphore before the verify driver spawns.

r[store.chunk.self-verify]

After validation the handler arms `PlaceholderGuard` (`r[store.put.drop-cleanup+2]`, `r[store.gc.orphan-heartbeat]` — heartbeats `manifests.updated_at`, reaps on drop) and inserts an `'uploading'` placeholder per **non-CA** output (`r[store.put.wal-manifest]`); CA outputs are handled per §6.3. The receive loop then, per `Chunk`: asserts `blake3(bytes) == digest` AND `digest ∈ Begin.novel` else `INVALID_ARGUMENT`, acquires a permit from `Semaphore(VERIFY_WINDOW)` (default 64, bounding receive↔verify backlog to ~16 MiB), issues the S3 `PutObject` via `cas::put` (`r[store.cas.upload-bounded]`), then signals `tx_map[digest]`. **rio-store, not the builder, reaches S3.** The whole-NAR `Vec<u8>` is gone; per-stream working set is `≤ VERIFY_WINDOW × 256 KiB` plus the verify driver's bounded prefetch (§6.3).

Commit is gated on the §6.3 verify verdict and runs in **one transaction** across all outputs: insert `manifest_data`, `nar_index.root_node`, `directories`, and `file_blobs` rows; UPSERT chunk refcounts; `UPDATE chunks SET durable=TRUE`; flip every placeholder `uploading → complete`. This is also where `PutPathChunked` outputs become servable by `GetDirectory`/`ReadBlob`/`StatBlob` — the castore tables are written here, not by a later indexer pass. Builder death mid-stream leaves placeholders (heartbeat lapses, reaped per `r[store.gc.orphan-heartbeat]`) and refcount-0 chunks (`r[store.chunk.grace-ttl]` sweep). `PutPathChunked` is idempotent (`r[store.put.idempotent]`) — re-drive after transport failure repeats `HasChunks` and re-sends only still-missing chunks.

### 6.3 NarHash trust — sync verify, pipelined

r[store.put.narhash-sync]

The builder is untrusted (`r[builder.upload.references-scanned]` context: outputs are adversary-controlled bytes); rio-store signs `narinfo`. Each `outputs[i].nar_hash` in `Begin` is therefore **claimed**, not attested. rio-store independently recomputes it before commit — the same `r[store.integrity.verify-on-put]` contract `PutPath` already honors — without reintroducing the whole-NAR buffer §6.2 eliminates.

After §6.2 validation passes, the handler spawns one verify driver per output that walks that output's `chunk_manifest` in NAR order, concurrent with the receive loop. The driver emits `nar::Encoder` framing from the §6.2-validated `Directory` tree (the §6.1 invariant that `chunk_manifest` order equals canonical-NAR-walk order over `root_node`, with per-file `len` runs summing to `FileEntry.size`, is what makes the interleave well-defined). For each manifest entry it obtains the chunk body, feeds framing + body into the per-output SHA-256, releases one `VERIFY_WINDOW` permit, and drops the body:

| `digest` is… | source |
|---|---|
| in `Begin.novel`, first occurrence | `rx_map[digest].await` — oneshot signalled by the receive loop after `cas::put` |
| in `Begin.novel`, subsequent occurrence | `cas::get(digest)` — the first occurrence's `cas::put` has completed, so the chunk is in S3 (and typically the replica's moka tier) |
| not in `Begin.novel` | `cas::get(digest)` from the tiered backend, prefetch window ≤32 |

Upload and verify overlap: deduped-chunk fetches run while novel chunks are still streaming, so wall-clock added is approximately `max(0, deduped_fetch_time − upload_time)` rather than `Σ nar_size / sha256_throughput`.

On client stream end the receive loop drops `tx_map`; any verify driver parked on an unsent novel digest observes `Canceled`. Per output, the verdict is **match** (`sha256 == outputs[i].nar_hash`), **mismatch**, **incomplete** (a `Canceled` rx — declared-novel chunk never sent), or **unavailable** (a `cas::get` error — transient S3 fault or a deduped chunk GC'd between the builder's `HasChunks` and now).

| Outcome (any output) | Status | Side effects |
|---|---|---|
| all match | commit | §6.2 single-transaction commit |
| any mismatch | `FAILED_PRECONDITION` | `rio_store_narhash_mismatch_total++`; structured-log `{store_path, drv_path, builder_pod, claimed, computed}`; placeholders deleted via `PlaceholderGuard` reap |
| any incomplete | `FAILED_PRECONDITION` | `rio_store_putpath_incomplete_total++`; placeholders reaped |
| any unavailable, no mismatch | `UNAVAILABLE` | `rio_store_putpath_verify_unavailable_total++`; placeholders reaped; builder retries per `r[builder.upload.retry]` |

In every non-commit case, novel chunks already in S3-standard are refcount-0 orphans for `r[store.chunk.grace-ttl]` sweep — they are content-addressed (cannot poison), not yet `durable` (cannot cause `HasChunks` skips), and the same orphan path already covers builder-crash-mid-stream. A *mismatch* means a builder NAR-framing bug or a compromised builder, and is alert-worthy and approximately never; *incomplete* and *unavailable* are infra-retry conditions, not alerts.

r[store.put.chunked-ca]

**CA outputs** (`claims.is_ca = true`): the §6.2 placeholder is **not** claimed at `Begin` — this is how `PutPathChunked` honors `r[sec.authz.ca-path-derived+2]` ("CA-path recompute MUST run BEFORE the `'uploading'` placeholder is claimed") without buffering. Receive and verify proceed identically; on all-match, the store recomputes each output's CA path from the **server-computed** `nar_hash` via `make_fixed_output(name, computed_nar_hash, recursive=true, refs)`, asserts it equals `outputs[i].store_path` (else `PERMISSION_DENIED` — the builder lied about either `nar_hash` or `store_path`), and then performs the §6.2 single-transaction commit, claiming and completing each manifest row in the same transaction. Idempotent if a row is already `'complete'`. Chunk protection during the upload window is `r[store.chunk.grace-ttl]` alone, identical to the non-CA mismatch path; CA uploads are not long enough for that to matter.

`'complete'` therefore uniformly implies `nar_hash` is store-verified, across both `PutPath` and `PutPathChunked`. There is no `'quarantined'` state, no `nar_hash_verified` column, no verify-worker queue, no narinfo serving gate.

**Considered and rejected:** async verify with a `'complete' && !verified` window (same total S3 reads and SHA-256 CPU, just time-shifted; the only beneficiary of early commit is scheduler dispatch a few seconds sooner, while every consumer of manifest status would have to know which RPC produced the row — complexity not paid for); trusting builder `nar_hash` outright, with or without an ephemeral per-pod signature (rio-store's narinfo signature would attest a value computed by a process whose address space an adversary-controlled build could plausibly influence); local-disk spool before S3 write (preserves the incidental "S3 write ⟹ nar_hash verified" ordering of buffered `PutPath`, but adds a per-replica capacity limit and a disk round-trip per novel chunk for hygiene that orphan-GC already provides); routing `is_ca` uploads to legacy `PutPath` (re-admits the whole-NAR buffer for exactly the floating-CA case, and requires a hole in `r[store.put.builder-chunked-only]`); dropping `nar_hash` from the manifest entirely (breaks substitution by stock Nix clients — a non-goal to break).

### 6.4 Prior art

REAPI [`FindMissingBlobs`](https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto) → `HasChunks`; BuildBarn [ADR-0003 CAS decomposition](https://github.com/buildbarn/bb-adrs/blob/main/0003-cas-decomposition.md) → upload chunk granularity matches read-path chunk granularity; ostree `devino_to_csum_cache` → §6.1 input-reuse shortcut; [`mkcomposefs --digest-store`](https://man.archlinux.org/man/mkcomposefs.1.en) → walk-and-reflink-into-object-store (inapplicable directly: upper is local SSD, cache tier is an object store, no reflink — but the walk shape is the same); Nix [#4075](https://github.com/NixOS/nix/issues/4075)/[#7527](https://github.com/NixOS/nix/issues/7527) → existence-check before serialize, and the whole-NAR-granularity pain point this section eliminates.

---

# ADR-022: Lazy /nix/store filesystem — composefs-style

Status: **Accepted.** Two earlier candidates (EROFS+fscache, custom `riofs` kmod) were evaluated and set aside; see §3.

**Scope:** the builder-side `/nix/store` filesystem and the rio-store metadata that supports it. The canonical design reference is the [Design Overview](./022-design-overview.md); sequencing is in the [Implementation Plan](./022-implementation-plan.md).

---

## 0. Deployment context

Builder nodes run a NixOS-based AMI. Kernel configuration is first-party: `boot.kernelPatches[].extraStructuredConfig` in the AMI flake sets `EROFS_FS=y OVERLAY_FS=y FUSE_FS=y` and the node module asserts kernel ≥6.16 at boot. No custom Kconfig symbols are required — all three are stock-on in distro defconfigs; the patch block is for `=y` over `=m` only.

Device exposure: `/dev/fuse` reaches the builder via `rio-mountd` fd-handoff (§2.3); `/dev/kvm` reaches kvm-pool builds via `hostPath` CharDevice + `extra-sandbox-paths`. No device-plugin DaemonSet.

---

## 1. Problem statement

The pre-ADR-022 builder presents `/nix/store` via a FUSE filesystem (`rio-builder/src/fuse/`) that JIT-fetches whole store paths on first access. This is correct but has two structural costs:

1. **Metadata is userspace.** Every `stat`/`readdir`/`readlink` is a FUSE upcall. A `find /nix/store` over a chromium-scale closure (23k files) is ~23k context switches.
2. **Warm-but-partial files still upcall.** FUSE passthrough binds one backing fd at `open()`. A 200 MB `libLLVM.so` with 4 MB of hot `.rodata` either upcalls on every read until the whole file is materialized, or blocks `open()` for ~1.3 s fetching it whole. The kernel page cache cannot serve a warm range of a partially-fetched file without crossing to userspace.

The goal: `stat`/`readdir` are kernel-native; warm reads of any range hit page cache with zero crossings; cold fetch is file-granular, not path-granular.

---

## 2. Design — composefs-style stack

The mechanism is the one [composefs](https://github.com/containers/composefs) ships for ostree/podman: an EROFS image carrying **metadata only** (inodes, dirents, sizes, modes, xattrs — zero data blocks), stacked under overlayfs with a second **data-only lower** holding files named by content digest. Each regular-file inode in the metadata layer carries `user.overlay.redirect=/ab/<blake3>` + `user.overlay.metacopy`; overlayfs follows the redirect **lazily on first `open()`**, not at `mount(2)`.

The data-only lower is a thin FUSE mount serving `lookup(digest) → open → read` — back to FUSE, but **only for cold `open()`**. Warm `read()` is page-cache via the overlay; `stat`/`readdir` never leave the kernel.

### 2.1 Mount stack and lookup path

r[builder.fs.composefs-stack]

The build's `/nix/store` is a **single read-write overlayfs** with an SSD upper (build outputs land here) over two read-only lowers: the EROFS metadata image, and the digest-addressed FUSE as a data-only lower. Mount string: `overlay -o userxattr,upperdir=<ssd>/nix/store,workdir=<ssd>/work,lowerdir=<erofs_mnt>::<objects_dir>`; the merged dir is bind-mounted at `/nix/store` inside the build's mount namespace. The `::` separator marks the FUSE mount as a [data-only lower](https://docs.kernel.org/filesystems/overlayfs.html#data-only-lower-layers) — overlayfs path-walks only EROFS, following absolute redirects into FUSE for data. Upper/work dirs are local SSD (`r[builder.overlay.upper-not-overlayfs]`); outputs and the synthesized `db.sqlite` live under the upper root.

r[builder.fs.userxattr-mount]

`userxattr` makes the overlay mountable from an **unprivileged userns** and reads `user.overlay.{redirect,metacopy}` from EROFS. It forces `config->{metacopy=false, redirect_mode=NOFOLLOW}` ([params.c:988-1008](https://github.com/torvalds/linux/blob/master/fs/overlayfs/params.c)); the data-only-lower redirect path is gated independently on `ofs->numdatalayer > 0` ([`namei.c:1241`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/namei.c), [`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9), v6.16+) and is upper-agnostic. With `metacopy=false` forced, **copy-up of an input file is always a full data copy** from `ovl_path_lowerdata()` — i.e., the FUSE layer, which JIT-fetches ([copy_up.c:274,645,1025,1219](https://github.com/torvalds/linux/blob/master/fs/overlayfs/copy_up.c)); a build that modifies an input sees real bytes, not the 0-byte EROFS stub. Cross-directory `rename(2)` in the upper still returns `EXDEV` (no redirect xattr created under `NOFOLLOW`) — unchanged from the pre-ADR-022 unpriv-userns behavior.

| Syscall | Resolved by | FUSE upcalls |
|---|---|---|
| `stat`/`getattr`/`readdir`/`readlink` (input) | EROFS inode (kernel) | 0 |
| `open` input (cold) | overlay follows `user.overlay.redirect` → FUSE `lookup×2 + open` | **2 lookup + 1 open**, depth-independent |
| `read`/`mmap` input (cache hit / post-fill) | `FOPEN_PASSTHROUGH` → backing fd | **0** |
| `read`/`mmap` input (large file mid-fill) | FUSE `read` (~128 KiB/req) | O(touched / 128 KiB), once per file per node |
| **write / create output** | **overlay upper (SSD)** | 0 |
| modify input → copy-up | overlay full-data-copies from FUSE lowerdata into upper | as cold open+read |

§1's constraint — a 200 MB partially-hot `.so` either upcalls every read or blocks open — **is addressed by streaming open (§2.7) during fill, then passthrough (§2.4) thereafter.** The first open returns after the first chunk; uncached `read()` ranges upcall once during the background fill. Once the file is in the node-SSD cache, every subsequent open replies `FOPEN_PASSTHROUGH` and reads go kernel → ext4 with **zero FUSE involvement** — including after page-cache eviction.

### 2.2 Encoder — `libcomposefs` via FFI

r[builder.fs.stub-isize]

The metadata image must encode each regular file's **real `i_size`** with zero data blocks. overlayfs metacopy surfaces the metadata layer's `i_size` to `stat()`; a stub encoded with `i_size=0` reports 0 to userspace even though `read()` returns full data — `mmap(len=st_size)` then maps nothing.

The encoder is the C [`libcomposefs`](https://github.com/containers/composefs) (`GPL-2.0-or-later OR Apache-2.0` — Apache-2.0 chosen for linking; this is the library podman/ostree ship), called in-process via Rust FFI. rio's adapter (~80 LoC) walks the merged closure `NarIndex` calling `lcfs_node_new()` / `lcfs_node_set_{mode,size,mtime,payload}()` / `lcfs_node_add_child()`, then `lcfs_write_to(memfd)` emits the EROFS image. Spike-measured (`~/tmp/spike-libcomposefs-ffi/`): **~46 ms / 23k files** (~2 µs/inode), correct `i_size` including >4 GB, non-UTF8 names preserved, symlinks preserved.

The library is built from a nix-patched `pkgs.composefs`. The patch (`nix/patches/libcomposefs-user-xattr.patch`, ~25 lines) adds two flags, both upstreamable: **`LCFS_BUILD_USER_XATTR_OVERLAY`** makes the writer emit `user.overlay.{redirect,metacopy}` instead of the hardcoded `trusted.` prefix ([`lcfs-internal.h:37-50`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-internal.h)); **`LCFS_FLAGS_NO_ROOT_WHITEOUTS`** skips `add_overlay_whiteouts(root)` and `set_overlay_opaque(root)` ([`lcfs-writer-erofs.c:1374,1378`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-writer-erofs.c)) — those create 256 `chardev(0:0)` nodes `00`..`ff` plus a `trusted.overlay.opaque` at the image root for OCI layering, which would otherwise pollute `/nix/store`. With both flags set the output carries only `user.*` xattrs and no root chardevs (spike-verified via `getfattr -m '^trusted'` empty + no `c---------` entries).

**Fallback (zero-patch):** [`mkfs.erofs --tar=headerball`](https://github.com/erofs/erofs-utils) from upstream `erofs-utils` (≥1.8) — purpose-built for meta-only overlay data-only-lower images; takes a header-only tar with PAX `SCHILY.xattr.*`, ~160 LoC Rust emitter + subprocess, ~280 ms / 23k files. Spike-verified all properties pass. Chosen against only because FFI gives in-process error reporting and ~6× faster encode with comparably small owned code.

r[builder.fs.metacopy-xattr-shape]

`user.overlay.metacopy` must be either zero-length (legacy) or ≥4 bytes encoding `struct ovl_metacopy { u8 version; u8 len; u8 flags; u8 digest_algo; u8 digest[]; }` ([overlayfs.h:165-171](https://github.com/torvalds/linux/blob/master/fs/overlayfs/overlayfs.h)). 1–3 bytes → kernel `"metacopy file '%pd' has too small xattr"` ([util.c:1280](https://github.com/torvalds/linux/blob/master/fs/overlayfs/util.c)). With `LCFS_BUILD_USER_XATTR_OVERLAY` set and no fs-verity digest supplied, `libcomposefs` emits the zero-length form.

### 2.3 Mount sequence and privilege boundary

Privilege is split: a node-level **`rio-mountd`** (CAP_SYS_ADMIN, DaemonSet) handles the three operations the unprivileged builder cannot: open `/dev/fuse`, create the EROFS superblock, and broker `FUSE_DEV_IOC_BACKING_OPEN` (which requires init-ns `CAP_SYS_ADMIN` — [`backing.c:91-93`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c), kernel `TODO: relax once backing files are visible to lsof`). Per build connection on the UDS:

1. `fuse_fd = open("/dev/fuse")`; `dup` it (mountd keeps one, sends the other) → `mount("fuse", objects_dir, …, "fd=N,…")` where `objects_dir = /var/rio/objects/{build_id}` → `SCM_RIGHTS` the fd to the builder
2. `fsopen("erofs")` → `fsconfig(FSCONFIG_SET_FLAG, "ro")` → `fsconfig(FSCONFIG_SET_STRING, "source", meta_image)` → `fsconfig(FSCONFIG_CMD_CREATE)` → `fsmount(…)` → `SCM_RIGHTS` the detached-mount fd
3. `mkdir /var/rio/staging/{build_id}` with the requesting builder's uid/gid (from `SO_PEERCRED`). The UDS socket is mode 0660, group `rio-builder`; mountd verifies `SO_PEERCRED.gid == rio-builder` and rejects others.
4. Serve `seq`-tagged requests for the build's lifetime (tokio per-conn task; replies echo `seq` so out-of-order `Promote` correlates): **`BackingOpen{fd}`** — `ioctl(kept_fuse_fd, FUSE_DEV_IOC_BACKING_OPEN, {fd}) → backing_id` (mountd does not inspect the fd — the ioctl rejects depth>0 backing, and `backing_id` is conn-scoped). **`BackingClose{id}`**. **`PromoteChunks{[chunk_digest]}`** (batched ≤64) — verify-copy each `staging/chunks/<hex>` → `/var/rio/chunks/ab/<hex>`. **`Promote{digest}`** (on `spawn_blocking` + `Semaphore(num_cpus)`) — verify-copy the assembled file from staging into cache (§2.4).

The builder, inside its unprivileged userns (after receiving the fds from step 1-2):

a. `fuser::Session::from_fd(fuse_fd)` → spawn `digest_fuse::serve` (§2.4) — **must be answering before (c)**
b. `move_mount(erofs_fd, "", AT_FDCWD, meta_mnt, MOVE_MOUNT_F_EMPTY_PATH)`
c. `mount("overlay", merged, "overlay", 0, "userxattr,upperdir=<ssd>/nix/store,workdir=<ssd>/work,lowerdir=<meta_mnt>::<objects_dir>")` — then bind `merged` at `/nix/store` inside the build's mount namespace

Teardown — builder closes the UDS (or its pod exits):

- `rio-mountd` does `umount2(objects_dir, MNT_DETACH)` + `rmdir(objects_dir)` + `rm -rf staging/{build_id}` + `close(kept_fuse_fd)`; the builder's mount-ns death takes the overlay + erofs mounts with it.

r[builder.mountd.orphan-scan]

Crash-safety: `rio-mountd` start-up scans `/var/rio/objects/*` and `/var/rio/staging/*` for orphans from a prior crash and removes them.

r[builder.fs.fd-handoff-ordering]

**Ordering is load-bearing:** the `/dev/fuse` fd MUST be received and the digest-FUSE server MUST be answering before builder step (c). overlayfs probes each lower's root at `mount(2)`; with no one serving `/dev/fuse`, that probe deadlocks. The `fsconfig` `"ro"` flag MUST precede `CMD_CREATE` — `MOUNT_ATTR_RDONLY` on `fsmount` is per-mount, not per-superblock, and erofs otherwise opens the bdev RW → `EACCES` on a read-only loop.

**No build-start merge step**: one EROFS image per closure, generated from the union of the closure's `NarIndex` rows.

### 2.4 Digest-FUSE handler

r[builder.fs.digest-fuse-open]

The data-only lower is a `fuser` filesystem rooted at the per-build `objects_dir` exposing exactly two directory levels: 256 prefix dirs (`00`..`ff`) and leaf files named by the remaining 62 hex chars of `blake3(file_content)`. `lookup(prefix, name)` consults a `file_digest → (size, executable)` map populated from the closure's `NarIndex`; unknown digests return `ENOENT`.

r[builder.fs.passthrough-on-hit]

`open` is a **broker, not a server**: its job is to ensure the file exists in the node-SSD backing cache and hand the kernel a passthrough fd to it. The handler negotiates `FUSE_PASSTHROUGH` at `init` (with `max_stack_depth = 1`; the backing cache is a non-stacking fs). On `open(digest)`:

1. **Cache hit** (`/var/rio/cache/ab/<digest>` exists, mountd-owned): open it `O_RDONLY`, send the fd to `rio-mountd` → receive `backing_id` (mountd does the `FUSE_DEV_IOC_BACKING_OPEN` ioctl, §2.3); reply `FOPEN_PASSTHROUGH | backing_id`. All subsequent `read`/`mmap` go kernel → backing file via `fuse_passthrough_read_iter`; the handler sees no further upcalls for this fd. `release` sends `BackingClose{id}` to mountd.
2. **Cache miss, `size ≤ STREAM_THRESHOLD`**: fetch whole into `/var/rio/staging/{build_id}/<digest>.partial` (chunk fan-out via `store_fetch.rs`), per-chunk + whole-file verify, send `Promote{digest}` → mountd verify-copies into cache → as (1).
3. **Cache miss, `size > STREAM_THRESHOLD`**: streaming-open (§2.7) — reply `FOPEN_KEEP_CACHE`, serve `read` from staging `.partial` during background fill. The fill task sources each chunk from `/var/rio/chunks/` first, falling back to `GetChunks` for misses (firing `PromoteChunk` for each). On completion: whole-file verify, `Promote{digest}`. Next `open` hits (1). Concurrent builds race on `PromoteChunk` and share progress at chunk granularity — no leader/follower coordination needed.
4. Within-build `.partial` orphan: `flock(LOCK_NB)` not held → unlink + retry.

The FUSE `read` op exists only for case (3)'s streaming window; in steady state every open is passthrough.

r[builder.fs.shared-backing-cache]

The FUSE **mount point** is per-build (`/var/rio/objects/{build_id}/`, §2.3) for cross-pod isolation. The **backing cache** (`/var/rio/cache/ab/<digest>`) is shared node-SSD, **owned by `rio-mountd` and read-only to builder pods** (mode 0755/0444). Builders cannot write the cache directly — a sandbox-escaped build writing poisoned bytes there would otherwise be passthrough-read by the next build unverified (the same lateral-movement surface §2.7 rejects for a cluster-wide cache).

r[builder.mountd.promote-verified]

Instead, the builder stages fetched files in a **per-build staging dir** (`/var/rio/staging/{build_id}/`, builder-writable, created by mountd at `Mount` time) and mountd verify-copies into the cache on `Promote` — see §2.4. mountd opens the staging file `O_RDONLY|O_NOFOLLOW`, rejects non-regular or oversized, creates `cache/ab/<digest>.promoting` (mountd-owned, `O_EXCL|O_WRONLY`, 0444), **stream-copies while hashing**, verifies `blake3 == digest`, renames `.promoting → final`. The copy is the integrity boundary: the cache file is a distinct inode mountd created and hashed.

r[builder.fs.node-chunk-cache]

**Cross-build fetch dedup is chunk-granular** for files > `STREAM_THRESHOLD`. mountd also owns `/var/rio/chunks/ab/<chunk_blake3>` (read-only to builders). The streaming fill task `open()`s the chunk cache entry first (ENOENT → miss); for each miss it writes the verified chunk into `.partial` at offset *and* into `staging/chunks/<hex>`, batching digests into `PromoteChunks{[..]}` (mountd verify-copies into the chunk cache). Assembly proceeds from the build's own staging — `PromoteChunks` is purely for *other* builds' benefit and never blocks this build's fill. Concurrent builds race on the chunk-cache rename; the loser reads the winner's chunk — **no builder fetches a chunk another build on this node already verified.** Chunks are independently content-addressed, so mountd's `blake3(bytes) == chunk_digest` check is context-free (unlike file ranges, which would need the chunk-map). `BACKING_OPEN`'s `d_is_reg` gate ([`backing.c:105-108`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c)) and this context-free-verify constraint together pin the design to "regular files whose names are their hashes" — block-device alternatives (ublk, dm-verity) fail the ioctl. Files ≤ threshold skip the chunk cache (whole-file fetch is fast enough; dup-fetch cost negligible).

This is **the only FUSE in the stack**, and it is hit only on cold `open()`. Spike-measured cold lookups are exactly 2 regardless of the merged path's depth — overlayfs walks the EROFS dirent chain in-kernel, then the redirect is one absolute jump.

### 2.5 Spike evidence

Core-stack nixosTests on `adr-022` ([`composefs-spike.nix`](../../nix/tests/scenarios/composefs-spike.nix), [`composefs-spike-scale.nix`](../../nix/tests/scenarios/composefs-spike-scale.nix), ported in `15a9db79`); chromium-146 closure topology (357 store paths, 23 218 regular files, 8 221 dirs, 3 374 symlinks) with synthetic file content:

| Metric | Measured |
|---|---|
| `mount -t overlay` wall-clock | **<10 ms** (below `time(1)` granularity) |
| FUSE upcalls during mount | lookup=0 getattr=0 open=0 read=0 |
| EROFS metadata image | **5.3 MiB** (≈239 B/file), encoded in **70 ms** |
| `find -type f` over 23 218 files | 60 ms, **0 FUSE upcalls** |
| `find -printf %s` sum over 23 218 files | 1 795 354 094 B == manifest, 120 ms, **0 FUSE upcalls** |
| Cold `lookup` upcalls (any depth) | **2** (prefix + digest) |
| Cold `read` upcalls, 31 MB file | 244 (≈128 KiB/req) |
| **Warm `read` upcalls** | **0** (all samples) |
| FUSE handler peak RSS | 8.9 MB |

Follow-on spikes (consolidated on `adr-022`):

| Commit | Test | Finding |
|---|---|---|
| `15a9db79` | [`composefs-spike-stream.nix`](../../nix/tests/scenarios/composefs-spike-stream.nix) | Streaming-open (§2.7): `FOPEN_KEEP_CACHE` set at `open()` does not suppress cold-page upcalls (2049 reads on first `dd` of 256 MiB), only prevents invalidation — second `dd` 0 upcalls. **No mode-flip needed.** `mmap(MAP_PRIVATE)` page-faults route through FUSE `read`. `open()` 256 MiB with 10 ms/chunk backend → 10.3 ms (vs 2560 ms whole-file). |
| `42aa81b2` | [`spike-access-data/RESULTS.md`](../../nix/tests/lib/spike-access-data/RESULTS.md) | Access patterns: real consumers touch **0.3-33%** of giant `.so`/`.a` (link-against-libLLVM 2.79% bimodal head+tail; `opt --version` 32.77% scattered/266 ranges; `libicudata` 0.28%). `ld.so` uses no `MAP_POPULATE`/`fadvise`. |
| `af8db499` | [`composefs-spike-priv.nix`](../../nix/tests/scenarios/composefs-spike-priv.nix) | Privilege boundary (§2.3): all 6 questions PASS on kernel 6.18.20 — fd-handoff, stack-survives-mounter-exit, unpriv-userns-inherits, **`userxattr` unpriv overlay**, teardown-under-load (no D-state), `fsopen`/`fsmount` detached-fd handoff. |
| `9492019c` | [`kvm-hostpath-spike.nix`](../../nix/tests/scenarios/kvm-hostpath-spike.nix) | `/dev/kvm` via `extra-sandbox-paths`: `ioctl(KVM_GET_API_VERSION)=12` from inside Nix sandbox; smarter-device-manager not required. |

### 2.6 Integrity — fs-verity does not apply; per-file blake3 in handler

r[builder.fs.file-digest-integrity]

composefs's native integrity story embeds an fs-verity digest in the metacopy xattr; overlayfs's `ovl_validate_verity()` compares it against `fsverity_get_digest()` on the lower inode at `open()`. **That in-kernel API reads `inode->i_verity_info`, which FUSE never populates** — FUSE's fs-verity support ([`9fe2a036`](https://git.kernel.org/linus/9fe2a036a23ceeac402c4fde8ec37c02ab25f133), 6.10) is ioctl-forwarding only: no `fsverity_operations`, no `S_VERITY`, no kernel-side Merkle tree. overlayfs sees "no fs-verity digest" on a FUSE lower regardless. (And ioctl-forwarding would not help: the daemon answering `FS_IOC_MEASURE_VERITY` is the same daemon serving the bytes — no stronger than the daemon verifying blake3 itself.) Per-file integrity therefore lives in the digest-FUSE handler: **verify each chunk's blake3 against its content-address on arrival** (chunks are blake3-addressed in the CAS layer; `rio-store/src/chunker.rs`); never serve a byte from an unverified chunk. For files ≤ the streaming threshold (§2.7), the whole-file blake3 against `file_digest` additionally runs before `open()` returns. For files > threshold, per-chunk verification covers the streaming window and the whole-file `file_digest` check runs at fill-complete, gating the `.partial → /var/rio/cache/ab/<digest>` rename. The digest is the filename — the check is structural. `file_digest` also serves as the per-file content-address for the Directory-merkle layer enabling closure delta-sync; see `components/store.md` §NAR index.

### 2.7 Failure modes and streaming open

| Failure | Kernel behavior | rio handling |
|---|---|---|
| **FUSE handler crash** | overlayfs `open()` on a redirect target → `ENOTCONN`. **Passthrough-opened files keep working** — `fuse_passthrough_read_iter` ([passthrough.c:28-51](https://github.com/torvalds/linux/blob/master/fs/fuse/passthrough.c)) reads `ff->passthrough` directly with no `fc->connected` check; `fuse_abort_conn` ([dev.c:2451-2522](https://github.com/torvalds/linux/blob/master/fs/fuse/dev.c)) never touches `ff->passthrough` or `fc->backing_files_map`. Streaming-mode opens lose their `read` server. | Abort is terminal — the build is failed-infra and re-queued; the next pod issues a fresh `Mount{}` and gets a new `fuse_conn`. The IDR slot for the dead connection's backing-ids leaks until unmount (bounded; reaped at `fuse_conn_put`). **No D-state**, no `restore` dance. |
| **FUSE handler hung mid-fetch** | `open()` blocks in `S` (interruptible — `request_wait_answer` [dev.c:552/568](https://github.com/torvalds/linux/blob/master/fs/fuse/dev.c) `wait_event_interruptible`/`_killable`). | Per-spawn `tokio::timeout` returns `EIO` to the open; build fails loudly. |
| **Redirect target ENOENT** | overlayfs `open()` → `ENOENT`. | Handler returns ENOENT only for digests outside the closure's declared-input allowlist — correct (JIT fetch imperative). |
| **Build completes / pod exits** | Builder mount-ns death drops overlay+erofs; `objects_dir` FUSE mount persists in init-ns. | `rio-mountd` detaches it on UDS close (§2.3 teardown); start-up scan reaps orphans from a prior crash. |

r[builder.fs.streaming-open-threshold]

**Streaming open is the during-fill mode for large files.** The 1000 largest files in nixpkgs are *all* >64 MiB (median 179 MiB, 7 files >1 GiB; `top1000.csv`), and access-pattern measurement (`42aa81b2`) shows consumers touch 0.3-33% of them — whole-file fetch over-fetches 64-99.7%. For files > `STREAM_THRESHOLD` (default 8 MiB) on cache miss, `open()` cannot reply passthrough (no complete backing fd yet) so it replies `FOPEN_KEEP_CACHE`, spawns a background fill task, and returns after the first chunk (~10 ms). `read(off,len)` upcalls once per uncached page during fill (priority-bumping the requested range); `mmap(MAP_PRIVATE)` page-faults route through the same `read` path, covering linkers. ~80 LoC; spike-proven (`15a9db79`) — `KEEP_CACHE` does not suppress cold-page upcalls, only invalidation, so no mode-flip is needed within the streaming window. Once the fill completes and renames into `/var/rio/cache/`, the next `open()` of that digest takes the passthrough path (§2.4 case 1) — the streaming mode is one-shot per file per node.

**Considered and rejected for the partial-file case:** encoder-side file-splitting into N redirect targets (overlayfs `redirect` is single-path per inode; not expressible); allowlist-bounded prefetch of giants at mount (violates the JIT-fetch imperative — fetches inputs the build may never touch); FSx-Lustre-backed cluster-wide `objects` cache (violates the builder air-gap — shared writable FS between untrusted builders is a cache-poisoning + lateral-movement surface).

### 2.8 Kconfig (NixOS)

```nix
boot.kernelPatches = [{
  name = "overlay-composefs";
  patch = null;
  extraStructuredConfig = with lib.kernel; {
    EROFS_FS         = yes;
    OVERLAY_FS       = yes;       # nixpkgs default =m
    FUSE_FS          = yes;
    FUSE_PASSTHROUGH = yes;
  };
}];
```

Requires kernel **≥6.16** ([`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9): data-only-lower redirect honored under `userxattr`); subsumes ≥6.9 (`FUSE_PASSTHROUGH`, [`7dc4e97a4f9a`](https://git.kernel.org/linus/7dc4e97a4f9a)) and ≥5.2 (`fsopen`/`fsmount`/`move_mount`). The NixOS-node module asserts version + config at boot.

r[builder.fs.passthrough-stack-depth]

**Stacking depth:** with `max_stack_depth = 1` at FUSE `init`, the FUSE superblock has `s_stack_depth = 1` ([`inode.c:1439-1444`](https://github.com/torvalds/linux/blob/master/fs/fuse/inode.c) — the in-kernel comment explicitly names overlay-as-upper as the supported case). Overlay over `{EROFS(0), FUSE(1)}` is depth 2; `FILESYSTEM_MAX_STACK_DEPTH = 2` and overlayfs checks `>` ([`super.c:1207-1208`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/super.c)), so 2 passes. `BACKING_OPEN` rejects backing files whose `s_stack_depth >= max_stack_depth` ([`backing.c:110-112`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c)) — with `=1`, the backing cache must be depth-0 (ext4/xfs hostPath), not another overlay or FUSE. `FUSE_PASSTHROUGH` and `FUSE_WRITEBACK_CACHE` are mutually exclusive (inode.c:1440); the digest-FUSE is read-only and does not request writeback. The privilege-boundary spike VM test asserts the overlay mount succeeds at depth 2, an unprivileged `BACKING_OPEN` fails `EPERM`, and a mountd-brokered one succeeds with reads reaching the ext4 backing.

---

## 3. Alternatives considered

Two other approaches were evaluated in depth before composefs-style was spiked. Both remain viable kernel-filesystem designs; neither is retained as a fallback.

**EROFS + fscache on-demand.** Per-store-path EROFS bootstrap blobs in S3, merged at build-start into one mount; `cachefilesd`-style userspace daemon answers `/dev/cachefiles` on-demand `READ{cookie,off,len}` upcalls by reverse-mapping `(cookie,off) → nar_offset → chunk-range`. ~2 700 owned LoC, all userspace. Ruled out: ~70 ms mount latency (one eager `OPEN` per device-slot at mount, ×357 paths), no kernel-side per-file dedup (cachefiles key is `(cookie,range)` not content), per-path S3 artifact + GC tracking, and a `(cookie,off)→nar_offset` reverse-map that exists only to bridge fscache's range-addressing to rio's chunk-addressing.

**Custom `riofs` kernel module.** ~2 800 LoC in-tree-style C: `read_folio` posts `{chunk_digest}` to a `/dev/riofs` ring, userspace fetches and `write()`s back. Elegant runtime (no merge, no rio-store change, native chunk addressing, optional kernel-side digest cache), but ~800 LoC of genuinely-novel folio-lock/completion kernel code with a 2-3 min VM dev loop and zero upstream review/fuzz coverage. The latency wins were ≤2% of cold-miss (network-bound) and the recurring VFS-API-churn + we-are-the-only-debuggers cost is permanent.

composefs-style (§2) drops the reverse-map, device-slot ceiling, eager-OPEN mount cost, cachefiles daemon, and rio-store S3 artifact of the fscache approach; matches both alternatives on the warm path; achieves structural per-file dedup neither can without extra machinery; and is ~1 800 owned LoC with zero kernel code and a smaller privileged surface than the pre-ADR-022 FUSE setup.

---

## 4. Decision

**composefs-style (§2)**, with no maintained fallback. Spike evidence (§2.5) shows <10 ms mount, 0 warm-read upcalls, kernel-side per-file dedup, 5.3 MiB metadata for a chromium closure. The known cost — whole-file cold `open()` of giants — is resolved by streaming-open (§2.7), which ships unconditionally.

---

## 5. Sources

Primary:
- [`containers/composefs`](https://github.com/containers/composefs) — mechanism, [`composefs-dump(5)`](https://github.com/containers/composefs/blob/main/man/composefs-dump.md)
- [`containers/composefs` `libcomposefs`](https://github.com/containers/composefs) — FFI encoder; [`lcfs-writer.h`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-writer.h), [`lcfs-writer-erofs.c`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-writer-erofs.c), [`lcfs-internal.h`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-internal.h)
- [`erofs/erofs-utils`](https://github.com/erofs/erofs-utils) — `mkfs.erofs --tar=headerball` (fallback noted in §2.2)
- [`Documentation/filesystems/overlayfs.rst` §Data-only lower layers, §Metadata only copy up](https://docs.kernel.org/filesystems/overlayfs.html)
- [`fs/overlayfs/util.c` `ovl_get_redirect_xattr`/`ovl_check_metacopy_xattr`/`ovl_validate_verity`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/util.c)
- [`fs/overlayfs/namei.c:1237-1247`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/namei.c) + [`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9) — data-only-lower redirect gate under `userxattr` (`ofs->numdatalayer > 0`, not `config->metacopy`)
- [`9fe2a036`](https://git.kernel.org/linus/9fe2a036a23ceeac402c4fde8ec37c02ab25f133) — FUSE fs-verity ioctl-forwarding (does not populate `i_verity_info`)
- [`fs/verity/measure.c`](https://github.com/torvalds/linux/blob/master/fs/verity/measure.c), [`include/linux/fsverity.h`](https://github.com/torvalds/linux/blob/master/include/linux/fsverity.h) — `fsverity_get_digest()` / `fsverity_get_info()`
- [snix castore data model](https://snix.dev/docs/components/castore/data-model/) — per-file merkle motivation
- Spikes (consolidated on `adr-022`): core/scale/stream `15a9db79` ([`composefs-spike.nix`](../../nix/tests/scenarios/composefs-spike.nix), [`-scale.nix`](../../nix/tests/scenarios/composefs-spike-scale.nix), [`-stream.nix`](../../nix/tests/scenarios/composefs-spike-stream.nix), [`spike_digest_fuse.rs`](../../rio-builder/src/bin/spike_digest_fuse.rs), [`spike_stream_fuse.rs`](../../rio-builder/src/bin/spike_stream_fuse.rs)); privilege boundary `af8db499` ([`-priv.nix`](../../nix/tests/scenarios/composefs-spike-priv.nix), [`spike_mountd.rs`](../../rio-builder/src/bin/spike_mountd.rs)); access patterns `42aa81b2` ([`spike-access-data/RESULTS.md`](../../nix/tests/lib/spike-access-data/RESULTS.md)); kvm hostPath `9492019c` ([`kvm-hostpath-spike.nix`](../../nix/tests/scenarios/kvm-hostpath-spike.nix))
- `~/src/nix-index/main/top1000.csv` — 1000 largest files in nixpkgs (streaming-open sizing)

Our code:
- `rio-store/src/grpc/put_path.rs`, `cas.rs`, `chunker.rs`, `manifest.rs`
- `rio-proto/proto/types.proto`
- `rio-builder/src/fuse/{mod,ops,fetch}.rs`, `overlay.rs`

---

## 6. Extension: chunked output upload (`PutPathChunked`)

The §2 stack optimizes the **read** side — inputs reach the build via per-file redirects into a content-addressed lower. Outputs still leave the build via the pre-ADR-022 `PutPath`: two disk passes over the overlay upper (refscan, then NAR-stream), full NAR over gRPC, store-side **whole-NAR `Vec<u8>` buffer** ([`put_path.rs:431/485`](../../rio-store/src/grpc/put_path.rs)) before FastCDC. Output bytes are traversed ≥4× and the RAM buffer is the standing 40 GiB-RSS hazard the `nar_bytes_budget` semaphore exists to fence.

`PutPathChunked` moves chunking to the builder and reduces the wire to missing chunks plus a manifest. The justification holds at zero dedup: the store-side RAM buffer disappears, builder-side disk reads drop 2×→1×, and FastCDC CPU moves off the shared rio-store replicas onto per-build ephemeral cores. Dedup is upside, not the gate.

r[store.put.chunked]

```text
builder ──gRPC──▶ rio-store ──verify blake3──▶ S3 PutObject ──best-effort──▶ S3 Express (per-AZ)
   │  (only egress)     │  (per-chunk, on arrival;             (TieredChunkBackend §9)
   │                    │   no whole-NAR buffer)
   └── air-gapped: never reaches S3, S3 Express, or any network endpoint other than rio-store
```

**This does not widen the builder trust boundary or the cache-tier surface.** The builder's only network egress remains rio-store gRPC — the air-gap invariant is unchanged. S3 standard and S3 Express are reached **exclusively by rio-store**, exactly as in `PutPath` today; the difference is internal to rio-store: it S3-writes each verified chunk on arrival instead of accumulating the full NAR in a `Vec<u8>` first. rio-mountd is uninvolved; the per-AZ Express cache stays unreachable from the builder — chunks reach it via rio-store's existing `TieredChunkBackend.put` (S3-standard first, then best-effort Express; [Design Overview §9](./022-design-overview.md)). The §2.7 rejection of a builder-writable shared FS stands unchanged.

### 6.1 Builder-side fused walk

r[builder.upload.fused-walk]

After the build exits, `upload_all_outputs` walks each output's directory tree **once**, in canonical NAR entry order (`r[builder.nar.entry-name-safety]`). Per regular file, a single read drives four sinks in lockstep: FastCDC rolling-hash boundary detection (same `16/64/256 KiB` params as `rio-store/src/chunker.rs`, `r[store.cas.fastcdc]`) emitting `(offset, len, blake3)` per chunk; a whole-file blake3 accumulator yielding `file_digest`; the Boyer-Moore reference scanner over raw bytes (`r[builder.upload.references-scanned]`); and the SHA-256 accumulator over **NAR-framed** bytes (the walk emits NAR framing — `nar::Encoder` headers/padding — into the SHA-256 sink only; no NAR byte stream is materialized). At end-of-walk the builder holds `{chunk_manifest, file_digests, root_dir_digest, refs, nar_hash, nar_size}` having read each output byte exactly once. This closes `TODO(P0433)` (refs forced into a separate pre-pass) and `TODO(P0434)` (manifest-first upload).

r[builder.upload.chunked-manifest]

The chunk manifest is the ordered list `[(chunk_digest, len)]` per file, alongside the `Directory` tree (`r[store.index.dir-digest]`). It is sufficient for rio-store to reconstruct the NAR byte stream from CAS chunks without builder participation.

**Input-reuse shortcut (builder-local, no protocol change).** Before chunking an output regular file, the walk may consult a `(size, file_digest)` table of the build's declared inputs — already known from the EROFS metadata image's redirect xattrs (§2.1). On a size match, a content `cmp` against the composefs-lower file confirms identity and the input's `file_digest` and chunk list are reused without hashing. This is the [ostree `devino_to_csum_cache`](https://ostreedev.github.io/ostree/reference/ostree-OstreeRepo.html) pattern adapted to a content-addressed lower; it pays off for `cp`-heavy and fixed-output builds.

### 6.2 Wire protocol

r[store.chunk.has-chunks-durable]

`HasChunks([chunk_digest]) → bitmap` is the chunk-granular sibling of the file-level `HasBlobs` (P0573, `r[store.castore.blob-read]`). A bit is set **only if the chunk is S3-durable** — i.e., referenced by at least one *complete* manifest, not merely refcount ≥1. The distinction is I-201 (stranded-chunk race): a SIGKILL between refcount-bump and S3 `PutObject`, combined with a concurrent uploader's presence-skip, permanently strands the digest. Under durable-presence semantics two builders racing on the same novel chunk both see `false`, both upload, and the second S3 `PutObject` is an idempotent overwrite of identical content. The builder MAY first probe `HasBlobs([file_digest])` to short-circuit whole files before chunking them.

r[store.chunk.self-verify]

`PutPathChunked` is a client-stream: `Begin{hmac_token, store_path, deriver, refs, root_dir_digest, nar_hash, nar_size, chunk_manifest}` then zero or more `Chunk{digest, bytes}` for the digests `HasChunks` reported absent. rio-store validates the HMAC assignment token (`r[store.hmac.san-bypass]` — scheduler-signed authorization for *this* builder to upload *this* derivation's outputs), inserts a placeholder manifest (`r[store.put.wal-manifest]`), then for each received `Chunk` asserts `blake3(bytes) == digest` and — **rio-store, not the builder** — issues the S3 `PutObject` via `cas::put` (×32-parallel, `r[store.cas.upload-bounded]`). The whole-NAR `Vec<u8>` is gone: rio-store's working set per stream is one ≤256 KiB chunk in flight, not `nar_size` bytes. A `Chunk` whose digest was not declared in `Begin.chunk_manifest`, or whose blake3 mismatches, fails the stream with `INVALID_ARGUMENT`.

Commit: when every `chunk_manifest` digest is S3-durable (uploaded now or pre-existing), the placeholder flips `uploading → complete` in the manifest WAL. Builder death mid-stream leaves a placeholder and orphan chunks; both are swept by the existing GC (`r[store.gc.orphan-heartbeat]`, `r[store.chunk.grace-ttl]`). `PutPathChunked` is idempotent (`r[store.put.idempotent]`) — re-drive after transport failure repeats `HasChunks` and re-sends only still-missing chunks.

### 6.3 NarHash trust — async verify

r[store.put.narhash-async]

The builder is untrusted (`r[builder.upload.references-scanned]` context: outputs are adversary-controlled bytes); rio-store signs `narinfo`. The `nar_hash` in `Begin` is therefore **claimed**, not attested. On commit, rio-store records `nar_hash_verified = false` and enqueues a background job that NAR-serializes the path from CAS chunks (via `chunk_manifest` + `Directory` tree — same machinery as `ReadBlob`, `r[store.castore.blob-read]`), computes SHA-256, and compares. The verify reads chunks from S3 (standard or Express) — never from the builder — so the wire cost stays missing-chunks-only.

The path is **immediately usable for chunk-addressed reads**: §2's digest-FUSE, `ReadBlob`, and delta-sync (`r[gw.substitute.dag-delta-sync]`) key on `file_digest`/`chunk_digest`, which are self-certifying. Only the legacy binary-cache surface (`narinfo` + NAR fetch) depends on `nar_hash`; serving a `narinfo` for an unverified path **blocks on the verify job** (or returns 404, configuration-dependent).

r[store.put.narhash-quarantine]

On mismatch the path is quarantined: manifest flips to `quarantined`, the path is excluded from all query surfaces, and an alert fires carrying `{store_path, drv_path, builder_pod, claimed_nar_hash, computed_nar_hash}`. Chunks are **not** deleted — they are content-addressed and may be legitimately referenced by other manifests. A mismatch is either a builder bug (NAR-framing divergence in the fused walk's SHA-256 sink) or a compromised builder; both warrant operator attention, neither warrants serving the claimed hash.

**Considered and rejected:** synchronous verify before commit (re-reads every chunk on the hot path; defeats the latency win for the common case where downstream consumers are rio builds, not `nix copy`); trusting builder `nar_hash` outright (rio-store's narinfo signature would attest an adversary-controlled value); dropping `nar_hash` from the manifest entirely (breaks substitution by stock Nix clients — a non-goal to break).

### 6.4 Prior art

REAPI [`FindMissingBlobs`](https://github.com/bazelbuild/remote-apis/blob/main/build/bazel/remote/execution/v2/remote_execution.proto) → `HasChunks`; BuildBarn [ADR-0003 CAS decomposition](https://github.com/buildbarn/bb-adrs/blob/main/0003-cas-decomposition.md) → upload chunk granularity matches read-path chunk granularity; ostree `devino_to_csum_cache` → §6.1 input-reuse shortcut; [`mkcomposefs --digest-store`](https://man.archlinux.org/man/mkcomposefs.1.en) → walk-and-reflink-into-object-store (inapplicable directly: upper is local SSD, cache tier is an object store, no reflink — but the walk shape is the same); Nix [#4075](https://github.com/NixOS/nix/issues/4075)/[#7527](https://github.com/NixOS/nix/issues/7527) → existence-check before serialize, and the whole-NAR-granularity pain point this section eliminates.

---

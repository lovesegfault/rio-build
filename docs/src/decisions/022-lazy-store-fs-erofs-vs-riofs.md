# ADR-022: Lazy /nix/store filesystem — EROFS+fscache vs custom riofs kmod vs composefs-style

Status: **Superseded 2026-04-05 — Path C (composefs-style); see §5.** Prior status: Accepted (Path A — EROFS+fscache).

---


**Scope:** deep technical comparison of phase-2 candidates from [PLAN-KERNEL-FS.md](PLAN-KERNEL-FS.md) §2.1/§2.2/§2.6. V1/V8/V9 are answered with primary sources. **§3a (Path C)** added 2026-04-05 after spike validation; original §2/§3 retained as the evaluation record.

---

## 0. Deployment context — settled, no longer a tiebreaker

**Decision (external to this report):** builder nodes move to a NixOS-based AMI regardless of A vs B. Consequences:

| | Under Bottlerocket (was) | Under NixOS (now) |
|---|---|---|
| **A** Kconfig | `_ONDEMAND` symbols compiled out (confirmed: [`bottlerocket-kernel-kit kernel-6.{12,18}/config-full-bottlerocket-x86_64`](https://github.com/bottlerocket-os/bottlerocket-kernel-kit) has `# CONFIG_EROFS_FS_ONDEMAND is not set`, `# CONFIG_CACHEFILES_ONDEMAND is not set`). Would need 2-line upstream PR or custom variant. | `boot.kernelPatches = [{ name = "erofs-ondemand"; patch = null; extraStructuredConfig = { EROFS_FS_ONDEMAND = yes; CACHEFILES_ONDEMAND = yes; }; }];` — five lines in our AMI flake. |
| **B** module load | `lockdown=integrity` default ([bottlerocket#813](https://github.com/bottlerocket-os/bottlerocket/issues/813)) rejects unsigned `.ko`; kmod-kit doesn't ship the signing key. Would need `lockdown=none` (security regression) or custom variant with signed module. | `boot.extraModulePackages = [ config.boot.kernelPackages.riofs ];` where `riofs` is a `nix/kmod/riofs.nix` derivation. NixOS doesn't enable lockdown by default; if we choose to, we control the keyring. |
| **Net** | Asymmetric — A had a path back to stock; B never did. | **Symmetric** — both are first-party config in an AMI we already own. |

For the record, the host's own NixOS 6.19.9 also ships `_ONDEMAND` off — these symbols are off in essentially every distro defconfig. Both candidates need a kernel config we control; with NixOS, we have one.

**The rest of this report compares on pure technical merit:** code owned, latency, dedup, blast radius, debugging, churn, time-to-prototype, exit cost.

---

## 1. Recap — what both deliver that FUSE cannot

(See [PLAN-KERNEL-FS.md §1](PLAN-KERNEL-FS.md).) The unreachable-via-FUSE property: **a warm range of a partially-materialized file is read with zero userspace crossings.** FUSE passthrough binds one backing fd at `open()`; a 200 MB `libLLVM.so` with 4 MB of hot `.rodata` either upcalls on every read until fully fetched or blocks `open()` for ~1.3 s. Both A and B serve that 4 MB from page cache after first touch with the other 196 MB unfetched. Both also make `lookup`/`stat`/`readdir` kernel-native.

---

## 2. Candidate A — EROFS + fscache on-demand

### A.1 rio-store write-path changes

Today ([`put_path.rs:222-658`](rio-store/src/grpc/put_path.rs)):

```text
PutPath stream → buffer NAR (Vec<u8>) → SHA-256 verify → if ≥INLINE_THRESHOLD:
  cas::put_chunked(): chunker::chunk_nar(&nar)  # FastCDC 16/64/256 KiB over raw NAR bytes
    → upsert chunk refcounts (PG) → parallel S3 PUT new chunks → manifest row (ChunkRef[]) status=complete
```

No per-file index, no NAR parse on the write path. For EROFS, step 6 grows a third action **after** `put_chunked` succeeds:

```rust
// nar_data is still in memory, already SHA-verified.
let tree = rio_nix::nar::parse(&nar_data)?;                 // NarEntry tree (rio-nix already has this)
let boot = erofs::Bootstrap::from_nar(&tree, &chunk_manifest, BLK_64K)?;  // §A.2
backend.put(&format!("boot/{nar_hash}.erofs"), boot.bytes())?;
metadata::set_bootstrap(&pool, &store_path_hash, boot.len())?;            // new manifests.boot_size column
```

**LoC:** ~800 in `rio-store/src/erofs.rs` (on-disk encoder — see §A.2), ~100 in `put_path.rs`/`put_path_batch.rs`, ~50 in `metadata/queries.rs`, one migration (`manifests.boot_size BIGINT NULL`). **No `mkfs.erofs` shell-out** — it wants a directory on disk; we have a NAR in RAM.

**When:** at PutPath. The NAR is already buffered ([`put_path.rs:431`](rio-store/src/grpc/put_path.rs) `nar_data: Vec<u8>`); one more walk is ~free. Lazy generation would re-download chunks → reassemble → parse, which is the I-110 burst we built batching to avoid. **Backfill** for existing manifests: one-shot `xtask backfill-erofs-boot` (walk `manifests WHERE boot_size IS NULL` → GetPath → encode → upload).

**Where:** S3 sibling to chunks, `s3://…/boot/<narhash>.erofs`. GC deletes it when the manifest row goes (1:1, no refcount).

### A.2 EROFS image structure — bootstrap vs blobs; can blobs BE our chunks?

EROFS regular-file data has two layouts ([`fs/erofs/erofs_fs.h`](https://github.com/torvalds/linux/blob/master/fs/erofs/erofs_fs.h)):

- **Flat:** contiguous blocks at `startblk_lo` in the primary device.
- **Chunk-indexed** (`EROFS_CHUNK_FORMAT_INDEXES`, 5.15+): per-file array of 8-byte `struct erofs_inode_chunk_index { __le16 startblk_hi; __le16 device_id; __le32 startblk_lo; }`. `device_id` selects one of ≤65 535 "extra devices" (= blobs) named in the superblock device table; `startblk` is a **block-aligned** offset within that blob. Chunk size = `block_size << blkbits`, power-of-2, 4 KiB–1 MiB.

A **bootstrap** (Nydus term; "meta blob") = small EROFS image with superblock + inodes + dirents + per-file chunk-index arrays + device table, **no file data**. EROFS asks fscache for blob data by `(cookie = device-slot tag, off, len)`.

**Can data blobs BE our FastCDC chunks?** **No**, two structural reasons:

1. **Alignment.** `startblk` is a block number. EROFS chunks are `4KiB × 2ⁿ`; FastCDC chunks ([`chunker.rs:32-39`](rio-store/src/chunker.rs)) are 16-256 KiB at content-defined byte boundaries. A FastCDC cut at NAR byte 17 313 is unrepresentable.
2. **Cardinality.** `device_id` is `__le16` → 65 535 blobs max. A chromium closure ≈ 600 000 FastCDC chunks.

**The mapping that works** (= what RAFS v6 does): one **logical blob per store path**, content = "concatenation of this NAR's regular-file payloads, NAR-walk order, zero-padded to block boundaries." The blob *never exists in S3* — it's just a `cookie_key` string. When the daemon gets `READ{cookie, off, len}`:

```text
(off,len) in logical-blob space
  → which file?                            (per-path file-offset table; phase-1 builds this)
  → where in NAR byte stream?              (file.nar_offset + (off − file.blob_offset))
  → which FastCDC chunks cover that range? (binsearch ChunkRef cumsum; ≤2 boundary + N interior)
  → GetChunk×k → assemble [off,off+len) → pwrite(anon_fd) → ioctl COMPLETE
```

Worst-case over-fetch: `2 × CHUNK_MAX − len` ≈ **<512 KiB** per cold miss. Interior chunks land whole in moka.

**`mkfs.erofs --blobdev` / RAFS v6:** `mkfs.erofs --chunksize=65536 --blobdev=X` builds the chunk-indexed layout from a *directory*, writing data to `X` and metadata to the image. RAFS v6 = EROFS-on-disk + a feature flag + Nydus xattrs. We need vanilla chunk-indexed EROFS, encoded in-process from an in-memory NAR; [`nydus-rafs`](https://github.com/dragonflyoss/nydus/tree/master/rafs) is the Apache-2.0 Rust reference.

### A.3 Builder mount sequence

Replacing [`mount_fuse_background()`](rio-builder/src/fuse/mod.rs:494):

```rust
pub fn mount_erofs_background(mount_point: &Path, cache_dir: &Path,
                              closure_boot: &Path, clients: StoreClients,
                              rt: Handle) -> Result<ErofsMount> {
    // 1. /dev/cachefiles: configure + bind ondemand. Order matters; one write() per cmd.
    let dev = OpenOptions::new().read(true).write(true).open("/dev/cachefiles")?;
    dev.write_all(format!("dir {}", cache_dir.display()).as_bytes())?;
    dev.write_all(b"tag rio")?;
    dev.write_all(b"bind ondemand")?;             // ← mode switch; fd is now pollable

    // 2. Spawn upcall handler BEFORE mount — mount() triggers OPEN for the bootstrap;
    //    nobody listening = mount() blocks in D.
    let handler = rt.spawn(fscache_upcall_loop(dev.try_clone()?, clients, ...));

    // 3. Mount. source="none" (data via fscache); fsid = bootstrap cookie; domain_id =
    //    blob-sharing namespace (one per node so STS pods reuse warm blobs across builds).
    //    Device table is IN the bootstrap superblock (erofs_deviceslot[]) — no `device=` opts.
    nix::mount::mount(Some("none"), mount_point, Some("erofs"),
        MsFlags::MS_RDONLY | MsFlags::MS_NODEV,
        Some(format!("fsid=rio-boot-{build_id},domain_id=rio").as_str()))?;
    Ok(ErofsMount { mount_point, handler, dev })
}
```

**Serving the bootstrap.** EROFS's first act post-`mount()` is reading its own superblock — via fscache. The OPEN handler must recognize `cookie_key == "rio-boot-<id>"` and serve the local merged-bootstrap file (`pread` from SSD).

**Build-start merge** (before mount). Extend `ManifestHint` ([`types.proto:207`](rio-proto/proto/types.proto)) with `optional bytes boot_blob = 4` (~1-10 KB each; ~15 MB for 3 000 paths). `erofs::merge(&boots)`: one root dir with N store-path children, splice each subtree, union device tables, renumber `device_id`s, rewrite chunk indices. ~400 LoC; [`nydus-image merge`](https://github.com/dragonflyoss/nydus) is the reference. Critical-path latency = ~15 MB batched download (~100 ms cluster-net) + in-memory splice (V4 target <200 ms for 300 k inodes).

### A.4 `/dev/cachefiles` upcall protocol — exact wire format

From local 6.18 uapi [`include/uapi/linux/cachefiles.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/cachefiles.h) — **frozen userspace ABI**:

```c
#define CACHEFILES_MSG_MAX_SIZE  1024
enum cachefiles_opcode { CACHEFILES_OP_OPEN, CACHEFILES_OP_CLOSE, CACHEFILES_OP_READ };

struct cachefiles_msg {     // 16-byte header on every read()
    __u32 msg_id;           // echo in reply
    __u32 opcode;
    __u32 len;              // total incl header
    __u32 object_id;        // sticky per cache file
    __u8  data[];
};
struct cachefiles_open {    // OP_OPEN payload
    __u32 volume_key_size;  // NUL-terminated string ("erofs,<domain_id>")
    __u32 cookie_key_size;  // opaque binary (our blob tag)
    __u32 fd;               // ← anon_fd installed in OUR fd table; pwrite() target
    __u32 flags;
    __u8  data[];           // [volume_key][cookie_key]
};
struct cachefiles_read {    // OP_READ payload
    __u64 off; __u64 len;
};
#define CACHEFILES_IOC_READ_COMPLETE  _IOW(0x98, 1, int)   // ioctl(anon_fd, …, msg_id)
```

| Req | Reply |
|---|---|
| `OP_OPEN` | `write(dev_fd, "copen <msg_id>,<size_or_-errno>")` — text, on the **device** fd |
| `OP_READ` | `pwrite(anon_fd, data, len, off)` then `ioctl(anon_fd, CACHEFILES_IOC_READ_COMPLETE, msg_id)` — on the **anon** fd |
| `OP_CLOSE` | none; `close(anon_fd)`, drop object_id |

**rio-builder daemon** (`tokio::io::unix::AsyncFd`, not `mio` like Nydus, to share the runtime with [`fetch.rs`](rio-builder/src/fuse/fetch.rs)):

```rust
async fn fscache_upcall_loop(dev: File, clients: StoreClients,
                             objects: DashMap<u32, ObjState>, idx: CookieIndex) -> ! {
    let dev = AsyncFd::new(dev)?;
    let mut buf = [0u8; 1024];
    loop {
        let n = dev.readable().await?.try_io(|f| f.get_ref().read(&mut buf))??;
        let hdr = Msg::parse(&buf[..n]);
        match hdr.opcode {
            Open => {
                let o = OpenMsg::parse(&buf[16..n]);
                let anon = unsafe { OwnedFd::from_raw_fd(o.fd as RawFd) };
                let size = match idx.lookup(&o.cookie_key) {
                    Some(b) => { objects.insert(hdr.object_id, ObjState{anon, b}); b.size as i64 }
                    None    => -libc::ENOENT as i64,
                };
                dev.get_ref().write_all(format!("copen {},{}", hdr.msg_id, size).as_bytes())?;
            }
            Read => {
                let r = ReadMsg::parse(&buf[16..n]);
                let st = objects.get(&hdr.object_id).unwrap().clone();
                tokio::spawn(async move {                    // out-of-order completion ok (keyed by msg_id)
                    let bytes = fetch_blob_range(&clients, &st.b, r.off, r.len).await;  // §A.2 reverse-map
                    pwrite(st.anon.as_raw_fd(), &bytes, r.off as i64)?;
                    unsafe { fscache_cread(st.anon.as_raw_fd(), hdr.msg_id as u64) }?;  // ioctl_write_int!(.., 0x98, 1)
                });
            }
            Close => { objects.remove(&hdr.object_id); }
        }
    }
}
```

`/dev/cachefiles` is single-reader; concurrency comes from spawn-per-READ. Nydus does the same with a thread pool ([`fs_cache.rs`](https://github.com/dragonflyoss/nydus/blob/master/service/src/fs_cache.rs)).

### A.5 overlayfs stacking

EROFS sets `sb->s_stack_depth = 0` ([`fs/erofs/super.c:660-671`](https://github.com/torvalds/linux/blob/master/fs/erofs/super.c) only checks the *backing file*'s depth in file-backed mode; fscache mode has no backing-file fs). `overlay(upper=tmpfs, lower=erofs-on-fscache)` is depth 1 — **frees** the slot we currently spend on FUSE's `max_stack_depth=1`. composefs ships exactly this stack in production.

### A.6 Failure modes

| Failure | Kernel behavior | rio handling |
|---|---|---|
| **Daemon crash mid-READ** | Reader in `D` on `folio_wait_bit`. On `/dev/cachefiles` close, requests stay queued (since 6.4, [`c8383054506c`](https://git.kernel.org/linus/c8383054506c)). Next daemon writes `"restore"` instead of `"bind ondemand"` → kernel re-delivers pending → readers unblock. **Mounts survive.** | Supervisor respawns task with `restore=true`. No I-055-class ENOTCONN storm. ([`fs_cache.rs:269-315`](https://github.com/dragonflyoss/nydus/blob/master/service/src/fs_cache.rs) reference impl.) |
| **Daemon hung** | Reader in `D` indefinitely (no kernel timeout). | Per-spawn `tokio::time::timeout` (same as today's `jit_fetch_timeout`); on timeout `pwrite` zeros + ioctl so build fails its checksum loudly rather than wedging the node. |
| **`copen -ENOENT`** | EROFS read fails `-EIO`. | Correct — we don't have the path. |
| **Backing store full** | cachefiles culling (`bcull`/`brun` watermarks) evicts cold objects. | Give cachefiles the SSD budget; retire our `cache_dir` LRU on this path. |

### A.7 Kconfig (NixOS)

```nix
boot.kernelPatches = [{
  name = "erofs-ondemand";
  patch = null;
  extraStructuredConfig = with lib.kernel; {
    EROFS_FS          = yes;      # nixpkgs default =m; =y avoids modprobe ordering
    EROFS_FS_ONDEMAND = yes;
    CACHEFILES        = yes;
    CACHEFILES_ONDEMAND = yes;
    NETFS_SUPPORT     = yes;      # fscache backend
  };
}];
```

That is the entire kernel-side change for A.

---

## 3. Candidate B — custom `riofs` kernel module

### B.1 Mount-blob format

The builder hands the kernel a serialized index file path (or fd via `fsconfig(FSCONFIG_SET_FD, "index", …, idx_fd)` — cleaner). `fill_super` `kernel_read()`s it into a `kvmalloc`'d buffer and parses fixed-LE structs:

```c
struct riom_header { u8 magic[8]; u32 n_paths, n_inodes, n_chunks, strtab_len; };
struct riom_path   { u32 name_off; u32 root_ino; };
struct riom_inode  { u32 ino, parent, name_off; u16 mode, kind; u64 size;
                     u32 first_extent; u16 n_extents; u16 _pad; };       // DIR: first_child/n_children; LNK: target_off
struct riom_extent { u64 file_off; u32 chunk_idx; u32 chunk_off; u32 len; u32 _pad; };
// then: u8 chunk_digest[n_chunks][32]; u8 strtab[strtab_len];
```

For 3 000 paths × 100 files × 3 extents + 200 k digests ≈ **42 MB**, held for mount lifetime. Generated in-process by rio-builder from `BatchGetManifest`'s `ManifestHint`s — **no rio-store change, no S3 artifact**.

**Hardening:** every offset bounds-checked in `fill_super`; on failure `-EUCLEAN` and refuse mount. The bytes derive from NAR contents (filenames, sizes) → a malicious NAR could try OOB offsets. Same parser-hardening as `fs/erofs/super.c` but without LKML review.

### B.2 Upcall protocol

**Transport: `miscdevice` (`/dev/riofs`).** Direct precedent in `/dev/fuse`, `/dev/cachefiles`; ~200 LoC ring on `kfifo`/`xarray`; mainline `kernel::miscdevice` Rust binding exists. Netlink is overkill; io_uring (`.uring_cmd`) is a later optimization (upcall is network-bound, not syscall-bound).

```c
struct riofs_req       { u32 req_id; u32 _pad; u8 digest[32]; };           // K→U via read()
struct riofs_reply_hdr { u32 req_id; i32 err; u32 len; u32 _pad; /* u8 data[len] */ };  // U→K via write()
```

**Chunk-addressed, not byte-addressed** — the entire point of B over A. Userspace is ~50 LoC around `clients.get_chunk(digest)`. No reverse-map, no over-fetch.

**Kernel cold path:**

```c
static int riofs_read_folio(struct file *f, struct folio *folio) {
    struct riofs_inode_info *ri = RIOFS_I(folio->mapping->host);
    loff_t pos = folio_pos(folio);
    size_t len = min_t(size_t, folio_size(folio), i_size_read(...) - pos), done = 0;
    while (done < len) {
        struct riom_extent *e = riofs_find_extent(ri, pos + done);     // binsearch
        size_t in_chunk = e->chunk_off + (pos + done - e->file_off);
        size_t take     = min(len - done, e->len - (pos + done - e->file_off));
        struct riofs_chunk *c = riofs_chunk_lookup(sb, e->chunk_idx);  // optional digest cache
        if (!c) {
            riofs_post_and_wait(sb, e->chunk_idx, folio);              // kfifo push, wake poll_wait,
            c = riofs_chunk_lookup(sb, e->chunk_idx);                  //   wait_for_completion
            if (!c) { folio_unlock(folio); return -EIO; }
        }
        memcpy_to_folio(folio, done, c->data + in_chunk, take);
        done += take;
    }
    folio_zero_range(folio, done, folio_size(folio) - done);
    folio_mark_uptodate(folio); folio_unlock(folio);
    return 0;
}
```

`riofs_dev_write` parses reply, `kvmalloc`s chunk, `copy_from_user`, stores in `rhashtable` keyed by `chunk_idx`, `complete_all()` — every folio waiting on that digest wakes. **This is the cross-file dedup A can't do in-kernel.** Chunk eviction: `register_shrinker()` LRU, or write-through to a per-inode sparse backing file (`kernel_write` under `cache_dir`). For v0: no kernel chunk cache, page cache + userspace moka only.

### B.3 VFS surface — every op

From mainline [`include/linux/fs.h`](https://github.com/torvalds/linux/blob/master/include/linux/fs.h):

| Table | Member | riofs | Notes |
|---|---|---|---|
| `file_system_type` | `.name`, `.init_fs_context`, `.kill_sb` | **req** | `kill_anon_super` + free index. `.fs_flags = 0` (no `FS_USERNS_MOUNT`). |
| `fs_context_operations` | `.parse_param`, `.get_tree`, `.free` | **req** | `get_tree_nodev(fc, riofs_fill_super)` |
| `super_operations` | `.alloc_inode`/`.free_inode` | **req** | embed `riofs_inode_info` |
|  | `.statfs`, `.put_super` | **req** | `simple_statfs`; free index, unregister miscdev |
|  | `.show_options` | nice | echo `index=` |
| `inode_operations` (dir) | `.lookup`, `.getattr` | **req** | binsearch children; `d_splice_alias` |
| `inode_operations` (reg) | `.getattr` | **req** | `generic_fillattr` |
| `inode_operations` (lnk) | `.get_link`, `.getattr` | **req** | return strtab ptr |
|  | `.listxattr` | optional | NARs can carry `security.capability` |
| `file_operations` (dir) | `.iterate_shared`, `.llseek` | **req** | walk children |
| `file_operations` (reg) | `.read_iter` = **`generic_file_read_iter`** | **req** | page-cache-backed; this is the trick |
|  | `.mmap` = `generic_file_readonly_mmap` | **req** | `ld.so` mmaps `.so` |
|  | `.llseek`, `.splice_read` | std | `generic_file_llseek`, `filemap_splice_read` |
| **`address_space_operations`** | `.read_folio` | **req** | §B.2 — the novel code |
|  | `.readahead` | **strongly rec** | batch N folios → post all chunks → one wake. ~80 LoC. |
|  | `.migrate_folio` | std | `filemap_migrate_folio` |
|  | everything else | **omit** | RO fs |
| miscdev `file_operations` | `.read_iter`, `.write_iter`, `.poll`, `.open`, `.release` | **req** | `.release`: re-dump pending on next open (cachefiles-style failover) ≈ +100 LoC |

Novel code: `read_folio` + `readahead` + miscdev ring + `fill_super` parser. Everything else is `generic_*`/`simple_*` forwards. **2.5-3.5 kLoC C.**

### B.4 Rust-for-Linux status (V8, answered)

Surveyed `torvalds/linux` master `rust/kernel/`:

| Abstraction | Mainline | riofs use |
|---|---|---|
| `module!{}`, `Arc/Mutex/SpinLock/CondVar`, `KBox/KVec/KVVec`, `workqueue` | ✓ | ✓ |
| **`miscdevice::MiscDevice`** | **✓** | **`/dev/riofs` 100% safe Rust** |
| `uaccess::UserSlice{Reader,Writer}`, `page::Page` | ✓ | ✓ partial |
| **`kernel::fs::*`** | **only `file.rs` + `kiocb.rs`** | **insufficient** |
| `FileSystem`/`SuperBlock`/`INode`/`inode_operations` traits | ✗ mainline; rust-vfs branch only | carry ~1.5 kLoC out-of-tree |
| **`address_space_operations` / `Folio` API** | **✗ everywhere** | **`unsafe extern "C"` only** |
| `rhashtable`/`completion`/`kfifo` | ✗ | raw `bindings::*` |

Mainline `rust/kernel/fs/` has **two files**. A safe-Rust `read_folio` does not exist. Rust riofs = ~1 200 safe + ~350 unsafe FFI shim + **~1 500 carried** rust-vfs patches, perpetually rebased. **If B is chosen: write it in C.** `smatch`+`sparse`+KASAN on 3 kLoC catch the same bug classes; `fs/romfs` is a near-verbatim template. Revisit Rust when `rust/kernel/fs/` has >2 files.

### B.5 Build/ship — NixOS

```nix
# nix/kmod/riofs.nix
{ lib, stdenv, kernel }:
stdenv.mkDerivation {
  pname = "riofs"; version = "0.1";
  src = ./src;                                          # riofs.c, Kbuild
  nativeBuildInputs = kernel.moduleBuildDependencies;
  makeFlags = kernel.moduleMakeFlags ++ [ "M=$(PWD)" ];
  installPhase = ''install -Dm644 riofs.ko \
    $out/lib/modules/${kernel.modDirVersion}/extra/riofs.ko'';
  meta.platforms = lib.platforms.linux;
}
# AMI module
boot.extraModulePackages = [ (pkgs.callPackage ../kmod/riofs.nix
                               { inherit (config.boot.kernelPackages) kernel; }) ];
boot.kernelModules = [ "riofs" ];
```

Kernel-version-locked; rebuilds with `boot.kernelPackages`. KASAN dev variant: `boot.kernelPackages = pkgs.linuxPackages_latest_hardened` or a `structuredExtraConfig.KASAN = yes` overlay for the VM-test kernel only. **No signing dance, no lockdown, no sdk container.** Dev loop: `nix build .#nixosTests.riofs-smoke` (qemu VM with the module loaded) — same machinery as existing `nix/tests/`. The "quarterly VFS churn" becomes "fix the build when bumping nixpkgs," same class of work as any other dependency.

### B.6 VFS API churn — concrete history

`address_space_operations`-relevant, last 10 releases:

| Ver | Change | riofs hit? |
|---|---|---|
| 5.18 | `readpage`→`read_folio`; `page*`→`folio*` everywhere | **rewrite signature + body** |
| 5.19 | `readpages` removed | port if used |
| 6.0 | `migratepage`→`migrate_folio` | one-liner |
| 6.3 | `getattr` gained `mnt_idmap*` first arg | **3 signatures** |
| 6.8 | `error_remove_page`→`error_remove_folio` | n/a |
| 6.12 | `writepage` removed; `write_begin/end` `file*`→`kiocb*` | n/a (RO) |
| ongoing | iomap conversion pressure on simple RO fs | **risk** if romfs/cramfs get converted |

**4 of 10** releases would have needed a non-trivial patch → **~every other kernel bump** (~5 mo). Each is 1-4 h mechanical *if* tracking LKML; +1 d bisect if discovered via build break. Mitigated by: NixOS pins the kernel; bumps are deliberate; aops surface is minimal.

---

## 3a. Candidate C — composefs-style (EROFS metadata + overlay redirect → digest-addressed FUSE lower)

**Added 2026-04-05.** Not evaluated in the original A-vs-B analysis. The mechanism is the one [composefs](https://github.com/containers/composefs) ships for ostree/podman: an EROFS image carrying **metadata only** (inodes, dirents, sizes, modes, xattrs — zero data blocks), stacked under overlayfs with a second **data-only lower** holding files named by content digest. Each regular-file inode in the metadata layer carries `user.overlay.redirect=/ab/<blake3>` + `user.overlay.metacopy`; overlayfs follows the redirect **lazily on first `open()`**, not at `mount(2)`.

Relative to A: no fscache, no cachefiles daemon, no device table, no `(cookie,off)→nar_offset` reverse-map. Relative to B: no kernel code. The data-only lower is a thin FUSE mount serving `lookup(digest) → open → read` — back to FUSE, but **only for cold `open()`**, which is exactly where JIT-fetch-by-digest should block. Warm `read()` is page-cache via the overlay; `stat`/`readdir` never leave the kernel.

### C.1 Mount stack and lookup path

r[builder.fs.composefs-stack]

The builder mounts three layers: (1) EROFS metadata image loop-mounted RO; (2) digest-addressed FUSE at the per-build `objects_dir`; (3) `overlay -o ro,userxattr,lowerdir=<erofs>::<objects_dir>` at `/nix/store`. The `::` separator marks the FUSE mount as a **data-only lower** ([`Documentation/filesystems/overlayfs.rst`](https://docs.kernel.org/filesystems/overlayfs.html#data-only-lower-layers)) — overlayfs will not `lookup()` into it for path resolution, only follow absolute redirects.

r[builder.fs.userxattr-mount]

With `userxattr`, the overlay reads `user.overlay.{redirect,metacopy}` xattrs and is mountable from an **unprivileged userns**; do **not** pass `metacopy=on`/`redirect_dir=on` explicitly (rejected — `params.c:988-1008`). The presence of a `::` data-only lower independently enables following absolute redirects into it regardless of those options ([`namei.c:1241`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/namei.c), commit [`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9), v6.16+) — gated on `ofs->numdatalayer > 0`, not `config->metacopy`. The in-tree comment reads: "Don't require redirect=follow and metacopy=on in this case." This is the maintainer-designed userns-safe path; its safety condition ("lower layer is read-only and `user.overlay.redirect` cannot be modified") is structurally satisfied by EROFS.

| Syscall | Resolved by | FUSE upcalls |
|---|---|---|
| `stat`/`getattr` | EROFS inode (kernel) | 0 |
| `readdir` | EROFS dirents (kernel) | 0 |
| `open` (cold) | overlayfs follows `redirect` xattr → FUSE `lookup(prefix-dir)` + `lookup(digest)` + `open` | **2 lookup + 1 open**, depth-independent |
| `read` (cold) | FUSE `read` upcalls, ~128 KiB/req via readahead | O(filesize / 128 KiB) |
| `read` (warm) | page cache | **0** |

§1's killer constraint — FUSE passthrough binds one backing fd at `open()` so a 200 MB partially-hot `.so` either upcalls every read or blocks open — **is addressed by streaming open (§C.7).** The handler returns from `open()` after the first chunk; uncached `read()` ranges upcall once during the background fill, then 0 upcalls while pages remain cached. The FUSE-on-read cost is bounded to `filesize / 128 KiB` upcalls **once per file per node** (re-served from the SSD backing file if cgroup pressure evicts pages).

### C.2 Encoder — `mkcomposefs --from-file`

r[builder.fs.stub-isize]

The metadata image must encode each regular file's **real `i_size`** with zero data blocks. overlayfs metacopy surfaces the metadata layer's `i_size` to `stat()`; a stub encoded with `i_size=0` reports 0 to userspace even though `read()` returns full data — `mmap(len=st_size)` then maps nothing. Bare `mkfs.erofs` over a directory of 0-byte staging files is therefore **insufficient**.

[`mkcomposefs --from-file`](https://github.com/containers/composefs/blob/main/man/composefs-dump.md) takes a `composefs-dump(5)` text manifest (one line per node: escaped path, size, mode, nlink, uid, gid, rdev, mtime, **payload** = redirect target, content, **digest**) and emits the EROFS image directly — no staging dir, no per-file `setxattr`, correct `i_size`. The dump format is line-oriented and trivially generated from a `NarIndex` walk (the per-store-path file index — `{path, kind, size, executable, file_digest}` per entry, computed at PutPath-time); rio's encoder is a `NarIndex → dump-text` serializer plus either a subprocess call or a port of [`libcomposefs/lcfs-writer-erofs.c`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-writer-erofs.c) (~2.1 kLoC C, `GPL-2.0-or-later OR Apache-2.0`).

r[builder.fs.metacopy-xattr-shape]

`user.overlay.metacopy` must be either zero-length (legacy) or ≥4 bytes encoding `struct ovl_metacopy { u8 version; u8 len; u8 flags; u8 _pad; /* optional fsverity digest */ }`. 1-3 bytes → kernel `EIO "metacopy xattr too small"`. mkcomposefs writes the zero-length form when the dump's digest field is `-`.

**`user.*` prefix vs mkcomposefs:** `mkcomposefs --user-xattrs` does **not** flip the prefix of the *generated* redirect/metacopy xattrs — `OVERLAY_XATTR_PREFIX` is hardcoded `"trusted."` ([`lcfs-internal.h:37-50`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-internal.h)); the flag only filters *input* xattrs. Workaround: dump lines set `PAYLOAD=-` and carry explicit `user.overlay.redirect=/ab/<digest> user.overlay.metacopy=` as trailing xattr fields; mkcomposefs honors those, `i_size` is preserved. mkcomposefs still auto-emits a stray `trusted.overlay.metacopy` for size>0 metadata-only inodes (~24 B/file, harmless under `-o userxattr`). A ~10-line libcomposefs patch making the prefix configurable is upstreamable but not blocking.

### C.3 Mount sequence and privilege boundary

Privilege is split: a node-level **`rio-mountd`** (~50 LoC, CAP_SYS_ADMIN, DaemonSet) opens `/dev/fuse` + the EROFS superblock and hands both fds to the unprivileged builder over a UDS; the builder does the overlay mount itself inside its own userns. `rio-mountd` per request:

1. `open("/dev/fuse")` → `mount("fuse", objects_dir, …, "fd=N,…")` where `objects_dir = /var/rio/objects/{build_id}` → `SCM_RIGHTS` the fd to the builder
2. `fsopen("erofs")` → `fsconfig(FSCONFIG_SET_FLAG, "ro")` → `fsconfig(FSCONFIG_SET_STRING, "source", meta_image)` → `fsconfig(FSCONFIG_CMD_CREATE)` → `fsmount(…)` → `SCM_RIGHTS` the detached-mount fd
3. return (per request; the daemon process persists for the next pod and tracks the UDS connection for teardown)

The builder, inside its unprivileged userns:

4. `fuser::Session::from_fd(fuse_fd)` → spawn `digest_fuse::serve` (§C.4)
5. `move_mount(erofs_fd, "", AT_FDCWD, meta_mnt, MOVE_MOUNT_F_EMPTY_PATH)`
6. `mount("overlay", mount_point, "overlay", MS_RDONLY, "userxattr,lowerdir=<meta_mnt>::<objects_dir>")`

Teardown — builder closes the UDS (or its pod exits):

7. `rio-mountd` does `umount2(objects_dir, MNT_DETACH)` + `rmdir(objects_dir)`; the builder's mount-ns death takes the overlay + erofs mounts with it. Crash-safety: `rio-mountd` start-up scans `/var/rio/objects/*` for orphaned mounts and detaches them.

r[builder.fs.fd-handoff-ordering]

**Ordering is load-bearing:** the `/dev/fuse` fd MUST be received and the digest-FUSE server MUST be answering before step 6. overlayfs probes each lower's root at `mount(2)`; with no one serving `/dev/fuse`, that probe deadlocks. The `fsconfig` `"ro"` flag MUST precede `CMD_CREATE` — `MOUNT_ATTR_RDONLY` on `fsmount` is per-mount, not per-superblock, and erofs otherwise opens the bdev RW → `EACCES` on a read-only loop.

**No build-start merge step** (cf. §A.3): one EROFS image per closure, generated from the union of the closure's `NarIndex` rows — same input as A's merge but emitted as composefs-dump text, no device-table renumbering.

### C.4 Digest-FUSE handler

r[builder.fs.digest-fuse-open]

The data-only lower is a `fuser` filesystem rooted at the per-build `objects_dir` exposing exactly two directory levels: 256 prefix dirs (`00`..`ff`) and leaf files named by the remaining 62 hex chars of `blake3(file_content)`. `lookup(prefix, name)` consults a `file_digest → (size, executable)` map populated from the closure's `NarIndex`; unknown digests return `ENOENT`. `open` JIT-fetches the file by digest (see §C.7 for the fetch shape) and returns `FOPEN_KEEP_CACHE`. `read` serves from the materialized backing file. The handler reuses [`rio-builder/src/fuse/fetch.rs`](rio-builder/src/fuse/fetch.rs)'s bounded-memory chunk fan-out; the lookup key changes from `store_path` to `file_digest`.

r[builder.fs.shared-backing-cache]

The FUSE **mount point** is per-build (`/var/rio/objects/{build_id}/`, §C.3) for cross-pod isolation. The **backing cache** where fetched bytes land is shared node-SSD (`/var/rio/cache/ab/<digest>`). On `open`, the handler first checks the shared cache; on miss it `O_EXCL`-creates `/var/rio/cache/ab/<digest>.partial` — a concurrent fetcher that loses the `O_EXCL` race waits on inotify for the `.partial→<digest>` rename instead of double-fetching. This gives §C.7's node-level dedup ("second build pays 0") without exposing one build's mount to another.

This is **the only FUSE in the stack**, and it is hit only on cold `open()`. Spike-measured cold lookups are exactly 2 regardless of the merged path's depth — overlayfs walks the EROFS dirent chain in-kernel, then the redirect is one absolute jump.

### C.5 Spike evidence

Core-stack nixosTests on `adr-022` ([`composefs-spike.nix`](../../nix/tests/scenarios/composefs-spike.nix), [`composefs-spike-scale.nix`](../../nix/tests/scenarios/composefs-spike-scale.nix), ported in `15a9db79`); chromium-146 closure topology (357 store paths, 23 218 regular files, 8 221 dirs, 3 374 symlinks) with synthetic file content:

| Metric | Measured |
|---|---|
| `mount -t overlay` wall-clock | **<10 ms** (below `time(1)` granularity) |
| FUSE upcalls during mount | lookup=0 getattr=0 open=0 read=0 |
| EROFS metadata image (mkcomposefs) | **5.3 MiB** (≈239 B/file), encoded in **70 ms** |
| `find -type f` over 23 218 files | 60 ms, **0 FUSE upcalls** |
| `find -printf %s` sum over 23 218 files | 1 795 354 094 B == manifest, 120 ms, **0 FUSE upcalls** |
| Cold `lookup` upcalls (any depth) | **2** (prefix + digest) |
| Cold `read` upcalls, 31 MB file | 244 (≈128 KiB/req) |
| **Warm `read` upcalls** | **0** (all samples) |
| FUSE handler peak RSS | 8.9 MB |

Against A's targets: mount **<10 ms vs ~70 ms** (357 eager OPENs × ~200 µs); warm-read identical; metadata footprint 5.3 MiB vs ~15 MB boot-blob budget.

Follow-on spikes (consolidated on `adr-022`):

| Commit | Test | Finding |
|---|---|---|
| `15a9db79` | [`composefs-spike-stream.nix`](../../nix/tests/scenarios/composefs-spike-stream.nix) | Streaming-open (§C.7): `FOPEN_KEEP_CACHE` set at `open()` does not suppress cold-page upcalls (2049 reads on first `dd` of 256 MiB), only prevents invalidation — second `dd` 0 upcalls. **No mode-flip needed.** `mmap(MAP_PRIVATE)` page-faults route through FUSE `read`. `open()` 256 MiB with 10 ms/chunk backend → 10.3 ms (vs 2560 ms whole-file). |
| `42aa81b2` | [`spike-access-data/RESULTS.md`](../../nix/tests/lib/spike-access-data/RESULTS.md) | Access patterns: real consumers touch **0.3-33%** of giant `.so`/`.a` (link-against-libLLVM 2.79% bimodal head+tail; `opt --version` 32.77% scattered/266 ranges; `libicudata` 0.28%). `ld.so` uses no `MAP_POPULATE`/`fadvise`. |
| `af8db499` | [`composefs-spike-priv.nix`](../../nix/tests/scenarios/composefs-spike-priv.nix) | Privilege boundary (§C.3): all 6 questions PASS on kernel 6.18.20 — fd-handoff, stack-survives-mounter-exit, unpriv-userns-inherits, **`userxattr` unpriv overlay**, teardown-under-load (no D-state), `fsopen`/`fsmount` detached-fd handoff. |
| `9492019c` | [`kvm-hostpath-spike.nix`](../../nix/tests/scenarios/kvm-hostpath-spike.nix) | `/dev/kvm` via `extra-sandbox-paths` (§C.8): `ioctl(KVM_GET_API_VERSION)=12` from inside Nix sandbox; smarter-device-manager not required. |

### C.6 Integrity — fs-verity does not apply; per-file blake3 in handler

r[builder.fs.file-digest-integrity]

composefs's native integrity story embeds an fs-verity digest in the metacopy xattr; the kernel checks it against the backing file's fs-verity measurement at `open()`. **This requires fs-verity enabled on the lower filesystem**, which a FUSE lower cannot provide. Per-file integrity for C therefore lives in the digest-FUSE handler: **verify each chunk's blake3 against its content-address on arrival** (chunks are blake3-addressed in the CAS layer; `rio-store/src/chunker.rs`); never serve a byte from an unverified chunk. For files ≤ the streaming threshold (§C.7), the whole-file blake3 against `file_digest` additionally runs before `open()` returns. For files > threshold, per-chunk verification covers the streaming window and the whole-file `file_digest` check runs at fill-complete, gating the `.partial → /var/rio/cache/ab/<digest>` rename. The digest is the filename — the check is structural. This is the `file_digest` field proposed independently as a `NarIndexEntry` extension; here it is load-bearing. `file_digest` also serves as the per-file content-address for the Directory-merkle layer enabling closure delta-sync (U5); see `components/store.md` §NAR index.

What C **doesn't** have that B does: in-kernel range verification. C trusts the FUSE handler not to lie about the bytes it serves; B's `read_folio` could verify chunk digests kernel-side. For rio's threat model (builder is the FUSE server; builder is already trusted to not corrupt its own build), this is not a regression from A.

### C.7 Failure modes and the partial-file trade-off

| Failure | Kernel behavior | rio handling |
|---|---|---|
| **FUSE handler crash** | overlayfs `open()` on a redirect target → `ENOTCONN`. Existing open files keep their page-cache content (warm reads unaffected). | Supervisor respawns; next `open()` reconnects. **No D-state**, no `restore` dance. Simpler than A. |
| **FUSE handler hung mid-fetch** | `open()` blocks in `S` (interruptible — FUSE, not folio lock). | Per-spawn `tokio::timeout` returns `EIO` to the open; build fails loudly. Same shape as today's `jit_fetch_timeout`. |
| **Redirect target ENOENT** | overlayfs `open()` → `ENOENT`. | Handler returns ENOENT only for digests outside the closure's declared-input allowlist — correct (JIT fetch imperative). |
| **Build completes / pod exits** | Builder mount-ns death drops overlay+erofs; `objects_dir` FUSE mount persists in init-ns. | `rio-mountd` detaches it on UDS close (§C.3 step 7); start-up scan reaps orphans from a prior crash. |
| **Partial-file hot ranges** | First `open()` of a 200 MB `.so` blocks for the whole file. Subsequent reads of any range are page-cache. | **The trade-off.** See below. |

r[builder.fs.streaming-open-threshold]

**Streaming open ships unconditionally.** The 1000 largest files in nixpkgs are *all* >64 MiB (median 179 MiB, 7 files >1 GiB; `top1000.csv`), and access-pattern measurement (`42aa81b2`) shows consumers touch 0.3-33% of them — whole-file fetch over-fetches 64-99.7%. The mitigation, spike-proven (`15a9db79`): the digest-FUSE handler sets `FOPEN_KEEP_CACHE` **unconditionally** at `open()`. Files ≤ `STREAM_THRESHOLD` (default 8 MiB) fetch-whole-then-return. Files > threshold spawn a background fill task and return after the first chunk (~10 ms); `read(off,len)` upcalls once per uncached page during fill (priority-bumping the requested range), then 0 upcalls while pages remain cached — under cgroup memory pressure, evicted pages re-upcall and are re-served from the SSD backing file. ~80 LoC; **no mode-transition** — `KEEP_CACHE` does not suppress cold-page upcalls, only invalidation, so the kernel page cache *is* the transition. `mmap(MAP_PRIVATE)` page-faults route through the same `read` path, covering linkers. The shared node-SSD backing cache (§C.4) means the second build to touch `libLLVM.so` on that node pays 0.

**Considered and rejected for the partial-file case:** encoder-side file-splitting into N redirect targets (overlayfs `redirect` is single-path per inode; not expressible); allowlist-bounded prefetch of giants at mount (violates the JIT-fetch imperative — fetches inputs the build may never touch); composefs+fscache hybrid for giants only (resurrects ~70% of Path A's deleted LoC — cachefiles daemon, reverse-map, second encoder — for marginal gain over streaming-open); FSx-Lustre-backed cluster-wide `objects` cache (violates the builder air-gap — shared writable FS between untrusted builders is a cache-poisoning + lateral-movement surface).

### C.8 Kconfig (NixOS)

```nix
boot.kernelPatches = [{
  name = "overlay-composefs";
  patch = null;
  extraStructuredConfig = with lib.kernel; {
    EROFS_FS         = yes;
    OVERLAY_FS       = yes;       # nixpkgs default =m
    FUSE_FS          = yes;
    # No EROFS_FS_ONDEMAND, no CACHEFILES — C doesn't use fscache.
  };
}];
```

All three are stock-on in essentially every distro; the patch block is for `=y` over `=m` only. Requires kernel **≥6.16** ([`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9): data-only-lower redirect honored under `userxattr`) + **≥5.2** (`fsopen`/`fsmount`/`move_mount`); the NixOS-node module asserts this at boot.

**Device exposure:** no smarter-device-manager. `/dev/fuse` reaches the builder via `rio-mountd` fd-handoff (§C.3; builder pod never opens the device). `/dev/kvm` reaches the kvm-pool build via plain `hostPath` CharDevice volume + `nodeSelector: rio.build/kvm` + `nix.settings.extra-sandbox-paths=["/dev/kvm"]` (`65fecde2`).

---

## 4. Head-to-head

| Axis | **(A) EROFS + fscache** | **(B) `riofs` kmod** | **(C) composefs-style** |
|---|---|---|---|
| **Total LoC owned** | **~2 700** = 0 kernel + ~1 200 daemon (poll loop, reverse-map, cookie idx) + ~950 rio-store (encoder + PutPath + migration) + ~400 builder merge + ~150 nix/helm. Plus ~400 LoC vendored Nydus protocol parsing (Apache-2.0, attributed). | **~3 600** = ~2 800 kernel C + ~500 builder (`/dev/riofs` loop + `.riom` serializer) + **0 rio-store** + ~100 `nix/kmod/` + ~200 VM-test scaffolding. (Rust path: +~1 500 carried rust-vfs — don't.) | **~1 450** = 0 kernel + ~450 digest-FUSE (reuses `fuse/fetch.rs` fan-out) + ~80 streaming-open (§C.7) + ~250 `NarIndex→dump` serializer + ~300 rio-store (`file_digest` in NarIndex, PutPath blake3-per-file) + ~50 `rio-mountd` fd-handoff + ~170 builder mount + ~150 nix/helm. **Smaller privileged surface than today's FUSE setup** (rio-mountd opens 2 fds and exits; builder pod has zero device exposure). Subprocess `mkcomposefs`; porting `lcfs-writer-erofs.c` is +~2 100 if shell-out is unacceptable. |
| **Distribution of complexity** | All userspace; 100% `cargo nextest`-able; bugs = wrong bytes (build fails its checksum, loud). The fiddly part (reverse-map) is `proptest`-able. | ~800 LoC genuinely-novel kernel (read_folio + ring + waiters); ~2 000 romfs-shaped boilerplate. Bugs = hung folio lock, UAF on evicted chunk, `copy_from_user` length error. Dev loop = VM rebuild (~2-3 min). | All userspace; **no reverse-map, no merge-splice, no cookie state machine.** The fiddly part is the dump-text escaper (`proptest`-able against `mkcomposefs` round-trip). Digest-FUSE is a subset of today's `fuse/ops.rs`. |
| **rio-store write-path Δ** | +encoder, +PutPath hook, +migration, +S3 object class, +GC wiring, +backfill job. | **None.** | +`file_digest` per `NarIndexEntry` (one blake3 per regular file during `nar_ls`; bytes already in RAM). No new S3 object class. |
| **Build-start latency added** | ~15 MB boot-blob batch fetch + in-mem merge of ~300 k inodes (V4: target <200 ms; cache merged result per-closure-hash on STS pods to amortize). | `.riom` serialize from already-in-memory `ManifestHint`s — **~10 ms**. | `NarIndex` rows → dump text → `mkcomposefs` → 5.3 MiB image in **70 ms** measured; + loop-mount + overlay mount **<10 ms**. **No eager OPEN per device-slot.** |
| **Persistent artifacts** | `boot/<narhash>.erofs` per store path in S3 (~0.3% of NAR size). GC-tracked. | **None.** | **None required.** Metadata image generated per-build from `NarIndex` rows; optionally cache per-closure-hash on node SSD. |
| **Cold-miss latency** | `read_folio` → netfs → fscache → cachefiles xarray → poll wake → user `read` → reverse-map (~5 µs) → `GetChunk×k` (**~2-8 ms**) → assemble → `pwrite` → ioctl → fill folio. ≈ **net + ~40 µs + ≤512 KiB over-fetch.** | `read_folio` → kfifo push → poll wake → user `read` → `GetChunk×1` (**~2-8 ms**) → `write` → `copy_from_user` → `memcpy_to_folio` → `complete_all`. ≈ **net + ~15 µs, no over-fetch.** B saves ~25 µs + ≤256 KiB/miss; **both dominated by network RTT — effective tie.** | `open()` → overlay redirect → 2 FUSE lookups + 1 open → return after **first chunk** (streaming open, §C.7); subsequent uncached `read()` → priority-bumped chunk fetch. ≈ **net + ~10 ms to first byte; touched-range fetch only.** Effectively range-granular for files > `STREAM_THRESHOLD`; whole-file for small files (no over-fetch, no reverse-map). |
| **Warm-read latency** (page cache hit) | `filemap_read` → folio uptodate → copy. **No fs code runs.** | Identical. **Exact tie.** | Identical. **Exact tie** — spike-verified 0 upcalls. |
| **Cross-path dedup** (kernel caches once?) | **No.** cachefiles key = `(cookie, byte-range)`. Same chunk in two paths = two upcalls, two SSD extents. Dedup only in userspace moka (2nd upcall ~50 µs not ~5 ms). | **Yes** with optional kernel digest cache (§B.2): one upcall fills all waiters across files. v0 without it: same as A. **B wins iff V11 shows >5% intra-closure sharing AND we build the cache.** | **Yes, structurally** at file granularity. Two paths redirecting to `/ab/<digest>` open the **same inode** — one page-cache copy, one node-SSD copy, one fetch ever. No optional cache to build. |
| **Daemon-crash blast radius** | In-flight readers `D` on folio lock; next daemon writes `restore`, kernel re-delivers, readers unblock, **mounts survive, build continues**. Best-in-class. | Design choice. Cheap path: `.release` errors waiters → build `-EIO` → pod restart (~30 s lost). Match A: +~100 LoC re-dump-on-reopen. **A by default; tie if B spends the LoC.** | In-flight `open()` → `ENOTCONN` (FUSE), build `-EIO`. **No `D`-state** (FUSE waits are `S`, interruptible). Warm reads unaffected. Supervisor respawn → next open works. **Simpler than A's `restore` dance; same loss as B's cheap path.** |
| **Daemon-hang** | Reader `D` forever (no kernel timeout). Mitigate: per-spawn `tokio::timeout` → on expiry pwrite zeros + complete → build fails checksum. | Same problem, same mitigation. | `open()` blocks in `S`; per-spawn `tokio::timeout` → `EIO`. Same mitigation, **interruptible wait**. |
| **Debugging** | Userspace: `tracing`/`tokio-console`. Kernel: **upstream** `trace_events/{erofs,cachefiles,netfs,fscache}/*`; `bpftrace` works day-1; `/proc/fs/fscache/stats`. Hung task = upstream's bug. | Userspace: same. Kernel: **we write** `TRACE_EVENT(riofs_*)` (~50 LoC); then `bpftrace`/ftrace work. Oops/hung-folio = **our** vmcore: `crash`/`drgn`/`decode_stacktrace.sh`/KASAN. NixOS makes the KASAN-kernel VM-test cheap, but it's still our afternoon. | Userspace: `tracing` + FUSE upcall counters. Kernel: **upstream** `trace_events/{erofs,overlayfs,fuse}/*`. The whole hot path is page cache + overlay + EROFS — three of the most-exercised subsystems in container workloads. |
| **Upstream review/fuzz** | LKML-reviewed, syzkaller-covered, CVE-tracked (Gao Xiang, David Howells). | None unless we run it. syzkaller descriptors for `.riom` mount-blob + `/dev/riofs` proto ≈ ~200 LoC syz-lang (V10). | LKML-reviewed (overlayfs metacopy/redirect: Amir Goldstein; EROFS: Gao Xiang; composefs: Alexander Larsson). The exact stack ships in podman/ostree. |
| **API churn** | **uapi-frozen** (`cachefiles.h` is `include/uapi/`; EROFS on-disk is versioned). | **Internal API** — ~40% of releases touch a signature we implement. ~1 d/quarter under NixOS's deliberate-bump model. | **uapi-frozen** (overlayfs mount opts + `user.overlay.*` xattrs, EROFS on-disk, FUSE protocol, `fsopen`/`fsmount` syscalls). composefs-dump(5) is versioned. |
| **Kernel config (NixOS)** | 5-line `extraStructuredConfig`. | ~30-line `nix/kmod/riofs.nix` + `extraModulePackages`. Both trivial. | **None required** beyond `=y` over `=m` (EROFS/OVERLAY/FUSE all stock-on). |
| **Time to first prototype** | **~3 wk.** Wk1: Kconfig + vendored cachefiles loop + 1-path bootstrap. Wk2: in-process encoder + golden tests via loop-device mount (`EROFS_FS_BACKED_BY_FILE` is on everywhere — can validate encoder without fscache). Wk3: merge + multi-path + overlay flip. | **~4 wk.** Wk1: romfs-clone, static tree, mount+overlay+stat works. Wk2: miscdev ring + `read_folio` + Rust stub. Wk3: `.riom` serializer + `readahead` + first real build under VM-test. Wk4: KASAN soak + first oops + fix. | **~2 wk.** Wk1: `file_digest` in NarIndex + dump serializer + digest-FUSE (subset of existing `fuse/ops.rs`). Wk2: mount wiring + first real build + node-SSD digest cache. **Spike already has the VM harness** (§C.5). |
| **Exit cost** | Delete daemon + encoder; `boot/*` are dead S3 → GC sweeps; revert `extraStructuredConfig`. **Low.** | Delete `nix/kmod/` + `extraModulePackages` line. **No persistent data.** **Marginally lower.** | Delete digest-FUSE + dump serializer. **No persistent data, no Kconfig.** **Lowest.** |

### 4.1 What the table doesn't capture

**A's complexity is *adapter* complexity** — NAR→EROFS, FastCDC→block-aligned, our-namespace→fscache's. None of it is hard; all of it is fiddly; all of it is exhaustively unit-testable in userspace (`proptest`: NAR → encode → loop-mount → diff against `nix-store --restore`).

**B's complexity is *systems* complexity** — 800 LoC of folio-lock/completion/copy_from_user where bugs are oopses. It's testable (KUnit + KASAN VM), but the inner loop is 2-3 min not 5 s.

**B is a smaller *runtime* system.** No S3 artifact, no merge step, no fscache cookie/volume/culling state machine, no encoder, no rio-store change. One blob, one device, one message type. If you drew the box diagram, B has fewer boxes.

**A is a smaller *owned-risk* system.** Zero kernel LoC. The boxes A adds are upstream's boxes — syzkaller'd, CVE-tracked. When `netfs` refactors (it does, ~yearly), Gao Xiang fixes EROFS, not us. When B's `read_folio` deadlocks under a memory-pressure race we didn't anticipate, the entire planet's expert population is "whoever wrote it."

**B's chunk-native protocol is elegant but cheap to forgo.** A's reverse-map is ~150 LoC of binary search; the over-fetch is <512 KiB against a moka cache that already holds whole chunks. The wall-clock cost of A's impedance mismatch is microseconds per cold miss against millisecond network RTTs.

**C trades nothing once streaming-open is in.** A and B can serve a 4 MB hot range of a 200 MB file without fetching the other 196 MB; C with streaming-open (§C.7) returns `open()` after the first chunk (~10 ms) and demand-fetches only touched ranges thereafter — effectively range-granular above `STREAM_THRESHOLD`. C drops the cachefiles daemon, the device-slot ceiling, the eager-OPEN mount cost, the reverse-map, and the rio-store S3 artifact — and gains structural per-file dedup that A cannot achieve and B only achieves with an optional kernel cache. The node-local digest cache amortizes C's remaining over-fetch (≤8 MiB/file below the threshold) to once per unique file per node lifetime.

---

## 5. Recommendation

> **Superseded 2026-04-05.** The original recommendation below chose A over B without evaluating C. Spike evidence (§C.5) shows C dominates A on mount latency (<10 ms vs ~70 ms), matches A on the warm path, achieves kernel-side per-file dedup A structurally cannot, and is ~half the owned LoC with no cachefiles daemon and no rio-store S3 artifact. **New decision: Path C**, with A retained as the documented fallback if overlay-on-FUSE-data-only-lower exhibits an unforeseen production issue. C's known cost — whole-file cold `open()` of giants — is resolved by streaming-open (§C.7), which ships unconditionally based on `top1000.csv` + access-pattern evidence.

### 5.0 Original recommendation (retained for the record)

**With NixOS neutralizing deployment, this is close — but A (EROFS+fscache) remains the recommendation, with B as a credible 2-week parallel spike if the team has kernel-C appetite.**

The PLAN-KERNEL-FS.md decision matrix said: "if we're building a custom AMI regardless, B's end-to-end simplicity is the smaller system." That reading is *correct about runtime simplicity* but undersells three things:

1. **The "zero owned kernel LoC" property is worth more than ~900 LoC of userspace adapter.** A's 2 700 LoC are testable with `cargo nextest` in 5 s and debuggable with `RUST_LOG=trace`. B's 2 800 kernel LoC are testable in a 2-3 min VM loop and debuggable with `drgn` against a vmcore. For an org without standing kernel expertise, the second is a different *kind* of cost — not bigger, but spikier and harder to schedule. The I-055 breaker cascade and I-043 overlayfs negative-dentry incidents both took days because the failure was below the daemon; B puts ~800 LoC of *our* novel code in that same below-the-daemon stratum.

2. **B's headline wins are small in wall-clock.** No-over-fetch, no-reverse-map, kernel-side-dedup, no-merge-step: every one of these is real, and every one of these is ≤2% of cold-miss latency (network-bound) or ≤200 ms of build-start (amortizable by caching merged bootstraps per closure-hash). B is *more elegant*; it is not measurably *faster* on the metrics that gate phase-2 (`first_open_seconds{≥16MB}` p99).

3. **A's costs are front-loaded; B's recur.** A's encoder + merge are write-once. B's VFS-churn touch-ups + "we are the only debuggers of `riofs` oopses" + syzkaller-harness ownership are forever. Under NixOS the per-incident cost of B's recurrences is lower than under Bottlerocket (better dev loop), but the incident *count* is the same.

**Where B legitimately wins** and should be revisited:

- **V4 fails:** bootstrap merge >1 s on the chromium closure and per-closure-hash caching doesn't amortize it. B's `.riom` serialize is ~10 ms unconditionally.
- **V11 shows dense sharing:** intra-closure FastCDC chunk reuse >15% (e.g. many `lib*.a` with shared object files) — B's kernel digest-cache turns N upcalls into 1; A pays N context switches.
- **rio-store write-path coupling proves painful:** the encoder + migration + backfill + S3 object class is the only piece of A that touches a *stateful* service. If PutPath latency or GC complexity grows uncomfortably, B's "zero rio-store change" becomes decisive.
- **Team composition:** if there's standing kernel-C experience, B's spiky-debugging cost shrinks and its runtime-simplicity wins.

**Concrete plan:**

1. **Week 0 (parallel, cheap):** answer V4 (`nydus-image merge` 3 000 captured bootstraps — or hand-roll the splice and time it) and V11 (walk a chromium closure's manifests, count `Σ per-file chunk refs ÷ distinct chunks`). One day. If V4 >1 s **and** V11 >15%, flip to B.
2. **Weeks 1-4: build A** per §A.7 → §A.1 → §A.3 → overlay flip behind `RIO_STORE_BACKEND=erofs`. The encoder validates against loop-device mount (`EROFS_FS_BACKED_BY_FILE=y` is on in stock nixpkgs) before fscache is even in play.
3. **Optional weeks 1-2 in parallel: B spike** — `fs/romfs`-clone with `read_folio` posting to a miscdevice, in C, under `nix/kmod/` + a `nix/tests/riofs-smoke.nix` VM test with KASAN. If it mounts and serves one file in 2 wk with no KASAN splats, B's risk estimate drops and the week-4 decision has real data on both sides.
4. **Keep FUSE as the fallback** behind the existing flag throughout — all three share [`fetch.rs`](rio-builder/src/fuse/fetch.rs).

---

## 6. Sources

Primary (read for this report):
- [`include/uapi/linux/cachefiles.h`](https://github.com/torvalds/linux/blob/master/include/uapi/linux/cachefiles.h) — wire format (verified locally: `/nix/store/*/linux-headers-6.18.7/include/linux/cachefiles.h`)
- [`fs/erofs/erofs_fs.h`](https://github.com/torvalds/linux/blob/master/fs/erofs/erofs_fs.h) — `erofs_inode_chunk_index`, `EROFS_CHUNK_FORMAT_*`
- [`fs/erofs/super.c:660-671`](https://github.com/torvalds/linux/blob/master/fs/erofs/super.c) — `s_stack_depth` handling
- [`fs/erofs/fscache.c`](https://github.com/torvalds/linux/blob/master/fs/erofs/fscache.c) (664 LoC), [`fs/cachefiles/ondemand.c`](https://github.com/torvalds/linux/blob/master/fs/cachefiles/ondemand.c) (761 LoC)
- [`include/linux/fs.h`](https://github.com/torvalds/linux/blob/master/include/linux/fs.h) — current aops/iops/fops tables
- [`dragonflyoss/nydus service/src/fs_cache.rs`](https://github.com/dragonflyoss/nydus/blob/master/service/src/fs_cache.rs) — Apache-2.0 daemon; `bind ondemand`/`restore`/`copen`
- [`bottlerocket-os/bottlerocket-kernel-kit packages/kernel-6.{12,18}/config-full-bottlerocket-x86_64`](https://github.com/bottlerocket-os/bottlerocket-kernel-kit) — V1 (informational; NixOS supersedes)
- [`torvalds/linux rust/kernel/`](https://github.com/torvalds/linux/tree/master/rust/kernel) listing — V8: `fs/` = `file.rs` + `kiocb.rs` only; `miscdevice.rs` ✓
- [`Documentation/filesystems/erofs.rst`](https://erofs.docs.kernel.org), [`Documentation/filesystems/caching/cachefiles.rst`](https://www.kernel.org/doc/html/latest/filesystems/caching/cachefiles.html)

Our code:
- `/home/bemeurer/src/rio-build/main/rio-store/src/grpc/put_path.rs`, `cas.rs`, `chunker.rs`, `manifest.rs`
- `/home/bemeurer/src/rio-build/main/rio-proto/proto/types.proto`
- `/home/bemeurer/src/rio-build/main/rio-builder/src/fuse/{mod,ops,fetch}.rs`, `overlay.rs`

Path C primary (read for §3a):
- [`containers/composefs`](https://github.com/containers/composefs) — mechanism, `mkcomposefs`, [`composefs-dump(5)`](https://github.com/containers/composefs/blob/main/man/composefs-dump.md), [`lcfs-writer-erofs.c`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-writer-erofs.c)
- [`Documentation/filesystems/overlayfs.rst` §Data-only lower layers, §Metadata only copy up](https://docs.kernel.org/filesystems/overlayfs.html)
- [`fs/overlayfs/util.c` `ovl_get_redirect_xattr`/`ovl_check_metacopy_xattr`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/util.c) — `struct ovl_metacopy` shape
- [`fs/overlayfs/namei.c:1237-1247`](https://github.com/torvalds/linux/blob/master/fs/overlayfs/namei.c) + [`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9) — data-only-lower redirect gate under `userxattr` (`ofs->numdatalayer > 0`, not `config->metacopy`)
- [snix castore data model](https://snix.dev/docs/components/castore/data-model/) — the per-file merkle that motivated evaluating C
- [`libcomposefs/lcfs-internal.h:37-50`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-internal.h) — `OVERLAY_XATTR_PREFIX` hardcoded `trusted.`
- Spikes (consolidated on `adr-022`): core/scale/stream `15a9db79` ([`composefs-spike.nix`](../../nix/tests/scenarios/composefs-spike.nix), [`-scale.nix`](../../nix/tests/scenarios/composefs-spike-scale.nix), [`-stream.nix`](../../nix/tests/scenarios/composefs-spike-stream.nix), [`spike_digest_fuse.rs`](../../rio-builder/src/bin/spike_digest_fuse.rs), [`spike_stream_fuse.rs`](../../rio-builder/src/bin/spike_stream_fuse.rs)); privilege boundary `af8db499` ([`-priv.nix`](../../nix/tests/scenarios/composefs-spike-priv.nix), [`spike_mountd.rs`](../../rio-builder/src/bin/spike_mountd.rs)); access patterns `42aa81b2` ([`spike-access-data/RESULTS.md`](../../nix/tests/lib/spike-access-data/RESULTS.md)); kvm hostPath `9492019c` ([`kvm-hostpath-spike.nix`](../../nix/tests/scenarios/kvm-hostpath-spike.nix))
- `~/src/nix-index/main/top1000.csv` — 1000 largest files in nixpkgs (P0575 sizing)

Background:
- cachefiles failover [`c8383054506c`](https://git.kernel.org/linus/c8383054506c) (6.4)
- [`twoliter`](https://github.com/bottlerocket-os/twoliter) (informational)

---

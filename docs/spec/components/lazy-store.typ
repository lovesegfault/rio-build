#import "/lib/rio.typ": *

#show: rio.with(
  domains: ("store.fs",),
  paper: (
    title: [Lazy Store Filesystem],
    supertitle: "ARCHITECTURE DECISION RECORD",
    status: "Accepted (Path A — EROFS + fscache)",
    date: "2026-02",
  ),
)

#info(title: [Scope])[
  Deep technical comparison of the two phase-2 candidates from
  `PLAN-KERNEL-FS.md` §2.1/§2.2/§2.6. V1/V8/V9 are now answered with primary
  sources. Builder nodes are moving to a NixOS-based AMI regardless of A vs B,
  which removes deployment as a tiebreaker — both candidates are first-party
  config in an AMI we already own. The rest of this document compares on pure
  technical merit: code owned, latency, dedup, blast radius, debugging, churn,
  time-to-prototype, exit cost.
]

= Deployment context — settled, no longer a tiebreaker

#figure(
  caption: [Deployment posture under Bottlerocket (was) vs NixOS (now).],
  table(
    columns: (auto, 1fr, 1fr),
    align: (left, left, left),
    table.header([], [*Under Bottlerocket (was)*], [*Under NixOS (now)*]),
    [*A* Kconfig],
    [
      `_ONDEMAND` symbols compiled out (confirmed:
      #link(
        "https://github.com/bottlerocket-os/bottlerocket-kernel-kit",
      )[`bottlerocket-kernel-kit`]
      `kernel-6.{12,18}/config-full-bottlerocket-x86_64` has
      `# CONFIG_EROFS_FS_ONDEMAND is not set`,
      `# CONFIG_CACHEFILES_ONDEMAND is not set`). Would need 2-line upstream PR
      or custom variant.
    ],
    [
      `boot.kernelPatches = [{ name = "erofs-ondemand"; patch = null; extraStructuredConfig = { EROFS_FS_ONDEMAND = yes; CACHEFILES_ONDEMAND = yes; }; }];`
      — five lines in our AMI flake.
    ],

    [*B* module load],
    [
      `lockdown=integrity` default
      (#link("https://github.com/bottlerocket-os/bottlerocket/issues/813")[bottlerocket\#813])
      rejects unsigned `.ko`; kmod-kit doesn't ship the signing key. Would need
      `lockdown=none` (security regression) or custom variant with signed
      module.
    ],
    [
      `boot.extraModulePackages = [ config.boot.kernelPackages.riofs ];` where
      `riofs` is a `nix/kmod/riofs.nix` derivation. NixOS doesn't enable
      lockdown by default; if we choose to, we control the keyring.
    ],

    [*Net*],
    [Asymmetric — A had a path back to stock; B never did.],
    [*Symmetric* — both are first-party config in an AMI we already own.],
  ),
)

For the record, the host's own NixOS 6.19.9 also ships `_ONDEMAND` off — these
symbols are off in essentially every distro defconfig. Both candidates need a
kernel config we control; with NixOS, we have one.

= What both deliver that FUSE cannot

The unreachable-via-@fuse property: *a warm range of a partially-materialized
file is read with zero userspace crossings.* FUSE passthrough binds one backing
fd at `open()`; a 200 MB `libLLVM.so` with 4 MB of hot `.rodata` either upcalls
on every read until fully fetched or blocks `open()` for \~1.3 s. Both A and B
serve that 4 MB from page cache after first touch with the other 196 MB
unfetched. Both also make `lookup`/`stat`/`readdir` kernel-native.

= Candidate A — EROFS + fscache on-demand

== rio-store write-path changes

Today (#(refs.gh)("rio-store/src/grpc/put_path/mod.rs")):

```text
PutPath stream → buffer NAR (Vec<u8>) → SHA-256 verify → if ≥INLINE_THRESHOLD:
  cas::put_chunked(): chunker::chunk_nar(&nar)  # FastCDC 16/64/256 KiB over raw NAR bytes
    → upsert chunk rows (PG) → parallel S3 PUT new chunks → manifest row (ChunkRef[]) status=complete
```

No per-file index, no @nar parse on the write path. For EROFS, step 6 grows a
third action *after* `put_chunked` succeeds:

```rust
// nar_data is still in memory, already SHA-verified.
let tree = rio_nix::nar::parse(&nar_data)?;                 // NarEntry tree (rio-nix already has this)
let boot = erofs::Bootstrap::from_nar(&tree, &chunk_manifest, BLK_64K)?;  // §A.2
backend.put(&format!("boot/{nar_hash}.erofs"), boot.bytes())?;
metadata::set_bootstrap(&pool, &store_path_hash, boot.len())?;            // new manifests.boot_size column
```

*LoC:* \~800 in `rio-store/src/erofs.rs` (on-disk encoder — see §A.2), \~100 in
`put_path.rs`/`put_path_batch.rs`, \~50 in `metadata/queries.rs`, one migration
(`manifests.boot_size BIGINT NULL`). *No `mkfs.erofs` shell-out* — it wants a
directory on disk; we have a NAR in RAM.

*When:* at PutPath. The NAR is already buffered
(#(refs.gh)("rio-store/src/grpc/put_path/mod.rs") `nar_data: Vec<u8>`); one
more walk is \~free. Lazy generation would re-download chunks → reassemble →
parse, which is the I-110 burst we built batching to avoid. *Backfill* for
existing manifests: one-shot `xtask backfill-erofs-boot` (walk
`manifests WHERE boot_size IS NULL` → GetPath → encode → upload).

*Where:* S3 sibling to chunks, `s3://…/boot/<narhash>.erofs`. GC deletes it
when the manifest row goes (1:1 with its manifest, never shared).

== EROFS image structure — bootstrap vs blobs; can blobs BE our chunks? <a-2>

EROFS regular-file data has two layouts
(#link("https://github.com/torvalds/linux/blob/master/fs/erofs/erofs_fs.h")[`fs/erofs/erofs_fs.h`]):

- *Flat:* contiguous blocks at `startblk_lo` in the primary device.
- *Chunk-indexed* (`EROFS_CHUNK_FORMAT_INDEXES`, 5.15+): per-file array of
  8-byte
  `struct erofs_inode_chunk_index { __le16 startblk_hi; __le16 device_id; __le32 startblk_lo; }`.
  `device_id` selects one of ≤65 535 "extra devices" (= blobs) named in the
  superblock device table; `startblk` is a *block-aligned* offset within that
  blob. Chunk size = `block_size << blkbits`, power-of-2, 4 KiB–1 MiB.

A *bootstrap* (Nydus term; "meta blob") = small EROFS image with superblock +
inodes + dirents + per-file chunk-index arrays + device table, *no file data*.
EROFS asks fscache for blob data by `(cookie = device-slot tag, off, len)`.

*Can data blobs BE our @fastcdc chunks?* *No*, two structural reasons:

+ *Alignment.* `startblk` is a block number. EROFS chunks are `4KiB × 2ⁿ`;
  FastCDC chunks (#(refs.gh)("rio-store/src/chunker.rs:32")) are 16–256 KiB at
  content-defined byte boundaries. A FastCDC cut at NAR byte 17 313 is
  unrepresentable.
+ *Cardinality.* `device_id` is `__le16` → 65 535 blobs max. A chromium @closure
  ≈ 600 000 FastCDC chunks.

*The mapping that works* (= what RAFS v6 does): one *logical blob per store
path*, content = "concatenation of this NAR's regular-file payloads, NAR-walk
order, zero-padded to block boundaries." The blob _never exists in S3_ — it's
just a `cookie_key` string. When the daemon gets `READ{cookie, off, len}`:

```text
(off,len) in logical-blob space
  → which file?                            (per-path file-offset table; phase-1 builds this)
  → where in NAR byte stream?              (file.nar_offset + (off − file.blob_offset))
  → which FastCDC chunks cover that range? (binsearch ChunkRef cumsum; ≤2 boundary + N interior)
  → GetChunk×k → assemble [off,off+len) → pwrite(anon_fd) → ioctl COMPLETE
```

Worst-case over-fetch: `2 × CHUNK_MAX − len` ≈ *\<512 KiB* per cold miss.
Interior chunks land whole in moka.

`mkfs.erofs --blobdev` / RAFS v6: `mkfs.erofs --chunksize=65536 --blobdev=X`
builds the chunk-indexed layout from a _directory_, writing data to `X` and
metadata to the image. RAFS v6 = EROFS-on-disk + a feature flag + Nydus xattrs.
We need vanilla chunk-indexed EROFS, encoded in-process from an in-memory NAR;
#link("https://github.com/dragonflyoss/nydus/tree/master/rafs")[`nydus-rafs`]
is the Apache-2.0 Rust reference.

== Builder mount sequence

Replacing #(refs.gh)("rio-builder/src/fuse/mod.rs:494")
`mount_fuse_background()`:

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

*Serving the bootstrap.* EROFS's first act post-`mount()` is reading its own
superblock — via fscache. The OPEN handler must recognize
`cookie_key == "rio-boot-<id>"` and serve the local merged-bootstrap file
(`pread` from SSD).

*Build-start merge* (before mount). Extend `ManifestHint`
(#(refs.gh)("rio-proto/proto/types.proto:207")) with
`optional bytes boot_blob = 4` (\~1–10 KB each; \~15 MB for 3 000 paths).
`erofs::merge(&boots)`: one root dir with N store-path children, splice each
subtree, union device tables, renumber `device_id`s, rewrite chunk indices.
\~400 LoC;
#link("https://github.com/dragonflyoss/nydus")[`nydus-image merge`] is the
reference. Critical-path latency = \~15 MB batched download (\~100 ms
cluster-net) + in-memory splice (V4 target \<200 ms for 300 k inodes).

== `/dev/cachefiles` upcall protocol — exact wire format

From local 6.18 uapi
#link(
  "https://github.com/torvalds/linux/blob/master/include/uapi/linux/cachefiles.h",
)[`include/uapi/linux/cachefiles.h`]
— *frozen userspace ABI*:

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

#figure(
  caption: [`/dev/cachefiles` request/reply forms.],
  table(
    columns: (auto, 1fr),
    align: (left, left),
    table.header([*Req*], [*Reply*]),
    [`OP_OPEN`],
    [`write(dev_fd, "copen <msg_id>,<size_or_-errno>")` — text, on the *device*
      fd],

    [`OP_READ`],
    [`pwrite(anon_fd, data, len, off)` then
      `ioctl(anon_fd, CACHEFILES_IOC_READ_COMPLETE, msg_id)` — on the *anon*
      fd],

    [`OP_CLOSE`], [none; `close(anon_fd)`, drop `object_id`],
  ),
)

*rio-builder daemon* (`tokio::io::unix::AsyncFd`, not `mio` like Nydus, to
share the runtime with #(refs.gh)("rio-builder/src/fuse/fetch/")):

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

`/dev/cachefiles` is single-reader; concurrency comes from spawn-per-READ.
Nydus does the same with a thread pool
(#link("https://github.com/dragonflyoss/nydus/blob/master/service/src/fs_cache.rs")[`fs_cache.rs`]).

== overlayfs stacking

EROFS sets `sb->s_stack_depth = 0`
(#link("https://github.com/torvalds/linux/blob/master/fs/erofs/super.c")[`fs/erofs/super.c:660-671`]
only checks the _backing file_'s depth in file-backed mode; fscache mode has no
backing-file fs). `overlay(upper=tmpfs, lower=erofs-on-fscache)` is depth 1 —
*frees* the slot we currently spend on FUSE's `max_stack_depth=1`. composefs
ships exactly this stack in production.

== Failure modes <a-6>

#figure(
  caption: [Candidate-A failure modes and rio handling.],
  table(
    columns: (auto, 1fr, 1fr),
    align: (left, left, left),
    table.header([*Failure*], [*Kernel behavior*], [*rio handling*]),
    [*Daemon crash mid-READ*],
    [Reader in `D` on `folio_wait_bit`. On `/dev/cachefiles` close, requests
      stay queued (since 6.4,
      #link("https://git.kernel.org/linus/c8383054506c")[`c8383054506c`]). Next
      daemon writes `"restore"` instead of `"bind ondemand"` → kernel
      re-delivers pending → readers unblock. *Mounts survive.*],
    [Supervisor respawns task with `restore=true`. No I-055-class ENOTCONN
      storm.
      (#link("https://github.com/dragonflyoss/nydus/blob/master/service/src/fs_cache.rs")[`fs_cache.rs:269-315`]
      reference impl.)],

    [*Daemon hung*],
    [Reader in `D` indefinitely (no kernel timeout).],
    [Per-spawn `tokio::time::timeout` (same as today's `jit_fetch_timeout`); on
      timeout `pwrite` zeros + ioctl so build fails its checksum loudly rather
      than wedging the node.],

    [*`copen -ENOENT`*],
    [EROFS read fails `-EIO`.],
    [Correct — we don't have the path.],

    [*Backing store full*],
    [cachefiles culling (`bcull`/`brun` watermarks) evicts cold objects.],
    [Give cachefiles the SSD budget; retire our `cache_dir` LRU on this path.],
  ),
)

== Kconfig (NixOS)

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

= Candidate B — custom `riofs` kernel module

== Mount-blob format

The builder hands the kernel a serialized index file path (or fd via
`fsconfig(FSCONFIG_SET_FD, "index", …, idx_fd)` — cleaner). `fill_super`
`kernel_read()`s it into a `kvmalloc`'d buffer and parses fixed-LE structs:

```c
struct riom_header { u8 magic[8]; u32 n_paths, n_inodes, n_chunks, strtab_len; };
struct riom_path   { u32 name_off; u32 root_ino; };
struct riom_inode  { u32 ino, parent, name_off; u16 mode, kind; u64 size;
                     u32 first_extent; u16 n_extents; u16 _pad; };       // DIR: first_child/n_children; LNK: target_off
struct riom_extent { u64 file_off; u32 chunk_idx; u32 chunk_off; u32 len; u32 _pad; };
// then: u8 chunk_digest[n_chunks][32]; u8 strtab[strtab_len];
```

For 3 000 paths × 100 files × 3 extents + 200 k digests ≈ *42 MB*, held for
mount lifetime. Generated in-process by rio-builder from `BatchGetManifest`'s
`ManifestHint`s — *no rio-store change, no S3 artifact*.

*Hardening:* every offset bounds-checked in `fill_super`; on failure `-EUCLEAN`
and refuse mount. The bytes derive from NAR contents (filenames, sizes) → a
malicious NAR could try OOB offsets. Same parser-hardening as
`fs/erofs/super.c` but without LKML review.

== Upcall protocol <b-2>

*Transport: `miscdevice` (`/dev/riofs`).* Direct precedent in `/dev/fuse`,
`/dev/cachefiles`; \~200 LoC ring on `kfifo`/`xarray`; mainline
`kernel::miscdevice` Rust binding exists. Netlink is overkill; io_uring
(`.uring_cmd`) is a later optimization (upcall is network-bound, not
syscall-bound).

```c
struct riofs_req       { u32 req_id; u32 _pad; u8 digest[32]; };           // K→U via read()
struct riofs_reply_hdr { u32 req_id; i32 err; u32 len; u32 _pad; /* u8 data[len] */ };  // U→K via write()
```

*Chunk-addressed, not byte-addressed* — the entire point of B over A. Userspace
is \~50 LoC around `clients.get_chunk(digest)`. No reverse-map, no over-fetch.

*Kernel cold path:*

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

`riofs_dev_write` parses reply, `kvmalloc`s chunk, `copy_from_user`, stores in
`rhashtable` keyed by `chunk_idx`, `complete_all()` — every folio waiting on
that digest wakes. *This is the cross-file dedup A can't do in-kernel.* Chunk
eviction: `register_shrinker()` LRU, or write-through to a per-inode sparse
backing file (`kernel_write` under `cache_dir`). For v0: no kernel chunk cache,
page cache + userspace moka only.

== VFS surface — every op

From mainline
#link("https://github.com/torvalds/linux/blob/master/include/linux/fs.h")[`include/linux/fs.h`]:

#figure(
  caption: [VFS ops table for `riofs`. Novel code: `read_folio` + `readahead` +
    miscdev ring + `fill_super` parser. Everything else is
    `generic_*`/`simple_*` forwards. *2.5–3.5 kLoC C.*],
  table(
    columns: (auto, auto, auto, 1fr),
    align: (left, left, center, left),
    table.header([*Table*], [*Member*], [*riofs*], [*Notes*]),
    [`file_system_type`],
    [`.name`, `.init_fs_context`, `.kill_sb`],
    [*req*],
    [`kill_anon_super` + free index. `.fs_flags = 0` (no `FS_USERNS_MOUNT`).],

    [`fs_context_operations`],
    [`.parse_param`, `.get_tree`, `.free`],
    [*req*],
    [`get_tree_nodev(fc, riofs_fill_super)`],

    [`super_operations`],
    [`.alloc_inode`/`.free_inode`],
    [*req*],
    [embed `riofs_inode_info`],

    [],
    [`.statfs`, `.put_super`],
    [*req*],
    [`simple_statfs`; free index,
      unregister miscdev],

    [], [`.show_options`], [nice], [echo `index=`],
    [`inode_operations` (dir)],
    [`.lookup`, `.getattr`],
    [*req*],
    [binsearch children; `d_splice_alias`],

    [`inode_operations` (reg)], [`.getattr`], [*req*], [`generic_fillattr`],
    [`inode_operations` (lnk)],
    [`.get_link`, `.getattr`],
    [*req*],
    [return strtab ptr],

    [], [`.listxattr`], [optional], [NARs can carry `security.capability`],
    [`file_operations` (dir)],
    [`.iterate_shared`, `.llseek`],
    [*req*],
    [walk children],

    [`file_operations` (reg)],
    [`.read_iter` = *`generic_file_read_iter`*],
    [*req*],
    [page-cache-backed; this is the trick],

    [],
    [`.mmap` = `generic_file_readonly_mmap`],
    [*req*],
    [`ld.so` mmaps
      `.so`],

    [],
    [`.llseek`, `.splice_read`],
    [std],
    [`generic_file_llseek`,
      `filemap_splice_read`],

    [*`address_space_operations`*],
    [`.read_folio`],
    [*req*],
    [@b-2 — the novel code],

    [],
    [`.readahead`],
    [*strongly rec*],
    [batch N folios → post all chunks →
      one wake. \~80 LoC.],

    [], [`.migrate_folio`], [std], [`filemap_migrate_folio`],
    [], [everything else], [*omit*], [RO fs],
    [miscdev `file_operations`],
    [`.read_iter`, `.write_iter`, `.poll`, `.open`, `.release`],
    [*req*],
    [`.release`: re-dump pending on next open (cachefiles-style failover) ≈
      +100 LoC],
  ),
)

== Rust-for-Linux status (V8, answered)

Surveyed `torvalds/linux` master `rust/kernel/`:

#figure(
  caption: [Rust-for-Linux abstraction availability vs `riofs` needs.],
  table(
    columns: (1fr, auto, auto),
    align: (left, center, left),
    table.header([*Abstraction*], [*Mainline*], [*riofs use*]),
    [`module!{}`, `Arc/Mutex/SpinLock/CondVar`, `KBox/KVec/KVVec`,
      `workqueue`],
    [✓],
    [✓],

    [*`miscdevice::MiscDevice`*], [*✓*], [*`/dev/riofs` 100% safe Rust*],
    [`uaccess::UserSlice{Reader,Writer}`, `page::Page`], [✓], [✓ partial],
    [*`kernel::fs::*`*], [*only `file.rs` + `kiocb.rs`*], [*insufficient*],
    [`FileSystem`/`SuperBlock`/`INode`/`inode_operations` traits],
    [✗ mainline; rust-vfs branch only],
    [carry \~1.5 kLoC out-of-tree],

    [*`address_space_operations` / `Folio` API*],
    [*✗ everywhere*],
    [*`unsafe extern "C"` only*],

    [`rhashtable`/`completion`/`kfifo`], [✗], [raw `bindings::*`],
  ),
)

Mainline `rust/kernel/fs/` has *two files*. A safe-Rust `read_folio` does not
exist. Rust riofs = \~1 200 safe + \~350 unsafe FFI shim + *\~1 500 carried*
rust-vfs patches, perpetually rebased. *If B is chosen: write it in C.*
`smatch`+`sparse`+KASAN on 3 kLoC catch the same bug classes; `fs/romfs` is a
near-verbatim template. Revisit Rust when `rust/kernel/fs/` has >2 files.

== Build/ship — NixOS

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

Kernel-version-locked; rebuilds with `boot.kernelPackages`. KASAN dev variant:
`boot.kernelPackages = pkgs.linuxPackages_latest_hardened` or a
`structuredExtraConfig.KASAN = yes` overlay for the VM-test kernel only. *No
signing dance, no lockdown, no sdk container.* Dev loop:
`nix build .#nixosTests.riofs-smoke` (qemu VM with the module loaded) — same
machinery as existing `nix/tests/`. The "quarterly VFS churn" becomes "fix the
build when bumping nixpkgs," same class of work as any other dependency.

== VFS API churn — concrete history

`address_space_operations`-relevant, last 10 releases:

#figure(
  caption: [Kernel VFS API churn affecting a hypothetical `riofs`. *4 of 10*
    releases would have needed a non-trivial patch → \~every other kernel bump
    (\~5 mo). Each is 1–4 h mechanical _if_ tracking LKML; +1 d bisect if
    discovered via build break.],
  table(
    columns: (auto, 1fr, auto),
    align: (left, left, left),
    table.header([*Ver*], [*Change*], [*riofs hit?*]),
    [5.18],
    [`readpage`→`read_folio`; `page*`→`folio*` everywhere],
    [*rewrite signature + body*],

    [5.19], [`readpages` removed], [port if used],
    [6.0], [`migratepage`→`migrate_folio`], [one-liner],
    [6.3], [`getattr` gained `mnt_idmap*` first arg], [*3 signatures*],
    [6.8], [`error_remove_page`→`error_remove_folio`], [n/a],
    [6.12],
    [`writepage` removed; `write_begin/end` `file*`→`kiocb*`],
    [n/a (RO)],

    [ongoing],
    [iomap conversion pressure on simple RO fs],
    [*risk* if romfs/cramfs get converted],
  ),
)

Mitigated by: NixOS pins the kernel; bumps are deliberate; aops surface is
minimal.

= Head-to-head

#figure(
  caption: [Head-to-head comparison on every axis.],
  table(
    columns: (auto, 1fr, 1fr),
    align: (left, left, left),
    table.header([*Axis*], [*(A) EROFS + fscache*], [*(B) `riofs` kmod (C)*]),
    [*Total LoC owned*],
    [*\~2 700* = 0 kernel + \~1 200 daemon (poll loop, reverse-map, cookie idx)
      + \~950 rio-store (encoder + PutPath + migration) + \~400 builder merge +
      \~150 nix/helm. Plus \~400 LoC vendored Nydus protocol parsing
      (Apache-2.0, attributed).],
    [*\~3 600* = \~2 800 kernel C + \~500 builder (`/dev/riofs` loop + `.riom`
      serializer) + *0 rio-store* + \~100 `nix/kmod/` + \~200 VM-test
      scaffolding. (Rust path: +\~1 500 carried rust-vfs — don't.)],

    [*Distribution of complexity*],
    [All userspace; 100% `cargo nextest`-able; bugs = wrong bytes (build fails
      its checksum, loud). The fiddly part (reverse-map) is `proptest`-able.],
    [\~800 LoC genuinely-novel kernel (`read_folio` + ring + waiters); \~2 000
      romfs-shaped boilerplate. Bugs = hung folio lock, UAF on evicted chunk,
      `copy_from_user` length error. Dev loop = VM rebuild (\~2–3 min).],

    [*rio-store write-path Δ*],
    [+encoder, +PutPath hook, +migration, +S3 object class, +GC wiring,
      +backfill job.],
    [*None.*],

    [*Build-start latency added*],
    [\~15 MB boot-blob batch fetch + in-mem merge of \~300 k inodes (V4: target
      \<200 ms; cache merged result per-closure-hash on STS pods to amortize).],
    [`.riom` serialize from already-in-memory `ManifestHint`s — *\~10 ms*.],

    [*Persistent artifacts*],
    [`boot/<narhash>.erofs` per @store-path in S3 (\~0.3% of NAR size).
      GC-tracked.],
    [*None.*],

    [*Cold-miss latency*],
    [`read_folio` → netfs → fscache → cachefiles xarray → poll wake → user
      `read` → reverse-map (\~5 µs) → `GetChunk×k` (*\~2–8 ms*) → assemble →
      `pwrite` → ioctl → fill folio. ≈ *net + \~40 µs + ≤512 KiB over-fetch.*],
    [`read_folio` → kfifo push → poll wake → user `read` → `GetChunk×1`
      (*\~2–8 ms*) → `write` → `copy_from_user` → `memcpy_to_folio` →
      `complete_all`. ≈ *net + \~15 µs, no over-fetch.* B saves \~25 µs +
      ≤256 KiB/miss; *both dominated by network RTT — effective tie.*],

    [*Warm-read latency* (page cache hit)],
    [`filemap_read` → folio uptodate → copy. *No fs code runs.*],
    [Identical. *Exact tie.*],

    [*Cross-path chunk dedup*],
    [*No.* cachefiles key = `(cookie, byte-range)`. Same chunk in two paths =
      two upcalls, two SSD extents. Dedup only in userspace moka (2nd upcall
      \~50 µs not \~5 ms).],
    [*Yes* with optional kernel digest cache (@b-2): one upcall fills all
      waiters across files. v0 without it: same as A. *B wins iff V11 shows
      >5% intra-closure sharing AND we build the cache.*],

    [*Daemon-crash blast radius*],
    [In-flight readers `D` on folio lock; next daemon writes `restore`, kernel
      re-delivers, readers unblock, *mounts survive, build continues*.
      Best-in-class.],
    [Design choice. Cheap path: `.release` errors waiters → build `-EIO` → pod
      restart (\~30 s lost). Match A: +\~100 LoC re-dump-on-reopen. *A by
      default; tie if B spends the LoC.*],

    [*Daemon-hang*],
    [Reader `D` forever (no kernel timeout). Mitigate: per-spawn
      `tokio::timeout` → on expiry pwrite zeros + complete → build fails
      checksum.],
    [Same problem, same mitigation.],

    [*Debugging*],
    [Userspace: `tracing`/`tokio-console`. Kernel: *upstream*
      `trace_events/{erofs,cachefiles,netfs,fscache}/*`; `bpftrace` works
      day-1; `/proc/fs/fscache/stats`. Hung task = upstream's bug.],
    [Userspace: same. Kernel: *we write* `TRACE_EVENT(riofs_*)` (\~50 LoC);
      then `bpftrace`/ftrace work. Oops/hung-folio = *our* vmcore:
      `crash`/`drgn`/`decode_stacktrace.sh`/KASAN. NixOS makes the
      KASAN-kernel VM-test cheap, but it's still our afternoon.],

    [*Upstream review/fuzz*],
    [LKML-reviewed, syzkaller-covered, CVE-tracked (Gao Xiang, David Howells).],
    [None unless we run it. syzkaller descriptors for `.riom` mount-blob +
      `/dev/riofs` proto ≈ \~200 LoC syz-lang (V10).],

    [*API churn*],
    [*uapi-frozen* (`cachefiles.h` is `include/uapi/`; EROFS on-disk is
      versioned).],
    [*Internal API* — \~40% of releases touch a signature we implement.
      \~1 d/quarter under NixOS's deliberate-bump model.],

    [*Kernel config (NixOS)*],
    [5-line `extraStructuredConfig`.],
    [\~30-line `nix/kmod/riofs.nix` + `extraModulePackages`. Both trivial.],

    [*Time to first prototype*],
    [*\~3 wk.* Wk1: Kconfig + vendored cachefiles loop + 1-path bootstrap. Wk2:
      in-process encoder + golden tests via loop-device mount
      (`EROFS_FS_BACKED_BY_FILE` is on everywhere — can validate encoder
      without fscache). Wk3: merge + multi-path + overlay flip.],
    [*\~4 wk* (C). Wk1: romfs-clone, static tree, mount+overlay+stat works.
      Wk2: miscdev ring + `read_folio` + Rust stub. Wk3: `.riom` serializer +
      `readahead` + first real build under VM-test. Wk4: KASAN soak + first
      oops + fix.],

    [*Exit cost*],
    [Delete daemon + encoder; `boot/*` are dead S3 → GC sweeps; revert
      `extraStructuredConfig`. *Low.*],
    [Delete `nix/kmod/` + `extraModulePackages` line. *No persistent data.*
      *Marginally lower.*],
  ),
)

== What the table doesn't capture

*A's complexity is _adapter_ complexity* — NAR→EROFS, FastCDC→block-aligned,
our-namespace→fscache's. None of it is hard; all of it is fiddly; all of it is
exhaustively unit-testable in userspace (`proptest`: NAR → encode → loop-mount
→ diff against `nix-store --restore`).

*B's complexity is _systems_ complexity* — 800 LoC of
folio-lock/completion/`copy_from_user` where bugs are oopses. It's testable
(KUnit + KASAN VM), but the inner loop is 2–3 min not 5 s.

*B is a smaller _runtime_ system.* No S3 artifact, no merge step, no fscache
cookie/volume/culling state machine, no encoder, no rio-store change. One blob,
one device, one message type. If you drew the box diagram, B has fewer boxes.

*A is a smaller _owned-risk_ system.* Zero kernel LoC. The boxes A adds are
upstream's boxes — syzkaller'd, CVE-tracked. When `netfs` refactors (it does,
\~yearly), Gao Xiang fixes EROFS, not us. When B's `read_folio` deadlocks under
a memory-pressure race we didn't anticipate, the entire planet's expert
population is "whoever wrote it."

*B's chunk-native protocol is elegant but cheap to forgo.* A's reverse-map is
\~150 LoC of binary search; the over-fetch is \<512 KiB against a moka cache
that already holds whole chunks. The wall-clock cost of A's impedance mismatch
is microseconds per cold miss against millisecond network RTTs.

= Recommendation

*With NixOS neutralizing deployment, this is close — but A (EROFS+fscache)
remains the recommendation, with B as a credible 2-week parallel spike if the
team has kernel-C appetite.*

The `PLAN-KERNEL-FS.md` decision matrix said: "if we're building a custom AMI
regardless, B's end-to-end simplicity is the smaller system." That reading is
_correct about runtime simplicity_ but undersells three things:

+ *The "zero owned kernel LoC" property is worth more than \~900 LoC of
  userspace adapter.* A's 2 700 LoC are testable with `cargo nextest` in 5 s
  and debuggable with `RUST_LOG=trace`. B's 2 800 kernel LoC are testable in a
  2–3 min VM loop and debuggable with `drgn` against a vmcore. For an org
  without standing kernel expertise, the second is a different _kind_ of cost —
  not bigger, but spikier and harder to schedule. The I-055 breaker cascade and
  I-043 @overlayfs negative-dentry incidents both took days because the failure
  was below the daemon; B puts \~800 LoC of _our_ novel code in that same
  below-the-daemon stratum.

+ *B's headline wins are small in wall-clock.* No-over-fetch, no-reverse-map,
  kernel-side-dedup, no-merge-step: every one of these is real, and every one
  of these is ≤2% of cold-miss latency (network-bound) or ≤200 ms of
  build-start (amortizable by caching merged bootstraps per closure-hash). B is
  _more elegant_; it is not measurably _faster_ on the metrics that gate
  phase-2 (`first_open_seconds{≥16MB}` p99).

+ *A's costs are front-loaded; B's recur.* A's encoder + merge are write-once.
  B's VFS-churn touch-ups + "we are the only debuggers of `riofs` oopses" +
  syzkaller-harness ownership are forever. Under NixOS the per-incident cost of
  B's recurrences is lower than under Bottlerocket (better dev loop), but the
  incident _count_ is the same.

*Where B legitimately wins* and should be revisited:

- *V4 fails:* bootstrap merge >1 s on the chromium closure and
  per-closure-hash caching doesn't amortize it. B's `.riom` serialize is
  \~10 ms unconditionally.
- *V11 shows dense sharing:* intra-closure FastCDC chunk reuse >15% (e.g. many
  `lib*.a` with shared object files) — B's kernel digest-cache turns N upcalls
  into 1; A pays N context switches.
- *rio-store write-path coupling proves painful:* the encoder + migration +
  backfill + S3 object class is the only piece of A that touches a _stateful_
  service. If PutPath latency or GC complexity grows uncomfortably, B's "zero
  rio-store change" becomes decisive.
- *Team composition:* if there's standing kernel-C experience, B's
  spiky-debugging cost shrinks and its runtime-simplicity wins.

*Concrete plan:*

+ *Week 0 (parallel, cheap):* answer V4 (`nydus-image merge` 3 000 captured
  bootstraps — or hand-roll the splice and time it) and V11 (walk a chromium
  closure's manifests, count `Σ per-file chunk refs ÷ distinct chunks`). One
  day. If V4 >1 s *and* V11 >15%, flip to B.
+ *Weeks 1–4: build A* per Kconfig → write-path → mount sequence → overlay flip
  behind `RIO_STORE_BACKEND=erofs`. The encoder validates against loop-device
  mount (`EROFS_FS_BACKED_BY_FILE=y` is on in stock nixpkgs) before fscache is
  even in play.
+ *Optional weeks 1–2 in parallel: B spike* — `fs/romfs`-clone with
  `read_folio` posting to a miscdevice, in C, under `nix/kmod/` + a
  `nix/tests/riofs-smoke.nix` VM test with KASAN. If it mounts and serves one
  file in 2 wk with no KASAN splats, B's risk estimate drops and the week-4
  decision has real data on both sides.
+ *Keep FUSE as the fallback* behind the existing flag throughout — all three
  share #(refs.gh)("rio-builder/src/fuse/fetch/").

= Rationale

== The hard part: cache-miss cascading failure // challenge #11

The lazy store filesystem makes every cold read a network call to rio-store.
When rio-store is overloaded or degraded, those reads become slow, and the
system enters a positive-feedback loop: rio-store slow → reads block → builds
stall → executors look degraded → scheduler reassigns → more executors hit
rio-store → amplification → controller scales up executors → more rio-store
load.

The mitigations are layered, and the kernel-filesystem choice doesn't change
them — both A and B funnel cold misses through the same userspace fetch path
(#(refs.gh)("rio-builder/src/fuse/fetch/")):

- *Fetch timeout.* `fetch_extract_insert` wraps the entire
  gRPC-fetch-plus-stream-drain in `GRPC_STREAM_TIMEOUT` (300 s). A stalled
  store returns `EIO` to the build rather than blocking a kernel thread
  forever. Concurrent-fetch waiters time out after 30 s and return `EAGAIN`.
  Under EROFS this is the per-spawn `tokio::time::timeout` in @a-6.
- *Scheduler cache-check circuit breaker.* The scheduler's `FindMissingPaths`
  trips open after 5 consecutive failures; while open, `SubmitBuild` is
  rejected with `StoreUnavailable` instead of queueing every derivation as a
  cache miss.
- *Scheduler #gls("backpressure").* Actor-queue-depth hysteresis (80% activate, 60%
  deactivate) refuses new submissions when the actor is overloaded.
- *Executor circuit breaker.* The executor tracks consecutive store-fetch
  failures; when the breaker opens, FUSE lookups fail fast with `EIO` and the
  build surfaces an infra-classed outcome instead of stalling on a degraded
  store (the stream-era `store_degraded` heartbeat flag retired with the
  heartbeat at the 1d builder collapse). See
  #rref("builder.fuse.circuit-breaker").

A's daemon-crash failover (@a-6) is strictly better than the FUSE baseline here:
mounts survive a daemon restart, so a transient rio-store blip that crashes the
upcall handler doesn't take the build down with it.

== The hard part: local I/O performance under a userspace daemon // challenge #13

This ADR exists because the FUSE baseline pays a kernel→userspace→kernel
crossing on every `read()` from the build sandbox, even when the data is
already on local SSD. The Phase-1a spike (EKS AL2023, kernel 6.12, c8a.xlarge)
measured 10–50× overhead vs direct reads at p50 across concurrency 1–16. FUSE
passthrough (`FUSE_PASSTHROUGH`, Linux 6.9+) didn't help the
open-read-close-per-file pattern because `lookup()`/`open()` dominate, not
`read()` — and passthrough binds one backing fd at `open()`, so a
partially-materialized file can't benefit at all.

Both candidates fix this the same way: warm reads come from the page cache via
`filemap_read` with *no fs code running*. `lookup`/`stat`/`readdir` are
kernel-native (EROFS or `riofs` inode tables, no upcall). The only userspace
crossing left is the cold miss, which is network-bound regardless. The 2×
direct-read threshold that would have triggered the bind-mount fallback is
cleared by an order of magnitude.

= Sources

Primary (read for this report):
- #link(
    "https://github.com/torvalds/linux/blob/master/include/uapi/linux/cachefiles.h",
  )[`include/uapi/linux/cachefiles.h`]
  — wire format
- #link(
    "https://github.com/torvalds/linux/blob/master/fs/erofs/erofs_fs.h",
  )[`fs/erofs/erofs_fs.h`]
  — `erofs_inode_chunk_index`, `EROFS_CHUNK_FORMAT_*`
- #link(
    "https://github.com/torvalds/linux/blob/master/fs/erofs/super.c",
  )[`fs/erofs/super.c:660-671`]
  — `s_stack_depth` handling
- #link(
    "https://github.com/torvalds/linux/blob/master/fs/erofs/fscache.c",
  )[`fs/erofs/fscache.c`]
  (664 LoC),
  #link(
    "https://github.com/torvalds/linux/blob/master/fs/cachefiles/ondemand.c",
  )[`fs/cachefiles/ondemand.c`]
  (761 LoC)
- #link(
    "https://github.com/torvalds/linux/blob/master/include/linux/fs.h",
  )[`include/linux/fs.h`]
  — current aops/iops/fops tables
- #link(
    "https://github.com/dragonflyoss/nydus/blob/master/service/src/fs_cache.rs",
  )[`dragonflyoss/nydus service/src/fs_cache.rs`]
  — Apache-2.0 daemon; `bind ondemand`/`restore`/`copen`
- #link(
    "https://github.com/bottlerocket-os/bottlerocket-kernel-kit",
  )[`bottlerocket-os/bottlerocket-kernel-kit`]
  — V1 (informational; NixOS supersedes)
- #link(
    "https://github.com/torvalds/linux/tree/master/rust/kernel",
  )[`torvalds/linux rust/kernel/`]
  listing — V8: `fs/` = `file.rs` + `kiocb.rs` only; `miscdevice.rs` ✓
- #link("https://erofs.docs.kernel.org")[`Documentation/filesystems/erofs.rst`],
  #link(
    "https://www.kernel.org/doc/html/latest/filesystems/caching/cachefiles.html",
  )[`Documentation/filesystems/caching/cachefiles.rst`]

Our code:
- #(refs.gh)("rio-store/src/grpc/put_path/"), #(refs.gh)("rio-store/src/cas.rs"),
  #(refs.gh)("rio-store/src/chunker.rs"), #(refs.gh)("rio-store/src/manifest.rs")
- #(refs.gh)("rio-proto/proto/types.proto")
- #(refs.gh)("rio-builder/src/fuse/mod.rs"),
  #(refs.gh)("rio-builder/src/fuse/ops.rs"),
  #(refs.gh)("rio-builder/src/fuse/fetch/"),
  #(refs.gh)("rio-builder/src/overlay.rs")

Background:
- #link("https://github.com/containers/composefs")[`containers/composefs`] —
  overlay-on-erofs-on-fscache in production
- cachefiles failover
  #link("https://git.kernel.org/linus/c8383054506c")[`c8383054506c`] (6.4)
- #link("https://github.com/bottlerocket-os/twoliter")[`twoliter`]
  (informational)

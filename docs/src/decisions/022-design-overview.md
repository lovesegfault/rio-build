# ADR-022 Design Overview — composefs-style lazy `/nix/store`

**Status:** canonical reference for the post-ADR-022 builder filesystem and store metadata model. Decision rationale and alternatives are in [ADR-022](./022-lazy-store-fs-erofs-vs-riofs.md); sequencing is in the [implementation plan](./022-implementation-plan.md). This document describes the system as designed, not how it was arrived at.

---

## 1. What it is

Each rio-builder presents `/nix/store` to the build sandbox as a three-layer read-only mount: an **EROFS metadata image** (inodes, dirents, sizes, modes — zero data bytes) stacked under **overlayfs** with a **digest-addressed FUSE** data-only lower. The build sees a complete, fully-populated store; bytes are fetched from rio-store only when a file is `open()`ed, keyed by the file's blake3 content digest.

This replaces the previous whole-path-granularity FUSE store (`rio-builder/src/fuse/`) with file-granularity lazy fetch, kernel-native metadata operations, and structural cross-path deduplication.

On the store side, ADR-022 introduces the **NAR index** (per-file `{path, size, mode, file_digest}` computed at PutPath time), the **Directory merkle layer** (`dir_digest`/`root_digest` over the same index), a **tiered chunk backend** (per-AZ S3 Express One Zone cache in front of S3 standard), and a runtime-configurable **S3 binary-cache compatibility layer** (stock-Nix `.narinfo` + compressed NAR dual-written to S3-standard so the bucket substitutes without rio running).

## 2. Properties

| Property | Statement |
|---|---|
| **Kernel-native metadata** | `stat`, `getattr`, `readdir`, `readlink` over `/nix/store` are served entirely by the EROFS layer in-kernel. Zero userspace crossings, regardless of cache state. |
| **Lazy file-granular fetch** *(rio-builder only)* | A file's bytes are fetched from rio-store on first `open()`, not at mount time and not as part of a whole-store-path NAR. A build that touches 5% of its closure fetches ≈5% of the bytes. External consumers (dev laptops, CI runners) reach rio-store via the gateway substituter (§8) or the HTTP binary-cache surface (`narinfo/*.narinfo` + signed NARs), not this stack. |
| **Cache-hit reads are kernel-direct** | Once a file is in the node-SSD backing cache, `open()` replies `FOPEN_PASSTHROUGH` with the cache fd; all reads go kernel → ext4 with zero FUSE involvement, including after page-cache eviction. The FUSE `read` path is reached only during the streaming-fill window of a large cold miss. |
| **Structural per-file dedup** | Two store paths containing byte-identical files redirect to the same digest object. One fetch, one SSD copy, one page-cache copy — without any explicit dedup pass. |
| **Streaming open for large files** | Files above `STREAM_THRESHOLD` (default 8 MiB) return from `open()` after the first chunk arrives; the remainder fills in the background. `read()` of an unfilled range demand-fetches it. A build linking against a 200 MB `libLLVM.so` fetches only the ranges the linker touches. |
| **Minimal privileged surface** | The only privileged component is `rio-mountd`, a node-level daemon that opens `/dev/fuse` + an EROFS superblock and hands both fds to the unprivileged builder over a Unix socket. The builder mounts overlay itself inside its own user namespace. The build sandbox has zero device exposure. |
| **Declared-input enforcement** | The digest-FUSE handler answers `lookup()` only for digests in the build's declared input closure; everything else is `ENOENT`. The build cannot read store paths it did not declare. |
| **Delta-sync distribution** | `nix copy --from rio-store` and inter-region replication walk a Directory merkle DAG. Unchanged subtrees are skipped in one batch RPC; bandwidth scales with change size, not closure size. |
| **Per-AZ chunk cache** | All rio-store replicas in an availability zone share an S3 Express One Zone directory bucket as a read-through cache. A new replica starts warm; S3 standard GET cost is once per chunk per AZ, not once per replica. |
| **S3 self-sufficient (configurable)** | When `binary_cache_compat` is enabled, every `PutPath` additionally writes a stock-Nix `.narinfo` and compressed NAR to S3-standard. `nix copy --from s3://bucket` works with no rio process running; PostgreSQL becomes a performance tier, not a correctness tier. Disabled ("pure rio mode") halves S3 storage at the cost of PG being load-bearing for any substitution. |

## 3. Architecture

```
┌──────────────────────────── builder pod (unprivileged) ────────────────────────────┐
│                                                                                    │
│  build sandbox sees /nix/store as:                                                 │
│                                                                                    │
│    overlay (rw, userxattr)                                                         │
│    ├── upper     = local SSD                 ← build outputs + db.sqlite land here │
│    ├── lower[0]  = EROFS metadata image      ← stat/readdir/readlink served here   │
│    │               (loop-mounted, ~5 MiB                                           │
│    │                for a chromium closure)                                        │
│    └── lower[1]  = digest-FUSE               ← open/read redirected here via       │
│         (data-only, "::") at                    user.overlay.redirect xattr        │
│         /var/rio/objects/{build_id}/                                               │
│                  │                                                                 │
│                  │ backed by                                                       │
│                  ▼                                                                 │
│    /var/rio/cache/ab/<blake3>      node-shared SSD (mountd-owned, builder-ro)      │
│    /var/rio/staging/{build_id}/    per-build, builder-writable; promoted by mountd │
│                  │                                                                 │
└──────────────────│─────────────────────────────────────────────────────────────────┘
                   │ on cache miss: GetChunks(chunk_digests) gRPC stream
                   ▼
┌─────────────────────────────── rio-store ──────────────────────────────────────────┐
│                                                                                    │
│   GetNarIndex(nar_hash) → NarIndex { entries: [{path, kind, size, exec,            │
│                                                 file_digest, dir_digest}],         │
│                                      root_digest }                                 │
│                                                                                    │
│   GetChunks([chunk_digest]) → stream<bytes>       (batched, server-streamed)       │
│                                                                                    │
│   TieredChunkBackend:  S3 Express (per-AZ) ──read-through──► S3 (authoritative)    │
│                                                                                    │
│   castore:  GetDirectory / HasDirectories / HasBlobs / ReadBlob   (delta-sync)     │
│                                                                                    │
└────────────────────────────────────────────────────────────────────────────────────┘

┌── rio-mountd (DaemonSet, hostPID, CAP_SYS_ADMIN) ──┐
│  setup:  open("/dev/fuse") (keep dup), mount fuse  │──── SCM_RIGHTS ───► builder
│          fsopen("erofs")/fsconfig/fsmount          │
│          mkdir staging/{build_id} (builder uid)    │
│  serve:  BackingOpen{fd} → ioctl BACKING_OPEN → id │◄─── per-open ─────► builder
│          Promote{digest} → verify-copy → cache     │
│  owns:   /var/rio/cache/ (read-only to builders)   │
│  on UDS close: umount2(MNT_DETACH), rm staging     │
│  on start: scan + reap orphaned objects_dir mounts │
└────────────────────────────────────────────────────┘
```

## 4. The mount stack

r[builder.fs.composefs-stack]

The builder assembles `/nix/store` from three layers per build:

1. **EROFS metadata image** — a read-only filesystem image containing every inode in the build's input closure with correct `i_size`, mode, mtime, symlink targets, and directory entries, but **zero data blocks**. Each regular file inode carries two xattrs: `user.overlay.redirect=/ab/<blake3-hex>` and a zero-length `user.overlay.metacopy`. Generated per build from the union of the closure's `NarIndex` rows via the FFI EROFS encoder; ~5 MiB / ~46 ms for a chromium-scale closure (23k files).

2. **digest-FUSE** — a `fuser` filesystem mounted at `/var/rio/objects/{build_id}/` exposing exactly two directory levels: 256 prefix dirs `00`..`ff`, and leaf files named by the remaining 62 hex chars of `blake3(file content)`. Serves `lookup`/`open`/`read` only; everything else is `ENOSYS`.

3. **overlayfs** — a single **RW** mount with `upperdir=<ssd>/nix/store, workdir=<ssd>/work, userxattr, lowerdir=<erofs_mnt>::<objects_dir>`; the merged dir is bind-mounted at the build's `/nix/store`. The `::` separator marks the FUSE mount as a [data-only lower layer](https://docs.kernel.org/filesystems/overlayfs.html#data-only-lower-layers): overlayfs will not path-walk into it, only follow absolute redirects from the metadata layer. **Build outputs and the synthesized `db.sqlite` land in the upper** — P0560 only swaps the `lowerdir=` value from the pre-ADR-022 FUSE mount to `<erofs>::<fuse>`.

r[builder.fs.userxattr-mount]

The `userxattr` mount option makes overlayfs read `user.overlay.*` xattrs (instead of `trusted.overlay.*`) and permits the mount from an unprivileged user namespace. The `metacopy=on` and `redirect_dir=on` options are **not** passed — they are rejected in combination with `userxattr`. Following redirects into the data-only lower is gated independently on the presence of a `::` layer (`ofs->numdatalayer > 0`, kernel `5ef7bcdeecc9`, ≥6.16), and that gate's safety condition — the lower is read-only and its redirect xattrs are immutable — is structurally satisfied by EROFS.

### Per-syscall resolution

| Syscall on `/nix/store/...` | Resolved by | FUSE upcalls |
|---|---|---|
| `stat`, `getattr`, `getxattr` | EROFS inode | 0 |
| `readdir` | EROFS dirents | 0 |
| `readlink` | EROFS symlink target | 0 |
| `open()` input | overlay reads `user.overlay.redirect` → FUSE `lookup(prefix)` + `lookup(digest)` + `open` | 2 lookup + 1 open, **independent of path depth** |
| `read()` / `mmap` input, file in node cache | `FOPEN_PASSTHROUGH` → kernel reads backing fd directly | **0** |
| `read()` / `mmap` input, large file mid-fill | FUSE `read` (≈128 KiB per upcall) until fill completes | O(touched bytes / 128 KiB), once per file per node |
| **write / create output** | **overlay upper (SSD)** | 0 |
| modify input → copy-up | overlay full-data-copies from FUSE lowerdata (`metacopy=false` under `userxattr`) | as cold open+read |

## 5. Store-side metadata: the NAR index

r[store.index.file-digest]

For every store path, rio-store maintains a **NAR index**: a flat list of `NarIndexEntry { path, kind, size, executable, nar_offset, file_digest, dir_digest }` describing each file, directory, and symlink in the path's NAR. `file_digest` is `blake3(file content)` for regular files; `dir_digest` is defined in §8.

r[store.index.putpath-eager]
r[store.index.non-authoritative]
r[store.index.nar-ls-streaming]

The index is computed eagerly during `PutPath` from the NAR stream (`nar_ls` + per-file blake3 in a single forward `Read` pass — no `Seek`, bounded memory regardless of NAR size), and persisted to the `nar_index` table keyed by `nar_hash`. It is **derived, non-authoritative** state: if a row is missing, `GetNarIndex` recomputes it synchronously from the stored NAR and writes it back. There is no separate artifact in object storage — the index is regenerable from the NAR.

r[store.index.rpc]

`GetNarIndex(nar_hash) → NarIndex` is the single RPC a builder needs, per store path in its closure, to construct both the EROFS metadata image and the digest-FUSE's lookup allowlist.

## 6. Builder-side data path: digest-FUSE

r[builder.fs.digest-fuse-open]

The digest-FUSE handler is the **only** FUSE in the stack and is reached **only on cold `open()`**. It holds:

- A `file_digest → (size, executable)` map built from the closure's `NarIndex` rows. `lookup()` for any digest outside this map returns `ENOENT` — this is the declared-input allowlist.
- A `DigestResolver` mapping `file_digest → (nar_hash, nar_offset, size) → chunk-range` for fetch.

r[builder.fs.passthrough-on-hit]

The handler negotiates `FUSE_PASSTHROUGH` at `init` (`max_stack_depth = 1`). `FUSE_DEV_IOC_BACKING_OPEN` requires init-ns `CAP_SYS_ADMIN` ([`backing.c:91-93`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c)), so the ioctl is brokered by `rio-mountd` (§10), which kept a `dup()` of this build's `/dev/fuse` fd. On `open(digest)`:

1. **Cache hit** at `/var/rio/cache/ab/<digest>`: send `cache_fd` to `rio-mountd` over the UDS → receive `backing_id`; reply `FOPEN_PASSTHROUGH | backing_id`. All `read`/`mmap` on this open go kernel → backing file; the handler sees nothing further until `release` (which sends `BackingClose{id}` to mountd).
2. **Cache miss, `size ≤ STREAM_THRESHOLD`**: fetch into `staging/{build_id}/<digest>.partial` verifying each chunk on arrival, whole-file blake3 verify, `Promote{digest}` → mountd verify-copies into cache → as (1).
3. **Cache miss, `size > STREAM_THRESHOLD`**: streaming path (§7). The fill task sources each chunk from `/var/rio/chunks/` (mountd-owned, RO) first; misses go to `GetChunks` and are written into `.partial` + `staging/chunks/`, with digests batched into `PromoteChunks` (assembly proceeds from own staging; the batch is for other builds). Concurrent builds **share progress at chunk granularity** — no leader/follower, no sentinel. On completion: whole-file verify, rename `.partial → <hex>`, `Promote{digest}`. Next `open` hits (1).
4. Within-build `.partial` orphan (unheld `flock`) → unlink + retry.

The FUSE `read` op exists only for case (3)'s window; in steady state every open is passthrough and the handler is a broker, not a server.

r[builder.fs.shared-backing-cache]
r[builder.fs.node-digest-cache]
r[builder.mountd.promote-verified]

The FUSE **mount point** is per-build (`/var/rio/objects/{build_id}/`) so one build's mount namespace never exposes another's. The **backing cache** (`/var/rio/cache/`) is node-shared SSD, **owned by `rio-mountd` and read-only to builder pods**. Builders fetch into a per-build **staging dir** (`/var/rio/staging/{build_id}/`, builder-writable); after verify they send `Promote{digest}` to mountd, which stream-copies from staging into a fresh mountd-owned cache file while re-hashing, and renames into place only on `blake3 == digest`. The copy is the integrity boundary — the cache inode is one mountd created and verified; a sandbox-escaped build cannot poison it.

r[builder.fs.node-chunk-cache]

For files > `STREAM_THRESHOLD`, mountd also owns `/var/rio/chunks/ab/<chunk_blake3>`: the streaming fill task `open()`s here before `GetChunks`, writes misses into its own staging, and batches `PromoteChunks{[digest]}` for other builds' benefit (assembly never blocks on it). Chunks are independently content-addressed, so mountd's verify is context-free — concurrent builds share progress at chunk granularity without coordination. The second build to open `libLLVM.so` reads its chunks from local SSD even while the first is mid-fill. Eviction is mountd's LRU sweep over both cache and chunks under disk-pressure watermark.

r[builder.fs.digest-resolve]

The `DigestResolver` is built once per build from `(NarIndex, ChunkList)` for each store path in the closure. For a `file_digest`, it returns the `(nar_hash, byte-range)` where that file's content lives in some NAR, then `partition_point` on that NAR's chunk-size cumsum yields the exact chunk slice to fetch. When the same `file_digest` appears under multiple NARs (the dedup case), any occurrence is valid; the resolver picks the one with the smallest enclosing chunk range.

## 7. Streaming open

r[builder.fs.streaming-open]
r[builder.fs.streaming-open-threshold]

Streaming open is the **during-fill mode** for files > `STREAM_THRESHOLD` (default 8 MiB) on cache miss — the only case where `open()` cannot reply passthrough because no complete backing fd exists yet.

`open()` spawns a background fill task and returns `FOPEN_KEEP_CACHE` after the **first chunk** is verified and written (~10 ms). `read(off, len)` for a range not yet filled blocks on the fill high-water-mark, priority-bumping the requested chunk range to the head of the fetch queue; `mmap(MAP_PRIVATE)` page faults route through the same `read` path. `KEEP_CACHE` does not suppress cold-page upcalls, only cross-open invalidation, so no mode-flip is needed within the streaming window. Whole-file digest verification runs at fill-complete and gates the `Promote{digest}` request to mountd.

Real-world builds touch 0.3–33% of giant `.so`/`.a` files in scattered or bimodal patterns; streaming open means the untouched 67–99.7% is fetched in the background (or not at all if the build finishes first), and every subsequent open of the same file on that node is passthrough.

## 8. Directory merkle layer and delta-sync

r[store.index.dir-digest]
r[store.castore.canonical-encoding]

In the same `nar_ls` pass that computes `file_digest`, a bottom-up second pass computes `dir_digest` for every directory: `blake3(canonical-encode(Directory { files, directories, symlinks }))`, where `Directory` is the snix-compatible [`castore.proto`](https://git.snix.dev/snix/snix/raw/branch/canon/snix/castore/protos/castore.proto) message with children sorted by name. `root_digest` is the top directory's `dir_digest`. Each encoded `Directory` body is stored in a `directories` table keyed by its digest (`ON CONFLICT DO NOTHING` — bodies are content-addressed and shared across store paths).

r[store.castore.directory-rpc]
r[store.castore.blob-read]
r[gw.substitute.dag-delta-sync]

The castore RPC surface is `GetDirectory(digest) → Directory`, `HasDirectories([digest]) → bitmap`, `HasBlobs([file_digest]) → bitmap`, and `ReadBlob(file_digest) → stream<bytes>`. `ReadBlob` resolves `file_digest → (nar_hash, nar_offset)` via the `file_blobs` table and streams the file content sliced from the underlying chunks — a snix-compatible client can substitute from rio-store holding only digests, without knowing rio's chunk layout. A delta-sync client (gateway substituter, inter-region replicator) syncing a closure to a target that already holds most of it:

1. Sends `HasDirectories([root_digest, ...])` for the closure's roots.
2. For each `false`, fetches the `Directory`, recurses into child `dir_digest`s with another `HasDirectories` batch.
3. At the leaves, `HasBlobs([file_digest, ...])` for the files under changed directories; fetches only the `false` ones.

Unchanged subtrees — typically the vast majority of a closure after an incremental rebuild — are pruned in O(1) RPCs at the subtree root. Sync bandwidth and RPC count scale with the *change*, not the closure. **Delta-sync requires a rio-aware receiver** (a rio-store replica or a host running rio-gateway as a local proxy) — a stock Nix client without `HasDirectories` falls through to the narinfo/NAR binary-cache path.

This layer has zero cost on the builder serving path: the mount stack and digest-FUSE never consult `dir_digest`. It exists for rio-store as a distribution substrate.

## 9. Tiered chunk backend

r[store.backend.tiered-get-fallback]
r[store.backend.tiered-put-remote-first]

`TieredChunkBackend` composes two `S3ChunkBackend` instances — a per-AZ S3 Express One Zone directory bucket as `local`, the regional S3 standard bucket as `remote`:

- `put(digest, bytes)` writes to **S3 standard only**. S3 standard is always authoritative.
- `get(digest)` reads the Express bucket first; on miss, reads S3 standard and writes through to Express. **Read-through is the only path that fills Express.**

This makes Express purely a serve-side concern by data flow: `PutPath*` and `PutChunk` never touch the Express client. A freshly written chunk is moka-hot in the writing replica; the first cross-replica read in that AZ pays one S3-standard RTT and fills Express; subsequent reads hit Express. The dropped warm-on-put step would have added ~7 ms PUT latency per chunk (see measured numbers below) for a benefit moka already provides locally.

One directory bucket is provisioned **per availability zone**; each store pod is configured with the bucket for its own AZ (resolved from the node's `topology.kubernetes.io/zone` label). A newly-scaled replica is warm immediately; S3 standard GET is paid once per chunk per AZ. Any single Express bucket (or its AZ) becoming unavailable degrades that AZ's replicas to direct S3-standard reads — slower, not down — and leaves other AZs unaffected. The cache tier is toggled by a single helm value (`store.chunkBackend.kind`), and flipping it back to `s3` is instant and lossless.

**Deployment prerequisite:** S3 Express One Zone is available only in specific AZ-IDs (as of 2026-04: `use1-az4/5/6`, `use2-az1/2`, `usw2-az1/3/4`, `aps1-az1/3`, `apne1-az1/4`, `euw1-az1/3`, `eun1-az1/2/3`). EKS subnets must land in supported AZ-IDs — verify via `aws ec2 describe-availability-zones --query 'AvailabilityZones[].[ZoneName,ZoneId]'` (the letter suffix `us-east-1a` is account-randomized; the `use1-azN` ID is physical). A replica scheduled in an AZ without Express runs with `local=None` — degraded to S3-standard-only, functional.

**Measured latency.** At 64 KiB chunks, concurrency 32, on `c8a.48xlarge` in `usw2-az1` (2026-04-09; `~/tmp/rio-express-bench/results/full-matrix.json`): Express GET 4.54 ms p50 / 6.98 ms p99 / 9.97 ms p99.9 at 446 MB/s; Express PUT 7.10 ms p50 / 8.91 ms p99 at 293 MB/s. In the same file-per-chunk access pattern, EBS gp3 read p50 0.76 ms but p99.9 58.5 ms; io2 read p50 0.47 ms but p99.9 62.7 ms — filesystem fd-recycle and journal stalls dominate the EBS tail. Express's p99.9 (10 ms) is the tightest of the four backends in rio's actual access pattern, despite the slowest p50.

r[infra.express.bounded-eviction]

**Bounded cache and eviction.** Each Express bucket is a bounded MRU working set, **not** a mirror of S3 standard. Target size is `chunk_backend.express.target_bytes` (default 8 TiB); a per-AZ leader-elected rio-store replica runs an hourly sweep that lists the bucket, sums sizes, and if over `target_bytes × evict_high_watermark` (default 1.10) deletes oldest-by-`LastModified` objects until under `target_bytes × evict_low_watermark` (default 0.90). Because Express is filled only via read-through, an object's `LastModified` ≈ the last time any replica in this AZ cold-missed on it — chunks that stay moka-hot have old `LastModified` and are correct to evict (a replica reading them won't reach Express anyway). An evicted chunk is still in S3 standard; the next miss pays one S3-standard RTT and refills Express. Eviction is never data loss. S3 Lifecycle expiration on directory buckets ([supported](https://docs.aws.amazon.com/AmazonS3/latest/userguide/directory-buckets-objects-lifecycle.html), age-based only) is configured as a defense-in-depth ceiling — it cannot enforce a size target, so the application sweep is authoritative.

**Considered alternative — FSx for Lustre.** The original ADR-022 draft used one FSx Lustre filesystem per AZ as `local`, mounted via the aws-fsx-csi-driver. Dropped because `ChunkBackend::{get,put}(digest)` on whole 16–256 KiB blobs is an object API — FSx's POSIX surface is unused, while costing a Lustre client kernel module on the NixOS AMI (out-of-tree, kernel-version-pinned), a CSI driver, per-AZ PVC zone-pinning, and a 1.2 TiB ≈ $175/mo/AZ capacity floor. S3 Express reuses the existing `S3ChunkBackend` code path and needs only an `aws_s3_directory_bucket` per AZ. The trade is higher per-chunk read p50 at L2 (4.5 ms vs sub-ms; see measured numbers above), behind the per-replica moka L1, on a path that is already batched-parallel and already tolerates S3-standard fallback as "slower, not down"; per-request cost crosses over with FSx storage cost at roughly 4K sustained GET/s.

**Considered alternative — ingest/serve role split.** A `--role={serve|ingest|all}` deployment split (separate Deployments, ingest registers `PutPath*`/`PutChunk` only, serve registers reads only) was evaluated and deferred. With `r[store.put.builder-chunked-only]` enforcing client-side FastCDC from builders, rio-store's ingest CPU is reduced to BLAKE3 verification plus, when `binary_cache_compat` is enabled, zstd compression — light enough that the monolith holds. The split is cheap to add later (one config enum, conditional gRPC service registration in `main.rs`, a second helm Deployment) if `rio_store_put_path_duration_seconds{rpc}` p99 under production load shows contention with `GetChunks` serving; building it preemptively would be the mistake. A lighter variant — async write fan-out, where `PutPath` returns after PG + S3-standard chunk commit and Express/compat writes go through a bounded background queue — was also considered; it reduces tail latency without addressing the scaling-axis mismatch and is likewise deferred until measured.

## 10. Storage tiers and the binary-cache compatibility layer

Four tiers, by role:

| Tier | Holds | Loss means |
|---|---|---|
| **S3 standard** (regional) | chunks (always); `{hash}.narinfo` + `nar/{filehash}.nar.zst` (when `binary_cache_compat` enabled) | unrecoverable data loss — this is the source of truth |
| **S3 Express One Zone** (per-AZ) | bounded MRU chunk cache (~8 TiB target, evicted — §9) | that AZ's replicas cold-read from S3 standard; nothing lost (§9) |
| **PostgreSQL** (single-region) | narinfo, manifests, refcounts, `nar_index`, `directories`/`file_blobs`, scheduler state | with compat **enabled**: stock-Nix substitution from S3 still works; rio's gRPC surface, dedup, and CA-cutoff are down. With compat **disabled**: nothing substitutes. PG is rebuildable-in-principle from S3 when compat is enabled (every narinfo+NAR is there). |
| **moka** (per-replica, in-memory) | hottest chunks + singleflight coalescing | one replica cold-reads its L2; nothing lost |

**Chunk topology invariant:** regardless of `binary_cache_compat` mode, the chunk layout is always exactly one S3-standard regional bucket (canonical, unbounded, holds every chunk) plus N S3-Express per-AZ buckets (bounded read-through caches). The compat toggle controls **only** whether `.narinfo` + `nar/*.nar.zst` are *additionally* written to S3-standard; it never affects whether chunks land there. "Pure rio mode" still has every byte in S3-standard — the chunks just aren't reassembled into stock-Nix-readable NARs.

r[store.compat.runtime-toggle]

The binary-cache compatibility layer is a **runtime** config value (`store.binary_cache_compat.enabled`), not a build flag. Toggling it OFF stops new compat writes but leaves existing `.narinfo`/`nar/*.nar.zst` objects in place; toggling ON resumes for subsequent `PutPath` calls (a reconciler backfills the gap — see the implementation plan). Default is **ON** for the migration phase; operators flip to OFF once all consumers are rio-aware to reclaim ~2× S3 storage.

r[store.compat.nar-on-put]

When enabled, `PutPath` writes the NAR — zstd-compressed by default — to S3-standard at `nar/{FileHash}.nar.zst`, where `FileHash = sha256(compressed bytes)`. The bytes are reassembled from chunks immediately post-commit (chunks were just written and are moka-hot).

r[store.compat.narinfo-on-put]

When enabled, `PutPath` writes a stock-Nix-format `.narinfo` to S3-standard at `{StorePathHash}.narinfo`. Unlike the HTTP server's on-the-fly narinfo (which omits `FileHash`/`FileSize`), the compat-written narinfo includes them — the compressed artifact exists, so the fields are computable. Signatures are the same ones stored in PG (signed at `PutPath` time over the standard fingerprint).

r[store.compat.write-after-commit]

Compat writes happen **after** the PostgreSQL transaction commits, **synchronously** within the `PutPath` handler. A crash between PG-commit and compat-write leaves a PG row with no S3 narinfo — recoverable (the reconciler re-emits from chunks). A compat-write failure is logged and metered but does **not** roll back the PG commit or fail the `PutPath` RPC: the path is durably stored and rio-servable; only the stock-Nix fallback is missing for that one path. `write_mode = "async"` (background queue, lower `PutPath` latency, larger reconciler backlog on crash) is a documented future option, not implemented in the first pass.

r[store.compat.stock-nix-substitute]

With compat enabled and at least one `nix-cache-info` object present at the bucket root, the S3-standard bucket is a valid `nix copy --from s3://bucket?region=…` substituter with no rio process running. This is the migration on-ramp (existing Nix infrastructure reads the bucket directly while rio is rolled out) and the disaster-recovery floor (PG outage degrades to the slower stock-Nix path instead of a total outage).

**Considered alternative — inline storage in PostgreSQL.** Earlier rio-store versions stored NARs ≤256 KiB directly in a `manifests.inline_blob` `BYTEA` column, bypassing chunking and S3. Dropped: with the compat layer above, those bytes are durably in S3 anyway (inside `nar/*.nar.zst`), so inline's durability rationale is gone; what remained — saving one S3 GET per tiny path — costs a second code path through `PutPath`/`GetPath`/`ManifestKind`, puts byte-serving load on PostgreSQL, and in pure-rio mode leaves PG holding the only copy. Removing it gives one code path, uniform dedup (identical `.drv` files across builds share a chunk), and tiny NARs hit moka/Express like everything else. The trade is ~79% more S3 objects by count (mostly sub-16 KiB single-chunk paths) — negligible at S3-standard PUT cost, and on the read path these are exactly what the moka L1 absorbs.

## 11. Privilege boundary

r[builder.mountd.erofs-handoff]
r[builder.fs.fd-handoff-ordering]

The builder pod runs unprivileged with no device mounts. EROFS lacks `FS_USERNS_MOUNT` and `/dev/fuse` is not openable from the pod, so a node-level `rio-mountd` DaemonSet (host PID namespace, `CAP_SYS_ADMIN`) does the two privileged operations and nothing else:

| Step | Actor | Action |
|---|---|---|
| 1 | rio-mountd | `fuse_fd = open("/dev/fuse")`; **keep a `dup()`**; `mount("fuse", "/var/rio/objects/{build_id}", …, "fd=N,…")`; send fd over UDS via `SCM_RIGHTS` |
| 2 | builder | receive fuse fd; spawn digest-FUSE server on it (`fuser::Session::from_fd`) |
| 3 | rio-mountd | `fsopen("erofs")`; `fsconfig(FSCONFIG_SET_FLAG, "ro")`; `fsconfig(…, "source", image)`; `fsconfig(CMD_CREATE)`; `fsmount()`; send detached-mount fd |
| 4 | builder | `move_mount(erofs_fd, "", AT_FDCWD, meta_mnt, MOVE_MOUNT_F_EMPTY_PATH)` |
| 5 | builder (in own userns) | `mount("overlay", merged, "overlay", 0, "userxattr,upperdir=<ssd>/nix/store,workdir=<ssd>/work,lowerdir=<meta_mnt>::<objects_dir>")`; bind `merged` → `/nix/store` |
| per-open | builder → rio-mountd | `BackingOpen{cache_fd}` over UDS (`SCM_RIGHTS`) → mountd `ioctl(kept_fuse_fd, FUSE_DEV_IOC_BACKING_OPEN)` → reply `backing_id`. `BackingClose{id}` on release. mountd does not inspect the fd — the ioctl rejects depth>0 backing, and `backing_id` is conn-scoped. |
| per-promote | builder → rio-mountd | `Promote{digest}` → mountd opens `staging/<digest>`, stream-copies into mountd-owned `cache/ab/<digest>.promoting` while hashing, verifies `blake3 == digest`, renames into place, unlinks staging. |
| teardown | rio-mountd | on UDS close: `umount2(objects_dir, MNT_DETACH)`; `rm -rf staging/{build_id}`; `rmdir`; drop kept fuse-fd. Builder mount-ns death drops overlay+erofs. On daemon start: scan `/var/rio/objects/*` + `/var/rio/staging/*` and reap orphans (`r[builder.mountd.orphan-scan]`). |

**Ordering is load-bearing:** the digest-FUSE server must be answering before step 5. overlayfs probes each lower's root at `mount(2)`; an unserved FUSE there deadlocks the mount syscall. The `fsconfig "ro"` flag must precede `CMD_CREATE` (it is a superblock flag; `MOUNT_ATTR_RDONLY` on `fsmount` is per-mount and does not stop EROFS opening the loop device read-write).

`rio-mountd` holds, per build, the UDS connection, a dup of that build's `/dev/fuse` fd, and the staging dirfds; ~250 LoC. Requests carry a `seq: u32` echoed in replies (so `spawn_blocking` `Promote` can reply out-of-order); errors are typed (`DigestMismatch`/`NotRegular`/`TooLarge` are build-fatal, `Retryable(..)` is infra-retry). The UDS socket is mode 0660 group `rio-builder`; mountd checks `SO_PEERCRED.gid` and rejects others. It is a strictly smaller privileged surface than the pre-ADR-022 model, where the builder pod itself held `CAP_SYS_ADMIN`. The brokered `BACKING_OPEN` registers an fd the builder already holds (conn-scoped, depth-0-only); `Promote` is the integrity boundary for the shared cache.

## 12. Integrity

r[builder.fs.file-digest-integrity]

Per-file integrity is enforced in the digest-FUSE handler, not by the kernel:

- **Per-chunk:** every chunk arriving from `GetChunks` is blake3-verified against its content address before its bytes are written to `.partial` or served to a `read()`. The handler never serves an unverified byte.
- **Whole-file (builder-side):** `blake3(file) == file_digest` is checked before `Promote` (small files: before `open()` returns; streamed files: at fill completion). A mismatch fails the build with an infrastructure error and discards the staging file.
- **Whole-file (mountd-side):** `Promote` independently re-hashes during the copy into cache and rejects on mismatch. This is the boundary that keeps the shared cache trustworthy against a compromised builder.

composefs's native fs-verity-in-metacopy integrity does **not** apply here. overlayfs validates it via the in-kernel `fsverity_get_digest()` API, which reads `inode->i_verity_info`; FUSE's fs-verity support (kernel ≥6.10, [`9fe2a036`](https://git.kernel.org/linus/9fe2a036a23ceeac402c4fde8ec37c02ab25f133)) is ioctl-forwarding only and never populates that — overlayfs sees no measurement on a FUSE lower. A daemon-supplied measurement would in any case be no stronger than the daemon-side blake3 above. The threat model is unchanged from the pre-ADR-022 FUSE store: the builder is the FUSE server and is already trusted not to corrupt its own build. The path to genuine kernel-side verification is making the data-only lower a real ext4/xfs hostPath with fs-verity enabled on materialized files — see the implementation plan's deferred list.

## 13. Failure modes

| Failure | Kernel/stack behavior | rio handling |
|---|---|---|
| digest-FUSE handler crash | next `open()` → `ENOTCONN`; **passthrough-opened files keep working** (kernel holds the backing fd); streaming-mode opens lose their `read` server | supervisor respawns; in-flight streaming build fails `EIO`. No D-state, no recovery protocol. |
| digest-FUSE hung mid-fetch | `open()` blocks in `S` (interruptible) | per-spawn `tokio::timeout` returns `EIO`; build classified infrastructure-failure (`r[builder.result.input-eio-is-infra]`) and re-queued |
| redirect target `ENOENT` | `open()` → `ENOENT` | only returned for digests outside the declared-input allowlist — correct behavior |
| chunk integrity mismatch | n/a (userspace) | fetch aborted, `.partial` discarded, build fails infrastructure-error |
| rio-mountd crash | existing mounts unaffected; new build-starts block on UDS connect | DaemonSet restarts; start-up orphan scan detaches stale `objects/{build_id}` mounts |
| Express cache tier unavailable | n/a | `TieredChunkBackend` falls back to direct S3-standard reads; metric `rio_store_tiered_local_hit_ratio` drops |
| PostgreSQL unavailable | n/a | rio-store gRPC + HTTP surfaces fail (no manifests, no narinfo). With `binary_cache_compat` enabled, clients substitute directly from `s3://bucket` (stock-Nix path); with it disabled, nothing substitutes until PG recovers. |
| compat S3 write fails post-commit | n/a | `PutPath` succeeds; `rio_store_compat_write_failures_total` increments; reconciler picks the path up on its next sweep |
| builder pod OOM-kill mid-fill | `staging/<digest>.partial` left with no `flock` holder | mountd reaps the staging dir on UDS close. A retry within the same build finds `.partial` with no lock holder → unlinks → restarts fill. |

r[builder.fs.fetch-circuit]

A circuit breaker on the digest-FUSE fetch path trips on sustained rio-store unreachability and fails the build fast rather than letting every `open()` time out individually.

## 14. Encoder

r[builder.fs.composefs-encode]
r[builder.fs.stub-isize]
r[builder.fs.metacopy-xattr-shape]

The EROFS metadata image is produced in-process by [`libcomposefs`](https://github.com/containers/composefs) (the C library podman/ostree ship; `Apache-2.0`) via Rust FFI. rio's adapter (~80 LoC) walks the closure's merged `NarIndex` calling `lcfs_node_new()` / `lcfs_node_set_{mode,size,mtime,payload}()` / `lcfs_node_add_child()` per entry, then `lcfs_write_to(memfd)`. The library is built from a nix-patched `pkgs.composefs`: a ~25-line patch adds `LCFS_BUILD_USER_XATTR_OVERLAY` (emit `user.overlay.*` instead of `trusted.*`) and `LCFS_FLAGS_NO_ROOT_WHITEOUTS` (skip the 256 OCI whiteout chardevs + root opaque). With both flags set the output carries only `user.*` xattrs and no root chardevs — clean, no workarounds.

No staging directory, no subprocess. The image is regenerable and need not be persisted; builders may cache it on node SSD keyed by closure hash.

## 15. Observability

r[obs.metric.digest-fuse]
r[obs.metric.chunk-backend-tiered]
r[obs.metric.compat]

| Metric | Meaning |
|---|---|
| `rio_builder_digest_fuse_open_seconds` (histogram) | wall-clock from `open()` upcall to reply, labeled `{hit="node_ssd"\|"remote", streamed="0"\|"1"}` |
| `rio_builder_digest_fuse_fetch_bytes_total` | bytes fetched from rio-store on behalf of digest-FUSE, labeled `{hit}` |
| `rio_builder_digest_fuse_upcalls_total` | FUSE upcalls by `{op="lookup"\|"open"\|"read"}` |
| `rio_builder_composefs_encode_seconds` | metadata-image generation time per build |
| `rio_store_tiered_local_hit_ratio` | Express-tier hits ÷ total `get()` per replica |
| `rio_store_compat_write_seconds` (histogram) | wall-clock for the post-commit narinfo+NAR S3 write, labeled `{result="ok"\|"err"}` |
| `rio_store_compat_write_failures_total` | compat writes that failed post-commit (reconciler backlog) |
| `rio_store_nar_index_compute_seconds` | `nar_ls` + blake3 pass duration at PutPath |
| `rio_builder_digest_fuse_open_mode_total` | per-`open()` reply, labeled `{mode="passthrough"\|"keep_cache"}` — passthrough-not-negotiated visible as `passthrough`=0 |
| `rio_builder_digest_fuse_open_case_total` | per-`open()` decision, labeled `{case="hit"\|"miss_small"\|"miss_stream"\|"wait_fetching"}` |
| `rio_mountd_request_seconds` (histogram) | UDS request latency, labeled `{op="mount"\|"backing_open"\|"backing_close"\|"promote_chunks"\|"promote"}` |
| `rio_mountd_promote_bytes_total` | bytes copied into cache by `Promote` |
| `rio_mountd_promote_reject_total` | rejected promotes, labeled `{reason="mismatch"\|"not-regular"\|"too-large"\|"race-timeout"}` |
| `rio_mountd_promote_inflight` (gauge) | current `Promote` copy tasks |
| `rio_mountd_connections_current` (gauge) | live UDS connections (== builds on this node) |
| `rio_mountd_cache_free_bytes` (gauge) | `statvfs(cache_dir)` free, sampled at LRU-sweep interval |

The mount stack's hot path is page cache + overlayfs + EROFS; kernel-side latency is observable via the upstream `tracepoint:{erofs,overlayfs,fuse}:*` events without rio-specific instrumentation.

## 16. Platform requirements

r[infra.node.kernel-composefs]

- **Kernel ≥ 6.16** — [`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9) makes overlayfs honor data-only-lower redirects under `userxattr`.
- **Kernel ≥ 5.2** — `fsopen`/`fsconfig`/`fsmount`/`move_mount` syscalls (subsumed by the above).
- `CONFIG_EROFS_FS=y`, `CONFIG_OVERLAY_FS=y`, `CONFIG_FUSE_FS=y`, `CONFIG_FUSE_PASSTHROUGH=y`. All stock-on; the NixOS node module sets `=y` over `=m` and asserts the kernel version at boot. **No** `EROFS_FS_ONDEMAND`, no `CACHEFILES*`.
- `r[builder.fs.passthrough-stack-depth]`: the node-SSD backing cache (`/var/rio/cache/`) must be a non-stacking filesystem (ext4/xfs hostPath). FUSE with `max_stack_depth=1` under overlay reaches `FILESYSTEM_MAX_STACK_DEPTH=2`; a stacking fs as backing would exceed it.
- Nix-patched `pkgs.composefs` providing `libcomposefs.so` (`nix/patches/libcomposefs-user-xattr.patch`); `bindgen` + `clang` at build time for the `-sys` crate.
- `/dev/fuse` reachable by `rio-mountd` (host device); **not** mounted into builder pods.

## 17. Normative requirements index

The `r[...]` markers appearing in this document and in [ADR-022 §2](./022-lazy-store-fs-erofs-vs-riofs.md) are the spec-traceability anchors for `tracey`. Each has exactly one `// r[impl ...]` site and at least one `# r[verify ...]` site; `tracey query rule <id>` lists them.

| Domain | Markers |
|---|---|
| Mount stack | `builder.fs.composefs-stack` · `builder.fs.userxattr-mount` · `builder.fs.fd-handoff-ordering` · `builder.overlay.composefs-lower` |
| Encoder | `builder.fs.composefs-encode` · `builder.fs.stub-isize` · `builder.fs.metacopy-xattr-shape` |
| digest-FUSE | `builder.fs.digest-fuse-open` · `builder.fs.passthrough-on-hit` · `builder.fs.passthrough-stack-depth` · `builder.fs.digest-resolve` · `builder.fs.file-digest-integrity` · `builder.fs.fetch-circuit` · `builder.fs.shared-backing-cache` · `builder.fs.node-digest-cache` · `builder.fs.node-chunk-cache` · `builder.fs.streaming-open` · `builder.fs.streaming-open-threshold` |
| Privilege | `builder.mountd.erofs-handoff` · `builder.mountd.backing-broker` · `builder.mountd.promote-verified` · `builder.mountd.orphan-scan` · `builder.mountd.concurrency` |
| Result classification | `builder.result.input-eio-is-infra` · `builder.fs.parity` |
| NAR index | `store.index.file-digest` · `store.index.nar-ls-offset` · `store.index.nar-ls-streaming` · `store.index.table-cascade` · `store.index.non-authoritative` · `store.index.sync-on-miss` · `store.index.putpath-eager` · `store.index.putpath-bg-warm` · `store.index.rpc` |
| Directory DAG | `store.index.dir-digest` · `store.castore.canonical-encoding` · `store.castore.directory-rpc` · `store.castore.blob-read` · `store.castore.gc` · `store.castore.tenant-scope` · `gw.substitute.dag-delta-sync` |
| Tiered backend | `store.backend.tiered-get-fallback` · `store.backend.tiered-put-remote-first` · `infra.express.cache-tier` · `infra.express.bounded-eviction` |
| Transport | `store.chunk.batched-stream` · `proto.chunk.bytes-zerocopy` · `store.chunk.tonic-tuned` · `builder.fetch.batched-stream` |
| Platform | `infra.node.kernel-composefs` |
| Observability | `obs.metric.digest-fuse` · `obs.metric.mountd` · `obs.metric.chunk-backend-tiered` · `obs.metric.express-eviction` · `obs.metric.compat` |
| Binary-cache compat | `store.compat.runtime-toggle` · `store.compat.nar-on-put` · `store.compat.narinfo-on-put` · `store.compat.write-after-commit` · `store.compat.stock-nix-substitute` · `store.compat.gc-coupled` |
| Chunked upload (§6) | `store.put.chunked` · `builder.upload.fused-walk` · `store.put.builder-chunked-only` |

# ADR-022 Implementation Plan — composefs-style lazy store + per-AZ S3 Express chunk cache

**Status:** sequencing only — design is [ADR-022 §2](./022-lazy-store-fs-erofs-vs-riofs.md) + [Design Overview](./022-design-overview.md) + ADR-023.
**Plan-number range:** P0541–P0582 (gaps at 0542/0547/0558 are abandoned numbers; do not reuse).
**Clean-cutover constraint:** no FUSE fallback flag, no `RIO_STORE_BACKEND` selector. P0560 deletes the old FUSE module wholesale.
**Cross-region forward-compat:** object store (S3/GCS) is authoritative for bytes; S3 Express One Zone is a per-AZ read-through cache; PG is single-region. Nothing here precludes cross-region deployment (object-store-authoritative, cache tier stateless) but it is not implemented. No DRA. **Express AZ-ID availability constrains region/AZ choice** — see [Design Overview §9](./022-design-overview.md).
**Migration-number range:** `033_*` (last shipped: `032_derivations_size_class_floor.sql`).

---

## How we got here

The pre-ADR-022 builder serves `/nix/store` via FUSE with whole-store-path JIT fetch — every `stat`/`readdir` is a userspace upcall, and a partially-hot 200 MB `.so` either upcalls every read or blocks `open()` for the whole file. Two kernel-filesystem replacements were evaluated and set aside ([ADR-022 §3](./022-lazy-store-fs-erofs-vs-riofs.md#3-alternatives-considered)): EROFS+fscache (cachefiles daemon, device-slot table, per-path S3 artifact) and a custom `riofs` kmod (~800 LoC novel kernel C). A composefs-style stack — EROFS metadata + overlayfs redirect → digest-addressed FUSE — was then spiked at chromium scale and found to dominate both: <10 ms mount, zero warm-read upcalls, structural per-file dedup, ~half the owned code, zero kernel code. Two follow-on spikes closed the open questions: the unprivileged-userns mount works via `userxattr` + a small privileged helper (`rio-mountd`), and `FOPEN_KEEP_CACHE` handles giant partially-read files without a mode transition. The spike evidence below is the validation record; everything else in this document is forward-looking sequencing.

<details><summary>Superseded plan versions</summary>

PLAN-GRAND-REFACTOR V1 (Path A / EROFS+fscache), V2/V3 (Path C, mkcomposefs subprocess) archived at `~/tmp/stress-test/`. P0540/P0542/P0547/P0558 were Path-A artifacts and are abandoned numbers.

</details>

---

## Spike evidence

Core-stack nixosTests consolidated on `adr-022` (commit `15a9db79`); chromium-146 closure topology (357 store paths, 23 218 regular files, 8 221 dirs, 3 374 symlinks) with synthetic content.

| Metric | Result |
|---|---|
| `mount -t overlay` wall-clock | **<10 ms**; 0 FUSE upcalls during mount |
| `find -type f` over 23 218 files | 60 ms, **0 FUSE upcalls** |
| `find -printf %s` sum | matches manifest; 120 ms, 0 upcalls |
| Cold `lookup` upcalls (any depth) | **2** (depth-independent) |
| Warm `read` upcalls | **0** |
| Metadata image (chromium closure) | **5.3 MiB**, encoded in **70 ms** |

**Privilege-boundary evidence** (P0541, commit `af8db499` on `adr-022`, kernel 6.18.20) — all six PASS:

| Subtest | Result |
|---|---|
| `userns-overlay` | PASS — unpriv builder mounts overlay itself with `-o ro,userxattr,lowerdir=<meta>::<objects>`. **Gotcha:** explicit `metacopy=on`/`redirect_dir=on` are **rejected** under `userxattr` (`params.c:988-1008`). The `::` data-only lower independently enables redirect-following — gated on `ofs->numdatalayer > 0` (`namei.c:1241`, [`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9), v6.16+), not `config->metacopy`. |
| `userns-fuse-self` | PASS — builder cannot `open("/dev/fuse")` without `privileged:true` or device-plugin, but with fd-handoff it never needs to: `rio-mountd` opens+mounts in init-ns, passes the connected fd. **Builder pod drops `smarter-devices/fuse:1` entirely.** |
| `kvm-hostpath-spike` (`9492019c` on `adr-022`) | PASS — Nix sandbox sees `/dev/kvm` via `extra-sandbox-paths` + char-device hostPath; `requiredSystemFeatures=["kvm"]` build does `ioctl(KVM_GET_API_VERSION)` → 12. **smarter-device-manager dropped entirely** — `/dev/kvm` is a capability flag (node label + hostPath), not a counted resource. |
| `erofs-loop-unpriv` | FAIL as expected (`EPERM`) — confirms P0567 mandatory. |
| `fsmount-handoff-erofs` | PASS — **Gotcha:** `fsconfig(FSCONFIG_SET_FLAG, "ro")` must precede `CMD_CREATE` or `move_mount` later fails `EROFS`. |
| `fuse-dev-fd-handoff` | PASS — `/dev/fuse` fd via SCM_RIGHTS works; `fuser` accepts pre-opened fd. |
| `teardown-under-load` | PASS — reader wakes `ENOTCONN` <1s, no D-state. |

**Passthrough validation** is **P0578** (separate spike — P0541 is DONE and must stay DONE for the dag-runner).

**Gotcha (ordering):** `/dev/fuse` fd MUST be received and the digest-FUSE serving **before** the overlay mount — overlayfs probes lowers at `mount(2)`; an unserved FUSE deadlocks the mounter.

**§2.7 large-file evidence** (P0575 promoted to critical-path on this basis):

| Commit / source | Finding |
|---|---|
| nix-index `top1000.csv` (external dataset, 2026-04-05) | nixpkgs top-1000 files: **all >64 MiB** (min 117 MiB, median 179 MiB, 267 >256 MiB, 7 >1 GiB). 248 are `.so`/`.a`. Floor — proprietary closures worse. |
| `42aa81b2` (`adr-022`, `nix/tests/lib/spike-access-data/RESULTS.md`) | Real consumers read **0.3-33%** of giants: link-time `libLLVM.so` 2.79% bimodal head+tail; `opt --version` 32.77% scattered/266 ranges; `libicudata` preload 0.28%. No `MAP_POPULATE`/`fadvise`. |
| `15a9db79` (`adr-022`, `composefs-spike-stream.nix`) | Streaming-open mechanism PASS: 256 MiB `open()` = **10.3 ms** (vs 2560 ms whole-file); `FOPEN_KEEP_CACHE` from start → 2nd `dd` **0 read upcalls**; `mmap` page-faults route through FUSE `read`; **no mode-transition needed** (KEEP_CACHE doesn't suppress cold upcalls, only prevents invalidation). |
| alternatives survey | File-splitting at encoder **infeasible** (overlayfs `redirect` is single-path). Allowlist prefetch **violates JIT-fetch imperative**. FSx-backed cluster-wide objects cache **rejected** — violates builder air-gap. |

**Key encoder findings** (now ADR-022 §2.2): stub inodes MUST carry real `i_size`. The metacopy xattr must be 0-length or ≥4 bytes (`struct ovl_metacopy{u8 version,len,flags,digest_algo}`). **Unpriv overlay (P0541) requires `user.overlay.{redirect,metacopy}`, NOT `trusted.*`** — overlayfs under `userxattr` reads only the `user.` prefix; the nix-carried `libcomposefs` patch adds `LCFS_BUILD_USER_XATTR_OVERLAY` so the writer emits `user.*` directly (and `LCFS_FLAGS_NO_ROOT_WHITEOUTS` to skip the 256 OCI whiteout chardevs). fs-verity-in-metacopy doesn't verify when the lower is FUSE — per-file integrity lives in the digest-FUSE handler (§2.6).

---

## Prerequisites (in flight separately — NOT phased here)

| Track | Status | Owns |
|---|---|---|
| **NixOS node cutover** (full Bottlerocket replacement) | dispatched (`nixos-cutover` agent) | `nix/nixos-node/{hardening,kernel}.nix`, `karpenter.yaml` amiSelectorTerms→tag, `xtask ami push`, ADR-021 |
| `kernel.nix` standalone module with `EROFS_FS=y OVERLAY_FS=y FUSE_FS=y FUSE_PASSTHROUGH=y`, **kernel ≥6.16** ([`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9): data-only-lower redirect honored under `userxattr`; subsumes ≥6.9 `FUSE_PASSTHROUGH` [`7dc4e97a4f9a`](https://git.kernel.org/linus/7dc4e97a4f9a)) | part of cutover | `nix/nixos-node/kernel.nix` — **MUST be importable by `nix/tests/fixtures/`** so VM tests reuse the AMI's exact config. **No `EROFS_FS_ONDEMAND`, no `CACHEFILES*`.** Module asserts version at boot. |
| Device exposure: **no smarter-device-manager** | part of cutover | `/dev/fuse` → fd-handoff from `rio-mountd` DS (P0567); `/dev/kvm` → `hostPath{type:CharDevice}` + `nodeSelector{rio.build/kvm}` on kvm-pool pods + `extra-sandbox-paths=["/dev/kvm"]` in builder nix.conf. `nix/nixos-node/static-pods.nix` drops the device-plugin pod. |

**This plan assumes the cutover lands first.** No old-FUSE fallback — same greenfield cutover as Bottlerocket→NixOS. Rollback for builder-side regressions is `xtask k8s eks down && up` from a known-green commit.

**Greenfield deployment constraint** (settled 2026-04-04, unchanged): we control the only deployment. Migration path is `xtask k8s eks down && up`. NO backfill jobs, NO old-binary compat shims, NO dual-read paths. When this plan's phases are ready to flip on, tear down + redeploy.

---

## User journeys (every phase traces to one)

| ID | User | Journey | Today | After |
|---|---|---|---|---|
| **U1** | build submitter | `nix build .#chromium`; closure 200 GB, build reads 5% | builder fetches whole touched store-paths (~per-path JIT, rev 63); warm reads via FUSE passthrough but cold = whole NAR | builder fetches **only the files the build opens**, on-demand, at file granularity; `stat`/`readdir` are kernel-native; warm reads = page-cache, zero crossings; identical files across store paths share one node-SSD copy and one page-cache copy |
| **U2** | operator | scales `rio-store` 3→12 under load | each replica cold-misses S3 independently; 12× GET cost; 12× moka warm-up | per-AZ S3 Express cache tier serves all replicas in that AZ; new replica is warm; S3-standard GET only on first-in-AZ cold miss. Cache-tier-AZ down → cold reads from S3 standard, not outage. |
| **U3** | operator | something is wrong at 02:00 | unclear which layer | one single-flag rollback for cache tier (`store.chunkBackend.kind=s3`, instant + lossless); builder-side rollback is greenfield `down && up` from known-green commit |
| **U4** | operator | wants to know if the new path is better | no per-file metrics | grafana: `rio_builder_digest_fuse_open_seconds` p99, `…_fetch_bytes_total{hit=node_ssd|remote}`, `rio_store_tiered_local_hit_ratio` |
| **U5** | deployment consumer | `nix copy --from rio-store` to a **rio-aware** receiver (rio-store replica, or host running rio-gateway proxy) that already has 95% of the target closure | walks chunk-list per store path; O(all-chunks) `HasChunk` RPCs even for unchanged paths | walks Directory DAG; `HasDirectories([root_digest,…])` short-circuits unchanged subtrees in one batch RPC; fetches only changed files. **Sync bandwidth ∝ change size, not closure size.** Stock-nix clients without `HasDirectories` fall through to P0566's narinfo/NAR binary-cache surface. |
| **U6** | operator / migrating org | PostgreSQL is unavailable, or rio-store is not yet deployed in a consumer environment | nothing substitutes — chunks in S3 are unreadable without the PG manifest index | with `binary_cache_compat` enabled, `nix copy --from s3://bucket?region=…` works directly against S3-standard with no rio process running. PG-loss is a degradation (no dedup serving, no CA-cutoff, no `FindMissingPaths`), not an outage. Migration on-ramp: existing Nix infra reads the bucket as a plain binary cache while rio rolls out. |

**Sequencing rule (U3, unchanged):** every phase boundary is `.#ci` green. Phases 0-4 are deploy-safe (store-side or test-only). Phase 5 (P0560) is the hard cutover: builders REQUIRE the composefs lower from that commit forward.

---

## DAG overview

```
P0576 EXT: nixos-cutover sentinel (kernel.nix ≥6.16 importable, /dev/fuse, AMI) ───────────────┐
                                                                                               │
┌── Phase 0 (gate + scaffold; ≤4-way parallel) ──┐                                             │
P0569 spike:composefs   P0541 spike:mount-priv   P0578 spike:passthrough    P0543 measure        P0544 spec-scaffold
(DONE — sentinel row)   (userns overlay; erofs   V4/V11/V12 + closure wc    ADR-023 (tiered, per-AZ)
                         loop unpriv? fsmount                               + ADR-022 §2 r[...] markers
                         handoff for erofs)      + aarch64 kernel
   │                       │                          │                     │
   └────────────────── Phase-0 gate: all PASS ────────┴─────────────────────┘
                                                                                               │
┌── Phase 1 (primitives; ≤8-way parallel) ──┐  all dep on P0544                                │
P0545 proto    P0546 nar_ls    P0572 dir merkle  P0570 DigestResolver   P0548 Tiered    P0549 blob-API  P0550 fetch.rs hoist
(NarIndex      (rio-nix;       (dir_digest/      (file_digest →         (S3 Express →   (string-keyed,  (StoreClients →
 +file_digest   +blake3)        root_digest;      nar coords →           S3 fallback)    narinfo/ ns)    store_fetch.rs)
 +dir_digest)                   directories tbl)  chunk range)                                            │
                                                                                                  ▼
                                                                               P0568 GetChunks server-stream
                                                                               (K_server=256; prost .bytes();
                                                                                tonic adaptive_window; obs)
   │              │               │                   │                        │                │
   ▼              │               │                   │                        │                │
P0551 migration 033 ◄─────────────┼───────────────────┼─────(blob ns)──────────┘                │
   │              │               │                   │                                         │
   ▼              ▼               │                   │                                         │
P0552 GetNarIndex + indexer_loop  │                   │                                         │
   │                              │                   ▼                                         │
   │                              │   ┌── Phase 3 cache-tier infra (parallel w/ Phase 2) ──┐    │
   │                              │   P0553 s3-express.tf (per-AZ directory bucket) + IAM       │
   │                              │      └─► P0554 helm AZ→bucket map ──► P0555 vm:tiered-cache
   │                              │             ★ FIRST SHIPPED VALUE (U2)
   │                              │             P0579 compat config+helm ──► P0566 compat writer ──► P0580 vm:compat-substitute  ★ U6 LANDS
   │                              │                                              └─► P0581 compat GC ──► P0582 compat reconciler
   │                              │
┌── Phase 4 composefs store-side (gated on Phase-0 + P0546) ──┐
P0556 composefs-sys + encode.rs (libcomposefs FFI; nix-patched user.* + no-root-whiteouts) + golden VM
   │
P0557 PutPath eager nar_index (try_acquire-gated; NAR in RAM → nar_ls+blake3) ◄─(P0551, P0552)
   │
┌── Phase 5 composefs builder-side ──┐                                                          │
P0567 rio-mountd DaemonSet (fd-handoff + BACKING_OPEN broker + Promote + cache owner) ◄─────────┤(P0576, P0578)
   │                                                                                            │
P0559 composefs/{digest_fuse,circuit}.rs ◄─(P0545, P0550, P0567, P0568, P0570)
   │
P0571 mountd-owned cache LRU + per-build staging ◄─(P0559, P0567)
   │
P0575 streaming open() for files > STREAM_THRESHOLD ◄─(P0559, P0570, P0571)
   │
P0560 [ATOMIC] §A mount.rs+overlay+DELETE old-FUSE  §B fixture kernel + vm:composefs + FUSE-assert sweep
   │
P0562 audit: tracey builder.fuse.* empty + r[verify builder.fs.parity]  ★ CUTOVER GATE (U1)
   │
┌── Phase 6 obs + finalize ──┐
P0563 metrics+dashboard+alerts   P0564 helm: wire mountd DS + kernel assertion   P0565 runbooks

┌── Phase 7 Directory DAG / delta-sync (U5; parallel with Phases 4-6 after P0546) ──┐
P0572 dir_digest/root_digest in NarIndex + directories table (bottom-up in P0546 pass)
   │
P0573 DirectoryService RPC: GetDirectory / HasDirectories / HasBlobs (batch)
   │
P0577 BlobService.Read(file_digest) server-stream (snix-compatible blob fetch)
   │
P0574 gateway substituter: Directory-DAG delta-sync client  ★ U5 LANDS
```

**Hidden dependencies surfaced:**

| Edge | Why it's non-obvious |
|---|---|
| P0549 blob-API → P0566 | `ChunkBackend` trait today is `[u8;32]`-addressed only (`rio-store/src/backend/chunk.rs:91`). `{h}.narinfo` / `nar/{h}.nar.zst` / `nix-cache-info` need string-keyed `put_blob/get_blob/delete_blob`. |
| P0576 (kernel.nix sentinel) → P0560 | Test-VM kernel must have the same `extraStructuredConfig` as the AMI. `kernel.nix` MUST be a standalone NixOS module importable by `nix/tests/fixtures/`. |
| P0550 fetch.rs hoist → P0559 | `rio-builder/src/fuse/fetch.rs:20,32-33` import `fuser::Errno`, `super::NixStoreFs`, `super::cache`. **NOT a pure `git mv`** — hoist `StoreClients` + `fetch_chunks_parallel` core to `store_fetch.rs`; leave FUSE-typed wrappers in `fuse/fetch.rs` *temporarily* (P0560 deletes them with the rest of `fuse/`). ~150 LoC of actual refactor, not zero. |
| P0544 spec-scaffold → everything with `r[impl …]` | `tracey-validate` in `.#ci` fails on dangling `r[impl X]` where `r[X]` has no spec text. Markers must be on `sprint-1` before any code phase merges. |
| P0548 → P0553 | Terraform may land first, but the helm flip to `kind: tiered` MUST NOT — `TieredChunkBackend` semantics (S3-sync put, FS write-through on get) are what make the cache tier safe to enable. |
| P0541 (all 6 PASS) → P0567 minimal | EROFS lacks `FS_USERNS_MOUNT`; builder also can't open `/dev/fuse` unprivileged. `rio-mountd` (init-ns `CAP_SYS_ADMIN`) opens `/dev/fuse` + does `fsopen("erofs")/fsconfig("ro")/fsmount` → SCM_RIGHTS **both** fds → exits. Builder receives fds, serves digest-FUSE on the fuse-fd, `move_mount()`s the erofs-fd, then mounts overlay itself in its userns (`userxattr`). |
| P0546 blake3 → P0570 | `DigestResolver` keys by `file_digest`; the digest must exist in `NarIndexEntry` before the resolver can be built. |
| P0546 ↔ P0572 | `dir_digest` is computed bottom-up over `file_digest` of children — same pass, same RAM. P0572 extends P0546's `nar_ls` rather than re-walking. |
| P0573 batch RPCs ← I-110 lesson | per-digest unary `HasDirectory` against a 50k-node DAG is the I-110 PG-wall again. `HasDirectories([digest]) → bitmap` and `HasBlobs([file_digest]) → bitmap` are batch from day one. |
| P0571 → P0560 | Node-SSD cache is the digest-FUSE's backing dir; mount sequence in P0560 references `/var/rio/objects`. If P0571 slips, P0560 uses `tmpfs` (loses cross-build amortization but functions). |
| P0575 → P0560 | streaming-open is part of `digest_fuse.rs`; P0560's `vm-composefs-e2e cold-read` exercises it. P0575 must land before §B's <500 ms assertion is meaningful. |

---

## Phase 0 — Spike gate + scaffold (de-risk before committing)

Spikes are throwaway on `spike/*` branches; results captured in `.stress-test/sessions/2026-04-NN-phase0-gate.md`. P0543/P0544 ship to sprint-1.

### P0569 — SPIKE sentinel: composefs-style validated
**Crate:** `spike` · **Deps:** none · **Complexity:** — · **Status: DONE 2026-04-05**

Dependency-tracking row only. Consolidated as `15a9db79` on `adr-022` (originals `9c162024`/`a1394c0b`/`9415f9e2`); see §Spike evidence. **Exit met:** mount <10 ms, warm read = 0 upcalls, `stat` correct via `mkcomposefs`, depth-independent cold lookup. ADR-022 reopened in C's favor (`adr-022` `4a716900`..`b6794962`).

### P0541 — SPIKE: composefs privilege boundary + mount handoff
**Status: DONE — all six subtests PASS** (commit `af8db499` on `adr-022`, kernel 6.18.20). Results table in §Spike evidence above. Confirms overlay mount stays in the unprivileged builder via `userxattr`.
**Files:** `nix/tests/scenarios/spike-composefs-priv.nix` — VM imports `nixos-node/kernel.nix`; runs as unpriv-userns user.

### P0578 — SPIKE: passthrough-under-overlay + brokered `BACKING_OPEN`
**Crate:** `spike, nix` · **Deps:** P0541 · **Complexity:** LOW · **Status:** UNIMPL

Extends `composefs-spike-priv.nix` with a `passthrough-under-overlay` subtest. Asserts: (i) overlay mount succeeds with FUSE lower at `max_stack_depth=1` (depth 2 = `FILESYSTEM_MAX_STACK_DEPTH`); (ii) unprivileged `ioctl(FUSE_DEV_IOC_BACKING_OPEN)` → `EPERM`; (iii) root-process ioctl on a `dup()` of the same `/dev/fuse` fd succeeds and `FOPEN_PASSTHROUGH` open under overlay reads correctly from ext4 backing; (iv) reads continue after `kill -9` of the FUSE server; (v) brokered `Promote` with mismatched blake3 → mountd rejects, cache file absent; (vi) **`BackingOpen` RTT**: 10k iter, p99 < 200 µs; (vii) **`Promote` throughput**: 256 MiB ×3, ≥ 1.0 GiB/s; (viii) **copy-up**: overlay with `upperdir`+`userxattr`+`::` mounts; `chmod` a redirected input → upper has full data (not 0 bytes); (ix) **cache-readonly**: unpriv `open(cache/ab/X, O_WRONLY)` → `EACCES`; (x) **concurrency**: fire 1 GiB `Promote`, concurrently 100 `BackingOpen`, assert p99 < 1 ms; (xi) **Promote hardening**: `staging/<hex>` is symlink → `Err(NotRegular)`; FIFO → `Err(NotRegular)`.

Each as an independent `subtests=[...]` entry (failures isolate). `# r[verify builder.fs.passthrough-stack-depth]` `# r[verify builder.mountd.{backing-broker,promote-verified,concurrency}]` at the entries. **Exit:** `nix build .#checks.x86_64-linux.vm-composefs-spike-priv` green.

### P0543 — V4/V11/V12 measurement + closure-size + aarch64 kernel sanity
**Crate:** `xtask` · **Deps:** none · **Complexity:** LOW
| File | Change |
|---|---|
| `xtask/src/k8s/measure.rs` | new — `xtask measure v4` (synth metadata-image-build latency on chromium closure via `composefs::encode::build_image`), `xtask measure v11` (intra-closure chunk-reuse %), **`xtask measure v12` (tune `STREAM_THRESHOLD` — ingest nix-index `top1000.csv` + `nix/tests/lib/spike-access-data/RESULTS.md` (`42aa81b2`); compute the size at which whole-file fetch latency exceeds p50 first-range-touched latency)**, `xtask measure closure-paths` (`nix path-info -r nixpkgs#chromium \| wc -l` for both arches) |
| `.stress-test/metrics/v4-v11-v12.json` | output |
| `nix/checks.nix` | `node-kernel-config-aarch64`: `pkgsCross.aarch64-multiplatform` eval of `nixos-node/kernel.nix`; assert `EROFS_FS` / `OVERLAY_FS` / `FUSE_FS` resolve `=y` in the cross config. Build-eval only. |

**Exit:**
- `v4_p99_ms < 200`. Spike measured 70 ms encode + <10 ms mount for chromium; this is headroom check. FAIL → cache the metadata image per-closure-hash on node SSD (P0571 already provides the dir).
- `v12_stream_threshold_bytes`. **Tuning, not a gate.** P0575 ships unconditionally (top1000.csv + access-probe `da6148cd` already prove the 64 MiB question). V12 picks the `STREAM_THRESHOLD` config default (initial: 8 MiB ≈ 60-120 ms whole-file at 1 Gbps).
- `node-kernel-config-aarch64` builds. FAIL → fix `kernel.nix` for aarch64 before P0576 flips DONE.
- ~~`closure_paths_* < 65535`~~, ~~`max_nar_size_* < 4 GiB`~~ — **gates removed** (no device table; `nar_ls` is streaming unconditionally per P0546). Measurements kept as informational.

### P0544 — Spec scaffold (all `r[…]` markers + ADR-023)
**Crate:** `docs` · **Deps:** none · **Complexity:** LOW
| File | Change |
|---|---|
| `docs/src/decisions/022-lazy-store-fs-erofs-vs-riofs.md` | merge `adr-022` (refocused §2 Design / §3 Alternatives). Carries 13 markers: `r[builder.fs.{composefs-stack, userxattr-mount, stub-isize, metacopy-xattr-shape, fd-handoff-ordering, digest-fuse-open, passthrough-on-hit, passthrough-stack-depth, shared-backing-cache, file-digest-integrity, streaming-open-threshold}]` + `r[builder.mountd.{promote-verified, orphan-scan}]`. |
| `docs/src/decisions/022-design-overview.md` | merge `adr-022`. Canonical design reference. |
| `docs/src/decisions/023-tiered-chunk-backend.md` | new — object store (S3 today; GCS-ready via `ObjectStoreBackend` trait) is authoritative for bytes; **one S3 Express One Zone directory bucket per AZ** is a disposable read-through cache. Both tiers are `S3ChunkBackend` instances; `put` = remote sync + local async; `get` = local → remote fallback + write-through. PG `chunk_refs` is single-writer arbiter (single-region). **No DRA.** Forward-compat for cross-region: cache tier is stateless and metadata-agnostic; object-store cross-region replication + a globally-consistent metadata store would suffice, but neither is in scope here. Explicitly states: any single cache-tier-AZ outage = that AZ's replicas cold-read from S3 standard, not service outage; rollback `kind=s3` is instant + lossless. Records FSx-for-Lustre as the considered alternative. Carries `r[infra.express.cache-tier]`. |
| `docs/src/components/store.md` | append §"NAR index" (incl. `file_digest`) + §"Tiered chunk backend" + §"BlobService" + §"Binary-cache compatibility layer" (`r[store.compat.*]`) |
| `docs/src/components/builder.md` | **rewrite** §"FUSE Store" → §"composefs lazy lower" + §"digest-FUSE handler" + §"rio-mountd" (delete pre-ADR-022 whole-path FUSE description) |
| `docs/src/components/gateway.md` | append `r[gw.substitute.dag-delta-sync]` spec text |
| `docs/src/security.md` | rewrite §Boundary-3 (builder pods now unprivileged; `/dev/fuse` via fd-handoff; PSA tightens from `privileged`; mountd is the new `CAP_SYS_ADMIN` holder + integrity gate for shared cache); update §Known-Limitations |
| `docs/src/multi-tenancy.md` | append `directory_tenants` / `file_blob_tenants` rows to the tenant-scoping table |
| `docs/src/deployment.md` | append `r[infra.node.kernel-composefs]` spec text |
| `docs/src/observability.md` | append metric rows |
| `.config/tracey/config.styx` | spec `include` += `decisions/023-tiered-chunk-backend.md`, `deployment.md` (so `infra.express.cache-tier` and `infra.node.kernel-composefs` are scannable) |

**Exit:** `tracey query validate` 0 errors; `.#ci` green.

**Phase-0 gate (go/no-go):** P0569 DONE; P0541 subtests route P0567/P0559/P0560 design (do NOT block the gate). Record in `.stress-test/sessions/`. Phases 1–3 are design-agnostic and proceed regardless.

---

## Phase 1 — Primitives (≤7-way parallel; all dep on P0544)

### P0545 — proto: NarIndex with `file_digest`
**Crate:** `rio-proto` · **Deps:** P0544 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-proto/proto/types.proto` | `message NarIndexEntry { bytes path=1; Kind kind=2; uint64 size=3; bool executable=4; uint64 nar_offset=5; bytes target=6; bytes file_digest=7; }` — `path`/`target` are `bytes` not `string` (NAR names are arbitrary non-NUL/non-slash bytes; non-UTF8 is legal). `file_digest` is blake3 of regular-file content (32 bytes; empty for dirs/symlinks). `message NarIndex { repeated NarIndexEntry entries=1; }` |
| `rio-proto/proto/store.proto` | `rpc GetNarIndex(...)`; `rpc GetNarIndexBatch(NarHashList) returns (stream NarIndexResponse)` (build-start fetches ~357 indices; batch avoids per-path RTT) |
| `xtask regen mocks` | run |

**Exit:** `.#ci` green.

### P0546 — rio-nix: streaming `nar_ls` + blake3-per-file
**Crate:** `rio-nix` · **Deps:** P0544, P0545 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-nix/src/nar.rs` | `pub fn nar_ls<R: Read>(r) -> Result<Vec<NarLsEntry>>` — sibling to `parse()` (~line 238); **single forward pass, no `Seek`, bounded memory regardless of NAR size.** Maintains a running byte counter for `nar_offset`; for `Regular`, records the offset after the `"contents"` length-prefix, then streams the `size` bytes through `blake3::Hasher` in 64 KiB blocks (bytes touched once, never buffered whole). `NarLsEntry { …, file_digest: [u8;32] }`. `// r[impl store.index.nar-ls-offset]` `// r[impl store.index.file-digest]` `// r[impl store.index.nar-ls-streaming]` |
| `rio-nix/fuzz/fuzz_targets/nar_ls.rs` + `Cargo.toml` + `nix/fuzz.nix` | new — includes a >4 GiB synthetic NAR via `io::repeat()` slices to assert no buffering |
| tests | proptest: `serialize(tree)` → `nar_ls` → `&nar[off..off+size] == content` AND `file_digest == blake3(content)`; explicit test with reader wrapper that panics on `seek()`. `// r[verify ...]` |

The `Read+Seek` variant is not implemented — callers that have a `Vec<u8>` wrap it in `Cursor` and the streaming impl is no slower.

**Exit:** `.#ci` green incl. `fuzz-nar_ls`.

### P0548 — TieredChunkBackend (object-store authoritative; S3 Express read-through cache)
**Crate:** `rio-store` · **Deps:** P0544 · **Complexity:** LOW
`rio-store/src/backend/tiered.rs`: `TieredChunkBackend { local: Option<S3ChunkBackend>, remote: S3ChunkBackend }`. `put` = remote sync then local best-effort; `get` = local → remote fallback + write-through; `local=None` degrades to pass-through. Both tiers are the existing `S3ChunkBackend` — **no `backend/fs.rs`**, no new put-idempotence (S3 PutObject already is). `// r[impl store.backend.{tiered-get-fallback,tiered-put-remote-first}]`. **Exit:** `.#ci` green.

### P0549 — ChunkBackend blob-API
**Crate:** `rio-store` · **Deps:** P0544, P0548 · **Complexity:** LOW
Extend `ChunkBackend` with string-keyed `put_blob/get_blob/delete_blob` for P0566's `narinfo/`/`manifests/` sidecars (the `[u8;32]`-addressed chunk API can't express named objects). **Exit:** `.#ci` green.

### P0568 — Batched `GetChunks` server-stream + prost-bytes + tonic residuals + obs
**Crate:** `rio-proto, rio-store, rio-builder` · **Deps:** P0545, P0550 · **Complexity:** MED
`rpc GetChunks(stream GetChunksRequest) returns (stream ChunkData)` with `K_server=256` server-side fan-out; `prost(bytes = "bytes")` for zero-copy; tonic `adaptive_window`. Spike-validated `spike/rtt-bench` @ `96cfd098`. The digest-FUSE `open` handler (P0559) is the consumer. **Exit:** `.#ci` green; live A/B ≥4× cold-fetch reduction.

### P0550 — fetch.rs core hoist (NOT a pure mv)
**Crate:** `rio-builder` · **Deps:** P0544 · **Complexity:** MED
Hoist `StoreClients` + `fetch_chunks_parallel` from `rio-builder/src/fuse/fetch.rs` (which imports `fuser::Errno`, `super::NixStoreFs`) to `rio-builder/src/store_fetch.rs`; leave old-FUSE-typed wrappers in `fuse/fetch.rs` until P0560 deletes them. ~150 LoC actual refactor. **Exit:** `.#ci` green; existing FUSE VM tests unchanged.

### P0572 — Directory merkle layer: `dir_digest`/`root_digest` + `directories` table
**Crate:** `rio-proto, rio-nix, rio-store` · **Deps:** P0545, P0546 · **Complexity:** LOW (~50 LoC compute + table)

Closes the subtree-merkle gap vs snix at **zero serving-path cost** — the mount stack and digest-FUSE are unaffected. The work happens once at PutPath/index time (<1 ms on top of P0546's blake3 pass; bytes already in RAM).

| File | Change |
|---|---|
| `rio-proto/proto/types.proto` | `NarIndexEntry { …; bytes dir_digest = 8; }` (populated when `kind==DIR`; blake3 of canonical Directory encoding); `NarIndex { …; bytes root_digest = 2; }` |
| `rio-proto/proto/castore.proto` | new — vendor snix [`castore.proto`](https://git.snix.dev/snix/snix/raw/branch/canon/snix/castore/protos/castore.proto) (MIT): `message Directory { repeated DirectoryNode directories; repeated FileNode files; repeated SymlinkNode symlinks; }` with `FileNode{name, digest, size, executable}`, `DirectoryNode{name, digest, size}`, `SymlinkNode{name, target}`. **Pin canonical encoding rule** in a doc-comment: fields sorted by `name` (bytes-lex), no unknown fields, prost's default field-order encode. **snix issue #111**: prost determinism is not formally guaranteed across versions — add a golden-bytes test that fails loudly on encoder drift. `// r[impl store.castore.canonical-encoding]` |
| `rio-nix/src/nar.rs` | `nar_ls` second pass (bottom-up over the entry list, deepest-first): for each `kind==DIR`, build `Directory{…}` from immediate children's `file_digest`/`dir_digest`/`target`, encode, `dir_digest = blake3(encoded)`. `root_digest` = top dir's `dir_digest`. ~50 LoC. `// r[impl store.index.dir-digest]` |
| `migrations/033_nar_index.sql` (P0551 — same migration) | `+ CREATE TABLE directories (digest bytea PRIMARY KEY, body bytea NOT NULL, refcount integer NOT NULL DEFAULT 0); CREATE TABLE directory_tenants (digest bytea NOT NULL REFERENCES directories(digest) ON DELETE CASCADE, tenant_id uuid NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE, PRIMARY KEY (digest, tenant_id)); CREATE INDEX directory_tenants_tenant_idx ON directory_tenants (tenant_id, digest);` |
| `rio-store/src/nar_index.rs` (P0552) | after `set_nar_index`: `INSERT INTO directories … ON CONFLICT (digest) DO UPDATE SET refcount = directories.refcount + 1` (UNNEST, **sorted** input per `r[store.chunk.refcount-txn]`); `INSERT INTO directory_tenants (digest, $tenant_id) ON CONFLICT DO NOTHING`. Same shape for `file_blobs`/`file_blob_tenants`. `// r[impl store.castore.gc]` `// r[impl store.castore.tenant-scope]` |
| `rio-store/src/gc.rs` (existing sweep) | in the per-manifest sweep txn, before `DELETE narinfo` cascades: decode the dying `nar_index.entries`, `UPDATE directories SET refcount=refcount-1 WHERE digest=ANY($sorted)` + same for `file_blobs`; `DELETE FROM directories WHERE digest=ANY($zeros)` (no S3 object → hard-delete; junction rows go via `ON DELETE CASCADE`). |
| tests | proptest: `serialize(tree)` → `nar_ls` → re-derive `dir_digest` from children == stored value. snix-interop golden: known tree → `root_digest` matches snix's `tvix-store import` output (fixture bytes pinned). **GC**: PutPath A and B sharing a subtree → `directories.refcount==2` for the shared digest → GC A → `refcount==1` → GC B → row gone. `// r[verify store.index.dir-digest]` `// r[verify store.castore.{canonical-encoding,gc}]` |

**Measured benefit today is small** (12.1% dir-sharing on chromium, ~90% of which is empty dirs; no subtree-integrity consumer in `security.md`). This is **optionality** that becomes load-bearing under U5: snix `castore.proto` interop + `root_digest` as a closure-level cache key + the Directory DAG that P0573/P0574 walk.

**Exit:** `.#ci` green; `dir_digest`/`root_digest` populated for all regular paths; golden-bytes encoding test pinned.

### P0570 — `DigestResolver`: `file_digest → chunk-fetch coords`
**Crate:** `rio-builder` · **Deps:** P0544, P0545, P0550 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-builder/src/composefs/resolver.rs` | new — `struct DigestResolver { by_digest: HashMap<[u8;32], FileCoords>, cumsum: HashMap<NarHash, Vec<u64>> }` where `FileCoords { nar_hash, nar_offset, size }`. `fn new(closure: &[(NarHash, NarIndex, ChunkList)]) -> Self` builds both maps. `fn resolve(&self, digest) -> Option<(NarHash, &[ChunkRef], Range<u64>)>` — looks up coords, `partition_point` on the per-NAR cumsum, returns the chunk slice + byte slice within. **Handles dedup:** if the same `file_digest` appears under multiple `nar_hash`, store the first (any will do — content is identical by construction; pick the one with smallest enclosing chunk-range as a tie-break). `// r[impl builder.fs.digest-resolve]` |
| tests | proptest: synth N NARs with overlapping files → resolver returns coords whose `&nar[range] blake3 == digest`. `// r[verify ...]` |

**Exit:** `.#ci` green.

---

## Phase 2 — Store nar_index

### P0551 — migration 033
**Crate:** `rio-store` · **Deps:** P0545 · **Complexity:** LOW
| File | Change |
|---|---|
| `migrations/033_nar_index.sql` | `CREATE TABLE nar_index (store_path_hash bytea PRIMARY KEY REFERENCES manifests ON DELETE CASCADE, entries bytea NOT NULL, created_at timestamptz DEFAULT now()); ALTER TABLE manifests ADD COLUMN nar_indexed boolean NOT NULL DEFAULT false; CREATE INDEX manifests_nar_index_pending_idx ON manifests(updated_at) WHERE NOT nar_indexed AND status='complete';` — **partial-index work-queue** (precedent: migration 031's `WHERE status='uploading'`). PG forbids subqueries in partial-index predicates, so the queue is a same-table bool flag; indexer flips `nar_indexed=true` on success (HOT-update eligible). |
| ~~`migrations/034_manifests_boot_size.sql`~~ | **NOT created** — no boot blobs |
| `rio-store/tests/migrations.rs` | append `(33, "<sha384>")` to `PINNED` |
| `rio-store/src/migrations.rs` | `M_033` doc-const |
| `rio-store/src/metadata/queries.rs` | `get/set_nar_index`, `list_nar_index_pending(limit)`. `// r[impl store.index.table-cascade]` |
| `xtask regen sqlx` | run |

**Exit:** `.#ci` green.

### P0552 — GetNarIndex handler + indexer loop
**Crate:** `rio-store` · **Deps:** P0545, P0546, P0551 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-store/src/nar_index.rs` | new — `compute(pool, backend, store_path)`: fetch chunks → reassemble → `nar_ls` (now emits `file_digest`) → `set_nar_index`. Guard: `nar_index_sync_max_bytes` config (default 4 GiB). `// r[impl store.index.{non-authoritative,sync-on-miss}]` |
| same | `indexer_loop(pool, backend)` — poll `list_nar_index_pending(32)` → `compute` → sleep 5 s if empty. `// r[impl store.index.putpath-bg-warm]` |
| `rio-store/src/grpc/mod.rs` | `get_nar_index()`: PG hit → return; miss → `compute()` write-through. `// r[impl store.index.rpc]` |
| `rio-store/src/main.rs` | `tokio::spawn(indexer_loop(...))` |
| `rio-store/src/lib.rs` | `pub mod nar_index;` + `rio_store_narindex_{compute_seconds,cache_hits_total}` |
| tests | ephemeral PG: PutPath 3-file NAR → `GetNarIndex` 3 entries with non-empty `file_digest` → second call cache-hit. `// r[verify ...]` |

**Exit:** `.#ci` green.

---

## Phase 3 — Cache-tier infra (parallel with Phase 2; depends only P0548)  ★ FIRST SHIPPED VALUE (U2)

### P0553 — terraform: per-AZ S3 Express directory bucket + dedicated store SG/NodeClass + IAM
**Crate:** `infra` · **Deps:** P0548 · **Complexity:** LOW

**One `aws_s3_directory_bucket` per supported AZ-ID** (`for_each = toset(local.express_az_ids)`). Store pods address the bucket for their own AZ via env (P0554). `TieredChunkBackend` is AZ-count-agnostic — each replica sees exactly one local bucket name (or none); terraform just provisions N of them. **No CSI driver, no PVC, no Lustre kernel module.**

| File | Change |
|---|---|
| `infra/eks/s3-express.tf` | `resource "aws_s3_directory_bucket" "cache" { for_each = toset(local.express_az_ids); bucket = "rio-chunk-cache--${each.key}--x-s3"; location { name = each.key; type = "AvailabilityZone" } }`; IAM policy `s3express:CreateSession` + `s3express:*` attached to the store IRSA role. `// r[impl infra.express.cache-tier]` |
| `infra/eks/outputs.tf` | `express_bucket_by_az_id` map for helm |
| `infra/eks/variables.tf` | `express_az_ids` — intersection of subnet zone-ids with the Express-supported set; empty list → cache tier disabled cluster-wide |

**Exit:** `tofu apply` creates one directory bucket per supported AZ + store SG/NodeClass; `.#ci` green.

### P0554 — helm: chunkBackend.tiered + per-AZ Express bucket env
**Crate:** `infra, xtask` · **Deps:** P0548, P0553 · **Complexity:** LOW
`store.chunkBackend.kind={s3|tiered}` helm value (default `s3`); when `tiered`, `store.chunkBackend.expressBucketByAzId` populated from terraform output. Store Deployment exposes node zone via downward-API env from `topology.kubernetes.io/zone`; container resolves zone→zone-id at startup (IMDS `placement/availability-zone-id`) and selects its bucket; no match → `local=None`. `S3ChunkBackend` for `local` uses zonal endpoint `https://s3express-{az_id}.{region}.amazonaws.com`. **Exit:** `helm template --set store.chunkBackend.kind=tiered` renders; `.#ci` green. ★ FIRST SHIPPED VALUE (U2)

### P0555 — VM test: tiered-backend cache semantics
**Crate:** `nix` · **Deps:** P0548, P0554 · **Complexity:** MED
`nix/tests/scenarios/store-tiered.nix`: two store replicas + two minio instances (one "local" Express stand-in, one shared "remote" S3-standard); subtests `cold-miss-fallback`, `put-remote-first`, `replica-warm-from-peer-write`, `local-none-passthrough`. **Exit:** `nix build .#checks.x86_64-linux.vm-store-tiered` green; `.#ci` green.

### P0579 — `binary_cache_compat` config + helm
**Crate:** `rio-store, infra` · **Deps:** P0544 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-store/src/config.rs` | `pub struct BinaryCacheCompat { enabled: bool /* default true */, bucket: Option<String> /* None → chunk_backend.s3.bucket */, compression: CompatCompression /* Zstd|Xz|None, default Zstd */, write_mode: CompatWriteMode /* SyncAfterCommit only for now */ }`. `// r[impl store.compat.runtime-toggle]` |
| `infra/helm/rio/values.yaml` | `store.binaryCacheCompat.{enabled,bucket,compression}` (default `enabled: true`) |
| `infra/helm/rio/templates/store-deployment.yaml` | env `RIO_STORE__BINARY_CACHE_COMPAT__*` from values |
| `docs/src/configuration.md` | rows added (P0544 also touches; serialise) |

**Exit:** `.#ci` green; `helm template` renders the env block.

### P0566 — binary-cache compat writer (stock-Nix `.narinfo` + `nar/*.nar.zst` to S3-standard)
**Crate:** `rio-store` · **Deps:** P0549, P0579 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-store/src/compat/writer.rs` | new — `async fn write(&self, path_info: &PathInfo, manifest: &ManifestKind) -> Result<(), CompatError>`. Reassemble NAR bytes from `manifest` (chunked: `ChunkCache::get` over the just-written chunks, moka-hot; inline: read `inline_blob`). Stream through `async-compression` zstd encoder while computing `sha256(compressed)`. `put_blob("nar/{file_hash}.nar.zst", body)`. Render narinfo via the existing `rio-nix::narinfo::render` (same one the HTTP server uses) **with** `FileHash`/`FileSize`/`Compression` populated; `put_blob("{store_path_hash}.narinfo", body)`. On first-ever write, `put_blob("nix-cache-info", "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n")` if absent. `// r[impl store.compat.{nar-on-put,narinfo-on-put}]` `// r[impl obs.metric.compat]` |
| `rio-store/src/grpc/put_path.rs` | after the PG-commit (status flips `'uploading'`→`'complete'`), if `cfg.binary_cache_compat.enabled`: `compat_writer.write(...).await` — failure logged + `rio_store_compat_write_failures_total` inc, **does not** fail the RPC. `// r[impl store.compat.write-after-commit]` |
| `rio-store/src/lib.rs` | `pub mod compat;` + `rio_store_compat_write_seconds{result}` histogram + `rio_store_compat_write_failures_total` counter |
| tests | unit: PutPath a 2-chunk path with compat ON → in-memory S3 backend has `{hash}.narinfo` and `nar/{filehash}.nar.zst`; round-trip narinfo via `rio-nix::narinfo::parse`; decompressed NAR sha256 == `nar_hash`. compat OFF → neither object present. `// r[verify store.compat.{nar-on-put,narinfo-on-put,write-after-commit,runtime-toggle}]` |

**Exit:** `.#ci` green.

### P0580 — VM test: stock-Nix substitutes from S3 with rio-store stopped  ★ U6 LANDS
**Crate:** `nix` · **Deps:** P0566 · **Complexity:** MED
`nix/tests/scenarios/store-compat.nix`: rio-store + minio + stock `pkgs.nix`. Subtests `stock-nix-substitute` (PutPath a 3-path closure → `systemctl stop rio-store` → `nix copy --from 's3://rio?endpoint=http://minio:9000&region=dummy' --to /tmp/out /nix/store/…` → verify all 3 paths land + `nix store verify` passes), `compat-off-no-narinfo` (compat=OFF → PutPath → `aws s3 ls` shows `chunks/` only, no `.narinfo`). Wire at `nix/tests/default.nix` `subtests = [ … ]` with `# r[verify store.compat.stock-nix-substitute]` and `# r[verify store.compat.runtime-toggle]`. **Exit:** `nix build .#checks.x86_64-linux.vm-store-compat` green; `.#ci` green.

### P0581 — compat GC integration
**Crate:** `rio-store` · **Deps:** P0566 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-store/src/gc.rs` | per-manifest sweep txn already enqueues chunk keys to `pending_s3_deletes`; extend to also enqueue `{store_path_hash}.narinfo` and `nar/{file_hash}.nar.zst` (file_hash read from the dying narinfo row's `compat_file_hash` column). Runs regardless of current `enabled` value — past compat writes are GC'd even if compat is now OFF. `// r[impl store.compat.gc-coupled]` |
| `migrations/033_nar_index.sql` (P0551 — same migration) | `+ ALTER TABLE narinfo ADD COLUMN compat_file_hash bytea;` (nullable; populated by P0566 on successful compat write) |
| tests | GC a path with compat objects → both keys appear in `pending_s3_deletes`. `// r[verify store.compat.gc-coupled]` |

**Exit:** `.#ci` green.

### P0582 — compat reconciler (deferred-priority)
**Crate:** `rio-store` · **Deps:** P0566, P0581 · **Complexity:** LOW
`rio-store/src/compat/reconciler.rs`: background loop, `SELECT store_path_hash FROM narinfo WHERE compat_file_hash IS NULL AND status='complete' LIMIT 64` → `compat_writer.write(...)` for each → sleep 30s if empty. Handles the crash-between-PG-commit-and-S3-write window and backfills paths ingested while compat was OFF. Spawned in `main.rs` only when `enabled`. **Exit:** `.#ci` green; unit: insert a `compat_file_hash IS NULL` row → one tick → row populated + S3 objects present.

---

## Phase 4 — composefs store-side (gated on Phase-0 PASS + P0546)

### P0556 — `composefs-sys` + `encode.rs` — `libcomposefs` FFI encoder + nix patch + golden VM test
**Crate:** `rio-builder, composefs-sys, nix` · **Deps:** P0569 PASS, P0546 · **Complexity:** LOW (~100 LoC owned + ~25-line C patch)

The encoder is the C `libcomposefs` (Apache-2.0) via Rust FFI — see ADR-022 §2.2. In-process, upstream-maintained (containers/composefs; what podman/ostree ship), spike-measured ~46 ms / 23k files. A small nix-carried patch adds the two flags we need; both upstreamable.

| File | Change |
|---|---|
| `nix/patches/libcomposefs-user-xattr.patch` | ~25 lines against `libcomposefs/{lcfs-internal.h,lcfs-writer.h,lcfs-writer-erofs.c}`: (a) `LCFS_BUILD_USER_XATTR_OVERLAY` flag — when set, `OVERLAY_XATTR_{REDIRECT,METACOPY,OPAQUE}` use the `user.` prefix instead of hardcoded `trusted.`; (b) `LCFS_FLAGS_NO_ROOT_WHITEOUTS` — gates `add_overlay_whiteouts(root)` (:1378) and `set_overlay_opaque(root)` (:1374), which otherwise add 256 `chardev(0:0)` whiteouts `00`..`ff` + a root opaque xattr for OCI layering. Prototype: `~/tmp/spike-libcomposefs-ffi/user-xattr-prefix.patch`. |
| `nix/overlays/composefs.nix` | `final: prev: { composefs = prev.composefs.overrideAttrs (o: { patches = (o.patches or []) ++ [ ../patches/libcomposefs-user-xattr.patch ]; }); }`; wire into `flake.nix` overlays. |
| `rio-builder/composefs-sys/{Cargo.toml,build.rs,src/lib.rs}` | new `-sys` crate. `build.rs` (~18 LoC): `pkg_config::probe("composefs")` for link flags; `bindgen::builder().header("lcfs-writer.h").allowlist_function("lcfs_.*").allowlist_type("lcfs_.*").generate()`. `lib.rs`: `include!(concat!(env!("OUT_DIR"), "/bindings.rs"))`. Dev-shell already provides `LIBCLANG_PATH`. |
| `rio-builder/Cargo.toml` | `+ composefs-sys = { path = "composefs-sys" }` |
| `rio-builder/src/composefs/encode.rs` | `pub fn build_image(roots: &[(StorePath, &NarIndex)]) -> Result<Vec<u8>>` (~80 LoC): `lcfs_node_new()` for `/`, `/nix`, `/nix/store`; for each `(store_path, idx)` insert the store-path node (which **may itself be a regular file or symlink** — root `entry.kind` decides) and recurse. Per node: `lcfs_node_set_mode(match kind { Dir => 0o40555, Regular if executable => 0o100555, Regular => 0o100444, Symlink => 0o120777 })`; `lcfs_node_set_mtime(&timespec{tv_sec:1, tv_nsec:0})`; `lcfs_node_set_uid/gid(0)`. Regular files: `lcfs_node_set_size(entry.size)` + `lcfs_node_set_payload(format!("{:02x}/{}", d[0], hex(&d[1..])))` (the redirect target). Symlinks: `lcfs_node_set_payload(target)`. `lcfs_node_add_child(parent, name_bytes, child)`. Then `opts.flags = LCFS_BUILD_USER_XATTR_OVERLAY \| LCFS_FLAGS_NO_ROOT_WHITEOUTS`; `lcfs_write_to(root, &opts)` with a `write_cb` that appends to a `Vec<u8>` (or memfd). RAII wrapper frees nodes via `lcfs_node_unref`. — overlayfs metacopy surfaces the **EROFS** inode's mode/mtime to `stat()`, so these are what the build sees. `// r[impl builder.fs.{composefs-encode,stub-isize,metacopy-xattr-shape}]` |
| `rio-builder/src/composefs/mod.rs` | `pub mod encode;` |
| `nix/tests/scenarios/composefs-encoder.nix` | standalone (no k8s): fixture NarIndex (3 store paths incl. one whose root is a regular file, ~50 files, one >u32::MAX sparse, one executable, one with non-UTF8 name) → `build_image` → write to tmpfile → `losetup + mount -t erofs` → `find -printf '%s %p\n'` matches NarIndex; `stat -c '%a %Y'` on regular/exec/dir matches `444/555/555` and `1`; `getfattr -n user.overlay.redirect` on a regular file matches `/{digest[..2]}/{digest[2..]}`; **`getfattr -d -m '^trusted'` is empty**; **`find / -maxdepth 1 -type c` is empty** (no whiteout chardevs). Reuses spike harness `nix/tests/lib/spike_stage.py` patterns. |
| `nix/tests/default.nix` | `# r[verify builder.fs.{stub-isize,composefs-encode,metacopy-xattr-shape}]` at `subtests=["golden-loop-mount"]` |
| `rio-builder/fuzz/fuzz_targets/composefs_encode.rs` + wiring | fuzz the adapter: arbitrary `NarIndex` → `build_image` → image is valid EROFS (loop-mount via `memfd_create` + `fsopen("erofs")`) |

**Exit:** `nix build .#checks.x86_64-linux.vm-composefs-encoder` green; `.#ci` green.

### P0557 — PutPath eager `nar_index` compute (no encode)
**Crate:** `rio-store` · **Deps:** P0551, P0552 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-store/src/grpc/put_path.rs` (~431, after `cas::put_chunked` Ok) | `if let Ok(permit) = index_sem.clone().try_acquire_owned() { tokio::spawn(async move { let _p = permit; nar_index::compute_from_bytes(pool, &nar_bytes, store_path).await }) }` — eager only if a permit is *immediately* free; otherwise leave for `indexer_loop` (≤5 s pickup). NAR bytes passed as `Arc<Vec<u8>>`. `index_sem` sized by config `nar_index_concurrency` (default 4). `// r[impl store.index.putpath-eager]` |
| `rio-store/src/grpc/put_path_batch.rs` | same gate |
| `rio-store/src/nar_index.rs` | `compute_from_bytes(pool, &[u8], path)` — `Cursor::new(bytes)` → `nar_ls` → `set_nar_index`. Reuses RAM, no chunk fetch. |
| `rio-store/src/config.rs` | `+ nar_index_concurrency: usize` (default 4) |

**Exit:** `.#ci` green; `vm-protocol-warm` asserts `GetNarIndex` returns within 100 ms after PutPath (eager path hit).

---

## Phase 5 — composefs builder-side

### P0559 — `composefs/{digest_fuse,circuit}.rs`
**Crate:** `rio-builder` · **Deps:** P0545, P0550, P0567, P0568, P0570 · **Complexity:** MED (~500 LoC)
| File | Change |
|---|---|
| `rio-builder/src/composefs/digest_fuse.rs` | `fuser::Filesystem` impl rooted at the per-build mount `/var/rio/objects/{build_id}` exposing exactly two levels: 256 prefix dirs (`00`..`ff`, static inodes 2-257) + leaf files named by remaining 62 hex chars. **Startup**: `setrlimit(RLIMIT_NOFILE, 65536)` — a chromium link opens ~2-3k `.so`/`.a` concurrently; default 1024 would `EMFILE`. **`init`**: `config.set_max_stack_depth(1)` (negotiates `FUSE_PASSTHROUGH`; `fuser` ≥0.17, [lib.rs:254](https://github.com/cberner/fuser/blob/master/src/lib.rs)). `lookup(parent, name)`: parent==ROOT → prefix-dir inode; parent==prefix → parse `[prefix‖name]` as `[u8;32]`, `resolver.resolve(digest)` → `FileAttr{ size, mode: if executable {0o555} else {0o444}, ino: hash-derived }`; unknown → `ENOENT` (declared-input allowlist — JIT-fetch imperative). **`open(ino)`**: look up backing path in **shared node-SSD cache** `/var/rio/cache/{aa}/{rest}` (P0571). **(a) hit** (`/var/rio/cache/` is mountd-owned, builder-readonly) → open cache file O_RDONLY, send fd to rio-mountd UDS (`BackingOpen{fd}`, SCM_RIGHTS) → recv `backing_id: u32` → `let bid = BackingId::create_raw(backing_id); self.backing.insert(fh, backing_id); reply.opened_passthrough(fh, flags, &bid)` ([`fuser::ReplyOpen`/`BackingId::create_raw`](https://github.com/cberner/fuser/blob/master/src/reply.rs); the ioctl runs in mountd because `BACKING_OPEN` needs init-ns `CAP_SYS_ADMIN`). **(b) miss + `size ≤ STREAM_THRESHOLD`** → `O_EXCL`-create `staging/{build_id}/<hex>.partial` + `flock(LOCK_EX)` (within-build concurrent open of same digest: loser `flock(LOCK_NB)` — held → condvar-wait on a per-digest `Notify`, then re-check cache; not held → orphan, unlink + retry); `circuit.call(‖ store_fetch::fetch_chunks_parallel(...))` into `.partial`, **verifying each chunk's blake3 on arrival** (§2.6); whole-file verify; `rename .partial → <hex>`; `Promote{digest}` (mountd opens `staging/<hex>` — no suffix) → as (a). **(c) miss + `size > STREAM_THRESHOLD`** → P0575 streaming path. Chunk source: check `/var/rio/chunks/ab/<chunk_hex>` (RO) first; for misses, `GetChunks` → verify → write `staging/chunks/<chunk_hex>` → fire `PromoteChunk` (fire-and-forget). Assemble into `.partial` from chunks. Reply `FOPEN_KEEP_CACHE` after first chunk; fill+`Promote` in background. **Cross-build dedup is chunk-granular and automatic** — concurrent fillers race on `PromoteChunk`'s rename; no sentinel, no leader. **`read(ino, fh, off, size)`**: only reached for case (c) — `pread` the staging `.partial`; see P0575. **`release(fh)`**: if `self.backing.remove(&fh)` had an id, send `BackingClose{id}` (case-c opens have none). **BackingId LRU**: `self.backing` is an `LruCache` capped at `MAX_BACKING_IDS` (4096); on insert-evict, `BackingClose` the victim (next read of that fd falls back to FUSE `read` from cache file — still correct, just one extra crossing). All mountd UDS sends wrapped in `timeout(mountd_request_timeout)` (default 30 s; `Promote` of a streamed file uses `max(30s, 2×size/(1 GiB/s))`); expiry → `EIO` + infra-retry. Per-open `tokio::time::timeout(jit_fetch_timeout)` on the fetch; expiry → `EIO`. Prototype: `rio-builder/src/bin/spike_digest_fuse.rs` on `adr-022` (`af8db499`). `// r[impl builder.fs.{digest-fuse-open,passthrough-on-hit,file-digest-integrity,shared-backing-cache}]` |
| `rio-builder/src/config.rs` | `+ mountd_request_timeout: Duration` (30 s); `+ disable_passthrough: bool` (env `RIO_DISABLE_PASSTHROUGH` — when true, case (a) replies `FOPEN_KEEP_CACHE` over the cache file with no `BackingOpen`; escape hatch). |
| `rio-builder/src/lib.rs` | `+ rio_builder_digest_fuse_open_mode_total{mode="passthrough"\|"keep_cache"}`; `+ rio_builder_digest_fuse_open_case_total{case="hit"\|"miss_small"\|"miss_stream"}`; `+ rio_builder_digest_fuse_chunk_source_total{src="node_chunks"\|"remote"}` (I-056: expose every gate input). |
| `rio-builder/src/composefs/circuit.rs` | port of `fuse/circuit.rs` — breaker around `fetch_chunks_parallel`. A store outage must trip the breaker, not stream EIOs into every `open()` on the node. `// r[impl builder.fs.fetch-circuit]` |
| `rio-builder/src/composefs/mod.rs` | `pub mod digest_fuse; pub mod circuit; pub mod resolver; pub mod dump; pub mod encode; pub mod mount;` |
| `rio-builder/src/lib.rs` | `pub mod composefs;` + `rio_builder_digest_fuse_{open_seconds,fetch_bytes_total{hit},integrity_fail_total,eio_total}`. `// r[impl obs.metric.digest-fuse]` |
| tests | unit: `lookup` hex parsing round-trip; `resolve(unknown) == ENOENT`. `// r[verify builder.fs.digest-fuse-open]` |

**Mode summary:** cache hit → passthrough (zero further upcalls). Cache miss ≤ threshold → fetch-whole then passthrough. Cache miss > threshold → P0575 streaming during fill, then next open is passthrough. The FUSE `read` op is reachable only in the streaming window.

**Exit:** `.#ci` green (unit only).

### P0567 — `rio-mountd` DaemonSet (fd-handoff + `BACKING_OPEN` broker + `Promote`/`PromoteChunk`)
**Crate:** `rio-builder, infra` · **Deps:** P0576, P0578 · **Complexity:** MED (~250 LoC + helm)

The unprivileged builder cannot (a) loop-mount EROFS (no `FS_USERNS_MOUNT`), (b) open `/dev/fuse`, (c) call `FUSE_DEV_IOC_BACKING_OPEN`/`_CLOSE` (init-ns `CAP_SYS_ADMIN` — [`backing.c:91-93,147-149`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c)), or (d) write the shared cache (integrity boundary). One DaemonSet per node with `CAP_SYS_ADMIN` brokers all four. **No overlay mount, no upcall relay** — builder does FUSE-serve + `move_mount` + overlay itself.

**Concurrency:** tokio multi-thread runtime; one async task per accepted UDS connection. Within a conn, requests are length-prefix-framed and pipelined — `BackingOpen`/`BackingClose`/`PromoteChunk` are answered inline (sub-ms). `Promote` acquires a process-wide `Semaphore(num_cpus)` permit, then runs its copy+hash loop on `tokio::task::spawn_blocking` so it never blocks the conn's `BackingOpen` traffic. `// r[impl builder.mountd.concurrency]`

| File | Change |
|---|---|
| `rio-builder/src/composefs/mountd_proto.rs` | new — UDS wire types shared with P0559. `struct Frame { seq: u32, body: Req\|Resp }` (every reply echoes `seq`; out-of-order replies from `spawn_blocking` `Promote` are correlatable). `enum Req { Mount{image_path, build_id}, BackingOpen{fd: RawFd}, BackingClose{id: u32}, PromoteChunks{chunk_digests: Vec<[u8;32]>}, Promote{digest: [u8;32]} }`. `enum Resp { Mounted{fuse_fd, erofs_fd, staging_quota_bytes: u64}, BackingId(u32), Promoted, Err(ErrKind) }`. `enum ErrKind { Retryable(String), DigestMismatch, NotRegular, TooLarge, RaceTimeout }` — builder maps `DigestMismatch`/`NotRegular`/`TooLarge` to **build-failure** (not infra-retry; re-fetch would loop). Length-prefix bincode framing; client holds `HashMap<u32, oneshot::Sender<Resp>>`. |
| `rio-builder/src/bin/rio-mountd.rs` | new — listens on `/run/rio-mountd.sock` (mode 0660, group `rio-builder`); rejects connections where `SO_PEERCRED.gid != rio-builder`. Owns `/var/rio/cache/` (0755, files 0444). Per accepted connection: **`Mount{image_path, build_id}`** → `fuse_fd = open("/dev/fuse", O_RDWR)`; **`kept = dup(fuse_fd)`** stored in conn state; `mount("none", "/var/rio/objects/{build_id}", "fuse.rio-digest", MS_NODEV\|MS_NOSUID, "fd=<fuse_fd>,rootmode=40555,user_id=<peer_uid>,group_id=<peer_gid>,allow_other,default_permissions")`; `mkdir -p /var/rio/staging/{build_id}/chunks` chown peer_uid; `conn.staging_dirfd = open(staging/{build_id}, O_DIRECTORY)`, `conn.staging_chunks_dirfd = openat(staging_dirfd, "chunks", O_DIRECTORY)`; `loop_dev = losetup_ro(image_path)`; `fsopen("erofs")` → `fsconfig(SET_STRING,"source",loop_dev)` → **`fsconfig(SET_FLAG,"ro")`** → `fsconfig(CMD_CREATE)` → `erofs_fd = fsmount(_, 0, MOUNT_ATTR_NODEV)`; reply `[fuse_fd, erofs_fd]` via SCM_RIGHTS; close sent copies. **`BackingOpen{fd}`** (fd via SCM_RIGHTS) → `ioctl(kept, FUSE_DEV_IOC_BACKING_OPEN, &fuse_backing_map{fd, flags:0}) → backing_id`; reply `backing_id` (mountd does not inspect the fd; the ioctl rejects depth>0 backing and `backing_id` is conn-scoped). **`BackingClose{id}`** → ioctl. **`PromoteChunks{chunk_digests}`** (batched, ≤64/batch) → for each: `openat(conn.staging_chunks_dirfd, hex, O_RDONLY\|O_NOFOLLOW)`; reject `!S_ISREG` or `st_size > FASTCDC_MAX_BYTES` (`rio-store/src/chunker.rs` constant — must match); read-all + verify `blake3 == chunk_digest`; write `/var/rio/chunks/ab/{hex}.tmp` 0444; rename (on `EEXIST` → already promoted, fine); unlink staging. One `Promoted` reply per batch. Runs inline. `// r[impl builder.fs.node-chunk-cache]` **`Promote{digest}`** → `src = openat(conn.staging_dirfd, hex, O_RDONLY\|O_NOFOLLOW)`; `fstat(src)` — reject `!S_ISREG` or `st_size > RIO_MOUNTD_MAX_PROMOTE_BYTES` (default 4 GiB). Create `cache/ab/{hex}.promoting` `O_EXCL\|O_WRONLY` 0444 — on `EEXIST`, stat `cache/ab/{hex}`: exists → reply `Promoted`; else inotify-wait ≤2 s then re-stat (or `Err("promote-race-timeout")`). Copy loop: `read(64 KiB)` with per-call `timeout(5s)` → `hasher.update` → `write`. Verify `hasher.finalize() == digest` else `unlink .promoting` + `Err("digest-mismatch")` + `promote_reject_total{reason="mismatch"}.inc()`. `rename .promoting → final`; `unlinkat(staging_dirfd, hex)`; reply `Promoted`. **On UDS conn-drop:** `umount2(objects_dir, MNT_DETACH)` + `losetup -d` + `rmdir(objects_dir)` + `rm -rf staging/{build_id}` + `close(kept)`. **Start-up:** scan `/var/rio/{objects,staging}/*` and `/var/rio/{cache,chunks}/**/*.{promoting,tmp}` for orphans; reap. `// r[impl builder.mountd.{erofs-handoff,backing-broker,promote-verified,orphan-scan}]` |
| `infra/helm/rio-build/templates/mountd-ds.yaml` | new — DaemonSet, hostPath `/run/rio-mountd.sock` + `/var/rio/{images,cache,staging,objects}` + `/dev/fuse`. `securityContext: {privileged: false, capabilities.add: [SYS_ADMIN]}`, `runAsUser: 0`, seccomp `RuntimeDefault`. Builder pods get `fsGroup: rio-builder` for socket access. nodeSelector: builder/fetcher nodepools. |
| `docs/src/components/builder.md` | `r[builder.mountd.{erofs-handoff,backing-broker,promote-verified,orphan-scan}]` spec text |

**Exit:** `.#ci` green; exercised end-to-end by P0560§B.

### P0571 — mountd-owned cache LRU sweep + staging-dir lifecycle + cache-hit metrics
**Crate:** `rio-builder, infra` · **Deps:** P0559, P0567 · **Complexity:** LOW

`r[builder.fs.shared-backing-cache]` + `r[builder.fs.node-chunk-cache]`: the **backing cache** (`/var/rio/cache/ab/<file_digest>`) and **chunk cache** (`/var/rio/chunks/ab/<chunk_digest>`) are mountd-owned, builder-readonly; builders stage to per-build `/var/rio/staging/{build_id}/` and `Promote`/`PromoteChunk` (P0567). Cross-build dedup for >threshold files is chunk-granular via the chunk cache.

| File | Change |
|---|---|
| `rio-builder/src/bin/rio-mountd.rs` | mountd owns `/var/rio/{cache,chunks}/` (P0567); this plan adds the LRU sweep: periodic `statvfs` on each of `/var/rio/{cache,chunks,staging}` (may be separate partitions) — if `min(free%) < 10%`, atime-ordered `readdir` + `unlink` over `chunks/` first (intermediate, regenerable), then `cache/` (passthrough targets), until `min(free%) > 20%`. **Sweep also covers `/var/rio/staging/*`** (orphaned staging from crashed builds). **Cache, chunks, staging dirs MUST be on a non-stacking fs** (ext4/xfs; `r[builder.fs.passthrough-stack-depth]`). The disk-ownership freedom may be used to put `/var/rio/chunks/` on a dedicated partition to isolate IOPS from the build's overlay-upper. `// r[impl builder.fs.node-digest-cache]` |
| `rio-builder/src/composefs/digest_fuse.rs` | `rio_builder_objects_cache_{hit_total,bytes}` metrics. |
| `infra/helm/rio-build/templates/builder-sts.yaml` | hostPath `/var/rio/cache` mounted **RO**; `/var/rio/staging` and `/var/rio/objects` RW |
| `nix/nixos-node/eks-node.nix` | `systemd.tmpfiles.rules = ["d /var/rio/cache 0755 root root -" "d /var/rio/staging 0755 root root -" "d /var/rio/objects 0755 root root -"]` |

**FSx-backed cluster-wide cache rejected** — violates builder air-gap: a shared writable FS across untrusted builders is a cache-poisoning + lateral-movement surface. The same logic motivates mountd-owned per-node cache.

**Exit:** `.#ci` green.

### P0560 — [ATOMIC] composefs lower cutover: mount + DELETE old-FUSE + fixture kernel + VM test  ★ HARD CUTOVER
**Crate:** `rio-builder, nix` · **Deps:** P0576, P0556, P0557, P0559, P0567, P0571, P0575 · **Complexity:** HIGH (two-part atomic)

**One worktree, one PR, one `.#ci` gate.** §A alone breaks every existing VM test (fixtures lack `kernel.nix`; existing scenarios assert old-FUSE metrics); §B alone has nothing to test.

#### §A — `rio-builder`: mount.rs + overlay composefs lower + delete old-FUSE
**Complexity:** MED (add) + LOW (delete)
| File | Change |
|---|---|
| `rio-builder/src/composefs/mount.rs` | `mount_composefs_background(mount_point, objects_dir, image_dir, closure: &[(NarHash, NarIndex, ChunkList)], uds, clients, rt) -> ComposefsMount` — (1) build `DigestResolver::new(closure)`; (2) `composefs::encode::build_image(closure, image_dir/{build_id}.erofs)`; (3) connect `rio-mountd` UDS, send `Mount{image_path}`, recv `[fuse_fd, erofs_fd]` via SCM_RIGHTS; (4) **spawn `digest_fuse::serve(fuse_fd, objects_dir, resolver, clients)` and wait for ready — MUST be serving before step 6** (overlayfs probes lowers at `mount(2)`; an unserved FUSE deadlocks the mounter — P0541 ordering gotcha); (5) `move_mount(erofs_fd, "", AT_FDCWD, meta_mnt, MOVE_MOUNT_F_EMPTY_PATH)`; (6) `mount("overlay", mount_point, "overlay", MS_RDONLY, "ro,userxattr,lowerdir={meta_mnt}::{objects_dir}")` in builder userns — **NO explicit `metacopy=on`/`redirect_dir=on`** (rejected under `userxattr`, `params.c:988-1008`; the `::` data-only lower independently enables redirect-following — gated on `ofs->numdatalayer > 0` per `namei.c:1241` / `5ef7bcdeecc9` v6.16+, not `config->metacopy`). `Drop`: `umount2(overlay, MNT_DETACH)` → `umount2(meta_mnt)` → close UDS (mountd `losetup -d` on conn-drop) → abort FUSE task (any blocked `open()` wakes `ENOTCONN`, interruptible — no D-state). Hard-fail with actionable error if UDS connect fails (`"rio-mountd not running on this node — is the DaemonSet (P0567) deployed?"`) or any input's `NarIndex` is empty (`"store has not indexed {nar_hash} — is P0557 deployed? GetNarIndex returned 0 entries"`). `// r[impl builder.fs.composefs-stack]` |
| `rio-builder/src/executor/inputs.rs` | unconditionally: `BatchGetManifest` + `GetNarIndex` for closure → `mount_composefs_background`. Delete the `cache.register_inputs(...)` JIT block. |
| `rio-builder/src/executor/mod.rs` | **PORT** `is_input_materialization_failure`: recognise `EIO` from digest-FUSE `open()` (fetch failure or integrity fail) + breaker-tripped state as infra-retry, not derivation-failure. `// r[impl builder.result.input-eio-is-infra]` |
| `rio-builder/src/overlay.rs` (~214) | `OverlayMount::new(lower: ComposefsMount)` — single concrete type. `// r[impl builder.overlay.composefs-lower]` |
| `rio-builder/src/main.rs` | drop `mount_fuse_background()` call site; drop `fuse_cache` construction |

**Deletion inventory** (cutover earns back code):

| Path / symbol | Why it can go | ~LoC |
|---|---|---|
| `rio-builder/src/fuse/ops.rs` | old-FUSE `Filesystem` impl — EROFS+overlay in kernel does metadata; digest-FUSE (P0559) is a 2-level flat dir, not a store-path tree | 786 |
| `rio-builder/src/fuse/cache.rs` | `Cache`, `JitClass`, `known_inputs`/`register_inputs` — the metadata image IS the allowlist; `DigestResolver` is the new gate | 1356 |
| `rio-builder/src/fuse/mod.rs` (most) | `mount_fuse_background`, `FuseMount`, `NixStoreFs`. **`ensure_fusectl_mounted` and Drop fusectl-abort are KEPT** (moved to `composefs/digest_fuse.rs` — same I-165 abort discipline) | ~450 |
| `rio-builder/src/fuse/{inode.rs,lookup.rs}` | inode bookkeeping + name lookup — EROFS in kernel | 254+91 |
| `rio-builder/src/fuse/circuit.rs` | **PORTED** to `composefs/circuit.rs` (P0559) | (moved) |
| `rio-builder/src/fuse/read.rs` | passthrough fd registration — page cache via overlay | (whole file) |
| `rio-builder/src/fuse/fetch.rs` old-FUSE wrappers | `ensure_cached`, `prefetch_path_blocking` — P0550 hoisted keepers | ~1700 residual |
| `rio-builder/src/executor/mod.rs` `RIO_BUILDER_JIT_FETCH` block | I-043 escape hatch — old-FUSE-specific | ~40 |
| spec markers | `r[builder.fuse.{jit-lookup,jit-register,lookup-caches+2,fetch-chunk-fanout,fetch-bounded-memory}]`. `r[builder.result.input-enoent-is-infra+2]` REWORDED → `input-eio-is-infra`. | docs |
| `infra/helm/rio-build/templates/karpenter.yaml` `rio-builder-{fuse,kvm}` NodeOverlays | **DROPPED** — both existed to advertise `smarter-devices/*` capacity. fuse: rio-mountd fd-passes. kvm: hostPath + `nodeSelector{rio.build/kvm}` (the metal NodePool already labels+taints; capacity is unbounded so no overlay needed). | helm |
| `values.yaml` `fuseCacheSize` + `infra/helm/crds/builderpools.rio.build.yaml:152` + `templates/builderpool.yaml:24` + `values/vmtest-full.yaml:151` + `rio-controller` `BuilderPoolSpec` field + `fixtures.rs:173`/`apply_tests.rs:404`/`disruption_tests.rs:70` | digest-cache dir is node-level hostPath (P0571), not per-pool | helm+CRD+tests |
| `templates/networkpolicy.yaml:67` `builderS3Cidr` egress carve-out | presigned-URL fetch path gone; builder is pure rio-store gRPC | helm |

**Net:** ~**−4 600 LoC**. The `rio-builder/src/fuse/` directory reduces to nothing; `rio-builder/src/composefs/` is ~700 LoC total.

#### §B — `nix`: fixture kernel cutover + vm:composefs end-to-end
**Complexity:** HIGH

| File | Change |
|---|---|
| `nix/tests/fixtures/k3s-prod-parity.nix` | unconditionally `imports = [ ../../nixos-node/kernel.nix ]`; deploy `rio-mountd` DS in-cluster; hostPath `/var/rio/{objects,images}` |
| `nix/tests/scenarios/composefs-e2e.nix` | fixture `{storeReplicas=1;}`. `cold-read`: build drv that `dd bs=4k count=1` from a 100 MB input → assert `digest_fuse_open_seconds_count > 0` AND `dd` output correct AND streaming mode hit (>threshold). `warm-read`: second `dd` same file → `open_seconds_count` unchanged AND **`digest_fuse_read_upcalls_total` unchanged** (passthrough — no read upcalls). `passthrough-small`: `dd` a 1 MiB input twice → both opens reply passthrough; assert `read_upcalls_total == 0` across both. `cross-build-dedup`: two drvs with one shared input file → second build's `fetch_bytes_total{hit="node_ssd"} > 0`. `eio-on-fetch-fail`: stop rio-store mid-open → opener sees `EIO` (not hang) within `jit_fetch_timeout` + `is_input_materialization_failure` classifies as infra-retry. `integrity-fail`: corrupt one chunk in the store backend → opener sees `EIO` + `integrity_fail_total == 1`. `stat-kernel-side`: `find /nix/store -type f -printf '%s\n' \| sha256sum` matches expected with `digest_fuse_open_seconds_count == 0` (no upcalls for stat-walk). `cross-build-dedup-streaming`: launch two builds **concurrently** sharing one >threshold input → assert build-B's `chunk_source_total{src="remote"}` × `FASTCDC_MAX` < input size (most chunks came from `/var/rio/chunks/`). `mountd-restart`: kill mountd mid-build, assert orphan-scan reaps `objects/`+`staging/` on restart and next build succeeds. `cache-readonly`: from inside the build sandbox, `open("/var/rio/cache/ab/test", O_WRONLY\|O_CREAT)` → `EACCES`. |
| `nix/tests/scenarios/{lifecycle,protocol,gc,...}.nix` | **sweep:** delete every old-FUSE-specific assertion (`fuse_cache_hits`, `/var/rio/fuse-store`). **Drop all `smarter-devices/*` from worker pod fixtures** — fuse via rio-mountd fd-pass, kvm via hostPath. |
| `nix/tests/default.nix` | `# r[verify builder.fs.{composefs-stack,userxattr-mount,fd-handoff-ordering,digest-fuse-open,shared-backing-cache,file-digest-integrity,node-digest-cache,digest-resolve,streaming-open-threshold}]` `# r[verify builder.overlay.composefs-lower]` `# r[verify builder.result.input-eio-is-infra]` `# r[verify builder.mountd.erofs-handoff]` `# r[verify obs.metric.digest-fuse]` at `subtests=[...]`; spike harness `nix/tests/{scenarios/composefs-spike*.nix, lib/spike_stage.py, lib/chromium-tree.tsv.zst}` consolidated on `adr-022` (`15a9db79`) as regression guards; `timeout=1800` |

**Exit (whole P0560):** `nix build .#checks.x86_64-linux.vm-composefs-e2e` green; full `.#ci` green with composefs as the only lower.

### P0562 — Post-cutover audit  ★ CUTOVER GATE (U1)
**Crate:** `nix` · **Deps:** P0560 · **Complexity:** LOW

| Check | How |
|---|---|
| No old-FUSE markers remain | `tracey query rule builder.fuse.*` returns empty |
| No old-FUSE / device-plugin strings in code/helm | `grep -rn 'fuse_cache\|/var/rio/fuse-store\|fuseCacheSize\|NixStoreFs\|smarter-devices\|smarter-device-manager\|rio-builder-fuse\|fuseMaxDevices\|kvmMaxDevices' rio-*/ infra/ nix/` returns empty |
| No stray cachefiles/boot-blob strings | `grep -rn 'cachefiles\|CACHEFILES\|boot_blob\|boot_size' rio-*/ infra/ nix/ docs/src/components/` returns empty |
| Parity | full `.#ci` re-run; `# r[verify builder.fs.parity]` on `lifecycle` |

**Exit:** all four checks pass; `.#ci` green.

---

## Phase 6 — Observability + finalize

### P0563 — metrics + dashboard + alerts
**Crate:** `infra` · **Deps:** P0544, P0548, P0559 · **Complexity:** LOW
| File | Change |
|---|---|
| `infra/helm/rio-build/dashboards/composefs.json` | panels: `digest_fuse_open_seconds` p50/p99, `fetch_bytes_total` rate by `hit` label, `objects_cache_bytes` per node, `integrity_fail_total`, `narindex_compute_seconds` |
| `infra/helm/rio-build/templates/prometheusrule.yaml` | `RioBuilderDigestFuseStall`: `increase(open_seconds_count[2m]) == 0 AND increase(open_seconds_sum[2m]) > 0 for 60s` (opens started but none completed). `RioBuilderIntegrityFail`: `increase(integrity_fail_total[5m]) > 0`. `RioStoreNarIndexBacklog`: `narindex_pending > 1000 for 10m`. |
| `xtask/src/regen/grafana.rs` | include dashboard |

**Exit:** `.#ci` green; `xtask grafana` shows dashboard.

### P0564 — helm cleanup + mountd DS wiring + kernel-feature assertion
**Crate:** `infra` · **Deps:** P0554, P0560, P0567 · **Complexity:** LOW
| File | Change |
|---|---|
| `infra/helm/rio-build/templates/_helpers.tpl` | Unconditional helm assertion: `{{- if and .Values.karpenter.enabled (not (has "OVERLAY_FS_DATA_ONLY" .Values.karpenter.amiKernelFeatures)) }}{{ fail "AMI must be built with nix/nixos-node/kernel.nix (≥6.16, OVERLAY_FS=y); run xtask ami push" }}{{- end }}`. |
| `infra/helm/rio-build/values.yaml` | delete `fuseCacheSize`, `builderS3Cidr`, **entire `devicePlugin.*` block** (`{fuse,kvm}MaxDevices`, `image`); add `mountd.{image}`, `objectsCache.{hostPath,lowWatermarkPct,highWatermarkPct}`; `karpenter.amiKernelFeatures: [...]` |
| `infra/helm/rio-build/templates/karpenter.yaml` | delete **both** `rio-builder-{fuse,kvm}` NodeOverlays (capacity advertisement for resources no pod requests). Metal NodePool keeps its `rio.build/kvm: "true"` label+taint — that is the nodeSelector target. |
| `infra/helm/rio-build/templates/device-plugin.yaml` + `nix/nixos-node/smarter-device-manager/` | **DELETED** — no consumers. fuse via fd-handoff; kvm via hostPath. |
| `infra/helm/rio-build/templates/NOTES.txt` | drop the smarter-devices section. |
| `infra/helm/rio-build/values/vmtest-full-nonpriv.yaml` | drop the device-plugin re-enable block (lines ~73-77). |
| `rio-controller/src/reconcilers/common/sts.rs` | builders/fetchers stay **`privileged: false`** unconditionally; mount `rio-mountd` UDS hostPath + `/var/rio/{objects,images}` hostPaths. **Drop all `resources.limits."smarter-devices/*"`.** kvm-pool pods: add `volumes: [{name: kvm, hostPath: {path: /dev/kvm, type: CharDevice}}]` + matching `volumeMounts` + `nodeSelector: {rio.build/kvm: "true"}` + toleration for the metal taint. |
| `rio-builder` nix.conf (or executor sandbox setup) | kvm-pool only: `extra-sandbox-paths = ["/dev/kvm"]`, `system-features += "kvm"`. Spike-verified (`vm-kvm-hostpath-spike`): sandboxed `requiredSystemFeatures=["kvm"]` build can `ioctl(KVM_GET_API_VERSION)`. |
| `flake.nix` helm-lint | drop `fuseCacheSize` parity assertion; add `amiKernelFeatures`-populated assertion |

**Exit:** `helm template` renders; `.#ci` green.

### P0565 — Cutover runbooks
**Crate:** `docs` · **Deps:** P0555, P0562, P0564 · **Complexity:** LOW
| File | Change |
|---|---|
| `docs/src/runbooks/tiered-cache-cutover.md` | new — flip `store.chunkBackend.kind=tiered`; rollback `kind=s3` |
| `docs/src/runbooks/mountd-crash-loop.md` | symptom: `kube_pod_container_status_restarts_total{container="rio-mountd"}` rising + node's builds `EIO`. Action: `kubectl logs -p`; if persistent, cordon node, drain builders, capture `/var/rio/{cache,staging}` listing. |
| `docs/src/runbooks/promote-reject-nonzero.md` | symptom: `rio_mountd_promote_reject_total{reason="mismatch"} > 0` — a builder presented bytes that don't hash to the claimed digest (rio-store corruption or compromised builder). Action: identify `build_id` from mountd log; check `rio_store_integrity_fail_total`; if store clean, treat the builder pod as suspect — cordon node, preserve staging dir for forensics. |
| `docs/src/runbooks/single-node-builds-slow.md` | triage tree: (1) `open_mode_total{mode="passthrough"} == 0` → kernel/init negotiation failed, check `dmesg`; (2) `promote_inflight` pegged → Promote backlog, check `cache_free_bytes`; (3) `mountd_request_seconds{op="backing_open"}` p99 > 1 ms → mountd CPU-starved; (4) else → upstream (`fetch_bytes_total{hit="remote"}` rate vs `rio_store_*`). |
| `docs/src/runbooks/composefs-cutover.md` | (1) ensure cache-tier flip done; (2) `xtask k8s eks down && up` from a P0562-green commit (greenfield — `nar_index` populates from scratch via PutPath eager + indexer_loop); (3) `xtask stress chromium`; (4) compare `fetch_bytes_total{hit="remote"}` — expect ≥10× reduction vs whole-NAR baseline on builds that touch <10% of files; expect `objects_cache_hit_ratio` climbing on repeat builds; (5) rollback = `down && up` from pre-P0560 commit |

**Exit:** `.#ci` green.

### P0575 — §2.7 mitigation (i): streaming `open()` for large files
**Crate:** `rio-builder` · **Deps:** P0559, P0570, P0571 · **Complexity:** LOW (~80 LoC) · **Priority: same tier as P0559**

**Unconditional** — top1000.csv shows all 1000 largest nixpkgs files >64 MiB (248 `.so`/`.a`, max 1.88 GiB); access-probe `42aa81b2` shows real consumers touch 0.3-33% (bimodal head+tail or scattered); spike `15a9db79` proves the mechanism works.

| File | Change |
|---|---|
| `rio-builder/src/composefs/digest_fuse.rs` | **The during-fill mode** for P0559's case (c) — `size > STREAM_THRESHOLD` on cache miss. `open()` spawns fill task, returns `FOPEN_KEEP_CACHE` after the **first chunk** lands. **Chunk source (per chunk):** `open("/var/rio/chunks/ab/<chunk_hex>", O_RDONLY)` — success → write into `.partial` at offset; `ENOENT` → `GetChunks`, verify `blake3==chunk_digest`, write into `.partial` at offset **and** into `staging/chunks/<chunk_hex>`, append digest to a per-fill `Vec<[u8;32]>`. Every 32 chunks or at EOF: `PromoteChunks{batch}` (await reply, but assembly continues from own staging — `PromoteChunks` is purely for *other* builds; this build never reads `/var/rio/chunks/` for chunks it just fetched). **Staging quota**: track `staging_bytes`; if > `Mounted.staging_quota_bytes`, evict oldest `staging/chunks/*` (re-readable from `/var/rio/chunks/`). `read(off,len)`: filled → serve from `.partial`; else priority-bump and condvar-wait. On completion → whole-file blake3-verify → `rename .partial → <hex>` → `Promote{digest}`. Next `open()` is P0559 case (a). Prototype: `spike_stream_fuse.rs` (`15a9db79`). `// r[impl builder.fs.{streaming-open,node-chunk-cache}]` |
| `rio-builder/src/composefs/tests/stream.rs` | unit harness adapted from `spike_stream_fuse.rs`: tmpfs staging + mock mountd; assert `open()` of synth 32 MiB returns <50 ms with first-chunk landed; second open after fill is passthrough (read upcalls = 0). **Orphan**: pre-create unlocked `staging/<hex>.partial` → `open()` unlinks + refetches. |
| same | This IS the per-read-upcall behavior ADR-022 §1 rejected for the warm path — but it applies **only during the cold-fill window of the first open of a large file on that node**. After fill: **0 upcalls while pages remain cached**; under cgroup memory pressure evicted pages re-upcall and are re-served from the SSD backing file. The fill window cost is exactly `filesize / 128 KiB` upcalls, once. |
| `rio-builder/src/config.rs` | `stream_threshold_bytes: u64` (default `8 * 1024 * 1024`). |

**Exit:** `cargo nextest run -p rio-builder composefs::tests::stream` green (unit harness); `vm-composefs-e2e cold-read` is the integration check at P0560.

---

## Phase 7 — Directory DAG / delta-sync (U5; parallel with Phases 4-6 after P0546+P0572)

### P0573 — DirectoryService RPC surface
**Crate:** `rio-proto, rio-store` · **Deps:** P0572 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-proto/proto/store.proto` | `rpc GetDirectory(GetDirectoryRequest) returns (Directory)`; `rpc HasDirectories(HasDirectoriesRequest) returns (HasBitmap)`; `rpc HasBlobs(HasBlobsRequest) returns (HasBitmap)` — all batch (`repeated bytes digests = 1`; `HasBitmap { bytes bitmap = 1; }` one bit per request index, per **I-110 lesson**: per-digest unary against a 50k-node DAG is the PG wall again). Wire-compatible with snix `castore.proto`'s `DirectoryService.Get` where it overlaps. `// r[impl store.castore.directory-rpc]` |
| `rio-store/src/grpc/directory.rs` | new — all queries **tenant-scoped via junction** (`r[store.castore.tenant-scope]`): `get_directory(digest)`: `SELECT body FROM directories d JOIN directory_tenants t USING(digest) WHERE d.digest=$1 AND t.tenant_id=$2` (NotFound otherwise — body leaks child names/digests). `has_directories(digests)`: `SELECT d.digest FROM directories d JOIN directory_tenants t USING(digest) WHERE d.digest=ANY($1) AND t.tenant_id=$2` → bitmap. `has_blobs(file_digests)`: same shape via `file_blob_tenants`. `tenant_id` from JWT `Claims.sub`; fail-closed `UNAUTHENTICATED` if absent. |
| `migrations/033_nar_index.sql` (via P0572) | `CREATE TABLE file_blobs(digest bytea PRIMARY KEY, nar_hash bytea NOT NULL, nar_offset bigint NOT NULL, refcount integer NOT NULL DEFAULT 0); CREATE TABLE file_blob_tenants (digest bytea NOT NULL REFERENCES file_blobs(digest) ON DELETE CASCADE, tenant_id uuid NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE, PRIMARY KEY (digest, tenant_id)); CREATE INDEX file_blob_tenants_tenant_idx ON file_blob_tenants (tenant_id, digest);` — populated in P0572's bottom-up pass via refcount-UPSERT (sorted UNNEST) + tenant-junction insert. **GIN-on-`nar_index.entries` is not viable**: `entries` is BYTEA (encoded proto), not JSONB, so a GIN expression index would require a proto-decoding `IMMUTABLE` PG function tied to the wire format (versioning hazard). The separate table is the derived index for `HasBlobs` AND carries the `(nar_hash, nar_offset)` coords P0574's `dag_sync` and P0577's `ReadBlob` key on. |
| `rio-store/src/lib.rs` | `rio_store_directory_{get_seconds,has_batch_size}` |
| tests | ephemeral PG: PutPath nested tree as tenant-A → `GetDirectory(root_digest)` returns correct children; `HasDirectories([root, unknown])` → `[1,0]` bitmap. **Cross-tenant denial**: tenant-B `HasDirectories([root])` → `[0]`; tenant-B `GetDirectory(root)` → NotFound; tenant-B `ReadBlob(file_digest)` → NotFound. `// r[verify store.castore.{directory-rpc,tenant-scope}]` |

**Exit:** `.#ci` green.

### P0577 — `BlobService.Read(file_digest)` server-stream
**Crate:** `rio-proto, rio-store` · **Deps:** P0573 · **Complexity:** LOW (~40 LoC)

Completes the snix-compatible castore surface: a client holding only a `file_digest` (from a `Directory` body) can fetch the bytes without knowing rio's chunk layout.

| File | Change |
|---|---|
| `rio-proto/proto/store.proto` | `rpc ReadBlob(ReadBlobRequest) returns (stream BlobChunk)` — `ReadBlobRequest { bytes file_digest = 1; }`, `BlobChunk { bytes data = 1; }`. Wire-compatible with snix `castore.proto BlobService.Read`. `// r[impl store.castore.blob-read]` |
| `rio-store/src/grpc/directory.rs` | `read_blob(file_digest)`: `SELECT nar_hash, nar_offset FROM file_blobs f JOIN file_blob_tenants t USING(digest) WHERE f.digest=$1 AND t.tenant_id=$2` → resolve to chunk-range via the manifest's chunk cumsum (same `partition_point` as P0570) → stream via existing `GetChunks` machinery, slicing first/last chunk to the file boundary. NotFound if no tenant-scoped row. |
| tests | ephemeral PG: PutPath → `ReadBlob(file_digest)` body == original file content; `blake3(body) == file_digest`. `// r[verify store.castore.blob-read]` |

**Exit:** `.#ci` green.

### P0574 — Gateway substituter: Directory-DAG delta-sync client  ★ U5 LANDS
**Crate:** `rio-gateway` · **Deps:** P0573, P0577 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-gateway/src/substitute/dag_sync.rs` | new — `async fn sync_closure(local: &dyn LocalStore, remote: StoreClient, roots: &[StorePath])` → for each root, `GetNarIndex` → `root_digest`. BFS the Directory DAG: batch `HasDirectories(frontier)` against **local** store; for present digests, prune subtree; for absent, `GetDirectory(d)` → enqueue child dir digests + collect child `file_digest`s. After BFS: batch `HasBlobs(collected_file_digests)` against local; for absent, fetch via `GetChunks` (P0568) keyed by `DigestResolver`-style coords (P0573's `file_blobs` table provides them). Reassemble NARs locally from materialized blobs + Directory tree (NAR is derived, à la snix nar-bridge). `// r[impl gw.substitute.dag-delta-sync]` |
| `rio-gateway/src/substitute/mod.rs` | `nix copy --from rio://` path: if remote advertises `directory-service` capability AND closure `root_digest` is available, use `dag_sync`; else fall through to chunk-list path (today's behavior). |
| `rio-gateway/src/lib.rs` | `rio_gateway_dagsync_{subtrees_pruned_total,blobs_fetched_total,bytes_saved_total}` |
| `nix/tests/scenarios/dag-delta-sync.nix` | two-store fixture: store-A has closure v1; store-B has closure v2 (one file changed in a deep subdir). `nix copy --from rio://store-B` on store-A → assert `subtrees_pruned_total > (total_dirs × 0.9)` AND `blobs_fetched_total == 1`. |
| `nix/tests/default.nix` | `# r[verify gw.substitute.dag-delta-sync]` |

**Exit:** `.#ci` green; VM scenario demonstrates O(changed-subtrees) discovery.

---

## `onibus dag append` rows

```jsonl
{"plan":576,"title":"EXT: nixos-cutover landed (kernel.nix ≥6.16 importable + /dev/fuse + AMI; OVERLAY_FS/EROFS_FS/FUSE_FS/FUSE_PASSTHROUGH =y)","deps":[],"crate":"ext","priority":99,"status":"RESERVED","complexity":null,"note":"sentinel; coordinator flips DONE when nixos-cutover agent merges. ≥6.16 for 5ef7bcdeecc9 (data-only-lower redirect under userxattr); subsumes ≥6.9 FUSE_PASSTHROUGH"}
{"plan":569,"title":"SPIKE sentinel: composefs-style validated at chromium scale (mount<10ms, warm=0 upcalls, mkcomposefs i_size correct)","deps":[],"crate":"spike","priority":99,"status":"DONE","complexity":null,"note":"consolidated 15a9db79 on adr-022; ADR-022 reopened (4a716900..b6794962)"}
{"plan":541,"title":"SPIKE: composefs privilege boundary (userns-overlay/erofs-loop-unpriv/fsmount-handoff-erofs/fuse-dev-fd-handoff/teardown-under-load)","deps":[],"crate":"spike,nix","priority":95,"status":"DONE","complexity":"MED","note":"all 6 PASS, kernel 6.18.20; commit af8db499 on adr-022; overlay stays in builder via userxattr"}
{"plan":578,"title":"SPIKE: passthrough-under-overlay (depth=2 mount; unpriv BACKING_OPEN→EPERM; brokered ioctl on dup'd /dev/fuse; reads-survive-kill; Promote integrity)","deps":[541],"crate":"spike,nix","priority":95,"status":"UNIMPL","complexity":"LOW","note":"extends composefs-spike-priv.nix; gates P0559/P0567 design"}
{"plan":543,"title":"V4/V11/V12 + closure-paths + aarch64 kernel-config sanity","deps":[],"crate":"xtask,nix","priority":90,"status":"UNIMPL","complexity":"LOW","note":"V12 tunes STREAM_THRESHOLD (P0575 ships unconditionally); closure_paths<65535 + max_nar_size gates REMOVED"}
{"plan":544,"title":"Spec scaffold: ADR-022 §2 + design-overview + ADR-023 (per-AZ tiered) + r[...] markers","deps":[],"crate":"docs","priority":95,"status":"UNIMPL","complexity":"LOW","note":"merges adr-022 (9 builder.fs.* markers); tracey markers MUST precede r[impl]"}
{"plan":545,"title":"proto: NarIndex (+file_digest) / GetNarIndex","deps":[544],"crate":"rio-proto","priority":90,"status":"UNIMPL","complexity":"LOW","note":"no boot_blob"}
{"plan":546,"title":"rio-nix streaming nar_ls (Read-only single-pass; offset-tracking + blake3-per-file) + fuzz","deps":[544,545],"crate":"rio-nix","priority":90,"status":"UNIMPL","complexity":"MED","note":"no Seek, bounded memory regardless of NAR size; blake3 streamed once; populates file_digest"}
{"plan":548,"title":"TieredChunkBackend (S3 standard authoritative; S3 Express read-through cache)","deps":[544],"crate":"rio-store","priority":90,"status":"UNIMPL","complexity":"LOW","note":"both tiers are S3ChunkBackend; no backend/fs.rs"}
{"plan":549,"title":"ChunkBackend blob-API (put_blob/get_blob/delete_blob)","deps":[544,548],"crate":"rio-store","priority":85,"status":"UNIMPL","complexity":"LOW","note":"serialise after 548; used by P0566 narinfo/manifests sidecar only"}
{"plan":550,"title":"Hoist StoreClients+fetch_chunks_parallel → store_fetch.rs (NOT pure mv)","deps":[544],"crate":"rio-builder","priority":85,"status":"UNIMPL","complexity":"MED","note":"fetch.rs:20,32-33 imports fuser"}
{"plan":568,"title":"Batched GetChunks server-stream (K_server=256) + prost .bytes() + tonic residuals + obs","deps":[545,550],"crate":"rio-proto,rio-store,rio-builder,infra","priority":85,"status":"UNIMPL","complexity":"MED","note":"spike-validated 96cfd098"}
{"plan":570,"title":"DigestResolver: file_digest → (nar_hash, nar_offset, size) → chunk-range","deps":[544,545,550],"crate":"rio-builder","priority":85,"status":"UNIMPL","complexity":"LOW","note":"digest-FUSE open path"}
{"plan":551,"title":"migration 033_nar_index + manifests.nar_indexed bool + queries (no 034)","deps":[545],"crate":"rio-store","priority":85,"status":"UNIMPL","complexity":"LOW","note":"partial-index work-queue WHERE NOT nar_indexed (precedent: 031); PG forbids cross-table predicate"}
{"plan":552,"title":"GetNarIndex handler + indexer_loop","deps":[545,546,551],"crate":"rio-store","priority":85,"status":"UNIMPL","complexity":"MED","note":"nar_index_sync_max_bytes guard; entries carry file_digest"}
{"plan":553,"title":"infra/eks/s3-express.tf per-AZ directory bucket (for_each express_az_ids) + dedicated rio-store SG/NodeClass + s3express IAM","deps":[548],"crate":"infra","priority":80,"status":"UNIMPL","complexity":"LOW","note":"per-AZ from day one; TieredChunkBackend is AZ-count-agnostic; no CSI/PVC/kmod"}
{"plan":554,"title":"helm chunkBackend.tiered + per-AZ Express bucket env (downward-API zone → IMDS zone-id → bucket)","deps":[548,553],"crate":"infra,xtask","priority":80,"status":"UNIMPL","complexity":"LOW","note":"FIRST SHIPPED VALUE (U2)"}
{"plan":555,"title":"VM test: tiered-backend cache semantics","deps":[548,554],"crate":"nix","priority":80,"status":"UNIMPL","complexity":"MED","note":""}
{"plan":579,"title":"binary_cache_compat config + helm (runtime toggle, default ON)","deps":[544],"crate":"rio-store,infra","priority":80,"status":"UNIMPL","complexity":"LOW","note":"U6 foundation"}
{"plan":566,"title":"binary-cache compat writer: stock-Nix .narinfo + nar/*.nar.zst to S3-standard post-commit","deps":[549,579],"crate":"rio-store","priority":80,"status":"UNIMPL","complexity":"MED","note":"reassemble from moka-hot chunks; FileHash/FileSize populated; failure non-fatal to PutPath"}
{"plan":580,"title":"VM test: stock-Nix substitutes from S3 with rio-store stopped","deps":[566],"crate":"nix","priority":80,"status":"UNIMPL","complexity":"MED","note":"U6 LANDS"}
{"plan":581,"title":"compat GC: enqueue narinfo+nar.zst to pending_s3_deletes on sweep; narinfo.compat_file_hash column","deps":[566],"crate":"rio-store","priority":75,"status":"UNIMPL","complexity":"LOW","note":"runs regardless of current enabled value"}
{"plan":582,"title":"compat reconciler: backfill compat_file_hash IS NULL rows","deps":[566,581],"crate":"rio-store","priority":60,"status":"UNIMPL","complexity":"LOW","note":"crash-window + toggle-ON backfill; deferrable"}
{"plan":556,"title":"libcomposefs FFI encoder (composefs-sys + encode.rs) + nix patch (user.* prefix, no-root-whiteouts) + golden VM + fuzz","deps":[569,546],"crate":"rio-builder,composefs-sys,nix","priority":85,"status":"UNIMPL","complexity":"LOW","note":"~100 LoC owned (18 build.rs + ~80 adapter) + ~25-line C patch; spike ~46ms/23k files; both flags upstreamable"}
{"plan":557,"title":"PutPath eager nar_index compute (try_acquire-gated; no encode)","deps":[551,552],"crate":"rio-store","priority":80,"status":"UNIMPL","complexity":"LOW","note":"nar_ls+blake3 while NAR in RAM; no S3 artifact"}
{"plan":567,"title":"rio-mountd DaemonSet (fd-handoff + BACKING_OPEN broker + Promote/PromoteChunk verify-copy + cache+chunks ownership + metrics)","deps":[576,578],"crate":"rio-builder,infra","priority":80,"status":"UNIMPL","complexity":"MED","note":"~250 LoC; tokio async per-conn, Promote on spawn_blocking+Semaphore; PromoteChunk inline sub-ms; owns mountd_proto.rs; integrity boundary for shared cache+chunks"}
{"plan":559,"title":"composefs/{digest_fuse,circuit}.rs (2-level digest dir; FOPEN_PASSTHROUGH on cache-hit via mountd broker; staging+Promote on miss)","deps":[545,550,567,568,570],"crate":"rio-builder","priority":80,"status":"UNIMPL","complexity":"MED","note":"~500 LoC; prototype spike_digest_fuse.rs; passthrough is steady-state, read-upcall only during P0575 fill window"}
{"plan":571,"title":"mountd-owned /var/rio/cache LRU sweep + per-build staging + cache-hit metrics","deps":[559,567],"crate":"rio-builder,infra","priority":80,"status":"UNIMPL","complexity":"LOW","note":"cache is mountd-owned readonly (HOLE fix); flock orphan detection. cluster-wide shared-FS cache REJECTED — builder air-gap"}
{"plan":575,"title":"streaming open() for files > STREAM_THRESHOLD (during-fill KEEP_CACHE; priority-bump read; Promote on completion)","deps":[559,570,571],"crate":"rio-builder","priority":80,"status":"UNIMPL","complexity":"LOW","note":"~80 LoC; spike 1dad4f3c proves no mode-flip; unit-level exit via tests/stream.rs"}
{"plan":560,"title":"[ATOMIC] composefs cutover: §A mount+overlay+DELETE old-FUSE (~-4600 LoC) §B fixture kernel + vm:composefs-e2e + spike-regression cherry-pick","deps":[576,556,557,559,567,571,575],"crate":"rio-builder,nix","priority":80,"status":"UNIMPL","complexity":"HIGH","note":"hard cutover; one worktree, one PR, one .#ci gate"}
{"plan":562,"title":"Post-cutover audit (tracey builder.fuse.* empty; grep clean incl. cachefiles/boot_blob; .#ci re-run)","deps":[560],"crate":"nix","priority":80,"status":"UNIMPL","complexity":"LOW","note":"CUTOVER GATE"}
{"plan":563,"title":"Metrics: digest-fuse + tiered dashboards + alerts","deps":[544,548,559],"crate":"infra","priority":70,"status":"UNIMPL","complexity":"LOW","note":""}
{"plan":564,"title":"helm cleanup + mountd DS wiring + kernel assertion (drop smarter-device-manager entirely)","deps":[554,560,567],"crate":"infra,rio-controller,nix","priority":75,"status":"UNIMPL","complexity":"LOW","note":"builders privileged:false; DELETE device-plugin.yaml + both NodeOverlays + nixos-node/smarter-device-manager; kvm via hostPath CharDevice + nodeSelector + extra-sandbox-paths (vm-kvm-hostpath-spike PASS)"}
{"plan":565,"title":"Cutover runbooks (cache-tier, composefs)","deps":[555,562,564],"crate":"docs","priority":65,"status":"UNIMPL","complexity":"LOW","note":""}
{"plan":572,"title":"Directory merkle layer: dir_digest/root_digest in NarIndex + directories+file_blobs tables + bottom-up compute in nar_ls","deps":[545,546,551],"crate":"rio-proto,rio-nix,rio-store","priority":85,"status":"UNIMPL","complexity":"LOW","note":"U5 foundation; zero serving-path cost; snix castore.proto vendored (MIT); pin canonical encoding (snix #111)"}
{"plan":573,"title":"DirectoryService RPC: GetDirectory / HasDirectories / HasBlobs (batch bitmap; I-110 lesson)","deps":[572],"crate":"rio-proto,rio-store","priority":80,"status":"UNIMPL","complexity":"MED","note":"snix-wire-compatible where overlapping"}
{"plan":577,"title":"BlobService.Read(file_digest) server-stream (snix-compatible; file_blobs→chunk-range→GetChunks slice)","deps":[573],"crate":"rio-proto,rio-store","priority":80,"status":"UNIMPL","complexity":"LOW","note":"~40 LoC; completes castore surface"}
{"plan":574,"title":"Gateway substituter: Directory-DAG delta-sync client (nix copy walks DAG, prunes present subtrees)","deps":[573,577],"crate":"rio-gateway,nix","priority":75,"status":"UNIMPL","complexity":"MED","note":"U5 LANDS; falls through to chunk-list when remote lacks capability"}
```

---

## tracey `r[…]` marker inventory (P0544 writes spec; later phases write impl/verify)

| Marker | Spec file (P0544) | `r[impl]` (plan) | `r[verify]` site (plan) |
|---|---|---|---|
| `store.backend.tiered-get-fallback` | components/store.md | tiered.rs `get()` (P0548) | vm-store-tiered `cold-miss-fallback` (P0555) |
| `store.backend.tiered-put-remote-first` | components/store.md | tiered.rs `put()` (P0548) | vm-store-tiered `put-remote-first` (P0555) |
| `store.index.nar-ls-offset` | components/store.md | rio-nix/nar.rs (P0546) | proptest in nar.rs (P0546) |
| `store.index.file-digest` | components/store.md | rio-nix/nar.rs (P0546) | proptest in nar.rs (P0546) |
| `store.index.table-cascade` | components/store.md | metadata/queries.rs (P0551) | rio-store/tests/nar_index.rs (P0552) |
| `store.index.non-authoritative` | components/store.md | nar_index.rs `compute()` (P0552) | rio-store/tests/nar_index.rs (P0552) |
| `store.index.sync-on-miss` | components/store.md | nar_index.rs (P0552) | rio-store/tests/nar_index.rs (P0552) |
| `store.index.putpath-bg-warm` | components/store.md | nar_index.rs `indexer_loop` (P0552) | vm-composefs-e2e `cold-read` (P0560§B) |
| `store.index.putpath-eager` | components/store.md | put_path.rs (P0557) | vm-protocol-warm (P0557) |
| `store.index.rpc` | components/store.md | grpc/mod.rs (P0552) | rio-store/tests/nar_index.rs (P0552) |
| `builder.fs.composefs-stack` | decisions/022 §2.1 | composefs/mount.rs (P0560§A) | vm-composefs-e2e `cold-read` (P0560§B) |
| `builder.fs.userxattr-mount` | decisions/022 §2.1 | composefs/mount.rs (P0560§A) | vm-composefs-e2e (P0560§B) + composefs-spike-priv (`adr-022`) |
| `builder.fs.fd-handoff-ordering` | decisions/022 §2.3 | composefs/mount.rs (P0560§A) | vm-composefs-e2e (P0560§B) |
| `builder.fs.stub-isize` | decisions/022 §2.2 | composefs/encode.rs (P0556) | vm-composefs-encoder `golden-loop-mount` (P0556) |
| `builder.fs.metacopy-xattr-shape` | decisions/022 §2.2 | composefs/encode.rs (P0556) | vm-composefs-encoder (P0556) |
| `builder.fs.composefs-encode` | components/builder.md | composefs/encode.rs (P0556) | vm-composefs-encoder (P0556) |
| `builder.fs.digest-fuse-open` | decisions/022 §2.4 | composefs/digest_fuse.rs (P0559) | vm-composefs-e2e `cold-read` (P0560§B) + unit (P0559) |
| `builder.fs.passthrough-on-hit` | decisions/022 §2.4 | composefs/digest_fuse.rs (P0559) | vm-composefs-e2e `passthrough-small`+`warm-read` (P0560§B) |
| `builder.fs.passthrough-stack-depth` | decisions/022 §2.8 | composefs/digest_fuse.rs init (P0559) | composefs-spike-priv `passthrough-under-overlay` (P0578) |
| `builder.fs.file-digest-integrity` | decisions/022 §2.6 | composefs/digest_fuse.rs (P0559) | vm-composefs-e2e `integrity-fail` (P0560§B) |
| `builder.fs.digest-resolve` | components/builder.md | composefs/resolver.rs (P0570) | proptest (P0570) + vm-composefs-e2e (P0560§B) |
| `builder.fs.fetch-circuit` | components/builder.md | composefs/circuit.rs (P0559) | vm-composefs-e2e `eio-on-fetch-fail` (P0560§B) |
| `builder.fs.node-digest-cache` | components/builder.md | composefs/digest_fuse.rs (P0571) | vm-composefs-e2e `cross-build-dedup` (P0560§B) |
| `builder.fs.node-chunk-cache` | decisions/022 §2.4 | composefs/digest_fuse.rs (P0575) + bin/rio-mountd.rs (P0567) | vm-composefs-e2e `cross-build-dedup-streaming` (P0560§B) |
| `builder.fs.shared-backing-cache` | decisions/022 §2.4 | composefs/digest_fuse.rs (P0559+P0571) | vm-composefs-e2e `cross-build-dedup` (P0560§B) |
| `builder.fs.streaming-open` | components/builder.md | composefs/digest_fuse.rs (P0575) | vm-composefs-e2e `cold-read` <50ms (P0560§B) |
| `builder.fs.streaming-open-threshold` | decisions/022 §2.7 | config.rs (P0575) | vm-composefs-e2e `cold-read` (P0560§B) |
| `store.index.dir-digest` | components/store.md | rio-nix/nar.rs (P0572) | proptest (P0572) |
| `store.castore.canonical-encoding` | components/store.md | rio-proto/castore.proto (P0572) | golden-bytes (P0572) |
| `store.castore.directory-rpc` | components/store.md | rio-store/grpc/directory.rs (P0573) | unit (P0573) |
| `store.castore.blob-read` | components/store.md | rio-store/grpc/directory.rs (P0577) | unit (P0577) |
| `store.castore.gc` | components/store.md | rio-store/nar_index.rs + gc.rs (P0572) | rio-store/tests/gc.rs (P0572) |
| `store.castore.tenant-scope` | components/store.md | rio-store/grpc/directory.rs (P0573+P0577) | unit cross-tenant-probe (P0573) |
| `store.index.nar-ls-streaming` | components/store.md | rio-nix/nar.rs (P0546) | unit panic-on-seek (P0546) |
| `gw.substitute.dag-delta-sync` | components/gateway.md | rio-gateway/substitute/dag_sync.rs (P0574) | vm-dag-delta-sync (P0574) |
| `builder.result.input-eio-is-infra` | components/builder.md | executor/mod.rs (P0560§A, ported) | vm-composefs-e2e `eio-on-fetch-fail` (P0560§B) |
| `builder.mountd.erofs-handoff` | components/builder.md | bin/rio-mountd.rs (P0567) | vm-composefs-e2e `cold-read` (P0560§B) |
| `builder.mountd.backing-broker` | components/builder.md | bin/rio-mountd.rs (P0567) | composefs-spike-priv `passthrough-under-overlay` (P0578) |
| `builder.mountd.promote-verified` | decisions/022 §2.4 | bin/rio-mountd.rs (P0567) | composefs-spike-priv `passthrough-under-overlay` (P0578) + vm-composefs-e2e `integrity-fail` (P0560§B) |
| `builder.mountd.orphan-scan` | decisions/022 §2.3 | bin/rio-mountd.rs (P0567) | vm-composefs-e2e `mountd-restart` (P0560§B) |
| `builder.mountd.concurrency` | components/builder.md | bin/rio-mountd.rs (P0567) | composefs-spike-priv (vi) (P0578) |
| `obs.metric.mountd` | observability.md | bin/rio-mountd.rs (P0567) | vm-composefs-e2e (P0560§B) |
| `builder.overlay.composefs-lower` | components/builder.md | overlay.rs (P0560§A) | vm-composefs-e2e (P0560§B) |
| `builder.fs.parity` | components/builder.md | (verify-only) | lifecycle (P0562) |
| `store.compat.runtime-toggle` | components/store.md | config.rs (P0579) | unit + vm-store-compat `compat-off-no-narinfo` (P0566+P0580) |
| `store.compat.nar-on-put` | components/store.md | compat/writer.rs (P0566) | unit (P0566) |
| `store.compat.narinfo-on-put` | components/store.md | compat/writer.rs (P0566) | unit (P0566) |
| `store.compat.write-after-commit` | components/store.md | grpc/put_path.rs (P0566) | unit (P0566) |
| `store.compat.stock-nix-substitute` | components/store.md | (verify-only) | vm-store-compat `stock-nix-substitute` (P0580) |
| `store.compat.gc-coupled` | components/store.md | gc.rs (P0581) | rio-store/tests/gc.rs (P0581) |
| `obs.metric.compat` | observability.md | rio-store/lib.rs (P0566) | vm-store-compat (P0580) |
| `obs.metric.chunk-backend-tiered` | observability.md | rio-store/lib.rs (P0548) | vm-store-tiered (P0555) |
| `obs.metric.digest-fuse` | observability.md | rio-builder/lib.rs (P0559) | vm-composefs-e2e (P0560§B) |
| `proto.chunk.bytes-zerocopy` | components/store.md | rio-proto/build.rs (P0568) | unit (P0568) |
| `store.chunk.batched-stream` | components/store.md | rio-store/grpc/chunk.rs (P0568) | live A/B dashboard (P0568) |
| `store.chunk.tonic-tuned` | components/store.md | rio-store/main.rs (P0568) | (config-only) |
| `builder.fetch.batched-stream` | components/builder.md | rio-builder/store_fetch.rs (P0568) | live A/B dashboard (P0568) |
| `infra.express.cache-tier` | decisions/023 | infra/eks/s3-express.tf (P0553) | (live-only — runbook P0565) |
| `infra.node.kernel-composefs` | deployment.md | nix/nixos-node/kernel.nix (prereq) | nix/checks.nix node-kernel-config (prereq) |

59 markers. P0560 DELETES legacy `r[builder.fuse.*]`; P0562 audits via `tracey query uncovered | grep -E 'composefs|tiered|index|digest-fuse|compat'` → empty.
`config.styx` `test_include`: P0544 verifies `rio-nix/src/nar.rs` and `rio-builder/src/composefs/resolver.rs` are in scope (or adds them).

---

## Rollback (one-flag for cache tier, greenfield for builder)

| Layer | Rollback | How |
|---|---|---|
| Tiered cache → direct-S3 | `store.chunkBackend.kind=s3` (helm) | Single flag, instant + lossless — S3 was always authoritative. |
| composefs → old-FUSE | **none** (old-FUSE deleted at P0560) | `xtask k8s eks down && up` from a pre-P0560 commit. Greenfield principle. |

**Helm assertion** (`_helpers.tpl`, P0564): `{{- if and .Values.karpenter.enabled (not (has "OVERLAY_FS_DATA_ONLY" .Values.karpenter.amiKernelFeatures)) }}{{ fail "AMI must be built with nix/nixos-node/kernel.nix (≥6.16, EROFS+OVERLAY+FUSE); run xtask ami push" }}{{- end }}`.

---

## File-collision matrix (for `onibus collisions check`)

| File | Touched by | Serialisation |
|---|---|---|
| `rio-store/src/backend/{chunk.rs,tiered.rs,mod.rs}` | P0548, P0549 | P0548 → P0549 (dep edge) |
| `rio-store/src/grpc/mod.rs` | P0552, P0557 | P0552 → P0557 (dep edge) |
| `rio-store/src/grpc/put_path.rs` | P0566, P0557 | independent hunks (sidecar-write vs eager-index); both append after `complete_manifest` — P0557 rebases on P0566 |
| `rio-store/src/nar_index.rs` | P0552 (create), P0572 (directories insert), P0557 (eager) | P0552 → P0572 → P0557 |
| `rio-store/src/lib.rs` | P0548, P0552, P0557 | append-only metric registrations; dep chain serialises |
| `rio-builder/src/composefs/digest_fuse.rs` | P0559 (create), P0571 (cache metrics), P0575 (streaming) | P0559 → P0571 → P0575 |
| `rio-builder/src/composefs/mountd_proto.rs` | P0567 (create), P0559 (consume) | P0567 → P0559 |
| `rio-builder/src/bin/rio-mountd.rs` | P0567 (create), P0571 (LRU sweep) | P0567 → P0571 |
| `rio-builder/src/composefs/mod.rs` | P0556, P0567, P0559, P0570, P0560§A | append-only `pub mod`; P0560 last |
| `rio-builder/src/store_fetch.rs` | P0550 (create), P0568 (batched client), P0559 (call) | P0550 → P0568 → P0559 |
| `rio-proto/build.rs` | P0568 only | — |
| `rio-builder/src/overlay.rs` | P0560 only | — |
| `nix/tests/default.nix` | P0555, P0556, P0560§B, P0562 | append-only scenario entries |
| `nix/tests/fixtures/k3s-prod-parity.nix` | P0555, P0560§B | P0555 adds args; P0560§B adds unconditional kernel.nix import |
| `infra/helm/rio-build/values.yaml` | P0554, P0564 | distinct top-level keys |
| `rio-controller/src/reconcilers/common/sts.rs` | P0564 only | — |
| `nix/nixos-node/eks-node.nix` | P0564, P0571 | distinct hunks (drop smarter-device-manager static-pod vs tmpfiles) |
| `migrations/033_nar_index.sql` | P0551, P0572 (adds `directories`+`file_blobs` tables) | P0551 → P0572; same migration file (greenfield) |
| `rio-proto/proto/types.proto` | P0545, P0572 | P0545 → P0572 (append fields 7, 8) |
| `rio-nix/src/nar.rs` | P0546, P0572 | P0546 → P0572 (second pass in same fn) |
| `rio-store/src/grpc/directory.rs` | P0573 (create), P0577 (ReadBlob) | P0573 → P0577 |
| `rio-gateway/src/substitute/` | P0574 only | — |
| `infra/helm/rio-build/templates/mountd-ds.yaml` | P0567 (create), P0564 (wire values) | P0567 → P0564 |

---

## Commands cheat-sheet

```bash
# Phase 0 spikes (P0569 already DONE; cherry-pick its tests)
git -C ../main/.claude/worktrees/agent-acf26042 log --oneline -3
nix build .#checks.x86_64-linux.vm-spike-composefs-priv  # P0541

# Phase 0 measurement
nix develop -c cargo xtask measure v4-v11-v12 --closure chromium

# Phase 3 cache-tier flip (FIRST SHIPPED VALUE)
nix develop -c cargo xtask k8s -p eks tofu apply -target=aws_s3_directory_bucket.cache
nix develop -c cargo xtask k8s -p eks down && nix develop -c cargo xtask k8s -p eks up
nix develop -c cargo xtask k8s -p eks grafana   # watch tiered_local_hit_ratio climb

# Phase 5 composefs cutover (greenfield — old-FUSE deleted at P0560)
nix develop -c cargo xtask k8s -p eks down
nix develop -c cargo xtask k8s -p eks up   # from a P0562-green commit
nix develop -c cargo xtask k8s -p eks rsb -- nixpkgs#chromium

# Any-phase CI gate
/nixbuild .#ci

# Cache-tier rollback (instant + lossless)
helm upgrade rio infra/helm/rio-build --reuse-values --set store.chunkBackend.kind=s3
# Builder-side rollback: down && up from a pre-P0560 commit
```

---

## Explicitly deferred (out of scope)

- Non-reproducibility `nar_hash` mismatch detection at PutPath
- Per-replica chunk-dedup metrics
- aarch64-specific encoder/mount validation (proptest covers; live aarch64 builder is the soak; P0543 covers config-eval only)
- **`libcomposefs` upstream PRs** (good citizenship; we carry the nix patch so neither blocks) — (a) `LCFS_BUILD_USER_XATTR_OVERLAY` flag to emit `user.overlay.*` instead of `trusted.*`; (b) `LCFS_FLAGS_NO_ROOT_WHITEOUTS` to skip the 256 OCI chardev whiteouts + root opaque. If both land, drop `nix/patches/libcomposefs-user-xattr.patch`.
- **`mkfs.erofs --tar=headerball` fallback** — upstream `erofs-utils` (≥1.8) purpose-built mode for meta-only overlay data-only layers; ~160 LoC PAX-header emitter + subprocess, ~280 ms / 23k files (spike-verified at `~/tmp/spike-headerball/`). Zero-patch option; switch to it if patch maintenance or `libcomposefs` ABI churn becomes a problem.
- **Kernel `BACKING_OPEN` `d_is_reg` relaxation** — [`backing.c:105-108`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c) rejects block-device fds. If lifted upstream, `ublk`-per-giant becomes a viable shared-verified-partial primitive (still needs chunk-addressed verify underneath, but would let B passthrough A's in-progress fill).
- **fs-verity on the digest cache** — would give kernel-side integrity for warm reads from `/var/rio/objects`, but requires a real fs (not FUSE) as the data-only lower. Possible future: digest-FUSE materializes into an ext4/xfs hostPath with fs-verity, overlay's lower2 is that dir directly (no FUSE on warm path at all). Followup after P0562.
- **Cross-region deployment** — globally-consistent metadata store, object-store cross-region replication, per-region cache tiers. This plan ensures forward-compat (object-store-authoritative, cache tier stateless) but does not implement it.
- **Upload-path chunk-granular dedup (`PutPathChunked`)** — now designed at [ADR-022 §6](./022-lazy-store-fs-erofs-vs-riofs.md#6-extension-chunked-output-upload-putpathchunked); sequencing into plan docs is the follow-on. Depends on primitives this plan already lands: `file_digest` (P0551), Directory merkle + `file_blobs` table (P0572), `HasBlobs`/`HasDirectories` batch RPCs (P0573), `ReadBlob` (P0577). Net-new: `HasChunks` RPC, builder-side fused walk (`upload.rs` rewrite), `nar_hash_verified` column + async-verify worker, `quarantined` manifest state.

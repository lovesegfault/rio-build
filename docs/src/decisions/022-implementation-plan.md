# ADR-022 Implementation Plan — FSx shared chunk backend + composefs-style lazy store

**Status:** sequencing only — design settled 2026-04-05, review-panel-finalized 2026-04-06 ([ADR-022 §C](./022-lazy-store-fs-erofs-vs-riofs.md) @ `b6794962`, ADR-023). Supersedes PLAN-GRAND-REFACTOR V1 (2026-04-04) and V2 (2026-04-05); prior versions archived at `~/tmp/stress-test/`. **V2→V3:** synced with ADR-022 review-panel resolutions — per-chunk verify integrity model, per-build mount + shared backing cache, kernel ≥6.16, rio-mountd teardown, `5ef7bcdeecc9` mechanism citation, consolidated spike refs.
**Plan-number range:** P0540–P0576 (V1's P0540-P0568 retained; P0542/P0547/P0558 dropped; P0541/P0556/P0557/P0559/P0560/P0567 rewritten; P0569-P0576 new). V1's P0539 renumbered → **P0576** (collision: dag.jsonl already has P0539 = "Observability stack").
**Multi-region forward-compat (unchanged from V1):** object store (S3/GCS) is authoritative for bytes; FSx is a per-AZ read-through cache tier; PG is single-region today with ADR-024 reserved for global metadata (DSQL/Spanner/CockroachDB). No DRA.
**Clean-cutover constraint** (unchanged): no FUSE fallback flag, no `RIO_STORE_BACKEND` selector. P0560 deletes the FUSE module wholesale.
**Migration-number range:** `033_*` (last shipped: `032_derivations_size_class_floor.sql`). V1's `034_*` dropped — no boot-blob backlog under Path C.

---

## V1 → V2 delta

V1 was sequenced around ADR-022 **Path A** (EROFS + fscache + cachefiles ondemand). On 2026-04-05, the spikes in §Spike evidence validated **Path C** (composefs-style: EROFS metadata-only image + overlayfs `redirect`/`metacopy` → digest-addressed FUSE data-only lower) at chromium scale. Path C dominates A on mount latency (<10 ms vs ~70 ms), matches A on the warm path (0 upcalls), achieves kernel-side per-file dedup A structurally cannot, and is roughly half the owned LoC with **no cachefiles daemon, no device table, no `boot/` S3 artifact, no `(cookie,off)→nar_offset` reverse-map**. ADR-022 was reopened and superseded in C's favor; A is the documented fallback.

| Plan | V1 scope | V2 scope | Reason |
|---|---|---|---|
| P0539→**P0576** | EXT: nixos-cutover sentinel (kernel ≥6.8, `EROFS_FS_ONDEMAND`, `/dev/cachefiles`) | EXT: nixos-cutover sentinel (kernel **≥6.16**; `EROFS_FS`/`OVERLAY_FS`/`FUSE_FS` `=y`). **Renumbered** — dag.jsonl P0539 is live "Observability stack". | C uses no fscache/cachefiles. Data-only-lower redirect under `userxattr` needs ≥6.16 (`5ef7bcdeecc9`). |
| P0540 | SPIKE: chunk-indexed EROFS encoder | unchanged (Path-A fallback validation) | Retained for the record; not on critical path. |
| **P0541** | SPIKE: `/dev/cachefiles` ondemand protocol | **REWRITTEN**: composefs privilege/mount validation | cachefiles spike no longer load-bearing. New blocker: can unpriv builder loop-mount EROFS? (expected NO → P0567 minimal mount-helper). |
| P0543 | V4/V11 + closure-paths + max-nar-size | + **V12 `STREAM_THRESHOLD` tuning** | V12 no longer gates P0575 (top1000.csv already answered: build it). V12 tunes the threshold. `closure_paths<65535` gate dropped (no device table). |
| P0544 | spec scaffold (ADR-023/024 + r[...]) | + ADR-022 §C markers (`builder.fs.*`) | New normative requirements from §C. |
| P0545 | proto: NarIndex + boot_blob | NarIndex + **`file_digest`**; **drop boot_blob** | `file_digest` = redirect-xattr value AND integrity key (§C.6). No boot blobs. |
| P0546 | nar_ls (offset-tracking) | + **blake3-per-regular-file while streaming** | Populates `file_digest`. Bytes already in RAM. |
| **P0547** | chunk-range cumsum (cachefiles `READ{off,len}` reverse-map) | **DROPPED** — superseded by P0570 | Digest-FUSE keys by `file_digest`, not `(cookie,off)`. The `file_digest → chunk-range` map is P0570. |
| P0548-P0550, P0568 | TieredChunkBackend / blob-API / fetch hoist / GetChunks stream | **unchanged** | Path-agnostic. |
| P0551 | migrations 033 nar_index + 034 boot_size | **033 only**; 034 dropped | No boot-blob backlog. nar_index entries blob now carries `file_digest`. |
| P0552 | GetNarIndex + indexer_loop | unchanged shape; `compute()` gains blake3 | nar_index is the only derived metadata; `boot_size` sentinel → `nar_index` row presence is the queue. |
| P0553-P0555, P0566 | FSx cache tier + helm + VM test + self-describing S3 | **unchanged** | Path-agnostic (U2 track). |
| **P0556** | EROFS chunk-indexed encoder (~800 LoC) | **REWRITTEN**: `NarIndex → composefs-dump(5) → mkcomposefs --from-file` (~250 LoC) | §C.2. No device table, no chunk indices, no blobmap xattr. |
| **P0557** | PutPath eager-encode boot_blob | **REWRITTEN**: PutPath eager-compute nar_index (with `file_digest`) | No encoding step; just `nar_ls`+blake3 while NAR is in RAM. |
| **P0558** | GC sweep deletes `boot/` | **DROPPED** | No `boot/` namespace. |
| **P0559** | erofs/{cachefiles,merge,circuit}.rs (~900 LoC) | **REWRITTEN**: `composefs/{digest_fuse,circuit}.rs` (~450 LoC) | §C.4. No cachefiles UDS client, no merge-splice, no `O_DIRECT` bounce, no copen/ioctl. |
| **P0560** | [ATOMIC] cachefiles mount + DELETE FUSE | [ATOMIC] composefs mount + DELETE FUSE | §C.3 mount stack. Same deletion inventory + the digest-FUSE mount. |
| P0562-P0565 | audit / metrics / helm cleanup / runbooks | references updated (cachefiles→composefs) | Mechanical. |
| **P0567** | rio-cachefilesd DaemonSet (~300 LoC + helm) | **REWRITTEN**: `rio-mountd` fd-handoff helper (~50 LoC + helm) — opens `/dev/fuse` + `fsopen("erofs")`, SCM_RIGHTS both, exits | P0541 PASS: builder does overlay itself (`userxattr`). Helper holds no state. |
| **P0569** | — | NEW: SPIKE sentinel — composefs validated (consolidated `15a9db79` on `adr-022`) | Already DONE; dag row for dependency tracking. |
| **P0570** | — | NEW: `DigestResolver` — `file_digest → (nar_hash, nar_offset, size) → chunk-range` | Replaces P0547's role at the digest-FUSE `open` path. |
| **P0571** | — | NEW: shared node-SSD backing cache (`/var/rio/cache/ab/<digest>`) + per-build FUSE mount (`/var/rio/objects/{build_id}/`) | `r[builder.fs.shared-backing-cache]` — node-level dedup with cross-pod isolation. |
| **P0572** | — | NEW: Directory merkle layer (`dir_digest`/`root_digest` in NarIndex + `directories` table) | U5 foundation. Closes subtree-merkle gap vs snix at zero serving-path cost. |
| **P0573** | — | NEW: DirectoryService RPC (`GetDirectory`/`HasDirectories`/`HasBlobs` batch) | U5 read surface; snix `castore.proto` wire-compatible. |
| **P0574** | — | NEW: delta-sync client in gateway substituter | U5 lands for users — `nix copy` walks Directory DAG, fetches only changed subtrees. |
| **P0575** | — | NEW: §C.7 mitigation (i) — streaming `open()` for large files. **Critical-path, unconditional.** | top1000.csv: all 1000 >64 MiB, 248 `.so`/`.a`; access-probe `42aa81b2`: 0.3-33% touched; spike `15a9db79`: mechanism works, no mode-flip needed. |

**Net LoC vs V1:** Path C is ~1 450 owned LoC vs A's ~2 700 (ADR-022 §4). The encoder drops from ~800 to ~250; P0559 from ~900 to ~450; P0567 from ~300 to ~50 (a **smaller privileged surface than today's FUSE setup** — helper opens two fds and exits, holds no state). P0547/P0558 disappear. P0570/P0571 add ~250; P0575 adds ~80. The U5 track (P0572-P0574) is ~+400 and is independent of Path-C — it shares only the P0545/P0546 NarIndex base.

**What V2 buys:** U1 = file-granular lazy fetch with kernel-native metadata + structural per-file dedup. U2 = per-AZ chunk cache (unchanged from V1). **U5 = deployment sync bandwidth proportional to *change size*, not closure size** — `nix copy --from rio` and multi-region replication walk the Directory DAG and skip subtrees the receiver already has.

### V2 → V3 delta (ADR-022 review-panel sync)

| Area | V2 | V3 |
|---|---|---|
| §C.6 integrity | whole-file blake3 "before returning" from `open()` (contradicted streaming-open) | **per-chunk blake3 verify on arrival**; whole-file `file_digest` check gates `.partial`→final rename |
| P0571/P0559 topology | single `/var/rio/objects` (shared-vs-per-build undecided) | **per-build FUSE mount** `/var/rio/objects/{build_id}/` + **shared backing cache** `/var/rio/cache/ab/<digest>`; `O_EXCL` on `.partial`, loser inotify-waits |
| P0567 teardown | conn-drop → `umount2`+`losetup -d` | + `rmdir` per-build dirs; **start-up orphan scan** of `/var/rio/objects/*` |
| Kernel floor | ≥6.6 | **≥6.16** ([`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9): data-only-lower redirect under `userxattr`) |
| userxattr+`::` mechanism | "implies metacopy" | gated on `ofs->numdatalayer > 0` (`namei.c:1241`), **not** `config->metacopy`; maintainer-designed userns-safe path |
| P0575 warm-upcall claim | "never after" | "0 upcalls **while pages remain cached**; cgroup reclaim re-upcalls, re-served from SSD" |
| Spike refs | `worktree-agent-*` ephemeral branches | consolidated on `adr-022`: `15a9db79` (core/scale/stream), `af8db499` (priv), `42aa81b2` (access), `9492019c` (kvm); ADR `4a716900`/`db930e82`/`b6794962` |
| `lcfs-writer-erofs.c` LoC | ~1.2k | **~2.1k** (`GPL-2.0-or-later OR Apache-2.0`) |

---

## Spike evidence (Path C validation)

Core-stack nixosTests consolidated on `adr-022` (commit `15a9db79`); chromium-146 closure topology (357 store paths, 23 218 regular files, 8 221 dirs, 3 374 symlinks) with synthetic content. Original measurement commits `9c162024`/`a1394c0b`/`9415f9e2` are subsumed by the port.

| Commit | Test | Result |
|---|---|---|
| `9c162024` | `composefs-spike.nix` (3-file hypothesis) | warm `read` upcalls = 0; `stat` upcalls = 0; mount = 2 lookup + 1 getattr total |
| `a1394c0b` | `composefs-spike-scale.nix` (23 218 files via `mkfs.erofs`+setfattr) | mount **<10 ms**, 0 upcalls; `find -type f` 60 ms / 0 upcalls; cold lookup **depth-independent (=2)**; warm read = 0; **gotcha:** `stat size=0` (stub `i_size=0`) |
| `9415f9e2` | same, encoder swapped to `mkcomposefs --from-file` | `stat` sizes correct; image **5.3 MiB** / encode **70 ms** (vs 6.3 MiB / 480 ms); `find -printf %s` sum = 1 795 354 094 B == manifest, 120 ms, 0 upcalls; warm read = 0 (no regression) |

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

**Gotcha (ordering):** `/dev/fuse` fd MUST be received and the digest-FUSE serving **before** the overlay mount — overlayfs probes lowers at `mount(2)`; an unserved FUSE deadlocks the mounter.

**§C.7 large-file evidence** (P0575 promoted to critical-path on this basis):

| Commit / source | Finding |
|---|---|
| nix-index `top1000.csv` (external dataset, 2026-04-05) | nixpkgs top-1000 files: **all >64 MiB** (min 117 MiB, median 179 MiB, 267 >256 MiB, 7 >1 GiB). 248 are `.so`/`.a`. Floor — proprietary closures worse. |
| `42aa81b2` (`adr-022`, `nix/tests/lib/spike-access-data/RESULTS.md`) | Real consumers read **0.3-33%** of giants: link-time `libLLVM.so` 2.79% bimodal head+tail; `opt --version` 32.77% scattered/266 ranges; `libicudata` preload 0.28%. No `MAP_POPULATE`/`fadvise`. |
| `15a9db79` (`adr-022`, `composefs-spike-stream.nix`) | Streaming-open mechanism PASS: 256 MiB `open()` = **10.3 ms** (vs 2560 ms whole-file); `FOPEN_KEEP_CACHE` from start → 2nd `dd` **0 read upcalls**; `mmap` page-faults route through FUSE `read`; **no mode-transition needed** (KEEP_CACHE doesn't suppress cold upcalls, only prevents invalidation). |
| alternatives survey | File-splitting at encoder **infeasible** (overlayfs `redirect` is single-path). Allowlist prefetch **violates JIT-fetch imperative** (`feedback_jit_fetch_imperative.md`). composefs+fscache hybrid resurrects ~70% of Path A's deleted LoC. FSx-backed cluster-wide objects cache **rejected** — violates builder air-gap (`project_builder_airgap.md`). |

**Key encoder findings** (now ADR-022 §C.2): stub inodes MUST carry real `i_size` (mkcomposefs does this; bare `mkfs.erofs` on 0-byte staging files does not). The metacopy xattr must be 0-length or ≥4 bytes (`struct ovl_metacopy{u8 version,len,flags,_pad}`). mkcomposefs sets the redirect xattr automatically from the dump's PAYLOAD field; with DIGEST=`-` it writes legacy 0-byte metacopy. **Unpriv overlay (P0541) requires `user.overlay.{redirect,metacopy}`, NOT `trusted.*`** — overlayfs under `userxattr` reads only the `user.` prefix. fs-verity-in-metacopy doesn't verify when the lower is FUSE — per-file integrity lives in the digest-FUSE handler (§C.6).

---

## Path A fallback

Per ADR-022 §5: if overlay-on-FUSE-data-only-lower exhibits an unforeseen production issue, V1's P0556-A/P0559-A/P0567-A shapes are the documented fallback. P0540's chunk-indexed encoder spike and the original P0541 cachefiles findings (`.stress-test/sessions/2026-04-05-phase0-gate.md`) remain valid for that path. **This plan does not carry the fallback implementations; switching paths is `git checkout` of V1 + a fresh `down && up`.**

---

## Prerequisites (in flight separately — NOT phased here)

| Track | Status | Owns |
|---|---|---|
| **NixOS node cutover** (full Bottlerocket replacement) | dispatched (`nixos-cutover` agent) | `nix/nixos-node/{hardening,kernel}.nix`, `karpenter.yaml` amiSelectorTerms→tag, `xtask ami push`, ADR-021 |
| `kernel.nix` standalone module with `EROFS_FS=y OVERLAY_FS=y FUSE_FS=y`, **kernel ≥6.16** ([`5ef7bcdeecc9`](https://git.kernel.org/linus/5ef7bcdeecc9): data-only-lower redirect honored under `userxattr`) + ≥5.2 (`fsopen`/`fsmount`/`move_mount`) | part of cutover | `nix/nixos-node/kernel.nix` — **MUST be importable by `nix/tests/fixtures/`** so VM tests reuse the AMI's exact config. **No `EROFS_FS_ONDEMAND`, no `CACHEFILES*`** — Path C uses neither. Module asserts version at boot. |
| Device exposure: **no smarter-device-manager** | part of cutover | `/dev/fuse` → fd-handoff from `rio-mountd` DS (P0567); `/dev/kvm` → `hostPath{type:CharDevice}` + `nodeSelector{rio.build/kvm}` on kvm-pool pods + `extra-sandbox-paths=["/dev/kvm"]` in builder nix.conf. `nix/nixos-node/static-pods.nix` drops the device-plugin pod. |

**This plan assumes the cutover lands first.** No old-FUSE fallback — same greenfield cutover as Bottlerocket→NixOS. Rollback for builder-side regressions is `xtask k8s eks down && up` from a known-green commit.

**Greenfield deployment constraint** (settled 2026-04-04, unchanged): we control the only deployment. Migration path is `xtask k8s eks down && up`. NO backfill jobs, NO old-binary compat shims, NO dual-read paths. When this plan's phases are ready to flip on, tear down + redeploy.

---

## User journeys (every phase traces to one)

| ID | User | Journey | Today | After |
|---|---|---|---|---|
| **U1** | build submitter | `nix build .#chromium`; closure 200 GB, build reads 5% | builder fetches whole touched store-paths (~per-path JIT, rev 63); warm reads via FUSE passthrough but cold = whole NAR | builder fetches **only the files the build opens**, on-demand, at file granularity; `stat`/`readdir` are kernel-native; warm reads = page-cache, zero crossings; identical files across store paths share one node-SSD copy and one page-cache copy |
| **U2** | operator | scales `rio-store` 3→12 under load | each replica cold-misses S3 independently; 12× GET cost; 12× moka warm-up | per-AZ FSx cache tier serves all replicas in that AZ; new replica is warm; S3 GET only on first-in-AZ cold miss. Cache-tier-AZ down → cold reads from S3, not outage. |
| **U3** | operator | something is wrong at 02:00 | unclear which layer | one single-flag rollback for cache tier (`store.chunkBackend.kind=s3`, instant + lossless); builder-side rollback is greenfield `down && up` from known-green commit |
| **U4** | operator | wants to know if the new path is better | no per-file metrics | grafana: `rio_builder_digest_fuse_open_seconds` p99, `…_fetch_bytes_total{hit=node_ssd|remote}`, `rio_store_tiered_local_hit_ratio` |
| **U5** | deployment consumer | `nix copy --from rio-store` to a NixOS host / multi-region replica that already has 95% of the target closure | walks chunk-list per store path; O(all-chunks) `HasChunk` RPCs even for unchanged paths | walks Directory DAG; `HasDirectories([root_digest,…])` short-circuits unchanged subtrees in one batch RPC; fetches only changed files. **Sync bandwidth ∝ change size, not closure size.** Builders are ephemeral but build *outputs* are not — this is rio-store as distribution substrate. |

**Sequencing rule (U3, unchanged):** every phase boundary is `.#ci` green. Phases 0-4 are deploy-safe (store-side or test-only). Phase 5 (P0560) is the hard cutover: builders REQUIRE the composefs lower from that commit forward.

---

## DAG overview

```
P0576 EXT: nixos-cutover sentinel (kernel.nix ≥6.16 importable, /dev/fuse, AMI) ───────────────┐
                                                                                               │
┌── Phase 0 (gate + scaffold; ≤4-way parallel) ──┐                                             │
P0569 spike:composefs   P0541 spike:mount-priv   P0543 measure              P0544 spec-scaffold
(DONE — sentinel row)   (userns overlay; erofs   V4/V11/V12 + closure wc    ADR-023 (tiered) + ADR-024 stub
                         loop unpriv? fsmount    + max-nar-size             + ADR-022 §C r[...] markers
                         handoff for erofs)      + aarch64 kernel
   │                       │                          │                     │
   └────────────────── Phase-0 gate: all PASS ────────┴─────────────────────┘
                       (P0540 retained off-path: Path-A fallback validation; P0542 dropped)
                                                                                               │
┌── Phase 1 (primitives; ≤8-way parallel) ──┐  all dep on P0544                                │
P0545 proto    P0546 nar_ls    P0572 dir merkle  P0570 DigestResolver   P0548 Tiered    P0549 blob-API  P0550 fetch.rs hoist
(NarIndex      (rio-nix;       (dir_digest/      (file_digest →         (local FS →     (string-keyed,  (StoreClients →
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
   │                              │   P0553 fsx.tf (no DRA) + csi + store-SG/NodeClass          │
   │                              │      └─► P0554 helm PVC + zone-pin ──► P0555 vm:tiered-cache
   │                              │             ★ FIRST SHIPPED VALUE (U2)
   │                              │             └─► P0566 self-describing S3 (direct write)
   │                              │
┌── Phase 4 composefs store-side (gated on Phase-0 + P0546) ──┐
P0556 composefs/dump.rs (NarIndex → composefs-dump(5) text) + mkcomposefs golden VM
   │
P0557 PutPath eager nar_index (try_acquire-gated; NAR in RAM → nar_ls+blake3) ◄─(P0551, P0552)
   │
┌── Phase 5 composefs builder-side ──┐                                                          │
P0559 composefs/{digest_fuse,circuit}.rs ◄─(P0545, P0550, P0568, P0570)                         │
   │                                                                                            │
P0567 rio-mountd DaemonSet (open /dev/fuse + fsopen("erofs"); SCM_RIGHTS both fds; exits) ◄─────┤(P0576)
   │                                                                                            │
P0571 node-SSD /var/rio/objects digest cache ◄─(P0559)
   │
P0575 streaming open() for files > STREAM_THRESHOLD ◄─(P0559, P0570)
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
P0574 gateway substituter: Directory-DAG delta-sync client  ★ U5 LANDS
```

**Hidden dependencies surfaced** (carried from V1 + Path-C-specific):

| Edge | Why it's non-obvious |
|---|---|
| P0549 blob-API → P0566 | `ChunkBackend` trait today is `[u8;32]`-addressed only (`rio-store/src/backend/chunk.rs:91`). `narinfo/{h}` / `manifests/{h}` need string-keyed `put_blob/get_blob/delete_blob`. (V1 also needed it for `boot/`; V2 doesn't.) |
| P0576 (kernel.nix sentinel) → P0560 | Test-VM kernel must have the same `extraStructuredConfig` as the AMI. `kernel.nix` MUST be a standalone NixOS module importable by `nix/tests/fixtures/`. Path C drops the ondemand options but keeps the importable-module discipline. |
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

### P0540 — SPIKE: minimal chunk-indexed EROFS encoder + loop-mount (Path-A fallback)
**Unchanged from V1.** Retained off the critical path as Path-A fallback validation. **Status: PASS 2026-04-05** per `.stress-test/sessions/2026-04-05-phase0-gate.md`.

### P0541 — SPIKE: composefs privilege boundary + mount handoff
**Status: DONE — all six subtests PASS** (commit `af8db499` on `adr-022`, kernel 6.18.20). Results table in §Spike evidence above. Routes P0567 to its minimal shape (open `/dev/fuse` + `fsopen("erofs")`, hand off both, exit) and confirms overlay mount stays in the unprivileged builder via `userxattr`.
**Files:** `nix/tests/scenarios/spike-composefs-priv.nix` — VM imports `nixos-node/kernel.nix`; runs as unpriv-userns user. Subtest table preserved at `.stress-test/sessions/2026-04-05-phase0-gate.md`.

### ~~P0542 — SPIKE: FSx Lustre multi-client rename atomicity~~ **DROPPED** (unchanged from V1)
Under the tiered design (S3 authoritative, FSx is per-AZ cache), no two store replicas race writes on the same FSx — each AZ's replicas write-through to their local cache independently, and S3 PUT is the serialization point. Multi-client rename atomicity is moot.

### P0543 — V4/V11/V12 measurement + closure-size + aarch64 kernel sanity
**Crate:** `xtask` · **Deps:** none · **Complexity:** LOW
| File | Change |
|---|---|
| `xtask/src/k8s/measure.rs` | new — `xtask measure v4` (synth metadata-image-build latency on chromium closure: NarIndex rows → dump text → `mkcomposefs`), `xtask measure v11` (intra-closure chunk-reuse %), **`xtask measure v12` (tune `STREAM_THRESHOLD` — ingest nix-index `top1000.csv` + `nix/tests/lib/spike-access-data/RESULTS.md` (`42aa81b2`); compute the size at which whole-file fetch latency exceeds p50 first-range-touched latency)**, `xtask measure closure-paths` (`nix path-info -r nixpkgs#chromium \| wc -l` for both arches), `xtask measure max-nar-size` (`nix path-info -r --json … \| jq 'max_by(.narSize)'`) |
| `.stress-test/metrics/v4-v11-v12.json` | output |
| `nix/checks.nix` | `node-kernel-config-aarch64`: `pkgsCross.aarch64-multiplatform` eval of `nixos-node/kernel.nix`; assert `EROFS_FS` / `OVERLAY_FS` / `FUSE_FS` resolve `=y` in the cross config. Build-eval only. |

**Exit:**
- `v4_p99_ms < 200`. Spike measured 70 ms encode + <10 ms mount for chromium; this is headroom check. FAIL → cache the metadata image per-closure-hash on node SSD (P0571 already provides the dir).
- `v12_stream_threshold_bytes`. **Tuning, not a gate.** P0575 ships unconditionally (top1000.csv + access-probe `da6148cd` already prove the 64 MiB question). V12 picks the `STREAM_THRESHOLD` config default (initial: 8 MiB ≈ 60-120 ms whole-file at 1 Gbps).
- `max_nar_size_* < nar_index_sync_max_bytes` (4 GiB default). FAIL → pull streaming `nar_ls` into P0546. **Unchanged from V1; still gates P0546.**
- `node-kernel-config-aarch64` builds. FAIL → fix `kernel.nix` for aarch64 before P0576 flips DONE.
- ~~`closure_paths_* < 65535`~~ — **gate removed** (no device table under Path C). Measurement kept as informational.

### P0544 — Spec scaffold (all `r[…]` markers + ADR-023 + ADR-024 stub)
**Crate:** `docs` · **Deps:** none · **Complexity:** LOW
| File | Change |
|---|---|
| `docs/src/decisions/022-lazy-store-fs-erofs-vs-riofs.md` | merge `adr-022` (`4a716900`..`b6794962`: §Path C + supersession + spike-sync + review-panel fixes). Carries 9 markers: `r[builder.fs.{composefs-stack, userxattr-mount, stub-isize, metacopy-xattr-shape, fd-handoff-ordering, digest-fuse-open, shared-backing-cache, file-digest-integrity, streaming-open-threshold}]`. |
| `docs/src/decisions/023-tiered-chunk-backend.md` | new — **unchanged from V1**: object store (S3 today; GCS-ready via `ObjectStoreBackend` trait) is authoritative for bytes; per-AZ fast filesystem (FSx-for-Lustre today) is a disposable read-through cache. `put` = object-store sync + local-FS async; `get` = local-FS → object-store fallback + write-through. PG `chunk_refs` is single-writer arbiter (single-region). **No DRA.** Forward-compat: cache tier is stateless and metadata-agnostic; multi-region = S3 CRR/MRAP + ADR-024. Explicitly states: cache-tier-AZ outage = cold reads from S3, not service outage; rollback `kind=s3` is instant + lossless. |
| `docs/src/decisions/024-global-metadata.md` | new stub — `Status: Proposed`. **Unchanged from V1.** |
| `docs/src/components/store.md` | append §"NAR index" (incl. `file_digest`) + §"Tiered chunk backend" |
| `docs/src/components/builder.md` | append §"composefs lazy lower" + §"digest-FUSE handler" + §"rio-mountd" |
| `docs/src/observability.md` | append metric rows |

**Exit:** `tracey query validate` 0 errors; `.#ci` green.

**Phase-0 gate (go/no-go):** P0569 DONE; P0541 subtests per decision tree route P0567/P0559/P0560 design (do NOT block the gate). Record in `.stress-test/sessions/`. Phases 1–3 proceed regardless (Path-C-agnostic).

---

## Phase 1 — Primitives (≤7-way parallel; all dep on P0544)

### P0545 — proto: NarIndex with `file_digest`
**Crate:** `rio-proto` · **Deps:** P0544 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-proto/proto/types.proto` | `message NarIndexEntry { string path=1; Kind kind=2; uint64 size=3; bool executable=4; uint64 nar_offset=5; string target=6; bytes file_digest=7; }` — `file_digest` is blake3 of regular-file content (32 bytes; empty for dirs/symlinks). `message NarIndex { repeated NarIndexEntry entries=1; }` |
| `rio-proto/proto/store.proto` | `rpc GetNarIndex(...)`; **NO `boot_blob`, NO `want_boot_blob`** |
| `xtask regen mocks` | run |

**Exit:** `.#ci` green.

### P0546 — rio-nix: `nar_ls` + blake3-per-file
**Crate:** `rio-nix` · **Deps:** P0544, P0545 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-nix/src/nar.rs` | `pub fn nar_ls<R: Read+Seek>(r) -> Result<Vec<NarLsEntry>>` — sibling to `parse()` (~line 238); tracks `stream_position()`; for `Regular`, records `nar_offset` after `"contents"` length-prefix, **then streams the `size` bytes through `blake3::Hasher` while seeking** (read in 64 KiB blocks; the bytes are touched once, here). `NarLsEntry { …, file_digest: [u8;32] }`. `// r[impl store.index.nar-ls-offset]` `// r[impl store.index.file-digest]` |
| `rio-nix/fuzz/fuzz_targets/nar_ls.rs` + `Cargo.toml` + `nix/fuzz.nix` | new |
| tests | proptest: `serialize(tree)` → `nar_ls` → `&nar[off..off+size] == content` AND `file_digest == blake3(content)`. `// r[verify ...]` |

**Exit:** `.#ci` green incl. `fuzz-nar_ls`.

### ~~P0547 — rio-common: chunk-range math~~ **DROPPED**
Under Path A this mapped `(cookie, off, len) → chunk-range` for the cachefiles `READ` handler. Path C's digest-FUSE keys by `file_digest`, not byte offset. The replacement is **P0570** (`DigestResolver`), which still does a cumsum binsearch internally but is keyed differently and lives in `rio-builder` (it's builder-local state, not a generic primitive).

### P0548 — TieredChunkBackend (object-store authoritative; local-FS read-through cache)
**Unchanged from V1.** See V1 §P0548 for full file table. `// r[impl store.backend.{tiered-get-fallback,tiered-put-remote-first,fs-put-idempotent}]`. **Exit:** `.#ci` green.

### P0549 — ChunkBackend blob-API
**Unchanged from V1** (now used only by P0566's `narinfo/`/`manifests/` sidecars; no `boot/` namespace). **Exit:** `.#ci` green.

### P0568 — Batched `GetChunks` server-stream + prost-bytes + tonic residuals + obs
**Unchanged from V1.** Spike-validated 2026-04-05 (`spike/rtt-bench` @ `96cfd098`). The digest-FUSE `open` handler (P0559) is the consumer instead of the cachefiles `READ` handler; `fetch_chunks_parallel` callsite moves but the RPC shape is identical. **Exit:** `.#ci` green; live A/B ≥4× cold-fetch reduction.

### P0550 — fetch.rs core hoist (NOT a pure mv)
**Unchanged from V1.** Hoists `StoreClients` + `fetch_chunks_parallel` to `rio-builder/src/store_fetch.rs`; leaves old-FUSE-typed wrappers in `fuse/fetch.rs` until P0560 deletes them. **Exit:** `.#ci` green; existing FUSE VM tests unchanged.

### P0572 — Directory merkle layer: `dir_digest`/`root_digest` + `directories` table
**Crate:** `rio-proto, rio-nix, rio-store` · **Deps:** P0545, P0546 · **Complexity:** LOW (~50 LoC compute + table)

Closes the subtree-merkle gap vs snix at **zero serving-path cost** — Path C's mount stack and digest-FUSE are unaffected. The work happens once at PutPath/index time (<1 ms on top of P0546's blake3 pass; bytes already in RAM).

| File | Change |
|---|---|
| `rio-proto/proto/types.proto` | `NarIndexEntry { …; bytes dir_digest = 8; }` (populated when `kind==DIR`; blake3 of canonical Directory encoding); `NarIndex { …; bytes root_digest = 2; }` |
| `rio-proto/proto/castore.proto` | new — vendor snix [`castore.proto`](https://git.snix.dev/snix/snix/raw/branch/canon/snix/castore/protos/castore.proto) (MIT): `message Directory { repeated DirectoryNode directories; repeated FileNode files; repeated SymlinkNode symlinks; }` with `FileNode{name, digest, size, executable}`, `DirectoryNode{name, digest, size}`, `SymlinkNode{name, target}`. **Pin canonical encoding rule** in a doc-comment: fields sorted by `name` (bytes-lex), no unknown fields, prost's default field-order encode. **snix issue #111**: prost determinism is not formally guaranteed across versions — add a golden-bytes test that fails loudly on encoder drift. `// r[impl store.castore.canonical-encoding]` |
| `rio-nix/src/nar.rs` | `nar_ls` second pass (bottom-up over the entry list, deepest-first): for each `kind==DIR`, build `Directory{…}` from immediate children's `file_digest`/`dir_digest`/`target`, encode, `dir_digest = blake3(encoded)`. `root_digest` = top dir's `dir_digest`. ~50 LoC. `// r[impl store.index.dir-digest]` |
| `migrations/033_nar_index.sql` (P0551 — same migration) | `+ CREATE TABLE directories (digest bytea PRIMARY KEY, body bytea NOT NULL);` |
| `rio-store/src/nar_index.rs` (P0552) | after `set_nar_index`: `INSERT INTO directories … ON CONFLICT (digest) DO NOTHING` for each dir entry. |
| tests | proptest: `serialize(tree)` → `nar_ls` → re-derive `dir_digest` from children == stored value. snix-interop golden: known tree → `root_digest` matches snix's `tvix-store import` output (fixture bytes pinned). `// r[verify store.index.dir-digest]` `// r[verify store.castore.canonical-encoding]` |

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
| `migrations/033_nar_index.sql` | `CREATE TABLE nar_index (store_path_hash bytea PRIMARY KEY REFERENCES manifests ON DELETE CASCADE, entries bytea NOT NULL, created_at timestamptz DEFAULT now()); ALTER TABLE manifests ADD COLUMN nar_indexed boolean NOT NULL DEFAULT false; CREATE INDEX manifests_nar_index_pending_idx ON manifests(updated_at) WHERE NOT nar_indexed AND status='complete';` — **partial-index work-queue** (precedent: migration 031's `WHERE status='uploading'`). PG forbids subqueries in partial-index predicates, so the queue is a same-table bool flag; indexer flips `nar_indexed=true` on success (HOT-update eligible). Replaces V1's `boot_size IS NULL` sentinel. |
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

### P0553 — terraform: FSx cache tier + dedicated store SG/NodeClass + csi-driver
**Unchanged from V1.** See V1 §P0553. **Exit:** `tofu apply` creates FS + store SG/NodeClass; `.#ci` green.

### P0554 — helm: store PVC + chunkBackend.tiered + zone-pin
**Unchanged from V1.** See V1 §P0554. **Exit:** `helm template --set store.chunkBackend.kind=tiered` renders; `.#ci` green.

### P0555 — VM test: tiered-backend cache semantics
**Unchanged from V1.** See V1 §P0555. **Exit:** `nix build .#checks.x86_64-linux.vm-store-tiered` green; `.#ci` green.

### P0566 — Self-describing S3 bucket (narinfo + manifest sidecar, direct object-store write)
**Unchanged from V1.** See V1 §P0566. **Exit:** `.#ci` green.

---

## Phase 4 — composefs store-side (gated on Phase-0 PASS + P0546)

### P0556 — `composefs/dump.rs` serializer + `mkcomposefs` golden VM test
**Crate:** `rio-builder, nix` · **Deps:** P0569 PASS, P0546 · **Complexity:** MED (~250 LoC)

The encoder is a `NarIndex` → [`composefs-dump(5)`](https://github.com/containers/composefs/blob/main/man/composefs-dump.md) text serializer + a subprocess call to `mkcomposefs --from-file`. `mkcomposefs` writes the EROFS metadata image directly with correct `i_size`, redirect xattr from the PAYLOAD field, and 0-byte metacopy (DIGEST=`-`). The dump format is line-oriented; the only fiddly part is path escaping (octal `\ooo` for bytes outside `[!-~]\{/}` per the man page).

| File | Change |
|---|---|
| `rio-builder/src/composefs/dump.rs` | `pub fn write_dump<W: Write>(w, roots: &[(StorePath, &NarIndex)]) -> io::Result<()>` — emits one line per node: `<escaped /nix/store/…/path> <size> <mode_octal> 1 0 0 0 0.0 - - - <xattrs…>` where regular files carry **explicit trailing xattrs** `user.overlay.redirect=/{file_digest[..2]}/{file_digest[2..]} user.overlay.metacopy=` (PAYLOAD field is `-`; see encode.rs note). Dirs/symlinks: no xattrs. Parent-before-child ordering (sort by path). One synthetic root `/` + `/nix` + `/nix/store` dir lines. **Real `i_size` from `entry.size`** (§C.2 — mkcomposefs writes it into the inode even with PAYLOAD=`-`, verified). `// r[impl builder.fs.stub-isize]` `// r[impl builder.fs.metacopy-xattr-shape]` |
| `rio-builder/src/composefs/encode.rs` | `pub fn build_image(roots, out: &Path) -> Result<ImageMeta>` — `write_dump` to a tempfile → `Command::new("mkcomposefs").args(["--from-file", dump, out])` → return `ImageMeta { size, sha256 }`. **Xattr prefix MUST be `user.overlay.{redirect,metacopy}`** (P0541: unpriv overlay with `userxattr` reads only `user.*`). **`mkcomposefs --user-xattrs` does NOT switch the generated prefix** — empirically verified 2026-04-05: the flag is an input-xattr filter only (`LCFS_BUILD_USER_XATTRS` doc-comment, `lcfs-writer.h:27`); generated redirect/metacopy are hardcoded `trusted.` (`OVERLAY_XATTR_PREFIX`, `lcfs-internal.h:37-50`, set at `lcfs-writer-erofs.c:1173,1184`). Hence dump.rs writes PAYLOAD=`-` (suppresses auto-`trusted.overlay.redirect`) and supplies `user.overlay.{redirect,metacopy}` as explicit xattr fields. mkcomposefs still auto-emits a stray `trusted.overlay.metacopy` for size>0 metadata-only inodes — harmless (`-o userxattr` ignores `trusted.*`; ~24 B/file image overhead). Alternative if image size matters: ~10-line nixpkgs overlay patch adding `LCFS_BUILD_USERXATTR_OVERLAY` to switch the prefix (upstreamable). mkcomposefs binary path from `RIO_MKCOMPOSEFS` env (set by Nix wrapper) or `which`. `// r[impl builder.fs.composefs-encode]` |
| `rio-builder/src/composefs/mod.rs` | `pub mod dump; pub mod encode;` |
| `nix/tests/scenarios/composefs-encoder.nix` | standalone (no k8s): fixture NarIndex (3 store paths, ~50 files, one >u32::MAX sparse) → `build_image` → `losetup + mount -t erofs` → `find -printf '%s %p\n'` matches NarIndex; `getfattr -n user.overlay.redirect` on a regular file matches `/{digest[..2]}/{digest[2..]}`; `getfattr -n trusted.overlay.redirect` returns `ENODATA` (asserts the prefix switch landed). **Reuses spike harness `nix/tests/lib/spike_stage.py` patterns** (consolidated on `adr-022`, `15a9db79`). |
| `nix/tests/default.nix` | `# r[verify builder.fs.{stub-isize,composefs-encode,metacopy-xattr-shape}]` at `subtests=["golden-loop-mount"]` |
| `flake.nix` dev shell + `nix/nixos-node/` | `+ pkgs.composefs` (provides `mkcomposefs`) |
| `rio-builder/fuzz/fuzz_targets/composefs_dump.rs` + wiring | fuzz the dump-text escaper: arbitrary `NarIndex` → `write_dump` → `mkcomposefs --from-file` exits 0 (no parse error) |

**Shell-out vs port:** start with subprocess `mkcomposefs` (build-time tool, not data-plane; ~70 ms for chromium). Porting [`lcfs-writer-erofs.c`](https://github.com/containers/composefs/blob/main/libcomposefs/lcfs-writer-erofs.c) (~2.1 kLoC, `GPL-2.0-or-later OR Apache-2.0`) is a followup IF (a) subprocess overhead shows in V4, OR (b) we need in-process error reporting. **Decision: subprocess.** `WONTFIX(P0556)` comment at the `Command::new` site.

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

### ~~P0558 — GC sweep deletes boot/~~ **DROPPED**
No `boot/` artifacts under Path C. `nar_index` rows cascade-delete with `manifests` (P0551 FK).

---

## Phase 5 — composefs builder-side

### P0559 — `composefs/{digest_fuse,circuit}.rs`
**Crate:** `rio-builder` · **Deps:** P0545, P0550, P0568, P0570 · **Complexity:** MED (~450 LoC)
| File | Change |
|---|---|
| `rio-builder/src/composefs/digest_fuse.rs` | `fuser::Filesystem` impl rooted at the per-build mount `/var/rio/objects/{build_id}` exposing exactly two levels: 256 prefix dirs (`00`..`ff`, static inodes 2-257) + leaf files named by remaining 62 hex chars. `lookup(parent, name)`: parent==ROOT → prefix-dir inode; parent==prefix → parse `[prefix‖name]` as `[u8;32]`, `resolver.resolve(digest)` → `FileAttr{ size, mode: if executable {0o555} else {0o444}, ino: hash-derived }`; unknown → `ENOENT` (declared-input allowlist — JIT-fetch imperative). `open(ino)`: look up backing path in **shared node-SSD cache** `/var/rio/cache/{aa}/{rest}` (P0571); if present → `reply.opened(fh, FOPEN_KEEP_CACHE)`; if absent → `O_EXCL`-create `/var/rio/cache/{aa}/{rest}.partial` (a concurrent fetcher that loses the `O_EXCL` race inotify-waits for the `.partial`→final rename instead of double-fetching) → `circuit.call(\|\| store_fetch::fetch_chunks_parallel(clients, resolver.chunks_for(digest)))` writing into `.partial`, **verifying each chunk's blake3 against its content-address on arrival** (§C.6; chunks are blake3-addressed in the CAS layer — never serve a byte from an unverified chunk; mismatch → `EIO` + `error!` log) → on completion, whole-file blake3-verify against `file_digest` → `rename .partial → final` → opened. `read(ino, fh, off, size)`: `pread` the backing file. **No `O_DIRECT`, no aligned-bounce, no ioctl** (those were cachefiles-specific). Per-open `tokio::time::timeout(jit_fetch_timeout)`; on expiry → `EIO`. Reuses `rio-builder/src/fuse/ops.rs` skeleton for `init`/`getattr`/`readdir` boilerplate; **prototype: `rio-builder/src/bin/spike_digest_fuse.rs` on `adr-022` (`af8db499`)**. `// r[impl builder.fs.{digest-fuse-open,file-digest-integrity,shared-backing-cache}]` |
| `rio-builder/src/composefs/circuit.rs` | port of `fuse/circuit.rs` — breaker around `fetch_chunks_parallel`. A store outage must trip the breaker, not stream EIOs into every `open()` on the node. `// r[impl builder.fs.fetch-circuit]` |
| `rio-builder/src/composefs/mod.rs` | `pub mod digest_fuse; pub mod circuit; pub mod resolver; pub mod dump; pub mod encode; pub mod mount;` |
| `rio-builder/src/lib.rs` | `pub mod composefs;` + `rio_builder_digest_fuse_{open_seconds,fetch_bytes_total{hit},integrity_fail_total,eio_total}`. `// r[impl obs.metric.digest-fuse]` |
| tests | unit: `lookup` hex parsing round-trip; `resolve(unknown) == ENOENT`. `// r[verify builder.fs.digest-fuse-open]` |

**The partial-file trade-off (ADR-022 §C.7):** `open()` of a 200 MB `.so` blocks for the whole file on first touch. P0571's node-SSD cache amortizes to once-per-node-lifetime; **P0575 (streaming-open, unconditional)** makes that first open fast. The simple whole-file path here applies only to files ≤ `STREAM_THRESHOLD`.

**Exit:** `.#ci` green (unit only).

### P0567 — `rio-mountd` DaemonSet (fd-handoff only: `/dev/fuse` + EROFS mount-fd)
**Crate:** `rio-builder, infra` · **Deps:** P0576, P0541 · **Complexity:** LOW (~50 LoC + helm)

P0541 PASS routes this to its minimal shape. The unprivileged builder cannot (a) loop-mount EROFS (no `FS_USERNS_MOUNT`) nor (b) open `/dev/fuse` without device-plugin/privilege. One DaemonSet per node with init-ns `CAP_SYS_ADMIN` does **only**: open `/dev/fuse` O_RDWR; `losetup_ro(image)`; `fsopen("erofs")`/`fsconfig(SET_STRING,"source",loop)`/`fsconfig(SET_FLAG,"ro")`/`fsconfig(CMD_CREATE)`/`fsmount(NODEV)`; SCM_RIGHTS **both** fds back; close locals; exit handler. **No persistent state, no overlay mount, no upcall relay.** Builder does FUSE-serve (on the received fuse-fd) + `move_mount` (the received erofs-fd) + overlay mount itself.

| File | Change |
|---|---|
| `rio-builder/src/bin/rio-mountd.rs` | new — listens on `/run/rio-mountd.sock`. On `Mount{image_path, objects_dir}` (image at hostPath `/var/rio/images/{build_id}.erofs`): `fuse_fd = open("/dev/fuse", O_RDWR)`; `mount("none", objects_dir, "fuse.rio-digest", MS_NODEV|MS_NOSUID, "fd=<fuse_fd>,rootmode=40555,user_id=<req_uid>,group_id=<req_gid>,allow_other,default_permissions")` (spike Q1 shape — root mounts, then passes fd; builder serves via `fuser::Session::from_fd`); `loop_dev = losetup_ro(image_path)?`; `fsopen("erofs")` → `fsconfig(SET_STRING,"source",loop_dev)` → **`fsconfig(SET_FLAG,"ro")`** → `fsconfig(CMD_CREATE)` → `erofs_fd = fsmount(_, 0, MOUNT_ATTR_NODEV)`; reply with `[fuse_fd, erofs_fd]` via SCM_RIGHTS; close both; **on UDS conn-drop (builder closes or pod exits): `umount2(objects_dir, MNT_DETACH)` + `losetup -d` + `rmdir(objects_dir)`**. **Start-up:** scan `/var/rio/objects/*` for orphaned mounts from a prior crash and detach them. `// r[impl builder.mountd.erofs-handoff]` |
| `infra/helm/rio-build/templates/mountd-ds.yaml` | new — DaemonSet, hostPath `/run/rio-mountd.sock` + `/var/rio/images` + `/dev/fuse`. `securityContext: {privileged: false, capabilities.add: [SYS_ADMIN]}`, seccomp `RuntimeDefault`. nodeSelector: builder/fetcher nodepools. |
| `docs/src/components/builder.md` | `r[builder.mountd.erofs-handoff]` spec |

**Exit:** `.#ci` green; exercised end-to-end by P0560§B.

### P0571 — Shared node-SSD backing cache + per-build FUSE mount
**Crate:** `rio-builder, infra` · **Deps:** P0559 · **Complexity:** LOW

`r[builder.fs.shared-backing-cache]`: the FUSE **mount point** is per-build (`/var/rio/objects/{build_id}/`, created by P0567 `rio-mountd`) for cross-pod isolation. The **backing cache** where fetched bytes land is shared node-SSD (`/var/rio/cache/ab/<digest>`). On `open()`, the handler checks the shared cache first; on miss it `O_EXCL`-creates `/var/rio/cache/ab/<digest>.partial` — a concurrent fetcher losing the `O_EXCL` race waits on inotify for the `.partial`→final rename instead of double-fetching. This gives §C.7's node-level dedup ("second build pays 0") without exposing one build's mount to another.

| File | Change |
|---|---|
| `rio-builder/src/composefs/digest_fuse.rs` | `cache_dir` is a hostPath (`/var/rio/cache`), NOT pod-ephemeral. `open()` checks disk first (already in P0559 spec); this plan adds: (a) startup `statvfs` watermark check — if free < 10%, spawn LRU sweep (atime-ordered `readdir` + `unlink` until free > 20%); (b) `rio_builder_objects_cache_{hit_total,bytes,evicted_total}` metrics. `// r[impl builder.fs.node-digest-cache]` |
| `infra/helm/rio-build/templates/builder-sts.yaml` | hostPath volume `/var/rio/cache` mounted RW; `/var/rio/objects` mounted RW (per-build subdirs created by mountd) |
| `nix/nixos-node/eks-node.nix` | `systemd.tmpfiles.rules = ["d /var/rio/cache 0755 root root -" "d /var/rio/objects 0755 root root -"]` |

**This is the §C.7 amortizer:** the second build on a node to touch `libLLVM.so` pays 0 fetch. Identical files across store paths share one entry (structural — the redirect target is the digest).

**FSx-backed cluster-wide cache rejected** — violates builder air-gap (`project_builder_airgap.md`): a shared writable FS across untrusted builders is a cache-poisoning + lateral-movement surface. Per-node SSD; P0575 fires O(nodes×giants).

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

**Deletion inventory** (cutover earns back code — **bigger than V1**: no cachefiles UDS client either):

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

**Net:** ~**−4 600 LoC** (V1 was −3 000; V2 additionally never *adds* the ~900 LoC cachefiles client + ~300 LoC cachefilesd that V1 would have, and the encoder is ~550 LoC smaller). The `rio-builder/src/fuse/` directory reduces to nothing; `rio-builder/src/composefs/` is ~700 LoC total.

#### §B — `nix`: fixture kernel cutover + vm:composefs end-to-end
**Complexity:** HIGH

| File | Change |
|---|---|
| `nix/tests/fixtures/k3s-prod-parity.nix` | unconditionally `imports = [ ../../nixos-node/kernel.nix ]`; deploy `rio-mountd` DS in-cluster; hostPath `/var/rio/{objects,images}` |
| `nix/tests/scenarios/composefs-e2e.nix` | fixture `{storeReplicas=1;}`. `cold-read`: build drv that `dd bs=4k count=1` from a 100 MB input → assert `digest_fuse_open_seconds_count > 0` AND `fetch_bytes_total{hit="remote"} ≈ 100 MiB` (whole-file — this IS §C.7's behavior) AND `dd` output correct. `warm-read`: second `dd` same file → `open_seconds_count` unchanged (node-SSD hit). `cross-build-dedup`: two drvs with one shared input file → second build's `fetch_bytes_total{hit="node_ssd"} > 0`. `eio-on-fetch-fail`: stop rio-store mid-open → opener sees `EIO` (not hang) within `jit_fetch_timeout` + `is_input_materialization_failure` classifies as infra-retry. `integrity-fail`: corrupt one chunk in the store backend → opener sees `EIO` + `integrity_fail_total == 1`. `stat-kernel-side`: `find /nix/store -type f -printf '%s\n' \| sha256sum` matches expected with `digest_fuse_open_seconds_count == 0` (no upcalls for stat-walk). |
| `nix/tests/scenarios/{lifecycle,protocol,gc,...}.nix` | **sweep:** delete every old-FUSE-specific assertion (`fuse_cache_hits`, `/var/rio/fuse-store`). **Drop all `smarter-devices/*` from worker pod fixtures** — fuse via rio-mountd fd-pass, kvm via hostPath. |
| `nix/tests/default.nix` | `# r[verify builder.fs.{composefs-stack,userxattr-mount,fd-handoff-ordering,digest-fuse-open,shared-backing-cache,file-digest-integrity,node-digest-cache,digest-resolve,streaming-open-threshold}]` `# r[verify builder.overlay.composefs-lower]` `# r[verify builder.result.input-eio-is-infra]` `# r[verify builder.mountd.erofs-handoff]` `# r[verify obs.metric.digest-fuse]` at `subtests=[...]`; spike harness `nix/tests/{scenarios/composefs-spike*.nix, lib/spike_stage.py, lib/chromium-tree.tsv.zst}` consolidated on `adr-022` (`15a9db79`) as regression guards; `timeout=1800` |

**Exit (whole P0560):** `nix build .#checks.x86_64-linux.vm-composefs-e2e` green; full `.#ci` green with composefs as the only lower.

### P0562 — Post-cutover audit  ★ CUTOVER GATE (U1)
**Crate:** `nix` · **Deps:** P0560 · **Complexity:** LOW

| Check | How |
|---|---|
| No old-FUSE markers remain | `tracey query rule builder.fuse.*` returns empty |
| No old-FUSE / device-plugin strings in code/helm | `grep -rn 'fuse_cache\|/var/rio/fuse-store\|fuseCacheSize\|NixStoreFs\|smarter-devices\|smarter-device-manager\|rio-builder-fuse\|fuseMaxDevices\|kvmMaxDevices' rio-*/ infra/ nix/` returns empty |
| No cachefiles strings (V1 leakage) | `grep -rn 'cachefiles\|CACHEFILES\|boot_blob\|boot_size' rio-*/ infra/ nix/ docs/src/components/` returns empty (ADR-022 keeps the term, components/ shouldn't) |
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
| `docs/src/runbooks/tiered-cache-cutover.md` | **unchanged from V1** |
| `docs/src/runbooks/composefs-cutover.md` | (1) ensure FSx flip done; (2) `xtask k8s eks down && up` from a P0562-green commit (greenfield — `nar_index` populates from scratch via PutPath eager + indexer_loop); (3) `xtask stress chromium`; (4) compare `fetch_bytes_total{hit="remote"}` — expect ≥10× reduction vs whole-NAR baseline on builds that touch <10% of files; expect `objects_cache_hit_ratio` climbing on repeat builds; (5) rollback = `down && up` from pre-P0560 commit |

**Exit:** `.#ci` green.

### P0575 — §C.7 mitigation (i): streaming `open()` for large files
**Crate:** `rio-builder` · **Deps:** P0559, P0570 · **Complexity:** LOW (~80 LoC) · **Priority: same tier as P0559**

**Unconditional** — top1000.csv shows all 1000 largest nixpkgs files >64 MiB (248 `.so`/`.a`, max 1.88 GiB); access-probe `42aa81b2` shows real consumers touch 0.3-33% (bimodal head+tail or scattered); spike `15a9db79` proves the mechanism works. The earlier "V12-gated / WONTFIX-if-under-64MiB" framing is obsolete.

| File | Change |
|---|---|
| `rio-builder/src/composefs/digest_fuse.rs` | `open()` always replies with `FOPEN_KEEP_CACHE` (spike-proven: KEEP_CACHE does **not** suppress cold-page upcalls, only prevents invalidation across opens — there is no mode-transition step). Files where `size ≤ STREAM_THRESHOLD` (config; default 8 MiB, V12 tunes): P0559's simple path — fetch-whole → blake3-verify → write → rename → return. Files `> STREAM_THRESHOLD`: spawn fill task writing into `/var/rio/cache/{aa}/{rest}.partial` (`O_EXCL`-created; concurrent fetchers inotify-wait on rename per P0571); return from `open()` after the **first chunk** lands. The fill task **verifies each chunk's blake3 against its content-address on arrival** (§C.6; never serve a byte from an unverified chunk). `read(off, len)`: if `[off, off+len)` filled → serve; else priority-bump that range on the fill task's queue and condvar-wait. On full-file completion → whole-file blake3-verify against `file_digest` → `rename .partial → final` (the rename is gated on the whole-file check; ≤threshold path runs the same check before `open()` returns). `mmap(MAP_PRIVATE)` page-faults route through this same `read()` path (spike-verified) — linker path covered. **Prototype: `rio-builder/src/bin/spike_stream_fuse.rs` on `adr-022` (`15a9db79`).** `// r[impl builder.fs.streaming-open]` |
| same | This IS the per-read-upcall behavior ADR-022 §1 rejected for the warm path — but it applies **only during the cold-fill window of the first open of a large file on that node**. After fill: **0 upcalls while pages remain cached**; under cgroup memory pressure evicted pages re-upcall and are re-served from the SSD backing file. The fill window cost is exactly `filesize / 128 KiB` upcalls, once. |
| `rio-builder/src/config.rs` | `stream_threshold_bytes: u64` (default `8 * 1024 * 1024`). |

**Exit:** `vm-composefs-e2e` `cold-read` of 256 MiB file: first `pread(4k, off=0)` returns in <50 ms; second full `dd` shows 0 read upcalls; full-file blake3 verified before `.partial → final` rename.

---

## Phase 7 — Directory DAG / delta-sync (U5; parallel with Phases 4-6 after P0546+P0572)

### P0573 — DirectoryService RPC surface
**Crate:** `rio-proto, rio-store` · **Deps:** P0572 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-proto/proto/store.proto` | `rpc GetDirectory(GetDirectoryRequest) returns (Directory)`; `rpc HasDirectories(HasDirectoriesRequest) returns (HasBitmap)`; `rpc HasBlobs(HasBlobsRequest) returns (HasBitmap)` — all batch (`repeated bytes digests = 1`; `HasBitmap { bytes bitmap = 1; }` one bit per request index, per **I-110 lesson**: per-digest unary against a 50k-node DAG is the PG wall again). Wire-compatible with snix `castore.proto`'s `DirectoryService.Get` where it overlaps. `// r[impl store.castore.directory-rpc]` |
| `rio-store/src/grpc/directory.rs` | new — `get_directory(digest)`: `SELECT body FROM directories WHERE digest=$1`. `has_directories(digests)`: `SELECT digest FROM directories WHERE digest = ANY($1)` → bitmap. `has_blobs(file_digests)`: `SELECT digest FROM file_blobs WHERE digest = ANY($1)` → bitmap. |
| `migrations/033_nar_index.sql` (via P0572) | `CREATE TABLE file_blobs(digest bytea PRIMARY KEY, nar_hash bytea NOT NULL, nar_offset bigint NOT NULL);` — populated in P0572's bottom-up pass via `INSERT … ON CONFLICT(digest) DO NOTHING`. **GIN-on-`nar_index.entries` is not viable**: `entries` is BYTEA (encoded proto), not JSONB, so a GIN expression index would require a proto-decoding `IMMUTABLE` PG function tied to the wire format (versioning hazard). The separate table is the derived index for `HasBlobs` AND carries the `(nar_hash, nar_offset)` coords P0574's `dag_sync` needs AND is what a future `BlobService.Read(file_digest)` keys on. |
| `rio-store/src/lib.rs` | `rio_store_directory_{get_seconds,has_batch_size}` |
| tests | ephemeral PG: PutPath nested tree → `GetDirectory(root_digest)` returns correct children; `HasDirectories([root, unknown])` → `[1,0]` bitmap. `// r[verify store.castore.directory-rpc]` |

**Exit:** `.#ci` green.

### P0574 — Gateway substituter: Directory-DAG delta-sync client  ★ U5 LANDS
**Crate:** `rio-gateway` · **Deps:** P0573 · **Complexity:** MED
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
{"plan":576,"title":"EXT: nixos-cutover landed (kernel.nix ≥6.16 importable + /dev/fuse + AMI; OVERLAY_FS/EROFS_FS/FUSE_FS =y)","deps":[],"crate":"ext","priority":99,"status":"RESERVED","complexity":null,"note":"sentinel; coordinator flips DONE when nixos-cutover agent merges. ≥6.16 for 5ef7bcdeecc9 (data-only-lower redirect under userxattr). NO CACHEFILES/ONDEMAND options needed under Path C. (Was V1's P0539; renumbered — dag.jsonl P0539 = Observability stack.)"}
{"plan":569,"title":"SPIKE sentinel: composefs-style validated at chromium scale (mount<10ms, warm=0 upcalls, mkcomposefs i_size correct)","deps":[],"crate":"spike","priority":99,"status":"DONE","complexity":null,"note":"consolidated 15a9db79 on adr-022; ADR-022 reopened (4a716900..b6794962)"}
{"plan":540,"title":"SPIKE: chunk-indexed EROFS encoder + loop-mount (Path-A fallback)","deps":[],"crate":"spike","priority":50,"status":"DONE","complexity":"LOW","note":"retained off critical path; PASS 2026-04-05"}
{"plan":541,"title":"SPIKE: composefs privilege boundary (userns-overlay/erofs-loop-unpriv/fsmount-handoff-erofs/fuse-dev-fd-handoff/teardown-under-load)","deps":[],"crate":"spike,nix","priority":95,"status":"DONE","complexity":"MED","note":"all 6 PASS, kernel 6.18.20; commit af8db499 on adr-022; overlay stays in builder via userxattr; mountd hands off /dev/fuse+erofs fds only"}
{"plan":543,"title":"V4/V11/V12 + closure-paths + max-nar-size + aarch64 kernel-config sanity","deps":[],"crate":"xtask,nix","priority":90,"status":"UNIMPL","complexity":"LOW","note":"V12 tunes STREAM_THRESHOLD (P0575 ships unconditionally); closure_paths<65535 gate REMOVED (no device table)"}
{"plan":544,"title":"Spec scaffold: ADR-022 §C merge + ADR-023 (tiered) + ADR-024 stub + r[...] markers","deps":[],"crate":"docs","priority":95,"status":"UNIMPL","complexity":"LOW","note":"merges adr-022 4a716900..b6794962 (9 builder.fs.* markers); tracey markers MUST precede r[impl]"}
{"plan":545,"title":"proto: NarIndex (+file_digest) / GetNarIndex","deps":[544],"crate":"rio-proto","priority":90,"status":"UNIMPL","complexity":"LOW","note":"no boot_blob"}
{"plan":546,"title":"rio-nix nar_ls (offset-tracking + blake3-per-file) + fuzz","deps":[544,545],"crate":"rio-nix","priority":90,"status":"UNIMPL","complexity":"MED","note":"blake3 streamed once; populates file_digest"}
{"plan":548,"title":"TieredChunkBackend (S3 authoritative; local-FS read-through cache)","deps":[544],"crate":"rio-store","priority":90,"status":"UNIMPL","complexity":"MED","note":"unchanged from V1"}
{"plan":549,"title":"ChunkBackend blob-API (put_blob/get_blob/delete_blob)","deps":[544,548],"crate":"rio-store","priority":85,"status":"UNIMPL","complexity":"LOW","note":"serialise after 548; used by P0566 narinfo/manifests sidecar only"}
{"plan":550,"title":"Hoist StoreClients+fetch_chunks_parallel → store_fetch.rs (NOT pure mv)","deps":[544],"crate":"rio-builder","priority":85,"status":"UNIMPL","complexity":"MED","note":"unchanged from V1; fetch.rs:20,32-33 imports fuser"}
{"plan":568,"title":"Batched GetChunks server-stream (K_server=256) + prost .bytes() + tonic residuals + obs","deps":[545,550],"crate":"rio-proto,rio-store,rio-builder,infra","priority":85,"status":"UNIMPL","complexity":"MED","note":"unchanged from V1; spike-validated 96cfd098"}
{"plan":570,"title":"DigestResolver: file_digest → (nar_hash, nar_offset, size) → chunk-range","deps":[544,545,550],"crate":"rio-builder","priority":85,"status":"UNIMPL","complexity":"LOW","note":"replaces V1 P0547 at the digest-FUSE open path"}
{"plan":551,"title":"migration 033_nar_index + manifests.nar_indexed bool + queries (no 034)","deps":[545],"crate":"rio-store","priority":85,"status":"UNIMPL","complexity":"LOW","note":"partial-index work-queue WHERE NOT nar_indexed (precedent: 031); PG forbids cross-table predicate"}
{"plan":552,"title":"GetNarIndex handler + indexer_loop","deps":[545,546,551],"crate":"rio-store","priority":85,"status":"UNIMPL","complexity":"MED","note":"nar_index_sync_max_bytes guard; entries carry file_digest"}
{"plan":553,"title":"infra/eks/fsx.tf cache tier (no DRA) + dedicated rio-store SG/NodeClass + csi IRSA","deps":[548],"crate":"infra","priority":80,"status":"UNIMPL","complexity":"MED","note":"unchanged from V1"}
{"plan":554,"title":"helm store-pvc + chunkBackend.tiered + zone-pin to FSx AZ","deps":[548,553],"crate":"infra,xtask","priority":80,"status":"UNIMPL","complexity":"MED","note":"unchanged from V1; FIRST SHIPPED VALUE (U2)"}
{"plan":555,"title":"VM test: tiered-backend cache semantics","deps":[548,554],"crate":"nix","priority":80,"status":"UNIMPL","complexity":"MED","note":"unchanged from V1"}
{"plan":566,"title":"Self-describing S3 bucket (narinfo+manifests sidecar)","deps":[549,554],"crate":"rio-store","priority":75,"status":"UNIMPL","complexity":"LOW","note":"unchanged from V1"}
{"plan":556,"title":"composefs/dump.rs (NarIndex→composefs-dump(5)) + mkcomposefs golden VM + fuzz","deps":[569,546],"crate":"rio-builder,nix","priority":85,"status":"UNIMPL","complexity":"MED","note":"~250 LoC; subprocess mkcomposefs; PAYLOAD=- + explicit user.overlay.{redirect,metacopy} xattrs (--user-xattrs is input-filter only, verified)"}
{"plan":557,"title":"PutPath eager nar_index compute (try_acquire-gated; no encode)","deps":[551,552],"crate":"rio-store","priority":80,"status":"UNIMPL","complexity":"LOW","note":"nar_ls+blake3 while NAR in RAM; no S3 artifact"}
{"plan":559,"title":"composefs/{digest_fuse,circuit}.rs (2-level digest dir; FOPEN_KEEP_CACHE; blake3-verify on open)","deps":[545,550,568,570],"crate":"rio-builder","priority":80,"status":"UNIMPL","complexity":"MED","note":"~450 LoC; prototype spike_digest_fuse.rs; no O_DIRECT/ioctl/copen"}
{"plan":567,"title":"rio-mountd DaemonSet (open /dev/fuse + fsopen('erofs')/ro/fsmount; SCM_RIGHTS both fds; exits)","deps":[576,541],"crate":"rio-builder,infra","priority":80,"status":"UNIMPL","complexity":"LOW","note":"~50 LoC; smaller priv surface than today's FUSE setup; overlay mount stays in builder (userxattr)"}
{"plan":571,"title":"Shared node-SSD /var/rio/cache backing + per-build /var/rio/objects/{build_id} mount + LRU watermark sweep","deps":[559],"crate":"rio-builder,infra","priority":80,"status":"UNIMPL","complexity":"LOW","note":"r[builder.fs.shared-backing-cache]; O_EXCL .partial + inotify-wait. FSx cluster-wide cache REJECTED — builder air-gap"}
{"plan":575,"title":"§C.7 mitigation (i): streaming open() for files > STREAM_THRESHOLD (KEEP_CACHE always; priority-bump read; mmap covered)","deps":[559,570],"crate":"rio-builder","priority":80,"status":"UNIMPL","complexity":"LOW","note":"~80 LoC; spike 1dad4f3c proves no mode-flip; top1000.csv+da6148cd prove necessity"}
{"plan":560,"title":"[ATOMIC] composefs cutover: §A mount+overlay+DELETE old-FUSE (~-4600 LoC) §B fixture kernel + vm:composefs-e2e + spike-regression cherry-pick","deps":[576,556,557,559,567,571,575],"crate":"rio-builder,nix","priority":80,"status":"UNIMPL","complexity":"HIGH","note":"hard cutover; one worktree, one PR, one .#ci gate"}
{"plan":562,"title":"Post-cutover audit (tracey builder.fuse.* empty; grep clean incl. cachefiles/boot_blob; .#ci re-run)","deps":[560],"crate":"nix","priority":80,"status":"UNIMPL","complexity":"LOW","note":"CUTOVER GATE"}
{"plan":563,"title":"Metrics: digest-fuse + tiered dashboards + alerts","deps":[544,548,559],"crate":"infra","priority":70,"status":"UNIMPL","complexity":"LOW","note":""}
{"plan":564,"title":"helm cleanup + mountd DS wiring + kernel assertion (drop smarter-device-manager entirely)","deps":[554,560,567],"crate":"infra,rio-controller,nix","priority":75,"status":"UNIMPL","complexity":"LOW","note":"builders privileged:false; DELETE device-plugin.yaml + both NodeOverlays + nixos-node/smarter-device-manager; kvm via hostPath CharDevice + nodeSelector + extra-sandbox-paths (vm-kvm-hostpath-spike PASS)"}
{"plan":565,"title":"Cutover runbooks (FSx, composefs)","deps":[555,562,564],"crate":"docs","priority":65,"status":"UNIMPL","complexity":"LOW","note":""}
{"plan":572,"title":"Directory merkle layer: dir_digest/root_digest in NarIndex + directories+file_blobs tables + bottom-up compute in nar_ls","deps":[545,546],"crate":"rio-proto,rio-nix,rio-store","priority":85,"status":"UNIMPL","complexity":"LOW","note":"U5 foundation; zero serving-path cost; snix castore.proto vendored (MIT); pin canonical encoding (snix #111)"}
{"plan":573,"title":"DirectoryService RPC: GetDirectory / HasDirectories / HasBlobs (batch bitmap; I-110 lesson)","deps":[572],"crate":"rio-proto,rio-store","priority":80,"status":"UNIMPL","complexity":"MED","note":"snix-wire-compatible where overlapping"}
{"plan":574,"title":"Gateway substituter: Directory-DAG delta-sync client (nix copy walks DAG, prunes present subtrees)","deps":[573],"crate":"rio-gateway,nix","priority":75,"status":"UNIMPL","complexity":"MED","note":"U5 LANDS; falls through to chunk-list when remote lacks capability"}
```

---

## tracey `r[…]` marker inventory (P0544 writes spec; later phases write impl/verify)

| Marker | Spec file (P0544) | `r[impl]` (plan) | `r[verify]` site (plan) |
|---|---|---|---|
| `store.backend.fs-put-idempotent` | components/store.md | chunk.rs `put()` (P0548) | vm-store-tiered (P0555) |
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
| `builder.fs.composefs-stack` | decisions/022 §C.1 | composefs/mount.rs (P0560§A) | vm-composefs-e2e `cold-read` (P0560§B) |
| `builder.fs.userxattr-mount` | decisions/022 §C.1 | composefs/mount.rs (P0560§A) | vm-composefs-e2e (P0560§B) + composefs-spike-priv (`adr-022`) |
| `builder.fs.fd-handoff-ordering` | decisions/022 §C.3 | composefs/mount.rs (P0560§A) | vm-composefs-e2e (P0560§B) |
| `builder.fs.stub-isize` | decisions/022 §C.2 | composefs/dump.rs (P0556) | vm-composefs-encoder `golden-loop-mount` (P0556) |
| `builder.fs.metacopy-xattr-shape` | decisions/022 §C.2 | composefs/dump.rs (P0556) | vm-composefs-encoder (P0556) |
| `builder.fs.composefs-encode` | components/builder.md | composefs/encode.rs (P0556) | vm-composefs-encoder (P0556) |
| `builder.fs.digest-fuse-open` | decisions/022 §C.4 | composefs/digest_fuse.rs (P0559) | vm-composefs-e2e `cold-read` (P0560§B) + unit (P0559) |
| `builder.fs.file-digest-integrity` | decisions/022 §C.6 | composefs/digest_fuse.rs (P0559) | vm-composefs-e2e `integrity-fail` (P0560§B) |
| `builder.fs.digest-resolve` | components/builder.md | composefs/resolver.rs (P0570) | proptest (P0570) + vm-composefs-e2e (P0560§B) |
| `builder.fs.fetch-circuit` | components/builder.md | composefs/circuit.rs (P0559) | vm-composefs-e2e `eio-on-fetch-fail` (P0560§B) |
| `builder.fs.node-digest-cache` | components/builder.md | composefs/digest_fuse.rs (P0571) | vm-composefs-e2e `cross-build-dedup` (P0560§B) |
| `builder.fs.shared-backing-cache` | decisions/022 §C.4 | composefs/digest_fuse.rs (P0559+P0571) | vm-composefs-e2e `cross-build-dedup` (P0560§B) |
| `builder.fs.streaming-open` | components/builder.md | composefs/digest_fuse.rs (P0575) | vm-composefs-e2e `cold-read` <50ms (P0560§B) |
| `builder.fs.streaming-open-threshold` | decisions/022 §C.7 | config.rs (P0575) | vm-composefs-e2e `cold-read` (P0560§B) |
| `store.index.dir-digest` | components/store.md | rio-nix/nar.rs (P0572) | proptest (P0572) |
| `store.castore.canonical-encoding` | components/store.md | rio-proto/castore.proto (P0572) | golden-bytes (P0572) |
| `store.castore.directory-rpc` | components/store.md | rio-store/grpc/directory.rs (P0573) | unit (P0573) |
| `gw.substitute.dag-delta-sync` | components/gateway.md | rio-gateway/substitute/dag_sync.rs (P0574) | vm-dag-delta-sync (P0574) |
| `builder.result.input-eio-is-infra` | components/builder.md | executor/mod.rs (P0560§A, ported) | vm-composefs-e2e `eio-on-fetch-fail` (P0560§B) |
| `builder.mountd.erofs-handoff` | components/builder.md | bin/rio-mountd.rs (P0567) | vm-composefs-e2e `cold-read` (P0560§B) |
| `builder.overlay.composefs-lower` | components/builder.md | overlay.rs (P0560§A) | vm-composefs-e2e (P0560§B) |
| `builder.fs.parity` | components/builder.md | (verify-only) | lifecycle (P0562) |
| `store.s3.self-describing` | components/store.md | put_path.rs (P0566) | live: `aws s3 ls narinfo/` (P0566) |
| `obs.metric.chunk-backend-tiered` | observability.md | rio-store/lib.rs (P0548) | vm-store-tiered (P0555) |
| `obs.metric.digest-fuse` | observability.md | rio-builder/lib.rs (P0559) | vm-composefs-e2e (P0560§B) |
| `proto.chunk.bytes-zerocopy` | components/store.md | rio-proto/build.rs (P0568) | unit (P0568) |
| `store.chunk.batched-stream` | components/store.md | rio-store/grpc/chunk.rs (P0568) | live A/B dashboard (P0568) |
| `store.chunk.tonic-tuned` | components/store.md | rio-store/main.rs (P0568) | (config-only) |
| `builder.fetch.batched-stream` | components/builder.md | rio-builder/store_fetch.rs (P0568) | live A/B dashboard (P0568) |
| `infra.fsx.cache-tier` | decisions/023 | infra/eks/fsx.tf (P0553) | (live-only — runbook P0565) |
| `infra.node.kernel-composefs` | deployment.md | nix/nixos-node/kernel.nix (prereq) | nix/checks.nix node-kernel-config (prereq) |

37 markers (V1 had 38; cachefiles/boot-blob markers gone, file_digest/dir_digest/node-cache/mountd/castore/dag-sync added). P0560 DELETES legacy `r[builder.fuse.*]`; P0562 audits via `tracey query uncovered | grep -E 'composefs|tiered|index|digest-fuse'` → empty.
`config.styx` `test_include`: P0544 verifies `rio-nix/src/nar.rs` and `rio-builder/src/composefs/resolver.rs` are in scope (or adds them).

---

## Rollback (one-flag for cache tier, greenfield for builder)

| Layer | Rollback | How |
|---|---|---|
| Tiered cache → direct-S3 | `store.chunkBackend.kind=s3` (helm) | **Unchanged from V1.** Single flag, instant + lossless — S3 was always authoritative. |
| composefs → old-FUSE | **none** (old-FUSE deleted at P0560) | `xtask k8s eks down && up` from a pre-P0560 commit. Greenfield principle. |
| Path C → Path A | **none** | `git checkout` of V1's PLAN-GRAND-REFACTOR + `down && up`. ADR-022 §5 retains A as documented fallback; P0540's spike + V1's P0541 cachefiles findings are the validation. |

**Helm assertion** (`_helpers.tpl`, P0564): `{{- if and .Values.karpenter.enabled (not (has "OVERLAY_FS_DATA_ONLY" .Values.karpenter.amiKernelFeatures)) }}{{ fail "AMI must be built with nix/nixos-node/kernel.nix (≥6.16, EROFS+OVERLAY+FUSE); run xtask ami push" }}{{- end }}`.

---

## File-collision matrix (for `onibus collisions check`)

| File | Touched by | Serialisation |
|---|---|---|
| `rio-store/src/backend/{chunk.rs,tiered.rs,mod.rs}` | P0548, P0549 | P0548 → P0549 (dep edge) |
| `rio-store/src/grpc/mod.rs` | P0552, P0557 | P0552 → P0557 (dep edge) |
| `rio-store/src/grpc/put_path.rs` | P0566, P0557 | independent hunks (sidecar-write vs eager-index); both append after `complete_manifest` — P0557 rebases on P0566 |
| `rio-store/src/nar_index.rs` | P0552 (create), P0557 (extend) | P0552 → P0557 |
| `rio-store/src/lib.rs` | P0548, P0552, P0557 | append-only metric registrations; dep chain serialises |
| `rio-builder/src/composefs/digest_fuse.rs` | P0559 (create), P0571 (cache), P0575 (streaming) | P0559 → P0571 → P0575 |
| `rio-builder/src/composefs/mod.rs` | P0556, P0559, P0570, P0560§A | append-only `pub mod`; P0560 last |
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
| `rio-store/src/nar_index.rs` | P0552, P0557, P0572 | P0552 → P0572 → P0557 |
| `rio-store/src/grpc/directory.rs` | P0573 only | — |
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

# Phase 3 cache-tier flip (FIRST SHIPPED VALUE — unchanged from V1)
nix develop -c cargo xtask k8s -p eks tofu apply -target=aws_fsx_lustre_file_system.chunks
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

- riofs custom kmod (Path B) — A then C won (ADR-022)
- **Path A (EROFS+fscache)** — superseded by C; documented fallback only. V1's cachefiles findings (`.stress-test/sessions/2026-04-05-phase0-gate.md`) and P0540 retained for the record.
- Non-reproducibility `nar_hash` mismatch detection at PutPath
- Per-replica chunk-dedup metrics
- aarch64-specific encoder/mount validation (proptest covers; live aarch64 builder is the soak; P0543 covers config-eval only)
- **`mkcomposefs` Rust port** (`lcfs-writer-erofs.c`) — `WONTFIX(P0556)` unless V4 shows subprocess overhead matters or in-process error reporting is needed
- **Streaming `nar_ls`** for NARs > 4 GiB — pulled into P0546 IF P0543's `max_nar_size` gate FAILs; otherwise followup
- **`BlobService.Read(file_digest)` RPC** (snix-compatible blob fetch keyed by `file_digest` instead of chunk-list) — P0573's `file_blobs` table has the coords; the RPC is ~30 LoC. Followup once a non-rio client (e.g., raw snix) wants to substitute from rio-store.
- **fs-verity on the digest cache** — would give kernel-side integrity for warm reads from `/var/rio/objects`, but requires a real fs (not FUSE) as the data-only lower. Possible future: digest-FUSE materializes into an ext4/xfs hostPath with fs-verity, overlay's lower2 is that dir directly (no FUSE on warm path at all). Followup after P0562.
- **ADR-024 implementation** (PG → DSQL/Spanner/CockroachDB; S3 CRR/MRAP; `GcsChunkBackend`) — separate plan range; this plan only writes the stub + ensures forward-compat
- **Per-AZ FSx HA** — `var.cache_tier_azs = local.azs` flips one FSx → three; `TieredChunkBackend` is AZ-count-agnostic so this is pure ops
- **buffa protobuf views** — unchanged from V1's analysis; revisit only if post-P0568 profiling shows decode hotspot

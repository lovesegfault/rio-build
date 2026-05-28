# ADR-022 Implementation Plan — castore-FUSE lazy store + per-AZ S3 Express chunk cache

**Status:** Phase-0 gate passed (P0541, P0544, P0569 done; P0543 does not block the gate and is still UNIMPL; P0578 PARTIAL — kernel-mechanism subtests Q7-Q12 done; the deferred mountd-protocol subtests landed as `vm-mountd` under P0567, perf criteria measured there but ungated). Phase 1/2 in progress: P0545, P0546, P0548, P0549, P0550, P0551, P0552, P0568, P0570, P0572, P0573, P0577, P0588, P0589 done. Castore RPC surface (`GetDirectory`/`Has*`/`ReadBlob`/`StatBlob`) is now complete. Phase 3 in progress: P0567 done (rio-mountd daemon + UDS wire protocol + `vm-mountd` VM test + helm DS + eks-node `/var/rio` prjquota bind; the DS ships `enabled: false` until something dials it); P0559 done (the castore-FUSE itself — tree/open/circuit/mountd-client + the `Filesystem` impl, unit-tested; nothing mounts it yet); P0571 done (mountd's disk-pressure LRU over `cache`/`chunks` + the dead-staging backstop); P0575 done (streaming open for files above the threshold — first-chunk reply gate, chunk-cache-first windowed fill, `PromoteChunks` cross-build dedup). **P0560 is DONE (2026-05-28)** — the builder mounts a per-build castore FUSE via rio-mountd, the old JIT FUSE is deleted, executor pods / the NixOS module / the helm chart are wired to the node-local castore (incl. the assignment-HMAC mount the chart was missing), `vm-castore-e2e` covers the production chain, and the whole VM matrix runs on the fixture-level explicit-tenancy stopgap recorded in the P0560 reconciliation note (Phase 8 / P0590–P0593 is the product-side resolution and the stopgap's deletion). **P0562 (post-cutover audit) is DONE (2026-05-28)** — the JIT-FUSE rule/string sweep is clean (the deliberate survivors are listed in the P0562 reconciliation note) and `builder.fs.parity` is wired on the lifecycle suite. **Next on the critical path: P0574 (gateway delta-sync client); Phase 8 is sequenced after the cutover merges.** P0586 (PutPathChunked) is DONE — the wire surface, the DAG codecs, the builder fused walk, HasChunks + the durable flip, `validate_begin`, the blob-stream storage conversion (one format, eager castore index, GetPath framing regeneration), and the RPC handler (the §6.3 sequential verify walk + the §6.2 single-transaction commit) are landed, and the builder client switch is landed too (the builder now walks, probes HasChunks, and streams only novel chunks; the legacy PutPath/PutPathBatch client path is deleted), and `vm-put-path-chunked` verifies the flow end-to-end (multi-output single stream, cross-output refs, chunk dedup via HasChunks, floating-CA). P0557 (eager nar_index compute) is DONE — it landed inside P0586's storage conversion as a mandatory part of the complete transaction (the index can no longer be recomputed after ingest), not as the gated spawn originally planned; see the P0557 reconciliation note. Design is [ADR-022 §2](./022-lazy-store-fs-erofs-vs-riofs.md) + [Design Overview](./022-design-overview.md) + ADR-023. Per-item status is in the metadata line under each `### P05xx` heading.
**Plan-number range:** P0541–P0593 (gaps at 0542/0547/0558/0561/0587 are abandoned numbers; P0556 abandoned 2026-04-23 — do not reuse).
**Clean-cutover constraint:** no FUSE fallback flag, no `RIO_STORE_BACKEND` selector. P0560 deletes the old FUSE module wholesale.
**Cross-region forward-compat:** object store (S3/GCS) is authoritative for bytes; S3 Express One Zone is a per-AZ read-through cache; PG is single-region. Nothing here precludes cross-region deployment (object-store-authoritative, cache tier stateless) but it is not implemented. No DRA. **Express AZ-ID availability constrains region/AZ choice** — see [Design Overview §9](./022-design-overview.md).
**Migration-number range:** ADR-022 has shipped `062_nar_index`, `063_file_blob_size`, `064_directory_paths`, and `065_drop_nar_indexed` (062-064 were originally numbered 061-063; renumbered when the rebase onto `main` picked up `061_drv_logs.sql`. The `054_*` slot this plan originally reserved was consumed by unrelated work). Next free: `066_*`. The prose below says "migration 061" wherever it predates the renumber — read 061→062, 062→063, 063→064 for the ADR-022 migrations.

---

## How we got here

The pre-ADR-022 builder serves `/nix/store` via FUSE with whole-store-path JIT fetch — touching one file in a 1 GB output fetches 1 GB, a partially-hot 200 MB `.so` either upcalls every read or blocks `open()` for the whole file, and identical files across store paths are fetched + cached independently. Three replacements were evaluated and set aside ([ADR-022 §3](./022-lazy-store-fs-erofs-vs-riofs.md#3-alternatives-considered)): EROFS+fscache (cachefiles daemon, device-slot table), a custom `riofs` kmod (~800 LoC novel kernel C), and a composefs-style EROFS-metadata + data-only-lower stack (spiked at chromium scale; discarded — adds an encoder, an artifact type, a kernel ≥6.16 gate for a metadata-upcall win not shown to matter in build wall-clock). The accepted design is a **snix-style castore-FUSE**: serve the closure's Directory DAG (P0572) via FUSE with content-addressed inodes, infinite cache TTLs, and `FUSE_PASSTHROUGH` on `open()` — file-granular fetch, per-file + per-subtree dedup, warm-read zero crossings, no encoder, no kernel ≥6.16 gate. snix-store is the production validation. Two spikes carry over: the unprivileged-userns mount works via a small privileged helper (`rio-mountd`), and `FOPEN_KEEP_CACHE` handles giant partially-read files without a mode transition. The spike evidence below is the validation record; everything else in this document is forward-looking sequencing.

<details><summary>Superseded plan versions</summary>

PLAN-GRAND-REFACTOR V1 (Path A / EROFS+fscache), V2/V3 (Path C, mkcomposefs subprocess), V4 (Path C, libcomposefs FFI — this document pre-2026-04-23) archived at `~/tmp/stress-test/`. P0540/P0542/P0547/P0558 were Path-A artifacts and are abandoned numbers; P0556 was the V4 EROFS encoder, now abandoned.

</details>

---

## Spike evidence

Core-stack nixosTests consolidated on `adr-022` (commit `15a9db79`); chromium-146 closure topology (357 store paths, 23 218 regular files, 8 221 dirs, 3 374 symlinks) with synthetic content. The number that carries over to §2: **warm `read` upcalls = 0** under passthrough. The metadata-side zero-upcall numbers (mount <10 ms, `find` over 23 218 files in 60 ms with 0 upcalls, 5.3 MiB image encoded in 70 ms) measured the **rejected §3 EROFS alternative** — §2 pays one upcall per cold dirent and is dcache-absorbed thereafter (snix's exact configuration); the §3 numbers live in ADR-022 §3's deferred-alternative paragraph.

**Privilege-boundary evidence** (P0541, commit `af8db499` on `adr-022`, kernel 6.18.20) — all PASS:

| Subtest | Result |
|---|---|
| `userns-overlay` | PASS — unpriv builder mounts overlay itself with `-o userxattr,lowerdir=<lower>`. Carries over to §2 (single FUSE lower, no `::`). |
| `userns-fuse-self` | PASS — builder cannot `open("/dev/fuse")` without `privileged:true` or device-plugin, but with fd-handoff it never needs to: `rio-mountd` opens+mounts in init-ns, passes the connected fd. **Builder pod drops `smarter-devices/fuse:1` entirely.** |
| `kvm-hostpath-spike` (`9492019c` on `adr-022`) | PASS — Nix sandbox sees `/dev/kvm` via `extra-sandbox-paths` + char-device hostPath; `requiredSystemFeatures=["kvm"]` build does `ioctl(KVM_GET_API_VERSION)` → 12. **smarter-device-manager dropped entirely** — `/dev/kvm` is a capability flag (node label + hostPath), not a counted resource. |
| `erofs-loop-unpriv` / `fsmount-handoff-erofs` | **§3 alternative only** (EPERM as expected / PASS) — §2 has no EROFS mount. |
| `fuse-dev-fd-handoff` | PASS — `/dev/fuse` fd via SCM_RIGHTS works; `fuser` accepts pre-opened fd. |
| `teardown-under-load` | PASS — reader wakes `ENOTCONN` <1s, no D-state. |

**Passthrough validation** is **P0578** (separate spike — P0541 is DONE and must stay DONE for the dag-runner).

**Gotcha (ordering):** `/dev/fuse` fd MUST be received and the castore-FUSE serving **before** the overlay mount — overlayfs probes lowers at `mount(2)`; an unserved FUSE deadlocks the mounter.

**§2.8 large-file evidence** (P0575 promoted to critical-path on this basis):

| Commit / source | Finding |
|---|---|
| nix-index `top1000.csv` (external dataset, 2026-04-05) | nixpkgs top-1000 files: **all >64 MiB** (min 117 MiB, median 179 MiB, 267 >256 MiB, 7 >1 GiB). 248 are `.so`/`.a`. Floor — proprietary closures worse. |
| `42aa81b2` (`adr-022`, `nix/tests/lib/spike-access-data/RESULTS.md`) | Real consumers read **0.3-33%** of giants: link-time `libLLVM.so` 2.79% bimodal head+tail; `opt --version` 32.77% scattered/266 ranges; `libicudata` preload 0.28%. No `MAP_POPULATE`/`fadvise`. |
| `15a9db79` (`adr-022`, `composefs-spike-stream.nix`) | Streaming-open mechanism PASS: 256 MiB `open()` = **10.3 ms** (vs 2560 ms whole-file); `FOPEN_KEEP_CACHE` from start → 2nd `dd` **0 read upcalls**; `mmap` page-faults route through FUSE `read`; **no mode-transition needed** (KEEP_CACHE doesn't suppress cold upcalls, only prevents invalidation). |
| alternatives survey | Allowlist prefetch **violates JIT-fetch imperative**. FSx-backed cluster-wide objects cache **rejected** — violates builder air-gap. |

fs-verity doesn't verify when the lower is FUSE (ioctl-forwarding only, no `i_verity_info`) — per-file integrity lives in the FUSE `open()` handler (§2.7). EROFS-encoder findings (stub `i_size`, metacopy-xattr shape, `user.*` prefix) are §3-only; recorded in the deferred-alternative paragraph of ADR-022 §3.

---

## Prerequisites (in flight separately — NOT phased here)

| Track | Status | Owns |
|---|---|---|
| **NixOS node cutover** (full Bottlerocket replacement) | dispatched (`nixos-cutover` agent) | `nix/nixos-node/{hardening,kernel}.nix`, `karpenter.yaml` amiSelectorTerms→tag, `xtask ami push`, ADR-021 |
| `kernel.nix` standalone module: kernel **≥6.9** (`FUSE_PASSTHROUGH` [`7dc4e97a4f9a`](https://git.kernel.org/linus/7dc4e97a4f9a)) + `boot.kernelModules = ["fuse" "overlay"]` | **DONE** | `nix/nixos-node/kernel.nix` — **MUST be importable by `nix/tests/fixtures/`** (no `pins`/`specialArgs` deps) so VM tests reuse the AMI's exact config. **No `kernelPatches`** — stock nixpkgs has `FUSE_FS=m` `OVERLAY_FS=m` (`autoModules`), `FUSE_PASSTHROUGH=y` (Kconfig default); the §3 EROFS+cachefiles `_ONDEMAND` symbols were the only off-by-default config and went with §3, so the kernel is a binary-cache hit. `=m` + `boot.kernelModules` is functionally equivalent to `=y` (modules load in `basic.target` before any pod). Module asserts version at eval. |
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
| **U4** | operator | wants to know if the new path is better | no per-file metrics | grafana: `rio_builder_castore_fuse_open_seconds` p99, `…_fetch_bytes_total{hit=node_ssd|remote}`, `rio_store_tiered_local_hit_ratio` |
| **U5** | deployment consumer | `nix copy --from rio-store` to a **rio-aware** receiver (rio-store replica, or host running rio-gateway proxy) that already has 95% of the target closure | walks chunk-list per store path; O(all-chunks) `HasChunk` RPCs even for unchanged paths | walks Directory DAG; `HasDirectories([root_digest,…])` short-circuits unchanged subtrees in one batch RPC; fetches only changed files. **Sync bandwidth ∝ change size, not closure size.** Stock-nix clients without `HasDirectories` fall through to P0566's narinfo/NAR binary-cache surface. |
| **U6** | operator / migrating org | PostgreSQL is unavailable, or rio-store is not yet deployed in a consumer environment | nothing substitutes — chunks in S3 are unreadable without the PG manifest index | with `binary_cache_compat` enabled, `nix copy --from s3://bucket?region=…` works directly against S3-standard with no rio process running. PG-loss is a degradation (no dedup serving, no CA-cutoff, no `FindMissingPaths`), not an outage. Migration on-ramp: existing Nix infra reads the bucket as a plain binary cache while rio rolls out. |

**Sequencing rule (U3, unchanged):** every phase boundary is `/nixbuild --checks` green. Phases 0-4 are deploy-safe (store-side or test-only). Phase 5 (P0560) is the hard cutover: builders REQUIRE the castore-FUSE lower from that commit forward.

---

## DAG overview

```text
P0576 EXT: nixos-cutover sentinel (kernel.nix ≥6.9 importable, /dev/fuse, AMI) ────────────────┐
                                                                                               │
┌── Phase 0 (gate + scaffold; ≤4-way parallel) ──┐                                             │
P0569 spike:composefs   P0541 spike:mount-priv   P0578 spike:passthrough    P0543 measure        P0544 spec-scaffold
(DONE — §3 evidence)    (userns overlay; fuse-   V11/V12 + closure wc       ADR-023 (tiered, per-AZ)
                         dev fd-handoff;         + aarch64 kernel           + ADR-022 §2 r[...] markers
                         teardown)
   │                       │                          │                     │
   └────────────────── Phase-0 gate: all PASS ────────┴─────────────────────┘
                                                                                               │
┌── Phase 1 (primitives; ≤8-way parallel) ──┐  all dep on P0544                                │
P0545 proto    P0546 nar_ls    P0572 dir merkle  P0570 StatBlob         P0548 Tiered    P0549 blob-API  P0550 fetch.rs hoist
(NarIndex      (rio-nix;       (dir_digest/      (file_digest →         (S3 Express →   (string-keyed,  (StoreClients →
 +file_digest   +blake3)        root_digest;      ChunkMeta[];           S3 fallback)    narinfo/ ns)    store_fetch.rs)
 +dir_digest)                   directories tbl   server-side)                                            │
                                — LOAD-BEARING:                                                   ▼
                                §2.2 mount source)                            P0568 GetChunks server-stream
                                                                              (K_server=256; prost .bytes();
                                                                               tonic adaptive_window; obs)
   │              │               │                   │                        │                │
   ▼              │               │                   │                        │                │
P0551 migration 062 ◄─────────────┼───────────────────┼─────(blob ns)──────────┘                │
   │              │               │                   │                                         │
   ▼              ▼               ▼                   │                                         │
P0552 GetNarIndex + indexer_loop  P0573 GetDirectory  │                                         │
   │                              (recursive=true     ▼                                         │
   │                               server-BFS stream) ┌── Phase 3 cache-tier infra ────────┐    │
   │                              │                   P0553 s3-express.tf (per-AZ) + IAM        │
   │                              │                      └─► P0554 helm ──► P0555 vm:tiered
   │                              │                             ★ FIRST SHIPPED VALUE (U2)
   │                              │                             P0579..P0582 compat layer  ★ U6 LANDS
   │                              │                             P0583 drop inline_blob
   │                              │                             P0584 builder-chunked-only gate
   │                              │                             P0585 Express eviction sweeper
   │                              │
┌── Phase 4 store-side index (gated on Phase-0 + P0546) ──┐
P0557 PutPath eager nar_index (try_acquire-gated; NAR in RAM → nar_ls+blake3) ◄─(P0551, P0552, P0572, P0586) ⛔BLOCKED on P0586 — path_tenants race
P0556 [ABANDONED — §3 EROFS encoder; §2 has no image]
   │
┌── Phase 5 castore-FUSE builder-side ──┐                                                       │
P0567 rio-mountd DaemonSet (fd-handoff + BACKING_OPEN broker + Promote + cache owner) ◄─────────┤(P0576, P0578)
   │                                                                                            │
P0588 WorkAssignment.input_roots (proto fields 13/14 + dispatch closure walk) ◄─(P0572)  DONE
   │                                                                                            │
P0589 AssignmentClaims.{role,input_closure_digest} + dispatch ◄─(P0544, P0588)  DONE
   │   (sequenced before P0573/P0586/P0560/P0584 — all read these claim fields)
   │                                                                                            │
P0559 castore_fuse/{tree,open,circuit}.rs ◄─(P0550, P0567, P0568, P0570, P0572, P0573, P0577, P0588)
   │
P0571 mountd-owned cache LRU + per-build staging ◄─(P0559, P0567)
   │
P0575 streaming open() for files > STREAM_THRESHOLD ◄─(P0559, P0570, P0571)
   │
P0560 [ATOMIC] §A mount.rs+overlay+DELETE old-FUSE  §B fixture kernel + vm:castore-e2e + FUSE-assert sweep
   │
P0562 audit: tracey builder.fuse.* empty + r[verify builder.fs.parity]  ★ CUTOVER GATE (U1)
   │
┌── Phase 6 obs + finalize ──┐
P0563 metrics+dashboard+alerts   P0564 helm: wire mountd DS + kernel assertion   P0565 runbooks

┌── Phase 7 delta-sync + chunked upload (U5; serialised after P0573) ──┐
P0577 BlobService.Read(file_digest) server-stream (snix-compatible blob fetch)
   │
P0586 PutPathChunked: builder-side fused walk + HasChunks + pipelined sync narhash verify (closes TODO P0433/P0434)  DONE
   │
P0574 gateway substituter: Directory-DAG delta-sync client  ★ U5 LANDS
```

**Hidden dependencies surfaced:**

| Edge | Why it's non-obvious |
|---|---|
| P0549 blob-API → P0566 | `ChunkBackend` trait today is `[u8;32]`-addressed only (`rio-store/src/backend.rs`; P0548 splits this into `backend/{mod,tiered}.rs`). `{h}.narinfo` / `nar/{h}.nar.zst` / `nix-cache-info` need string-keyed `put_blob/get_blob/delete_blob`. |
| P0576 (kernel.nix sentinel) → P0560 | Test-VM kernel must be the same shape as the AMI (`boot.kernelModules`, version assertion). `kernel.nix` MUST be a standalone NixOS module importable by `nix/tests/fixtures/` — no `pins`/`specialArgs` deps. With stock kernel (no `kernelPatches`) "same shape" reduces to "same `linuxPackages` minor", which `pins.node_kernel_minor` already controls; the residual is the module assertion + module-load list. |
| P0550 fetch.rs hoist → P0559 | `rio-builder/src/fuse/fetch/mod.rs` import `fuser::Errno`, `super::NixStoreFs`, `super::cache`. **NOT a pure `git mv`** — hoist `StoreClients` + `fetch_chunks_parallel` core to `store_fetch.rs`; leave FUSE-typed wrappers in `fuse/fetch.rs` *temporarily* (P0560 deletes them with the rest of `fuse/`). ~150 LoC of actual refactor, not zero. |
| P0544 spec-scaffold → everything with `r[impl …]` | `tracey-validate` in the checks gate fails on dangling `r[impl X]` where `r[X]` has no spec text. Markers must be on `sprint-1` before any code phase merges. |
| P0548 → P0553 | Terraform may land first, but the helm flip to `kind: tiered` MUST NOT — `TieredChunkBackend` semantics (S3-sync put, FS write-through on get) are what make the cache tier safe to enable. |
| P0541 → P0567 minimal | Builder can't open `/dev/fuse` unprivileged and `BACKING_OPEN` needs init-ns `CAP_SYS_ADMIN`. `rio-mountd` opens `/dev/fuse`, mounts the FUSE at `/var/rio/castore/{build_id}`, SCM_RIGHTS the fd. Builder serves castore-FUSE on it, then mounts overlay itself in its userns (`userxattr,lowerdir=<castore_mnt>`). |
| P0572+P0573 → P0559 | The castore-FUSE serves the Directory DAG; it cannot mount without `GetDirectory(recursive=true)` returning the tree. P0572/P0573 move from "U5 optionality" to a hard P0559 prerequisite. |
| P0573 file_blobs → P0570/P0577 | `StatBlob`/`ReadBlob` resolve `file_digest → chunk-range` server-side via `file_blobs`; that table must exist (P0572's migration) before either RPC lands. |
| P0546 ↔ P0572 | `dir_digest` is computed bottom-up over `file_digest` of children — same pass, same RAM. P0572 extends P0546's `nar_ls` rather than re-walking. |
| P0573 batch RPCs ← I-110 lesson | per-digest unary `HasDirectory` against a 50k-node DAG is the I-110 PG-wall again. `HasDirectories([digest]) → bitmap` and `HasBlobs([file_digest]) → bitmap` are batch from day one. |
| P0571 → P0560 | Node-SSD cache is the castore-FUSE's backing dir; mount sequence in P0560 references `/var/rio/castore`. If P0571 slips, P0560 uses `tmpfs` (loses cross-build amortization but functions). |
| P0575 → P0560 | streaming-open is part of `castore_fuse/open.rs`; P0560's `vm-castore-e2e` streaming-large-input subtest exercises it. P0575 must land before §B's streaming assertions are meaningful. |

---

## Phase 0 — Spike gate + scaffold (de-risk before committing)

Spikes are throwaway on `spike/*` branches; results captured in `.stress-test/sessions/2026-04-NN-phase0-gate.md`. P0543/P0544 ship to sprint-1.

### P0569 — SPIKE sentinel: composefs-style validated (§3 alternative)
**Crate:** `spike` · **Deps:** none · **Complexity:** — · **Status: DONE 2026-04-05**

Dependency-tracking row only. Consolidated as `15a9db79` on `adr-022`. Validated the §3 EROFS alternative (now discarded). The streaming-open and privilege-boundary findings (`composefs-spike-stream.nix`, `-priv.nix`) carry over to §2; the metadata-zero-upcall findings do not.

### P0541 — SPIKE: composefs privilege boundary + mount handoff
**Status: DONE — all six subtests PASS** (commit `af8db499` on `adr-022`, kernel 6.18.20). Results table in §Spike evidence above. Confirms overlay mount stays in the unprivileged builder via `userxattr`.
**Files:** `nix/tests/scenarios/composefs-spike-priv.nix` — VM imports `nixos-node/kernel.nix`; runs as unpriv-userns user.

### P0578 — SPIKE: passthrough-under-overlay + brokered `BACKING_OPEN`
**Crate:** `spike, nix` · **Deps:** P0541 · **Complexity:** LOW · **Status:** PARTIAL 2026-05-19 (kernel mechanisms i–iv, viii, ix; mountd protocol v, xi–xv landed as `vm-mountd`; perf criteria vi/vii/x measured there but ungated — need one KVM-backed run to confirm)

Extends `composefs-spike-priv.nix` with a `passthrough-under-overlay` subtest. Asserts: (i) overlay mount succeeds with FUSE lower at `max_stack_depth=1` (depth 2 = `FILESYSTEM_MAX_STACK_DEPTH`); (ii) unprivileged `ioctl(FUSE_DEV_IOC_BACKING_OPEN)` → `EPERM`; (iii) root-process ioctl on a `dup()` of the same `/dev/fuse` fd succeeds and `FOPEN_PASSTHROUGH` open under overlay reads correctly from ext4 backing; (iv) reads continue after `kill -9` of the FUSE server; (v) brokered `Promote` with mismatched blake3 → mountd rejects, cache file absent; (vi) **`BackingOpen` RTT**: 10k iter, p99 < 200 µs; (vii) **`Promote` throughput**: 256 MiB ×3, ≥ 1.0 GiB/s; (viii) **copy-up**: overlay with `upperdir`+`userxattr,lowerdir=<castore_mnt>` (single FUSE lower, no `::`); `chmod`/`echo >>` a passthrough-backed input → upper has full file bytes (overlay copy-up reads through `FOPEN_PASSTHROUGH`); (ix) **cache-readonly**: unpriv `open(cache/ab/X, O_WRONLY)` → `EACCES`; (x) **concurrency**: fire 1 GiB `Promote`, concurrently 100 `BackingOpen`, assert p99 < 1 ms; (xi) **Promote hardening**: `staging/<hex>` is symlink → `Err(NotRegular)`; FIFO → `Err(NotRegular)`; (xii) **one-mount**: send second `Mount{b}` on same conn → `Err(AlreadyMounted)`, no second fuse mount; (xiii) **promote-bounded-copy**: spawn appender writing 1 GiB to `staging/<hex>` while `Promote{digest}` runs → `Err(DigestMismatch)` after exactly initial `st_size` bytes in `.promoting` (assert via `du` before unlink); (xiv) **staging-quota**: builder `dd` past `staging_quota_bytes` into staging dir → `ENOSPC`; (xv) **build-id-unique**: conn-A (uid 1000) `Mount{"shared"}` → ok; conn-B (uid 2000) `Mount{"shared"}` → `Err(DuplicateBuildId)`, no second mount, conn-A's staging dir untouched.

Each as an independent `subtests=[...]` entry (failures isolate). `# r[verify builder.fs.passthrough-stack-depth]` `# r[verify builder.mountd.{backing-broker,promote-verified,concurrency,one-mount,build-id-unique,promote-bounded-copy,staging-quota}]` at the entries. **Exit:** `nix build .#checks.x86_64-linux.vm-composefs-spike-priv` green.

> **Reconciliation (2026-05-19).** Split into two halves. **Kernel mechanisms (i–iv, viii, ix)** landed as `composefs-spike-priv.nix` Q7–Q12 + `spike_passthrough_fuse.rs`: passthrough under overlay at depth 2, `BACKING_OPEN` privilege boundary on a dup'd `/dev/fuse` fd, reads-survive-server-kill, copy-up, cache-readonly, no-read-upcall. These gate the §2 castore-FUSE design. **Mountd protocol (v–vii, x–xv)** — Promote integrity, RTT/throughput perf gates, one-mount, build-id-unique, promote-bounded-copy, staging-quota — need a UDS protocol prototype that is most of `bin/rio-mountd.rs`; deferring them to P0567's test suite avoids duplicating ~300 LoC of throwaway mountd that the production daemon would re-implement and re-test the next phase over. The corresponding `r[verify builder.mountd.{promote-verified,concurrency,one-mount,build-id-unique,promote-bounded-copy,staging-quota}]` stay at P0567. **Spike findings** (folded into `docs/spec/components/builder.typ` fuser-API note + ADR-022 §2.10 spike-evidence table — all bind on P0559's `castore_fuse` open path): (a) `KernelConfig::set_max_stack_depth(1)` alone does *not* enable passthrough — `add_capabilities(InitFlags::FUSE_PASSTHROUGH)` is also required, otherwise `BACKING_OPEN` is unconditionally `EPERM` because `fc->passthrough` never gets set; (b) the kernel's `FOPEN_PASSTHROUGH_MASK` rejects any `open()` reply combining `FOPEN_PASSTHROUGH` with `FOPEN_KEEP_CACHE` (or any other flag outside `{PASSTHROUGH, DIRECT_IO, PARALLEL_DIRECT_WRITES, NOFLUSH}`) with user-visible `EIO` — the §2.6 case-1/case-3 flag sets are mutually exclusive replies, never a union; (c) one `BackingId` per `file_digest`, refcounted across opens — a second concurrent passthrough open whose `fuse_backing` differs from the inode's recorded `fi->fb` is `-EBUSY` → `EIO`, and overlay copy-up issues several `dentry_open()`s of the lower in one syscall (`ovl_security_fileattr` + `vfs_fileattr_get` + `ovl_copy_up_data`) where the first's deferred `fput()` keeps `fi->fb` set across the others.

### P0543 — V11/V12 measurement + closure-size + aarch64 kernel sanity
**Crate:** `xtask` · **Deps:** none · **Complexity:** LOW
| File | Change |
|---|---|
| `xtask/src/k8s/measure.rs` | new — `xtask measure v11` (intra-closure chunk-reuse %), **`xtask measure v12` (tune `STREAM_THRESHOLD` — ingest nix-index `top1000.csv` + `nix/tests/lib/spike-access-data/RESULTS.md` (`42aa81b2`); compute the size at which whole-file fetch latency exceeds p50 first-range-touched latency)**, `xtask measure closure-paths` (`nix path-info -r nixpkgs#chromium \| wc -l` for both arches) |
| `.stress-test/metrics/v11-v12.json` | output |
| `nix/misc-checks.nix` | `node-kernel-config-{x86_64,aarch64}`: read `config.boot.kernelPackages.kernel.configfile` from the AMI module (and the `pkgsCross.aarch64-multiplatform` eval); assert `CONFIG_FUSE_PASSTHROUGH=y`, `CONFIG_FUSE_FS=[ym]`, `CONFIG_OVERLAY_FS=[ym]`. Catches a future nixpkgs upstream regression without forcing a kernel rebuild. Build-eval only. |

**Exit:**
- `v12_stream_threshold_bytes`. **Tuning, not a gate.** P0575 ships unconditionally (top1000.csv + access-probe `42aa81b2` already prove the 64 MiB question). V12 picks the `STREAM_THRESHOLD` config default (initial: 8 MiB ≈ 60-120 ms whole-file at 1 Gbps).
- `node-kernel-config-aarch64` builds. FAIL → fix `kernel.nix` for aarch64 before P0576 flips DONE.
- ~~`closure_paths_* < 65535`~~, ~~`max_nar_size_* < 4 GiB`~~ — **gates removed** (no device table; `nar_ls` is streaming unconditionally per P0546). Measurements kept as informational.

### P0544 — Spec scaffold (all `r[…]` markers + ADR-023)
**Crate:** `docs` · **Deps:** none · **Complexity:** LOW · **Status: DONE 2026-05-15** (`c85557a1`)
| File | Change |
|---|---|
| `docs/src/decisions/022-lazy-store-fs-erofs-vs-riofs.md` | merge `adr-022` (refocused §2 Design / §3 Alternatives). Carries the §2 + §6 markers: `r[builder.fs.{castore-stack, castore-dag-source, castore-inode-digest, castore-cache-config, fd-handoff-ordering, digest-fuse-open, passthrough-on-hit, passthrough-stack-depth, shared-backing-cache, file-digest-integrity, node-chunk-cache, streaming-open-threshold}]` + `r[builder.mountd.{promote-verified, orphan-scan}]` + the §6 chunked-upload markers (full list in tracey inventory below). |
| `docs/src/decisions/022-design-overview.md` | merge `adr-022`. Canonical design reference. Carries `r[builder.overlay.castore-lower]`, `r[builder.fs.parity]`, `r[builder.result.input-eio-is-infra]`, `r[builder.mountd.{fuse-handoff,backing-broker,concurrency}]`, `r[obs.metric.{castore-fuse,mountd}]`. |
| `docs/src/decisions/023-tiered-chunk-backend.md` | new — object store (S3 today; GCS-ready via `ObjectStoreBackend` trait) is authoritative for bytes; **one S3 Express One Zone directory bucket per AZ** is a disposable read-through cache. Both tiers are `S3ChunkBackend` instances; `put` = remote only (S3-standard); `get` = local → remote fallback + write-through; Express fills via read-through only. PG `chunk_refs` is single-writer arbiter (single-region). **No DRA.** Forward-compat for cross-region: cache tier is stateless and metadata-agnostic; object-store cross-region replication + a globally-consistent metadata store would suffice, but neither is in scope here. Explicitly states: any single cache-tier-AZ outage = that AZ's replicas cold-read from S3 standard, not service outage; rollback `kind=s3` is instant + lossless. Records FSx-for-Lustre as the considered alternative. Carries `r[infra.express.cache-tier]`. |
| `docs/src/components/store.md` | append §"NAR index" (incl. `file_digest`) + §"Tiered chunk backend" + §"BlobService" + §"Binary-cache compatibility layer" (`r[store.compat.*]`) |
| `docs/src/components/builder.md` | **rewrite** §"FUSE Store" → §"castore-FUSE lower" + §"open() handler" + §"rio-mountd" (delete pre-ADR-022 whole-path FUSE description) |
| `docs/src/components/gateway.md` | append `r[gw.substitute.dag-delta-sync]` spec text |
| `docs/src/security.md` | bump `r[sec.pod.fuse-device-plugin]` (`/dev/fuse` now via mountd fd-handoff, not base_runtime_spec); bump `r[common.hmac.claims]` (add `tenant`+`role`+`input_closure_digest`); add §Boundary-4 `r[sec.boundary.mountd]` (mountd threat surface: build_id traversal, disk-fill, cross-build interference, fd smuggling — and mitigations); rewrite §Known-Limitations #2/#3 (executors no longer hold `CAP_SYS_ADMIN`); update §Read-authorization (castore surface IS tenant-scoped); add `HasChunks` to §Cross-Tenant Chunk Probing |
| `docs/src/multi-tenancy.md` | append `directory_tenants` / `file_blob_tenants` rows to the tenant-scoping table |
| `docs/src/deployment.md` | append `r[infra.node.kernel-fuse-passthrough]` spec text |
| `docs/src/observability.md` | append metric rows |
| `.config/tracey/config.styx` | spec `include` += `decisions/023-tiered-chunk-backend.md`, `deployment.md` (so `infra.express.cache-tier` and `infra.node.kernel-fuse-passthrough` are scannable) |

**Exit:** `tracey query validate` 0 errors; `/nixbuild --checks` green.

**Phase-0 gate (go/no-go):** P0569 DONE; P0541 subtests route P0567/P0559/P0560 design (do NOT block the gate). Record in `.stress-test/sessions/`. Phases 1–3 are design-agnostic and proceed regardless.

---

## Phase 1 — Primitives (≤8-way parallel; all dep on P0544)

### P0545 — proto: NarIndex with `file_digest`
**Crate:** `rio-proto` · **Deps:** P0544 · **Complexity:** LOW · **Status: DONE 2026-05-15** (`13dd833a`)
| File | Change |
|---|---|
| `rio-proto/proto/types.proto` | `message NarIndexEntry { bytes path=1; Kind kind=2; uint64 size=3; bool executable=4; uint64 nar_offset=5; bytes target=6; bytes file_digest=7; }` — `path`/`target` are `bytes` not `string` (NAR names are arbitrary non-NUL/non-slash bytes; non-UTF8 is legal). `file_digest` is blake3 of regular-file content (32 bytes; empty for dirs/symlinks). `message NarIndex { repeated NarIndexEntry entries=1; }` |
| `rio-proto/proto/store.proto` | `rpc GetNarIndex(...)`; `rpc GetNarIndexBatch(NarHashList) returns (stream NarIndexResponse)` (build-start fetches ~357 indices; batch avoids per-path RTT) |
| `xtask regen mocks` | run |

**Exit:** `/nixbuild --checks` green.

### P0546 — rio-nix: streaming `nar_ls` + blake3-per-file
**Crate:** `rio-nix` · **Deps:** P0544, P0545 · **Complexity:** MED · **Status: DONE 2026-05-15** (`c2ac7c5b`)
| File | Change |
|---|---|
| `rio-nix/src/nar/` | `pub fn nar_ls<R: Read>(r) -> Result<Vec<NarLsEntry>>` — sibling to `parse()`; **single forward pass, no `Seek`, bounded memory regardless of NAR size.** Maintains a running byte counter for `nar_offset`; for `Regular`, records the offset after the `"contents"` length-prefix, then streams the `size` bytes through `blake3::Hasher` in 64 KiB blocks (bytes touched once, never buffered whole). `NarLsEntry { …, file_digest: [u8;32] }`. `// r[impl store.index.nar-ls-offset]` `// r[impl store.index.file-digest]` `// r[impl store.index.nar-ls-streaming]` |
| `rio-nix/fuzz/fuzz_targets/nar_ls.rs` + `Cargo.toml` + `nix/fuzz.nix` | new — includes a >4 GiB synthetic NAR via `io::repeat()` slices to assert no buffering |
| tests | proptest: `serialize(tree)` → `nar_ls` → `&nar[off..off+size] == content` AND `file_digest == blake3(content)`; explicit test with reader wrapper that panics on `seek()`. `// r[verify ...]` |

The `Read+Seek` variant is not implemented — callers that have a `Vec<u8>` wrap it in `Cursor` and the streaming impl is no slower.

**Exit:** `/nixbuild --checks` green incl. `fuzz-nar_ls`.

### P0548 — TieredChunkBackend (object-store authoritative; S3 Express read-through cache)
**Crate:** `rio-store` · **Deps:** P0544 · **Complexity:** LOW · **Status: DONE 2026-05-15** (`80b35f9f`)
`rio-store/src/backend/tiered.rs`: `TieredChunkBackend { local: Option<S3ChunkBackend>, remote: S3ChunkBackend }`. `put` = **remote only** (Express filled solely via `get`'s read-through); `get` = local → remote fallback + write-through; `local=None` degrades to pass-through. Both tiers are the existing `S3ChunkBackend` — **no `backend/fs.rs`**, no new put-idempotence (S3 PutObject already is). `// r[impl store.backend.{tiered-get-fallback,tiered-put-remote-first}]`. **Exit:** `/nixbuild --checks` green. Implementation note: the local (Express) tier gets a separate S3 client with `EXPRESS_MAX_ATTEMPTS=2` (not the default adaptive 10 attempts) so a throttling Express bucket fails over to the authoritative tier fast instead of burning the latency budget on retries that would lose to a direct S3-standard read.

### P0549 — ChunkBackend blob-API
**Crate:** `rio-store` · **Deps:** P0544, P0548 · **Complexity:** LOW · **Status: DONE 2026-05-15** (`7cde57fa`)
Extend `ChunkBackend` with string-keyed `put_blob/get_blob/delete_blob` for P0566's `narinfo/`/`manifests/` sidecars (the `[u8;32]`-addressed chunk API can't express named objects). `validate_blob_key()` rejects `..` segments, leading `/`, and the reserved `chunks/` prefix so a sidecar key can never alias a chunk object. `TieredChunkBackend` forwards blob ops to the remote tier only — sidecars are tiny and read-once-per-deploy; caching them in Express would burn directory-bucket quota for no read-amortization win. **Exit:** `/nixbuild --checks` green.

### P0568 — Batched `GetChunks` server-stream + prost-bytes + tonic residuals + obs
**Crate:** `rio-proto, rio-store, rio-builder` · **Deps:** P0545, P0550 · **Complexity:** MED · **Status: DONE 2026-05-15** (`16cefb46`)
`rpc GetChunks(stream GetChunksRequest) returns (stream ChunkData)` — bidi-stream so the §7 fill task can pipeline local-cache misses to the server as it walks the chunk list instead of front-loading a 4000-`stat()` scan. Server fans out `K_server=256` concurrent `cas::get_verified()` per stream (`buffer_unordered`; `ChunkData` carries `digest` so out-of-order delivery is fine), bounding per-stream peak memory at `K × CHUNK_MAX`. `prost(bytes = "bytes")` on `ChunkData.data` for a zero-copy moka-`Bytes` → wire encode. **`tonic residuals`** = audit the new server-stream against `r[proto.h2.adaptive-window+2]` — that marker requires a **fixed** ≥1 MiB initial window and forbids `http2_adaptive_window` (hyper's adaptive mode resets the explicit window to 65 535 bytes; an earlier plan revision said "tonic `adaptive_window`" and the spec was bumped to `+2` after the spike showed the reset). Per-chunk error policy: any NotFound/Corrupt/backend-error aborts the whole stream (`Status::{not_found,data_loss,unavailable}`); the client knows which digests landed and retries only the gap (chunks are content-addressed). The castore-FUSE `open` handler (P0559) is the consumer. **Exit:** `/nixbuild --checks` green; live A/B ≥4× cold-fetch reduction (P0559's exit measures it; P0568 exit is functional only). Spec marker `r[proto.chunk.batch-bidi]` added in `docs/src/components/proto.md`.

### P0550 — fetch.rs core hoist (NOT a pure mv)
**Crate:** `rio-builder` · **Deps:** P0544 · **Complexity:** MED · **Status: DONE 2026-05-15** (`796b3e11`)
Hoist `StoreClients` + the FUSE-independent fetch primitives (`JIT_MIN_THROUGHPUT_BPS`, `jit_fetch_timeout`, `RETRY_BACKOFF`, `jitter`) from `rio-builder/src/fuse/fetch/` (which imports `fuser::Errno`, `super::NixStoreFs`) to `rio-builder/src/store_fetch.rs`; leave old-FUSE-typed wrappers (`fetch_extract_insert`, `prefetch_path_blocking`, `stream_nar_to_spool`, `SyncSpool`) in `fuse/fetch/mod.rs` until P0560 deletes them. `runtime/{mod,prefetch,setup}.rs` callers switch to `crate::store_fetch::StoreClients` so they survive P0560; `fuse/fetch/mod.rs` re-exports for the surviving fuse-internal callers. `fetch_chunks_parallel` (the plan's original target) does not exist as a named fn yet — the parallel chunk fetch is the per-path `GetPath` server-stream; the batched cross-path variant is what P0568's `GetChunks` client adds on top of the hoisted `StoreClients`. **Exit:** `/nixbuild --checks` green; existing FUSE VM tests unchanged.

### P0572 — Directory merkle layer: `dir_digest`/`root_digest` + `directories` table
**Crate:** `rio-proto, rio-store` · **Deps:** P0545, P0546, P0551, P0552 · **Complexity:** LOW · **Status: DONE 2026-05-15** (`75ad6288`)

> Implementation note: the bottom-up `dir_digest` pass landed in `rio-store/src/castore.rs` rather than `rio-nix/src/nar/` — the canonical encoding is a prost encode of `rio_proto::castore::Directory`, and `rio-nix` cannot depend on `rio-proto` (the dependency runs the other way). `nar_ls` still emits the entry list; `castore::build()` does the second pass over it.
>
> **Reconciliation (post-DONE): castore tenancy is resolved at read time, not materialized.** Migration `064_directory_paths` (commit `d9d78a0e`) **drops** the `directory_tenants`/`file_blob_tenants` junctions this section and P0552/P0570/P0573/P0577/P0586 describe, and replaces them with `directory_paths (digest, store_path_hash)` mirroring `file_blobs`; every read joins through the path junction to `path_tenants`. Two reasons: (a) the materialized junctions were a one-shot snapshot of `path_tenants` taken at first-index time, keyed `(digest, tenant)` — coarser than the data they govern, so `ReadBlob`/`StatBlob` joining on digest alone could pick *another tenant's* NAR for a content-shared digest and leak that NAR's boundary-chunk hashes; (b) a tenant gaining a `path_tenants` row after first-index was permanently denied `DirectoryService` reads. Read every `JOIN directory_tenants`/`JOIN file_blob_tenants` in the file tables below as `JOIN directory_paths`/`file_blobs` `→ JOIN path_tenants ON store_path_hash`. The GC sweep's explicit `file_blob_tenants` cleanup is gone (path-junction rows cascade via the `manifests` FK). The same commit fences `set_nar_index` on the manifest's `claim_id` so an index computed before a GC + re-upload cannot land on the new manifest, and migration `063_file_blob_size` denormalizes `file_blobs.size` so the read path never decodes `nar_index.entries`. **This also voids the P0557 block's failure mode** — see the note there.

**Load-bearing for the §2 mount path** — the castore-FUSE serves this DAG. The work happens once at PutPath/index time (<1 ms on top of P0546's blake3 pass; bytes already in RAM).

| File | Change |
|---|---|
| `rio-proto/proto/types.proto` | `NarIndexEntry { …; bytes dir_digest = 8; }` (populated when `kind==DIR`; blake3 of canonical Directory encoding); `NarIndex { …; bytes root_digest = 2; }` |
| `rio-proto/proto/castore.proto` | new — vendor snix [`castore.proto`](https://git.snix.dev/snix/snix/raw/branch/canon/snix/castore/protos/castore.proto) (MIT): `message Directory { repeated DirectoryEntry directories; repeated FileEntry files; repeated SymlinkEntry symlinks; }` with `FileEntry{name, digest, size, executable}`, `DirectoryEntry{name, digest, size}`, `SymlinkEntry{name, target}`. **Pin canonical encoding rule** in a doc-comment: fields sorted by `name` (bytes-lex), no unknown fields, prost's default field-order encode. **snix issue #111**: prost determinism is not formally guaranteed across versions — add a golden-bytes test that fails loudly on encoder drift. `// r[impl store.castore.canonical-encoding]` |
| `rio-nix/src/nar/` | `nar_ls` second pass (bottom-up over the entry list, deepest-first): for each `kind==DIR`, build `Directory{…}` from immediate children's `file_digest`/`dir_digest`/`target`, encode, `dir_digest = blake3(encoded)`. `root_digest` = top dir's `dir_digest`. ~50 LoC. `// r[impl store.index.dir-digest]` |
| `migrations/062_nar_index.sql` (P0551 — pre-created) | `nar_index.root_node` (encoded `oneof{dir_digest, FileEntry, SymlinkEntry}` — what P0588's dispatch query reads), `directories` (digest PK, body, refcount), `directory_tenants`, `file_blobs` (a junction, not a refcounted singleton — one row per `(file_digest, containing-manifest)` so GC of one referrer cannot dangle the lookup), `file_blob_tenants` + indexes — **already created by P0551's migration**, no DDL change here. P0572 removes the four `ALLOW_DEAD` entries in `rio-store/tests/migrations.rs` when these tables get Rust callers. |
| `rio-store/src/nar_index.rs` (P0552) | after `set_nar_index`: write `root_node` column. `INSERT INTO directories … ON CONFLICT (digest) DO UPDATE SET refcount = directories.refcount + 1` (UNNEST, **sorted** input per `r[store.chunk.refcount-txn]`); `INSERT INTO directory_tenants (digest, $tenant_id) ON CONFLICT DO NOTHING`. `INSERT INTO file_blobs (digest, store_path_hash, nar_offset) … ON CONFLICT DO NOTHING` (sorted UNNEST); `INSERT INTO file_blob_tenants (digest, $tenant_id) ON CONFLICT DO NOTHING`. `// r[impl store.castore.gc]` `// r[impl store.castore.tenant-scope]` |
| `rio-store/src/gc/sweep.rs` (existing sweep) | in the per-manifest sweep txn, before `DELETE narinfo` cascades: decode the dying `nar_index.entries`, `UPDATE directories SET refcount=refcount-1 WHERE digest=ANY($sorted)`; `DELETE FROM directories WHERE digest=ANY($zeros)` (no S3 object → hard-delete; junction rows go via `ON DELETE CASCADE`). `file_blobs` rows for the dying manifest cascade-delete via the `manifests` FK — **no repoint needed; surviving referrers' rows remain**. After cascade: `DELETE FROM file_blob_tenants ft WHERE ft.digest = ANY($dying_file_digests) AND NOT EXISTS (SELECT 1 FROM file_blobs fb WHERE fb.digest = ft.digest)`. |
| tests | proptest: `serialize(tree)` → `nar_ls` → re-derive `dir_digest` from children == stored value. snix-interop golden: known tree → `root_digest` matches snix's `tvix-store import` output (fixture bytes pinned). **GC**: PutPath A and B sharing a subtree → `directories.refcount==2` for the shared digest → GC A → `refcount==1` → GC B → row gone. **file_blobs survives first-referrer GC**: PutPath A and B sharing a regular file (same `file_digest`) → 2 `file_blobs` rows → GC A → `ReadBlob(file_digest)` still resolves via B's row → GC B → no rows + `file_blob_tenants` row gone. `// r[verify store.index.dir-digest]` `// r[verify store.castore.{canonical-encoding,gc}]` |

**Load-bearing for the mount path** as of ADR-022 §2.2: P0559's castore-FUSE serves the Directory DAG directly (`lookup`/`readdir` from `Directory` bodies); the builder cannot mount without it. Also enables U5 (snix `castore.proto` interop + `root_digest` as a closure-level cache key + the DAG that P0574 walks). Measured 12.1% dir-sharing on chromium (~90% empty dirs).

**Exit:** `/nixbuild --checks` green; `dir_digest`/`root_digest` populated for all regular paths; golden-bytes encoding test pinned.

### P0570 — `StatBlob` RPC: server-side `file_digest → ChunkMeta[]`
**Crate:** `rio-proto, rio-store` · **Deps:** P0573 · **Complexity:** LOW · **Status: DONE 2026-05-18**
| File | Change |
|---|---|
| `rio-proto/proto/store.proto` | `rpc StatBlob(StatBlobRequest) returns (StatBlobResponse)` — `StatBlobRequest { bytes file_digest = 1; bool send_chunks = 2; }`, `StatBlobResponse { repeated ChunkMeta chunks = 1; uint32 first_chunk_skip = 2; uint32 last_chunk_take = 3; }`, `ChunkMeta { bytes digest = 1; uint64 size = 2; }`. The `first_chunk_skip`/`last_chunk_take` slice offsets are needed for files resolved via a legacy-`PutPath` manifest, whose whole-NAR FastCDC chunks straddle the file boundary; for `PutPathChunked`-ingested manifests both are full-chunk (`skip=0`, `take=chunks[last].size`). snix's [`BlobService.Stat`](https://git.snix.dev/snix/snix/raw/branch/canon/snix/castore/protos/rpc_blobstore.proto). `// r[impl store.castore.blob-stat]` |
| `rio-store/src/grpc/directory.rs` | `stat_blob(file_digest)`: shares the `file_blobs` + manifest-cumsum `partition_point` helper with `read_blob` (P0577) — `SELECT f.store_path_hash, f.nar_offset FROM file_blobs f JOIN file_blob_tenants t ON t.digest=f.digest JOIN manifests m ON m.store_path_hash=f.store_path_hash WHERE f.digest=$1 AND t.tenant_id=$2 AND m.status='complete' LIMIT 1` (any surviving complete referrer; the `manifests` FK guarantees liveness, the `status` filter excludes `'uploading'` placeholders) → resolve to chunk-range via that manifest's chunk cumsum → return `ChunkMeta[]` plus `first_chunk_skip = nar_offset − cumsum[range.start]` and `last_chunk_take = (nar_offset + file_size) − cumsum[range.end−1]` (same arithmetic P0577's `read_blob` already uses to slice first/last chunk bytes). Same JWT-or-HMAC tenant scoping as `GetDirectory`. The builder's `open()` calls this for `> STREAM_THRESHOLD` files to get the chunk list before checking `/var/rio/chunks/`; ≤ threshold calls `ReadBlob(file_digest)` directly. **No client-side DigestResolver** — `open()` holds only `(file_digest, size)` from the inode map and resolves server-side. |
| tests | proptest: synth N NARs with overlapping files → `StatBlob(file_digest).chunks` concatenate to bytes whose blake3 == digest. `// r[verify store.castore.blob-stat]` |

**Exit:** `/nixbuild --checks` green.

> **Reconciliation (2026-05-18).** Two deviations:
> (1) **Inline manifests → `FAILED_PRECONDITION`.** The plan's SQL didn't account for inline manifests, which have no `chunk_list`. A synthetic `ChunkMeta` for the inline blob would point at a digest that isn't in the chunk store. Files in inline NARs are below `STREAM_THRESHOLD`, so the caller should be on `ReadBlob`; `FAILED_PRECONDITION` makes the misroute visible. The query selects `inline_blob IS NOT NULL`, not the bytes, and `send_chunks=false` runs an `EXISTS` probe that skips `manifest_data` entirely — neither path detoasts a large blob.
> (2) **Fixed-geometry sweep, not proptest.** Each round-trip seeds a NAR+manifest+chunks into PG; proptest's shrink loop would re-seed hundreds of fixtures per failure. The sweep covers the boundary classes (file < / == / > chunk, chunk-aligned ends, 1-byte chunks, multi-chunk straddle), and `read_blob`'s whole-file BLAKE3 trailer is a tighter end-to-end check on the same `build_chunk_plan`.

---

## Phase 2 — Store nar_index

### P0551 — migration 062
**Crate:** `rio-store` · **Deps:** P0545 · **Complexity:** LOW · **Status: DONE 2026-05-15** (`678c206c`)
| File | Change |
|---|---|
| `migrations/062_nar_index.sql` | `CREATE TABLE nar_index (store_path_hash, entries, root_node, created_at)` + `manifests.nar_indexed` partial-index work-queue (precedent: migration 031's `WHERE status='uploading'`). PG forbids subqueries in partial-index predicates, so the queue is a same-table bool flag; indexer flips `nar_indexed=true` on success (HOT-update eligible). **The migration also pre-creates** P0572's `directories`/`directory_tenants`/`file_blobs`/`file_blob_tenants`, P0581's `narinfo.compat_file_hash`, and P0586's `chunks.durable` + `chunks_present_idx` so the file is pinned once. P0583's `DROP COLUMN inline_blob` is **not** here (the store still reads it); that gets its own migration. |
| ~~`migrations/055_manifests_boot_size.sql`~~ | **NOT created** — no boot blobs |
| `rio-store/tests/migrations.rs` | `(61, "<sha384>")` PINNED entry + `ALLOW_DEAD` entries for the four pre-created P0572 tables (removed when P0572 lands). |
| `rio-store/src/migrations.rs` | `M_062` doc-const |
| `rio-store/src/metadata/queries.rs` | `get/set_nar_index`, `list_nar_index_pending(limit)`. `// r[impl store.index.table-cascade]` |

**Exit:** `/nixbuild --checks` green.

### P0552 — GetNarIndex handler + indexer loop
**Crate:** `rio-store` · **Deps:** P0545, P0546, P0551 · **Complexity:** MED · **Status: DONE 2026-05-15** (`678c206c`)
| File | Change |
|---|---|
| `rio-store/src/nar_index.rs` | new — `compute(pool, backend, store_path)`: fetch chunks → reassemble → `nar_ls` (now emits `file_digest`) → `set_nar_index`. Guard: `nar_index_sync_max_bytes` config (default 4 GiB). `// r[impl store.index.{non-authoritative,sync-on-miss}]` |
| same | `indexer_loop(pool, backend)` — poll `list_nar_index_pending(32)` → `compute` → sleep 5 s if empty. `// r[impl store.index.putpath-bg-warm]` |
| `rio-store/src/grpc/mod.rs` | `get_nar_index()`: PG hit → return; miss → `compute()` write-through. `// r[impl store.index.rpc]` |
| `rio-store/src/main.rs` | `tokio::spawn(indexer_loop(...))` |
| `rio-store/src/lib.rs` | `pub mod nar_index;` + `rio_store_nar_index_{compute_seconds,cache_hits_total}` |
| tests | ephemeral PG: PutPath 3-file NAR → `GetNarIndex` 3 entries with non-empty `file_digest` → second call cache-hit. `// r[verify ...]` |

**Exit:** `/nixbuild --checks` green.

> **Reconciliation (2026-05-27).** The indexer half of this item no longer exists. P0586's storage conversion made the castore index a mandatory write inside the manifest-complete transaction, so the background warm path lost its purpose: `indexer_loop`, `list_nar_index_pending`, `bump_nar_index_retry`, the `compute()` reassembly entry point, the GetNarIndex sync-on-miss write-through, and the `main.rs` spawn were all deleted (`refactor(rio-store): remove the background NAR indexer`). Migration `065_drop_nar_indexed` then dropped the `manifests.nar_indexed` flag and the `manifests_nar_index_pending_idx` partial index — the work-queue state had no readers left and was being maintained on every manifests write. What survives of P0552 is the GetNarIndex/GetNarIndexBatch RPC surface reading the eagerly-written index, and `nar_index.rs` reduced to the proto conversion + digest extraction helpers. The `store.index.putpath-bg-warm` and `store.index.sync-on-miss` rules went away with the code paths.

---

## Phase 3 — Cache-tier infra (parallel with Phase 2; depends only P0548)  ★ FIRST SHIPPED VALUE (U2)

### P0553 — terraform: per-AZ S3 Express directory bucket + dedicated store SG/NodeClass + IAM
**Crate:** `infra` · **Deps:** P0548 · **Complexity:** LOW

**One `aws_s3_directory_bucket` per supported AZ-ID** (`for_each = toset(local.express_az_ids)`). Store pods address the bucket for their own AZ via env (P0554). `TieredChunkBackend` is AZ-count-agnostic — each replica sees exactly one local bucket name (or none); terraform just provisions N of them. **No CSI driver, no PVC, no Lustre kernel module.**

| File | Change |
|---|---|
| `infra/eks/s3-express.tf` | `resource "aws_s3_directory_bucket" "cache" { for_each = toset(local.express_az_ids); bucket = "rio-chunk-cache--${each.key}--x-s3"; location { name = each.key; type = "AvailabilityZone" } }`; IAM policy `s3express:CreateSession` + `s3express:*` attached to the store IRSA role. **Lifecycle (defense-in-depth, age-based — directory buckets [support expiration only](https://docs.aws.amazon.com/AmazonS3/latest/userguide/directory-buckets-objects-lifecycle.html), not size targets):** `aws_s3_bucket_lifecycle_configuration` with `expiration { days = 30 }` + bucket policy allowing `lifecycle.s3.amazonaws.com` `s3express:CreateSession` `SessionMode=ReadWrite`. The size-target sweep is P0585. `// r[impl infra.express.cache-tier]` |
| `infra/eks/outputs.tf` | `express_bucket_by_az_id` map for helm |
| `infra/eks/variables.tf` | `express_az_ids` — intersection of subnet zone-ids with the Express-supported set; empty list → cache tier disabled cluster-wide |

**Exit:** `tofu apply` creates one directory bucket per supported AZ + store SG/NodeClass; `/nixbuild --checks` green.

### P0554 — helm: chunkBackend.tiered + per-AZ Express bucket env
**Crate:** `infra, xtask` · **Deps:** P0548, P0553 · **Complexity:** LOW
`store.chunkBackend.kind={s3|tiered}` helm value (default `s3`); when `tiered`, `store.chunkBackend.expressBucketByAzId` populated from terraform output. Store Deployment exposes node zone via downward-API env from `topology.kubernetes.io/zone`; container resolves zone→zone-id at startup (IMDS `placement/availability-zone-id`) and selects its bucket; no match → `local=None`. `S3ChunkBackend` for `local` uses zonal endpoint `https://s3express-{az_id}.{region}.amazonaws.com`. **Exit:** `helm template --set store.chunkBackend.kind=tiered` renders; `/nixbuild --checks` green. ★ FIRST SHIPPED VALUE (U2)

### P0555 — VM test: tiered-backend cache semantics
**Crate:** `nix` · **Deps:** P0548, P0554 · **Complexity:** MED
`nix/tests/scenarios/store-tiered.nix`: two store replicas + two minio instances (one "local" Express stand-in, one shared "remote" S3-standard); subtests `cold-miss-fallback`, `put-remote-only` (assert local minio empty post-put), `replica-warm-via-read-through` (replica B's first read miss fills local; replica C's read hits local), `local-none-passthrough`. **Exit:** `nix build .#checks.x86_64-linux.vm-store-tiered` green; `/nixbuild --checks` green.

### P0579 — `binary_cache_compat` config + helm
**Crate:** `rio-store, infra` · **Deps:** P0544 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-store/src/config.rs` | `pub struct BinaryCacheCompat { enabled: bool /* default true */, bucket: Option<String> /* None → chunk_backend.s3.bucket */, compression: CompatCompression /* Zstd|Xz|None, default Zstd */, write_mode: CompatWriteMode /* SyncAfterCommit only for now */ }`. `// r[impl store.compat.runtime-toggle]` |
| `infra/helm/rio-build/values.yaml` | `store.binaryCacheCompat.{enabled,bucket,compression}` (default `enabled: true`) |
| `infra/helm/rio-build/templates/store-deployment.yaml` | env `RIO_STORE__BINARY_CACHE_COMPAT__*` from values |
| `docs/src/configuration.md` | rows added (P0544 also touches; serialise) |

**Exit:** `/nixbuild --checks` green; `helm template` renders the env block.

### P0566 — binary-cache compat writer (stock-Nix `.narinfo` + `nar/*.nar.zst` to S3-standard)
**Crate:** `rio-store` · **Deps:** P0549, P0579 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-store/src/compat/writer.rs` | new — `async fn write(&self, path_info: &PathInfo, chunk_list: &[ChunkRef]) -> Result<(), CompatError>`. Reassemble NAR bytes via `ChunkCache::get` over the just-written chunks (moka-hot). Stream through `async-compression` zstd encoder while computing `sha256(compressed)`. `put_blob("nar/{file_hash}.nar.zst", body)`. Render narinfo via the existing `rio-nix::narinfo::render` (same one the HTTP server uses) **with** `FileHash`/`FileSize`/`Compression` populated; `put_blob("{store_path_hash}.narinfo", body)`. On first-ever write, `put_blob("nix-cache-info", "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n")` if absent. `// r[impl store.compat.{nar-on-put,narinfo-on-put}]` `// r[impl obs.metric.compat]` |
| `rio-store/src/grpc/put_path/mod.rs` | after the PG-commit (status flips `'uploading'`→`'complete'`), if `cfg.binary_cache_compat.enabled`: `compat_writer.write(...).await` — failure logged + `rio_store_compat_write_failures_total` inc, **does not** fail the RPC. `// r[impl store.compat.write-after-commit]` |
| `rio-store/src/lib.rs` | `pub mod compat;` + `rio_store_compat_write_seconds{result}` histogram + `rio_store_compat_write_failures_total` counter |
| tests | unit: PutPath a 2-chunk path with compat ON → in-memory S3 backend has `{hash}.narinfo` and `nar/{filehash}.nar.zst`; round-trip narinfo via `rio-nix::narinfo::parse`; decompressed NAR sha256 == `nar_hash`. compat OFF → neither object present. `// r[verify store.compat.{nar-on-put,narinfo-on-put,write-after-commit,runtime-toggle}]` |

**Exit:** `/nixbuild --checks` green.

### P0580 — VM test: stock-Nix substitutes from S3 with rio-store stopped  ★ U6 LANDS
**Crate:** `nix` · **Deps:** P0566 · **Complexity:** MED
`nix/tests/scenarios/store-compat.nix`: rio-store + minio + stock `pkgs.nix`. Subtests `stock-nix-substitute` (PutPath a 3-path closure → `systemctl stop rio-store` → `nix copy --from 's3://rio?endpoint=http://minio:9000&region=dummy' --to /tmp/out /nix/store/…` → verify all 3 paths land + `nix store verify` passes), `compat-off-no-narinfo` (compat=OFF → PutPath → `aws s3 ls` shows `chunks/` only, no `.narinfo`). Wire at `nix/tests/default.nix` `subtests = [ … ]` with `# r[verify store.compat.stock-nix-substitute]` and `# r[verify store.compat.runtime-toggle]`. **Exit:** `nix build .#checks.x86_64-linux.vm-store-compat` green; `/nixbuild --checks` green.

### P0581 — compat GC integration
**Crate:** `rio-store` · **Deps:** P0551, P0566 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-store/src/gc/sweep.rs` | per-manifest sweep txn already enqueues chunk keys to `pending_s3_deletes`; extend to also enqueue `{store_path_hash}.narinfo` and `nar/{file_hash}.nar.zst` (file_hash read from the dying narinfo row's `compat_file_hash` column). Runs regardless of current `enabled` value — past compat writes are GC'd even if compat is now OFF. `// r[impl store.compat.gc-coupled]` |
| `migrations/062_nar_index.sql` (P0551 — pre-created) | `narinfo.compat_file_hash bytea` (nullable; populated by P0566 on successful compat write) — **already created by P0551's migration**, no DDL change here. |
| tests | GC a path with compat objects → both keys appear in `pending_s3_deletes`. `// r[verify store.compat.gc-coupled]` |

**Exit:** `/nixbuild --checks` green.

### P0582 — compat reconciler (deferred-priority)
**Crate:** `rio-store` · **Deps:** P0566, P0581 · **Complexity:** LOW
`rio-store/src/compat/reconciler.rs`: background loop, `SELECT n.store_path_hash FROM narinfo n JOIN manifests m USING (store_path_hash) WHERE n.compat_file_hash IS NULL AND m.status='complete' LIMIT 64` → `compat_writer.write(...)` for each → sleep 30s if empty. Handles the crash-between-PG-commit-and-S3-write window and backfills paths ingested while compat was OFF. Spawned in `main.rs` only when `enabled`. **Exit:** `/nixbuild --checks` green; unit: insert a `compat_file_hash IS NULL` row → one tick → row populated + S3 objects present.

### P0583 — drop `inline_blob` storage; all NARs chunked
**Crate:** `rio-store, rio-proto` · **Deps:** P0544, P0551 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-store/src/cas.rs` | delete `INLINE_THRESHOLD` const |
| `rio-store/src/manifest.rs` | delete `ManifestKind::Inline` variant; `ManifestKind` collapses to a single chunk-list shape (or is replaced by the bare `Vec<ChunkRef>`/`ChunkManifest` type — implementer's call) |
| `rio-store/src/grpc/put_path/mod.rs` | remove the `nar_len < INLINE_THRESHOLD` size-branch; every NAR goes through `put_chunked`. A NAR shorter than `CHUNK_MIN` yields a single chunk equal to the input (FastCDC behavior — no special-case needed) |
| `rio-store/src/grpc/get_path.rs` | remove the `ManifestKind::Inline` arm; `chunk_cache` is now required (drop the `Option<>` wrapper and the `failed_precondition("inline-only")` guard) |
| `rio-store/src/grpc/put_path_batch.rs` | drop the `INLINE_THRESHOLD` size-gate + `FailedPrecondition` fallback; batch handler chunks every output |
| `rio-store/src/grpc/chunk.rs` | drop the `Option<Arc<ChunkCache>>` wrapper + `require_cache()` guard (backend always present) |
| `rio-store/src/metadata/inline.rs` | **delete file**; fold any non-inline-specific helpers (placeholder-row insert, `update_narinfo_complete`) into `metadata/chunked.rs` or `metadata/mod.rs` |
| `rio-store/src/config.rs` | delete `ChunkBackendKind::Inline`; `chunk_backend` becomes a required field (no `Default`); error message names `filesystem`/`s3`/`memory` |
| `migrations/0NN_drop_inline_blob.sql` (new) | `ALTER TABLE manifests DROP COLUMN inline_blob;` — **separate migration**, NOT in 061: the column is read by `metadata::get_manifest`/`cas.rs`/`get_path.rs` until this plan removes those readers, so the DROP must land in the same commit as the code change. |
| tests | drop inline-specific test cases; update fixtures that relied on `chunk_backend = { kind = "inline" }` to use `memory` |
**Dropped marker:** `r[store.inline.threshold]` — remove any `r[impl store.inline.threshold]` / `r[verify …]` annotations in code.
**Note for P0566/P0582:** depend on this for the `chunk_list: &[ChunkRef]` signature simplification; sequence after P0583 if not already merged.
**Exit:** `/nixbuild --checks` green; `tracey query rule store.inline.threshold` reports no-such-rule.

### P0584 — builder-chunked-only auth gate
**Crate:** `rio-store` · **Deps:** P0586, P0589 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-store/src/grpc/put_path/mod.rs` | at the existing token-verify step: if `claims.role == Builder`, return `Status::permission_denied("builders must use PutPathChunked; PutPath is gateway/admin-only")` before any buffering. Same gate in `put_path_batch.rs`. `// r[impl store.put.builder-chunked-only]` |
| tests | unit: builder-role token → `PutPath` returns `PERMISSION_DENIED`; gateway-role token → `PutPath` proceeds; builder-role token → `PutPathChunked` proceeds. `// r[verify store.put.builder-chunked-only]` |
**Exit:** `/nixbuild --checks` green.

### P0585 — Express eviction sweeper (size-bounded MRU)
**Crate:** `rio-store, infra` · **Deps:** P0548, P0554 · **Complexity:** LOW
| File | Change |
|---|---|
| `rio-store/src/backend/express_sweep.rs` | new — `async fn sweep_loop(cfg, s3_express, lease)`. Per-AZ k8s Lease `rio-store-express-sweep-{az_id}` (rio-store has no leader election today; reuse `rio_scheduler::lease` — the in-tree replacement for `kube-leader-election` 0.43, dropped for a `Patch::Merge` race; either add `rio-scheduler` as a dep or hoist `lease/` to a shared crate). Loop every `sweep_interval_secs`: `ListObjectsV2` (paginated; sum `Size`, collect `(Key, LastModified, Size)` — at 8 TiB / 64 KiB avg ≈ 130M objects, ~1000-key pages ≈ 130K requests, ~10 min list at 200 req/s; acceptable hourly); set `rio_store_express_bytes{az_id}` gauge. If total > `target_bytes × evict_high_watermark`: sort by `LastModified` asc, `DeleteObjects` (batch 1000) oldest until under `target_bytes × evict_low_watermark`; inc `rio_store_express_evicted_total{az_id}` by deleted count. `// r[impl infra.express.bounded-eviction]` `// r[impl obs.metric.express-eviction]` |
| `rio-store/src/config.rs` | `ExpressConfig { target_bytes: u64 = 8_796_093_022_208, evict_high_watermark: f64 = 1.10, evict_low_watermark: f64 = 0.90, sweep_interval_secs: u64 = 3600 }` under `chunk_backend.tiered` |
| `rio-store/src/main.rs` | when `chunk_backend.kind == tiered` and `local.is_some()`: spawn `sweep_loop` task |
| `infra/helm/rio-build/templates/store-rbac.yaml` | `Role` allowing `coordination.k8s.io/leases` `get/create/update` in store namespace; bind to store SA |
| tests | unit: in-memory S3 backend with `LastModified` injectable; fill past high-watermark → sweep deletes oldest until under low-watermark; assert byte gauge + evicted counter. `// r[verify infra.express.bounded-eviction]` `// r[verify obs.metric.express-eviction]` |
**Exit:** `/nixbuild --checks` green; vm-store-tiered gains `evict-over-target` subtest (P0555 follow-on, optional).

---

## Phase 4 — store-side index (gated on Phase-0 PASS + P0546)

### P0556 — [ABANDONED] `composefs-sys` + `encode.rs` — `libcomposefs` FFI encoder
**Status: ABANDONED 2026-04-23.** Was the EROFS metadata-image encoder for the §3 composefs-style alternative. §2 castore-FUSE serves the Directory DAG directly via `lookup`/`readdir` — no image, no encoder, no `composefs-sys` crate, no `libcomposefs-user-xattr.patch`, no `composefs-encoder.nix` VM test, no `composefs_encode` fuzz target. Number kept for stability; do not reuse.

### P0557 — PutPath eager `nar_index` compute (no encode)
**Crate:** `rio-store` · **Deps:** P0551, P0552, P0572, P0586 · **Complexity:** LOW · **Status: DONE (landed inside P0586's storage conversion — see the reconciliation note)**

> **Reconciliation (post-DONE).** The eager index landed as part of P0586's
> blob-stream conversion rather than as the `try_acquire`-gated spawn the file
> table below describes, and it is **mandatory, not an optimization**: the
> conversion stores per-file content chunks and never persists the NAR byte
> stream, so the castore index cannot be recomputed after ingest — it MUST be
> written while the NAR bytes are in hand. `complete_manifest_in_conn` now
> takes the parsed NAR (`cas::ParsedNar`) and calls `set_nar_index_in_conn`
> inside the same transaction as the `uploading → complete` flip, for **every**
> ingest path (PutPath, PutPathBatch, the substituter, and the future
> PutPathChunked commit). There is no semaphore, no spawn, no work queue, and
> no `compute_from_bytes` — the background indexer and the sync-on-miss
> recompute were deleted outright (`store.index.authoritative` replaced
> `store.index.non-authoritative`/`sync-on-miss`/`putpath-bg-warm`). The
> migration-064 tenancy concern below is moot: `set_nar_index_in_conn` writes
> only content- and path-keyed rows; tenancy joins through `path_tenants` at
> read time.

> **Blocked (2026-05-19).** This item was planned against the P0551/P0552 shape of
> `set_nar_index(pool, hash, entries)` — one tenant-independent table write. P0572
> retroactively grew it to `set_nar_index(…, dag)` writing `directories`,
> `directory_tenants`, `file_blobs`, `file_blob_tenants`, with the two `*_tenants`
> inserts via `CROSS JOIN path_tenants`. `path_tenants` is populated by the
> *scheduler* (`upsert_path_tenants` in dispatch/merge), which fires after the
> worker reports build completion — i.e. after `PutPath` returns. An eager spawn
> inside `finalize_single` therefore *always* runs `set_nar_index` against an
> empty `path_tenants` table: the cross joins emit zero rows, `nar_indexed`
> flips `TRUE`, the indexer loop never re-touches the path, and the junction
> inserts are gated `if inserted` so no later pass repairs them. Result:
> `GetDirectory`/`HasDirectories`/`HasBlobs`/`ReadBlob`/`StatBlob` return
> `NotFound` for the path's owning tenant, permanently.
>
> The plan's own exit criterion (`GetNarIndex` < 100 ms) doesn't probe this:
> `GetNarIndex` is builder-internal (`reject_end_user_tenant`), never joins the
> junction tables, and goes green over the bug. The indexer loop carries the same
> latent race today (~ms scheduler RTT vs. 5 s poll — usually wins, not a
> guarantee).
>
> **Resolution: P0586.** Its commit txn writes `path_tenants` + `directory_tenants`
> + `file_blob_tenants` + `nar_index` in one transaction with `claims.tenant`
> threaded through the HMAC `WorkAssignment` claim — no race window. Once P0586
> lands, the eager-index optimization is correct by construction (the bytes are
> in RAM during the same txn). Implement P0557 inside P0586's commit path, not as
> a separate `finalize_single` spawn.
>
> **Re-examine (2026-05-21).** Migration 064 (see the P0572 reconciliation note)
> removed the write-time `path_tenants` cross-join that made this race
> *permanent*: `set_nar_index` now writes only content- and path-keyed rows
> (`directories`, `directory_paths`, `file_blobs`), and tenancy is joined from
> `path_tenants` at read time — a read that races the scheduler's
> `upsert_path_tenants` returns NotFound *until the row lands*, then self-heals.
> The remaining argument for keeping P0557 inside P0586's commit txn is
> coherence (one txn, bytes already in RAM), not correctness. Re-evaluate the
> BLOCKED status when picking up P0586.

| File | Change |
|---|---|
| `rio-store/src/grpc/put_path/mod.rs` (after `cas::put_chunked` Ok) | `if let Ok(permit) = index_sem.clone().try_acquire_owned() { tokio::spawn(async move { let _p = permit; nar_index::compute_from_bytes(pool, &nar_bytes, store_path).await }) }` — eager only if a permit is *immediately* free; otherwise leave for `indexer_loop` (≤5 s pickup). NAR bytes passed as `Arc<Vec<u8>>`. `index_sem` sized by config `nar_index_concurrency` (default 4). `// r[impl store.index.putpath-eager]` |
| `rio-store/src/grpc/put_path_batch.rs` | same gate |
| `rio-store/src/nar_index.rs` | `compute_from_bytes(pool, &[u8], path)` — `Cursor::new(bytes)` → `nar_ls` → `set_nar_index`. Reuses RAM, no chunk fetch. |
| `rio-store/src/config.rs` | `+ nar_index_concurrency: usize` (default 4) |
| `nix/tests/scenarios/protocol-warm.nix` | new subtest `eager-nar-index`: PutPath a 3-file NAR → `GetNarIndex` returns within 100 ms (eager path hit, before `indexer_loop` would have picked it up). |
| `nix/tests/default.nix` | wire `eager-nar-index` subtest with `# r[verify store.index.putpath-eager]` at the `subtests=[…]` entry |

**Exit:** `/nixbuild --checks` green; `vm-protocol-warm` `eager-nar-index` subtest green. The exit criterion above only probes `GetNarIndex`; add a tenant-scoped `GetDirectory`/`StatBlob` round-trip on a freshly-built path to actually catch the `path_tenants` race.

---

## Phase 5 — castore-FUSE builder-side

### P0588 — `WorkAssignment.input_roots` — scheduler→builder root_node transport — **DONE**
**Crate:** `rio-proto,rio-scheduler` · **Deps:** P0572 · **Complexity:** LOW (~40 LoC)

The builder's mount-time DAG prefetch (P0559) needs each input store path's `root_node` (a `dir_digest` for directory paths, or the `FileEntry`/`SymlinkEntry` directly for non-dir paths). The pre-castore design computed closure builder-side via `QueryPathInfo` BFS and called `GetNarIndex(nar_hash)`; the scheduler now computes the transitive closure at dispatch time (BFS over `narinfo.references`) and supplies the roots directly.

| File | Change |
|---|---|
| `rio-proto/proto/build_types.proto` | `repeated InputRoot input_roots = 13;` and `repeated string input_closure = 14;` — new field numbers, NOT field 3 (`proto_field_presence.rs` forbids reuse on principle even for never-populated reservations). `InputRoot { store_path, rio.castore.RootNode root_node }` reuses the existing `RootNode` oneof from `castore.proto` instead of redeclaring it. `input_closure` is the **transitive runtime closure of the build's inputs** (BFS over `narinfo.references`, equivalent to Nix `computeFSClosure(inputs)`; NOT `approx_input_closure` which is a shallow DAG-local approximation), sorted, exactly the value the scheduler hashes into `claims.input_closure_digest` (P0589). |
| `rio-scheduler/src/db/closure.rs` | `compute_input_roots(seeds)`: `WITH RECURSIVE` BFS over `narinfo.references` from the dispatch-time seeds, then `LEFT JOIN nar_index` for each closure path's `root_node`. Sorted output. Cycle-safe via `UNION` set semantics. `// r[impl sched.dispatch.input-roots]` |
| `rio-scheduler/src/actor/dispatch.rs` | `build_assignment_proto()` (the plan called this `compute_work_assignment()`; the real fn name is `build_assignment_proto`): seeds the closure walk with `approx_input_closure`, populates `input_roots` (decoding `RootNode` from `nar_index.root_node` BYTEA) and `input_closure`. Best-effort — on PG failure sends empty and the builder falls back to `QueryPathInfo` BFS. |
| `docs/spec/components/scheduler.typ` | `#r("sched.dispatch.input-roots")[…]` normative text: WorkAssignment MUST carry the build's transitive input closure and per-path castore roots; closure is the BFS over `narinfo.references`, NOT `approx_input_closure`; same sorted closure feeds `input_closure_digest`. |
| `rio-scheduler/src/db/tests/closure.rs` | three PG tests: 3-level closure resolves all `root_node`s sorted; unindexed/unknown paths survive with `None`; cyclic refs terminate. `// r[verify sched.dispatch.input-roots]` |

### P0589 — `AssignmentClaims.{role, input_closure_digest}` + dispatch populate — **DONE**
**Crate:** `rio-auth, rio-scheduler` · **Deps:** P0544, P0588 · **Complexity:** LOW

Adds the claim fields that P0573 (tenant for castore tenant-scoping), P0586 (`role`, `input_closure_digest` for `validate_begin`), and P0584 (`role` for the PutPath gate) all read. Sequenced *before* all of them so each compiles against a complete `AssignmentClaims`.

**Reconciliation note:** the plan originally called for a third field `tenant_id: Uuid`. By the time P0589 landed, `AssignmentClaims` already carried `tenant: Option<String>` (added by the bug_011 fix for `hw_perf_samples.submitting_tenant` attribution). Same semantic — a hyphenated UUID string. P0573/P0577 read `claims.tenant` and `Uuid::parse_str` it; no duplicate field. Likewise `TokenRole` only has a `Builder` variant — `Gateway`/`Admin` exist in the plan as forward-looking placeholders, but `AssignmentClaims` is exclusively scheduler-minted (the gateway uses `ServiceClaims`); add the variants when something actually issues them.

| File | Change |
|---|---|
| `rio-auth/src/hmac.rs` | `AssignmentClaims` gains `role: TokenRole` (`#[serde(default, skip_serializing_if = "TokenRole::is_default")]` — bug_011 wire-compat pattern) and `input_closure_digest: String` (hex `blake3(closure.join("\n"))`; `skip_serializing_if = "String::is_empty"`). `AssignmentClaims::digest_input_closure(&[String]) -> String` is the shared digest helper so the scheduler (signer) and the store (`validate_begin` verifier) compute identically. New dep: `blake3`. |
| `rio-scheduler/src/actor/dispatch.rs` | token issuance sets `role: TokenRole::Builder` and `input_closure_digest` from P0588's closure (`""` if the closure compute failed → store treats as "no attestation"). |
| `docs/spec/system/security.typ` | `r[common.hmac.claims]`: enumerate all eight fields, drop "exactly five", document the `skip_serializing_if` wire-compat story. |
| `rio-auth/src/hmac.rs` tests | `old_token_without_p0589_fields_parses` (default-valued fields elided + pre-P0589 token still parses); `closure_digest_deterministic_and_order_sensitive`. `// r[verify common.hmac.claims]` |
**Exit:** `/nixbuild --checks` green.

### P0559 — `castore_fuse/{tree,open,circuit}.rs`
**Crate:** `rio-builder` · **Deps:** P0550, P0567, P0568, P0570, P0572, P0573, P0577, P0588 · **Complexity:** MED (~650 LoC) · **Status:** DONE 2026-05-25 (unit-tested; the mount sequence that wires it into a build and the `vm-castore-e2e` exercise are P0560)

> **Reconciliation (2026-05-25).** Landed as `castore_fuse/{tree,open,circuit,mountd_client}.rs` + the `CastoreFs` `Filesystem` impl in `castore_fuse/mod.rs`, with these deviations. (a) **fuser is git-pinned past 0.17.0**: the planned `BackingId::create_raw(id)` reply path does not exist in any released fuser — 0.17.0 can only mint a `BackingId` by issuing `BACKING_OPEN` on the session's own fd, which is exactly the ioctl the unprivileged builder cannot make. The raw-marshalling API (`ReplyOpen::wrap_backing(u32)` / `BackingId::into_raw()`) is master-only; the pin is the workspace's first git dependency and drops at the first release containing `cberner/fuser@a664f194`. (b) **The mountd client is a sync dedicated-reader-thread client** (`mountd_client.rs`, not in the file table): FUSE callbacks run on fuser's thread pool, not in tokio, so the planned `HashMap<u32, oneshot::Sender>` correlation runs on a blocking `recvmsg` thread dispatching to per-seq `mpsc` rendezvous channels. Same out-of-order-reply semantics, no `block_on` re-entrancy. (c) **No new config fields yet**: nothing maps `Config → OpenerConfig` until P0560's mount sequence exists, so `mountd_request_timeout`/`dag_prefetch_timeout`/`max_backing_ids` would be unread fields. They land with P0560. `jit_fetch_timeout` and `disable_passthrough` will reuse the existing `fuse_fetch_timeout`/`fuse_passthrough` knobs rather than adding parallel ones with the same defaults and meaning. `setrlimit(RLIMIT_NOFILE)` is likewise P0560 startup. (d) **Case (c) (size > `STREAM_THRESHOLD`) falls back to whole-file `ReadBlob`** until P0575; `open_case_total{case="miss_stream"}` still counts how often the streaming path would have been taken. (e) **`circuit.rs` moved, not copied** — `fuse/circuit.rs` is a re-export shim that P0560 deletes with the rest of `fuse/`. (f) `rio_builder_castore_fuse_{lookup,readdir}_total` collapsed into `upcalls_total{op}` (the §14 table's shape); `chunk_source_total{src}` lands with the P0575 chunk path that gives it a second label value.

> **Reconciliation (2026-05-29, open() degrade ladder).** The `open.rs` row below says hitting the `max_backing_ids` ceiling makes `open()` return `EMFILE` (build-fatal). The implemented behavior deviates: at the ceiling the open degrades to a userspace `FOPEN_KEEP_CACHE` read of the cache entry instead. The ceiling exists only to bound mountd's IDR growth, and the file is fully servable from the cache fd the opener already holds — failing a long build for a resource-bounding limit contradicted the open() degrade ladder (evicted entries, promote races, and io-mode conflicts all degrade rather than fail). The default stays 4096 as planned.

| File | Change |
|---|---|
| `rio-builder/src/castore_fuse/tree.rs` | new — DAG prefetch + inode model (ADR §2.2-2.3). `pub async fn build_tree(store: &StoreClient, roots: &[(StorePath, RootNode)]) -> Result<InoMap>` where `RootNode ∈ {Dir(dir_digest), File(file_digest, size, exec), Symlink(target)}` (from `WorkAssignment.input_roots`, P0588). One **`GetDirectory(recursive=true)`** call seeded with all `Dir` roots' digests in field 3 (multi-root; I-110 lesson) — wrapped in `timeout(dag_prefetch_timeout)` (config, default 30 s) → infra-retry on expiry; insert each streamed body into `dirs: HashMap<[u8;32], Directory>` (deduped by digest). Build `inodes: HashMap<u64, Node>` where `Node ∈ {File{file_digest,size,exec}, Dir{dir_digest}, Symlink{target}}` and `ino = h(file_digest ‖ exec) / h(dir_digest) / h("l" ‖ target)` per ADR §2.3: low 63 bits of blake3 with bit 63 set. `FUSE_ROOT_ID` (=1) is synthetic — its `readdir` enumerates `roots` by store-path basename. `pub fn lookup(&self, parent_ino, name: &[u8]) -> Option<(u64, FileAttr)>` reads parent's `Directory` body, finds child by name. `pub fn attr(&self, ino) -> FileAttr` (mode `0o40555/0o100555/0o100444/0o120777`, mtime=1, uid/gid=0). `// r[impl builder.fs.{castore-dag-source,castore-inode-digest}]` |
| `rio-builder/src/castore_fuse/mod.rs` | `fuser::Filesystem` impl rooted at `/var/rio/castore/{build_id}`. **Startup**: `setrlimit(RLIMIT_NOFILE, 65536)`. **`init`**: `config.set_max_stack_depth(1)` (negotiates `FUSE_PASSTHROUGH`; `fuser` ≥0.17) **+ `config.add_capabilities(FUSE_DO_READDIRPLUS \| FUSE_READDIRPLUS_AUTO \| FUSE_PARALLEL_DIROPS \| FUSE_CACHE_SYMLINKS)`** (ADR §2.4 — snix's exact set). **`lookup(parent, name)`** → `tree.lookup(parent, name)` → `reply.entry(&Duration::MAX, &attr, 0)`; unknown → `reply.entry(&Duration::MAX, &FileAttr{ino:0,..Default::default()}, 0)` (kernel `fuse_lookup_name` treats `nodeid=0` as ENOENT-with-valid-timeout — caches at the FUSE layer; under overlay both forms cache, I-043, but `ino=0` is correct for bare-mount per ADR §2.4). **`getattr(ino)`** → `reply.attr(&Duration::MAX, &tree.attr(ino))`. **`readdir(ino, fh, off)`** + **`readdirplus(ino, fh, off)`** → enumerate `tree.dirs[dir_digest]` children (`readdirplus` pre-populates dcache; plain `readdir` is the fallback under `READDIRPLUS_AUTO` downgrade). **`opendir`** → `reply.opened(0, FOPEN_CACHE_DIR \| FOPEN_KEEP_CACHE)`. **`readlink(ino)`** → `reply.data(target)`. **`getxattr`** → `reply.error(ENODATA)`. **`listxattr`** → if `request.size() == 0` then `reply.size(0)` else `reply.data(&[])` — replying `size(0)` to a `size>0` call emits 8 NUL bytes that `fuse_verify_xattr_list` rejects → EIO; this broke `shutil.copy2`/venv once already (overlayfs probes `user.overlay.*` on every lower inode; unhandled would `ENOSYS` — matches snix). `// r[impl builder.fs.castore-cache-config]` `// r[impl builder.fs.listxattr-size-branch]` |
| `rio-builder/src/castore_fuse/open.rs` | **`open(ino)`** — resolve `ino → file_digest` via `tree.inodes`; look up backing path in **shared node-SSD cache** `/var/rio/cache/{aa}/{rest}` (P0571). **(a) hit** → open O_RDONLY, send fd to rio-mountd UDS (`BackingOpen{}` + SCM_RIGHTS cmsg) → recv `backing_id` → `reply.opened_passthrough(fh, flags, &BackingId::create_raw(id))`. **(b) miss + `size ≤ STREAM_THRESHOLD`** → `O_EXCL`-create `staging/{build_id}/<hex>.partial` + `flock(LOCK_EX)` (loser condvar-waits on the in-process per-`file_digest` `FillState` — small files: until completion); `tokio::time::timeout(jit_fetch_timeout, circuit.call(‖ ReadBlob(file_digest) → .partial))`, whole-file blake3 verify; `rename`; `Promote{digest}` → as (a). **(c) miss + `size > STREAM_THRESHOLD`** → `StatBlob(file_digest, send_chunks=true)` → P0575 streaming with that `ChunkMeta[]`. **`read`** only for (c). **`release(fh)`** → `BackingClose{id}` if present. **No BackingId LRU**: backing-ids are released only on `release(fh)`. The kernel holds its own ref on the backing file ([passthrough.c](https://github.com/torvalds/linux/blob/master/fs/fuse/passthrough.c)) for the open's lifetime — `BACKING_CLOSE` only frees the IDR slot, reads stay passthrough — so an LRU evict cannot "fall back to FUSE read" and the IDR is `idr_alloc_cyclic` so reuse-collision is not a concern. `max_backing_ids` (default 4096) is enforced as a per-build concurrent-open ceiling: on overflow, `open()` returns `EMFILE` (build-fatal, surfaced in metrics). All mountd sends wrapped in `timeout(mountd_request_timeout)`. Prototype: `spike_digest_fuse.rs` (`af8db499`). `// r[impl builder.fs.{digest-fuse-open,passthrough-on-hit,file-digest-integrity,shared-backing-cache}]` |
| `rio-builder/src/config.rs` | `+ mountd_request_timeout: Duration` (30 s); `+ jit_fetch_timeout: Duration` (60 s); `+ dag_prefetch_timeout: Duration` (30 s); `+ max_backing_ids: usize` (4096); `+ disable_passthrough: bool` (env `RIO_DISABLE_PASSTHROUGH` — escape hatch). |
| `rio-builder/src/lib.rs` | `+ rio_builder_castore_fuse_{lookup,readdir}_total` (cold-metadata counter); `+ rio_builder_castore_fuse_open_mode_total{mode}`; `+ rio_builder_castore_fuse_open_case_total{case}`; `+ rio_builder_castore_fuse_chunk_source_total{src}` (I-056). |
| `rio-builder/src/castore_fuse/circuit.rs` | port of `fuse/circuit.rs` — breaker around `fetch_chunks_parallel`. `// r[impl builder.fs.fetch-circuit]` |
| `rio-builder/src/castore_fuse/mod.rs` | `pub mod tree; pub mod open; pub mod circuit; pub mod mount;` |
| `rio-builder/src/lib.rs` | `pub mod castore_fuse;` + register `rio_builder_castore_fuse_{upcalls_total{op},open_seconds,fetch_bytes_total{hit},integrity_fail_total,eio_total}` + `rio_builder_castore_dag_prefetch_seconds` (overview §14). `// r[impl obs.metric.castore-fuse]` |
| tests | unit: `tree.lookup(ROOT, basename)` round-trip; per-digest ino — two paths same content → same ino; `readdirplus` returns children with `Duration::MAX` ttl. `// r[verify builder.fs.{digest-fuse-open,castore-inode-digest,castore-cache-config}]` |

**Mode summary:** lookup/getattr/readdir/readlink → in-memory DAG, `Duration::MAX` ttl, dcache absorbs repeats. open() cache hit → passthrough (zero further upcalls). Cache miss ≤ threshold → fetch-whole then passthrough. Cache miss > threshold → P0575 streaming during fill, then next open is passthrough. The FUSE `read` op is reachable only in the streaming window.

**Exit:** `/nixbuild --checks` green (unit only).

### P0567 — `rio-mountd` DaemonSet (fd-handoff + `BACKING_OPEN` broker + `Promote`/`PromoteChunks`)
**Crate:** `rio-builder, infra` · **Deps:** P0576, P0578 · **Complexity:** MED (~250 LoC + helm) · **Status:** DONE 2026-05-25 (end-to-end exercise of the deployed DS is P0560§B)

> **Reconciliation (2026-05-25, deployment tail).** `mountd-ds.yaml` + the eks-node `/var/rio` wiring landed with three deviations. (a) **`privileged: true`, not `privileged: false` + `CAP_SYS_ADMIN`**: the FUSE mounts mountd creates at `/var/rio/castore/{build_id}` must propagate out of the DS container to builder pods (their overlay lowerdir), which requires `mountPropagation: Bidirectional` on the `/var/rio` volumeMount, and the apiserver rejects Bidirectional on non-privileged containers. No seccompProfile is declared either — the runtime forces Unconfined for privileged containers. `hostPID` (design-overview §11's sketch) is omitted: no implemented codepath reads another process's namespace; revisit at P0560 if the e2e test disagrees. (b) **The eval-time `pquota` assert became two runtime layers**: module eval cannot observe what filesystem `/var/rio/staging` lands on at boot. Instead `rio-nvme-mount` bind-mounts `/var/rio` onto the prjquota instance-store XFS (a bind mount inherits the superblock's quota support; mountd uses `quotactl_fd`, no device path needed), and on nodes where that didn't happen `quota.rs::apply_project_quota` fails the first `Mount` rather than running unquota'd. EBS-only NodeClasses therefore cannot run castore-FUSE builds until they grow a prjquota volume. (c) **The DS ships `enabled: false`** — nothing dials the socket until P0559/P0560, and an idle privileged DaemonSet on every builder node is pure attack surface. `nix/tests/helm/25-mountd-ds.sh` renders it explicitly and pins the privilege-boundary invariants (namespace PSA, Bidirectional+privileged pairing, the three node-taint tolerations) since the default profile renders never exercise the template.

> **Reconciliation (2026-05-21).** The daemon (`castore_fuse/{mountd,mountd_proto}.rs` + `bin/rio-mountd.rs`) landed with four deviations from the file table below, all in the same direction — less hand-rolled unsafe, same invariants. (a) **postcard, not bincode**: bincode was removed from the workspace (RUSTSEC-2025-0141); postcard is the established serde wire format. (b) **`SOCK_SEQPACKET`, not `SOCK_STREAM` + length-prefix**: stream sockets associate `SCM_RIGHTS` with a byte position, not a frame — pipelined frames whose read boundaries drift from write boundaries can attach an fd to the wrong request. Message-boundary preservation makes one datagram == one frame == its fds, and `MSG_TRUNC` gives the `MAX_FRAME_BYTES` rejection for free. (c) **generic `Q_SETQUOTA`/`dqblk`, not XFS-specific `Q_XSETQLIM`/`fs_disk_quota`**: XFS wires the VFS quota ops, `libc` already ships the `dqblk` layout, and the generic path also covers ext4-with-`prjquota`. (d) **projid allocator starts at 1 after the startup orphan scan** instead of seeding from `xfs_quota -xc report`: the scan empties `staging/` before any connection exists, so a reused projid accounts from zero. The six previously-prose-only `r[builder.mountd.*]` rules got their col-0 definitions in design-overview §11 (not a new `components/builder.md` — that tree does not exist and is outside tracey scope). Still pending: `mountd-ds.yaml`, the eks-node `pquota` assert + `/var/rio` tmpfiles.
>
> **Reconciliation (2026-05-21, VM test).** The P0578-deferred mountd-protocol subtests landed as `nix/tests/scenarios/mountd.nix` (`vm-mountd`) — a single-VM scenario booting the real `rio-mountd` against an XFS-`prjquota` loopback, driven by `bin/spike_mountd_client.rs` (the builder-side stand-in until P0559's in-process client). Two deviations from the spike sketch: (a) **the perf criteria (vi `BackingOpen` p99 < 200 µs, vii `Promote` ≥ 1 GiB/s, x concurrent p99 < 1 ms) are printed as `PERF` lines, not gated** — the test runs under TCG on runners without `/dev/kvm`, where those numbers are off by 10-100× and a wall-clock gate is permanently red (ci-failure-patterns "Wall-clock gate under load": structural > retry > widen). (b) **the concurrency assertion is structural**: at least one `BackingOpen` reply must arrive before the in-flight `Promote`'s reply, which distinguishes spawn_blocking-concurrent from serialized independent of timing. The gid gate is tested at both layers (socket-file DAC for a non-group uid, `SO_PEERCRED` for root, which bypasses DAC). `vm-mountd` is excluded from the coverage-mode VM matrix — it has no `LLVM_PROFILE_FILE`/`collectCoverage` wiring (it does not use the fixture machinery that injects them); mountd VM coverage lands with `vm-castore-e2e` (P0560§B).
>
> **Reconciliation (2026-05-28, peer-credential gate dropped).** The `SO_PEERCRED.gid` check
> described in the file table below (and a briefly-added `/proc/<pid>/gid_map` user-namespace
> translation on top of it) was removed during the §A/§B review cycle. Three reasons: from the
> mountd DaemonSet's pid namespace the peer pid is not visible (`SO_PEERCRED.pid` = 0 without
> `hostPID`), so the translated check fail-closed-rejected every legitimate executor; under user
> namespaces the translation is self-asserted (any process can map the allowed gid onto its own
> egid); and the check duplicates what the socket's `0660 root:<allowed-gid>` mode already
> enforces at `connect(2)`. The access-control contract is now exposure + DAC only: the
> controller mounts `/run/rio-mountd` only into executor pods, the build sandbox never sees the
> socket path, and the socket file mode is the gate (`--allowed-gid` sets socket group
> ownership, nothing else). mountd still logs peer uid/gid and keeps the one-connection-per-uid
> binding (`builder.mountd.uid-bound`). Read the file table's "rejects connections where
> `SO_PEERCRED.gid` != rio-builder" prose and the note above ("SO_PEERCRED for root") with that
> in mind; `vm-mountd`'s gid-gate subtest now covers the DAC half only.

The unprivileged builder cannot (a) open `/dev/fuse`, (b) call `FUSE_DEV_IOC_BACKING_OPEN`/`_CLOSE` (init-ns `CAP_SYS_ADMIN` — [`backing.c:91-93,147-149`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c)), or (c) write the shared cache (integrity boundary). One DaemonSet per node with `CAP_SYS_ADMIN` brokers all three. **No overlay mount, no upcall relay** — builder does FUSE-serve + overlay itself.

**Concurrency:** tokio multi-thread runtime; one async task per accepted UDS connection. Within a conn, requests are length-prefix-framed and pipelined — `BackingOpen`/`BackingClose` are answered inline (sub-ms). `Promote` and `PromoteChunks` each acquire a process-wide `Semaphore(num_cpus)` permit, then run their copy+hash loop on `tokio::task::spawn_blocking` so neither blocks the conn's `BackingOpen` traffic — `PromoteChunks` is ≤16 MiB of disk I/O + hashing per batch, not sub-ms. Replies correlate via `seq` for both. `// r[impl builder.mountd.concurrency]`

| File | Change |
|---|---|
| `rio-builder/src/castore_fuse/mountd_proto.rs` | new — UDS wire types shared with P0559. `struct Frame { seq: u32, body: Req\|Resp }` (every reply echoes `seq`; out-of-order replies from `spawn_blocking` `Promote` are correlatable). `enum Req { Mount{build_id}, BackingOpen{/* fd via cmsg */}, BackingClose{id: u32}, PromoteChunks{chunk_digests: Vec<[u8;32]>}, Promote{digest: [u8;32]} }`. `enum Resp { Mounted{/* fuse_fd via cmsg */ staging_quota_bytes: u64}, BackingId(u32), Promoted, Err(ErrKind) }`. **fds travel in the frame's `SCM_RIGHTS` cmsg, never in the bincode body** — each frame carries an `ancillary_fds: u8` count. `enum ErrKind { Retryable(String), DigestMismatch, NotRegular, TooLarge, RaceTimeout, BadBuildId, AlreadyMounted, DuplicateBuildId, BatchTooLarge }` — builder maps `DigestMismatch`/`NotRegular`/`TooLarge`/`BadBuildId`/`AlreadyMounted`/`DuplicateBuildId`/`BatchTooLarge` to **build-failure** (not infra-retry; re-fetch would loop), `RaceTimeout` to `Retryable`. Length-prefix bincode framing with `MAX_FRAME_BYTES = 4096` enforced before deserialize (largest legitimate frame is `PromoteChunks` ≤64×32+overhead ≈ 2.1 KiB); reject oversize with `Retryable("oversize frame")`. Any `Req` other than `Mount{}` before `conn.kept.is_some()` → `Retryable("not mounted")`. Client holds `HashMap<u32, oneshot::Sender<Resp>>`. |
| `rio-builder/src/bin/rio-mountd.rs` | new — listens on `/run/rio-mountd.sock` (mode 0660, group `rio-builder`); rejects connections where `SO_PEERCRED.gid != rio-builder`. Per accepted connection, record `conn.peer_uid = SO_PEERCRED.uid` and reject any subsequent connection with the same `peer_uid` while one is live (k8s userns gives each pod a distinct host-uid range, so this binds one connection per build — `r[builder.mountd.uid-bound]`). Owns `/var/rio/cache/` (0755, files 0444). At start-up: `castore_base = open("/var/rio/castore", O_DIRECTORY|O_NOFOLLOW)`, `staging_base = open("/var/rio/staging", O_DIRECTORY|O_NOFOLLOW)` — all per-build path construction is `openat(base, build_id, …)`, never string concat. Process-wide `live_build_ids: Mutex<HashSet<String>>` and `next_projid: AtomicU32` (seeded at startup from `xfs_quota -xc 'report -p' staging_dev` max projid + 1, or 1 if none). **`Mount{build_id}`** → reject `Err(AlreadyMounted)` if `conn.kept.is_some()` (one Mount per connection lifetime — `r[builder.mountd.one-mount]`); reject `Err(BadBuildId)` unless `build_id` matches `^[A-Za-z0-9_-]{1,64}$` (no `/`, no `..`; `r[builder.mountd.build-id-validated]`); reject `Err(DuplicateBuildId)` unless `live_build_ids.insert(build_id.clone())` (a different uid's connection already owns this `build_id` — `r[builder.mountd.build-id-unique]`; uid-bound alone does not prevent a sandbox-escaped build with its own uid from `Mount{victim_id}`); `fuse_fd = open("/dev/fuse", O_RDWR)`; **`kept = dup(fuse_fd)`** stored in conn state; `mkdirat(castore_base, build_id, 0755)` then `mount("none", "/var/rio/castore/{build_id}", "fuse.rio-castore", MS_NODEV\|MS_NOSUID, "fd=<fuse_fd>,rootmode=40555,user_id=<peer_uid>,group_id=<peer_gid>,allow_other,default_permissions")`; `mkdirat(staging_base, build_id, 0700)` chown `conn.peer_uid` (mode 0700 — only this build's uid can read/write its staging); **set XFS project quota**: `conn.projid = next_projid.fetch_add(1)` (mountd-assigned monotonic, never derived from adversary-chosen `build_id` — a `hash32(build_id)` projid would let an attacker brute-force a 32-bit collision against an enumerated victim and share its quota); `ioctl(staging_dirfd, FS_IOC_FSSETXATTR, {fsx_projid=conn.projid, fsx_xflags|=PROJINHERIT})` then `quotactl(Q_XSETQLIM, staging_dev, conn.projid, {d_blk_hardlimit=staging_quota_bytes/512})` (`r[builder.mountd.staging-quota]` — kernel enforces `ENOSPC` on builder writes; mountd does not track bytes); `conn.staging_dirfd = openat(staging_base, build_id, O_DIRECTORY|O_NOFOLLOW)`, `conn.staging_chunks_dirfd = openat(staging_dirfd, "chunks", O_DIRECTORY|O_NOFOLLOW)` after `mkdirat`; reply `[fuse_fd]` via SCM_RIGHTS; close sent copy. **`BackingOpen{fd}`** (fd via SCM_RIGHTS) → `ioctl(kept, FUSE_DEV_IOC_BACKING_OPEN, &fuse_backing_map{fd, flags:0}) → backing_id`; reply `backing_id` (mountd does not inspect the fd; the ioctl rejects depth>0 backing and `backing_id` is conn-scoped). **`BackingClose{id}`** → ioctl. **`PromoteChunks{chunk_digests}`** → reject `Err(BatchTooLarge)` if `chunk_digests.len() > 64` (server-enforced; the ≤64 doc bound is a contract, not a hint); on `spawn_blocking` + `Semaphore(num_cpus)` (same as `Promote`; reply via `seq`). For each: `openat(conn.staging_chunks_dirfd, hex, O_RDONLY\|O_NOFOLLOW)`; reject `!S_ISREG` or `st_size > FASTCDC_MAX_BYTES` (`rio-store/src/chunker.rs` constant — must match); **read at most `st_size` bytes** (`r[builder.mountd.promote-bounded-copy]`) + verify `blake3 == chunk_digest`; write `/var/rio/chunks/ab/{hex}.tmp` 0444; rename (on `EEXIST` → already promoted, fine); unlink staging. One `Promoted` reply per batch. `// r[impl builder.fs.node-chunk-cache]` **`Promote{digest}`** → `src = openat(conn.staging_dirfd, hex, O_RDONLY\|O_NOFOLLOW)`; `fstat(src)` — reject `!S_ISREG` or `st_size > RIO_MOUNTD_MAX_PROMOTE_BYTES` (default 4 GiB; `Err(TooLarge)`). Create `cache/ab/{hex}.promoting` `O_EXCL\|O_WRONLY` 0444 wrapped in a `PromotingGuard` whose `Drop` unlinks unless defused — every error/panic path leaves no leaked `.promoting` (a leak makes every future `Promote` of that digest hit `RaceTimeout` until restart). On `O_EXCL` `EEXIST`, stat `cache/ab/{hex}`: exists → reply `Promoted`; else inotify-wait ≤2 s then re-stat; else stat `.promoting` mtime — if older than `MAX_PROMOTE_BYTES / MIN_PROMOTE_THROUGHPUT` (i.e. a stale leak from a prior panic), unlink and retry; else `Err(RaceTimeout)`. Copy loop: `read(64 KiB)` with per-call `timeout(5s)` → `hasher.update` → `write`, **tracking `bytes_copied`; stop and `Err(DigestMismatch)` if `bytes_copied + n > st_size`** (the builder owns the source inode and can append concurrently — `r[builder.mountd.promote-bounded-copy]`); on `read()==0` with `bytes_copied < st_size` (concurrent truncation) → `Err(DigestMismatch)` (P0578 subtest xiii covers append; a truncation variant is not separately tested but the `bytes_copied != st_size` check covers it). Verify `hasher.finalize() == digest` else `Err(DigestMismatch)` + `promote_reject_total{reason="mismatch"}.inc()`. `guard.defuse(); rename .promoting → final`; `unlinkat(staging_dirfd, hex)`; reply `Promoted`. **On UDS conn-drop:** `umount2(castore_mnt, MNT_DETACH)` + `rmdir(castore_mnt)` + `rm -rf staging/{build_id}` + `quotactl(Q_XSETQLIM, staging_dev, conn.projid, {0})` (release quota slot) + `live_build_ids.remove(build_id)` + `close(kept)`. **Start-up:** scan `/var/rio/{castore,staging}/*` and `/var/rio/{cache,chunks}/**/*.{promoting,tmp}` for orphans; reap. `// r[impl builder.mountd.{fuse-handoff,backing-broker,promote-verified,promote-bounded-copy,orphan-scan,one-mount,build-id-unique,staging-quota}]` |
| `infra/helm/rio-build/templates/mountd-ds.yaml` | new — DaemonSet, hostPath `/run/rio-mountd.sock` + `/var/rio/{cache,chunks,staging,castore}` + `/dev/fuse`. `securityContext: {privileged: false, capabilities.add: [SYS_ADMIN]}`, `runAsUser: 0`, seccomp `RuntimeDefault`. Builder pods get `fsGroup: rio-builder` for socket access. nodeSelector: builder/fetcher nodepools. |
| `docs/src/components/builder.md` | `r[builder.mountd.{build-id-validated,build-id-unique,uid-bound,staging-quota,promote-bounded-copy,one-mount}]` col-0 spec text + `r[builder.fs.listxattr-size-branch]` col-0 spec text. (`fuse-handoff`/`backing-broker`/`concurrency` already col-0-defined in design-overview §11; `promote-verified`/`orphan-scan` already col-0-defined in ADR §2.5/§2.6 — do NOT duplicate.) |
| `nix/nixos-node/eks-node.nix` | `/var/rio/staging` MUST be on XFS with `pquota` mount option for project-quota enforcement; assert at module eval. |

**Exit:** `/nixbuild --checks` green; exercised end-to-end by P0560§B.

### P0571 — mountd-owned cache LRU sweep + staging-dir lifecycle + cache-hit metrics
**Crate:** `rio-builder, infra` · **Deps:** P0559, P0567 · **Complexity:** LOW · **Status:** DONE 2026-05-25 (the sweep + dead-staging backstop; the builder-pod hostPath mounts move to P0560)

`r[builder.fs.shared-backing-cache]` + `r[builder.fs.node-chunk-cache]`: the **backing cache** (`/var/rio/cache/ab/<file_digest>`) and **chunk cache** (`/var/rio/chunks/ab/<chunk_digest>`) are mountd-owned, builder-readonly; builders stage to per-build `/var/rio/staging/{build_id}/` and `Promote`/`PromoteChunks` (P0567). Cross-build dedup for >threshold files is chunk-granular via the chunk cache.

| File | Change |
|---|---|
| `rio-builder/src/bin/rio-mountd.rs` | mountd owns `/var/rio/{cache,chunks}/` (P0567); this plan adds the LRU sweep: periodic `statvfs` on each of `/var/rio/{cache,chunks,staging}` (may be separate partitions) — if `min(free%) < 10%`, atime-ordered `readdir` + `unlink` over `chunks/` first (intermediate, regenerable), then `cache/` (passthrough targets), until `min(free%) > 20%`. **Sweep also covers `/var/rio/staging/*`** (orphaned staging from crashed builds). **Cache, chunks, staging dirs MUST be on a non-stacking fs** (ext4/xfs; `r[builder.fs.passthrough-stack-depth]`). The disk-ownership freedom may be used to put `/var/rio/chunks/` on a dedicated partition to isolate IOPS from the build's overlay-upper. `// r[impl builder.fs.node-digest-cache]` |
| `rio-builder/src/castore_fuse/open.rs` | `rio_builder_objects_cache_{hit_total,bytes}` metrics. |
| `infra/helm/rio-build/templates/builder-sts.yaml` | hostPath `/var/rio/cache` and `/var/rio/chunks` mounted **RO**; `/var/rio/staging` and `/var/rio/castore` RW |
| `nix/nixos-node/eks-node.nix` | `systemd.tmpfiles.rules = ["d /var/rio/cache 0755 root root -" "d /var/rio/chunks 0755 root root -" "d /var/rio/staging 0755 root root -" "d /var/rio/castore 0755 root root -"]` |

**FSx-backed cluster-wide cache rejected** — violates builder air-gap: a shared writable FS across untrusted builders is a cache-poisoning + lateral-movement surface. The same logic motivates mountd-owned per-node cache.

**Exit:** `/nixbuild --checks` green.

> **Reconciliation (2026-05-25).** Landed as `castore_fuse/sweep.rs` (a sibling module spawned by `mountd::run`, not inline in `bin/rio-mountd.rs` — the binary stays a clap shim). Three of the four file-table rows dissolved on contact with the code that already exists. (a) **The open.rs cache-hit metrics are P0559's `open_case_total{case="hit"}`** — `rio_builder_objects_cache_{hit_total,bytes}` appears nowhere in the §14 metrics table, and a hit-*bytes* counter is unmeasurable from a passthrough handler anyway: on a hit the kernel reads the backing file directly and the handler never sees another upcall. (b) **`builder-sts.yaml` does not exist** — builder pods come from the controller's pod template (`reconcilers/pool/pod.rs`), which already hostPath-mounts `/var/rio/cache` for the *old* FUSE; swapping those mounts to the cache-RO/staging-RW split is part of P0560's old-FUSE deletion, not something to do while both FUSEs nominally share the path. (c) **The eks-node tmpfiles rules landed with P0567** (`/var/rio/{castore,staging,cache,chunks}` already exist there). (d) The sweep's "staging lifecycle" is a *backstop*: connection teardown remains the primary cleanup; the sweep only retries `remove_dir_all` for build_ids no longer in `live_build_ids` (Mount inserts the id before mkdir, so absent-from-set ⇒ dead). Additions: `rio_mountd_cache_evicted_bytes_total{dir}` joined the §14 table — `cache_free_bytes` alone cannot distinguish "healthy" from "sweeping every tick because the SSD is too small".

### P0560 — [ATOMIC] castore-FUSE lower cutover: mount + DELETE old-FUSE + fixture kernel + VM test  ★ HARD CUTOVER
**Crate:** `rio-builder, nix` · **Deps:** P0576, P0557, P0559, P0567, P0571, P0575, P0589 · **Complexity:** HIGH (two-part atomic) · **Status: DONE 2026-05-28**

**One worktree, one PR, one `/nixbuild --checks` gate.** §A alone breaks every existing VM test (fixtures lack `kernel.nix`; existing scenarios assert old-FUSE metrics); §B alone has nothing to test.

#### §A — `rio-builder`: mount.rs + overlay castore-FUSE lower + delete old-FUSE
**Complexity:** MED (add) + LOW (delete)
| File | Change |
|---|---|
| `rio-builder/src/castore_fuse/mount.rs` | `mount_castore_background(mount_point, castore_mnt, roots: &[(StorePath, RootNode)], uds, clients, rt) -> CastoreMount` — (1) `tree::build_tree(store, roots)` (prefetch DAG via one multi-root `GetDirectory(recursive=true)` call, ADR §2.2); (2) connect `rio-mountd` UDS, send `Mount{build_id}`, recv `[fuse_fd]` via SCM_RIGHTS; (3) **spawn `castore_fuse::serve(fuse_fd, tree, clients)` and wait for ready — MUST be serving before step 4** (overlayfs probes lowers at `mount(2)`; an unserved FUSE deadlocks the mounter — P0541 ordering gotcha); (4) `mount("overlay", mount_point, "overlay", 0, "userxattr,upperdir=<ssd>/nix/store,workdir=<ssd>/work,lowerdir={castore_mnt}")` in builder userns. `Drop`: `umount2(overlay, MNT_DETACH)` → close UDS (mountd umounts FUSE on conn-drop) → abort FUSE task (any blocked `open()` wakes `ENOTCONN`, interruptible — no D-state). Hard-fail with actionable error if UDS connect fails (`"rio-mountd not running on this node — is the DaemonSet (P0567) deployed?"`) or any input's `GetDirectory` stream is empty (`"store has not indexed {root_digest} — is P0557 deployed?"`). `// r[impl builder.fs.{castore-stack,fd-handoff-ordering}]` |
| `rio-builder/src/executor/inputs.rs` | unconditionally: closure roots from `WorkAssignment.input_roots` (P0588) → `mount_castore_background`. Delete the `cache.register_inputs(...)` JIT block. |
| `rio-builder/src/executor/mod.rs` | **PORT** `is_input_materialization_failure`: recognise `EIO` from castore-FUSE `open()` (fetch failure or integrity fail) + breaker-tripped state as infra-retry, not derivation-failure. `// r[impl builder.result.input-eio-is-infra]` |
| `rio-builder/src/overlay.rs` (~214) | `OverlayMount::new(lower: CastoreMount)` — single concrete type. `// r[impl builder.overlay.castore-lower]` |
| `rio-builder/src/main.rs` | drop `mount_fuse_background()` call site; drop `fuse_cache` construction |

**Deletion inventory** (cutover earns back code):

| Path / symbol | Why it can go | ~LoC |
|---|---|---|
| `rio-builder/src/fuse/ops.rs` | old-FUSE `Filesystem` impl — castore-FUSE (P0559) serves the DAG with content-addressed inodes, not path-granular NAR materialization | 786 |
| `rio-builder/src/fuse/cache.rs` | `Cache`, `JitClass`, `known_inputs`/`register_inputs` — the in-memory DAG IS the allowlist | 1356 |
| `rio-builder/src/fuse/mod.rs` (most) | `mount_fuse_background`, `FuseMount`, `NixStoreFs`. **`ensure_fusectl_mounted` and Drop fusectl-abort are KEPT** (moved to `castore_fuse/mod.rs` — same I-165 abort discipline) | ~450 |
| `rio-builder/src/fuse/{inode.rs,attr.rs}` | inode bookkeeping + attr/lookup ops — castore_fuse/tree.rs replaces with content-addressed inos | 254+91 |
| `rio-builder/src/fuse/circuit.rs` | **PORTED** to `castore_fuse/circuit.rs` (P0559) | (moved) |
| `rio-builder/src/fuse/read.rs` | passthrough fd registration — page cache via overlay | (whole file) |
| `rio-builder/src/fuse/fetch/` old-FUSE wrappers | `ensure_cached`, `prefetch_path_blocking` — P0550 hoisted keepers | ~1700 residual |
| `rio-builder/src/executor/mod.rs` `RIO_BUILDER_JIT_FETCH` block | I-043 escape hatch — old-FUSE-specific | ~40 |
| spec markers | `r[builder.fuse.{jit-lookup,jit-register,lookup-caches+2,fetch-chunk-fanout,fetch-bounded-memory}]`. `r[builder.result.input-enoent-is-infra+2]` REWORDED → `input-eio-is-infra`. | docs |
| `infra/helm/rio-build/templates/karpenter.yaml` `rio-builder-{fuse,kvm}` NodeOverlays | **DROPPED** — both existed to advertise `smarter-devices/*` capacity. fuse: rio-mountd fd-passes. kvm: hostPath + `nodeSelector{rio.build/kvm}` (the metal NodePool already labels+taints; capacity is unbounded so no overlay needed). | helm |
| `values.yaml` `fuseCacheSize` + `infra/helm/crds/builderpools.rio.build.yaml:152` + `templates/builderpool.yaml:24` + `values/vmtest-full.yaml:151` + `rio-controller` `BuilderPoolSpec` field + `fixtures.rs:173`/`apply_tests.rs:404`/`disruption_tests.rs:70` | digest-cache dir is node-level hostPath (P0571), not per-pool | helm+CRD+tests |
| `templates/networkpolicy.yaml:67` `builderS3Cidr` egress carve-out | presigned-URL fetch path gone; builder is pure rio-store gRPC | helm |

**Net:** ~**−4 600 LoC**. The `rio-builder/src/fuse/` directory reduces to nothing; `rio-builder/src/castore_fuse/` is ~800 LoC total.

#### §B — `nix`: fixture kernel cutover + vm:castore-e2e
**Complexity:** HIGH

| File | Change |
|---|---|
| `nix/tests/fixtures/k3s-prod-parity.nix` | unconditionally `imports = [ ../../nixos-node/kernel.nix ]`; deploy `rio-mountd` DS in-cluster; hostPath `/var/rio/{castore,cache,chunks,staging}` |
| `nix/tests/scenarios/castore-e2e.nix` | fixture `{storeReplicas=1;}`. `cold-read`: build drv that `dd bs=4k count=1` from a 100 MB input → assert `castore_fuse_open_seconds_count > 0` AND `dd` output correct AND streaming mode hit (>threshold). `warm-read`: second `dd` same file → `open_seconds_count` unchanged AND **`rio_builder_castore_fuse_upcalls_total{op="read"}` unchanged** (passthrough — no read upcalls). `passthrough-small`: `dd` a 1 MiB input twice → both opens reply passthrough; assert `upcalls_total{op="read"} == 0` across both. `cross-build-dedup`: two drvs with one shared input file → second build's `fetch_bytes_total{hit="node_ssd"} > 0`. `inode-dedup`: two store paths sharing one file by content → `stat -c %i` returns the same inode for both paths; only one `open()` upcall. `eio-on-fetch-fail`: stop rio-store mid-open → opener sees `EIO` (not hang) within `jit_fetch_timeout` + `is_input_materialization_failure` classifies as infra-retry. `integrity-fail`: corrupt one chunk in the store backend → opener sees `EIO` + `integrity_fail_total == 1`. `stat-dcache-absorbed`: `find /nix/store -type f -printf '%s\n'` once → `rio_builder_castore_fuse_upcalls_total{op="lookup"} == N`; second `find` → unchanged (`Duration::MAX` ttl). `shutil-copy2`: `python3 -c 'import shutil; shutil.copy2("<input>", "<upper>")'` → succeeds (exercises `listxattr` size>0 path; `r[builder.fs.listxattr-size-branch]`). `cross-build-dedup-streaming`: launch two builds **concurrently** sharing one >threshold input → assert build-B's `chunk_source_total{src="remote"}` × `FASTCDC_MAX` < input size (most chunks came from `/var/rio/chunks/`). `mountd-restart`: kill mountd mid-build, assert orphan-scan reaps `castore/`+`staging/` on restart and next build succeeds. `cache-readonly`: from inside the build sandbox, `open("/var/rio/cache/ab/test", O_WRONLY\|O_CREAT)` → `EACCES`. |
| `nix/tests/scenarios/{lifecycle,protocol,gc,...}.nix` | **sweep:** delete every old-FUSE-specific assertion (`fuse_cache_hits`, `/var/rio/fuse-store`). **Drop all `smarter-devices/*` from worker pod fixtures** — fuse via rio-mountd fd-pass, kvm via hostPath. |
| `nix/tests/default.nix` | `# r[verify builder.fs.{castore-stack,castore-dag-source,castore-inode-digest,castore-cache-config,fd-handoff-ordering,digest-fuse-open,shared-backing-cache,file-digest-integrity,node-digest-cache,streaming-open-threshold}]` `# r[verify builder.overlay.castore-lower]` `# r[verify builder.result.input-eio-is-infra]` `# r[verify builder.mountd.fuse-handoff]` `# r[verify obs.metric.castore-fuse]` at `subtests=[...]`; `cp -a` of an input file succeeds (xattr ops). Spike harness `nix/tests/{scenarios/composefs-spike-{stream,priv}.nix, lib/spike_stage.py, lib/chromium-tree.tsv.zst}` kept as regression guards (stream + priv subtests apply to §2; the core/scale spikes are §3-only and may be dropped); `timeout=1800` |

**Exit (whole P0560):** `nix build .#checks.x86_64-linux.vm-castore-e2e` green; full `/nixbuild --checks` green with castore-FUSE as the only lower.

> **Reconciliation (2026-05-28, §B exercise — tenancy gap).** Wiring `vm-castore-e2e` and
> `vm-protocol-warm-standalone` against the cutover surfaced a gap this plan never addressed:
> the castore read surface is fail-closed tenant-scoped (P0573, by design — "DirectoryService
> requires a tenant: send a JWT or an HMAC assignment token"), but the standalone fixtures run
> single-tenant/anonymous, and nothing in the production chain writes `path_tenants` rows for
> client-uploaded sources/.drvs — only built outputs get rows (scheduler completion-time
> upsert; legacy `PutPath` ignores the JWT tenant for the junction, `PutPathChunked` only
> honours a JWT the builder never has). So after the cutover, builds are impossible in
> single-tenant mode and, even fully attributed, the builder cannot see client-pushed inputs.
> Bridged at the FIXTURE level, matrix-wide: `nix/tests/fixtures/standalone.nix` gained a
> `defaultTenant` option (tenant row + HMAC keys + authorized_keys-comment attribution + a
> shared `tenantStopgapSeedSql` narinfo-INSERT trigger in `nix/tests/common.nix` attributing
> registered paths to the stopgap tenant), the toxiproxy fixture now wraps standalone, the k3s
> fixture seeds the same SQL via `kubectl exec … psql` and carries the tenant in its key
> comments, and every build-running scenario (standalone and k3s) sets `defaultTenant =
> "vmtest"` — scenarios that submit via grpcurl or roll their own SSH keys carry the tenant
> explicitly. `vm-castore-e2e`'s warm-build / store-outage subtests were corrected to match the
> one-shot builder lifecycle. One piece was a PRODUCT gap, fixed here rather than in fixtures:
> the helm chart never mounted the assignment-token HMAC key into the scheduler (signer) or
> store (verifier), so k8s deployments could not produce tenant-claimed assignment tokens at
> all — an `assignmentHmac` mount family (secret `rio-hmac`, `RIO_HMAC_KEY_PATH`) was added to
> both. Product-level resolution of the rest is **Phase 8 (P0590–P0593): explicit tenancy, no
> anonymous access** — sequenced strictly AFTER this cutover merges; P0593 then deletes the
> fixture stopgap.
>
> **Reconciliation (2026-05-28, §B subtest set).** `vm-castore-e2e` landed with seven subtests
> (mountd-running, cold-build, streaming-large-input, warm-build, store-outage-infra-retry,
> mountd-restart, teardown-clean) instead of the sketch's fifteen. The differences: the
> mid-open EIO/integrity cases (`eio-on-fetch-fail`, `integrity-fail`) are represented by
> store-outage-infra-retry, which asserts the designed behaviour at a build boundary (worker
> observes the outage via connect-retry, the build is held back, never poisoned) — mid-open
> failure classification stays at unit level; `cache-readonly` and the staging-quota cases are
> already verified by `vm-mountd`; `shutil-copy2`/listxattr is a `castore_fuse` unit test
> (`r[verify builder.fuse.listxattr-empty]`); inode-dedup, passthrough-small and
> stat-dcache-absorbed are covered by the `castore_fuse` unit suite; cross-build chunk dedup is
> asserted structurally in warm-build (chunk-cache deltas) and in `vm-put-path-chunked`. The
> planned `<500 ms`/`<50 ms` wall-clock latency gates were not carried over (wall-clock gates
> flake under builder contention — structural assertions only).

### P0562 — Post-cutover audit  ★ CUTOVER GATE (U1)
**Crate:** `nix` · **Deps:** P0560 · **Complexity:** LOW · **Status: DONE 2026-05-28**

| Check | How |
|---|---|
| No old-FUSE markers remain | `tracey query rule builder.fuse.*` returns empty |
| No old-FUSE / device-plugin strings in code/helm | `grep -rn 'fuse_cache\|/var/rio/fuse-store\|fuseCacheSize\|NixStoreFs\|smarter-devices\|smarter-device-manager\|rio-builder-fuse\|fuseMaxDevices\|kvmMaxDevices' rio-*/ infra/ nix/` returns empty |
| No stray cachefiles/boot-blob strings | `grep -rn 'cachefiles\|CACHEFILES\|boot_blob\|boot_size' rio-*/ infra/ nix/ docs/src/components/` returns empty |
| Parity | full `/nixbuild --checks` re-run; `# r[verify builder.fs.parity]` on `lifecycle` |

**Exit:** all four checks pass; `/nixbuild --checks` green.

> **Reconciliation (2026-05-28).** Four deviations from the plan-time check wording.
> (1) `tracey query rule builder.fuse.*` is NOT empty by design: the JIT-only rules
> (`jit-lookup`, `jit-register`, `lookup-caches`, `cache-ephemeral-memory`, `warmgate.filter`,
> `passthrough`, `input-enoent-is-infra`) are gone (P0560's spec sync), but six rules stay
> under `builder.fuse.*` because they bind on the castore-FUSE fetch layer
> (`canonical-metadata+2`, `listxattr-empty`, `circuit-breaker+3`, `retry-jitter+2`,
> `fetch-progress-timeout+2`, `fetch-bounded-memory`). The audit re-pointed the orphaned
> `canonical-metadata+2` impl at `castore_fuse/tree.rs::attr` and removed the last code
> references to the deleted JIT path (the i115 QA scenario, which scraped the no-longer-emitted
> `rio_builder_fuse_jit_lookup_total`, plus its now-unused `scrape_builder` helper; the
> `ensure_cached` mention in the breaker module doc). Still pre-cutover and deferred to a
> follow-up spec pass: the `fetch-bounded-memory`/`fetch-progress-timeout+2` rule text and impl
> sites (the orphaned `get_path_nar_to_file`/`collect_nar_stream_to_writer` spool helpers in
> rio-proto), the JIT-era phrasing inside `circuit-breaker+3` ("`ensure_cached` AND
> `prefetch_path_blocking`") and `retry-jitter+2` ("The JIT fetch retry loop"), and builder.typ's
> "FUSE Store"/"FUSE Cache" prose + figure, which still describe the deleted JIT FUSE.
> (2) The string sweep is clean except one intentional hit: rio-controller's
> `no_per_pod_fuse_cache_volume` regression test, which exists to keep the deleted per-pod
> volume deleted. `docs/src/components/` no longer exists (component specs are
> `docs/spec/components/*.typ`); the remaining cachefiles/boot_blob strings live only in the
> retained EROFS-alternative analysis (`lazy-store.typ`) and the ADR decision docs.
> (3) `builder.fs.parity` is verified at the lifecycle gc-split's `refs-end-to-end` entry (the
> consumer build reads its dep through the castore lower and asserts the dep lands in PG
> narinfo."references" end-to-end) rather than a dedicated parity subtest; the matrix-wide
> `/nixbuild --checks` re-run is the merge gate of this branch.
> (4) The §"tracey marker inventory" obligation `tracey query uncovered | grep -E
> 'castore|tiered|index|digest-fuse|compat'` now returns exactly one rule: `obs.metric.compat`,
> owned by P0566 (compat writer, UNIMPL). `store.index.putpath-eager` and
> `obs.metric.chunk-backend-tiered` were implemented but unannotated — impl markers added at
> `rio-store/src/metadata/mod.rs` (the complete-transaction castore-index write) and
> `rio-store/src/lib.rs` (the tiered-backend metric registration block).

---

## Phase 6 — Observability + finalize

### P0563 — metrics + dashboard + alerts
**Crate:** `infra` · **Deps:** P0544, P0548, P0559 · **Complexity:** LOW
| File | Change |
|---|---|
| `infra/helm/rio-build/dashboards/castore-fuse.json` | panels: `rio_builder_castore_fuse_open_seconds` p50/p99, `rio_builder_castore_fuse_upcalls_total{op="lookup"}` rate (cold-metadata pressure), `fetch_bytes_total` rate by `hit` label, `objects_cache_bytes` per node, `integrity_fail_total`, `nar_index_compute_seconds` |
| `infra/helm/rio-build/templates/prometheusrule.yaml` | `RioBuilderDigestFuseStall`: `increase(open_seconds_count[2m]) == 0 AND increase(open_seconds_sum[2m]) > 0 for 60s` (opens started but none completed). `RioBuilderIntegrityFail`: `increase(integrity_fail_total[5m]) > 0`. `RioStoreNarIndexBacklog`: `nar_index_pending > 1000 for 10m`. |
| `xtask/src/regen/grafana.rs` | include dashboard |

**Exit:** `/nixbuild --checks` green; `xtask grafana` shows dashboard.

### P0564 — helm cleanup + mountd DS wiring + kernel-feature assertion
**Crate:** `infra` · **Deps:** P0554, P0560, P0567 · **Complexity:** LOW
| File | Change |
|---|---|
| `infra/helm/rio-build/templates/_helpers.tpl` | Unconditional helm assertion: `{{- if and .Values.karpenter.enabled (not (has "FUSE_PASSTHROUGH" .Values.karpenter.amiKernelFeatures)) }}{{ fail "AMI must be built with nix/nixos-node/kernel.nix (≥6.9, FUSE_PASSTHROUGH=y); run xtask ami push" }}{{- end }}`. |
| `infra/helm/rio-build/values.yaml` | delete `fuseCacheSize`, `builderS3Cidr`, **entire `devicePlugin.*` block** (`{fuse,kvm}MaxDevices`, `image`); add `mountd.{image}`, `objectsCache.{hostPath,lowWatermarkPct,highWatermarkPct}`; `karpenter.amiKernelFeatures: [...]` |
| `infra/helm/rio-build/templates/karpenter.yaml` | delete **both** `rio-builder-{fuse,kvm}` NodeOverlays (capacity advertisement for resources no pod requests). Metal NodePool keeps its `rio.build/kvm: "true"` label+taint — that is the nodeSelector target. |
| `infra/helm/rio-build/templates/device-plugin.yaml` + `nix/nixos-node/smarter-device-manager/` | **DELETED** — no consumers. fuse via fd-handoff; kvm via hostPath. |
| `infra/helm/rio-build/templates/NOTES.txt` | drop the smarter-devices section. |
| `infra/helm/rio-build/values/vmtest-full-nonpriv.yaml` | drop the device-plugin re-enable block (lines ~73-77). |
| `rio-controller/src/reconcilers/common/sts.rs` | builders/fetchers stay **`privileged: false`** unconditionally; mount `rio-mountd` UDS hostPath + `/var/rio/{castore,cache,chunks,staging}` hostPaths. **Drop all `resources.limits."smarter-devices/*"`.** kvm-pool pods: add `volumes: [{name: kvm, hostPath: {path: /dev/kvm, type: CharDevice}}]` + matching `volumeMounts` + `nodeSelector: {rio.build/kvm: "true"}` + toleration for the metal taint. |
| `rio-builder` nix.conf (or executor sandbox setup) | kvm-pool only: `extra-sandbox-paths = ["/dev/kvm"]`, `system-features += "kvm"`. Spike-verified (`vm-kvm-hostpath-spike`): sandboxed `requiredSystemFeatures=["kvm"]` build can `ioctl(KVM_GET_API_VERSION)`. |
| `flake.nix` helm-lint | drop `fuseCacheSize` parity assertion; add `amiKernelFeatures`-populated assertion |

**Exit:** `helm template` renders; `/nixbuild --checks` green.

### P0565 — Cutover runbooks
**Crate:** `docs` · **Deps:** P0555, P0562, P0564 · **Complexity:** LOW
| File | Change |
|---|---|
| `docs/src/runbooks/tiered-cache-cutover.md` | new — flip `store.chunkBackend.kind=tiered`; rollback `kind=s3` |
| `docs/src/runbooks/mountd-crash-loop.md` | symptom: `kube_pod_container_status_restarts_total{container="rio-mountd"}` rising + node's builds `EIO`. Action: `kubectl logs -p`; if persistent, cordon node, drain builders, capture `/var/rio/{cache,staging}` listing. |
| `docs/src/runbooks/promote-reject-nonzero.md` | symptom: `rio_mountd_promote_reject_total{reason="mismatch"} > 0` — a builder presented bytes that don't hash to the claimed digest (rio-store corruption or compromised builder). Action: identify `build_id` from mountd log; check `rio_store_narhash_mismatch_total`; if store clean, treat the builder pod as suspect — cordon node, preserve staging dir for forensics. |
| `docs/src/runbooks/single-node-builds-slow.md` | triage tree: (1) `open_mode_total{mode="passthrough"} == 0` → kernel/init negotiation failed, check `dmesg`; (2) `promote_inflight` pegged → Promote backlog, check `cache_free_bytes`; (3) `mountd_request_seconds{op="backing_open"}` p99 > 1 ms → mountd CPU-starved; (4) else → upstream (`fetch_bytes_total{hit="remote"}` rate vs `rio_store_*`). |
| `docs/src/runbooks/castore-fuse-cutover.md` | (1) ensure cache-tier flip done; (2) `xtask k8s eks down && up` from a P0562-green commit (greenfield — `nar_index` + `directories` populate from scratch via PutPath eager + indexer_loop); (3) `xtask stress chromium`; (4) compare `fetch_bytes_total{hit="remote"}` — expect ≥10× reduction vs whole-NAR baseline on builds that touch <10% of files; expect `objects_cache_hit_ratio` climbing on repeat builds; (5) rollback = `down && up` from pre-P0560 commit |

**Exit:** `/nixbuild --checks` green.

### P0575 — §2.8 mitigation (i): streaming `open()` for large files — **DONE**
**Crate:** `rio-builder` · **Deps:** P0559, P0570, P0571 · **Complexity:** LOW (~80 LoC) · **Priority: same tier as P0559**

**Unconditional** — top1000.csv shows all 1000 largest nixpkgs files >64 MiB (248 `.so`/`.a`, max 1.88 GiB); access-probe `42aa81b2` shows real consumers touch 0.3-33% (bimodal head+tail or scattered); spike `15a9db79` proves the mechanism works.

> **Reconciliation (2026-05-25).** Landed as `castore_fuse/stream.rs` (the fill machinery + its tests) with `open.rs` only dispatching case (c), with these deviations. (a) **Windowed batching, not a per-chunk walk**: chunks are processed in batches of 32 (the first batch is a single chunk so the `open()` reply gate is one chunk RTT away); each batch probes the node chunk cache, sends its misses as one `GetChunksRequest` frame on a per-fill bidi stream, writes the batch in file order, then stages + `PromoteChunks`s the misses. The plan's sequential open-local-else-fetch per chunk would serialize one RTT per chunk (~4 s of pure RTT for a 1 GiB file). (b) **`Mutex`+`Condvar`, not `watch`/`select!`/`DashMap`**: `read()` runs on fuser's thread pool, not in tokio, so the high-water mark and the first-chunk barrier are sync primitives; the per-digest map is a plain `Mutex<HashMap>` like the rest of the module. (c) **No `flock`**: the in-process `active_fills` map is the singleflight (same as P0559's whole-file path); `O_EXCL` + unlink-retry handles a crashed predecessor's orphan `.partial`. (d) **No client-side staging-quota eviction**: mountd unlinks each staged chunk after promoting it, so `staging/chunks/` is self-cleaning on the happy path and the XFS project quota is the backstop. (e) **No `config.rs` field yet** — same as P0559(c): `OpenerConfig.stream_threshold` is the plumbing point until P0560 maps `Config → OpenerConfig`. (f) **Inline-manifest fallback**: `StatBlob` → `FailedPrecondition` (no chunk list) streams a whole-file `ReadBlob` through the same fill/high-water machinery instead of failing. (g) `chunk_source_total{src}` (deferred here from P0559) is **not added** — `fetch_bytes_total{hit="node_ssd"|"remote"}` already carries the chunk-source split as a byte ratio. (h) A `Promote` failure after a verified fill does not fail the open — readers are already served from the staged copy; only the next open's cache hit is lost. (i) The `<50 ms` first-chunk latency assertion became a structural one (the open gate provably opens while a tail read is still blocked) — wall-clock gates flake under builder CPU contention; the real-network latency check is `vm-castore-e2e cold-read` (P0560§B).

| File | Change |
|---|---|
| `rio-builder/src/castore_fuse/open.rs` | **The during-fill mode** for P0559's case (c) — `size > STREAM_THRESHOLD` on cache miss. `open()` spawns fill task, returns `FOPEN_KEEP_CACHE` after the **first chunk** lands. **Chunk source (per chunk i):** `open("/var/rio/chunks/ab/<chunk_hex>", O_RDONLY)` — success → write into `.partial` at offset; `ENOENT` → `GetChunks`, verify `blake3==chunk_digest`, write into `.partial` at offset **and** into `staging/chunks/<chunk_hex>`, append digest to a per-fill `Vec<[u8;32]>`. **Slice first/last** per `StatBlobResponse.{first_chunk_skip, last_chunk_take}`: chunk[0] writes `bytes[first_chunk_skip..]` at `.partial` offset 0; chunk[last] writes `bytes[..last_chunk_take]`; staging always writes the whole chunk (it's content-addressed). Without this, a `file_digest` resolved via a legacy-`PutPath` manifest writes NAR framing/adjacent-file bytes at offset 0 and the whole-file blake3 fails. Every 32 chunks or at EOF: `PromoteChunks{batch}` (await reply, but assembly continues from own staging — `PromoteChunks` is purely for *other* builds; this build never reads `/var/rio/chunks/` for chunks it just fetched). **Staging quota**: track `staging_bytes`; if > `Mounted.staging_quota_bytes`, evict oldest `staging/chunks/*` (re-readable from `/var/rio/chunks/`). **Concurrent same-build opens** (e.g. `make -jN` both `dlopen` one large `.so`): per-`file_digest` fill state (`FillState{partial_fd, high_water: watch::Sender<u64>, first_chunk_barrier, result: watch::Sender<Option<io::Error>>}`) is shared in-process via `DashMap<[u8;32], Arc<FillState>>`. `O_EXCL` loser does NOT wait for full completion — it `select!{ first_chunk_barrier, result }` (error → reply `EIO` immediately), then replies `FOPEN_KEEP_CACHE` and its `read()` handlers consult the same shared `.partial` + `high_water`. `read(off,len)`: `off < high_water` → serve from `.partial`; else `select!{ high_water.changed(), result.changed() }` — on `result=Some(err)` reply `EIO` immediately (per-chunk blake3 mismatch, `GetChunks` stream error, circuit trip; the §2.7/§13 fail-fast promise); on high-water-advance re-check. The fill task sets `result` on every exit path (Ok or Err) and the §13 teardown sets `result=ENOTCONN` so waiters never block past `Drop`. On completion → whole-file blake3-verify → `rename .partial → <hex>` → `Promote{digest}`. Next `open()` is P0559 case (a). Prototype: `spike_stream_fuse.rs` (`15a9db79`). `// r[impl builder.fs.{streaming-open,node-chunk-cache}]` |
| `rio-builder/src/castore_fuse/tests/stream.rs` | unit harness adapted from `spike_stream_fuse.rs`: tmpfs staging + mock mountd; assert `open()` of synth 32 MiB returns <50 ms with first-chunk landed; second open after fill is passthrough (read upcalls = 0). **Orphan**: pre-create unlocked `staging/<hex>.partial` → `open()` unlinks + refetches. |
| same | This IS the per-read-upcall behavior ADR-022 §1 rejected for the warm path — but it applies **only during the cold-fill window of the first open of a large file on that node**. After fill: **0 upcalls while pages remain cached**; under cgroup memory pressure evicted pages re-upcall and are re-served from the SSD backing file. The fill window cost is exactly `filesize / 128 KiB` upcalls, once. |
| `rio-builder/src/config.rs` | `stream_threshold_bytes: u64` (default `8 * 1024 * 1024`). |

**Exit:** `cargo nextest run -p rio-builder castore_fuse::tests::stream` green (unit harness); `vm-castore-e2e` (cold-build + streaming-large-input) is the integration check at P0560.

---

## Phase 7 — delta-sync + chunked upload (U5; serialised after P0573 — note P0572/P0573 are now Phase-1/2 critical-path for P0559)

### P0573 — DirectoryService RPC surface — **DONE**
**Crate:** `rio-proto, rio-store` · **Deps:** P0572, P0589 · **Complexity:** MED

**Reconciliation note:** the `ReadBlob` cross-tenant denial test belongs to P0577 (where `ReadBlob` ships), not here. The bitmap byte order (bit i ⇔ digests[i], LSB-first, trailing bits zero) is documented in `types.proto` directly. Tests seed `directories`/`directory_tenants`/`file_blobs`/`file_blob_tenants` directly rather than driving the full `PutPath`+indexer chain — that path is covered by the `nar_index` tests.

| File | Change |
|---|---|
| `rio-proto/proto/store.proto` | `rpc GetDirectory(GetDirectoryRequest) returns (stream Directory)` — `GetDirectoryRequest { oneof by_what { bytes digest = 1; } bool recursive = 2; repeated bytes digests = 3; }`. `recursive=false` returns the single body; `recursive=true` server-side-BFS streams the whole subtree (snix's `DirectoryService.Get` semantics — first messages are the roots, subsequent are children deduped by digest). Field 3 is the **rio multi-root extension**: the BFS frontier is seeded from `{digest} ∪ digests`, deduped across all roots — the builder sends all closure `dir_digest` roots in one call (1 RPC / ~33 PG round-trips for chromium-scale, vs 357 RPCs / ~1000 PG round-trips per-root; **I-110 lesson**). snix clients omit field 3 (proto3 unknown-field) and stay wire-compatible. `rpc HasDirectories(HasDirectoriesRequest) returns (HasBitmap)`; `rpc HasBlobs(HasBlobsRequest) returns (HasBitmap)` — both batch (`repeated bytes digests = 1`; `HasBitmap { bytes bitmap = 1; }` one bit per request index, LSB-first within each byte). `// r[impl store.castore.directory-rpc]` |
| `rio-store/src/grpc/directory.rs` | new — all queries **tenant-scoped via junction** (`r[store.castore.tenant-scope]`): `get_directory(digest, recursive)`: `SELECT body FROM directories d JOIN directory_tenants t USING(digest) WHERE d.digest=$1 AND t.tenant_id=$2` (NotFound otherwise — body leaks child names/digests). For `recursive=true`: BFS frontier in batches of ≤256 (`WHERE d.digest=ANY($batch)`), yield each body, decode its `DirectoryEntry` children into next frontier, **dedup via `HashSet<[u8;32]>`** (shared subtrees sent once), stop at empty frontier. `has_directories(digests)` / `has_blobs(file_digests)`: junction-JOIN → bitmap. `tenant_id` from JWT `Claims.sub` **or** the HMAC assignment-token's `tenant` claim (`r[common.hmac.claims]` — already on `AssignmentClaims`; the builder presents an HMAC token, not a JWT); fail-closed `UNAUTHENTICATED` if neither present. |
| `migrations/062_nar_index.sql` (P0551 — pre-created) | `file_blobs`/`file_blob_tenants` tables already exist from P0551 — `file_blobs` is a `(digest, store_path_hash)` junction with FK→`manifests` `ON DELETE CASCADE`, populated in P0572's bottom-up pass via sorted-UNNEST insert + tenant-junction insert. **GIN-on-`nar_index.entries` is not viable**: `entries` is BYTEA (encoded proto), not JSONB, so a GIN expression index would require a proto-decoding `IMMUTABLE` PG function tied to the wire format (versioning hazard). The junction is the derived index for `HasBlobs` AND carries the `(store_path_hash, nar_offset)` coords P0574's `dag_sync` and P0570/P0577's `StatBlob`/`ReadBlob` key on. |
| `rio-store/src/lib.rs` | `rio_store_directory_{get_seconds,has_batch_size}` |
| tests | ephemeral PG: PutPath nested tree as tenant-A → `GetDirectory(root_digest)` returns correct children; `HasDirectories([root, unknown])` → `[1,0]` bitmap. **Cross-tenant denial**: tenant-B `HasDirectories([root])` → `[0]`; tenant-B `GetDirectory(root)` → NotFound; tenant-B `ReadBlob(file_digest)` → NotFound. `// r[verify store.castore.{directory-rpc,tenant-scope}]` |

**Exit:** `/nixbuild --checks` green.

### P0577 — `BlobService.Read(file_digest)` server-stream
**Crate:** `rio-proto, rio-store` · **Deps:** P0573 · **Complexity:** LOW (~40 LoC) · **Status:** DONE

Completes the snix-compatible castore surface: a client holding only a `file_digest` (from a `Directory` body) can fetch the bytes without knowing rio's chunk layout.

| File | Change |
|---|---|
| `rio-proto/proto/store.proto` | `rpc ReadBlob(ReadBlobRequest) returns (stream BlobChunk)` — `ReadBlobRequest { bytes file_digest = 1; }`, `BlobChunk { bytes data = 1; }`. Wire-compatible with snix `castore.proto BlobService.Read`. `// r[impl store.castore.blob-read]` |
| `migrations/063_file_blob_size.sql` | `ALTER TABLE file_blobs ADD COLUMN size BIGINT NOT NULL DEFAULT 0` — denormalize file size onto the junction so the read path never decodes `nar_index.entries`. Content-derived (same digest ⇒ same size), so two rows for one digest can't disagree. |
| `rio-store/src/castore.rs` | `DirectoryDag.file_blobs: Vec<([u8;32], u64, u64)>` — carry `(digest, nar_offset, size)` from `NarLsEntry` |
| `rio-store/src/grpc/directory.rs` | `read_blob(file_digest)`: `SELECT f.nar_offset, f.size, m.inline_blob, md.chunk_list FROM file_blobs f JOIN file_blob_tenants ft ON ft.digest=f.digest JOIN manifests m ON m.store_path_hash=f.store_path_hash AND m.status='complete' LEFT JOIN manifest_data md USING (store_path_hash) WHERE f.digest=$1 AND ft.tenant_id=$2 LIMIT 1`. Inline manifests: slice `inline_blob[nar_offset..nar_offset+size]`. Chunked: cumsum `partition_point` → ordered chunk slices `(hash, start, end)` → `cache.get_verified()` ×K=8 `buffered()` → stream `BlobChunk` frames. Whole-file BLAKE3 trailer (`rio_store_integrity_failures_total` on mismatch). NotFound if no tenant-scoped row. `// r[impl store.castore.blob-read]` |
| `rio-store/src/lib.rs` | `rio_store_directory_read_seconds` |
| tests | ephemeral PG + `MemoryChunkBackend`: inline / chunked / single-chunk-skip-and-take / zero-byte / cross-tenant-denial / no-backend / corrupt-chunk-list / size-overrun. `blake3(body) == file_digest`. `// r[verify store.castore.{blob-read,tenant-scope}]` |

**Reconciliation note:** the plan's original sketch resolved `file_digest → chunk-range` from `file_blobs` alone, but `file_blobs` (M_062) only carries `nar_offset` — `file_size` is needed for the chunk window's right edge. M_063 adds `file_blobs.size` rather than decoding `nar_index.entries` per call: the `entries` blob is O(files-in-NAR) (~2.5 MB for a chromium-scale output), and `read_blob` is the FUSE `open()` fast path. The "stream via existing GetChunks machinery" line meant the `ChunkCache` directly, not the gRPC `GetChunks` — `read_blob` uses the same `cache.get_verified()` `GetPath` does, with `buffered(8)` ordered prefetch.

**Exit:** `/nixbuild --checks` green.

### P0586 — `PutPathChunked`: builder-side fused walk + `HasChunks` + pipelined sync narhash verify
**Crate:** `rio-store, rio-builder, rio-proto` · **Deps:** P0551, P0572, P0573, P0577 · **Complexity:** HIGH · **Status: DONE 2026-05-28 — foundations, the storage conversion, the RPC handler, the builder client switch (legacy PutPath/PutPathBatch client deleted), and the `vm-put-path-chunked` end-to-end VM test are all landed**

Moves chunking to the builder; rio-store's per-stream working set drops from `nar_size` bytes to one ≤256 KiB chunk in flight. Closes `TODO(P0433)` (refs forced into separate pre-pass) and `TODO(P0434)` (manifest-first upload). Design at [§6](./022-lazy-store-fs-erofs-vs-riofs.md#6-extension-chunked-output-upload-putpathchunked).

> **Reconciliation (in progress).** Landed so far: the wire surface
> (`PutPathChunked`/`HasChunks` proto + `Unimplemented` stubs), the DAG codecs
> (`castore_util::{directory_digest,validate_directory}`,
> `castore_nar::{walk,nar_pieces}`, `nar::frame` + the `WalkObserver` hook on
> `dump_path`), the builder-side fused walk (`upload/walk.rs`, not yet wired
> into `upload_all_outputs`), `HasChunks` + the `chunks.durable` flip,
> `validate_begin` (`put_path_chunked/validate.rs`, tested but the RPC still
> returns `Unimplemented`), and — the largest deviation — the **storage
> conversion**: rather than adding per-file blob-stream chunking as a second
> format alongside the legacy whole-NAR chunks, **every** ingest path
> (PutPath, PutPathBatch, the substituter) now parses the NAR once, chunks per
> regular file, stores the blob stream (not the NAR) inline or chunked, writes
> the castore index in the complete transaction (subsuming P0557), and
> `GetPath` regenerates the NAR framing from the Directory DAG on read. There
> is exactly one storage format (`manifest_data.chunk_list` version 2; version
> 1 is rejected on read; deployments are greenfield). The background indexer,
> its work queue, and `GetNarIndex`'s sync-on-miss recompute were deleted.
> Other deviations from the file table below: `HasChunks` requires an
> authenticated caller (`store.chunk.has-chunks-authenticated` — the answer is
> still tenant-agnostic, the probe is not anonymous); the GC sweep clears
> `chunks.durable` alongside `uploaded_at`; `validate_directory` bounds entry
> names, symlink targets, and entry counts to the NAR reader's limits;
> `NarPieces` flushes its framing buffer at 256 KiB; §6.3 gained
> `store.put.verify-file-digest` (the verify task must recompute each file's
> BLAKE3 against `FileEntry.digest` or a hostile builder poisons the
> cross-tenant `file_digest → content` dedup namespace); and the
> `directory_tenants`/`file_blob_tenants` writes in the commit-txn description
> are `directory_paths` + read-time `path_tenants` JOINs per the migration-064
> note. The RPC handler (the §6.3 sequential verify walk + the §6.2
> single-transaction commit) is landed, with three deviations from the §6.2
> commit description: the commit transaction opens with a sorted
> `SELECT … FOR UPDATE` over every referenced chunk that both acquires the row
> locks in one globally-ordered statement and proves each chunk's S3 object is
> still claimable before `durable` flips (a chunk the GC sweep claimed
> mid-upload aborts the commit `UNAVAILABLE` instead of committing a manifest
> whose object the drain may already have deleted); the write-ahead `chunks`
> rows are inserted at `refcount = 0` with the grace clock restarted rather
> than refcounted (a refcount write-ahead would leak permanently on a builder
> crash because there is no `manifest_data` row for the orphan scanner to
> decrement from); and the CA path claims its post-verify placeholder via the
> shared `claim_placeholder` state machine rather than the single
> `INSERT … RETURNING` form (same loser-never-commits guarantee). The §6.3
> bounded prefetch for deduped chunks is deferred (fetches are sequential).
> Still to land: the builder client switch, MockStore support, and
> `vm-put-path-chunked`.

| File | Change |
|---|---|
| `rio-proto/proto/store.proto` | `rpc HasChunks(HasChunksRequest) returns (HasChunksResponse)` — digest list → bitmap, durable-presence semantics (bit set IFF referenced by ≥1 `complete` manifest, not refcount≥1; I-201). `rpc PutPathChunked(stream PutPathChunkedRequest) returns (PutPathResponse)` — `Begin{hmac_token, deriver, outputs: repeated OutputHeader{store_path, nar_hash, nar_size, refs, root_node, chunk_manifest}, directories: repeated castore.Directory, novel: repeated bytes, input_closure: repeated StorePath}` then `Chunk{digest, bytes}` for each `digest ∈ novel`. `// r[impl store.put.chunked]` `// r[impl store.put.chunked-wire]` |
| `rio-store/src/grpc/chunk.rs` | `has_chunks`: `SELECT blake3_hash FROM chunks WHERE blake3_hash = ANY($1) AND durable AND NOT deleted` — bitmap result. **No tenant JOIN** — chunks are content-addressed and tenant-agnostic per `r[store.castore.tenant-scope]` (`chunk_tenants` was dropped in migration 035). `find_missing_chunks`: add `AND c.durable` to the existing junction-JOIN query (closes the I-201 WAL-window race for the legacy presence check too). `put_chunk` standalone path: after S3 PUT confirms, `UPDATE chunks SET durable=TRUE WHERE blake3_hash=$1 AND NOT durable`. `// r[impl store.chunk.has-chunks-durable]` `// r[impl store.chunk.durable-flag]` |
| `rio-store/src/grpc/put_path_chunked/validate.rs` | `validate_begin(&Begin, &AssignmentClaims) -> Result<ValidatedBegin>`: token role accepted per `r[store.put.builder-chunked-only]`; `hash_part(deriver)==claims.drv_hash`; `blake3(sorted(input_closure))==claims.input_closure_digest`; `len(outputs) ≤ MAX_BATCH_OUTPUTS` and `outputs[*].store_path` pairwise-distinct; `output_paths = {outputs[*].store_path}`; per-output `store_path ∈ claims.expected_outputs` (non-CA), `nar_size ≤ MAX_NAR_SIZE`, `len(refs) ≤ MAX_REFERENCES`, `refs ⊆ input_closure ∪ output_paths`, `len(chunk_manifest) ≤ MAX_CHUNKS`; `len(directories) ≤ MAX_DIR_NODES`; for each `d ∈ directories` run `castore::Directory::validate(d)` (snix's checks — entry names single-component, sorted, no dups, child sizes consistent) then recompute `blake3(canonical-encode(d))`, build `HashMap<dir_digest, &Directory>`, walk each `root_node` asserting every reachable `dir_digest` present and per regular file the contiguous `chunk_manifest` `len` run sums to `FileEntry.size`; build `manifest_len: HashMap<Digest, u32>` over all outputs (assert all occurrences of `d` agree on `len` and `≤ FASTCDC_MAX_BYTES`); recompute the global-first-occurrence order and assert `Begin.novel` matches it exactly (membership, no-dups, AND order); `len(input_closure) ≤ MAX_INPUT_CLOSURE`; acquire `nar_bytes_budget` for `Σ manifest_len[d]` over `d ∉ novel` (self-consistent, not attested — §6.3 asserts actual `cas::get` length). All violations → `INVALID_ARGUMENT`. `// r[impl store.put.chunked-bounds]` |
| `rio-store/src/grpc/put_path_chunked/mod.rs` | new — on validated `Begin`: arm `PlaceholderGuard` (`r[store.put.drop-cleanup+2]`, `r[store.gc.orphan-heartbeat]`); per output check `'complete'` → mark `skipped[i]`; for non-skipped non-CA insert `'uploading'` placeholder with `references` (`r[store.put.wal-manifest]`, `r[store.put.placeholder-refs]`); if all skipped, drain remaining `Chunk` frames (each `cas::put`) and return OK. **Single sequential `verify_task(stream, validated, skipped)`**: `acc[i] = (Sha256, RefScanSink(input_closure ∪ output_paths))` per output; `seen: HashSet<Digest>`; `next_novel = 0`; spawn bounded `prefetch_task` (≤32 outstanding `cas::get` for upcoming non-first-occurrence positions, into a small LRU). For `out_idx in 0..N { for pos in 0..len(chunk_manifest[out_idx]) { d = …; emit nar::Encoder framing → acc[out_idx] (no-op if skipped[out_idx]); if d ∈ novel && d ∉ seen { frame = stream.next().await; assert frame.digest==d==novel[next_novel] && len==manifest_len[d] && blake3==d else INVALID_ARGUMENT; cas::put(d, bytes) (transient Err ⇒ Unavailable); seen.insert(d); next_novel+=1; body=bytes } else { body = prefetch_lru.get(d) or cas::get(d) (Err ⇒ Unavailable); assert body.len()==manifest_len[d] else Mismatch }; feed body → acc[out_idx] (no-op if skipped) } }`. After walk: extra `stream.next()` ⇒ INVALID_ARGUMENT; `next_novel < len(novel)` ⇒ Incomplete. Per non-skipped output: `Match` iff `acc[i].sha256==nar_hash && sorted(acc[i].scanned)==refs`. **Verdict**: any `Mismatch` → `FAILED_PRECONDITION`, `rio_store_{narhash,refs}_mismatch_total++`, structured-log; `Incomplete` → `FAILED_PRECONDITION`, `rio_store_putpath_incomplete_total++`; `Unavailable` (no Mismatch) → `UNAVAILABLE`, `rio_store_putpath_verify_unavailable_total++`. **Commit txn** (all Match): for `is_ca` outputs recompute `make_fixed_output(name, computed_nar_hash, true, refs)` and assert `== store_path` else `PERMISSION_DENIED`; per non-skipped output `INSERT … ON CONFLICT (store_path_hash) WHERE status='complete' DO NOTHING RETURNING store_path_hash` for the manifest row; for outputs in the `RETURNING` set ONLY: INSERT `manifest_data`, `nar_index.root_node`; UPSERT `directories` refcount += 1 per output-tree occurrence; INSERT `file_blobs` `ON CONFLICT DO NOTHING`; chunk refcount UPSERT; `UPDATE chunks SET durable=TRUE WHERE blake3_hash = ANY($sorted) AND NOT durable` (lock-order per `r[store.chunk.lock-order]`); ed25519-sign narinfo (`r[store.sig.fingerprint]`); flip `uploading→complete`; enqueue compat writes if enabled (`r[store.compat.nar-on-put]`). For ALL outputs (including idempotent-skipped): INSERT `directory_tenants`/`file_blob_tenants`/`path_tenants` for `claims.tenant` `ON CONFLICT DO NOTHING`. The legacy `grpc/put_path.rs` complete-txn gets the same `durable=TRUE` UPDATE. Idempotent re-drive (`r[store.put.idempotent]`). `// r[impl store.chunk.self-verify]` `// r[impl store.put.narhash-sync]` `// r[impl store.put.refs-sync]` `// r[impl store.put.chunked-ca]` `// r[impl store.integrity.verify-on-put]` `// r[impl store.atomic.multi-output]` `// r[impl store.castore.tenant-scope]` |
| `rio-builder/src/upload.rs` | rewrite `upload_all_outputs`: per output, single canonical-NAR-order walk (`r[builder.nar.entry-name-safety]`); the walk feeds the **full NAR byte sequence** (framing, entry names, symlink targets, file bytes) into per-output SHA-256 AND Boyer-Moore refscan (`r[builder.upload.references-scanned]`); per regular file, the same disk read additionally drives FastCDC (16/64/256 KiB, `r[store.cas.fastcdc]`) emitting `(offset,len,blake3)` + whole-file blake3 → `file_digest`; `Directory` body construction. Optional input-reuse shortcut: size match against the in-memory `(size, file_digest)` map of declared inputs (P0559's `tree::InoMap`) → `cmp` against the castore-FUSE lower → reuse input's `file_digest` (still re-FastCDC + SHA-256 + refscan over the `cmp`-read bytes; chunk-list reuse unsound for legacy whole-NAR-chunked inputs). End-of-walk over all outputs: dedupe `directories` by digest; compute `novel` = global-first-occurrence-ordered list of `HasChunks`-false digests (walk `outputs[0].chunk_manifest`..`outputs[N].chunk_manifest`, append each not-yet-seen `HasChunks`-false digest); client-stream `Begin{deriver, outputs[], directories[], novel[], input_closure: WorkAssignment.input_closure}` then each `novel` chunk **in `novel` order**. `// r[impl builder.upload.fused-walk]` `// r[impl builder.upload.chunked-manifest]` `// r[impl builder.upload.batch+2]` |
| `migrations/062_nar_index.sql` (P0551 — pre-created) | `chunks.durable boolean NOT NULL DEFAULT false` + `chunks_present_idx` partial index — **already created by P0551's migration**, no DDL change here. |
| `nix/tests/scenarios/put-path-chunked.nix` | new — (i) 2-output build → `PutPathChunked`; both outputs `GetPath` round-trip + servable via `GetDirectory`/`ReadBlob`; (ii) output with two byte-identical files (repeated `chunk_manifest` digest) → commit OK; (iii) tampered `Chunk` body → `INVALID_ARGUMENT`; (iv) wrong `outputs[0].nar_hash` → `FAILED_PRECONDITION` + mismatch counter + zero manifests committed; (v) builder closes after sending k/n novel → `FAILED_PRECONDITION` + incomplete counter; (vi) high-dedup output (most chunks pre-seeded) → commit OK; (vii) `Begin.nar_size > MAX_NAR_SIZE` and `len(directories) > MAX_DIR_NODES` → `INVALID_ARGUMENT` before any S3 write; (viii) `is_ca=true` token → no `'uploading'` placeholder mid-stream, then committed; same with wrong `store_path` → `PERMISSION_DENIED`; (ix) `HasChunks` reports false for refcount≥1-but-uploading chunk; (x) output bytes contain a closure-member hash omitted from `outputs[i].refs` → `FAILED_PRECONDITION` + `rio_store_refs_mismatch_total`; `Begin.input_closure` not matching `claims.input_closure_digest` → `INVALID_ARGUMENT`; (xi) `Chunk` frame out of `novel` order → `INVALID_ARGUMENT`; (xii) re-drive after one output already `'complete'` → that output skipped, others committed, tenant junctions written for both |
| `nix/tests/default.nix` | wire `vm-put-path-chunked` subtests; markers placed at the `subtests = [...]` entry per CLAUDE.md convention |

**Exit:** `/nixbuild --checks` green; `nix build .#checks.x86_64-linux.vm-put-path-chunked` green; 061's `PINNED` checksum is already pinned (no DDL change here).

### P0574 — Gateway substituter: Directory-DAG delta-sync client  ★ U5 LANDS
**Crate:** `rio-gateway` · **Deps:** P0573, P0577 · **Complexity:** MED
| File | Change |
|---|---|
| `rio-gateway/src/substitute/mod.rs` | new module — `substitute/{mod.rs,dag_sync.rs}`. There is no existing `handler/substitute.rs`; the pre-ADR-022 `nix copy --from rio://` path is the `wopQuerySubstitutablePathInfos`/`wopAddToStoreNar` handlers in `handler/opcodes_read.rs`/`opcodes_write.rs`. This plan does NOT move those; it adds the dag-sync path as a NEW substituter and the gateway dispatches based on advertised capability. |
| `rio-gateway/src/substitute/dag_sync.rs` | new — `async fn sync_closure(local: &dyn LocalStore, remote: StoreClient, roots: &[StorePath])` → for each root, `GetNarIndex` → `root_digest`. BFS the Directory DAG: batch `HasDirectories(frontier)` against **local** store; for present digests, prune subtree; for absent, `GetDirectory(d)` → enqueue child dir digests + collect child `file_digest`s. After BFS: batch `HasBlobs(collected_file_digests)` against local; for absent, fetch via `ReadBlob(file_digest)` (P0577) — server-side resolves coords via `file_blobs`. Reassemble NARs locally from materialized blobs + Directory tree (NAR is derived, à la snix nar-bridge). `// r[impl gw.substitute.dag-delta-sync]` |
| `rio-gateway/src/substitute/mod.rs` | capability dispatch: if remote advertises `directory-service` capability AND closure `root_digest` is available, use `dag_sync`; else fall through to the existing whole-NAR `wopAddToStoreNar` path in `handler/opcodes_write.rs`. |
| `rio-gateway/src/lib.rs` | `rio_gateway_dagsync_{subtrees_pruned_total,blobs_fetched_total,bytes_saved_total}` |
| `nix/tests/scenarios/dag-delta-sync.nix` | two-store fixture: store-A has closure v1; store-B has closure v2 (one file changed in a deep subdir). `nix copy --from rio://store-B` on store-A → assert `subtrees_pruned_total > (total_dirs × 0.9)` AND `blobs_fetched_total == 1`. |
| `nix/tests/default.nix` | `# r[verify gw.substitute.dag-delta-sync]` |

**Exit:** `/nixbuild --checks` green; VM scenario demonstrates O(changed-subtrees) discovery.

---

## Phase 8 — Explicit tenancy: no anonymous access (post-cutover)

**Decision (2026-05-28, owner):** tenants become explicit everywhere and anonymous access is
removed. The P0560 cutover put the builder's input path on the tenant-scoped castore surface
(`r[store.castore.tenant-scope]`), which exposed two gaps: single-tenant mode
(`tenant_id = NULL`) cannot satisfy the tenant requirement at all, and nothing writes
`path_tenants` rows for client-uploaded sources/.drvs (only the scheduler's completion-time
upsert covers built outputs). Rather than adding an anonymous carve-out to the castore surface,
the NULL-tenant mode is retired: a `default` tenant always exists (migration-seeded), every
session/build/upload is attributed to a real tenant, every store path gets ownership rows at
ingest, and unauthenticated callers are rejected across the store surface (the
"anonymous → unfiltered" carve-outs go away). **Sequenced strictly after P0560/P0562** — the
cutover ships with the fixture stopgap recorded in the P0560 reconciliation note; P0593 deletes
it. Working decisions, revisit at implementation: default tenant is named `default` and created
by migration (not bootstrap); the gateway attributes its store calls via the existing
service-token + tenant-id-header pattern (the scheduler's `SubstituteAuth::Service` shape)
rather than making end-user JWTs mandatory.

### P0590 — `default` tenant + explicit attribution (NULL tenant retired at the API boundary)
**Crate:** `rio-migrations, rio-gateway, rio-scheduler, docs` · **Deps:** P0560, P0589 · **Complexity:** LOW · **Status:** UNIMPL

> **Decision (2026-05-28): explicit key→tenant mapping.** Tenant attribution for SSH clients
> moves to an explicit, operator-managed mapping from authorized key to tenant (structured
> gateway config / Secret: key entry → tenant name), replacing the "tenant = free-text
> authorized_keys comment" mechanism. `gw.auth.tenant-from-key-comment` is superseded by a new
> rule for the explicit mapping (reword + `tracey bump`); a key with no mapping resolves to
> `default`, a mapping naming an unknown tenant still rejects. The VM-test stopgap (P0560)
> keeps using the comment field until P0593 retires it together with the rest of the stopgap.

| File | Change |
|---|---|
| `rio-migrations/migrations/0NN_default_tenant.sql` | `INSERT INTO tenants (tenant_name) VALUES ('default') ON CONFLICT (tenant_name) DO NOTHING` — the row every unattributed identity resolves to. New migration, pinned checksum. |
| `rio-gateway/src/server/connection.rs` | replace comment-derived attribution with the explicit key→tenant mapping above; keys without a mapping resolve to `default` instead of `None`; a mapping naming an unknown tenant still rejects. Spec rule replacement + `tracey bump`. |
| `rio-scheduler/src/grpc/scheduler_service.rs` | empty `tenant_name` → resolve `default` (today: `None` = single-tenant); missing `default` row → actionable `FAILED_PRECONDITION` ("default tenant missing — run migrations"). After resolution `BuildInfo.tenant_id` is always `Some`, so `AssignmentClaims.tenant` is always set. Spec `r[sched.tenant.resolve]` bump. |
| VM scenarios with grpcurl-direct `SubmitBuild` | pass `tenant_name` explicitly (scheduling cancel-timing, lifecycle helpers). |

**Exit:** every accepted build carries a non-NULL tenant; `/nixbuild --checks` green.

### P0591 — ownership at ingest: `path_tenants` for uploads + input closure + backfill
**Crate:** `rio-store, rio-gateway, rio-scheduler, rio-migrations` · **Deps:** P0590, P0586 · **Complexity:** MED · **Status:** UNIMPL

| File | Change |
|---|---|
| `rio-store/src/grpc/put_path/common.rs` | `authorize()` resolves the caller tenant from JWT **or** `AssignmentClaims.tenant` **or** service-token + `x-rio-probe-tenant-id`; legacy `PutPath`/`PutPathBatch` finalize inserts the `path_tenants` junction (same `insert_path_tenant_in_conn` the chunked commit txn already uses). |
| `rio-store/src/grpc/put_path_chunked/mod.rs` | tenant fallback to `claims.tenant` when no JWT — builder uploads then write their own junction rows instead of relying solely on the scheduler's completion upsert. |
| `rio-gateway/src/handler/opcodes_write.rs` | every relayed upload carries the session tenant (service token + tenant-id header); single-tenant sessions carry `default`. |
| `rio-scheduler` (merge/dispatch) | upsert `path_tenants` for the build's INPUT closure (sources + .drvs + input-drv outputs), batched UNNEST `ON CONFLICT DO NOTHING` — covers paths uploaded before this change and cross-tenant reuse of pre-existing paths. |
| backfill | one-shot migration (or `rio-cli admin` command): every `narinfo` row with zero `path_tenants` rows → `default`. Required for existing deployments; without it pre-existing paths stay invisible to the castore reads. |
| tests | store integration: gateway-shaped upload with tenant → junction row → tenant-scoped `GetDirectory`/`ReadBlob` succeed for that tenant and NotFound for another; scheduler unit: input-closure upsert. `// r[verify store.castore.tenant-scope]` additions. |

> **Reconciliation (2026-05-29, partial pull-forward).** The `put_path_chunked/mod.rs` row landed early as part of the post-cutover bug-fix pass: the chunked upload now resolves its `path_tenants` tenant from the JWT **or** `AssignmentClaims.tenant` via the shared `grpc::resolve_tenant_id` helper (the same mapping the castore read side uses), so HMAC-only builder uploads write their own junction rows. Everything else in this item — `authorize()`/legacy `PutPath`+`PutPathBatch` junctions, the gateway tenant header, the scheduler input-closure upsert, the backfill — remains UNIMPL.

**Exit:** tenant-scoped castore reads serve client-uploaded sources/.drvs to the owning tenant with NO fixture trigger; `/nixbuild --checks` green.

### P0592 — no anonymous access: mandatory keys + carve-out removal
**Crate:** `rio-store, rio-gateway, rio-scheduler, nix, infra, docs` · **Deps:** P0590, P0591 · **Complexity:** MED · **Status:** UNIMPL

| Item | Change |
|---|---|
| keys | HMAC assignment + service keys become required for store/scheduler/gateway: helm bootstrap already generates them; the standalone NixOS module path gains key generation/required options; the legacy unsigned format-string assignment token is removed. |
| `rio-store` reads | retire "anonymous → unfiltered": every read RPC requires a verified identity (gateway session tenant via service-token header or JWT; builder via assignment token; scheduler via service token). Builder-internal RPCs (`BatchQueryPathInfo`, `GetNarIndexBatch`, `GetPath`, `GetChunks`) require a verified identity too — they may stay cross-tenant where content-addressed (`GetChunks`), but never anonymous. `r[store.tenant.narinfo-filter]` rework + security.md read-authorization update. |
| `rio-gateway` | attaches the session tenant identity on ALL outbound store calls (reads included), and the `.drv` anonymous-lookup exemption (`r[gw.jwt.anon-drv-lookup]`) is replaced by the now-guaranteed ownership rows from P0591. |
| docs/spec | remove "single-tenant mode (tenant_id = NULL)" language across gateway/scheduler/store specs + `docs/src/multi-tenancy.md`; tracey bumps for every reworded rule. |

**Exit:** no unauthenticated RPC is accepted on the store surface; `/nixbuild --checks` green.

### P0593 — test estate: explicit tenancy by default; delete the fixture stopgap
**Crate:** `nix` · **Deps:** P0591, P0592 · **Complexity:** LOW-MED · **Status:** UNIMPL

| File | Change |
|---|---|
| `nix/tests/fixtures/standalone.nix` | tenancy becomes the default (every instance gets keys + a tenant-named key comment); DELETE the `defaultTenant` narinfo trigger + backfill SQL — the product now writes ownership rows at ingest (P0591). |
| `nix/tests/default.nix` + scenarios | sweep the rest of the matrix onto explicit tenancy: scheduling (incl. grpcurl-direct submits), observability, chaos, ca-cutoff, protocol-cold/-lix, security standalone instances; k3s fixtures create the tenant (bootstrap or psql) and use a tenant-named key comment — un-breaks the k3s build scenarios that remained red after P0560. |

**Exit:** every previously-red build-running VM scenario is green without per-test tenancy hacks; `vm-castore-e2e` / `vm-protocol-warm-standalone` keep passing with the stopgap removed.

---

## `onibus dag append` rows

```jsonl
{"plan":576,"title":"EXT: nixos-cutover landed (kernel.nix ≥6.9 importable + /dev/fuse + AMI; FUSE_PASSTHROUGH=y stock + boot.kernelModules)","deps":[],"crate":"ext","priority":99,"status":"RESERVED","complexity":null,"note":"sentinel; coordinator flips DONE when nixos-cutover agent merges. ≥6.9 for FUSE_PASSTHROUGH (7dc4e97a4f9a); EROFS_FS not required. kernel.nix landed standalone — stock kernel, no kernelPatches, EROFS/cachefiles _ONDEMAND symbols dropped (binary-cache hit)"}
{"plan":569,"title":"SPIKE sentinel: composefs-style validated at chromium scale (§3 alternative; stream/priv findings carry to §2)","deps":[],"crate":"spike","priority":99,"status":"DONE","complexity":null,"note":"consolidated 15a9db79 on adr-022; §3 EROFS alternative now discarded"}
{"plan":541,"title":"SPIKE: privilege boundary (userns-overlay/fuse-dev-fd-handoff/teardown-under-load; erofs subtests §3-only)","deps":[],"crate":"spike,nix","priority":95,"status":"DONE","complexity":"MED","note":"all PASS, kernel 6.18.20; commit af8db499 on adr-022; overlay stays in builder via userxattr"}
{"plan":578,"title":"SPIKE: passthrough-under-overlay (depth=2 mount; unpriv BACKING_OPEN→EPERM; brokered ioctl on dup'd /dev/fuse; reads-survive-kill; Promote integrity; Mount{build_id:\"../x\"}→BadBuildId; second-conn-same-uid→rejected)","deps":[541],"crate":"spike,nix","priority":95,"status":"PARTIAL","complexity":"LOW","note":"extends composefs-spike-priv.nix; gates P0559/P0567 design; Q7-Q12 done (kernel mechanisms); mountd-protocol correctness subtests landed as vm-mountd (P0567); perf criteria vi/vii/x are measured there but not gated — confirming them needs one KVM-backed run's PERF lines"}
{"plan":543,"title":"V11/V12 + closure-paths + aarch64 kernel-config sanity","deps":[],"crate":"xtask,nix","priority":90,"status":"UNIMPL","complexity":"LOW","note":"V12 tunes STREAM_THRESHOLD (P0575 ships unconditionally); V4 (encoder latency) dropped with §3; closure_paths<65535 + max_nar_size gates REMOVED"}
{"plan":544,"title":"Spec scaffold: ADR-022 §2 + design-overview + ADR-023 (per-AZ tiered) + r[...] markers","deps":[],"crate":"docs","priority":95,"status":"DONE","complexity":"LOW","note":"merges adr-022 markers (see tracey inventory below); tracey markers MUST precede r[impl]"}
{"plan":545,"title":"proto: NarIndex (+file_digest) / GetNarIndex","deps":[544],"crate":"rio-proto","priority":90,"status":"DONE","complexity":"LOW","note":"no boot_blob"}
{"plan":546,"title":"rio-nix streaming nar_ls (Read-only single-pass; offset-tracking + blake3-per-file) + fuzz","deps":[544,545],"crate":"rio-nix","priority":90,"status":"DONE","complexity":"MED","note":"no Seek, bounded memory regardless of NAR size; blake3 streamed once; populates file_digest"}
{"plan":548,"title":"TieredChunkBackend (S3 standard authoritative; S3 Express read-through cache)","deps":[544],"crate":"rio-store","priority":90,"status":"DONE","complexity":"LOW","note":"both tiers are S3ChunkBackend; no backend/fs.rs"}
{"plan":549,"title":"ChunkBackend blob-API (put_blob/get_blob/delete_blob)","deps":[544,548],"crate":"rio-store","priority":85,"status":"DONE","complexity":"LOW","note":"serialise after 548; used by P0566 narinfo/manifests sidecar only"}
{"plan":550,"title":"Hoist StoreClients+fetch_chunks_parallel → store_fetch.rs (NOT pure mv)","deps":[544],"crate":"rio-builder","priority":85,"status":"DONE","complexity":"MED","note":"fetch.rs:20,32-33 imports fuser"}
{"plan":568,"title":"Batched GetChunks server-stream (K_server=256) + prost .bytes() + tonic residuals + obs","deps":[545,550],"crate":"rio-proto,rio-store,rio-builder,infra","priority":85,"status":"DONE","complexity":"MED","note":"r[proto.chunk.batch-bidi]; abort-on-first-error; ChunkData.data → bytes::Bytes"}
{"plan":570,"title":"StatBlob RPC: server-side file_digest → ChunkMeta[] (snix BlobService.Stat; shares file_blobs+cumsum helper with P0577 ReadBlob)","deps":[573],"crate":"rio-proto,rio-store","priority":85,"status":"DONE","complexity":"LOW","note":"castore-FUSE open() resolves chunk-coords server-side; no client DigestResolver; r[store.castore.blob-stat]"}
{"plan":551,"title":"migration 062_nar_index + manifests.nar_indexed bool + queries","deps":[545],"crate":"rio-store","priority":85,"status":"DONE","complexity":"LOW","note":"partial-index work-queue WHERE NOT nar_indexed (precedent: 031); PG forbids cross-table predicate"}
{"plan":552,"title":"GetNarIndex handler + indexer_loop","deps":[545,546,551],"crate":"rio-store","priority":85,"status":"DONE","complexity":"MED","note":"nar_index_sync_max_bytes guard; entries carry file_digest"}
{"plan":553,"title":"infra/eks/s3-express.tf per-AZ directory bucket (for_each express_az_ids) + dedicated rio-store SG/NodeClass + s3express IAM","deps":[548],"crate":"infra","priority":80,"status":"UNIMPL","complexity":"LOW","note":"per-AZ from day one; TieredChunkBackend is AZ-count-agnostic; no CSI/PVC/kmod"}
{"plan":554,"title":"helm chunkBackend.tiered + per-AZ Express bucket env (downward-API zone → IMDS zone-id → bucket)","deps":[548,553],"crate":"infra,xtask","priority":80,"status":"UNIMPL","complexity":"LOW","note":"FIRST SHIPPED VALUE (U2)"}
{"plan":555,"title":"VM test: tiered-backend cache semantics","deps":[548,554],"crate":"nix","priority":80,"status":"UNIMPL","complexity":"MED","note":""}
{"plan":579,"title":"binary_cache_compat config + helm (runtime toggle, default ON)","deps":[544],"crate":"rio-store,infra","priority":80,"status":"UNIMPL","complexity":"LOW","note":"U6 foundation"}
{"plan":566,"title":"binary-cache compat writer: stock-Nix .narinfo + nar/*.nar.zst to S3-standard post-commit","deps":[549,579],"crate":"rio-store","priority":80,"status":"UNIMPL","complexity":"MED","note":"reassemble from moka-hot chunks; FileHash/FileSize populated; failure non-fatal to PutPath"}
{"plan":580,"title":"VM test: stock-Nix substitutes from S3 with rio-store stopped","deps":[566],"crate":"nix","priority":80,"status":"UNIMPL","complexity":"MED","note":"U6 LANDS"}
{"plan":581,"title":"compat GC: enqueue narinfo+nar.zst to pending_s3_deletes on sweep; narinfo.compat_file_hash column","deps":[551,566],"crate":"rio-store","priority":75,"status":"UNIMPL","complexity":"LOW","note":"runs regardless of current enabled value"}
{"plan":582,"title":"compat reconciler: backfill compat_file_hash IS NULL rows","deps":[566,581],"crate":"rio-store","priority":60,"status":"UNIMPL","complexity":"LOW","note":"crash-window + toggle-ON backfill; deferrable"}
{"plan":583,"title":"drop inline_blob: all NARs chunked; ChunkBackendKind::Inline removed; chunk_backend required","deps":[544,551],"crate":"rio-store,rio-proto","priority":80,"status":"UNIMPL","complexity":"MED","note":"greenfield: ALTER TABLE manifests DROP COLUMN inline_blob in mig 054; ManifestKind collapses; chunk_cache no longer Option"}
{"plan":589,"title":"AssignmentClaims.{role,input_closure_digest} + dispatch populate","deps":[544,588],"crate":"rio-auth,rio-scheduler","priority":92,"status":"DONE","complexity":"LOW","note":"sequenced before P0573/P0586/P0584 — all three read these fields; closure already in hand at dispatch via P0588"}
{"plan":584,"title":"builder-chunked-only auth gate: PutPath/PutPathBatch reject role=Builder","deps":[586,589],"crate":"rio-store","priority":80,"status":"UNIMPL","complexity":"LOW","note":"PERMISSION_DENIED before buffering; pushes FastCDC CPU to builders"}
{"plan":585,"title":"Express eviction sweeper: per-AZ Lease, size-bounded MRU (target 8 TiB, hi/lo watermark)","deps":[548,554],"crate":"rio-store,infra","priority":75,"status":"UNIMPL","complexity":"LOW","note":"LastModified=last-cold-miss (read-through-only fill); S3 Lifecycle is age-based ceiling only, app sweep is authoritative for size target"}
{"plan":586,"title":"PutPathChunked: multi-output Begin + bounds validate + fused walk + HasChunks + pipelined sync narhash+refs verify + CA defer-placeholder","deps":[551,572,573,577,589],"crate":"rio-store,rio-builder,rio-proto","priority":85,"status":"DONE","complexity":"HIGH","note":"closes TODO(P0433/P0434); landed: wire surface, DAG codecs, fused walk, HasChunks+durable, validate_begin, the blob-stream storage conversion (one format, eager castore index, GetPath framing regen -- subsumes P0557), the RPC handler (sequential verify walk + single-txn commit with a commit-time chunk-presence proof), the builder client switch (fused walk + HasChunks probe + Begin/novel-chunk stream; legacy PutPath/PutPathBatch client deleted), and vm-put-path-chunked (e2e multi-output stream, cross-output refs, HasChunks dedup, floating-CA)"}
{"plan":556,"title":"[ABANDONED] libcomposefs FFI encoder (composefs-sys + encode.rs) — §3 EROFS alternative","deps":[],"crate":"","priority":0,"status":"ABANDONED","complexity":null,"note":"2026-04-23: §2 castore-FUSE has no metadata image; encoder/patch/VM-test/fuzz all dropped"}
{"plan":557,"title":"PutPath eager nar_index compute (try_acquire-gated; no encode)","deps":[551,552,572,586],"crate":"rio-store","priority":80,"status":"DONE","complexity":"LOW","note":"landed inside P0586's storage conversion: complete_manifest_in_conn writes the castore index from the in-hand ParsedNar in the status-flip transaction, for every ingest path. Mandatory, not gated -- the index cannot be recomputed after ingest. The background indexer and sync-on-miss were deleted."}
{"plan":567,"title":"rio-mountd DaemonSet (fuse-fd-handoff + BACKING_OPEN broker + Promote/PromoteChunks verify-copy + cache+chunks ownership + build_id validation + per-uid conn binding + staging quota + metrics)","deps":[576,578],"crate":"rio-builder,infra","priority":80,"status":"DONE","complexity":"MED","note":"daemon + SOCK_SEQPACKET/postcard wire protocol + unit tests + vm-mountd VM test (P0578-deferred subtests, perf printed not gated) + mountd-ds.yaml (privileged for Bidirectional mountPropagation, ships enabled:false until P0559/P0560) + eks-node /var/rio prjquota bind; tokio async per-conn; Promote+PromoteChunks both on spawn_blocking+Semaphore; build_id ^[A-Za-z0-9_-]{1,64}$; openat(base_dirfd) not string-concat; one conn per peer_uid; kernel project-quota staging enforcement; e2e exercise of the deployed DS is P0560§B"}
{"plan":588,"title":"WorkAssignment.{input_roots,input_closure}: scheduler→builder root_node + sorted-closure transport (proto fields 13,14 + dispatch.rs closure walk)","deps":[572],"crate":"rio-proto,rio-scheduler","priority":85,"status":"DONE","complexity":"LOW","note":"~50 LoC; r[sched.dispatch.input-roots]; input_closure is exactly what P0589 hashes into claims.input_closure_digest"}
{"plan":559,"title":"castore_fuse/{tree,open,circuit}.rs (Directory-DAG tree; per-digest inodes; Duration::MAX ttl + READDIRPLUS/CACHE_DIR/CACHE_SYMLINKS/PARALLEL_DIROPS; FOPEN_PASSTHROUGH on cache-hit via mountd broker)","deps":[550,567,568,570,572,573,577,588],"crate":"rio-builder","priority":80,"status":"DONE","complexity":"MED","note":"tree+open+circuit+mountd_client+CastoreFs Filesystem impl; open() resolves chunk-coords server-side via ReadBlob (StatBlob/streaming is P0575); fuser git-pinned for the raw backing_id reply API; config fields + setrlimit deferred to P0560's mount sequence; exercised end-to-end by vm-castore-e2e (P0560§B)"}
{"plan":571,"title":"mountd-owned /var/rio/cache LRU sweep + per-build staging + cache-hit metrics","deps":[559,567],"crate":"rio-builder,infra","priority":80,"status":"DONE","complexity":"LOW","note":"cache is mountd-owned readonly (HOLE fix); flock orphan detection. cluster-wide shared-FS cache REJECTED — builder air-gap"}
{"plan":575,"title":"streaming open() for files > STREAM_THRESHOLD (StatBlob → ChunkMeta[]; during-fill KEEP_CACHE; priority-bump read; Promote on completion)","deps":[559,570,571],"crate":"rio-builder","priority":80,"status":"DONE","complexity":"LOW","note":"castore_fuse/stream.rs (FillState + windowed chunk fill) + the open.rs case-(c) dispatch; first-chunk reply gate, node-chunk-cache-first sourcing, per-chunk + whole-file verify, PromoteChunks batches; inline-manifest StatBlob FailedPrecondition falls back to a streaming ReadBlob; cold-read latency assertion is vm-castore-e2e (P0560§B)"}
{"plan":560,"title":"[ATOMIC] castore-FUSE cutover: §A mount+overlay+DELETE old-FUSE (~-4600 LoC) §B fixture kernel + vm:castore-e2e + spike-regression cherry-pick","deps":[576,557,559,567,571,575,589],"crate":"rio-builder,nix","priority":80,"status":"DONE","complexity":"HIGH","note":"landed 2026-05-28: castore_fuse/session.rs (not mount.rs), executor/controller/helm/module wiring incl. the assignmentHmac mount family, mountd peer gate reduced to socket DAC, vm-castore-e2e + matrix-wide fixture tenancy stopgap (see reconciliation notes); audit follow-up is P0562, product tenancy is Phase 8"}
{"plan":562,"title":"Post-cutover audit (tracey builder.fuse.* empty; grep clean incl. cachefiles/boot_blob; checks re-run)","deps":[560],"crate":"nix","priority":80,"status":"DONE","complexity":"LOW","note":"CUTOVER GATE; JIT rules/strings gone — builder.fuse.* deliberately keeps 6 castore-bound rules; parity verified at lifecycle refs-end-to-end; obsolete i115 QA scenario deleted; see reconciliation note"}
{"plan":563,"title":"Metrics: digest-fuse + tiered dashboards + alerts","deps":[544,548,559],"crate":"infra","priority":70,"status":"UNIMPL","complexity":"LOW","note":""}
{"plan":564,"title":"helm cleanup + mountd DS wiring + kernel assertion (drop smarter-device-manager entirely)","deps":[554,560,567],"crate":"infra,rio-controller,nix","priority":75,"status":"UNIMPL","complexity":"LOW","note":"builders privileged:false; DELETE device-plugin.yaml + both NodeOverlays + nixos-node/smarter-device-manager; kvm via hostPath CharDevice + nodeSelector + extra-sandbox-paths (vm-kvm-hostpath-spike PASS)"}
{"plan":565,"title":"Cutover runbooks (cache-tier, castore-FUSE)","deps":[555,562,564],"crate":"docs","priority":65,"status":"UNIMPL","complexity":"LOW","note":""}
{"plan":572,"title":"Directory merkle layer: dir_digest/root_digest in NarIndex + directories+file_blobs tables + nar_index.root_node column + bottom-up compute in castore.rs","deps":[545,546,551,552],"crate":"rio-proto,rio-store","priority":90,"status":"DONE","complexity":"LOW","note":"LOAD-BEARING for P0559 mount path (ADR §2.2); also U5 foundation; snix castore.proto vendored (MIT); pin canonical encoding (snix #111); pass lives in rio-store not rio-nix (rio-nix can't depend on rio-proto)"}
{"plan":573,"title":"DirectoryService RPC: GetDirectory(recursive=true server-BFS stream) / HasDirectories / HasBlobs (batch bitmap; I-110 lesson)","deps":[572,589],"crate":"rio-proto,rio-store","priority":90,"status":"DONE","complexity":"MED","note":"snix-wire-compatible; recursive=true is the P0559 mount-time prefetch path; tenant-scoping reads claims.tenant (P0589)"}
{"plan":577,"title":"BlobService.Read(file_digest) server-stream (snix-compatible; file_blobs→nar_index size→chunk-cumsum slice)","deps":[573],"crate":"rio-proto,rio-store","priority":80,"status":"DONE","complexity":"LOW","note":"completes castore surface; r[store.castore.blob-read]"}
{"plan":574,"title":"Gateway substituter: Directory-DAG delta-sync client (nix copy walks DAG, prunes present subtrees)","deps":[573,577],"crate":"rio-gateway,nix","priority":75,"status":"UNIMPL","complexity":"MED","note":"U5 LANDS; falls through to chunk-list when remote lacks capability"}
{"plan":590,"title":"Explicit tenancy: default tenant migration + empty key-comment/tenant_name resolve to it (NULL tenant retired at API boundary)","deps":[560,589],"crate":"rio-migrations,rio-gateway,rio-scheduler,docs","priority":85,"status":"UNIMPL","complexity":"LOW","note":"Phase 8; sequenced after the P0560 cutover merges; no anonymous access decision 2026-05-28"}
{"plan":591,"title":"Ownership at ingest: path_tenants written by PutPath/PutPathBatch/PutPathChunked (JWT|claims.tenant|service+header) + scheduler input-closure upsert + default-tenant backfill","deps":[590,586],"crate":"rio-store,rio-gateway,rio-scheduler,rio-migrations","priority":85,"status":"UNIMPL","complexity":"MED","note":"makes client-uploaded sources/.drvs visible to tenant-scoped castore reads; replaces the P0560 fixture trigger"}
{"plan":592,"title":"No anonymous access: mandatory HMAC keys, drop unsigned assignment tokens, retire anonymous-unfiltered read carve-outs + anon .drv lookup","deps":[590,591],"crate":"rio-store,rio-gateway,rio-scheduler,nix,infra,docs","priority":75,"status":"UNIMPL","complexity":"MED","note":"spec rework: store.tenant.narinfo-filter, gw.jwt.anon-drv-lookup, single-tenant-mode language removed"}
{"plan":593,"title":"Test estate on explicit tenancy: fixtures default to tenant+keys, delete standalone defaultTenant narinfo trigger, sweep standalone+k3s scenarios","deps":[591,592],"crate":"nix","priority":75,"status":"UNIMPL","complexity":"LOW","note":"deletes the P0560 stopgap (defaultTenant flag, tenantStopgapSeedSql triggers, tenant key comments, sla-sizing SLA_TENANT shim) once P0591/P0592 make it redundant; the build-running scenarios are already green via the stopgap"}
```

---

## tracey `r[…]` marker inventory (P0544 writes spec; later phases write impl/verify)

> **Spec-file column is the planned canonical location.** Where a marker pre-existed in `decisions/022-design-overview.md` §4–§15 (the canonical design reference, in tracey scope as of P0544), it stays there rather than being duplicated; `tracey query rule <id>` shows the actual defining file. ADR-022 §6 (chunked upload) and ADR-023 (tiered backend) carry their own markers. Component spec files carry the markers not covered by the ADR docs.
>
> **Rows are removed once the rule has both an `r[impl]` and an `r[verify]` in the tree** — `tracey query rule <id>` is then authoritative and a planned-location row can only drift from it. What remains below is the planned coverage for rules that are still uncovered, still untested, or not yet written into the spec.

| Marker | Spec file (P0544) | `r[impl]` (plan) | `r[verify]` site (plan) |
|---|---|---|---|
| `store.put.builder-chunked-only` | components/store.md | grpc/put_path/mod.rs token-role gate (P0584) | unit (P0584) |
| `store.index.authoritative` | decisions/022-design-overview §5 | metadata/mod.rs `complete_manifest_in_conn` (P0557/P0586) | rio-store/tests/grpc/nar_index.rs `put_path_writes_castore_index_atomically` |
| `store.index.putpath-eager` | components/store.md | cas.rs `ParsedNar` + metadata/mod.rs complete txn (P0557) | vm-protocol-warm (P0557) |
| `builder.fs.castore-stack` | decisions/022 §2.1 | castore_fuse/session.rs (P0560§A) | vm-castore-e2e `cold-build` (P0560§B) |
| `builder.fs.castore-dag-source` | decisions/022 §2.2 | castore_fuse/tree.rs (P0559) | vm-castore-e2e (P0560§B) |
| `builder.fs.castore-inode-digest` | decisions/022 §2.3 | castore_fuse/tree.rs (P0559) | unit (P0559) + vm-castore-e2e `inode-dedup` (P0560§B) |
| `builder.fs.castore-cache-config` | decisions/022 §2.4 | castore_fuse/mod.rs init (P0559) | unit (P0559) + vm-castore-e2e `stat-dcache-absorbed` (P0560§B) |
| `builder.fs.fd-handoff-ordering` | decisions/022 §2.5 | castore_fuse/session.rs (P0560§A) | vm-castore-e2e (P0560§B) |
| `builder.fs.digest-fuse-open` | decisions/022 §2.6 | castore_fuse/open.rs (P0559) | vm-castore-e2e `cold-build` (P0560§B) + unit (P0559) |
| `builder.fs.passthrough-on-hit` | decisions/022 §2.6 | castore_fuse/open.rs (P0559) | vm-castore-e2e `passthrough-small`+`warm-read` (P0560§B) |
| `builder.fs.passthrough-stack-depth` | decisions/022 §2.9 | castore_fuse/mod.rs init (P0559) | composefs-spike-priv `passthrough-under-overlay` (P0578) |
| `builder.fs.file-digest-integrity` | decisions/022 §2.7 | castore_fuse/open.rs (P0559) | vm-castore-e2e `integrity-fail` (P0560§B) |
| `builder.fs.fetch-circuit` | components/builder.md | castore_fuse/circuit.rs (P0559) | vm-castore-e2e `eio-on-fetch-fail` (P0560§B) |
| `builder.fs.node-digest-cache` | components/builder.md | castore_fuse/sweep.rs (P0571) | sweep.rs `evicts_oldest_first_and_stops_at_target` (P0571); vm-castore-e2e `cross-build-dedup` (P0560§B) |
| `builder.fs.node-chunk-cache` | decisions/022 §2.6 | castore_fuse/stream.rs (P0575) + bin/rio-mountd.rs (P0567) | stream.rs `streaming_fill_assembles_verifies_and_promotes` (P0575); vm-castore-e2e `cross-build-dedup-streaming` (P0560§B) |
| `builder.fs.shared-backing-cache` | decisions/022 §2.6 | castore_fuse/open.rs (P0559+P0571) | vm-castore-e2e `cross-build-dedup` (P0560§B) |
| `builder.fs.streaming-open` | components/builder.md | castore_fuse/stream.rs (P0575) | stream.rs `first_chunk_unblocks_open_before_fill_completes` (P0575); vm-castore-e2e `cold-build` (P0560§B) |
| `builder.fs.streaming-open-threshold` | decisions/022 §2.8 | castore_fuse/open.rs (P0575; the config.rs field lands with P0560's mount sequence) | vm-castore-e2e `cold-build` (P0560§B) |
| `gw.substitute.dag-delta-sync` | components/gateway.md | rio-gateway/substitute/dag_sync.rs (P0574) | vm-dag-delta-sync (P0574) |
| `builder.result.input-eio-is-infra` | components/builder.md | executor/mod.rs (P0560§A, ported) | vm-castore-e2e `eio-on-fetch-fail` (P0560§B) |
| `sec.boundary.mountd` | security.md | bin/rio-mountd.rs (P0567) | vm-mountd `gid-gate`+`traversal-reject`+`uid-bound` (P0567) |
| `builder.fs.listxattr-size-branch` | components/builder.md | castore_fuse/mod.rs (P0559) | vm-castore-e2e `shutil-copy2` (P0560§B) |
| `obs.metric.mountd` | observability.md | castore_fuse/mountd.rs (P0567) | vm-castore-e2e (P0560§B) |
| `builder.overlay.castore-lower` | components/builder.md | overlay.rs (P0560§A) | vm-castore-e2e (P0560§B) |
| `builder.fs.parity` | decisions/022 §4 | (verify-only) | lifecycle `refs-end-to-end` (P0562) |
| `store.compat.runtime-toggle` | components/store.md | config.rs (P0579) | unit + vm-store-compat `compat-off-no-narinfo` (P0566+P0580) |
| `store.compat.nar-on-put` | components/store.md | compat/writer.rs (P0566) | unit (P0566) |
| `store.compat.narinfo-on-put` | components/store.md | compat/writer.rs (P0566) | unit (P0566) |
| `store.compat.write-after-commit` | components/store.md | grpc/put_path/ (P0566) | unit (P0566) |
| `store.compat.stock-nix-substitute` | components/store.md | (verify-only) | vm-store-compat `stock-nix-substitute` (P0580) |
| `store.compat.gc-coupled` | components/store.md | gc/sweep.rs (P0581) | rio-store/tests/gc.rs (P0581) |
| `obs.metric.compat` | observability.md | rio-store/lib.rs (P0566) | vm-store-compat (P0580) |
| `obs.metric.chunk-backend-tiered` | observability.md | rio-store/lib.rs (P0548) | vm-store-tiered (P0555) |
| `obs.metric.castore-fuse` | observability.md | rio-builder/lib.rs (P0559) | vm-castore-e2e (P0560§B) |
| `infra.express.cache-tier` | decisions/023 | infra/eks/s3-express.tf (P0553) | (live-only — runbook P0565) |
| `infra.express.bounded-eviction` | 022-design-overview §9 | backend/express_sweep.rs (P0585) | unit (P0585) |
| `obs.metric.express-eviction` | observability.md | backend/express_sweep.rs (P0585) | unit (P0585) |
| `infra.node.kernel-fuse-passthrough` | deployment.md | nix/nixos-node/kernel.nix (prereq) | nix/checks.nix node-kernel-config (prereq) |
| `store.put.chunked` | decisions/022 §6 | rio-proto/store.proto + grpc/put_path_chunked.rs (P0586) | vm-put-path-chunked (P0586) |
| `store.chunk.has-chunks-durable` | decisions/022 §6.2 | grpc/chunk.rs has_chunks (P0586) | vm-put-path-chunked (P0586) |
| `store.chunk.durable-flag` | components/store.md | grpc/chunk.rs + put_path*.rs complete-txn (P0586) | vm-put-path-chunked `HasChunks-false-during-WAL` (P0586) |
| `store.chunk.self-verify` | decisions/022 §6.2 | grpc/put_path_chunked.rs (P0586) | vm-put-path-chunked (P0586) |
| `store.put.chunked-wire` | decisions/022 §6.2 | proto/store.proto (P0586) | vm-put-path-chunked (P0586) |
| `store.put.chunked-bounds` | decisions/022 §6.2 | put_path_chunked/validate.rs (P0586) | vm-put-path-chunked vii (P0586) |
| `store.put.narhash-sync` | decisions/022 §6.3 | put_path_chunked/mod.rs (P0586) | vm-put-path-chunked (P0586) |
| `store.put.refs-sync` | decisions/022 §6.3 | put_path_chunked/mod.rs (P0586) | vm-put-path-chunked x (P0586) |
| `store.put.chunked-ca` | decisions/022 §6.3 | put_path_chunked/mod.rs (P0586) | vm-put-path-chunked viii (P0586) |
| `builder.upload.fused-walk` | decisions/022 §6.1 | rio-builder/upload.rs (P0586) | vm-put-path-chunked (P0586) |
| `builder.upload.chunked-manifest` | decisions/022 §6.1 | rio-builder/upload.rs (P0586) | vm-put-path-chunked (P0586) |

(`composefs-stack`/`userxattr-mount`/`stub-isize`/`metacopy-xattr-shape`/`composefs-encode`/`erofs-handoff` retired; `castore-{stack,dag-source,inode-digest,cache-config}`/`fuse-handoff` added.) P0560 DELETES legacy `r[builder.fuse.*]`; P0562 audits via `tracey query uncovered | grep -E 'castore|tiered|index|digest-fuse|compat'` → empty.
`config.styx` `test_include`: P0544 verifies `rio-nix/src/nar/` and `rio-builder/src/castore_fuse/` are in scope (or adds them).

---

## Rollback (one-flag for cache tier, greenfield for builder)

| Layer | Rollback | How |
|---|---|---|
| Tiered cache → direct-S3 | `store.chunkBackend.kind=s3` (helm) | Single flag, instant + lossless — S3 was always authoritative. |
| castore-FUSE → old-FUSE | **none** (old-FUSE deleted at P0560) | `xtask k8s eks down && up` from a pre-P0560 commit. Greenfield principle. |

**Helm assertion** (`_helpers.tpl`, P0564): `{{- if and .Values.karpenter.enabled (not (has "FUSE_PASSTHROUGH" .Values.karpenter.amiKernelFeatures)) }}{{ fail "AMI must be built with nix/nixos-node/kernel.nix (≥6.9, FUSE_PASSTHROUGH=y); run xtask ami push" }}{{- end }}`.

---

## File-collision matrix (for `onibus collisions check`)

| File | Touched by | Serialisation |
|---|---|---|
| `rio-store/src/backend.rs` → `backend/{mod.rs,tiered.rs}` | P0548 (`git mv backend.rs backend/mod.rs` + add `tiered.rs`), P0549 | P0548 → P0549 (dep edge) |
| `rio-store/src/grpc/mod.rs` | P0552, P0557 | P0552 → P0557 (dep edge) |
| `rio-store/src/grpc/put_path/` | P0566, P0557, P0583, P0584 | P0583 (size-branch removal) first; P0584 (token-role gate, top of handler) independent of P0566/P0557 (append after `complete_manifest`) — P0557 rebases on P0566; P0566 deps include P0583 (jsonl encodes this) |
| `rio-store/src/grpc/get_path.rs` | P0583 only | — |
| `rio-store/src/metadata/{inline.rs,chunked.rs,mod.rs}` | P0583 only | — |
| `rio-store/src/nar_index.rs` | P0552 (create), P0572 (directories insert), P0557 (eager) | P0552 → P0572 → P0557 |
| `rio-store/src/lib.rs` | P0548, P0552, P0557 | append-only metric registrations; dep chain serialises |
| `rio-builder/src/castore_fuse/{tree,open,stream}.rs` | P0559 (create), P0571 (cache metrics), P0575 (streaming) | P0559 → P0571 → P0575 |
| `rio-builder/src/castore_fuse/mountd_proto.rs` | P0567 (create), P0559 (consume) | P0567 → P0559 |
| `rio-builder/src/bin/rio-mountd.rs` | P0567 (create), P0571 (LRU sweep) | P0567 → P0571 |
| `rio-builder/src/castore_fuse/mod.rs` | P0567, P0559, P0560§A | append-only `pub mod`; P0560 last |
| `rio-builder/src/store_fetch.rs` | P0550 (create), P0568 (batched client), P0559 (call) | P0550 → P0568 → P0559 |
| `rio-proto/build.rs` | P0568 only | — |
| `rio-builder/src/overlay.rs` | P0560 only | — |
| `nix/tests/default.nix` | P0555, P0560§B, P0562 | append-only scenario entries |
| `nix/tests/fixtures/k3s-prod-parity.nix` | P0555, P0560§B | P0555 adds args; P0560§B adds unconditional kernel.nix import |
| `infra/helm/rio-build/values.yaml` | P0554, P0564 | distinct top-level keys |
| `rio-controller/src/reconcilers/common/sts.rs` | P0564 only | — |
| `nix/nixos-node/eks-node.nix` | P0564, P0571 | distinct hunks (drop smarter-device-manager static-pod vs tmpfiles) |
| `migrations/062_nar_index.sql` | P0551 (creates the full ADR-022 castore schema) | P0581/P0586 schema is **pre-created** in 061 — those plans add only code, no DDL; pinned once. P0583's `DROP COLUMN inline_blob` is a separate migration (lands with the code change that stops reading it). |
| `migrations/063_file_blob_size.sql` | P0570/P0577 | `file_blobs.size` denormalization — the read path never decodes `nar_index.entries`. |
| `migrations/064_directory_paths.sql` | tenancy rework (`d9d78a0e`, post-P0572) | replaces `directory_tenants`/`file_blob_tenants` with `directory_paths` + read-time `path_tenants` joins — see the P0572 reconciliation note. |
| `rio-proto/proto/types.proto` | P0545, P0572 | P0545 → P0572 (append fields 7, 8) |
| `rio-proto/proto/store.proto` | P0568, P0570, P0573, P0577, P0586 | append-only RPC additions; no ordering constraint |
| `rio-builder/src/upload.rs` | P0586 only (rewrite) | — |
| `rio-store/src/grpc/chunk.rs` | P0568, P0586 | P0568 → P0586 (HasChunks appended) |
| `rio-nix/src/nar/` | P0546, P0572 | P0546 → P0572 (second pass in same fn) |
| `rio-auth/src/hmac.rs` | P0589 only | — |
| `rio-scheduler/src/actor/dispatch.rs` | P0588, P0589 | P0588 → P0589 |
| `docs/src/security.md` | P0544, P0589 | P0544 → P0589 (`r[common.hmac.claims]` text) |
| `rio-store/src/grpc/directory.rs` | P0573 (create), P0570 (StatBlob), P0577 (ReadBlob) | P0573 → P0570/P0577 |
| `rio-gateway/src/substitute/` | P0574 only | — |
| `infra/helm/rio-build/templates/mountd-ds.yaml` | P0567 (create), P0564 (wire values) | P0567 → P0564 |

---

## Commands cheat-sheet

```bash
# Phase 0 spikes (P0569 already DONE; cherry-pick its tests)
git -C ../main/.claude/worktrees/agent-acf26042 log --oneline -3
nix build .#checks.x86_64-linux.vm-composefs-spike-priv  # P0541

# Phase 0 measurement
nix develop -c cargo xtask measure v12 --closure chromium

# Phase 3 cache-tier flip (FIRST SHIPPED VALUE)
nix develop -c cargo xtask k8s -p eks tofu apply -target=aws_s3_directory_bucket.cache
nix develop -c cargo xtask k8s -p eks down && nix develop -c cargo xtask k8s -p eks up
nix develop -c cargo xtask k8s -p eks grafana   # watch tiered_local_hit_ratio climb

# Phase 5 castore-FUSE cutover (greenfield — old-FUSE deleted at P0560)
nix develop -c cargo xtask k8s -p eks down
nix develop -c cargo xtask k8s -p eks up   # from a P0562-green commit
nix develop -c cargo xtask k8s -p eks rsb -- nixpkgs#chromium

# Any-phase CI gate
/nixbuild --checks

# Cache-tier rollback (instant + lossless)
helm upgrade rio infra/helm/rio-build --reuse-values --set store.chunkBackend.kind=s3
# Builder-side rollback: down && up from a pre-P0560 commit
```

---

## Explicitly deferred (out of scope)

- Non-reproducibility `nar_hash` mismatch detection at PutPath
- Per-replica chunk-dedup metrics
- aarch64-specific mount validation (proptest covers; live aarch64 builder is the soak; P0543 covers config-eval only)
- **Kernel `BACKING_OPEN` `d_is_reg` relaxation** — [`backing.c:105-108`](https://github.com/torvalds/linux/blob/master/fs/fuse/backing.c) rejects block-device fds. If lifted upstream, `ublk`-per-giant becomes a viable shared-verified-partial primitive (still needs chunk-addressed verify underneath, but would let B passthrough A's in-progress fill).
- **fs-verity on the backing cache** — would give kernel-side integrity for warm passthrough reads from `/var/rio/cache/`. The cache is already real ext4/xfs hostPath; mountd would `FS_IOC_ENABLE_VERITY` after the `Promote` rename so passthrough reads are kernel-verified. Followup after P0562.
- **Cross-region deployment** — globally-consistent metadata store, object-store cross-region replication, per-region cache tiers. This plan ensures forward-compat (object-store-authoritative, cache tier stateless) but does not implement it.

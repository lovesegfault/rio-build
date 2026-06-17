# ADR-024: Native build protocol — `rio build` (second iteration)

Status: Proposed. Supersedes the first iteration of this ADR, preserved on branch `adr-024-v1`.

Requires: [ADR-022](./022-design-overview.md) — everything here builds on the castore model (Directory DAGs, per-file FastCDC chunking, NAR framing regenerated on read, tenant-scoped reads). This ADR adds the client and the protocol, not a storage model.

---

## Context

The only way to submit builds today is `nix build --store ssh-ng://<gateway>`: the legacy Nix daemon wire protocol over SSH. Measured against the live cluster (protoperf campaign, Leg B): **57s to first build start cold, 36s warm, of which 11s is SSH auth alone**. The protocol is serial, sends sources as raw uncompressed NARs with zero transfer dedup, and multiplexes everything over one SSH channel.

The first iteration of this ADR built the `rio://` eval-store plugin (M0) and proved the architecture:

- **Identity works.** Byte-identical drvPaths versus stock nix under real eval, with hard-fail cross-checks on every object. The lineage has since been re-proven at scale: 48,223/48,223 store derivations round-trip byte-identically with exact output-path identity recompute (DRVPROTO verification).
- **The bridge works.** A Rust core staticlib behind a thin C++ `nix::Store` shim, fork-safe, loaded into flake-pinned nix. This survives unchanged into the second iteration.

What the first iteration got fatally wrong was the client-side storage design. Measured (protoperf Leg A): a warm flake eval through the plugin ran **92× slower than stock nix** (85.3s vs 0.92s) — because every `lstat`/`readDir` during eval re-parsed a monolithic 8.9MB JSON source-DAG blob, with serde deserialization alone burning 35–60% of cycles. That is not a codec problem; it is a no-cache, wrong-granularity problem. This iteration fixes the granularity (per-directory objects), adds the cache, and aligns the local format with the wire format so that upload becomes pure digest negotiation.

Every decision below is backed by measurements taken this cycle on real data: the captured 8.9MB / 88,483-entry nixpkgs source DAG, real nixpkgs day-bump revision pairs, the 4,901-drv bench graph, and the full 48k-drv store corpus (evidence: the dagbench campaign — ANALYSIS, RESULTS, DRV-TRANSFER, DRV-CORPUS, DRVPROTO, CRITIQUE-VERDICTS — plus the protoperf report). The design also absorbed a 28-claim adversarial review (CRITIQUE-VERDICTS: 16 confirmed, 7 partial, 4 refuted); every confirmed finding is folded into the text below.

## Decision

Everything that moves between client and cluster is a content-addressed object keyed by `blake3(canonical bytes)`, 32 raw bytes, in one shared digest space: file-content **chunks** (FastCDC 16/64/256 KiB, unchanged from iteration one), source-tree **directories** (castore proto, per-directory blobs), and **derivations** (canonical rio-proto `Derivation` blobs). One protocol verb covers all three: the client sends the digest list it needs the cluster to have, the cluster answers a presence bitmap, the client uploads only the misses, zstd-framed. Build submission is a skeleton graph of digests — no derivation content rides in `SubmitBuild`. Derivations never touch the client disk: they live in eval-process memory and ship only when negotiation asks. The scheduler keeps drv blobs in rio-store like any other object, so a warm cluster receives kilobytes per rebuild, and the cold worst case (full 4.9k-drv graph, every miss uploaded and acked before submit) costs ~1.05s on a laptop link versus today's 57s.

### Identity rules

The core identity rules from iteration one survive: output paths remain input-addressed, `hashDerivationModulo` must match Nix byte-for-byte, ATerm exists only inside that hash computation, FastCDC constants for file content are the existing workspace values shared with the builder and store, and presence is a read (see Security below).

What changes is the canonical derivation form. **The canonical stored/negotiated form is a rio proto `Derivation` message under the castore canonical-encode rule** (sorted fields, defaults omitted, golden-tested) — NOT nix's derivation JSON, as iteration one chose. The decisive argument: negotiation keys on `blake3(canonical bytes)`; if those bytes are nix-emitted JSON, the digest space is coupled to nix's serializer, and a nix upgrade re-keys every derivation and destroys cross-version dedup. Measured: a mere pretty-vs-compact JSON formatting change re-keys 100% of digests, while the proto encoding is invariant (DRVPROTO). The proto form is also what the gateway parses on every upload — proto decode measured 3.8µs/drv vs 7.0µs for JSON, full cross-check (parse + `hashDerivationModulo`) 12–23µs/drv.

Verification gate, passed: **48,223/48,223** derivations (every `.drv` in a real store, superset of the bench corpus, including 21,518 structuredAttrs and 393 non-ASCII drvs) round-trip ATerm → proto → ATerm byte-identically, with `.drv` store paths and all 45,692 input-addressed plus 21,172 fixed-output output paths recomputed exactly — externally anchored to nix-minted on-disk paths (DRVPROTO). One honest negative: the raw size win over ATerm inverts under stream zstd, so size is NOT an argument for this choice and is deliberately not cited.

Schema judgment calls (full list in DRVPROTO): all string-ish fields are `bytes` (non-UTF-8 is legal in drvs); structuredAttrs stays an opaque `__json` env pair (no second canonicalization problem); no proto3 maps (sorted repeated pairs instead); hostile unsorted/dup-key inputs fail the gateway cross-check rather than silently re-keying.

JSON is demoted to a display projection (`rio drv show`). The gateway recomputes drv_path from received content and hard-errors on mismatch, and additionally re-encodes the parsed message under the canonical-encode rule and byte-compares with the received blob — non-canonical byte-variants (whitespace/ordering/dup-key games would otherwise yield unbounded digests per drv_path) are rejected, not stored. Hashing stays over received bytes, never a re-serialization.

### Derivations are memory-only client-side

A `HashMap<StorePath, Bytes>` in the eval store, gone at process exit. No drv blobs in the client CAS: drvs are a few KB (below the FastCDC minimum — chunking never applies), recomputed deterministically by every eval, and all consumers are in-process. With fork workers this means skeleton assembly happens *in the worker* before it reports — drv bytes die with the worker.

The residency bill lands on the **coordinator**, not the workers: a cold 100k-drv eval accumulates ~0.3–1GB of drv bytes there (3.4KB mean, fat p99 tail) until upload acks drain. Bytes are dropped on ack, so the coordinator retains only un-acked content for re-upload after a disconnect.

### Source-DAG metadata: per-directory castore-proto blobs + decoded cache

The replacement for the monolithic blob that caused the 92×: each directory is its own castore-proto blob, with a `digest → Arc<DecodedDir>` in-process cache in front. Bench-confirmed (dagbench RESULTS): 42ms on the 2,561-op warm trace against a 92ms budget, 2.7ms steady-state, 6.6MB RSS. The cache is structural, not an optimization: from a parsed handle every format does lookups in ~100ns–1µs; from raw bytes nothing acceptable exists at monolithic granularity.

The granularity is also what makes upload dedup work. Measured on a real nixpkgs day-bump (0.9% churn): per-directory structural dedup re-uploads **1.39MB**, while **every monolithic CDC layout tested re-uploads 100%** — one inserted entry shifts every downstream byte, redirtying every chunk. A single file edit dirties 6 blobs, 8KB.

Two bench-forced storage rules:

- **Pack files, not loose blobs.** Per-blob fsync cost 113.7s for one cold nixpkgs ingest on overlayfs; append-pack + single fsync = 49ms. Loose-blob layouts are disqualified on fsync-honest filesystems.
- **Compress packs, not blobs.** Per-blob zstd is a net loss at 140B mean blob size. zstd-at-rest applies at pack level (or per large record), never per blob.

### The client CAS: a fetch cache plus an index, not a mirror

The disk CAS stores: fetched content (flake inputs, fetchTree/fetchurl results, IFD outputs, daemonless fetch results — bytes with no other local home), the fingerprint index plus cluster-ack table (the persistent asset: a second invocation skips hashing on fingerprint hits and skips negotiation on ack hits), and the metadata packs (4.6MB / 49ms for nixpkgs — cheap, instant readback).

**Not stored: chunk copies of local working trees.** The origin tree is the byte store. Upload-on-miss re-reads the origin, re-chunks, verifies the digest still matches, then uploads; a mutated file fails the check → re-ingest → re-negotiate the delta — at most twice: after two failed re-ingests the tree is snapshotted into the CAS (copy-on-read, the escape hatch) and uploaded from the snapshot, so a tree mutated faster than the re-ingest time (~38s at nixpkgs scale) cannot livelock the loop. Origin deleted between negotiate and upload ⇒ that root fails with a named error; other roots proceed. This deletes most cold-ingest write I/O (the first iteration wrote ~54k blob copies per cold run). Not stored either: derivations (memory-only, above).

**Racy-fingerprint rule (git's rule).** The kernel files timestamps from a ~10ms coarse clock; a same-size in-place rewrite within the tick keeps the full `(size, mtime_ns, inode, ctime_ns)` fingerprint **99.2–99.9% of the time** (measured on overlayfs and tmpfs) — a fingerprint hit on stale content, and when the cluster already holds the stale digest, no code path ever re-reads the new bytes: a silent wrong-content build. So every fingerprint record stores its own write wall-time, and lookup distrusts (re-hashes) any entry with `mtime_ns ≥ record_write_time − slack`, slack ≥ the measured 10ms tick. Warm-path cost ≈ 0: only files modified within the same tick as their record re-hash, once.

#### Pack store layout, GC, repack

No loose objects anywhere. Hardened by the adversarial review (C7–C11, C25):

- **Write path**: each process appends to its own segment pack. Records are self-describing and resyncable (`magic, kind, len, digest, bytes`) — packs alone can rebuild everything else, and after a corrupted length the rebuild scanner resyncs forward to the next magic and verifies the digest (a mid-pack hole drops one record, not the tail). Each writer holds a **shared flock on its own segment** for the segment's lifetime; GC/repack skips any pack whose flock it cannot take exclusively — a live writer's segment is never repacked or unlinked out from under it.
- **Index**: one file, `digest → (pack, offset, len, kind)` plus the root table (store-path entries with last-use timestamps — the LRU clock moves from per-file `utimensat` into batched index records). Rewritten by **load + merge + rename under the exclusive flock**: re-read the on-disk index, union with the in-memory view, then rename — a plain write-new + rename silently discards a concurrent process's entries (demonstrated: 1000/1000 lost), and lost root-table entries mean the next GC mark misses those roots, i.e. data loss for fetched content. Corrupt or missing index = rebuild by scanning packs; a torn tail record fails its digest check and is truncated — but only by a process holding the exclusive GC flock that can ALSO take the segment's writer flock exclusively (owner provably gone). The index is a cache of the packs, never the truth.
- **GC = mark + repack, one mechanism**, under the existing exclusive advisory lock at open time (no daemon, no background thread): mark live blobs from live roots (LRU/size budget decides which roots survive; eviction unit stays the store-path entry; the 6h in-flight grace window is kept), then repack **one source segment at a time** — copy live records into the consolidated pack, swap the index, unlink that segment, repeat. Transient disk is one segment of headroom, not ~2× live bytes — the all-at-once variant ENOSPCs exactly when the size cap fires and the store can no longer shrink itself. Readers holding old fds are POSIX-safe; nothing is mutated in place, ever.
- **Triggers**, git-gc-auto style, checked cheaply at open: segment count > 64, or **approximate** dead-bytes ratio > 50%. The counter (`approx_dead += Σ bytes referenced by an evicted root`) overestimates by the shared fraction (~98% blob sharing across roots measured); worst harm is one spurious repack, and the mark inside GC remains ground truth. Size-cap pressure evicts LRU roots first, then repacks.

### Upload: build-referenced objects only, tenant-scoped presence

Sources and inputs upload through the existing castore surface: `HasBlobs`/`HasDirectories`/`HasChunks` bitmaps → upload misses. Negotiation and upload cover only **build-referenced** objects (inputSrcs closures + drv blobs), never every tree eval touched — this cap is what makes tenant-scoped presence affordable. The local format shares rio-store's digest space by construction (same bytes, same canonical-encode rule, golden-tested), so no conversion layer and no double hashing exist on the upload path.

**Tenancy** (settled by the adversarial review, C2): presence is **tenant-scoped for all three object kinds**. The fresh-tenant penalty is ≈0.2s per cold build (0.29MB zstd inputSrcs + 0.99MB drv stream at 50Mbps), not the ~30s a whole-nixpkgs re-upload would cost — package sources are FOD-fetched in-cluster, never client-uploaded. Shipped reality today: `HasChunks` is deliberately cross-tenant (a documented accepted trade-off from ADR-022 implementation) while `HasBlobs`/`HasDirectories` are tenant-scoped; **this ADR flips `HasChunks` to tenant-scoped** (P2 owns it), reconciling the policy across object kinds. Dedup-at-rest is unaffected either way: puts are write-through idempotent and chunk storage is digest-keyed global, so tenant-scoped presence costs upload bandwidth only, never storage. Dedup claims in this document hold within one tenant.

**Security: presence is a read** (unchanged from iteration one). The negotiation RPC is an oracle for what exists in the store; it sits behind the same tenant auth as uploads, and answers **missing** for anything the calling tenant cannot see, even when the bytes exist. The duplicate upload binds safely because upload verification already treats client-claimed digests as untrusted. If cross-tenant dedup ever matters, the upgrade path is proof-of-possession, never a global presence oracle.

### Build plan: Merkle negotiation over drv digests

`hashDerivationModulo` makes the drv graph a Merkle DAG, but the negotiation key is `blake3(canonical proto Derivation bytes)` — drv_path and the modulo hash remain verified payload fields, not keys, so the bitmap-presence RPC shape is reused verbatim across chunks, directories, and drvs.

- **One flat bulk `Has` over the full closure digest list.** The client knows the closure post-eval; the level-walk variant pays depth×RTT for nothing — empirically dead on the real corpus: DAG depth is 233 (not the ~30 the napkin assumed), so the walk costs 233 RTTs ≈ **9.4–10.3s at 40ms RTT** versus 1 round-trip flat (DRV-CORPUS).
- **Drv content becomes a castore blob kind**: durable, build-pinned GC, the scheduler DB stops being an accidental drv store, and the 16MB `SubmitBuild` budget becomes skeleton-only. Drv puts inherit the existing write-through idempotent discipline (unconditional write, idempotent overwrite — no present/absent timing oracle).
- **Edges ride as `input_drv_digests` inside nodes**; the explicit edge list and the inline `drv_content` field retire — via the two-phase rollout below, not in one step.
- **Churn is bimodal in practice** (measured on real nixpkgs revision pairs): a routine 1-week bump changes 0 drvs; a median source-leaf edit re-keys 83.7% of the graph — which IS the rebuild set; a 4-week bump across a staging merge changes 57%. The Merkle cascade is a non-issue under input addressing: the digest-redirty cone equals the rebuild cone, and those drvs must ship content anyway. (Re-examine if floating-CA outputs land — see What this is not.)
- **Compress the `SubmitBuild` stream itself**: skeleton zstd ratio measured 0.235 (input-digest references repeat each node digest ~7.4× and zstd dedups the repeats); at laptop bandwidth the skeleton dominates warm submits.

### Wire changes and rollout

`dag.proto` sketch: `DerivationNode` gains `drv_digest` and `input_drv_digests`; field 9 (`drv_content`) is reserved; `DerivationEdge`/`edges` retire; the gateway's `filter_and_inline_drv` becomes `upload_missing_drvs` plus the drv_path recompute and canonical re-encode byte-compare described under Identity.

**Two-phase rollout** (adversarial review C13): P2a is additive — the new digest fields land while `edges` + `drv_content` remain accepted, and the scheduler prefers digests when present. P2b retires the old fields one release later. Single-step retirement breaks the documented "scheduler and store first" deploy order: today's scheduler accepts a no-edges submission and marks every node Ready — concurrent dependency-less dispatch, a **silent mis-build window**, not a reject.

Stale-ack recovery: if the scheduler's submit-time bulk-verify rejects with missing digests (the cluster GC'd a blob the client remembered), the client evicts those acks, re-`Has`es, uploads, and resubmits — once; a second reject is a hard error. Client-side ack records carry a TTL ≤ the cluster's minimum unpinned-blob lifetime.

### The `rio build` client

`rio build <installable>...` behaves like nix-fast-build from the outside; everything between eval and build runs the native protocol.

**Process architecture** — three layers, two binaries, boundaries at the hard constraints:

```
rio build (coordinator — pure Rust, tokio/tonic, gRPC to gateway)
   │ exec + socketpair (length-delimited proto frames)
rio-eval parent (C++ embeds libexpr + the Rust eval-store staticlib)
   │ fork-no-exec, COW
eval workers (N ≈ cores)
      · boehmgc disabled (GC_DONT_GC; measured 1.19–1.39× RSS)
      · recycled after N attrs or RSS threshold (process exit IS the GC)
```

The coordinator owns the attr work queue, the global digest state, and the cluster connection; it **never forks** — the exec boundary keeps tokio/tonic/rustls out of every process that does. Forking happens only inside the eval parent, which controls its own threads; nix-eval-jobs forks workers from collector threads in production at Hydra scale, and a 10k-fork stress under malloc churn produced zero hangs (review C6). The exec split also confines libexpr linkage to one binary. GC_DONT_GC was adversarially re-checked: 1.19–1.39× RSS across three real workloads including a deliberately huge single attr (review C21); recycling bounds the rest. Before forking, the parent locks the flake and fetches inputs once (into the CAS — workers never re-fetch), enumerates the work list, and pre-warms the decoded-dir cache, fingerprint index, and cluster-ack table from coordinator feedback. Workers never talk to the cluster.

**Sharing by fork order, not IPC**: recycling re-forks from a parent that folded workers' reported digests back in, so each generation inherits the union. Workers **borrow** the pre-warmed cache read-only (no Arc clone per op — refcount writes would COW-dirty essentially every cache page), so the sharing win holds post-churn, not just at fork time. One advisory memfd claim table (digest → claimed/done, lock-free) prevents live workers double-uploading; correctness never depends on it. A claim is `(digest, pid, timestamp)` and is stale — ignorable — once the pid is dead or the claim is older than the largest plausible tree ingest (~60s); without that rule the dedup inverts into a stall on worker crash. Non-goal: sharing nix Values/thunks across workers.

**IPC**: each edge is one `socketpair(AF_UNIX, SOCK_STREAM)` — bidirectional single fd pair, inherited across exec and fork, `SCM_RIGHTS`-capable. Stream mode, not SEQPACKET: result frames carry drv-byte bursts ≥1MB, exceeding datagram comfort and needing a length prefix anyway — so frames are length-delimited proto on the stream. Both channel ends are Rust; the C++ shim never speaks the socket. Proto not because the channel needs it but because the payloads already ARE proto: skeleton `DerivationNode`s and canonical `Derivation` bytes cross from worker memory to rio-store blob without re-encoding — byte-stability by construction, not discipline. That is the complete cross-process inventory: two queue edges (coordinator↔parent, parent↔worker), the advisory claim table, and the disk-CAS flock. No upload spool, no sibling-worker IPC, no daemon — each queue not built is a failure mode not handled.

**Per attr, a worker**: evaluates (source trees ingest through the rio:// store — one walk produces chunks + per-directory blobs + NAR hash together; the fingerprint index skips unchanged trees, modulo the racy rule); lands drvs in the in-memory map; assembles its skeleton subgraph (per drv: `drv_digest`, `input_drv_digests`, system, features, output names, plus the canonical drv bytes keyed by digest — worker-side, because drv bytes die with the worker); streams `{skeleton nodes, drv bytes, source root digests}` to the coordinator and takes the next attr.

**Ingest is single-read, two hash planes.** Eval blocks on the NAR sha256 (the store path enters string context), so ingest sits on the eval critical path. Each file is read once in NAR traversal order, feeding a sequential NAR-sha256 spine (identity) and teeing per-file into a parallel FastCDC+blake3 plane; per-directory castore blobs fold bottom-up as children complete. All ingest threads spawn lazily *after* fork — zero threads exist at fork time, preserving the fork-safety rule.

The threading model is fixed by measurement + discrete-event simulation over a real 52,855-file nixpkgs walk: **R=8 blocking reader threads off a shared dirs+files deque, one sha256 spine, W=2 chunk workers, and a single 32MiB byte-budget semaphore** serving as both prefetch window and tee bound (sized by the largest single file; an oversized file is admitted when the budget is empty). Parallel readers — not a read-ahead window — are the load-bearing choice at both extremes: true-cold ingest is readdir-discovery-bound, and a single read-ahead issuer floors at ~2.7s regardless of window size, while the warm path is a per-file-open syscall race (91% of files ≤4KiB; bytes never bind) that parallel opens take to the 0.14s sha-CPU floor. Hash width is nearly irrelevant (the spine runs at SHA-NI speed, ~0.13s of budget; W=2 suffices). io_uring was evaluated and rejected: ~50ms gain on cold data, 4× loss on true-cold, because it batches reads but cannot parallelize discovery. Projected cold nixpkgs ingest: 0.47s (cold data) to 1.26s (pessimistic true-cold) on laptop-class NVMe — the P1 gate (≤2s) is therefore an NVMe gate by specification: EBS-class volumes floor at ~2.9s on device latency alone and do not qualify as the acceptance environment. Cross-tree parallelism is just N workers plus the claim table.

**The coordinator, pipelined** (eval and upload overlap; nothing waits for "eval finished"): (1) fold incoming nodes by digest into the global graph and the ack table; (2) negotiate — one bulk `Has` per object kind, tenant-scoped, build-referenced objects only, short-circuited by the ack cache; (3) upload misses, zstd-framed, largest-first so builds unblock early — restartable for free, since a re-`Has` after any disconnect only shrinks the miss set; (4) submit per root once that root's transitive skeleton is complete and its misses acked (the committed all-acked gate, below), **excluding nodes already submitted this session** — the measured 6-attr nixpkgs overlap is 4.47×; the filter drops skeleton bytes 2.41MB → 0.54MB; (5) render per-drv status lines from the event streams.

**Attach, detach, results**: Ctrl-C cancels by default — the client cancels every build this invocation submitted (the same `CancelBuild` RPC behind `--cancel`), prints the cancelled ids, and exits non-zero; a second Ctrl-C stops waiting for the cancel acks and prints reattach hints instead. `--detach` makes the interrupt leave the builds running cluster-side. The detach/reattach *capability* is unchanged — builds never need a connected client (this is why client-pull was rejected): `--attach <id>` reattaches via `WatchBuild` from any machine with the tenant credential, and a build watched via `--attach` is never cancelled by Ctrl-C. Completed outputs are imported into the local /nix/store by default — closure walked, pruned against the local daemon, streamed in topological order through the worker protocol, narHash-verified on arrival — with a `./result` out-link (`-o`/`--no-link` adjust it). `--no-fetch` leaves outputs in the cluster store; a machine without a reachable nix daemon falls back to materializing into the client CAS.

**IFD**: import-from-derivation blocks a worker mid-eval. The worker reports the needed drv as an immediate mini-submission; the coordinator builds it remotely, fetches the output into the CAS, and the worker resumes — correct but serialized, surfaced in the UI as "IFD stall: <drv>". The local-build fallback from iteration one (proven at 0.049s end-to-end) is **kept behind a flag**: remote IFD costs submit + dispatch + build + fetch per link, serialized along the import chain — deep IFD chains regress by minutes — and stays unpriced until the express-tier latency report lands. The escape hatch is not deleted before that number exists.

**Failure modes**: worker crash → its attr batch re-queues to a fresh fork, at most one attr's work lost (reported drv bytes are safe in the coordinator). Gateway disconnect → uploads resume via negotiation idempotence; streams reattach via `WatchBuild`. Coordinator crash → nothing cluster-side leaks (builds keep running, detached); a re-run is cheap by construction — warm CAS, warm cluster, near-zero misses.

### Performance: the honest numbers

Two submit-gating semantics exist; the design **commits to per-root all-acked** — `SubmitBuild` fires once a root's transitive skeleton is complete and its misses are uploaded and acked — because it is the simpler rule and keeps the scheduler's submit-time bulk-verify (reject early, never dispatch against dangling digests). The first-build-gated variant (submit on skeleton-complete, builders gate per-node on blob presence) needs a scheduler-side dispatch gate, buys back ~0.7s on cold/leaf scenarios only, and is recorded as a P3-or-later optimization, not a promise.

Time to first build start, **post-eval**, laptop profile (40ms RTT / 50Mbps), at 4.9k-drv scale, on the real corpus scenarios (depth 233; real nixpkgs revision pairs):

| Scenario | ssh-ng (measured) | All-acked (committed) | First-build-gated (P3+ opt.) |
|---|---|---|---|
| Cold 4,901-drv graph | 57s | 0.98s — 1.05s incl. inputSrcs | 302ms |
| Median leaf edit (re-keys 83.7% = the rebuild set) | 36s | 0.96s | 302ms |
| Routine 1-week nixpkgs bump (0 drvs changed) | 36s | 153ms | 153ms |
| 4-week bump across a staging merge (57% changed) | 36s | 297ms | 153ms |

That is ~54× over ssh-ng post-eval. Cold additionally ships the closure's 304 unique inputSrcs: 1.01MB raw / 0.29MB zstd, +47ms — small because nixpkgs package sources are FOD-fetched by the cluster. A project whose `src` is a local working tree uploads that tree cold, but ssh-ng pays the identical cost, so the comparison stays fair. End-to-end including client eval (which this protocol does not speed up — see What this is not): cold 66.8s → ~12.9s ≈ **5.2×**, warm 36.9s → ~1.3s ≈ **29×**.

These numbers are **linear in closure size, not flat**. The skeleton is a measured 334B/node raw (zstd 0.235), so a 100k-node warm submit is ~4.6s raw / ~1.9s compressed, and the 16MB `SubmitBuild` budget is exceeded around ~50k raw nodes (the underlying gRPC cap is 256MiB). Pagination or streaming above that scale is a P2 acceptance item, not an afterthought. Bytes follow the same shape: cold 16.75MB raw → 0.99MB stream-zstd; the 83.7% leaf cascade is 1.22MB compressed — and equals the set that must reach builders anyway under input addressing. Multi-root invocations do not multiply these numbers (the already-submitted filter, above).

## What this is not

- **Not a fix for the eval herd.** This protocol changes what a client *ships*, not what it *computes* — cold per-client eval CPU is unchanged (128 cold clients still burn ~1,500 CPU-s per wave; stress campaign). The herd fix is eval result sharing/caching, tracked separately; P3/P4 acceptance measures submission/upload behavior only and reports eval CPU without gating on it.
- **Not a remote-eval system.** Eval runs on the client, in Nix's libexpr, driven nix-eval-jobs-style. Only the `Store` the evaluator talks to is rio's.
- **Not a gateway replacement.** `ssh-ng://` stays for stock Nix clients as the compatibility path.
- **Not a new hash-defining encoding.** Output-path identity stays exactly Nix's; the proto digest identifies the blob, never the build.
- **Not content-addressed outputs.** Outputs remain input-addressed. One recorded caveat: the "Merkle cascade equals the rebuild cone" argument holds *under input addressing*; if floating-CA outputs land, the digest-redirty cone and the rebuild cone decouple, and the negotiation cost model must be re-examined.
- **Not a mirror of everything eval touches.** The client CAS is a fetch cache plus an index; local working trees are never copied into it except through the bounded mutation escape hatch.

## Considered alternatives

All killed empirically this cycle; do not resurface without new evidence.

- **Monolithic source-DAG encodings, including zero-copy formats** (prost/flatbuffers/rkyv whole-DAG, CDC-chunked for dedup). 100% chunk re-upload on a 0.9%-churn day bump for every layout tested — one inserted entry shifts every downstream offset and redirties every chunk; even JSON only dedups to 8.4/9.1MB. Per-directory structural: 1.39MB (RESULTS). Zero-copy's raw-bytes lookup margin (80–330ns/dir) is noise under the decoded cache.
- **Keep nix's derivation JSON as the canonical form** (iteration one's choice). Couples the digest space to nix's serializer: a formatting whim re-keys 100% of digests (measured); a nix upgrade re-keys every drv and breaks cross-version dedup. JSON also parses ~1.8× slower at the gateway cross-check (DRVPROTO).
- **Structural env-sharing for drv transfer** (dedup env strings via a digest table). Measured 1.17× versus stream-zstd's 16.93× — the redundancy is substring-shaped, not whole-value-shaped; and byte-exact server-side reconstruction under digest verify turns any codec bug into a build outage (DRV-TRANSFER/DRV-CORPUS; the ">3× over zstd" escape hatch resolved ~14× against).
- **Client-pull (no-upload) submission** — cluster fetches from the client on demand. Breaks build-detach (`--detach`, `WatchBuild` reattach) to save 24ms.
- **Level-walk Merkle negotiation** (root digest, descend on miss). Real DAG depth is 233 → 233 RTTs ≈ 9.4–10.3s at 40ms; flat bulk `Has` is one (DRV-CORPUS).
- **Loose-blob CAS layout.** 113.7s of fsync for one cold nixpkgs ingest on overlayfs versus 49ms append-pack (RESULTS M4); disqualified, not deprioritized.
- **zstd dictionaries for drv streams.** 7.18× on a homogeneous corpus but 2.65× on the churn sets that matter, versus 12.9× for plain closure-ordered streams — complexity for negative gain (DRV-CORPUS).
- **Per-blob zstd at rest.** Net size loss at 140B mean metadata blob size; digest-dense bytes barely compress. Pack-level compression only (RESULTS).

## Staging

**P1 — client CAS v2 + eval store (local only).** Per-directory castore blobs in append-packs, decoded-dir cache, stat fingerprint index with the racy rule, drv map in memory; the nix plugin (Rust core, thin C++ shim — iteration one's bridge, kept) on top. Accept, on a **pinned two-fixture list** (small-mixed AND one wide-touch eval visiting ≥20k distinct dirs — full-residency cost is 518ms vs the 92ms warm budget, so fixture choice could otherwise hide the regression; review C24): drvPath parity re-proven (structuredAttrs fixture included); warm trace ≤92ms on small-mixed; warm flake eval ≤1.10× stock on both fixtures; cold nixpkgs-source ingest ≤2s on local NVMe (the acceptance environment by specification — EBS-class volumes floor at ~2.9s on device latency alone). P1 also re-measures in Rust the three constants the threading simulation flagged as Python-shaky: real-NVMe cold latency/IOPS (decides R), warm open+read+close cost, FastCDC scan rate. Also: capture real op arguments once (plugin op-log) to retire the uniform-sampling caveat in the bench traces. No cluster dependency; starts immediately.

**P2 — wire: drv blob kind, skeleton submission, the external door.** rio-store drv blob kind (canonical proto bytes) with build-pinned GC, write-through idempotent puts; bulk `Has` for drv digests, tenant-scoped; **flip `HasChunks` to tenant-scoped**; chunk zstd-at-rest with its migration rule (digests over uncompressed bytes; reads sniff the zstd magic; legacy raw chunks age out); the externally reachable tenant-authenticated gRPC door — P3's client cannot connect without it; gateway `upload_missing_drvs` + drv_path recompute + canonical re-encode byte-compare; zstd stream framing on upload batches and `SubmitBuild`. Wire rollout two-phase as specified above (P2a additive, P2b retires `edges`/`drv_content` one release later, field 9 reserved). Accept: warm rebuild submission ships only the rebuild cone; gateway round-trip byte-stability test; cross-tenant presence isolation for all three object kinds; an external client authenticates and round-trips Has/upload through the door; **`SubmitBuild` paginates or streams above ~50k nodes**.

**P3 — `rio build` client.** Fork workers, pipelined fold→negotiate→upload→submit, attach/detach/fetch, flag-gated local IFD fallback. Accept: end-to-end build of the bench graph from a cold client beats the measured ssh-ng numbers by the simulated margins (±2×); 32× parallel clients show no per-client **submission/upload** herd. Cold eval CPU explicitly out of scope.

**P4 — scale validation.** Re-run the stress ladder with `rio build` clients, cold AND warm; express-tier latency measured via the shipped histogram (prices remote IFD). Accept: 128× clean with submission costs flat in both runs; eval-CPU herd reported, not gated; ladder report.

## Measurement plan

What gets re-measured, and when:

- **Express-tier latency** (the shipped histogram, P4) prices remote IFD per import-chain link; until that number exists the local-build IFD fallback stays flag-gated, not deleted.
- **Real op arguments** captured once during P1 (plugin op-log) retire the uniform-sampling caveat in the dagbench warm-trace numbers; the per-op count/size histograms from iteration one stay on from day one.
- **Placeholder constants now measured, pinned as regression gates**: skeleton 334B/node, skeleton zstd 0.235, gateway cross-check 12–23µs/drv — P2's byte-stability and acceptance tests assert against these, replacing the sim's estimates (250B, 50µs) they superseded.
- **The racy-fingerprint slack** (10ms tick) was measured on overlayfs and tmpfs; P1 re-validates on the developer-common filesystems before pinning the default.
- **Known oddity to check during P1**: the captured protoperf CAS stored one identical 8.9MB DAG under two store-path names — verify CAS v2's structural sharing makes the duplicate free.

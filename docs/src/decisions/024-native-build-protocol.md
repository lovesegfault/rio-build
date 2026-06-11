# ADR-024: Native build protocol — `rio build`

Status: Proposed

Requires: [ADR-022](./022-design-overview.md) — everything here builds on the castore model (Directory DAGs, per-file FastCDC chunking, NAR framing regenerated on read, tenant-scoped reads). This ADR adds the client and the protocol, not a storage model.

---

## Context

The only way to submit builds today is `nix build --store ssh-ng://<gateway>`: the legacy Nix daemon wire protocol over SSH. Measured costs (perf campaign, 2026-06):

- **Serial request/response.** Pipelining patches buy 1.25–4× on closure queries, but that is patching around the protocol, not fixing it.
- **Zero transfer dedup.** Sources upload as full NARs even when every byte is already in castore. A warm rebuild re-sends its entire source closure.
- **No compression.** NAR bytes go over the wire raw.
- **Double serialization.** The gateway unpacks NARs and re-chunks into castore — every uploaded byte is serialized twice.
- **One SSH channel.** Logs, uploads, and queries share a channel with head-of-line blocking; a severed channel kills the whole session (32/32 clients died to one transport event in the 2026-06 stress run).

The cluster-internal surfaces already solve most of this: castore holds FastCDC-chunked, BLAKE3-addressed content; `PutPathChunked` does digest-negotiated chunked upload with content verification; reads are tenant-scoped. What is missing is a client that speaks to them directly.

## Decision

Add `rio build`: a native client speaking a gRPC protocol in which **a build submission is a digest, and bulk data moves only when the cluster does not already have it**.

### Identity: derivation JSON, not ATerm

The canonical stored form of a derivation is Nix's derivation JSON format (the `nix derivation show` / `nix derivation add` schema), stored in castore as a blob and addressed by its BLAKE3 digest like any other content. The protocol, the scheduler, and the store never parse ATerm.

ATerm survives in exactly one place: store-path computation. Output paths remain input-addressed `/nix/store` paths, so `hashDerivationModulo` must match Nix byte-for-byte — `rio_nix::derivation::hash::hash_derivation_modulo` already implements this, serializing to ATerm internally as an implementation detail of the hash. No new hash-defining encoding is introduced: the JSON digest identifies the blob; build identity (output paths) stays defined by Nix's existing rules.

The ATerm *parser* is demoted to a compatibility boundary: the ssh-ng gateway ingests legacy `.drv` uploads and lowers them to JSON. Two hard rules there:

- **Hashing is always over the original ingested bytes**, never a re-serialization. ATerm→JSON→ATerm is not byte-stable in general (structuredAttrs re-dump through the JSON library's canonical form; a `.drv` whose embedded `__json` is not already canonical re-serializes to different bytes and a silently divergent drv path). The gateway verifies `unparse(parse(original)) == original` and stores the original bytes alongside the JSON when the round-trip differs.
- **The JSON schema version is pinned and recorded per blob.** Upstream's derivation JSON is version 4 and documented as experimental; rio owns the migration when v5 lands. Accepted cost — a rio-proprietary encoding would carry the same migration burden in a format nobody else maintains.

If outputs ever become content-addressed, the ATerm hashing step disappears too; nothing else in the protocol changes.

### Submission: one digest plus a skeleton graph

1. **Streaming eval with a rio-backed store.** The client embeds the nix-eval-jobs model (libexpr eval workers, jobs streamed as discovered) pointed at a custom `nix::Store` instead of the local store. `EvalState` takes the store object directly, and `RegisterStoreImplementation` lets a `rio://` scheme load as a plugin into stock nix. Nix's in-memory dummy store (`dummy-store.cc`) is the reference implementation: it demonstrates every override this design needs.

   **Mostly Rust, small C++, in-process.** The core is a `rio-evalstore` Rust staticlib linked into the plugin `.so`; a thin (~300-line) C++ shim implements the `nix::Store` vtable and calls the core through a hand-written `extern "C"` surface (~15 functions over bytes, paths, and streaming callbacks). Hashing, chunking, CAS, and protocol logic live in Rust (rio-nix already computes store paths and derivation hashes); C++ only marshals. Boundary discipline: every Rust export wraps in `catch_unwind` and reports failure through an error out-param the shim converts to `nix::Error`; streaming uses context-plus-callback trampolines in both directions (`Sink`/`Source` wrapped on the C++ side); no callback unwinds across the boundary. Fork-safety, adopted from day one because nix-eval-jobs forks workers without exec: no threads or async runtime at plugin load or store-config construction, state created lazily on first store operation, synchronous core. The embedded `rio build` client later links the same crate natively — the shim exists only to run inside stock nix.

   **The client CAS is the substrate.** The store's backing state is a persistent content-addressed cache on the client machine (XDG `~/.cache/rio/cas`): blobs and chunks by BLAKE3 digest, Directory DAGs, derivation JSON blobs, and a stat-fingerprint index keyed on `(path, size, mtime_ns, inode, ctime)`. Any fingerprint mismatch means re-hash, so a stale entry costs a hash, never wrong content. This gives: repeat evals that skip re-reading unchanged sources; `isValidPath` answered from the CAS index (everything this client has produced or fetched, across sessions); per-tenant upload-acknowledgement records that let negotiation skip cluster-known digests with zero RPCs and make interrupted runs resumable. Concurrent processes share the CAS via advisory locking — the CAS, not a daemon, is the cross-process dedup point.

   **CAS GC.** Unlike the node-side cache (pure read-through; any eviction is safe), the client CAS holds state a live eval depends on: nix has already been handed `ValidPathInfo` and will read paths back through the accessor. Three rules bound the disk without breaking runs: (1) the sweep runs at store open under the exclusive advisory lock — no daemon, no background thread (which the fork-safety rule forbids anyway); (2) the eviction unit is the store-path entry — LRU by last-use from a high to a low watermark, then a mark-sweep over surviving DAGs deletes unreferenced blobs (client scale makes a full mark-sweep at open cheap; no refcounting, and no half-evicted path `isValidPath` could lie about). Last-use is the path-entry metadata file's mtime, explicitly touched by the store on each access — same oldest-first file-timestamp sweep as the node cache, but kernel atime is not trusted (relatime/noatime make it useless for LRU); blobs carry no timestamps since reachability, not age, decides them; (3) a grace window — nothing touched within the last N hours (default 6) is evictable, which protects concurrent and crashed runs with zero session bookkeeping. If the watermark cannot be reached because everything is in grace, the sweep stops and logs: disk pressure never wins against a live run.

   **Integrity model.** Verification happens where bytes cross a trust boundary, never per read. Ingest is correct by construction — blob keys are derived by hashing the ingested bytes. Network fills (M1 cluster reads) verify digests on arrival, exactly as the builder fill path does. Local CAS reads trust the disk: the CAS sits in the same trust domain as the source trees it was hashed from and the node's post-promote backing files (ADR-022) — re-hashing one and not the others would be theater, not integrity. IFD copies into a local nix store are re-verified by the receiver against narHash.

   What the custom store intercepts:
   - `addToStore` (source trees referenced by eval) receives a `SourceAccessor` — for flake inputs and `fetchTree` results this is the fetcher's virtual tree (master's lazy trees are unconditional: inputs mount as accessors and copy to the store only when a derivation references them). The shim streams the tree through the accessor; the store path is computed locally and content lands in the client CAS with no `/nix/store` materialization. NAR-sha256 still requires reading every source byte once — wire bytes shrink, local read I/O does not — so the NAR hash and FastCDC chunking happen in the same pass.
   - `writeDerivation` receives the structured `Derivation` and captures its JSON in memory — no `.drv` files, no post-eval scanning. (Hooking `addToStoreFromDump` instead would yield ATerm bytes; `writeDerivation` specifically is the JSON capture point.)
   - **Cross-implementation path checks on every object.** The shim passes nix's own computed drvPath / store path alongside the content; the Rust core recomputes it via rio-nix and **hard-fails on mismatch**, printing both paths. A one-byte divergence between rio's hashing and nix's would otherwise produce valid-looking but wrong paths that fail far downstream; this makes the scariest failure mode loud, at negligible cost.
   - **Presence checks never block eval.** Eval calls single-path `isValidPath` synchronously inline in the eval loop — there is no batching hook, so mapping it to a network RPC would serialize thousands of round-trips into eval. The store answers from the CAS index; the upload layer dedups against the cluster asynchronously via the bulk presence RPC.
   - Store-path returns are synchronous (`copyPathToStore` puts the path into the string context immediately; callers expect `queryPathInfo` with `narHash` right after `addToStore`). The store returns `ValidPathInfo` immediately, answers valid for in-flight objects, and uploads complete asynchronously behind a pre-submit barrier: a derivation is not submitted until its inputs' uploads are acknowledged.

   **The read side is half the work.** `getFSAccessor` is pure virtual and `EvalState` mounts it at construction; every eval read of a store path — IFD outputs, `import` of copied sources, pure-eval reads — goes through the custom store. The contract includes `getFSAccessor` (`readFile`/`lstat`/`readDirectory` served from the client CAS, cluster-filled from M1), `queryPathInfo`, `readDerivation`/`readInvalidDerivation`, and `narFromPath` (NAR framing regenerated from the DAG). Op sizes split into two classes with different handling: eval-loop reads are tiny (bytes to tens of KB, thousands of calls — per-call FFI is fine, per-call network never is), while `addToStore` ingests and `narFromPath` extractions range to GBs and are streamed, never buffered whole. The store records per-op count/size histograms from day one so these assumptions are checked against real evals, not reasoned about.

   Eval runs in forked worker subprocesses (the nix-eval-jobs model) with the Boehm GC **disabled** (`GC_DONT_GC`, allocate-only): mark/sweep burns CPU walking a heap that process recycling reclaims wholesale. A worker is recycled when its RSS crosses a watermark — exit is the collection, O(1) instead of O(heap). This bounds eval memory at `watermark × workers`. Two costs are engineered, not inherited: in nix-eval-jobs an abnormally-dying worker (SIGSEGV, OOM-kill) aborts the entire run — only caught EvalErrors are per-attribute — so poison-attribute retry/quarantine is new work; and a recycled worker re-evaluates from the root to reach its next attribute, so the watermark trades GC savings against re-eval cost. Per-attribute error reporting (including the fatal/stack-overflow distinction) is part of the worker protocol from day one. Workers share the client CAS, so recycling costs nothing on the transfer side.

   Import-from-derivation initially falls back to a local build. That requires an explicit local `buildStore` passed alongside the eval store (the default builds *inside* the eval store) and the full read-side surface above: IFD outputs are copied into the eval store and read back through its accessor, and the local builder pulls input sources out via `narFromPath`. Delegating IFD builds to the cluster is a later step, not a blocker.

2. **CAS negotiation for everything; NAR is a hash, not a format.** Derivation blobs and source files flow through the same have/missing negotiation: client sends digests, store answers with the missing set, client uploads only those through the existing chunked-upload path. A warm closure (stdenv, common deps already known to the cluster) submits in hundreds of bytes regardless of closure size — the steady-state answer to "how do we encode 4,000 derivations efficiently" is "we don't send them".

   No NAR stream exists anywhere in this path. One walk over each source tree simultaneously FastCDC-chunks raw file bytes, builds the castore Directory DAG, and feeds NAR canonical order and framing into a hasher — source store paths derive from the NAR sha256, so the *number* is required but the *bytes* never are. This mirrors castore server-side (ADR-022: framing regenerated on read, never persisted). Client-side FastCDC parameters MUST be byte-identical to the server's so client uploads dedup against gateway-ingested chunks. The protocol constants are the existing workspace values (`rio_common::limits`): min 16 KiB, avg 64 KiB, max 256 KiB, fastcdc v2020, per-file chunking, BLAKE3 chunk keys — the same constants the builder and store already share (a mismatch silently drops dedup to zero, which is why they live in one crate).

3. **Skeleton graph for the scheduler.** The submit RPC carries a proto skeleton per node — derivation identity, input edges, output names, platform, required system features — so scheduling starts before derivation blobs are fetched and parsed. The skeleton is derived data: verified against the parsed derivation before the build executes, never trusted for identity. This message already exists: `dag.proto`'s `DerivationNode`/`DerivationEdge` is what the gateway submits to the scheduler today (`drv_path`, `system`, `required_features`, `output_names`, `is_fixed_output`, `expected_output_paths`, optional inline drv bytes). The native client emits the same DAG directly instead of making the gateway derive it; the inline `drv_content` fallback gives way to the castore drv-JSON blob reference as the blob kind lands.

4. **Parse once, cluster-wide.** The scheduler parses each unique derivation once, keyed by digest, and caches the parsed form.

5. **Results.** Build events and logs stream back over multiplexed HTTP/2 streams (no head-of-line blocking, per-stream reconnect — `SubmitBuild` returns the event stream and `WatchBuild` reattaches after a transport drop). Output download is opt-in (`--no-download` is the default, as in nix-fast-build); when requested, outputs fetch as chunks through the existing read path into the client CAS.

### Wire surface: mostly already shipped

M1 is exposure and scoping work, not new RPC design. The cluster-internal gRPC surface in `rio-proto` already provides every primitive this protocol needs:

| Protocol need | Existing RPC |
|---|---|
| have/missing negotiation | `ChunkService.HasChunks`, `DirectoryService.HasBlobs` / `HasDirectories` (bulk digest → bitmap), `StoreService.FindMissingPaths` (path level) |
| negotiated upload | `StoreService.PutPathChunked` |
| content reads (CAS fill, downloads) | `ChunkService.GetChunks`, `DirectoryService.ReadBlob` / `GetDirectory` |
| submit + events | `SchedulerService.SubmitBuild(nodes, edges) → stream BuildEvent`, `WatchBuild`, `CancelBuild` |

What is genuinely new for M1: (a) an **externally reachable, tenant-authenticated door** to these services — today they are cluster-internal and the only external surface is the ssh-ng gateway; the native client needs the gateway (or an adjacent listener) to terminate gRPC with the same tenant identity source the ssh path uses; (b) **tenant-scoped answers** on the `Has*` family per the security section below — the bitmap RPCs predate that rule; (c) the **derivation-JSON blob kind** in castore; (d) the client itself.

### Compression: zstd at rest, once

Chunks are zstd-compressed when written to the chunk backend and served compressed; transfer bytes equal stored bytes. Compression happens once at write time, and S3 serves pre-compressed frames directly. Per-message gRPC compression is not used; it is the wrong layer.

Today chunks are stored raw, so this is new work with a migration rule: **chunk digests are always over the uncompressed bytes** (identity never depends on the encoding), and the stored object is self-describing — the zstd magic number distinguishes a compressed frame from a raw legacy chunk, so reads sniff and serve either. New writes compress; legacy raw chunks are never bulk-migrated — they age out through GC or get rewritten opportunistically. `HasChunks`, dedup, and every digest comparison are unaffected because they never see encoded bytes.

### Security: presence is a read

The have/missing negotiation RPC is an oracle for what exists in the store, and derivation digests reveal what other tenants build. Two requirements, both inherited from the castore read rules:

- The negotiation RPC sits behind the same tenant auth as `PutPathChunked` — not a "read-only, therefore safe" endpoint.
- Answers are tenant-scoped: the store answers **missing** for any blob the calling tenant cannot see under the signature-visibility rules, even when the bytes exist. Cross-tenant upload dedup is deliberately lost; the duplicate upload binds safely because chunked-upload verification already treats client-claimed digests as untrusted. If cross-tenant dedup ever matters, the upgrade path is proof-of-possession, never a global presence oracle.

## What this is not

- **Not a gateway replacement.** `ssh-ng://` stays for stock Nix clients as the compatibility path.
- **Not a new evaluator.** Eval is Nix's libexpr, driven nix-eval-jobs-style. Only the `Store` the evaluator talks to is rio's; language, eval semantics, and store-path rules are untouched.
- **Not a new hash-defining encoding.** Output-path identity stays exactly Nix's. The JSON form is upstream's own schema.
- **Not content-addressed outputs.** Outputs remain input-addressed; CA is a possible future that only removes code from this design.

## Considered alternatives

- **Keep ATerm on the wire, pipeline harder.** Worth re-landing for the compat path (confirmed 1.25–4×), but cannot add dedup or compression, and double serialization remains.
- **A rio-proprietary binary derivation encoding** (the Tvix protobuf-Derivation road). Any second encoding that participates in hashing is a compatibility bug factory; one that doesn't is just a cache, which parse-once-by-digest already provides without a format.
- **Compress per-transfer (gRPC message compression).** Pays compression CPU on every transfer of the same bytes; the store cannot serve pre-compressed data from S3.
- **Global (tenant-blind) presence query for maximum dedup.** Cross-tenant build-content enumeration oracle; violates the tenant-scoped-reads invariant.
- **UDS daemon between shim and core.** A separate `rio` daemon process with the C++ shim forwarding over a socket. Rejected for M0: the in-process staticlib is simpler, faster, and the client CAS with advisory locking already provides cross-process sharing; IPC returns only if a shared long-lived daemon earns its keep later.

## Staging

- **M0 — store plugin, no network.** `nix build --eval-store 'rio://...' --plugin-files ...` against the client CAS only. Acceptance: byte-identical drvPaths vs stock nix on the same fixture, cross-checks all green. The plugin is built against the flake's pinned nix (`inputs.nix`, currently `github:NixOS/Nix/2.34.7`), which already has the full API surface this design needs (accessor-based `addToStore`, both `getFSAccessor` overloads, virtual `writeDerivation`, derivation JSON v4, the plugin loader). **Pin contract:** the `.so` loads only into binaries built from `inputs.nix` — never into any other nix build (symbols resolve at dlopen against the host's libnixstore; loader and headers must come from the same derivation set). M0 validates the store class, not the job pipeline: nix-eval-jobs's own job-shaping degrades on a non-LocalFS store (silently drops `system`, `inputDrvs`, cache status, required features).
- **M0.5 — local builds through the split.** `nix build --eval-store rio://` with the default local build store: nix copies each derivation and its input sources *out of* the rio eval store (`readDerivation`, `narFromPath`) into the local store and builds there. The M0 surface implements everything this needs but the parity check stops at drv production — M0.5 is the end-to-end validation that a real build runs, and it exercises the same extraction machinery IFD relies on. Acceptance: the fixture builds and its output hash matches a stock-nix build of the same fixture.
- **M1 — cluster wiring.** Expose the existing RPC surface (table above) through a tenant-authenticated external door; tenant-scope the `Has*` answers; negotiated chunk upload from the client CAS; cluster-backed reads filling the client CAS; derivation-JSON blob kind; zstd-at-rest. Before pointing nix-eval-jobs at the plugin: rebuild it against `inputs.nix` components (the repo's nix-eval-jobs is built against nixpkgs' separate nix 2.34 build — different ABI).
- **M2 — embedded `rio build`.** Links `rio-evalstore` natively (no plugin, no shim): worker management with poison-attribute quarantine, skeleton-graph submission, per-attribute error reporting, build-event streaming.
- **M3 — IFD delegation to the cluster.** Until then, IFD uses the explicit local `buildStore` fallback.

The Store vtable is thin but its shape is upstream's, and the interface churns across nix releases. Like nix-eval-jobs, the client pins an exact nix per release and budgets adaptation work per bump.

## Measurement plan

Before client implementation, instrument one real `nix build --store ssh-ng://` of `medium-mixed` and split wall time into eval / closure query / upload (bytes and time) / build wait / download — this sizes the prize per mechanism (dedup vs round-trips vs compression) and orders the work. The eval store's own op histograms validate the size-class assumptions from the first real eval. zstd dictionary training for derivation blobs is measure-first: plain zstd over CDC-chunked drv text may already capture the redundancy.

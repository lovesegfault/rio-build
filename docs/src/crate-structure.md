# Crate Structure

## Workspace Layout (8 crates)

```
rio-build/
├── Cargo.toml           # Workspace root
├── rio-common/          # Shared utilities (no rio-* deps — leaf)
├── rio-nix/             # Nix protocol types and wire format (no rio-* deps — leaf)
├── rio-proto/           # Protobuf/gRPC definitions
├── rio-test-support/    # Test harness (ephemeral PG, mock gRPC, wire helpers)
├── rio-gateway/         # SSH server + Nix worker protocol frontend
├── rio-scheduler/       # DAG-aware build scheduler
├── rio-store/           # NAR content-addressable store
└── rio-worker/          # Build executor + FUSE store
```

Not yet built (future phases):

- **rio-controller/** — Phase 3 Kubernetes operator
- **rio-cli/** — Phase 4 operator CLI
- **rio-dashboard/** — TypeScript SPA, not a Rust crate

## Dependency Graph

```mermaid
graph TD
    rio-common["rio-common<br/>(config, observability, limits, newtypes)"]
    rio-nix["rio-nix<br/>(protocol, derivations, NAR)"]
    rio-proto["rio-proto<br/>(gRPC definitions)"]
    rio-test-support["rio-test-support<br/>(PG, mock gRPC, wire)"]
    rio-store["rio-store<br/>(CAS, binary cache)"]
    rio-scheduler["rio-scheduler<br/>(DAG, scheduling)"]
    rio-gateway["rio-gateway<br/>(SSH, protocol handler)"]
    rio-worker["rio-worker<br/>(executor, FUSE, overlay)"]

    rio-proto --> rio-nix
    rio-proto -.->|dev| rio-common

    rio-test-support --> rio-nix
    rio-test-support --> rio-proto

    rio-gateway --> rio-nix
    rio-gateway --> rio-proto
    rio-gateway --> rio-common
    rio-gateway -.->|dev| rio-test-support

    rio-worker --> rio-nix
    rio-worker --> rio-proto
    rio-worker --> rio-common
    rio-worker -.->|dev| rio-test-support

    rio-scheduler --> rio-nix
    rio-scheduler --> rio-proto
    rio-scheduler --> rio-common
    rio-scheduler -.->|dev| rio-store
    rio-scheduler -.->|dev| rio-test-support

    rio-store --> rio-nix
    rio-store --> rio-proto
    rio-store --> rio-common
    rio-store -.->|dev| rio-test-support
```

Solid edges are prod dependencies; dashed are `[dev-dependencies]` only.

Notable edges:

- **`rio-proto → rio-nix`**: `ValidatedPathInfo` wraps `StorePath` from rio-nix. No cycle — rio-nix has no rio-* deps.
- **`rio-proto → rio-common` (dev-only)**: contract tests check `rio-common::limits` against proto-side `check_bound` enforcement.
- **`rio-scheduler → rio-nix` (prod)**: `Derivation` parsing for closure resolution and `StorePath` validation in the merge path.
- **`rio-scheduler → rio-store` (dev-only)**: integration tests spin up a real `StoreServiceServer` from `rio-store::grpc`.
- **`DrvHash` / `WorkerId` live in `rio-common::newtype`**: Arc<str>-backed string newtypes shared by scheduler, worker, and proto translation. Placing them in rio-common avoids a `proto → common → proto` cycle.

## Module Structure

### rio-common — Shared utilities

```
src/
├── lib.rs
├── bloom.rs           # Self-describing BloomFilter (blake3-based)
├── config.rs          # figment-based config layering helpers
├── grpc.rs            # gRPC timeouts, message-size constants
├── limits.rs          # MAX_NAR_SIZE, MAX_COLLECTION_COUNT, etc.
├── newtype.rs         # string_newtype! macro; DrvHash, WorkerId
├── observability.rs   # Tracing init, describe!() metric registration
└── task.rs            # spawn_monitored task wrapper
```

### rio-nix — Nix protocol and data types

```
src/
├── lib.rs
├── derivation/
│   ├── mod.rs         # Derivation struct + output types
│   ├── aterm.rs       # ATerm parser/serializer (.drv files)
│   └── hash.rs        # Derivation hash modulo computation
├── protocol/
│   ├── mod.rs
│   ├── opcodes.rs     # WorkerOp enum
│   ├── handshake.rs   # Version negotiation, magic bytes
│   ├── wire/
│   │   ├── mod.rs     # Primitives: u64, bytes, strings, collections
│   │   └── framed.rs  # Framed stream reader/writer
│   ├── stderr.rs      # STDERR_* framing (NEXT/LAST/ERROR/RESULT/WRITE)
│   ├── build.rs       # BasicDerivation + BuildResult wire types
│   ├── client.rs      # Client-side protocol (drives nix-daemon --stdio)
│   └── derived_path.rs # DerivedPath string parser
├── store_path.rs      # StorePath + nixbase32
├── nar.rs             # NAR streaming read/write/extract
├── narinfo.rs         # NarInfo parse/serialize + fingerprint()
└── hash.rs            # NixHash (SHA-256, SHA-512, BLAKE2)
```

Fuzz targets for the parsers live in `rio-nix/fuzz/` (separate workspace).

### rio-proto — gRPC definitions

```
proto/
├── types.proto        # Shared: PathInfo, DerivationNode, BuildEvent, Heartbeat
├── store.proto        # StoreService + ChunkService
├── scheduler.proto    # SchedulerService
├── worker.proto       # WorkerService
└── admin.proto        # AdminService (dashboard/CLI)
src/
├── lib.rs             # tonic::include_proto! + client re-exports
├── client.rs          # get_path_nar, collect_nar_stream helpers
├── interceptor.rs     # W3C traceparent inject/extract for tonic
└── validated.rs       # ValidatedPathInfo (proto → domain type validation)
```

### rio-gateway — Nix protocol frontend

```
src/
├── lib.rs
├── main.rs
├── server.rs          # russh SSH server
├── session.rs         # Per-client session state
├── translate.rs       # Nix protocol ↔ gRPC translation helpers
└── handler/
    ├── mod.rs         # Opcode dispatch loop
    ├── grpc.rs        # gRPC client wrappers (timeout + retry)
    ├── build.rs       # wopBuildPaths/wopBuildDerivation/wopBuildPathsWithResults
    ├── opcodes_read.rs  # Read-only opcodes (QueryPathInfo, NarFromPath, ...)
    └── opcodes_write.rs # Write opcodes (AddToStoreNar, AddMultipleToStore, ...)
```

### rio-scheduler — DAG scheduler

```
src/
├── lib.rs
├── main.rs
├── actor/             # Single-threaded actor owning all mutable state
│   ├── mod.rs         # Actor struct, spawn, message enum, push_ready helper
│   ├── build.rs       # SubmitBuild / CancelBuild handlers
│   ├── merge.rs       # DAG merge: cache-check, dedupe, transitions
│   ├── dispatch.rs    # Ready-queue drain → worker assignment
│   ├── completion.rs  # CompletionReport handler + EMA update + cascade
│   ├── worker.rs      # Heartbeat merge + worker liveness
│   └── tests/
│       ├── mod.rs
│       ├── helpers.rs     # MockStore, make_test_node, scripted events
│       ├── wiring.rs      # Actor spawn + channel plumbing
│       ├── coverage.rs    # Per-handler unit coverage
│       └── integration.rs # Multi-handler scenarios
├── state/
│   ├── mod.rs         # PriorityClass, re-exports
│   ├── newtypes.rs    # Scheduler-local newtypes
│   ├── derivation.rs  # DerivationState, DerivationStatus transitions
│   ├── build.rs       # BuildInfo, BuildState transitions
│   └── worker.rs      # WorkerInfo, heartbeat timeout tracking
├── dag/
│   ├── mod.rs         # Dag: node/edge storage, reverse-deps walk
│   └── tests.rs
├── grpc/
│   ├── mod.rs         # gRPC service impl → actor message send
│   └── tests.rs
├── logs/
│   ├── mod.rs         # LogBuffers: DashMap ring buffers per derivation
│   └── flush.rs       # LogFlusher: S3 gzip PUT on completion
├── admin.rs           # AdminService: DAG/worker introspection
├── assignment.rs      # Worker scoring (bloom locality + load) + size-class classify()
├── critical_path.rs   # Bottom-up priority computation + incremental update
├── db.rs              # build_history EMA UPSERT + PG helpers
├── estimator.rs       # Duration/memory estimates from build_history
└── queue.rs           # ReadyQueue: BinaryHeap with lazy invalidation
```

### rio-store — Content-addressable store

```
src/
├── lib.rs
├── main.rs
├── backend/
│   ├── mod.rs         # ChunkBackend trait + InMemory test impl
│   └── chunk.rs       # S3-compatible chunk backend
├── grpc/
│   ├── mod.rs         # StoreService + ChunkService skeleton
│   ├── put_path.rs    # PutPath streaming handler
│   ├── get_path.rs    # GetPath streaming handler
│   └── chunk.rs       # GetChunk / FindMissingChunks
├── cas.rs             # moka chunk cache + singleflight + BLAKE3 verify
├── chunker.rs         # FastCDC content-defined chunking
├── manifest.rs        # Chunk-list serialize/deserialize
├── metadata.rs        # narinfo + manifests PG tables (CRUD)
├── content_index.rs   # content_hash → store_path (CA early cutoff)
├── realisations.rs    # CA realisation storage (Register/Query)
├── signing.rs         # ed25519 narinfo signing
├── validate.rs        # ValidatedPathInfo checks (hash, refs, size)
└── cache_server.rs    # axum binary-cache HTTP (narinfo + nar.zst)
```

### rio-worker — Build executor

```
src/
├── lib.rs
├── main.rs
├── runtime.rs         # Worker runtime loop: poll scheduler → execute → report
├── executor/
│   ├── mod.rs         # execute_build: overlay → daemon → upload → report
│   ├── daemon.rs      # nix-daemon --stdio spawn + STDERR_RESULT drain
│   └── inputs.rs      # Input resolution: fetch_drv_from_store, resolve_inputs
├── fuse/
│   ├── mod.rs         # Filesystem impl + mount_fuse_background
│   ├── inode.rs       # Inode allocator + path↔ino maps
│   ├── fetch.rs       # GetPath → NAR extract → cache insert
│   ├── ops.rs         # fuser trait impls (getattr, readdir, open)
│   ├── lookup.rs      # lookup() + ensure_cached (materialize on demand)
│   ├── read.rs        # read() with passthrough fd
│   └── cache.rs       # SQLite-backed SSD cache with LRU eviction
├── overlay.rs         # overlayfs setup/teardown (host store + FUSE lower)
├── synth_db.rs        # Synthetic nix.sqlite for sandboxed nix-daemon
├── upload.rs          # HashingChannelWriter: stream NAR → PutPath
└── log_stream.rs      # LogBatcher: 64-line/100ms batch + rate/size limits
```

### rio-test-support — Test harness

```
src/
├── lib.rs             # TestDb re-export, TestResult alias
├── pg.rs              # Ephemeral PostgreSQL (initdb + postgres via PG_BIN)
├── wire.rs            # wire_bytes! macro, handshake/setOptions/stderr helpers
├── grpc.rs            # MockStore, MockScheduler, server spawn helpers
└── fixtures.rs        # test_store_path, test_drv_path, NAR builders
```

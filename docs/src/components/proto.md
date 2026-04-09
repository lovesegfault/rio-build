# rio-proto

Internal gRPC APIs between components + external API for tooling.

## Transport

r[proto.h2.adaptive-window]
All gRPC channels (client `Endpoint` builders and server `Server::builder()`) MUST enable `http2_adaptive_window` and set an initial per-stream window of at least 1 MiB (h2 default is 65 535 bytes). At cross-AZ RTT (~2-3 ms) the default 64 KiB window caps a `GetPath` NAR stream at ~20-30 MB/s regardless of link bandwidth — each 256 KiB chunk needs ~4 `WINDOW_UPDATE` round-trips before the next can flow. Adaptive-window BDP probing auto-tunes upward from the 1 MiB floor.

## gRPC Metadata Keys

`x-rio-*` header constants live in `rio_common::grpc` (proto-agnostic, lowercase per HTTP/2 header rules) and are re-exported at `rio_proto::*` so all callers reference one path. A typo in a string literal at one site silently breaks header propagation with no compile-time signal — using the constant is mandatory.

| Constant | Header | Direction | Carries |
|---|---|---|---|
| `BUILD_ID_HEADER` | `x-rio-build-id` | scheduler → gateway (response initial-metadata) | UUIDv7 build_id, set BEFORE first stream message so the gateway has it even on zero-event streams |
| `TRACE_ID_HEADER` | `x-rio-trace-id` | scheduler → gateway (response initial-metadata) | 32-hex W3C trace_id of the scheduler handler span — see `r[obs.trace.scheduler-id-in-metadata]` |
| `ASSIGNMENT_TOKEN_HEADER` | `x-rio-assignment-token` | executor → store (request metadata on `PutPath`/`PutPathBatch`) | HMAC-SHA256 token signed by scheduler; store verifies (executor_id, drv_hash, expected_outputs, expiry) |
| `TENANT_TOKEN_HEADER` | `x-rio-tenant-token` | gateway → scheduler/store (request metadata) | ed25519 JWT; missing header is pass-through (single-tenant mode), present-but-invalid is `Unauthenticated` |

r[proto.metadata.build-id]
`x-rio-build-id` MUST be set by the scheduler on `SubmitBuild` response **initial** metadata. Server-streaming RPCs send headers before any stream message, so the gateway can record the build_id even if the scheduler dies between MergeDag commit and the first `BuildEvent`.

r[proto.metadata.assignment-token]
`x-rio-assignment-token` is the **only** input the store trusts when authorizing `PutPath`. The token is minted scheduler-side at dispatch (HMAC over executor_id + drv_hash + expected_outputs + expiry) and carried through the executor verbatim. The store MUST reject uploads with a missing, expired, or mismatched-output token. Builder pods are airgapped and untrusted — builder-supplied data MUST NOT drive authorization; the token is the cryptographic link back to a scheduler decision.

r[proto.metadata.tenant-token]
`x-rio-tenant-token` is set by the gateway on every outbound RPC in JWT mode. Server-side (`rio_common::jwt_interceptor`): missing header is pass-through (dual-mode / single-tenant), present-but-unverifiable is `Unauthenticated`. Verified claims populate request extensions; handlers read tenant identity from extensions, never from request body fields.

## Services

```protobuf
// scheduler.proto --- gateway-facing RPCs
service SchedulerService {
  rpc SubmitBuild(SubmitBuildRequest) returns (stream BuildEvent);
  rpc WatchBuild(WatchBuildRequest) returns (stream BuildEvent);
  rpc QueryBuildStatus(QueryBuildRequest) returns (BuildStatus);
  rpc CancelBuild(CancelBuildRequest) returns (CancelBuildResponse);
  rpc ResolveTenant(ResolveTenantRequest) returns (ResolveTenantResponse);  // name→UUID for gateway JWT mint
}

// builder.proto --- executor-facing RPCs (same server process as SchedulerService)
// Covers BOTH builder and fetcher pods; the executor reports its role via HeartbeatRequest.kind.
service ExecutorService {
  // Bidirectional stream: scheduler sends assignments + prefetch hints + cancel signals;
  // executor sends log batches + completion reports + ack messages
  rpc BuildExecution(stream ExecutorMessage) returns (stream SchedulerMessage);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
}
```

> **Executor registration:** Executor registration is implicit and two-step: (1) the executor opens a `BuildExecution` bidirectional stream, (2) the executor calls the separate `Heartbeat` unary RPC with its initial capabilities (executor_id, systems, supported_features, size_class, kind). The scheduler creates the executor entry when it receives a heartbeat from an executor_id that also has an open `BuildExecution` stream. Periodic heartbeats update resource usage. See [rio-scheduler](./scheduler.md#executor-registration-protocol) for deregistration rules.

```protobuf
// store.proto --- inspired by tvix castore/store protos (MIT)
service StoreService {
  rpc PutPath(stream PutPathRequest) returns (PutPathResponse);
  rpc PutPathBatch(stream PutPathBatchRequest) returns (PutPathBatchResponse); // all-or-nothing multi-output upload
  rpc GetPath(GetPathRequest) returns (stream GetPathResponse);
  rpc QueryPathInfo(QueryPathInfoRequest) returns (PathInfo);
  rpc BatchQueryPathInfo(BatchQueryPathInfoRequest) returns (BatchQueryPathInfoResponse);  // I-110 batch: one ANY(...) PG query per BFS layer
  rpc BatchGetManifest(BatchGetManifestRequest) returns (BatchGetManifestResponse);        // I-110c batch: prime FUSE-warm hint cache
  rpc FindMissingPaths(FindMissingPathsRequest) returns (FindMissingPathsResponse);
  rpc QueryPathFromHashPart(QueryPathFromHashPartRequest) returns (PathInfo);  // wopQueryPathFromHashPart (29)
  rpc AddSignatures(AddSignaturesRequest) returns (AddSignaturesResponse);     // wopAddSignatures (37)
  rpc RegisterRealisation(RegisterRealisationRequest) returns (RegisterRealisationResponse);  // wopRegisterDrvOutput (42)
  rpc QueryRealisation(QueryRealisationRequest) returns (Realisation);         // wopQueryRealisation (43)
  rpc TenantQuota(TenantQuotaRequest) returns (TenantQuotaResponse);           // eventually-consistent quota lookup
}
```

> **PutPath stream shape:** `metadata` (1) → `nar_chunk` (0+) → `trailer` (1, mandatory). The `nar_hash` / `nar_size` go in the **trailer**, NOT the metadata — `metadata.info.nar_hash` MUST be empty (store rejects non-empty as a protocol violation). This enables single-pass streaming: the executor's `HashingChannelWriter` tee reads the file once, hashing + uploading simultaneously (~256 KiB peak memory, down from 8 GiB pre-phase2b).

r[proto.store.batch-rpc]
`BatchQueryPathInfo` and `BatchGetManifest` are **local-only** batch lookups: unlike `QueryPathInfo`/`GetPath` they do NOT do per-path upstream substitution or signature-visibility gating (both would re-introduce N round-trips). Callers needing those semantics use the singular RPCs. The batch RPCs exist because the builder's input-closure BFS + FUSE-warm stat loop were issuing ~800 singular RPCs per build — at 246 concurrent ephemeral builders that saturated the store's PG pool (acquire times → 11s → FUSE breaker → EIO). One batch call per BFS layer backed by `WHERE store_path_hash = ANY($1)` reduced it ~130×.

```protobuf
// Server-side chunking only — PutPath chunks via cas::put_chunked;
// callers fan out GetChunk to reassemble NARs from their manifests.
service ChunkService {
  rpc GetChunk(GetChunkRequest) returns (stream GetChunkResponse);
}

// store.proto — administrative RPCs. Separate service from StoreService
// so it can have distinct RBAC/TLS (admin ops are more privileged than
// PutPath/GetPath). The scheduler's AdminService.TriggerGC proxies to
// this after populating extra_roots from live builds.
service StoreAdminService {
  rpc TriggerGC(GCRequest) returns (stream GCProgress);             // mark/sweep; dry_run rolls back
  rpc VerifyChunks(VerifyChunksRequest) returns (stream VerifyChunksProgress);  // PG↔backend consistency audit (I-040 diag)
  rpc ListUpstreams(ListUpstreamsRequest) returns (ListUpstreamsResponse);      // per-tenant upstream cache CRUD
  rpc AddUpstream(AddUpstreamRequest) returns (UpstreamInfo);                   //   (r[store.substitute.upstream])
  rpc RemoveUpstream(RemoveUpstreamRequest) returns (Empty);
  rpc GetLoad(GetLoadRequest) returns (GetLoadResponse);            // per-replica pg_pool_utilization for ComponentScaler
}

// admin.proto — implemented by the rio-scheduler process (co-located with
// SchedulerService). The rio-cli and rio-dashboard call these RPCs.
// gRPC-Web compatibility required for the dashboard (via tonic-web).
service AdminService {
  rpc ClusterStatus(Empty) returns (ClusterStatusResponse);
  rpc ListExecutors(ListExecutorsRequest) returns (ListExecutorsResponse);
  rpc ListBuilds(ListBuildsRequest) returns (ListBuildsResponse);
  rpc GetBuildLogs(GetBuildLogsRequest) returns (stream BuildLogChunk);
  rpc TriggerGC(GCRequest) returns (stream GCProgress);
  rpc DrainExecutor(DrainExecutorRequest) returns (DrainExecutorResponse);
  rpc ClearPoison(ClearPoisonRequest) returns (ClearPoisonResponse);
  rpc ListTenants(Empty) returns (ListTenantsResponse);
  rpc CreateTenant(CreateTenantRequest) returns (CreateTenantResponse);
  rpc GetBuildGraph(GetBuildGraphRequest) returns (GetBuildGraphResponse);  // PG-backed DAG + live status colors (dashboard polls 5s)
  rpc GetSizeClassStatus(GetSizeClassStatusRequest) returns (GetSizeClassStatusResponse);  // SITA-E cutoffs + per-class queued/running
  rpc ListPoisoned(Empty) returns (ListPoisonedResponse);
  rpc InspectBuildDag(InspectBuildDagRequest) returns (InspectBuildDagResponse);  // actor in-memory DAG snapshot (I-025 diag)
  rpc DebugListExecutors(Empty) returns (DebugListExecutorsResponse);             // actor in-memory executor map (I-048b/c diag)
  rpc GetCapacityManifest(GetCapacityManifestRequest) returns (GetCapacityManifestResponse);  // queued-ready per-drv resource estimates (ADR-020 Manifest sizing)
  rpc GetEstimatorStats(GetEstimatorStatsRequest) returns (GetEstimatorStatsResponse);        // build_history EMA snapshot (I-124 diag)
}
```

r[proto.admin.diag-rpc]
`InspectBuildDag` and `DebugListExecutors` query the scheduler actor's **in-memory** state — what `dispatch_ready()` sees — NOT PostgreSQL. `GetBuildGraph`/`ListExecutors` read PG (work for completed builds, survive actor restart); the diagnostic pair surface live dispatch-filter inputs that PG can't show: `executor_has_stream` (assigned to a dead-stream executor = stuck forever, I-025) and `has_stream`/`warm`/`draining`/`store_degraded` per executor (PG `last_seen` says alive but actor map empty = bidi stream stuck on TCP keepalive to old leader, I-048b/c). `DebugListExecutors` is NOT leader-gated — a standby's empty map is itself diagnostic.

> **TriggerGC layering:** `AdminService.TriggerGC` (scheduler) proxies to `StoreAdminService.TriggerGC` (store). The scheduler populates `GCRequest.extra_roots` with expected output paths from all non-terminal derivations before forwarding — this protects in-flight build outputs that the executor hasn't uploaded yet. Calling `StoreAdminService.TriggerGC` directly bypasses this protection.

## Key Messages

### BuildExecution Bidirectional Stream

r[proto.stream.bidi]
The `BuildExecution` RPC replaces the previous `PullWork` + `ReportCompletion` design with a single bidirectional stream per executor, enabling:

- Scheduler-to-executor signals (assignment, cancellation, prefetch hints) without out-of-band RPCs
- Executor-to-scheduler signals (log batches, completion, ack) with reliability guarantees
- Assignment acknowledgment: the executor confirms receipt of each assignment

```protobuf
message ExecutorMessage {
  oneof msg {
    WorkAssignmentAck ack = 1;       // Executor confirms receipt of assignment
    BuildLogBatch log_batch = 2;      // Batched log lines (not per-line)
    CompletionReport completion = 3;  // Build result
    ProgressUpdate progress = 4;      // Resource usage, build phase
    ExecutorRegister register = 5;    // First message on BuildExecution stream:
                                      //   executor_id identity. Scheduler reads this
                                      //   to associate stream + heartbeat by same ID.
    PrefetchComplete prefetch_complete = 6;  // Warm-gate ACK: FUSE cache warmed the hinted paths
    BuildPhase phase = 7;             // Build phase change (forwarded resSetPhase)
  }
}

message SchedulerMessage {
  oneof msg {
    WorkAssignment assignment = 1;    // New work to execute
    CancelSignal cancel = 2;          // Cancel a specific derivation (executor pod termination only)
    PrefetchHint prefetch = 3;        // Paths to pre-warm in FUSE cache
  }
}
```

### ExecutorKind

r[proto.executor.kind]
`ExecutorKind` (in `build_types.proto`) is a two-value enum: `EXECUTOR_KIND_BUILDER = 0` (airgapped, runs arbitrary derivation code) and `EXECUTOR_KIND_FETCHER = 1` (open egress, FOD-only, hash-check bounded). Same `rio-builder` binary, different `RIO_EXECUTOR_KIND` env. The executor reports its kind in `HeartbeatRequest.kind`; the scheduler routes FODs to fetchers only and non-FODs to builders only — no fallback across kinds. Default is `BUILDER` (wire-compatible: old executors don't send field 10, scheduler reads zero).

### BuildPhase

`BuildPhase` carries a per-derivation phase change forwarded from the daemon's `STDERR_RESULT{SetPhase}` (e.g. `"unpackPhase"`, `"buildPhase"`). It is its **own** `oneof` arm on both `ExecutorMessage` and `BuildEvent` — NOT piggybacked on `BuildLogBatch` — so the scheduler relay stays a pure pass-through (no inspection of batch contents) and a phase edge isn't subject to the batcher's 100ms / 64-line buffering.

### BuildLogBatch

Log lines are **batched** for efficiency rather than sent per-line. The executor buffers up to 64 lines or 100ms (whichever comes first) and sends a batch. Use `bytes` (not `string`) for log content since build output may contain non-UTF-8 data.

```protobuf
message BuildLogBatch {
  string derivation_path = 1;    // Which derivation produced these lines
  repeated bytes lines = 2;      // Batch of log lines (raw bytes, not UTF-8)
  uint64 first_line_number = 3;  // For ordering
  string executor_id = 4;        // For debugging
}
```

### CompletionReport

Executor → scheduler message on the `BuildExecution` stream reporting the result of a single derivation build, including cgroup-v2-derived resource metrics:

```protobuf
message CompletionReport {
  string drv_path = 1;           // Derivation that completed
  BuildResult result = 2;        // Build result details (status, outputs, timing)
  string assignment_token = 3;   // Echoed from WorkAssignment
  uint64 peak_memory_bytes = 4;  // memory.peak from per-build cgroup (tree-wide, single read at end)
  uint64 output_size_bytes = 5;  // Sum of NAR sizes uploaded across all outputs
  double peak_cpu_cores = 6;     // Max of 1Hz-sampled cpu.stat delta (cores-equivalent; double for fractional cores)
}
```

`peak_memory_bytes` / `peak_cpu_cores` feed the `build_history` EMA columns for size-class memory-bump routing. Zero is the no-signal sentinel (cgroup setup failed or build failed before the cgroup was populated) — the scheduler keeps the prior EMA instead of dragging toward zero. cgroup v2 is a **hard requirement**; the executor fails startup if the delegated subtree is unavailable.

### HeartbeatRequest

r[proto.heartbeat.capability-fields]
Executors include inventory data in heartbeats so the scheduler can make informed placement decisions. Fields 8–11 are the **dispatch-filter capability set** the scheduler reads on every heartbeat: `size_class` (static, from `builder.toml`; empty = wildcard; fetchers always empty), `store_degraded` (FUSE breaker open → `has_capacity()` returns false), `kind` (builder/fetcher routing), `draining` (executor-authoritative — the executor knows whether it received SIGTERM; scheduler sets `worker.draining` from this field, NOT from `DrainExecutor` RPC or reconnect inference). All four default to zero/false (wire-compatible with old executors).

```protobuf
message HeartbeatRequest {
  string executor_id = 1;
  reserved 2;                      // was repeated string running_builds (→ optional running_build, field 12)
  ResourceUsage resources = 3;
  reserved 4;                      // was BloomFilter local_paths — locality routing dropped with persistent executors
  repeated string systems = 5;     // Systems this executor builds for (e.g. ["x86_64-linux", "aarch64-linux"])
  repeated string supported_features = 6;  // e.g. ["big-parallel", "kvm"]
  reserved 7;                      // was max_builds (always 1 now)
  string size_class = 8;           // Static size-class from builder.toml ("small"/"large"/"" = wildcard)
  bool store_degraded = 9;         // Store-upload circuit breaker OPEN; scheduler routes away until cleared
  ExecutorKind kind = 10;          // builder (airgapped) or fetcher (open egress, FOD-only)
  bool draining = 11;              // executor-authoritative drain flag; scheduler stops dispatching
  optional string running_build = 12;  // drv path in flight (at most one per executor); unset = idle
}
```

> **Locality routing removed:** Field 4 previously carried a bloom filter of cached store paths for transfer-cost-weighted placement. Under the ephemeral one-build-per-pod model an executor has no warm cache to advertise, so locality routing was dropped entirely.

### BuildEvent

Build progress is streamed to clients (gateways and dashboard) via `BuildEvent`:

```protobuf
message BuildEvent {
  string build_id = 1;
  uint64 sequence = 2;                     // Monotonic, for stream resumption
  google.protobuf.Timestamp timestamp = 3;
  oneof event {
    BuildStarted started = 4;
    BuildProgress progress = 5;
    BuildLogBatch log = 6;
    DerivationEvent derivation = 7;        // Per-derivation status changes
    BuildCompleted completed = 8;
    BuildFailed failed = 9;
    BuildCancelled cancelled = 10;
    BuildInputsResolved inputs_resolved = 11;  // CA placeholder resolution finished (post-BFS, pre-dispatch)
  }
}

message DerivationEvent {
  string derivation_path = 1;
  oneof status {
    DerivationQueued queued = 2;
    DerivationStarted started = 3;
    DerivationCompleted completed = 4;
    DerivationCached cached = 5;
    DerivationFailed failed = 6;
  }
}
```

The `sequence` field enables stream resumption: `WatchBuild` accepts a `since_sequence` parameter so gateways can reconnect after a restart and resume from where they left off (requires the scheduler to buffer recent events).

### SubmitBuildRequest

```protobuf
message SubmitBuildRequest {
  reserved 1;
  reserved "tenant_id";           // Migrated to tenant_name (field 9) — see below
  string priority_class = 2;       // "ci", "interactive", or "scheduled"
  repeated DerivationNode nodes = 3;  // All derivations in the DAG
  repeated DerivationEdge edges = 4;  // Dependency edges

  // Client build options. For ssh:// these propagate from wopSetOptions;
  // for ssh-ng they're populated gateway-side (P0310) or fall back to
  // executor config defaults (P0215 — ssh-ng never sends wopSetOptions).
  uint64 max_silent_time = 5;
  uint64 build_timeout = 6;
  uint64 build_cores = 7;
  bool keep_going = 8;             // Continue building independent derivations on failure
  string tenant_name = 9;          // Tenant name (from gateway's authorized_keys comment);
                                   //   scheduler resolves to UUID via tenants table.
                                   //   Empty string = single-tenant mode.
}

message DerivationNode {
  string drv_path = 1;             // Store path of the .drv file
  string drv_hash = 2;             // Input-addressed: store path; CA: modular hash
  string pname = 3;                // Package name (for duration estimation)
  string system = 4;               // e.g. "x86_64-linux"
  repeated string required_features = 5;
  repeated string output_names = 6; // e.g. ["out", "dev"]
  bool is_fixed_output = 7;        // FOD detection
  repeated string expected_output_paths = 8;  // Predicted output store paths
                                              // (for scheduler-side cache check: closes
                                              //  TOCTOU between gateway FindMissingPaths
                                              //  and DAG merge)
  bytes drv_content = 9;           // Inline ATerm-serialized .drv. Empty = executor fetches from store.
                                   // Populated by gateway's filter_and_inline_drv ONLY for nodes with
                                   // missing outputs (≤64KB per node, 16MB total DAG budget).
  uint64 input_srcs_nar_size = 10; // Sum of nar_size of this node's input_srcs (direct static sources,
                                   // NOT transitive). Estimator fallback for fresh (pname,system) with
                                   // no build_history. 0 = no-signal (skip fallback, use 30s default).
  bool is_content_addressed = 11;  // CA cutoff: set by gateway from has_ca_floating_outputs() ||
                                   // is_fixed_output(). Gates scheduler's hash-compare on completion.
  bytes ca_modular_hash = 12;      // 32-byte blake3 modular derivation hash (CA nodes from gateway BFS only;
                                   // empty for IA and single_node_from_basic fallback)
  bool needs_resolve = 13;         // ADR-018 shouldResolve: this node needs dispatch-time placeholder resolution
                                   // (CA floating OR IA with a CA-floating input's placeholder in env/args)
}

message DerivationEdge {
  string parent_drv_path = 1;      // Derivation that depends on child
  string child_drv_path = 2;       // Dependency
}
```

> **Size limits:** A full nixpkgs stdenv rebuild DAG contains ~60,000 nodes. At ~200 bytes per `DerivationNode`, the message is ~12MB. The gateway enforces `MAX_DAG_NODES` (1,048,576) before constructing the request. gRPC max message size should be set to at least 32MB.

> **Tenant identification:** `tenant_name` is set by the gateway from the SSH `authorized_keys` comment field, not from client-provided data. The scheduler resolves the name to a tenant UUID via the `tenants` table (see `r[sched.tenant.resolve]`). Field 1 (`tenant_id`) is reserved — it was removed when resolution moved scheduler-side. The tenant's JWT is propagated via gRPC metadata (`x-rio-tenant-token`) for downstream authorization checks. Note: `tenant_id` still appears as a UUID-string field in `BuildInfo` and `TenantInfo` — those are the **resolved** UUID, not the pre-resolution name.

> **BuildResultStatus ↔ Nix BuildStatus mapping:** The gRPC `BuildResultStatus` enum is a **subset** of Nix's wire `BuildStatus` with a different numbering scheme. The proto enum has `UNSPECIFIED=0` (proto3 default), then `BUILT=1`, `SUBSTITUTED=2`, `ALREADY_VALID=3`, `PERMANENT_FAILURE=4`, `TRANSIENT_FAILURE=5`, `CACHED_FAILURE=6`, `DEPENDENCY_FAILED=7`, `LOG_LIMIT_EXCEEDED=8`, `OUTPUT_REJECTED=9`, `INFRASTRUCTURE_FAILURE=10`. This differs from Nix's wire values (where `TransientFailure=6`, `DependencyFailed=10`). The executor (`executor.rs`) and gateway translate explicitly; they do NOT map 1:1. The proto enum is currently missing `InputRejected`, `TimedOut`, `MiscFailure`, `NotDeterministic`, `ResolvesToAlreadyValid`, and `NoSubstituters` — these Nix statuses currently round-trip through `PERMANENT_FAILURE` or `TRANSIENT_FAILURE` in the gRPC layer. `InfrastructureFailure` is gRPC-only (executor-internal errors: daemon crash, overlay failure); the gateway maps it to Nix `TransientFailure` (6).

### WatchBuildRequest

Decouples observation from submission. The dashboard and reconnecting gateways use this to subscribe to an existing build's event stream:

```protobuf
message WatchBuildRequest {
  string build_id = 1;
  uint64 since_sequence = 2;  // Resume from this sequence number (0 = from start)
}
```

## Proto File Organization

| File | Contents |
|---|---|
| `scheduler.proto` | `SchedulerService` --- gateway-facing RPCs (SubmitBuild, WatchBuild, QueryBuildStatus, CancelBuild, ResolveTenant) |
| `builder.proto` | `ExecutorService` --- executor-facing RPCs (BuildExecution, Heartbeat); covers builder + fetcher pods |
| `store.proto` | `StoreService`, `ChunkService`, `StoreAdminService` |
| `admin.proto` | `AdminService` --- dashboard and CLI RPCs |
| `types.proto` | Shared primitives: `PathInfo`, `ResourceUsage`, `BuildResultStatus`, `ExecutorKind`, store/chunk/GC/realisation RPC messages |
| `dag.proto` | DAG wire types: `DerivationNode`/`Edge`/`Event*`, `GraphNode`/`Edge`, `GetBuildGraph*` |
| `build_types.proto` | Build lifecycle: `BuildEvent*`, `SubmitBuildRequest`, `BuildResult`, `BuildStatus`, `ExecutorMessage`/`SchedulerMessage` bidi-stream types, `BuildPhase`, `Heartbeat*` |
| `admin_types.proto` | Admin RPC data types: `ClusterStatusResponse`, `ListExecutors*`/`Builds*`/`Tenants*`, `SizeClassStatus`, `DrainExecutor*`, `ClearPoison*` |

> **File layout vs. Rust module:** the four data-type `.proto` files all declare `package rio.types;`, so prost merges them into a single generated `rio.types.rs`. Rust callers see everything at `rio_proto::types::*` regardless of which source file a message lives in. The file split is for proto-file review locality only; there is no corresponding Rust namespace split.

Executor-facing RPCs are in a separate `ExecutorService` (in `builder.proto`) to allow distinct interceptors (auth, rate-limiting), independent evolution, and potential future separation to a dedicated port. Both `SchedulerService` and `ExecutorService` are served by the same scheduler binary.

## gRPC Configuration

**Max message size:** The default gRPC max message size (4MB) is insufficient for rio-build. A full nixpkgs stdenv rebuild DAG contains ~60,000 nodes (~12MB serialized). All gRPC services must be configured with `max_message_size = 32MB` (configurable via `RIO_GRPC_MAX_MESSAGE_SIZE`).

**Why not streaming DAG submission?** Streaming the DAG in batches was considered but rejected for Phase 1 simplicity. The single-message approach is adequate for nixpkgs stdenv and the overwhelming majority of real-world DAGs. If future workloads routinely exceed 32MB, a streaming `SubmitBuild` RPC can be added as a non-breaking protocol extension (new RPC, old one remains).

**Per-service configuration:** The `max_message_size` applies to all gRPC services:
- Gateway -> Scheduler (`SubmitBuild` is the largest message)
- Gateway -> Store (`GetPath` responses for large NARs use streaming, so unaffected)
- Executor -> Scheduler (`BuildExecution` stream messages are individually small)
- Executor -> Store (`PutPath` uses streaming, so unaffected)

## Client Helpers

`rio_proto::client` provides typed connection helpers so daemons don't open-code `Endpoint` construction. The `ProtoClient` trait associates each generated `XServiceClient<Channel>` with its `grpc.health.v1` service name and TLS-domain override; `ProtoClient::wrap` applies `max_message_size` once so per-binary connect blocks can't drift on where it's set.

r[proto.client.balanced]
K8s daemons MUST use `rio_proto::client::connect<C>(addrs)` (dispatches single-channel vs. health-aware balanced from `UpstreamAddrs`). When `balance_host` is set, `BalancedChannel` DNS-resolves the headless Service, probes each pod IP via `grpc.health.v1/Check` with the **named** service (e.g. `rio.scheduler.SchedulerService` — NOT empty string), and feeds `Change::Insert` for `SERVING` / `Change::Remove` for `NOT_SERVING` into tonic's p2c balancer. The scheduler runs `replicas=2`; only the leader serves RPCs (the standby returns `Unavailable` from leader-gated handlers). p2c only ejects on connection-level failure, so without the out-of-band health probe it would keep routing ~50% of calls to the standby. `BalancedChannel::new` blocks until the first probe cycle finds ≥1 `SERVING` endpoint. The TLS domain override (`ClientTlsConfig::domain_name`) decouples SAN verification from the connect URI so pod-IP connections verify against the Service-name cert. The h2 keepalive (30s PING + 10s PONG timeout) is NOT optional: `Change::Remove` drops the endpoint from selection but doesn't close existing TCP connections — without keepalive, a SIGKILLed peer (no FIN) leaves in-flight bidi streams pinned for kernel-TCP-keepalive (~2h). Named single-channel wrappers (`connect_store`, `connect_scheduler`, `connect_executor`, `connect_admin`, `connect_store_admin`) remain for tests, rio-cli, and ad-hoc callers.

**`current_traceparent()`** (in `rio_proto::interceptor`) returns the current span's W3C traceparent as a string for embedding in non-gRPC payloads — `WorkAssignment.traceparent` is the load-bearing case (ssh-ng has no metadata channel; see `r[sched.trace.assignment-traceparent]`). Pairs with `span_from_traceparent()` on the receiving side.

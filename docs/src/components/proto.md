# rio-proto

Internal gRPC APIs between components + external API for tooling.

## Services

```protobuf
// scheduler.proto --- gateway-facing RPCs
service SchedulerService {
  rpc SubmitBuild(SubmitBuildRequest) returns (stream BuildEvent);
  rpc WatchBuild(WatchBuildRequest) returns (stream BuildEvent);
  rpc QueryBuildStatus(QueryBuildRequest) returns (BuildStatus);
  rpc CancelBuild(CancelBuildRequest) returns (CancelBuildResponse);
}

// worker.proto --- worker-facing RPCs (same server process as SchedulerService)
service WorkerService {
  // Bidirectional stream: scheduler sends assignments + prefetch hints + cancel signals;
  // worker sends log batches + completion reports + ack messages
  rpc BuildExecution(stream WorkerMessage) returns (stream SchedulerMessage);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
}
```

> **Worker registration:** Worker registration is implicit and two-step: (1) the worker opens a `BuildExecution` bidirectional stream, (2) the worker calls the separate `Heartbeat` unary RPC with its initial capabilities (worker_id, systems, supported_features, max_builds, size_class, bloom_filter). The scheduler creates the worker entry when it receives a heartbeat from a worker_id that also has an open `BuildExecution` stream. Periodic heartbeats update the bloom filter and resource usage. See [rio-scheduler](./scheduler.md#worker-registration-protocol) for deregistration rules.

```protobuf
// store.proto --- inspired by tvix castore/store protos (MIT)
service StoreService {
  rpc PutPath(stream PutPathRequest) returns (PutPathResponse);
  rpc GetPath(GetPathRequest) returns (stream GetPathResponse);
  rpc QueryPathInfo(QueryPathInfoRequest) returns (PathInfo);
  rpc FindMissingPaths(FindMissingPathsRequest) returns (FindMissingPathsResponse);
  rpc ContentLookup(ContentLookupRequest) returns (ContentLookupResponse);
  rpc QueryPathFromHashPart(QueryPathFromHashPartRequest) returns (PathInfo);  // wopQueryPathFromHashPart (29)
  rpc AddSignatures(AddSignaturesRequest) returns (AddSignaturesResponse);     // wopAddSignatures (37)
  rpc RegisterRealisation(RegisterRealisationRequest) returns (RegisterRealisationResponse);  // wopRegisterDrvOutput (42)
  rpc QueryRealisation(QueryRealisationRequest) returns (Realisation);         // wopQueryRealisation (43)
}
```

> **PutPath stream shape:** `metadata` (1) → `nar_chunk` (0+) → `trailer` (1, mandatory). The `nar_hash` / `nar_size` go in the **trailer**, NOT the metadata — `metadata.info.nar_hash` MUST be empty (store rejects non-empty as a protocol violation). This enables single-pass streaming: the worker's `HashingChannelWriter` tee reads the file once, hashing + uploading simultaneously (~256 KiB peak memory, down from 8 GiB pre-phase2b).

```protobuf

service ChunkService {
  rpc PutChunk(stream PutChunkRequest) returns (PutChunkResponse);  // UNIMPLEMENTED — server-side chunking only
  rpc GetChunk(GetChunkRequest) returns (stream GetChunkResponse);
  rpc FindMissingChunks(FindMissingChunksRequest) returns (FindMissingChunksResponse);
}

// admin.proto — implemented by the rio-scheduler process (co-located with
// SchedulerService). The rio-cli and rio-dashboard call these RPCs.
// gRPC-Web compatibility required for the dashboard (via tonic-web).
service AdminService {
  rpc ClusterStatus(Empty) returns (ClusterStatusResponse);
  rpc ListWorkers(ListWorkersRequest) returns (ListWorkersResponse);
  rpc ListBuilds(ListBuildsRequest) returns (ListBuildsResponse);
  rpc GetBuildLogs(GetBuildLogsRequest) returns (stream BuildLogChunk);
  rpc TriggerGC(GCRequest) returns (stream GCProgress);
  rpc DrainWorker(DrainWorkerRequest) returns (DrainWorkerResponse);
  rpc ClearPoison(ClearPoisonRequest) returns (ClearPoisonResponse);
}
```

## Key Messages

### BuildExecution Bidirectional Stream

r[proto.stream.bidi]
The `BuildExecution` RPC replaces the previous `PullWork` + `ReportCompletion` design with a single bidirectional stream per worker, enabling:

- Scheduler-to-worker signals (assignment, cancellation, prefetch hints) without out-of-band RPCs
- Worker-to-scheduler signals (log batches, completion, ack) with reliability guarantees
- Assignment acknowledgment: the worker confirms receipt of each assignment

```protobuf
message WorkerMessage {
  oneof msg {
    WorkAssignmentAck ack = 1;       // Worker confirms receipt of assignment
    BuildLogBatch log_batch = 2;      // Batched log lines (not per-line)
    CompletionReport completion = 3;  // Build result
    ProgressUpdate progress = 4;      // Resource usage, build phase
    WorkerRegister register = 5;      // First message on BuildExecution stream:
                                      //   worker_id identity. Scheduler reads this
                                      //   to associate stream + heartbeat by same ID.
  }
}

message SchedulerMessage {
  oneof msg {
    WorkAssignment assignment = 1;    // New work to execute
    CancelSignal cancel = 2;          // Cancel a specific derivation (worker pod termination only)
    PrefetchHint prefetch = 3;        // Paths to pre-warm in FUSE cache
  }
}
```

### BuildLogBatch

Log lines are **batched** for efficiency rather than sent per-line. The worker buffers up to 64 lines or 100ms (whichever comes first) and sends a batch. Use `bytes` (not `string`) for log content since build output may contain non-UTF-8 data.

```protobuf
message BuildLogBatch {
  string derivation_path = 1;    // Which derivation produced these lines
  repeated bytes lines = 2;      // Batch of log lines (raw bytes, not UTF-8)
  uint64 first_line_number = 3;  // For ordering
  string worker_id = 4;          // For debugging
}
```

### CompletionReport

Worker → scheduler message on the `BuildExecution` stream reporting the result of a single derivation build, including cgroup-v2-derived resource metrics:

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

`peak_memory_bytes` / `peak_cpu_cores` feed the `build_history` EMA columns for size-class memory-bump routing. Zero is the no-signal sentinel (cgroup setup failed or build failed before the cgroup was populated) — the scheduler keeps the prior EMA instead of dragging toward zero. cgroup v2 is a **hard requirement**; the worker fails startup if the delegated subtree is unavailable.

### HeartbeatRequest

Workers include inventory data in heartbeats so the scheduler can make informed placement decisions:

```protobuf
message HeartbeatRequest {
  string worker_id = 1;
  repeated string running_builds = 2;
  ResourceUsage resources = 3;
  BloomFilter local_paths = 4;     // Bloom filter of cached store paths
  repeated string systems = 5;     // Systems this worker builds for (e.g. ["x86_64-linux", "aarch64-linux"])
  repeated string supported_features = 6;  // e.g. ["big-parallel", "kvm"]
  uint32 max_builds = 7;           // Maximum concurrent builds this worker accepts
  string size_class = 8;           // Static size-class from worker.toml ("small"/"large"/"" = wildcard)
}

message BloomFilter {
  bytes data = 1;
  uint32 hash_count = 2;
  uint32 num_bits = 3;
  BloomHashAlgorithm hash_algorithm = 4;  // enum: BLAKE3_256 (Kirsch-Mitzenmacher double-hash).
                                          // MURMUR3_128/XXHASH64 were never implemented; reserved.
  uint32 version = 5;                     // filter format version
}
```

The `BloomFilter` message is self-describing: it includes the hash algorithm and bit count so the scheduler can validate the filter without implicit coupling to worker code. Only `BLAKE3_256` is supported — blake3 is portable and deterministic across architectures (a requirement since the bloom filter crosses the wire between worker and scheduler; AES-NI-dependent hashes would produce different output on heterogeneous clusters).

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
  string tenant_id = 1;            // Tenant identifier (from SSH key mapping)
  string priority_class = 2;       // "ci", "interactive", or "scheduled"
  repeated DerivationNode nodes = 3;  // All derivations in the DAG
  repeated DerivationEdge edges = 4;  // Dependency edges

  // Client build options propagated from wopSetOptions
  uint64 max_silent_time = 5;
  uint64 build_timeout = 6;
  uint64 build_cores = 7;
  bool keep_going = 8;             // Continue building independent derivations on failure
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
  bytes drv_content = 9;           // Inline ATerm-serialized .drv. Empty = worker fetches from store.
                                   // Populated by gateway's filter_and_inline_drv ONLY for nodes with
                                   // missing outputs (≤64KB per node, 16MB total DAG budget).
  uint64 input_srcs_nar_size = 10; // Sum of nar_size of this node's input_srcs (direct static sources,
                                   // NOT transitive). Estimator fallback for fresh (pname,system) with
                                   // no build_history. 0 = no-signal (skip fallback, use 30s default).
}

message DerivationEdge {
  string parent_drv_path = 1;      // Derivation that depends on child
  string child_drv_path = 2;       // Dependency
}
```

> **Size limits:** A full nixpkgs stdenv rebuild DAG contains ~60,000 nodes. At ~200 bytes per `DerivationNode`, the message is ~12MB. The gateway should enforce a per-tenant `max_dag_size` limit (default: 100,000 nodes) before constructing the request. gRPC max message size should be set to at least 32MB.

> **Tenant identification:** `tenant_id` is set by the gateway from the SSH key -> tenant mapping, not from client-provided data. The tenant's JWT is propagated via gRPC metadata (`x-rio-tenant-token`) for downstream authorization checks.

> **BuildResultStatus ↔ Nix BuildStatus mapping:** The gRPC `BuildResultStatus` enum is a **subset** of Nix's wire `BuildStatus` with a different numbering scheme. The proto enum has `UNSPECIFIED=0` (proto3 default), then `BUILT=1`, `SUBSTITUTED=2`, `ALREADY_VALID=3`, `PERMANENT_FAILURE=4`, `TRANSIENT_FAILURE=5`, `CACHED_FAILURE=6`, `DEPENDENCY_FAILED=7`, `LOG_LIMIT_EXCEEDED=8`, `OUTPUT_REJECTED=9`, `INFRASTRUCTURE_FAILURE=10`. This differs from Nix's wire values (where `TransientFailure=6`, `DependencyFailed=10`). The worker (`executor.rs`) and gateway translate explicitly; they do NOT map 1:1. The proto enum is currently missing `InputRejected`, `TimedOut`, `MiscFailure`, `NotDeterministic`, `ResolvesToAlreadyValid`, and `NoSubstituters` — these Nix statuses currently round-trip through `PERMANENT_FAILURE` or `TRANSIENT_FAILURE` in the gRPC layer. `InfrastructureFailure` is gRPC-only (worker-internal errors: daemon crash, overlay failure); the gateway maps it to Nix `TransientFailure` (6).

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
| `scheduler.proto` | `SchedulerService` --- gateway-facing RPCs (SubmitBuild, WatchBuild, QueryBuildStatus, CancelBuild) |
| `worker.proto` | `WorkerService` --- worker-facing RPCs (BuildExecution, Heartbeat) |
| `store.proto` | `StoreService`, `ChunkService` |
| `admin.proto` | `AdminService` --- dashboard and CLI RPCs |
| `types.proto` | Shared message types (BuildEvent, HeartbeatRequest, ResourceUsage, BloomFilter, etc.) |

Worker-facing RPCs are in a separate `WorkerService` (in `worker.proto`) to allow distinct interceptors (auth, rate-limiting), independent evolution, and potential future separation to a dedicated port. Both `SchedulerService` and `WorkerService` are served by the same scheduler binary.

## gRPC Configuration

**Max message size:** The default gRPC max message size (4MB) is insufficient for rio-build. A full nixpkgs stdenv rebuild DAG contains ~60,000 nodes (~12MB serialized). All gRPC services must be configured with `max_message_size = 32MB` (configurable via `RIO_GRPC_MAX_MESSAGE_SIZE`).

**Why not streaming DAG submission?** Streaming the DAG in batches was considered but rejected for Phase 1 simplicity. The single-message approach is adequate for nixpkgs stdenv and the overwhelming majority of real-world DAGs. If future workloads routinely exceed 32MB, a streaming `SubmitBuild` RPC can be added as a non-breaking protocol extension (new RPC, old one remains).

**Per-service configuration:** The `max_message_size` applies to all gRPC services:
- Gateway -> Scheduler (`SubmitBuild` is the largest message)
- Gateway -> Store (`GetPath` responses for large NARs use streaming, so unaffected)
- Worker -> Scheduler (`BuildExecution` stream messages are individually small)
- Worker -> Store (`PutPath` uses streaming, so unaffected)

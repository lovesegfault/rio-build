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

> **Worker registration:** Worker registration is implicit and two-step: (1) the worker opens a `BuildExecution` bidirectional stream, (2) the worker calls the separate `Heartbeat` unary RPC with its initial capabilities (worker_id, system, supported_features, max_builds, bloom_filter). The scheduler creates the worker entry when it receives a heartbeat from a worker_id that also has an open `BuildExecution` stream. Periodic heartbeats update the bloom filter and resource usage. See [rio-scheduler](./scheduler.md#worker-registration-protocol) for deregistration rules.

```protobuf
// store.proto --- inspired by tvix castore/store protos (MIT)
service StoreService {
  rpc PutPath(stream PutPathRequest) returns (PutPathResponse);
  rpc GetPath(GetPathRequest) returns (stream GetPathResponse);
  rpc QueryPathInfo(QueryPathInfoRequest) returns (PathInfo);
  rpc FindMissingPaths(FindMissingPathsRequest) returns (FindMissingPathsResponse);
  rpc ContentLookup(ContentLookupRequest) returns (ContentLookupResponse);
}

service ChunkService {
  rpc PutChunk(stream PutChunkRequest) returns (PutChunkResponse);
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

### HeartbeatRequest

Workers include inventory data in heartbeats so the scheduler can make informed placement decisions:

```protobuf
message HeartbeatRequest {
  string worker_id = 1;
  repeated string running_builds = 2;
  ResourceUsage resources = 3;
  BloomFilter local_paths = 4;     // Bloom filter of cached store paths
}

message BloomFilter {
  bytes data = 1;
  uint32 hash_count = 2;
  uint32 num_bits = 3;
  BloomHashAlgorithm hash_algorithm = 4;  // enum: MURMUR3_128, XXHASH64
  uint32 version = 5;                     // filter format version
}
```

The `BloomFilter` message is self-describing: it includes the hash algorithm and bit count so the scheduler can validate the filter without implicit coupling to worker code.

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
}

message DerivationNode {
  string drv_path = 1;             // Store path of the .drv file
  string drv_hash = 2;             // Input-addressed: store path; CA: modular hash
  string pname = 3;                // Package name (for duration estimation)
  string system = 4;               // e.g. "x86_64-linux"
  repeated string required_features = 5;
  repeated string output_names = 6; // e.g. ["out", "dev"]
  bool is_fixed_output = 7;        // FOD detection
}

message DerivationEdge {
  string parent_drv_path = 1;      // Derivation that depends on child
  string child_drv_path = 2;       // Dependency
}
```

> **Size limits:** A full nixpkgs stdenv rebuild DAG contains ~60,000 nodes. At ~200 bytes per `DerivationNode`, the message is ~12MB. The gateway should enforce a per-tenant `max_dag_size` limit (default: 100,000 nodes) before constructing the request. gRPC max message size should be set to at least 32MB.

> **Tenant identification:** `tenant_id` is set by the gateway from the SSH key -> tenant mapping, not from client-provided data. The tenant's JWT is propagated via gRPC metadata (`x-rio-tenant-token`) for downstream authorization checks.

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

**Max message size:** The default gRPC max message size (4MB) is insufficient for rio-build. A full nixpkgs stdenv rebuild DAG contains ~60,000 nodes (~12MB serialized). All gRPC services must be configured with `max_message_size = 32MB` (configurable via `RIO_GRPC__MAX_MESSAGE_SIZE`).

**Why not streaming DAG submission?** Streaming the DAG in batches was considered but rejected for Phase 1 simplicity. The single-message approach is adequate for nixpkgs stdenv and the overwhelming majority of real-world DAGs. If future workloads routinely exceed 32MB, a streaming `SubmitBuild` RPC can be added as a non-breaking protocol extension (new RPC, old one remains).

**Per-service configuration:** The `max_message_size` applies to all gRPC services:
- Gateway -> Scheduler (`SubmitBuild` is the largest message)
- Gateway -> Store (`GetPath` responses for large NARs use streaming, so unaffected)
- Worker -> Scheduler (`BuildExecution` stream messages are individually small)
- Worker -> Store (`PutPath` uses streaming, so unaffected)

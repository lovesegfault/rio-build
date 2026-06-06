#import "/lib/rio.typ": *
#show: rio.with(domains: ("proto",))

= rio-proto

Internal gRPC APIs between components + external API for tooling.

== Transport

#r("proto.h2.adaptive-window+2")[
  All gRPC channels (client `Endpoint` builders and server
  `Server::builder()`) MUST set an explicit initial per-stream window of at
  least 1 MiB and MUST NOT enable `http2_adaptive_window`.
]

The h2 default 65 535-byte window caps a `GetPath` @nar stream at \~20--30 MB/s
at cross-@az RTT regardless of link bandwidth. hyper's `adaptive_window(true)`
resets `initial_stream_window_size` / `initial_conn_window_size` to
`SPEC_WINDOW_SIZE = 65 535` and BDP-probes upward from there; tonic's builder
applies it after the explicit initial-window calls, so setting both silently
discards the 1 MiB. A fixed 1 MiB gives ≥100 MB/s at RTT ≤10 ms --- no BDP
needed for in-cluster or cross-AZ.

#r("proto.client.streaming-open-bounded")[
  Every generated streaming-RPC open performed by a daemon crate MUST be
  raced against a deadline (and, where one exists, an abort signal) via a
  sanctioned bounding combinator; a naked `.method(req).await` open is
  forbidden.
]

A streaming *open* is the one await a caller's drain signal, grace clock,
or tick budget cannot see: a half-open peer (TCP up, HTTP/2 dead) parks
the task indefinitely. The `streaming-open-ban` policy check enforces
this with a banned-method list derived at check time from the proto
`FileDescriptorSet` --- protoc's own parse --- so a new streaming rpc is
born banned; sanctioned combinators are `rio_common::transport::bounded_open`,
`with_timeout_status`, and `with_timeout`.

#r("proto.h2.keepalive-server")[
  Every component's gRPC server MUST be constructed via
  `rio_common::server::tonic_builder` (or its tuned test variant), which
  applies the shared h2 PING keepalive interval/timeout and TCP
  keepalive; hand-chained per-daemon keepalive overrides are forbidden.
]

Keepalive is a connection-liveness property, not a per-daemon tuning
knob: a vanished peer (SIGKILL, netsplit) must be detected in
\~interval+timeout everywhere, and the client mirrors the same consts
(`rio_common::grpc::H2_KEEPALIVE_INTERVAL`/`_TIMEOUT`) so the directions
cannot drift. The `h2-keepalive-single-source` policy check pins the
knobs to the two chokepoints.

== gRPC Metadata Keys

`x-rio-*` header constants live in `rio_common::grpc` (proto-agnostic,
lowercase per HTTP/2 header rules) and are re-exported at `rio_proto::*` so all
callers reference one path. A typo in a string literal at one site silently
breaks header propagation with no compile-time signal --- using the constant is
mandatory.

#figure(
  table(
    columns: 4,
    align: (left, left, left, left),
    table.header([Constant], [Header], [Direction], [Carries]),
    [`BUILD_ID_HEADER`],
    [`x-rio-build-id`],
    [scheduler → gateway (response initial-metadata)],
    [UUIDv7 build_id, set BEFORE first stream message so the gateway has it
      even on zero-event streams],

    [`TRACE_ID_HEADER`],
    [`x-rio-trace-id`],
    [scheduler → gateway (response initial-metadata)],
    [32-hex W3C trace_id of the scheduler handler span --- see
      #rref("obs.trace.scheduler-id-in-metadata")],

    [`ASSIGNMENT_TOKEN_HEADER`],
    [`x-rio-assignment-token`],
    [executor → store (request metadata on `PutPath` / `PutPathBatch`)],
    [HMAC-SHA256 token signed by scheduler; store verifies (executor_id,
      drv_hash, expected_outputs, expiry)],

    [`TENANT_TOKEN_HEADER`],
    [`x-rio-tenant-token`],
    [gateway → scheduler/store (request metadata)],
    [ed25519 JWT; missing header is pass-through (single-tenant mode),
      present-but-invalid is `Unauthenticated`],
  ),
)

#r("proto.metadata.build-id")[
  `x-rio-build-id` MUST be set by the scheduler on `SubmitBuild` response
  *initial* metadata. Server-streaming RPCs send headers before any stream
  message, so the gateway can record the build_id even if the scheduler dies
  between MergeDag commit and the first `BuildEvent`.
]

#r("proto.metadata.assignment-token")[
  `x-rio-assignment-token` is the *only* input the store trusts when
  authorizing `PutPath`. The token is minted scheduler-side at dispatch (HMAC
  over executor_id + drv_hash + expected_outputs + expiry) and carried through
  the executor verbatim. The store MUST reject uploads with a missing,
  expired, or mismatched-output token. Builder pods are airgapped and
  untrusted --- builder-supplied data MUST NOT drive authorization; the token
  is the cryptographic link back to a scheduler decision.
]

#r("proto.metadata.tenant-token")[
  `x-rio-tenant-token` is set by the gateway on every outbound RPC in JWT
  mode. Server-side (`rio_auth::jwt_interceptor`): missing header is
  pass-through (dual-mode / single-tenant), present-but-unverifiable is
  `Unauthenticated`. Verified claims populate request extensions; handlers
  read tenant identity from extensions, never from request body fields.
]

== Services

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
// Covers BOTH builder and fetcher pods (same binary, RIO_EXECUTOR_KIND env).
service ExecutorService {
  // Pull-mode dispatch: a pod born knowing its derivation asks for it and
  // reports the outcome; no registration, heartbeat, or stream.
  rpc PullAssignment(PullAssignmentRequest) returns (PullAssignmentResponse);
  rpc ReportOutcome(ReportOutcomeRequest) returns (ReportOutcomeResponse);
}
```

#info(title: [No executor registration])[
  There is no registration step: the executor-lifecycle collapse removed the
  `BuildExecution` stream, the `Heartbeat` unary, and the in-memory executor
  entry they fed. A pod's existence is the controller's Job census; its
  liveness is the kubelet/Job lifecycle; the scheduler learns about it only
  through its pull (which binds the open attempt) and its report (or the
  pod-terminal / establishment classifiers when the report never arrives).
]

#info(title: [Pull-mode dispatch messages])[
  `PullAssignmentRequest{executor_token, intent_id}` →
  `PullAssignmentResponse{oneof outcome: WorkAssignment | Gone |
  NotYetReady{retry_after_seconds}}`: the pull-mode pod's single ask. The
  `WorkAssignment` arm reuses the existing dispatch payload unchanged (its
  `generation` field is observability-only on this path --- the fence is
  transaction-side). `ReportOutcomeRequest{exec_id, CompletionReport}` →
  `ReportOutcomeResponse{}` carries today's completion payload, idempotent
  by `exec_id`. On the admin surface,
  `ReportAttemptOutcomeRequest{intent_id, job_name, exec_id,
  AttemptTerminalReason, node_name}` is the controller's unified
  pod-terminal classification (the C4/C5 unification), and
  `ListOpenAttemptsRequest{}` → `ListOpenAttemptsResponse{repeated
  OpenAttempt, leader_for_secs}` is the ledger-backed open-attempt view
  (intent id, derivation, exec_id, executor identity, source node,
  generation, assignment age, deadline) that serves the controller's busy
  bridge and the operator fleet view. Semantics live in the rio-scheduler
  chapter §Pull-Mode Dispatch, the rio-controller chapter §Pull-mode
  attempt lifecycle, and the rio-builder chapter §Pull-Mode Client.
]

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

#info(title: [PutPath stream shape])[
  `metadata` (1) → `nar_chunk` (0+) → `trailer` (1, mandatory). The
  `nar_hash` / `nar_size` go in the *trailer*, NOT the metadata ---
  `metadata.info.nar_hash` MUST be empty (store rejects non-empty as a
  protocol violation). This enables single-pass streaming: the executor's
  `HashingChannelWriter` tee reads the file once, hashing + uploading
  simultaneously (\~256 KiB peak memory, down from 8 GiB pre-phase2b).
]

#r("proto.store.batch-rpc")[
  `BatchQueryPathInfo` and `BatchGetManifest` are *local-only* batch lookups:
  unlike `QueryPathInfo` / `GetPath` they do NOT do per-path upstream
  substitution or signature-visibility gating (both would re-introduce N
  round-trips). Callers needing those semantics use the singular RPCs.
]

The batch RPCs exist because the builder's input-@closure BFS + #gls("fuse")-warm stat
loop were issuing \~800 singular RPCs per build --- at 246 concurrent ephemeral
builders that saturated the store's PG pool (acquire times → 11s → FUSE breaker
→ EIO). One batch call per BFS layer backed by `WHERE store_path_hash =
ANY($1)` reduced it \~130×.

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
  rpc TriggerGC(GCRequest) returns (stream GCProgress);
  rpc CancelBuild(CancelBuildRequest) returns (CancelBuildResponse);  // operator override (caller_tenant=None); service-token gated
  rpc ClearPoison(ClearPoisonRequest) returns (ClearPoisonResponse);
  rpc ListTenants(Empty) returns (ListTenantsResponse);
  rpc CreateTenant(CreateTenantRequest) returns (CreateTenantResponse);
  rpc GetBuildGraph(GetBuildGraphRequest) returns (GetBuildGraphResponse);  // PG-backed DAG + live status colors (dashboard polls 5s)
  rpc GetSpawnIntents(GetSpawnIntentsRequest) returns (GetSpawnIntentsResponse);  // ADR-023 per-drv spawn intents, kind/system/feature filtered
  rpc ListPoisoned(Empty) returns (ListPoisonedResponse);
  rpc InspectBuildDag(InspectBuildDagRequest) returns (InspectBuildDagResponse);  // actor in-memory DAG snapshot (I-025 diag)
  rpc ReportAttemptOutcome(ReportAttemptOutcomeRequest) returns (ReportAttemptOutcomeResponse);  // pod-terminal classification (C4/C5 unification)
  rpc ListOpenAttempts(ListOpenAttemptsRequest) returns (ListOpenAttemptsResponse);              // ledger-backed open pull-mode attempts (busy bridge + OA5 fleet view)
}
```

#r("proto.admin.diag-rpc+2")[
  `InspectBuildDag` queries the scheduler actor's *in-memory* state --- what
  the pull-admission and retry logic see --- NOT PostgreSQL.
  `GetBuildGraph` / `ListExecutors` read durable state (work for completed
  builds, survive actor restart); the diagnostic surfaces live retry/backoff
  and open-attempt inputs that the durable views can't show (I-025 / I-062).
  (`DebugListExecutors` was removed with the in-memory executor map at the
  proto sweep; the open-attempt view plus the Job/pod census are the
  successors.)
]

#info(title: [TriggerGC layering])[
  `AdminService.TriggerGC` (scheduler) proxies to `StoreAdminService.TriggerGC`
  (store). The scheduler populates `GCRequest.extra_roots` with expected output
  paths from all non-terminal derivations before forwarding --- this protects
  in-flight build outputs that the executor hasn't uploaded yet. Calling
  `StoreAdminService.TriggerGC` directly bypasses this protection.
]

== Key Messages

=== BuildExecution stream (removed)

*Retired (executor-lifecycle collapse --- the stream session protocol):*
`proto.stream.bidi` normed the `BuildExecution` bidirectional stream (one
per executor, carrying assignment/cancel/prefetch downstream and
ack/log-batch/completion upstream). The RPC, its scheduler-side session
state, and the stream-only message arms were removed: dispatch is the
pull unary (`PullAssignment` binds the open attempt), results travel on
the `ReportOutcome` unary, log batches go to rio-store's
`LogService.AppendLog`, and cancellation is pod termination via the
controller's Job census. The rule id is retired with the section --- no
live surface remains for it to norm.

`ExecutorMessage` survives only as the builder-internal build-task →
runtime envelope (the spawned build task hands its completion and phase
edges to the pull loop through a process-lifetime channel typed with this
message; the pull loop forwards them via `ReportOutcome`). It is not sent
on any wire. The stream-era arms are reserved tombstones, quoting
`build_types.proto`:

```protobuf
message ExecutorMessage {
  reserved 4;  // was ProgressUpdate (mid-build ema; consumer removed with legacy sizer)
  // 2 was `BuildLogBatch log_batch` — batched log lines. Removed by the
  // build-log data-plane cutover: builders stream log batches to
  // rio-store's LogService.AppendLog instead of the scheduler.
  reserved 2;
  // 1 was `WorkAssignmentAck ack`, 5 was `ExecutorRegister register`,
  // 6 was `PrefetchComplete prefetch_complete` — removed with the
  // BuildExecution stream (executor-lifecycle collapse).
  reserved 1, 5, 6;
  reserved "ack", "register", "prefetch_complete";
  oneof msg {
    CompletionReport completion = 3;        // Build result
    BuildPhase phase = 7;                   // Build phase change (forwarded resSetPhase)
  }
}
```

The scheduler-to-executor envelope (`SchedulerMessage`, with its
assignment/cancel/prefetch arms) was deleted outright --- the pull
response carries the assignment, and the other two arms have no
pull-mode counterpart.

=== ExecutorKind

#r("proto.executor.kind+2")[
  `ExecutorKind` (in `build_types.proto`) is a two-value enum:
  `EXECUTOR_KIND_BUILDER = 0` (airgapped, runs arbitrary derivation code) and
  `EXECUTOR_KIND_FETCHER = 1` (open egress, FOD-only, hash-check bounded).
  Same `rio-builder` binary, different `RIO_EXECUTOR_KIND` env. Kind is
  fixed at spawn, not reported on the wire: the spawn-intent pool
  eligibility chokepoint routes FODs to the fetcher pool only and
  non-FODs to non-fetcher pools only (enforced at construction --- no
  fallback across kinds), the controller injects `RIO_EXECUTOR_KIND`
  from the pool spec into the pod, and the executor's own kind gate
  re-checks the assignment class against its env before building.
]

=== BuildPhase

`BuildPhase` carries a per-derivation phase change forwarded from the daemon's
`STDERR_RESULT{SetPhase}` (e.g. `"unpackPhase"`, `"buildPhase"`). It is its
*own* `oneof` arm on both the builder-internal `ExecutorMessage` envelope and
the wire-facing `BuildEvent` --- NOT piggybacked on `BuildLogBatch` --- because
it is a control-plane state edge the scheduler consumes, while log batches are
data-plane payload that no longer transits the scheduler at all, and a phase
edge isn't subject to the batcher's 100ms / 64-line buffering.

=== BuildLogBatch

Log lines are *batched* for efficiency rather than sent per-line. The executor
buffers up to 64 lines or 100ms (whichever comes first) and sends a batch ---
since the log-data-plane move, on the builder's `LogService.AppendLog` stream
to rio-store rather than to the scheduler (whose stream-era log relay was
removed with the `BuildExecution` stream). Use
`bytes` (not `string`) for log content since build output may contain non-UTF-8
data.

```protobuf
message BuildLogBatch {
  string derivation_path = 1;    // Which derivation produced these lines
  repeated bytes lines = 2;      // Batch of log lines (raw bytes, not UTF-8)
  uint64 first_line_number = 3;  // For ordering
  string executor_id = 4;        // For debugging
}
```

=== CompletionReport

The build-result payload for a single derivation, including
cgroup-v2-derived resource metrics. The build task hands it to the pull
loop inside the builder-internal `ExecutorMessage` envelope; the wire
carrier is the `ReportOutcome` unary (the stream-era carrier was removed
with the `BuildExecution` stream):

```protobuf
message CompletionReport {
  string drv_path = 1;           // Derivation that completed
  BuildResult result = 2;        // Build result details (status, outputs, timing)
  string assignment_token = 3;   // Echoed from WorkAssignment
  uint64 peak_memory_bytes = 4;  // memory.peak from per-build cgroup (tree-wide, single read at end)
  reserved 5;                    // was output_size_bytes
  double peak_cpu_cores = 6;     // Max of 1Hz-sampled cpu.stat delta (cores-equivalent; double for fractional cores)
}
```

`peak_memory_bytes` / `peak_cpu_cores` feed the `build_samples` table for the
ADR-023 @sla fit. Zero is the no-signal sentinel (cgroup setup failed or build
failed before the cgroup was populated). cgroup v2 is a *hard requirement*; the
executor fails startup if the delegated subtree is unavailable.

=== HeartbeatRequest (removed)

*Retired (1d proto sweep --- the heartbeat protocol):*
`proto.heartbeat.capability-fields` normed the dispatch-filter capability set
(`store_degraded`, `kind`, `draining`, `running_build`) the scheduler read on
every heartbeat. `HeartbeatRequest`/`HeartbeatResponse` and the `Heartbeat`
RPC were removed with the stream session: there is no scheduler-side
registration or capacity state left for those fields to feed. Capability
matching happens at the spawn-intent filter (`GetSpawnIntentsRequest.{kind,
systems, filter_features}`), the kind boundary is enforced at spawn and
re-checked by the executor's own kind gate
(#rref("builder.executor.kind-gate")), and a degraded store surfaces as the
affected build's infra-classed outcome rather than a capacity flag.

=== BuildEvent

Build progress is streamed to clients (gateways and dashboard) via
`BuildEvent`:

```protobuf
message BuildEvent {
  string build_id = 1;
  // 2 was `uint64 sequence` — removed with the WatchBuild resumability layer
  google.protobuf.Timestamp timestamp = 3;
  oneof event {
    BuildStarted started = 4;
    BuildProgress progress = 5;
    // 6 was BuildLogBatch log — the live tail now reaches the gateway via
    // rio-store's LogService.TailLog, not the scheduler's event stream.
    DerivationEvent derivation = 7;        // Per-derivation status changes
    BuildCompleted completed = 8;
    BuildFailed failed = 9;
    BuildCancelled cancelled = 10;
    BuildInputsResolved inputs_resolved = 11;  // CA placeholder resolution finished (post-BFS, pre-dispatch)
    BuildSnapshot snapshot = 14;           // Full-state snapshot (first WatchBuild message)
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

Reconnection is snapshot-first: a `WatchBuild` stream's first message is a
`BuildSnapshot` describing the build's current state (aggregate counts, the
per-derivation running set, terminal outcome if any), then the live broadcast
follows. There is no event cursor and no replay --- see the scheduler spec's
snapshot-first rule.

=== SubmitBuildRequest

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
  reserved 10;                     // was input_srcs_nar_size (closure-size proxy; ADR-023 supersedes)
  bool is_content_addressed = 11;  // CA cutoff: set by gateway from has_ca_floating_outputs() ||
                                   // is_fixed_output(). Gates scheduler's hash-compare on completion.
  bytes ca_modular_hash = 12;      // 32-byte blake3 modular derivation hash (CA nodes from gateway BFS only;
                                   // empty for IA and single-node BasicDerivation fallback)
  bool needs_resolve = 13;         // ADR-018 shouldResolve: this node needs dispatch-time placeholder resolution
                                   // (CA floating OR IA with a CA-floating input's placeholder in env/args)
}

message DerivationEdge {
  string parent_drv_path = 1;      // Derivation that depends on child
  string child_drv_path = 2;       // Dependency
}
```

#info(title: [Size limits])[
  A full nixpkgs stdenv rebuild @dag contains \~60,000 nodes. At \~200 bytes per
  `DerivationNode`, the message is \~12MB. The gateway enforces `MAX_DAG_NODES`
  (1,048,576) before constructing the request. gRPC max message size should be
  set to at least 32MB.
]

#info(title: [Tenant identification])[
  `tenant_name` is set by the gateway from the SSH `authorized_keys` comment
  field, not from client-provided data. The scheduler resolves the name to a
  tenant UUID via the `tenants` table (see #rref("sched.tenant.resolve")).
  Field 1 (`tenant_id`) is reserved --- it was removed when resolution moved
  scheduler-side. The tenant's JWT is propagated via gRPC metadata
  (`x-rio-tenant-token`) for downstream authorization checks. Note:
  `tenant_id` still appears as a UUID-string field in `BuildInfo` and
  `TenantInfo` --- those are the *resolved* UUID, not the pre-resolution name.
]

#info(title: [BuildResultStatus ↔ Nix BuildStatus mapping])[
  The gRPC `BuildResultStatus` enum is a *subset* of Nix's wire `BuildStatus`
  with a different numbering scheme. The proto enum has `UNSPECIFIED=0` (proto3
  default), then `BUILT=1`, `SUBSTITUTED=2`, `ALREADY_VALID=3`,
  `PERMANENT_FAILURE=4`, `TRANSIENT_FAILURE=5`, `CACHED_FAILURE=6`,
  `DEPENDENCY_FAILED=7`, `LOG_LIMIT_EXCEEDED=8`, `OUTPUT_REJECTED=9`,
  `INFRASTRUCTURE_FAILURE=10`. This differs from Nix's wire values (where
  `TransientFailure=6`, `DependencyFailed=10`). The executor (`executor.rs`)
  and gateway translate explicitly; they do NOT map 1:1. The proto enum is
  currently missing `InputRejected`, `TimedOut`, `MiscFailure`,
  `NotDeterministic`, `ResolvesToAlreadyValid`, and `NoSubstituters` --- these
  Nix statuses currently round-trip through `PERMANENT_FAILURE` or
  `TRANSIENT_FAILURE` in the gRPC layer. `InfrastructureFailure` is gRPC-only
  (executor-internal errors: daemon crash, overlay failure); the gateway maps
  it to Nix `TransientFailure` (6).
]

=== WatchBuildRequest

Decouples observation from submission. The dashboard and reconnecting gateways
use this to subscribe to an existing build's event stream:

```protobuf
message WatchBuildRequest {
  string build_id = 1;
  // 2 was `uint64 since_sequence` — removed with the WatchBuild resumability layer
}
```

== Proto File Organization

#figure(
  table(
    columns: 2,
    align: (left, left),
    table.header([File], [Contents]),
    [`scheduler.proto`],
    [`SchedulerService` --- gateway-facing RPCs (SubmitBuild, WatchBuild,
      QueryBuildStatus, CancelBuild, ResolveTenant)],

    [`builder.proto`],
    [`ExecutorService` --- executor-facing RPCs (PullAssignment,
      ReportOutcome, and the materialization pull/list/progress unaries);
      covers builder + fetcher pods],

    [`store.proto`], [`StoreService`, `ChunkService`, `StoreAdminService`],

    [`admin.proto`], [`AdminService` --- dashboard and CLI RPCs],

    [`types.proto`],
    [Shared primitives: `PathInfo`, `ResourceUsage`, `BuildResultStatus`,
      `ExecutorKind`, store/chunk/GC/realisation RPC messages],

    [`dag.proto`],
    [DAG wire types: `DerivationNode` / `Edge` / `Event*`, `GraphNode` /
      `Edge`, `GetBuildGraph*`],

    [`build_types.proto`],
    [Build lifecycle: `BuildEvent*`, `SubmitBuildRequest`, `BuildResult`,
      `BuildStatus`, the pull-mode dispatch payloads
      (`PullAssignment*` / `ReportOutcome*`, `WorkAssignment`,
      `CompletionReport`), `BuildPhase`],

    [`admin_types.proto`],
    [Admin RPC data types: `ClusterStatusResponse`,
      `ListExecutors*` / `Builds*` / `Tenants*`,
      `SpawnIntent` / `GetSpawnIntents*`, `OpenAttempt` /
      `ReportAttemptOutcome*`, `ClearPoison*`],
  ),
)

#info(title: [File layout vs. Rust module])[
  The four data-type `.proto` files all declare `package rio.types;`, so prost
  merges them into a single generated `rio.types.rs`. Rust callers see
  everything at `rio_proto::types::*` regardless of which source file a message
  lives in. The file split is for proto-file review locality only; there is no
  corresponding Rust namespace split.
]

Executor-facing RPCs are in a separate `ExecutorService` (in `builder.proto`)
to allow distinct interceptors (auth, rate-limiting), independent evolution,
and potential future separation to a dedicated port. Both `SchedulerService`
and `ExecutorService` are served by the same scheduler binary.

== gRPC Configuration

*Max message size:* The default gRPC max message size (4MB) is insufficient for
rio-build. A full nixpkgs stdenv rebuild DAG contains \~60,000 nodes (\~12MB
serialized). All gRPC services must be configured with `max_message_size =
32MB` (configurable via `RIO_GRPC_MAX_MESSAGE_SIZE`).

*Why not streaming DAG submission?* Streaming the DAG in batches was considered
but rejected for Phase 1 simplicity. The single-message approach is adequate
for nixpkgs stdenv and the overwhelming majority of real-world DAGs. If future
workloads routinely exceed 32MB, a streaming `SubmitBuild` RPC can be added as
a non-breaking protocol extension (new RPC, old one remains).

*Per-service configuration:* The `max_message_size` applies to all gRPC
services:
- Gateway → Scheduler (`SubmitBuild` is the largest message)
- Gateway → Store (`GetPath` responses for large NARs use streaming, so
  unaffected)
- Executor → Scheduler (the pull/report unaries are individually small)
- Executor → Store (`PutPath` uses streaming, so unaffected)

== Client Helpers

`rio_proto::client` provides typed connection helpers so daemons don't
open-code `Endpoint` construction. The `ProtoClient` trait associates each
generated `XServiceClient<Channel>` with its `grpc.health.v1` service name and
TLS-domain override; `ProtoClient::wrap` applies `max_message_size` once so
per-binary connect blocks can't drift on where it's set.

#r("proto.client.balanced")[
  K8s daemons MUST use `rio_proto::client::connect<C>(addrs)` (dispatches
  single-channel vs. health-aware balanced from `UpstreamAddrs`). When
  `balance_host` is set, `BalancedChannel` DNS-resolves the headless Service,
  probes each pod IP via `grpc.health.v1/Check` with the *named* service (e.g.
  `rio.scheduler.SchedulerService` --- NOT empty string), and feeds
  `Change::Insert` for `SERVING` / `Change::Remove` for `NOT_SERVING` into
  tonic's p2c balancer.
]

The scheduler runs `replicas=2`; only the leader serves RPCs (the standby
returns `Unavailable` from leader-gated handlers). p2c only ejects on
connection-level failure, so without the out-of-band health probe it would keep
routing \~50% of calls to the standby. `BalancedChannel::new` blocks until the
first probe cycle finds ≥1 `SERVING` endpoint. The TLS domain override
(`ClientTlsConfig::domain_name`) decouples SAN verification from the connect
URI so pod-IP connections verify against the Service-name cert. The h2
keepalive (30s PING + 10s PONG timeout) is NOT optional: `Change::Remove` drops
the endpoint from selection but doesn't close existing TCP connections ---
without keepalive, a SIGKILLed peer (no FIN) leaves in-flight bidi streams
pinned for kernel-TCP-keepalive (\~2h). Named single-channel wrappers
(`connect_store`, `connect_scheduler`, `connect_executor`, `connect_admin`,
`connect_store_admin`) remain for tests, rio-cli, and ad-hoc callers.

*`current_traceparent()`* (in `rio_proto::interceptor`) returns the current
span's W3C traceparent as a string for embedding in non-gRPC payloads ---
`WorkAssignment.traceparent` is the load-bearing case (ssh-ng has no metadata
channel; see #rref("sched.trace.assignment-traceparent")). Pairs with
`span_from_traceparent()` on the receiving side.

= Rationale

== Protocol-level integration // supersedes ADR-001
<proto-rationale-integration>

rio-build implements both the `ssh-ng://` remote store protocol and the build
hook protocol (for `--builders`). Nix clients connect transparently without
custom tooling. The `ssh-ng://` path gives full DAG visibility: the client
pushes the entire derivation closure, enabling global scheduling and
deduplication. The @build-hook path provides per-derivation delegation, useful
for compatibility with existing `nix.conf` setups and CI runners that already
use `--builders`. Both paths terminate at rio-gateway, which translates wire
protocol operations into the gRPC services defined above.

A custom REST/gRPC API with a CLI wrapper was rejected: it would require users
to install a separate tool and change their workflow, lose the benefit of Nix's
built-in remote build infrastructure, and require reimplementing dependency
tracking that Nix already handles during the protocol exchange. The legacy
`ssh://` protocol (which sends individual build requests without full closure
information) was also rejected because it would limit the scheduler's ability to
reason about the full DAG.

The result is zero-friction adoption --- any Nix user can point their store URI
at rio-build --- at the cost of implementing and maintaining the Nix wire
protocol precisely, tracking upstream changes across Nix releases, and carrying
an SSH transport layer.

*The hard part: protocol fidelity.* The ssh-ng / daemon protocol is complex,
versioned, and not formally specified. Handling version negotiation, all
required opcodes, and edge cases relies on cross-referencing the
#link("https://snix.dev/docs/reference/nix-daemon-protocol/")[Snix protocol
  docs], the #link("https://www.tweag.io/blog/2024-04-25-nix-protocol-in-rust/")[Tweag re-implementation notes], and the Nix C++ source
(`worker-protocol.hh`, `daemon.cc`, `remote-store.cc`).

== Custom Nix protocol implementation // supersedes ADR-008
<proto-rationale-custom-impl>

The Nix wire protocol, derivation parsing, NAR format, @store-path computation,
and @nixbase32 are implemented from scratch in the `rio-nix` crate. This keeps
rio-build MIT/Apache-2.0 dual-licensed.

The most complete existing Rust implementation is `nix-compat` (Tvix/Snix
ecosystem), which is GPL-3.0 --- only its protobuf definitions are
MIT-licensed. Any binary statically linking `nix-compat` would need to be
GPL-3.0, which conflicts with the project's licensing goals; depending on it
would also couple rio-build to the Tvix release cadence and limit our ability
to optimize for rio-build's specific needs (zero-copy NAR streaming, batch
opcode handling). Process-boundary isolation to contain the GPL adds latency on
every protocol operation and rests on a debated legal interpretation. Linking
`libnixstore` / `libnixutil` via FFI (LGPL-3.0) brings a large C++ dependency,
complicates cross-compilation, and ties the project to Nix's unstable internal
C++ API.

Targeting only modern protocol versions (1.35+, see
@proto-rationale-protocol-version) significantly reduces implementation scope.
The cost is significant upfront effort, independent tracking of upstream Nix
protocol changes (no shared maintenance with Tvix/Snix), and the risk of subtle
protocol bugs that `nix-compat` has already found and fixed.

== Protocol version 1.35+ (Nix 2.18+ / Lix) // supersedes ADR-010
<proto-rationale-protocol-version>

The Nix daemon protocol has accumulated legacy operations and compatibility
shims across many versions. rio-build targets protocol version 1.35+,
corresponding to Nix 2.18+ and *Lix* (which is policy-frozen at 1.35 --- its
`worker-protocol.hh` carries the comment _"must remain 1.35 forever in Lix,
since the protocol has diverged in CppNix such that we cannot assign newer
versions ourselves"_). rio-build advertises 1.38 and negotiates down.

The original floor was 1.37; lowering to 1.35 admits Lix clients at the cost of
one version-gated field pair (`BuildResult.cpu_user` / `cpu_system`, added in
1.37). All other features rio depends on predate 1.35: trusted user status in
the handshake (1.35), `wopBuildPathsWithResults` (1.34, structured build
results), and `wopAddMultipleToStore` (1.32, batch path addition). Clients
running Nix < 2.18 (or any implementation < 1.35) receive a clear error
directing them to upgrade.

#figure(
  caption: [Protocol-version landmarks. The authoritative source is the
    `PROTOCOL_VERSION` constant in Nix's `worker-protocol.hh`.],
  table(
    columns: 3,
    align: (left, left, left),
    table.header([Protocol Version], [Nix Release], [Key Features]),
    [1.35],
    [Nix 2.15--2.19 / Lix],
    [*Minimum for rio-build:* trusted status in handshake; Lix's frozen
      version],

    [1.37],
    [Nix 2.20--2.23],
    [CPU timing in BuildResult (gated in rio). 1.36 was never assigned to a
      release.],

    [1.38],
    [Nix 2.24+],
    [Feature exchange in handshake (gated in rio); *rio advertises this*.
      Latest released as of Nix 2.28.],
  ),
)

#figure(
  caption: [NixOS distribution mapping.],
  table(
    columns: 4,
    align: (left, left, left, left),
    table.header(
      [NixOS Release], [Nix Version], [Protocol Version], [Compatible?]
    ),
    [NixOS 23.05], [Nix 2.13], [1.34], [No],
    [NixOS 23.11], [Nix 2.18], [1.35], [Yes],
    [NixOS 24.05], [Nix 2.18], [1.35], [Yes],
    [NixOS 24.11], [Nix 2.24], [1.38], [Yes],
    [NixOS 25.05], [Nix 2.28], [1.38], [Yes],
    [Lix (any)], [---], [1.35], [Yes],
  ),
)

Supporting all protocol versions (1.10+) was rejected: implementing deprecated
operations and version-specific code paths would inflate the testing matrix
across years of Nix releases for little gain --- 2.18 is the oldest Nix in any
supported NixOS release. Keeping the floor at 1.37 was rejected because it
excludes Lix entirely, and supporting Lix costs only one `if version >= 1.37`
gate. The trade: users on Nix < 2.18 (e.g., NixOS 23.05 ships Nix 2.15) cannot
use rio-build without upgrading; and the version-gated `cpu_user` /
`cpu_system` field is the first such gate in the codebase --- future protocol
additions above 1.35 will need the same treatment if they affect wire shape.

# Rio Implementation TODO

Detailed implementation plan for the brokerless Rio architecture.

**Implementation Strategy:** Build incrementally with clear milestones. Each phase is testable independently.

## Phase 1: Single-Agent MVP (Data Plane Only)

**Goal:** Prove the data plane works end-to-end without any Raft coordination. Single agent, single CLI, basic build execution.

### 1.1 Protocol Definitions (rio-common)

- [ ] Create `proto/rio/v1/agent.proto` with complete gRPC service definition
  - [ ] Define `RioAgent` service with 7 RPCs (see DESIGN.md section 9)
  - [ ] Define message types: `QueueBuildRequest`, `BuildUpdate`, `OutputChunk`, etc.
  - [ ] Define enums: `AgentStatus`, `BuildState`, `CompressionType`
  - [ ] Add proper field numbers and deprecation comments
- [ ] Set up `tonic-prost-build` in `rio-common/build.rs`
- [ ] Generate Rust code from protobuf definitions
- [ ] Create `rio-common/src/types.rs` with shared types:
  - [ ] `DerivationHash` newtype (SHA256 hash of derivation bytes)
  - [ ] `AgentId` newtype (UUID)
  - [ ] Helper functions: `hash_derivation(&[u8]) -> DerivationHash`

### 1.2 Nix Integration Utilities (rio-common)

- [ ] Create `rio-common/src/nix_utils.rs` module
- [ ] Implement `parse_nix_config()` - runs `nix config show`, parses output
  - [ ] Extract `system` field
  - [ ] Extract `extra-platforms` (space-separated)
  - [ ] Extract `system-features` (space-separated)
  - [ ] Return structured `NixConfig` type
- [ ] Implement `run_nix_eval_jobs()` - wrapper for nix-eval-jobs command
  - [ ] Takes Nix expression path
  - [ ] Runs with flags: `--check-cache-status --show-required-system-features --show-input-drvs`
  - [ ] Parses JSON output line-by-line
  - [ ] Returns `EvalResult` with `drvPath`, `system`, `cacheStatus`, `neededBuilds`, etc.
- [ ] Implement `read_derivation_bytes(path: &Path) -> Result<Vec<u8>>`
- [ ] Implement `compute_derivation_hash(bytes: &[u8]) -> DerivationHash`

### 1.3 CLI Basic Flow (rio-build)

**Simplified for Phase 1:** Direct connection to single agent, no cluster discovery.

### Nix Protocol Implementation 🚧
- [x] Study nix-daemon crate API
  - [x] Review Store trait requirements (16 methods)
  - [x] Understand Progress trait and return types
  - [ ] Study DaemonProtocolAdapter usage
  - [ ] Study protocol version compatibility
- [x] Implement Store trait for Dispatcher (DispatcherStore) ✅
  - [x] is_valid_path - checks local /nix/store ✅
  - [x] query_pathinfo - returns PathInfo from local store ✅
  - [x] query_valid_paths - batch path validation ✅
  - [x] add_to_store - imports NAR data to local store ✅
  - [x] build_paths - enqueues jobs for async dispatch ✅
  - [x] has_substitutes - returns false (stub, OK for MVP)
  - [x] query_substitutable_paths - returns empty (stub, OK for MVP)
  - [x] query_valid_derivers - returns empty (stub, OK for MVP)
  - [x] query_missing - returns empty (stub, OK for MVP)
  - [x] query_derivation_output_map - returns empty (stub, OK for MVP)
  - [x] build_paths_with_results - returns error (stub, OK - not commonly used)
  - [x] ensure_path - returns error (stub, OK for MVP)
  - [x] add_temp_root - no-op (stub, OK - no GC in MVP)
  - [x] add_indirect_root - no-op (stub, OK - no GC in MVP)
  - [x] find_roots - returns empty (stub, OK - no GC in MVP)
  - [x] set_options - no-op (stub, OK for MVP)
- [x] Parse derivation files (.drv) ✅
  - [x] Extract derivation metadata (DerivationInfo struct) ✅
  - [x] Identify build dependencies (input_derivations field) ✅
  - [x] Determine required platform (system field) ✅
  - [x] Parse using 'nix derivation show' JSON output ✅
  - [ ] Extract required features (future)
- [x] Integrate with SSH server ✅
  - [x] Tunnel Nix protocol over SSH using DaemonProtocolAdapter (spawns per session)
  - [x] Handle protocol negotiation (DaemonProtocolAdapter.adopt())
  - [x] Create bidirectional channels with AsyncRead/AsyncWrite bridge
  - [x] Forward SSH data() to protocol adapter via to_protocol_tx
  - [x] Forward protocol responses back to SSH via background task + from_protocol_rx

### Build Queue Implementation ✅
- [x] Design BuildQueue structure
  - [x] Job queue data structure (VecDeque with Mutex)
  - [x] Job metadata storage (HashMap with RwLock)
  - [ ] Job priority handling (future)
  - [ ] Job deduplication (future)
- [x] Implement job queueing
  - [x] Add job to queue from buildPaths
  - [x] FIFO ordering
  - [ ] Queue size limits (future)
- [x] Implement job status tracking
  - [x] Job states (Queued, Dispatched, Building, Completed, Failed)
  - [x] Status queries by job ID
  - [x] Update job status
  - [ ] Job completion callbacks (future)
- [x] Tests: 5 unit tests covering enqueue, dequeue, FIFO, status updates

### Scheduler Implementation ✅
- [x] Design scheduling algorithm
  - [x] Platform matching (required)
  - [x] Load balancing (select builder with fewest jobs)
  - [ ] Feature matching (future)
  - [ ] Round-robin tie-breaking (future)
- [x] Implement scheduler
  - [x] Select builder for job
  - [x] Handle no available builders
  - [x] Get builders by platform
  - [ ] Retry logic for failed builds (future)
  - [ ] Builder affinity (future optimization)
- [x] Integrate with BuildQueue ✅
  - [x] Poll queue for new jobs (DispatcherLoop)
  - [x] Dispatch jobs to selected builders
  - [x] Handle builder failures (re-queueing)
- [x] Tests: 5 unit tests covering selection, platform matching, load balancing

### Build Dispatching ✅ COMPLETE
- [x] Implement ExecuteBuild RPC (dispatcher side)
  - [x] Create gRPC client to builder
  - [x] Send build request to selected builder
  - [x] Stream build logs back to client
  - [x] Handle build completion messages
  - [x] Handle build failures and connection errors
  - [x] Parse derivation to extract actual platform ✅
- [x] Implement ExecuteBuild RPC (builder side) ✅
  - [x] Create gRPC server in rio-builder
  - [x] Implement BuildService with execute_build() handler
  - [x] Call executor.execute_build() and stream responses
  - [x] Handle errors and client disconnection
  - [x] Wire gRPC server startup in main.rs ✅
- [x] Background DispatcherLoop ✅
  - [x] Poll BuildQueue for pending jobs
  - [x] Dispatch jobs to selected builders
  - [x] Handle build failures and re-queueing
  - [x] Concurrent job execution
  - [x] Graceful shutdown
- [ ] Transfer build dependencies (future optimization)
  - [ ] Check which inputs builder already has
  - [ ] Transfer missing inputs
  - [ ] Verify integrity

## Phase 3: Build Execution ✅ COMPLETE

### Builder Execution Engine ✅
- [x] Implement Executor in rio-builder
  - [x] Create Executor struct with execute_build() method
  - [x] Real build execution (not just simulation) ✅
  - [x] Add tracing instrumentation with job_id field
  - [x] Parse derivation bytes and save to temp file ✅
  - [ ] Check local /nix/store for dependencies (future optimization)
- [x] Invoke nix-build ✅
  - [x] Add run_nix_build() helper method
  - [x] Capture stdout/stderr with tokio::process::Command
  - [x] Handle exit status and errors
  - [x] Stream logs in real-time via gRPC ✅
  - [ ] Handle build timeouts (future)
- [x] Handle build outputs ✅
  - [x] Locate output paths from nix-build stdout ✅
  - [x] Export as NAR format (nix-store --dump) ✅
  - [x] Stream outputs as chunks ✅
- [x] Error handling
  - [x] Build failures (exit status check implemented)
  - [x] Detailed error messages with stderr ✅
  - [ ] Dependency fetch failures (future)
  - [ ] Timeout errors (future)
  - [ ] Out of disk space (future)

### Output Transfer ✅ COMPLETE
- [x] Transfer build outputs to dispatcher ✅
  - [x] Export outputs as NAR using nix-store --dump ✅
  - [x] Stream NAR data in 64KB chunks via gRPC ✅
  - [x] Reassemble chunks on dispatcher ✅
  - [x] Import to dispatcher's /nix/store using nix-store --import ✅
- [x] Make outputs queryable ✅
  - [x] Implement query_pathinfo to check local store ✅
  - [x] Return proper PathInfo metadata ✅
  - [x] Client can verify outputs exist ✅
- [x] Clean up temporary data ✅
  - [x] Remove temp derivation files after build ✅
  - [ ] Maintain builder disk space (future GC integration)

### Testing ✅ COMPREHENSIVE
- [x] Unit tests for DispatcherStore (10 tests, full Store trait coverage)
  - [x] is_valid_path with existing and nonexistent paths ✅
  - [x] query_valid_paths with mixed paths ✅
  - [x] query_pathinfo with local store checks ✅
  - [x] add_to_store with real NAR import ✅
  - [x] add_to_store with invalid NAR (error handling) ✅
  - [x] query_missing, has_substitutes, set_options
  - [x] build_paths enqueuing
- [x] Unit tests for BuildQueue (5 tests)
- [x] Unit tests for Scheduler (5 tests)
- [x] Unit tests for AsyncProgress (4 tests)
- [x] Unit tests for DispatcherLoop (5 tests)
- [x] Unit tests for Executor (5 tests - simulation, file I/O, real builds)
- [x] Unit tests for DerivationInfo (9 tests - parsing, platforms, errors)
- [x] Unit tests for NAR export/import (7 tests)
- [x] Integration tests for gRPC (5 tests)
- [x] Integration tests for SSH (4 tests)
- [x] Component integration tests (6 tests) ✅
  - [x] Store + DispatcherLoop + Builder integration ✅
  - [x] Full build cycle with outputs ✅
  - [x] Build failures ✅
  - [x] Concurrent builds ✅
  - [x] No builders available handling ✅
- [ ] End-to-end test: Real SSH connection with nix CLI (next)
  - [ ] Manual testing with nix-build --store ssh://
  - [ ] Automated SSH protocol test
  - [ ] Verify complete flow works
- **70 tests passing, ~65% coverage**

## Phase 4: Production Features (Future)

### Binary Cache Integration
- [ ] Support fetching from binary caches
  - [ ] Check cache.nixos.org before building
  - [ ] Support custom caches
  - [ ] Verify signatures
- [ ] Cache build results
  - [ ] Upload successful builds to cache
  - [ ] Configure cache endpoints
  - [ ] Manage cache credentials

### Build Result Caching
- [ ] Track completed builds
  - [ ] Store derivation -> output mapping
  - [ ] Cache in memory with TTL
  - [ ] Persist to database (optional)
- [ ] Skip redundant builds
  - [ ] Check if derivation already built
  - [ ] Return cached outputs
  - [ ] Handle non-deterministic builds

### Persistence
- [ ] Set up PostgreSQL database
  - [ ] Schema design for jobs, builders, build history
  - [ ] Connection pooling
- [ ] Persist build history
  - [ ] Job records
  - [ ] Build logs
  - [ ] Performance metrics
- [ ] Persist builder registry
  - [ ] Builder information
  - [ ] Builder statistics
  - [ ] Builder availability history

### Monitoring & Metrics 🚧
- [x] Implement structured tracing (documented in METRICS.md)
  - [x] Add #[tracing::instrument] to all critical paths
  - [x] SSH operations: auth, channel lifecycle, data transfer
  - [x] BuildQueue: enqueue, dequeue, status updates
  - [x] Scheduler: builder selection
  - [x] gRPC handlers: builder registration
  - [x] Structured fields: job_id, platform, builder_id, channel_id, derivation_path
  - [x] Span hierarchies for request correlation
  - [ ] JSON logging for production (future)
- [ ] Implement Prometheus metrics (Phase 3+, planned in METRICS.md)
  - [ ] Counters: builds_total, ssh_connections_total, grpc_requests_total
  - [ ] Gauges: queue_size, builders_active, builds_in_progress
  - [ ] Histograms: build_duration_seconds, queue_wait_time_seconds
  - [ ] HTTP /metrics endpoint
- [ ] Health check endpoints
  - [ ] Dispatcher health
  - [ ] Builder health
  - [ ] Database connectivity (Phase 4+)

### Advanced Scheduling
- [ ] Priority queues
  - [ ] User-specified priorities
  - [ ] Fast-track small builds
- [ ] Fair scheduling
  - [ ] Per-user quotas
  - [ ] Prevent starvation
- [ ] Resource-aware scheduling
  - [ ] Match CPU/memory requirements
  - [ ] Consider disk space
- [ ] Build affinity
  - [ ] Prefer builders with cached dependencies
  - [ ] Sticky builders for related builds

### Multi-tenancy
- [ ] User isolation
  - [ ] SSH key-based user identification
  - [ ] Per-user build queues
  - [ ] Resource quotas
- [ ] Access control
  - [ ] Builder access restrictions
  - [ ] Build log access control

### Web UI
- [ ] Dashboard
  - [ ] Active builds
  - [ ] Builder status
  - [ ] Queue depth
- [ ] Build logs viewer
  - [ ] Real-time log streaming
  - [ ] Historical logs
  - [ ] Search and filtering
- [ ] Builder management
  - [ ] Register/unregister builders
  - [ ] View builder details
  - [ ] Builder statistics

## Phase 5: Scale & Performance (Future)

### Horizontal Scaling
- [ ] Multiple dispatcher instances
  - [ ] Load balancer in front
  - [ ] Shared state via distributed storage
  - [ ] Leader election for scheduler
- [ ] Distributed build queue
  - [ ] Redis or similar for shared queue
  - [ ] Job distribution across dispatchers
- [ ] Shared builder pool
  - [ ] Builders connect to any dispatcher
  - [ ] Consistent view of builder availability

### Object Storage Integration
- [ ] S3-compatible storage for outputs
  - [ ] Upload build outputs to object storage
  - [ ] Download on demand
  - [ ] Presigned URLs for direct access
- [ ] Reduce dispatcher storage requirements
- [ ] Improve output availability

### Auto-scaling
- [ ] Cloud integration (AWS/GCP/Azure)
  - [ ] Launch builders on demand
  - [ ] Terminate idle builders
  - [ ] Cost optimization
- [ ] Scaling policies
  - [ ] Scale up based on queue depth
  - [ ] Scale down based on utilization
  - [ ] Min/max builder counts

### Performance Optimizations
- [ ] Build result caching (content-addressed)
- [ ] Dependency prefetching
- [ ] Parallel build execution
- [ ] Connection pooling
- [ ] Zero-copy transfers where possible

## Additional Tasks

### Documentation
- [ ] User guide
  - [ ] How to configure Nix to use Rio
  - [ ] How to run a builder
  - [ ] How to run a dispatcher
- [ ] API documentation
  - [ ] gRPC API reference
  - [ ] SSH/Nix protocol documentation
- [ ] Deployment guide
  - [ ] Docker/Kubernetes deployment
  - [ ] Systemd service files
  - [ ] Configuration examples

### Testing
- [ ] Unit tests for all modules
- [ ] Integration tests for gRPC
- [ ] Integration tests for Nix protocol
- [ ] End-to-end tests with real Nix builds
- [ ] Performance tests
- [ ] Chaos testing (network failures, builder crashes)

### CI/CD
- [ ] GitHub Actions workflow
  - [ ] Run tests on push
  - [ ] Run clippy
  - [ ] Check formatting
  - [ ] Build for multiple platforms
- [ ] Release automation
  - [ ] Semantic versioning
  - [ ] Changelog generation
  - [ ] Build binaries for release

### Packaging
- [ ] Nix package definition
  - [ ] Flake output for rio-dispatcher
  - [ ] Flake output for rio-builder
  - [ ] NixOS module for dispatcher
  - [ ] NixOS module for builder
- [ ] Docker images
  - [ ] dispatcher image
  - [ ] builder image
  - [ ] Multi-platform builds
- [ ] Binary cache integration
- [ ] Build result caching
- [ ] Multi-platform support verification
- [ ] TLS/mTLS for agent communication
- [ ] Authentication for CLI clients
- [ ] Monitoring and metrics
- [ ] Web UI for cluster status
- [ ] Create `rio-build/src/main.rs` with CLI argument parsing (clap)
  - [ ] Positional argument: Nix expression path
  - [ ] Flag: `--agent <url>` (default: `http://localhost:50051`)
- [ ] Create `rio-build/src/evaluator.rs`
  - [ ] Function: `evaluate_build(expr_path)` - calls nix-eval-jobs
  - [ ] Returns: `BuildInfo { drv_path, drv_hash, platform, features, dependencies }`
- [ ] Create `rio-build/src/client.rs`
  - [ ] Struct: `RioClient { agent_url, grpc_client }`
  - [ ] Method: `connect(url) -> Result<RioClient>`
  - [ ] Method: `submit_build(build_info) -> Result<impl Stream<BuildUpdate>>`
    - [ ] Read derivation bytes from `drv_path`
    - [ ] Call `QueueBuild` RPC
    - [ ] Handle response (Phase 1: only `BuildAssigned` variant)
    - [ ] Call `SubscribeToBuild` RPC
    - [ ] Return build update stream
- [ ] Create `rio-build/src/output_handler.rs`
  - [ ] Function: `handle_build_stream(stream)` - consumes BuildUpdate stream
  - [ ] For `LogLine`: print to stdout
  - [ ] For `OutputChunk`: collect chunks, decompress, reassemble NAR
  - [ ] For `BuildCompleted`: run `nix-store --import`, print success
  - [ ] For `BuildFailed`: print error, exit with code 1
- [ ] Wire up main flow:
  ```rust
  let build_info = evaluator::evaluate_build(expr_path)?;
  let client = RioClient::connect(agent_url)?;
  let stream = client.submit_build(build_info)?;
  output_handler::handle_build_stream(stream).await?;
  ```

### 1.4 Agent Basic Structure (rio-agent)

**Phase 1:** Single-threaded, no Raft, no concurrency. One build at a time.

- [ ] Create `rio-agent/src/main.rs` with argument parsing
  - [ ] Flag: `--listen <addr:port>` (default: `0.0.0.0:50051`)
  - [ ] Flag: `--data-dir <path>` (default: `/var/lib/rio`)
- [ ] Create `rio-agent/src/agent.rs`
  - [ ] Struct: `Agent` with fields:
    - [ ] `id: AgentId` (generate UUID on startup)
    - [ ] `platforms: Vec<String>` (from nix config)
    - [ ] `features: Vec<String>` (from nix config)
    - [ ] `current_build: Option<BuildJob>` (None = available)
    - [ ] `pending_dir: PathBuf` (`/tmp/rio-agent`)
  - [ ] Method: `new(data_dir) -> Agent` - queries nix config, creates pending_dir
  - [ ] Struct: `BuildJob` with fields:
    - [ ] `drv_hash: DerivationHash`
    - [ ] `drv_path: PathBuf`
    - [ ] `process: Child` (nix-build process)
    - [ ] `log_tx: mpsc::Sender<String>` (for log streaming)
    - [ ] `subscribers: Vec<mpsc::Sender<BuildUpdate>>`
- [ ] Create `rio-agent/src/grpc_server.rs`
  - [ ] Implement `RioAgent` gRPC service trait
  - [ ] Phase 1 minimal implementations:
    - [ ] `queue_build()` - stores drv to temp file, starts build, returns BuildAssigned
    - [ ] `subscribe_to_build()` - adds subscriber to current build, streams updates
    - [ ] Other RPCs: return `Unimplemented` status

### 1.5 Agent Build Execution (rio-agent)

- [ ] Create `rio-agent/src/builder.rs`
  - [ ] Function: `start_build(agent, drv_hash, drv_bytes) -> Result<BuildJob>`
    - [ ] Write drv_bytes to `/tmp/rio-agent/{drv_hash}.drv`
    - [ ] Spawn `nix-build /tmp/rio-agent/{drv_hash}.drv`
    - [ ] Capture stdout/stderr as async stream
    - [ ] Return BuildJob with process handle and log channel
  - [ ] Function: `stream_logs(build_job) -> impl Stream<BuildUpdate>`
    - [ ] Read lines from process stdout/stderr
    - [ ] Wrap in `BuildUpdate::LogLine` messages
    - [ ] Stream until process exits
  - [ ] Function: `wait_for_completion(build_job) -> Result<Vec<PathBuf>>`
    - [ ] Wait for process to exit
    - [ ] Parse output for result paths (parse nix-build output)
    - [ ] If exit code != 0, return BuildFailed
    - [ ] Return output paths on success

### 1.6 Agent Output Streaming (rio-agent)

Implement the NAR streaming pattern from DESIGN.md section "NAR Streaming Implementation Pattern".

- [ ] Create `rio-agent/src/nar_exporter.rs`
  - [ ] Function: `stream_outputs(output_paths, tx: mpsc::Sender<BuildUpdate>)`
    - [ ] Create unbounded channel for NAR chunks
    - [ ] Spawn blocking task:
      - [ ] Run `nix-store --export {paths...}` with pipe
      - [ ] Read chunks (1MB buffer)
      - [ ] Compress each chunk with `zstd::encode_all(chunk, 3)`
      - [ ] Send compressed chunks to channel
    - [ ] Async task:
      - [ ] Receive chunks from channel
      - [ ] Wrap in `BuildUpdate::OutputChunk` with sequence numbers
      - [ ] Send to subscriber stream
      - [ ] Send final chunk with `last_chunk: true`
    - [ ] Use `tokio::try_join` to run both tasks concurrently

### 1.7 CLI Output Import (rio-build)

- [ ] Enhance `rio-build/src/output_handler.rs`
  - [ ] Struct: `NarAssembler` - collects and orders chunks
    - [ ] Field: `chunks: BTreeMap<u32, Vec<u8>>` (chunk_index -> compressed data)
    - [ ] Method: `add_chunk(chunk: OutputChunk)`
    - [ ] Method: `is_complete() -> bool` (received last_chunk)
    - [ ] Method: `assemble() -> Result<Vec<u8>>` (decompress and concatenate in order)
  - [ ] Function: `import_nar(nar_bytes: &[u8]) -> Result<()>`
    - [ ] Spawn `nix-store --import`
    - [ ] Write nar_bytes to stdin
    - [ ] Wait for process completion
    - [ ] Return error if exit code != 0

### 1.8 Phase 1 Testing

- [ ] Create test Nix expressions in `tests/fixtures/`
  - [ ] `hello.nix` - simple package
  - [ ] `multi-output.nix` - package with out, dev, doc outputs
- [ ] Integration test: End-to-end build
  - [ ] Start agent process
  - [ ] Run CLI with test expression
  - [ ] Verify outputs imported to /nix/store
  - [ ] Verify build logs captured
- [ ] Test error scenarios:
  - [ ] Build failure (expression with error)
  - [ ] Invalid derivation
  - [ ] Agent unavailable

---

## Phase 2: Raft Cluster (Control Plane Only)

**Goal:** Prove Raft coordination works. Agents form cluster, elect leader, track membership. No builds yet - just cluster mechanics.

### 2.1 Raft Storage Setup (rio-agent)

- [ ] Add `rocksdb` dependency to `rio-agent/Cargo.toml`
- [ ] Create `rio-agent/src/storage.rs`
  - [ ] Struct: `RaftStorage` - wrapper around RocksDB
  - [ ] Column families: `raft_log`, `raft_state`
  - [ ] Implement `openraft::RaftLogStorage` trait
  - [ ] Implement `openraft::RaftStateMachineStorage` trait
  - [ ] Methods for log compaction and snapshotting

### 2.2 Raft State Machine (rio-agent)

- [ ] Create `rio-agent/src/state_machine.rs`
  - [ ] Define `ClusterState` struct (matches DESIGN.md section 1):
    ```rust
    struct ClusterState {
        agents: HashMap<AgentId, AgentInfo>,
        builds_in_progress: HashMap<DerivationHash, BuildTracker>,
        completed_builds: LruCache<DerivationHash, CompletedBuild>,
    }
    ```
  - [ ] Define `RaftCommand` enum (matches DESIGN.md section 1):
    ```rust
    enum RaftCommand {
        AgentJoined { id: AgentId, info: AgentInfo },
        AgentLeft { id: AgentId },
        AgentHeartbeat { id: AgentId, timestamp: Timestamp },
        BuildQueued { top_level, dependencies, platform, features },
        BuildStatusChanged { drv_hash, status },
        BuildCompleted { drv_hash, output_paths },
        BuildFailed { drv_hash, error },
    }
    ```
  - [ ] Implement `openraft::RaftStateMachine` trait
  - [ ] Method: `apply(command: RaftCommand) -> Result<Response>`
    - [ ] Pattern match on command
    - [ ] Update ClusterState accordingly
    - [ ] Return response data (e.g., selected agent for BuildQueued)

### 2.3 Deterministic Agent Assignment (rio-agent)

Implement the algorithm from DESIGN.md section 1 "Deterministic Agent Assignment".

- [ ] In `state_machine.rs`, implement `apply_build_queued()`
  - [ ] Step 1: Filter eligible agents (platform + features + Available status)
  - [ ] Step 2: Score agents by affinity
    - [ ] Count matching deps in `builds_in_progress`
    - [ ] Count matching deps in `completed_builds`
  - [ ] Step 3: Select highest affinity agent
  - [ ] Step 4: Tie-break by smallest agent_id (lexicographic)
  - [ ] Step 5: Update state:
    - [ ] Insert top-level build in `builds_in_progress`
    - [ ] Insert all dependencies with parent_build pointer
    - [ ] Set agent status to Busy
  - [ ] Return selected agent_id

### 2.4 Cluster Membership (rio-agent)

- [ ] Enhance `rio-agent/src/agent.rs`
  - [ ] Add field: `raft: Arc<Raft<...>>`
  - [ ] Method: `bootstrap() -> Agent` - creates single-node cluster
  - [ ] Method: `join(seed_url) -> Agent` - joins existing cluster via JoinCluster RPC
- [ ] Create `rio-agent/src/membership.rs`
  - [ ] Function: `handle_agent_joined(raft, agent_info)`
    - [ ] Propose RaftCommand::AgentJoined to cluster
    - [ ] Wait for commit
  - [ ] Function: `handle_agent_left(raft, agent_id)`
    - [ ] Propose RaftCommand::AgentLeft
    - [ ] Clean up builds assigned to that agent
- [ ] Implement gRPC RPCs for membership:
  - [ ] `JoinCluster` - leader receives request, proposes AgentJoined
  - [ ] `GetClusterMembers` - returns list of agents and current leader

### 2.5 Heartbeat System (rio-agent)

- [ ] Create `rio-agent/src/heartbeat.rs`
  - [ ] Function: `start_heartbeat_task(agent_id, raft)`
    - [ ] Loop every 10 seconds
    - [ ] Propose RaftCommand::AgentHeartbeat
  - [ ] Function: `check_failed_agents(cluster_state) -> Vec<AgentId>`
    - [ ] Iterate agents
    - [ ] Find agents where `last_heartbeat` > 30 seconds old
    - [ ] Return list of failed agent IDs
  - [ ] In state machine: Update `last_heartbeat` on AgentHeartbeat command
  - [ ] In state machine: On agent marked Down, remove all its builds

### 2.6 CLI Cluster Discovery (rio-build)

- [ ] Enhance `rio-build/src/client.rs`
  - [ ] Method: `discover_cluster(seed_urls) -> Result<ClusterInfo>`
    - [ ] Try connecting to each seed URL
    - [ ] Call `GetClusterMembers()` RPC
    - [ ] Parse response, identify leader
    - [ ] Cache cluster info for 60 seconds (timestamp + data)
  - [ ] Method: `connect_to_leader(cluster_info) -> Result<RioClient>`
    - [ ] Connect to leader agent's gRPC endpoint
    - [ ] Return client
- [ ] Add CLI config file support: `~/.config/rio/config.toml`
  - [ ] Field: `seed_agents = ["http://agent1:50051", "http://agent2:50051"]`
  - [ ] Load in main(), pass to client

### 2.7 Phase 2 Testing

- [ ] Create test: Bootstrap single-node cluster
  - [ ] Start agent with `--bootstrap`
  - [ ] Verify it becomes leader
  - [ ] Call GetClusterMembers, verify single member
- [ ] Create test: Three-node cluster formation
  - [ ] Bootstrap agent-1
  - [ ] Start agent-2 with `--join=agent-1`
  - [ ] Start agent-3 with `--join=agent-1`
  - [ ] Verify all three in cluster
  - [ ] Verify leader election
- [ ] Create test: Heartbeats and failure detection
  - [ ] Form 3-node cluster
  - [ ] Kill one agent (SIGKILL)
  - [ ] Verify others detect failure within 30s
  - [ ] Verify failed agent marked as Down
- [ ] Create test: Leader election
  - [ ] Form 3-node cluster
  - [ ] Kill leader
  - [ ] Verify new leader elected
  - [ ] Verify CLI can still discover new leader

---

## Phase 3: Distributed Build Coordination

**Goal:** Integrate builds with Raft. CLI submits to leader, Raft assigns to agent, agent builds and reports status.

### 3.1 Build Submission via Leader (rio-agent)

- [ ] Enhance `grpc_server.rs`, implement `QueueBuild` RPC:
  - [ ] Leader receives QueueBuildRequest
  - [ ] Compute drv_hash from derivation bytes
  - [ ] Check if build already in progress or completed:
    - [ ] Query Raft state: `cluster_state.builds_in_progress.get(drv_hash)`
    - [ ] If found: Return `AlreadyBuilding` or `AlreadyCompleted` response
  - [ ] Store derivation temporarily: `/tmp/rio-pending/{drv_hash}.drv`
  - [ ] Propose RaftCommand::BuildQueued to cluster
  - [ ] Wait for commit
  - [ ] Read assignment from state machine result
  - [ ] Return `BuildAssigned { agent_id, drv_hash }`

### 3.2 Agent Receives Assignment (rio-agent)

- [ ] Create `rio-agent/src/build_coordinator.rs`
  - [ ] Function: `on_raft_committed(agent, raft_cmd)`
    - [ ] Pattern match on RaftCommand
    - [ ] If `BuildQueued` and `selected_agent == self.id`:
      - [ ] Fetch derivation from leader via `FetchPendingBuild` RPC (if not leader)
      - [ ] Check dependencies: any building on self?
        - [ ] If yes: Queue build with `Queued { blocked_on }` status
        - [ ] If no: Start build immediately with `Building` status
      - [ ] Propose BuildStatusChanged to Raft

### 3.3 Multi-User Subscriptions (rio-agent)

- [ ] Enhance `BuildJob` struct in `agent.rs`:
  - [ ] Change: `subscribers: Vec<mpsc::Sender<BuildUpdate>>`
  - [ ] Add: `log_history: VecDeque<LogLine>` (cap at 10,000 lines)
- [ ] Enhance `SubscribeToBuild` RPC:
  - [ ] Find existing `BuildJob` by drv_hash
  - [ ] If not found: Return error "Build not found"
  - [ ] Create new subscriber channel
  - [ ] Send catch-up logs (from log_history)
  - [ ] Add subscriber to build_jobs list
  - [ ] Stream live updates as they arrive
- [ ] Enhance log streaming:
  - [ ] When log line arrives, append to log_history
  - [ ] Broadcast to all active subscribers

### 3.4 Build Completion Flow (rio-agent)

- [ ] In `builder.rs`, enhance `wait_for_completion()`:
  - [ ] After build succeeds, call `stream_outputs()` (from Phase 1)
  - [ ] After outputs streamed, propose RaftCommand::BuildCompleted
  - [ ] Send BuildUpdate::Completed to all subscribers
  - [ ] Clean up temp files: `/tmp/rio-agent/{drv_hash}.drv`
  - [ ] Check if any pending builds waiting on this one
  - [ ] If yes: Start next build from pending queue
  - [ ] If no: Mark agent as Available
- [ ] In `builder.rs`, handle build failure:
  - [ ] If build fails, propose RaftCommand::BuildFailed
  - [ ] Send BuildUpdate::Failed to all subscribers
  - [ ] Find dependent builds in pending queue
  - [ ] Cascade failure to dependents (see section 3.6)

### 3.5 Completed Build Cache (rio-agent)

- [ ] Enhance `ClusterState` in `state_machine.rs`:
  - [ ] Initialize `completed_builds: LruCache::new(100)` (cap at 100 entries)
  - [ ] On BuildCompleted command:
    - [ ] Remove from `builds_in_progress`
    - [ ] Insert in `completed_builds` with 5-minute TTL
- [ ] Implement `GetCompletedBuild` RPC:
  - [ ] Query Raft state for drv_hash
  - [ ] If in completed_builds:
    - [ ] Export outputs from /nix/store
    - [ ] Stream to CLI
  - [ ] If not found: Return NOT_FOUND status

### 3.6 Build Deduplication (rio-agent + rio-build)

- [ ] CLI enhancement in `client.rs`:
  - [ ] On `AlreadyBuilding` response:
    - [ ] Extract agent_id and drv_hash
    - [ ] Connect to assigned agent
    - [ ] Call `SubscribeToBuild(drv_hash)`
    - [ ] Handle stream normally (late joiner gets catch-up logs)
  - [ ] On `AlreadyCompleted` response:
    - [ ] Connect to agent with outputs
    - [ ] Call `GetCompletedBuild(drv_hash)`
    - [ ] Receive and import outputs

### 3.7 Phase 3 Testing

- [ ] Test: Single build, single user
  - [ ] Form 3-agent cluster
  - [ ] CLI submits build
  - [ ] Verify leader assigns to agent
  - [ ] Verify agent builds and streams logs
  - [ ] Verify outputs imported to CLI
- [ ] Test: Build deduplication (concurrent users)
  - [ ] User A submits build
  - [ ] 5 seconds later, User B submits same build
  - [ ] Verify only one build executes
  - [ ] Verify both users get logs and outputs
- [ ] Test: Build deduplication (recently completed)
  - [ ] User A builds, completes
  - [ ] User B submits same build 2 minutes later
  - [ ] Verify no rebuild, outputs served from cache
- [ ] Test: Leader failover during build
  - [ ] Start build
  - [ ] Kill leader mid-build
  - [ ] Verify new leader elected
  - [ ] Verify build continues on assigned agent
  - [ ] Verify CLI can still connect and get updates

---

## Phase 4: Advanced Features

**Goal:** Dependency tracking, build affinity, cascading failures, platform matching.

### 4.1 Dependency Tracking (rio-agent)

- [ ] Enhance `BuildTracker` in state_machine.rs:
  - [ ] Add field: `parent_build: Option<DerivationHash>`
  - [ ] When registering dependencies, set parent_build = Some(top_level_hash)
- [ ] In state machine, enhance `apply_build_queued()`:
  - [ ] Register all dependencies atomically in same command
  - [ ] Each dependency gets `parent_build` pointer to top-level
- [ ] In state machine, enhance `apply_build_completed()`:
  - [ ] Remove top-level from builds_in_progress
  - [ ] Remove ALL dependencies where parent_build = completed hash
  - [ ] Insert in completed_builds cache

### 4.2 Build Affinity (rio-agent)

Already implemented in Phase 3 (deterministic assignment scores by affinity). Add tests:

- [ ] Test: User A builds foo, User B builds bar (depends on foo)
  - [ ] Verify both assigned to same agent
  - [ ] Verify bar queued until foo completes
  - [ ] Verify bar auto-starts when foo done

### 4.3 Dependency Waiting (rio-agent)

- [ ] Enhance agent to track pending builds:
  - [ ] Field: `pending_builds: HashMap<DerivationHash, PendingBuild>`
  - [ ] Struct PendingBuild: `{ drv_bytes, blocked_on: Vec<DerivationHash>, subscribers }`
- [ ] When assigned build has dependencies building on self:
  - [ ] Don't start immediately
  - [ ] Add to pending_builds
  - [ ] Send status to subscribers: "Waiting for dependencies..."
  - [ ] Propose BuildStatusChanged with Queued status
- [ ] On build completion:
  - [ ] Iterate pending_builds
  - [ ] Remove completed drv_hash from all blocked_on lists
  - [ ] If any pending build now has empty blocked_on:
    - [ ] Start that build
    - [ ] Send status: "Dependencies ready, building..."

### 4.4 Cascading Failures (rio-agent)

- [ ] In `builder.rs`, function `on_build_failed()`:
  - [ ] Propose BuildFailed to Raft
  - [ ] Find all pending builds waiting on failed build
  - [ ] For each dependent:
    - [ ] Send BuildUpdate::Failed with message "Dependency failed"
    - [ ] Propose BuildFailed for dependent
    - [ ] Remove from pending_builds
  - [ ] Recursively fail transitive dependents

### 4.5 Platform and Feature Matching (rio-agent)

- [ ] In state machine, enhance `apply_build_queued()` filter:
  - [ ] Only consider agents where `agent.platforms.contains(cmd.platform)`
  - [ ] Only consider agents where `cmd.features.iter().all(|f| agent.features.contains(f))`
  - [ ] Only consider agents with status Available
- [ ] If no eligible agents:
  - [ ] State machine returns error response
  - [ ] Leader returns `NoEligibleAgents { reason }` to CLI
- [ ] CLI displays helpful error with agent capabilities

### 4.6 Agent-to-Agent Communication (rio-agent)

- [ ] Implement `FetchPendingBuild` RPC:
  - [ ] Non-leader agent requests derivation from leader
  - [ ] Leader reads from `/tmp/rio-pending/{drv_hash}.drv`
  - [ ] Returns drv_bytes and dependency list
  - [ ] Non-leader writes to `/tmp/rio-agent/{drv_hash}.drv`

### 4.7 Graceful Shutdown (rio-agent)

- [ ] Add signal handler (SIGTERM, SIGINT):
  - [ ] Set `shutting_down` flag
  - [ ] Stop accepting new build assignments
  - [ ] Wait for current build to complete (with timeout, e.g. 5 minutes)
  - [ ] If timeout expires: Send BuildFailed for current build
  - [ ] Propose RaftCommand::AgentLeft
  - [ ] Wait for Raft commit
  - [ ] Shut down gRPC server
  - [ ] Close Raft storage
  - [ ] Exit

### 4.8 Phase 4 Testing

- [ ] Test: Multi-derivation build with dependencies
  - [ ] Build expression with 5 derivations (linear dependency chain)
  - [ ] Verify all assigned to same agent (affinity)
  - [ ] Verify built in correct order
- [ ] Test: Cascading failure
  - [ ] Build A depends on B depends on C
  - [ ] C fails
  - [ ] Verify B and A marked as failed
  - [ ] Verify CLI gets error for all three
- [ ] Test: Platform mismatch
  - [ ] Cluster with only x86_64-linux agents
  - [ ] Submit build requiring aarch64-darwin
  - [ ] Verify NoEligibleAgents response
- [ ] Test: Feature requirement
  - [ ] Submit build requiring "kvm" feature
  - [ ] Verify only agents with kvm selected
- [ ] Test: Graceful shutdown
  - [ ] Start build
  - [ ] Send SIGTERM to agent
  - [ ] Verify build completes before shutdown
  - [ ] Verify agent removed from cluster

---

## Phase 5: Production Hardening

**Goal:** Error handling, retry logic, performance optimization, observability, security.

### 5.1 Comprehensive Error Handling

- [ ] CLI retry logic:
  - [ ] On connection failure: Retry up to 3 times with exponential backoff
  - [ ] On leader changed: Refresh cluster state, reconnect
  - [ ] On stream disconnect: Check build status, resubscribe if still building
  - [ ] On timeout: Cancel build, report error
- [ ] Agent error recovery:
  - [ ] On nix-build process crash: Mark build as failed
  - [ ] On disk full: Reject new builds, log warning
  - [ ] On Raft proposal timeout: Return "Cluster unavailable" to CLI
  - [ ] On RocksDB error: Log and potentially shut down

### 5.2 Observability

- [ ] Add tracing instrumentation:
  - [ ] Use `#[tracing::instrument]` on all public async functions
  - [ ] Add spans for: build lifecycle, Raft operations, gRPC calls
  - [ ] Log important state transitions (build started, completed, failed)
- [ ] Prometheus metrics:
  - [ ] Agent: builds_total, builds_active, builds_failed, raft_proposals_total
  - [ ] CLI: builds_submitted, build_duration_seconds (histogram)
- [ ] Expose HTTP endpoint for metrics: `GET /metrics`
- [ ] Add structured logging (JSON format for production)

### 5.3 Performance Optimization

- [ ] Benchmark NAR streaming:
  - [ ] Measure throughput for 1GB, 10GB outputs
  - [ ] Verify constant memory usage
  - [ ] Test different chunk sizes (512KB, 1MB, 2MB)
- [ ] Optimize Raft proposal batching:
  - [ ] Batch multiple heartbeats if leader is busy
  - [ ] Measure latency for BuildQueued → BuildAssigned
- [ ] Test with large build closures:
  - [ ] 100+ derivations in single build
  - [ ] Measure Raft command size
  - [ ] Verify deterministic assignment remains fast (<100ms)

### 5.4 Failure Scenario Testing

- [ ] Test: Agent dies mid-build
  - [ ] Verify CLI detects disconnect
  - [ ] Verify CLI retries on different agent
  - [ ] Verify builds re-group via affinity
- [ ] Test: CLI dies mid-build
  - [ ] Verify agent completes build anyway
  - [ ] Verify outputs cached
  - [ ] Verify CLI can reconnect and fetch completed build
- [ ] Test: Network partition
  - [ ] Partition cluster 2+1 (majority + minority)
  - [ ] Verify majority side continues
  - [ ] Verify minority side rejects builds (no quorum)
  - [ ] Heal partition, verify cluster converges
- [ ] Test: All agents down
  - [ ] CLI should fail gracefully with clear error
  - [ ] Verify CLI doesn't infinite-loop retry
- [ ] Test: Leader election during build submission
  - [ ] CLI sends QueueBuild to leader
  - [ ] Leader receives but crashes before proposing to Raft
  - [ ] CLI should timeout and retry with new leader

### 5.5 Security (Production)

- [ ] TLS for agent-to-agent communication:
  - [ ] Generate certificates (self-signed for testing, proper CA for prod)
  - [ ] Configure tonic to use TLS for Raft RPC
  - [ ] Add `--tls-cert` and `--tls-key` flags to agent
- [ ] mTLS for CLI-to-agent communication:
  - [ ] Require client certificates
  - [ ] Verify client cert on agent side
  - [ ] Add `--client-cert` flag to CLI
- [ ] Authentication tokens (optional):
  - [ ] JWT tokens for CLI authentication
  - [ ] Agent validates token on each RPC
  - [ ] Token contains user identity for audit logs

### 5.6 Additional RPCs and Features

- [ ] HTTP debugging endpoints (agent):
  - [ ] `GET /status` - agent health
  - [ ] `GET /status/builds` - current builds
  - [ ] `GET /status/cluster` - cluster members
- [ ] Build cancellation:
  - [ ] CLI: Ctrl-C handler
  - [ ] Send CancelBuild RPC
  - [ ] Agent: Kill nix-build process
  - [ ] Propose BuildFailed to Raft
- [ ] Build timeout:
  - [ ] Add `max_build_time` to QueueBuildRequest
  - [ ] Agent: Watchdog timer
  - [ ] If exceeded: Kill process, mark failed

### 5.7 Documentation

- [ ] User documentation:
  - [ ] Quick start guide
  - [ ] Installation instructions
  - [ ] Configuration reference
  - [ ] Troubleshooting guide
- [ ] Operator documentation:
  - [ ] Deployment guide (systemd units, docker, k8s)
  - [ ] Cluster sizing recommendations
  - [ ] Backup and recovery procedures
  - [ ] Monitoring and alerting setup
- [ ] Developer documentation:
  - [ ] Architecture overview (point to DESIGN.md)
  - [ ] Contributing guide
  - [ ] Release process

---

## Future Enhancements (Post-MVP)

- [ ] Web UI for cluster visualization
- [ ] Build history database (PostgreSQL)
- [ ] Binary cache integration (push to cache on completion)
- [ ] Multi-build concurrency per agent (build slots)
- [ ] Distributed build cache (agent-to-agent NAR streaming)
- [ ] Build priority and queueing
- [ ] Resource-based scheduling (CPU, memory, disk)
- [ ] Heterogeneous builds (cross-compilation support)
- [ ] Build sandboxing (nix-daemon containers)

---
## Current Focus

**Phase 1:** Single-Agent MVP - Prove data plane works without Raft complexity.

**Next Milestones:**
1. Complete protocol definitions (1.1)
2. Implement basic CLI flow (1.3)
3. Implement basic agent build execution (1.4-1.5)
4. End-to-end test (1.8)

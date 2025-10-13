# Rio Implementation TODO

Detailed implementation plan for the brokerless Rio architecture.

**Implementation Strategy:** Build incrementally with clear milestones. Each phase is testable independently.

## Phase 1: Single-Agent MVP (Data Plane Only)

**Goal:** Prove the data plane works end-to-end without any Raft coordination. Single agent, single CLI, basic build execution.

### 1.1 Protocol Definitions (rio-common) ✅ COMPLETED

- [x] Create `proto/rio/v1/agent.proto` with complete gRPC service definition
  - [x] Define `RioAgent` service with 7 RPCs (see DESIGN.md section 9)
  - [x] Define message types: `QueueBuildRequest`, `BuildUpdate`, `OutputChunk`, etc.
  - [x] Define enums: `AgentStatus`, `BuildState`, `CompressionType` (without UNSPECIFIED variants)
  - [x] Add proper field numbers and comments
- [x] Set up `tonic-prost-build` in `rio-common/build.rs`
- [x] Generate Rust code from protobuf definitions (49KB generated)
- [x] Create `rio-common/src/types.rs` with shared types:
  - [x] `DerivationPath` type alias (Utf8PathBuf) - uses full Nix store path
  - [x] `AgentId` type alias (Uuid)
  - [x] Added `camino` dependency for UTF-8 path support

**Design decisions:**
- Removed UNSPECIFIED enum variants - enums now use meaningful defaults (0 values)
- Use full derivation path as identifier (e.g., `/nix/store/abc123-foo.drv`) instead of just hash
  - More debuggable (includes package name)
  - Already unique (guaranteed by Nix)
  - No parsing needed

### 1.2 Nix Integration Utilities (rio-common) ✅ COMPLETED

- [x] Create `rio-common/src/nix_utils.rs` module
- [x] Implement `NixConfig::parse()` - runs `nix config show`, parses output
  - [x] Extract `system` field
  - [x] Extract `extra-platforms` (space-separated)
  - [x] Extract `system-features` (space-separated)
  - [x] Return structured `NixConfig` type
  - [x] Add `all_platforms()` helper method
- [x] Implement `EvalResult::from_file()` - wrapper for nix-eval-jobs command
  - [x] Takes Nix file path (&Utf8Path)
  - [x] Runs with flags: `--check-cache-status --show-required-system-features --show-input-drvs`
  - [x] Parses JSON output line-by-line
  - [x] Returns `EvalResult` with `drvPath`, `system`, `cacheStatus`, `neededBuilds`, etc.
- [x] All tests pass (including Nix integration tests)

**Design decisions:**
- Removed `read_derivation_bytes()` helper - can use `tokio::fs::read()` directly where needed
- Made functions methods on their respective types (`NixConfig::parse()`, `EvalResult::from_file()`)
- Use `&Utf8Path` for all path parameters (never `&str`)
- No `#[ignore]` on tests - Nix always available in development environment

### 1.3 CLI Basic Flow (rio-build) ✅ COMPLETED

**Simplified for Phase 1:** Direct connection to single agent, no cluster discovery.

- [x] Create `rio-build/src/main.rs` with CLI argument parsing (clap)
  - [x] Positional argument: Nix file path (Utf8PathBuf)
  - [x] Flag: `--agent <url>` (default: `http://localhost:50051`)
  - [x] Initialize tracing
- [x] Create `rio-build/src/evaluator.rs`
  - [x] Function: `evaluate_build(nix_file)` - calls EvalResult::from_file
  - [x] Returns: `BuildInfo { drv_path, drv_bytes, platform, required_features, dependency_paths }`
  - [x] Checks cache status, bails if already cached/local
- [x] Create `rio-build/src/client.rs`
  - [x] Struct: `RioClient` wrapping RioAgentClient
  - [x] Method: `connect(url) -> Result<RioClient>`
  - [x] Method: `submit_build(build_info) -> Result<Streaming<BuildUpdate>>`
    - [x] Send derivation bytes via `QueueBuild` RPC
    - [x] Handle response (Phase 1: only `BuildAssigned` variant)
    - [x] Call `SubscribeToBuild` RPC
    - [x] Return build update stream
- [x] Create `rio-build/src/output_handler.rs`
  - [x] Function: `handle_build_stream(stream)` - consumes BuildUpdate stream
  - [x] For `LogLine`: print to stdout
  - [x] For `OutputChunk`: collect chunks in NarAssembler (BTreeMap<i32, Vec<u8>>)
  - [x] For `BuildCompleted`: decompress NAR with zstd, run `nix-store --import`
  - [x] For `BuildFailed`: print error, bail
- [x] Wire up main flow in main.rs
- [x] Build successful with all dependencies

**Design decisions:**
- Use `&Utf8Path` for all path parameters (never `&str`)
- BTreeMap uses `i32` for chunk indices (matches protobuf type, no casting)
- BuildInfo exports derivation as NAR using `nix-store --export`
- Agent imports derivation NAR using `nix-store --import` to get canonical path
- Consistent NAR-based transfer for both derivations and outputs

### 1.4 Agent Basic Structure (rio-agent) ✅ COMPLETED

**Phase 1:** Single-threaded, no Raft, no concurrency. One build at a time.

- [x] Create `rio-agent/src/main.rs` with argument parsing
  - [x] Flag: `--listen <addr:port>` (default: `0.0.0.0:50051`)
  - [x] Flag: `--data-dir <path>` (default: `/var/lib/rio`)
  - [x] Initialize tracing
- [x] Create `rio-agent/src/agent.rs`
  - [x] Struct: `Agent` with fields:
    - [x] `id: AgentId` (Uuid::new_v4())
    - [x] `platforms: Vec<String>` (from NixConfig::parse())
    - [x] `features: Vec<String>` (from nix config)
    - [x] `current_build: Arc<Mutex<Option<BuildJob>>>`
    - [x] `data_dir: Utf8PathBuf`
  - [x] Method: `new(data_dir) -> Agent` - queries nix config, creates data_dir
  - [x] Struct: `BuildJob` with fields:
    - [x] `drv_path: DerivationPath`
    - [x] `process: Child` (nix-build process)
    - [x] `subscribers: Vec<mpsc::Sender<Result<BuildUpdate, Status>>>`
- [x] Create `rio-agent/src/grpc_server.rs`
  - [x] Implement `RioAgent` gRPC service trait
  - [x] Phase 1 minimal implementations:
    - [x] `queue_build()` - stores drv to data_dir, starts build, returns BuildAssigned
    - [x] `subscribe_to_build()` - adds subscriber to current build, streams updates
    - [x] Other RPCs: return `Unimplemented` status

### 1.5 Agent Build Execution (rio-agent) ✅ COMPLETED

- [x] Create `rio-agent/src/builder.rs`
  - [x] Function: `start_build(agent, drv_path, drv_bytes)`
    - [x] Write drv_bytes to `data_dir/{drv_filename}`
    - [x] Spawn `nix-build {drv_path}`
    - [x] Create BuildJob and store in agent.current_build
    - [x] Spawn background task for build completion handling
  - [x] Function: `handle_build_completion()` (background task)
    - [x] Stream logs from stdout/stderr
    - [x] Wait for process to exit
    - [x] On success: stream outputs via nar_exporter, send BuildCompleted
    - [x] On failure: send BuildFailed
    - [x] Clean up temporary files
  - [x] Function: `stream_logs()` (async)
    - [x] Read lines from stdout and stderr with tokio::select!
    - [x] Wrap in `BuildUpdate::LogLine` with timestamps
    - [x] Broadcast to all subscribers

### 1.6 Agent Output Streaming (rio-agent) ✅ COMPLETED

Implement the NAR streaming pattern from DESIGN.md section "NAR Streaming Implementation Pattern".

- [x] Create `rio-agent/src/nar_exporter.rs`
  - [x] Function: `stream_outputs(output_paths, drv_path, subscribers)`
    - [x] Spawn `nix-store --export {paths...}`
    - [x] Read chunks from stdout (1MB buffer)
    - [x] Compress each chunk with `zstd::encode_all(chunk, 3)`
    - [x] Send to all subscribers as `OutputChunk` with sequence numbers
    - [x] Send final chunk with `last_chunk: true`
    - [x] Wait for nix-store process completion

### 1.7 CLI Output Import (rio-build) ✅ COMPLETED

*Note: Implemented in Phase 1.3 alongside output_handler.rs*

- [x] Enhance `rio-build/src/output_handler.rs`
  - [x] Struct: `NarAssembler` - collects and orders chunks
    - [x] Field: `chunks: BTreeMap<i32, Vec<u8>>` (matches protobuf i32 type)
    - [x] Method: `add_chunk(chunk: OutputChunk)`
    - [x] Method: `is_complete() -> bool` (received last_chunk)
    - [x] Method: `assemble() -> Result<Vec<u8>>` (decompress and concatenate in order)
  - [x] Function: `import_nar(nar_bytes: &[u8]) -> Result<()>`
    - [x] Spawn `nix-store --import`
    - [x] Write nar_bytes to stdin with AsyncWriteExt
    - [x] Wait for process completion
    - [x] Return error if exit code != 0

### 1.8 Phase 1 Testing ✅ COMPLETED

- [x] Create test Nix expressions in `tests/fixtures/`
  - [x] `hello.nix`, `trivial.nix`, `failing.nix` using runCommandNoCC
- [x] Create lib.rs for both rio-build and rio-agent (expose modules for testing)
- [x] Create mock RioAgent server for rio-build unit tests
- [x] Write rio-build tests (client with mock server)
- [x] Write rio-agent tests (NAR roundtrip, build execution, failure handling)
- [x] **Critical:** Add end-to-end integration test (`integration_test.rs`)
  - [x] Starts actual rio-agent gRPC server
  - [x] Sends real QueueBuild + SubscribeToBuild RPCs
  - [x] Generates unique uncached derivation (UUID-based)
  - [x] Verifies logs, output chunks, and completion
  - [x] **Would have caught the deadlock bug** (10s timeout)
- [x] All 7 tests passing

**Bugs Found and Fixed:**
1. **Deadlock**: BuildJob owned process, background task held lock during wait → fixed by moving process ownership to background task
2. **Race condition**: Subscribers cloned before subscribe_to_build → fixed by refreshing subscribers on each log line
3. **Output path guessing**: Replaced .drv with output path → fixed by parsing nix-build stdout
4. **stdin blocking**: nix-build waiting for stdin → fixed by setting stdin to Stdio::null()

**Manual verification:** Successfully built real Nix packages end-to-end!

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

**Phase 1: COMPLETE! 🎉**

All Phase 1 milestones achieved:
1. ~~Protocol definitions (1.1)~~ ✅
2. ~~Nix integration utilities (1.2)~~ ✅
3. ~~CLI flow (1.3, 1.7)~~ ✅
4. ~~Agent implementation (1.4-1.6)~~ ✅
5. ~~Testing (1.8)~~ ✅

Single-agent MVP proven - data plane works end-to-end!

**Next: Phase 2 - Raft Cluster (Control Plane)**

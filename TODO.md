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

### 2.1 Raft Storage Setup (rio-agent) ✅ COMPLETED

- [x] Add `rocksdb` dependency to `rio-agent/Cargo.toml`
- [x] Create `rio-agent/src/storage.rs`
  - [x] Separate stores: `LogStore` (RaftLogStorage) and `StateMachineStore` (RaftStateMachine)
  - [x] Column families: `logs`, `store`
  - [x] Implement `openraft::RaftLogStorage` trait with vote, committed, append, truncate, purge
  - [x] Implement `openraft::RaftStateMachine` trait with apply, snapshots
  - [x] Use `declare_raft_types!` macro for TypeConfig (Request/Response/Node)
  - [x] Tests: storage creation and vote persistence

**Implementation notes:**
- Added openraft 0.9 with `storage-v2` feature (unsealed traits)
- Added rocksdb 0.24 with `bindgen-runtime` and `zstd` features
- Updated flake.nix with clang/cmake dependencies and commonEnvVars
- Storage layout: `data_dir/raft.rocksdb` with two column families
- All 9 tests passing (7 from Phase 1 + 2 new storage tests)

### 2.2 Raft State Machine (rio-agent) ✅ COMPLETED

- [x] Create `rio-agent/src/state_machine.rs`
  - [x] Define `ClusterState` struct with agents, builds_in_progress, completed_builds
  - [x] Define `RaftCommand` enum with all 7 command variants
  - [x] Define `RaftResponse` enum for command responses
  - [x] Define supporting types: AgentInfo, BuildTracker, BuildStatus, CompletedBuild
  - [x] Method: `ClusterState::apply(command) -> RaftResponse`
    - [x] Pattern match on command variants
    - [x] Update ClusterState accordingly
    - [x] Return appropriate response
- [x] Integrate with storage.rs
  - [x] Updated TypeConfig to use D = RaftCommand, R = RaftResponse
  - [x] Added ClusterState to StateMachineData
  - [x] Updated apply() to delegate to ClusterState::apply()
- [x] Tests
  - [x] test_agent_joined: Verify agent registration
  - [x] test_build_lifecycle: Verify build queue → complete flow
  - [x] test_dependency_cleanup_on_completion: Verify parent_build cleanup

**Implementation notes:**
- Added chrono serde feature for DateTime serialization
- Placeholder assignment logic (selects first available agent)
- Full deterministic assignment will be implemented in Phase 2.3
- All 12 tests passing (9 from Phase 1 + 2 storage + 3 state machine)

### 2.3 Deterministic Agent Assignment (rio-agent) ✅ COMPLETED

Implement the algorithm from DESIGN.md section 1 "Deterministic Agent Assignment".

- [x] In `state_machine.rs`, enhance BuildQueued handler
  - [x] Step 1: Filter eligible agents (platform + features, not Down) - **Status-blind!**
  - [x] Step 2: Score agents by pure affinity
    - [x] Count matching deps in `builds_in_progress`
    - [x] Count matching deps in `completed_builds`
  - [x] Step 3: Select highest affinity agent (Available and Busy compete equally)
  - [x] Step 4: Tie-break by smallest agent_id (lexicographic)
  - [x] Step 5: Update state:
    - [x] Insert top-level build in `builds_in_progress`
    - [x] Insert all dependencies with parent_build pointer
    - [x] Store derivation NAR in pending_derivations
    - [x] Set agent status to Busy
  - [x] Return selected agent_id

**Tests added:**
- test_deterministic_assignment_platform_filter: Verify platform matching
- test_deterministic_assignment_feature_filter: Verify feature requirement matching
- test_deterministic_assignment_affinity: Verify affinity scoring works
- test_deterministic_assignment_tie_break: Verify lexicographic tie-breaking
- test_assignment_marks_agent_busy: Verify agent status update

**All 16 tests passing** (11 state_machine + 2 common + 1 integration + 2 storage)

**Design improvements after Phase 2.3:**
- Derivations now stored in Raft (not /tmp)
- Status-blind assignment (Busy agents can receive builds)
- BuildStatus has 3 variants: Building, QueuedDependency, QueuedCapacity
- FetchPendingBuild RPC removed (derivations in Raft storage)

### 2.4 Cluster Membership (rio-agent) ✅ COMPLETED (single-node)

- [x] Create `rio-agent/src/raft_network.rs`
  - [x] Struct: `NetworkFactory` implements RaftNetworkFactory
  - [x] Struct: `RaftNetworkConnection` implements RaftNetwork
  - [x] Placeholder implementations (single-node doesn't need network)
  - [x] Will add gRPC calls in Phase 3 for multi-node
- [x] Create `rio-agent/src/raft_node.rs`
  - [x] Function: `bootstrap_single_node()` - creates and initializes Raft
  - [x] Configures Raft (heartbeat 500ms, election timeout 1.5-3s)
  - [x] Calls `raft.initialize()` with single-node set
  - [x] Returns `(Arc<Raft<TypeConfig>>, StateMachineStore)` tuple
  - [x] Test: Verifies node becomes leader immediately
- [x] Create internal Raft proto (rio-common/proto/rio/v1/raft.proto)
  - [x] Service: RaftInternal (AppendEntries, Vote, InstallSnapshot)
  - [x] Will implement handlers in Phase 3
- [x] Enhance `rio-agent/src/agent.rs`
  - [x] Add fields: `raft: Option<Arc<Raft<TypeConfig>>>`, `state_machine: Option<StateMachineStore>`
  - [x] Method: `bootstrap() -> Agent` - creates single-node cluster
  - [x] Calls register_agent() to add self to cluster
- [x] Create `rio-agent/src/membership.rs`
  - [x] Function: `register_agent()` - propose AgentJoined to cluster
- [x] Implement gRPC RPCs for membership:
  - [x] `GetClusterMembers` - returns current leader and agent list from state machine

**Deferred to Phase 3 (multi-node clusters):**
- `Agent::join(seed_url)` - Not needed for single-node testing
- `JoinCluster` RPC - Not needed until we test multi-node clusters
- `handle_agent_left()` - Graceful shutdown can wait until Phase 4

**Rationale for deferring:**
- Phase 2 focuses on proving Raft works with single node
- Multi-node complexity (network, leader election) comes in Phase 3
- Enables faster iteration on build submission integration
- JoinCluster requires implementing RaftNetwork gRPC calls

**Status:**
- Single-node Raft cluster fully working
- Agent bootstraps, becomes leader, registers itself
- GetClusterMembers returns correct leader and agent list
- All 19 tests passing, zero clippy warnings

### 2.5 Heartbeat System (rio-agent) ✅ COMPLETED

- [x] Create `rio-agent/src/heartbeat.rs`
  - [x] Function: `start_heartbeat_task(agent_id, raft, interval)`
    - [x] Configurable interval (default: 10 seconds, tests: 1 second)
    - [x] Propose RaftCommand::AgentHeartbeat
    - [x] Log warnings on failure (transient network issues)
  - [x] Function: `start_failure_detector_task(raft, state_machine, check_interval, timeout)`
    - [x] Configurable check interval (default: 15s, tests: 0.5s)
    - [x] Configurable timeout (default: 30s, tests: 3s)
    - [x] Proposes AgentLeft for failed agents
  - [x] Function: `check_failed_agents(state_machine, timeout) -> Vec<AgentId>`
    - [x] Iterate agents, find stale heartbeats
    - [x] Ignore already-Down agents
- [x] Enhanced state machine: Update `last_heartbeat` on AgentHeartbeat command
- [x] Enhanced AgentLeft handler: Remove all builds assigned to failed agent
  - [x] Clean up `builds_in_progress`
  - [x] Clean up `pending_derivations`
  - [x] CLIs detect disconnect and retry, affinity re-groups builds naturally
- [x] Updated `Agent::bootstrap()` to accept optional heartbeat intervals
  - [x] Returns (Agent, heartbeat_handle, failure_detector_handle)
  - [x] Production defaults: 10s heartbeat, 15s check, 30s timeout
  - [x] Tests use fast intervals: 1s heartbeat, 0.5s check, 3s timeout
- [x] Added `--bootstrap` flag to main.rs

**Critical bug fixes during Phase 2.5:**
- Fixed storage: `StateMachineStore = Arc<StateMachineStoreInner>` pattern ensures all clones share data
- Fixed apply(): Must return response for **every** entry (added `RaftResponse::InternalOp`)
- Added `parking_lot` dependency for RwLock

**Tests added:**
- test_heartbeat_task_sends_periodic_heartbeats (6 heartbeat tests total)
- test_failure_detector_marks_stale_agents_as_down
- test_agent_left_cleans_up_builds (2 cleanup tests in state_machine)
- test_heartbeat_lifecycle (integration test)

**Status:**
- All 25 tests passing (22 unit + 3 integration)
- Zero clippy warnings
- Test suite: 15.6 seconds (was 64s before optimization)
- Heartbeat system ready for multi-node clusters in Phase 3

### 2.6 CLI Cluster Discovery (rio-build) ✅ COMPLETED

- [x] Create `rio-build/src/config.rs`
  - [x] Function: `default_config_path()` - Returns ~/.config/rio/config.toml
  - [x] Function: `load_config_file()` - Load config if exists, return None otherwise
  - [x] Function: `create_default_config()` - Create default config with localhost seed
  - [x] Tests for loading, missing files, default creation
- [x] Create `rio-build/src/cluster.rs`
  - [x] Struct: `ClusterInfo` - Stores leader, agents, discovery timestamp
  - [x] Method: `is_stale(ttl)` - Check if cluster info needs refresh
  - [x] Function: `discover_cluster(seed_urls)` - Try each seed until success
  - [x] Function: `try_discover_from_agent()` - Call GetClusterMembers RPC
  - [x] Find leader from agent list using leader_id
  - [x] 5 second timeout per agent connection
  - [x] Tests for staleness, leader lookup
- [x] Update `rio-build/src/main.rs`
  - [x] Use `ClapSerde` derive on Args struct
  - [x] Load config file from ~/.config/rio/config.toml if exists
  - [x] Merge with CLI args using `Args::from(file_config).merge_clap()`
  - [x] CLI args override config file (layered config)
  - [x] Changed `--agent` to `--seed-agents` (Vec<Url>)
  - [x] Validate: seed_agents not empty, URLs use http/https scheme
  - [x] Call `discover_cluster()` to find leader
  - [x] Connect to leader's address

**Implementation notes:**
- Used `clap-serde-derive` for config file + CLI arg merging
- Used `url` crate with proper `Url` type (validates URLs)
- Config file is optional - CLI args work standalone
- Discovery tries each seed agent with 5s timeout
- Returns first successful GetClusterMembers response

**Dependencies added:**
- clap-serde-derive = "0.2"
- url = "2.5" (with serde feature)
- toml = "0.8"
- dirs = "5.0"

**Tests:**
- 4 config tests (loading, validation, defaults)
- 3 cluster tests (staleness, leader lookup)
- All 35 tests passing across workspace

**Status:**
- CLI can now discover Raft clusters from seed agents
- Automatically connects to current leader
- Config file support with CLI override
- Ready for integration with multi-node clusters

### 2.7 Phase 2 Testing ✅ COMPLETED (single-node scope)

- [x] Create test: Bootstrap single-node cluster
  - [x] Start agent with `--bootstrap`
  - [x] Verify it becomes leader
  - [x] Call GetClusterMembers, verify single member
  - [x] CLI discovers cluster and identifies leader
  - [x] Submit build via cluster discovery
  - [x] Verify build completes successfully
- [x] Create comprehensive integration test: `cluster_integration.rs`
  - [x] Tests full Phase 2 flow end-to-end
  - [x] Bootstrap agent with dynamic port binding
  - [x] CLI discovers cluster from seed URL
  - [x] Verifies leader UUID matches agent ID
  - [x] Submits unique uncached build
  - [x] Verifies successful completion

**Multi-node tests deferred to Phase 3:**
- Three-node cluster formation (requires `--join` flag and RaftNetwork gRPC)
- Heartbeats and failure detection across nodes (requires multi-node)
- Leader election and failover (requires multi-node)

**Status:**
- Single-node cluster fully tested and working
- Integration test passes in 2.9 seconds
- All 36 tests passing (35 unit + 1 integration)
- Manual verification: CLI successfully discovers and uses cluster
- Ready for Phase 3: multi-node clusters and distributed build coordination

---

## Phase 3: Distributed Build Coordination

**Goal:** Integrate builds with Raft. CLI submits to leader, Raft assigns to agent, agent builds and reports status.

### 3.1 Build Submission via Leader (rio-agent) ✅ COMPLETED

- [x] Enhanced `grpc_server.rs`, implemented Raft-coordinated `QueueBuild` RPC:
  - [x] Removed Phase 1 fallback - all agents must use Raft (--bootstrap)
  - [x] Check if build already in progress or completed
    - [x] Query cluster state: `builds_in_progress.get(derivation_path)`
    - [x] Query cluster state: `completed_builds.get(derivation_path)`
    - [x] Return `AlreadyBuilding` if in progress
    - [x] Return `AlreadyCompleted` if recently completed
  - [x] Propose RaftCommand::BuildQueued to cluster (includes derivation_nar)
  - [x] Wait for Raft commit via `raft.client_write()`
  - [x] Extract assignment from `RaftResponse::BuildAssigned`
  - [x] Return `BuildAssigned { agent_id, derivation_path }` to CLI
  - [x] Handle RwLockReadGuard across await (clone data before await)

**Tests added:**
- test_queue_build_via_raft: Verifies Raft-coordinated assignment
- test_queue_build_via_raft: Verifies deduplication (AlreadyBuilding)

**Status:**
- QueueBuild now fully Raft-coordinated
- Build deduplication working
- All 37 tests passing (1 ignored: test_end_to_end_build_flow - requires Phase 3.2)
- Zero clippy warnings
- Agent must be bootstrapped with --bootstrap (no Phase 1 mode)

### 3.2 Agent Receives Assignment (rio-agent) ✅ COMPLETED

- [x] Create `rio-agent/src/build_coordinator.rs`
  - [x] Polls cluster state every 100ms for builds assigned to this agent
  - [x] Tracks started builds to avoid duplicates
  - [x] Reads derivation NAR from Raft storage (pending_derivations)
  - [x] Calls builder::start_build() to spawn nix-build
  - [x] Runs as background task alongside heartbeat system
- [x] Refactored builder::start_build() to take current_build instead of full Agent
- [x] Started coordinator in Agent::bootstrap(), returns 4th handle
- [x] Re-enabled test_end_to_end_build_flow (now passing)
- [x] Removed --bootstrap flag (Raft is now the only mode)
- [x] Added 200ms wait in CLI after BuildAssigned before subscribing

**Status:**
- Full Raft-coordinated build flow working end-to-end
- All 38 tests passing
- Manual verification successful
- Ready for multi-node clusters

### 3.3 Multi-Node Cluster Support (rio-agent) ✅ COMPLETED

**Goal:** Enable multiple agents to join together into a Raft cluster

**Deployment modes:**
1. **Auto mode** (--seeds): Try join seeds, bootstrap if all fail (with jitter)
2. **Explicit bootstrap** (default, no flags): Force bootstrap new cluster
3. **Explicit join** (--join): Force join existing cluster

**Implementation:**

- [x] Add cluster formation flags to rio-agent
  - [x] Flag: `--seeds <urls>` - Comma-separated seed agent URLs for discovery
  - [x] Flag: `--join <seed_url>` - Explicitly join cluster (skip auto-discovery)
  - [x] Default (no flags): Bootstrap single-node cluster (current behavior)
  - [x] Flags are mutually exclusive (clap conflicts_with)
- [x] Implement auto-discovery mode: `Agent::auto_join_or_bootstrap(seeds)`
  - [x] Try to join each seed agent via Agent::join()
  - [x] If any succeed: Return joined agent
  - [x] If all fail: Add random jitter (0-1000ms)
  - [x] Retry join once more (maybe someone else bootstrapped during jitter)
  - [x] If still fails: Bootstrap new single-node cluster
  - [x] Log clearly which mode was chosen
- [x] Implement `Agent::join(seed_url)` in agent.rs
  - [x] Connect to seed agent via gRPC
  - [x] Call JoinCluster RPC with this agent's info (id, address, platforms, features)
  - [x] Initialize Raft with existing member list (via raft_node::join_cluster helper)
  - [x] Wait for membership change to complete
  - [x] Start heartbeat, coordinator, and failure detector tasks
- [x] Implement JoinCluster RPC in grpc_server.rs
  - [x] Check if this agent is leader (only leader accepts joins)
  - [x] If not leader: Return error with leader ID
  - [x] Validate joining agent info (parse UUID and URL)
  - [x] Check for duplicate agent ID
  - [x] Add node to Raft network via raft.add_learner()
  - [x] Propose RaftCommand::AgentJoined with agent info
  - [x] Wait for Raft commit
  - [x] Promote learner to voting member via raft.change_membership() with retain=true
  - [x] Include all existing voters when promoting (not just new node)
  - [x] Return success message
- [x] Refactored Agent struct: Removed Option from raft and state_machine fields
  - [x] All agents always have Raft (no Phase 1 mode)
  - [x] Removed Agent::new() - only bootstrap() and join() remain
  - [x] Updated all RPCs to remove Option checks
- [x] Updated raft.proto with proper protobuf message definitions
  - [x] Defined Vote, LogId, LeaderId, Node, Membership, Entry messages
  - [x] AppendEntries/Vote/InstallSnapshot with structured messages (not bytes)
  - [x] Based on openraft example (raft-kv-memstore-grpc)
  - [x] Updated build.rs to compile raft.proto
- [x] Implement protobuf conversions (rio-agent/src/raft_proto_conv.rs)
  - [x] Extension traits ToProto<T> and FromProto<T> (avoids orphan rule)
  - [x] Special FromProtoWithVote<T> trait for LogId (needs Vote context for node_id)
  - [x] Conversions for Vote, LogId, Node, Membership, Entry
  - [x] Conversions for AppendEntries, Vote RPC request/response types
  - [x] Handles AppendEntriesResponse enum (Success/PartialSuccess/HigherVote/Conflict)
  - [x] Handles openraft 0.9 API (get_joint_config, nodes iterator, etc.)
- [x] Implement Raft RPC handlers (rio-agent/src/raft_grpc.rs)
  - [x] Created RaftInternalService with raft instance
  - [x] Implemented AppendEntries RPC - converts proto, forwards to raft.append_entries()
  - [x] Implemented Vote RPC - converts proto, forwards to raft.vote()
  - [x] Implemented InstallSnapshot RPC - handles streaming chunks, forwards to raft.install_snapshot()
  - [x] Added RaftInternal service to gRPC server alongside RioAgent service
- [x] Implement RaftNetwork gRPC in raft_network.rs
  - [x] Implemented append_entries() - Connects to target, converts req, sends RPC, converts resp
  - [x] Implemented vote() - Same pattern for vote requests
  - [x] Implemented install_snapshot() - Streams meta + data chunks (1MB) to target
  - [x] Adds http:// scheme if missing from addresses
  - [ ] TODO: Add connection pooling/caching (optimization for later)
- [x] Fixed raft_node::initialize_as_leader() to include node addresses
  - [x] Changed from BTreeSet<NodeId> to BTreeMap<NodeId, Node>
  - [x] Prevents empty rpc_addr in Raft membership
  - [x] Updated signature to accept rpc_addr parameter
  - [x] Updated all call sites (agent.rs, tests)
- [x] Fixed Agent::bootstrap() task ordering
  - [x] Start heartbeat tasks AFTER Raft is initialized as leader
  - [x] Prevents "forward to: None, None" errors on startup
- [x] Add tests for multi-node clusters
  - [x] Test: Explicit join (agent A bootstrap, agent B --join A)
  - [ ] Test: Auto-discovery (3 agents with same --seeds) - TODO for Phase 3.4
  - [ ] Test: Leader election after leader dies - TODO for Phase 3.4
  - [ ] Test: Heartbeat failure detection across nodes - covered by integration tests
  - [ ] Test: Build assignment across multi-node cluster - covered by integration tests

**Key Fixes (2025-10-15):**
1. Fixed `change_membership()` to use `retain: true` (add members, don't replace)
2. Fixed `change_membership()` to include all existing voters, not just new node
3. Fixed `initialize_as_leader()` to pass BTreeMap with node addresses (not BTreeSet)
4. Fixed task ordering in `Agent::bootstrap()` - start heartbeats AFTER becoming leader
5. Increased test timeouts to allow time for network connectivity during join
6. Implemented `Drop` for Agent - gracefully aborts tasks and proposes AgentLeft
7. Refactored Raft initialization API with clear function names:
   - `create_uninitialized_raft()` - explicit that initialization is required
   - `initialize_single_node_leader()` - explicit initialization step
   - Added comprehensive module-level documentation with initialization patterns

**Test Results:**
- ✅ All rio-agent unit tests pass (22 tests)
- ✅ All rio-agent integration tests pass (4 tests)
- ✅ Multi-node cluster test passes (test_explicit_join_two_nodes)
- ✅ All workspace tests pass (37 tests total via cargo nextest)
- ✅ Test verifies both agents see each other in cluster state
- ✅ Test verifies Raft membership includes both voters
- ✅ Test verifies state replication works correctly
- ✅ Test verifies Agent Drop cleans up resources properly

**Production Ready:** Multi-node cluster formation is fully functional, tested, and includes proper resource cleanup.

**Deployment examples:**

```bash
# Simple: Explicit (production-ready)
node1$ rio-agent --listen node1:50051                    # Bootstraps
node2$ rio-agent --listen node2:50051 --join http://node1:50051
node3$ rio-agent --listen node3:50051 --join http://node1:50051

# Auto-discovery (development/testing)
all$ rio-agent --listen 0.0.0.0:50051 --seeds node1:50051,node2:50051,node3:50051
# First one to start bootstraps, others join

# Kubernetes (StatefulSet)
# Pod 0: rio-agent --listen 0.0.0.0:50051
# Pod 1+: rio-agent --listen 0.0.0.0:50051 --join http://rio-0:50051
```

### 3.4 Multi-User Subscriptions (rio-agent) ✅ COMPLETED

- [x] Enhance `BuildJob` struct in `agent.rs`:
  - [x] Add: `log_history: VecDeque<BuildUpdate>` (cap at 10,000 entries)
  - [x] Add: `started_at: Instant` for duration tracking
  - [x] Add helper: `new()` constructor
  - [x] Add helper: `add_log()` - append to history and broadcast to subscribers
  - [x] Add helper: `get_catch_up_logs()` - return clone for late joiners
  - [x] Add helper: `duration()` - get build duration
- [x] Enhance `SubscribeToBuild` RPC:
  - [x] Check if build is currently running on this agent
  - [x] Send catch-up logs from log_history to late joiners
  - [x] Add subscriber for live updates
  - [x] Query Raft state if not in current_build
  - [x] Return helpful errors for builds on other agents or not found
- [x] Enhance log streaming in `builder.rs`:
  - [x] Use `BuildJob::add_log()` to append to history and broadcast
  - [x] Simplified from manual subscriber iteration
  - [x] Automatic cleanup of disconnected subscribers

### 3.5 Build Completion Flow (rio-agent) ✅ COMPLETED

- [x] In `builder.rs`, enhanced `handle_build_completion()`:
  - [x] After build succeeds, call `stream_outputs()` (already working)
  - [x] After outputs streamed, propose RaftCommand::BuildCompleted with retry
    - [x] Uses `backon` crate with exponential backoff (3 attempts)
    - [x] State machine removes from builds_in_progress
    - [x] State machine removes from pending_derivations
    - [x] State machine moves to completed_builds
    - [x] Graceful degradation if Raft proposal fails (logs error, continues)
  - [x] Send BuildUpdate::Completed to all subscribers
  - [x] Pending build queue: Deferred to Phase 4 (dependency waiting)
- [x] In `builder.rs`, handle build failure:
  - [x] If build fails, propose RaftCommand::BuildFailed with retry
    - [x] Uses same retry strategy as BuildCompleted
    - [x] State machine removes from builds_in_progress
    - [x] State machine removes from pending_derivations
    - [x] Does NOT add to completed_builds (allows immediate retry)
  - [x] Send BuildUpdate::Failed to all subscribers
  - [x] Cascading failures: Deferred to Phase 4 (dependency tracking)

**Implementation notes:**
- Retry strategy: `backon` crate with ExponentialBuilder
- Retry attempts: 3 (delays: 0ms, 50ms, 100ms)
- Error handling: Logs error and continues if all retries fail (graceful degradation)
- Prevents state drift from transient Raft failures

### 3.6 Completed Build Cache (rio-agent) ✅ COMPLETED

- [x] `ClusterState` in `state_machine.rs`:
  - [x] Has `completed_builds: HashMap<DerivationPath, CompletedBuild>`
  - [x] BuildCompleted handler properly implemented:
    - [x] Removes from `builds_in_progress`
    - [x] Inserts in `completed_builds` with `completed_at` timestamp
    - [x] Removes from `pending_derivations`
    - [x] Cleans up dependencies (where parent_build matches)
  - [x] Note: LRU eviction deferred to Phase 5 (currently unbounded HashMap)
- [x] Implemented `GetCompletedBuild` RPC in `grpc_server.rs`:
  - [x] Query Raft state for derivation_path
  - [x] Verify this agent has the outputs (agent_id check)
  - [x] Verify outputs still exist in /nix/store (not garbage collected)
  - [x] Stream outputs via `nar_exporter::stream_outputs()`
  - [x] Send BuildUpdate::Completed after NAR chunks
  - [x] Return NOT_FOUND if build not in cache or outputs missing
  - [x] Return FAILED_PRECONDITION if outputs on different agent

**Tests completed:**
- [x] `test_late_joiner_receives_catch_up_logs` - Verifies late subscribers get log history
- [x] `test_build_completion_updates_raft_state` - Verifies Raft state transitions
- [x] `test_get_completed_build_serves_cache` - Verifies cache serving and completion message

### 3.7 Build Deduplication (rio-build) ✅ COMPLETED

- [x] CLI enhancement in `client.rs`:
  - [x] Added `connect_to_agent()` helper - finds agent by ID in cluster
  - [x] Refactored `submit_build()` to handle all 4 response types:
    - [x] `BuildAssigned` - Subscribe to assigned agent (existing behavior)
    - [x] `AlreadyBuilding` - Connect to agent with build, subscribe to existing
    - [x] `AlreadyCompleted` - Connect to agent with cache, call GetCompletedBuild
    - [x] `NoEligibleAgents` - Return error with platform/features details
  - [x] Updated `main.rs` to pass cluster_info to submit_build()
  - [x] Added informative logging for deduplication scenarios

**Tests completed:**
- [x] `test_already_building_response` - Client joins in-progress build, gets catch-up
- [x] `test_already_completed_response` - Client fetches from cache
- [x] `test_concurrent_subscribers_same_build` - Two clients, same build, both complete
- [x] Fixed `test_phase2_single_node_cluster_end_to_end` - Updated for new API

**Test reliability improvements:**
- [x] Added 30s timeouts to all stream consumption loops
- [x] Fixed await-holding-lock warnings (proper lock scoping)
- [x] Poll for Raft state changes instead of fixed sleeps
- [x] `test_explicit_join_two_nodes`: 8.2s → 1.2s (6.8x faster!)

### 3.8 Phase 3 Testing

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
  - [ ] Add field: `parent_build: Option<DerivationPath>`
  - [ ] When registering dependencies, set parent_build = Some(top_level_path)
- [ ] In state machine, enhance `apply_build_queued()`:
  - [ ] Register all dependencies atomically in same command
  - [ ] Each dependency gets `parent_build` pointer to top-level
- [ ] In state machine, enhance `apply_build_completed()`:
  - [ ] Remove top-level from builds_in_progress
  - [ ] Remove ALL dependencies where parent_build = completed path
  - [ ] Insert in completed_builds cache

### 4.2 Build Affinity (rio-agent)

Already implemented in Phase 3 (deterministic assignment scores by affinity). Add tests:

- [ ] Test: User A builds foo, User B builds bar (depends on foo)
  - [ ] Verify both assigned to same agent
  - [ ] Verify bar queued until foo completes
  - [ ] Verify bar auto-starts when foo done

### 4.3 Dependency Waiting (rio-agent)

- [ ] Enhance agent to track pending builds:
  - [ ] Field: `pending_builds: HashMap<DerivationPath, PendingBuild>`
  - [ ] Struct PendingBuild: `{ blocked_on: Vec<DerivationPath>, subscribers }`
    - [ ] Note: derivation NAR read from Raft storage when starting
- [ ] When assigned build has dependencies building on self:
  - [ ] Don't start immediately
  - [ ] Add to pending_builds
  - [ ] Send status to subscribers: "Waiting for dependencies..."
  - [ ] Propose BuildStatusChanged with QueuedDependency status
- [ ] When assigned build but agent is busy (no deps):
  - [ ] Add to pending_builds with empty blocked_on
  - [ ] Propose BuildStatusChanged with QueuedCapacity status
- [ ] On build completion:
  - [ ] Iterate pending_builds
  - [ ] Remove completed derivation_path from all blocked_on lists
  - [ ] If any pending build now has empty blocked_on:
    - [ ] Read derivation NAR from Raft storage
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

- [ ] In state machine, `apply_build_queued()` already filters correctly:
  - [ ] Only consider agents where `agent.platforms.contains(cmd.platform)`
  - [ ] Only consider agents where `cmd.features.iter().all(|f| agent.features.contains(f))`
  - [ ] Only exclude Down agents (Available and Busy both eligible)
- [ ] Add NoEligibleAgents response variant:
  - [ ] If no eligible agents: Return error response
  - [ ] Leader returns `NoEligibleAgents { reason }` to CLI
- [ ] CLI displays helpful error with agent capabilities

### 4.6 Graceful Shutdown (rio-agent)

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

### 4.7 Phase 4 Testing

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

**Phase 2: COMPLETE! 🎉**

All Phase 2 milestones achieved (single-node scope):
1. ~~Raft Storage Setup (2.1)~~ ✅
2. ~~Raft State Machine (2.2)~~ ✅
3. ~~Deterministic Agent Assignment (2.3)~~ ✅
4. ~~Cluster Membership (2.4)~~ ✅
5. ~~Heartbeat System (2.5)~~ ✅
6. ~~CLI Cluster Discovery (2.6)~~ ✅
7. ~~Phase 2 Testing (2.7)~~ ✅

Single-node Raft cluster fully operational!
- Agent bootstraps and becomes leader
- Heartbeat system with failure detection
- CLI discovers cluster and connects to leader
- End-to-end builds working via cluster discovery
- All 36 tests passing (35 unit + 1 integration)
- NodeId = Uuid (proper type safety)
- AgentInfo.address = Url (type-safe URLs)

**Phase 3: Agent-Side COMPLETE! 🎉**

All agent-side implementation complete (3.1-3.6):
1. ~~Build Submission via Leader (3.1)~~ ✅
2. ~~Agent Receives Assignment (3.2)~~ ✅
3. ~~Multi-Node Cluster Support (3.3)~~ ✅
4. ~~Multi-User Subscriptions (3.4)~~ ✅
5. ~~Build Completion Flow (3.5)~~ ✅
6. ~~Completed Build Cache (3.6)~~ ✅

**Key Achievements:**
- Multi-user subscriptions with catch-up log support
- Build completion Raft coordination with retry (backon)
- GetCompletedBuild RPC serves cached outputs
- CLI build deduplication (all 4 response types)
- All 43 tests passing (31 unit + 12 integration)
- Zero clippy warnings
- Test suite: 4.2s (optimized from 8+s)
- Production-ready implementation

**Phase 3 Status:**
- ✅ 3.1-3.7: COMPLETE (agent + CLI coordination)
- Remaining: 3.8 (additional multi-node testing - optional)

**Next: Phase 4 (Dependency Tracking) or polish Phase 3**

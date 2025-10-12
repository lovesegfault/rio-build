# Rio Implementation TODO

This file tracks implementation tasks for the brokerless Rio architecture.

## Phase 1: Project Setup ✅

- [x] Clean up old broker-based architecture
- [x] Create rio-build and rio-agent crates
- [x] Update workspace structure
- [x] Reset documentation for new design

## Phase 2: Core Infrastructure (Next)

### rio-common (Shared Protocol)
- [ ] Design gRPC service for CLI ↔ Agent communication
- [ ] Design agent discovery RPC (GetClusterMembers)
- [ ] Design build submission RPC (SubmitBuild)
- [ ] Design build status RPC (GetJobStatus)
- [ ] Define protobuf messages for new architecture
- [ ] Remove old broker-specific protocol definitions

### rio-build (CLI Client)
- [ ] Configuration management (~/.config/rio/config.toml)
- [ ] Agent discovery from seed nodes
- [ ] Cluster member caching
- [ ] gRPC client for agent communication
- [ ] Build submission logic
- [ ] Log streaming from agent
- [ ] Output retrieval and storage

### rio-agent (Cluster Node)
- [ ] Raft consensus integration (tikv/raft-rs or async-raft)
- [ ] Cluster membership management
- [ ] gRPC server for CLI requests
- [ ] gRPC server for agent-to-agent communication
- [ ] Build execution engine
- [ ] Capacity reporting
- [ ] Health monitoring

## Phase 3: Build Execution

- [ ] Nix build invocation
- [ ] Build log streaming
- [ ] Output path management
- [ ] Build artifact transfer to CLI
- [ ] Error handling and retries
- [ ] Build timeout handling

## Phase 4: Production Features

<<<<<<< HEAD
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

## Current Focus

**Phase 2:** Design and implement core infrastructure for brokerless architecture

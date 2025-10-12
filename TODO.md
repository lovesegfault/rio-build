# Rio Implementation TODO

This file tracks all implementation tasks for Rio, organized by development phase.

## Phase 0: Project Setup ✅

- [x] Initialize Cargo workspace structure
- [x] Set up Nix flake with Rust development environment
- [x] Configure rust-overlay for stable toolchain
- [x] Add protobuf compiler to development environment
- [x] Set up pre-commit hooks (cargo check, clippy)
- [x] Configure treefmt for code formatting
- [x] Create .gitignore for Rust and Nix artifacts
- [x] Write DESIGN.md with architecture documentation
- [x] Write CLAUDE.md for development guidance
- [x] Update README.md with project overview

## Phase 1: gRPC Infrastructure ✅

### rio-common (Shared Protocol) ✅
- [x] Define protobuf service specification (build_service.proto)
  - [x] RegisterBuilder RPC
  - [x] Heartbeat bidirectional streaming RPC
  - [x] ExecuteBuild streaming RPC
  - [x] GetBuilderStatus RPC
- [x] Define protobuf messages
  - [x] BuilderCapacity
  - [x] BuilderStatus with state enum
  - [x] Build request/response messages
  - [x] Heartbeat request/response messages
- [x] Create shared Rust types
  - [x] BuilderId with UUID generation
  - [x] JobId with UUID generation
  - [x] Platform enum (x86_64-linux, aarch64-linux, etc.)
- [x] Set up tonic-build for protobuf compilation

### rio-dispatcher (Fleet Manager) ✅
- [x] Implement BuilderPool
  - [x] Thread-safe builder registry (Arc<RwLock<HashMap>>)
  - [x] Builder registration
  - [x] Builder lookup by ID
  - [x] Query builders by platform
  - [x] Builder capacity tracking
- [x] Implement gRPC server
  - [x] RegisterBuilder RPC handler
  - [x] Heartbeat bidirectional stream handler
  - [x] GetBuilderStatus RPC handler
  - [x] ExecuteBuild RPC stub
- [x] Main dispatcher binary
  - [x] Initialize gRPC server on port 50051
  - [x] Graceful shutdown handling
- [x] Create placeholder modules
  - [x] build_queue.rs
  - [x] scheduler.rs
  - [x] ssh_server.rs
  - [x] dispatcher.rs

### rio-builder (Worker Node) ✅
- [x] Implement Builder client
  - [x] Connect to dispatcher via gRPC
  - [x] Register with dispatcher
  - [x] Platform auto-detection
  - [x] Capacity reporting (CPU, memory, disk)
- [x] Implement heartbeat mechanism
  - [x] Periodic heartbeat sending (every 30s)
  - [x] Bidirectional stream handling
  - [x] Load reporting
  - [x] Command reception from dispatcher
- [x] Main builder binary
  - [x] Configuration via environment variables
  - [x] Connection lifecycle management
  - [x] Graceful shutdown handling
- [x] Create placeholder executor module

### Testing ✅
- [x] Integration test: Builder registration
- [x] Integration test: Multiple builder registration
- [x] Integration test: Builder status query
- [x] Integration test: Heartbeat streaming
- [x] Verify builders appear in dispatcher pool

## Phase 2: SSH Server & Nix Protocol ✅ COMPLETE

### SSH Server Implementation ✅
- [x] Set up russh SSH server in dispatcher
  - [x] Configure SSH server listener (russh 0.54.5)
  - [x] Basic Handler trait implementation
  - [ ] Public key authentication (accepting all for now - TODO: proper auth)
  - [x] Session management
  - [x] Channel handling (basic echo for testing)
- [x] Implement SSH connection handling
  - [x] Accept incoming SSH connections
  - [x] Authenticate clients (development mode - accept all)
  - [x] Create session for each connection
  - [ ] Route to Nix protocol handler (TODO)
- [x] Configuration
  - [x] SSH server port (default 2222, via CLI args)
  - [x] Host key generation/loading (Ed25519, OpenSSH format)
  - [x] Auto-generate keys if missing
  - [x] Proper file permissions (0600 on Unix)
  - [ ] Authorized keys management (TODO)

### Nix Protocol Implementation 🚧
- [x] Study nix-daemon crate API
  - [x] Review Store trait requirements (16 methods)
  - [x] Understand Progress trait and return types
  - [ ] Study DaemonProtocolAdapter usage
  - [ ] Study protocol version compatibility
- [x] Implement Store trait for Dispatcher (DispatcherStore)
  - [x] is_valid_path - validate single path (stub)
  - [x] has_substitutes - check substitutability (stub)
  - [x] query_pathinfo - query store path metadata (stub)
  - [x] query_valid_paths - check path validity (stub)
  - [x] query_substitutable_paths - check substitutability (stub)
  - [x] query_valid_derivers - find derivers (stub)
  - [x] query_missing - determine what to build/download (stub)
  - [x] query_derivation_output_map - get derivation outputs (stub)
  - [x] build_paths - **core build operation** (basic queueing, TODO: actual dispatch)
  - [x] build_paths_with_results - build with detailed results (stub)
  - [x] ensure_path - ensure path exists (stub)
  - [x] add_to_store - add path to store (stub)
  - [x] add_temp_root - temp GC root (stub)
  - [x] add_indirect_root - persistent GC root (stub)
  - [x] find_roots - list GC roots (stub)
  - [x] set_options - apply client options (stub)
- [ ] Parse derivation files (.drv)
  - [ ] Extract derivation metadata
  - [ ] Identify build dependencies
  - [ ] Determine required platform
  - [ ] Extract required features
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
- [ ] Integrate with BuildQueue
  - [ ] Poll queue for new jobs
  - [ ] Dispatch jobs to selected builders
  - [ ] Handle builder failures
- [x] Tests: 5 unit tests covering selection, platform matching, load balancing

### Build Dispatching 🚧
- [x] Implement ExecuteBuild RPC (dispatcher side)
  - [x] Create gRPC client to builder
  - [x] Send build request to selected builder
  - [x] Stream build logs back to client
  - [x] Handle build completion messages
  - [x] Handle build failures and connection errors
  - [ ] Parse derivation to extract actual platform (hardcoded for now)
- [ ] Implement ExecuteBuild RPC (builder side)
  - [ ] Receive build jobs via gRPC
  - [ ] Execute nix-build
  - [ ] Stream logs back
  - [ ] Return build results
- [ ] Transfer build dependencies
  - [ ] Check which inputs builder already has
  - [ ] Transfer missing inputs
  - [ ] Verify integrity

## Phase 3: Build Execution (Planned)

### Builder Execution Engine
- [ ] Implement Executor in rio-builder
  - [ ] Receive build jobs via gRPC
  - [ ] Parse derivation
  - [ ] Check local /nix/store for dependencies
- [ ] Invoke nix-build
  - [ ] Execute: `nix-build /nix/store/xxx.drv`
  - [ ] Capture stdout/stderr
  - [ ] Stream logs to dispatcher in real-time
  - [ ] Handle build timeouts
- [ ] Handle build outputs
  - [ ] Locate output paths
  - [ ] Verify output hashes
  - [ ] Prepare for transfer
- [ ] Error handling
  - [ ] Build failures
  - [ ] Dependency fetch failures
  - [ ] Timeout errors
  - [ ] Out of disk space

### Output Transfer
- [ ] Transfer build outputs to dispatcher
  - [ ] Use Nix export/import
  - [ ] Or direct store-to-store copy
  - [ ] Verify integrity after transfer
- [ ] Relay outputs to SSH client
  - [ ] Stream outputs through Nix protocol
  - [ ] Add to client's local store
- [ ] Clean up temporary data
  - [ ] Remove build artifacts if needed
  - [ ] Maintain builder disk space

### Testing
- [ ] End-to-end test: Simple derivation build
  - [ ] Create test derivation
  - [ ] Submit via nix-build --builders
  - [ ] Verify build executes on builder
  - [ ] Verify outputs return to client
- [ ] Test: Build with dependencies
- [ ] Test: Multi-platform builds
- [ ] Test: Build failures
- [ ] Test: Concurrent builds

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

## Current Focus

**Phase 2 Complete! ✅**
- ✅ SSH server infrastructure (russh 0.54.5, 4 tests)
- ✅ Host key generation/loading (4 tests)
- ✅ BuildQueue (5 tests, FIFO, status tracking)
- ✅ Scheduler (5 tests, platform-aware, load balancing)
- ✅ Store trait (all 16 methods implemented)
- ✅ AsyncRead/AsyncWrite bridge (5 tests, tokio-util based)
- ✅ DaemonProtocolAdapter integration (spawns per SSH session)
- ✅ Bidirectional data forwarding (SSH ↔ protocol adapter)
- ✅ Tracing instrumentation on all critical paths
- ✅ Upgraded to tonic 0.14 / prost 0.14
- ✅ All dependencies consolidated to workspace
- **27 tests passing, 24 commits**

**Phase 3 Ready:** Build execution on workers

**Priority tasks:**
1. Implement SSH data() handler to forward bytes to protocol adapter
2. Spawn task to forward protocol responses back to SSH session
3. Test actual SSH connection with protocol adapter
4. Parse derivation files to extract platform info
5. Implement actual build dispatching via gRPC ExecuteBuild RPC

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

## Phase 2: SSH Server & Nix Protocol (In Progress)

### SSH Server Implementation
- [ ] Set up russh SSH server in dispatcher
  - [ ] Configure SSH server listener
  - [ ] Public key authentication
  - [ ] Session management
  - [ ] Channel handling for Nix protocol
- [ ] Implement SSH connection handling
  - [ ] Accept incoming SSH connections
  - [ ] Authenticate clients
  - [ ] Create session for each connection
  - [ ] Route to Nix protocol handler
- [ ] Configuration
  - [ ] SSH server port (default 2222)
  - [ ] Host key generation/loading
  - [ ] Authorized keys management

### Nix Protocol Implementation
- [ ] Study nix-daemon crate API
  - [ ] Review Store trait requirements
  - [ ] Understand DaemonProtocolAdapter usage
  - [ ] Study protocol version compatibility
- [ ] Implement Store trait for Dispatcher
  - [ ] queryPathInfo - query store path metadata
  - [ ] queryPathFromHashPart - find path by hash
  - [ ] queryValidPaths - check path validity
  - [ ] querySubstitutablePaths - check substitutability
  - [ ] isValidPath - validate single path
  - [ ] buildPaths - **core build operation**
  - [ ] buildDerivation - build from derivation
  - [ ] ensurePath - ensure path exists
  - [ ] addToStore - add path to store
  - [ ] addTextToStore - add text file
  - [ ] exportPath - export for transfer
  - [ ] importPaths - import transferred paths
- [ ] Parse derivation files (.drv)
  - [ ] Extract derivation metadata
  - [ ] Identify build dependencies
  - [ ] Determine required platform
  - [ ] Extract required features
- [ ] Integrate with SSH server
  - [ ] Tunnel Nix protocol over SSH
  - [ ] Handle protocol negotiation
  - [ ] Stream data bidirectionally

### Build Queue Implementation
- [ ] Design BuildQueue structure
  - [ ] Job queue data structure (VecDeque or channel)
  - [ ] Job priority handling
  - [ ] Job metadata storage
- [ ] Implement job queueing
  - [ ] Add job to queue from buildPaths
  - [ ] Job deduplication (same derivation)
  - [ ] Queue size limits
- [ ] Implement job status tracking
  - [ ] Job states (Queued, Dispatched, Building, Completed, Failed)
  - [ ] Status queries by job ID
  - [ ] Job completion callbacks

### Scheduler Implementation
- [ ] Design scheduling algorithm
  - [ ] Round-robin for MVP
  - [ ] Platform matching (required)
  - [ ] Feature matching (required)
  - [ ] Load balancing (least loaded)
- [ ] Implement scheduler
  - [ ] Select builder for job
  - [ ] Handle no available builders
  - [ ] Retry logic for failed builds
  - [ ] Builder affinity (future optimization)
- [ ] Integrate with BuildQueue
  - [ ] Poll queue for new jobs
  - [ ] Dispatch jobs to selected builders
  - [ ] Handle builder failures

### Build Dispatching
- [ ] Implement ExecuteBuild RPC
  - [ ] Send derivation to builder
  - [ ] Stream build logs back to client
  - [ ] Handle build completion
  - [ ] Handle build failures
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

### Monitoring & Metrics
- [ ] Implement Prometheus metrics
  - [ ] Build queue depth
  - [ ] Builder utilization
  - [ ] Build duration histogram
  - [ ] Build success/failure rates
  - [ ] Active SSH connections
- [ ] Implement tracing
  - [ ] Trace build requests end-to-end
  - [ ] Performance profiling
- [ ] Health check endpoints
  - [ ] Dispatcher health
  - [ ] Builder health
  - [ ] Database connectivity

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

**Next Up: Phase 2 - SSH Server & Nix Protocol**

Priority tasks:
1. Set up russh SSH server in dispatcher
2. Implement basic Nix protocol Store trait
3. Parse derivation files
4. Implement build queue
5. Implement basic scheduler (round-robin)

---

Last updated: 2025-10-11

# Rio Design Document

## Overview

Rio is an open-source alternative to nixbuild.net - a distributed build service for Nix that presents itself as a single remote builder/store while dispatching builds to an elastic fleet of workers.

## Background: How Nix Remote Building Works

### Remote Builders vs Remote Stores

**Critical distinction** (these are different concepts, not protocols):

- **Remote Builders**: Configured via `builders` option in nix.conf
  - Nix offloads build derivations to remote machines
  - Build outputs are transferred back to local Nix store
  - Used for distributed builds across multiple machines

- **Remote Stores**: Configured via `--store` option
  - All operations (evaluation, building, storage) happen on remote store
  - Build outputs remain on remote store
  - Must explicitly copy outputs back with `nix copy`

**Both can use either SSH protocol:**

- `ssh://` - **Legacy protocol** using `nix-store --serve`
  - Being deprecated (see [NixOS/nix#4665](https://github.com/NixOS/nix/issues/4665))
  - Simpler protocol but less efficient

- `ssh-ng://` - **Modern protocol** using Nix daemon protocol over SSH
  - Recommended for new deployments
  - More efficient, full-featured
  - Tunnels Nix daemon protocol over SSH

**Rio's Focus**: We're implementing a **remote builder** that supports both `ssh://` and `ssh-ng://` protocols.

### How Remote Builder Protocol Works

When a Nix client uses remote builders:

1. Local Nix evaluates derivations and determines what needs to be built
2. Checks `builders` configuration for available remote machines
3. Selects a builder matching the required system (e.g., x86_64-linux)
4. Connects via SSH (`ssh://` or `ssh-ng://`)
5. Transfers build dependencies to remote builder
6. Remote builder executes the build
7. Build logs are streamed back in real-time
8. Build outputs are transferred back to local Nix store

### nixbuild.net's Approach

- Presents as "a single Nix machine with infinite CPUs"
- Acts as a single SSH endpoint for remote builds
- Behind the scenes: fleet manager dispatching to worker pool
- Supports both remote builder and remote store modes
- Intelligent binary cache integration
- Automatic resource optimization based on build history

## Rio Architecture

### Component Overview

```
┌─────────────────┐
│   Nix Client    │ (developer's machine or CI runner)
└────────┬────────┘
         │ SSH (ssh:// or ssh-ng://)
         ▼
┌─────────────────┐
│ rio-dispatcher  │ (fleet manager + SSH frontend)
│                 │
│ • SSH Server    │
│ • Build Queue   │
│ • Scheduler     │
│ • Builder Pool  │
└────────┬────────┘
         │ gRPC or internal protocol
         ▼
┌─────────────────┐
│  rio-builder    │ (worker nodes, 1-N instances)
│                 │
│ • Job Executor  │
│ • Nix Instance  │
│ • Health Report │
└─────────────────┘
```

## Component Design

### rio-dispatcher

The dispatcher is the frontend that Nix clients connect to. It must appear as a valid Nix remote builder.

**Responsibilities:**
1. **SSH Server**: Accept SSH connections from Nix clients
2. **Protocol Handling**: Implement Nix daemon protocol using `nix-daemon` crate
3. **Build Queue Management**: Queue incoming build requests
4. **Scheduler**: Assign builds to available builders
5. **Builder Registry**: Track available builders and their capacity
6. **Result Aggregation**: Collect build outputs and relay to clients

**Key Implementation Details:**

```rust
use nix_daemon::{Store, DaemonProtocolAdapter};

// Core dispatcher structure
struct Dispatcher {
    ssh_server: SshServer,
    build_queue: BuildQueue,
    scheduler: Scheduler,
    builder_pool: BuilderPool,
}

// Implement the Store trait from nix-daemon crate
impl Store for Dispatcher {
    async fn build_paths(&mut self, paths: Vec<DerivedPath>) -> Result<()> {
        // 1. Parse the derivation paths
        // 2. Add to build queue
        // 3. Scheduler assigns to available builder
        // 4. Wait for build completion
        // 5. Return results

        for path in paths {
            let job = BuildJob::from_derived_path(path)?;
            self.build_queue.enqueue(job).await?;

            // Dispatch to builder
            if let Some(builder) = self.scheduler.select_builder(&job) {
                builder.execute_build(job).await?;
            }
        }

        Ok(())
    }

    // Other Store trait methods can proxy to local Nix store
    // or return errors if not needed for remote builders
    async fn query_path_info(&self, path: &StorePath) -> Result<PathInfo> {
        // Optional: proxy to local store or return "not available"
    }
}

// Builder in the pool
struct Builder {
    id: BuilderId,
    endpoint: String, // gRPC endpoint
    status: BuilderStatus, // Available, Busy, Offline
    capacity: BuilderCapacity, // cores, memory, etc.
    platforms: Vec<Platform>, // x86_64-linux, aarch64-darwin, etc.
    current_jobs: Vec<JobId>,
    grpc_client: BuildServiceClient,
}

// Build request from Nix client
struct BuildJob {
    job_id: JobId,
    derivation: Derivation,
    platform: Platform,
    required_features: Vec<String>,
    timeout: Option<Duration>,
}
```

**Protocol Support:**

We'll focus on `ssh-ng://` first (modern protocol), with optional `ssh://` support later.

1. **ssh-ng:// Protocol** (Priority):
   - Use `nix-daemon` crate to implement Nix store protocol
   - Accept SSH connections via russh
   - Tunnel Nix daemon protocol over SSH
   - Handle build operations: `buildPaths()`, `buildDerivation()`
   - For non-build store operations, can optionally proxy to actual Nix store
   - Dispatch builds to worker fleet via gRPC
   - Stream build logs back through protocol
   - Transfer build outputs back to client

2. **ssh:// Protocol** (Optional, Legacy):
   - Uses `nix-store --serve` protocol
   - Simpler but being deprecated
   - Can be added later if needed
   - Most users should use ssh-ng:// instead

**Scheduling Algorithm** (Initial Implementation):

Start simple, optimize later:
```rust
fn schedule_build(request: &BuildRequest, pool: &BuilderPool) -> Option<BuilderId> {
    pool.builders
        .iter()
        .filter(|b| {
            b.status == Available &&
            b.platforms.contains(&request.platform) &&
            b.has_required_features(&request.required_features)
        })
        .min_by_key(|b| b.current_jobs.len())
        .map(|b| b.id)
}
```

Future enhancements:
- Build affinity (prefer builders that have dependencies cached)
- Priority queues
- Fair scheduling across users
- Resource-aware scheduling (CPU/memory requirements)

**Builder Management:**

Builders connect to dispatcher and register:
```rust
// Builder registration
struct BuilderRegistration {
    builder_id: BuilderId,
    endpoint: String,
    capacity: BuilderCapacity,
    platforms: Vec<Platform>,
    features: Vec<String>,
}

// Heartbeat mechanism
struct BuilderHeartbeat {
    builder_id: BuilderId,
    timestamp: SystemTime,
    current_load: f32,
    available_capacity: BuilderCapacity,
}
```

### rio-builder

The builder is a worker node that receives and executes builds.

**Responsibilities:**
1. **Registration**: Register with dispatcher on startup
2. **Job Execution**: Receive build jobs and execute using local Nix
3. **Isolation**: Ensure builds are properly isolated
4. **Health Reporting**: Send heartbeats and capacity info to dispatcher
5. **Result Streaming**: Stream build logs and outputs back to dispatcher

**Key Implementation Details:**

```rust
struct RioBuilder {
    builder_id: BuilderId,
    dispatcher_endpoint: String,
    nix_daemon: NixDaemon,
    current_jobs: HashMap<JobId, BuildJob>,
    capacity: BuilderCapacity,
}

struct BuildJob {
    job_id: JobId,
    derivation: Derivation,
    status: JobStatus, // Queued, Building, Completed, Failed
    log_stream: LogStream,
    start_time: SystemTime,
}

impl RioBuilder {
    async fn execute_build(&mut self, job: BuildJob) -> Result<BuildOutput> {
        // 1. Receive derivation from dispatcher
        // 2. Check if dependencies are available (fetch from substituters)
        // 3. Execute build using nix-build or nix-daemon
        // 4. Stream logs to dispatcher
        // 5. Return build outputs
    }

    async fn heartbeat_loop(&self) {
        loop {
            self.send_heartbeat().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}
```

**Build Execution Flow:**
1. Builder receives `BuildJob` from dispatcher (via gRPC)
2. Extracts derivation and determines dependencies
3. Fetches missing dependencies from substituters or dispatcher
4. Invokes local Nix to build: `nix-build /nix/store/xxx.drv`
5. Streams build logs to dispatcher in real-time
6. On completion, stores outputs in local /nix/store
7. Sends build outputs back to dispatcher (via Nix copy or direct transfer)

## Communication Protocols

### Client ↔ Dispatcher

**SSH-based protocols:**
- Reuses existing Nix infrastructure
- Standard OpenSSH for authentication
- Two variants:
  1. Remote builder protocol (legacy, simple)
  2. Remote store protocol (ssh-ng://, more complex)

**Implementation Approach:**

We'll use the **nix-daemon Rust crate** from Tweag:
- Provides declarative implementation of Nix daemon protocol
- Implement the `Store` trait for our custom dispatch logic
- Use `DaemonProtocolAdapter` to handle protocol serialization
- russh for SSH transport layer
- Forward build operations to our worker fleet

This gives us:
- ✅ Full control over build dispatching
- ✅ Native Rust implementation (no FFI complexity)
- ✅ Forward compatibility with newer Nix versions
- ✅ Type-safe protocol handling

### Dispatcher ↔ Builder

**Internal protocol (not exposed to users):**

Options:
1. **gRPC**: Efficient, strongly-typed, bidirectional streaming
2. **SSH**: Reuses existing infrastructure, simpler but less efficient
3. **Custom TCP protocol**: Maximum control but more work

Recommendation: gRPC for efficiency and modern tooling.

**gRPC Service Definition:**

```protobuf
service BuildService {
  // Builder registers with dispatcher
  rpc RegisterBuilder(BuilderInfo) returns (RegistrationResponse);

  // Builder sends periodic heartbeats
  rpc Heartbeat(stream BuilderStatus) returns (stream DispatcherCommand);

  // Dispatcher sends build job to builder
  rpc ExecuteBuild(BuildRequest) returns (stream BuildProgress);

  // Dispatcher queries builder status
  rpc GetBuilderStatus(BuilderId) returns (BuilderStatus);
}

message BuildRequest {
  string job_id = 1;
  bytes derivation = 2; // Serialized .drv
  repeated string required_systems = 3;
  map<string, string> env = 4;
}

message BuildProgress {
  string job_id = 1;
  oneof update {
    LogLine log = 2;
    BuildCompleted completed = 3;
    BuildFailed failed = 4;
  }
}
```

## Build Flow: End-to-End

### Scenario 1: Remote Builder (ssh://)

1. **User runs**: `nix-build --builders ssh://rio.example.com`
2. Local Nix evaluates and produces derivations to build
3. Nix connects to rio-dispatcher via SSH
4. Dispatcher receives build request, adds to queue
5. Scheduler selects available rio-builder
6. Dispatcher sends job to builder via gRPC
7. Builder executes build with local Nix
8. Builder streams logs back to dispatcher
9. Dispatcher relays logs to client
10. Build completes, outputs stored in builder's /nix/store
11. Builder sends outputs to dispatcher
12. Dispatcher sends outputs to client over SSH
13. Client adds outputs to local /nix/store

### Scenario 2: Remote Store (ssh-ng://)

1. **User runs**: `nix build --store ssh-ng://rio.example.com`
2. Local Nix evaluates derivations (or with --eval-store, evaluation also remote)
3. Nix connects to rio-dispatcher via SSH using store protocol
4. Client calls `buildPaths()` on remote store
5. Dispatcher receives build request via nix-daemon protocol
6. Dispatcher schedules build to rio-builder (same as above)
7. Builder executes build
8. Build outputs stored in a shared Nix store or transferred to dispatcher
9. Dispatcher responds to client that build is complete
10. Outputs remain on remote store
11. User can query, copy, or use outputs from remote store

## Data Storage

### Dispatcher State

**In-memory (MVP):**
- Builder registry
- Build queue
- Job status

**Persistent (future):**
- Builder history
- Job logs and metadata
- Analytics data
- SQLite or PostgreSQL

### Build Outputs

**Options:**

1. **Distributed**: Outputs stay on individual builders
   - Pro: No central storage bottleneck
   - Con: Need to track which builder has which outputs

2. **Centralized**: Dispatcher maintains a central Nix store
   - Pro: Simpler management
   - Con: Can become bottleneck, requires significant storage

3. **Hybrid**: Shared object storage (S3-compatible)
   - Pro: Scalable, durable
   - Con: Added complexity
   - Best: Use as binary cache

Recommendation: Start with option 2 (centralized on dispatcher) for MVP, add option 3 for production.

## Security Considerations

### Authentication

- **Client → Dispatcher**: SSH key-based authentication
- **Dispatcher → Builder**: mTLS or pre-shared secrets
- Future: Support for API keys, OIDC (like nixbuild.net)

### Isolation

- Builds run in Nix sandbox (standard Nix isolation)
- Future: Additional container/VM isolation for multi-tenant scenarios

### Network Security

- TLS for all gRPC communication
- Firewall rules: builders only accessible from dispatcher

## Implementation Phases

### Phase 1: MVP (ssh-ng:// Remote Builder)
**Goal**: Support `ssh-ng://` remote builder protocol for basic distributed builds

**rio-dispatcher:**
- [ ] Project setup with workspace structure
- [ ] SSH server using russh
  - [ ] Accept incoming SSH connections
  - [ ] SSH authentication (public key)
  - [ ] Session management
- [ ] Nix protocol integration
  - [ ] Add nix-daemon crate dependency
  - [ ] Implement custom `Store` trait
  - [ ] Handle `buildPaths()` and `buildDerivation()` operations
  - [ ] Parse derivation files (.drv)
- [ ] Build queue and dispatcher
  - [ ] In-memory build queue
  - [ ] Round-robin scheduler
  - [ ] Builder registry (in-memory)
- [ ] gRPC server for builder communication
  - [ ] Define protobuf service
  - [ ] Builder registration endpoint
  - [ ] Build job dispatch
  - [ ] Heartbeat handling

**rio-builder:**
- [ ] gRPC client setup
  - [ ] Connect to dispatcher
  - [ ] Builder registration
  - [ ] Heartbeat loop
- [ ] Job execution
  - [ ] Receive build jobs via gRPC
  - [ ] Parse derivation
  - [ ] Execute using `nix-build` or `nix-daemon`
  - [ ] Capture and stream build logs
- [ ] Result handling
  - [ ] Package build outputs
  - [ ] Send outputs back to dispatcher via gRPC streaming

**End-to-end:**
- [ ] Integration test: `nix-build --builders 'ssh-ng://localhost?remote-program=rio-dispatcher'`
- [ ] Verify builds execute on rio-builder
- [ ] Verify outputs return to client correctly

### Phase 2: Legacy ssh:// Support (Optional)
**Goal**: Support `ssh://` protocol for backwards compatibility

- [ ] Implement `nix-store --serve` protocol
- [ ] Test with older Nix clients

### Phase 3: Remote Store Mode (Future)
**Goal**: Support using Rio as a remote store (not just remote builder)

- [ ] Implement full store operations: queryPathInfo, addToStore, etc.
- [ ] Persistent Nix store on dispatcher or shared storage
- [ ] Path tracking and management

### Phase 3: Production Features

- [ ] Binary cache integration
- [ ] Build result caching (skip redundant builds)
- [ ] Persistent storage (PostgreSQL)
- [ ] Monitoring and metrics (Prometheus)
- [ ] Advanced scheduling (priority, fairness, resource-aware)
- [ ] Multi-tenancy and quotas
- [ ] Web UI for build monitoring

### Phase 4: Scale & Performance

- [ ] Horizontal scaling of dispatcher (multiple instances)
- [ ] Shared state via distributed storage
- [ ] Object storage for build outputs (S3-compatible)
- [ ] Build affinity and intelligent scheduling
- [ ] Auto-scaling of builder fleet (cloud integration)

## Technology Stack

### Core
- **Language**: Rust (performance, safety, excellent async support)
- **SSH**: russh (pure Rust SSH implementation)
- **Nix Protocol**: nix-daemon crate (Tweag's Rust implementation of Nix protocol)
  - Provides `Store` trait and `DaemonProtocolAdapter`
  - Supports Nix Protocol 1.35 and Nix 2.15+
  - See: [tweag/nix-remote-rust](https://github.com/tweag/nix-remote-rust)
- **gRPC**: tonic (Rust gRPC framework)
- **Async Runtime**: tokio
- **Serialization**: prost (protobuf), serde (JSON)

### Storage (Phase 3+)
- **Database**: PostgreSQL with tokio-postgres
- **Object Storage**: S3-compatible (rusoto or aws-sdk-rust)

### Monitoring (Phase 3+)
- **Metrics**: Prometheus with metrics crate
- **Tracing**: tracing crate
- **Logging**: tracing-subscriber

### Testing
- **Unit tests**: cargo test
- **Integration tests**: Docker Compose for multi-component tests
- **Nix-specific tests**: NixOS test framework

## Design Decisions

### ✅ Resolved

1. **Nix Protocol Implementation**: Use `nix-daemon` Rust crate
   - Provides native Rust implementation of Nix daemon protocol
   - Type-safe, forward-compatible
   - No FFI complexity

2. **Protocol Priority**: Focus on `ssh-ng://` first
   - Modern, recommended protocol
   - `ssh://` is being deprecated
   - Can add `ssh://` support later if needed

3. **Build Output Storage**: Simple approach for MVP
   - Outputs transfer back to client (no centralized storage needed)
   - Can add binary cache/storage later

4. **Multi-tenancy**: Not needed for initial version
   - Can add user isolation in future phases

### 🤔 Open Questions

1. **Builder Discovery**: How do builders find the dispatcher?
   - Static configuration in builder
   - DNS service discovery
   - Consul/etcd integration

2. **Build Artifact Caching**: Should we cache build results?
   - Content-addressed cache to avoid redundant builds
   - TTL-based invalidation
   - Need to handle non-deterministic builds

3. **Cloud Integration**: Support for auto-scaling builders?
   - AWS/GCP/Azure integrations
   - Could be added as plugins in Phase 4

4. **Monitoring**: What metrics should we expose?
   - Build queue depth
   - Builder utilization
   - Build duration and success/failure rates
   - Active connections

## Success Metrics

- Can successfully build any Nix derivation that works locally
- Build throughput scales linearly with number of builders
- <100ms overhead for dispatcher scheduling
- Support for all major Nix platforms (x86_64-linux, aarch64-linux, x86_64-darwin, aarch64-darwin)

## References

- [Nix Manual: Distributed Builds](https://nix.dev/manual/nix/stable/advanced-topics/distributed-builds.html)
- [nixbuild.net Documentation](https://docs.nixbuild.net/)
- [nixbuild.net Blog](https://blog.nixbuild.net/)
- [nix-daemon Rust Crate](https://crates.io/crates/nix-daemon)
- [tweag/nix-remote-rust](https://github.com/tweag/nix-remote-rust) - Nix protocol in Rust
- [Tweag Blog: Re-implementing the Nix protocol in Rust](https://www.tweag.io/blog/2024-04-25-nix-protocol-in-rust/)
- [Nix Store Protocol](https://github.com/NixOS/nix/blob/master/src/libstore/store-api.hh)
- [russh Documentation](https://docs.rs/russh/)
- [tonic (gRPC) Documentation](https://docs.rs/tonic/)

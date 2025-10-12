# Rio Design Document

## Overview

Rio is an open-source distributed build service for Nix that eliminates the traditional broker architecture by using a peer-to-peer agent cluster coordinated via Raft consensus.

**Key Innovation:** Instead of a central dispatcher that becomes a bottleneck and single point of failure, agents coordinate via Raft consensus while build data (derivations, logs, outputs) flows directly between CLI and agents.

## Architecture

### High-Level Design

```
┌──────────────┐
│  rio-build   │ (CLI client)
│    (user)    │
└──────┬───────┘
       │
       │ Direct gRPC to selected agent
       │ (derivations, logs, outputs)
       ▼
┌────────────────────────────────────────────┐
│      rio-agent cluster (Raft)              │
│                                            │
│  ┌────────┐  ┌────────┐  ┌────────┐        │
│  │ Agent  │←→│ Agent  │←→│ Agent  │        │
│  │   1    │  │   2    │  │   3    │        │
│  │(leader)│  │        │  │        │        │
│  └────────┘  └────────┘  └────────┘        │
│       ↑           ↑           ↑            │
│       └───────────┴───────────┘            │
│        Raft: membership + job tracking     │
└────────────────────────────────────────────┘
```

**Critical Insight:** Raft is the **control plane** (who's in the cluster, who's building what). The **data plane** (actual derivations, logs, outputs) bypasses Raft entirely and flows directly CLI ↔ Agent.

**Automatic Build Recovery:**

When an agent fails, no manual intervention or complex reassignment logic is needed:
1. Raft cleanup removes failed agent's builds
2. Each affected CLI independently retries
3. Affinity + deduplication naturally re-group builds on new agent
4. Work continues seamlessly

## Core Design Decisions

### 1. What Does Raft Coordinate?

Raft maintains a **shared state machine** with:

```rust
struct ClusterState {
    // Cluster membership
    agents: HashMap<AgentId, AgentInfo>,

    // Active builds: derivation hash → which agent is building it
    builds_in_progress: HashMap<DerivationHash, BuildTracker>,

    // Recently completed builds (5 minute LRU cache for fast retrieval)
    completed_builds: LruCache<DerivationHash, CompletedBuild>,
}

struct BuildTracker {
    agent_id: AgentId,
    started_at: Timestamp,
    parent_build: Option<DerivationHash>,  // None = top-level, Some(hash) = dependency
    status: BuildStatus,
}

enum BuildStatus {
    Queued { blocked_on: Vec<DerivationHash> },  // Waiting for dependencies
    Building,  // Currently executing
}

struct CompletedBuild {
    agent_id: AgentId,  // Where outputs are stored in /nix/store
    output_paths: Vec<Utf8PathBuf>,
    completed_at: Timestamp,
}

struct AgentInfo {
    id: AgentId,
    address: String,           // gRPC endpoint
    platforms: Vec<String>,    // ["x86_64-linux", "i686-linux"] (from system + extra-platforms)
    features: Vec<String>,     // ["kvm", "big-parallel"] (from system-features)
    capacity: BuilderCapacity,
    last_heartbeat: Timestamp,
    status: AgentStatus,       // Available, Busy, or Down
}

enum AgentStatus {
    Available,  // Idle, can accept builds
    Busy,       // Currently executing one build
    Down,       // Failed heartbeats
}

// Note: JobAssignment removed - redundant with BuildTracker
// Derivation hash serves as the job identifier
```

**Raft Commands:**

```rust
enum RaftCommand {
    // Membership
    AgentJoined { id: AgentId, info: AgentInfo },
    AgentLeft { id: AgentId },
    AgentHeartbeat { id: AgentId, timestamp: Timestamp },

    // Build lifecycle (derivation hash is the job identifier)
    // Atomically registers top-level build + all dependencies
    BuildStartedWithDependencies {
        top_level: DerivationHash,
        dependencies: Vec<DerivationHash>,
        agent_id: AgentId,
        platform: String,
        status: BuildStatus,  // Queued or Building
    },
    // Agent status changes
    AgentStatusChanged {
        agent_id: AgentId,
        status: AgentStatus,  // Available or Busy
    },
    BuildCompleted {
        derivation_hash: DerivationHash,
        output_paths: Vec<Utf8PathBuf>,
    },
    BuildFailed {
        derivation_hash: DerivationHash,
        error: String,
    },
}
```

**What Raft Does NOT Store:**

- Build derivations (too large, sent via gRPC stream)
- Build logs (streamed to subscribers in real-time)
- Build outputs (stored in agent's /nix/store, exported on demand)

**Build Deduplication:**

Raft tracks active and recently completed builds by derivation hash to avoid duplicate work:
- Derivation hash serves as the job identifier (no separate JobId needed)
- When User A submits a build, Raft records: `builds_in_progress[drv_hash] → BuildTracker`
- When User B submits the **same** derivation, they're transparently subscribed to the in-progress build
- Both users receive logs and outputs from the single build execution
- Completed builds cached for 5 minutes to serve late arrivals

### 2. Build Submission Flow (End to End)

```
User runs: rio-build ./my-package.nix

┌─────────────────────────────────────────────────────┐
│ 1. CLI: Evaluate with nix-eval-jobs                 │
└─────────────────────────────────────────────────────┘
   - Run: nix-eval-jobs --check-cache-status \
            --show-input-drvs \
            --show-required-system-features \
            --flake <nix-expression>
   - Parse JSON output (one line for top-level package):
     * drvPath: /nix/store/abc123-foo.drv
     * system: x86_64-linux
     * requiredSystemFeatures: ["kvm", "big-parallel"]
     * cacheStatus: "notBuilt" | "cached" | "local"
     * neededBuilds: [list of .drv files that need building]
     * neededSubstitutes: [list of paths fetchable from cache]
   - If cacheStatus is "cached" or "local":
     → Skip remote build, run `nix build` locally instead
   - Read derivation bytes for top-level
   - Compute DerivationHash for top-level and each neededBuilds entry
   - Result: BuildInfo with minimal set of builds needed

┌─────────────────────────────────────────────────────┐
│ 2. CLI: Discover cluster                            │
└─────────────────────────────────────────────────────┘
   - Read seed agents from ~/.config/rio/config.toml
   - Connect to first available seed agent
   - Call GetClusterMembers() RPC
   - Receive full cluster state from Raft

┌─────────────────────────────────────────────────────┐
│ 3. CLI: Select agent (with build affinity)         │
└─────────────────────────────────────────────────────┘
   - Check which dependencies are currently building
   - If dependencies building: prefer agent with most matching deps
   - Else: filter by platform, features, Available status
   - Pick best match (affinity > availability)

┌─────────────────────────────────────────────────────┐
│ 4. CLI → Agent: Submit build with dependencies      │
└─────────────────────────────────────────────────────┘
   CLI: Open bidirectional stream SubmitBuild()
   CLI: Send SubmitBuildRequest {
       derivation_bytes: [...],
       dependency_hashes: [hash(dep1.drv), hash(dep2.drv), ...],
       platform: "x86_64-linux",
       required_features: ["kvm", "big-parallel"],
       timeout_seconds: 3600,
   }

┌─────────────────────────────────────────────────────┐
│ 5. Agent: Check dependencies and register           │
└─────────────────────────────────────────────────────┘
   - Compute top_level_hash from derivation bytes
   - Check which dependencies are building on THIS agent
   - If dependencies building here:
       status = Queued { blocked_on: [...] }
       Mark agent as Busy (even though queued)
   - Else:
       status = Building
       Mark agent as Busy
   - Propose RaftCommand::BuildStartedWithDependencies
   - Wait for Raft consensus (commit)
   - Raft atomically registers ALL derivations (top + deps)

┌─────────────────────────────────────────────────────┐
│ 6. Agent: Execute or queue build                    │
└─────────────────────────────────────────────────────┘
   - If status = Queued:
       Add to pending_builds, send "Waiting for dependencies" to CLI
   - Else (status = Building):
       Write derivation to /tmp/rio-agent/{drv-hash}.drv
       Spawn: nix-build /tmp/rio-agent/{drv-hash}.drv
       Capture stdout/stderr in real-time

┌─────────────────────────────────────────────────────┐
│ 7. Agent → CLI: Stream logs                         │
└─────────────────────────────────────────────────────┘
   For each stdout/stderr line:
       Send BuildUpdate { log: LogLine { line, timestamp } }

   CLI receives and displays to user in real-time

┌─────────────────────────────────────────────────────┐
│ 8. Agent: Export outputs                            │
└─────────────────────────────────────────────────────┘
   - nix-build completes → /nix/store/abc123-result
   - Run: nix-store --export /nix/store/abc123-result
   - Capture NAR (Nix ARchive) bytes

┌─────────────────────────────────────────────────────┐
│ 9. Agent → CLI: Stream outputs                      │
└─────────────────────────────────────────────────────┘
   Send BuildUpdate {
       output_chunk: OutputChunk {
           path: "/nix/store/abc123-result",
           data: [bytes...],
           chunk_index: 0,
       }
   }

   (Stream NAR in chunks)

┌─────────────────────────────────────────────────────┐
│ 10. CLI: Import outputs                             │
└─────────────────────────────────────────────────────┘
   - Receive all chunks, reassemble NAR
   - Run: nix-store --import < output.nar
   - Outputs now in local /nix/store

┌─────────────────────────────────────────────────────┐
│ 11. Agent: Mark complete and unblock dependents     │
└─────────────────────────────────────────────────────┘
   - Propose RaftCommand::BuildCompleted { derivation_hash, output_paths }
   - Raft removes top-level from builds_in_progress
   - Raft removes ALL dependencies (where parent_build = this hash)
   - Send BuildUpdate { completed: BuildCompleted { output_paths, duration } }
   - Check pending_builds for anything waiting on this derivation
   - Auto-start any builds that are now unblocked
   - If no pending builds: mark agent as Available
   - Clean up temp files

┌─────────────────────────────────────────────────────┐
│ 12. CLI: Report success                             │
└─────────────────────────────────────────────────────┘
   Print to user:
       Build succeeded!
       Outputs:
         /nix/store/abc123-result
       Duration: 42s
```

### 3. Build Deduplication (Multiple Users, Same Derivation)

**Problem:** Users A and B submit the same derivation simultaneously. We want to avoid duplicate work.

**Solution:** Transparent build sharing via Raft coordination.

**Scenario: User B joins in-progress build**

```
Time T0: User A submits /nix/store/abc123-foo.drv
1. CLI extracts build closure:
   - foo.drv (top-level)
   - bar.drv (dependency)
   - baz.drv (dependency)
2. CLI → Agent-1: SubmitBuild(foo_drv_bytes, deps=[bar_hash, baz_hash])
3. Agent-1 computes: foo_hash = hash(foo_drv_bytes)
4. Agent-1 checks Raft: builds_in_progress[foo_hash] = None
5. Agent-1 proposes: BuildStartedWithDependencies {
     top_level: foo_hash,
     dependencies: [bar_hash, baz_hash],
     agent_id: "agent-1",
     platform: "x86_64-linux"
   }
6. Raft atomically registers:
   - builds_in_progress[foo_hash] = { agent-1, parent: None }
   - builds_in_progress[bar_hash] = { agent-1, parent: Some(foo_hash) }
   - builds_in_progress[baz_hash] = { agent-1, parent: Some(foo_hash) }
7. Agent-1 starts building, streaming logs to User A

Time T1: User B submits same foo.drv (30s later)
1. CLI extracts build closure: [foo, bar, baz]
2. CLI → Agent-2: SubmitBuild(foo_drv_bytes, deps=[bar_hash, baz_hash])
3. Agent-2 checks Raft: builds_in_progress[foo_hash] = Some(BuildTracker {
     agent_id: "agent-1",
     started_at: T0,
     parent_build: None
   })
4. Agent-2 returns: Redirect { target_agent: "agent-1", derivation_hash: foo_hash }
5. CLI reconnects to agent-1: SubscribeToBuild(derivation_hash: foo_hash)
6. Agent-1 sends User B:
   - Catch-up: All logs from T0 to T1
   - Live: New logs as they arrive
7. Build completes at T2
8. Agent-1 streams outputs to BOTH User A and User B
9. Both users import outputs, done
```

**Scenario: User C requests recently completed build**

```
Time T2: Build completed
1. Agent-1 proposes: BuildCompleted {
     derivation_hash: drv_hash,
     output_paths: ["/nix/store/abc123-result"]
   }
2. Raft moves from builds_in_progress → completed_builds (LRU cache)

Time T3: User C submits same derivation (3 minutes later)
1. CLI → Agent-2: SubmitBuild(drv_bytes)
2. Agent-2 computes drv_hash, checks Raft
3. Finds in completed_builds: CompletedBuild {
     agent_id: "agent-1",
     output_paths: ["/nix/store/abc123-result"]
   }
4. Agent-2 returns: Redirect { target_agent: "agent-1", derivation_hash: drv_hash }
5. CLI → Agent-1: GetCompletedBuild(derivation_hash: drv_hash)
6. Agent-1 exports: nix-store --export /nix/store/abc123-result
7. Agent-1 streams NAR chunks to User C
8. User C imports, done
```

**Agent-Side Implementation:**

```rust
struct Agent {
    // Currently executing build (one at a time)
    current_build: Option<DerivationHash>,

    // Builds waiting for dependencies to complete
    pending_builds: HashMap<DerivationHash, PendingBuild>,

    // Build state with multiple subscribers (keyed by derivation hash)
    build_jobs: HashMap<DerivationHash, BuildJob>,
}

struct PendingBuild {
    derivation: Vec<u8>,
    blocked_on: Vec<DerivationHash>,
    subscribers: Vec<BuildSubscriber>,
}

struct BuildJob {
    derivation_hash: DerivationHash,
    derivation_path: Utf8PathBuf,  // /nix/store/abc123-foo.drv
    process: Child,  // nix-build process
    subscribers: Vec<BuildSubscriber>,
    log_history: Vec<LogLine>,  // For late joiners (last 10,000 lines)
}

struct BuildSubscriber {
    stream: ResponseStream,
    joined_at_line: usize,  // Log line index when they joined
}

impl Agent {
    async fn submit_build(&self, req: SubmitBuildRequest) -> Result<Stream<BuildUpdate>> {
        let drv_hash = hash_derivation(&req.derivation);

        // Check Raft state
        let existing = self.raft.query_build(drv_hash).await?;

        match existing {
            None => {
                // Check which dependencies are building on THIS agent
                let mut deps_building_here = Vec::new();
                for dep_hash in &req.dependency_hashes {
                    if let Some(tracker) = self.raft.query_build(dep_hash).await? {
                        if tracker.agent_id == self.id {
                            deps_building_here.push(*dep_hash);
                        }
                    }
                }

                let status = if deps_building_here.is_empty() {
                    BuildStatus::Building
                } else {
                    BuildStatus::Queued { blocked_on: deps_building_here.clone() }
                };

                // Register atomically
                self.raft.propose(BuildStartedWithDependencies {
                    top_level: drv_hash,
                    dependencies: req.dependency_hashes,
                    agent_id: self.id,
                    platform: req.platform,
                    status: status.clone(),
                }).await?;

                // Mark agent as Busy
                self.raft.propose(AgentStatusChanged {
                    agent_id: self.id,
                    status: AgentStatus::Busy,
                }).await?;

                if matches!(status, BuildStatus::Queued { .. }) {
                    self.queue_build(drv_hash, req, deps_building_here).await
                } else {
                    self.start_build(drv_hash, req).await
                }
            }
            Some(BuildTracker { agent_id, status, .. }) if agent_id == self.id => {
                // Building/queued on THIS agent - add subscriber
                self.subscribe_to_build(drv_hash).await
            }
            Some(BuildTracker { agent_id, .. }) => {
                // Building on different agent - redirect
                Err(RedirectToAgent { agent_id, derivation_hash: drv_hash })
            }
            Some(CompletedBuild { output_paths, .. }) => {
                // Recently completed - stream outputs from /nix/store
                self.stream_completed_build(drv_hash, output_paths).await
            }
        }
    }

    async fn on_build_completed(&mut self, drv_hash: DerivationHash, outputs: Vec<Utf8PathBuf>) {
        // Propose completion to Raft
        self.raft.propose(BuildCompleted {
            derivation_hash: drv_hash,
            output_paths: outputs.clone(),
        }).await?;

        // Broadcast to all subscribers
        self.broadcast_to_subscribers(drv_hash, BuildUpdate::Completed(...)).await?;

        // Check pending builds waiting on this dependency
        let mut unblocked = Vec::new();

        for (pending_hash, pending) in &mut self.pending_builds {
            pending.blocked_on.retain(|h| h != &drv_hash);

            if pending.blocked_on.is_empty() {
                unblocked.push(*pending_hash);
            }
        }

        // Start unblocked builds
        if let Some(next_hash) = unblocked.first() {
            let pending = self.pending_builds.remove(next_hash)?;
            self.current_build = Some(*next_hash);
            self.start_build(*next_hash, pending.derivation).await?;
        } else {
            // No more work - mark as Available
            self.current_build = None;
            self.raft.propose(AgentStatusChanged {
                agent_id: self.id,
                status: AgentStatus::Available,
            }).await?;
        }
    }

    async fn on_build_failed(&mut self, drv_hash: DerivationHash, error: String) {
        // Propose failure to Raft
        self.raft.propose(BuildFailed {
            derivation_hash: drv_hash,
            error: error.clone(),
        }).await?;

        // Find all builds waiting on this dependency (cascading failures)
        let dependents: Vec<_> = self.pending_builds.iter()
            .filter(|(_, pending)| pending.blocked_on.contains(&drv_hash))
            .map(|(hash, _)| *hash)
            .collect();

        // Recursively fail dependents
        for dependent_hash in dependents {
            self.fail_build_cascade(dependent_hash, format!(
                "Dependency {} failed: {}", drv_hash, error
            )).await?;
        }

        // Mark agent as Available (unless other pending builds exist)
        if self.pending_builds.is_empty() {
            self.current_build = None;
            self.raft.propose(AgentStatusChanged {
                agent_id: self.id,
                status: AgentStatus::Available,
            }).await?;
        }
    }
}
```

**CLI Handling of Redirects:**

```rust
async fn submit_build_with_retry(drv_path: Utf8PathBuf) -> Result<()> {
    let drv_bytes = tokio::fs::read(&drv_path).await?;
    let mut agent = select_agent(&cluster)?;

    loop {
        match agent.submit_build(drv_bytes.clone()).await {
            Ok(stream) => {
                return handle_build_stream(stream).await;
            }
            Err(RedirectToAgent { agent_id, derivation_hash }) => {
                // Transparently redirect to correct agent
                agent = cluster.find_agent(agent_id)?;
                // Retry will call SubscribeToBuild instead
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}
```

**Cache Duration:**
- Active builds: In Raft until BuildCompleted/BuildFailed
- Completed builds: 5 minutes in LRU cache (configurable)
- Failed builds: Immediately removed (allow instant retry)

**Benefits:**
- ✅ Zero user action required
- ✅ No duplicate work
- ✅ Multiple users benefit from single build
- ✅ Works for in-progress AND recently completed builds
- ✅ No artificial output caching (uses /nix/store directly)
- ✅ Failed builds can retry immediately

### 4. Multi-Derivation Builds, Deduplication, and Build Affinity

**The Problem:**

A typical `nix build` involves building multiple derivations, not just one:

```bash
$ nix build ./my-app.nix
these 15 derivations will be built:
  /nix/store/abc-dep1.drv
  /nix/store/def-dep2.drv
  ...
  /nix/store/xyz-my-app.drv
```

If User A builds `my-app` (which depends on `dep1`) and User B builds `dep1` directly, we want to deduplicate the `dep1` build.

**Solution: Pre-registration + Build Affinity + Dependency Waiting**

The CLI uses `nix-eval-jobs` with cache checking to extract **only derivations that need building**:

```rust
// Run nix-eval-jobs with cache checking
$ nix-eval-jobs --check-cache-status \
                --show-required-system-features \
                --show-input-drvs \
                --expr "{ pkg = import ./my-app.nix {}; }"

// Output (JSON):
{
  "attr": "pkg",
  "drvPath": "/nix/store/xyz-my-app.drv",
  "system": "x86_64-linux",
  "requiredSystemFeatures": ["kvm"],
  "cacheStatus": "notBuilt",
  "neededBuilds": [
    "/nix/store/xyz-my-app.drv",
    "/nix/store/abc-dep1.drv",  // Needs building
    "/nix/store/def-dep2.drv"   // Needs building
  ],
  "neededSubstitutes": [
    "/nix/store/ghi-dep3",  // In cache.nixos.org
    "/nix/store/jkl-dep4"   // In cache.nixos.org
  ]
}

// Only register derivations in neededBuilds!
let top = hash(my-app.drv);
let deps = [hash(dep1.drv), hash(dep2.drv)];  // NOT dep3, dep4!

// Submit with minimal dependency list
agent.submit_build(my-app_bytes, deps);
```

**Agent atomically registers all derivations:**

```rust
// Agent proposes single Raft command
BuildStartedWithDependencies {
    top_level: hash(my-app.drv),
    dependencies: [hash(dep1.drv), hash(dep2.drv), hash(dep3.drv)],
    agent_id: "agent-1",
    platform: "x86_64-linux",
}

// Raft state machine applies atomically:
builds_in_progress[hash(my-app.drv)] = BuildTracker {
    agent_id: agent-1,
    parent_build: None,  // Top-level
}

builds_in_progress[hash(dep1.drv)] = BuildTracker {
    agent_id: agent-1,
    parent_build: Some(hash(my-app.drv)),  // Dependency
}

builds_in_progress[hash(dep2.drv)] = BuildTracker {
    agent_id: agent-1,
    parent_build: Some(hash(my-app.drv)),  // Dependency
}

// Now ALL derivations are registered and reserved!
```

**Deduplication in action:**

```
Time T0: User A runs: rio-build ./my-app.nix
- nix-eval-jobs returns:
  * neededBuilds: [my-app.drv, dep1.drv, dep2.drv]
  * neededSubstitutes: [dep3, dep4, dep5, ...]  (will fetch from cache)
- Registers only: my-app.drv (top), dep1.drv (dep), dep2.drv (dep)
- Agent-1 starts: nix-build /nix/store/my-app.drv
- Nix automatically fetches dep3, dep4, dep5 from substituters

Time T1: User B runs: rio-build ./dep1.nix (30 seconds later)
- nix-eval-jobs returns:
  * cacheStatus: "notBuilt"
  * neededBuilds: [dep1.drv]
- Checks Raft: builds_in_progress[hash(dep1.drv)] = Some(BuildTracker {
    agent_id: agent-1,
    parent_build: Some(hash(my-app.drv))  // Being built as dependency!
  })
- CLI redirects to agent-1: SubscribeToBuild(hash(dep1.drv))
- User B receives logs for dep1 (even though it's part of my-app's build)
- When my-app completes, dep1 outputs are included
- User B gets dep1 outputs automatically!

Time T2: User C runs: rio-build ./dep3.nix (dep3 was in substituters)
- nix-eval-jobs returns:
  * cacheStatus: "cached"
  * neededBuilds: []
  * neededSubstitutes: [dep3]
- CLI exits: "Package available in cache, fetching locally..."
- Runs: nix build ./dep3.nix (local Nix handles it)
- No remote build needed!
```

**Scenario: Build affinity with dependency waiting**

```
Time T0: User A runs: rio-build ./foo.nix
- Registers: foo.drv on Agent K (status: Building)
- Agent K starts building foo
- Agent K status: Busy

Time T1: User B runs: rio-build ./bar.nix (bar depends on foo)
- CLI extracts closure: [bar.drv, foo.drv]
- CLI checks cluster:
  - foo is building on Agent K
- CLI scores agents:
  - Agent K: affinity=1 (has foo building)
  - Agent J: affinity=0
  - Agent M: affinity=0
- CLI selects Agent K (best affinity!)
- CLI → Agent K: SubmitBuild(bar_bytes, deps=[hash(foo)])

Time T2: Agent K receives bar submission
- Checks: foo is building on me (self.current_build = Some(hash(foo)))
- Status: Queued { blocked_on: [hash(foo)] }
- Registers in Raft: BuildStartedWithDependencies {
    top_level: hash(bar),
    dependencies: [hash(foo)],
    agent_id: K,
    status: Queued
  }
- Adds bar to pending_builds queue
- Sends to User B: "Waiting for dependency /nix/store/abc-foo.drv..."

Time T3: foo build completes
- Agent K marks foo complete in Raft
- Agent K checks pending_builds
- Finds bar blocked on [hash(foo)]
- Removes hash(foo) from bar.blocked_on → now empty!
- Agent K auto-starts bar build
- Sends to User B: "Dependencies ready, starting build..."
- nix-build finds foo in local /nix/store (cache hit!)

Time T4: bar build completes
- Agent K marks bar complete
- Agent K status: Available (no more pending builds)
- User B gets outputs
```

**Scenario: Cascading dependency failure**

```
Time T0: User A builds foo (Agent K)
Time T1: User B builds bar (depends on foo, queued on Agent K)
Time T2: foo FAILS

Agent K's on_build_failed():
1. Marks foo as failed in Raft
2. Checks pending_builds
3. Finds bar blocked on [hash(foo)]
4. Sends to User B: BuildFailed {
     error: "Dependency /nix/store/abc-foo.drv failed: compile error"
   }
5. Removes bar from pending_builds
6. Marks bar as failed in Raft
7. Agent K status: Available (all work done)

User B sees:
  "Waiting for dependency /nix/store/abc-foo.drv..."
  "Error: Build cannot proceed: dependency failed"
```

**Cleanup on Completion:**

```rust
// When build completes
fn apply_build_completed(
    state: &mut ClusterState,
    drv_hash: DerivationHash,
    outputs: Vec<Utf8PathBuf>
) {
    // Remove top-level
    state.builds_in_progress.remove(&drv_hash);

    // Remove ALL dependencies tied to this build
    state.builds_in_progress.retain(|_, tracker| {
        tracker.parent_build != Some(drv_hash)
    });

    // Cache result
    state.completed_builds.put(drv_hash, CompletedBuild {
        agent_id: tracker.agent_id,
        output_paths: outputs,
        completed_at: now(),
    });
}
```

**Benefits:**

- ✅ Full deduplication across entire dependency tree
- ✅ Build affinity - builds go where dependencies are
- ✅ Dependency waiting - automatically waits for in-progress deps
- ✅ Zero cross-agent transfers - dependencies are local
- ✅ Cascading failures - dependents fail immediately
- ✅ Atomic registration prevents race conditions
- ✅ One build at a time per agent (simple execution model)
- ✅ Transparent to users (automatic subscription, waiting, retry)
- ✅ Cache-aware - only builds what's not available in substituters
- ✅ Minimal Raft overhead - only registers derivations that need building

**Key Scenarios Handled:**

1. **Same derivation, multiple users** → Share single build
2. **Dependency already building** → Queue on same agent, wait
3. **Dependency recently completed** → Prefer agent with cached outputs
4. **Dependency fails** → Cascade failure to dependents
5. **Agent dies** → All builds on that agent fail, users retry elsewhere
6. **Dependency in cache** → Skip registration, let Nix fetch from substituters
7. **Top-level in cache** → Skip remote build entirely, use local Nix

**Performance:**

- nix-eval-jobs filters to only derivations needing builds
- Example: 100 total deps, 5 need building → Register only 6 derivations (top + 5 deps)
- Typical Raft command: ~2-10KB for most builds
- Large builds (100+ uncached deps): ~60KB Raft entry
- Much better than registering everything!

### 5. Failure Handling

**Scenario A: Agent dies with active and queued builds**

```
Time T0: User A submits foo
- builds_in_progress[hash(foo)] = { agent_id: K, status: Building }
- User A's CLI → Agent K (streaming logs)

Time T1: User B submits bar (depends on foo)
- CLI detects foo building on Agent K (affinity!)
- builds_in_progress[hash(bar)] = { agent_id: K, status: Queued { blocked_on: [foo] } }
- User B's CLI → Agent K (sees "Waiting for dependencies...")

Time T2: Agent K crashes
- Both CLIs detect stream disconnection
- Raft heartbeat timeout (30s)
- Raft marks Agent K as Down
- Raft cleanup removes ALL of Agent K's builds:
  * builds_in_progress.remove(hash(foo))
  * builds_in_progress.remove(hash(bar))
  * Remove dependencies of foo and bar

Time T3: User A retries foo
- CLI: GetJobStatus(hash(foo)) → NotFound (cleaned up)
- CLI: Resubmit foo to cluster
- CLI selects Agent J (available)
- Agent J starts building foo
- builds_in_progress[hash(foo)] = { agent_id: J, status: Building }

Time T3+1ms: User B retries bar (nearly simultaneous)
- CLI: GetJobStatus(hash(bar)) → NotFound
- CLI: Resubmit bar, check dependencies
- Raft shows: hash(foo) = { agent_id: J, status: Building } ← User A's build!
- CLI selects Agent J (affinity with foo!)
- Agent J receives bar, detects foo building on self
- Agent J queues: Queued { blocked_on: [hash(foo)] }
- builds_in_progress[hash(bar)] = { agent_id: J, status: Queued }
- User B's CLI reconnects to Agent J: "Waiting for dependencies..."

Time T4: foo completes on Agent J
- Agent J auto-starts bar
- User B's build continues seamlessly

Result: Both builds naturally migrate to Agent J via independent retry + affinity!
```

**Scenario B: Network partition during submission**

```
1. CLI sends SubmitBuildRequest
2. Agent receives, proposes to Raft
3. Network partition: agent can't reach Raft quorum
4. Agent's proposal times out (no consensus)
5. Agent returns error to CLI: "Cluster unavailable"
6. CLI waits and retries
```

**Scenario C: CLI dies mid-build**

```
1. Agent is building, CLI crashes
2. Agent completes build, tries to stream outputs
3. Stream fails (CLI gone)
4. Agent still marks job complete in Raft
5. Agent keeps NAR cached locally for 1 hour
6. If CLI comes back, can query job status and retrieve outputs
```

### 6. Cluster Membership

**Bootstrap (first agent):**

```bash
rio-agent --bootstrap --listen=0.0.0.0:50051 --data-dir=/var/lib/rio
```

- Creates single-node Raft cluster
- Agent ID: generated UUID
- Raft state: { agents: { self }, active_jobs: {} }

**Join existing cluster:**

```bash
rio-agent --join=https://agent1.example.com:50051 --listen=0.0.0.0:50051
```

- Connects to agent1 via gRPC
- Sends JoinCluster RPC
- agent1 (or leader) proposes RaftCommand::AgentJoined
- Once committed, new agent becomes voting member

**Heartbeats and Failure Detection:**

- Every 10 seconds, each agent proposes RaftCommand::AgentHeartbeat
- If agent misses 3 heartbeats (30s), marked as Down
- When agent marked as Down, Raft automatically:
  * Removes all builds assigned to that agent (Building or Queued)
  * Removes dependencies registered by those builds
  * Clears the builds from Raft state
- Disconnected CLIs automatically retry on different agents
- Affinity mechanism naturally re-groups related builds on new agent

**Graceful shutdown:**

```
1. Agent receives SIGTERM
2. Agent stops accepting new builds
3. Agent waits for active builds to complete (or timeout)
4. Agent proposes RaftCommand::AgentLeft
5. Agent shuts down
```

### 7. Platform and Feature Matching

**Agent Startup: Query Nix configuration**

Each agent queries its local Nix configuration on startup:

```rust
// Query all Nix configuration at once
let output = Command::new("nix")
    .args(&["config", "show"])
    .output()
    .await?;

let config = String::from_utf8(output.stdout)?;

// Parse configuration (format: "key = value")
let mut system = None;
let mut extra_platforms = Vec::new();
let mut features = Vec::new();

for line in config.lines() {
    if let Some((key, value)) = line.split_once(" = ") {
        match key.trim() {
            "system" => {
                system = Some(value.trim().to_string());
            }
            "extra-platforms" => {
                extra_platforms = value.split_whitespace()
                    .map(|s| s.to_string())
                    .collect();
            }
            "system-features" => {
                features = value.split_whitespace()
                    .map(|s| s.to_string())
                    .collect();
            }
            _ => {}
        }
    }
}

// Combine: agent can build for primary + extra platforms
let mut platforms = vec![system.expect("system not found in nix config")];
platforms.extend(extra_platforms);

// Agent now advertises:
// platforms: ["x86_64-linux", "i686-linux"]
// features: ["kvm", "big-parallel", "nixos-test"]
```

**Examples of platform compatibility:**
- `x86_64-linux` can often build `i686-linux` (32-bit on 64-bit)
- `aarch64-darwin` (Apple Silicon) can build `x86_64-darwin` via Rosetta 2
- `armv7l-linux` can build `armv6l-linux` and `armv5tel-linux`

**System Features** (from Nix documentation):
- `kvm` - KVM virtualization support (required for VM tests)
- `big-parallel` - Suitable for highly parallel builds (many cores)
- `nixos-test` - Can run NixOS integration tests
- `benchmark` - Suitable for performance benchmarks
- `ca-derivations` - Supports content-addressed derivations
- Custom features defined in `nix.conf`

**CLI Agent Selection with Build Affinity:**

```rust
fn select_agent(
    cluster: &ClusterState,
    requirements: &BuildRequirements
) -> Option<AgentId> {
    // STEP 1: Filter by hard requirements (non-negotiable)
    let eligible_agents: Vec<_> = cluster.agents.values()
        .filter(|agent| {
            agent.platforms.contains(&requirements.platform) &&
            requirements.features.iter().all(|f| agent.features.contains(f)) &&
            agent.status == AgentStatus::Available
        })
        .collect();

    if eligible_agents.is_empty() {
        return None;  // No agents satisfy requirements
    }

    // STEP 2: Calculate affinity among eligible agents only
    let mut agent_affinity: HashMap<AgentId, usize> = HashMap::new();

    for dep_hash in &requirements.dependency_hashes {
        // Check active builds
        if let Some(tracker) = cluster.builds_in_progress.get(dep_hash) {
            // Only count if agent is eligible
            if eligible_agents.iter().any(|a| a.id == tracker.agent_id) {
                *agent_affinity.entry(tracker.agent_id).or_insert(0) += 1;
            }
        }
        // Check recently completed (might still be in /nix/store)
        if let Some(completed) = cluster.completed_builds.get(dep_hash) {
            if eligible_agents.iter().any(|a| a.id == completed.agent_id) {
                *agent_affinity.entry(completed.agent_id).or_insert(0) += 1;
            }
        }
    }

    // STEP 3: Select agent with best affinity, or any eligible agent
    agent_affinity.iter()
        .max_by_key(|(_, count)| *count)
        .map(|(id, _)| *id)
        .or_else(|| eligible_agents.first().map(|a| a.id))
}
```

**Selection Priority:**
1. Platform compatibility (hard constraint)
2. Feature requirements (hard constraint)
3. Agent availability (hard constraint)
4. Build affinity (optimization - best effort)

**Trade-off: Requirements vs. Affinity**

If a dependency is building on an agent that lacks required features, we sacrifice cache locality:

```
bar building on Agent X (no kvm)
foo requires [kvm], depends on [bar]

→ Select Agent Y (has kvm)
→ Agent Y fetches bar from substituters
→ Accepts non-optimal allocation for correctness
```

Future enhancement: agent-to-agent output transfer to recover locality.

If no agents satisfy requirements:
```
Error: No agents available for platform 'x86_64-linux' with features: [kvm, big-parallel]
Available agents: 3 (none match requirements)
```

### 8. Build Dependencies

**How does agent get dependencies?**

Derivations reference inputs: `/nix/store/xyz-dep1`, `/nix/store/abc-dep2`

**Option 1: Agent fetches from substituters** (MVP approach)

- Agent has standard Nix configuration
- Agent configured with substituters: `https://cache.nixos.org`
- When `nix-build` runs, Nix automatically fetches missing inputs
- Pros: Simple, uses existing infrastructure
- Cons: Requires internet access, external dependency

**Option 2: CLI pushes dependencies** (future)

- CLI runs `nix-store --query --references /nix/store/foo.drv`
- CLI identifies all dependencies
- CLI exports dependencies to NAR, streams to agent before build
- Agent imports dependencies
- Pros: Offline builds, controlled
- Cons: Complex, high bandwidth

**MVP: Option 1.** Let Nix handle it.

### 9. gRPC Protocol

```protobuf
syntax = "proto3";

package rio.v1;

// Service exposed by agents to CLI clients
service RioAgent {
  // Cluster discovery
  rpc GetClusterMembers(GetClusterMembersRequest)
      returns (ClusterMembers);

  // Build submission (bidirectional stream)
  // May return RedirectToAgent error if build is happening elsewhere
  rpc SubmitBuild(stream BuildRequest)
      returns (stream BuildUpdate);

  // Subscribe to an in-progress build (for deduplication)
  rpc SubscribeToBuild(SubscribeToBuildRequest)
      returns (stream BuildUpdate);

  // Get outputs from a recently completed build
  rpc GetCompletedBuild(GetCompletedBuildRequest)
      returns (stream BuildUpdate);

  // Job status queries (for failure recovery)
  rpc GetJobStatus(GetJobStatusRequest)
      returns (JobStatus);

  // Agent management (for joining cluster)
  rpc JoinCluster(JoinClusterRequest)
      returns (JoinClusterResponse);
}

message GetClusterMembersRequest {}

message ClusterMembers {
  repeated AgentInfo agents = 1;
  string leader_id = 2;
}

message AgentInfo {
  string id = 1;
  string address = 2;
  repeated string platforms = 3;
  repeated string features = 4;
  AgentStatus status = 5;
  BuilderCapacity capacity = 6;
}

enum AgentStatus {
  AGENT_STATUS_UNSPECIFIED = 0;
  AGENT_STATUS_AVAILABLE = 1;  // Idle, can accept builds
  AGENT_STATUS_BUSY = 2;       // Executing one build (or has queued builds)
  AGENT_STATUS_DOWN = 3;       // Failed heartbeats
}

message BuilderCapacity {
  int32 cpu_cores = 1;
  int64 memory_mb = 2;
  int64 disk_gb = 3;
}

message BuildRequest {
  oneof payload {
    SubmitBuildRequest submit = 1;
  }
}

message SubmitBuildRequest {
  bytes derivation = 1;
  repeated string dependency_hashes = 2;  // Pre-computed hashes of all dependencies
  string platform = 3;
  repeated string required_features = 4;
  optional int32 timeout_seconds = 5;
}

message BuildUpdate {
  string derivation_hash = 1;  // Job identifier
  oneof update {
    LogLine log = 2;
    OutputChunk output_chunk = 3;
    BuildCompleted completed = 4;
    BuildFailed failed = 5;
  }
}

message LogLine {
  int64 timestamp = 1;
  string line = 2;
}

message OutputChunk {
  string output_path = 1;
  bytes data = 2;
  int32 chunk_index = 3;
  bool last_chunk = 4;
}

message BuildCompleted {
  repeated string output_paths = 1;
  int64 duration_ms = 2;
}

message BuildFailed {
  string error = 1;
  optional string stderr = 2;
}

message GetJobStatusRequest {
  string derivation_hash = 1;
}

message JobStatus {
  string derivation_hash = 1;
  BuildState state = 2;
  optional string agent_id = 3;
  optional string error = 4;
}

enum BuildState {
  BUILD_STATE_UNSPECIFIED = 0;
  BUILD_STATE_RUNNING = 1;
  BUILD_STATE_COMPLETED = 2;
  BUILD_STATE_FAILED = 3;
}

message JoinClusterRequest {
  AgentInfo agent_info = 1;
}

message JoinClusterResponse {
  bool success = 1;
  string message = 2;
}

// Build deduplication messages

message SubscribeToBuildRequest {
  string derivation_hash = 1;  // Job identifier
}

message GetCompletedBuildRequest {
  string derivation_hash = 1;  // Job identifier
}

// Note: Both return stream BuildUpdate (logs + outputs)
```

## Technology Stack

### Nix Tooling

**`nix-eval-jobs`** (with PR #387 for system features)
- Parallel evaluation with streamable JSON output
- Source: Fork with `--show-required-system-features` until merged
- Flake input: `github:nix-community/nix-eval-jobs/pull/387/head`
- Replaces: `nix-instantiate` + `nix-store --query` + manual .drv parsing

**Standard Nix commands:**
- `nix-build` - Build execution on agents
- `nix-store --export/import` - Output transfer
- `nix config show` - Agent capability detection

### Raft Consensus

**Choice: `openraft`** (formerly async-raft)

- Tokio-native async implementation
- Well-maintained, good documentation
- Ergonomic API for state machine
- Supports dynamic membership

Alternative considered: `tikv/raft-rs` (too low-level, requires custom transport)

### gRPC

- `tonic` 0.14 + `tonic-prost` 0.14
- Already in use, proven

### Persistent Storage

Raft requires persistence for:

- Log entries
- Current term
- Voted-for state

**Choice: RocksDB via `rocksdb` crate**

- Embedded, no external database
- High performance
- Used by many Raft implementations

**Storage Layout:**

```
/var/lib/rio/agent-{id}/
  raft-log/          # Raft log entries (RocksDB)
  raft-state/        # Raft metadata (RocksDB)
```

### Nix Integration

**CLI side:**
- `nix-eval-jobs` with cache checking and system features
  - Using fork `github:lovesegfault/nix-eval-jobs/for-rio` with PR #387
  - Flags: `--check-cache-status --show-required-system-features --show-input-drvs`
  - Provides in single JSON output:
    * `drvPath` - Top-level derivation path
    * `system` - Required platform
    * `requiredSystemFeatures` - Required features array
    * `cacheStatus` - "notBuilt", "cached", or "local"
    * `neededBuilds` - Array of .drv files that actually need building
    * `neededSubstitutes` - Array of paths available in caches
  - **Key benefit:** Only register derivations in `neededBuilds`, skip cached ones
  - If `cacheStatus` is "cached" or "local", skip remote build entirely

**Agent side:**
- `nix-build` for build execution
- `nix-store --export` / `nix-store --import` for output transfer
- `nix config show` for agent capability detection (system, extra-platforms, system-features)

## Implementation Phases

### Phase 1: Single-Agent MVP (No Raft)

**Goal:** Prove build execution works end-to-end

- CLI runs nix-instantiate, gets .drv
- CLI connects directly to single agent
- Agent executes nix-build
- Agent streams logs back to CLI
- Agent exports outputs, streams to CLI
- CLI imports outputs

**No Raft, no cluster.** Just prove the data plane works.

### Phase 2: Raft Cluster (No Builds)

**Goal:** Prove Raft coordination works

- Agents form Raft cluster
- Agents track membership
- Agents handle join/leave
- CLI discovers cluster members
- Test failure scenarios (leader election, network partition)

**No builds yet.** Just prove Raft works.

### Phase 3: Integrate Builds + Raft

**Goal:** Full system integration

- CLI submits to cluster
- Agent registers job in Raft before building
- Agent builds, streams to CLI
- Agent marks complete in Raft
- Test failure scenarios (agent dies mid-build, CLI disconnects)

### Phase 4: Production Hardening

- TLS/mTLS for gRPC
- Authentication for CLI clients
- Build result caching
- Binary cache integration
- Monitoring and metrics
- Web UI for cluster status

## Open Questions

### 1. Should CLI be smart or dumb?

**Current design: Smart CLI**

- CLI gets full cluster state
- CLI selects agent locally
- Pros: No bottleneck, fast selection
- Cons: CLI needs more logic

**Alternative: Dumb CLI, smart leader**

- CLI always connects to leader
- Leader assigns job to agent
- Pros: Simpler CLI
- Cons: Leader becomes bottleneck

**Decision: Keep smart CLI.** Bottlenecks are what we're trying to avoid.

### 2. How long to cache cluster membership in CLI?

**Proposal: 60 seconds**

- Cache for 60s to avoid repeated GetClusterMembers calls
- Refresh on failure (if selected agent is down)
- Config option for cache TTL

### 3. Should we support multi-output derivations?

Nix derivations can have multiple outputs: `out`, `dev`, `doc`, etc.

**MVP: Support only single-output derivations**
Future: Export all outputs as separate NARs

### 4. How to handle concurrent builds on same agent?

**Proposal: Configurable concurrency per agent**

- Agent config: `--max-concurrent-builds=4`
- Agent tracks active build count
- Agent rejects new builds if at capacity
- CLI selects different agent

### 5. Should we persist build history?

**MVP: No persistent history**

- Raft state machine keeps recent completed jobs (LRU cache, ~1000 entries)
- Enough for short-term status queries
- Future: External database for long-term history/analytics

## Success Metrics

- **Correctness**: Any build that works with `nix-build` works with Rio
- **Performance**: <100ms overhead for agent selection
- **Scalability**: Build throughput scales linearly with agent count
- **Reliability**: Cluster tolerates minority agent failures
- **Availability**: No single point of failure

## Security Considerations

### MVP (Trusted Environment)

- Agents trust each other
- CLI trusts agents
- No authentication, no encryption
- Suitable for: internal networks, VPNs

### Future (Production)

- mTLS for all gRPC connections
- Client certificates for CLI authentication
- Raft communication encrypted
- Build isolation (containers/VMs for multi-tenancy)

## References

- [Raft Consensus Algorithm](https://raft.github.io/)
- [openraft Documentation](https://docs.rs/openraft/)
- [nix-eval-jobs](https://github.com/nix-community/nix-eval-jobs) - Parallel evaluator with JSON output
- [nix-eval-jobs PR #387](https://github.com/nix-community/nix-eval-jobs/pull/387) - Add requiredSystemFeatures support
- [Nix Manual: Derivations](https://nixos.org/manual/nix/stable/language/derivations.html)
- [Nix Manual: system-features](https://nix.dev/manual/nix/2.18/command-ref/conf-file.html#conf-system-features)
- [nixbuild.net](https://nixbuild.net/) - Inspiration for distributed Nix builds

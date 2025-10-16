# Rio Design Document

## Overview

Rio is an open-source distributed build service for Nix that eliminates the traditional broker architecture by using a peer-to-peer agent cluster coordinated via Raft consensus.

**Key Innovation:** Instead of a central dispatcher that becomes a bottleneck and single point of failure, agents coordinate via Raft consensus while build data (derivations, logs, outputs) flows directly between CLI and agents.

## Architecture

### High-Level Design

```mermaid
graph TB
    CLI[rio-build CLI]

    subgraph Cluster[rio-agent Cluster]
        Leader[Agent 1 - Leader]
        Agent2[Agent 2]
        Agent3[Agent 3]

        Leader <-->|Raft consensus| Agent2
        Agent2 <-->|Raft consensus| Agent3
        Agent3 <-->|Raft consensus| Leader
    end

    CLI -->|QueueBuild| Leader
    Leader -->|BuildAssigned| CLI
    CLI -->|SubscribeToBuild| Agent2
    Agent2 -->|Logs + Outputs| CLI
```

**Critical Insights:**

- **Control Plane (Raft):** Membership, build tracking, deterministic assignment
- **Data Plane (gRPC):** Derivations, logs, outputs flow directly CLI ↔ Agent
- **No Bottleneck:** Data bypasses Raft entirely, streams point-to-point
- **Deterministic:** All agents run same state machine → agree on assignment

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

    // Active builds: derivation path → which agent is building it
    builds_in_progress: HashMap<DerivationPath, BuildTracker>,

    // Recently completed builds (5 minute LRU cache for fast retrieval)
    completed_builds: LruCache<DerivationPath, CompletedBuild>,

    // Pending derivations: NAR bytes cached while build is active
    // Removed on BuildCompleted/BuildFailed via log compaction
    pending_derivations: HashMap<DerivationPath, Vec<u8>>,
}

struct BuildTracker {
    agent_id: Option<AgentId>,  // None when queued, Some when claimed
    suggested_agents: Vec<AgentId>,  // Ranked by affinity (best first)
    started_at: Timestamp,
    parent_build: Option<DerivationPath>,  // None = top-level, Some(path) = dependency
    status: BuildStatus,
}

enum BuildStatus {
    Queued,    // Waiting for agent to claim
    Claimed,   // Agent claimed (committed to build), may be busy with another build
    Building,  // Currently executing on agent
}

struct CompletedBuild {
    agent_id: AgentId,  // Where outputs are stored in /nix/store
    output_paths: Vec<Utf8PathBuf>,
    completed_at: Timestamp,
}

struct AgentInfo {
    id: AgentId,               // UUID (also serves as Raft NodeId)
    address: Url,              // gRPC endpoint (e.g., "http://agent1:50051")
    platforms: Vec<String>,    // ["x86_64-linux", "i686-linux"] (from system + extra-platforms)
    features: Vec<String>,     // ["kvm", "big-parallel"] (from system-features)
    capacity: BuilderCapacity,
    last_heartbeat: Timestamp,
    status: AgentStatus,       // Available, Busy, or Down
}

enum AgentStatus {
    Available = 0,  // Idle, can accept builds (default)
    Busy = 1,       // Currently executing one build
    Down = 2,       // Failed heartbeats
}

// Note: JobAssignment removed - redundant with BuildTracker
// Derivation path serves as the job identifier
```

**Raft Commands:**

```rust
enum RaftCommand {
    // Membership
    AgentJoined { id: AgentId, info: AgentInfo },
    AgentLeft { id: AgentId },
    AgentHeartbeat { id: AgentId, timestamp: Timestamp },

    // Build lifecycle (derivation path is the job identifier)
    // Leader proposes this when CLI submits work
    // State machine creates ranked list of suggested agents
    BuildQueued {
        top_level: DerivationPath,
        derivation_nar: Vec<u8>,  // NAR bytes, replicated to all agents
        dependencies: Vec<DerivationPath>,
        platform: String,
        features: Vec<String>,
    },
    // Agent proposes this to claim a queued build
    BuildClaimed {
        derivation_path: DerivationPath,
        agent_id: AgentId,
        affinity_score: usize,  // Agent's self-calculated affinity
    },
    // Agent proposes this when starting execution
    BuildStarted {
        derivation_path: DerivationPath,
    },
    BuildCompleted {
        derivation_path: DerivationPath,
        output_paths: Vec<Utf8PathBuf>,
    },
    BuildFailed {
        derivation_path: DerivationPath,
        error: String,
    },
}
```

**What Raft Stores:**

- Build metadata (which agent building, dependencies, status)
- Derivation NARs (temporarily while build is active, compacted after completion)

**What Raft Does NOT Store:**

- Build logs (streamed to subscribers in real-time via gRPC)
- Build outputs (stored in agent's /nix/store, exported on demand via gRPC)

**Build Claiming Model:**

When a build is queued, the Raft state machine creates a ranked list of suggested agents based on affinity. Agents then self-select and claim builds:

```rust
fn apply_build_queued(state: &mut ClusterState, cmd: BuildQueued) -> Vec<AgentId> {
    // 1. Filter eligible agents (platform + features, not Down)
    let eligible: Vec<_> = state.agents.values()
        .filter(|a| {
            a.platforms.contains(&cmd.platform) &&
            cmd.features.iter().all(|f| a.features.contains(f)) &&
            a.status != AgentStatus::Down
        })
        .collect();

    // 2. Score ALL eligible agents by affinity (count matching dependencies)
    let mut agent_scores: Vec<(AgentId, usize)> = eligible.iter()
        .map(|agent| {
            let mut score = 0;
            for dep_path in &cmd.dependencies {
                if let Some(tracker) = state.builds_in_progress.get(dep_path) {
                    if tracker.agent_id == Some(agent.id) {
                        score += 1;
                    }
                }
                if let Some(completed) = state.completed_builds.get(dep_path) {
                    if completed.agent_id == agent.id {
                        score += 1;
                    }
                }
            }
            (agent.id, score)
        })
        .collect();

    // 3. Sort by score (descending), then by agent_id (ascending) for determinism
    agent_scores.sort_by(|(id_a, score_a), (id_b, score_b)| {
        match score_b.cmp(score_a) {  // Higher score better
            std::cmp::Ordering::Equal => id_a.cmp(id_b),  // Smaller ID wins tie
            other => other,
        }
    });

    let suggested_agents: Vec<AgentId> = agent_scores.iter().map(|(id, _)| *id).collect();

    // 4. Create build in Queued state (no agent assigned yet)
    state.builds_in_progress.insert(cmd.top_level, BuildTracker {
        agent_id: None,  // Will be set when agent claims
        suggested_agents: suggested_agents.clone(),
        status: BuildStatus::Queued,
        parent_build: None,
        started_at: now(),
    });

    // Register dependencies as queued too
    for dep_path in cmd.dependencies {
        state.builds_in_progress.insert(dep_path, BuildTracker {
            agent_id: None,
            suggested_agents: suggested_agents.clone(),
            status: BuildStatus::Queued,
            parent_build: Some(cmd.top_level),
            started_at: now(),
        });
    }

    // Store derivation NAR (all agents have it now)
    state.pending_derivations.insert(cmd.top_level, cmd.derivation_nar);

    // Do NOT mark any agent as Busy (they haven't claimed yet)

    suggested_agents  // Return ranked list to CLI
}

fn apply_build_claimed(state: &mut ClusterState, cmd: BuildClaimed) -> Result<()> {
    if let Some(tracker) = state.builds_in_progress.get_mut(&cmd.derivation_path) {
        // Verify build is still Queued
        if tracker.status != BuildStatus::Queued {
            return Err("Build already claimed");
        }

        // Check suggested agents or timeout
        let elapsed = now() - tracker.started_at;
        if !tracker.suggested_agents.contains(&cmd.agent_id) && elapsed < 2s {
            return Err("Wait for suggested agents");
        }

        // Accept claim
        tracker.agent_id = Some(cmd.agent_id);
        tracker.status = BuildStatus::Claimed;

        // Mark agent as Busy
        if let Some(agent) = state.agents.get_mut(&cmd.agent_id) {
            agent.status = AgentStatus::Busy;
        }

        Ok(())
    } else {
        Err("Build not found")
    }
}
```

**Why this works:**
- ✅ Truthful status - Queued means not claimed yet, Building means actually executing
- ✅ Distributed claiming - Agents self-select based on local state
- ✅ Affinity optimization - State machine creates ranked suggestions
- ✅ No races - Raft linearizability ensures first claim wins
- ✅ Fallback mechanism - Non-suggested agents can claim after timeout
- ✅ Capacity awareness - Busy agents don't claim new builds
- ✅ Dependency awareness - Agents calculate affinity before claiming
- ✅ Durable - Derivations in Raft survive leader election

**Hybrid Claiming Model: Centralized Ranking + Distributed Selection**

Rio combines centralized affinity optimization with distributed agent claiming:

1. **State machine creates ranked suggestions** (centralized, deterministic)
   - Filter eligible agents by platform/features
   - Score all agents by affinity (count matching dependencies)
   - Sort by score (high→low), tie-break by agent_id (deterministic)
   - Return ranked list: [best_agent, second_best, ...]

2. **Agents self-select builds to claim** (distributed, responsive)
   - Skip if busy (current_build is Some)
   - Skip if dependencies still building
   - Claim immediately if in suggested list and have affinity
   - Claim after 2s timeout if not suggested but have affinity

3. **Raft ensures atomicity** (first claim wins, others rejected)

**Why claiming instead of assignment:**

Traditional assignment has semantic issues:
- ❌ State says "Building" but agent hasn't started yet
- ❌ Agent might be busy and unable to accept
- ❌ No way to represent "assigned but not acknowledged"

Claiming model fixes this:
- ✅ `Queued` means no agent committed yet
- ✅ `Claimed` means agent explicitly accepted
- ✅ `Building` means actually executing
- ✅ Status always matches reality

**How agents decide to claim:**
- **Capacity:** Only claim if not currently busy
- **Dependencies:** Only claim if dependencies available
- **Affinity:** Prefer builds with dependencies on us
- **Priority:** Suggested agents claim first, others wait for timeout

**Benefits:**
- Truthful state transitions (status matches reality)
- Affinity optimization preserved (ranked suggestions)
- Distributed decision-making (agents know local state)
- Automatic load balancing (busy agents don't claim)
- Dependency awareness (agents check before claiming)
- No tunable parameters needed

**How agents decide when to claim builds:**

Agents watch Raft state for queued builds and actively claim them. **Busy agents CAN claim builds** to preserve affinity - they'll start them when free:

```rust
async fn build_coordinator_loop() {
    loop every 100ms {
        // If we're free, start any builds we've claimed
        if current_build.is_none() {
            for (drv_path, tracker) in builds_in_progress {
                if tracker.status == Claimed && tracker.agent_id == us {
                    start_build(drv_path);
                    raft.propose(BuildStarted { drv_path });
                    break;
                }
            }
        }

        // Find queued builds we could claim (even if busy)
        for (drv_path, tracker) in builds_in_progress {
            if tracker.status != Queued {
                continue;
            }

            // Check if we match requirements
            if !matches_platform_and_features(tracker) {
                continue;
            }

            // Calculate our affinity score
            let our_score = count_dependencies_on_us(tracker.dependencies);

            // Check if dependencies are ready
            if any_dependency_still_building(tracker.dependencies) {
                continue;  // Wait for dependencies
            }

            // Decide whether to claim
            let elapsed = now() - tracker.started_at;
            let should_claim = if tracker.suggested_agents.contains(us) {
                // We're suggested - claim if we have affinity
                our_score > 0 || elapsed > 200ms
            } else if elapsed > 2s {
                // Timeout passed - any agent with affinity can claim
                our_score > 0
            } else {
                false  // Not our turn yet
            };

            if should_claim {
                // Try to claim the build
                raft.propose(BuildClaimed {
                    derivation_path: drv_path,
                    agent_id: us,
                    affinity_score: our_score,
                });
                // If claim accepted, start build and propose BuildStarted
                // If claim rejected (race - another agent claimed), try next build
                break;  // Only claim one build per iteration
            }
        }
    }
}
```

**Example claiming scenario (affinity with busy agent):**
```
Cluster state:
- Agent A: Building foo.drv, has completed bar.drv recently
- Agent B: Available
- Agent C: Available

New build: baz.drv depends on bar.drv

State machine creates suggestions:
1. Score Agent A: 1 (has bar.drv completed)
2. Score Agent B: 0 (no affinity)
3. Score Agent C: 0 (no affinity)
4. Ranked list: [A, B, C]

Build claiming:
1. Agent A (busy, but suggested with score=1): Claims immediately
   - Build state: Claimed, agent_id: A, status: Claimed
   - Agent A continues building foo.drv
2. Agents B,C (suggested, score=0): Don't claim (no affinity)
3. When foo.drv completes:
   - Agent A sees baz.drv in Claimed state with agent_id=A
   - Agent A starts baz.drv
   - Proposes BuildStarted
   - Status: Claimed → Building

Result:
- baz.drv builds on Agent A (optimal - bar.drv already in /nix/store)
- Instant cache hit for bar.drv dependency
- **Affinity preserved even though Agent A was busy!**
```

**Build Deduplication:**

- Derivation path serves as the job identifier (no separate JobId needed)
- When User A submits a build, leader proposes BuildQueued to Raft
- State machine assigns to best agent
- When User B submits the **same** derivation, they're transparently subscribed to the in-progress build
- Both users receive logs and outputs from the single build execution
- Completed builds cached for 5 minutes to serve late arrivals

### 2. Build Submission Flow (End to End)

#### Sequence Diagram

```mermaid
sequenceDiagram
    actor User
    participant CLI as rio-build CLI
    participant Leader as Agent (Leader)
    participant Raft as Raft Cluster
    participant Agent as Assigned Agent
    participant Nix as nix-build

    User->>CLI: rio-build ./app.nix
    CLI->>CLI: nix-eval-jobs<br/>(extract drv + deps)

    CLI->>Leader: QueueBuild(drv_nar_bytes, deps)
    Leader->>Raft: Propose BuildQueued(with NAR)
    Raft->>Raft: Replicate to all agents<br/>Deterministic assignment<br/>(select Agent K)
    Raft-->>Leader: Committed, assigned to K
    Leader->>CLI: BuildAssigned(agent_id: K)

    CLI->>Agent: SubscribeToBuild(derivation_path)

    Agent->>Agent: Read NAR from Raft storage

    Agent->>Nix: nix-build {derivation_path}
    loop Build Logs
        Nix-->>Agent: stdout/stderr
        Agent->>CLI: BuildUpdate(log)
    end

    Nix-->>Agent: Build complete
    Agent->>Agent: nix-store --export
    Agent->>CLI: BuildUpdate(output_chunks)
    Agent->>Raft: Propose BuildCompleted

    CLI->>CLI: nix-store --import
    CLI->>User: Build succeeded!
```

#### Detailed Steps

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
   - Export derivation as NAR: `nix-store --export /nix/store/abc123-foo.drv`
   - Result: BuildInfo with derivation NAR bytes and minimal set of builds needed

┌─────────────────────────────────────────────────────┐
│ 2. CLI: Connect to cluster leader                   │
└─────────────────────────────────────────────────────┘
   - Read seed agents from ~/.config/rio/config.toml (or --seed-agents flag)
   - Try each seed agent URL with 5s timeout
   - Call GetClusterMembers() RPC on first responsive agent
   - Parse response: leader_id (UUID string) and agents list
   - Find leader's address from agents list
   - Connect to leader at discovered address

┌─────────────────────────────────────────────────────┐
│ 3. CLI → Leader: Submit work to queue               │
└─────────────────────────────────────────────────────┘
   CLI → Leader: QueueBuild(derivation_path, drv_nar_bytes, deps, platform, features)

   Leader receives derivation NAR bytes via gRPC

┌─────────────────────────────────────────────────────┐
│ 4. Leader: Propose to Raft (with derivation NAR)    │
└─────────────────────────────────────────────────────┘
   Leader proposes: BuildQueued {
     derivation_path,
     derivation_nar,  // NAR bytes included in Raft command
     platform,
     features,
     dependency_paths,
   }

   Raft replicates to all agents (gRPC compresses on wire)
   All agents store in RocksDB (compressed at rest)

┌─────────────────────────────────────────────────────┐
│ 5. Raft: Deterministic agent assignment             │
└─────────────────────────────────────────────────────┘
   ALL agents apply BuildQueued to state machine:

   1. Filter eligible agents (platform + features, not Down)
   2. Score ALL agents by affinity (count matching dependencies)
      - Busy and Available agents compete equally
   3. Select highest affinity score
   4. Tie-breaker: lexicographically smallest agent_id
   5. Update state:
      - builds_in_progress[derivation_path] = { agent_id: selected, ... }
      - pending_derivations[derivation_path] = derivation_nar
      - agents[selected].status = Busy

   Result: ALL agents agree on assignment (deterministic!)

┌─────────────────────────────────────────────────────┐
│ 6. Assigned agent: Read derivation and start        │
└─────────────────────────────────────────────────────┘
   Agent K sees: "I was assigned derivation_path"

   Read derivation from Raft storage (already replicated):
     - drv_nar = state_machine.pending_derivations.get(&derivation_path)

   Import derivation to Nix store:
     - echo drv_nar | nix-store --import
     - Returns canonical path: /nix/store/hash-foo.drv

   Check current workload:
     - If currently building → Add to pending_builds queue
     - Else if deps building on self → Queue with Queued status
     - Else → Start immediately with Building status

   Spawn: nix-build /nix/store/hash-foo.drv

┌─────────────────────────────────────────────────────┐
│ 7. Leader: Respond to CLI with assignment           │
└─────────────────────────────────────────────────────┘
   Leader waits for Raft commit
   Leader reads state machine: derivation_path assigned to Agent K
   Leader → CLI: BuildAssigned { agent_id: K, derivation_path }

┌─────────────────────────────────────────────────────┐
│ 8. CLI → Assigned Agent: Subscribe to build         │
└─────────────────────────────────────────────────────┘
   CLI → Agent K: SubscribeToBuild(derivation_path)
   Agent K streams: logs + outputs
   CLI displays logs in real-time

┌─────────────────────────────────────────────────────┐
│ 9. Agent: Export outputs                            │
└─────────────────────────────────────────────────────┘
   - nix-build completes → /nix/store/abc123-result
   - Run: nix-store --export /nix/store/abc123-result
   - Capture NAR (Nix ARchive) bytes

┌─────────────────────────────────────────────────────┐
│ 10. Agent → CLI: Stream outputs                     │
└─────────────────────────────────────────────────────┘
   Send BuildUpdate {
       output_chunk: OutputChunk {
           data: [bytes...],
           chunk_index: 0,
           last_chunk: false,
           compression: COMPRESSION_TYPE_ZSTD,
       }
   }

┌─────────────────────────────────────────────────────┐
│ 11. CLI: Import outputs                             │
└─────────────────────────────────────────────────────┘
   - Receive all chunks, reassemble NAR
   - Run: nix-store --import < output.nar
   - Outputs now in local /nix/store

┌─────────────────────────────────────────────────────┐
│ 12. Agent: Mark complete and unblock dependents     │
└─────────────────────────────────────────────────────┘
   - Propose RaftCommand::BuildCompleted { derivation_path, output_paths }
   - Raft removes top-level from builds_in_progress
   - Raft removes ALL dependencies (where parent_build = this path)
   - Raft removes derivation from pending_derivations
   - Raft compacts log entry (derivation NAR freed from storage)
   - Send BuildUpdate { completed: BuildCompleted { output_paths, duration } }
   - Check pending_builds for anything waiting on this derivation
   - Auto-start any builds that are now unblocked
   - If no pending builds: mark agent as Available

┌─────────────────────────────────────────────────────┐
│ 13. CLI: Report success                             │
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

#### Deduplication Sequence

```mermaid
sequenceDiagram
    actor UserA as User A
    actor UserB as User B
    participant Leader
    participant Raft
    participant AgentK as Agent K

    Note over UserA,AgentK: User A submits foo.drv

    UserA->>Leader: QueueBuild(foo)
    Leader->>Raft: BuildQueued(foo)
    Raft->>Raft: Assign to Agent K
    Leader->>UserA: BuildAssigned(K, foo)
    UserA->>AgentK: SubscribeToBuild(foo)
    AgentK->>AgentK: Start building foo
    AgentK-->>UserA: Log: "Building..."

    Note over UserB,AgentK: User B submits same foo.drv (30s later)

    UserB->>Leader: QueueBuild(foo)
    Leader->>Leader: Check Raft:<br/>foo already building on K
    Leader->>UserB: AlreadyBuilding(K, foo)
    UserB->>AgentK: SubscribeToBuild(foo)
    AgentK-->>UserB: Catch-up logs
    AgentK-->>UserA: Log: "Continuing..."
    AgentK-->>UserB: Log: "Continuing..."

    Note over AgentK: Build completes

    AgentK->>Raft: BuildCompleted(foo)
    AgentK-->>UserA: Outputs
    AgentK-->>UserB: Outputs
    UserA->>UserA: Import
    UserB->>UserB: Import
```

#### Detailed Scenario: User B joins in-progress build

```
Time T0: User A submits /nix/store/abc123-foo.drv
1. CLI extracts build closure:
   - foo.drv (top-level)
   - bar.drv (dependency)
   - baz.drv (dependency)
2. CLI → Leader: QueueBuild(foo_path, foo_nar_bytes, deps=[bar_path, baz_path])
3. Leader proposes: BuildQueued {
     top_level: foo_path,
     derivation_nar: foo_nar_bytes,
     dependencies: [bar_path, baz_path],
     platform: "x86_64-linux",
     features: []
   }
4. Raft replicates to all agents, state machine applies BuildQueued:
   - Filters eligible agents (platform + features, not Down)
   - Scores by affinity (no deps yet, all score 0)
   - Tie-breaks: selects Agent K (smallest agent_id among eligible)
   - Atomically registers:
     * builds_in_progress[foo_path] = { agent: K, parent: None, status: Building }
     * builds_in_progress[bar_path] = { agent: K, parent: Some(foo_path), status: Building }
     * builds_in_progress[baz_path] = { agent: K, parent: Some(foo_path), status: Building }
     * pending_derivations[foo_path] = foo_nar_bytes
     * agents[K].status = Busy
5. Agent K sees assignment, reads NAR from own Raft storage
6. Agent K starts building, streaming logs to User A

Time T1: User B submits same foo.drv (30s later)
1. CLI extracts build closure: [foo, bar, baz]
2. CLI → Leader: QueueBuild(foo_path, foo_nar_bytes, deps=[bar_path, baz_path])
3. Leader checks Raft: builds_in_progress[foo_path] = Some(BuildTracker {
     agent_id: K,
     started_at: T0,
     parent_build: None
   })
4. Leader returns: AlreadyBuilding { agent_id: K, derivation_path: foo_path }
5. CLI → Agent K: SubscribeToBuild(foo_path)
6. Agent K sends User B:
   - Catch-up: All logs from T0 to T1
   - Live: New logs as they arrive
7. Build completes at T2
8. Agent K streams outputs to BOTH User A and User B
9. Both users import outputs, done
```

**Scenario: User C requests recently completed build**

```
Time T2: Build completed
1. Agent-1 proposes: BuildCompleted {
     derivation_path: drv_path,
     output_paths: ["/nix/store/abc123-result"]
   }
2. Raft moves from builds_in_progress → completed_builds (LRU cache)

Time T3: User C submits same derivation (3 minutes later)
1. CLI → Agent-2: QueueBuild(drv_path, drv_nar_bytes, ...)
2. Agent-2 checks Raft for drv_path
3. Finds in completed_builds: CompletedBuild {
     agent_id: "agent-1",
     output_paths: ["/nix/store/abc123-result"]
   }
4. Agent-2 returns: AlreadyCompleted { agent_id: "agent-1", derivation_path: drv_path }
5. CLI → Agent-1: GetCompletedBuild(derivation_path: drv_path)
6. Agent-1 exports: nix-store --export /nix/store/abc123-result
7. Agent-1 streams NAR chunks to User C
8. User C imports, done
```

**Agent-Side Implementation:**

When an agent is notified of a build assignment (by watching Raft commits), it determines
the appropriate BuildStatus and manages its local queue.

See "How BuildStatus is determined" section above for the complete logic.

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

**State machine atomically registers all derivations:**

When BuildQueued is applied, the state machine registers the top-level and all dependencies:

```rust
// Leader proposes BuildQueued (includes dependencies)
BuildQueued {
    top_level: "/nix/store/xyz-my-app.drv",
    derivation_nar: <NAR bytes>,
    dependencies: [
        "/nix/store/abc-dep1.drv",
        "/nix/store/def-dep2.drv",
        "/nix/store/ghi-dep3.drv"
    ],
    platform: "x86_64-linux",
    features: [],
}

// Raft state machine applies atomically (on ALL agents):
builds_in_progress["/nix/store/xyz-my-app.drv"] = BuildTracker {
    agent_id: agent-1,
    parent_build: None,  // Top-level
}

builds_in_progress["/nix/store/abc-dep1.drv"] = BuildTracker {
    agent_id: agent-1,
    parent_build: Some("/nix/store/xyz-my-app.drv"),  // Dependency
}

builds_in_progress["/nix/store/def-dep2.drv"] = BuildTracker {
    agent_id: agent-1,
    parent_build: Some("/nix/store/xyz-my-app.drv"),  // Dependency
}

// Now ALL derivations are registered and reserved!
```

**Deduplication in action:**

```
Time T0: User A runs: rio-build ./my-app.nix
- nix-eval-jobs returns:
  * neededBuilds: [my-app.drv, dep1.drv, dep2.drv]
  * neededSubstitutes: [dep3, dep4, dep5, ...]  (will fetch from cache)
- CLI → Leader: QueueBuild(my-app_path, my-app_nar, deps=[dep1_path, dep2_path])
- Raft assigns to Agent K
- Agent K starts: nix-build /nix/store/my-app.drv
- Nix automatically fetches dep3, dep4, dep5 from substituters

Time T1: User B runs: rio-build ./dep1.nix (30 seconds later)
- nix-eval-jobs returns:
  * cacheStatus: "notBuilt"
  * neededBuilds: [dep1.drv]
- CLI → Leader: QueueBuild(dep1_path, dep1_nar, deps=[])
- Leader checks Raft: builds_in_progress[dep1_path] = Some(BuildTracker {
    agent_id: K,
    parent_build: Some(my-app_path)  // Being built as dependency!
  })
- Leader returns: AlreadyBuilding { agent_id: K, derivation_path: dep1_path }
- CLI → Agent K: SubscribeToBuild(dep1_path)
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

#### Build Affinity and Dependency Waiting

```mermaid
sequenceDiagram
    actor UserA as User A
    actor UserB as User B
    participant Leader
    participant Raft
    participant AgentK as Agent K

    Note over UserA,AgentK: User A builds foo

    UserA->>Leader: QueueBuild(foo)
    Leader->>Raft: BuildQueued(foo)
    Raft->>Raft: Assign to Agent K
    AgentK->>AgentK: Start building foo
    AgentK-->>UserA: Logs streaming...

    Note over UserB,AgentK: User B builds bar (depends on foo)

    UserB->>Leader: QueueBuild(bar, deps=[foo])
    Leader->>Raft: BuildQueued(bar, deps=[foo])
    Raft->>Raft: Score agents:<br/>K has foo (affinity=1)<br/>→ Assign to Agent K
    Leader->>UserB: BuildAssigned(K, bar)
    UserB->>AgentK: SubscribeToBuild(bar)

    AgentK->>AgentK: Check: foo building on self<br/>→ Queue bar (blocked on foo)
    AgentK->>Raft: BuildStatusChanged(bar, QueuedDependency)
    AgentK-->>UserB: "Waiting for dependency foo..."

    Note over AgentK: foo completes

    AgentK->>AgentK: Auto-start bar<br/>(foo in /nix/store)
    AgentK->>Raft: BuildStatusChanged(bar, Building)
    AgentK-->>UserB: "Dependencies ready, building..."
    AgentK-->>UserB: bar logs streaming...

    AgentK->>Raft: BuildCompleted(bar)
    AgentK-->>UserB: Outputs
```

**Scenario: Build affinity with dependency waiting**

```
Time T0: User A runs: rio-build ./foo.nix
- Registers: foo.drv on Agent K (status: Building)
- Agent K starts building foo
- Agent K status: Busy

Time T1: User B runs: rio-build ./bar.nix (bar depends on foo)
- CLI extracts closure: [bar.drv, foo.drv]
- CLI → Leader: QueueBuild(bar, bar_nar, deps=[foo])
- Raft state machine scores agents:
  - Agent K: affinity=1 (has foo building)
  - Agent J: affinity=0
  - Agent M: affinity=0
- State machine selects Agent K (best affinity!)
- Leader → CLI: BuildAssigned(K, bar)

Time T2: Raft assigns bar to Agent K (affinity with foo)
- Agent K sees assignment in Raft log
- Agent K is busy building foo
- Agent K checks: foo is currently building on me
- Agent K proposes: BuildStatusChanged { bar, QueuedDependency { blocked_on: [foo] } }
- Agent K adds bar to pending_builds queue
- Agent K sends to User B: "Waiting for dependency /nix/store/abc-foo.drv..."

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
    drv_path: DerivationPath,
    outputs: Vec<Utf8PathBuf>
) {
    // Remove top-level
    state.builds_in_progress.remove(&drv_path);

    // Remove ALL dependencies tied to this build
    state.builds_in_progress.retain(|_, tracker| {
        tracker.parent_build != Some(drv_path.clone())
    });

    // Remove derivation NAR (no longer needed)
    state.pending_derivations.remove(&drv_path);

    // Cache result
    state.completed_builds.put(drv_path, CompletedBuild {
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

#### Agent Failure and Recovery

```mermaid
sequenceDiagram
    actor UserA as User A
    actor UserB as User B
    participant Leader
    participant Raft
    participant AgentK as Agent K (dies)
    participant AgentJ as Agent J

    UserA->>Leader: QueueBuild(foo)
    Leader->>Raft: BuildQueued(foo)
    Raft->>Raft: Assign to K
    AgentK->>AgentK: Building foo...
    AgentK-->>UserA: Logs...

    UserB->>Leader: QueueBuild(bar, deps=[foo])
    Leader->>Raft: BuildQueued(bar, deps=[foo])
    Raft->>Raft: Affinity → Assign to K
    AgentK->>AgentK: Queue bar (blocked on foo)
    AgentK-->>UserB: "Waiting for foo..."

    Note over AgentK: Agent K crashes!

    AgentK--xUserA: Stream disconnected
    AgentK--xUserB: Stream disconnected

    Note over Raft: Heartbeat timeout (30s)

    Raft->>Raft: Mark K as Down<br/>Remove all K's builds<br/>(foo, bar)

    UserA->>Leader: GetBuildStatus(foo)
    Leader-->>UserA: NOT_FOUND
    UserA->>Leader: QueueBuild(foo) [RETRY]
    Leader->>Raft: BuildQueued(foo)
    Raft->>Raft: Assign to J
    AgentJ->>AgentJ: Start building foo

    UserB->>Leader: GetBuildStatus(bar)
    Leader-->>UserB: NOT_FOUND
    UserB->>Leader: QueueBuild(bar, deps=[foo]) [RETRY]
    Leader->>Raft: BuildQueued(bar, deps=[foo])
    Raft->>Raft: Affinity with foo on J<br/>→ Assign to J
    AgentJ->>AgentJ: Queue bar (blocked on foo)

    Note over AgentJ: Builds naturally re-grouped!

    AgentJ-->>UserA: foo completes
    AgentJ->>AgentJ: Auto-start bar
    AgentJ-->>UserB: bar completes
```

**Scenario A: Agent dies with active and queued builds**

```
Time T0: User A submits foo
- builds_in_progress[foo_path] = { agent_id: K, status: Building }
- User A's CLI → Agent K (streaming logs)

Time T1: User B submits bar (depends on foo)
- Raft assigns to Agent K (affinity!)
- builds_in_progress[bar_path] = { agent_id: K, status: QueuedDependency { blocked_on: [foo_path] } }
- User B's CLI → Agent K (sees "Waiting for dependencies...")

Time T2: Agent K crashes
- Both CLIs detect stream disconnection
- Raft heartbeat timeout (30s)
- Raft marks Agent K as Down
- Raft cleanup removes ALL of Agent K's builds:
  * builds_in_progress.remove(foo_path)
  * builds_in_progress.remove(bar_path)
  * Remove dependencies of foo and bar

Time T3: User A retries foo
- CLI: GetBuildStatus(foo_path) → NotFound (cleaned up)
- CLI: Resubmit foo to cluster
- CLI selects Agent J (available)
- Agent J starts building foo
- builds_in_progress[foo_path] = { agent_id: J, status: Building }

Time T3+1ms: User B retries bar (nearly simultaneous)
- CLI: GetBuildStatus(bar_path) → NotFound
- CLI: Resubmit bar, check dependencies
- Raft shows: foo_path = { agent_id: J, status: Building } ← User A's build!
- Raft assigns to Agent J (affinity with foo!)
- Agent J receives assignment, detects foo building on self
- Agent J proposes: BuildStatusChanged { bar, QueuedDependency { blocked_on: [foo_path] } }
- builds_in_progress[bar_path] = { agent_id: J, status: QueuedDependency { blocked_on: [foo_path] } }
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
4. Agent still marks build complete in Raft
5. Build moved to completed_builds cache (5 minutes)
6. Outputs remain in agent's /nix/store
7. If CLI reconnects: GetCompletedBuild(derivation_path) from assigned agent
8. Agent exports outputs from /nix/store on demand
```

### 6. Cluster Membership

**Bootstrap (first agent):**

```bash
rio-agent --listen=0.0.0.0:50051 --data-dir=/var/lib/rio
```

- Creates single-node Raft cluster automatically
- Agent ID: generated UUID
- Raft state: { agents: { self }, active_jobs: {} }

**Join existing cluster:**

```bash
# Explicit join (production)
rio-agent --join=http://agent1.example.com:50051 --listen=0.0.0.0:50051

# Auto-discovery (development)
rio-agent --seeds=agent1:50051,agent2:50051 --listen=0.0.0.0:50051
```

- Connects to seed agent(s) via gRPC
- Sends JoinCluster RPC
- Seed agent (or leader) proposes RaftCommand::AgentJoined
- Once committed, new agent becomes voting member
- Auto-discovery tries each seed, bootstraps if all fail (with jitter)

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

**Error Handling:**

If no agents satisfy requirements:
```
Error: No agents available for platform 'x86_64-linux' with features: [kvm, big-parallel]
Available agents: 3 (none match requirements)
```

This is detected by the Raft state machine when applying BuildQueued - the `eligible` filter returns empty.

### 8. Derivation Transfer (CLI → Agent)

**Critical Discovery:** Derivations must be transferred as NARs, not raw .drv bytes.

**Why:**
- `nix-build` requires derivations to be in `/nix/store` at their canonical path
- Cannot build from `/tmp/foo.drv` or arbitrary locations
- `nix-store --add` doesn't work for derivations (computes wrong hash from filename)
- Must use `nix-store --import` to get derivations into the store

**Correct Transfer Flow:**

```bash
# CLI side:
nix-store --export /nix/store/x80j8hd76ca0yx7d8k4qn8fpqgbraqav-hello.drv > derivation.nar
# Send NAR bytes via gRPC

# Agent side:
cat derivation.nar | nix-store --import
# Returns: /nix/store/x80j8hd76ca0yx7d8k4qn8fpqgbraqav-hello.drv
nix-build /nix/store/x80j8hd76ca0yx7d8k4qn8fpqgbraqav-hello.drv
```

**Implementation:**
- CLI: Use `nix-store --export` to create NAR from derivation
- Protocol: `QueueBuildRequest.derivation` contains NAR bytes (not raw .drv)
- Agent: Pipe NAR bytes to `nix-store --import` to get canonical path
- Agent: Build using the imported store path

**Benefits:**
- ✅ Consistent with output transfer (both use NAR export/import)
- ✅ Derivation automatically placed at correct store path
- ✅ No manual path construction or hash computation
- ✅ Nix handles all validation

**Note:** Same mechanism used for both derivations and build outputs throughout Rio.

### 9. Build Dependencies

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

// Service exposed by agents to CLI clients and other agents
service RioAgent {
  // Cluster discovery - returns list of agents and current leader
  rpc GetClusterMembers(GetClusterMembersRequest)
      returns (ClusterMembers);

  // Build submission (leader only - queues work for Raft assignment)
  // Leader includes derivation NAR in BuildQueued command for Raft
  rpc QueueBuild(QueueBuildRequest)
      returns (QueueBuildResponse);

  // Subscribe to an in-progress build (for deduplication and redirects)
  // Agent streams logs and outputs to subscriber
  rpc SubscribeToBuild(SubscribeToBuildRequest)
      returns (stream BuildUpdate);

  // Get outputs from a recently completed build
  // Returns cached outputs from agent's /nix/store
  rpc GetCompletedBuild(GetCompletedBuildRequest)
      returns (stream BuildUpdate);

  // Build status queries (for failure recovery)
  // Check if a build is queued, building, completed, or not found
  rpc GetBuildStatus(GetBuildStatusRequest)
      returns (BuildStatusResponse);

  // Agent management (for joining cluster)
  // New agent calls this on seed agent to join Raft cluster
  rpc JoinCluster(JoinClusterRequest)
      returns (JoinClusterResponse);
}

// ============================================================================
// Cluster Discovery
// ============================================================================

message GetClusterMembersRequest {}

message ClusterMembers {
  repeated AgentInfo agents = 1;
  string leader_id = 2;  // Current Raft leader's agent ID (UUID string)
}

message AgentInfo {
  string id = 1;                        // UUID string (also Raft NodeId)
  string address = 2;                   // gRPC endpoint URL (e.g., "http://agent1:50051")
  repeated string platforms = 3;        // ["x86_64-linux", "i686-linux"]
  repeated string features = 4;         // ["kvm", "big-parallel"]
  AgentStatus status = 5;               // Available, Busy, or Down
  BuilderCapacity capacity = 6;         // Hardware specs
}

enum AgentStatus {
  AGENT_STATUS_AVAILABLE = 0;  // Idle, can accept builds (default)
  AGENT_STATUS_BUSY = 1;       // Executing one build (or has queued builds)
  AGENT_STATUS_DOWN = 2;       // Failed heartbeats
}

message BuilderCapacity {
  int32 cpu_cores = 1;
  int64 memory_mb = 2;
  int64 disk_gb = 3;
}

// ============================================================================
// Build Submission
// ============================================================================

// Build submission to leader
// CLI sends derivation NAR bytes and dependency list to leader
message QueueBuildRequest {
  string derivation_path = 1;                 // Full store path (e.g., /nix/store/abc-foo.drv)
  bytes derivation = 2;                       // NAR bytes from nix-store --export
  repeated string dependency_paths = 3;       // Paths of dependencies from neededBuilds
  string platform = 4;                        // e.g., "x86_64-linux"
  repeated string required_features = 5;      // e.g., ["kvm", "big-parallel"]
  optional int32 timeout_seconds = 6;         // Optional build timeout
}

message QueueBuildResponse {
  oneof result {
    BuildAssigned assigned = 1;               // Build assigned to agent
    AlreadyBuilding already_building = 2;     // Build already in progress
    AlreadyCompleted already_completed = 3;   // Build recently completed (in cache)
    NoEligibleAgents no_agents = 4;           // No agents match requirements
  }
}

message BuildAssigned {
  string agent_id = 1;          // Which agent was assigned this build
  string derivation_path = 2;   // Derivation store path (job identifier)
}

message AlreadyBuilding {
  string agent_id = 1;          // Which agent is currently building
  string derivation_path = 2;   // Derivation store path
}

message AlreadyCompleted {
  string agent_id = 1;          // Where outputs are stored
  string derivation_path = 2;   // Derivation store path
}

message NoEligibleAgents {
  string reason = 1;  // "No agents with platform x86_64-linux and features [kvm]"
}

// ============================================================================
// Build Updates (streaming)
// ============================================================================

message BuildUpdate {
  string derivation_path = 1;  // Job identifier (derivation store path)
  oneof update {
    LogLine log = 2;             // Build log line
    OutputChunk output_chunk = 3; // Compressed NAR chunk
    BuildCompleted completed = 4; // Build succeeded
    BuildFailed failed = 5;       // Build failed
  }
}

message LogLine {
  int64 timestamp = 1;  // Unix timestamp (milliseconds)
  string line = 2;      // Log line content (with newline)
}

message OutputChunk {
  bytes data = 1;              // Compressed NAR data (zstd by default)
  int32 chunk_index = 2;       // Sequence number for ordering
  bool last_chunk = 3;         // True if this is the final chunk
  CompressionType compression = 4;  // Compression algorithm used
}

enum CompressionType {
  COMPRESSION_TYPE_ZSTD = 0;   // Default compression (zstd level 3)
  COMPRESSION_TYPE_NONE = 1;   // No compression (for debugging)
}

message BuildCompleted {
  repeated string output_paths = 1;  // /nix/store paths (all outputs)
  int64 duration_ms = 2;             // Build duration in milliseconds
}

message BuildFailed {
  string error = 1;        // Error message
  optional string stderr = 2;  // Optional stderr capture
}

// ============================================================================
// Build Status Query
// ============================================================================

message GetBuildStatusRequest {
  string derivation_path = 1;  // Derivation store path
}

message BuildStatusResponse {
  string derivation_path = 1;
  BuildState state = 2;
  optional string agent_id = 3;  // Set if queued, building, or completed
  optional string error = 4;     // Set if failed
}

enum BuildState {
  BUILD_STATE_NOT_FOUND = 0;   // Build not known to cluster (default)
  BUILD_STATE_QUEUED = 1;      // Waiting for dependencies
  BUILD_STATE_BUILDING = 2;    // Currently executing
  BUILD_STATE_COMPLETED = 3;   // Successfully completed (in cache)
  BUILD_STATE_FAILED = 4;      // Build failed
}

// ============================================================================
// Cluster Membership
// ============================================================================

message JoinClusterRequest {
  AgentInfo agent_info = 1;  // New agent's info
}

message JoinClusterResponse {
  bool success = 1;
  string message = 2;  // Error message if success = false
}

// ============================================================================
// Build Subscription
// ============================================================================

message SubscribeToBuildRequest {
  string derivation_path = 1;  // Which build to subscribe to
}

message GetCompletedBuildRequest {
  string derivation_path = 1;  // Which completed build to fetch
}

// Note: Both SubscribeToBuild and GetCompletedBuild return stream BuildUpdate
```

**Output Compression:**

All build outputs are compressed with zstd (level 3) before streaming to reduce bandwidth usage:

- **10GiB output → ~2-4GiB transfer**: Typical compression ratio 2.5-5x for build artifacts
- **Agent side**: `nix-store --export | zstd -3 | chunk | gRPC stream`
- **CLI side**: Reassemble chunks → decompress → `nix-store --import`
- **Performance**: zstd level 3 provides good balance (~500MB/s compression, high ratio)
- **Backward compatible**: CompressionType enum allows future algorithms or uncompressed fallback

For large builds (>1GiB), this significantly reduces transfer time and agent bandwidth consumption.

**NAR Streaming Implementation Pattern:**

Streaming build outputs requires bridging blocking `nix-store --export` with async gRPC. We use the **channel bridge pattern** inspired by hydra-queue-runner:

```rust
// Agent-side: Stream outputs to CLI
async fn stream_build_outputs(
    store: nix_utils::LocalStore,
    output_paths: Vec<Utf8PathBuf>,
    build_update_tx: mpsc::Sender<BuildUpdate>,
) -> anyhow::Result<()> {
    // 1. Create unbounded channel for NAR chunks
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel();

    // 2. Spawn blocking task for nix-store --export (runs in separate thread pool)
    let export_task = tokio::task::spawn_blocking(move || {
        // Callback closure: called by Nix with each chunk
        let callback = move |data: &[u8]| {
            // Compress chunk with zstd
            let compressed = zstd::stream::encode_all(data, 3)?;

            // Send to channel (thread-safe, async-safe)
            chunk_tx.send(compressed).is_ok()
        };

        // Export NAR with callback (blocking)
        store.export_paths(&output_paths, callback)?;
        Ok::<(), anyhow::Error>(())
    });

    // 3. Async task: Forward chunks to gRPC stream
    let mut chunk_index = 0;
    let stream_task = async {
        while let Some(compressed_chunk) = chunk_rx.recv().await {
            build_update_tx.send(BuildUpdate {
                output_chunk: Some(OutputChunk {
                    data: compressed_chunk,
                    chunk_index,
                    last_chunk: false,
                    compression: CompressionType::Zstd as i32,
                }),
                ..Default::default()
            }).await?;

            chunk_index += 1;
        }
        Ok::<(), anyhow::Error>(())
    };

    // 4. Run both tasks concurrently, wait for completion
    futures::future::try_join(export_task.await?, stream_task).await?;

    // 5. Send final chunk marker
    build_update_tx.send(BuildUpdate {
        output_chunk: Some(OutputChunk {
            last_chunk: true,
            chunk_index,
            ..Default::default()
        }),
        ..Default::default()
    }).await?;

    Ok(())
}
```

**Key Components:**

1. **`spawn_blocking`**: Runs blocking Nix calls in tokio's dedicated thread pool (doesn't block async runtime)
2. **Callback closure**: `nix-store --export` calls this with each chunk as it's produced
3. **mpsc channel**: Thread-safe bridge between blocking export and async streaming
4. **`UnboundedReceiverStream`**: Wraps channel receiver as async `Stream` for gRPC
5. **`try_join`**: Both tasks run concurrently until both complete

**Benefits:**

- ✅ **Constant memory**: Only current chunk buffered (~1MB), not entire NAR
- ✅ **Non-blocking**: Export runs in separate thread, doesn't block tokio runtime
- ✅ **Streaming starts immediately**: No waiting for full export
- ✅ **Backpressure**: Channel naturally blocks if receiver is slow
- ✅ **Clean separation**: Sync (Nix C++) and async (gRPC) worlds never mix

**Log Streaming:**

Same pattern applies for build logs:

```rust
let (mut child, mut log_output) = nix_utils::realise_drv(&drv, &options).await?;

let log_stream = async_stream::stream! {
    while let Some(chunk) = log_output.next().await {
        match chunk {
            Ok(line) => yield BuildUpdate {
                log: Some(LogLine {
                    timestamp: now(),
                    line: format!("{line}\n"),
                }),
                ..Default::default()
            },
            Err(e) => {
                tracing::error!("Log read error: {e}");
                break;
            }
        }
    }
};

client.subscribe_to_build(Request::new(log_stream)).await?;
```

Using `async_stream::stream!` macro simplifies creating streams from async iterators.

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

### Compression

- `zstd` 0.13 for output compression
- Level 3 compression (good speed/ratio balance)
- Reduces large build output transfers by 2.5-5x

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
  raft.rocksdb/      # RocksDB with column families:
    logs/            # Raft log entries (includes BuildQueued with derivation NARs)
    store/           # Raft metadata (vote, committed, snapshots)
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

## Open Questions

### 1. Agent assignment model

**Decision: Raft state machine assigns builds deterministically**

- CLI submits to leader via QueueBuild RPC
- Leader proposes BuildQueued to Raft
- State machine on ALL agents runs same selection logic
- Deterministic assignment (affinity + tie-breaker)
- No races, no stale cluster state issues
- Simpler CLI, more robust

### 2. How long to cache cluster membership in CLI?

**Proposal: 60 seconds**

- Cache for 60s to avoid repeated GetClusterMembers calls
- Refresh on failure (if selected agent is down)
- Config option for cache TTL

### 3. Multi-output derivations are fully supported

Nix derivations commonly have multiple outputs: `out`, `dev`, `doc`, `man`, etc. Rio supports these naturally because:

1. **NAR format is multi-output aware**: `nix-store --export` takes multiple paths and produces a single interleaved NAR stream containing all outputs
2. **Import is automatic**: `nix-store --import` extracts all outputs from the stream automatically
3. **Protocol represents all outputs**: `BuildCompleted` has `repeated string output_paths` to list all outputs
4. **No special handling needed**: The streaming pipeline works identically whether exporting one output or fifty

Example:
```bash
# Agent exports multiple outputs in single stream
nix-store --export \
  /nix/store/abc-hello \
  /nix/store/def-hello-dev \
  /nix/store/ghi-hello-doc \
  | zstd -3 | gRPC stream

# CLI imports single stream, gets all outputs
gRPC stream | zstd -d | nix-store --import
# Result: All three outputs in /nix/store
```

### 4. How to handle concurrent builds on same agent?

**Decision: One build at a time per agent (MVP)**

- Agent status: Available or Busy (binary)
- Simple execution model
- Future: Add configurable concurrency with build slots

### 5. Should we persist build history?

**Decision: Short-term cache in Raft only (MVP)**

- Raft state machine keeps recent completed builds (5 minute LRU cache)
- Enough for build deduplication and status queries
- Future: External database for long-term history/analytics

### 6. Derivation storage and leader election ✅ RESOLVED

**Decision: Store derivations in Raft log itself**

Derivations are included in the BuildQueued command and replicated via Raft:
- BuildQueued includes `derivation_nar: Vec<u8>` field (~1-100 KB)
- Replicated to all agents via Raft consensus (gRPC compresses on wire)
- Stored durably in RocksDB on each agent (compressed at rest)
- Survives leader election, crashes, network partitions
- Removed on BuildCompleted/BuildFailed via state machine cleanup

**Benefits:**
- ✅ No volatile /tmp storage on leader
- ✅ No FetchPendingBuild RPC needed (simpler architecture)
- ✅ Enables queueing on Busy agents (all agents have derivation)
- ✅ Durable across leader elections
- ✅ gRPC + RocksDB compression handle size efficiently

**Size impact:**
- Typical derivation: 1-10 KB (compressed to ~300-3000 bytes on wire/disk)
- Large derivation: 100 KB (compressed to ~20-30 KB)
- Network: 3x amplification for 3-node cluster (acceptable for small NARs)
- Disk: Temporary, freed on BuildCompleted

**Comparison to alternatives:**
- Leader /tmp: Lost on election, requires FetchPendingBuild RPC
- CLI retry: Wastes network, poor UX
- Raft storage: Durable, simple, enables queueing

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
- [hydra-queue-runner](https://github.com/helsinki-systems/hydra-queue-runner) - Rust rewrite of Hydra's queue runner with gRPC (inspired NAR streaming pattern)

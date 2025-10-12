# Rio Design Document

## Overview

Rio is an open-source distributed build service for Nix that eliminates the traditional broker architecture by using a peer-to-peer agent cluster coordinated via Raft consensus.

## Architecture

**Brokerless Design:**

```
┌──────────────┐
│  rio-build   │ (CLI client)
│    (user)    │
└──────┬───────┘
       │ gRPC to any agent
       ▼
┌────────────────────────────────────┐
│      rio-agent cluster (Raft)     │
│                                    │
│  ┌────────┐  ┌────────┐  ┌────────┐
│  │ Agent  │←→│ Agent  │←→│ Agent  │
│  │   1    │  │   2    │  │   3    │
│  │(leader)│  │        │  │        │
│  └────────┘  └────────┘  └────────┘
└────────────────────────────────────┘
```

## Component Design

### rio-build (CLI)

The client tool that users run to execute builds.

**Responsibilities:**
- Parse Nix expressions and generate derivations
- Discover agent cluster members
- Select appropriate agent for build submission
- Stream build logs to user
- Receive and store build outputs

### rio-agent (Cluster Node)

Worker nodes that form a Raft cluster for coordination while executing builds.

**Responsibilities:**
- Participate in Raft consensus for cluster coordination
- Accept build requests from CLI clients
- Execute builds using local Nix
- Stream logs back to CLI
- Coordinate with peers for load balancing
- Report capacity and status

## Communication

**CLI ↔ Agent:** gRPC (direct connection, no broker)
**Agent ↔ Agent:** Raft consensus protocol + gRPC

## Discovery

CLI maintains a config file with seed agent addresses:
```toml
# ~/.config/rio/config.toml
agents = [
  "https://agent1.example.com:50051",
  "https://agent2.example.com:50051"
]
```

On connection, CLI queries any agent for full cluster membership via Raft state.

## Key Design Decisions

1. **No Broker** - Eliminates single point of failure and bottleneck
2. **Raft Consensus** - Proven distributed coordination without external dependencies
3. **Direct Data Flow** - Build artifacts stream directly from agent to CLI
4. **Own CLI** - Not constrained by Nix's remote builder protocol

## Implementation Status

🚧 **Clean slate** - Ready to design and implement brokerless architecture

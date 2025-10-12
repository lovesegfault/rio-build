# Rio

**Distributed builds for Nix** - A brokerless, peer-to-peer build service

Rio is a distributed build service for Nix that eliminates the traditional broker architecture by using a peer-to-peer agent cluster coordinated via Raft consensus.

## Architecture

Unlike traditional architectures with a central dispatcher, Rio uses a **brokerless design**:

```
┌──────────────┐
│  rio-build   │ (your CLI)
│   (client)   │
└──────┬───────┘
       │ gRPC to any agent
       ▼
┌────────────────────────────────────┐
│      rio-agent cluster (Raft)     │
│                                    │
│  ┌────────┐  ┌────────┐  ┌────────┐
│  │ Agent  │←→│ Agent  │←→│ Agent  │
│  │   1    │  │   2    │  │   3    │
│  └────────┘  └────────┘  └────────┘
└────────────────────────────────────┘
```

**Key Benefits:**
- No single point of failure
- No central bottleneck for build artifacts
- Horizontal scalability via Raft
- Direct CLI ↔ Agent communication

## Components

- **rio-build**: CLI client for submitting builds and streaming logs
- **rio-agent**: Cluster node that participates in Raft consensus and executes builds
- **rio-common**: Shared protocol definitions (gRPC)

## Status

🚧 **Clean slate** - Project restructured for brokerless architecture

**Next steps:**
- Design new gRPC protocol for CLI ↔ Agent communication
- Integrate Raft consensus library
- Implement cluster discovery and membership

## Getting Started

See [DESIGN.md](DESIGN.md) for comprehensive architecture documentation.

See [CLAUDE.md](CLAUDE.md) for development setup and commands.

See [TODO.md](TODO.md) for implementation roadmap.

## Quick Commands

```bash
# Enter development environment
nix develop

# Build all workspace members
cargo build

# Build specific binary
cargo build -p rio-build
cargo build -p rio-agent

# Run tests
cargo test

# Run clippy linter
cargo clippy

# Format code
nix fmt

# Full check (clippy + tests + docs + coverage)
nix flake check
```

## Future Vision

Once implemented, usage will look like:

```bash
# Configure agent cluster
rio-build config set agents https://agent1.example.com:50051,https://agent2.example.com:50051

# Submit a build
rio-build ./my-package.nix

# Agents discover each other via Raft
# Build executes on available agent
# Logs stream back to CLI
# Outputs stored locally
```

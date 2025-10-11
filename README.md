# Rio

**Distributed builds for Nix** - An open-source alternative to nixbuild.net

Rio is a build service for Nix that presents itself as a single remote builder while dispatching builds to an elastic fleet of workers.

## Overview

Rio acts as a remote builder endpoint for Nix, accepting SSH connections using the standard Nix protocols (`ssh://` and `ssh-ng://`). Behind the scenes, it intelligently dispatches builds to a pool of worker nodes, providing the illusion of "a single Nix machine with infinite CPUs."

## Architecture

- **rio-dispatcher**: Fleet manager and SSH frontend that Nix clients connect to
- **rio-builder**: Worker nodes that execute builds

```
Nix Client → (SSH) → rio-dispatcher → (gRPC) → rio-builder(s)
```

## Features

### Implemented ✅
- gRPC communication between dispatcher and builders
- Builder registration and heartbeat mechanism
- Builder pool management
- Multi-platform support detection (x86_64-linux, aarch64-linux, etc.)

### In Progress 🚧
- SSH server for Nix client connections
- Nix protocol handler using nix-daemon crate
- Build queue and scheduler
- Build execution on worker nodes

### Planned 📋
- Binary cache integration
- Build result caching
- Horizontal scaling of dispatcher
- Web UI for monitoring

## Getting Started

See [DESIGN.md](DESIGN.md) for comprehensive architecture documentation.

See [CLAUDE.md](CLAUDE.md) for development setup and commands.

## Status

🚧 **Early development** - Basic gRPC infrastructure is working!

**What's working:**
- ✅ Builders can register with dispatcher via gRPC
- ✅ Heartbeat mechanism keeps connection alive
- ✅ Builder pool tracks available builders

**What's next:**
- SSH server for Nix client connections
- Nix protocol integration
- Actual build execution

## Quick Test

Terminal 1 - Start dispatcher:
```bash
nix develop
cargo run -p rio-dispatcher
```

Terminal 2 - Start a builder:
```bash
nix develop
cargo run -p rio-builder
```

You should see the builder register and start sending heartbeats!

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
# or with custom settings:
cargo run -p rio-dispatcher -- --grpc-addr=0.0.0.0:50051 --log-level=debug
```

Terminal 2 - Start a builder:
```bash
nix develop
cargo run -p rio-builder
# or with custom settings:
cargo run -p rio-builder -- --platforms=x86_64-linux --features=kvm
```

You should see the builder register and start sending heartbeats!

## CLI Reference

Both binaries support configuration via CLI arguments or environment variables:

**rio-dispatcher:**
- `--grpc-addr` (env: `RIO_GRPC_ADDR`): gRPC server address (default: 0.0.0.0:50051)
- `--ssh-addr` (env: `RIO_SSH_ADDR`): SSH server address (default: 0.0.0.0:2222)
- `--ssh-host-key` (env: `RIO_SSH_HOST_KEY`): Path to SSH host key
- `--log-level` (env: `RIO_LOG_LEVEL`): Log level (default: info)

**rio-builder:**
- `--dispatcher-endpoint` (env: `RIO_DISPATCHER_ENDPOINT`): Dispatcher to connect to (default: http://localhost:50051)
- `--platforms` (env: `RIO_PLATFORMS`): Platforms to support, comma-separated (auto-detected if not provided)
- `--features` (env: `RIO_FEATURES`): Features to advertise, comma-separated
- `--log-level` (env: `RIO_LOG_LEVEL`): Log level (default: info)

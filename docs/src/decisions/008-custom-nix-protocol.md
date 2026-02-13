# ADR-008: Custom Nix Protocol Implementation

## Status
Accepted

## Context
rio-build must parse and generate the Nix wire protocol (daemon protocol), derivation files, NAR archives, store path computations, and nixbase32 encoding. Existing Rust implementations exist in the `nix-compat` crate (part of the Tvix/Snix ecosystem), which is licensed under GPL-3.0 (only the protobuf definitions are MIT-licensed).

## Decision
Implement the Nix wire protocol, derivation parsing, NAR format, store path computation, and nixbase32 from scratch in the `rio-nix` crate. This keeps rio-build MIT/Apache-2.0 dual-licensed.

References for implementation:
- Snix protocol documentation (protocol format descriptions).
- Tweag blog posts on the Nix daemon protocol.
- Nix C++ source code (`libstore/daemon.cc`, `libstore/remote-store.cc`).
- Protocol specification from the Nix manual.

The implementation targets protocol version 1.37+ only (see ADR-010), reducing the surface area by excluding legacy protocol paths.

## Alternatives Considered
- **Use nix-compat (GPL-3.0)**: The most complete Rust implementation of Nix protocols. Well-tested and actively maintained. However, GPL-3.0 is incompatible with the project's MIT/Apache-2.0 licensing goals --- any binary statically linking nix-compat would need to be GPL-3.0 licensed. Additionally, depending on nix-compat couples rio-build to the Tvix project's release cadence and design priorities, and limits our ability to optimize the protocol implementation for rio-build's specific needs (e.g., zero-copy NAR streaming, batch opcode handling).
- **Process-boundary isolation for GPL containment**: Run nix-compat in a separate process, communicating via IPC. Technically avoids GPL linking requirements. However, this adds latency on every protocol operation, complicates deployment, and the legal interpretation of GPL across process boundaries is debated.
- **Use the Nix C++ libraries via FFI**: Link against `libnixstore` and `libnixutil`. These are LGPL-3.0 (more permissive than GPL for dynamic linking) but bring a large C++ dependency, complicate cross-compilation, and tie the project to Nix's unstable internal C++ API.
- **Contribute to nix-compat and request relicensing**: Unlikely to succeed given the Tvix project's licensing choices. Would also create a dependency on an external project's release cadence.

## Consequences
- **Positive**: Clean MIT/Apache-2.0 licensing with no GPL dependencies.
- **Positive**: Full control over the protocol implementation, optimized for rio-build's specific needs.
- **Positive**: Targeting only modern protocol versions (1.37+) significantly reduces implementation scope.
- **Negative**: Significant upfront implementation effort for protocol, NAR, derivation parsing, and store path computation.
- **Negative**: Must track upstream Nix protocol changes independently. No shared maintenance with the Tvix/Snix ecosystem.
- **Negative**: Risk of subtle protocol bugs that nix-compat has already found and fixed.

# ADR-010: Protocol Version 1.35+ (Nix 2.18+ / Lix)

## Status
Accepted (revised from 1.37 floor — see Lix rationale below)

## Context
The Nix daemon protocol has evolved over many versions, accumulating legacy operations and compatibility shims. Supporting all protocol versions increases implementation and testing burden significantly. A minimum version must be chosen that balances compatibility with implementation simplicity.

## Decision
Target protocol version 1.35+, corresponding to Nix 2.18+ and **Lix** (which is policy-frozen at 1.35 — its `worker-protocol.hh` carries the comment *"must remain 1.35 forever in Lix, since the protocol has diverged in CppNix such that we cannot assign newer versions ourselves"*). rio-build advertises 1.38 and negotiates down.

The original floor was 1.37 for the features below; lowering to 1.35 admits Lix clients at the cost of one version-gated field pair (`BuildResult.cpu_user`/`cpu_system`, added in 1.37). All other features rio depends on predate 1.35:

- **Trusted user status in handshake** (1.35): Allows rio-gateway to communicate per-user permissions.
- **`wopBuildPathsWithResults`** (1.34): Returns structured build results instead of requiring separate queries.
- **`wopAddMultipleToStore`** (1.32): Batch path addition, reducing round trips for large closures.
- **CPU timing in BuildResult** (1.37): Gated — written only when the negotiated version is >= 1.37; Lix clients receive the 1.35 wire shape without these fields.

Clients running Nix < 2.18 (or any implementation < 1.35) receive a clear error message directing them to upgrade.

## Alternatives Considered
- **Support all protocol versions (1.10+)**: Maximum compatibility but requires implementing deprecated operations, version-specific code paths, and extensive testing across Nix versions spanning years of changes. The legacy operations (e.g., `wopBuildPaths` without results) would require additional round trips and workarounds.
- **Keep floor at 1.37**: Excludes Lix entirely. Lix is the primary alternative Nix implementation in active use; supporting it costs one `if version >= 1.37` gate.
- **Protocol version negotiation with deep fallbacks**: Implement the modern path plus legacy fallbacks for clients < 1.35. Doubles the protocol surface area and testing matrix without clear benefit, since 2.18 is the oldest Nix in any supported NixOS release.

## Version Compatibility

| Protocol Version | Nix Release | Key Features |
|-----------------|-------------|--------------|
| 1.35 | Nix 2.18 / Lix | **Minimum for rio-build:** trusted status in handshake; Lix's frozen version |
| 1.36 | Nix 2.19 | Minor fixes |
| 1.37 | Nix 2.20 | CPU timing in BuildResult (gated in rio) |
| 1.38 | Nix 2.21 | Feature exchange in handshake (gated in rio); **rio advertises this** |
| 1.39 | Nix 2.22 | Minor protocol refinements |
| 1.40 | Nix 2.23 | Additional BuildResult fields |
| 1.41 | Nix 2.24 | Latest stable at time of writing |

**NixOS distribution mapping:**

| NixOS Release | Nix Version | Protocol Version | Compatible? |
|--------------|-------------|-----------------|-------------|
| NixOS 23.05 | Nix 2.15 | 1.33 | No |
| NixOS 23.11 | Nix 2.18 | 1.35 | Yes |
| NixOS 24.05 | Nix 2.21 | 1.38 | Yes |
| NixOS 24.11 | Nix 2.24 | 1.41 | Yes |
| Lix (any) | — | 1.35 | Yes |

> **Note:** Protocol version numbers are approximate mappings based on Nix source history. The authoritative source is the `PROTOCOL_VERSION` constant in Nix's `worker-protocol.hh`. Users should verify their Nix version with `nix --version` and the protocol version via connection logs.

## Consequences
- **Positive**: Significantly smaller protocol implementation surface. No legacy operation support needed.
- **Positive**: Access to modern protocol features (batch operations, structured results) simplifies the gateway.
- **Positive**: Lix clients work as remote-builder/remote-store clients without modification.
- **Positive**: Nix 2.18 covers every NixOS release still in support.
- **Negative**: Users on Nix < 2.18 (e.g., NixOS 23.05 ships Nix 2.15) cannot use rio-build without upgrading Nix.
- **Negative**: One version-gated wire field (`cpu_user`/`cpu_system`) — first such gate in the codebase; future protocol additions above 1.35 will need the same treatment if they affect wire shape.

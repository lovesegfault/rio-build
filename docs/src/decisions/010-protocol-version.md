# ADR-010: Protocol Version 1.37+ (Nix 2.20+)

## Status
Accepted

## Context
The Nix daemon protocol has evolved over many versions, accumulating legacy operations and compatibility shims. Supporting all protocol versions increases implementation and testing burden significantly. A minimum version must be chosen that balances compatibility with implementation simplicity.

## Decision
Target protocol version 1.37+ only, corresponding to Nix 2.20+. This version includes several features that rio-build depends on:

- **CPU timing in BuildResult**: Enables accurate build time tracking for scheduling and analytics.
- **Trusted user status in handshake**: Allows rio-gateway to enforce per-user permissions based on the Nix client's trust level.
- **`wopBuildPathsWithResults`**: Returns structured build results instead of requiring separate queries. Simplifies the gateway's build result handling.
- **`wopAddMultipleToStore`**: Batch path addition, reducing round trips for large closures.

Clients running Nix < 2.20 receive a clear error message directing them to upgrade.

## Alternatives Considered
- **Support all protocol versions (1.10+)**: Maximum compatibility but requires implementing deprecated operations, version-specific code paths, and extensive testing across Nix versions spanning years of changes. The legacy operations (e.g., `wopBuildPaths` without results) would require additional round trips and workarounds.
- **Target Nix 2.24+ (latest stable)**: Even newer protocol features but would exclude users on LTS distributions that ship Nix 2.20-2.23. The marginal protocol improvements do not justify the compatibility loss.
- **Protocol version negotiation with fallbacks**: Implement the modern path plus legacy fallbacks for older clients. Doubles the protocol surface area and testing matrix without clear benefit, since organizations can standardize on a Nix version.

## Version Compatibility

| Protocol Version | Nix Release | Key Features |
|-----------------|-------------|--------------|
| 1.35 | Nix 2.18 | Baseline modern protocol |
| 1.36 | Nix 2.19 | Minor fixes |
| 1.37 | Nix 2.20 | **Minimum for rio-build:** CPU timing in BuildResult, trusted status in handshake |
| 1.38 | Nix 2.21 | `wopAddMultipleToStore` batching improvements |
| 1.39 | Nix 2.22 | Minor protocol refinements |
| 1.40 | Nix 2.23 | Additional BuildResult fields |
| 1.41 | Nix 2.24 | Latest stable at time of writing |

**NixOS distribution mapping:**

| NixOS Release | Nix Version | Protocol Version | Compatible? |
|--------------|-------------|-----------------|-------------|
| NixOS 23.05 | Nix 2.15 | 1.33 | No |
| NixOS 23.11 | Nix 2.18 | 1.35 | No |
| NixOS 24.05 | Nix 2.21 | 1.38 | Yes |
| NixOS 24.11 | Nix 2.24 | 1.41 | Yes |

> **Note:** Protocol version numbers are approximate mappings based on Nix source history. The authoritative source is the `PROTOCOL_VERSION` constant in Nix's `worker-protocol.hh`. Users should verify their Nix version with `nix --version` and the protocol version via connection logs.

## Consequences
- **Positive**: Significantly smaller protocol implementation surface. No legacy operation support needed.
- **Positive**: Access to modern protocol features (batch operations, structured results) simplifies the gateway.
- **Positive**: Nix 2.20 is over a year old; most active Nix users have upgraded.
- **Negative**: Users on older Nix versions (e.g., NixOS 23.05 ships Nix 2.15) cannot use rio-build without upgrading Nix.
- **Negative**: Creates a hard dependency on a specific Nix version, which must be documented clearly.

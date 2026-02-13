# ADR-001: Protocol-Level Integration

## Status
Accepted

## Context
rio-build needs a way for Nix clients to submit builds. The integration mechanism determines how transparent the system is to end users and how much DAG visibility the scheduler has. A seamless developer experience requires that existing `nix build` workflows work without modification.

## Decision
rio-build implements both the `ssh-ng://` remote store protocol and the build hook protocol (for `--builders`). Nix clients connect transparently without custom tooling.

The `ssh-ng://` path gives rio-build full DAG visibility: the client pushes the entire derivation closure, enabling global scheduling and deduplication. The build hook path provides per-derivation delegation, which is useful for compatibility with existing `nix.conf` setups and CI runners that already use `--builders`.

Both paths terminate at rio-gateway, which translates wire protocol operations into internal gRPC calls to the scheduler and store.

## Alternatives Considered
- **Custom REST/gRPC API with a CLI wrapper**: Would require users to install a separate tool and change their workflow. Loses the benefit of Nix's built-in remote build infrastructure. Would also require reimplementing dependency tracking that Nix already handles during the protocol exchange.
- **Nix remote builder over plain SSH (legacy protocol)**: The older `ssh://` protocol sends individual build requests without full closure information. This limits the scheduler's ability to reason about the full DAG and schedule optimally.
- **Nix plugin / post-build hook approach**: Hooks into Nix's build lifecycle via plugins. Fragile across Nix versions, requires per-client installation, and does not provide pre-build scheduling control.

## Consequences
- **Positive**: Zero-friction adoption. Any Nix user can point their store URI at rio-build and benefit immediately. Full DAG visibility enables intelligent scheduling and global deduplication.
- **Positive**: Two integration modes cover different deployment scenarios (dedicated remote store vs. auxiliary builder).
- **Negative**: Must implement and maintain the Nix wire protocol precisely, tracking upstream changes across Nix releases.
- **Negative**: The `ssh-ng://` path requires an SSH transport layer (or a shim), adding operational complexity.

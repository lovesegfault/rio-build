# ADR-004: CA-Ready Design

## Status
Accepted

## Context
Content-addressed (CA) derivations are an experimental Nix feature that enables early cutoff optimization: if a derivation's output is identical to a previous build, downstream rebuilds are skipped. Most of nixpkgs is input-addressed today, but CA adoption is expected to grow. The data model must support both modes without a later migration.

## Decision
The data model is CA-ready from Phase 2c. This means:

- PostgreSQL tables include content-indexed lookups and a `realisations` table mapping `(drv_hash, output_name)` to `(output_path, output_hash)`.
- Gateway stubs for `wopRegisterDrvOutput` and `wopQueryRealisation` write and read this metadata.
- Input-addressed derivations remain the primary execution path.
- CA cutoff-compare is implemented ([P0251](../../.claude/work/plan-0251-ca-cutoff-compare-completion.md)); propagate + DAG cascade is [P0252](../../.claude/work/plan-0252-ca-cutoff-propagate-skipped.md); CA derivation resolution (rewriting `inputDrvs` to replace placeholder output paths with realized paths) is [P0253](../../.claude/work/plan-0253-ca-resolution-inputdrvs-rewrite.md).

## Alternatives Considered
- **Input-addressed only, add CA later**: Simpler initially but risks a painful schema migration. CA support touches the store, scheduler, and protocol layers. Retrofitting it would require coordinated changes across all components.
- **CA-first from day one**: Would require implementing derivation resolution, output rewriting, and early cutoff before the basic build pipeline works. Too much complexity upfront given that CA derivations are still experimental in Nix (2026).
- **Ignore CA entirely**: Viable short-term but limits the system's longevity. As nixpkgs moves toward CA, a build backend that cannot exploit early cutoff will do significantly more redundant work.

## Consequences
- **Positive**: Schema and protocol are forward-compatible. CA support activates without a data migration.
- **Positive**: Input-addressed builds work immediately; CA is an incremental addition.
- **Negative**: The Phase 2c data model is slightly more complex than a pure input-addressed design.
- **Negative**: CA early cutoff (Phase 5) depends on upstream Nix CA stabilization. If CA remains experimental indefinitely, the investment has limited return.

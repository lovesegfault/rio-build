# ADR-002: Evaluation Is External

## Status
Accepted

## Context
Nix builds have two phases: evaluation (turning `.nix` expressions into derivations) and execution (building those derivations). A build service must decide whether to handle evaluation, execution, or both.

## Decision
Nix handles evaluation. rio-build receives derivations via the protocol and orchestrates distributed execution only. Evaluation scheduling and VCS integration are explicitly out of scope. Users run `nix-eval-jobs`, `nix build`, or CI orchestrators to produce derivations, which are then submitted to rio-build.

This cleanly separates concerns: the Nix evaluator is a complex, rapidly evolving component with IFD (import-from-derivation) semantics, flake resolution, and channel handling. rio-build focuses on what it can do better than existing tools: distributed, scheduled, cached execution.

## Alternatives Considered
- **Full eval + build service (Hydra model)**: Hydra combines evaluation and building. This requires tracking the Nix evaluator's behavior, managing evaluation workers, handling IFD scheduling, and dealing with evaluation memory/time limits. Significant complexity for a component that Nix already provides.
- **Embedded evaluator via nix-eval-jobs**: Bundling `nix-eval-jobs` inside rio-build would give a turnkey experience but couples the release cycle to nix-eval-jobs, introduces a native dependency, and blurs the service boundary.
- **Accepting Nix expressions directly (eval-on-submit)**: Would require sandboxed evaluation infrastructure, creating a second scheduling dimension. Evaluation resource requirements differ fundamentally from build requirements.

## Consequences
- **Positive**: Dramatically simpler system. The scheduler only reasons about derivation DAGs, not Nix expressions.
- **Positive**: Users keep control over evaluation (pinned Nix versions, custom evaluator flags, IFD policies).
- **Negative**: Users must run evaluation themselves before submitting to rio-build. This adds a step compared to Hydra's "point at a flake" model.
- **Negative**: Cannot optimize across the eval-build boundary (e.g., cancelling evaluation early if a build fails).

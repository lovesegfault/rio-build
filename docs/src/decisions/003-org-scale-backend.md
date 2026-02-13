# ADR-003: Org-Scale Build Backend

## Status
Accepted

## Context
The Nix ecosystem lacks a production-grade, multi-tenant build backend. Hydra is single-user oriented and monolithic. Buildbot-nix and ofborg serve specific niches. Organizations running Nix at scale need shared build infrastructure with isolation, observability, and API access across teams.

## Decision
rio-build is a multi-project, multi-user, persistent service with observability, a web dashboard, and an API. It replaces Hydra's build execution and binary cache layers. It does not replace evaluation scheduling or VCS integration, which are handled by external orchestrators (GitHub Actions, `nix-eval-jobs`, etc.).

The service is designed for org-scale: multiple teams sharing a build cluster, with per-project isolation, priority scheduling, and a unified binary cache.

## Alternatives Considered
- **Hydra with patches**: Hydra's architecture (single-machine evaluator, Perl/C++ codebase, local nix-daemon builds) does not scale horizontally. Patching it for multi-node distributed builds would require rewriting most of the system.
- **Buildbot-nix**: A thin wrapper around Buildbot for Nix CI. Suitable for single-project CI but lacks shared binary cache semantics, multi-tenancy, and Nix protocol integration.
- **Nix remote builders via SSH (ad-hoc)**: Works for small teams but provides no scheduling intelligence, no shared state, no dashboard, and no build deduplication across users.
- **Cachix Deploy or similar SaaS**: Outsources the problem but creates vendor lock-in and may not meet data residency or customization requirements.

## Consequences
- **Positive**: Organizations get a single, shared build backend with proper multi-tenancy, replacing ad-hoc SSH builder configurations.
- **Positive**: Unified binary cache across all builds eliminates redundant work.
- **Negative**: Higher operational complexity than single-user solutions. Requires PostgreSQL, object storage, and Kubernetes.
- **Negative**: Scope is large; phased delivery is essential to avoid over-engineering early phases.

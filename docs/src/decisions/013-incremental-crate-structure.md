# ADR-013: Incremental Crate Structure

## Status
Accepted

## Context
Rust workspace organization affects build times, API boundaries, and team development velocity. The final architecture envisions 9 crates, but defining all crate boundaries upfront risks premature abstraction. Module boundaries often shift during early implementation as the design is validated against real-world constraints.

## Decision
Start with 3 crates:

- **`rio-nix`**: Nix protocol, derivation parsing, NAR format, store path computation.
- **`rio-build`**: Scheduler, gateway, worker orchestration (single binary initially).
- **`rio-proto`**: Protobuf/gRPC definitions and generated code.

Split into the full multi-crate structure as module boundaries are validated through implementation. The planned eventual structure adds `rio-store`, `rio-gateway`, `rio-scheduler`, `rio-builder`, and `rio-controller` as independent crates. (FUSE was originally planned as a separate `rio-fuse` crate but shipped as a module inside `rio-builder`.)

Splitting criteria: extract a crate when its API surface stabilizes, when it needs independent testing, or when build times justify parallel compilation.

## Alternatives Considered
- **Full 9-crate structure from the start**: Clean separation from day one. However, early-stage code frequently moves between modules as designs evolve. Refactoring across crate boundaries (moving types, changing visibility) is significantly more friction than refactoring within a single crate.
- **Single crate (monolith)**: Minimum ceremony. But loses the ability to enforce API boundaries at compile time, makes it harder to reason about dependencies, and build times scale poorly as the codebase grows.
- **Two crates (library + binary)**: Common Rust pattern. Provides one split point but does not separate the protocol implementation from the build logic. `rio-nix` has fundamentally different dependencies and concerns from the build system.

## Consequences
- **Positive**: Fast iteration in early phases. Module boundaries can shift without cross-crate refactoring overhead.
- **Positive**: `rio-nix` is isolated from the start, enabling independent testing of protocol correctness.
- **Positive**: Clear migration path: extract crates one at a time as boundaries solidify.
- **Negative**: Temporary coupling between components that will eventually be separate crates. Must be disciplined about module-level separation within `rio-build` even before crate extraction.
- **Negative**: The split points are judgment calls. Splitting too early or too late both have costs.

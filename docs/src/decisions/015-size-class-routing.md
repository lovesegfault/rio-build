# ADR-015: Size-Class Routing with Adaptive Cutoffs

## Status
Accepted

## Context

Nix build workloads are heavy-tailed: most derivations complete in seconds (shell scripts, trivial packages), but a small fraction consume hours of CPU and gigabytes of RAM (GCC, LLVM, Firefox, NixOS system closures). The current design uses a single `WorkerPool` CRD with uniform pod sizing, meaning every worker is provisioned for worst-case resource demands. This wastes resources when running many small builds and may still be insufficient for the largest.

[Harchol-Balter's "Task Assignment with Unknown Duration" (JACM 2002)](https://dl.acm.org/doi/10.1145/506147.506154) demonstrates that under heavy-tailed distributions, size-based task routing (SITA-E) dramatically outperforms uniform assignment by isolating short jobs from long jobs on separate, right-sized hosts. The TAGS variant handles the case where job sizes are unknown a priori by using duration estimates with retroactive correction.

rio-build already has the building blocks: per-derivation EMA duration estimation via `build_history`, closure-size-based fallbacks for unknown derivations, and a worker scoring algorithm that could be extended with size-class filtering.

## Decision

Introduce a `WorkerPoolSet` CRD that defines multiple size-class worker pools (e.g., small/medium/large) with different resource allocations and concurrency limits. The scheduler classifies derivations into size classes based on estimated duration and routes them to the appropriate pool.

Key design choices:

1. **SITA-E for cutoff equalization:** Cutoff boundaries between size classes are automatically adjusted to equalize load across pools, using historical build duration data. Operators provide initial cutoffs; the system learns optimal values over time.

2. **Passive misclassification (not TAGS kill/restart):** When a build exceeds 2x its class cutoff, it is marked as misclassified and the EMA estimate is updated with a penalty. The build continues on its current worker because Nix builds are non-preemptible --- killing and restarting wastes all prior work and produces an identical result (deterministic builds).

3. **Resource-aware class bumping:** Beyond duration, the scheduler tracks peak memory, CPU, and output size per `(pname, system)`. A derivation that fits the "small" duration class but historically OOM-kills on small workers is bumped to "medium."

## Alternatives Considered

- **Uniform pools (current design):** Simple but wasteful. Over-provisioning every worker for the worst case means 90% of builds use 10% of their allocated resources. Under-provisioning risks OOM kills for the largest builds.

- **Pure TAGS (kill/restart on class boundary):** TAGS kills jobs that exceed the class cutoff and restarts them on a larger host. This works for preemptible web requests but not for Nix builds --- a killed 30-minute GCC build must restart from scratch, wasting the entire prior computation. Deterministic builds produce the same output on any host, so restarting provides no benefit.

- **Manual operator-defined cutoffs only:** Simpler than adaptive learning, but requires operators to know the build duration distribution a priori. This breaks down as the package set changes over time and differs between deployments.

- **Per-derivation resource requests (Kubernetes VPA-style):** Each derivation would carry its own resource request, and the scheduler would bin-pack onto workers. More granular than size classes but much more complex (requires per-derivation resource prediction, dynamic pod resizing, and tighter scheduler/K8s integration). Size classes are a simpler 80/20 solution.

## Consequences

- **Positive:** Resource utilization improves dramatically. Small workers with high concurrency (8 builds/pod) handle the bulk of trivial derivations at low cost. Large workers with dedicated resources handle the long tail without OOM risk.
- **Positive:** Automatic cutoff learning adapts to workload changes without operator intervention.
- **Positive:** Misclassification is self-correcting via EMA feedback --- each mistake improves future routing.
- **Negative:** Adds a new CRD (`WorkerPoolSet`) and reconciler complexity to the controller.
- **Negative:** Cold start: with no build history, all derivations use operator-configured cutoffs or the default fallback (30s). Cutoff quality improves with data.
- **Negative:** Size-class boundaries create potential for queue imbalance: if all ready derivations are "medium" but only "small" workers are idle, the derivations wait even though resources exist. Mitigation: allow overflow routing (small class can spill to medium workers when small queue is empty).

# Quint checks for formal protocol models in docs/spec/models/.
#
# Each model is a Quint specification (https://quint-lang.org — TLA+
# semantics behind a typed, effect-checked, programmer-facing syntax) of
# a distributed protocol that the prose spec in docs/spec/components/
# describes normatively. `quint verify` explores the state space and
# asserts the named invariants; a violation prints a step-by-step
# counterexample trace to the build log and fails the check.
#
# Why Quint next to Kani: nix/kani.nix proves *Rust-code* properties
# ("no panic, no overflow, postcondition holds") over rio-lease's
# functions. Quint proves *protocol* properties ("no two nodes ever both
# think they hold the lease") over the abstract distributed algorithm —
# the thing the Rust implements but cannot itself observe across nodes.
# Same crate, two complementary proof obligations. (Quint replaces the
# hand-written TLA+ in nix/tla.nix; during the migration both toolchains
# coexist and the TLA+ checks remain authoritative until the Quint port
# reproduces every documented result.)
#
# Backends — why the default is `tlc`, not quint's own default:
#   - `tlc`: transpiles the spec to TLA+ and runs TLC's parallel BFS over
#     the FULL reachable state space — the same checker, the same
#     exhaustive guarantee, and the same all-cores parallelism as
#     nix/tla.nix's mkTlcCheck. This is the CI backend.
#   - `apalache` (quint's own default if no --backend is given — which is
#     exactly why this constructor must default to `tlc`): bounded
#     symbolic model checking via one Z3 context, incrementally unrolled.
#     Single-threaded, and its cost is superlinear in the step bound —
#     measured at adoption time as an order of magnitude slower than the
#     tlc backend at its default 10-step bound and unable to complete a
#     20-step bound on a spec the tlc backend exhausts in under a second
#     (the figures are in the evaluation record and the introducing
#     commits). It proves "no violation within maxSteps steps", not "no
#     violation". Use it only for a property that is shallow by
#     construction, or in the dev loop where finding a shallow
#     counterexample in seconds beats waiting for the exhaustive run.
#
# The tlc backend requires a FINITE state space: every variable that can
# grow without bound (a counter, a resourceVersion, a clock) needs a
# ceiling as an action PRECONDITION in the .qnt itself (see cas.qnt's
# MAX_RV). Without one TLC never terminates. Apalache does not need the
# ceiling (it bounds by step count) but the ceiling must be present
# anyway so the same spec is checkable under both backends.
#
# Where the tools come from: quintPkg's bin/quint is a wrapper that puts
# its own JRE on PATH and sets QUINT_HOME to the package's share/quint,
# which carries the Apalache 0.56.1 distribution as a store path. The
# tlc backend runs `java -cp <QUINT_HOME>/apalache-dist-*/apalache/lib/
# apalache.jar tlc2.TLC` — TLC ships inside the Apalache jar, so neither
# backend needs pkgs.tlaplus, a network connection, or a writable HOME.
# The apalache backend starts a gRPC server on a fixed port (8822); the
# nix sandbox's private network namespace means parallel apalache-backend
# checks on one builder cannot collide on it.
#
# Caching: the .qnt model is the only eval-time input. Editing it
# rehashes the derivation and re-runs the checker. State space is
# bounded via the ceilings in the .qnt. A check around ~1min needs no
# further optimization; there is no hard ceiling, but a model over ~5min
# should be tuned if that's possible without losing the interleavings
# that make its invariants non-vacuous. A correct slow check beats a
# fast faulty one — never shrink constants past the point where a
# deliberately-weakened test stops producing its counterexample. A
# genuinely unbounded model belongs in a manual `packages.*` target,
# not here.
#
# r[verify ...] markers live HERE at the wiring point, not in the .qnt
# files — same discipline as nix/tla.nix, nix/kani.nix's `kani-checks`
# attrset, and nix/tests/default.nix's `subtests` entries: a marker at
# the wiring point structurally proves the check is built; a marker in
# the .qnt header would claim "verified" even if this file's attr were
# deleted. .config/tracey/config.styx must list nix/quint.nix under
# `test_include` for tracey to scan the markers.
#
# Gating: each model entry below is wrapped in `lib.optionalAttrs
# (builtins.pathExists …)` so the check only appears in checks.* once
# the .qnt file lands — same trade-off as nix/tla.nix (the wiring +
# markers can land first and the model commit turns the check on
# without an intermediate red gate).
{
  pkgs,
  lib,
  unfilteredRoot,
  # Quint 0.32.0 + the bundled Apalache dist, evaluated from the
  # `nixpkgs-quint` flake input rather than `pkgs` (the primary nixpkgs
  # only has 0.30.0, which has no Apalache and tries to download one at
  # runtime — impossible in the sandbox). See the input's comment in
  # flake.nix for when this goes away.
  quintPkg,
}:
let
  modelsDir = unfilteredRoot + "/docs/spec/models";

  # One `quint verify` run per (model, main-module, invariant-set,
  # backend) tuple. `main` selects the module within the file — a model
  # with several fault regimes declares one core module plus one thin
  # instantiation module per regime, and each regime is its own check so
  # a regression in the core protocol surfaces in the small fast check
  # instead of buried in the largest one (the same role nix/tla.nix's
  # `config` argument plays for .cfg files).
  mkQuintCheck =
    {
      name,
      spec,
      main ? spec,
      invariants,
      backend ? "tlc",
      # apalache backend only: the step bound. Not passed to the tlc
      # backend — TLC's exhaustive BFS has no step bound and feeding it
      # one would silently weaken the check's guarantee.
      maxSteps ? 10,
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ quintPkg ];
        # Only the one .qnt file. A model that imports another file
        # (a shared harness, a Choreo vendored module) extends the
        # fileset here — keeping it narrow means an unrelated docs/
        # edit doesn't re-run every quint check.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = modelsDir + "/${spec}.qnt";
        };
        # Surfaced in `nix log` and error messages.
        env.MODEL = spec;
        env.MAIN = main;
      }
      ''
        set -euo pipefail
        # Both backends write working files under the cwd (apalache:
        # `_apalache-out/`; tlc: a generated .tla/.cfg temp tree) and
        # $src is read-only — run from the writable scratch dir and
        # reference the spec by absolute path.
        cd "$TMPDIR"

        # Bound the checker to the cores Nix actually allotted this
        # build. The tlc backend's default is every visible host core,
        # which oversubscribes badly under nix-fast-build's parallel
        # checks gate (each TLC instance thinks it owns the box).
        # NIX_BUILD_CORES=0 means "all"; TLC's `auto` does the same, so
        # pass it through. Same shape as nix/tla.nix's -workers cap.
        # --tlc-config accepts only JVM/runtime knobs ({workers,
        # maxHeap, stackSize}) — it is NOT a .cfg-directive escape
        # hatch; everything else lives in the .qnt.
        workers="''${NIX_BUILD_CORES:-1}"
        [ "$workers" = "0" ] && workers='"auto"'
        printf '{"workers": %s}\n' "$workers" > tlc-config.json

        # The transcript is the proof artifact: it records the
        # invariants checked, the state count, and the depth — keep it.
        # `quint verify` exits nonzero on a violation, runCommand fails,
        # the check is red. (`| tee` is pipefail-safe — tee consumes
        # everything; never replace it with `| head`, see
        # .claude/rules/ci-failure-patterns.md.)
        quint verify \
          --backend=${backend} \
          --main=${main} \
          --invariant=${lib.concatStringsSep "," invariants} \
          ${
            if backend == "tlc" then "--tlc-config=tlc-config.json" else "--max-steps=${toString maxSteps}"
          } \
          "$src/${spec}.qnt" 2>&1 | tee $out
      '';
in
{
  # Expose the constructor so a future cross-model aggregate (or an
  # ad-hoc spike) can build its own checks without going through the
  # gated attrs below.
  inherit mkQuintCheck;

  # Per-model checks, gated on the .qnt file existing. Spliced into
  # checks.* via misc-checks.nix — `lib.optionalAttrs` makes the attr
  # absent (not present-and-failing) until the model lands.
  checks =
    lib.optionalAttrs (builtins.pathExists (modelsDir + "/cas.qnt")) {
      # The CAS fragment of the leader-election protocol: the rv-guarded
      # apiserver PUT admits at most one writer per resourceVersion (the
      # hard half of the at-most-one-leader rule — the half the apiserver's
      # optimistic concurrency makes unconditional). Cross-validated
      # against the TLA+ reference, which carries the same marker until
      # the migration replaces it. This check doubles as the permanent
      # quint-toolchain smoke test: it is by far the smallest model here,
      # so "the quint toolchain is broken" is caught at merge-gate rather
      # than at the next model change, the same role cov-smoke plays for
      # the coverage pipeline. The second invariant exists to keep the
      # multi-invariant (--invariant=a,b) code path exercised.
      # r[verify sched.lease.at-most-one-leader+3]
      quint-cas-smoke = mkQuintCheck {
        name = "cas-smoke";
        spec = "cas";
        invariants = [
          "atMostOneCASWinner"
          "rvInBounds"
        ];
      };
    }
    // lib.optionalAttrs (builtins.pathExists (modelsDir + "/leaderElection.qnt")) {
      # rio-lease's leader-election protocol over a Kubernetes Lease
      # object: per-node clocks (bounded skew), the observed-record
      # staleness clock, local self-fencing, crash/recovery, and the
      # write-ahead generation claim. The Quint port of the TLA+ model
      # (which carries the same markers until the migration deletes it).
      # The base regime disables every operator/infrastructure fault so a
      # regression in the core protocol surfaces in this check rather than
      # buried in the larger fault-injection regimes.
      #
      # It verifies atMostOneCASWinner (the apiserver admits at most one
      # writer per resourceVersion), boundedDualLeadership (every
      # dual-belief state has a discovery mechanism armed),
      # staleLeaderHasStaleGeneration (concurrent believers always have
      # distinct generations -- the bridge to the executor-side generation
      # fence; the invariant the pre-fix protocol falsified), plus the
      # loopInterval and clockSkewBound precondition tripwires and the
      # boundsOK ceiling tripwire. The fetch-max-seed marker covers the
      # seeding-and-claiming encoding inside steal (the generation derives
      # from the lease's transition count and the PG floor advances at
      # acquisition time).
      #
      # The state count, depth, and wall-clock are in this check's output
      # transcript and in the commit that introduced (or last re-measured)
      # the model -- never here. The durable claim: the port's state count
      # is exactly |NODES|! times the symmetry-reduced TLA+ predecessor's,
      # minus the self-symmetric states -- the structural-identity
      # evidence that the two state spaces are the same space.
      # r[verify sched.lease.at-most-one-leader+3]
      # r[verify sched.lease.k8s-lease]
      # r[verify sched.recovery.fetch-max-seed+2]
      quint-leader-election = mkQuintCheck {
        name = "leader-election";
        spec = "leaderElection";
        main = "leaderElectionBase";
        invariants = [
          "boundsOK"
          "clockSkewBound"
          "atMostOneCASWinner"
          "loopInterval"
          "boundedDualLeadership"
          "staleLeaderHasStaleGeneration"
        ];
      };

      # The same model with the Lease-object-deletion fault enabled
      # (MAX_DELETES = 1): `kubectl delete lease` resets the
      # leaseTransitions counter the generation derives from — the one
      # fault that re-arms the generation-collision failure mode the
      # transition-count derivation closed — while PG survives. The
      # write-ahead claim (the generation is durably recorded in PG's
      # claims ledger inside the acquisition step, before dispatch is
      # ungated) is what survives it: the checker proves
      # staleLeaderHasStaleGeneration still holds with the fault
      # injected, and the deliberately-weakened run (the claim reverted
      # to the pre-claim protocol) is falsified with a shallow
      # steal-delete-steal trace — two believers at the same generation.
      # See leaderElectionDeletion's module comment for the red-first
      # procedure and the rest of the regime's non-vacuity evidence.
      # r[verify sched.lease.generation-claim]
      quint-leader-election-deletion = mkQuintCheck {
        name = "leader-election-deletion";
        spec = "leaderElection";
        main = "leaderElectionDeletion";
        invariants = [
          "boundsOK"
          "clockSkewBound"
          "atMostOneCASWinner"
          "loopInterval"
          "boundedDualLeadership"
          "staleLeaderHasStaleGeneration"
        ];
      };

      # The same model under the asymmetric-TTL constants — THE HEALTHY
      # REGIME. The base and deletion regimes keep zero fence/steal
      # separation and model the degraded case, where dual belief is
      # reachable and boundedDualLeadership (every dual-belief state has
      # a discovery mechanism armed) is the operative property. This
      # regime gives the model the separation the asymmetric-TTL change
      # gives production (fence at LEASE_TTL - FENCE_MARGIN, steal at
      # LEASE_TTL + FENCE_MARGIN), and the operative property upgrades to
      # neverDual: no two replicas ever simultaneously believe they lead,
      # over the full state space. neverDual holds iff
      # STEAL_AFTER - FENCE_AFTER >= RENEW + 2*MAX_SKEW (production:
      # 2*FENCE_MARGIN >= RENEW_INTERVAL + 2*clock_skew — an 8s
      # separation against a 5s renew interval leaves a 1.5s skew
      # budget, which is rio-lease's compile-time assertion). The
      # boundary is measured from both sides — one tick less separation
      # and neverDual is violated — and the second-acquisition
      # reachability probe proves the contention it constrains actually
      # happens. See leaderElectionAsymmetric's module comment for the
      # boundary procedure and the non-vacuity evidence; the measured
      # depths are in the introducing commit. neverDual is the
      # verification of both the self-fence ordering claim (the victim
      # has provably stopped believing before any thief steals) and the
      # at-most-one-leader soft half (the dual-belief window is empty
      # under bounded skew).
      # r[verify sched.lease.at-most-one-leader+3]
      # r[verify sched.lease.self-fence+2]
      quint-leader-election-asymmetric = mkQuintCheck {
        name = "leader-election-asymmetric";
        spec = "leaderElection";
        main = "leaderElectionAsymmetric";
        invariants = [
          "boundsOK"
          "clockSkewBound"
          "atMostOneCASWinner"
          "loopInterval"
          "boundedDualLeadership"
          "staleLeaderHasStaleGeneration"
          "neverDual"
        ];
      };

      # The same model with both PostgreSQL-side faults enabled and the
      # Lease-deletion fault disabled (MAX_CLAIM_FAILURES = 1,
      # MAX_RESTORES = 1, MAX_DELETES = 0): a claim INSERT that fails
      # between the seed read and the claim write while recovery proceeds
      # anyway (the production proceed-on-failure path), and a
      # point-in-time restore that regresses the floor to zero. Every
      # invariant still holds: the Lease object's transition count is an
      # independent epoch source, so each PG fault alone is survivable —
      # the Lease and the PG ledger are REDUNDANT epoch sources and a
      # generation collision requires destroying both. The boundary is
      # measured: re-enabling the deletion fault alongside either PG
      # fault violates staleLeaderHasStaleGeneration with a shallow
      # steal-delete-steal trace (see leaderElectionPgFaults's module
      # comment for the conjunction-evidence procedure; the trace
      # summaries and depths are in the introducing commit). Those
      # conjunctions are the documented, accepted residuals of the
      # proceed-on-failure choice and of relying on PG as the
      # post-deletion backstop; this check pins the claim that they are
      # the ONLY ways a PG-side fault reaches a collision.
      # r[verify sched.lease.generation-claim]
      quint-leader-election-pg-faults = mkQuintCheck {
        name = "leader-election-pg-faults";
        spec = "leaderElection";
        main = "leaderElectionPgFaults";
        invariants = [
          "boundsOK"
          "clockSkewBound"
          "atMostOneCASWinner"
          "loopInterval"
          "boundedDualLeadership"
          "staleLeaderHasStaleGeneration"
        ];
      };
    };
}

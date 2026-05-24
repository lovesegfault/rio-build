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
# Same crate, two complementary proof obligations. (The models here
# replaced a hand-written TLA+ toolchain; the port was validated against
# every documented result of its predecessor before the TLA+ artifacts
# were removed — the migration commits carry the evidence.)
#
# Backends — why the default is `tlc`, not quint's own default:
#   - `tlc`: transpiles the spec to TLA+ and runs TLC's parallel BFS over
#     the FULL reachable state space — an exhaustive check of every
#     reachable state, parallel across every core the build is allotted.
#     This is the CI backend.
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
# ceiling as an action PRECONDITION in the .qnt itself (see the MAX_RV /
# MAX_TIME / MAX_GEN preconditions in leaderElection.qnt). Without one
# TLC never terminates. Apalache does not need the ceiling (it bounds by
# step count) but the ceiling must be present anyway so the same spec is
# checkable under both backends.
#
# Where the tools come from: pkgs.quint's bin/quint is a wrapper that puts
# its own JRE on PATH and sets QUINT_HOME to the package's share/quint,
# which carries the bundled Apalache distribution as a store path. The
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
# files — same discipline as nix/kani.nix's `kani-checks` attrset and
# nix/tests/default.nix's `subtests` entries: a marker at
# the wiring point structurally proves the check is built; a marker in
# the .qnt header would claim "verified" even if this file's attr were
# deleted. .config/tracey/config.styx must list nix/quint.nix under
# `test_include` for tracey to scan the markers.
#
# The checks reference their .qnt files unconditionally: a missing or
# renamed model file is a loud eval/build failure, never a silently
# absent check. (The bootstrap-era `lib.optionalAttrs (pathExists …)`
# gating that let the wiring land before the model was dropped once
# every wired model existed — a gated check whose model vanishes
# disappears from checks.* instead of failing, which converts "the
# proof is gone" into a silent coverage loss.)
{
  pkgs,
  lib,
  unfilteredRoot,
  # nix/checks.nix's nextest reuse-build helpers and rio-lease's prebuilt
  # test binary, threaded through misc-checks.nix. Only the mbt-rio-lease
  # conformance check below consumes them — the model checks need
  # nothing from the Rust build.
  mkNextestRun,
  mkNextestMeta,
  rioLeaseTestBin,
}:
let
  modelsDir = unfilteredRoot + "/docs/spec/models";

  # The leader-election model as its own single-file store path (the
  # same narrowing mkQuintCheck's src uses): the conformance check
  # depends on the model's content, not on whatever tree unfilteredRoot
  # points at — an unrelated docs/ edit must not re-run it, a model edit
  # must.
  leaseModel = lib.fileset.toSource {
    root = modelsDir;
    fileset = modelsDir + "/leaderElection.qnt";
  };

  # The invariant set every log-buffer lifecycle regime check asserts:
  # the five model-A invariants of the build-log verification design,
  # the three phase-3 calibration invariants (the stale-seal muting dual
  # of the binding gate, the frozen-row immutability half of
  # obs.log.finalize-immutable, and the stored-coverage monotonicity
  # half of obs.log.stored-coverage-preserved — each added because a
  # historical fix's calibration override produced a harmful state no
  # original invariant observed), plus the boundsOK ceiling tripwire.
  # One list so the regimes cannot silently drift apart on which
  # properties they prove.
  logInvariants = [
    "boundsOK"
    "noCrossExecContamination"
    "lineSpanExact"
    "bindingGateExcludesForeignExecutors"
    "everyRetainedEntryIsJustified"
    "noSilentLineLoss"
    "noStaleSealOnLiveCarrier"
    "finalizedRowFrozen"
    "storedCoverageNeverRegresses"
  ];

  # One `quint verify` run per (model, main-module, invariant-set,
  # backend) tuple. `main` selects the module within the file — a model
  # with several fault regimes declares one core module plus one thin
  # instantiation module per regime, and each regime is its own check so
  # a regression in the core protocol surfaces in the small fast check
  # instead of buried in the largest one (the role a separate .cfg per
  # regime played for the hand-written-TLA+ predecessor of this file).
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
      # Additional .qnt files (relative to docs/spec/models, without
      # the extension) that `spec` imports. A calibration module under
      # calibration/ imports ../logBufferLifecycle, so the main model
      # must be staged alongside it at the same relative position —
      # quint resolves `from "../x"` against the importing file's
      # directory.
      extraSpecs ? [ ],
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        # Only the named .qnt files. A model that imports another file
        # (a shared harness, a calibration override's parent model)
        # extends the fileset via extraSpecs — keeping it narrow means
        # an unrelated docs/ edit doesn't re-run every quint check.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
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
        # pass it through.
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

  # The dual of mkQuintCheck: an expect-violation (non-vacuity) check.
  # `quint verify` runs against a single witness predicate of the form
  # "the interesting thing never happens", and the check PASSES only
  # when the checker reports a violation -- machine-checked evidence
  # that the scenario is still reachable in that regime's explored
  # space. A witness that stops being violated means the regime's
  # headline invariant has gone vacuous (the contention it constrains
  # can no longer arise), which previously surfaced only as
  # "re-verify by hand after a constant change" notes in the model's
  # module comments.
  #
  # One witness per check: a conjunction of witnesses is violated as
  # soon as ANY conjunct is, so a reachable witness would mask an
  # unreachable one.
  #
  # Failure modes are distinguished: "no violation found" (the vacuity
  # signal) and tool errors (typecheck failure, checker crash) both
  # fail, with different messages -- success requires the checker's own
  # violation report in the transcript, not just a nonzero exit.
  #
  # tlc backend only: TLC stops at the first violation, so these runs
  # are far cheaper than the exhaustive proofs (timings live in each
  # check's transcript and the introducing commit's message).
  mkQuintWitnessCheck =
    {
      name,
      spec,
      main ? spec,
      # The witness `val` expected to be violated.
      witness,
      # Same semantics as mkQuintCheck's extraSpecs.
      extraSpecs ? [ ],
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        # Same fileset narrowing as mkQuintCheck.
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
        };
        env = {
          MODEL = spec;
          MAIN = main;
          WITNESS = witness;
        };
      }
      ''
        set -euo pipefail
        cd "$TMPDIR"

        # Same worker bound as mkQuintCheck (see its comment).
        workers="''${NIX_BUILD_CORES:-1}"
        [ "$workers" = "0" ] && workers='"auto"'
        printf '{"workers": %s}\n' "$workers" > tlc-config.json

        # The violation is the EXPECTED outcome, so the verify call's
        # nonzero exit must not abort the script -- run it as an `if`
        # condition. The transcript (including the counterexample trace,
        # which is the reachability evidence) is the check's output.
        if quint verify \
          --backend=tlc \
          --main=${main} \
          --invariant=${witness} \
          --tlc-config=tlc-config.json \
          "$src/${spec}.qnt" 2>&1 | tee $out
        then
          echo "" >&2
          echo "${name}: witness ${witness} was NOT violated in ${main}." >&2
          echo "The scenario it probes is no longer reachable; the regime's invariants may now hold vacuously." >&2
          exit 1
        fi

        # A nonzero exit alone is not enough: a typecheck error or a
        # checker crash also exits nonzero. Require quint's own
        # violation report.
        if ! grep -qF '[violation] Found an issue' $out; then
          echo "" >&2
          echo "${name}: quint verify failed without reporting a violation of ${witness} (tool error?)." >&2
          exit 1
        fi
      '';
in
{
  # Expose the constructors so a future cross-model aggregate (or an
  # ad-hoc spike) can build its own checks without going through the
  # attrs below.
  inherit mkQuintCheck mkQuintWitnessCheck;

  # Per-model checks. Spliced into checks.* via misc-checks.nix.
  checks = {
    # rio-lease's leader-election protocol over a Kubernetes Lease
    # object: per-node clocks (bounded skew), the observed-record
    # staleness clock, local self-fencing, crash/recovery, and the
    # write-ahead generation claim. Ported from (and replacing) the
    # hand-written TLA+ model; its r[verify ...] markers moved here
    # with it.
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
    # acquisition time). The model collapses acquire/seed/claim into one
    # atomic step; the unobserved-holder-change window that collapse
    # hides is discharged at the implementation level by
    # sched.recovery.bump-confirm (a claim target the durable floor
    # cannot vouch for requires a post-claim Leading round) and by
    # sched.lease.rebound (a still-leading round that observes a moved
    # transition count re-runs recovery) -- see the steal action doc.
    #
    # The state count, depth, and wall-clock are in this check's output
    # transcript and in the commit that introduced (or last re-measured)
    # the model -- never here. The durable claim: the port's state count
    # is exactly |NODES|! times the symmetry-reduced TLA+ predecessor's,
    # minus the self-symmetric states -- the structural-identity
    # evidence that the two state spaces are the same space.
    # r[verify sched.lease.at-most-one-leader+3]
    # r[verify sched.lease.k8s-lease+2]
    # r[verify sched.recovery.fetch-max-seed+4]
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
    # r[verify sched.lease.generation-claim+2]
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
    # r[verify sched.lease.generation-claim+2]
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

    # Non-vacuity witnesses. Each regime's headline invariant constrains
    # a scenario that must itself be REACHABLE for the green check to
    # mean anything; each check below passes only when the checker
    # violates its witness -- the machine-checked replacement for the
    # "re-verify by hand after a constant change" notes in the model's
    # module comments. Deliberately no tracey markers here: the spec
    # rules are verified by the regime checks above; these guard those
    # checks against going vacuous.

    # Dual belief is reachable in the base regime: the antecedent of
    # boundedDualLeadership and staleLeaderHasStaleGeneration actually
    # arises (a deposed leader that has not yet noticed its loss).
    # neverDual doubles as the witness -- it is the asymmetric regime's
    # headline invariant and the base regime's reachability probe.
    quint-leader-election-witness-dual-belief = mkQuintWitnessCheck {
      name = "leader-election-witness-dual-belief";
      spec = "leaderElection";
      main = "leaderElectionBase";
      witness = "neverDual";
    };

    # A second acquisition (a steal of a previously-held lease) is
    # reachable in the asymmetric regime: neverDual holds there because
    # of the fence/steal separation, not because the widened threshold
    # made contention unreachable within the clock ceiling.
    quint-leader-election-witness-second-acquisition = mkQuintWitnessCheck {
      name = "leader-election-witness-second-acquisition";
      spec = "leaderElection";
      main = "leaderElectionAsymmetric";
      witness = "atMostOneAcquisition";
    };

    # The steal-delete-steal sequence is explored in the deletion
    # regime: its staleLeaderHasStaleGeneration verdict is about a state
    # space that actually contains the post-deletion re-acquisition the
    # write-ahead claim defends against.
    quint-leader-election-witness-deletion-resteal = mkQuintWitnessCheck {
      name = "leader-election-witness-deletion-resteal";
      spec = "leaderElection";
      main = "leaderElectionDeletion";
      witness = "noReacquisitionAfterDeletion";
    };

    # The retain-on-own-claim re-acquisition is explored in the deletion
    # regime: a deposed-by-deletion holder re-steals its recreated lease
    # and retains its entry generation because the claims-ledger row at
    # the floor is its own -- the fetch-max-seed / generation-claim
    # retain clause the regime checks claim to verify, exercised rather
    # than merely expressible.
    quint-leader-election-witness-deletion-retain = mkQuintWitnessCheck {
      name = "leader-election-witness-deletion-retain";
      spec = "leaderElection";
      main = "leaderElectionDeletion";
      witness = "noRetainAfterDeletion";
    };

    # The foreign-tie bump is explored in the deletion regime: after a
    # deleteLease the surviving claims-ledger floor (owned by the deletion
    # victim) ties a foreign stealer's restarted entry generation and must
    # be exceeded -- the bump that keeps staleLeaderHasStaleGeneration
    # true past the deletion. A ceiling or guard change that silently
    # stranded that arm of seedFor would leave the regime check green
    # while the tie case went unexplored; this check pins it.
    quint-leader-election-witness-deletion-foreign-tie = mkQuintWitnessCheck {
      name = "leader-election-witness-deletion-foreign-tie";
      spec = "leaderElection";
      main = "leaderElectionDeletion";
      witness = "noForeignTieBumpAfterDeletion";
    };

    # The floor-above-entry bump is explored in the deletion regime: a
    # holder deposed by deleteLease that crashes and recovers re-acquires
    # on the renew edge, and the surviving claims-ledger floor exceeds
    # what the recreated lease's transition count can vouch for, so the
    # claim path must bump past it -- the restore the deletion-regime
    # header calls LOAD-BEARING for staleLeaderHasStaleGeneration. A
    # ceiling or guard change that silently stranded that arm would leave
    # the deletion regime check green while the proof claim quietly
    # narrowed; this check pins it. A red here means the renew-edge
    # instance of the arm is no longer reachable: either a deliberate
    # regime-constant/guard change (adjust the constants or retire this
    # check with the same deliberation) or the regression this check
    # exists to catch.
    quint-leader-election-witness-deletion-floor-bump = mkQuintWitnessCheck {
      name = "leader-election-witness-deletion-floor-bump";
      spec = "leaderElection";
      main = "leaderElectionDeletion";
      witness = "noFloorBumpAfterDeletion";
    };

    # The claim-INSERT failure is explored in the pg-faults regime: its
    # "each PG fault alone is survivable" verdict is about a state space
    # in which the proceed-on-failure path actually fires, not one where
    # the fault never bites.
    quint-leader-election-witness-claim-failure = mkQuintWitnessCheck {
      name = "leader-election-witness-claim-failure";
      spec = "leaderElection";
      main = "leaderElectionPgFaults";
      witness = "noClaimFailure";
    };

    # The restore-then-reclaim sequence is explored in the pg-faults
    # regime: a point-in-time restore zeroes the floor and a later
    # acquisition's claim re-raises it, so surviving the floor
    # regression is a property of the explored space rather than a
    # vacuous one.
    quint-leader-election-witness-restore-reclaim = mkQuintWitnessCheck {
      name = "leader-election-witness-restore-reclaim";
      spec = "leaderElection";
      main = "leaderElectionPgFaults";
      witness = "noReclaimAfterRestore";
    };

    # ---------------------------------------------------------------------
    # rio-scheduler's build-log buffer lifecycle (LogBuffers + LogFlusher
    # + the actor paths that drive them), modeled as a single replica
    # against an adversarial environment of worker batches, lease
    # transitions, recovery runs, flusher ticks, terminals, and reaps
    # (docs/spec/models/logBufferLifecycle.qnt). The regimes share one
    # core module and one invariant set (`logInvariants` above — the five
    # model-A invariants of the build-log verification design, the three
    # phase-3 calibration invariants, and the boundsOK ceiling tripwire);
    # each regime is its own check so a regression in the core entry
    # lifecycle surfaces in the small fast check instead of buried in the
    # fault-injection ones. The state counts, depths, and wall-clocks are
    # in each check's output transcript and the introducing commit's
    # message.

    # The base regime: no lease transitions, no faults, no evictions. The
    # replica holds the lease for the whole trace; the adversaries are
    # the worker (batch ordering, gaps, foreign executors, re-deliveries)
    # and the unbiased scheduling of the flusher against the actor. The
    # load-bearing verification of the binding gate, the span arithmetic
    # over plain and interior-hole payloads, the seal/drain ordering, and
    # the sealed-empty + cleanup reaps.
    # r[verify sched.log.batch-binding]
    # r[verify obs.log.gap-span+2]
    # r[verify obs.log.entry-justified]
    # r[verify obs.log.line-conservation]
    quint-log-base = mkQuintCheck {
      name = "log-base";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleBase";
      invariants = logInvariants;
    };

    # The lease-flap regime: foreign steals, lose/re-acquire edges,
    # same-epoch re-acquires, rebounds, recovery re-runs, and the interim
    # leader's PG-visible effects (finalizing a row, extending a row past
    # this replica's retained ring, re-dispatching under a fresh exec).
    # The load-bearing verification of the cross-exec restamp clearing
    # (exec-keyed), the stored-coverage reconciliation and the gap-merge
    # fold's span arithmetic (the recovered-prefix branch is only
    # reachable here), the stored-row coverage monotonicity under the
    # interim leader's extensions (storedCoverageNeverRegresses — the
    # interim row extension and the reconcile fold are only reachable
    # here), the deferred-final tenure pin, the tenure-orphan reap, and
    # the conservation law across failovers.
    # r[verify obs.log.exec-keyed+2]
    # r[verify obs.log.gap-span+2]
    # r[verify obs.log.entry-justified]
    # r[verify obs.log.line-conservation]
    # r[verify obs.log.stored-coverage-preserved]
    quint-log-flap = mkQuintCheck {
      name = "log-flap";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFlap";
      invariants = logInvariants;
    };

    # The fault regimes: the local fault-injection branches on top of the
    # flap regime's lease budget, split into three checks by fault class
    # (the all-faults product regime multiplies the flap regime's state
    # count by every fault class's branching factor at once and blows the
    # per-check budget; the split keeps each within the same order as the
    # largest leader-election checks while every fault switch stays
    # exercised against the full lease budget in exactly one of the three
    # — see the regime modules' header for what the split costs and why
    # that is acceptable).
    #
    # Local faults: push-coupled ring evictions and flush-channel-full
    # enqueue failures. The load-bearing verification of the eviction's
    # disclosed head loss and the enqueue-failure reap.
    # r[verify obs.log.entry-justified]
    # r[verify obs.log.line-conservation]
    quint-log-fault-local = mkQuintCheck {
      name = "log-fault-local";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFaultLocal";
      invariants = logInvariants;
    };

    # Recovery faults: DAG-load failures and TOCTOU-discarded recoveries.
    # The load-bearing verification of the degraded-tenure
    # retain-everything posture.
    # r[verify obs.log.entry-justified]
    # r[verify obs.log.line-conservation]
    quint-log-fault-recovery = mkQuintCheck {
      name = "log-fault-recovery";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFaultRecovery";
      invariants = logInvariants;
    };

    # The terminal-persist fault: PG keeps the assignment live after a
    # terminal, unlocking the post-terminal interim subtree. The
    # load-bearing verification of the refused-UPSERT reap, the
    # sealed-entry cross-exec restamp (noStaleSealOnLiveCarrier's
    # contended state — a sealed entry meeting a restamp — is only
    # reachable here), and the frozen-row latch (finalizedRowFrozen's
    # contended state — a finalized row coexisting with a live same-exec
    # entry — is only reachable here).
    # r[verify obs.log.entry-justified]
    # r[verify obs.log.line-conservation]
    # r[verify obs.log.exec-keyed+2]
    # r[verify obs.log.finalize-immutable]
    quint-log-fault-persist = mkQuintCheck {
      name = "log-fault-persist";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFaultPersist";
      invariants = logInvariants;
    };

    # The finalize-guard fault: the deferral path. The load-bearing
    # verification of the deferred-final retention against the tenure
    # pin.
    # r[verify obs.log.entry-justified]
    # r[verify obs.log.line-conservation]
    quint-log-fault-guard = mkQuintCheck {
      name = "log-fault-guard";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFaultGuard";
      invariants = logInvariants;
    };

    # Non-vacuity witnesses for the log-buffer lifecycle regimes. Same
    # discipline as the leader-election witnesses above: each check
    # passes only when the checker VIOLATES its witness, proving the
    # scenario the regime's invariants constrain is actually reachable in
    # that regime's explored space. Each witness is wired in the regime
    # whose constants make it reachable; a witness that stops being
    # violated means that regime's invariants have gone vacuous for the
    # behavior it probes.

    # A row is completed in the base regime — the headline non-vacuity
    # probe: noSilentLineLoss only bites for finalized records and
    # lineSpanExact only bites for written rows.
    quint-log-witness-completed-row = mkQuintWitnessCheck {
      name = "log-witness-completed-row";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleBase";
      witness = "noCompletedRow";
    };

    # The ingestion gate rejects a batch numbered below what the entry
    # has already accounted for — the rejection arm of
    # sched.executor.input-bounds is exercised, not merely encoded.
    quint-log-witness-non-monotone-rejection = mkQuintWitnessCheck {
      name = "log-witness-non-monotone-rejection";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleBase";
      witness = "noNonMonotoneRejection";
    };

    # The periodic sealed-empty reap fires (orphan shape 1 of 4): a
    # sealed entry whose ring is empty at a periodic tick is discarded.
    quint-log-witness-sealed-empty-reap = mkQuintWitnessCheck {
      name = "log-witness-sealed-empty-reap";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleBase";
      witness = "noSealedEmptyReap";
    };

    # The stored-coverage reconciliation caches a recovered prefix in the
    # flap regime — the gap-merge fold (the span arithmetic's hard case)
    # is actually exercised. Without this the lineSpanExact verdict is
    # about plain contiguous payloads only.
    quint-log-witness-recovered-prefix = mkQuintWitnessCheck {
      name = "log-witness-recovered-prefix";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFlap";
      witness = "noRecoveredPrefix";
    };

    # A final flush request outlives the leadership tenure that enqueued
    # it — the state the deferred-final tenure pin
    # (obs.log.deferred-final-retry's drop obligation) exists to catch.
    quint-log-witness-final-across-tenures = mkQuintWitnessCheck {
      name = "log-witness-final-across-tenures";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFlap";
      witness = "noFinalPendingAcrossTenures";
    };

    # A cross-exec restamp clears a NON-empty ring — the clearing
    # noCrossExecContamination depends on is load-bearing, not merely
    # reachable on empty rings.
    quint-log-witness-cross-exec-clear = mkQuintWitnessCheck {
      name = "log-witness-cross-exec-clear";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFlap";
      witness = "noCrossExecRestampClear";
    };

    # flush_final's out-of-tenure drop arm reaps a sealed empty entry
    # (orphan shape 2 of 4).
    quint-log-witness-tenure-drop-reap = mkQuintWitnessCheck {
      name = "log-witness-tenure-drop-reap";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFlap";
      witness = "noTenureDropReap";
    };

    # A holder change is re-acquired with the generation unchanged — the
    # saturated regime in which the generation alone cannot identify the
    # tenure and the acquire-epoch half of req_in_tenure is load-bearing.
    quint-log-witness-gen-pinned = mkQuintWitnessCheck {
      name = "log-witness-gen-pinned";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFlap";
      witness = "noGenPinnedHolderChange";
    };

    # The periodic refused-UPSERT reap fires (orphan shape 3 of 4):
    # another tenure finalized the execution, the frozen row refuses the
    # snapshot UPSERT, and the sealed orphan is discarded. Persist-fault
    # regime, not flap: the shape needs a sealed entry whose execution
    # another tenure finalizes, a terminal that seals also persists the
    # terminal status (which removes the live assignment the interim's
    # finalization is keyed on), and only the terminal-persist FAILURE
    # leaves the assignment live for the interim to finalize behind the
    # seal.
    quint-log-witness-refused-upsert-reap = mkQuintWitnessCheck {
      name = "log-witness-refused-upsert-reap";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFaultPersist";
      witness = "noRefusedUpsertReap";
    };

    # An accepting push's eviction drops a non-empty head prefix — the
    # disclosed head-loss channel of the conservation law actually fires.
    quint-log-witness-eviction = mkQuintWitnessCheck {
      name = "log-witness-eviction";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFaultLocal";
      witness = "noEviction";
    };

    # The terminal epilogue's enqueue-failure reap fires (orphan shape 4
    # of 4).
    quint-log-witness-enqueue-fail-reap = mkQuintWitnessCheck {
      name = "log-witness-enqueue-fail-reap";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFaultLocal";
      witness = "noEnqueueFailReap";
    };

    # Calibration entry #0: the accept gate is the load-bearing fix for
    # the obs.log.gap-span+2 falsification. The Ungated module is the
    # flap regime with ENABLE_ACCOUNTED_FLOOR = false (the batch accept
    # predicate degrades to the pre-fix comparison against the ring's
    # current tail, which resets when the stored-coverage reconcile
    # empties the ring); lineSpanExact MUST be violated there. A green
    # exhaustive quint-log-flap plus a red lineSpanExact here is the
    # machine-checked statement "the gate is necessary and sufficient at
    # these bounds" — if this check ever stops finding the violation,
    # the gate has stopped being the thing that prevents it.
    quint-log-witness-gap-span-ungated = mkQuintWitnessCheck {
      name = "log-witness-gap-span-ungated";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleUngated";
      witness = "lineSpanExact";
    };

    # flush_final's already-finalized refusal arm fires — the
    # non-vacuity probe for the finalize-once guard partition. Every
    # other refusal/reap arm has one of these; without it nothing in CI
    # proves the post-44905298b consult arm is reachable and the
    # finalizedRowFrozen verdict could go vacuous for the final path.
    # Fault-persist regime: the only one in which a finalized row can
    # coexist with a pending in-tenure final for the same execution.
    quint-log-witness-already-finalized-refusal = mkQuintWitnessCheck {
      name = "log-witness-already-finalized-refusal";
      spec = "logBufferLifecycle";
      main = "logBufferLifecycleFaultPersist";
      witness = "noAlreadyFinalizedRefusal";
    };

    # ---------------------------------------------------------------------
    # Permanent calibration witnesses (phase 3). Each restores one
    # historical fix's pre-fix behavior via a calibration switch
    # (docs/spec/models/calibration/) and MUST still falsify the
    # invariant the fix protects: a green exhaustive regime check plus a
    # red invariant here is the machine-checked statement "the fix is
    # necessary and sufficient at these bounds". Only the overrides that
    # guard against a plausible model regression are wired here; the
    # full corpus verdict table is log-invariant-map.md's phase-3
    # findings section and the rest of the override modules stay in
    # calibration/ as re-runnable evidence. Unlike the non-vacuity
    # witnesses above, these carry r[verify ...] markers: each is the
    # machine-checked statement that a specific normative behavior is
    # the load-bearing fix for its rule's invariant, which is a
    # verification claim about the rule and not just about the regime
    # checks' non-vacuity.

    # effefb0a1: a flush payload's span contribution reverts to its
    # physical line count instead of its line-number span. The cheapest
    # falsification in the corpus (base regime, one holey ring, no
    # failover) and the most plausible regression (a refactor
    # "simplifying" ringSpan back to a count).
    # r[verify obs.log.gap-span+2]
    quint-log-calib-physical-count-span = mkQuintWitnessCheck {
      name = "log-calib-physical-count-span";
      spec = "calibration/lines";
      extraSpecs = [ "logBufferLifecycle" ];
      main = "logBufferLifecyclePhysicalCountSpan";
      witness = "lineSpanExact";
    };

    # 6c26e85f8: a cross-exec restamp reverts to carrying the prior
    # execution's lines into the new execution's entry. The one corpus
    # row that falsifies an original model-A invariant outright.
    # r[verify obs.log.exec-keyed+2]
    quint-log-calib-cross-exec-carries-lines = mkQuintWitnessCheck {
      name = "log-calib-cross-exec-carries-lines";
      spec = "calibration/restamps";
      extraSpecs = [ "logBufferLifecycle" ];
      main = "logBufferLifecycleCrossExecCarriesLines";
      witness = "noCrossExecContamination";
    };

    # f8ce10b8e (and 463090eb7's orphan shape): no recurring reaper for
    # a sealed non-empty entry whose execution another tenure finalized.
    # The refused-UPSERT reap is individually load-bearing — deleting it
    # from the model (e.g. during the phase-6 reap unification) must
    # turn this red.
    # r[verify obs.log.entry-justified]
    quint-log-calib-no-refused-upsert-reap = mkQuintWitnessCheck {
      name = "log-calib-no-refused-upsert-reap";
      spec = "calibration/reaps";
      extraSpecs = [ "logBufferLifecycle" ];
      main = "logBufferLifecycleNoRefusedUpsertReap";
      witness = "everyRetainedEntryIsJustified";
    };

    # 81824cfbb: no reaper for a sealed entry whose ring the
    # stored-coverage reconcile empties after its pending final is
    # consumed. The sealed-empty reap is individually load-bearing. The
    # deepest counterexample in the calibration corpus — random
    # simulation does not find it.
    # r[verify obs.log.entry-justified]
    quint-log-calib-no-sealed-empty-reap = mkQuintWitnessCheck {
      name = "log-calib-no-sealed-empty-reap";
      spec = "calibration/reaps";
      extraSpecs = [ "logBufferLifecycle" ];
      main = "logBufferLifecycleNoSealedEmptyReap";
      witness = "everyRetainedEntryIsJustified";
    };

    # Implementation conformance (model-based testing). The regime checks
    # above prove the PROTOCOL; this one proves rio-lease implements
    # that protocol: rio-lease/src/mbt_tests.rs replays traces generated
    # from the leaderElectionBase regime against the real election
    # machinery — the #[quint_run] simulation walks `step` with quint's
    # --mbt action tracking under a seed pinned in the test attribute,
    # the four mbt_run_* tests replay the model's named runs from their
    # ITF traces — and diffs the projected state (lease, leading, gen)
    # after every step. Model↔implementation drift therefore surfaces
    # here as a red check, not as a review-time judgment call.
    #
    # Wiring: the same prebuilt rio-lease test binary nextest-rio-lease
    # runs (a rio-lease source edit rebuilds both), with quint on PATH
    # and the model file staged into the remapped nextest workspace (a
    # model edit re-runs this check — that coupling is the point). The
    # staged copy serves both spec lookups in mbt_tests.rs: the
    # #[quint_run] attribute's workspace-relative path resolves against
    # the test CWD, and the named-run replays read the RIO_MBT_SPEC_PATH
    # override (their compile-time fallback path names the sandbox that
    # compiled the binary, which does not exist here). The mbt_* tests
    # are #[ignore]d, so nextest-rio-lease stays quint-free and this
    # check is the one place they run in CI.
    #
    # The markers below cover what the replayed traces genuinely
    # exercise end-to-end: Lease-object CAS acquisition/renewal/conflict
    # (k8s-lease), the single-writer-per-resourceVersion race and the
    # belief/lease agreement diffed at every step (at-most-one-leader),
    # and the blind-leader self-fence flip (self-fence). The PG-side
    # rules (generation-claim, fetch-max-seed) are NOT marked: the
    # phase-1 projection omits genHW and the mock has no claims ledger.
    # r[verify sched.lease.k8s-lease+2]
    # r[verify sched.lease.at-most-one-leader+3]
    # r[verify sched.lease.self-fence+2]
    mbt-rio-lease = mkNextestRun {
      name = "mbt-rio-lease";
      member = "rio-lease";
      meta = mkNextestMeta { rio-lease = rioLeaseTestBin; };
      extraRuntimeInputs = [ pkgs.quint ];
      extraArgs = [
        "-E"
        "package(rio-lease) and test(/mbt_/)"
        "--run-ignored"
        "all"
      ];
      preRun = ''
        export RIO_MBT_SPEC_PATH=$TMPDIR/ws/docs/spec/models/leaderElection.qnt
      '';
      postWsSetup = ''
        mkdir -p $ws/docs/spec/models
        cp ${leaseModel}/leaderElection.qnt $ws/docs/spec/models/
      '';
      postRun = ''
        # The module-level nextest args pass --no-tests=warn, so a
        # filter that matches nothing would otherwise yield a green
        # check that ran no conformance test at all (e.g. after a test
        # module rename). Assert at least one test actually ran; the
        # exit code already covers failures.
        grep -E ' [1-9][0-9]* tests? run:' $out/log > /dev/null || {
          echo "mbt-rio-lease: the mbt_* filter matched no tests" >&2
          exit 1
        }
      '';
    };
  };
}

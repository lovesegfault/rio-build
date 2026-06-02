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
  # nix/checks.nix's nextest reuse-build helpers and the prebuilt
  # rio-lease / rio-store test binaries, threaded through flake.nix's
  # quintChecks binding. Only the mbt-rio-lease and mbt-rio-logservice
  # conformance checks below consume them — the model checks need
  # nothing from the Rust build.
  mkNextestRun,
  mkNextestMeta,
  rioLeaseTestBin,
  rioStoreTestBin,
}:
let
  modelsDir = unfilteredRoot + "/docs/spec/models";

  # The §9.1 successor conjunction for materializationJob.qnt (shared
  # by the five per-regime holds checks below; the C-prime stage
  # record's property table is the authoritative gloss).
  matJobInvariants = [
    "boundsOK"
    "noFromSourceWhileJobUnresolved"
    "jobResolutionSound"
    "routingRequiresDurableVouchOrFailFast"
    "unresolvedJobAlwaysArmed"
    "noWrongfulTerminalFailure"
    "noWrongfulFromSourceRouting"
    "successConsumptionCoversLiveWanted"
    "interestUnionLiveOnly"
    "pinCoversIngestUntilAllInterestTerminal"
    "failoverPreservesJobs"
    "fencedJobWritesOnly"
    "kindMatchesWorker"
    "materializationNeverPoisons"
    "materializationInvisibleToBuildBudgets"
    "atMostOneUnresolvedJobPerDrv"
    "atMostOneClaimWinner"
    "wrongfulFailFastBoundedPerJob"
    "creationLeavesTenantResolvable"
    "materializationCrashChargedOnce"
    "crossBuildWantedIsolation"
  ];

  # The leader-election model as its own single-file store path (the
  # same narrowing mkQuintCheck's src uses): the conformance check
  # depends on the model's content, not on whatever tree unfilteredRoot
  # points at — an unrelated docs/ edit must not re-run it, a model edit
  # must.
  leaseModel = lib.fileset.toSource {
    root = modelsDir;
    fileset = modelsDir + "/leaderElection.qnt";
  };

  # Same narrowing for the LogService model, consumed by the
  # mbt-rio-logservice conformance check below.
  logServiceModel = lib.fileset.toSource {
    root = modelsDir;
    fileset = modelsDir + "/logService.qnt";
  };

  # The TLC backend's quint->TLA+ conversion goes through the bundled
  # Apalache acting as a gRPC server. When `quint verify` has to spawn
  # that server itself, two failure modes bite:
  #   - the first conversion against a cold (un-JITed) server can die
  #     with an empty-details gRPC INTERNAL error for larger models
  #     (retryPolicy.qnt reproduces it deterministically; the same
  #     request against a warmed server succeeds in ~1 s), and
  #   - quint's own spawn path installs an uncaughtException handler
  #     that swallows exactly that error and exits 0 without ever
  #     running TLC — which a naive "exit 0 == proved" check would
  #     happily report as a green exhaustive proof.
  # Both constructors below therefore start the bundled Apalache server
  # themselves (so it persists across attempts and quint never installs
  # its swallow-and-exit handler), retry the verify against the
  # progressively warmed server, and accept a result only when quint's
  # own verdict line ("[ok] No violation found" / "[violation] Found an
  # issue") is present in the transcript — exit codes alone are not
  # trusted in either direction.
  #
  # The server heap is parameterized because the conversion request for
  # the largest modules (the gateway-lifecycle calibration overrides that
  # carry both a rich connection alphabet and the in-session machinery)
  # exceeds the 4 GiB default — the server OOMs parsing the request and
  # every later attempt against the wedged JVM fails too. Only the checks
  # that need it pass a larger value: the default renders byte-identical
  # script text, so existing checks do not rehash.
  apalacheServerPreludeWithHeap = serverHeapMb: ''
    # Start the bundled Apalache server (the same distribution quint
    # would spawn) so it outlives individual quint invocations and stays
    # JIT-warm across retries. Its chatter goes to a side log, keeping
    # the transcript to quint + TLC output. Port 8822 is quint's default;
    # the sandbox's private network namespace keeps parallel checks from
    # colliding on it.
    apalache_jar=$(ls ${pkgs.quint}/share/quint/apalache-dist-*/apalache/lib/apalache.jar)
    ${pkgs.jdk21_headless}/bin/java -Xmx${toString serverHeapMb}m -XX:+UseG1GC -jar "$apalache_jar" server --port=8822 \
      > apalache-server.log 2>&1 &
    apalache_pid=$!
    trap 'kill $apalache_pid 2>/dev/null || true' EXIT
    for _ in $(seq 1 90); do
      if (exec 3<>/dev/tcp/127.0.0.1/8822) 2>/dev/null; then
        exec 3>&- 3<&-
        break
      fi
      sleep 1
    done

    # The gRPC port opens well before the server can serve a LARGE
    # quint->TLA+ conversion: for roughly the first 30-60 s after startup
    # the conversion of a regime-sized module returns an empty-details
    # INTERNAL error while small modules convert fine, and the window is
    # long enough that three back-to-back verify attempts can all land
    # inside it (measured against retryPolicy.qnt's regime modules:
    # requests at +8/+14/+21 s after port-open fail, +58 s succeeds; the
    # server log shows no error — the request is simply not served yet).
    # Warm the server by running THE model conversion this check is about
    # to need, retrying until it succeeds, so quint verify below starts
    # against a server that has already proven it can convert this exact
    # module. $MODEL/$MAIN are the env attrs every check sets; $src is
    # the staged model fileset.
    for _ in $(seq 1 30); do
      if quint compile --target tlaplus --main=$MAIN \
          --server-endpoint=localhost:8822 "$src/$MODEL.qnt" > /dev/null 2>&1; then
        break
      fi
      sleep 5
    done

    # Run `quint verify "$@"`, retrying while the transcript carries no
    # verdict line (the residual cold-conversion failure shape, should
    # the warm-up above have raced the server). Sets quint_status; the
    # transcript of the accepted attempt is in $out.
    run_quint_verify() {
      quint_status=0
      for attempt in 1 2 3; do
        if quint verify --server-endpoint=localhost:8822 "$@" 2>&1 | tee $out; then
          quint_status=0
        else
          quint_status=$?
        fi
        if grep -qE '\[ok\] No violation found|\[violation\] Found an issue' $out; then
          return 0
        fi
        echo "attempt $attempt produced no TLC verdict (cold-server conversion failure?); retrying" >&2
        sleep 10
      done
      return 0
    }
  '';

  # The default-heap prelude every existing check interpolates; renders
  # the exact text the constructors carried before the heap was
  # parameterized, so no existing derivation rehashes.
  apalacheServerPrelude = apalacheServerPreludeWithHeap 4096;

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
      # the extension) that `spec` imports. A module that overrides or
      # instantiates another model file needs that file staged alongside
      # it at the same relative position — quint resolves `from "../x"`
      # against the importing file's directory.
      extraSpecs ? [ ],
      # Non-default step action (an override module can expose an
      # alternative transition relation, the way the retired Stage-C
      # calibration corpus exposed its pre-fix `calibStep`). null means
      # quint's default (`step`).
      step ? null,
      # Pin the TLC worker count to the value the wiring measurement used
      # (closure-evidence Phase-1 review finding MCI-6): an exhaustive
      # check wired from a measured budget must run at the measurement's
      # worker count or the budget is meaningless — TLC wall-clock does
      # not scale linearly in workers, so "converges in N min at 60
      # workers" says nothing about other widths. null (the default)
      # keeps the pre-Phase-1 behavior: derive from NIX_BUILD_CORES.
      workers ? null,
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        # Only the named .qnt files. A model that imports another file
        # (a shared harness, an override module's parent model) extends
        # the fileset via extraSpecs — keeping it narrow means an
        # unrelated docs/ edit doesn't re-run every quint check.
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
        ${
          lib.optionalString (workers != null) ''
            workers=${toString workers}
          ''
        }printf '{"workers": %s}\n' "$workers" > tlc-config.json

        ${apalacheServerPrelude}

        # The transcript is the proof artifact: it records the
        # invariants checked, the state count, and the depth — keep it.
        # (`| tee` is pipefail-safe — tee consumes everything; never
        # replace it with `| head`, see
        # .claude/rules/ci-failure-patterns.md.)
        run_quint_verify \
          --backend=${backend} \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"} \
          --invariant=${lib.concatStringsSep "," invariants} \
          ${
            if backend == "tlc" then "--tlc-config=tlc-config.json" else "--max-steps=${toString maxSteps}"
          } \
          "$src/${spec}.qnt"

        # "Proved" requires the checker's own verdict, not just a zero
        # exit: a quint crash that never reached TLC must never read as
        # a green exhaustive proof.
        if [ "$quint_status" -ne 0 ]; then
          echo "" >&2
          echo "${name}: quint verify failed (status $quint_status) — see the transcript above." >&2
          exit 1
        fi
        if ! grep -qF '[ok] No violation found' $out; then
          echo "" >&2
          echo "${name}: quint verify exited 0 but reported no verdict (tool error / conversion failure?)." >&2
          exit 1
        fi
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
      # Same semantics as mkQuintCheck's step (no current caller; kept
      # for override modules that select a non-default transition
      # relation).
      step ? null,
      # Apalache server heap (MiB) for the quint->TLA+ conversion. The
      # default matches the historical hardcoded value; only the largest
      # override modules (whose conversion request OOMs a 4 GiB server)
      # need more — see apalacheServerPreludeWithHeap.
      serverHeapMb ? 4096,
      # Same semantics as mkQuintCheck's workers (MCI-6 pinning).
      workers ? null,
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
        ${
          lib.optionalString (workers != null) ''
            workers=${toString workers}
          ''
        }printf '{"workers": %s}\n' "$workers" > tlc-config.json

        ${apalacheServerPreludeWithHeap serverHeapMb}

        # The violation is the EXPECTED outcome, so the verify call's
        # nonzero exit must not abort the script -- the retry helper
        # captures the status instead. The transcript (including the
        # counterexample trace, which is the reachability evidence) is
        # the check's output.
        run_quint_verify \
          --backend=tlc \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"} \
          --invariant=${witness} \
          --tlc-config=tlc-config.json \
          "$src/${spec}.qnt"

        if [ "$quint_status" -eq 0 ] && grep -qF '[ok] No violation found' $out; then
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

  # Expect-violation check via the RUST SIMULATOR (`quint run`) instead
  # of TLC. For violations that are real, confirmed, and load-bearing for
  # the campaign record, but whose BFS level sits past a gate-compatible
  # TLC budget (the closure-evidence post-0d triage record: TLC's BFS is
  # correct and finds these given enough wall-clock — the per-level state
  # growth at the affected scopes simply puts them in the
  # tens-of-minutes-to-hours class). The simulator finds the same
  # violations in seconds because a random walk pays no exhaustive
  # frontier cost.
  #
  # Semantics: bounded random search, NOT a proof — exactly right for an
  # expect-violation check, whose claim is existential ("this scenario is
  # reachable"), not universal. A found violation is a found violation
  # regardless of backend; only "no violation found" differs in strength,
  # and for these checks that outcome is a FAILURE either way.
  #
  # Flake discipline: maxSamples must be sized so the per-run miss
  # probability is negligible. With a per-trace hit rate p (measure it:
  # the introducing commit records traces-to-first-hit), P(miss) =
  # (1-p)^maxSamples ~= exp(-p*maxSamples); size maxSamples >= 25/p so
  # P(miss) <= ~1e-11. Cost note: the simulator's early exit is
  # per-worker-batch, not per-trace — high-hit-rate violations return in
  # under a second, but a low-rate violation's run costs roughly the
  # FULL sample budget's wall clock (measured: ~10s per million samples
  # per step-of-14 at the closure-evidence duo scope), so keep
  # maxSamples at the flake-math minimum rather than padding it. If a
  # previously-green sim check starts failing, the model has drifted
  # under the scenario (the same signal a TLC witness check gives) —
  # investigate, never just bump samples.
  mkQuintSimWitnessCheck =
    {
      name,
      spec,
      main ? spec,
      # The witness `val` expected to be violated.
      witness,
      # Per-run sample / step bounds (see the flake-discipline note).
      maxSamples ? 2000000,
      maxSteps ? 15,
      # Same semantics as mkQuintCheck's extraSpecs / step.
      extraSpecs ? [ ],
      step ? null,
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

        # The violation is the EXPECTED outcome: `quint run` exits
        # nonzero when it finds one, so don't let that abort the script.
        status=0
        quint run \
          --backend=rust \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"} \
          --invariant=${witness} \
          --max-samples=${toString maxSamples} \
          --max-steps=${toString maxSteps} \
          "$src/${spec}.qnt" 2>&1 | tee $out || status=$?

        if grep -qF '[ok] No violation found' $out; then
          echo "" >&2
          echo "${name}: witness ${witness} was NOT violated in ${main} within ${toString maxSamples} samples x ${toString maxSteps} steps." >&2
          echo "The scenario it probes is no longer reachable (or its hit rate collapsed); the regime's invariants may now hold vacuously." >&2
          exit 1
        fi

        # Require the simulator's own violation report -- a crash or a
        # typecheck error must not read as a successful expect-violation.
        if ! grep -qF '[violation] Found an issue' $out; then
          echo "" >&2
          echo "${name}: quint run failed without reporting a violation of ${witness} (tool error?)." >&2
          exit 1
        fi
      '';

  # Bounded-simulation HOLDS check: the dual of mkQuintSimWitnessCheck.
  # `quint run` over maxSamples random traces must find NO violation of
  # the named invariants. This is bounded evidence, not proof — where an
  # exhaustive TLC conjunction exists (wired or manual target) it remains
  # the proof obligation; this constructor's role is the GHA-wired
  # deliverable for FIXED properties whose exhaustive conjunctions exceed
  # every gate-compatible TLC budget (the closure-evidence Phase-1 flips:
  # owner decision 4's A17/L2 wiring).
  #
  # Vacuity discipline: a holds check proves nothing if the model can no
  # longer reach the states the property constrains. Every check built
  # from this constructor MUST name its paired expect-violation
  # calibration pin in its comment — the pin re-introduces the pre-fix
  # behavior and must keep falsifying the same property, which is the
  # machine-checked evidence that the property is still about something
  # reachable. A holds check without a falsifying pair is not wired here.
  #
  # Multiple invariants are conjoined with "and" into one expression
  # (`quint run --invariant` accepts an expression; the comma form is
  # verify-only). A violation therefore does not name the failing
  # conjunct — re-run per-invariant to identify it, same as TLC's q_inv.
  mkQuintSimHoldsCheck =
    {
      name,
      spec,
      main ? spec,
      # The invariant `val` names expected to HOLD.
      invariants,
      # Per-run sample / step bounds. Sizing: large enough that the paired
      # pin's violation class would be re-found if it re-opened (use the
      # pin's own measured budget as the floor); the introducing commit
      # records the measurement.
      maxSamples ? 2000000,
      maxSteps ? 15,
      # Same semantics as mkQuintCheck's extraSpecs / step.
      extraSpecs ? [ ],
      step ? null,
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
          INVARIANTS = lib.concatStringsSep " and " invariants;
        };
      }
      ''
        set -euo pipefail
        cd "$TMPDIR"

        # A violation exits nonzero; capture the status so the verdict
        # grep below (not the exit code) decides the outcome.
        status=0
        quint run \
          --backend=rust \
          --main=${main} \
          ${lib.optionalString (step != null) "--step=${step}"} \
          --invariant='${lib.concatStringsSep " and " invariants}' \
          --max-samples=${toString maxSamples} \
          --max-steps=${toString maxSteps} \
          "$src/${spec}.qnt" 2>&1 | tee $out || status=$?

        if grep -qF '[violation] Found an issue' $out; then
          echo "" >&2
          echo "${name}: a violation of (${lib.concatStringsSep " and " invariants}) was found in ${main}." >&2
          echo "A Phase-1 fixed property regressed (or its model encoding drifted): capture the trace above" >&2
          echo "and triage against the paired calibration pin before touching this check." >&2
          exit 1
        fi

        # "Holds" requires the simulator's own verdict, not just exit 0:
        # a crash that never ran any trace must not read as bounded
        # evidence.
        if [ "$status" -ne 0 ]; then
          echo "" >&2
          echo "${name}: quint run failed (status $status) without reporting a violation — tool error." >&2
          exit 1
        fi
        if ! grep -qF '[ok] No violation found' $out; then
          echo "" >&2
          echo "${name}: quint run exited 0 but reported no verdict (tool error?)." >&2
          exit 1
        fi
      '';

  # Deterministic named-run replay: `quint test` over a regime module's
  # `run` definitions. The runs are the model's executable scenario pins
  # (the documented-divergence reproducers and the happy-path narratives);
  # this check keeps them green as the model evolves, the same way a unit
  # test pins a hand-computed history. Cheap (no state-space exploration),
  # so one check per regime module is fine.
  #
  # The match pattern is anchored to the `...Run` naming convention so a
  # regime with no runs fails loudly (zero matches) instead of silently
  # passing -- a renamed run must not drop out of CI unnoticed.
  mkQuintRunCheck =
    {
      name,
      spec,
      main ? spec,
      match ? ".*Run$",
      extraSpecs ? [ ],
    }:
    pkgs.runCommand "quint-${name}"
      {
        nativeBuildInputs = [ pkgs.quint ];
        src = lib.fileset.toSource {
          root = modelsDir;
          fileset = lib.fileset.unions (map (s: modelsDir + "/${s}.qnt") ([ spec ] ++ extraSpecs));
        };
        env = {
          MODEL = spec;
          MAIN = main;
          MATCH = match;
        };
      }
      ''
        set -euo pipefail
        cd "$TMPDIR"

        quint test \
          --main=${main} \
          --match='${match}' \
          "$src/${spec}.qnt" 2>&1 | tee $out

        # `quint test` exits nonzero on a failing run; additionally require
        # that at least one run actually matched and passed, so a rename
        # that empties the match cannot turn into a silent green check.
        if ! grep -qE ' [1-9][0-9]* passing' $out; then
          echo "" >&2
          echo "${name}: no runs matched '${match}' in ${main}." >&2
          exit 1
        fi
      '';
in
{
  # Expose the constructors so a future cross-model aggregate (or an
  # ad-hoc spike) can build its own checks without going through the
  # attrs below.
  inherit
    mkQuintCheck
    mkQuintWitnessCheck
    mkQuintSimWitnessCheck
    mkQuintSimHoldsCheck
    mkQuintRunCheck
    ;

  # [LONG] checks (Tier 2 of the closure-evidence Phase-1 three-tier
  # wiring; owner decision 3 / adjudication OQ6): check names that are
  # wired into checks.* — the LOCAL merge gate (`/nixbuild --checks`)
  # carries them under the raised 15–30-minute budget — but are EXCLUDED
  # from the GHA formal matrix (flake.nix removes them from
  # ciMatrix.formal): a 15–30-min-at-60-workers TLC check needs hours of
  # pod wall-clock on the 4–16 vCPU rio-ci spot runners, over the 45-min
  # job timeout even as a singleton shard — the same runner-class
  # rationale that keeps cov-smoke out of the GHA matrices. Every entry
  # here MUST (a) exist in `checks` below, (b) pin `workers` to its
  # qualifying measurement's count, and (c) name its Tier-1 GHA companion
  # (the holdsInSim+pin pair covering the same property) in its comment.
  #
  # Track E knobs (FORMAL_SHARD_SIZE, --max-jobs, timeout-minutes, runner
  # labels) are FROZEN for this campaign: per-check pod wall-clock is a
  # constraint no shard knob fixes (sharding divides checks among jobs,
  # it never shortens one check). Pressure to change them is
  # stop-and-report, never a unilateral CI edit.
  #
  # Currently EMPTY: the Phase-1 Wave-4 measurement campaign (invariant
  # map, Wave-4 stage record) found no exhaustive conjunction that
  # converges within the 5–30-minute Tier-2 window at 60 workers — every
  # candidate is either Tier 1 (none qualified) or a Tier-3 documented
  # manual target. The mechanism is wired so a Phase-2 conjunction that
  # does converge can be added here without touching flake.nix again.
  longChecks = [ ];

  # Per-model checks. Imported by flake.nix as the quintChecks binding,
  # which merges them into checks.* and hands the same attrset to the
  # CI matrix's `formal` kind.
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

    # ------------------------------------------------------------------
    # rio-store's LogService: the build-log session/chunk/dedup protocol
    # (model C of the log-formal campaign — the successor to the retired
    # in-scheduler logBufferLifecycle model). The model covers the
    # builder's at-least-once ack-trimmed uploader, the open-time
    # binding + completeness gate, the per-stream ingest session and its
    # accept predicate, the immutable chunk manifest, the read path's
    # ordered-walk dedup over overlapping sessions, the completeness
    # fold, and the TTL sweep. The acceptance test for the model is the
    # 22-bug calibration table in docs/spec/models/log-invariant-map.md
    # re-run against this architecture. State counts, depths, and
    # wall-clocks are in the introducing commit's message and each
    # check's transcript.
    #
    # Marker scope: append-auth, completeness-gate, session-keyed, and
    # exec-keyed are marked on the regime that makes each load-bearing.
    # store.log.chunk-immutable is NOT marked (the model has no
    # overwrite action, so immutability is by construction rather than
    # checked; the seq-burning and object-before-manifest halves are
    # unit-tested code branches). store.log.ingest-bounds and
    # store.log.tail-reconnect are NOT marked (resource bounds and the
    # since_line reconnect cursor are outside the model's scope — see
    # the model header's priced omissions).
    # ------------------------------------------------------------------

    # The single-writer core: one execution, one session, the
    # adversarial batch stream (fabricated gaps, re-sends, lines past
    # the recorded final count) against the accept predicate, the
    # per-append completeness ceiling, the contiguous-prefix cut, the
    # read fold, and the completeness fold. ackImpliesDurable is NOT
    # asserted here: the fabricating client can poison its own ack
    # watermark (see the val's doc in the model) and the per-ack
    # durability claim is only made for the honest uploader.
    # r[verify store.log.completeness-gate]
    quint-log-service-base = mkQuintCheck {
      name = "log-service-base";
      spec = "logService";
      main = "logServiceBase";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesForeignWriters"
        "noSilentLineLoss"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
      ];
    };

    # Two executions of one derivation: the supersession scenarios. The
    # auth gate rejects the superseded execution's reopen; the
    # superseded execution's still-open session keeps writing to its own
    # (old) execution's log only; the two manifests grow concurrently
    # under disjoint exec-keyed namespaces.
    # r[verify store.log.append-auth]
    # r[verify obs.log.exec-keyed+2]
    quint-log-service-redispatch = mkQuintCheck {
      name = "log-service-redispatch";
      spec = "logService";
      main = "logServiceRedispatch";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesForeignWriters"
        "noSilentLineLoss"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
      ];
    };

    # Two sessions for one execution: the at-least-once re-send shape.
    # A chunk commits but its ack is lost, the builder replays the same
    # lines to a fresh session, the detached predecessor's drain commits
    # overlapping chunks, and the read path's (first_line, session_id)
    # ordered walk serves each line exactly once. The honest uploader's
    # ack-implies-durable refinement is asserted here (no fabrication).
    # r[verify store.log.session-keyed]
    quint-log-service-resend = mkQuintCheck {
      name = "log-service-resend";
      spec = "logService";
      main = "logServiceResend";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesForeignWriters"
        "noSilentLineLoss"
        "ackImpliesDurable"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
      ];
    };

    # The TTL sweep against a live ingest pipeline: the expired-only
    # guard, the chunks-before-execution-row deletion order with a cut
    # interleavable between the two DELETEs, and the disclosed-loss
    # accounting for swept lines.
    quint-log-service-sweep = mkQuintCheck {
      name = "log-service-sweep";
      spec = "logService";
      main = "logServiceSweep";
      invariants = [
        "boundsOK"
        "noCrossExecContamination"
        "authGateExcludesForeignWriters"
        "noSilentLineLoss"
        "ackImpliesDurable"
        "servedSpanExact"
        "completeLogServesAllProduced"
        "completenessGate"
      ];
    };

    # Non-vacuity witnesses for the LogService regimes. Each check
    # passes only when the checker violates its witness — machine-checked
    # evidence that the scenario a regime's headline invariant constrains
    # is actually reachable in that regime's explored space. Deliberately
    # no tracey markers here: the spec rules are verified by the regime
    # checks above; these guard those checks against going vacuous.

    # A log reads complete in the base regime: the conservation law and
    # the open-time seal only bite against a complete log.
    quint-log-service-witness-completed-log = mkQuintWitnessCheck {
      name = "log-service-witness-completed-log";
      spec = "logService";
      main = "logServiceBase";
      witness = "noCompletedLog";
    };

    # The per-append completeness ceiling actually drops or truncates a
    # batch — the post-terminal injection the gate exists for is
    # attempted, not merely encodable.
    quint-log-service-witness-past-final = mkQuintWitnessCheck {
      name = "log-service-witness-past-final";
      spec = "logService";
      main = "logServiceBase";
      witness = "noPastFinalRejection";
    };

    # The monotone floor actually rejects a batch numbered below the
    # session's high-water mark.
    quint-log-service-witness-non-monotone = mkQuintWitnessCheck {
      name = "log-service-witness-non-monotone";
      spec = "logService";
      main = "logServiceBase";
      witness = "noNonMonotoneRejection";
    };

    # A session learns its completeness ceiling AFTER accepting lines
    # (the seal lands mid-stream): the case that distinguishes the
    # per-append gate from the open-time gate and produces the disclosed
    # pre-refresh residual completenessGate carves out.
    quint-log-service-witness-mid-stream-ceiling = mkQuintWitnessCheck {
      name = "log-service-witness-mid-stream-ceiling";
      spec = "logService";
      main = "logServiceBase";
      witness = "noMidStreamCeiling";
    };

    # An uploader abandons its drain with un-acked lines: the
    # disclosed-loss channel the conservation law's fourth disjunct
    # routes through actually fires.
    quint-log-service-witness-abandoned = mkQuintWitnessCheck {
      name = "log-service-witness-abandoned";
      spec = "logService";
      main = "logServiceBase";
      witness = "noAbandonedWithUnacked";
    };

    # A manifest carries a forward gap: the completeness fold's gap arm
    # and the cut's split-at-gaps behavior are exercised.
    quint-log-service-witness-gapped-manifest = mkQuintWitnessCheck {
      name = "log-service-witness-gapped-manifest";
      spec = "logService";
      main = "logServiceBase";
      witness = "noGappedManifest";
    };

    # The open-time seal rejects a stream open for an already-complete
    # log.
    quint-log-service-witness-complete-open-rejected = mkQuintWitnessCheck {
      name = "log-service-witness-complete-open-rejected";
      spec = "logService";
      main = "logServiceBase";
      witness = "noCompleteOpenRejection";
    };

    # Two chunks of one execution from different sessions overlap in
    # line range: the re-sent-batch-after-ambiguous-disconnect shape the
    # read path's dedup exists for. servedSpanExact's verdict in the
    # resend regime is about a state space that actually contains the
    # overlap.
    quint-log-service-witness-overlap = mkQuintWitnessCheck {
      name = "log-service-witness-overlap";
      spec = "logService";
      main = "logServiceResend";
      witness = "noOverlappingChunks";
    };

    # The read path's watermark actually suppresses a duplicate (the
    # total yield count falls short of the sum of the chunks' claimed
    # counts): the dedup is exercised, not merely encoded.
    quint-log-service-witness-dedup = mkQuintWitnessCheck {
      name = "log-service-witness-dedup";
      spec = "logService";
      main = "logServiceResend";
      witness = "noDuplicateSuppressed";
    };

    # The auth gate rejects a superseded execution's reopen — the
    # foreign-token rejection authGateExcludesForeignWriters is about.
    quint-log-service-witness-superseded-rejected = mkQuintWitnessCheck {
      name = "log-service-witness-superseded-rejected";
      spec = "logService";
      main = "logServiceRedispatch";
      witness = "noSupersededOpenRejection";
    };

    # Two executions' logs grow concurrently (the superseded execution's
    # session still holds lines while both manifests are non-empty): the
    # contended state noCrossExecContamination constrains.
    quint-log-service-witness-concurrent-execs = mkQuintWitnessCheck {
      name = "log-service-witness-concurrent-execs";
      spec = "logService";
      main = "logServiceRedispatch";
      witness = "noConcurrentExecWriters";
    };

    # The TTL sweep deletes a non-empty manifest: the sweep's deletion
    # arm is reachable and the disclosed-loss accounting for swept lines
    # is exercised against a real deletion.
    quint-log-service-witness-swept = mkQuintWitnessCheck {
      name = "log-service-witness-swept";
      spec = "logService";
      main = "logServiceSweep";
      witness = "noSweptChunks";
    };

    # ------------------------------------------------------------------
    # rio-scheduler's retry/poison/cascade machinery: the post-collapse
    # model (retry-formal Phase 1c). retryPolicy.qnt encodes the code as
    # it exists after the Phase-1b nine-site collapse -- every entry
    # point's verdict is the reference fold (decide()) over the durable
    # attempt ledger, evaluated and persisted in the appending
    # transaction.
    #
    # Executor-campaign 1c' (T-1c'.6): the as-built-CHANNEL regimes
    # (worker / dual / crash / failover) and their named-run and witness
    # checks are retired with the stream machinery they modeled -- the
    # attempt-opening dispatch push, the disconnect channel (E5), the
    # recently_disconnected-correlated controller reports (E6/E7), the
    # correlation-TTL establishment, the backstop (E8) and the
    # dispatch-time fleet-exhaust arm (E9) no longer exist in the code.
    # The pull-mode environment regime (retryPolicyPull, wired at 1b as
    # quint-retry-policy-pull in the executor-campaign block below) is
    # the wired proof of the same fold invariants over the live
    # event-arrival environment; it now also carries the model-checked
    # markers the retired regime checks held. The retirement record --
    # including the disposition of every retired non-vacuity pin and
    # what carries it now -- is in
    # docs/spec/models/retry-invariant-map.md (the executor-campaign
    # 1c' retirement section); the witness vals stay defined in the
    # core module, and the load-bearing ones are re-pinned on the pull
    # regime here.
    #
    # The Stage-B as-built encoding (retryPolicyAsBuilt.qnt), its
    # Stage-C calibration corpus (calibration/retry-*.qnt) and the six
    # quint-retry-calib-* checks were retired in the retry campaign's
    # Phase 2 once the acceptance table consolidated their evidence --
    # see docs/spec/models/retry-invariant-map.md.

    # Non-vacuity witnesses re-pinned on the pull regime (T-1c'.6): the
    # contended states the retired regimes' witnesses pinned that are
    # still constructible -- and load-bearing for the pull check's HOLD
    # list -- are re-proven reachable in the pull regime's explored
    # space (threshold poison, the cache-hit poison clear, the
    # established-crash loop reaching a terminal, a failover landing on
    # a non-empty history). Two further re-pins (the poison-TTL expiry
    # clear and the failover-on-a-live-poisoned-row state) are verified
    # violating on this regime but are demoted to documented manual
    # targets instead of wired checks: their derivations repeatedly hit
    # the cold-server conversion failure documented above while four
    # identically-shaped siblings built green, so the demotion is a
    # tooling-budget call, recorded in the retry map, not a
    # reachability gap. The pins whose producers were deleted with the
    # stream machinery (the appending-tx fault at TX_FAULTS = 0, the
    # as-built channel-ordering shapes) are likewise recorded as
    # retired in the retry map rather than silently dropped.
    quint-retry-policy-pull-witness-threshold = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-threshold";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noThresholdPoison";
    };
    quint-retry-policy-pull-witness-cache-hit = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-cache-hit";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noCacheHitClear";
    };
    quint-retry-policy-pull-witness-crash-terminal = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-crash-terminal";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noCrashLoopTerminal";
    };
    quint-retry-policy-pull-witness-failover-history = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-failover-history";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noFailoverWithHistory";
    };

    # ------------------------------------------------------------------
    # rio-store's chunk reference-counting subsystem, AS BUILT: the exact
    # chunks.refcount counter and the claim/heartbeat/drop-guard/token/
    # reaper machinery that maintains it (chunkLiveness.qnt — the
    # refcount-formal campaign's Phase-0 Stage-B model; the invariant ↔
    # rule map is docs/spec/models/refcount-invariant-map.md). Four
    # exhaustive regimes mirror design §3.2: base (one writer, no
    # faults), crash (process death between transactions, the drain's
    # mid-commit crash), contend (two writers sharing chunks against the
    # GC pipeline and drain), corrupt (an unparseable chunk_list at
    # deletion time — the C12 skip). The corrupt regime checks the
    # CARVED forms of CR-2/CR-3 (the rules' sanctioned-deviation
    # clauses); the unconditional forms falsifying there is the
    # pre-registered as-built deviation, wired below as expect-violation
    # checks so the leak class stays demonstrated, not assumed. State
    # counts, depths and wall-clocks live in the introducing commits'
    # messages and the checks' transcripts.
    #
    # Marker scope: of the Stage-A spec-audit rules, the surviving ones
    # (no-live-collect, liveness-not-presence) keep verify markers here,
    # on the regime that makes each load-bearing; the two as-built
    # counter rules the audit added (refcount-meaning,
    # refcount-decrement) were retired with the counter writers in
    # Release B, so these checks no longer claim spec coverage for them
    # — the checks themselves stay wired against the as-built model
    # until its own retirement (Phase 2). Pre-existing mechanism rules
    # the as-built machinery still implements (upsert-inserted,
    # chunk-upload-committed, placeholder-claim, orphan-heartbeat) keep
    # the model-checked form on top of their unit-test markers; rules
    # whose text now describes the replacement collector (refcount-txn's
    # upsert+touch pairing, grace-ttl, pending-deletes,
    # bounded-garbage-retention, two-phase) carry their verify markers
    # at the chunkCollect wirings below instead. Witness checks carry no
    # markers (same policy as the other models).
    # ------------------------------------------------------------------

    # The base regime: one writer over two paths and three hashes with
    # the full chunk-list alphabet, the explicit abort/rollback paths,
    # the path sweep, the orphan-chunk sweep and the drain — no faults.
    # The counter must equal the manifest fold at every state, garbage
    # must stay reclaimable, and no referenced chunk's object may be
    # deleted.
    # r[verify store.chunk.no-live-collect]
    quint-chunk-liveness-base = mkQuintCheck {
      name = "chunk-liveness-base";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      invariants = [
        "boundsOK"
        "m023NonNegative"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr3CounterRefinesFold"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
      ];
    };

    # The crash regime: two writers with process death enabled at every
    # in-flight phase (C1/C2/C5/C6/C7), the drain's S3-delete-then-
    # commit-fails window (C10), and the stale-reclaim repair pair (the
    # 300 s hot-path reclaim and the 15-minute scanner). The counter
    # stays exactly equal to the fold through every crash window (the
    # abandoned rows keep their manifests, so the garbage is accounted
    # garbage), a reclaimed chunk's cleared uploaded_at forces the next
    # writer to re-PUT instead of trusting the counter, and a live
    # heartbeating owner is never reaped.
    # r[verify store.chunk.no-live-collect]
    # r[verify store.chunk.liveness-not-presence]
    # r[verify store.cas.upsert-inserted+3]
    # r[verify store.cas.chunk-upload-committed]
    # r[verify store.gc.orphan-heartbeat]
    # r[verify store.put.placeholder-claim+2]
    quint-chunk-liveness-crash = mkQuintCheck {
      name = "chunk-liveness-crash";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      invariants = [
        "boundsOK"
        "m023NonNegative"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr3CounterRefinesFold"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
      ];
    };

    # The contend regime: two live writers sharing a chunk against the
    # GC pipeline and the drain — the G4a collect-vs-re-reference TOCTOU
    # family, the orphan-chunk sweep's select-vs-update race (C11), the
    # by-count batch sweep, and the late-cleanup no-op contention. No
    # process death: every interleaving is a healthy-process schedule.
    # r[verify store.chunk.no-live-collect]
    # r[verify store.chunk.liveness-not-presence]
    quint-chunk-liveness-contend = mkQuintCheck {
      name = "chunk-liveness-contend";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      invariants = [
        "boundsOK"
        "m023NonNegative"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr3CounterRefinesFold"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
      ];
    };

    # The corrupt regime: an existing manifest_data.chunk_list can rot
    # and every deletion path that parses it at delete time skips the
    # decrement while still deleting the manifest (C12). The CARVED
    # invariant forms hold: the counter equals the fold plus exactly the
    # observably-skipped decrements, stranded garbage is exactly the
    # skipped amount, and the data-loss invariant is untouched (the skip
    # errs toward retention). The unconditional forms are the
    # pre-registered falsifications below, never invariants here.
    # r[verify store.chunk.no-live-collect]
    quint-chunk-liveness-corrupt = mkQuintCheck {
      name = "chunk-liveness-corrupt";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
      invariants = [
        "boundsOK"
        "m023NonNegative"
        "cr1NoLiveChunkCollected"
        "cr2CarvedCorrupt"
        "cr3CarvedCorrupt"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
      ];
    };

    # The deterministic reproducer runs, one check per regime: the
    # happy-path walkthrough, the own-heartbeat token no-op (C4), the
    # crash-then-hot-path-reclaim-then-re-upload shape (I-040/I-207),
    # the shared-chunk by-count batch sweep, and the corrupt-skip
    # permanent leak are replayed step by step with their expectations
    # re-asserted.
    quint-chunk-liveness-runs-base = mkQuintRunCheck {
      name = "chunk-liveness-runs-base";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
    };
    quint-chunk-liveness-runs-crash = mkQuintRunCheck {
      name = "chunk-liveness-runs-crash";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
    };
    quint-chunk-liveness-runs-contend = mkQuintRunCheck {
      name = "chunk-liveness-runs-contend";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
    };
    quint-chunk-liveness-runs-corrupt = mkQuintRunCheck {
      name = "chunk-liveness-runs-corrupt";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
    };

    # Non-vacuity witnesses for the chunkLiveness regimes. Each check
    # passes only when the checker violates its witness — machine-checked
    # evidence that the scenario a regime's invariants constrain is
    # actually reachable in that regime's explored space. Deliberately no
    # tracey markers here (same policy as the other models' witnesses).

    # The base regime's headline states: a complete chunked upload
    # exists, a backend delete actually fires, a referenced chunk whose
    # presence is not yet confirmed exists (the M_033 precondition), the
    # own-heartbeat token no-op (C4) is reachable, and the heartbeat
    # actually resets staleness.
    quint-chunk-liveness-witness-complete-upload = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-complete-upload";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noCompleteUpload";
    };
    quint-chunk-liveness-witness-backend-delete = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-backend-delete";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noBackendDelete";
    };
    quint-chunk-liveness-witness-m033-precondition = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-m033-precondition";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noUnconfirmedReferencedChunk";
    };
    quint-chunk-liveness-witness-stale-token = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-stale-token";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noStaleTokenRollbackNoop";
    };
    quint-chunk-liveness-witness-heartbeat-reset = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-heartbeat-reset";
      spec = "chunkLiveness";
      main = "chunkLivenessBase";
      witness = "noHeartbeatReset";
    };

    # The crash regime's fault alphabet is reachable: the C1 (claimed),
    # C2 (upgraded) and C5 (cleanup-pending — the state C3/C7 collapse
    # onto) crash windows, the two-writers-staged-then-crashed C6 shape,
    # the abandoned-but-accounted leak state those windows leave behind,
    # and both stale-reclaim repair paths (hot-path and scanner)
    # actually firing.
    quint-chunk-liveness-witness-crash-claimed = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-crash-claimed";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noCrashAtClaimed";
    };
    quint-chunk-liveness-witness-crash-upgraded = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-crash-upgraded";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noCrashAfterUpgrade";
    };
    quint-chunk-liveness-witness-crash-pending-reap = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-crash-pending-reap";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noCrashBeforeReap";
    };
    quint-chunk-liveness-witness-double-crash-staged = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-double-crash-staged";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noDoubleCrashStaged";
    };
    quint-chunk-liveness-witness-abandoned-accounting = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-abandoned-accounting";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noAbandonedAccounting";
    };
    quint-chunk-liveness-witness-hotpath-reclaim = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-hotpath-reclaim";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noHotpathReclaim";
    };
    quint-chunk-liveness-witness-scanner-reap = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-scanner-reap";
      spec = "chunkLiveness";
      main = "chunkLivenessCrash";
      witness = "noScannerReap";
    };

    # The contend regime's contended states: a shared chunk decremented
    # by count in one batch transaction, the drain re-check skipping a
    # resurrected chunk, the orphan-chunk sweep's inner re-check
    # excluding a candidate resurrected after the outer SELECT (C11),
    # and an owner-side cleanup no-opping against a foreign or missing
    # row.
    quint-chunk-liveness-witness-shared-by-count = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-shared-by-count";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      witness = "noSharedByCountDecrement";
    };
    quint-chunk-liveness-witness-drain-resurrect = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-drain-resurrect";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      witness = "noDrainResurrectSkip";
    };
    quint-chunk-liveness-witness-orphan-recheck = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-orphan-recheck";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      witness = "noOrphanRecheckSave";
    };
    quint-chunk-liveness-witness-late-cleanup-noop = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-late-cleanup-noop";
      spec = "chunkLiveness";
      main = "chunkLivenessContend";
      witness = "noLateCleanupNoop";
    };

    # The pre-registered as-built deviations of the corrupt regime
    # (refcount-invariant-map.md, CR-2/CR-3 rows): with a corrupt
    # chunk_list skip, the exact counter-equals-fold form and the
    # unconditional no-stranded-garbage form MUST falsify — the C12
    # permanent leak the design's replacement exists to dissolve. These
    # are reproducer checks for documented defects, not regressions: if
    # either stops falsifying, the model has stopped reaching the leak
    # (or the code stopped leaking) and the invariant map's Stage-B
    # section must be revisited. The third check pins the literal leak
    # shape (refcount above zero with no referencing manifest).
    quint-chunk-liveness-corrupt-c12-overcount = mkQuintWitnessCheck {
      name = "chunk-liveness-corrupt-c12-overcount";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
      witness = "cr3CounterRefinesFold";
    };
    quint-chunk-liveness-corrupt-c12-stranded = mkQuintWitnessCheck {
      name = "chunk-liveness-corrupt-c12-stranded";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
      witness = "cr2NoStrandedGarbage";
    };
    quint-chunk-liveness-witness-corrupt-leak = mkQuintWitnessCheck {
      name = "chunk-liveness-witness-corrupt-leak";
      spec = "chunkLiveness";
      main = "chunkLivenessCorrupt";
      witness = "noCorruptLeak";
    };

    # The threshold-ordering inversion: with the hot-path reclaim
    # threshold lowered to the heartbeat deadline (production:
    # 30 s heartbeat vs 300 s hot-path vs 900 s scanner), reaping a
    # live, progressing owner becomes reachable and S5 falsifies — the
    # machine-checked evidence that the scaled clock constants preserve
    # the ordering the heartbeat/reclaim design depends on, and that S5
    # does not hold vacuously in the well-ordered regimes.
    quint-chunk-liveness-threshold-order = mkQuintWitnessCheck {
      name = "chunk-liveness-threshold-order";
      spec = "chunkLiveness";
      main = "chunkLivenessThresholdOrder";
      witness = "s5LiveOwnerNeverReaped";
    };

    # Stage-C calibration witnesses for the chunk-refcount subsystem (the
    # historical-fix corpus replayed against the as-built model — the
    # refcount-formal campaign's Phase-0 Stage C). Each check
    # instantiates the as-built chunkLiveness model, swaps ONE owner-side
    # entry point for its PRE-FIX behavior (the calibration module's
    # `calibStep`), and passes only while the checker still falsifies the
    # invariant the corresponding historical fix protects — machine-
    # checked evidence that the model would re-find that bug class if it
    # were reintroduced, and that the invariant is not vacuous for it.
    # The full per-commit calibration table (and the evidence-only
    # override modules that are not wired here) lives in
    # docs/spec/models/refcount-invariant-map.md; the wired ones are the
    # representative per-family regression guards (one per encodable
    # family, deepest consequence, cheap state space). Deliberately no
    # tracey markers (same policy as the other witness checks).
    #
    # The G1 (token rollback) and G2 (inline-only reap) guards were
    # retired at Release B of the refcount-formal campaign: the
    # PlaceholderToken and the decrement family they guarded no longer
    # exist in the code, so the regressions they watched for cannot
    # recur by construction. Their override modules
    # (calibration/refcount-g{1,2}.qnt) stay committed as evidence and
    # remain re-runnable by hand; the disposition record is in the
    # invariant map ("Release B calibration-check disposition"). The
    # surviving G3/G4a/G5 guards below watch mechanisms that outlived
    # the counter (presence keyed on uploaded_at, the drain re-check,
    # the heartbeat) and stay wired against the as-built model until
    # Phase 2 re-points them at the model of record.

    # G3 (dd5c11376 / M_033): the needs-upload verdict is keyed on the
    # liveness record instead of uploaded_at — a writer skips the PUT for
    # a chunk nobody confirmed, the 2026-04-06 data-loss precondition.
    quint-refcount-calib-g3-counter-presence = mkQuintWitnessCheck {
      name = "refcount-calib-g3-counter-presence";
      spec = "calibration/refcount-g3";
      main = "refcountCalibG3CounterAsPresence";
      extraSpecs = [ "chunkLiveness" ];
      step = "calibStep";
      witness = "cr4PresenceFromConfirmedUpload";
    };

    # G4a (aa738a5d7 / M_006): the drain loses its same-transaction
    # re-check before DeleteObject — a chunk resurrected by a re-upload
    # between soft-delete and drain loses its object while referenced
    # (the data-loss invariant's action form).
    quint-refcount-calib-g4a-drain-recheck = mkQuintWitnessCheck {
      name = "refcount-calib-g4a-drain-recheck";
      spec = "calibration/refcount-g4a";
      main = "refcountCalibG4aDrainNoRecheck";
      extraSpecs = [ "chunkLiveness" ];
      step = "calibStep";
      witness = "cr1NoLiveChunkCollected";
    };

    # G5 (a1b49b4a3): no heartbeat — a live, progressing upload outlives
    # the stale threshold and the reclaim path reaps it mid-flight.
    quint-refcount-calib-g5-no-heartbeat = mkQuintWitnessCheck {
      name = "refcount-calib-g5-no-heartbeat";
      spec = "calibration/refcount-g5";
      main = "refcountCalibG5NoHeartbeat";
      extraSpecs = [ "chunkLiveness" ];
      step = "calibStep";
      witness = "s5LiveOwnerNeverReaped";
    };

    # ------------------------------------------------------------------
    # rio-store's chunk-liveness subsystem, REPLACEMENT shape: lazy
    # mark-and-collect over the durable manifests (chunkCollect.qnt —
    # the refcount-formal campaign's Phase-1a / plan T-1a.5 model,
    # design §4/§4.6; the verdict table and witness list are in
    # docs/spec/models/refcount-invariant-map.md "Replacement model").
    # The model is the counter-free end state: no refcount, no
    # decrement/token machinery; the collector of
    # rio-store/src/gc/collect.rs (snapshot → fail-closed mark →
    # per-batch sweep → finish) modeled with its live (soft-delete +
    # enqueue) arm, the migration-068 last_referenced_at touch, the
    # T-pre.1 deleted-guard on the presence commit, reap/path-sweep as
    # path-row janitors, and the deleted-only drain re-check. Four
    # exhaustive regimes mirror the as-built model's; two further
    # exhaustive regimes carry the holds-halves of the §4.6
    # falsification pairs (writer-transaction bound; late-mark guard
    # under a relaxed heartbeat contract); each pair's falsify-half is
    # an expect-violation check against an instantiation that flips
    # exactly one design guard off. State counts, depths and
    # wall-clocks live in the introducing commits' messages and the
    # checks' transcripts.
    #
    # Marker scope: the two replacement rules added by plan T-1a.5
    # (store.chunk.liveness-derived, store.gc.chunk-collect) carry
    # their verify markers here, on the regimes that make each
    # load-bearing; their implementation-side markers live with the
    # live collector arm (gc::collect). Surviving mechanism rules whose
    # text is unchanged (no-live-collect, liveness-not-presence,
    # upsert-inserted, chunk-upload-committed, placeholder-claim,
    # orphan-heartbeat) gain the replacement-model form on top of their
    # as-built chunk-liveness markers, which stay in place until the
    # as-built model's own retirement (Phase 2). Rules amended at the
    # cutover to describe collector behavior (grace-ttl,
    # bounded-garbage-retention, pending-deletes) carry their bumped
    # verify markers here only — the as-built model checks the
    # pre-cutover machinery, not the amended sentences. Witness and
    # falsification checks carry no markers (same policy as every
    # other expect-violation check).
    # ------------------------------------------------------------------

    # The base regime: one writer over two paths and three hashes with
    # the full chunk-list alphabet, the collector and the drain — no
    # faults. Liveness is recomputed from the manifests each cycle; no
    # referenced chunk's object may be deleted, garbage must stay
    # reclaimable, and presence is never inferred from liveness.
    # r[verify store.chunk.liveness-derived]
    # r[verify store.gc.chunk-collect]
    # r[verify store.chunk.no-live-collect]
    # r[verify store.gc.bounded-garbage-retention+2]
    # r[verify store.chunk.refcount-txn+2]
    quint-chunk-collect-base = mkQuintCheck {
      name = "chunk-collect-base";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The crash regime: two writers with process death enabled at every
    # in-flight phase, the drain's S3-delete-then-commit-fails window,
    # the path-row janitors, and the collector. A crashed upload's
    # manifest is reaped by the janitors; its chunks are then ordinary
    # unreferenced rows the next cycle collects — no counter repair
    # exists or is needed, and the cleared uploaded_at still forces the
    # next writer to re-PUT instead of trusting liveness for presence.
    # r[verify store.chunk.liveness-derived]
    # r[verify store.chunk.no-live-collect]
    # r[verify store.gc.bounded-garbage-retention+2]
    # r[verify store.chunk.liveness-not-presence]
    # r[verify store.cas.upsert-inserted+3]
    # r[verify store.cas.chunk-upload-committed]
    # r[verify store.gc.orphan-heartbeat]
    # r[verify store.put.placeholder-claim+2]
    quint-chunk-collect-crash = mkQuintCheck {
      name = "chunk-collect-crash";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The contend regime: two live writers sharing chunks against the
    # collector and the drain — the mark-stale race the
    # last_referenced_at touch closes (a manifest committing after the
    # cycle's mark snapshot re-references an old chunk), the
    # resurrect-vs-drain TOCTOU with the deleted-only re-check, and the
    # late-cleanup no-op contention. No process death. The touch/grace
    # term this regime exercises is the amended grace-ttl eligibility
    # predicate (zero references at the mark snapshot AND older than
    # grace from GREATEST(created_at, last_referenced_at)).
    # r[verify store.chunk.liveness-derived]
    # r[verify store.gc.chunk-collect]
    # r[verify store.chunk.no-live-collect]
    # r[verify store.gc.bounded-garbage-retention+2]
    # r[verify store.chunk.liveness-not-presence]
    # r[verify store.gc.pending-deletes+2]
    # r[verify store.chunk.grace-ttl+2]
    quint-chunk-collect-contend = mkQuintCheck {
      name = "chunk-collect-contend";
      spec = "chunkCollect";
      main = "chunkCollectContend";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The corrupt regime: an existing manifest_data.chunk_list can rot
    # (C12). Under the replacement the polarity is fail-closed — the
    # validation pass aborts the cycle, nothing anywhere is collected
    # while the corruption persists, and the operator quarantine of
    # adjudication 7 is the sanctioned remediation. CR-2 is checked in
    # its carved form here; the unconditional form's expected
    # falsification is wired below as an expect-violation check.
    # r[verify store.chunk.liveness-derived]
    # r[verify store.gc.chunk-collect]
    # r[verify store.chunk.no-live-collect]
    # r[verify store.gc.bounded-garbage-retention+2]
    quint-chunk-collect-corrupt = mkQuintCheck {
      name = "chunk-collect-corrupt";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2CarvedCorrupt"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The writer-transaction-bound holds-half (§4.1 collect-soundness
    # condition): the upgrade transaction is split into begin/commit
    # and its duration is ceiling-bounded at the grace window by a tick
    # precondition — CR-1 holds for every interleaving and any
    # cycle/scan placement. The overrun falsify-half below relaxes the
    # ceiling by one tick. In production the bound is the monitored
    # rio_store_chunk_upgrade_tx_seconds assumption (plan T-1a.4).
    # r[verify store.gc.chunk-collect]
    # r[verify store.chunk.no-live-collect]
    quint-chunk-collect-writer-bounded = mkQuintCheck {
      name = "chunk-collect-writer-bounded";
      spec = "chunkCollect";
      main = "chunkCollectWriterBounded";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "cr2NoStrandedGarbage"
        "cr4PresenceFromConfirmedUpload"
        "s4OwnerOnlyMutation"
        "s5LiveOwnerNeverReaped"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The late-mark holds-half (Phase-1 input list item 1): under a
    # relaxed heartbeat contract (a live owner may stall past the
    # reclaim thresholds), the T-pre.1 `AND deleted = FALSE` guard on
    # mark_chunks_uploaded keeps CR-1 standing. The invariant list is
    # deliberately CR-1 plus the structural/ownership set: the
    # relaxation itself makes S5 violable and opens benign bookkeeping
    # windows (orphan re-PUT, transient uploaded_at-vs-object skew)
    # with no data-loss content — the regime module's comment carries
    # the full rationale. The unguarded falsify-half is wired below.
    # r[verify store.chunk.no-live-collect]
    # r[verify store.cas.chunk-upload-committed]
    quint-chunk-collect-latemark-guarded = mkQuintCheck {
      name = "chunk-collect-latemark-guarded";
      spec = "chunkCollect";
      main = "chunkCollectLateMarkGuarded";
      invariants = [
        "boundsOK"
        "cr1NoLiveChunkCollected"
        "s4OwnerOnlyMutation"
        "l3NoForeignFreshen"
        "noReferencedChunkSwept"
      ];
    };

    # The deterministic reproducer runs, one check per regime: the
    # collector happy path and the capped-cycle/backstop resume shape,
    # the crashed-upload-collected-one-cycle-later shape, the
    # mark-stale touch-protection walkthrough, and the fail-closed
    # abort + quarantine-then-resume narratives are replayed step by
    # step with their expectations re-asserted.
    quint-chunk-collect-runs-base = mkQuintRunCheck {
      name = "chunk-collect-runs-base";
      spec = "chunkCollect";
      main = "chunkCollectBase";
    };
    quint-chunk-collect-runs-crash = mkQuintRunCheck {
      name = "chunk-collect-runs-crash";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
    };
    quint-chunk-collect-runs-contend = mkQuintRunCheck {
      name = "chunk-collect-runs-contend";
      spec = "chunkCollect";
      main = "chunkCollectContend";
    };
    quint-chunk-collect-runs-corrupt = mkQuintRunCheck {
      name = "chunk-collect-runs-corrupt";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
    };

    # Non-vacuity witnesses for the chunkCollect regimes. Each check
    # passes only when the checker violates its witness — the contended
    # state the regime's invariants constrain is actually reachable.
    # Deliberately no tracey markers (same policy as every witness
    # check).

    # The base regime's headline states: a complete chunked upload
    # exists, a backend delete fires, a referenced-but-unconfirmed
    # chunk exists (the M_033 precondition restated over the fold), the
    # heartbeat actually resets staleness, and a collect batch actually
    # collects something.
    quint-chunk-collect-witness-complete-upload = mkQuintWitnessCheck {
      name = "chunk-collect-witness-complete-upload";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noCompleteUpload";
    };
    quint-chunk-collect-witness-backend-delete = mkQuintWitnessCheck {
      name = "chunk-collect-witness-backend-delete";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noBackendDelete";
    };
    quint-chunk-collect-witness-m033-precondition = mkQuintWitnessCheck {
      name = "chunk-collect-witness-m033-precondition";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noUnconfirmedReferencedChunk";
    };
    quint-chunk-collect-witness-heartbeat-reset = mkQuintWitnessCheck {
      name = "chunk-collect-witness-heartbeat-reset";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noHeartbeatReset";
    };
    quint-chunk-collect-witness-chunk-collected = mkQuintWitnessCheck {
      name = "chunk-collect-witness-chunk-collected";
      spec = "chunkCollect";
      main = "chunkCollectBase";
      witness = "noChunkCollected";
    };

    # The crash regime's fault alphabet is reachable: the C1/C2/C5
    # crash windows, the two-writers-staged-then-crashed C6 shape, the
    # abandoned-upload garbage those windows leave for the collector,
    # and both stale-reclaim repair paths firing.
    quint-chunk-collect-witness-crash-claimed = mkQuintWitnessCheck {
      name = "chunk-collect-witness-crash-claimed";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noCrashAtClaimed";
    };
    quint-chunk-collect-witness-crash-upgraded = mkQuintWitnessCheck {
      name = "chunk-collect-witness-crash-upgraded";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noCrashAfterUpgrade";
    };
    quint-chunk-collect-witness-crash-pending-reap = mkQuintWitnessCheck {
      name = "chunk-collect-witness-crash-pending-reap";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noCrashBeforeReap";
    };
    quint-chunk-collect-witness-double-crash-staged = mkQuintWitnessCheck {
      name = "chunk-collect-witness-double-crash-staged";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noDoubleCrashStaged";
    };
    quint-chunk-collect-witness-abandoned-upload = mkQuintWitnessCheck {
      name = "chunk-collect-witness-abandoned-upload";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noAbandonedUploadGarbage";
    };
    quint-chunk-collect-witness-hotpath-reclaim = mkQuintWitnessCheck {
      name = "chunk-collect-witness-hotpath-reclaim";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noHotpathReclaim";
    };
    quint-chunk-collect-witness-scanner-reap = mkQuintWitnessCheck {
      name = "chunk-collect-witness-scanner-reap";
      spec = "chunkCollect";
      main = "chunkCollectCrash";
      witness = "noScannerReap";
    };

    # The contend regime's contended states: the §4.6 mark-stale
    # reachability witness (a post-snapshot upgrade re-references an
    # unmarked, past-grace chunk and only the touch retains it while a
    # sweep batch runs — the race the new column exists to close), the
    # drain re-check skipping a resurrected chunk under the
    # deleted-only re-check, and a claim-gated cleanup no-opping
    # against a foreign row.
    quint-chunk-collect-witness-mark-miss-touch-saved = mkQuintWitnessCheck {
      name = "chunk-collect-witness-mark-miss-touch-saved";
      spec = "chunkCollect";
      main = "chunkCollectContend";
      witness = "noMarkMissSavedByTouch";
    };
    quint-chunk-collect-witness-drain-resurrect = mkQuintWitnessCheck {
      name = "chunk-collect-witness-drain-resurrect";
      spec = "chunkCollect";
      main = "chunkCollectContend";
      witness = "noDrainResurrectSkip";
    };
    quint-chunk-collect-witness-late-cleanup-noop = mkQuintWitnessCheck {
      name = "chunk-collect-witness-late-cleanup-noop";
      spec = "chunkCollect";
      main = "chunkCollectContend";
      witness = "noLateCleanupNoop";
    };

    # The corrupt regime's reachability set: the fail-closed abort
    # actually fires (the alert image), the adjudication-7 quarantine
    # fires, and CR-2's unconditional structural form is — exactly as
    # the design owns in §4.4 — unsatisfiable while a corrupt manifest
    # coexists with collectable garbage (the fail-closed pause made
    # visible; the carved form is the regime's invariant above).
    quint-chunk-collect-witness-parse-abort = mkQuintWitnessCheck {
      name = "chunk-collect-witness-parse-abort";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
      witness = "noParseFailureAbort";
    };
    quint-chunk-collect-witness-quarantine = mkQuintWitnessCheck {
      name = "chunk-collect-witness-quarantine";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
      witness = "noQuarantine";
    };
    quint-chunk-collect-corrupt-pause-stranded = mkQuintWitnessCheck {
      name = "chunk-collect-corrupt-pause-stranded";
      spec = "chunkCollect";
      main = "chunkCollectCorrupt";
      witness = "cr2NoStrandedGarbage";
    };

    # The threshold-ordering inversion, carried over from the as-built
    # model: with the hot-path reclaim threshold lowered to the
    # heartbeat deadline, reaping a live owner becomes reachable and S5
    # falsifies — the path-row janitors keep the ordering dependence
    # under the replacement.
    quint-chunk-collect-threshold-order = mkQuintWitnessCheck {
      name = "chunk-collect-threshold-order";
      spec = "chunkCollect";
      main = "chunkCollectThresholdOrder";
      witness = "s5LiveOwnerNeverReaped";
    };

    # The §4.6 falsification pairs, falsify-halves: each check
    # instantiates the SAME transition relation with exactly one design
    # guard flipped off and passes only while the checker still
    # falsifies CR-1 — machine-checked evidence that each replacement
    # mechanism is load-bearing, not decorative. The matching
    # holds-halves are the wired exhaustive checks above (contend for
    # the touch, corrupt for fail-closed, writer-bounded for the
    # transaction bound, latemark-guarded for the deleted-guard).

    # Touch removed (ENABLE_TOUCH = false; design §4.6 (ii), the
    # required falsification for the one mechanism with no historical
    # fix): a manifest commits after the mark snapshot against an old
    # uploaded chunk, the writer skips the PUT, the sweep collects the
    # chunk and the drain deletes the only copy.
    quint-chunk-collect-no-touch-falsifies-cr1 = mkQuintWitnessCheck {
      name = "chunk-collect-no-touch-falsifies-cr1";
      spec = "chunkCollect";
      main = "chunkCollectNoTouch";
      witness = "cr1NoLiveChunkCollected";
    };

    # Writer-transaction bound relaxed by one tick past grace
    # (WRITER_TX_BOUND = GRACE + 1): an upgrade transaction that
    # outlives the grace window leaves its backdated touch before the
    # cycle cutoff, so the post-commit re-evaluation still collects the
    # chunk the just-committed manifest references — the §4.1
    # collect-soundness condition, exercised.
    quint-chunk-collect-writer-overrun-falsifies-cr1 = mkQuintWitnessCheck {
      name = "chunk-collect-writer-overrun-falsifies-cr1";
      spec = "chunkCollect";
      main = "chunkCollectWriterOverrun";
      witness = "cr1NoLiveChunkCollected";
    };

    # Fail-closed rule removed (ENABLE_FAIL_CLOSED = false): the mark
    # silently skips an unparseable manifest instead of aborting — the
    # forbidden C12 polarity flip — and a chunk whose only referrer is
    # the corrupt manifest is collected and drained while that manifest
    # exists (design §4.4 / adjudication 5).
    quint-chunk-collect-parse-skip-falsifies-cr1 = mkQuintWitnessCheck {
      name = "chunk-collect-parse-skip-falsifies-cr1";
      spec = "chunkCollect";
      main = "chunkCollectParseSkip";
      witness = "cr1NoLiveChunkCollected";
    };

    # Late-mark guard removed under the relaxed heartbeat contract
    # (ENABLE_MARK_DELETED_GUARD = false): a stalled owner's late
    # presence commit re-asserts uploaded_at on a collected chunk and
    # the next writer trusts it — the M_033 harm shape with no counter
    # involved (Phase-1 input list item 1 / plan T-pre.1).
    quint-chunk-collect-latemark-unguarded-falsifies-cr1 = mkQuintWitnessCheck {
      name = "chunk-collect-latemark-unguarded-falsifies-cr1";
      spec = "chunkCollect";
      main = "chunkCollectLateMarkUnguarded";
      witness = "cr1NoLiveChunkCollected";
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

    # Implementation conformance for the LogService model: rio-store/
    # src/logs/mbt_tests.rs replays traces from logService.qnt against
    # the real open-gate / ingest-session / chunk-manifest / read-path /
    # TTL-sweep code, diffing the projected state (the manifest, the
    # session high-water/ceiling/buffer, the lifecycle row's terminal
    # stamp) after every step and re-running the real TailLog dedup walk
    # over the manifest after every step (the servedSpanExact invariant
    # checked against the implementation's fold instead of the model's).
    # Six named runs span all four regime modules (base, redispatch,
    # resend, sweep); the #[quint_run] simulation random-walks the base
    # regime under a pinned seed. Builder-side uploader actions and the
    # in-place buffer drop of the abort path are driver bookkeeping —
    # the scoping rationale lives in the test module's header.
    #
    # Wiring: the same prebuilt rio-store test binary nextest-rio-store
    # runs, with quint on PATH, postgres on PATH (the `member`
    # indirection pulls in rio-store's runtimeTestInputs — the ephemeral
    # PG the projection reads real manifest rows from), and the model
    # staged into the remapped nextest workspace so a model edit re-runs
    # this check.
    #
    # The markers cover what the replayed traces exercise end-to-end:
    # the latest-assignment + already-complete open rejections
    # (append-auth), the open-time seal and the per-append ceiling with
    # its mid-stream refresh (completeness-gate), and the
    # overlapping-session manifest dedup on the read path
    # (session-keyed).
    # r[verify store.log.append-auth]
    # r[verify store.log.completeness-gate]
    # r[verify store.log.session-keyed]
    mbt-rio-logservice = mkNextestRun {
      name = "mbt-rio-logservice";
      member = "rio-store";
      meta = mkNextestMeta { rio-store = rioStoreTestBin; };
      extraRuntimeInputs = [ pkgs.quint ];
      extraArgs = [
        "-E"
        "package(rio-store) and test(/mbt_/)"
        "--run-ignored"
        "all"
      ];
      preRun = ''
        export RIO_MBT_SPEC_PATH=$TMPDIR/ws/docs/spec/models/logService.qnt
      '';
      postWsSetup = ''
        mkdir -p $ws/docs/spec/models
        cp ${logServiceModel}/logService.qnt $ws/docs/spec/models/
      '';
      postRun = ''
        # Same no-tests guard as mbt-rio-lease: --no-tests=warn would
        # otherwise turn a filter that matches nothing into a green
        # check that proved nothing.
        grep -E ' [1-9][0-9]* tests? run:' $out/log > /dev/null || {
          echo "mbt-rio-logservice: the mbt_* filter matched no tests" >&2
          exit 1
        }
      '';
    };

    # ==================================================================
    # rio-controller's reconcile protocols (controller-formal Phase 0,
    # Stage B): two tick-level models of the two reconcile loops.
    #
    #   spawnCoherence.qnt (Model J) — the L1 Pool reconciler's
    #   same-tick coherence protocol (pool/{jobs,job}.rs): the intent
    #   poll, the 3-valued placeable gate x pool kind, the Job
    #   LIST/census, the stale/excess/orphan reap arms with their
    #   fail-closed gates, the headroom arithmetic, the 409-deduped
    #   spawn pass and the dispatched_cells-arming ack. Six
    #   configurations: base / fault-rpc / fault-lease / fault-stale on
    #   the production Builder+CRD shape, plus the crd-absent and
    #   Fetcher-pool postures (the C1/C2 adjudications in
    #   docs/spec/models/controller-invariant-map.md).
    #
    #   nodeclaimLifecycle.qnt (Model N) — the L10 NodeClaim-pool
    #   reconciler's mirror lifecycle (nodeclaim_pool/): the lease-edge
    #   polarity table (prev_idle / recorded_boot / inflight_created /
    #   the sketches reload latch), the ⊥-streak early-return and
    #   consolidate-only modes, vanish detection vs the controller's own
    #   reaps, the recency-gated Registered clears, the placeable-gate
    #   producer guarantee, and the global / per-class / per-tick cover
    #   budgets over the hw-class config mirror. Four regimes: base /
    #   fault-rpc / fault-lease / fault-karpenter.
    #
    # The Stage-A invariant map (controller-invariant-map.md) is the
    # rule <-> invariant ledger for both models; its Stage-B section
    # records the per-regime verdicts. The two pre-registered as-built
    # falsifications (the ⊥-tick early-return observation skip) are
    # wired as expect-violation checks plus a named-run check below —
    # they stay green only while the documented defect reproduces, and
    # flip to HOLD checks when the fix lands. State counts, depths and
    # wall-clocks live in the introducing commit messages and the
    # checks' transcripts.
    # ==================================================================

    # ---- Model J: exhaustive regime checks ---------------------------

    # Production shape (Builder pool, CRD present), no faults: the
    # healthy spawn/reap/ack lifecycle against an environment that moves
    # between ticks, including the informer-lagged Job.status.ready vs
    # the live pod phase and arbitrary (stale-allowed) gate publishes.
    # r[verify ctrl.pool.tick-ordering]
    # r[verify ctrl.pool.ack-spawned-soundness]
    quint-spawn-coherence-base = mkQuintCheck {
      name = "spawn-coherence-base";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
      ];
    };

    # Per-RPC ⊥ faults (GetSpawnIntents / ListExecutors / the ack / the
    # Job create): the per-consumer degradation matrix — spawn fail-open,
    # stale+excess reap fail-closed, orphan reap behind its 3-arm gate,
    # nothing acked on a failed poll.
    # r[verify ctrl.pool.degraded-polarity]
    quint-spawn-coherence-fault-rpc = mkQuintCheck {
      name = "spawn-coherence-fault-rpc";
      spec = "spawnCoherence";
      main = "spawnCoherenceFaultRpc";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
      ];
    };

    # Failover faults: the L10 producer unarms the gate (lease loss) and
    # the scheduler restarts (executor map + dispatched_cells empty,
    # leader young) — the re-ack of already-Pending Jobs re-arms
    # dispatched_cells and the orphan gate stays fail-closed against the
    # young leader's partial executor list.
    # r[verify ctrl.pool.ack-spawned-soundness]
    quint-spawn-coherence-fault-lease = mkQuintCheck {
      name = "spawn-coherence-fault-lease";
      spec = "spawnCoherence";
      main = "spawnCoherenceFaultLease";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
      ];
    };

    # Per-read snapshot staleness: the intent poll may read the previous
    # tick's queue while the Job LIST reads the current state — the
    # I-183 incoherence direction the channels allow. The reap arms stay
    # safe under it (the grace windows and membership reaps bound the
    # damage to churn).
    # r[verify ctrl.pool.tick-ordering]
    quint-spawn-coherence-fault-stale = mkQuintCheck {
      name = "spawn-coherence-fault-stale";
      spec = "spawnCoherence";
      main = "spawnCoherenceFaultStale";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
      ];
    };

    # CRD-absent configuration (static-node / k3s without Karpenter):
    # gate_armed is false every tick — Builder spawns pass through
    # unfiltered (fail-open) while the excess reap stays suppressed
    # (fail-closed), the C1 adjudication of the placeable-gate rule.
    # r[verify ctrl.nodeclaim.placeable-gate+5]
    quint-spawn-coherence-crd-absent = mkQuintCheck {
      name = "spawn-coherence-crd-absent";
      spec = "spawnCoherence";
      main = "spawnCoherenceCrdAbsent";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
      ];
    };

    # Fetcher pool with the CRD present: never spawn-filtered, excess
    # reap keyed only on scheduler reachability — the C2 adjudication.
    # r[verify ctrl.nodeclaim.placeable-gate+5]
    quint-spawn-coherence-fetcher = mkQuintCheck {
      name = "spawn-coherence-fetcher";
      spec = "spawnCoherence";
      main = "spawnCoherenceFetcher";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
      ];
    };

    # ---- Model J: non-vacuity witnesses ------------------------------
    # Each passes only when the checker violates the witness — the
    # contended scenarios the invariants constrain stay reachable.
    # Deliberately no tracey markers (same policy as the other models'
    # witnesses).

    # An excess-pending reap actually fires.
    quint-spawn-coherence-witness-excess-reap = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-excess-reap";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReachExcessReap";
    };
    # An orphan-running reap actually fires (3-arm gate passed).
    quint-spawn-coherence-witness-orphan-reap = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-orphan-reap";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReachOrphanReap";
    };
    # A 409 dedupe occurs (the deterministic-name collision path).
    quint-spawn-coherence-witness-409 = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-409";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReach409Dedupe";
    };
    # An unarmed gate blocks a spawn an armed gate would have made.
    quint-spawn-coherence-witness-gate-blocked = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-gate-blocked";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReachGateBlockedSpawn";
    };
    # A stale-selector (fingerprint drift) reap fires.
    quint-spawn-coherence-witness-drift-reap = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-drift-reap";
      spec = "spawnCoherence";
      main = "spawnCoherenceBase";
      witness = "canReachDriftReap";
    };
    # crd-absent: a Builder spawn proceeds ungated (fail-open half).
    quint-spawn-coherence-witness-ungated-spawn = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-ungated-spawn";
      spec = "spawnCoherence";
      main = "spawnCoherenceCrdAbsent";
      witness = "canReachUngatedSpawn";
    };
    # crd-absent: an excess-pending surplus is left unreaped (fail-closed
    # half — the documented C1 operational cost).
    quint-spawn-coherence-witness-suppressed-excess = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-suppressed-excess";
      spec = "spawnCoherence";
      main = "spawnCoherenceCrdAbsent";
      witness = "canReachSuppressedExcess";
    };

    # ---- Model N: exhaustive regime checks ---------------------------

    # Healthy lifecycle, no faults: create -> register -> busy/idle ->
    # idle-reap with the FFD reservation respected, the per-class clamp
    # over the config mirror, and the placeable publish.
    # r[verify ctrl.nodeclaim.budget.per-class+2]
    quint-nodeclaim-lifecycle-base = mkQuintCheck {
      name = "nodeclaim-lifecycle-base";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleBase";
      invariants = [
        "boundsOK"
        "idleReapSafety"
        "iceMarkSoundness"
        "bootSampleNotLost"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
      ];
    };

    # Per-RPC ⊥ faults: the ⊥-streak's early-return window and
    # consolidate-only mode (reap + prune, no create / republish / ack),
    # ack and create failures (failed creates consume no budget), the
    # ceilings-not-loaded fail-closed gate and the unknown-cell drop.
    # idleReapSafety and bootSampleNotLost are deliberately NOT in this
    # list: their falsification on this regime is the pre-registered
    # as-built defect (the early-return observation skip) and is pinned
    # by the expect-violation checks below.
    # r[verify ctrl.nodeclaim.consolidate-only-degraded]
    # r[verify ctrl.nodeclaim.budget.per-class+2]
    quint-nodeclaim-lifecycle-fault-rpc = mkQuintCheck {
      name = "nodeclaim-lifecycle-fault-rpc";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      invariants = [
        "boundsOK"
        "iceMarkSoundness"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
      ];
    };

    # Lease faults: lose/acquire edges, PG reload failures and the
    # controller restart — the per-field lease-edge polarity table (the
    # unconditional prev_idle clear, the Ok-arm-only suppress clears,
    # the reload latch gating persist) and the producer-side gate
    # guarantee (unarmed on loss before the consumer's next tick).
    # r[verify ctrl.nodeclaim.lease-edge-polarity]
    # r[verify ctrl.nodeclaim.placeable-gate+5]
    # r[verify ctrl.nodeclaim.ice-mark-clear]
    quint-nodeclaim-lifecycle-fault-lease = mkQuintCheck {
      name = "nodeclaim-lifecycle-fault-lease";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      invariants = [
        "boundsOK"
        "idleReapSafety"
        "iceMarkSoundness"
        "bootSampleNotLost"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
      ];
    };

    # Karpenter faults: launch failures, the 1s-GC-vs-10s-tick vanish
    # race and spot termination edges — NodeClaim conservation (the
    # controller's own reaps are removed from inflight_created before
    # detect_vanished, so an ICE mark is only ever emitted for a claim
    # that genuinely vanished or launch-failed).
    # r[verify ctrl.nodeclaim.inflight-conservation]
    # r[verify ctrl.nodeclaim.ice-mark-clear]
    quint-nodeclaim-lifecycle-fault-karpenter = mkQuintCheck {
      name = "nodeclaim-lifecycle-fault-karpenter";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultKarpenter";
      invariants = [
        "boundsOK"
        "idleReapSafety"
        "iceMarkSoundness"
        "bootSampleNotLost"
        "noMassClearAfterFailover"
        "reloadLatchRespected"
        "singleEffectiveProvisioner"
        "gateProducerGuarantee"
        "provisioningBudget"
        "coverRespectsMask"
        "degradedCoverPolarity"
      ];
    };

    # ---- Model N: pre-registered as-built falsifications -------------
    # The ⊥-tick early-return observation skip (the documented TODO in
    # reconcile_once's ⊥ arm; entry 1 of the invariant map's
    # expected-as-built-falsifications list). Each check passes only
    # while the checker still falsifies the invariant on the as-built
    # encoding — when the skip is fixed, these flip to HOLD invariants
    # in the fault-rpc regime check above (the same flip protocol the
    # retry campaign used). The deterministic reproducer traces are
    # pinned by the named-run check below.

    # prev_idle is not pruned across an unobserved busy period: a stale
    # entry conflates two idle spells and reaps a freshly-idle claim.
    quint-nodeclaim-falsification-idle-conflation = mkQuintWitnessCheck {
      name = "nodeclaim-falsification-idle-conflation";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "idleReapSafety";
    };
    # A Registered edge inside the early-return window ages past the
    # recency gate before any observation runs: the boot sample and its
    # ICE clear are lost although the edge happened under this tenure.
    quint-nodeclaim-falsification-boot-sample-lost = mkQuintWitnessCheck {
      name = "nodeclaim-falsification-boot-sample-lost";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "bootSampleNotLost";
    };
    # The deterministic reproducer runs for both falsifications
    # (idleConflationRun, bootSampleLostRun) — the precise documented
    # traces stay pinned even though TLC's BFS may report a different
    # counterexample first.
    quint-nodeclaim-runs-fault-rpc = mkQuintRunCheck {
      name = "nodeclaim-runs-fault-rpc";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
    };

    # ---- Model N: non-vacuity witnesses ------------------------------

    # An idle claim is actually reaped (past threshold, unreserved).
    quint-nodeclaim-witness-idle-reap = mkQuintWitnessCheck {
      name = "nodeclaim-witness-idle-reap";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleBase";
      witness = "canReachIdleReap";
    };
    # A deficit cover actually creates a NodeClaim.
    quint-nodeclaim-witness-create = mkQuintWitnessCheck {
      name = "nodeclaim-witness-create";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleBase";
      witness = "canReachCreate";
    };
    # The per-class budget binds while the global budget still has room.
    quint-nodeclaim-witness-class-budget = mkQuintWitnessCheck {
      name = "nodeclaim-witness-class-budget";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleBase";
      witness = "canReachClassBudgetBound";
    };
    # A claim vanishes between ticks (Karpenter fast-GC) and is detected
    # as an ICE mark.
    quint-nodeclaim-witness-vanish = mkQuintWitnessCheck {
      name = "nodeclaim-witness-vanish";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultKarpenter";
      witness = "canReachVanishMark";
    };
    # An ICE mark is emitted and a recency-gated Registered clear for
    # the same (still-masked) cell is emitted on a later tick.
    quint-nodeclaim-witness-clear-after-mark = mkQuintWitnessCheck {
      name = "nodeclaim-witness-clear-after-mark";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultKarpenter";
      witness = "canReachClearAfterMark";
    };
    # The ⊥-streak reaches the threshold and consolidate-only reaps
    # something.
    quint-nodeclaim-witness-consolidate-only-reap = mkQuintWitnessCheck {
      name = "nodeclaim-witness-consolidate-only-reap";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachConsolidateOnlyReap";
    };
    # A NodeClaim create fails (and consumes no budget).
    quint-nodeclaim-witness-create-failure = mkQuintWitnessCheck {
      name = "nodeclaim-witness-create-failure";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachCreateFailure";
    };
    # A tick with unplaced demand and unknown ceilings performs no
    # creates (the fail-closed half)…
    quint-nodeclaim-witness-ceilings-fail-closed = mkQuintWitnessCheck {
      name = "nodeclaim-witness-ceilings-fail-closed";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachCeilingsFailClosed";
    };
    # …and creates resume once the ceilings are present (the resume
    # half, in the same regime whose init starts without them).
    quint-nodeclaim-witness-create-resumes = mkQuintWitnessCheck {
      name = "nodeclaim-witness-create-resumes";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachCreate";
    };
    # An intent is dropped solely because its cell is unconfigured.
    quint-nodeclaim-witness-unknown-cell = mkQuintWitnessCheck {
      name = "nodeclaim-witness-unknown-cell";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultRpc";
      witness = "canReachUnknownCellDrop";
    };
    # A lease handoff occurs with a non-empty inflight_created.
    quint-nodeclaim-witness-handoff-inflight = mkQuintWitnessCheck {
      name = "nodeclaim-witness-handoff-inflight";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      witness = "canReachAcquireWithInflight";
    };
    # The acquire-with-failed-reload path runs a degraded tick (persist
    # gated, latch still set).
    quint-nodeclaim-witness-degraded-reload = mkQuintWitnessCheck {
      name = "nodeclaim-witness-degraded-reload";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      witness = "canReachDegradedReloadTick";
    };
    # After an acquire, a fresh registration edge still emits a
    # Registered clear…
    quint-nodeclaim-witness-fresh-clear-after-acquire = mkQuintWitnessCheck {
      name = "nodeclaim-witness-fresh-clear-after-acquire";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      witness = "canReachFreshClearAfterAcquire";
    };
    # …while an old registration is recorded without one (the mass-clear
    # the recency gate prevents).
    quint-nodeclaim-witness-stale-record-only = mkQuintWitnessCheck {
      name = "nodeclaim-witness-stale-record-only";
      spec = "nodeclaimLifecycle";
      main = "nodeclaimLifecycleFaultLease";
      witness = "canReachStaleRecordOnly";
    };

    # ---- Controller Stage-C calibration witnesses --------------------
    # The controller-formal historical-fix corpus replayed against the
    # two reconcile models (controller-invariant-map.md, the Stage-C
    # calibration table). Each check instantiates the as-built model,
    # swaps ONE tick action for its PRE-FIX behavior (the calibration
    # module's `calibStep`) and passes only while the checker still
    # falsifies the invariant the corresponding historical fix protects
    # — machine-checked evidence that the model would re-find that bug
    # class if it were reintroduced, and that the invariant is not
    # vacuous for it. One representative per falsifying family with a
    # plausible regression path; the remaining override modules under
    # docs/spec/models/calibration/ are evidence modules (not wired).
    # Deliberately no tracey markers: the spec rules are verified by the
    # HOLD regime checks above, not by these pre-fix reproductions.

    # G-A (fba9086dc): losing the live pod-phase re-check before the
    # excess-pending DELETE reaps a Job whose pod is already running.
    quint-ctrl-calib-ga-live-recheck = mkQuintWitnessCheck {
      name = "ctrl-calib-ga-live-recheck";
      spec = "calibration/controller-ga";
      main = "gaCalibNoLiveRecheck";
      extraSpecs = [ "spawnCoherence" ];
      step = "calibStep";
      witness = "reapSafety";
    };

    # G-B (cdc78f839): acking the attempted spawn slice instead of the
    # successfully-created set arms dispatched_cells with no Job behind
    # it.
    quint-ctrl-calib-gb-ack-spawned-only = mkQuintWitnessCheck {
      name = "ctrl-calib-gb-ack-spawned-only";
      spec = "calibration/controller-gb";
      main = "gbCalibAckAttempted";
      extraSpecs = [ "spawnCoherence" ];
      step = "calibStep";
      witness = "ackSoundness";
    };

    # M1 (79f86b888): clearing prev_idle only on the reload Ok arm lets
    # a stale entry survive a failed-reload acquire and over-reap a
    # freshly idle claim (the amplify-polarity row of the lease-edge
    # table).
    quint-ctrl-calib-m1-acquire-clear = mkQuintWitnessCheck {
      name = "ctrl-calib-m1-acquire-clear";
      spec = "calibration/controller-m1";
      main = "m1CalibAcquireClearOkOnly";
      extraSpecs = [ "nodeclaimLifecycle" ];
      step = "calibStep";
      witness = "idleReapSafety";
    };

    # M2 (08d49c52c): dropping the consolidate-only inflight prune makes
    # the controller's own reap read as a Karpenter vanish on the next
    # full tick — a spurious ICE mark.
    quint-ctrl-calib-m2-consolidate-prune = mkQuintWitnessCheck {
      name = "ctrl-calib-m2-consolidate-prune";
      spec = "calibration/controller-m2";
      main = "m2CalibNoConsolidatePrune";
      extraSpecs = [ "nodeclaimLifecycle" ];
      step = "calibStep";
      witness = "iceMarkSoundness";
    };

    # M3/M4 (703cbf42a): clearing the reload latch on the load attempt
    # (not on Ok) lets the same tick persist the stale standby snapshot
    # over PG.
    quint-ctrl-calib-m34-reload-latch = mkQuintWitnessCheck {
      name = "ctrl-calib-m34-reload-latch";
      spec = "calibration/controller-m34";
      main = "m34CalibLatchClearOnAttempt";
      extraSpecs = [ "nodeclaimLifecycle" ];
      step = "calibStep";
      witness = "reloadLatchRespected";
    };

    # FFD/cover (family-level): sizing cover against the global budget
    # only lets one cell exceed its per-class fleet cap.
    quint-ctrl-calib-ffd-class-clamp = mkQuintWitnessCheck {
      name = "ctrl-calib-ffd-class-clamp";
      spec = "calibration/controller-ffd";
      main = "ffdCalibNoClassClamp";
      extraSpecs = [ "nodeclaimLifecycle" ];
      step = "calibStep";
      witness = "provisioningBudget";
    };

    # ------------------------------------------------------------------
    # Executor-lifecycle campaign (#1), Phase 0 Stage B: the AS-BUILT
    # scheduler⇄executor session protocol.
    #
    # Model S (executorSession.qnt) — RE-TARGETED at 1c' to the pull
    # protocol as landed (T-1c'.5): the PullAssignment admission and
    # fenced mint, the ReportOutcome intake (fold_report + the AD5
    # abort arm), the controller's idempotent ReportAttemptOutcome
    # (second installment / synthesized verdict / no-attempt no-op),
    # the establishment sweep over the durable open-attempt view
    # (store-probe adopt arm + the C2 charge arm), the claims-floor
    # generation fence, and the pod's own bounded lifecycle around
    # them (the OA6(a) NotYetReady wait state, the charge-free idle
    # exit, death, replacement). One intent, bucketed time;
    # budgets/charges are imported from retryPolicy.qnt's pull-mode
    # regime, the spawn/Job lifecycle from spawnCoherence.qnt, the
    # lease from leaderElection.qnt (see the model header's
    # assume-guarantee checklist).
    #
    # The Stage-B AS-BUILT encoding of the stream session machinery was
    # frozen, unwired, as executorSessionAsBuilt.qnt (P14) and retired
    # on 2026-05-29 — the file is deleted, git history is the archive
    # (the invariant map's retirement record). Its retired checks and
    # the disposition of every retired witness/calibration check are
    # recorded in the invariant map's Phase-1c' re-target record.
    #
    # Model D (executorDelivery.qnt) was retired with the 1d builder
    # collapse (T-1d.4): the stream-era delivery choreography it modeled
    # (permanent sink, relay swap, half-close flush, drain gate, idle
    # exit, generation watermark) no longer exists. Its guarantees'
    # carriers are recorded in the invariant map's Model-D retirement
    # record: the pull report-retry loop and exit-code unit batteries
    # (builder.completion.exactly-once-or-death+2 verify markers), the
    # scheduler-side report idempotency tests, the chaos VM suite, and
    # — Phase 2 — the fold_report Kani contract. The model file and its
    # two f2d calibration override modules are deleted (git history
    # holds them); the f2d calibration family's acceptance verdict is
    # pre-staged in the retirement record.
    #
    # The invariant ↔ rule map, the Stage-B record for the frozen
    # as-built model, and the Phase-1c' re-target record (verdicts,
    # bounds, retirements) are in
    # docs/spec/models/executor-invariant-map.md.

    # The base regime: the full pull lifecycle with no leader faults —
    # forecast spawn before Ready, the bounded NotYetReady retry loop
    # and its charge-free idle exit (OA6(a)), delivery and idempotent
    # re-pull, the worker report (success / failure / abort), pod death
    # with and without uploaded outputs, the controller's no-attempt
    # no-op / second installment / synthesized verdict, and both
    # establishment arms. Verifies the re-targeted invariant set:
    # at-most-one-open-attempt, NotYetReady inertness, the re-based
    # repair-armed property, no fabricated completion, the
    # establishment-window discipline and the per-attempt
    # classification dedup.
    # r[verify sched.executor.one-shot+2]
    # r[verify sched.executor.pull-not-ready+2]
    # r[verify sched.attempt.establishment-window+3]
    quint-executor-session-base = mkQuintCheck {
      name = "executor-session-base";
      spec = "executorSession";
      main = "executorSessionBase";
      invariants = [ "allInvariants" ];
    };

    # The fault-leader regime: one leader failover (claim-before-serve
    # floor ratchet) plus below-floor write attempts by a deposed
    # believer. The durable open-attempt state must survive the
    # failover untouched, the post-failover authority keeps serving
    # pulls off the durable rows, and every stale-authority transaction
    # is fenced to a no-op — StaleAuthorityWritesAreInert, the checked
    # successor of the dual-belief residual the as-built model priced
    # (T-0e.2 / the 1c' lease-checklist re-derivation).
    # r[verify sched.lease.generation-fence+3]
    # r[verify sched.lease.claim-before-advertise+2]
    quint-executor-session-fault-leader = mkQuintCheck {
      name = "executor-session-fault-leader";
      spec = "executorSession";
      main = "executorSessionFaultLeader";
      invariants = [ "allInvariants" ];
    };

    # Non-vacuity witnesses for the re-targeted Model S. Each check
    # passes only when the contended state is still reachable in the
    # named regime. The first two are the OA6(a) reachability pair the
    # 1c' plan names (the warm-start delivery after NotYetReady, and
    # the never-Ready charge-free idle exit); the rest pin every
    # closer/repair path of the open-attempt row plus the orphaned
    # open attempt and the report-idempotency arms. The retired
    # as-built witnesses (phantom, drain-pending, half-dead,
    # stale-epoch, rollback, adopt, failover-inflight,
    # deposed-believer, reap-after-stall, two-channel-death,
    # establishment, race-ahead) are recorded with their dispositions
    # in the invariant map's Phase-1c' re-target record.
    quint-executor-session-witness-warm-start = mkQuintWitnessCheck {
      name = "executor-session-witness-warm-start";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachWarmStartDeliver";
    };
    quint-executor-session-witness-idle-exit = mkQuintWitnessCheck {
      name = "executor-session-witness-idle-exit";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachNeverReadyIdleExit";
    };
    quint-executor-session-witness-establishment = mkQuintWitnessCheck {
      name = "executor-session-witness-establishment";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachEstablishmentCharge";
    };
    quint-executor-session-witness-store-adopt = mkQuintWitnessCheck {
      name = "executor-session-witness-store-adopt";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachStoreProbeAdopt";
    };
    quint-executor-session-witness-synthesized-close = mkQuintWitnessCheck {
      name = "executor-session-witness-synthesized-close";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachSynthesizedClose";
    };
    quint-executor-session-witness-second-installment = mkQuintWitnessCheck {
      name = "executor-session-witness-second-installment";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachSecondInstallment";
    };
    quint-executor-session-witness-repull = mkQuintWitnessCheck {
      name = "executor-session-witness-repull";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachIdempotentRepull";
    };
    quint-executor-session-witness-orphaned-attempt = mkQuintWitnessCheck {
      name = "executor-session-witness-orphaned-attempt";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachOrphanedOpenAttempt";
    };
    quint-executor-session-witness-late-report = mkQuintWitnessCheck {
      name = "executor-session-witness-late-report";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachLateReportIgnored";
    };
    quint-executor-session-witness-no-attempt-noop = mkQuintWitnessCheck {
      name = "executor-session-witness-no-attempt-noop";
      spec = "executorSession";
      main = "executorSessionBase";
      witness = "canReachNoAttemptNoop";
    };
    # The two fault-leader probes: a below-floor authority actually
    # attempts a write and is fenced (StaleAuthorityWritesAreInert is
    # not vacuous), and the post-failover authority serves a pull off
    # the durable rows.
    quint-executor-session-witness-stale-fenced = mkQuintWitnessCheck {
      name = "executor-session-witness-stale-fenced";
      spec = "executorSession";
      main = "executorSessionFaultLeader";
      witness = "canReachStaleFenced";
    };
    quint-executor-session-witness-post-failover-deliver = mkQuintWitnessCheck {
      name = "executor-session-witness-post-failover-deliver";
      spec = "executorSession";
      main = "executorSessionFaultLeader";
      witness = "canReachPostFailoverDeliver";
    };

    # ---- Executor-lifecycle Stage-C calibration witnesses -------------
    # The executor-lifecycle (campaign #1) historical-fix corpus replayed
    # against Models S and D (executor-invariant-map.md, the Stage-C
    # calibration section). Each check instantiates a model, swaps ONE
    # action for its PRE-FIX behavior (the calibration module's
    # `calibStep`) and passes only while the checker still falsifies the
    # invariant the corresponding historical fix protects — machine-
    # checked evidence that the model would re-find that bug class if it
    # were reintroduced, and that the invariant is not vacuous for it.
    #
    # 1c' flip (T-1c'.5): the wired Model-S calibration checks tracked
    # the AS-BUILT stream machinery, which the deletion commits removed
    # — the states they pinned are unconstructible by design on the
    # pull path, so they are retired with records (the per-family
    # "cannot recur by construction" verdicts are pre-staged in the
    # invariant map's Phase-1c' re-target record):
    #   - quint-executor-calib-f1-stale-epoch (stream-epoch attribution
    #     — no streams, no epochs; per-unary identity + the generation
    #     fence are the successors),
    #   - quint-executor-calib-f2-phantom-drain (phantom two-strike —
    #     no push channel, so a phantom binding cannot form),
    #   - quint-executor-calib-f3-stall-credit (worker-time reaper
    #     stall credit — no scheduler-side liveness reaper exists; the
    #     Job lifecycle and the establishment window own liveness),
    #   - quint-executor-calib-f5-closed-stream (closed-stream dispatch
    #     exclusion — no scheduler-side placement decision exists).
    # Their override modules stayed under calibration/ as evidence over
    # the frozen executorSessionAsBuilt.qnt until the 2026-05-29
    # as-built retirement deleted both (git history is the archive; the
    # invariant map's retirement record has the dispositions). F4
    # re-encodes against the re-targeted model below;
    # F2d (Model D, builder half) retired with Model D at the 1d builder
    # collapse — the machinery it pinned (completion_pending arming, the
    # half-close flush) no longer exists, so the state is
    # unconstructible by design; verdict pre-staged in the invariant
    # map's retirement record. Deliberately no tracey markers (the spec
    # rules are verified by the HOLD regime checks above, not by these
    # pre-fix reproductions).

    # F4 (death attribution / the I-197 double-charge precondition),
    # re-encoded against the re-targeted model: losing the
    # establishment-window gate lets the establishment sweep charge an
    # open attempt while its classifying report still has its window —
    # the pull-path analog of the correlation-entry/last_completed
    # discriminator the as-built representative pinned.
    quint-executor-calib-f4-establishment-window = mkQuintWitnessCheck {
      name = "executor-calib-f4-establishment-window";
      spec = "calibration/executor-f4-pull-establish-early";
      main = "executorCalibF4PullEstablishEarly";
      extraSpecs = [ "executorSession" ];
      step = "calibStep";
      witness = "establishmentOnlyAfterWindowCloses";
    };

    # ------------------------------------------------------------------
    # Executor-lifecycle campaign: the retryPolicy PULL-MODE environment
    # regime (the T-0e.3 re-derivation, wired at 1b as an additional
    # regime; the wired set since the 1c' retirement of the
    # as-built-channel regimes, T-1c'.6). Same fold, same invariants as
    # the retired channel regimes — what changes is the event-arrival
    # environment: the attempt opens at PullAssignment, the worker
    # classes arrive over the ReportOutcome unary, the controller's
    # pod-terminal classification is the idempotent ReportAttemptOutcome
    # row fill (with the no-attempt no-op), the establishment sweep is
    # the only time-based repair, and the exclusion/fleet-exhaust inputs
    # are re-keyed to source nodes with the AD2 small-fleet clause (the
    # spawn-intent gate's NoEligibleSource arm).
    #
    # The regime module is retryPolicyPull in retryPolicy.qnt; its
    # transition relation is `pullStep`, selected via --step.
    # ------------------------------------------------------------------

    # The exhaustive pull-mode regime check: two source nodes, the full
    # pull alphabet (pull-open, worker-report classes, pod deaths with
    # the OOM/Deadline controller fills, the no-attempt no-op, the
    # establishment, the spawn-gate exhaust, source-universe shrink, the
    # resets) plus one leader failover with the open-attempt carve-out
    # (the pull attempt and the durable budgets survive; nothing is
    # forgiven or fabricated). The HOLD list is the same invariant set
    # the as-built regimes proved — the refinement tripwires, the
    # charge-once/no-double-count discipline, the poison/clear
    # lifecycle, and the failover-budget acceptance property — now over
    # the pull-path event-arrival environment. Since the 1c' retirement
    # this check also carries the model-checked markers the retired
    # regime checks held (the rules' unit-test and kani markers are
    # unchanged).
    # r[verify sched.retry.per-executor-budget+4]
    # r[verify sched.dispatch.fleet-exhaust+4]
    # r[verify sched.retry.counters-refine-history+2]
    # r[verify sched.retry.no-double-count]
    # r[verify sched.retry.verdict-channel-invariant]
    # r[verify sched.poison.cascade-dependents]
    # r[verify sched.retry.failover-budget]
    # r[verify sched.retry.recovery-projection+3]
    quint-retry-policy-pull = mkQuintCheck {
      name = "retry-policy-pull";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      invariants = [
        "boundsOK"
        "attemptsChargedOnce"
        "countersRefineHistory"
        "verdictMatchesFold"
        "durableMirrorsCharges"
        "noDoubleCount"
        "poisonIsTerminalUntilCleared"
        "cascadeReachesExactlyTheDependents"
        "failoverPreservesHistory"
        "recoveryNeverFabricatesFailures"
        "placementSound"
        "clearedPoisonClearsDurably"
        "clearedPoisonScrubsExclusions"
        "recoveryPreservesPoisonStatus"
      ];
    };

    # Non-vacuity witnesses for the pull-mode regime (same
    # expect-violation discipline as the other witness checks; no tracey
    # markers on witnesses). The four scenarios the 1b gate names plus
    # the controller-fill charge: a pull opens an attempt; a no-report
    # crash is established and charged; a pod-terminal report that finds
    # no open attempt is dropped charge-free (the
    # sched.attempt.no-attempt-no-op arm); the exhausted source universe
    # poisons FleetExhausted (the AD2 small-fleet clause / spawn-gate
    # arm); and the ReportAttemptOutcome fill actually charges (the
    # C4/C5-unified pod-terminal second installment feeds the fold).
    quint-retry-policy-pull-witness-opens-attempt = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-opens-attempt";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noPullOpensAttempt";
    };
    quint-retry-policy-pull-witness-establishment = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-establishment";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noEstablishedCrashCharge";
    };
    quint-retry-policy-pull-witness-no-attempt-noop = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-no-attempt-noop";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noNoAttemptReportNoop";
    };
    quint-retry-policy-pull-witness-fleet-exhaust = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-fleet-exhaust";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noFleetExhaustPoison";
    };
    quint-retry-policy-pull-witness-fill-charge = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-fill-charge";
      spec = "retryPolicy";
      main = "retryPolicyPull";
      step = "pullStep";
      witness = "noAtCapTermination";
    };

    # ------------------------------------------------------------------
    # Substitution-replacement Phase A (T-5.1/T-5.2): the materialization
    # attempt class — the kind partition the campaign adds to the attempt
    # ledger (design §2.5/§9.2, OQ1 amendments 1-2). Two invariants:
    # materializationNeverPoisons (no materialization charge sequence —
    # including establishment-written crash charges — ever produces a
    # Poison/Cancel verdict or touches the cascade) and
    # materializationInvisibleToBuildBudgets (materialization charges feed
    # exactly one budget, their own; every build-side budget view is
    # untouched by them). Both are encoded in the pre-state-snapshot
    # tripwire style over bounded counters (the model's section-11
    # encoding note explains why no product-state predicate can do it);
    # both calibrated by working-tree falsification before wiring (the
    # introducing commit records the transcripts).
    #
    # Why a separate regime instead of extending quint-retry-policy-pull's
    # own alphabet: the materialization counters are independent state, so
    # enabling them in a regime multiplies its reachable state space by
    # the number of reachable counter combinations — a full product; the
    # partition is the point. The build-only pull regime is the model's
    # largest check, and any product factor >= 2 on it breaks the
    # merge-gate wall-clock threshold (the Phase-A stop-condition-8
    # 2x-baseline/5-min rule). The materialization-coexistence regime
    # (retryPolicyPullMat) therefore carries the product over the build
    # channels the partition is about — worker-report charges, the
    # no-report crash and its build-side establishment (the OQ1
    # adjacency: BOTH establishment channels are reachable, and the
    # invariants pin that the materialization one never feeds the build
    # fold), dispatch, the spawn-gate exhaust and the source-universe
    # shrink. The controller-fill machinery, the resets and leader
    # failover stay the build-only regime's concern: they explore
    # build-internal lifecycles the materialization class is structurally
    # independent of (the partition invariants force exactly that), so
    # their exclusion does not weaken what this check proves about the
    # partition.
    #
    # The dormant regime (retryPolicyPull, ENABLE_MATERIALIZATION = false)
    # keeps a bit-identical reachable state space — the wired-check
    # invariance half of Phase A's dormancy criterion 5, re-proven by
    # quint-retry-policy-pull reporting the same distinct-state count and
    # depth as its pre-extension baseline.
    #
    # The HOLD list is the build-only regime's full list PLUS the two
    # partition invariants: the pre-existing invariants are re-proven
    # over materialization interleavings (a materialization action
    # between any two build events must not perturb any of them).
    # r[verify sched.materialize.routing+2]
    quint-retry-policy-pull-materialization = mkQuintCheck {
      name = "retry-policy-pull-materialization";
      spec = "retryPolicy";
      main = "retryPolicyPullMat";
      step = "pullStep";
      invariants = [
        "boundsOK"
        "attemptsChargedOnce"
        "countersRefineHistory"
        "verdictMatchesFold"
        "durableMirrorsCharges"
        "noDoubleCount"
        "poisonIsTerminalUntilCleared"
        "cascadeReachesExactlyTheDependents"
        "failoverPreservesHistory"
        "recoveryNeverFabricatesFailures"
        "placementSound"
        "clearedPoisonClearsDurably"
        "clearedPoisonScrubsExclusions"
        "recoveryPreservesPoisonStatus"
        "materializationNeverPoisons"
        "materializationInvisibleToBuildBudgets"
      ];
    };

    # Non-vacuity witness for the materialization-coexistence regime: the
    # establishment crash channel (OQ1 amendment 1) is reachable and
    # charges the materialization budget — the contended channel both
    # partition invariants protect against (a crashed store replica's
    # establishment is THE site that could plausibly write executor_crash
    # into the build ledger; this witness pins that the model exercises
    # it, so the invariants are never vacuously green).
    quint-retry-policy-pull-witness-materialization-crash = mkQuintWitnessCheck {
      name = "retry-policy-pull-witness-materialization-crash";
      spec = "retryPolicy";
      main = "retryPolicyPullMat";
      step = "pullStep";
      witness = "noMaterializationCrashCharge";
    };

    # ------------------------------------------------------------------
    # Substitution-replacement Phase C-prime: the materialization-job
    # lifecycle model (docs/spec/models/materializationJob.qnt),
    # re-targeted to the AS-BUILT Phase B system (the six handoff
    # deltas: the finding-11 mark discriminator + clear-mirror, the
    # executor channel redial, the wanted backfill, the park
    # re-evaluation split, the five-origin set, the recovery view
    # rebuild, the must_substitute claim upgrade). The §9.1 successor
    # property table (21 invariants incl. the three at-decision-time
    # re-encodes and the delta-derived invariants), the §9.3
    # calibration-transfer verdict table, and the exhaustive-budget
    # measurement record live in
    # docs/spec/models/substitution-replacement-invariant-map.md
    # (C-prime stage record).
    #
    # Check-set shape (the closure-evidence Phase-1 precedent): the
    # exhaustive TLC conjunctions do not converge inside a
    # gate-compatible budget at any modeled scope (the C-prime
    # measurement table; the reduced -Ex scopes are documented manual
    # targets with zero-violation bounded coverage), so the GHA-wired
    # deliverable per regime is the bounded-simulation HOLDS check —
    # each named pin below is its falsifiability pair — plus the
    # witness set (every §9.1 property's contended scenario stays
    # demonstrably reachable) and the §9.3 calibration pins (TLC,
    # first-violation — the permanent expect-violation record that the
    # model re-finds every transferred bug class).
    # ------------------------------------------------------------------

    # Deterministic named-run pins (the delta scenario narratives:
    # happy path, the park split, the four-conjunct fail-fast, the
    # unmarked arm-3 from-source, the marked-claim upgrade, the legacy
    # backfill, the reprobe reset; stale-reset and the failover
    # redial/view-rebuild run under the regimes that enable them).
    quint-materialization-runs-base = mkQuintRunCheck {
      name = "materialization-runs-base";
      spec = "materializationJob";
      main = "materializationJobBase";
      match = "happyPathRun|parkSplitRun|prunedFailFastRun|unmarkedArm3FromSourceRun|markedClaimUpgradeRun|legacyBackfillRun|reprobeResetRun";
    };
    quint-materialization-runs-failover = mkQuintRunCheck {
      name = "materialization-runs-failover";
      spec = "materializationJob";
      main = "materializationJobFailover";
      match = "failoverRedialRun|failoverViewRebuildRun";
    };
    quint-materialization-runs-adversarial = mkQuintRunCheck {
      name = "materialization-runs-adversarial";
      spec = "materializationJob";
      main = "materializationJobAdversarialStore";
      match = "staleResetRun";
    };

    # Bounded-simulation HOLDS checks, one per design-scale regime —
    # the full 21-invariant §9.1 conjunction. Bounded evidence, not
    # proof: the exhaustive Ex-scope conjunctions are the documented
    # manual targets (commands + measured budgets in the C-prime stage
    # record). Sample sizing: the paired calibration pins' violation
    # classes are found by the simulator in well under 100 K samples at
    # the smaller Ex scopes; 2 M samples at design scale clears the
    # 25/p flake floor with two orders of headroom.
    #
    # Paired falsifiability pins (the constructor's vacuity rule —
    # every invariant in the list has at least one pin or witness
    # below): quint-materialization-calib-* (19 expect-violation pins)
    # + the marked-claim / post-failover-claim witnesses (the B1/B3
    # liveness flips).
    # r[verify sched.materialize.job]
    # r[verify sched.materialize.routing+2]
    quint-materialization-holds-base = mkQuintSimHoldsCheck {
      name = "materialization-holds-base";
      spec = "materializationJob";
      main = "materializationJobBase";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # r[verify store.materialize.executor+2]
    quint-materialization-holds-failover = mkQuintSimHoldsCheck {
      name = "materialization-holds-failover";
      spec = "materializationJob";
      main = "materializationJobFailover";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # r[verify sched.materialize.pinning]
    quint-materialization-holds-adversarial-store = mkQuintSimHoldsCheck {
      name = "materialization-holds-adversarial-store";
      spec = "materializationJob";
      main = "materializationJobAdversarialStore";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    # The claims-floor fence's model-level verification post-D-prime:
    # fencedJobWritesOnly (in the conjunction) is the living oracle for
    # the uniform fence over job-table and wanted-relation writes — the
    # column-agnostic core of the durability rule (the evidence-column
    # subject was deleted; migration 080). Falsifiability pair:
    # quint-materialization-calib-f11-unfenced-resolve.
    # r[verify sched.evidence.durability+2]
    quint-materialization-holds-stale-tenure = mkQuintSimHoldsCheck {
      name = "materialization-holds-stale-tenure";
      spec = "materializationJob";
      main = "materializationJobStaleTenure";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };
    quint-materialization-holds-crash-loop = mkQuintSimHoldsCheck {
      name = "materialization-holds-crash-loop";
      spec = "materializationJob";
      main = "materializationJobCrashLoop";
      invariants = matJobInvariants;
      maxSamples = 2000000;
      maxSteps = 15;
    };

    # Non-vacuity witnesses (rust simulator; expect-violation). Every
    # §9.1 property's contended scenario + every delta's new behavior
    # stays demonstrably reachable. No tracey markers on witnesses.
    # Sample sizing: every witness here was first found in under 300 K
    # samples (most in the first batches); 2 M is the flake floor.

    # The happy path resolves a job successfully.
    quint-materialization-witness-success = mkQuintSimWitnessCheck {
      name = "materialization-witness-success";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noSuccessResolution";
    };
    # The four-conjunct fail-fast fires (delta 1: needs the
    # pruned-origin mark — keeps the arm-3 corner non-vacuous).
    quint-materialization-witness-fail-fast = mkQuintSimWitnessCheck {
      name = "materialization-witness-fail-fast";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noFailFast";
    };
    # The unmarked arm-3 from-source disposition (delta 1, the B2 fix
    # row): if this stops violating, the 6-row table's release arm has
    # gone unreachable and the routing checks are vacuous there.
    quint-materialization-witness-unmarked-arm3 = mkQuintSimWitnessCheck {
      name = "materialization-witness-unmarked-arm3";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noUnmarkedArm3FromSource";
    };
    # A MARKED node's job is claimed (delta 6 — the B1 admission-gap
    # regression guard: pre-B1 the must_substitute refusal made this
    # unreachable; the mat-b1-claim-refuses-marked calibration module
    # is the recorded pre-fix flip).
    quint-materialization-witness-marked-claim = mkQuintSimWitnessCheck {
      name = "materialization-witness-marked-claim";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noMarkedClaim";
    };
    # A claim lands AFTER a failover staled every executor channel
    # (delta 2a — the B3 redial liveness guard: without redialChannel
    # this is unreachable; the mat-b3-no-redial calibration module is
    # the recorded pre-fix flip).
    quint-materialization-witness-post-failover-claim = mkQuintSimWitnessCheck {
      name = "materialization-witness-post-failover-claim";
      spec = "materializationJob";
      main = "materializationJobFailover";
      witness = "noPostFailoverClaim";
    };
    # The budget park fires (delta 3: rides the InfraFailure
    # consumption).
    quint-materialization-witness-park = mkQuintSimWitnessCheck {
      name = "materialization-witness-park";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noPark";
    };
    # A Broken-evidence park (the MD-D1 stalled-gauge population) is
    # reachable (delta 3).
    quint-materialization-witness-stalled-park = mkQuintSimWitnessCheck {
      name = "materialization-witness-stalled-park";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noStalledBrokenPark";
    };
    # The park re-evaluation resolves a Vouched/Pending park from
    # source (delta 3, the PD-20 arm).
    quint-materialization-witness-park-reeval = mkQuintSimWitnessCheck {
      name = "materialization-witness-park-reeval";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noParkReevalResolve";
    };
    # A legacy (flag-off-era) build's relation is backfilled at
    # creation (delta 2b, the B4 fix's contended state).
    quint-materialization-witness-legacy-backfill = mkQuintSimWitnessCheck {
      name = "materialization-witness-legacy-backfill";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noLegacyBackfill";
    };
    # A stale_reset job resets a Completed node (delta 4): runs in the
    # adversarial-store regime (the GC'd-outputs shape needs GC).
    quint-materialization-witness-stale-reset = mkQuintSimWitnessCheck {
      name = "materialization-witness-stale-reset";
      spec = "materializationJob";
      main = "materializationJobAdversarialStore";
      witness = "noStaleResetCreation";
    };
    # A reprobe-origin job (the AS-5 reset lane, delta 4).
    quint-materialization-witness-reprobe = mkQuintSimWitnessCheck {
      name = "materialization-witness-reprobe";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noReprobeCreation";
    };
    # The §5.3 pin release fires (job resolved + all interest
    # terminal) — the pinning rule's release half stays reachable.
    quint-materialization-witness-pin-release = mkQuintSimWitnessCheck {
      name = "materialization-witness-pin-release";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noPinRelease";
    };
    # A crashed materialization attempt is established (the PP-4
    # channel both partition invariants constrain).
    quint-materialization-witness-crash-establishment = mkQuintSimWitnessCheck {
      name = "materialization-witness-crash-establishment";
      spec = "materializationJob";
      main = "materializationJobCrashLoop";
      witness = "noCrashEstablishment";
    };
    # A build-kind attempt opens (kindMatchesWorker /
    # noFromSourceWhileJobUnresolved non-vacuity).
    quint-materialization-witness-build-attempt = mkQuintSimWitnessCheck {
      name = "materialization-witness-build-attempt";
      spec = "materializationJob";
      main = "materializationJobBase";
      witness = "noBuildAttempt";
    };

    # ---- Materialization C-prime calibration pins ---------------------
    # The §9.3 transfer corpus, executed (the go/no-go core): each pin
    # instantiates the re-targeted model at a reduced Ex scope, swaps
    # ONE action for its PRE-FIX behavior (the override's calibStep)
    # and passes only while the checker still falsifies the §9.1
    # property the as-built mechanism protects. Verdict table (every
    # transferred family dispositioned, incl. the by-construction rows
    # and the two liveness witness-flips): the C-prime stage record.
    # TLC backend, first-violation (13-27 s each at the qualifying
    # measurement). No tracey markers on calibration checks.

    # F8 + F13 (CE-33/CE-58 — THE anchor): the evidence-blind builder
    # pull delivers from source while the job is unresolved.
    quint-materialization-calib-f8-pull-ignores-job = mkQuintWitnessCheck {
      name = "materialization-calib-f8-pull-ignores-job";
      spec = "calibration/mat-f8-pull-ignores-job";
      main = "matCalibF8PullIgnoresJob";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "noFromSourceWhileJobUnresolved";
    };
    # F10 L1-half (CE-48(i)): the pre-T-4.3 failover drops the view
    # without the rebuild — the pending job's armed action strands.
    quint-materialization-calib-f10-view-drop = mkQuintWitnessCheck {
      name = "materialization-calib-f10-view-drop";
      spec = "calibration/mat-f10-failover-view-drop";
      main = "matCalibF10FailoverViewDrop";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "unresolvedJobAlwaysArmed";
    };
    # B5(a): the dedup re-feed overwrites armament state (park/holder).
    quint-materialization-calib-b5a-refeed-overwrite = mkQuintWitnessCheck {
      name = "materialization-calib-b5a-refeed-overwrite";
      spec = "calibration/mat-b5a-refeed-overwrite";
      main = "matCalibB5aRefeedOverwrite";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "unresolvedJobAlwaysArmed";
    };
    # CE-17 / F6 / F2(a): success consumption without the coverage
    # re-check (the wanted-growth race).
    quint-materialization-calib-ce17-skip-coverage = mkQuintWitnessCheck {
      name = "materialization-calib-ce17-skip-coverage";
      spec = "calibration/mat-ce17-skip-coverage";
      main = "matCalibCe17SkipCoverage";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "successConsumptionCoversLiveWanted";
    };
    # F2(b) / BC-3: the establishment adopts on output presence.
    quint-materialization-calib-f2b-establish-adopt = mkQuintWitnessCheck {
      name = "materialization-calib-f2b-establish-adopt";
      spec = "calibration/mat-f2b-establish-adopt";
      main = "matCalibF2bEstablishAdopt";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "successConsumptionCoversLiveWanted";
    };
    # F3 soundness (i): InfraFailure charged as Unobtainable corrupts
    # the one-shot's evidence.
    quint-materialization-calib-f3-infra-as-unobtainable = mkQuintWitnessCheck {
      name = "materialization-calib-f3-infra-as-unobtainable";
      spec = "calibration/mat-f3-infra-as-unobtainable";
      main = "matCalibF3InfraAsUnobtainable";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "noWrongfulTerminalFailure";
    };
    # F3 soundness (ii) / E5: budget exhaustion fails instead of parks.
    quint-materialization-calib-f3-park-failfast = mkQuintWitnessCheck {
      name = "materialization-calib-f3-park-failfast";
      spec = "calibration/mat-f3-park-failfast";
      main = "matCalibF3ParkFailFast";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "materializationNeverPoisons";
    };
    # F3 permissiveness (CE-60/CE-3): unverified missing paths route
    # from source while upstream offers them.
    quint-materialization-calib-f3p-unsound-report = mkQuintWitnessCheck {
      name = "materialization-calib-f3p-unsound-report";
      spec = "calibration/mat-f3p-unsound-report";
      main = "matCalibF3pUnsoundReport";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "noWrongfulFromSourceRouting";
    };
    # F7 (CE-30/CE-28): the un-keyed resolution.
    quint-materialization-calib-f7-unkeyed-resolve = mkQuintWitnessCheck {
      name = "materialization-calib-f7-unkeyed-resolve";
      spec = "calibration/mat-f7-unkeyed-resolve";
      main = "matCalibF7UnkeyedResolve";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "jobResolutionSound";
    };
    # F9 (CE-41 re-pointed): routing trusts a divergent in-memory
    # children view over the durable relation.
    quint-materialization-calib-f9-divergent-evidence = mkQuintWitnessCheck {
      name = "materialization-calib-f9-divergent-evidence";
      spec = "calibration/mat-f9-divergent-evidence";
      main = "matCalibF9DivergentEvidence";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "routingRequiresDurableVouchOrFailFast";
    };
    # F11 (CE-50, the a17-unfenced analogue): a stale tenure's job
    # resolve applies below the claims floor.
    quint-materialization-calib-f11-unfenced-resolve = mkQuintWitnessCheck {
      name = "materialization-calib-f11-unfenced-resolve";
      spec = "calibration/mat-f11-unfenced-resolve";
      main = "matCalibF11UnfencedResolve";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "fencedJobWritesOnly";
    };
    # B4 (delta 2b): probe creation without the legacy backfill.
    quint-materialization-calib-b4-no-backfill = mkQuintWitnessCheck {
      name = "materialization-calib-b4-no-backfill";
      spec = "calibration/mat-b4-no-backfill";
      main = "matCalibB4NoBackfill";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "creationLeavesTenantResolvable";
    };
    # The partial-unique-index dedup, slot-widened.
    quint-materialization-calib-dedup-removed = mkQuintWitnessCheck {
      name = "materialization-calib-dedup-removed";
      spec = "calibration/mat-dedup-removed";
      main = "matCalibDedupRemoved";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "atMostOneUnresolvedJobPerDrv";
    };
    # PP-4 (i): the establishment writes executor_crash for the
    # materialization kind.
    quint-materialization-calib-pp4-build-charge = mkQuintWitnessCheck {
      name = "materialization-calib-pp4-build-charge";
      spec = "calibration/mat-pp4-establish-build-charge";
      main = "matCalibPp4EstablishBuildCharge";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "materializationInvisibleToBuildBudgets";
    };
    # PP-4 (ii): the establishment closes the attempt charge-free.
    quint-materialization-calib-pp4-uncharged = mkQuintWitnessCheck {
      name = "materialization-calib-pp4-uncharged";
      spec = "calibration/mat-pp4-establish-uncharged";
      main = "matCalibPp4EstablishUncharged";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "materializationCrashChargedOnce";
    };
    # B2-strong shape (a): ingest without the pin INSERT re-finds the
    # GC-after-vouch trace shape.
    quint-materialization-calib-b2-no-pin = mkQuintWitnessCheck {
      name = "materialization-calib-b2-no-pin";
      spec = "calibration/mat-b2-no-pin-at-ingest";
      main = "matCalibB2NoPinAtIngest";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "pinCoversIngestUntilAllInterestTerminal";
    };
    # F5 / PP-5 (i): the wholesale relation overwrite breaks
    # cross-build isolation.
    quint-materialization-calib-f5-wanted-overwrite = mkQuintWitnessCheck {
      name = "materialization-calib-f5-wanted-overwrite";
      spec = "calibration/mat-f5-wanted-overwrite";
      main = "matCalibF5WantedOverwrite";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "crossBuildWantedIsolation";
    };
    # F4 B4-half + the C5/CE-7 closure evidence: the dead-inclusive
    # stored-union coverage read (a terminal build's stale wants drive
    # the decision) — the behavior the live-only §6 join replaced.
    quint-materialization-calib-f4-dead-union = mkQuintWitnessCheck {
      name = "materialization-calib-f4-dead-union";
      spec = "calibration/mat-f4-dead-union";
      main = "matCalibF4DeadUnion";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "interestUnionLiveOnly";
    };
    # F1 permissiveness (CE-1): the by-construction row's REACHABILITY
    # half — the covered-creation space (the state the as-built
    # presence re-check excludes) is reachable under the override, so
    # the recorded no-falsification verdict (the §9.1 conjunction holds
    # over that space — the C-prime stage record's by-construction
    # evidence) is about a real space, not vacuity.
    quint-materialization-calib-f1-covered-creation = mkQuintWitnessCheck {
      name = "materialization-calib-f1-covered-creation";
      spec = "calibration/mat-f1-no-presence-recheck";
      main = "matCalibF1NoPresenceRecheck";
      extraSpecs = [ "materializationJob" ];
      step = "calibStep";
      witness = "noCoveredCreationJob";
    };

    # ------------------------------------------------------------------
    # spawnCoherence PD-7 extension (substitution-replacement Phase B,
    # design §9.2): the GetSpawnIntents job filter — served sets
    # exclude intents whose node carries an unresolved materialization
    # job. The six pre-existing regimes carry ENABLE_MAT_JOBS = false
    # (constant-false var; reachable state space unchanged — the
    # regime-split dormancy rule); this regime turns the environment
    # on. Weakened test: deleting the serve-time conjunct re-finds
    # VMatJobServed (procedure in the regime module's comment; depth in
    # the introducing commit).
    # r[verify sched.materialize.job]
    quint-spawn-coherence-mat-jobs = mkQuintCheck {
      name = "spawn-coherence-mat-jobs";
      spec = "spawnCoherence";
      main = "spawnCoherenceMatJobs";
      invariants = [
        "ceilingRespected"
        "reapSafety"
        "orphanRemoved"
        "ackSoundness"
        "ackCoversPending"
        "degradedPolarity"
        "gateFailClosed"
        "freedSlotsSpendable"
        "matJobFilteredFromIntents"
      ];
    };
    # The job filter actually filters (a queued, job-carrying intent
    # existed at a successful fresh poll) — matJobFilteredFromIntents'
    # non-vacuity.
    quint-spawn-coherence-witness-mat-filtered = mkQuintWitnessCheck {
      name = "spawn-coherence-witness-mat-filtered";
      spec = "spawnCoherence";
      main = "spawnCoherenceMatJobs";
      witness = "canReachMatJobFiltered";
    };

    # ------------------------------------------------------------------
    # Gateway connection/session lifecycle campaign (gw-session-formal,
    # round-2 Track B), Phase 0 Stage C: the rio-gateway accept → auth →
    # channel open → exec admission → protocol session → teardown
    # lifecycle as built — capacity permits and gauges, deadlines and
    # keepalives, force-close arming/enforcement, the cancel-on-disconnect
    # obligations, egress pacing and the three-stage drain — modeled
    # against an explicit russh transport environment
    # (docs/spec/models/gwConnLifecycle.qnt). The invariant ↔ spec-rule
    # map, the Stage-B B-measure, the Stage-C check-set decision and the
    # per-check verdicts live in
    # docs/spec/models/gw-session-invariant-map.md.
    #
    # Check-set shape (the §2e pre-registered fallback ladder, applied at
    # the Stage-B measurement milestone): the full-alphabet regimes do not
    # exhaust inside the per-check budget (the base regime alone is in the
    # tens of millions of distinct states, and fallback (2) shrinks it
    # only ≈2×), so the merge-gated EXHAUSTIVE checks are the per-family
    # restricted-alphabet modules (§2e fallbacks (3)/(4): every structural
    # and environment bound intact, conn B at connection level, one corpus
    # family's letters per check, asserting the full 34-property
    # `allInvariants` conjunction over that family's reachable space).
    # The full-alphabet regime modules carry the witness, expect-violation
    # and named-run checks below — those stop at the first violation, so
    # they stay cheap on the unrestricted alphabets. Which cross-family
    # interleavings are therefore NOT exhaustively explored is recorded
    # per property in the invariant map's Stage-C record.
    # ------------------------------------------------------------------

    # Pre-auth establishment / occupancy family (corpus F2/F3 pre-auth
    # half, GW-3/GW-4): the ConnStage machine, accept-time conn permits,
    # the gauge latch on the first auth callback (accept and reject
    # outcomes), silent / KEX-parked / never-auth peers, the two-phase
    # pre-auth deadline, the empty grace on idle authenticated
    # connections, decide-implies-arm at both polite-disconnect sites,
    # force-close enforcement, and every designed reap letter — at the
    # connection level (the admission interplay is the next check's).
    # r[verify gw.conn.lifecycle]
    # r[verify gw.conn.real-connection-marker]
    # r[verify gw.conn.force-close]
    # r[verify gw.conn.keepalive+2]
    quint-gw-lifecycle-fam-preauth = mkQuintCheck {
      name = "gw-lifecycle-fam-preauth";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamPreauth";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Admission family (corpus F2 post-auth half, the GW-18 admission
    # surface): exec admission against the empty-connection grace (the
    # arm/disarm/re-arm cycle, the documented W3/P4 admit-vs-fire race),
    # the per-session handshake and opcode-idle deadlines against
    # occupancy-withholding peers, re-admission after the session count
    # touches zero, and channel opens not counting as activity — on conn
    # A's two channels with the egress/finish machinery abstracted to
    # channel-close release (famWedge / famUpstream own those paths).
    # r[verify gw.conn.lifecycle]
    # r[verify gw.conn.exit-status+3]
    # r[verify gw.conn.exec-request]
    # r[verify gw.handshake.timeout]
    quint-gw-lifecycle-fam-admission = mkQuintCheck {
      name = "gw-lifecycle-fam-admission";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamAdmission";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Connection-cap family (GW-12 surface): MAX_CONNECTIONS dialed to 1
    # (the §2e cap-regime value) so the over-cap permit-None path — the
    # connection that reaches its first auth callback permit-less, is
    # counted there, and is torn down instead of authenticating — is
    # exhaustively explored with two modeled connections.
    # r[verify gw.conn.cap]
    # r[verify gw.conn.real-connection-marker]
    quint-gw-lifecycle-fam-cap = mkQuintCheck {
      name = "gw-lifecycle-fam-cap";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamCap";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Egress-pacing / wedge family (corpus F5, GW-2/GW-8, the W5 vanish
    # reap): window-credited sends, the send and close-out budgets firing
    # only against transport-withholding peers, capacity release before
    # close-out, the close-out ordering, queue parks against non-reading
    # peers, stall → force-close arming, and the designed transport reaps
    # (keepalive / TCP_USER_TIMEOUT) for vanished peers — exhaustive for
    # one egress session on conn A (the wedge candidates are
    # single-session shapes; the multi-session content lives in
    # famUpstream/famHostile and the full-regime witnesses). Conn B is
    # the compliant control for the P5 clauses.
    # r[verify gw.conn.send-deadline]
    # r[verify gw.conn.session-cap+2]
    # r[verify gw.conn.force-close]
    # r[verify gw.conn.exit-status+3]
    quint-gw-lifecycle-fam-wedge = mkQuintCheck {
      name = "gw-lifecycle-fam-wedge";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamWedge";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Channel-accounting / hostile-open family (corpus F7, GW-5/GW-6/GW-7):
    # forged and duplicate closes are ignored, over-bound and non-session
    # opens terminate the connection (russh residue stays bounded), burst
    # opens hit the per-connection bound, the write half is consumed by at
    # most one exec, the open/close bookkeeping never goes negative, and
    # capacity exhaustion is signaled by exec rejection only (conn B opens
    # its channel and takes the session-cap rejection — the P3 surface).
    # r[verify gw.conn.channel-limit+4]
    # r[verify gw.conn.channel-types]
    # r[verify gw.conn.per-channel-state]
    # r[verify gw.conn.exec-request]
    # r[verify gw.conn.session-cap+2]
    quint-gw-lifecycle-fam-hostile = mkQuintCheck {
      name = "gw-lifecycle-fam-hostile";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamHostile";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Upstream / teardown-obligation family (corpus F6/F9, GW-9/GW-10/
    # GW-16/GW-19; the W10/W2 surface): the rpc-wait deadline (and the
    # deliberately deadline-free build-event-wait), terminal vs non-Wire
    # build-stream outcomes against the shipped tracking policy, the
    # cancel loop on every non-panic exit, the upstream-stream release at
    # session exit, the close-out ladder, and transient accept errors.
    # Also asserts w10TriggerAbsent — the §4 W10 decision-rule trigger (a
    # build leaving the tracked set with no terminal outcome, no
    # CancelBuild attempt and the upstream stream still held after session
    # exit) is unreachable; the owner's W10 sign-off consumes this
    # together with the s16-terminal-only falsification trace below.
    # r[verify gw.conn.cancel-on-disconnect+3]
    # r[verify gw.store.transient-retry]
    # r[verify gw.conn.accept-resilience]
    # r[verify gw.conn.exit-status+3]
    quint-gw-lifecycle-fam-upstream = mkQuintCheck {
      name = "gw-lifecycle-fam-upstream";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamUpstream";
      step = "famStep";
      invariants = [
        "allInvariants"
        "w10TriggerAbsent"
      ];
    };

    # Drain family (corpus F10, GW-13): SIGTERM at any point, the
    # NOT_SERVING → accept-stop → session-drain-expiry → exit staging,
    # accept-stop terminating no established connection or session, the
    # drain-expiry shutdown-token cancel of a tracked build, and exit only
    # at full quiescence.
    # r[verify gw.drain.three-stage]
    # r[verify gw.conn.session-drain]
    quint-gw-lifecycle-fam-drain = mkQuintCheck {
      name = "gw-lifecycle-fam-drain";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamDrain";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Degraded-tier family (the §2e fault-degraded surface): the exec-gate
    # panic splitter and proto-task panics (S4's leak term, S8's
    # no-stranded-permit clause, S10's panic carve-out), the
    # TCP_USER_TIMEOUT setsockopt failure, write parks ordered against
    # force-close arming (the W7 ordering bit), and the inactivity reap
    # backstop that exists only in this regime's alphabet.
    # r[verify gw.conn.force-close]
    # r[verify gw.conn.exec-request]
    # r[verify gw.conn.keepalive+2]
    quint-gw-lifecycle-fam-degraded = mkQuintCheck {
      name = "gw-lifecycle-fam-degraded";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFamDegraded";
      step = "famStep";
      invariants = [ "allInvariants" ];
    };

    # Non-vacuity witnesses for the gateway lifecycle model, on the
    # FULL-alphabet regime modules (they stop at the first violation, so
    # the unrestricted alphabets stay affordable). Each check passes only
    # when the checker still reaches the contended scenario; one that
    # stops violating means the regime's invariants have gone vacuous for
    # it. Deliberately no tracey markers here (same policy as the other
    # models' witnesses).
    quint-gw-lifecycle-witness-server-side-release = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-server-side-release";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachServerSideEndingReleasesEarly";
    };
    quint-gw-lifecycle-witness-grace-fires-idle = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-grace-fires-idle";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachGraceFiresOnIdleConn";
    };
    quint-gw-lifecycle-witness-exec-within-grace = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-exec-within-grace";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachExecWithinGraceSurvives";
    };
    quint-gw-lifecycle-witness-session-cap-rejected = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-session-cap-rejected";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachSessionCapExecRejected";
    };
    quint-gw-lifecycle-witness-mux-touch-zero-exec = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-mux-touch-zero-exec";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachMuxTouchZeroThenExec";
    };
    quint-gw-lifecycle-witness-mux-sibling-quiescence = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-mux-sibling-quiescence";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachMuxSiblingQuiescence";
    };
    quint-gw-lifecycle-witness-close-out-order = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-close-out-order";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "canReachCloseOutCompletesInOrder";
    };
    quint-gw-lifecycle-witness-over-cap-auth-torn = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-over-cap-auth-torn";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleCap";
      witness = "canReachOverCapAuthTorn";
    };
    quint-gw-lifecycle-witness-kex-parked-reclaimed = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-kex-parked-reclaimed";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultOccupancy";
      witness = "canReachKexParkedReclaimed";
    };
    quint-gw-lifecycle-witness-stall-arms-force-close = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-stall-arms-force-close";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachStallArmsForceClose";
    };
    quint-gw-lifecycle-witness-forged-close-ignored = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-forged-close-ignored";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachForgedCloseIgnored";
    };
    quint-gw-lifecycle-witness-over-bound-open = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-over-bound-open";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachOverBoundOpenTerminates";
    };
    quint-gw-lifecycle-witness-burst-hits-bound = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-burst-hits-bound";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachBurstHitsBound";
    };
    quint-gw-lifecycle-witness-vanish-reclaimed = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-vanish-reclaimed";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      witness = "canReachVanishReclaimedDesigned";
    };
    quint-gw-lifecycle-witness-nonwire-removes-tracked = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-nonwire-removes-tracked";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultUpstream";
      witness = "canReachNonWireRemovesTracked";
    };
    quint-gw-lifecycle-witness-drain-expiry-cancel = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-drain-expiry-cancel";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultDrain";
      witness = "canReachDrainExpiryCancel";
    };
    quint-gw-lifecycle-witness-parked-inactivity-reclaim = mkQuintWitnessCheck {
      name = "gw-lifecycle-witness-parked-inactivity-reclaim";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultDegraded";
      witness = "canReachParkedReclaimedByInactivity";
    };

    # Pre-registered expected as-built falsifications (design §3/§4/§6,
    # the invariant map's probe table): each strict variant is EXPECTED to
    # be violated by today's code, and the check passes only while the
    # counterexample still materializes — a probe that stops falsifying
    # after a code or model change is a finding (the documented trade-off
    # moved), not a pass. The traces these capture are the model-first
    # evidence behind the §4 W3/W7/W10/P12 dispositions. No tracey
    # markers (the spec rules are verified by the HOLD checks above).
    quint-gw-lifecycle-falsification-l5-no-carve-out = mkQuintWitnessCheck {
      name = "gw-lifecycle-falsification-l5-no-carve-out";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      witness = "l5StrictNoCarveOut";
    };
    quint-gw-lifecycle-falsification-s16-terminal-only = mkQuintWitnessCheck {
      name = "gw-lifecycle-falsification-s16-terminal-only";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultUpstream";
      witness = "s16StrictTerminalOnly";
    };
    quint-gw-lifecycle-falsification-s10-panic = mkQuintWitnessCheck {
      name = "gw-lifecycle-falsification-s10-panic";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultDegraded";
      witness = "s10StrictIncludingPanic";
    };
    quint-gw-lifecycle-falsification-l1-no-inactivity = mkQuintWitnessCheck {
      name = "gw-lifecycle-falsification-l1-no-inactivity";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultDegraded";
      witness = "l1StrictNoInactivity";
    };

    # The two deterministic named runs: the compliant peer's full
    # lifecycle (the P5 completion half — no budget, force-close or reap
    # ever fires against it, and every resource returns to zero) and the
    # GW-2/C14 stalled-send wedge response (release before close-out,
    # close-out under its own budget, force-close armed). Replayed step by
    # step with their expectations re-asserted; each is pinned to the
    # regime whose alphabet it needs.
    quint-gw-lifecycle-run-compliant-peer = mkQuintRunCheck {
      name = "gw-lifecycle-run-compliant-peer";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleBase";
      match = "compliantLifecycleRun";
    };
    quint-gw-lifecycle-run-stall-wedge = mkQuintRunCheck {
      name = "gw-lifecycle-run-stall-wedge";
      spec = "gwConnLifecycle";
      main = "gwConnLifecycleFaultTransport";
      match = "stallArmsForceCloseRun";
    };

    # ---- Gateway lifecycle Stage-C calibration witnesses ----------------
    # The historical-fix corpus replayed against the as-built model (the
    # gw-session-formal campaign's Phase-0 Stage C). Each check
    # instantiates the gwConnLifecycle core at a family-restricted scope,
    # swaps ONE action for its PRE-FIX behavior (the calibration module's
    # `calibStep`) and passes only while the checker still falsifies the
    # invariant the corresponding historical fix protects — machine-checked
    # evidence that the model would re-find that bug class if it were
    # reintroduced, and that the invariant is not vacuous for it. The full
    # per-candidate calibration table (verdict @ depth, states, the
    # trace-walk notes, and the evidence-only override modules and
    # T-direction runs that are not wired here) lives in
    # docs/spec/models/gw-session-invariant-map.md; the wired ones are the
    # representative per-family regression guards (one per falsifying
    # family, deepest consequence, cheap state space — every check stops at
    # its first violation). Deliberately no tracey markers (same policy as
    # the other models' witness/calibration checks).

    # F1 (443670d43 / GW-1): the session permit/gauge release keyed on the
    # sessions map (client action) again — a server-side ending leaves
    # capacity held with nothing armed to release it.
    quint-gw-calib-f1-server-side-release = mkQuintWitnessCheck {
      name = "gw-calib-f1-server-side-release";
      spec = "calibration/gw-f1-capacity";
      main = "gwCalibF1ServerSideKeepsGuard";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "l2SendSettleArmed";
      # This module's conversion request is one of the two that OOM the
      # default 4 GiB Apalache server (occupancy letters x the full
      # close-out ladder); validated at 8 GiB.
      serverHeapMb = 8192;
    };

    # F2 (79912eda0 / GW-3): connection emptiness measured on open channels
    # again — a channel open disarms the empty grace and the
    # open-without-exec population sits with no deadline armed.
    quint-gw-calib-f2-open-disarms-grace = mkQuintWitnessCheck {
      name = "gw-calib-f2-open-disarms-grace";
      spec = "calibration/gw-f2-occupancy";
      main = "gwCalibF2OpenDisarmsGrace";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s20GraceArmedExactlyWhenEmpty";
    };

    # F3 (1c46d9781 / GW-4): a polite disconnect queued without arming the
    # force-close at the same decision point.
    quint-gw-calib-f3-decide-without-arm = mkQuintWitnessCheck {
      name = "gw-calib-f3-decide-without-arm";
      spec = "calibration/gw-f3-force-close";
      main = "gwCalibF3DecideWithoutArm";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s12DecideImpliesArmed";
    };

    # F4 (9739aca65 / GW-5): channel_close decrements unconditionally again
    # (forged/duplicate closes skew open_channels; exec honored on
    # never-accepted channels).
    quint-gw-calib-f4-forged-close-decrement = mkQuintWitnessCheck {
      name = "gw-calib-f4-forged-close-decrement";
      spec = "calibration/gw-f4-channel-accounting";
      main = "gwCalibF4ForgedCloseDecrements";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s7ChannelAccounting";
    };

    # F5 (51123b2be / GW-2): the close-out sends ordered before the
    # SessionGuard drop again — capacity release waits on the peer draining
    # the handle queue.
    quint-gw-calib-f5-release-after-close-out = mkQuintWitnessCheck {
      name = "gw-calib-f5-release-after-close-out";
      spec = "calibration/gw-f5-egress";
      main = "gwCalibF5ReleaseAfterCloseOut";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s15ReleaseBeforeCloseOut";
    };

    # F6 (0f476d6f0/a207ee15c / GW-9): a session-exit edge skips
    # cancel_active_builds again — the recurring case-completeness shape.
    quint-gw-calib-f6-exit-edge-skips-cancel = mkQuintWitnessCheck {
      name = "gw-calib-f6-exit-edge-skips-cancel";
      spec = "calibration/gw-f6-teardown";
      main = "gwCalibF6ExitEdgeSkipsCancel";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s10CancelOnSessionEnd";
      # Same conversion-size OOM as f1-server-side-release (the build
      # chain x the close-out ladder); validated at 8 GiB.
      serverHeapMb = 8192;
    };

    # F7 (765671437 / GW-13): the shutdown token only observed at the
    # opcode-read point again — drain expiry exits with an in-flight build
    # never cancelled.
    quint-gw-calib-f7-drain-expiry-no-cancel = mkQuintWitnessCheck {
      name = "gw-calib-f7-drain-expiry-no-cancel";
      spec = "calibration/gw-f7-drain";
      main = "gwCalibF7DrainExpiryNoCancel";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "l4DrainObligationsArmed";
    };

    # F9 (755f49744 / GW-16): an upstream RPC await inside a session loses
    # its deadline again — the session parks in rpc-wait unbounded.
    quint-gw-calib-f9-rpc-wait-no-deadline = mkQuintWitnessCheck {
      name = "gw-calib-f9-rpc-wait-no-deadline";
      spec = "calibration/gw-f9-upstream-deadline";
      main = "gwCalibF9RpcWaitNoDeadline";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "l6RpcWaitDeadlineArmed";
    };

    # F10 (9b693441f / GW-19): a transient accept error terminates the
    # listener (and the process, with every live session) again.
    quint-gw-calib-f10-accept-error-fatal = mkQuintWitnessCheck {
      name = "gw-calib-f10-accept-error-fatal";
      spec = "calibration/gw-f10-accept";
      main = "gwCalibF10AcceptErrorFatal";
      extraSpecs = [ "gwConnLifecycle" ];
      step = "calibStep";
      witness = "s18ListenerSurvivesAcceptErrors";
    };

    # ------------------------------------------------------------------
    # Closure-evidence: the Phase D-prime survivors core + the two kept
    # calibration pins. Model: docs/spec/models/closureEvidence.qnt —
    # the PREDECESSOR system's evidence lifecycle (the walk, the
    # topdown_pruned / closure_hole columns, the Substituting status,
    # the forgiveness chain), retained as the campaign record until its
    # A6 archive; the production machinery it verified was deleted by
    # substitution-replacement Phase D-prime (Waves D3-D6, migration
    # 080) — the store-owned materialization job (the
    # quint-materialization-* family above) is the only substitution
    # mechanism. The 27 walk-era checks that verified the deleted
    # machinery were pruned HERE in the same landing that wired the
    # survivors core (FP-1: never a window with neither the old oracle
    # nor the new); per-check dispositions (retired-with-mechanism vs
    # superseded-by-NAME) are in the pruning commit's body and the
    # Phase D-prime stage record
    # (docs/spec/models/substitution-replacement-invariant-map.md).
    # The held-back exhaustive targets, the historical verdicts and the
    # retired manual-target commands live in
    # docs/spec/models/closure-evidence-invariant-map.md (history is
    # append-only); the unwired calibration overrides under
    # docs/spec/models/calibration/closure-*.qnt carry per-file
    # retirement notes.

    # The survivors core, single-tenure two-build scope: the six
    # invariants whose subjects SURVIVE the deletion — A14 status
    # terminality, A15 dep gating, A22 condemnation co-ownership
    # scoping, B9 stale-Produced verify, B10 prune demand-set, L3
    # progress-armed liveness — over the restricted post-deletion
    # action alphabet (the walk letters are excluded from the step; see
    # the closureEvidenceSurvivors module's scope note, including the
    # recorded FailoverDuo B9 corner that fixed these constants).
    # B9/B10/A14/L3 are contended at this scope (prune, reap-survivor,
    # cross-build wanted shapes). Trace inclusion makes this a
    # regression oracle for recorded-green verdicts, not new coverage.
    # Paired falsifiability pins (the sim-holds vacuity rule):
    # quint-closure-calib-f1-stale-produced (B9) and
    # quint-closure-calib-f4-demand-drop (B10) below.
    # r[verify sched.merge.substitute-topdown+12]
    quint-closure-survivors-core = mkQuintSimHoldsCheck {
      name = "closure-survivors-core";
      spec = "closureEvidence";
      main = "closureEvidenceSurvivors";
      step = "survivorsStep";
      invariants = [
        "terminalIsTerminal"
        "readyImpliesDeclaredDepsProduced"
        "condemnRequiresLiveCoOwnedFailure"
        "staleProducedNeverUnlocksDependents"
        "demandSetSurvivesPrune"
        "liveBuildTerminalOrProgressArmed"
      ];
      maxSamples = 2000000;
      maxSteps = 15;
    };

    # The survivors core, failover scope: the same six invariants and
    # restricted alphabet at the FailoverEx constants (single build,
    # one failover, one lost best-effort write) — A15's recovery-time
    # half and A22's condemnation scoping are contended here (their
    # full-alphabet record at this scope is the Wave-2b re-hunt).
    quint-closure-survivors-failover = mkQuintSimHoldsCheck {
      name = "closure-survivors-failover";
      spec = "closureEvidence";
      main = "closureEvidenceSurvivorsFailover";
      step = "survivorsStep";
      invariants = [
        "terminalIsTerminal"
        "readyImpliesDeclaredDepsProduced"
        "condemnRequiresLiveCoOwnedFailure"
        "staleProducedNeverUnlocksDependents"
        "demandSetSurvivesPrune"
        "liveBuildTerminalOrProgressArmed"
      ];
      maxSamples = 2000000;
      maxSteps = 15;
    };

    # ---- Kept calibration pins (the survivors' falsifiability) --------
    # Each instantiates the closure-evidence model at the named override
    # module's constants, swaps the named action(s) for their PRE-FIX
    # behavior (the override's calibStep) and passes only while the
    # checker still falsifies the property the fix protects — the
    # machine-checked record that the model's invariant set re-finds
    # that bug class. These two stay wired because the mechanisms their
    # fixes guard SURVIVE Phase D-prime (the stale-Produced verify and
    # the prune demand-set are post-deletion production behavior, B9 and
    # B10 of the survivors core above); the other closure pins and
    # witnesses guarded walk-era mechanisms and retired with them.
    # No tracey markers on calibration checks (house convention).

    # F1 soundness (CE-2, 29f0a8afa): no stale-Produced verify — a
    # resubmission's parent ends Ready above a Produced child whose
    # live-wanted outputs are absent.
    quint-closure-calib-f1-stale-produced = mkQuintWitnessCheck {
      name = "closure-calib-f1-stale-produced";
      spec = "calibration/closure-f1-stale-produced";
      main = "closureCalibF1StaleProduced";
      extraSpecs = [ "closureEvidence" ];
      step = "calibStep";
      witness = "staleProducedNeverUnlocksDependents";
      # The closureEvidence file gained the two survivors-core module
      # instantiations at T-D7.1; the quint->TLA+ conversion request now
      # OOMs the 4 GiB default server (measured: heap-space fatal at
      # 4096, converts at 8192 — the gw-f1/f6 precedent).
      serverHeapMb = 8192;
    };

    # F4 demand-set completeness (CE-66, 85213119d): the prune's demand
    # set is the structural roots only — an explicitly requested
    # non-root is silently dropped.
    quint-closure-calib-f4-demand-drop = mkQuintWitnessCheck {
      name = "closure-calib-f4-demand-drop";
      spec = "calibration/closure-f4-demand-drop";
      main = "closureCalibF4DemandDrop";
      extraSpecs = [ "closureEvidence" ];
      step = "calibStep";
      witness = "demandSetSurvivesPrune";
      # Same conversion-size OOM as the f1 pin above (the survivors-core
      # modules); validated at 8 GiB.
      serverHeapMb = 8192;
    };
  };
}

# Per-crate Kani verification: hand crateBuildKani's output to
# `kani verify-artifacts`.
#
# crateBuildKani (flake.nix) compiles every workspace member with
# kani-compiler and stashes the `*.symtab.out` + `kani-metadata.json`
# sidecars in the crate's `lib/` output. This file copies them to a
# writable scratch dir and runs `kani verify-artifacts` on it. kani-driver
# owns the entire CBMC pipeline (link, specialize, instrument-contracts,
# add-library, undefined-functions, backedges, cbmc), the verdict
# rendering, should_panic inversion, loop contracts, and parallel harness
# scheduling. We don't replicate any of it.
#
# `kani verify-artifacts` is in our pinned `lovesegfault/kani/rio-build`
# fork (kani-0.67.0 + verify-artifacts + compiler-defaults patches),
# pending upstream PRs. See nix/kani-toolchain.nix.
#
# Caching model:
#   - kani-compiler compile phase: per-crate cached via crateBuildKani.
#     Editing rio-common rebuilds rio-common's kani drv + downstream
#     dependents; editing a test file rebuilds nothing.
#   - kani-driver verify phase (this file): re-runs when the workspace
#     member's own kani drv changes, which transitively means when the
#     member or any reachable dep changes. Correct — `--reachability=harnesses`
#     produces a goto program that closes over the harness's call graph.
#
# Limitation: only lib-only workspace members. Members with [[bin]] hit a
# link error in crateBuildKani — deps are MIR-only. See
# nix/crate2nix.nix's `excludeCrateTypes` for the cdylib analogue.
#
# r[verify ...] markers go HERE — at the `kani-checks` attrset entry that
# wires a member, NOT in the harness function or the .typ spec source.
# Same wiring-point discipline as `subtests = [...]` in
# nix/tests/default.nix (P0341). .config/tracey/config.styx already lists
# this file under `test_include`.
{
  pkgs,
  kaniToolchain,
  crateBuildKani,
}:
let
  mkKaniCheck =
    {
      name,
      crate,
      # Exact harness-count tripwire: when non-null, the check fails
      # unless kani-driver's summary reports exactly this many verified
      # harnesses — so a harness silently dropping out of the artifact
      # set (a cfg/module-path regression, an accidental delete) is a
      # red check, not a quieter green one. `null` keeps the weaker
      # at-least-one guard only.
      expectedHarnesses ? null,
    }:
    pkgs.runCommand "kani-${name}"
      {
        nativeBuildInputs = [
          # `kani` (with cbmc, kissat, gcc on its wrapper PATH).
          kaniToolchain.kani-driver-wrapped
        ];
        # buildRustCrate's `lib` output holds `lib/{*.rlib,*.symtab.out,
        # *.kani-metadata.json}`. The `or crate` fallback supports a
        # `{ lib = <store-path>; }` shim for ad-hoc testing.
        src = crate.lib or crate;
        # Surfaced in `nix log` and error messages.
        env.MEMBER = name;
        env.EXPECTED_HARNESSES = if expectedHarnesses == null then "" else toString expectedHarnesses;
      }
      ''
        set -euo pipefail

        artifacts="$src/lib"
        if [ ! -d "$artifacts" ]; then
          echo "ERROR: $artifacts does not exist (expected a crateBuildKani lib output)." >&2
          exit 1
        fi

        # The artifact dir is a read-only store path. `kani verify-artifacts`
        # writes the linked `.out` file next to each `.symtab.out`
        # (Project::try_new()), so it needs a writable copy.
        work=$(mktemp -d)
        cp "$artifacts"/* "$work/"
        chmod -R u+w "$work"

        # kani-driver exits 1 on any verification failure — runCommand's
        # `set -e` propagates that. tee into $out so `cat result` and
        # `nix log` carry the per-harness verdicts.
        #
        # `--jobs` takes an optional value (clap `[<JOBS>]`); the `=` form
        # binds it to the flag instead of being parsed as a positional
        # artifact dir. kani requires `--output-format=terse` with `--jobs`
        # (parallel harness output would otherwise interleave); terse still
        # prints the per-harness verdict + the final
        # `Complete - N successfully verified harnesses` summary line.
        kani verify-artifacts "$work" \
          -Z unstable-options \
          --output-format=terse \
          --jobs="''${NIX_BUILD_CORES:-1}" \
          2>&1 | tee "$out"

        # Non-vacuity guard, same discipline as mbt-rio-lease's "N tests
        # run" grep and mkQuintWitnessCheck's violation-report grep
        # (nix/quint.nix): kani-driver exits 0 when the artifact set
        # contains zero proof harnesses ("No proof harnesses ... were
        # found to verify"), so the exit code alone cannot distinguish
        # "all harnesses verified" from "verified nothing". Require the
        # summary line to report at least one verified harness; kani
        # already exits 1 on any verification failure, so this only has
        # to catch the verified-nothing case (and any future exit-0 path
        # that produces no summary line at all).
        if ! grep -qE 'Complete - [1-9][0-9]* successfully verified harnesses' "$out"; then
          echo "kani-$MEMBER: no proof harnesses were verified (vacuous check)." >&2
          echo "Check that $artifacts contains a *.kani-metadata.json with a non-empty proof_harnesses list:" >&2
          echo "either the #[cfg(kani)] proofs module stopped being compiled into the kani build, or the crateBuildKani / kani-compiler-defaults plumbing changed (flake.nix, nix/kani-toolchain.nix)." >&2
          exit 1
        fi

        # Exact harness-count tripwire (per-member, opt-in): a harness
        # that silently stops being compiled or discovered must fail the
        # check, not shrink it.
        if [ -n "$EXPECTED_HARNESSES" ]; then
          if ! grep -qE "Complete - $EXPECTED_HARNESSES successfully verified harnesses" "$out"; then
            echo "kani-$MEMBER: expected exactly $EXPECTED_HARNESSES verified harnesses; summary line disagrees:" >&2
            grep -E 'Complete - [0-9]+ successfully verified harnesses' "$out" >&2 || true
            echo "If a harness was deliberately added or removed, update expectedHarnesses at this member's entry in nix/kani.nix." >&2
            exit 1
          fi
        fi
      '';
in
{
  # Expose the constructor so ad-hoc spike tests can wire their own checks.
  inherit mkKaniCheck;

  # rio-lease: lease/election state machine. One #[kani::proof_for_contract(decide_pure)]
  # harness verifies the four #[kani::ensures] iff-clauses on decide_pure() over
  # the full input domain. The contract case structure parallels the action
  # partition in docs/spec/models/leaderElection.qnt.
  # r[verify sched.lease.k8s-lease+2]
  # r[verify sched.lease.at-most-one-leader+3]
  kani-rio-lease = mkKaniCheck {
    name = "rio-lease";
    crate = crateBuildKani.members.rio-lease;
  };

  # rio-store: the log-chunk decision kernels (rio-store/src/logs/kernel.rs).
  # Five harnesses:
  #   - check_visit_chunk_contract / check_accept_verdict_contract:
  #     #[kani::proof_for_contract] over the full input domain — the
  #     chunk-interval arithmetic cannot overflow under the manifest
  #     BIGINT precondition, and the accept verdict partitions its
  #     inputs exactly as docs/spec/models/logService.qnt::acceptVerdict.
  #   - check_dedup_{pair,triple}_serves_union_exactly_once: the read
  #     path's ordered-walk dedup serves each line at most once and the
  #     served set equals the union of the chunks' ranges (the model's
  #     servedSpanExact, over the full u64 domain instead of MAX_LINE=3).
  #   - check_manifest_covers_no_uncovered_point: a manifest reported as
  #     covering [0, up_to) really has no gap (the soundness direction
  #     of the completeness predicate that seals a log against appends).
  # r[verify store.log.session-keyed]
  # r[verify store.log.ingest-bounds]
  # r[verify store.log.completeness-gate]
  kani-rio-store = mkKaniCheck {
    name = "rio-store";
    crate = crateBuildKani.members.rio-store;
  };

  # rio-scheduler: the retry/poison decision kernels
  # (rio-scheduler/src/retry_policy.rs — decide()/classify()/placeable()
  # and the reference fold's counter arithmetic).
  #
  # MANUAL TARGET for now — run with
  #   nix build .#kani-toolchain.kani-checks.kani-rio-scheduler
  # It is deliberately NOT gated in checks.*: CBMC on these harnesses
  # inside rio-scheduler's artifact context (the goto model inherits the
  # crate's full reachable code, Arc-backed identifiers, and the
  # f64 timestamp conversions) did not complete inside a merge-gate
  # budget when introduced (>18 min per harness without convergence;
  # numbers in the introducing commit's message). Gate it once the
  # counter-arithmetic kernels are extracted into a dependency-light
  # context the way rio-store's logs/kernel.rs was (the recorded Phase-2
  # deferral in docs/spec/models/retry-invariant-map.md), and add the
  # r[verify] markers for the covered sched.retry.* rules at this wiring
  # point at the same time (markers are deliberately absent until the
  # check actually runs in CI — the rules keep their existing
  # unit-test / model-check verify sites meanwhile).
  #
  # Six harnesses:
  #   - check_decide_contract: #[kani::proof_for_contract] over bounded
  #     arbitrary attempt suffixes, scaled budgets, and optional legacy
  #     seeds — the verdict partition is consistent with the final
  #     counters (each terminal verdict names a budget really at its
  #     bound; fleet-exhaust is unreachable from decide()), a Requeue
  #     verdict never exceeds a budget cap, the exclusion set contains
  #     the executor of every charged threshold attempt plus the legacy
  #     seed's members, the seed floor never drops below the frozen
  #     mirror columns, and (overflow checks on) the fold's counter
  #     arithmetic cannot overflow over the domain.
  #   - check_decide_deterministic: same inputs, two calls, equal
  #     Decisions.
  #   - check_legacy_seed_merge_monotone: the P5 seed-vs-unseeded
  #     two-call form — legacy and suffix evidence both preserved,
  #     channel budgets seed-independent, reset-bearing suffixes ignore
  #     the seed (the `sched.retry.recovery-projection+2` floor
  #     semantics).
  #   - check_classify_contract: the classification partition iff per
  #     observed-failure variant; the exemption predicate is exactly
  #     promoted-or-CONCURRENT_PUTPATH on the worker channel and
  #     promoted on the controller channel (the exempt-infra-cap
  #     definition, both channels).
  #   - check_placeable_contract: the placement partition iff — empty
  #     fleet defers, exhaustion requires every eligible worker
  #     excluded.
  #   - check_fold_fleet_exhaust_arm: the fold-side fleet-exhaust arm
  #     (E1) needs a non-empty fully-failed fleet; an empty fleet never
  #     poisons.
  kani-rio-scheduler = mkKaniCheck {
    name = "rio-scheduler";
    crate = crateBuildKani.members.rio-scheduler;
    expectedHarnesses = 6;
  };
}

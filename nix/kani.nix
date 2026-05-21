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
    { name, crate }:
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
      '';
in
{
  # Expose the constructor so ad-hoc spike tests can wire their own checks.
  inherit mkKaniCheck;

  # rio-lease: lease/election state machine. One #[kani::proof_for_contract(decide_pure)]
  # harness verifies the four #[kani::ensures] iff-clauses on decide_pure() over
  # the full input domain. The contract case structure parallels the action
  # partition in docs/spec/models/LeaderElection.tla.
  # r[verify sched.lease.k8s-lease]
  # r[verify sched.lease.at-most-one-leader+3]
  kani-rio-lease = mkKaniCheck {
    name = "rio-lease";
    crate = crateBuildKani.members.rio-lease;
  };
}

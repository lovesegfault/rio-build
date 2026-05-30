{
  description = "rio-build - Nix build orchestration";

  inputs = {
    nix = {
      url = "github:NixOS/Nix/2.34.7";
      inputs = {
        flake-compat.follows = "flake-compat";
        flake-parts.follows = "flake-parts";
        git-hooks-nix.follows = "git-hooks-nix";
        nixpkgs.follows = "nixpkgs";
        # Upstream-test-only nixpkgs pins (used by nix's own integration
        # tests, not by the .nix-cli package we consume). Stub to drop
        # them from flake.lock.
        nixpkgs-23-11.follows = "";
        nixpkgs-regression.follows = "";
      };
    };

    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };

    # Spec-coverage tool (nix/tracey.nix). Flake input (not fetchFromGitHub)
    # so importCargoLock reads Cargo.lock from a pre-fetched path — no IFD.
    tracey-src = {
      url = "github:lovesegfault/tracey/typst-spec";
      flake = false;
    };

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    # RustSec advisory DB for cargo-deny (hermetic — no network at
    # build time). Bump via `nix flake update advisory-db` to pick up
    # new advisories.
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };

    # Per-crate Nix builds (evaluation PoC — see
    # .claude/notes/crate2nix-migration-assessment.md). Pinned to master
    # for the experimental JSON output (Cargo.json + lib/build-from-json.nix:
    # feature resolution in Rust, no 6k+ line Cargo.nix checked in).
    # PR #453 added native devDependencies to the JSON output, so no
    # post-processing is needed for test builds.
    #
    # We consume two surfaces:
    #   - `lib/build-from-json.nix` as a source file (no inputs needed)
    #   - The CLI binary for `crate2nix generate --format json`
    #
    # Everything else in crate2nix's flake (devshell, cachix,
    # pre-commit-hooks, nix-test-runner, crate2nix_stable bootstrap)
    # is upstream dev tooling. Their flake-parts wiring imports
    # `inputs.devshell.flakeModule` unconditionally at the top level —
    # eager module eval means `follows = ""` on devshell breaks
    # `packages.default` even though the CLI build itself doesn't
    # touch devshell.
    #
    # `flake = false` sidesteps the whole thing: zero transitive
    # inputs in flake.lock. The CLI is built via the callPackage-
    # compatible `crate2nix/default.nix` entrypoint (checked-in
    # Cargo.nix + nixpkgs' buildRustCrate; same machinery as
    # crate2nix's own bootstrap). See `crate2nixCli` in the
    # perSystem let-block.
    #
    # Pinned to a fork branch that adds the `isHost` flag to
    # `buildRustCrateForPkgs` (host-vs-target graph distinction).
    # crateBuildKani needs it to compile proc-macros and build scripts
    # with vanilla rustc while target crates use kani-compiler.
    # Upstream: https://github.com/nix-community/crate2nix/pull/481.
    # Revert to `github:nix-community/crate2nix` once #481 lands.
    crate2nix = {
      url = "github:lovesegfault/crate2nix/is-host-flag";
      flake = false;
    };

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    git-hooks-nix = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-compat.follows = "flake-compat";
    };

    # Helm charts as Nix derivations (FODs — hash-pinned, cached). The
    # bitnami PG subchart + rook-ceph operator + cluster charts come from
    # here. Alternative was vendoring .tgz into git (ugly) or hand-rolling
    # a `helm pull` FOD (nixhelm already did that work). Only the
    # chartsDerivations output is used; nixhelm's transitive inputs
    # (pyproject-nix etc) are unused but pulled into flake.lock — cost of
    # one flake input.
    nixhelm = {
      url = "github:farcaller/nixhelm";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        # Only chartsDerivations is consumed; the helmupdater Python
        # tool (which these inputs feed) is unused.
        pyproject-nix.follows = "";
        pyproject-build-systems.follows = "";
        uv2nix.follows = "";
      };
    };
  };

  outputs =
    inputs@{
      flake-parts,
      nixpkgs,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } (
      { withSystem, flake-parts-lib, ... }:
      {
        imports = [
          inputs.treefmt-nix.flakeModule
          inputs.git-hooks-nix.flakeModule
        ];

        # Custom perSystem option for the CI matrix data — the only
        # non-derivation attrset that needs a stable .# path. Exposed at
        # the flake top level via withSystem below; no legacyPackages
        # bridge needed. Declaring `options` means everything else goes
        # under `config` (module-system rules).
        options.perSystem = flake-parts-lib.mkPerSystemOption {
          options.ciMatrix = nixpkgs.lib.mkOption {
            type = nixpkgs.lib.types.lazyAttrsOf nixpkgs.lib.types.raw;
            description = "GHA matrix data: {checks, formal, fuzz, vm-test, coverage} → name→drv attrsets";
          };
        };

        config = {
          systems = [
            "x86_64-linux"
            "aarch64-linux"
          ];

          # NixOS modules for deploying rio services. These are consumed by
          # the standalone-fixture VM tests (nix/tests/fixtures/standalone.nix)
          # and can be reused for real deployments. Each module reads
          # `services.rio.package` for binaries, so callers must set that to
          # a workspace build.
          flake.nixosModules = {
            store = ./nix/modules/store.nix;
            scheduler = ./nix/modules/scheduler.nix;
            gateway = ./nix/modules/gateway.nix;
            worker = ./nix/modules/builder.nix;
          };

          # CI integration — see the perSystem ciMatrix definition.
          # Linux-only CI runners, so hardcode x86_64-linux. ci.yml /
          # nix/gen_matrix.py address `.#githubActions.<kind>.<name>`.
          flake.githubActions = withSystem "x86_64-linux" ({ config, ... }: config.ciMatrix);

          perSystem =
            {
              config,
              pkgs,
              system,
              ...
            }:
            let
              # Read version from Cargo.toml
              cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
              inherit (cargoToml.workspace.package) version;

              # `nix-everything` without its `checkInputs` test gate. Tests
              # need a `nix-daemon`/`nix-store` runtime, not Nix's own test
              # suite — which fails on some remote builders' sandbox limits
              # (e.g. `readLinkAt.works` ENAMETOOLONG) and then blocks
              # checks.nextest-rio-* on a derivation outside this repo. The
              # wrapper output is byte-identical with and without the gate.
              nixForTests = inputs.nix.packages.${system}.nix.overrideAttrs (_: {
                doCheck = false;
              });

              # --------------------------------------------------------------
              # Rust toolchains
              # --------------------------------------------------------------
              #
              # Stable: single source of truth for CI (clippy, nextest,
              # workspace build, coverage, docs). Read from
              # rust-toolchain.toml so `rustup` users and Nix users agree.
              # Guarantees releases are stable-compatible.
              rustStable = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

              # Nightly: used by the default dev shell and fuzz builds.
              # selectLatestNightlyWith auto-picks the most recent nightly
              # that has all requested components, so we're never blocked on
              # a bad nightly.
              #
              # NOTE: non-hermetic by design — bumping rust-overlay changes
              # the nightly date and invalidates the fuzz-build cache. If
              # this becomes a problem, pin to rust-bin.nightly."YYYY-MM-DD".
              rustNightly = pkgs.rust-bin.selectLatestNightlyWith (
                toolchain:
                toolchain.default.override {
                  extensions = [
                    "rust-src"
                    "llvm-tools-preview"
                    "rustfmt"
                    "clippy"
                    "rust-analyzer"
                  ];
                }
              );

              # nixpkgs rustPlatform wired to our rust-overlay toolchains.
              # Stable: used by nix/tracey.nix (external tool, edition-2024
              # capable toolchain). Nightly: used by nix/fuzz.nix
              # (libfuzzer-sys needs -Zsanitizer=address).
              rustPlatformStable = pkgs.makeRustPlatform {
                rustc = rustStable;
                cargo = rustStable;
              };
              rustPlatformNightly = pkgs.makeRustPlatform {
                rustc = rustNightly;
                cargo = rustNightly;
              };

              # Source root for filesets
              unfilteredRoot = ./.;

              # Cargo.toml [workspace] members — adding/removing a crate is
              # a Cargo.toml-only change; filesets and check sets are derived.
              inherit (cargoToml.workspace) members;
              # Drop workspace-hack from per-member maps. It's the
              # crate2nix-stubbed hakari crate (nix/crate2nix.nix zeroes
              # its deps) — not real source, so clippy/doc/nextest/lcov
              # over its 1-line stub is a no-op. Passed to checks.nix and
              # nix/lib/filesets.nix so the filter has one definition.
              noHack = pkgs.lib.filterAttrs (n: _: n != "workspace-hack");

              # Per-member + workspace filesets (see nix/lib/filesets.nix).
              # Derives bin/test fileset splits, lcov extract patterns, and
              # the workspace union from `members`.
              inherit
                (import ./nix/lib/filesets.nix {
                  inherit (pkgs) lib;
                  inherit members unfilteredRoot noHack;
                })
                memberFilesets
                covExtractPatterns
                workspaceFileset
                workspaceSrc
                memberSrcs
                manifestsFileset
                stubTargetFiles
                ;

              # Prefix every key in an attrset. Used to surface per-member
              # derivations under flake packages.
              prefixed = p: pkgs.lib.mapAttrs' (n: v: pkgs.lib.nameValuePair "${p}${n}" v);

              # sys-crate linkage policy (see nix/lib/sys-crates.nix).
              # crate2nix crateOverrides reference .crates.<name>; devShell
              # consumes the derived .allEnv/.allLibs aggregates.
              sysCrateEnv = import ./nix/lib/sys-crates.nix { inherit pkgs; };

              # Workspace binaries (crate2nix per-crate build, stripped in
              # nix/crate2nix.nix). What VM tests, worker-vm, crdgen, and
              # the docker `all` aggregate consume.
              rio-workspace = crateBuild.workspaceBins;

              # Per-crate stripped bins, keyed by crate name (rio-gateway,
              # rio-builder, …). docker.nix consumes these so each image
              # only carries the binary it ships — the wshack-nix stub win
              # (657→~344 rust drvs for builder) reaches the image build.
              rio-crates = crateBuild.memberBins;

              # Coverage-instrumented workspace. crate2nix parallel tree
              # with globalExtraRustcOpts=["-Cinstrument-coverage"]. Used
              # by vmTestsCov + nix/coverage.nix. NOT stripped (stripping
              # removes the __llvm_covfun/__llvm_covmap sections llvm-cov
              # needs). remap-path-prefix at compile time collapses the
              # closure to glibc+syslibs — fits k3s containerd tmpfs.
              rio-workspace-cov = crateBuildCov.workspaceBins;
              rio-crates-cov = crateBuildCov.memberBins;

              # --------------------------------------------------------------
              # Fuzz build pipeline (extracted to nix/fuzz.nix)
              # --------------------------------------------------------------
              #
              # Produces fuzz.runs — 2min checks, keyed fuzz-<target>.
              # The compiled fuzz binaries are run-time inputs of those
              # derivations, not exposed standalone.
              fuzz = import ./nix/fuzz.nix {
                inherit
                  pkgs
                  rustNightly
                  sysCrateEnv
                  unfilteredRoot
                  memberSrcs
                  ;
                inherit (pkgs) lib;
                crate2nixSrc = inputs.crate2nix;
              };

              # Spec-coverage CLI + web dashboard. The SPA is built via
              # fetchPnpmDeps in nix/tracey.nix and embedded at compile time.
              traceyPkg = import ./nix/tracey.nix {
                inherit pkgs;
                rustPlatform = rustPlatformNightly;
                inherit (inputs) tracey-src;
              };

              # mdbook-style HTML generator for typst sources. Not in
              # nixpkgs; packaged here for the docs build.
              shiroaPkg = pkgs.callPackage ./nix/shiroa.nix { };

              # Typst design-book pipeline: hermetic typst env (rioTypst),
              # PDF (docs-pdf), shiroa HTML (docs). See nix/docs.nix.
              # `xtaskBin` forward-references crateBuild (defined below) —
              # nix let-bindings are mutually recursive so this is fine.
              docsLib = import ./nix/docs.nix {
                inherit pkgs shiroaPkg;
                inherit (pkgs) lib;
                inherit (inputs) tracey-src self;
                xtaskBin = crateBuild.memberBins.xtask;
              };

              # crate2nix CLI built from source against OUR nixpkgs.
              # inputs.crate2nix is `flake = false` (bare source tree) so
              # its 8 transitive flake inputs (devshell, cachix,
              # pre-commit-hooks, nix-test-runner, crate2nix_stable, …)
              # don't bloat flake.lock. `crate2nix/default.nix` is the
              # callPackage-compatible entrypoint — same one upstream's
              # bootstrap uses — reads the checked-in Cargo.nix and
              # builds via pkgs.buildRustCrate.
              #
              # The only nixpkgs-version risk here is `callPackage
              # Cargo.nix` — if upstream's Cargo.nix template references
              # a buildRustCrate attr our nixpkgs lacks, the CLI build
              # fails. In practice the template surface is stable (the
              # template itself is what crate2nix generates for every
              # user, so it's tested against a wide nixpkgs range). If
              # this does break on a nixpkgs bump: pin
              # `inputs.crate2nix-nixpkgs` separately and pass that
              # through as `pkgs` here.
              crate2nixCli = pkgs.callPackage "${inputs.crate2nix}/crate2nix/default.nix" {
                cargo = rustStable;
              };

              # ──────────────────────────────────────────────────────────
              # crate2nix JSON-mode build
              # ──────────────────────────────────────────────────────────
              #
              # Per-crate build pipeline using pkgs.buildRustCrate + a
              # pre-resolved Cargo.json. See nix/crate2nix.nix and
              # .claude/notes/crate2nix-migration-assessment.md for the
              # rationale and caveats. Exposed below as
              # packages.workspace + packages.rio-<crate>. Per-crate src
              # isolation comes from `memberSrcs` (nix/lib/filesets.nix) —
              # crate2nix.nix intercepts `crate_.src` at
              # buildRustCrateForPkgs so editing one member only rehashes
              # its own derivation.
              mkCrateBuild =
                extra:
                import ./nix/crate2nix.nix (
                  {
                    inherit
                      pkgs
                      sysCrateEnv
                      workspaceSrc
                      memberSrcs
                      ;
                    inherit (pkgs) lib;
                    rust = rustStable;
                    crate2nixSrc = inputs.crate2nix;
                  }
                  // extra
                );
              crateBuild = mkCrateBuild { };

              # Coverage-instrumented tree: re-import with the
              # instrumentation flags applied to WORKSPACE MEMBERS ONLY
              # (localExtraRustcOpts). The third-party dep derivations end
              # up bit-identical to crateBuild's — same rustc argv, same
              # inputs — so the two trees share every dep store path and
              # only the members exist twice (one normal + one
              # instrumented variant each).
              #
              # Instrumenting the deps too (globalExtraRustcOpts) would
              # build a second, fully disjoint copy of the entire dep
              # graph whose coverage data the lcov pipeline then discards
              # anyway (the report step extracts rio-* paths only — see
              # the localRemap comment in nix/crate2nix.nix). Same
              # local-only compromise the fuzz tree makes for sancov.
              #
              # `stripBins = false` keeps __llvm_covfun/__llvm_covmap
              # intact in the scrubbed member binaries.
              #
              # The flag set is deliberately minimal — the instrumented
              # member should be the normal member plus instrumentation,
              # nothing else. line-tables-only is the one extra: the
              # base build carries no debuginfo, and coverage-mode VM
              # test failures need legible backtraces.
              crateBuildCov = mkCrateBuild {
                localExtraRustcOpts = [
                  "-Cinstrument-coverage"
                  "-Cdebuginfo=line-tables-only"
                ];
                stripBins = false;
              };

              # Kani verification tree: every crate compiled by
              # kani-compiler so workspace members emit goto-C symbol
              # tables alongside their .rlib. Deps get
              # `--reachability=none` (MIR-only rlib, no codegen);
              # workspace members get `--reachability=harnesses`
              # (one goto-C model per `#[kani::proof]`). The flag list
              # mirrors `kani-driver`'s base_rustc_flags() +
              # LibConfig::new() + kani_rustc_flags() so the artifacts
              # are bit-identical to what `cargo kani` would produce.
              #
              # The `--reachability=*` knob is read from `-Cllvm-args`,
              # NOT a bare flag — kani-compiler is a rustc_driver plugin
              # that parses its own args from `config.opts.cg.llvm_args`.
              # rustc aggregates `-Cllvm-args` across occurrences but
              # kani's clap parser ERRORS on a duplicate `--reachability`
              # (verified empirically; clap derive uses `ArgAction::Set`
              # but does not allow multiple occurrences). Deps therefore
              # rely on kani-compiler's `default_value = "none"`
              # (kani-compiler/src/args.rs) and only workspace members
              # pass `--reachability=harnesses` via localExtraRustcOpts.
              # rustc flags for crateBuildKani. Soundness-critical flags
              # (panic=abort, overflow-checks, always-encode-mir,
              # mir-enable-passes, --cfg=kani, --check-cfg, …) are now set by
              # kani-compiler unconditionally (lovesegfault/kani/rio-build's
              # compiler-defaults patch); we only pass install-layout flags,
              # routing markers, and the flags kani-compiler deliberately does
              # NOT default.
              kaniBaseFlags = [
                # Gates `--extern noprelude:` and other restricted flags. Not a
                # kani-compiler default (caller preference) but load-bearing here.
                "-Z"
                "unstable-options"
                # LibConfig::new() — kani sysroot + library injection. `--sysroot`
                # points at the kani install root; rustc appends
                # `lib/rustlib/<target>/lib/`. `-L` makes libkani.rlib resolvable
                # for the bare `--extern kani`.
                "--sysroot"
                "${kaniToolchain.kani}"
                "-L"
                "${kaniToolchain.kani}/lib"
                "--extern"
                "kani"
                "--extern"
                "noprelude:std=${kaniToolchain.kani}/lib/libstd.rlib"
                # The kanitool attribute namespace. NOT a kani-compiler default
                # because rustc errors on a duplicate registration and cargo kani
                # already passes these. Build systems must pass them explicitly.
                "-Z"
                "crate-attr=feature(register_tool)"
                "-Z"
                "crate-attr=register_tool(kanitool)"
                # No-op here: buildRustCrate appends `-Clinker=cc` AFTER
                # extraRustcOpts (build-crate.nix), and rustc is last-flag-wins,
                # so `cc` actually links — and rlibs never invoke a linker
                # anyway. kani-compiler does NOT default `-Clinker` (it would
                # clobber a real linker a build system needs). Kept defensively.
                "-C"
                "linker=echo"
                # Marker flag — without it kani-compiler falls back to vanilla
                # rustc_driver. This is what gates the compiler-defaults patch:
                # only kani-mode invocations get the soundness flags.
                "--kani-compiler"
              ];
              crateBuildKani = mkCrateBuild {
                rust = kaniToolchain.kani-rustc;
                # No explicit --reachability for deps: kani-compiler
                # defaults to `none` and clap rejects a duplicate flag.
                globalExtraRustcOpts = kaniBaseFlags;
                # Host artifacts (build scripts, proc-macros, and their
                # dep closures) compile with vanilla rustc opts.
                # kani-compiler is still the rustc binary
                # (rust = kani-rustc above) but without `--kani-compiler`
                # it falls back to vanilla rustc_driver, so proc-macros
                # produce loadable .so files instead of goto-C JSON
                # stubs and build scripts execute normally. Mirrors
                # `cargo kani`'s `-Z target-applies-to-host`.
                globalExtraRustcOptsHost = [ ];
                # The same split for WORKSPACE MEMBERS that land in the
                # host graph (rio-test-support's build.rs depends on
                # rio-proto, which drags rio-common/rio-nix/
                # workspace-hack into the host closure of any member
                # that depends on rio-test-support — rio-store does, via
                # its feature-unified `test-utils`). localExtraRustcOpts
                # below is a set of kani-compiler arguments riding in
                # `-Cllvm-args`; without `--kani-compiler` (host crates
                # drop it via globalExtraRustcOptsHost) vanilla rustc
                # forwards them to LLVM, which rejects
                # `--reachability=harnesses` as an unknown argument.
                localExtraRustcOptsHost = [ ];
                localExtraRustcOpts = [
                  "-Cllvm-args=--reachability=harnesses"
                  # Function contracts: gate behind kani-compiler's
                  # `-Z function-contracts` unstable feature. Without it,
                  # any `#[kani::ensures]`/`#[kani::requires]`/
                  # `#[kani::proof_for_contract]` errors out at compile
                  # time (kani-compiler/src/kani_middle/attributes.rs:441).
                  # The flag travels inside `-Cllvm-args` (the kani-compiler
                  # arg channel — see encode_as_rustc_arg in
                  # kani-driver/src/util.rs); rustc aggregates `-Cllvm-args`
                  # across occurrences, and the attached `-Zname` form
                  # avoids whitespace that buildRustCrate's lib.sh would
                  # split into separate (and thus rustc-bound) words.
                  "-Cllvm-args=-Zfunction-contracts"
                  # Contract codegen needs the kani_macros-synthesized
                  # contract-wrapper closures to remain distinct mono-
                  # items. buildRustCrate's release profile sets
                  # `-C opt-level=3` (build-crate.nix:32), which enables
                  # MIR inlining and absorbs those closures; kani-compiler
                  # then panics with `Function '<closure>' is not declared`
                  # in codegen_modifies_contract (contract.rs:128).
                  # `cargo kani` always runs the dev profile (opt-level=0),
                  # so this is the upstream-default behavior. extraRustcOpts
                  # is appended AFTER the release-derived opt flag
                  # (build-crate.nix:52) and rustc is last-flag-wins, so
                  # this overrides cleanly for workspace members without
                  # touching dep drvPaths.
                  #
                  # CONSTRAINT: contracted functions must be defined in
                  # workspace members. Deps compile at opt-level=3 without
                  # -Zfunction-contracts (only localExtraRustcOpts gets
                  # these flags), so a #[kani::ensures] on a dep function
                  # fails loudly at compile time — either the inlining
                  # panic (codegen_modifies_contract: "Function '<closure>'
                  # is not declared") or the unstable-feature error. To
                  # support dep-crate contracts the whole crateBuildKani
                  # tree would need release=false + the contracts flag in
                  # globalExtraRustcOpts (matching cargo kani's dev
                  # profile).
                  "-Copt-level=0"
                ];
                # Target deps are compiled `--reachability=none` —
                # kani-compiler skips codegen and only encodes MIR into
                # the rlib. Linking a cdylib/staticlib from such a crate
                # fails ("symbol not defined" at the version script).
                # `cargo kani` never builds these types for deps; cargo
                # only requests what a downstream crate links against
                # (rlib). `-Clinker=echo` in kaniBaseFlags doesn't help:
                # buildRustCrate appends `-Clinker=cc` after
                # extraRustcOpts so `cc` wins. Drop the linked-output
                # types instead. Affects only crates that declare them
                # (crc-fast, wasm-streams in the current lockfile).
                excludeCrateTypes = [
                  "cdylib"
                  "staticlib"
                ];
                # The kani tree has *distinct* host and target drvs for
                # the same crate (only the kani tree differentiates).
                # buildRustCrate's `-C metadata` filename suffix doesn't
                # incorporate `extraRustcOpts`, so a host rlib and its
                # target sibling have the *same* filename but different
                # SVHs. Stop proc-macros from leaking their host rlib
                # closure into `target/deps/` where it would shadow the
                # target rlibs (E0460). Other trees see no host/target
                # split (host == target drv), so the leak is invisible
                # there and the default stays off to preserve drvPaths.
                pruneProcMacroTransitiveDeps = true;
                # kani artifacts are .rlib + .symtab.out + .json sidecars,
                # not binaries; strip is a no-op. Default kept explicit.
                stripBins = true;
              };

              # ──────────────────────────────────────────────────────────
              # crate2nix check backends: clippy, tests, doc
              # ──────────────────────────────────────────────────────────
              #
              # Per-crate checks layered on the crate2nix build graph.
              # Deps are built once with regular rustc and stay cached;
              # workspace members are rebuilt per-check with the
              # appropriate driver (clippy-driver, rustc --test, rustdoc).
              # See nix/checks.nix for the wrapper mechanics — notably the
              # clippy wrapper strips lib.sh's hardcoded `--cap-lints
              # allow` (which rustc treats as non-overridable) before
              # forwarding to clippy-driver.
              #
              # Each workspace member gets its own check derivation →
              # touching rio-scheduler only re-clippy's rio-scheduler +
              # its dependents, not the full workspace.
              #
              # Exposed below as checks.* and packages.clippy-* / doc-*
              # for targeted invocation.
              crateChecks = import ./nix/checks.nix (
                {
                  inherit
                    pkgs
                    rustStable
                    crateBuild
                    crateBuildCov
                    covExtractPatterns
                    noHack
                    ;
                  inherit (pkgs) lib;
                }
                # Per-member runtime inputs/env, source filesets, and nextest
                # CLI args (see nix/lib/nextest-args.nix). Keeps the
                # crate2nix test-runner wiring out of flake.nix's let block.
                // import ./nix/lib/nextest-args.nix {
                  inherit
                    pkgs
                    unfilteredRoot
                    workspaceFileset
                    manifestsFileset
                    memberFilesets
                    stubTargetFiles
                    goldenTestEnv
                    nixForTests
                    ;
                }
              );

              # rio-dashboard Svelte SPA (lint + test + svelte-check + vite build
              # in sandbox). src is scoped to rio-dashboard/ — Rust changes don't
              # invalidate this drv.
              rioDashboard = import ./nix/dashboard.nix { inherit pkgs; };

              # --------------------------------------------------------------
              # Golden conformance test fixtures
              # --------------------------------------------------------------
              #
              # Precomputed store paths for live-daemon golden tests. In hermetic
              # remote build sandboxes, `nix eval`/`nix build` fail because
              # /nix/var is read-only. Building these as nativeCheckInputs makes
              # them available in the sandbox store; env vars tell the tests
              # where they are. Tests compute narHash/narSize themselves via
              # `nix-store --dump` (legacy, no state dir needed). Locally, tests
              # fall back to `nix eval` if the env var is unset.
              goldenTestPath = pkgs.writeText "rio-golden-test" "golden test data\n";

              # CA-path fixture: fixed-output derivation with a known flat hash.
              # FODs don't need the ca-derivations experimental feature, so this
              # builds on any Nix. Its ca field (`fixed:sha256:...`) is what the
              # query_path_from_hash_part_ca test validates.
              # Hash is sha256("ca-golden-test-data") in SRI format.
              goldenCaPath = pkgs.runCommand "rio-ca-golden" {
                outputHashMode = "flat";
                outputHashAlgo = "sha256";
                outputHash = "sha256-ZofhPTz/XO99Dn3kQMcBaG3vHoMFiD9kHTTtuvf2KNM=";
              } "echo -n ca-golden-test-data > $out";

              # Golden-test env vars — shared by nextest check, mutants,
              # and golden-matrix. Adding a new golden-fixture env var
              # here propagates to all runners. (Previously duplicated at
              # each site; a new var would be easy to add to one and
              # forget the rest.)
              goldenTestEnv = {
                RIO_GOLDEN_TEST_PATH = "${goldenTestPath}";
                RIO_GOLDEN_CA_PATH = "${goldenCaPath}";
                RIO_GOLDEN_FORCE_HERMETIC = "1";
              };

              # --------------------------------------------------------------
              # Kani Rust Verifier toolchain
              # --------------------------------------------------------------
              #
              # kani-compiler, kani-driver, the always-encode-mir sysroot.
              # Built from source against a nightly pinned by kani's
              # rust-toolchain.toml (NOT rustNightly — kani-compiler links
              # rustc_private, so the date is exact). `nix build
              # .#kani-toolchain`. crateBuildKani + nix/kani.nix consume
              # this for per-member CBMC verification.
              kaniToolchain = import ./nix/kani-toolchain.nix {
                inherit pkgs;
                inherit (pkgs) lib;
              };

              # Per-member CBMC verification. Shared between
              # packages.kani-toolchain.kani-checks (manual `nix build
              # .#kani-toolchain.kani-checks.<name>`) and checks.* (where
              # members with #[kani::proof] harnesses are gated). See
              # nix/kani.nix for the pipeline and the r[verify] markers.
              kaniChecks = import ./nix/kani.nix {
                inherit pkgs kaniToolchain crateBuildKani;
              };

              # --------------------------------------------------------------
              # Formal protocol models (quint/TLC) + MBT conformance checks
              # --------------------------------------------------------------
              #
              # Imported at the flake level (not spliced inside
              # misc-checks.nix, where this set used to live) so ciMatrix
              # below can hand the quint/mbt set — together with the kani
              # members — its own GHA matrix kind (`formal`) without a
              # second eval of nix/quint.nix. checks.* still receives the
              # exact same attrset (same drvs), so the local gate is
              # unchanged.
              quintChecks =
                (import ./nix/quint.nix {
                  inherit pkgs unfilteredRoot;
                  inherit (pkgs) lib;
                  # nextest reuse-build helpers plus the prebuilt rio-lease
                  # and rio-store test binaries, for the mbt-rio-lease and
                  # mbt-rio-logservice conformance checks (they run the
                  # #[ignore]d mbt_* tests against the committed Quint
                  # models with quint on PATH — same test binaries the
                  # per-member nextest checks run, different filter and
                  # environment).
                  inherit (crateChecks) mkNextestRun mkNextestMeta;
                  rioLeaseTestBin = crateChecks.testBins.rio-lease;
                  rioStoreTestBin = crateChecks.testBins.rio-store;
                }).checks;

              # --------------------------------------------------------------
              # Non-rustc check derivations (shared by checks.* and ci aggregate)
              # --------------------------------------------------------------
              miscChecks = import ./nix/misc-checks.nix {
                inherit
                  pkgs
                  inputs
                  config
                  version
                  unfilteredRoot
                  workspaceFileset
                  manifestsFileset
                  stubTargetFiles
                  rustStable
                  rustPlatformStable
                  traceyPkg
                  subcharts
                  dockerImages
                  nodeAmi
                  docsLib
                  ;
                xtaskBin = crateBuild.memberBins.xtask;
              };

              # Container images (Linux-only — dockerTools uses Linux VM
              # namespaces for layering). Worker image includes nix + fuse3
              # + util-linux + passwd stubs; others are minimal.
              #
              # Factored into a function so the coverage pipeline can rebuild
              # images with the instrumented workspace (dockerImagesCov below).
              mkDockerImages =
                {
                  rio-crates,
                  coverage ? false,
                }:
                (import ./nix/docker.nix {
                  inherit
                    pkgs
                    rio-crates
                    coverage
                    ;
                  # Dashboard only for the non-coverage image set.
                  # nginx+static has no LLVM instrumentation and the
                  # coverage VM fixture doesn't deploy it — passing
                  # null elides the `dashboard` attr (docker.nix
                  # optionalAttrs guard) so the linkFarm doesn't
                  # reference a redundant drv.
                  rioDashboard = if coverage then null else rioDashboard;
                });
              dockerImages = mkDockerImages { inherit rio-crates; };

              # Instrumented-image set for the coverage VM tests. A named
              # binding (rather than an inline call at the vmTestsCov use
              # site) so anything else that needs the cov images reuses
              # this exact derivation set instead of instantiating a
              # second, never-cached copy via another mkDockerImages call.
              dockerImagesCov = mkDockerImages {
                rio-crates = rio-crates-cov;
                coverage = true;
              };

              # NixOS EKS node AMI builder (ADR-021, see nix/node-ami.nix).
              # Exposed below as packages.<system>.ami (and
              # packages.x86_64-linux.ami-bios for legacy BIOS boot).
              nodeAmi = import ./nix/node-ami.nix {
                inherit nixpkgs;
                inherit (inputs) self;
              };

              # Subcharts from nixhelm (FODs — hash-pinned `helm pull`).
              # Referenced by: helm-lint check (symlinked into charts/ in-sandbox),
              # packages.helm.<name> (`cargo xtask {eks deploy,dev apply}` symlink
              # from the result path into the working-tree charts/ — gitignored).
              subcharts = import ./nix/helm-charts.nix {
                inherit (inputs) nixhelm;
                inherit system;
              };

              # --------------------------------------------------------------
              # Scenario×fixture VM tests (Linux-only — need NixOS VMs + KVM)
              # --------------------------------------------------------------
              #
              #   vm-protocol-{warm,cold}-standalone — 3 VMs: opcode coverage
              #   vm-scheduling-{core,disrupt}-standalone — 5 VMs: fanout, resource-floor, cgroup
              #   vm-security-standalone — 3 VMs: HMAC, JWT, tenant-resolve
              #   vm-observability-standalone — 5 VMs: metrics, traces, logs
              #   vm-ca-cutoff-standalone — CA-on-CA cutoff propagation
              #   vm-chaos-standalone — fault injection
              #   vm-lifecycle-{core,recovery,autoscale,pool,prod-parity}-k3s
              #   vm-le-{stability,build}-k3s — 2-node k3s fixture (fragment splits)
              #   vm-security-nonpriv-k3s — privileged-hardening e2e
              #   vm-cli-k3s — rio-cli integration
              #   vm-dashboard-k3s — gRPC-Web via Gateway + nginx
              #   vm-netpol-k3s — NetworkPolicy enforcement
              #
              # mkVmTests: build the attrset for a given (workspace,
              # dockerImages, coverage) triple — see nix/tests/wiring.nix.
              # vmTests uses the normal build; vmTestsCov uses the
              # instrumented build + coverage=true so common.nix sets
              # LLVM_PROFILE_FILE and appends collectCoverage to each
              # testScript.
              vmWiring = import ./nix/tests/wiring.nix {
                inherit pkgs system inputs;
              };

              vmTests = vmWiring.mkVmTests {
                inherit rio-workspace dockerImages;
                coverage = false;
              };

              # Coverage-mode VM tests. Not in `checks` (too slow for flake
              # check) — consumed by nix/coverage.nix for the per-test +
              # merged lcov (packages.coverage.vm-*). Each per-test lcov
              # exposes its raw run at `.raw` via passthru, so
              # `.#coverage.vm-<scenario>.raw` builds just the
              # coverage-mode VM test (profraws at result/coverage/).
              vmTestsCov =
                removeAttrs
                  (vmWiring.mkVmTests {
                    rio-workspace = rio-workspace-cov;
                    dockerImages = dockerImagesCov;
                    coverage = true;
                  })
                  # prod-parity asserts readOnlyRootFilesystem=true (PSA-restricted);
                  # coverage-mode bumps PSA to privileged → assertion deterministically
                  # fails. The test is ABOUT PSA — running it under a mode that changes
                  # PSA defeats the point. No coverage delta lost: PSA rendering is
                  # Helm+YAML, no r[impl]-annotated Rust.
                  #
                  # nixos-node boots no rio-* binaries (nodeadm + kubelet only) —
                  # zero profraws, so a coverage-mode rebuild is wasted CI time
                  # and would skew after_n_builds.
                  [
                    "vm-lifecycle-prod-parity-k3s"
                    "vm-nixos-node"
                    # Lix client variant: rio-side coverage is identical to
                    # vm-protocol-warm-standalone (only the client differs,
                    # and the client isn't instrumented). Excluding keeps
                    # after_n_builds stable.
                    "vm-protocol-warm-lix-standalone"
                  ];

              # --------------------------------------------------------------
              # Coverage merge pipeline (Linux-only — depends on vmTestsCov)
              # --------------------------------------------------------------
              #
              # nix/coverage.nix merges profraws from each coverage-mode VM
              # test with the unit-test lcov, producing combined + per-test
              # lcov + genhtml report.
              #
              # stripPrefix: buildRustCrate's --remap-path-prefix maps
              # sandbox → `/`, so profraws reference `/rio-store/src/...`.
              # Strip the leading slash to get repo-relative paths that
              # genhtml can resolve against workspaceSrc.
              #
              # Coverage mode uses crateBuildCov (stripBins=false) —
              # same closure-scrub but skips strip so the __llvm_covfun /
              # __llvm_covmap sections llvm-cov needs stay intact.
              coverage = import ./nix/coverage.nix {
                # workspaceSrc is the genhtml source root — coverage.nix
                # cd's there so repo-relative lcov paths resolve.
                inherit
                  pkgs
                  rustStable
                  rio-workspace-cov
                  vmTestsCov
                  workspaceSrc
                  covExtractPatterns
                  ;
                unitCoverage = crateChecks.coverage;
              };

              # --------------------------------------------------------------
              # Multi-Nix golden conformance matrix (in checks)
              # --------------------------------------------------------------
              #
              # Runs golden_conformance against 3 daemon variants: pinned Nix,
              # nixpkgs nix_2_28, nixVersions.git (lix dropped — see
              # golden-matrix.nix for why). In `checks` as golden-<variant> —
              # 2/3 daemons substitute from cache.nixos.org so per-PR cost is
              # just the nextest invocations, and gen-matrix
              # cache-filters them when the conformance binary's closure
              # didn't change.
              goldenMatrix = import ./nix/golden-matrix.nix {
                inherit pkgs inputs system;
                inherit (crateChecks) mkNextestRun mkNextestMeta testBins;
              };

              # --------------------------------------------------------------
              # Mutation testing (dev-only — NOT in checks)
              # --------------------------------------------------------------
              inherit
                (import ./nix/mutants.nix {
                  inherit
                    pkgs
                    version
                    unfilteredRoot
                    workspaceFileset
                    manifestsFileset
                    stubTargetFiles
                    rustStable
                    rustPlatformStable
                    sysCrateEnv
                    goldenTestEnv
                    ;
                  nixPkg = nixForTests;
                })
                mutants
                mutants-smoke
                mutants-report-assert
                ;

              # ──────────────────────────────────────────────────────────
              # GitHub Actions integration
              # ──────────────────────────────────────────────────────────
              #
              # CI matrix data consumed by .github/workflows/ci.yml via the
              # top-level `flake.githubActions` alias. Keeps "what runs in
              # CI" policy in Nix — the workflow is a thin consumer that
              # evaluates this to generate matrices.
              #
              # <name>: attrsets where keys → GHA matrix entries and
              #   values → derivations to build. Add/remove entries here;
              #   the workflow picks them up automatically via
              #   `nix run .#gen-matrix` (nix/gen_matrix.py).
              #
              # Runner selection by naming convention: entries with a `vm-`
              # prefix run on `rio-ci-kvm` (bare-metal, /dev/kvm mounted);
              # everything else on `rio-ci` (spot). This keeps the flake
              # emitting simple name→drv maps without per-entry metadata.
              #
              # The formal-verification lane: every checks.* entry wired
              # through nix/quint.nix (exhaustive model checks, witnesses,
              # run pins, calibration witnesses, mbt-* conformance) or
              # nix/kani.nix (per-member CBMC proofs). intersectAttrs
              # against config.checks keeps the lane a strict subset of
              # the local gate — a quint/kani check only reaches CI's
              # formal matrix once it is actually wired into checks.*, and
              # the matrix values ARE the checks.* derivations. ~160
              # entries today and growing by tens per campaign, each one a
              # JVM+TLC or CBMC run: that is why it is its own matrix kind
              # (sharded by gen_matrix.py) instead of part of the `checks`
              # kind, whose catch-all `misc` cluster it used to starve to
              # death on a single runner.
              formalChecks = builtins.intersectAttrs (quintChecks // kaniChecks) config.checks;

              ciMatrix = {
                # Rust + static checks. Derived from config.checks — same
                # P0525 rationale as the old .#ci aggregate: a manual list
                # had drifted to miss executor-seed-layer-parity,
                # node-ami-eval, and codecov-matrix-sync (the very check
                # P0525 added that aggregate for, now retired). Subtract
                # what other matrices already cover (fuzz, vm-test) plus
                # `cov-smoke` (needs KVM; the checks matrix runs on
                # non-KVM rio-ci). cov-smoke is NOT re-added to any other
                # matrix: the coverage-infra assertion it carries lives
                # inside `perTestLcov.${smokeScenario}` (nix/coverage.nix
                # mkPerTestLcov), which IS the
                # `ciMatrix.coverage.vm-protocol-warm-standalone` entry —
                # already built on a KVM runner for codecov upload. A
                # second entry here would rebuild the same ~5-10min
                # instrumented VM scenario on a parallel runner whenever
                # the lcov drv is uncached (~73% of commits). cov-smoke
                # stays in `checks.*` only, for local
                # `nix-fast-build .#checks` (single host, shared store —
                # no double-build). attrNames forces only key names, not
                # values — codecov-matrix-sync's value reads
                # ciMatrix.coverage, but its KEY is a literal, so no
                # recursion.
                checks = builtins.removeAttrs config.checks (
                  builtins.attrNames fuzz.runs
                  ++ builtins.attrNames vmTests
                  ++ builtins.attrNames formalChecks
                  ++ [ "cov-smoke" ]
                );
                # Formal-verification checks (quint/TLC, MBT, kani). Their
                # build IS the verification run, so the kind is fanned out
                # into balanced shards by gen_matrix.py instead of being
                # one cluster. See formalChecks above.
                formal = formalChecks;
                # 2min fuzz runs, one matrix entry per target. Keys are
                # fuzz-<target> (from nix/fuzz.nix). On a cold cache each
                # entry rebuilds the shared fuzz-build derivation, but
                # spot CPU is cheap and the cache fills after first green.
                fuzz = fuzz.runs;
                # Normal VM tests. Keys: vm-<scenario>-<fixture>. Per-test
                # red/green signal in the GHA UI.
                vm-test = vmTests;
                # lcov-producing jobs, one per Codecov flag. `unit-*`
                # run on spot (one per workspace member, so a single-
                # crate edit only rebuilds that one); `vm-*` need KVM
                # (instrumented VM tests → profraw → lcov). Workflow
                # picks runs-on by prefix. Each entry is file-shaped
                # (the lcov is $out directly) to match perTestLcov.
                coverage =
                  pkgs.lib.mapAttrs' (
                    n: d:
                    pkgs.lib.nameValuePair "unit-${n}" (
                      pkgs.runCommand "rio-cov-unit-${n}" { } "ln -s ${d}/lcov.info $out"
                    )
                  ) crateChecks.covLcovs
                  // coverage.perTestLcov;

              };
            in
            {
              # Free-form, not enumerated by `nix flake show`, not checked
              # by `nix flake check`. Things that tooling reaches into by
              # path (CI, xtask) or that are nested debug maps. Everything
              # that was a flat re-export of an internal let-binding with
              # zero external callers is gone — the let-bindings stay, the
              # `.#<name>` alias does not.

              # CI matrix data → custom perSystem option (declared at the
              # mkFlake top), surfaced as `flake.githubActions` via
              # withSystem. The let-binding stays so coverage.passthru and
              # codecov-matrix-sync can reference it without `config.`.
              inherit ciMatrix;

              # Import rust-overlay
              _module.args.pkgs = import nixpkgs {
                inherit system;
                overlays = [ inputs.rust-overlay.overlays.default ];
                # codecov-cli (baked into packages.coverage-upload via
                # nix/coverage-upload.py) transitively depends on
                # test-results-parser, which is FSL-1.1 —
                # the test-analytics path we never call. Everything
                # else stays unfree-free; if a second name appears
                # here, reconsider.
                config.allowUnfreePredicate = p: nixpkgs.lib.getName p == "test-results-parser";
              };

              # Configure treefmt
              treefmt.config = {
                flakeCheck = false;
                projectRootFile = "flake.nix";

                programs = {
                  nixfmt.enable = true;

                  # Rust formatting. Uses the nightly toolchain so the
                  # default (nightly) devshell doesn't pull in a second
                  # full stable toolchain just for rustfmt. CI/dev parity
                  # is preserved because both run THIS treefmtEval —
                  # `nix develop .#stable -c cargo fmt` (raw stable
                  # rustfmt) may diverge; use `treefmt` instead.
                  rustfmt = {
                    enable = true;
                    package = rustNightly;
                  };

                  # TOML formatting
                  taplo.enable = true;

                  # Typst formatting
                  typstyle.enable = true;
                };
                settings.global.excludes = [
                  # cargo-hakari owns this file's format. taplo and hakari
                  # disagree on array layout → `hakari generate` sees drift
                  # after every treefmt pass, breaking regen idempotency.
                  "workspace-hack/Cargo.toml"
                ];
              };

              # Configure git hooks
              pre-commit = {
                check.enable = true;

                settings.excludes = [
                  # Fuzz corpus seeds are exact binary/text inputs; trailing
                  # newlines would change what the fuzzer sees.
                  "^fuzz/.+/corpus/"
                  # Vendored patch files must stay byte-exact: unified-diff
                  # context lines for blank lines are a single space, which
                  # trim-trailing-whitespace would strip, corrupting the
                  # patch.
                  "^nix/patches/.+\\.patch$"
                ];

                settings.hooks = {
                  treefmt.enable = true;
                  convco.enable = true;
                  ripsecrets.enable = true;
                  check-added-large-files = {
                    enable = true;
                    # Cargo.json is the crate2nix pre-resolved dependency
                    # graph (~500 KB, grows with dep count). Treated like
                    # Cargo.lock: generated + checked in, reviewed on
                    # regeneration. See nix/crate2nix.nix.
                    excludes = [ "^Cargo\\.json$" ];
                  };
                  check-merge-conflicts.enable = true;
                  end-of-file-fixer.enable = true;
                  trim-trailing-whitespace.enable = true;
                  deadnix.enable = true;
                  nil.enable = true;
                  statix.enable = true;

                  # No kubeconform hook: it fetches ~300MB of schemas from
                  # raw.githubusercontent.com at runtime, which fails in the
                  # hermetic remote build sandbox (config.checks.pre-commit
                  # runs all hooks there). Run it interactively if needed:
                  #   helm template rio infra/helm/rio-build --set global.image.tag=x \
                  #     | kubeconform -strict -skip CustomResourceDefinition,Certificate,...
                  # The helm-lint flake check above catches template syntax
                  # errors without network.
                }
                # Custom writeShellScript hooks (check-mutants-marker,
                # sqlx-prepare-check, crate2nix-check, hakari-check).
                // import ./nix/pre-commit-hooks.nix { inherit pkgs crate2nixCli; };
              };

              # --------------------------------------------------------------
              # Dev shells (extracted to nix/devshell.nix)
              # --------------------------------------------------------------
              devShells = import ./nix/devshell.nix {
                inherit
                  pkgs
                  rustStable
                  rustNightly
                  sysCrateEnv
                  traceyPkg
                  crate2nixCli
                  docsLib
                  shiroaPkg
                  kaniToolchain
                  ;
                treefmtWrapper = config.treefmt.build.wrapper;
                preCommitInstall = config.pre-commit.installationScript;
                # Hermetically packaged quint-llm-kit MCP servers (KB
                # search + LSP bridge) for the project-scoped .mcp.json;
                # dev-shell-only, never referenced by checks.*.
                quintMcp = pkgs.callPackage ./nix/quint-mcp.nix { };
              };

              # `nix run .#docs` — serve the post-processed HTML tree via
              # miniserve. The `bin` output of docsLib.docs holds the
              # wrapper; the `out` output is the static tree (what
              # `nix build .#docs` symlinks at `result`).
              apps.docs = {
                type = "app";
                program = "${docsLib.docs.bin}/bin/rio-docs";
              };

              # --------------------------------------------------------------
              # Packages — minimal set of deployable / top-level outputs.
              # --------------------------------------------------------------
              # Per-member check derivations live in `checks` (granular,
              # for nix-fast-build streaming). Debug/manual targets are
              # passthru on packages.{ci,coverage,helm,dockerImages,mutants}
              # (reachable by attr path, not enumerated by `nix flake show`).
              packages = {
                default = rio-workspace;
                workspace = rio-workspace;
                dashboard = rioDashboard;
                # nix/pins.toml rendered as *.auto.tfvars.json. snake_case
                # keys in pins.toml → direct toJSON passthrough, no mapping
                # layer. Regenerate the committed copy:
                #   cargo xtask regen tfvars
                tfvars = pkgs.writeText "generated.auto.tfvars.json" (builtins.toJSON (import ./nix/pins.nix));
                # Typst design book outputs.
                inherit (docsLib) docs docs-pdf;
                # Kani Rust Verifier toolchain — manual build (`nix build
                # .#kani-toolchain`), not a check. Heavy: pulls a second
                # nightly with rustc-dev (~2 GB) and rebuilds std with
                # always-encode-mir. Other kaniToolchain exports
                # (kani-rustc, kani-sysroot, kaniNightly,
                # kani-driver-wrapped) hang off this attr as passthru —
                # the project deliberately has no legacyPackages bridge.
                kani-toolchain = kaniToolchain.kani.overrideAttrs (old: {
                  passthru = (old.passthru or { }) // {
                    inherit (kaniToolchain)
                      kani-driver-wrapped
                      kani-rustc
                      kani-sysroot
                      kaniNightly
                      ;
                    # Per-member kani-built outputs (.rlib + goto-C
                    # sidecars). `nix build .#kani-toolchain.crates.rio-lease`.
                    # passthru, not a checks.* member: the kani tree is
                    # ~500 drvs/member and a manual verification target.
                    crates = crateBuildKani.members;
                    # Per-member CBMC verification — reads the goto-C
                    # sidecars from `crates.<name>` and runs the
                    # goto-cc/goto-instrument/cbmc pipeline per harness.
                    # `nix build .#kani-toolchain.kani-checks.kani-rio-lease`.
                    # Members with #[kani::proof] harnesses are also gated
                    # in checks.* (alongside cov-smoke / mutants-smoke) —
                    # this passthru is the manual-target alias. See
                    # nix/kani.nix for the pipeline + r[verify] markers.
                    kani-checks = kaniChecks;
                  };
                });
              }
              # Container images. `.#dockerImages` is the linkFarm xtask
              # `eks push` walks; individual images at `.#dockerImages.<name>`
              # via passthru (gateway, scheduler, store, builder, controller,
              # bootstrap, dashboard, executorSeed, vmTestSeed). The flat
              # `docker-<name>` aliases are gone — pure re-exports, no callers.
              # overrideAttrs (not `drv // { passthru = … }`) so mkDerivation's
              # extendDerivation promotes passthru attrs to top-level — that's
              # what makes `.#dockerImages.executorSeed` resolve.
              // {
                # Helm charts from nixhelm (unpacked dirs). xtask
                # `.#helm.<name>` + the README symlink workflow consume
                # individual charts via passthru; the linkFarm aggregate
                # is `nix build .#helm` for all four at once.
                helm =
                  (pkgs.linkFarm "rio-helm-charts" (
                    pkgs.lib.mapAttrsToList (name: path: { inherit name path; }) subcharts
                  )).overrideAttrs
                    (old: {
                      passthru = (old.passthru or { }) // subcharts;
                    });

                # Parallel evaluator behind `packages.gen-matrix` (and
                # nix-fast-build). Re-exported so local experiments use
                # the same pinned binary whose JSONL schema gen_matrix.py
                # depends on.
                inherit (pkgs) nix-eval-jobs;

                # CI matrix generator (.github/workflows/ci.yml gen-matrix
                # job). writePython3Bin runs flake8 at build time, so a
                # lint error fails the package build before the script
                # ever runs in CI. nix-eval-jobs is baked in via
                # replaceVars so the workflow needs no separate
                # `nix build .#nix-eval-jobs` step.
                gen-matrix =
                  pkgs.writers.writePython3Bin "gen-matrix"
                    {
                      # Comments tracking the 79-col limit read worse
                      # than the occasional long line.
                      flakeIgnore = [ "E501" ];
                    }
                    (
                      pkgs.replaceVars ./nix/gen_matrix.py {
                        nix_eval_jobs = "${pkgs.nix-eval-jobs}/bin/nix-eval-jobs";
                      }
                    );

                # Codecov uploader. The {name: outPath} map comes from
                # gen-matrix via $COVERAGE_PATHS (NOT baked in — baking
                # would couple this derivation's eval to every coverage
                # target evaluating cleanly, defeating best-effort upload).
                # Only the codecovcli store path is substituted.
                # writePython3Bin's flake8 runs on the post-substitution
                # body; the placeholder is inside a string literal so it
                # passes pre-substitution too.
                coverage-upload =
                  pkgs.writers.writePython3Bin "coverage-upload"
                    {
                      # The substituted store path overflows 79 cols.
                      flakeIgnore = [ "E501" ];
                    }
                    (
                      pkgs.replaceVars ./nix/coverage-upload.py {
                        codecovcli = "${pkgs.codecov-cli}/bin/codecovcli";
                      }
                    );

                dockerImages =
                  (pkgs.linkFarm "rio-docker-images" (
                    pkgs.lib.mapAttrsToList
                      (name: drv: {
                        name = "${name}.tar.zst";
                        path = drv;
                      })
                      (
                        # push.rs walks this linkFarm and runs `skopeo copy
                        # docker-archive:` on every entry. Structural filter:
                        # only attrs produced by dockerTools.buildLayeredImage
                        # (which sets passthru.imageTag) are pushable. This
                        # excludes oci-archive seeds (executorSeed/vmTestSeed →
                        # AMI/k3s, not ECR), parity checks, and non-image
                        # passthrus exported for misc-checks (bootstrapScript,
                        # dashboardReadonlyMethods, dashboardNginxConf). A
                        # removeAttrs denylist here previously leaked
                        # dashboardNginxConf → ECR rejected the camelCase repo
                        # name; the imageTag gate makes that class of leak
                        # unrepresentable.
                        pkgs.lib.filterAttrs (_: v: pkgs.lib.isDerivation v && v ? imageTag) dockerImages
                      )
                  )).overrideAttrs
                    (old: {
                      # Full nix/docker.nix attrset (incl. the non-ECR
                      # oci-archive seeds and misc-checks helpers that the
                      # linkFarm filter above excludes).
                      passthru = (old.passthru or { }) // dockerImages;
                    });

                # ──────────────────────────────────────────────────────────
                # NixOS EKS node AMI (ADR-021). Replaces bottlerocket@latest
                # for builder/fetcher Karpenter NodePools.
                #
                #   nix build .#ami                       # native to <system>
                #   nix build .#packages.aarch64-linux.ami  # cross-target from x86
                #   cargo xtask k8s -p eks ami push --arch x86_64
                #
                # Output dir contains the disk image plus `nix-support/
                # image-info.json` (label, system, file, boot_mode) which
                # `xtask ami push` reads for coldsnap upload + register-image.
                #
                # Keyed off the eval `system` (not flat per-arch attrs): the
                # derivations are native to nodeSystem regardless of where
                # they're exposed, so the path tells the truth. xtask asks
                # for `.#packages.<target>-linux.ami` explicitly.
                # ──────────────────────────────────────────────────────────
                ami = nodeAmi system { };
              }
              // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
                # I-205: x86 .metal SKUs are legacy-bios ONLY (zero support
                # UEFI per `aws ec2 describe-instance-types`). arm64 .metal
                # is UEFI, so this attr is meaningless there — see nodeAmi
                # comment for the EC2NodeClass split.
                ami-bios = nodeAmi system { efi = false; };
              }
              // {
                # CRD YAML for the crds-drift check. runCommand invokes
                # the crdgen binary, which writes one `<crd-name>.yaml`
                # per CRD into $out; misc-checks.nix:crds-drift `diff -r`s
                # against infra/helm/crds/. `cargo xtask regen crds` does
                # NOT use this — it runs `cargo run --bin crdgen` directly
                # to avoid a nix build in the dev loop. Same binary, same
                # bytes.
                #
                # Why not auto-regenerate in CI: the committed YAML is
                # what operators `kubectl apply`. Regenerating on every
                # commit means a CRD schema change silently updates the
                # deployed file — we want that change REVIEWED (it may
                # be backward-incompatible).
                #
                # crdgen lives in rio-crds (the lightest kube-adjacent
                # leaf), not the whole-workspace binary set, so this
                # output and the crds-drift check cache-hit on any
                # non-CRD edit.
                crds = pkgs.runCommand "rio-crds" { } ''
                  mkdir -p $out
                  ${crateBuild.memberBins.rio-crds}/bin/crdgen $out
                '';

                # ──────────────────────────────────────────────────────────
                # CI aggregate (manual — `nix build .#ci`)
                # ──────────────────────────────────────────────────────────
                #
                # Everything the GHA pipeline (.github/workflows/ci.yml)
                # builds, as one local target: the union of the five
                # ciMatrix kinds. Built FROM ciMatrix — the same value
                # gen-matrix evaluates — so it cannot drift from what CI
                # runs (the drift that retired the old hand-curated .#ci,
                # P0525). Not covered: CI steps that aren't nix builds
                # (gen-matrix eval, codecov upload, docs.yml Pages
                # deploy). cov-smoke is absent here exactly as it is in
                # CI: its assertion lives inside the coverage entry for
                # the smoke scenario, which IS built.
                #
                # Needs KVM (vm-test and coverage vm-* constituents). On
                # a cold cache this is the entire CI workload on one
                # host; anything already in the binary cache substitutes.
                # Pass --keep-going to surface every failure like CI's
                # per-cluster `nix build --keep-going` does.
                #
                #   nix build .#ci                        # the full CI build set
                #   nix build .#ci.vm-test                # one matrix kind
                #   nix build .#ci.checks.clippy-rio-nix  # one entry
                #
                # $out symlinks one dir per kind, so colliding entry
                # names stay distinct (vm-test/vm-X vs coverage/vm-X).
                # Same linkFarm+overrideAttrs passthru idiom as helm /
                # dockerImages above.
                ci =
                  let
                    aggregate =
                      farmName: entries:
                      (pkgs.linkFarm farmName (pkgs.lib.mapAttrsToList (name: path: { inherit name path; }) entries))
                      .overrideAttrs
                        (old: {
                          passthru = (old.passthru or { }) // entries;
                        });
                  in
                  aggregate "rio-ci" (pkgs.lib.mapAttrs (kind: aggregate "rio-ci-${kind}") ciMatrix);

                # ──────────────────────────────────────────────────────────
                # Coverage (manual — NOT a check)
                # ──────────────────────────────────────────────────────────
                #
                #   coverage      — unit + VM merged (~25min, needs KVM).
                #                   result/lcov.info, result/html/,
                #                   result/per-test/.
                #   coverage.unit — lcov -a over per-crate unit lcovs (~5min)
                #   coverage.vm   — lcov -a over per-scenario VM lcovs
                #   coverage.html — html/ subdir only
                #   coverage.{unit-<crate>,vm-<scenario>} — per-entry lcov,
                #                   same set as ciMatrix.coverage
                coverage = coverage.full.overrideAttrs (old: {
                  passthru =
                    (old.passthru or { })
                    # Per-entry lcovs (unit-<crate> | vm-<scenario>) — same
                    # set as ciMatrix.coverage.
                    // ciMatrix.coverage
                    // {
                      # The two mid-tier aggregates and the html-only view.
                      # `nix build .#coverage.unit` etc.
                      unit = crateChecks.coverage;
                      vm = coverage.vmLcov;
                      html = pkgs.runCommand "rio-coverage-html" { } ''
                        ln -s ${coverage.full}/html $out
                      '';
                    };
                });

                # Mutation-testing sweep. Dev-only (`nix build .#mutants`);
                # multi-hour, not gated. report-assert is the cheap
                # threshold gate that depends on the full sweep output.
                mutants = mutants.overrideAttrs (old: {
                  passthru = (old.passthru or { }) // {
                    report-assert = mutants-report-assert;
                  };
                });
              };

              # --------------------------------------------------------------
              # Checks — flat granular derivations. The CI gate is
              # `nix-fast-build --flake .#checks.<system>` which streams
              # per-attr eval+build via nix-eval-jobs.
              # --------------------------------------------------------------
              checks =
                # Per-member rustc-driven checks. Each attr is one
                # derivation depending on that member's per-crate src
                # fileset (nix/crate2nix.nix) — editing rio-cli leaves
                # checks.clippy-rio-scheduler cached.
                prefixed "clippy-" crateChecks.clippy
                // prefixed "clippy-test-" crateChecks.clippyTest
                // prefixed "doc-" crateChecks.doc
                // prefixed "nextest-" crateChecks.nextestRuns
                # Wire-protocol conformance against 3 daemon variants.
                // prefixed "golden-" goldenMatrix.runs
                // {
                  dashboard = rioDashboard;
                }
                # Workspace-level policy checks (deny, helm-lint,
                # tracey-validate, crds-drift, tfvars-fresh, …).
                // miscChecks
                # Formal protocol-model checks: quint/TLC per-regime
                # proofs, witness/run/calibration checks, and the mbt-*
                # conformance runs (nix/quint.nix). Imported at the flake
                # level so ciMatrix.formal can reuse the same attrset.
                // quintChecks
                # Design-book builds (`docs-pdf`, `docs-html` + smokes).
                // docsLib.checks
                # 2min fuzz runs (Linux-only). Compiled binaries shared
                # across targets via rio-{nix,store}-fuzz-build.
                // fuzz.runs
                # Per-phase milestone VM tests (Linux-only, need KVM).
                # Debug interactively:
                #   nix build .#checks.x86_64-linux.vm-protocol-warm-standalone.driverInteractive
                #   ./result/bin/nixos-test-driver
                // vmTests
                // {
                  # cov-smoke: one coverage-mode VM scenario, asserts
                  # profraw→lcov pipeline works. ~5min. Catches "coverage
                  # infra broken" at merge-gate instead of 118 commits
                  # later via backgrounded `.#coverage`. Needs KVM —
                  # `nix flake check` on a non-KVM host will fail this;
                  # use nix-fast-build's --skip-cached or build the
                  # checks subset that excludes it. The actual gate
                  # assertion lives inside `perTestLcov.${smokeScenario}`
                  # (which this depends on); see nix/coverage.nix and the
                  # ciMatrix comment above. checks.*-only — not in any
                  # ciMatrix because the GHA `coverage` job already builds
                  # the self-asserting lcov drv on a KVM runner.
                  cov-smoke = coverage.smoke;
                  # mutants-smoke: bounded cargo-mutants run on
                  # rio-auth/src/jwt.rs (~5min cold). Proves the
                  # mutate→rebuild→retest→classify pipeline works and
                  # catches ≥1 mutant. The full sweep is .#mutants
                  # (dev-only, hours).
                  inherit mutants-smoke;
                  # kani-rio-lease: CBMC verification of rio-lease's
                  # decide_pure() against its #[kani::ensures] contracts
                  # (proof_for_contract, exhaustive over the input domain).
                  # Promoted from packages.kani-toolchain.kani-checks once
                  # the harness landed — a vacuous 0-harness check would
                  # have diluted the gate. The formal protocol model
                  # (quint-leader-election, in quintChecks) verifies the
                  # protocol-level safety property; this verifies the
                  # per-decision logic that complements it (richer case
                  # structure than the model's action partition — not a
                  # formal refinement). r[verify] markers are at the
                  # wiring point in nix/kani.nix.
                  #
                  # kani-rio-store: same pipeline over the log-chunk
                  # decision kernels (rio-store/src/logs/kernel.rs) — the
                  # chunk-interval arithmetic, the read-path overlap
                  # dedup, the accept verdict, and the completeness fold.
                  # The formal model (quint-log-service-*, in quintChecks)
                  # verifies the protocol over a bounded line domain;
                  # these harnesses verify the per-decision arithmetic
                  # over the full u64 domain.
                  #
                  # kani-rio-retry-kernel: same pipeline over the
                  # scheduler's retry/poison decision kernels
                  # (rio-retry-kernel) — the decide()/classify()/placeable()
                  # contracts, the legacy-seed floor, the fold's
                  # fleet-exhaust arm, and the proof-time bounded-set
                  # representation's set-semantics harness. Gated once the
                  # cfg(kani) IdSet/BoundedIdSet representation swap
                  # brought the harnesses inside the gate budget; the
                  # formal model (quint-retry-policy-*, in quintChecks)
                  # checks the protocol, these harnesses the decision
                  # arithmetic over bounded arbitrary inputs.
                  #
                  # This inherit is also what routes a kani proof into the
                  # CI `formal` matrix lane: ciMatrix.formal intersects
                  # (quintChecks // kaniChecks) with config.checks, so a
                  # NEW kani-* check needs its one-line promotion here to
                  # be sharded into that lane (quint checks need no extra
                  # step — the whole quintChecks set is merged into
                  # checks.* above, so they flow through automatically).
                  inherit (kaniChecks) kani-rio-lease kani-rio-store kani-rio-retry-kernel;
                  # Regression: per-node profraw extract must not drop
                  # filename-colliding profraws across multi-worker nodes.
                  # No KVM needed (synthetic tarballs).
                  cov-extract-nocollide = coverage.extractNoCollide;
                  # gen-matrix's embedded unit tests, run against the
                  # flake8-checked, replaceVars-substituted script (the
                  # exact bytes CI executes). A logic regression in the
                  # matrix generator is caught by the local checks gate
                  # instead of by a malformed CI run.
                  gen-matrix-selftest =
                    pkgs.runCommand "gen-matrix-selftest" { nativeBuildInputs = [ config.packages.gen-matrix ]; }
                      ''
                        gen-matrix --self-test
                        touch $out
                      '';
                  codecov-matrix-sync =
                    let
                      expected = builtins.length (builtins.attrNames ciMatrix.coverage);
                      declared = pkgs.lib.toInt (
                        builtins.head (
                          builtins.match ".*after_n_builds: ([0-9]+).*" (builtins.readFile ./.github/codecov.yml)
                        )
                      );
                    in
                    assert pkgs.lib.assertMsg (expected == declared) ''
                      .github/codecov.yml after_n_builds=${toString declared} but coverage matrix has ${toString expected} entries.
                      Update .github/codecov.yml → codecov.notify.after_n_builds to ${toString expected}.
                    '';
                    # Named (not pkgs.emptyFile) so `nix log` /
                    # nix-fast-build attribution shows which check this is
                    # — eval-time asserts are invisible otherwise once
                    # they pass.
                    pkgs.runCommand "rio-codecov-matrix-sync" { } "touch $out";
                };

              # Formatter for 'nix fmt'
              formatter = config.treefmt.build.wrapper;
            };
        };
      }
    );
}

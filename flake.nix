{
  description = "rio-build - Nix build orchestration";

  inputs = {
    nix = {
      url = "github:NixOS/Nix/2.33.3";
      inputs = {
        flake-parts.follows = "flake-parts";
        git-hooks-nix.follows = "git-hooks-nix";
      };
    };

    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    git-hooks-nix = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      flake-parts,
      nixpkgs,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      imports = [
        inputs.treefmt-nix.flakeModule
        inputs.git-hooks-nix.flakeModule
      ];

      # NixOS modules for deploying rio services. These are consumed by the
      # phase-milestone VM tests (nix/tests/phase*.nix) and can be reused
      # for real deployments. Each module reads `services.rio.package` for
      # binaries, so callers must set that to a workspace build.
      flake.nixosModules = {
        store = ./nix/modules/store.nix;
        scheduler = ./nix/modules/scheduler.nix;
        gateway = ./nix/modules/gateway.nix;
        worker = ./nix/modules/worker.nix;
      };

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

          # Crane for CI: stable toolchain, reproducible.
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustStable;

          # Crane for fuzz builds + default dev shell: nightly.
          craneLibNightly = (inputs.crane.mkLib pkgs).overrideToolchain rustNightly;

          # Source root for filesets
          unfilteredRoot = ./.;

          # Common arguments for all crane builds
          commonArgs = {
            src = pkgs.lib.fileset.toSource {
              root = unfilteredRoot;
              fileset = pkgs.lib.fileset.unions [
                # All standard Cargo sources (Cargo.toml, Cargo.lock, .rs files, etc.)
                (craneLib.fileset.commonCargoSources unfilteredRoot)
                # Proto files for gRPC code generation
                (pkgs.lib.fileset.fileFilter (file: file.hasExt "proto") unfilteredRoot)
                # SQL migrations (embedded at compile time via sqlx::migrate!)
                ./migrations
                # cargo-deny config (license + advisory policy)
                ./deny.toml
              ];
            };
            strictDeps = true;

            pname = "rio";
            inherit version;

            nativeBuildInputs = with pkgs; [
              pkg-config
              protobuf
              cmake
            ];

            buildInputs =
              with pkgs;
              [
                openssl
                llvmPackages.libclang.lib
                fuse3
              ]
              ++ lib.optionals stdenv.isDarwin [
                darwin.apple_sdk.frameworks.Security
                libiconv
              ];

            propagatedBuildInputs = with pkgs; [
              nix
            ];

            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${rustStable}/lib/rustlib/src/rust/library";
            PROTOC = "${pkgs.protobuf}/bin/protoc";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            # Where rio-test-support finds initdb/postgres (falls back to PATH).
            PG_BIN = "${pkgs.postgresql_18}/bin";
          };

          # Build dependencies only (for caching)
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          # Build the workspace
          rio-workspace = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              doCheck = false; # We'll run checks separately
            }
          );

          # --------------------------------------------------------------
          # Coverage-instrumented workspace build
          # --------------------------------------------------------------
          #
          # RUSTFLAGS=-Cinstrument-coverage injects LLVM profile-generate
          # instrumentation. Binaries write .profraw files (via atexit)
          # to LLVM_PROFILE_FILE. VM tests run these binaries, stop them
          # gracefully (C1-C4 graceful shutdown), collect profraws,
          # and nix/coverage.nix merges them with unit-test lcov.
          #
          # Distinct pname → distinct store path, builds in parallel
          # with the non-instrumented workspace. RUSTFLAGS invalidates
          # cargoArtifacts so a separate deps cache is used.
          covArgs = commonArgs // {
            # -Z coverage-options=branch enables branch coverage (if/
            # else, match arms). Default -C instrument-coverage only
            # emits block (line/region) data. -Z is nightly-gated;
            # RUSTC_BOOTSTRAP=1 unlocks it on the stable toolchain
            # (stable rustc binaries contain the code, just feature-
            # gated). This is coverage-only — the non-instrumented
            # workspace build stays pure stable for CI parity.
            RUSTFLAGS = "-C instrument-coverage -Z coverage-options=branch";
            RUSTC_BOOTSTRAP = "1";
            pname = "rio-cov";
            # Build scripts and proc-macros are ALSO instrumented; when
            # they run at compile time (tonic-prost-build, sqlx macros),
            # they try to write profraws to CWD — RO in the sandbox →
            # "LLVM Profile Error: Read-only file system" noise. Discard
            # build-time profraws. At RUNTIME, the VM's systemd env sets
            # LLVM_PROFILE_FILE=/var/lib/rio/cov/... which overrides this.
            LLVM_PROFILE_FILE = "/dev/null";
          };
          cargoArtifactsCov = craneLib.buildDepsOnly covArgs;
          rio-workspace-cov = craneLib.buildPackage (
            covArgs
            // {
              cargoArtifacts = cargoArtifactsCov;
              doCheck = false;
            }
          );

          # --------------------------------------------------------------
          # Fuzz build pipeline (extracted to nix/fuzz.nix)
          # --------------------------------------------------------------
          #
          # Produces:
          #   fuzz.builds.rio-{nix,store}-fuzz-build  — compiled target binaries
          #   fuzz.smoke    — 30s PR-tier checks, keyed fuzz-smoke-<target>
          #   fuzz.nightly  — 10min nightly runs, keyed fuzz-nightly-<target>
          fuzz = import ./nix/fuzz.nix {
            inherit
              pkgs
              craneLib
              craneLibNightly
              unfilteredRoot
              ;
          };

          # Spec-coverage CLI (CI: `tracey query validate`).
          # Dashboard is stubbed — see nix/tracey.nix for why.
          traceyPkg = import ./nix/tracey.nix { inherit craneLib pkgs; };

          # --------------------------------------------------------------
          # Golden conformance test fixtures
          # --------------------------------------------------------------
          #
          # Precomputed store paths for live-daemon golden tests. In hermetic
          # sandboxes (nixbuild.net), `nix eval`/`nix build` fail because
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

          # --------------------------------------------------------------
          # Check derivations (extracted so checks and aggregates share them)
          # --------------------------------------------------------------
          cargoChecks = {
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );
            # License + advisory audit. Policy: deny GPL-3.0 (project is
            # MIT/Apache), fail on RustSec advisories with a curated ignore
            # list in deny.toml. Runs as part of `nix flake check` and all
            # ci-* aggregates.
            #
            # craneLib.cargoDeny vendors the advisory DB at build time, so
            # the check is hermetic (no network). Bump the flake inputs to
            # pick up new advisories.
            deny = craneLib.cargoDeny (commonArgs // { inherit cargoArtifacts; });
            nextest = craneLib.cargoNextest (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoNextestExtraArgs = "--no-tests=warn";
                nativeCheckInputs = with pkgs; [
                  inputs.nix.packages.${system}.default
                  openssh
                  postgresql_18
                ];
                # Golden fixture paths. String interpolation puts them in
                # the build closure so they're in the sandbox store; tests
                # read the env var to find them (see golden/daemon.rs).
                RIO_GOLDEN_TEST_PATH = "${goldenTestPath}";
                RIO_GOLDEN_CA_PATH = "${goldenCaPath}";
              }
            );
            doc = craneLib.cargoDoc (
              commonArgs
              // {
                inherit cargoArtifacts;
                # Deny rustdoc warnings (private_intra_doc_links, broken_intra_doc_links,
                # unclosed HTML tags) — catches doc regressions in CI.
                RUSTDOCFLAGS = "-Dwarnings";
              }
            );
            # Spec-coverage validation: fails on broken r[...] references,
            # duplicate requirement IDs, or unparseable include files. Does
            # NOT fail on uncovered/untested — those are informational.
            #
            # Uses cleanSource (not cleanCargoSource) because tracey needs
            # docs/**/*.md and .config/tracey/config.styx, which crane's
            # source filter drops. tracey's daemon writes .tracey/daemon.sock
            # under the working dir, so we cp to a writable tmpdir first.
            #
            # v1.3.0 bug: `tracey query validate` always exits 0 even when it
            # reports errors. Workaround: grep for "0 total error(s)". Remove
            # the grep once upstream fixes the exit code.
            tracey-validate =
              pkgs.runCommand "rio-tracey-validate"
                {
                  src = pkgs.lib.cleanSource ./.;
                  nativeBuildInputs = [ traceyPkg ];
                }
                ''
                  cp -r $src $TMPDIR/work
                  chmod -R +w $TMPDIR/work
                  cd $TMPDIR/work
                  # Remove any stale .tracey/ state that cleanSource may have
                  # included (committed .gitignore excludes it, but belt+braces).
                  # HOME is set so tracey's daemon-state dir (if it writes one
                  # outside the working dir) goes somewhere writable.
                  rm -rf .tracey/
                  export HOME=$TMPDIR
                  tracey query validate 2>&1 | tee validate.log
                  grep -q '0 total error(s)' validate.log || {
                    echo "FAIL: tracey validate found errors (see above)"
                    exit 1
                  }
                  touch $out
                '';
            # Coverage via `cargo llvm-cov nextest` (NOT `cargo llvm-cov test`).
            #
            # Root cause of the previous flakiness: `cargo test` runs all
            # tests in a single process with --test-threads=NCPU. Scheduler
            # tests share ONE ephemeral PG server via `static PG: OnceLock`
            # (rio-test-support/src/pg.rs). Under ~56-way parallelism +
            # llvm-cov instrumentation overhead on nixbuild.net, that PG
            # server saturates on query execution (not connection count —
            # max_connections=500 is plenty). `test_scheduler_cache_check_
            # skips_build` is uniquely vulnerable: it does a gRPC
            # FindMissingPaths call INSIDE the actor's serial event loop
            # with a 30s timeout (merge.rs:412). The in-process store's
            # handler does a PG query → stalls → 30s elapses → timeout →
            # cache check returns empty → derivation stays Created →
            # assertion expecting Completed fails at ~35s total.
            #
            # nextest's per-test-process model eliminates this structurally:
            # each test gets its OWN PG server (own `PG` static). This is
            # why the `nextest` check above never flaked. cargoNextest with
            # `withLlvmCov = true` runs `cargo llvm-cov nextest` under the
            # hood — same isolation, plus coverage.
            coverage = craneLib.cargoNextest (
              commonArgs
              // {
                inherit cargoArtifacts;
                withLlvmCov = true;
                cargoNextestExtraArgs = "--no-tests=warn";
                # crane's cargoNextest does `mkdir -p $out` in buildPhase, so
                # $out is a directory. The old cargoLlvmCov wrote lcov to $out
                # directly (a file). lcov consumers downstream (lcov --summary
                # in the dev shell) need updating to $out/lcov.info.
                #
                # --branch: enable branch coverage (-Z coverage-options=
                # branch under the hood, nightly-gated). RUSTC_BOOTSTRAP
                # unlocks on stable. Matches rio-workspace-cov so unit +
                # VM lcovs merge with consistent BRDA record structure.
                cargoLlvmCovExtraArgs = "--branch --lcov --output-path $out/lcov.info";
                RUSTC_BOOTSTRAP = "1";
                # cargoNextest runs tests in checkPhase (vs cargoLlvmCov
                # which used buildPhase), so nativeCheckInputs is correct
                # here — matching the plain `nextest` check above.
                nativeCheckInputs = with pkgs; [
                  inputs.nix.packages.${system}.default
                  openssh
                  postgresql_18
                ];
                # We don't need deps-caching output from this derivation;
                # the plain nextest check already provides that. llvm-cov's
                # instrumented artifacts also don't usefully seed non-cov
                # downstream builds.
                doInstallCargoArtifacts = false;
                dontFixup = true;
                RIO_GOLDEN_TEST_PATH = "${goldenTestPath}";
                RIO_GOLDEN_CA_PATH = "${goldenCaPath}";
              }
            );
          };

          # Container images (Linux-only — dockerTools uses Linux VM
          # namespaces for layering). Worker image includes nix + fuse3
          # + util-linux + passwd stubs; others are minimal.
          #
          # Factored into a function so the coverage pipeline can rebuild
          # images with the instrumented workspace (dockerImagesCov below).
          mkDockerImages =
            ws:
            pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
              import ./nix/docker.nix {
                inherit pkgs;
                rio-workspace = ws;
              }
            );
          dockerImages = mkDockerImages rio-workspace;

          # --------------------------------------------------------------
          # Per-phase VM tests (Linux-only — need NixOS VMs + KVM)
          # --------------------------------------------------------------
          #
          # Each validates the corresponding milestone in docs/src/phases/.
          #
          #   vm-phase1a — 2 VMs: read-only opcodes (path-info, store ls)
          #   vm-phase1b — 3 VMs: single-worker end-to-end build
          #   vm-phase2a — 4 VMs: distributed build across 2+ workers
          #   vm-phase2b — 5 VMs: chain + cache-hit + log pipeline + Tempo (OTLP)
          #   vm-phase2c — 5 VMs: CA + critical-path + size-class + circuit-breaker
          #   vm-phase3a — 3 VMs: k3s operator, WorkerPool CRD, cgroup memory.peak
          #   vm-phase3b — 4 VMs: mTLS, HMAC, recovery, GC, Build CRD
          #
          # mkVmTests: build the attrset for a given (workspace,
          # dockerImages, coverage) triple. vmTests uses the normal
          # build; vmTestsCov uses the instrumented build + coverage=
          # true so common.nix sets LLVM_PROFILE_FILE and appends
          # collectCoverage to each testScript.

          # Request a minimum CPU allocation from nixbuild.net. Each
          # VM has `virtualisation.cores = 4` in common.nix; without
          # this, nixbuild.net's heuristic allocation can under-provision
          # (vm-phase2a once got 5 CPUs for 4 VMs → 16 vCPUs on 5
          # physical, 2 VMs fell back to TCG, worker1's kernel boot
          # starved at PCI enumeration → Shell disconnected flake).
          # numVMs × 4 (cores per VM) + 1 for the test driver itself.
          withMinCpu =
            numVMs: test:
            test.overrideTestDerivation {
              NIXBUILDNET_MIN_CPU = toString (numVMs * 4 + 1);
            };

          mkVmTests =
            {
              rio-workspace,
              dockerImages,
              coverage,
            }:
            let
              vmTestArgs = {
                inherit pkgs rio-workspace coverage;
                rioModules = inputs.self.nixosModules;
              };
              # phase3a/3b need dockerImages + crds on top of the base args.
              k3sArgs = vmTestArgs // {
                inherit dockerImages;
                inherit (inputs.self.packages.${system}) crds;
              };
            in
            pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
              vm-phase1a = withMinCpu 2 (import ./nix/tests/phase1a.nix vmTestArgs);
              vm-phase1b = withMinCpu 3 (import ./nix/tests/phase1b.nix vmTestArgs);
              vm-phase2a = withMinCpu 4 (import ./nix/tests/phase2a.nix vmTestArgs);
              vm-phase2b = withMinCpu 5 (import ./nix/tests/phase2b.nix vmTestArgs);
              vm-phase2c = withMinCpu 5 (import ./nix/tests/phase2c.nix vmTestArgs);
              # 3 VMs but k8s is 8-core (k3s + worker pod) → higher
              # MIN_CPU than the count would suggest.
              vm-phase3a = withMinCpu 4 (import ./nix/tests/phase3a.nix k3sArgs);
              vm-phase3b = withMinCpu 4 (import ./nix/tests/phase3b.nix k3sArgs);
            };

          vmTests = mkVmTests {
            inherit rio-workspace dockerImages;
            coverage = false;
          };

          # Coverage-mode VM tests. Not in `checks` (too slow for flake
          # check) — exposed as packages.cov-vm-phaseXY for manual runs
          # + consumed by nix/coverage.nix for the merged lcov.
          vmTestsCov = mkVmTests {
            rio-workspace = rio-workspace-cov;
            dockerImages = mkDockerImages rio-workspace-cov;
            coverage = true;
          };

          # --------------------------------------------------------------
          # Coverage merge pipeline (Linux-only — depends on vmTestsCov)
          # --------------------------------------------------------------
          #
          # nix/coverage.nix merges profraws from each coverage-mode VM
          # test with the unit-test lcov, producing combined + per-test
          # lcov + genhtml report. See that file for the full pipeline.
          coverage = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
            import ./nix/coverage.nix {
              inherit
                pkgs
                rustStable
                rio-workspace-cov
                vmTestsCov
                ;
              commonSrc = commonArgs.src;
              unitCoverage = cargoChecks.coverage;
            }
          );

          # --------------------------------------------------------------
          # CI aggregate targets
          # --------------------------------------------------------------
          #
          # Single-target validation bundles. Built via linkFarmFromDrvs —
          # result is a directory of symlinks to each constituent's output
          # (inspectable with `ls result/`).
          #
          #                   | 30s fuzz smoke | 10min fuzz nightly
          #   ----------------+----------------+-------------------
          #   no VM tests     | ci-local-fast  | ci-local-slow
          #   with VM tests   | ci-fast        | ci-slow
          #
          # VM-including aggregates are Linux-only (need KVM) — typically
          # built via nix-build-remote. On non-Linux, ci-local-* degrades
          # to cargoChecks + pre-commit only (fuzz is also Linux-only).

          # Base constituents present in every aggregate.
          # fuzz-build is NOT listed — it's a transitive dep of every fuzz
          # derivation and will be built regardless.
          ciBaseDrvs = [
            rio-workspace
            cargoChecks.clippy
            cargoChecks.nextest
            cargoChecks.doc
            cargoChecks.coverage
            cargoChecks.deny
            cargoChecks.tracey-validate
            # pre-commit is auto-generated by git-hooks-nix; referencing
            # config.checks from packages is acyclic.
            config.checks.pre-commit
          ];

          mkCiAggregate =
            {
              name,
              fuzz,
              withVmTests,
            }:
            pkgs.linkFarmFromDrvs "rio-${name}" (
              ciBaseDrvs
              ++ builtins.attrValues fuzz
              ++ pkgs.lib.optionals withVmTests (builtins.attrValues vmTests)
            );

          ciAggregates = {
            ci-local-fast = mkCiAggregate {
              name = "ci-local-fast";
              fuzz = fuzz.smoke;
              withVmTests = false;
            };
            ci-local-slow = mkCiAggregate {
              name = "ci-local-slow";
              fuzz = fuzz.nightly;
              withVmTests = false;
            };
          }
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            ci-fast = mkCiAggregate {
              name = "ci-fast";
              fuzz = fuzz.smoke;
              withVmTests = true;
            };
            ci-slow = mkCiAggregate {
              name = "ci-slow";
              fuzz = fuzz.nightly;
              withVmTests = true;
            };
          };
        in
        {
          # Import rust-overlay
          _module.args.pkgs = import nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          # Configure treefmt
          treefmt.config = {
            flakeCheck = false;
            projectRootFile = "flake.nix";

            programs = {
              nixfmt.enable = true;

              # Rust formatting (stable rustfmt — nightly rustfmt can
              # produce different output, and we want CI/dev parity here)
              rustfmt = {
                enable = true;
                package = rustStable;
              };

              # TOML formatting
              taplo.enable = true;
            };
          };

          # Configure git hooks
          pre-commit = {
            check.enable = true;

            settings.excludes = [
              "docs/mermaid\\.min\\.js$"
              # Fuzz corpus seeds are exact binary/text inputs; trailing
              # newlines would change what the fuzzer sees.
              "^rio-nix/fuzz/corpus/"
              "^rio-store/fuzz/corpus/"
            ];

            settings.hooks = {
              treefmt.enable = true;
              convco.enable = true;
              ripsecrets.enable = true;
              check-added-large-files.enable = true;
              check-merge-conflicts.enable = true;
              end-of-file-fixer.enable = true;
              trim-trailing-whitespace.enable = true;
              deadnix.enable = true;
              nil.enable = true;
              statix.enable = true;
            };
          };

          # --------------------------------------------------------------
          # Dev shells
          # --------------------------------------------------------------
          #
          # Default = nightly so `cargo fuzz run` works out of the box.
          # CI builds still use stable (see craneLib above), so if you
          # write nightly-only code, checks.clippy / checks.nextest will
          # catch it.
          #
          # Use `nix develop .#stable` for strict CI-parity dev.
          devShells =
            let
              shellPackages = with pkgs; [
                # Cargo tools
                cargo-edit
                cargo-expand
                cargo-fuzz # works in default (nightly) shell; errors on stable
                cargo-nextest
                cargo-outdated
                cargo-watch

                # Debugging tools
                lldb
                gdb
                lcov # `lcov --summary`/`--list` on the coverage output

                # Documentation
                mdbook
                mdbook-mermaid

                # Integration test deps
                postgresql_18

                # Formatting (nix fmt also works, but direct treefmt is handy)
                config.treefmt.build.wrapper

                # Spec-coverage: `tracey query validate`, `tracey web`
                traceyPkg
              ];
              shellEnv = {
                RUST_BACKTRACE = "1";
                PROTOC = "${pkgs.protobuf}/bin/protoc";
                LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
                PG_BIN = "${pkgs.postgresql_18}/bin";
                shellHook = config.pre-commit.installationScript;
              };
            in
            {
              default = craneLibNightly.devShell (
                shellEnv
                // {
                  inherit (config) checks;
                  packages = shellPackages;
                  RUST_SRC_PATH = "${rustNightly}/lib/rustlib/src/rust/library";
                }
              );

              stable = craneLib.devShell (
                shellEnv
                // {
                  inherit (config) checks;
                  packages = shellPackages;
                  RUST_SRC_PATH = "${rustStable}/lib/rustlib/src/rust/library";
                }
              );
            };

          # --------------------------------------------------------------
          # Packages
          # --------------------------------------------------------------
          packages = {
            default = rio-workspace;
            # Instrumented build (inspection: `objdump -h result/bin/rio-store
            # | grep llvm_prf`). Used by vmTestsCov and nix/coverage.nix.
            inherit rio-workspace-cov;
            # debug: nix build .#fuzz-build-nix / .#fuzz-build-store
            fuzz-build-nix = fuzz.builds.rio-nix-fuzz-build;
            fuzz-build-store = fuzz.builds.rio-store-fuzz-build;
          }
          # Container images: docker-{gateway,scheduler,store,worker}
          # plus a linkFarm aggregate at `.#dockerImages` (milestone
          # target per docs/src/phases/phase2b.md:46).
          # Linux-only — optionalAttrs means these simply don't exist
          # on Darwin, rather than failing evaluation.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            docker-gateway = dockerImages.gateway;
            docker-scheduler = dockerImages.scheduler;
            docker-store = dockerImages.store;
            docker-worker = dockerImages.worker;
            docker-controller = dockerImages.controller;
            dockerImages = pkgs.linkFarm "rio-docker-images" (
              pkgs.lib.mapAttrsToList (name: drv: {
                name = "${name}.tar.gz";
                path = drv;
              }) dockerImages
            );

            # CRD YAML for kustomize. runCommand invokes the crdgen
            # binary (serde_yaml write-only) and dumps two YAML
            # documents (WorkerPool + Build) to $out. Kustomize
            # references this via `nix build .#crds` → result is a
            # file, copy to deploy/base/crds.yaml and commit.
            #
            # NOT a derivation that the kustomize base depends on
            # directly (kustomize needs real files, not /nix/store
            # paths). It's a convenience for regeneration:
            #   nix build .#crds && cp result deploy/base/crds.yaml
            #
            # Why not auto-regenerate in CI: the committed YAML is
            # what operators `kubectl apply`. Regenerating on every
            # commit means a CRD schema change silently updates the
            # deployed file — we want that change REVIEWED (it may
            # be backward-incompatible).
            crds = pkgs.runCommand "rio-crds.yaml" { } ''
              ${rio-workspace}/bin/crdgen > $out
            '';

            # ──────────────────────────────────────────────────────────
            # VM coverage targets (manual — NOT in ci-fast/ci-slow)
            # ──────────────────────────────────────────────────────────
            #
            # coverage-full: unit + all 7 VM tests merged. ~25min,
            # needs KVM (run via nix-build-remote). Output:
            #   result/lcov.info   — combined, stripped to workspace paths
            #   result/html/       — genhtml report
            #   result/per-test/   — vm-phase*.lcov individual breakdowns
            coverage-full = coverage.full;
            # Same data as coverage-full, HTML-only output at result/
            # (no lcov.info / per-test subdirs). Mirrors coverage-html's
            # relationship to the unit-test coverage check.
            coverage-full-html = pkgs.runCommand "rio-coverage-full-html" { } ''
              ln -s ${coverage.full}/html $out
            '';
            # VM-only combined (no unit-test merge). Debugging.
            coverage-vm = coverage.vmLcov;
          }
          # Per-test lcovs: coverage-vm-phase1a etc. Useful for
          # "why is X not covered" — inspect one VM test's
          # contribution in isolation.
          // pkgs.lib.mapAttrs' (n: v: pkgs.lib.nameValuePair "coverage-${n}" v) coverage.perTestLcov
          # Coverage-mode VM test runs: cov-vm-phase1a etc. Build
          # one to get the raw profraws at result/coverage/<node>/.
          # Used during smoke debugging.
          // pkgs.lib.mapAttrs' (n: v: pkgs.lib.nameValuePair "cov-${n}" v) vmTestsCov
          // {

            # HTML coverage report generated from the lcov tracefile.
            # View: `xdg-open result/index.html`.
            #
            # The coverage derivation's $out is a DIRECTORY (crane's
            # cargoNextest does `mkdir -p $out`), with the tracefile
            # at $out/lcov.info — see cargoLlvmCovExtraArgs on the
            # `coverage` check above. The old cargoLlvmCov wrote lcov
            # to $out itself (a plain file).
            #
            # The tracefile embeds absolute sandbox paths like
            # /build/nix-build-rio-nextest-*.drv-*/source/rio-foo/src/bar.rs
            # Strip everything up through `source/` so genhtml can resolve
            # against ${commonArgs.src}. The pattern anchors on `source/`
            # (not a specific sandbox dir layout) so it survives builder
            # differences — the old cargoLlvmCov had /nix/var/nix/builds/,
            # nixbuild.net's, crane's cargoNextest uses /build/. All share
            # the unpackPhase convention of unpacking into `source/`.
            coverage-html = pkgs.runCommand "rio-coverage-html" { } ''
              ${pkgs.lcov}/bin/lcov \
                --substitute 's|^/[^[:space:]]*/source/||' \
                --extract ${cargoChecks.coverage}/lcov.info 'rio-*' \
                --output-file $TMPDIR/cleaned.lcov
              cd ${commonArgs.src}
              ${pkgs.lcov}/bin/genhtml $TMPDIR/cleaned.lcov \
                --branch-coverage --output-directory $out
            '';
          }
          # 10-minute nightly fuzz runs — NOT in checks, explicitly invoked
          # by the nightly pipeline. Shares rio-{nix,store}-fuzz-build with
          # the smoke tier.
          # TODO(phase3b): corpus persistence (S3 upload/download) so runs
          # accumulate findings instead of discarding the work corpus.
          // fuzz.nightly
          // ciAggregates;

          # --------------------------------------------------------------
          # Checks (run with 'nix flake check')
          # --------------------------------------------------------------
          checks = {
            build = rio-workspace;
          }
          // cargoChecks
          # 30s PR-tier fuzz smokes (Linux-only). Compiled binaries
          # shared with the nightly-tier packages via
          # rio-{nix,store}-fuzz-build.
          // fuzz.smoke
          # Per-phase milestone VM tests (Linux-only, need KVM).
          # Debug interactively:
          #   nix build .#checks.x86_64-linux.vm-phase2a.driverInteractive
          #   ./result/bin/nixos-test-driver
          // vmTests;

          # Formatter for 'nix fmt'
          formatter = config.treefmt.build.wrapper;
        };
    };
}

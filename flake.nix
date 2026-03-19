{
  description = "rio-build - Nix build orchestration";

  inputs = {
    nix = {
      url = "github:NixOS/Nix/2.34.1";
      inputs = {
        flake-compat.follows = "flake-compat";
        flake-parts.follows = "flake-parts";
        git-hooks-nix.follows = "git-hooks-nix";
      };
    };

    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };

    # Spec-coverage tool (nix/tracey.nix). Flake input (not fetchFromGitHub)
    # so crane reads Cargo.lock from a pre-fetched path — no IFD.
    tracey-src = {
      url = "github:bearcove/tracey/2446b4f7433c6220c18737737970f6eccbe2081d";
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

    crane.url = "github:ipetkov/crane";

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    git-hooks-nix = {
      url = "github:cachix/git-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-compat.follows = "flake-compat";
    };

    # Self-hosted binary cache push CLI. Pinned in flake.lock;
    # included in githubActions.build so the first CI job pushes
    # it to rio-nix-cache → subsequent jobs substitute from S3
    # in-region (fast, no curl/GitHub-cache round-trip).
    niks3 = {
      url = "github:Mic92/niks3/v1.4.0";
      inputs = {
        nixpkgs.follows = "nixpkgs";
        flake-parts.follows = "flake-parts";
        treefmt-nix.follows = "treefmt-nix";
        # process-compose isn't a dep of the package build path;
        # leave it unfollowed rather than polluting our inputs.
      };
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
      # standalone-fixture VM tests (nix/tests/fixtures/standalone.nix) and
      # can be reused
      # for real deployments. Each module reads `services.rio.package` for
      # binaries, so callers must set that to a workspace build.
      flake.nixosModules = {
        store = ./nix/modules/store.nix;
        scheduler = ./nix/modules/scheduler.nix;
        gateway = ./nix/modules/gateway.nix;
        worker = ./nix/modules/worker.nix;
      };

      # CI integration — see the perSystem githubActions definition.
      # Linux-only CI runners, so hardcode x86_64-linux.
      flake.githubActions = inputs.self.legacyPackages.x86_64-linux.githubActions;

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
                ./.cargo/deny.toml
                # nextest config (CI profile with JUnit output, test groups)
                ./.config/nextest.toml
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
            # nixbuildnet's adaptive scheduler decays Rust builds
            # (nextest went 24 cores @ 3min → 6 cores @ 8min — optimizes
            # utilization not throughput). Pin minimums on all crane drvs.
            # VM tests already do this per-test via withMinCpu (below).
            NIXBUILDNET_MIN_CPU = "64";
            NIXBUILDNET_MIN_MEM = "65536"; # 64GB in MB
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
          # gracefully (SIGTERM → drain → atexit profraw flush), collect profraws,
          # and nix/coverage.nix merges them with unit-test lcov.
          #
          # Distinct pname → distinct store path, builds in parallel
          # with the non-instrumented workspace. RUSTFLAGS invalidates
          # cargoArtifacts so a separate deps cache is used.
          covArgs = commonArgs // {
            RUSTFLAGS = "-C instrument-coverage";
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
          #   fuzz.runs   — 2min checks, keyed fuzz-<target>
          fuzz = import ./nix/fuzz.nix {
            inherit
              pkgs
              craneLib
              craneLibNightly
              unfilteredRoot
              ;
          };

          # Spec-coverage CLI + web dashboard. The SPA is built via
          # fetchPnpmDeps in nix/tracey.nix and embedded at compile time.
          traceyPkg = import ./nix/tracey.nix {
            inherit craneLib pkgs;
            inherit (inputs) tracey-src;
          };

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
                # `--profile ci` enables retries + test-groups (.config/nextest.toml).
                # `--no-tests=warn`: workspace has leaf bins with no tests.
                cargoNextestExtraArgs = "--no-tests=warn --profile ci";
                nativeCheckInputs = with pkgs; [
                  # nix-cli (not .default / nix-everything): the latter has
                  # nix-functional-tests as a *buildInput*, which fails on
                  # non-NixOS builders (sandboxing/overlayfs tests need priv).
                  # nix-cli has the full bin/ set (nix, nix-daemon, nix-store)
                  # — everything the golden conformance tests need.
                  inputs.nix.packages.${system}.nix-cli
                  openssh
                  postgresql_18
                ];
                # Golden fixture paths (golden/daemon.rs reads these env vars).
                RIO_GOLDEN_TEST_PATH = "${goldenTestPath}";
                RIO_GOLDEN_CA_PATH = "${goldenCaPath}";
                # Force hermetic golden mode — on builders where the Nix
                # sandbox is disabled or relaxed (e.g. early ARC runner
                # config), /nix/var/nix/db can leak into the build. The
                # test's filesystem-based hermetic-detection then tries
                # to symlink the host's locked Nix db → spawned daemon
                # crashes. This env var bypasses detection; tests take
                # the precompute-metadata path unconditionally.
                RIO_GOLDEN_FORCE_HERMETIC = "1";
                # Nix's log prefix wrapper (`rio-nextest> ...`) turns
                # the progress bar into one-line-per-tick spam. `auto`
                # should detect non-TTY but nix's stderr pipe confuses
                # the heuristic. (`show-progress` is user-config-only,
                # can't set it in .config/nextest.toml.)
                NEXTEST_HIDE_PROGRESS_BAR = "1";
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
                  set -o pipefail
                  tracey query validate 2>&1 | tee $out
                '';

            # Helm chart lint + template for all value profiles. Catches
            # Go-template syntax errors, missing required values, bad YAML
            # in rendered output. Subcharts are NOT vendored — charts/ is
            # gitignored; the PG chart is symlinked from the nix store (via
            # nix/helm-charts.nix, nixhelm FOD) because `helm dependency
            # build` needs network and fails in the sandbox.
            # kubeconform schema validation is NOT here (it fetches schemas
            # from raw.githubusercontent.com; nixbuild.net sandbox blocks
            # that) — it's a pre-commit hook instead (runs on dev laptop
            # with network).
            helm-lint =
              let
                chart = pkgs.lib.cleanSource ./infra/helm/rio-build;
              in
              pkgs.runCommand "rio-helm-lint"
                {
                  nativeBuildInputs = [ pkgs.kubernetes-helm ];
                }
                ''
                  cp -r ${chart} $TMPDIR/chart
                  chmod -R +w $TMPDIR/chart
                  cd $TMPDIR/chart
                  # PG subchart from nixhelm (FOD). Rook is NOT a subchart
                  # (separate release, operator-first lifecycle) so helm-lint
                  # doesn't need it.
                  mkdir -p charts
                  ln -s ${subcharts.postgresql} charts/postgresql
                  helm lint .
                  # Default (prod) profile: tag must be set (empty → bad image ref).
                  helm template rio . --set global.image.tag=test > /dev/null
                  helm template rio . -f values/dev.yaml > /dev/null
                  helm template rio . -f values/vmtest-full.yaml > /dev/null
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
                cargoLlvmCovExtraArgs = "--lcov --output-path $out/lcov.info";
                # cargoNextest runs tests in checkPhase (vs cargoLlvmCov
                # which used buildPhase), so nativeCheckInputs is correct
                # here — matching the plain `nextest` check above.
                nativeCheckInputs = with pkgs; [
                  inputs.nix.packages.${system}.nix-cli
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
                # See the nextest check above — same sandbox-detection bypass
                # and progress-bar suppression.
                RIO_GOLDEN_FORCE_HERMETIC = "1";
                NEXTEST_HIDE_PROGRESS_BAR = "1";
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
            {
              rio-workspace,
              coverage ? false,
            }:
            pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
              import ./nix/docker.nix {
                inherit pkgs rio-workspace coverage;
              }
            );
          dockerImages = mkDockerImages { inherit rio-workspace; };

          # Subcharts from nixhelm (FODs — hash-pinned `helm pull`).
          # Referenced by: helm-lint check (symlinked into charts/ in-sandbox),
          # packages.helm-* (`just eks deploy` / `just dev apply` symlink from
          # the result path into the working-tree charts/ — gitignored).
          subcharts = import ./nix/helm-charts.nix {
            inherit (inputs) nixhelm;
            inherit system;
          };

          # --------------------------------------------------------------
          # Scenario×fixture VM tests (Linux-only — need NixOS VMs + KVM)
          # --------------------------------------------------------------
          #
          #   vm-protocol-{warm,cold}-standalone — 3 VMs: opcode coverage
          #   vm-scheduling-{core,disrupt}-standalone — 5 VMs: fanout, size-class, cgroup
          #   vm-security-standalone — 3 VMs: mTLS, HMAC, tenant-resolve
          #   vm-observability-standalone — 5 VMs: metrics, traces, logs
          #   vm-lifecycle-{core,ctrlrestart,recovery,reconnect,autoscale}-k3s
          #   vm-le-{stability,build}-k3s — 2-node k3s fixture (fragment splits)
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
              allTests = import ./nix/tests {
                inherit
                  pkgs
                  rio-workspace
                  dockerImages
                  system
                  coverage
                  ;
                rioModules = inputs.self.nixosModules;
                inherit (inputs) nixhelm;
              };
              # Per-test builder CPU hint. withMinCpu sets
              # NIXBUILDNET_MIN_CPU (numVMs × 4 + 1) to prevent
              # oversubscription → TCG fallback → qemu stall. Default 4
              # for anything not in the table.
              cpuHints = {
                # 3 VMs (control+worker+client). Control is 4-core.
                vm-protocol-warm-standalone = 3;
                vm-protocol-cold-standalone = 3;
                # 5 VMs: control + wsmall1/wsmall2/wlarge + client.
                # Both scheduling splits boot the full 3-worker fixture.
                vm-scheduling-core-standalone = 5;
                vm-scheduling-disrupt-standalone = 5;
                # 3 VMs: control + worker + client.
                vm-security-standalone = 3;
                # 5 VMs: control + worker1/2/3 + client.
                vm-observability-standalone = 5;
                # 3 VMs but k3s-server is 8-core 6GB + k3s-agent 8-core 4GB.
                # All lifecycle + leader-election splits boot the same
                # 2-node k3s fixture.
                vm-lifecycle-core-k3s = 8;
                vm-lifecycle-ctrlrestart-k3s = 8;
                vm-lifecycle-recovery-k3s = 8;
                vm-lifecycle-reconnect-k3s = 8;
                vm-lifecycle-autoscale-k3s = 8;
                vm-le-stability-k3s = 8;
                vm-le-build-k3s = 8;
              };
            in
            pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
              pkgs.lib.mapAttrs (name: withMinCpu (cpuHints.${name} or 4)) allTests
            );

          vmTests = mkVmTests {
            inherit rio-workspace dockerImages;
            coverage = false;
          };

          # Coverage-mode VM tests. Not in `checks` (too slow for flake
          # check) — exposed as packages.cov-vm-phaseXY for manual runs
          # + consumed by nix/coverage.nix for the merged lcov.
          vmTestsCov = mkVmTests {
            rio-workspace = rio-workspace-cov;
            dockerImages = mkDockerImages {
              rio-workspace = rio-workspace-cov;
              coverage = true;
            };
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
          # CI aggregate target
          # --------------------------------------------------------------
          #
          # Single-target validation bundle. Built via linkFarmFromDrvs —
          # result is a directory of symlinks to each constituent's output
          # (inspectable with `ls result/`).
          #
          # All VM tests (including k3s scenarios) + 2min fuzz. On
          # non-Linux, degrades to cargo checks + pre-commit only (VM
          # tests and fuzz are both optionalAttrs isLinux upstream).
          # Linux+KVM required for the full set — typically via
          # `nix-build-remote -- .#ci`.

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
            cargoChecks.helm-lint
            # pre-commit is auto-generated by git-hooks-nix; referencing
            # config.checks from packages is acyclic.
            config.checks.pre-commit
          ];

          ci = pkgs.linkFarmFromDrvs "rio-ci" (
            ciBaseDrvs
            ++ builtins.attrValues fuzz.runs
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux (builtins.attrValues vmTests)
          );

          # ──────────────────────────────────────────────────────────
          # GitHub Actions integration
          # ──────────────────────────────────────────────────────────
          #
          # Structured attrset consumed by .github/workflows/ci.yml.
          # Keeps "what runs in CI" policy in Nix — the workflow is a
          # thin consumer that evaluates this to generate matrices.
          #
          # matrix.<name>: attrsets where keys → GHA matrix entries and
          #   values → derivations to build. Add/remove entries here;
          #   the workflow picks them up automatically via `nix eval`.
          #
          # Runner selection by naming convention: entries with a `vm-`
          # prefix run on `rio-ci-kvm` (bare-metal, /dev/kvm mounted);
          # everything else on `rio-ci` (spot). This keeps the flake
          # emitting simple name→drv maps without per-entry metadata.
          #
          # CI runners are Linux-only. vmTests/coverage/fuzz.runs are
          # all optionalAttrs isLinux upstream, so this whole block is
          # too — on Darwin it's {} (harmless).
          githubActions = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            matrix = {
              # Cargo/static checks. Excludes `coverage` (superseded by
              # the coverage matrix below — Codecov merges flags).
              checks = {
                inherit (cargoChecks)
                  clippy
                  nextest
                  doc
                  deny
                  tracey-validate
                  helm-lint
                  ;
                inherit (config.checks) pre-commit;
              };
              # 2min fuzz runs, one matrix entry per target. Keys are
              # fuzz-<target> (from nix/fuzz.nix). On a cold cache each
              # entry rebuilds the shared fuzz-build derivation, but
              # spot CPU is cheap and the cache fills after first green.
              fuzz = fuzz.runs;
              # Normal VM tests. Keys: vm-phase1a etc. Per-test
              # red/green signal in the GHA UI.
              vm-test = vmTests;
              # lcov-producing jobs, one per Codecov flag. `unit`
              # runs on spot; `vm-*` need KVM (instrumented VM tests
              # → profraw → lcov). Workflow picks runs-on by prefix.
              coverage = {
                unit = coverage.unitLcov;
              }
              // coverage.perTestLcov;
            };
            # niks3 CLI for cache pushes. niks3-push action builds
            # this via `nix build --print-out-paths` and includes
            # the store path in its push — so the first job to
            # complete uploads it to S3, subsequent jobs substitute.
            inherit (inputs.niks3.packages.${system}) niks3;
          };
        in
        {
          # Exported via legacyPackages (free-form, not checked by
          # `nix flake check`). The top-level `flake.githubActions`
          # alias above makes it accessible as `.#githubActions.*`.
          legacyPackages = { inherit githubActions; };

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

              # No kubeconform hook: it fetches ~300MB of schemas from
              # raw.githubusercontent.com at runtime, which fails in the
              # nixbuild.net sandbox (config.checks.pre-commit runs all
              # hooks there). Run it interactively if needed:
              #   helm template rio infra/helm/rio-build --set global.image.tag=x \
              #     | kubeconform -strict -skip CustomResourceDefinition,Certificate,...
              # The helm-lint flake check above catches template syntax
              # errors without network.
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
                stress-ng # flake-repro under load (rio-ci-flake-fixer/validator)

                # Documentation
                mdbook
                mdbook-mermaid

                # Integration test deps
                postgresql_18

                # Local dev stack (`process-compose up`)
                process-compose

                # Formatting (nix fmt also works, but direct treefmt is handy)
                config.treefmt.build.wrapper

                # Spec-coverage: `tracey query validate`, `tracey web`
                traceyPkg

                # Deploy tooling for infra/eks/. Large closures (awscli2
                # pulls python3 + botocore) but the user asked for
                # everything-in-one-shell over a separate .#deploy.
                # Scripts under infra/eks/ also carry nix-shell shebangs
                # pointing at these same packages, so they work even if
                # someone runs them outside `nix develop`.
                awscli2
                # opentofu (not terraform: BSL license → unfree in nixpkgs)
                # with providers bundled via withPlugins. No `tofu init`
                # download step — providers are in the nix store, pinned by
                # nixpkgs rev. .terraform.lock.hcl is gitignored (nix is the
                # lock). The provider set must cover transitive module deps
                # too (EKS module pulls cloudinit + null).
                (opentofu.withPlugins (p: [
                  p.hashicorp_aws
                  p.hashicorp_helm
                  p.hashicorp_kubernetes
                  p.hashicorp_random
                  p.hashicorp_tls
                  p.hashicorp_time
                  p.hashicorp_cloudinit # transitive: terraform-aws-modules/eks
                  p.hashicorp_null # transitive: terraform-aws-modules/eks
                ]))
                kubectl
                skopeo # nix build .#docker-* | skopeo copy docker-archive:... docker://ECR
                kubernetes-helm
                kubeconform # ad-hoc schema validation (no pre-commit hook — fetches 300MB, sandbox blocks)
                yq-go # scripts/split-crds.sh + nix/helm-render.nix
                grpcurl # manual AdminService poking when rio-cli isn't enough
                jq # push-images.sh parses terraform output + smoke-test.sh
                openssl # openssl rand 32 → HMAC key
                git # push-images.sh dirty-tree check + rev-parse for image tag
                just # deploy workflow recipes (see justfile at repo root)

                # Python env for .claude/lib/ + co-located skill scripts —
                # pydantic models are the agent-boundary contracts (each
                # script has `--schema` to print JSON Schema).
                (python3.withPackages (ps: [
                  ps.pydantic
                  ps.pytest
                ]))
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
            # Helm charts from nixhelm (unpacked dirs). `just dev apply` and
            # `just eks deploy` build these and symlink/install from the
            # result path. PG must be in charts/ even when
            # condition: postgresql.enabled is false — Helm validates
            # charts/ against Chart.yaml BEFORE evaluating conditions.
            helm-postgresql = subcharts.postgresql;
            helm-rook-ceph = subcharts.rook-ceph;
            helm-rook-ceph-cluster = subcharts.rook-ceph-cluster;
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
            docker-fod-proxy = dockerImages.fod-proxy;
            docker-bootstrap = dockerImages.bootstrap;
            docker-all = dockerImages.all;
            dockerImages = pkgs.linkFarm "rio-docker-images" (
              pkgs.lib.mapAttrsToList (name: drv: {
                name = "${name}.tar.zst";
                path = drv;
              }) dockerImages
            );

            # Dev worker VM (QEMU + NixOS). Reuses nix/modules/worker.nix
            # with SLiRP networking to reach the host's control plane.
            # Run: result-worker-vm/bin/run-rio-worker-dev-vm
            worker-vm =
              (nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  ./nix/dev-worker-vm.nix
                  { services.rio.package = rio-workspace; }
                ];
              }).config.system.build.vm;

            # CRD YAML for kustomize. runCommand invokes the crdgen
            # binary (serde_yaml write-only) and dumps two YAML
            # documents (WorkerPool + Build) to $out. Kustomize
            # references this via `nix build .#crds` → result is a
            # file; ./scripts/split-crds.sh result splits it into
            # one-file-per-CRD under infra/helm/crds/.
            #
            # NOT a derivation that the kustomize base depends on
            # directly (kustomize needs real files, not /nix/store
            # paths). It's a convenience for regeneration:
            #   nix build .#crds && cp result infra/k8s/base/crds.yaml
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
            # VM coverage targets (manual — NOT in .#ci)
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
          # contribution in isolation. `or {}`: coverage is
          # optionalAttrs isLinux → empty on Darwin → no attr error.
          // pkgs.lib.mapAttrs' (n: v: pkgs.lib.nameValuePair "coverage-${n}" v) (coverage.perTestLcov or { })
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
                --output-directory $out
            '';
          }
          // {
            inherit ci;
          };

          # --------------------------------------------------------------
          # Checks (run with 'nix flake check')
          # --------------------------------------------------------------
          checks = {
            build = rio-workspace;
          }
          // cargoChecks
          # 2min fuzz runs (Linux-only). Compiled binaries shared
          # across targets via rio-{nix,store}-fuzz-build.
          // fuzz.runs
          # Per-phase milestone VM tests (Linux-only, need KVM).
          # Debug interactively:
          #   nix build .#checks.x86_64-linux.vm-phase2a.driverInteractive
          #   ./result/bin/nixos-test-driver
          // vmTests
          # Eval-time assertion: codecov.yml after_n_builds must equal the
          # coverage matrix length. Catches drift when vm-* fragments are
          # added without bumping the Codecov gate. See
          # docs/src/remediations/phase4a/05-coverage-session-drop.md.
          # Linux-only because githubActions is optionalAttrs isLinux.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            codecov-matrix-sync =
              let
                expected = builtins.length (builtins.attrNames githubActions.matrix.coverage);
                # Single-line grep is fine here; if someone rewrites
                # codecov.yml to split the key across lines, the check
                # fails closed (no match → head-on-null error) rather
                # than passing silently.
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
              pkgs.emptyFile;
          };

          # Formatter for 'nix fmt'
          formatter = config.treefmt.build.wrapper;
        };
    };
}

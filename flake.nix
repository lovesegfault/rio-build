{
  description = "rio-build - Nix build orchestration";

  inputs = {
    nix = {
      url = "github:NixOS/Nix/2.34.6";
      inputs = {
        flake-compat.follows = "flake-compat";
        flake-parts.follows = "flake-parts";
        git-hooks-nix.follows = "git-hooks-nix";
      };
    };

    # Multi-version Nix compat matrix inputs (weekly tier — NOT in .#ci).
    # See nix/golden-matrix.nix + docs/src/verification.md § Protocol
    # Conformance. Each input provides a nix-daemon binary; the golden
    # conformance suite runs once per daemon to surface protocol-version
    # divergences early.
    #
    # No `nixpkgs.follows` — following our nixpkgs breaks both the 2.20
    # and Lix builds (they pin specific nixpkgs revs for their own
    # dependency constraints). Accepting the eval cost is cheap for a
    # weekly-only target; the lock entries are inert until
    # `.#golden-matrix` is built.
    nix-stable = {
      url = "github:NixOS/nix/2.20-maintenance";
      # 2.20's flake predates the flake-parts split — minimal follows.
      # Branch-deletion is survivable: the lockfile pins the rev, so
      # only explicit `nix flake update nix-stable` breaks if upstream
      # deletes the branch (and nixpkgs caches tarballs).
      inputs.flake-compat.follows = "flake-compat";
    };
    nix-unstable = {
      url = "github:NixOS/nix";
      inputs = {
        flake-compat.follows = "flake-compat";
        flake-parts.follows = "flake-parts";
        git-hooks-nix.follows = "git-hooks-nix";
      };
    };
    lix = {
      url = "git+https://git.lix.systems/lix-project/lix";
      inputs.flake-compat.follows = "flake-compat";
    };

    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };

    # Spec-coverage tool (nix/tracey.nix). Flake input (not fetchFromGitHub)
    # so importCargoLock reads Cargo.lock from a pre-fetched path — no IFD.
    tracey-src = {
      url = "github:bearcove/tracey";
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
    crate2nix = {
      url = "github:nix-community/crate2nix";
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
        worker = ./nix/modules/builder.nix;
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

          # Shared fileset for the crate2nix workspaceSrc. buildRustCrate
          # works on per-crate directories, so each workspace member's
          # subtree must be included verbatim (no commonCargoSources
          # filter — that strips proto/, which rio-proto's build.rs
          # needs as ./proto/).
          workspaceFileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./rio-auth
            ./rio-cli
            ./rio-common
            ./rio-controller
            ./rio-crds
            ./rio-gateway
            ./rio-nix/src
            ./rio-nix/Cargo.toml
            ./rio-nix/proptest-regressions
            ./rio-proto
            ./rio-scheduler
            ./rio-store/src
            ./rio-store/tests
            ./rio-store/Cargo.toml
            ./rio-test-support
            ./rio-builder
            ./xtask
            ./workspace-hack
            ./migrations
            # sqlx offline query cache — content-addressed JSON per
            # query!(...) callsite. Generated by `cargo xtask regen sqlx`.
            # Required at compile time when SQLX_OFFLINE=1 so the macro
            # doesn't try to connect to PG during the crate2nix build.
            # maybeMissing: resilience against accidental `.sqlx/`
            # deletion during dev — `cargo sqlx prepare` regenerates.
            # (.sqlx/*.json is committed, so a fresh clone WILL have it.)
            (pkgs.lib.fileset.maybeMissing ./.sqlx)
            # Seccomp profile JSON (embedded via include_str! in
            # rio-controller tests — build-time presence check so a
            # missing profile fails compile, not silently at deploy).
            ./nix/nixos-node/seccomp
          ];
          workspaceSrc = pkgs.lib.fileset.toSource {
            root = unfilteredRoot;
            fileset = workspaceFileset;
          };

          # Prefix every key in an attrset. Used to surface per-member
          # derivations under flake packages.
          prefixed = p: pkgs.lib.mapAttrs' (n: v: pkgs.lib.nameValuePair "${p}${n}" v);

          # ──────────────────────────────────────────────────────────────
          # sys-crate linkage: per-crate single source of truth
          # ──────────────────────────────────────────────────────────────
          #
          # Each sys-crate that system-links instead of vendoring C gets
          # its env-var escape hatch + system lib here. Per-crate shape
          # so crate2nix crateOverrides can reference .crates.<name>
          # directly; devShell consumes the derived .allEnv/.allLibs
          # aggregates.
          #
          # Adding a sys-crate: add a .crates.<name> entry here, add the
          # override in nix/crate2nix.nix referencing it, done.
          sysCrateEnv =
            let
              crates = {
                # build.rs:49-53 escape hatch: routes build_linked →
                # pkg-config probe instead of compiling the bundled
                # amalgamation (sqlx's `sqlite` → sqlx-sqlite/bundled
                # feature chain otherwise forces vendoring).
                # bundled_bindings stays — precompiled Rust bindings,
                # no bindgen; SQLite 3.x ABI stability makes them work
                # against any 3.x system lib.
                libsqlite3-sys = {
                  env.LIBSQLITE3_SYS_USE_PKG_CONFIG = "1";
                  libs = [ pkgs.sqlite ];
                };
                # build.rs:30 escape hatch: probe → system libzstd.
                zstd-sys = {
                  env.ZSTD_SYS_USE_PKG_CONFIG = "1";
                  libs = [ pkgs.zstd ];
                };
                # No escape-hatch env var — fuser's build.rs already
                # defaults to pkg-config (never bundles).
                fuser = {
                  env = { };
                  libs = [ pkgs.fuse3 ];
                };
              };
            in
            {
              inherit crates;
              # Derived aggregates for the dev shell (workspace-wide
              # buildInputs + env).
              allEnv = pkgs.lib.foldl' (a: c: a // c.env) { } (pkgs.lib.attrValues crates);
              allLibs = pkgs.lib.concatMap (c: c.libs) (pkgs.lib.attrValues crates);
            };

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
          rio-workspace-cov = crateBuildCov.workspaceBinsCov;
          rio-crates-cov = crateBuildCov.memberBinsCov;

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
              rustNightly
              rustPlatformNightly
              unfilteredRoot
              workspaceFileset
              ;
          };

          # Spec-coverage CLI + web dashboard. The SPA is built via
          # fetchPnpmDeps in nix/tracey.nix and embedded at compile time.
          traceyPkg = import ./nix/tracey.nix {
            inherit pkgs;
            rustPlatform = rustPlatformNightly;
            inherit (inputs) tracey-src;
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
          # packages.workspace + packages.rio-<crate>.
          mkCrateBuild =
            extra:
            import ./nix/crate2nix.nix (
              {
                inherit
                  pkgs
                  rustStable
                  sysCrateEnv
                  workspaceSrc
                  ;
                inherit (pkgs) lib;
                crate2nixSrc = inputs.crate2nix;
              }
              // extra
            );
          crateBuild = mkCrateBuild { };

          # Coverage-instrumented tree: re-import with
          # globalExtraRustcOpts=["-Cinstrument-coverage"]. Doubles the
          # derivation count (645 normal + 645 instrumented), but each
          # half caches independently — touching a workspace crate only
          # rebuilds that crate's two variants + dependents.
          crateBuildCov = mkCrateBuild {
            globalExtraRustcOpts = [
              "-Cinstrument-coverage"
              "-Ccodegen-units=16"
              "-Cdebuginfo=line-tables-only"
            ];
          };

          # ──────────────────────────────────────────────────────────
          # crate2nix check backends: clippy, tests, doc
          # ──────────────────────────────────────────────────────────
          #
          # Per-crate checks layered on the crate2nix build graph.
          # Deps are built once (regular rustc, 645 cached drvs);
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
          crateChecks = import ./nix/checks.nix {
            inherit
              pkgs
              rustStable
              crateBuild
              crateBuildCov
              ;
            inherit (pkgs) lib;
            # Runtime inputs for test execution. Mirrors crane's
            # cargoNextest nativeCheckInputs — postgres for ephemeral
            # PG bootstrap (rio-test-support), nix-cli for golden
            # conformance tests (nix-store --dump, nix-instantiate),
            # openssh for rio-gateway SSH accept tests.
            runtimeTestInputs = with pkgs; [
              inputs.nix.packages.${system}.nix
              openssh
              postgresql_18
            ];
            # Env vars for test runners. PG_BIN so rio-test-support
            # finds initdb/postgres; RIO_GOLDEN_* so golden tests
            # don't try to `nix build` their fixture in-sandbox.
            testEnv = goldenTestEnv // {
              PG_BIN = "${pkgs.postgresql_18}/bin";
            };
            # nextest reuse-build runner. Synthesizes --cargo-metadata
            # and --binaries-metadata JSON from the crate2nix test
            # binaries; runs with the `ci` profile (retries, test
            # groups from .config/nextest.toml). Per-test-process
            # isolation — no PDEATHSIG/libtest thread race, so
            # wrapper-level PG bootstrap not needed. `--no-tests=warn`
            # because rio-cli has zero tests (bin-only crate).
            #
            # Fileset = workspaceSrc PLUS .config/nextest.toml
            # (--workspace-remap needs to find it relative to the
            # workspace root). The base fileset omits .config/
            # because buildRustCrate doesn't need it.
            workspaceSrc = pkgs.lib.fileset.toSource {
              root = unfilteredRoot;
              fileset = pkgs.lib.fileset.unions [
                workspaceFileset
                ./.config/nextest.toml
                # metrics_registered tests grep the per-component
                # metrics tables at runtime (rio-test-support
                # grep_spec_names reads ../docs/src/observability.md
                # via fs::read_to_string). Adding a row breaks the
                # nextest drv hash, not the build/clippy/doc hashes.
                ./docs/src/observability.md
              ];
            };
            nextestExtraArgs = [
              "--profile"
              "ci"
              "--no-tests=warn"
            ];
          };

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
              rustStable
              rustPlatformStable
              traceyPkg
              subcharts
              dockerImages
              nodeAmi
              ;
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
            pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
              import ./nix/docker.nix {
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
              }
            );
          dockerImages = mkDockerImages { inherit rio-crates; };

          # NixOS EKS node AMI builder (ADR-021). Exposed below as
          # packages.node-ami-{x86_64,aarch64}. The amazon-image.nix
          # builder module emits a directory with the disk image +
          # nix-support/image-info.json (consumed by `xtask ami push`).
          #
          # `nodeSystem` is the TARGET arch, independent of the eval
          # host — same shape as the dockerImages multi-arch build.
          # specialArgs threads pins.nix through so module files can
          # read kernel/nodeadm pins without `import ../../pins.nix`
          # scattershot.
          nodeAmi =
            nodeSystem:
            {
              # I-205: x86_64 .metal SKUs are legacy-bios ONLY (zero
              # support UEFI per `aws ec2 describe-instance-types`). The
              # bios variant swaps uki-boot.nix for bios-boot.nix and
              # registers boot_mode=legacy-bios; everything else is
              # identical so the rio-metal EC2NodeClass can select it for
              # the rio-builder-metal NodePool while rio-default keeps
              # the UEFI/UKI image for virtualized + arm64 .metal.
              efi ? true,
            }:
            (nixpkgs.lib.nixosSystem {
              system = nodeSystem;
              specialArgs = {
                pins = import ./nix/pins.nix;
                # Layer-cache warm for ephemeral builder/fetcher pods
                # (PLAN-PREBAKE / r[infra.node.prebake-layer-warm]).
                # self.packages.${nodeSystem} is safe inside perSystem
                # — flake-parts resolves the cross-arch attr without
                # recursion (nodeSystem ≠ eval system is the common
                # case: x86 host builds the aarch64 AMI).
                rioSeedImages = [
                  inputs.self.packages.${nodeSystem}.docker-executor-seed
                ];
              };
              modules = [
                (nixpkgs + "/nixos/maintainers/scripts/ec2/amazon-image.nix")
                ./nix/nixos-node
                (if efi then ./nix/nixos-node/uki-boot.nix else ./nix/nixos-node/bios-boot.nix)
                {
                  # raw → coldsnap uploads directly to an EBS snapshot
                  # via the EBS Direct API (no S3 / VM-Import round-trip,
                  # ~20min → ~2min for an 8 GB image).
                  amazonImage.format = "raw";
                  virtualisation.diskSize = "auto";
                  ec2.efi = efi;
                }
              ];
            }).config.system.build.amazonImage;

          # Subcharts from nixhelm (FODs — hash-pinned `helm pull`).
          # Referenced by: helm-lint check (symlinked into charts/ in-sandbox),
          # packages.helm-* (`cargo xtask {eks deploy,dev apply}` symlink from
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
          #   vm-scheduling-{core,disrupt}-standalone — 5 VMs: fanout, resource-floor, cgroup
          #   vm-security-standalone — 3 VMs: mTLS, HMAC, tenant-resolve
          #   vm-observability-standalone — 5 VMs: metrics, traces, logs
          #   vm-ca-cutoff-standalone — CA-on-CA cutoff propagation
          #   vm-chaos-standalone — fault injection
          #   vm-lifecycle-{core,recovery,autoscale,pool,prod-parity}-k3s
          #   vm-le-{stability,build}-k3s — 2-node k3s fixture (fragment splits)
          #   vm-security-nonpriv-k3s — privileged-hardening e2e
          #   vm-cli-k3s — rio-cli integration
          #   vm-dashboard-k3s, vm-dashboard-gateway-k3s — gRPC-Web + envoy
          #   vm-netpol-k3s — NetworkPolicy enforcement
          #
          # mkVmTests: build the attrset for a given (workspace,
          # dockerImages, coverage) triple. vmTests uses the normal
          # build; vmTestsCov uses the instrumented build + coverage=
          # true so common.nix sets LLVM_PROFILE_FILE and appends
          # collectCoverage to each testScript.

          # Request a minimum CPU allocation from the remote builder. Each
          # VM has `virtualisation.cores = 4` in common.nix; without
          # this, the builder's heuristic allocation can under-provision
          # (vm-scheduling-core once got 5 CPUs for 4 VMs → 16 vCPUs on 5
          # physical, 2 VMs fell back to TCG, worker1's kernel boot
          # starved at PCI enumeration → Shell disconnected flake).
          #
          # Floor of 64 vCPU / 128GB: prevents KVM contention across
          # concurrent VM-test builds on the same host. With ~60-190
          # CPU hosts, 64 vCPU floor caps at ~1-3 concurrent VM-test
          # builds per host (previously ~9 at old ×4 formula → up to
          # ~45 concurrent qemu KVM_CREATE_VM → some lose the race,
          # "failed to initialize kvm: Permission denied" → TCG
          # fallback or hard fail). 128GB floor ensures the k3s tests
          # (≈20GB peak) plus qemu+test-driver overhead have headroom.
          # cpuHints is still consulted for the ×4 formula when it
          # exceeds the floor (future >16-VM tests).
          withMinCpu =
            numVMs: test:
            let
              byVMs = numVMs * 4 + 1;
              cpuFloor = 64;
              memFloor = 131072;
            in
            test.overrideTestDerivation {
              NIXBUILDNET_MIN_CPU = toString (pkgs.lib.max byVMs cpuFloor);
              NIXBUILDNET_MIN_MEM = toString memFloor;
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
                # Lix-client VM test (vm-protocol-warm-lix-standalone).
                # Same `or` fallback as nix/golden-matrix.nix — Lix
                # exposes .default today; future renames covered.
                lixPackage = inputs.lix.packages.${system}.nix-cli or inputs.lix.packages.${system}.default;
              };
              # Per-test builder CPU hint. withMinCpu sets
              # NIXBUILDNET_MIN_CPU (numVMs × 4 + 1) to prevent
              # oversubscription → TCG fallback → qemu stall. Fallthrough:
              # 8 for -k3s suffix, else 4 (see mapAttrs below).
              cpuHints = {
                # 3 VMs (control+worker+client). Control is 4-core.
                vm-protocol-warm-standalone = 3;
                vm-protocol-warm-lix-standalone = 3;
                vm-protocol-cold-standalone = 3;
                # 5 VMs: control + wsmall1/wsmall2/wlarge + client.
                # Both scheduling splits boot the full 3-worker fixture.
                vm-scheduling-core-standalone = 5;
                vm-scheduling-disrupt-standalone = 5;
                # 3 VMs: control + worker + client.
                vm-security-standalone = 3;
                # 3 VMs: control + worker + client. Single-worker
                # standalone fixture (ca-cutoff chain is serial anyway).
                vm-ca-cutoff-standalone = 3;
                # 3 VMs: control + worker + client. toxiproxy runs as a
                # systemd unit on control, not a separate VM.
                vm-chaos-standalone = 3;
                # 5 VMs: control + worker1/2/3 + client.
                vm-observability-standalone = 5;
                # 3 VMs but k3s-server is 8-core 6GB + k3s-agent 8-core 4GB.
                # All lifecycle + leader-election splits boot the same
                # 2-node k3s fixture.
                vm-lifecycle-core-k3s = 8;
                vm-lifecycle-recovery-k3s = 8;
                vm-lifecycle-autoscale-k3s = 8;
                vm-le-stability-k3s = 8;
                vm-le-build-k3s = 8;
                # k3s nonpriv e2e (base_runtime_spec /dev/fuse +
                # cgroup rw-remount).
                vm-security-nonpriv-k3s = 8;
                # k3s + Cilium Gateway API (+cilium-envoy image). No builds.
                vm-dashboard-gateway-k3s = 8;
                # Same fixture + rio-dashboard nginx image. curl via
                # nginx → Cilium Gateway → scheduler (tonic-web).
                vm-dashboard-k3s = 8;
                # k3s base fixture. rio-cli AdminService smoke.
                vm-cli-k3s = 8;
                # k3s base fixture. Worker egress NetworkPolicy enforce.
                vm-netpol-k3s = 8;
                # Same 2-node k3s fixture + bootstrap Job backoff.
                # Asserts PSA-restricted — NOT in vmTestsCov (see removeAttrs below).
                vm-lifecycle-prod-parity-k3s = 8;
              };
            in
            # Dead-entry guard: every cpuHints key must name a real test.
            # Before this assert, vm-lifecycle-bps-k3s and vm-fod-proxy-k3s
            # sat here for months after deletion — the old comment claimed a
            # "T539" check caught dead entries; that check never existed.
            # Gated on !coverage: vmTestsCov's allTests is a strict subset
            # (vm-dashboard-k3s is optionalAttrs-gated on dockerImages?
            # dashboard, absent in coverage mode); checking the full set
            # once is sufficient.
            assert
              coverage
              ||
                pkgs.lib.assertMsg (pkgs.lib.all (k: allTests ? ${k}) (pkgs.lib.attrNames cpuHints))
                  "cpuHints has entries for tests not in nix/tests/default.nix: ${
                    toString (pkgs.lib.filter (k: !(allTests ? ${k})) (pkgs.lib.attrNames cpuHints))
                  }";
            pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
              pkgs.lib.mapAttrs (
                name:
                withMinCpu (
                  cpuHints.${name}
                    # k3s fixture: 2-node cluster, k3s-server 8-core + k3s-agent
                    # 8-core. Every -k3s test in the table is 8; encode that as
                    # the suffix default so new -k3s tests don't fall through to
                    # 4. Catchup-fix precedent: d6f74e27 + fa55ef13 both added
                    # forgotten -k3s entries. Dead entries fail the assert above.
                    or (if pkgs.lib.hasSuffix "-k3s" name then 8 else 4)
                )
              ) allTests
            );

          vmTests = mkVmTests {
            inherit rio-workspace dockerImages;
            coverage = false;
          };

          # Coverage-mode VM tests. Not in `checks` (too slow for flake
          # check) — exposed as packages.cov-vm-<scenario> for manual runs
          # + consumed by nix/coverage.nix for the merged lcov.
          vmTestsCov =
            removeAttrs
              (mkVmTests {
                rio-workspace = rio-workspace-cov;
                dockerImages = mkDockerImages {
                  rio-crates = rio-crates-cov;
                  coverage = true;
                };
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
          # Coverage mode uses workspaceBinsCov (not workspaceBins) —
          # same closure-scrub but skips strip so the __llvm_covfun /
          # __llvm_covmap sections llvm-cov needs stay intact.
          coverage = pkgs.lib.optionalAttrs pkgs.stdenv.isLinux (
            import ./nix/coverage.nix {
              # workspaceSrc is the genhtml source root — coverage.nix
              # cd's there so repo-relative lcov paths resolve.
              inherit
                pkgs
                rustStable
                rio-workspace-cov
                vmTestsCov
                workspaceSrc
                ;
              unitCoverage = crateChecks.coverage;
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
          # Derived from config.checks — every check is a CI constituent.
          # Before P0525 this was a manual list that had drifted to 44/45:
          # codecov-matrix-sync (the one guarding codecov.yml drift) was
          # missing, so after_n_builds went 2 commits stale before ee957551
          # hand-patched it. `nix flake check` caught it; `.#ci` (the merge
          # gate) did not. Deriving closes the class.
          #
          # config.checks is the flake-parts merged result: our checks
          # attrset + git-hooks module's pre-commit. That attrset already
          # //-merges vmTests and fuzz.runs, so they appear here without
          # explicit addition. On non-Linux it's smaller (vmTests, fuzz,
          # codecov-matrix-sync are all optionalAttrs isLinux upstream) —
          # ci degrades to Rust checks + pre-commit automatically.
          #
          # builtins.attrValues is attr-name sorted → stable linkFarm hash.
          ci = pkgs.linkFarmFromDrvs "rio-ci" (
            builtins.attrValues config.checks
            ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
              # cov-smoke: one coverage-mode VM scenario, asserts
              # profraw→lcov pipeline works. ~5min. Catches
              # "coverage infra broken" at merge-gate instead of
              # 118 commits later via backgrounded coverage-full.
              # NOT a check (too slow for `nix flake check` on a
              # non-KVM host) — CI-aggregate only.
              coverage.smoke
            ]
          );

          # --------------------------------------------------------------
          # Multi-Nix golden conformance matrix (weekly tier)
          # --------------------------------------------------------------
          #
          # Runs golden_conformance against 4 daemon variants: pinned Nix,
          # Nix 2.20-maintenance, Nix master, Lix. Weekly cron invokes
          # `nix build .#golden-matrix`. NOT in `.#ci` — building three
          # extra Nix source trees is a 60-90min cold-cache tax. Exported
          # Linux-only at the `packages` site (the flake inputs eval fine
          # on Darwin but `nix-daemon` needs a real unix socket + /nix
          # layout, and we don't run the matrix on macs anyway).
          goldenMatrix = import ./nix/golden-matrix.nix {
            inherit pkgs inputs system;
            inherit (crateChecks) mkNextestRun;
          };

          # --------------------------------------------------------------
          # Mutation testing (weekly tier — NOT in .#ci)
          # --------------------------------------------------------------
          inherit
            (import ./nix/mutants.nix {
              inherit
                pkgs
                version
                unfilteredRoot
                workspaceFileset
                rustStable
                rustPlatformStable
                sysCrateEnv
                goldenTestEnv
                ;
              nixPkg = inputs.nix.packages.${system}.nix;
            })
            mutants
            mutants-smoke
            ;

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
              # Rust + static checks. Derived from config.checks — same
              # P0525 rationale as .#ci above: a manual list had drifted
              # to miss executor-seed-layer-parity, node-ami-eval, and
              # codecov-matrix-sync (the very check P0525 added .#ci
              # derivation for). Subtract what other matrices already
              # cover (fuzz, vm-test, coverage) plus `build` (clippy/
              # nextest deps already build the workspace). attrNames
              # forces only key names, not values — codecov-matrix-sync's
              # value reads githubActions.matrix.coverage, but its KEY is
              # a literal, so no recursion.
              checks = builtins.removeAttrs config.checks (
                [
                  "build"
                ]
                ++ builtins.attrNames fuzz.runs
                ++ builtins.attrNames vmTests
              );
              # 2min fuzz runs, one matrix entry per target. Keys are
              # fuzz-<target> (from nix/fuzz.nix). On a cold cache each
              # entry rebuilds the shared fuzz-build derivation, but
              # spot CPU is cheap and the cache fills after first green.
              fuzz = fuzz.runs;
              # Normal VM tests. Keys: vm-<scenario>-<fixture>. Per-test
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
              ;
            treefmtWrapper = config.treefmt.build.wrapper;
            preCommitInstall = config.pre-commit.installationScript;
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
            # Helm charts from nixhelm (unpacked dirs). `cargo xtask {dev apply,
            # eks deploy}` build these and symlink/install from the result
            # path. PG must be in charts/ even when
            # condition: postgresql.enabled is false — Helm validates
            # charts/ against Chart.yaml BEFORE evaluating conditions.
            helm-postgresql = subcharts.postgresql;
            helm-rook-ceph = subcharts.rook-ceph;
            helm-rook-ceph-cluster = subcharts.rook-ceph-cluster;
            helm-cilium = subcharts.cilium;
            # nix/pins.nix rendered as *.auto.tfvars.json. snake_case
            # keys in pins.nix → direct toJSON passthrough, no mapping
            # layer. Regenerate the committed copy:
            #   nix build .#tfvars && jq -S . result > infra/eks/generated.auto.tfvars.json
            tfvars = pkgs.writeText "generated.auto.tfvars.json" (builtins.toJSON (import ./nix/pins.nix));
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
            docker-builder = dockerImages.builder;
            docker-fetcher = dockerImages.fetcher;
            docker-controller = dockerImages.controller;
            docker-bootstrap = dockerImages.bootstrap;
            docker-dashboard = dockerImages.dashboard;
            # AMI layer-cache seed (oci-archive tarball, both
            # builder+fetcher refs). NOT pushed to ECR — baked into
            # the NixOS node AMI's containerd content store.
            docker-executor-seed = dockerImages.executorSeed;
            # k3s VM-test seed (oci-archive tarball, all 6 component
            # refs, deduped layers). NOT pushed to ECR — preloaded via
            # services.k3s.images in nix/tests/fixtures/k3s-full.nix.
            # Replaces the former docker-all aggregate (W1: that image
            # was pushed to ECR via the linkFarm below despite never
            # being used on EKS).
            docker-vmtest-seed = dockerImages.vmTestSeed;
            dockerImages = pkgs.linkFarm "rio-docker-images" (
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
            );

            # Dev worker VM (QEMU + NixOS). Reuses nix/modules/builder.nix
            # with SLiRP networking to reach the host's control plane.
            # Run: result-worker-vm/bin/run-rio-builder-dev-vm
            worker-vm =
              (nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  ./nix/dev-builder-vm.nix
                  { services.rio.package = rio-workspace; }
                ];
              }).config.system.build.vm;

            # ──────────────────────────────────────────────────────────
            # NixOS EKS node AMI (ADR-021). Replaces bottlerocket@latest
            # for builder/fetcher Karpenter NodePools.
            #
            #   nix build .#node-ami-x86_64    # → result/nixos-amazon-image-*.vhd
            #   cargo xtask k8s -p eks ami push --arch x86_64
            #
            # Output dir contains the disk image plus `nix-support/
            # image-info.json` (label, system, file, boot_mode) which
            # `xtask ami push` reads for coldsnap upload + register-image.
            #
            # Per-arch attrs (NOT keyed off the eval host's `system`): the
            # build host cross-builds both, like .#packages.<sys>.
            # dockerImages. xtask asks for both explicitly.
            # ──────────────────────────────────────────────────────────
            node-ami-x86_64 = nodeAmi "x86_64-linux" { };
            node-ami-aarch64 = nodeAmi "aarch64-linux" { };
            # I-205: x86 .metal NodePool only — see nodeAmi comment.
            node-ami-x86_64-bios = nodeAmi "x86_64-linux" { efi = false; };

            # CRD YAML for the crds-drift check. runCommand invokes
            # the crdgen binary and dumps Pool + ComponentScaler to
            # $out; misc-checks.nix:crds-drift splits it via
            # split-crds.py and diffs against infra/helm/crds/.
            # `cargo xtask regen crds` does NOT use this — it runs
            # `cargo run --bin crdgen` directly to avoid a nix build
            # in the dev loop.
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
            # coverage-full: unit + all VM tests merged. ~25min,
            # needs KVM (run via nix-build-remote). Output:
            #   result/lcov.info   — combined, stripped to workspace paths
            #   result/html/       — genhtml report
            #   result/per-test/   — vm-<scenario>.lcov individual breakdowns
            coverage-full = coverage.full;
            # cov-smoke: fast (~5min) one-scenario coverage-infra
            # smoke. Also in .#ci (blocking). Manual run for
            # debugging: `nix build .#cov-smoke && cat result/summary`.
            cov-smoke = coverage.smoke;
            # Same data as coverage-full, HTML-only output at result/
            # (no lcov.info / per-test subdirs). Mirrors coverage-html's
            # relationship to the unit-test coverage check.
            coverage-full-html = pkgs.runCommand "rio-coverage-full-html" { } ''
              ln -s ${coverage.full}/html $out
            '';
            # VM-only combined (no unit-test merge). Debugging.
            coverage-vm = coverage.vmLcov;
          }
          # Per-test lcovs: coverage-vm-<scenario> etc. Useful for
          # "why is X not covered" — inspect one VM test's
          # contribution in isolation. `or {}`: coverage is
          # optionalAttrs isLinux → empty on Darwin → no attr error.
          // prefixed "coverage-" (coverage.perTestLcov or { })
          # Coverage-mode VM test runs: cov-vm-<scenario> etc. Build
          # one to get the raw profraws at result/coverage/<node>/.
          # Used during smoke debugging.
          // prefixed "cov-" vmTestsCov
          // {
            # HTML coverage report from the unit-test lcov.
            # crateChecks.coverage already emits repo-relative paths
            # (`rio-*/src/...`) — no strip needed, just genhtml.
            coverage-html = pkgs.runCommand "rio-coverage-html" { } ''
              cd ${workspaceSrc}
              ${pkgs.lcov}/bin/genhtml ${crateChecks.coverage}/lcov.info \
                --output-directory $out
            '';
            inherit ci;
          }
          # Per-member crate2nix derivations. Keys are the crate
          # names (rio-scheduler, rio-common, ...). See
          # .claude/notes/crate2nix-migration-assessment.md.
          // crateBuild.members
          # Per-member check derivations for targeted runs:
          #   nix build .#clippy-rio-scheduler
          #   nix build .#doc-rio-nix
          // prefixed "clippy-" crateChecks.clippy
          // prefixed "clippy-test-" crateChecks.clippyTest
          // prefixed "test-bin-" crateChecks.testBins
          // prefixed "doc-" crateChecks.doc
          // {
            # Raw symlinkJoin of all built crate outputs. References
            # the intermediate .rlib tree — use workspace-bins for
            # docker/VM tests.
            inherit (crateBuild) workspace;
            # Stripped binary-only variant — what VM tests/docker
            # consume. Closure ~glibc+syslibs.
            workspace-bins = crateBuild.workspaceBins;
            # crate2nix CLI for the dev shell (`crate2nix generate
            # --format json -o Cargo.json` regenerates after lockfile
            # changes).
            crate2nix-cli = crate2nixCli;
            # Aggregate check derivations (same as checks.* but
            # exposed as packages for --print-out-paths convenience).
            clippy-all = crateChecks.clippyCheck;
            doc-all = crateChecks.docCheck;
            # nextest reuse-build runner — characteristic
            # `PASS [Xs] crate::test` output, test groups, retries.
            # Binaries synthesized from crate2nix testBinDrvs, no
            # cargo invocation. nextest-meta is the cached metadata
            # derivation for debugging / manual `cargo-nextest run
            # --binaries-metadata result/binaries-metadata.json`.
            nextest-all = crateChecks.nextest;
            nextest-meta = crateChecks.nextestMetadata;
            # Coverage: nextest run against instrumented binaries
            # (raw profraws at $out/profraw/) + merged lcov.
            cov-profraw = crateChecks.covProfraw;
            inherit (crateChecks) coverage;
            # Toolchain wrappers for debugging the arg-filtering:
            #   nix build .#clippy-rustc
            #   ./result/bin/rustc --version   # → clippy version
            clippy-rustc = crateChecks.clippyRustc;
            rustdoc-rustc = crateChecks.rustdocRustc;
            # Instrumented workspace (symlinkJoin). Inspection:
            #   objdump -h result/bin/rio-store | grep llvm_prf
            workspace-cov = crateBuildCov.workspace;
          }
          # Per-test VM packages (Linux-only — mkVmTests wraps in
          # optionalAttrs isLinux):
          #   nix build .#vm-protocol-warm-standalone
          #   nix build .#cov-vm-lifecycle-core-k3s
          // vmTests
          # Multi-Nix golden matrix (weekly). Exported Linux-only:
          # nix-daemon needs a unix socket; macOS matrix not supported.
          # Under `packages` not `checks` → `nix flake check` won't
          # build the three extra Nix source trees on every push.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            golden-matrix = goldenMatrix;
            inherit mutants mutants-smoke;
          };

          # --------------------------------------------------------------
          # Checks (run with 'nix flake check')
          # --------------------------------------------------------------
          checks = {
            build = rio-workspace;
            clippy = crateChecks.clippyCheck;
            doc = crateChecks.docCheck;
            inherit (crateChecks) nextest;
            dashboard = rioDashboard;
          }
          // miscChecks
          # 2min fuzz runs (Linux-only). Compiled binaries shared
          # across targets via rio-{nix,store}-fuzz-build.
          // fuzz.runs
          # Per-phase milestone VM tests (Linux-only, need KVM).
          # Debug interactively:
          #   nix build .#checks.x86_64-linux.vm-protocol-warm-standalone.driverInteractive
          #   ./result/bin/nixos-test-driver
          // vmTests
          # Eval-time assertion: codecov.yml after_n_builds must equal the
          # coverage matrix length. Catches drift when vm-* fragments are
          # added without bumping the Codecov gate.
          # Linux-only because githubActions is optionalAttrs isLinux.
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            # Regression: per-node profraw extract must not drop
            # filename-colliding profraws across multi-worker nodes.
            # No KVM needed (synthetic tarballs).
            cov-extract-nocollide = coverage.extractNoCollide;
            codecov-matrix-sync =
              let
                expected = builtins.length (builtins.attrNames githubActions.matrix.coverage);
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
              # Named (not pkgs.emptyFile) so `ls result/` of .#ci shows
              # which constituent this is — eval-time asserts are invisible
              # otherwise once they pass.
              pkgs.runCommand "rio-codecov-matrix-sync" { } "touch $out";
          };

          # Formatter for 'nix fmt'
          formatter = config.treefmt.build.wrapper;
        };
    };
}

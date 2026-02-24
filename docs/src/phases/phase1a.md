# Phase 1a: Wire Format + Read-Only Protocol (Months 1-3)

**Goal:** Prove the protocol approach with a read-only store.

## Tasks

- [x] **FUSE+overlay+sandbox platform validation (Week 1-2)**
  - Create a standalone test pod on EKS with `CAP_SYS_ADMIN` + `CAP_SYS_CHROOT`
  - Mount a FUSE filesystem at `/nix/store` (using `fuser` crate, serving test data)
  - Create overlayfs with FUSE mount as lower layer, local SSD as upper
  - Generate synthetic SQLite DB in upper layer
  - Run `nix-build` inside the Nix sandbox within the overlay
  - **Validation checklist:**
    - Kernel version compatibility: test on EKS AL2023 (kernel 6.1+) and at least one other distro kernel
    - overlayfs-over-FUSE correctness: verify file reads, directory listings, and symlink resolution through the full stack
    - FUSE read latency: measure p50/p99 latency for cached reads under concurrent access (target: < 2x direct filesystem reads)
    - Concurrent build isolation: run two builds simultaneously on the same FUSE mount with separate overlays, verify no cross-contamination
    - Synthetic SQLite consistency: verify Nix sees all expected paths in the SQLite DB and can resolve them via the FUSE mount
    - `nix-build` under overlayfs: verify that build outputs land in the correct overlay upper layer
  - **Go/no-go gate:** If the full chain (FUSE → overlayfs → sandbox → build) fails on the target EKS kernel, or FUSE read latency exceeds 5x direct reads, activate the [fallback plan](#fuseoverlay-fallback-plan)
  - **Results (EKS AL2023, kernel 6.12, c8a.xlarge, K8s 1.35):**
    - Kernel compatibility: **PASS** — overlayfs-over-FUSE works on AL2023 kernel 6.12.
    - overlayfs-over-FUSE correctness: **PASS** — file reads, directory listings, and symlink resolution all verified through the full FUSE → overlay stack.
    - Concurrent build isolation: **PASS** — two overlays on the same FUSE mount with separate upper layers; no cross-contamination, FUSE lower layer untouched.
    - Synthetic SQLite DB: **PASS** — 450 store paths registered with real NAR hashes (sha256, computed via `nix-store --dump`). Nix reads the DB and resolves paths through the overlay.
    - `nix-build` under overlayfs: **PASS** — trivial derivation built successfully with `sandbox = true` inside a mount namespace with overlay bind-mounted onto `/nix/store`. The Nix sandbox (chroot, user/mount/PID namespaces) created a build environment using only store paths from the derivation's transitive closure, confirming the full FUSE → overlayfs → Nix sandbox → build chain works. Output appeared in the overlay upper layer.
    - FUSE read latency (standard mode, 74k files, 647 MB):

      | Concurrency | Direct p50 | FUSE p50 | Ratio | Direct p99 | FUSE p99 | Ratio |
      |---|---|---|---|---|---|---|
      | 1 | 2us | 20us | 10x | 6us | 46us | 7.7x |
      | 4 | 2us | 25us | 12.5x | 6us | 82us | 13.7x |
      | 16 | 2us | 103us | 51.5x | 7us | 215us | 30.7x |

    - FUSE read latency (passthrough mode, `fuser` 0.17, `FUSE_PASSTHROUGH`): **No improvement** over standard mode. Passthrough eliminates `read()` context switches but the benchmark bottleneck is `lookup()`/`open()` calls which still traverse userspace. Production `rio-fuse` should keep file handles open across reads to benefit from passthrough. See [spike findings](../components/worker.md#fuse-passthrough-mode-linux-69).
    - **Go/no-go: GO** — the full chain works. FUSE read overhead exceeds the 5x fail gate at all concurrency levels, but the overhead is dominated by per-file lookup/open calls (which still traverse userspace), not by fundamental FUSE read-path limitations. Known mitigations (file handle caching, multi-threaded FUSE dispatch, passthrough for sustained reads) address the bottleneck directly. The architecture is viable.
    - **Remaining gap:** The custom seccomp profile (`seccomp-spike.json`) was installed on nodes but the spike pod ran with `privileged: true` (required for `/dev/fuse` device cgroup access), which bypasses seccomp. The seccomp profile is not yet validated end-to-end; this requires a FUSE device plugin (e.g., `smarter-device-manager`) to provide `/dev/fuse` without requiring `privileged: true`.
- [x] **SSH server (`russh`)**: Accept SSH connections, negotiate channels, and pipe channel I/O to the protocol handler. Per-channel protocol state with independent handshake and option negotiation. Public key auth with explicit password rejection. Task abort on channel close via `Drop`.
- [x] Workspace scaffolding: 3 initial crates (rio-nix, rio-build, rio-proto stub)
- [x] Structured logging with tracing + JSON subscriber from day one. Root span carries `component` field per observability spec. Default format is JSON.
- [x] Basic metrics counters (connections, opcodes, errors). 7 gateway metrics: `connections_total`, `connections_active`, `opcodes_total`, `opcode_duration_seconds`, `handshakes_total`, `channels_active`, `errors_total`. Gauges properly decremented on cleanup via `Drop`.
- [x] CI pipeline with nix flake check (clippy, tests, docs, coverage)
- [x] `rio-nix` wire format: u64 LE, padded strings, collections. Safety bounds enforced (64 MiB max string, 1M max collection). Proptest roundtrips for all primitives (u64, bool, bytes, string, strings, string\_pairs). Fuzz targets for wire primitives and opcode payloads. Framed streams deferred to Phase 1b.
- [x] Store path types: parsing, validation, nixbase32. Private fields with accessor methods enforce invariants at construction time. `DerivedPath` parsing handles `path!*` and `path!out,dev` formats.
- [x] Hash types: SHA-256, SHA-512, SHA-1, truncated hashes. Private fields with accessors. `truncate_for_store_path` returns `Result` (no panics in library code). `FromStr` implemented for `HashAlgo`.
- [x] Handshake: full 4-phase byte-level sequence for 1.37+ (magic, version, features for ≥1.38, affinity, reserveSpace, version string, trusted status, STDERR\_LAST). Single canonical implementation via `server_handshake_split`. Tested for 1.37 (skip features) and 1.38 (full exchange), invalid magic, and version rejection.
- [x] STDERR streaming loop (all 8 message types: NEXT, READ, WRITE, LAST, ERROR, START\_ACTIVITY, STOP\_ACTIVITY, RESULT). Wire-level tests for each type. `StderrError::simple` parameterized with program name.
- [x] Read-only opcodes: `wopIsValidPath` (1), `wopQueryPathInfo` (26), `wopQueryValidPaths` (31), `wopAddTempRoot` (11), `wopSetOptions` (19). Additional opcodes beyond spec: `wopNarFromPath` (38), `wopQueryPathFromHashPart` (29), `wopAddSignatures` (37, stub), `wopQueryMissing` (40). Store errors send STDERR\_ERROR to client instead of silent connection close.
- [x] Unknown opcode handling (return STDERR_ERROR + close connection to prevent stream desync)
- [x] `rio-store`: in-memory backend for path metadata. `StorePath`-keyed HashMaps (no String allocation per lookup). Poisoned lock recovery. `import_from_nix_store` helper for dev/test pre-population.
- [x] Protocol conformance test suite: 104 tests total. Byte-level direct protocol tests for every implemented opcode. Integration tests against real `nix` CLI (`nix store info`, `nix path-info`, `nix store ls`). Structural golden tests for handshake and opcode response formats. Error path tests (missing NAR, invalid path format, multi-opcode sequences, DerivedPath format). Truncated/malformed wire input tests. Full UTF-8 proptest coverage.
- [x] Live-daemon golden conformance tests: each test starts an isolated nix-daemon, exchanges with it, and compares the response field-by-field against rio-build at the byte level. 8 scenarios (handshake, IsValidPath found/not-found, QueryPathInfo found/not-found, QueryValidPaths, AddTempRoot, QueryMissing) with STDERR activity stripping. Replaced stored binary fixtures to eliminate fixture staleness and silent-pass-on-missing-files. Also fixed three conformance bugs found through golden testing (SetOptions spurious result value, QueryPathInfo narHash wire format, QueryMissing opaque-path categorization).

## Milestones

- [x] **Week 2-3:** SSH handshake completes successfully
- [x] **Week 6-8:** `nix path-info --store ssh-ng://localhost /nix/store/...-hello` returns correct info
- [x] **Month 3:** `nix store ls --store ssh-ng://localhost /nix/store/...` works

### FUSE+Overlay Fallback Plan

If the FUSE+overlay spike fails (e.g., kernel incompatibility on EKS, unacceptable FUSE overhead), the fallback is a bind-mount approach with explicit store path materialization via `nix-store --realise`. This trades lazy loading for simplicity: all input paths are fully materialized on the worker's local disk before the build starts, and bind-mounted into the sandbox. Decision gate: end of Week 2.

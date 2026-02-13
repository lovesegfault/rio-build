# Phase 1a: Wire Format + Read-Only Protocol (Months 1-3)

**Goal:** Prove the protocol approach with a read-only store.

## Tasks

- [ ] **FUSE+overlay+sandbox platform validation (Week 1-2)**
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
- [ ] **SSH server (`russh`)**: Accept SSH connections, negotiate channels, and pipe channel I/O to the protocol handler. This is a prerequisite for the Week 2-3 handshake milestone.
- [ ] Workspace scaffolding: 3 initial crates (rio-nix, rio-build, rio-proto stub)
- [ ] Structured logging with tracing + JSON subscriber from day one
- [ ] Basic metrics counters (connections, opcodes, errors)
- [ ] CI pipeline with nix flake check
- [ ] `rio-nix` wire format: u64 LE, padded strings, framed streams
- [ ] Store path types: parsing, validation, nixbase32
- [ ] Hash types: SHA-256, SHA-512, SHA-1, truncated hashes
- [ ] Handshake: full byte-level sequence for 1.37+ (including affinity, reserveSpace, version string, trusted status)
- [ ] STDERR streaming loop (all 7 message types)
- [ ] Read-only opcodes: `wopIsValidPath` (1), `wopQueryPathInfo` (26), `wopQueryValidPaths` (31), `wopAddTempRoot` (11), `wopSetOptions` (19)
- [ ] Unknown opcode handling (return STDERR_ERROR)
- [ ] `rio-store`: in-memory backend for path metadata
- [ ] Protocol conformance test suite: record bytes from real nix-daemon, use as golden tests

## Milestones

- **Week 2-3:** SSH handshake completes successfully
- **Week 6-8:** `nix path-info --store ssh-ng://localhost /nix/store/...-hello` returns correct info
- **Month 3:** `nix store ls --store ssh-ng://localhost /nix/store/...` works

### FUSE+Overlay Fallback Plan

If the FUSE+overlay spike fails (e.g., kernel incompatibility on EKS, unacceptable FUSE overhead), the fallback is a bind-mount approach with explicit store path materialization via `nix-store --realise`. This trades lazy loading for simplicity: all input paths are fully materialized on the worker's local disk before the build starts, and bind-mounted into the sandbox. Decision gate: end of Week 2.

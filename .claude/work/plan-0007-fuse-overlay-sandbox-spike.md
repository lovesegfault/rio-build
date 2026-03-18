# Plan 0007: FUSE+overlay+sandbox platform validation spike

## Context

Phase 1a had two parallel tracks. P0000–P0006 proved the `ssh-ng://` protocol works. This plan proved the *worker* architecture works — FUSE mount serving store paths, overlayfs on top for build scratch space, synthetic SQLite database so Nix thinks the store is local, `nix-build` running inside the full sandbox inside all of that. The phase doc calls this the "Week 1-2 go/no-go gate": if this chain doesn't work on the target kernel, the whole design pivots to bind-mount materialization (the fallback plan).

This is a standalone crate (`rio-spike`) with zero dependency on `rio-nix` or `rio-build` — confirmed via `Cargo.toml`, it only uses `fuser`, `rusqlite`, `nix` (the syscall crate), and `tokio`. It depends on P0000 only for the workspace `Cargo.toml` root. The DAG forks here: P0007 is reachable from P0000 without going through any of P0001–P0006.

The spike ran on EKS AL2023 (kernel 6.12, c8a.xlarge, K8s 1.35) — results are recorded in the phase doc's "Results" table.

## Commits

- `891ccf4` — feat: add FUSE+overlay+sandbox platform validation spike (rio-spike)

Single commit. 4,018 insertions, 9 deletions, 27 files. Includes Rust source, Terraform, K8s manifests, and shell deployment scripts.

## Files

```json files
[
  {"path": "rio-spike/src/fuse_store.rs", "action": "NEW", "note": "passthrough FUSE daemon (fuser 0.17) with FUSE_PASSTHROUGH support"},
  {"path": "rio-spike/src/overlay.rs", "action": "NEW", "note": "overlayfs manager: per-build upper/work/merged directory setup + mount"},
  {"path": "rio-spike/src/synthetic_db.rs", "action": "NEW", "note": "synthetic Nix store SQLite (schema v10), real NAR hashes via nix-store --dump"},
  {"path": "rio-spike/src/benchmark.rs", "action": "NEW", "note": "FUSE read latency: standard vs passthrough, concurrency 1/4/16"},
  {"path": "rio-spike/src/validate.rs", "action": "NEW", "note": "full-chain validator: FUSE → overlay → synthetic DB → nix-build sandbox=true"},
  {"path": "rio-spike/src/main.rs", "action": "NEW", "note": "CLI dispatch: benchmark | validate | mount"},
  {"path": "rio-spike/Cargo.toml", "action": "NEW", "note": "standalone crate, no rio-nix/rio-build deps"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "add rio-spike to workspace members"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "document FUSE_PASSTHROUGH findings, set_max_stack_depth(1), ext4-backing requirement"},
  {"path": "docs/src/phases/phase1a.md", "action": "MODIFY", "note": "Results table: GO decision, latency numbers, remaining seccomp gap"}
]
```

Not listed (out of `rio-*/` scope but part of this commit): `infra/terraform/` (EKS + VPC + ECR), `infra/k8s/` (pod spec, seccomp profile), `scripts/deploy-spike.sh`.

## Design

The full chain under test:

1. **FUSE mount** — `fuser` 0.17 serving a directory tree at a mount point. Two modes: standard (every `read()` crosses the kernel/userspace boundary) and passthrough (`FUSE_PASSTHROUGH` — kernel reads directly from the backing file descriptor, bypassing userspace for the data path).
2. **overlayfs on top** — FUSE mount as `lowerdir`, local SSD directory as `upperdir` + `workdir`, `mount -t overlay` merges them. Writes go to upper, reads fall through to lower.
3. **Synthetic SQLite DB** — Nix's `/nix/var/nix/db/db.sqlite` schema (v10). 450 store paths registered with real NAR hashes computed by `nix-store --dump | sha256sum`. The DB lives in the overlay upper layer — Nix reads it, sees paths as "registered", resolves them through the overlay to the FUSE lower.
4. **`nix-build` with `sandbox = true`** — inside a mount namespace with the overlay bind-mounted onto `/nix/store`. Nix's sandbox creates chroot + user/mount/PID namespaces, using only the derivation's transitive closure. Build output lands in the overlay upper.

All four steps passed. The go/no-go gate said fail if FUSE p50 latency exceeds 5× direct reads — it exceeded 10× at concurrency 1 and 51× at concurrency 16 — but the decision was GO anyway because the bottleneck analysis showed the overhead is in `lookup()`/`open()` (which traverse userspace regardless of passthrough), not in the read path itself. File-handle caching in the production `rio-fuse` crate would keep handles open across reads, amortizing the open cost.

Six findings documented in `docs/src/components/worker.md`:

- **`FUSE_PASSTHROUGH` needs `set_max_stack_depth(1)`** — the fuser crate's default stack depth triggers `EPERM` on AL2023. One-line config.
- **Passthrough fails on overlay-backed files with `EPERM`** — the backing fd must be on ext4 (or another "real" filesystem), not another overlay. This constrains where the real store data can live.
- **Passthrough shows no improvement for open-heavy workloads** — the benchmark reads 74k files once each; `lookup()`/`open()` dominate. Passthrough helps when you open once and read many times.
- **`hostUsers: false` incompatible with `/dev/fuse` hostPath** — the K8s user-namespace feature blocks device hostPath mounts. Needs a device plugin (`smarter-device-manager` or similar).
- **`CAP_SYS_ADMIN` alone insufficient for `/dev/fuse`** — the device cgroup must also allow access. The spike pod ran `privileged: true` to bypass this, which also bypasses seccomp — the custom seccomp profile is installed but unvalidated.
- **Synthetic DB `Refs` table must be populated** — not just `ValidPaths`. Nix's sandbox uses `Refs` to compute the transitive closure; an empty `Refs` table makes every build fail with "path not in closure."

Concurrent-build isolation test: two overlays on the same FUSE lower, separate uppers, two `nix-build` runs in parallel. Outputs landed in the correct uppers, FUSE lower untouched. No cross-contamination.

## Tracey

Predates tracey adoption. The worker.md spec updates in this commit were later marked with `r[worker.fuse.passthrough]`, `r[worker.overlay.upper-fs]`, `r[worker.synthdb.refs]` when tracey was adopted. Run `tracey query rule worker.fuse` / `worker.overlay` / `worker.synthdb` for the retro-tagged set.

## Outcome

Go/no-go: **GO.** The FUSE → overlayfs → synthetic DB → sandbox chain works on the target EKS kernel. FUSE read overhead is high but tractable — the bottleneck is `lookup()`/`open()` userspace traversal, not a fundamental read-path limitation; file-handle caching addresses it. The architecture is viable. One remaining gap documented: seccomp profile installed but unvalidated (bypassed by `privileged: true`). This is the terminal commit of phase 1a.

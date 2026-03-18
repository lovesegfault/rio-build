# Plan 0148: Dev/prod overlay fixes + cgroup rw remount for non-privileged

## Design

First real-world deploy attempts to a dev k8s cluster surfaced a string of config-shaped failures. Each is a one-line fix; together they're "everything that breaks when you actually run `kubectl apply -k deploy/overlays/dev`."

**`e0b03b9` — docker tag `dev` for dev overlay.** Dev overlay was pulling `:latest`, which didn't exist in the local registry. Explicit `:dev` tag.

**`731047f` + `ed3f8ca` — cgroup rw remount.** Containerd mounts `/sys/fs/cgroup` read-only for non-privileged pods, even with `CAP_SYS_ADMIN`. Worker's `BuildCgroup::new` couldn't create leaf cgroups. First fix: `mount(MS_REMOUNT, ...)` to clear RO. Second fix: `MS_REMOUNT|MS_BIND` — `MS_REMOUNT` alone replaces ALL changeable flags (cleared `nosuid`/`nodev`/`noexec` as side effect, harmless for cgroup2fs but imprecise) AND fails if the runtime set RO via bind-remount path (per-mount-point flag) rather than superblock. `MS_REMOUNT|MS_BIND` modifies only per-mount-point flags. Added `TODO(phase4)`: this path is dead under `privileged: true` (the only shipped config) — revisit when non-privileged + device-plugin path (ADR-012) lands.

**`e3621e6` + `af36b0a` — gateway SSH host key + authorized_keys via secretGenerator.** Gateway Deployment expected `/etc/rio/ssh/host_key` and `/etc/rio/ssh/authorized_keys`. Dev overlay never created the Secret. Added `secretGenerator` in kustomization with placeholder keys (dev only — prod uses cert-manager or external secret).

**`72cb59e` + `107493f` + `1a36a6c` — figment `__` separator for nested env vars.** `store.chunk_backend.kind = "s3"` via env should be `RIO_CHUNK_BACKEND__KIND=s3` (double underscore → nested). Dev overlay had single underscore → figment ignored it → default `local` backend. Prod overlay had the value as a JSON string `'{"kind":"s3",...}'` → figment doesn't parse JSON from env for enum variants. Fixed both overlays. Added figment env-var tests in `rio-store/src/main.rs` for `ChunkBackendKind` tagged enum. `099d9bb` corrected stale claim in `nix/modules/store.nix` that "env vars can't express tagged enums" — they can, with `__`.

**`484d2fa` — actually set `privileged: true` on dev WorkerPool.** Dev `workerpool-example.yaml` had a comment "set privileged for local dev" but the field was commented out. Uncommented.

## Files

```json files
[
  {"path": "infra/overlays/dev/kustomization.yaml", "action": "MODIFY", "note": "docker tag dev + secretGenerator for ssh + figment __ env vars"},
  {"path": "infra/overlays/dev/workerpool-example.yaml", "action": "MODIFY", "note": "uncomment privileged: true"},
  {"path": "infra/overlays/dev/.gitignore", "action": "NEW", "note": "ignore generated ssh keys"},
  {"path": "infra/overlays/dev/ssh/README.md", "action": "NEW", "note": "dev key generation instructions"},
  {"path": "infra/overlays/prod/kustomization.yaml", "action": "MODIFY", "note": "figment __ env vars (JSON string doesn't parse)"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "MS_REMOUNT|MS_BIND cgroup rw remount + TODO(phase4)"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "figment env-var tests for ChunkBackendKind"},
  {"path": "rio-store/Cargo.toml", "action": "MODIFY", "note": "test deps"},
  {"path": "nix/modules/store.nix", "action": "MODIFY", "note": "correct stale claim about tagged enum env vars"},
  {"path": "nix/tests/phase2c.nix", "action": "MODIFY", "note": "use __ separator"}
]
```

## Tracey

No tracey markers landed. Deployment configuration fixes.

## Entry

- Depends on P0127: phase 3a complete (deploy overlays, WorkerPool CRD, cgroup code).
- Depends on P0132: dev/prod overlay structure exists.

## Exit

Merged as `e0b03b9..099d9bb` (10 commits). `.#ci` green at merge. This is the terminal plan of phase 3b — `phase-3b` tag points at `099d9bb`.

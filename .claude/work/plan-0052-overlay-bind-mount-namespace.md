# Plan 0052: Overlay bind-mount into daemon namespace — NIX_STORE_DIR env was broken

## Context

The pre-VM-test fix that made P0053 possible. P0031's `spawn_daemon_in_namespace` set `NIX_STORE_DIR={merged}/nix/store` as an environment variable. Two problems:

1. That path doesn't exist — FUSE presents store paths at the merged root, not under a `nix/store/` subdir.
2. More fundamentally: derivations hardcode `/nix/store/...` paths in their build scripts. The daemon's store root must be the canonical `/nix/store`. An env variable can't change what's baked into the derivation's ATerm.

The fix: `unshare(CLONE_NEWNS)` (already done), then **bind-mount the overlay merged dir AT `/nix/store`** in the daemon's mount namespace. Also bind-mount synth DB at `/nix/var/nix/db` and `nix.conf` at `/etc/nix`. Now the daemon's view of `/nix/store` IS the overlay — reads fall through to FUSE lower, writes copy-up to upper.

Without this, every build fails at "builder: /nix/store/{hash}-bash: No such file or directory" because the builder path resolves in the wrong mount namespace.

## Commits

- `3c4b260` — fix(rio-worker): bind-mount overlay into daemon namespace instead of broken NIX_STORE_DIR env

## Files

```json files
[
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "spawn_daemon_in_namespace pre_exec: bind-mount {merged}→/nix/store, {synth_db}→/nix/var/nix/db, {nix_conf}→/etc/nix; remove NIX_STORE_DIR env"},
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "upperdir is now {upper}/nix/store/ so outputs land where upload.rs scans"}
]
```

## Design

**Bind-mount in `pre_exec`:** `Command::pre_exec` runs in the forked child before `exec`. The child has already `unshare(CLONE_NEWNS)`'d. Three `mount --bind` calls: overlay merged → `/nix/store`, synth DB file → `/nix/var/nix/db/db.sqlite`, generated nix.conf → `/etc/nix/nix.conf`. These mounts are only visible in the daemon's namespace; the worker process sees the real host `/nix/store`.

**Upper dir shape:** overlay `upperdir` moved from `{upper}` to `{upper}/nix/store/`. Since the merged dir is bind-mounted at `/nix/store`, outputs written to `/nix/store/{hash}-out` land at `{upper}/nix/store/{hash}-out`. `upload.rs::scan_new_outputs` scans `{upper}/nix/store/` for store-path-shaped directories.

**Why not chroot?** A chroot would also need `/proc`, `/dev`, `/tmp`, the nix-daemon binary, glibc, etc. Bind-mounts are surgical — change only what needs changing. The daemon still sees the host's `/proc`, runs its own sandbox per-build.

This still isn't quite right — P0056 discovers that the daemon's sandbox (which bind-mounts `inputSrcs` into a chroot) needs the *host's* `/nix/store` stacked as an overlay lower, or the nix-daemon binary itself is unreachable after bind-mount.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[worker.executor.bind-mount-store]`.

## Outcome

Merged as `3c4b260`. Single commit. This was written specifically to enable P0053's VM test — it's the last thing that made `nix-build --store ssh-ng://control` in the VM even get past "builder not found."

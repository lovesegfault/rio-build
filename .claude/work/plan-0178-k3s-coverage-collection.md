# Plan 0178: k3s coverage collection — POD_NAME profraw, multi-node stop order, zstd images

## Design

Coverage collection from pods is trickier than from systemd units — pods restart with the same `LLVM_PROFILE_FILE` path, overwriting profraws. The standalone fixture's `collectCoverage` didn't work for k3s.

**POD_NAME in profraw path (`6f8c0e5`):** `LLVM_PROFILE_FILE=/var/lib/rio/cov/rio-%p-%m.profraw` → `rio-$(POD_NAME)-%p-%m.profraw`. Pod restarts get fresh names; no overwrite.

**Pod-level coverage (`f3a8fca`):** helm chart gains `coverage.enabled` knob that mounts a hostPath volume at `/var/lib/rio/cov` and sets `LLVM_PROFILE_FILE`. `collectCoverage` in `common.nix` learns to `kubectl cp` from pods.

**Wait for pods not just deploy (`7fd2f95`):** `rollout status` returns ~30-90s before the last pod is actually ready to tar.

**Multi-node stop order (`b294567`):** stop workers across ALL nodes before scheduler (standalone only stopped workers on the local node; k3s has workers on both).

**`rio-cli` profraws never processed (`633f4d6`):** `rio-cli` binary wasn't in `covBins` + had no `LLVM_PROFILE_FILE`.

**Filter k3s tests (`38167e0`):** nullglob-hardened profraw guard.

**Direct-FUSE coverage (`219bf0d`):** overlay doesn't delegate `readdir`/`access` to FUSE; add a direct-FUSE `ls` to cover those callbacks.

**zstd images + timeout headroom (`68b1192`, `076de36`):** coverage-instrumented binaries are larger; images compress to same tarball size with zstd; coverage runs need +30s per scenario.

## Files

```json files
[
  {"path": "nix/coverage.nix", "action": "MODIFY", "note": "rio-cli in covBins; POD_NAME in LLVM_PROFILE_FILE; filter k3s"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "collectCoverage: stop workers ALL nodes; kubectl cp from pods; wait for pods not deploy; nullglob guard"},
  {"path": "infra/helm/rio-build/values/vmtest-full.yaml", "action": "MODIFY", "note": "coverage.enabled knob; hostPath mount; POD_NAME env"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "zstd compression for coverage images"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "direct-FUSE ls covers readdir/access; cov timeout headroom"}
]
```

## Tracey

No tracey markers — coverage infrastructure.

## Entry

- Depends on P0173: k3s-full fixture (coverage mode for it)
- Depends on P0174: scheduler token-aware shutdown (profraw flush precondition)

## Exit

Merged as `38167e0`, `f3a8fca`, `7fd2f95`, `633f4d6`, `b294567`, `219bf0d`, `6f8c0e5`, `68b1192`, `076de36` (9 commits). k3s-full coverage collects cleanly.

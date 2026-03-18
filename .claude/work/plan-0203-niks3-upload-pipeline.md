# Plan 0203: niks3 upload pipeline rewrite — async hook/daemon/drain + OIDC batching

## Design

The niks3 post-build-hook from P0149 was synchronous: every built path invoked the hook → OIDC token exchange → S3 upload → continue. With ~500 paths per CI run, ~500 OIDC calls. OIDC rate-limited → uploads stalled.

**Async hook/daemon/drain (`86e6c24`):** post-build-hook now just writes the path to a queue file (append-only, millisecond); a separate daemon process drains the queue in batches. Pushes every built path, even on failure (the hook runs before the build result is known).

**OIDC batching (`644d74e`):** one OIDC token per batch, not per path. `flush` prints for visibility (buffered stdout hid progress). Stream daemon log to CI output. `subprocess.timeout` on the drain step so a stalled upload doesn't hang the whole CI job.

**Intermediate (`8cd9561`):** hook format tweaks.

These are the final three commits before the `phase-4a` tag.

## Files

```json files
[
  {"path": "nix/niks3-hook.py", "action": "MODIFY", "note": "append-to-queue (millisecond); batch uploads; one OIDC per batch; flush prints; stream daemon log"}
]
```

## Tracey

No tracey markers — CI infrastructure.

## Entry

- Depends on P0149: GitHub Actions matrix workflow (niks3 was part of that)

## Exit

Merged as `86e6c24`, `8cd9561`, `644d74e` (3 commits). `.#ci` green. OIDC calls per CI run: ~500 → ~5. **Terminal plan of phase 4a.** The `phase-4a` tag lands after `644d74e`.

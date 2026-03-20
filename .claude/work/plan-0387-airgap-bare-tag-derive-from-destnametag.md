# Plan 0387: Derive airgap bare-tag overrides from pulled.*.destNameTag

consol-mc180 finding. Two sites hardcode bare-tag image strings that MUST exactly match `nix/docker-pulled.nix` `finalImageName:finalImageTag` (containerd's airgap cache is tag-indexed, not digest-indexed — exact-string lookup):

| Site | Hardcoded bare-tag | Must match |
|---|---|---|
| [`k3s-full.nix:105`](../../nix/tests/fixtures/k3s-full.nix) | `"docker.io/envoyproxy/envoy:distroless-v1.37.1"` | [`docker-pulled.nix:96-97`](../../nix/docker-pulled.nix) `envoy-distroless` |
| [`vmtest-full-nonpriv.yaml:33`](../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml) | `ghcr.io/smarter-project/smarter-device-manager:v1.20.12` | [`docker-pulled.nix:64-65`](../../nix/docker-pulled.nix) `smarter-device-manager` |

`dockerTools.pullImage` exposes `destNameTag` on its output derivation — it's `"${finalImageName}:${finalImageTag}"`, exactly the bare-tag string containerd needs for cache-hit. Verified via `nix eval`:

```
$ nix eval --impure --expr '(pkgs.dockerTools.pullImage { finalImageName="foo/bar"; finalImageTag="v1.2.3"; ... }).destNameTag'
"foo/bar:v1.2.3"
```

Deriving from `pulled.<name>.destNameTag` eliminates the drift window: bumping `docker-pulled.nix` `finalImageTag` auto-bumps the override. This also **simplifies** [P0304](plan-0304-trivial-batch-p0222-harness.md)-T119/T136's lockstep-assert checks — there's no sync to assert when the tag is derived, not duplicated. T119/T136 become redundant (or degrade to "assert `pulled.*` is in `extraImages` ⇔ override is set", which is a different axis).

**YAML caveat:** `vmtest-full-nonpriv.yaml` is static YAML — can't reference Nix attrs. But the values-file layering goes through [`k3s-full.nix:88-123`](../../nix/tests/fixtures/k3s-full.nix)'s `helmRendered` block, which already passes `extraSet` overrides via `--set-string`. The `devicePlugin.image` override can move from the YAML file into `extraSet` in the Nix caller at [`default.nix:330-343`](../../nix/tests/default.nix), same as the `dashboard.envoyImage` override already does at `k3s-full.nix:105`.

## Entry criteria

- [P0379](plan-0379-envoy-image-digest-pin.md) merged (DONE per dag.jsonl) — the `k3s-full.nix:105` hardcode arrived with P0379's fix for the airgap/digest-pin cache-miss; this plan derives that hardcode instead

## Tasks

### T1 — `refactor(nix-tests):` k3s-full.nix — envoyImage override derives from pulled.envoy-distroless.destNameTag

MODIFY [`nix/tests/fixtures/k3s-full.nix:99-106`](../../nix/tests/fixtures/k3s-full.nix):

```nix
# Before:
// (pkgs.lib.optionalAttrs envoyGatewayEnabled {
  # containerd airgap cache is tag-indexed (docker-pulled.nix:54),
  # not digest-indexed. values.yaml:410 default has @sha256: suffix
  # (P0379 digest-pin) → exact-string miss → ImagePullBackOff.
  # Override to bare-tag to match preloaded finalImageTag. Same
  # pattern as vmtest-full-nonpriv.yaml:33 devicePlugin.image.
  "dashboard.envoyImage" = "docker.io/envoyproxy/envoy:distroless-v1.37.1";
})

# After:
// (pkgs.lib.optionalAttrs envoyGatewayEnabled {
  # containerd airgap cache is tag-indexed, not digest-indexed.
  # values.yaml default has @sha256: suffix (P0379 digest-pin) →
  # exact-string miss → ImagePullBackOff. Override to bare-tag
  # DERIVED from the preload FOD's finalImageName:finalImageTag
  # (destNameTag attr) — bumping docker-pulled.nix auto-bumps here.
  "dashboard.envoyImage" = pulled.envoy-distroless.destNameTag;
})
```

The `pulled` attrset is already in scope at `:41`. `destNameTag` is a plain string attribute on the FOD derivation (not `outPath` — doesn't force a build at eval time).

### T2 — `refactor(nix-tests):` default.nix — devicePlugin.image override via extraValues (derived)

MODIFY [`nix/tests/default.nix:330-343`](../../nix/tests/default.nix). Add `extraValues` to the `vm-security-nonpriv-k3s` fixture invocation so the `devicePlugin.image` override is Nix-derived instead of YAML-hardcoded:

```nix
vm-security-nonpriv-k3s = security.privileged-hardening-e2e {
  fixture = k3sFull {
    # Layer vmtest-full-nonpriv.yaml for workerPool.privileged:false +
    # devicePlugin.enabled + nodeSelector/tolerations null. The
    # devicePlugin.image override is passed via extraValues below
    # (DERIVED from the preload FOD — no drift window).
    extraValuesFiles = [
      ../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml
    ];
    # containerd airgap cache is tag-indexed (pullImage's finalImageTag),
    # not digest-indexed. Chart default is digest-pinned (@sha256:…) →
    # exact-string miss → ImagePullBackOff. destNameTag is
    # "${finalImageName}:${finalImageTag}" — the preloaded cache key.
    extraValues = {
      "devicePlugin.image" = pulled.smarter-device-manager.destNameTag;
    };
    extraImages = [ pulled.smarter-device-manager ];
  };
};
```

`pulled` is already in scope at `default.nix:72`. The `extraValues` attrset merges into `extraSet` at [`k3s-full.nix:107`](../../nix/tests/fixtures/k3s-full.nix) (`// extraValues` — caller wins, right-biased).

### T3 — `refactor(infra):` vmtest-full-nonpriv.yaml — drop hardcoded devicePlugin.image

MODIFY [`infra/helm/rio-build/values/vmtest-full-nonpriv.yaml:29-33`](../../infra/helm/rio-build/values/vmtest-full-nonpriv.yaml). Delete the `image:` key + its comment block — T2's `extraValues` now provides it. Keep `enabled: true` + `nodeSelector: null` + `tolerations: null`:

```yaml
devicePlugin:
  # vmtest-full.yaml disables this (smarter-device-manager image not in
  # the default airgap set). This scenario passes extraImages=[pulled.
  # smarter-device-manager], so flip it back on. The image override is
  # passed via extraValues in nix/tests/default.nix (derived from
  # pulled.smarter-device-manager.destNameTag — no drift window).
  enabled: true
  # VM nodes don't have rio.build/node-role labels or the worker taint.
  nodeSelector: null
  tolerations: null
```

### T4 — `refactor(nix-tests):` k3s-full.nix — postgresql tag via extraSet (optional — check at dispatch)

**CONDITIONAL.** [`vmtest-full.yaml:179-181`](../../infra/helm/rio-build/values/vmtest-full.yaml) sets `postgresql.image.tag` to match [`docker-pulled.nix:39`](../../nix/docker-pulled.nix) `bitnami-postgresql.finalImageTag = "18.3.0"`. Same drift-window. Check at dispatch whether moving this override into `k3s-full.nix` `extraSet` (unconditional — all k3s scenarios need postgres) is cleaner:

```nix
extraSet = {
  # Postgres tag matches the preloaded FOD. Bumping docker-pulled.nix
  # auto-bumps here.
  "postgresql.image.tag" = pulled.bitnami-postgresql.imageTag;
}
```

Note: `imageTag` not `destNameTag` here — the chart separates `repository` and `tag` keys. The `destNameTag` attr would give the full `docker.io/bitnami/postgresql:18.3.0` which doesn't match the chart's split schema. Use the raw `finalImageTag` passthru — check if `pullImage` exposes it as `.imageTag` or if extraction from `destNameTag` (`builtins.elemAt (builtins.split ":" ...) 2`) is needed.

If YAML-layering stays cleaner (one file, human-readable), skip T4 with rationale. The envoy/devicePlugin cases are unambiguous wins (they pass the FULL image ref, `destNameTag` is exactly right); postgres splits repo/tag so the derivation is slightly less clean.

### T5 — `docs(infra):` P0304-T119/T136 lockstep-asserts — note simplification

MODIFY [P0304](plan-0304-trivial-batch-p0222-harness.md) T119/T136 task-body paragraphs (no fence changes — prose note only). Add a `> **SIMPLIFIED by [P0387]:**` blockquote above each:

> **SIMPLIFIED by [P0387](plan-0387-airgap-bare-tag-derive-from-destnametag.md):** the bare-tag is now DERIVED from `pulled.<name>.destNameTag`, not hardcoded. There's no drift-to-assert. T119/T136 collapse to: assert `dashboard.envoyImage`/`devicePlugin.image` overrides are SET when the corresponding `pulled.*` appears in `extraImages` (preload-but-no-override = wasted preload; override-but-no-preload = `ImagePullBackOff`). Different axis, simpler check.

This is prose guidance for the T119/T136 dispatcher — NOT a T5 exit criterion (P0304 dispatches independently).

## Exit criteria

- `/nixbuild .#ci` green (or nextest-standalone clause-4c — `nix/tests/`-only change is clause-4a candidate if rust-tier drv-identical)
- `grep 'destNameTag' nix/tests/fixtures/k3s-full.nix` → ≥1 hit (T1: envoy override derived)
- `grep 'distroless-v1.37.1' nix/tests/fixtures/k3s-full.nix` → 0 hits (T1: hardcode gone)
- `grep 'destNameTag' nix/tests/default.nix` → ≥1 hit (T2: devicePlugin override derived)
- `grep '^  image:' infra/helm/rio-build/values/vmtest-full-nonpriv.yaml` → 0 hits (T3: devicePlugin.image hardcode removed from YAML)
- `nix eval .#checks.x86_64-linux.vm-security-nonpriv-k3s.drvPath --raw` succeeds (eval-time — `destNameTag` is a string attr, doesn't force the FOD build)
- Render-check: manually invoke helm-render with the post-T2 `extraValues` and grep the output for `ghcr.io/smarter-project/smarter-device-manager:v1.20.12` → ≥1 hit (derived tag matches what the hardcode was)
- `grep 'SIMPLIFIED by' .claude/work/plan-0304-*.md` → ≥2 hits (T5: T119+T136 both annotated)

## Tracey

No new markers. This is drift-elimination plumbing — `destNameTag` derivation is a Nix-evaluation-time string interpolation, not a spec'd behavior. The airgap cache-key semantics are documented in [`docker-pulled.nix:33-37`](../../nix/docker-pulled.nix) inline comments; no `r[...]` marker covers them (infra conventions, not component spec).

## Files

```json files
[
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "T1: :105 'docker.io/envoyproxy/envoy:distroless-v1.37.1' hardcode → pulled.envoy-distroless.destNameTag; :100-104 comment update"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T2: :330-343 vm-security-nonpriv-k3s +extraValues devicePlugin.image=pulled.smarter-device-manager.destNameTag"},
  {"path": "infra/helm/rio-build/values/vmtest-full-nonpriv.yaml", "action": "MODIFY", "note": "T3: delete :29-33 image: hardcode + comment; keep enabled/nodeSelector/tolerations"},
  {"path": ".claude/work/plan-0304-trivial-batch-p0222-harness.md", "action": "MODIFY", "note": "T5: +SIMPLIFIED blockquote above T119 (:2005) and T136 (:2178) task prose — lockstep-asserts simplified to preload⇔override-set check"}
]
```

```
nix/tests/
├── fixtures/k3s-full.nix           # T1: :105 derive envoy destNameTag
└── default.nix                     # T2: :330-343 derive devicePlugin destNameTag
infra/helm/rio-build/values/
└── vmtest-full-nonpriv.yaml        # T3: drop image: hardcode
.claude/work/
└── plan-0304-*.md                  # T5: T119/T136 simplification note
```

## Dependencies

```json deps
{"deps": [379], "soft_deps": [369, 304, 283], "note": "discovered_from=consol-mc180. P0379 (DONE) added the k3s-full.nix:105 hardcode this plan derives — without P0379 there's no envoy override to refactor. P0369 (DONE, soft-dep) added the devicePlugin digest-pin context. Soft-dep P0304-T119/T136 (lockstep-asserts): T5 here annotates both with SIMPLIFIED blockquotes; T119/T136 stay in P0304 but collapse to preload⇔override-set axis. If P0304 dispatches T119/T136 BEFORE this plan, they implement the old-style string-compare assert; after this plan merges, that assert trivially passes (derived string == derived string). Sequence-independent but this-first is cleaner. Soft-dep P0283 (UNIMPL — dashboard image preload conditional at k3s-full.nix:166; touches same file, non-overlapping section :99-106 vs :166). k3s-full.nix collision=10, default.nix collision=15 — both mid-traffic; T1/T2 edits are localized inside existing optionalAttrs/fixture blocks, low conflict surface."}
```

**Depends on:** [P0379](plan-0379-envoy-image-digest-pin.md) — DONE; the `k3s-full.nix:105` hardcode arrived with it.

**Conflicts with:** [`k3s-full.nix`](../../nix/tests/fixtures/k3s-full.nix) collision=10. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T132 touches `:177` `extraImagesBumpGiB` (different section). [P0283](plan-0283-dashboard-image-preload.md) touches `:166` dashboard conditional (different section). T1 here touches `:99-106`. All non-overlapping. [`default.nix`](../../nix/tests/default.nix) collision=15 — T2 adds `extraValues` inside the `vm-security-nonpriv-k3s` block at `:330-343`; no other UNIMPL plan edits this specific fixture invocation.

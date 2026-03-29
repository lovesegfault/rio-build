# Plan 980824203: Generalize rio.optBool → rio.optScalar for int/bool optional fields

[P0468](plan-0468-helm-optbool-helper.md) added `rio.optBool` to [`_helpers.tpl:215`](../../infra/helm/rio-build/templates/_helpers.tpl) because `{{- with .hostUsers }}` skips falsy values — `hostUsers: false` in values produced NO key, and the controller default (`true`) won. The `hasKey`-based helper fixed that for booleans.

But [`builderpool.yaml:43`](../../infra/helm/rio-build/templates/builderpool.yaml) still uses `{{- with $bp.terminationGracePeriodSeconds }}` — an **integer** field. `terminationGracePeriodSeconds: 0` is valid k8s (immediate SIGKILL on delete), but `with` treats `0` as falsy and skips the key. Same bug, different type.

The fix is to generalize `rio.optBool` → `rio.optScalar` (or add a sibling `rio.optInt`) so any scalar field where zero/false is a meaningful value renders correctly. Discovered during [P0468](plan-0468-helm-optbool-helper.md) review.

## Entry criteria

- [P0468](plan-0468-helm-optbool-helper.md) merged (`rio.optBool` exists at `_helpers.tpl:215`). DONE on `sprint-1`.

## Tasks

### T1 — `fix(helm):` add rio.optScalar helper (or rio.optInt sibling)

MODIFY [`infra/helm/rio-build/templates/_helpers.tpl`](../../infra/helm/rio-build/templates/_helpers.tpl). Either generalize the existing `rio.optBool` to `rio.optScalar`:

```gotmpl
{{/*
rio.optScalar — renders `key: value` only when the context has the key.
Uses hasKey instead of `with` so zero/false/"" render correctly.
Usage: {{- include "rio.optScalar" (list $ctx "fieldName" $ctx.fieldName) | nindent 2 }}
*/}}
{{- define "rio.optScalar" -}}
{{- $ctx := index . 0 -}}
{{- $key := index . 1 -}}
{{- $val := index . 2 -}}
{{- if hasKey $ctx $key }}
{{ $key }}: {{ $val }}
{{- end }}
{{- end }}
```

…and keep `rio.optBool` as a deprecated alias, OR add `rio.optInt` as a sibling if the rendering differs (it shouldn't — YAML scalars are uniform).

### T2 — `fix(helm):` apply at builderpool.yaml terminationGracePeriodSeconds

MODIFY [`infra/helm/rio-build/templates/builderpool.yaml:43`](../../infra/helm/rio-build/templates/builderpool.yaml). Replace:

```gotmpl
{{- with $bp.terminationGracePeriodSeconds }}
terminationGracePeriodSeconds: {{ . }}
{{- end }}
```

With:

```gotmpl
{{- include "rio.optScalar" (list $bp "terminationGracePeriodSeconds" $bp.terminationGracePeriodSeconds) | nindent 2 }}
```

Audit other templates for the same pattern — grep for `with .*Seconds\|with .*Count\|with .*Replicas` (integer fields likely to have meaningful zero).

### T3 — `test(helm):` helm-lint assert for terminationGracePeriodSeconds: 0

MODIFY [`flake.nix`](../../flake.nix) helm-lint check (or wherever the P0468 `hostUsers:false` assert landed — commit `1c392fa8`). Add a parallel assert: `terminationGracePeriodSeconds: 0` in test values → rendered YAML contains `terminationGracePeriodSeconds: 0`.

## Exit criteria

- `helm template rio-build infra/helm/rio-build --set builderPools[0].terminationGracePeriodSeconds=0 | grep 'terminationGracePeriodSeconds: 0'` → match
- `grep 'rio.optScalar\|rio.optInt' infra/helm/rio-build/templates/_helpers.tpl` → ≥1 hit
- `grep 'with.*terminationGracePeriodSeconds' infra/helm/rio-build/templates/builderpool.yaml` → 0 hits (replaced)
- flake.nix helm-lint assert for `terminationGracePeriodSeconds: 0` passes
- `/nbr .#ci` green

## Tracey

No domain markers — helm templating correctness, not rio-* behavior. P0468's precedent: no tracey for helm helpers.

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/_helpers.tpl", "action": "MODIFY", "note": "T1: rio.optScalar helper"},
  {"path": "infra/helm/rio-build/templates/builderpool.yaml", "action": "MODIFY", "note": "T2: apply at :43 terminationGracePeriodSeconds"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T3: helm-lint assert for int-zero rendering"}
]
```

```
infra/helm/rio-build/templates/
├── _helpers.tpl        # T1: rio.optScalar
└── builderpool.yaml    # T2: apply
flake.nix               # T3: assert
```

## Dependencies

```json deps
{"deps": [468], "soft_deps": [], "note": "Extends P0468's rio.optBool to cover integers."}
```

**Depends on:** [P0468](plan-0468-helm-optbool-helper.md) — `rio.optBool` at `_helpers.tpl:215` is the template to generalize.

**Conflicts with:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T980824202 adds a helm-lint assert for `hostUsers` in the same `flake.nix` region. Both are additive — should merge cleanly.

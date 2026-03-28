# Plan 973647702: CONSOLIDATION — helm `rio.optBool` helper (live correctness bug)

Consolidator finding with a **live correctness bug** inside: [`fetcherpool.yaml:26`](../../infra/helm/rio-build/templates/fetcherpool.yaml) uses `{{- with $fp.hostUsers }}` to render the optional `hostUsers` field. Helm's `with` is falsy on `false` — setting `hostUsers: false` in values produces NO `hostUsers:` key in the rendered CR, which means the controller's default applies instead of the explicit `false`. [`builderpool.yaml:37`](../../infra/helm/rio-build/templates/builderpool.yaml) does it correctly with `{{- if hasKey $bp "hostUsers" }}` — renders the key whenever it's SET, regardless of value.

This is the classic Helm optional-bool footgun. Four callsites across the chart have this pattern (hostUsers, possibly privileged, hostNetwork, topologySpread — grep at dispatch). Extract a `_helpers.tpl` named template `rio.optBool` that wraps the `hasKey` check, apply at all sites.

## Tasks

### T1 — `fix(helm):` add rio.optBool named template to _helpers.tpl

MODIFY [`infra/helm/rio-build/templates/_helpers.tpl`](../../infra/helm/rio-build/templates/_helpers.tpl). Add:

```gotmpl
{{/*
Render an optional boolean field. Unlike `with`, this renders when the
value is explicitly `false`. Usage:
  {{- include "rio.optBool" (list $ctx "hostUsers" $ctx.hostUsers) | nindent 2 }}
*/}}
{{- define "rio.optBool" -}}
{{- $ctx := index . 0 -}}
{{- $key := index . 1 -}}
{{- $val := index . 2 -}}
{{- if hasKey $ctx $key }}
{{ $key }}: {{ $val }}
{{- end }}
{{- end -}}
```

The list-arg pattern is standard Helm for multi-arg partials. `$ctx` is the map to `hasKey` against (e.g., `$fp` or `$bp`); `$key` is the field name; `$val` is the value to render.

### T2 — `fix(helm):` apply rio.optBool at fetcherpool.yaml + sweep other with-bool sites

MODIFY [`infra/helm/rio-build/templates/fetcherpool.yaml`](../../infra/helm/rio-build/templates/fetcherpool.yaml) at `:26`:

```diff
-  {{- with $fp.hostUsers }}
-  hostUsers: {{ . }}
-  {{- end }}
+  {{- include "rio.optBool" (list $fp "hostUsers" $fp.hostUsers) | nindent 2 }}
```

MODIFY [`infra/helm/rio-build/templates/builderpool.yaml`](../../infra/helm/rio-build/templates/builderpool.yaml) at `:37` — already correct (`hasKey`), but migrate to the helper for consistency.

Sweep: `grep -n 'with.*\.\(hostUsers\|privileged\|hostNetwork\|topologySpread\|readOnly\)' infra/helm/rio-build/templates/` — every hit is a potential `with`-on-bool bug. Apply the helper.

### T3 — `test(helm):` helm-lint assert — hostUsers:false renders

MODIFY [`flake.nix`](../../flake.nix) in the `helm-lint` check (search for `enableMutatingMethods` — that's an existing assert in the same block). Add:

```bash
# rio.optBool: explicit false MUST render (with-on-bool footgun guard)
rendered=$(helm template . -f values/vmtest-full-nonpriv.yaml --set fetcherPool.hostUsers=false)
echo "$rendered" | yq 'select(.kind=="FetcherPool") | .spec.hostUsers' | grep -q '^false$' \
  || { echo "FAIL: hostUsers:false did not render (with-on-bool bug)"; exit 1; }
```

## Exit criteria

- `/nixbuild .#ci` → green
- `grep -c 'rio.optBool' infra/helm/rio-build/templates/_helpers.tpl` → ≥1 (helper defined)
- `grep 'with.*\.hostUsers' infra/helm/rio-build/templates/*.yaml` → 0 hits (all migrated)
- `helm template . --set fetcherPool.hostUsers=false | yq 'select(.kind=="FetcherPool").spec.hostUsers'` → `false` (explicit false renders)
- `helm template . | yq 'select(.kind=="FetcherPool").spec | has("hostUsers")'` → `false` (unset stays unset — no spurious key)

## Tracey

No markers — helm templating correctness, no spec behavior. The `hostUsers` field's semantics are covered by `r[ctrl.crd.host-users-network-exclusive]` ([`controller.md:380`](../../docs/src/components/controller.md)) but this plan fixes the RENDERING, not the semantics.

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/_helpers.tpl", "action": "MODIFY", "note": "T1: add rio.optBool named template"},
  {"path": "infra/helm/rio-build/templates/fetcherpool.yaml", "action": "MODIFY", "note": "T2: :26 with→rio.optBool (LIVE BUG FIX)"},
  {"path": "infra/helm/rio-build/templates/builderpool.yaml", "action": "MODIFY", "note": "T2: :37 hasKey→rio.optBool (consistency)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T3: helm-lint assert hostUsers:false renders"}
]
```

```
infra/helm/rio-build/templates/
├── _helpers.tpl               # T1: rio.optBool
├── fetcherpool.yaml           # T2: bug fix
└── builderpool.yaml           # T2: consistency
flake.nix                      # T3: helm-lint assert
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [459], "note": "Consolidator finding — LIVE BUG at fetcherpool.yaml:26 (with-on-bool drops explicit false). No hard deps — _helpers.tpl and both pool yamls exist. Soft-dep P0459 (netpol/scheduling hardening touched fetcherpool.yaml — check for rebase conflict at dispatch, likely non-overlapping since P0459 edits the DS scheduling block not the spec block). discovered_from=consolidator."}
```

**Depends on:** none — all files exist.
**Soft-dep:** [P0459](plan-0459-adr019-netpol-scheduling-hardening.md) — touched `fetcherpool.yaml` (DS scheduling); non-overlapping with `:26` spec block.
**Conflicts with:** [`flake.nix`](../../flake.nix) helm-lint block — P0311-T48 (GRPCRoute method-count assert), P0311-T51 (yq thirdparty-image loop), P0304-T86 (`/tmp`→`$TMPDIR`), P0304-T136 (envoy lockstep). All additive bash asserts in the same `checkPhase`; append-only, rebase-clean. Check at dispatch.

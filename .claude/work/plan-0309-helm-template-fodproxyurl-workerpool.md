# Plan 0309: Helm — template fodProxyUrl into workerpool.yaml

[P0243](plan-0243-vm-fod-proxy-scenario.md)'s VM test works around a templating gap with a manual `kubectl patch` at [`fod-proxy.nix:116`](../../nix/tests/scenarios/fod-proxy.nix) (p243 worktree). The CRD field exists at [`workerpool.rs:248`](../../rio-controller/src/crds/workerpool.rs) (`pub fod_proxy_url: Option<String>`), the controller reads it, the worker honors it ([`config.rs:100`](../../rio-worker/src/config.rs) `fod_proxy_url: Option<String>`). Only the Helm template is missing the field.

The `TODO(P0243)` at [`fod-proxy.nix:103`](../../nix/tests/scenarios/fod-proxy.nix) even supplies the exact template line:
```
{{ if .Values.fodProxy.enabled }}fodProxyUrl: http://rio-fod-proxy:3128{{ end }}
```

~5 lines in [`workerpool.yaml`](../../infra/helm/rio-build/templates/workerpool.yaml), ~3 lines in [`values.yaml`](../../infra/helm/rio-build/values.yaml), retag the TODO, delete the kubectl-patch workaround.

## Entry criteria

- [P0243](plan-0243-vm-fod-proxy-scenario.md) merged (`fod-proxy.nix` exists with the `TODO(P0243)` + kubectl-patch workaround)

## Tasks

### T1 — `feat(helm):` template fodProxyUrl field

MODIFY [`infra/helm/rio-build/templates/workerpool.yaml`](../../infra/helm/rio-build/templates/workerpool.yaml) — add after `sizeClass` (near `:24`), before `image`:

```yaml
  {{- if .Values.fodProxy.enabled }}
  # Forward-proxy URL for FOD builds. Controller reads this from the
  # WorkerPool spec (crds/workerpool.rs:248) and injects it as
  # RIO_FOD_PROXY_URL into the worker env. Worker only forwards
  # http_proxy/https_proxy to the daemon when is_fixed_output=true.
  fodProxyUrl: {{ .Values.fodProxy.url | default (printf "http://rio-fod-proxy.%s.svc:3128" .Values.namespace.name) }}
  {{- end }}
```

The `default` uses the in-cluster Service DNS name. The test's hardcoded `http://rio-fod-proxy:3128` works inside the same namespace with search-domain resolution; the FQDN form is more explicit.

### T2 — `feat(helm):` values.yaml fodProxy block

MODIFY [`infra/helm/rio-build/values.yaml`](../../infra/helm/rio-build/values.yaml) — add a top-level `fodProxy` block (check if P0243 already added one for the squid Deployment; if so, just add the `url` override key):

```yaml
fodProxy:
  enabled: false
  # Override the default in-cluster URL if the proxy lives elsewhere.
  # url: http://squid.egress.svc:3128
  # allowedDomains populated by P0243 — confirm the block exists and
  # merge this key in.
```

### T3 — `test(nix):` drop kubectl-patch workaround

MODIFY [`nix/tests/scenarios/fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix):

1. At `:103` — retag `TODO(P0243)` → close it (delete the TODO comment block; the gap is now filled)
2. At `:102-117` (the `kubectl patch` block) — delete it. With T1's template, `helm install` with `fodProxy.enabled=true` in `extraValues` wires `WorkerPool.spec.fodProxyUrl` automatically.
3. Add an assertion that the field landed:
   ```python
   # Helm now templates fodProxyUrl directly — no patch needed.
   fod_url = k3s_server.succeed(
       "k3s kubectl get workerpool default -n ${ns} "
       "-o jsonpath='{.spec.fodProxyUrl}'"
   ).strip()
   assert "rio-fod-proxy" in fod_url, f"expected proxy URL, got {fod_url!r}"
   ```

### T4 — `test(helm):` template unit test

If `infra/helm/rio-build/tests/` exists (helm-unittest or similar), add a case: `fodProxy.enabled=true` → rendered `workerpool.yaml` contains `fodProxyUrl: http://rio-fod-proxy.rio-system.svc:3128`. If no unit-test harness exists, skip — T3's VM assertion covers it.

## Exit criteria

- `/nbr .#ci` green — `vm-fod-proxy` scenario passes without the kubectl-patch
- `grep 'TODO(P0243)' nix/tests/scenarios/fod-proxy.nix` → 0 hits for the workerpool-template TODO (the `:285` followup TODO is [P0308](plan-0308-fod-buildresult-propagation-namespace-hang.md)'s to close)
- `grep 'kubectl patch.*fodProxyUrl' nix/tests/scenarios/fod-proxy.nix` → 0 hits
- `helm template infra/helm/rio-build --set fodProxy.enabled=true | grep fodProxyUrl` → 1 hit
- `helm template infra/helm/rio-build --set fodProxy.enabled=false | grep fodProxyUrl` → 0 hits (conditional works)

## Tracey

References existing markers:
- `r[worker.fod.verify-hash]` — T3's VM assertion indirectly verifies the full wiring chain (`values.yaml` → Helm → CRD → controller → worker env → daemon spawn). The marker at [`worker.md:297`](../../docs/src/components/worker.md) already describes `WorkerPool.spec.fodProxyUrl` → `spawn_daemon_in_namespace` injection. No new markers.

## Files

```json files
[
  {"path": "infra/helm/rio-build/templates/workerpool.yaml", "action": "MODIFY", "note": "T1: fodProxyUrl conditional field near :24"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T2: fodProxy.url override key (merge into P0243's fodProxy block)"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T3: delete kubectl-patch workaround at :102-117, close TODO(P0243) at :103, add spec.fodProxyUrl assertion"}
]
```

```
infra/helm/rio-build/
├── templates/workerpool.yaml     # T1: {{- if .Values.fodProxy.enabled }}
└── values.yaml                   # T2: fodProxy.url override
nix/tests/scenarios/fod-proxy.nix # T3: drop patch, assert templated field
```

## Dependencies

```json deps
{"deps": [243], "soft_deps": [0308], "note": "~5-line helm fix. fod-proxy.nix:103 TODO supplies the exact template line. CRD field exists (workerpool.rs:248). discovered_from=P0243. Soft-dep on P0308: both edit fod-proxy.nix but non-overlapping sections (:103-117 here, :285 there) — can land in either order."}
```

**Depends on:** [P0243](plan-0243-vm-fod-proxy-scenario.md) — `fod-proxy.nix` + the TODO anchor. **P0243 gated in merge queue.**
**Conflicts with:** [`fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) also edited by [P0308](plan-0308-fod-buildresult-propagation-namespace-hang.md) (the `:285` TODO) — non-overlapping sections, either order works. [`workerpool.yaml`](../../infra/helm/rio-build/templates/workerpool.yaml) and [`values.yaml`](../../infra/helm/rio-build/values.yaml) low-traffic.

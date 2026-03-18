# Plan 0283: Dashboard VM smoke — curl only, no Playwright

**De-risks R9 by not taking the risk.** Playwright = ~500MB chromium FOD + font-config sandbox issues + version-lock brittleness + zero precedent in `nix/tests/`. curl proves: (a) nginx serves SPA, (b) nginx→Envoy→scheduler chain works, (c) SPA routing fallback works. That IS the integration surface. Rendering is covered by vitest.

## Entry criteria

- [P0273](plan-0273-envoy-sidecar-grpc-web.md) merged (Envoy config exists)
- [P0282](plan-0282-docker-helm-dashboard-deploy.md) merged (nginx image + conf exists)

## Tasks

### T1 — `test(vm):` dashboard node in standalone fixture

MODIFY [`nix/tests/fixtures/standalone.nix`](../../nix/tests/fixtures/standalone.nix) — optional `dashboard` node: nginx serving `rioDashboard` + Envoy sidecar process translating to scheduler's mTLS port. VM-internal hostnames.

### T2 — `test(vm):` dashboard.nix scenario

NEW [`nix/tests/scenarios/dashboard.nix`](../../nix/tests/scenarios/dashboard.nix):

```nix
# r[verify dash.journey.build-to-logs]  (col-0 header — proves end-to-end chain works)
{ fixture, ... }: {
  name = "dashboard-smoke";
  testScript = ''
    start_all()
    scheduler.wait_for_unit("rio-scheduler")
    dashboard.wait_for_unit("nginx")
    dashboard.wait_for_unit("envoy")
    dashboard.wait_for_open_port(80)

    # SPA served: index.html has Svelte mount point
    dashboard.succeed("curl -sf http://localhost/ | grep -q 'id=\"app\"'")

    # SPA routing fallback: /builds/xyz returns index.html (try_files)
    dashboard.succeed("curl -sf http://localhost/builds/nonexistent | grep -q 'id=\"app\"'")

    # gRPC-Web through nginx → Envoy → scheduler: DATA frame
    dashboard.succeed(
        "curl -sf -X POST http://localhost/rio.admin.AdminService/ClusterStatus "
        "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
        r"--data-binary $'\x00\x00\x00\x00\x00' "
        "| xxd | head -1 | grep -q '^00000000: 00'"
    )

    # Server-streaming through nginx (proxy_buffering off) → Envoy: 0x80 trailer
    dashboard.succeed(
        "printf '\\x00\\x00\\x00\\x00\\x0a\\x0a\\x08nonexist' > /tmp/req.bin && "
        "curl -sf -X POST http://localhost/rio.admin.AdminService/GetBuildLogs "
        "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
        "--data-binary @/tmp/req.bin | xxd | tail -5 | grep -q ' 80'"
    )
  '';
}
```

### T3 — `test(vm):` register scenario

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix) — add `vm-dashboard-standalone` (standalone fixture, not k3s-full).

## Exit criteria

- `/nbr .#checks.x86_64-linux.vm-dashboard-standalone` passes
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[dash.journey.build-to-logs]` — T2 VM-verifies the end-to-end chain (nginx→Envoy→scheduler streaming). Full journey at unit level in P0278+P0279+P0280; this proves deployment chain.

## Files

```json files
[
  {"path": "nix/tests/fixtures/standalone.nix", "action": "MODIFY", "note": "T1: optional dashboard node (nginx + Envoy processes)"},
  {"path": "nix/tests/scenarios/dashboard.nix", "action": "NEW", "note": "T2: curl-only smoke — 4 assertions"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T3: register (soft-conflict P0254/P0268, one line each)"}
]
```

```
nix/tests/
├── fixtures/standalone.nix       # T1: dashboard node option
├── scenarios/dashboard.nix       # T2: NEW curl smoke
└── default.nix                   # T3: register
```

## Dependencies

```json deps
{"deps": [273, 282], "soft_deps": [254, 268], "note": "NO Playwright (R9). curl-only proves nginx→Envoy→scheduler chain. default.nix soft-conflict with P0254/P0268 — one-line additions each. USER A7: grep id='app' (Svelte) not id='root' (React)."}
```

**Depends on:** [P0273](plan-0273-envoy-sidecar-grpc-web.md) — Envoy config. [P0282](plan-0282-docker-helm-dashboard-deploy.md) — nginx conf + image.
**Conflicts with:** `default.nix` soft-conflict with [P0254](plan-0254-ca-metrics-vm-demo.md)/[P0268](plan-0268-chaos-harness-toxiproxy.md) (all one-line adds).

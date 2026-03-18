# Plan 0243: VM FOD-proxy scenario + registration

phase4c.md:48-49 + **Q4 default accepted** — assert BOTH build-failure AND `TCP_DENIED` log-grep. Build-failure proves *something* blocked the fetch; log-grep proves *Squid* blocked it (not a network misconfig).

**Scope shrink (GT4):** `dockerImages.fod-proxy` already exists at [`nix/docker.nix:192-224`](../../nix/docker.nix) — squid + cacert image. **Scope shrink (GT5):** [`infra/helm/rio-build/templates/fod-proxy.yaml`](../../infra/helm/rio-build/templates/fod-proxy.yaml) already exists. This plan is **scenario-only**.

**R8 — Squid ConfigMap hot-reload.** Squid does NOT watch its config file. After patching the ConfigMap allowlist, `kubectl delete pod <squid>` to force a restart with the new config. OR check if [`fod-proxy.yaml:11`](../../infra/helm/rio-build/templates/fod-proxy.yaml) supports `extraAllowlist` helm value (avoids the restart).

**Soft conflict with [P0241](plan-0241-vm-section-g-netpol.md) on `nix/tests/default.nix`** — both add 1 import + 1 mkTest. Coordinator dispatches sequentially; NOT a dag dep.

## Tasks

### T1 — `test(vm):` fod-proxy.nix scenario

NEW `nix/tests/scenarios/fod-proxy.nix`:

```nix
{ pkgs, mkTest, k3sFull, helmRender, drvs, ... }:
mkTest {
  name = "fod-proxy";
  fixture = k3sFull;
  helmValues = {
    fodProxy.enabled = true;
    # If the chart supports it, add the local origin here:
    # fodProxy.extraAllowlist = [ "localhost:8081" ];
  };

  # Local origin: busybox httpd serving a known file. Pattern from drvs.coldBootstrapServer.
  extraNodes.origin = { ... }: {
    services.nginx = {  # or busybox httpd — whichever is cheaper
      enable = true;
      virtualHosts."localhost".locations."/fixture.tar.gz".return = "200 'fake-tarball-bytes'";
    };
    networking.firewall.allowedTCPPorts = [ 8081 ];
  };

  testScript = ''
    import time

    # Wait for FOD-proxy pod ready
    server.wait_until_succeeds("kubectl get pod -l app=fod-proxy | grep Running", timeout=120)

    # ---- Assert 1: allowlisted FOD → success + TCP_MISS ----
    with subtest("fod-proxy-allowed: allowlisted fetch succeeds"):
        # If extraAllowlist helm value isn't supported, patch the ConfigMap then restart squid:
        server.succeed("kubectl patch configmap fod-proxy-config --type merge -p "
                       "'{\"data\":{\"allowlist.conf\":\"acl allowed dstdomain origin\\nhttp_access allow allowed\"}}'")
        # R8: squid doesn't hot-reload — restart the pod
        server.succeed("kubectl delete pod -l app=fod-proxy")
        server.wait_until_succeeds("kubectl get pod -l app=fod-proxy | grep Running", timeout=60)

        # Submit FOD build fetching from origin
        build_id = submit_build(server, drv_path="${drvs.fodFetchFromOrigin}")
        server.wait_until_succeeds(f"rio-cli build-status {build_id} | grep -q Succeeded", timeout=120)

        # Squid log: TCP_MISS (proxy forwarded the request)
        server.succeed("kubectl logs -l app=fod-proxy | grep -q TCP_MISS")
        print("fod-proxy-allowed PASS: build succeeded + TCP_MISS in squid log")

    # ---- Assert 2: non-allowlisted FOD → fail + TCP_DENIED ----
    with subtest("fod-proxy-denied: non-allowlisted fetch fails"):
        # Submit FOD build fetching from 1.1.1.1 (not allowlisted)
        build_id = submit_build(server, drv_path="${drvs.fodFetchFromPublic}")

        # Build fails (fetch blocked)
        server.wait_until_succeeds(f"rio-cli build-status {build_id} | grep -q Failed", timeout=120)

        # Q4: ALSO assert TCP_DENIED in squid log (proves squid blocked it, not network misconfig)
        server.succeed("kubectl logs -l app=fod-proxy | grep -q 'TCP_DENIED/403'")
        print("fod-proxy-denied PASS: build failed + TCP_DENIED/403 in squid log")

    # ---- Assert 3: non-FOD → no http_proxy env ----
    with subtest("fod-proxy-nonfod: non-FOD has no http_proxy"):
        # Sentinel build that writes its env to $out
        build_id = submit_build(server, drv_path="${drvs.sentinelWriteEnv}")
        server.wait_until_succeeds(f"rio-cli build-status {build_id} | grep -q Succeeded", timeout=60)

        # Check the output: http_proxy should NOT be present
        out_path = server.succeed(f"rio-cli build-output {build_id}").strip()
        env_contents = server.succeed(f"cat {out_path}")
        assert "http_proxy" not in env_contents, f"non-FOD build got http_proxy: {env_contents}"
        print("fod-proxy-nonfod PASS: non-FOD env has no http_proxy")
  '';
}
```

**`drvs.fodFetchFromOrigin` / `drvs.fodFetchFromPublic` / `drvs.sentinelWriteEnv` check:** verify these fixture derivations exist in `nix/tests/drvs/`. If not, create them (~10 lines each of nix).

### T2 — `test(vm):` register in default.nix

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix):

```nix
fod-proxy = import ./scenarios/fod-proxy.nix;
vm-fod-proxy-k3s = fod-proxy { inherit pkgs mkTest k3sFull helmRender drvs; };
```

Independent lines from P0241. Coordinator dispatches sequentially.

## Exit criteria

- `/nbr .#ci` green — including new `vm-fod-proxy-k3s`
- `fod-proxy-allowed`: allowlisted FOD build succeeds AND `TCP_MISS` in squid log
- `fod-proxy-denied`: non-allowlisted FOD build fails AND `TCP_DENIED/403` in squid log (Q4: BOTH assertions)
- `fod-proxy-nonfod`: non-FOD build's env has no `http_proxy`

## Tracey

No markers — FOD-proxy is infrastructure, not rio-build spec behavior.

## Files

```json files
[
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "NEW", "note": "T1: squid proxy scenario — allowed (TCP_MISS), denied (fail+TCP_DENIED), non-FOD (no http_proxy). Reuses existing dockerImages.fod-proxy + fod-proxy.yaml template."},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T2: import + vm-fod-proxy-k3s mkTest (SOFT conflict with P0241 — independent lines)"}
]
```

```
nix/tests/
├── scenarios/fod-proxy.nix   # T1 (NEW)
└── default.nix               # T2: import + mkTest (soft conflict w/ P0241)
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [241], "note": "Wave-1 frontier. SOFT conflict with P0241 on default.nix — coordinator dispatches sequentially, NOT a dag dep. Reuses existing fod-proxy image (GT4) + helm template (GT5)."}
```

**Depends on:** none. `dockerImages.fod-proxy` exists (GT4); `fod-proxy.yaml` template exists (GT5).
**Conflicts with:** `nix/tests/default.nix` — soft conflict with [P0241](plan-0241-vm-section-g-netpol.md). Independent lines. Coordinator serializes dispatch.

**Hidden check at dispatch:** `grep -A5 allowlist infra/helm/rio-build/templates/fod-proxy.yaml` — check if `extraAllowlist` helm value is supported. If yes, use it instead of ConfigMap patch + pod restart (R8).

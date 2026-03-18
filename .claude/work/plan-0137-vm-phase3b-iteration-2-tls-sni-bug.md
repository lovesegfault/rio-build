# Plan 0137: vm-phase3b iteration 2 + TLS SNI bug fix

## Design

The first real integration pass. Iteration 1 (P0136) was a placeholder — one `__noChroot` test, no k3s, plaintext. Iteration 2 brought the full end-to-end: mTLS + HMAC + recovery + GC, all exercised in one 4-VM test. And in doing so it immediately found two bugs that no unit test had caught.

**The TLS SNI bug (latent since P0128):** `load_client_tls` set `domain_name` on the `ClientTlsConfig` globally via `OnceLock`. But gateway and worker both connect to scheduler AND store. Whichever `domain_name` was set (e.g., `"rio-scheduler"`) would match the scheduler cert's SAN but NOT the store cert's SAN → TLS handshake failure on one of the two connections. The test surfaced this as "gateway can't upload to store after submitting to scheduler." Fix: remove the `domain_name` override entirely. tonic derives SNI from the URL host per-connection — `https://rio-scheduler.rio-system.svc:9001` → SNI `rio-scheduler.rio-system.svc`, which IS in the cert's dnsNames. The `domain_name` override was fighting the default behavior.

**The two VM-found bugs (`9d4ad44`):**

1. `validate_dag` (`__noChroot` rejection) was called only in `wopBuildDerivation` — NOT `wopBuildPaths` or `wopBuildPathsWithResults`. Modern `nix-build` uses opcode 46 (`wopBuildPathsWithResults`). The check never ran. Real security gap — vm-phase3b G1 showed "build submitted" instead of rejection. Fix: call `validate_dag` in all three handlers after dedup, before `filter_and_inline_drv`.

2. The `rio-nix-conf` ConfigMap mount (from P0132) used `subPath: nix.conf`. With `optional: true` + missing ConfigMap (vm-phase3a never applied `rio-nix-conf`), K8s creates an empty file at `/etc/rio/nix.conf` — NOT ENOENT. `setup_nix_conf` read empty → wrote empty `nix.conf` → Nix defaults → `substitute = true` → daemon tried `cache.nixos.org` → airgap VM DNS hang. vm-phase3a timed out at 841s (was ~400s). Fix: drop `subPath`, mount ConfigMap as directory at `/etc/rio/nix-conf/`, read `/etc/rio/nix-conf/nix.conf`. Missing ConfigMap → empty dir → clean ENOENT → fallback to const. Also: `setup_nix_conf` treats empty file as fallback (belt+suspenders).

**vm-phase3b iteration 2 test sections:** T1-T3 (mTLS: `kubectl get certificate` all Ready, `openssl s_client` from worker pod sees server cert, standby shows NOT_SERVING on health port), B1 (HMAC: upload without token → `PERMISSION_DENIED`), S1 (recovery: restart scheduler, build survives), C1 (GC dry-run), G1 (`__noChroot` rejection — now actually works). Added k3s node via new `common.mkK3sNode` helper (extracted from phase3a.nix). NixOS module `worker.nix` extended to pass TLS env vars + hostPath mount the cert Secret.

## Files

```json files
[
  {"path": "nix/tests/phase3b.nix", "action": "MODIFY", "note": "iteration 2 — T1-T3/B1/S1/C1/G1 sections + k3s node"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "mkK3sNode helper (extracted from phase3a.nix)"},
  {"path": "nix/modules/worker.nix", "action": "MODIFY", "note": "TLS env vars + hostPath mount"},
  {"path": "rio-common/src/tls.rs", "action": "MODIFY", "note": "remove domain_name override (SNI bug fix)"},
  {"path": "rio-common/tests/tls_integration.rs", "action": "MODIFY", "note": "remove domain_name from test setup"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "remove domain_name param"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "remove domain_name param"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "remove domain_name param"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "remove domain_name param"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "validate_dag in wopBuildPaths + wopBuildPathsWithResults"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "setup_nix_conf reads /etc/rio/nix-conf/nix.conf, treats empty as fallback"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "mount ConfigMap as dir not subPath"},
  {"path": "docs/src/phases/phase3b.md", "action": "MODIFY", "note": "iteration 2 section + TLS SNI bug writeup"}
]
```

## Tracey

No tracey markers landed. This is integration + bugfix — the features being integrated already have their markers from P0128-P0135.

## Entry

- Depends on P0128: mTLS (exercises + fixes SNI bug).
- Depends on P0130: state recovery (S1 section).
- Depends on P0131: HMAC (B1 section).
- Depends on P0132: K8s manifests + nix-conf mount (fixes subPath bug).
- Depends on P0133: `__noChroot` rejection (G1 section, fixes handler coverage bug).
- Depends on P0134: GC (C1 dry-run section).
- Depends on P0135: reconnect.
- Depends on P0136: vm-phase3b scaffold.

## Exit

Merged as `9d4ad44..85279e4` (2 commits). `.#ci` green at merge — vm-phase3b iteration 2 passes all sections. vm-phase3a back to ~400s after nix-conf subPath fix.

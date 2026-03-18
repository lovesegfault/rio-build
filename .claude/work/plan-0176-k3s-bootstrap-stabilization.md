# Plan 0176: k3s bootstrap stabilization — PG namespace, mTLS PKI, NodePort wait, pods/proxy scrape

## Design

The k3s-full fixture from P0173 was ~60% pass rate on first land. 23 commits of iteration to get it reliably green. The fixes are individually small but compound; each commit body is a mini-debugging session log.

**PG namespace + airgap (`78c8648`, `8b8d6ce`):** bitnami PG subchart used a different namespace than rio; image registry pinned in `vmtest-full.yaml` so airgap preload matches. **Namespace before SA (`b1484cc`):** helm-render ordering. **Gateway backoff reset + not in waitReady (`b1484cc`, `49a4cc1`):** empty `authorized_keys` → exit; can't wait for it before SSH key setup.

**mTLS in k3s (`c16801a`, `5c88a96`):** decouple `tls.enabled` from cert-manager via `tls.provisioner` knob; openssl PKI Secrets for k3s-full (no cert-manager in VM).

**NodePort wait (`6384b60`, `5ac09e6`, `220b618`):** `ssh-keyscan` → `nc -z` (russh doesn't handshake ssh-keyscan); `--etcd-disable-snapshots`; accidentally deleted `rollout restart` restored. **YAML separator + RBAC (`b897e59`, `458da2e`):** missing `---` between CRDs; `events.k8s.io` RBAC for recorder.

**Failover timing (`8546dec`, `ccadc09`, `2ed1dcb`, `7763b00`, `70de962`, `245a351`):** standby-wins is a race with graceful `step_down` (relaxed assertion); timeouts bumped repeatedly for port-forward churn under load; `--since=5m` for log check; `globalTimeout` bump.

**Metric scrapes via pods/proxy (`97f1305`, `095b94a`, `32c8731`):** port-forward was dying under load (kubelet log stream dies during scrape). Switched to apiserver `pods/proxy` subresource. **Numeric port** — k3s apiserver panics on named port in pods/proxy URL.

**psql in k3s (`6cd23da`, `656cce3`):** bitnami socket not at libpq default → `-h 127.0.0.1`; TCP auth not trust → `PGPASSWORD=rio`.

**No IFD for pki Secret (`27c4603`):** non-determinism × remote build; compute openssl certs inside the testScript instead.

## Files

```json files
[
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "PG ns, mTLS PKI Secrets, NodePort wait via nc -z, psql_k8s -h 127.0.0.1 + PGPASSWORD, no-IFD certs"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "failover timing relaxation, settle-wait, --since=5m, globalTimeout"},
  {"path": "nix/tests/scenarios/leader-election.nix", "action": "MODIFY", "note": "standby-wins relaxed (step_down race), non-empty-non-old wait"},
  {"path": "nix/tests/lib/assertions.py", "action": "MODIFY", "note": "pods/proxy metric scrape (no port-forward); sched_metric_wait 60→120s"},
  {"path": "infra/helm/rio-build/values/vmtest-full.yaml", "action": "MODIFY", "note": "postgresql.image.registry pinned"},
  {"path": "infra/helm/rio-build/templates/scheduler.yaml", "action": "MODIFY", "note": "tls.provisioner knob"},
  {"path": "infra/helm/crds/builds.rio.build.yaml", "action": "MODIFY", "note": "YAML separator"},
  {"path": "infra/helm/rio-build/templates/rbac.yaml", "action": "MODIFY", "note": "events.k8s.io RBAC"},
  {"path": "nix/helm-render.nix", "action": "MODIFY", "note": "namespace before SA ordering; nodeSelector/tolerations null not {}"}
]
```

## Tracey

No tracey markers — test infrastructure stabilization.

## Entry

- Depends on P0173: k3s-full fixture + scenarios (this stabilizes it)

## Exit

Merged as `78c8648..32c8731` (23 commits). k3s scenarios consistently green on `.#ci`.

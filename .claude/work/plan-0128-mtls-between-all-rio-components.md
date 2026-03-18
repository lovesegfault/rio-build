# Plan 0128: mTLS between all rio components

## Design

Phase 3a left all inter-service gRPC plaintext — acceptable for in-cluster k3s dev, untenable for production EKS where pods share a VPC with other workloads. This plan wired mutual TLS (client cert + server cert, both CA-verified) across all five binaries: gateway, scheduler, store, worker, controller.

The implementation hinged on a single chokepoint: every gRPC client in the codebase went through `rio-proto::client::connect_channel`, which had been a plain `http://` endpoint builder since phase 1b. Rather than threading `ClientTlsConfig` through ~11 call sites (including the controller's lazy per-reconcile connects that held only `String` addresses in `Ctx`), the fix introduced a `OnceLock<Option<ClientTlsConfig>>` global — each binary's `main()` calls `init_client_tls()` once after config load, before any connect. The OnceLock reads None → plaintext (dev mode), Some → `https://` + `.tls_config()`.

Three binaries (gateway, worker, store) lacked `rustls::crypto::aws_lc_rs::default_provider().install_default()` — only scheduler and controller had it from phase 3a's lease/kube setup. Without it, any TLS use panics at handshake. The fix added the install call as the first line of `main()` in all five.

The subtlest piece was the plaintext health port split. Scheduler and store now listen on two ports: the main gRPC port (strict mTLS) and a separate health-only port (`:9101`/`:9102`, plaintext). K8s readiness probes hit the health port — they can't present client certs. But the health port MUST share the same `HealthReporter` as the main port: scheduler's existing `health-toggle-loop` flips `SchedulerService` serving status based on leader election, and that status must be visible on BOTH ports. A fresh reporter on the plaintext port would always show SERVING → standby scheduler is marked Ready → K8s load-balances to it → cluster split. `HealthReporter` is `Clone`; the fix calls `health_reporter()` once and adds the returned service to both `Server::builder()` instances.

cert-manager manifests (prod overlay only) build a self-signed bootstrap `ClusterIssuer` → root CA `Certificate` → namespace-scoped `Issuer` → per-component `Certificate`s with dnsNames covering all four K8s DNS forms (`rio-scheduler`, `rio-scheduler.rio-system`, `rio-scheduler.rio-system.svc`, `rio-scheduler.rio-system.svc.cluster.local`). Worker certificates use a wildcard for the StatefulSet headless DNS. All certs use `privateKey.encoding: PKCS8` — cert-manager's default `PKCS1` isn't compatible with `rustls`'s EC key loader. The controller's workerpool `builders.rs` was extended to mount the worker cert Secret at `/etc/rio/tls/` and set `RIO_TLS__CERT_PATH` env vars.

Integration tests in `rio-common/tests/tls_integration.rs` use the `rcgen` crate to generate an in-memory CA + certs, then verify: (a) connect without client identity → handshake failure, (b) connect with valid identity → success, (c) connect with cert signed by wrong CA → failure.

## Files

```json files
[
  {"path": "rio-common/src/tls.rs", "action": "NEW", "note": "TlsConfig struct + load_server_tls/load_client_tls helpers"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "pub mod tls"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "rcgen dev-dep"},
  {"path": "rio-common/tests/tls_integration.rs", "action": "NEW", "note": "client-cert-required handshake tests"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "OnceLock<ClientTlsConfig> + init_client_tls + https switch"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "aws_lc_rs install + init_client_tls call"},
  {"path": "rio-gateway/Cargo.toml", "action": "MODIFY", "note": "tonic tls-aws-lc feature"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "aws_lc_rs install"},
  {"path": "rio-worker/src/config.rs", "action": "MODIFY", "note": "tls: TlsConfig field"},
  {"path": "rio-worker/Cargo.toml", "action": "MODIFY", "note": "tonic tls-aws-lc feature"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "aws_lc_rs install + ServerTlsConfig + second Server on health_addr"},
  {"path": "rio-store/Cargo.toml", "action": "MODIFY", "note": "tonic tls-aws-lc feature"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "ServerTlsConfig + shared health_reporter on :9101"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "init_client_tls call"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "tls_secret_name CRD field"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "mount tls Secret + RIO_TLS__ env vars"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "tls mount assertions"},
  {"path": "infra/base/scheduler.yaml", "action": "MODIFY", "note": "readinessProbe grpc.port: 9101"},
  {"path": "infra/base/store.yaml", "action": "MODIFY", "note": "readinessProbe grpc.port: 9102"},
  {"path": "infra/base/crds.yaml", "action": "MODIFY", "note": "regenerated WorkerPool CRD with tls_secret_name"},
  {"path": "infra/overlays/prod/cert-manager.yaml", "action": "NEW", "note": "ClusterIssuer + CA + per-component Certificates"},
  {"path": "infra/overlays/prod/tls-mounts.yaml", "action": "NEW", "note": "Secret volume mounts"},
  {"path": "infra/overlays/prod/kustomization.yaml", "action": "MODIFY", "note": "add cert-manager.yaml resource"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "tonic features = [transport, tls-aws-lc]"}
]
```

## Tracey

Markers implemented:
- `r[verify sec.boundary.grpc-hmac]` — TLS integration test (`f5a8583`). The `sec.boundary.grpc-hmac` rule covers both mTLS (transport) and HMAC (authorization); this plan lands the transport half. The `impl` annotation arrives with P0131 (HMAC verify in PutPath).

The TLS SNI bug — `load_client_tls` set `domain_name` globally but gateway/worker connect to BOTH scheduler AND store — was found later by vm-phase3b iteration 2 and is fixed in P0137.

## Entry

- Depends on P0127: phase 3a complete (gRPC `connect_channel` chokepoint, health_reporter pattern, controller workerpool builders all exist).

## Exit

Merged as `c67430e..f5a8583` (6 commits). `.#ci` green at merge. Integration test `tls_integration.rs` spawns in-memory tonic server with `client_ca_root` and verifies handshake failure without client identity.

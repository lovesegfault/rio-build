# Plan 0163: rio-cli admin client crate — in-pod kubectl-exec pattern

## Design

Thin wrapper over `rio_proto::AdminServiceClient` — three subcommands (`create-tenant`, `list-tenants`, `status`) where `status` bundles `ClusterStatus` + `ListWorkers` + `ListBuilds` summary. Designed for `kubectl exec deploy/rio-scheduler -- rio-cli <cmd>`: the scheduler pod already has `RIO_TLS__{CERT,KEY,CA}_PATH` set and certs mounted at `/etc/rio/tls/`, so `rio-cli` picks up mTLS to `localhost:9001` with zero extra config. This sidesteps the cert-extraction dance a standalone client would need.

Reuses `rio_common::config::load` (same `RIO_` prefix, `__` nesting), `rio_common::tls::load_client_tls`, `rio_proto::client::connect_admin` — same TLS plumbing every other component uses. TLS is env-only (no flags); the in-pod case sets it via env anyway.

Preceded by `73f86f0`: enabled sqlx `tls-rustls-aws-lc-rs` feature — Aurora PostgreSQL 15+ has `rds.force_ssl=1` by default; without this, every Aurora connection string fails at TLS negotiation. Feature matches the workspace's existing rustls CryptoProvider choice (aws-lc-rs, not ring — picking the wrong one pulls in a second provider → panic).

**Late fix (`e70a80f`):** three bugs in one `rpc<T>` helper. (1) `connect_admin` has 10s CONNECT_TIMEOUT but RPC calls had no deadline — wedged scheduler (recovery stuck, lease contention) hung `rio-cli` forever; remediations 01/08 shifted scheduler startup timing enough to trigger 10-min hangs in `vm-test-run-rio-cli`. Now: 30s `tokio::time::timeout` per RPC. (2) bare `?` on `tonic::Status` gave `status: Unavailable` with no RPC name. (3) `status` ran 3 sequential RPCs with print-between-`?` — if `list_workers` failed after `cluster_status`, summary was already printed → looked truncated.

## Files

```json files
[
  {"path": "rio-cli/Cargo.toml", "action": "NEW", "note": "new crate; deps on rio-proto, rio-common, tokio, clap"},
  {"path": "rio-cli/src/main.rs", "action": "NEW", "note": "create-tenant/list-tenants/status subcommands; rpc<T> helper"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "add rio-cli to workspace members"},
  {"path": "rio-scheduler/Cargo.toml", "action": "MODIFY", "note": "sqlx tls-rustls-aws-lc-rs feature (73f86f0)"}
]
```

## Tracey

No tracey markers — tooling crate, no spec behaviors.

## Entry

- Depends on P0156: admin RPCs (rio-cli is their client)

## Exit

Merged as `73f86f0`, `6b3ea84`, `7e9ff5c` (3 commits) + `e70a80f` (timeout/context fix late in phase). `.#ci` green. `kubectl exec deploy/rio-scheduler -- rio-cli status` verified on EKS.

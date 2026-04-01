# Configuration Reference

rio-build uses TOML configuration files with environment variable overrides. Each component reads its own config file. Environment variables use the `RIO_` prefix with `__` for nesting (e.g., `RIO_STORE__INLINE_THRESHOLD=262144`).

Precedence (highest to lowest): CLI flags > environment variables > config file > compiled defaults.

## Gateway

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `listen_addr` | string | `0.0.0.0:2222` | SSH listen address and port |
| `host_key` | string | (required) | SSH host key file path. If unset or missing, gateway generates an ephemeral key (breaks `known_hosts` on restart --- set in production). |
| `authorized_keys` | string | (required) | Authorized SSH keys file path |
| `scheduler_addr` | string | (required) | Scheduler gRPC endpoint |
| `store_addr` | string | (required) | Store gRPC endpoint |

> **Compile-time constants (not configurable):** `MIN_CLIENT_VERSION = 0x125` (1.37) --- the minimum Nix worker-protocol version accepted.

## Scheduler

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `listen_addr` | string | `0.0.0.0:9001` | gRPC listen address |
| `database_url` | string | (required) | PostgreSQL connection string |
| `w_locality` | const | 0.7 | Weight for transfer-cost locality scoring (compile-time const, not configurable) |
| `w_load` | const | 0.3 | Weight for executor load scoring (compile-time const, not configurable) |
| `default_duration_estimate` | Duration | 30s | Fallback build duration estimate |
| `ema_alpha` | f64 | 0.3 | EMA smoothing factor for duration estimates |
| `poison_threshold` | u32 | 3 | Failures across different executors before poisoning |
| `poison_ttl` | Duration | 24h | Time before poison state expires |
| `max_retries` | u32 | 2 | Maximum retry attempts per derivation |
| `hmac_key_path` | path | (unset) | HMAC-SHA256 key file for assignment token signing. Env: `RIO_HMAC_KEY_PATH`. Same file must be configured on the store. |
| `store_admin_addr` | string | (unset) | Store admin gRPC endpoint (for `TriggerGC` proxy). If unset, `AdminService.TriggerGC` returns UNIMPLEMENTED. |

## Store

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `listen_addr` | string | `0.0.0.0:9002` | gRPC listen address |
| `cache_http_addr` | socket addr | (unset) | Binary cache HTTP listen address. None = don't spawn the HTTP server. |
| `database_url` | string | (required) | PostgreSQL connection string |
| `metrics_addr` | socket addr | `0.0.0.0:9092` | Prometheus metrics listen address |
| `chunk_backend` | tagged enum | `{ kind = "inline" }` | Where chunks live. See `ChunkBackendKind` below. |
| `chunk_cache_capacity_bytes` | u64 | 2147483648 (2 GiB) | moka LRU capacity for chunk reads (shared across all services). |
| `signing_key_path` | path | (unset) | ed25519 narinfo signing key (Nix secret-key format). None = signing disabled. |
| `hmac_key_path` | path | (unset) | HMAC-SHA256 key file for assignment token verification on PutPath. Env: `RIO_HMAC_KEY_PATH`. Same file as scheduler. None = no token verification (dev mode). |

`chunk_backend` TOML syntax (tagged enum):

```toml
# Default — all NARs inline in PG, no chunk backend
chunk_backend = { kind = "inline" }

# Local filesystem (256-subdir fanout by hash prefix)
chunk_backend = { kind = "filesystem", base_dir = "/var/rio/chunks" }

# S3 (credentials from aws-sdk default chain — env vars, IRSA, instance profile)
chunk_backend = { kind = "s3", bucket = "rio-chunks", prefix = "" }
```

> **Compile-time constants (not configurable):** `INLINE_THRESHOLD` = 256 KiB, `CHUNK_MIN` = 16 KiB, `CHUNK_AVG` = 64 KiB, `CHUNK_MAX` = 256 KiB. These live in `rio-store/src/cas.rs` and `chunker.rs`. BLAKE3-verify-on-read and SHA-256-verify-on-put are always on (no config toggle).

> **GC configuration:** GC is triggered via `StoreAdminService.TriggerGC` (or proxied through scheduler `AdminService.TriggerGC` which adds live-build roots). `GcRequest.grace_period_hours` defaults to **2h**. The orphan scanner and S3 drain task are spawned in `main.rs` with compile-time constants (`DRAIN_INTERVAL = 30s`, orphan stale threshold = 2h). See [store: GC](./components/store.md#two-phase-garbage-collection).

## Builder

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `worker_id` | string | (auto: hostname) | Builder identity. Empty → auto-detect via hostname. |
| `scheduler_addr` | string | (required) | Scheduler gRPC endpoint |
| `store_addr` | string | (required) | Store gRPC endpoint |
| `systems` | list\<string\> | (auto: `{arch}-{os}`) | Nix systems this builder can build for (any-match). Env `RIO_SYSTEMS` is comma-separated; TOML is an array. |
| `features` | list\<string\> | `[]` | `requiredSystemFeatures` this builder supports (all-match). Same env/TOML format as `systems`. |
| `fuse_mount_point` | path | `/var/rio/fuse-store` | FUSE mount point. **Never** `/nix/store` --- that would shadow the host store. |
| `fuse_cache_dir` | path | `/var/rio/cache` | Local SSD cache directory for rio-fuse |
| `fuse_cache_size_gb` | u64 | 50 | Maximum FUSE cache size in GB (LRU eviction above this) |
| `fuse_threads` | u32 | 4 | Number of FUSE daemon threads |
| `fuse_passthrough` | bool | true | Enable kernel passthrough (Linux 6.9+). Disable only for debugging. |
| `overlay_base_dir` | path | `/var/rio/overlays` | Base directory for per-build overlay upper/work layers |
| `metrics_addr` | socket addr | `0.0.0.0:9093` | Prometheus metrics listen address |
| `health_addr` | socket addr | `0.0.0.0:9193` | HTTP `/healthz` + `/readyz` listen address (builder has no gRPC server) |
| `log_rate_limit` | u64 | 10000 | Maximum log lines per second per build (0 = unlimited) |
| `log_size_limit` | u64 | 104857600 (100MB) | Maximum total log bytes per build (0 = unlimited) |
| `size_class` | string | `""` | Size-class label (e.g., `small`, `large`). If the scheduler has `size_classes` configured, builders with an empty `size_class` are **rejected**. |
| `max_leaked_mounts` | usize | 3 | After this many overlay-teardown (`umount2`) failures, the builder refuses new builds with `InfrastructureFailure`. |
| `daemon_timeout_secs` | u64 | 7200 (2h) | Timeout for the local `nix-daemon --stdio` subprocess when the client didn't set `build_timeout`. |
| `executor_kind` | enum | `builder` | `builder` (airgapped, regular derivations) or `fetcher` (egress-open, FODs only). Set via `RIO_EXECUTOR_KIND`. See [ADR-019](./decisions/019-builder-fetcher-split.md). |

> **Heartbeat interval** is a compile-time constant (`HEARTBEAT_INTERVAL_SECS = 10` in `rio-common::limits`), not a configurable parameter. Changing it would require the scheduler's heartbeat-timeout to be adjusted in lockstep.

## Controller

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `health_addr` | socket addr | `0.0.0.0:9194` | HTTP `/healthz` listen address |
| `metrics_addr` | socket addr | `0.0.0.0:9094` | Prometheus metrics listen address |
| `scheduler_addr` | string | (required) | Scheduler gRPC endpoint (for queue depth queries + DrainWorker on finalizer) |

> **The controller is NOT leader-elected** (single replica by design). Only the scheduler uses a Kubernetes Lease (see scheduler `RIO_LEASE_NAME` / `RIO_LEASE_NAMESPACE` env vars documented in [scheduler: Leader Election](./components/scheduler.md#leader-election)).

## TLS / mTLS

Application-level mTLS is configured via a nested `TlsConfig` on each component:

| Env var | Description |
|---------|-------------|
| `RIO_TLS__CERT_PATH` | Our certificate (PEM). Server presents on accept; client presents for mTLS. |
| `RIO_TLS__KEY_PATH` | Our private key (PEM, PKCS8). cert-manager's `encoding: PKCS8` emits this for EC keys. |
| `RIO_TLS__CA_PATH` | CA bundle (PEM). Server verifies client certs against this; client verifies server cert. |

All three must be set together (partial config is a startup error). When set:
- Scheduler + store: main gRPC port requires client certs. A second plaintext listener on `health_addr` (`RIO_HEALTH_ADDR`, default `:9101`/`:9102`) serves ONLY `grpc.health.v1.Health` for K8s probes, sharing the SAME `HealthReporter` so leadership status propagates.
- Gateway, builder, controller: client-side TLS for outgoing connections (`connect_*` in `rio-proto/client.rs`).

For K8s deployments, the prod overlay's `cert-manager.yaml` issues per-component certificates from a self-signed CA. See [Security & Threat Model](./security.md) for the threat model.

## Observability

Observability is configured via **environment variables only** (not figment/TOML) because `init_tracing()` runs before config parsing:

| Env Var | Type | Default | Description |
|-----------|------|---------|-------------|
| `RIO_OTEL_ENDPOINT` | string | (unset → no OTel) | OTLP collector endpoint. If unset, only local Prometheus metrics + JSON logs are emitted. |
| `RIO_OTEL_SAMPLE_RATE` | f64 | 1.0 | Trace sampling rate |
| `RIO_LOG_FORMAT` | enum | `json` | `json` or `pretty` |

The OTel service name is auto-set per component (not user-configurable). See [observability.md](./observability.md) for trace structure and metric details.

## Retry Policy

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `retry_backoff_base` | Duration | 5s | Initial retry backoff duration |
| `retry_backoff_multiplier` | f64 | 2.0 | Exponential backoff multiplier |
| `retry_backoff_max` | Duration | 300s | Maximum backoff duration cap |
| `retry_backoff_jitter` | f64 | 0.2 | Random jitter factor (0.0–1.0) added to backoff |
| `retry_on_different_executor` | bool | true | Retry failed derivations on a different executor |

Configured on the scheduler. See [errors.md](./errors.md) for retry semantics and failure classification.

## Multi-Tenancy Quotas

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `max_concurrent_builds` | u32 | 50 | Maximum concurrent build requests per tenant |
| `max_dag_size` | u32 | 10000 | Maximum derivations in a single build DAG |
| `max_store_size` | u64 | 1099511627776 (1TB) | Maximum total store usage per tenant |
| `max_nar_upload_size` | u64 | 10737418240 (10GB) | Maximum single NAR upload size |

Configured per tenant via the admin API or CRD annotations. See [multi-tenancy.md](./multi-tenancy.md) for enforcement details.

## PostgreSQL Operations

The scheduler and store share a PostgreSQL cluster (separate schemas). This section covers operational concerns.

### Connection Pooling

All components use connection pooling via `sqlx`'s built-in pool. Key settings:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `database_pool_min` | u32 | 2 | Minimum idle connections per component |
| `database_pool_max` | u32 | 10 | Maximum connections per component |
| `database_acquire_timeout` | Duration | 30s | Timeout for acquiring a connection from the pool |

For production deployments with many builder pods, deploy PgBouncer between components and PostgreSQL to multiplex connections. Use transaction-mode pooling (not session-mode) since rio-build does not use prepared statements across transaction boundaries.

> **Note:** The scheduler's leader election uses a **Kubernetes Lease** (`coordination.k8s.io/v1`), not PostgreSQL. PgBouncer mode has no effect on leader election. See [scheduler: Leader Election](./components/scheduler.md#leader-election) for details.

## gRPC

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `grpc.max_message_size` | u32 | 33554432 (32MB) | Maximum gRPC message size in bytes. Must be >= 32MB for large DAG submissions (nixpkgs stdenv is ~12MB). Applies to all gRPC services. |

Environment variable: `RIO_GRPC_MAX_MESSAGE_SIZE`

### High Availability

- **Development:** Single PostgreSQL instance is sufficient.
- **Production:** Use a managed HA service (RDS Multi-AZ, Cloud SQL HA, or Patroni on self-hosted). The store and scheduler tolerate brief leader failovers (connection retry with backoff).
- **Read replicas:** Dashboard queries via `AdminService` can be directed to read replicas. Configure via `database_read_url` (optional; defaults to `database_url`).

### Schema Migration

Migrations are managed via `sqlx migrate` with numbered migration files in each crate's `migrations/` directory.

- **Forward-compatible:** New columns use `ADD COLUMN ... DEFAULT` so old code tolerates new schema.
- **Blue-green safe:** During rolling deployments, both old and new code versions may run simultaneously. Migrations must be compatible with both.
- **Rollback scripts:** Each migration has a corresponding `down.sql` for rollback. Tested in CI.
- **Migration on startup:** Each component runs pending migrations on startup (with an advisory lock to prevent concurrent migration).

## Configuration via CRD (Runtime)

The `BuilderPool` CRD provides runtime-configurable parameters that the controller reconciles without component restarts. See [controller.md](./components/controller.md) for the full CRD spec.

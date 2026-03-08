# Configuration Reference

rio-build uses TOML configuration files with environment variable overrides. Each component reads its own config file. Environment variables use the `RIO_` prefix with `__` for nesting (e.g., `RIO_STORE__INLINE_THRESHOLD=262144`).

Precedence (highest to lowest): CLI flags > environment variables > config file > compiled defaults.

## Gateway

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `listen_addr` | string | `0.0.0.0:2222` | SSH listen address and port |
| `max_connections` | u32 | 1000 | Maximum concurrent SSH connections |
| `max_channels_per_connection` | u32 | 32 | Maximum SSH channels per connection |
| `protocol_version_min` | u32 | `0x125` (1.37) | Minimum Nix protocol version accepted. Nix encodes protocol versions as `(major << 8) \| minor` on the wire, so version 1.37 = `(1 << 8) \| 37` = `0x125`. Corresponds to Nix 2.20+. |
| `host_key_path` | string | `/etc/rio/ssh_host_ed25519_key` | SSH host key file |
| `authorized_keys_path` | string | `/etc/rio/authorized_keys` | Authorized SSH keys file |
| `scheduler_addr` | string | `rio-scheduler:50051` | Scheduler gRPC endpoint |
| `store_addr` | string | `rio-store:50052` | Store gRPC endpoint |

## Scheduler

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `listen_addr` | string | `0.0.0.0:50051` | gRPC listen address |
| `database_url` | string | (required) | PostgreSQL connection string |
| `w_locality` | f64 | 1.0 | Weight for transfer-cost locality scoring (normalized) |
| `w_load` | f64 | 0.5 | Weight for worker load scoring |
| `default_duration_estimate` | Duration | 30s | Fallback build duration estimate |
| `ema_alpha` | f64 | 0.3 | EMA smoothing factor for duration estimates |
| `orphan_timeout` | Duration | 5m | Timeout before orphaned derivations are cancelled |
| `preemption_reserve_fraction` | f64 | 0.25 | Fraction of workers reserved for high-priority builds |
| `poison_threshold` | u32 | 3 | Failures across different workers before poisoning |
| `poison_ttl` | Duration | 24h | Time before poison state expires |
| `max_retries` | u32 | 2 | Maximum retry attempts per derivation |

## Store

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `listen_addr` | string | `0.0.0.0:50052` | gRPC listen address |
| `http_listen_addr` | string | `0.0.0.0:8080` | Binary cache HTTP listen address |
| `database_url` | string | (required) | PostgreSQL connection string |
| `s3_bucket` | string | (required) | S3 bucket for chunk storage |
| `s3_prefix` | string | `chunks/` | S3 key prefix |
| `inline_threshold` | u64 | 262144 (256KB) | NARs below this size bypass chunking |
| `chunk_target` | u64 | 65536 (64KB) | FastCDC target chunk size |
| `chunk_min` | u64 | 16384 (16KB) | FastCDC minimum chunk size |
| `chunk_max` | u64 | 262144 (256KB) | FastCDC maximum chunk size |
| `cache_size` | u64 | 2147483648 (2GB) | In-process LRU chunk cache size |
| `gc_grace_period` | Duration | 24h | Grace period between GC mark and sweep |
| `orphan_scanner_interval` | Duration | 7d | Frequency of orphan chunk scanner |
| `signing_key_path` | string | `/etc/rio/signing-key` | ed25519 signing key for narinfo |
| `verify_on_read` | bool | true | BLAKE3-verify chunks on read from S3 |
| `verify_nar_on_put` | bool | true | SHA-256-verify NAR on PutPath |

## Worker

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `worker_id` | string | (auto: hostname) | Worker identity. Empty → auto-detect via hostname. |
| `scheduler_addr` | string | (required) | Scheduler gRPC endpoint |
| `store_addr` | string | (required) | Store gRPC endpoint |
| `max_builds` | u32 | 1 | Maximum concurrent builds on this worker |
| `systems` | list\<string\> | (auto: `{arch}-{os}`) | Nix systems this worker can build for (any-match). Env `RIO_SYSTEMS` is comma-separated; TOML is an array. |
| `features` | list\<string\> | `[]` | `requiredSystemFeatures` this worker supports (all-match). Same env/TOML format as `systems`. |
| `fuse_mount_point` | path | `/var/rio/fuse-store` | FUSE mount point. **Never** `/nix/store` --- that would shadow the host store. |
| `fuse_cache_dir` | path | `/var/rio/cache` | Local SSD cache directory for rio-fuse |
| `fuse_cache_size_gb` | u64 | 50 | Maximum FUSE cache size in GB (LRU eviction above this) |
| `fuse_threads` | u32 | 4 | Number of FUSE daemon threads |
| `fuse_passthrough` | bool | true | Enable kernel passthrough (Linux 6.9+). Disable only for debugging. |
| `overlay_base_dir` | path | `/var/rio/overlays` | Base directory for per-build overlay upper/work layers |
| `metrics_addr` | socket addr | `0.0.0.0:9093` | Prometheus metrics listen address |
| `health_addr` | socket addr | `0.0.0.0:9193` | HTTP `/healthz` + `/readyz` listen address (worker has no gRPC server) |
| `log_rate_limit` | u64 | 10000 | Maximum log lines per second per build (0 = unlimited) |
| `log_size_limit` | u64 | 104857600 (100MB) | Maximum total log bytes per build (0 = unlimited) |
| `size_class` | string | `""` | Size-class label (e.g., `small`, `large`). If the scheduler has `size_classes` configured, workers with an empty `size_class` are **rejected**. |
| `max_leaked_mounts` | usize | 3 | After this many overlay-teardown (`umount2`) failures, the worker refuses new builds with `InfrastructureFailure`. |
| `daemon_timeout_secs` | u64 | 7200 (2h) | Timeout for the local `nix-daemon --stdio` subprocess when the client didn't set `build_timeout`. |

> **Heartbeat interval** is a compile-time constant (`HEARTBEAT_INTERVAL_SECS = 10` in `rio-common::limits`), not a configurable parameter. Changing it would require the scheduler's heartbeat-timeout to be adjusted in lockstep.

## Controller

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `listen_addr` | string | `0.0.0.0:8081` | HTTP listen address for health probes |
| `scheduler_addr` | string | `rio-scheduler:50051` | Scheduler gRPC endpoint (for queue depth queries) |
| `worker_pool_sync_interval` | Duration | 30s | How often to reconcile WorkerPool state |
| `gc_schedule` | string | `0 3 * * *` | Cron schedule for automated GC triggers (Phase 4) |
| `leader_election_lease_name` | string | `rio-controller-leader` | Kubernetes Lease name for leader election |

## TLS / mTLS

mTLS between internal components is typically provided by the service mesh (Istio/Linkerd). If no service mesh is deployed, application-level TLS can be configured:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `tls_enabled` | bool | false | Enable application-level TLS |
| `tls_cert_path` | string | `/etc/rio/tls/tls.crt` | TLS certificate file |
| `tls_key_path` | string | `/etc/rio/tls/tls.key` | TLS private key file |
| `tls_ca_path` | string | `/etc/rio/tls/ca.crt` | CA certificate for mTLS client verification |

These parameters apply to all gRPC-serving components (gateway, scheduler, store). When a service mesh provides mTLS, set `tls_enabled = false` (the default) and rely on the mesh for encryption and authentication.

> **Security warning:** The default `tls_enabled = false` means all internal gRPC traffic is plaintext. This is only appropriate when a service mesh (Istio, Linkerd) provides transparent mTLS. Deployments without a service mesh MUST set `tls_enabled = true` and provide valid certificates. See [Security & Threat Model](./security.md) for details.

## Observability

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `otel_endpoint` | string | `http://otel-collector:4317` | OTLP collector endpoint for traces and metrics |
| `otel_service_name` | string | (auto-set per component) | OpenTelemetry service name |
| `otel_sample_rate` | f64 | 1.0 (dev) / 0.1 (prod) | Trace sampling rate |

These parameters apply to all components. See [observability.md](./observability.md) for trace structure and metric details.

## Retry Policy

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `retry_backoff_base` | Duration | 5s | Initial retry backoff duration |
| `retry_backoff_multiplier` | f64 | 2.0 | Exponential backoff multiplier |
| `retry_backoff_max` | Duration | 300s | Maximum backoff duration cap |
| `retry_backoff_jitter` | f64 | 0.2 | Random jitter factor (0.0–1.0) added to backoff |
| `retry_on_different_worker` | bool | true | Retry failed derivations on a different worker |

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

For production deployments with many worker pods, deploy PgBouncer between components and PostgreSQL to multiplex connections. Use transaction-mode pooling (not session-mode) since rio-build does not use prepared statements across transaction boundaries.

> **Warning:** The scheduler's leader election uses PostgreSQL advisory locks, which are **session-scoped**. The leader election connection MUST bypass PgBouncer transaction-mode pooling and use either session-mode pooling or a direct connection to PostgreSQL. See [scheduler: Leader Election](./components/scheduler.md#leader-election) for details.

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

The `WorkerPool` CRD provides runtime-configurable parameters that the controller reconciles without component restarts. See [controller.md](./components/controller.md) for the full CRD spec.

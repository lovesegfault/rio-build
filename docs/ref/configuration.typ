#import "/lib/rio.typ": *

#show: rio.with(domains: none)

rio-build uses TOML configuration files with environment variable overrides.
Each component reads its own config file. Environment variables use the `RIO_`
prefix with `__` for nesting (e.g., `RIO_CHUNK_BACKEND__KIND=s3`).

Precedence (highest to lowest): CLI flags > environment variables > config
file > compiled defaults.

The per-component tables below are generated from the `Config` structs (via
`schemars` → `xtask regen docs-data` → `gen/config.json`); the *Default*
column is `Config::default()` serialized. Nested tables flatten as dotted
keys (`jwt.required` ↔ TOML `[jwt] required = …` ↔ env `RIO_JWT__REQUIRED`).

#let _cfg = json("/gen/config.json").components

// One config-reference table per component. `fields` is the
// `flatten_schema()` output: `[{key, type, default, description}]`.
// Descriptions are rustdoc first-sentences with backticked code spans;
// split on backticks and interleave raw() so `foo` renders as code
// without eval()ing arbitrary markup.
#let _md(s) = {
  s
    .split("`")
    .enumerate()
    .map(((i, part)) => if calc.odd(i) { raw(part) } else { part })
    .join()
}
#let _cfg-table(fields) = table(
  columns: (auto, auto, auto, 1fr),
  table.header([Key], [Type], [Default], [Description]),
  ..fields
    .map(f => (
      raw(f.key),
      raw(f.type),
      if f.default == "" [---] else { raw(f.default) },
      _md(f.description),
    ))
    .flatten(),
)

= Gateway

#_cfg-table(_cfg.gateway)

#info[
  *Compile-time constants (not configurable):* `MIN_CLIENT_VERSION = 0x123`
  (1.35) --- the minimum Nix worker-protocol version accepted. 1.35 is Lix's
  frozen protocol version.
]

= Scheduler

#_cfg-table(_cfg.scheduler)

#info[
  *`[sla]` table:* ADR-023 #gls("sla")-driven sizing config is mandatory and
  structured (no env override). It is documented separately in
  #cross-link("/spec/components/scheduler.typ")[scheduler: SLA sizing] and is
  not flattened into the table above.
]

#info[
  *Compile-time constants (not configurable):* `DEFAULT_DURATION_SECS = 60.0`
  --- fallback build-duration estimate when no SLA fit exists for the `(pname,
  system, tenant)` key. `POISON_TTL = 24h` --- time before a poisoned
  derivation auto-expires; checked on each housekeeping tick.
]

= Store

#_cfg-table(_cfg.store)

`chunk_backend` TOML syntax (tagged enum):

```toml
# Default — all NARs inline in PG, no chunk backend
chunk_backend = { kind = "inline" }

# Local filesystem (256-subdir fanout by hash prefix)
chunk_backend = { kind = "filesystem", base_dir = "/var/rio/chunks" }

# S3 (credentials from aws-sdk default chain — env vars, IRSA, instance profile)
chunk_backend = { kind = "s3", bucket = "rio-chunks", prefix = "" }
```

#info[
  *Compile-time constants (not configurable):* `INLINE_THRESHOLD` = 256 KiB,
  `CHUNK_MIN` = 16 KiB, `CHUNK_AVG` = 64 KiB, `CHUNK_MAX` = 256 KiB. These
  live in `rio-store/src/cas.rs` and `chunker.rs`. #gls("blake3")-verify-on-read and
  SHA-256-verify-on-put are always on (no config toggle).
]

#info[
  *GC configuration:* GC is triggered via `StoreAdminService.TriggerGC` (or
  proxied through scheduler `AdminService.TriggerGC` which adds live-build
  roots). `GcRequest.grace_period_hours` defaults to *#(refs.const)("DEFAULT_GC_GRACE_HOURS")h*. The orphan scanner
  and S3 drain task are spawned in `main.rs` with compile-time constants
  (`DRAIN_INTERVAL = 30s`, orphan stale threshold = 15min). See
  #cross-link("/spec/components/store.typ")[store: GC].
]

= Builder

#_cfg-table(_cfg.builder)

#info[
  *Heartbeat interval* is a compile-time constant (`HEARTBEAT_INTERVAL_SECS =
  10` in `rio-common::limits`), not a configurable parameter. Changing it
  would require the scheduler's heartbeat-timeout to be adjusted in lockstep.
]

= Controller

#_cfg-table(_cfg.controller)

#info[
  // Tripwire: refs.cfg asserts the key exists in gen/config.json —
  // if the controller's lease config goes away this box is stale and
  // the typst build fails here instead of silently rotting.
  #let _ = (refs.cfg)("controller", "nodeclaim_pool.lease_name")
  Leader-elected components: #(refs.leased-components)() (each holds a
  Kubernetes Lease via `rio_lease`; for the controller it's the
  `nodeclaim_pool` reconciler since ADR-023 §13b). The chart default is
  still single-replica for the controller; see
  #cross-link("/spec/components/controller.typ")[the controller
    component spec] for the lease scoping.
]

= Transport

There is no application-level TLS. Components run plaintext gRPC servers;
encryption is provided by Cilium WireGuard at the overlay layer
(`r[sec.transport.cilium-wireguard]`). K8s gRPC health probes hit the single
main port directly.

#table(
  columns: 2,
  table.header([Env var], [Description]),
  [`RIO_SERVICE_HMAC_KEY_PATH`],
  // Derived from `rg 'HmacSigner::load' rio-*/src/` + helm
  // `rio-service-hmac` mounts; keep in sync with security.typ's
  // §Service-token-bypass + Secret Inventory row.
  [Service-token HMAC key (raw bytes). Minters {controller, cli, scheduler,
    gateway, dashboard} sign `x-rio-service-token`; verifiers {scheduler
    `AdminService`, store `StoreAdminService` + `PutPath`}. Separate from the
    assignment-token key. See `r[sec.authz.service-token]`.],

  [`RIO_DASHBOARD__CORS_ALLOW_ORIGINS`],
  [(scheduler) Comma-separated CORS allowed origins for gRPC-Web. Defaults to
    the in-cluster dashboard nginx Service hostname.],
)

= Observability

Observability is configured via *environment variables only* (not the layered
config/TOML) because `init_tracing()` runs before config parsing:

#table(
  columns: 4,
  table.header([Env Var], [Type], [Default], [Description]),
  [`RIO_OTEL_ENDPOINT`],
  [string],
  [(unset → no OTel)],
  [OTLP collector endpoint. If unset, only local Prometheus metrics + JSON
    logs are emitted.],

  [`RIO_OTEL_SAMPLE_RATE`], [f64], [1.0], [Trace sampling rate],
  [`RIO_LOG_FORMAT`], [enum], [`json`], [`json` or `pretty`],
)

The OTel service name is auto-set per component (not user-configurable). See
#cross-link("/spec/system/observability.typ")[Observability] for trace structure and metric
details.

= Multi-Tenancy Quotas

#table(
  columns: 4,
  table.header([Parameter], [Type], [Default], [Description]),
  [`max_concurrent_builds`],
  [u32],
  [50],
  [Maximum concurrent build requests per tenant],

  [`max_dag_size`],
  [u32],
  [10000],
  [Maximum derivations in a single build @dag],

  [`max_store_size`],
  [u64],
  [1099511627776 (1TB)],
  [Maximum total store usage per tenant],

  [`max_nar_upload_size`],
  [u64],
  [10737418240 (10GB)],
  [Maximum single @nar upload size],
)

Configured per tenant via the admin API or @crd annotations. See
#cross-link("/spec/system/tenancy.typ")[Multi-Tenancy] for enforcement details.

= PostgreSQL Operations

The scheduler and store share a PostgreSQL cluster (separate schemas). This
section covers operational concerns.

== Connection Pooling

All components use connection pooling via `sqlx`'s built-in pool. For
production deployments with many builder pods, deploy PgBouncer between
components and PostgreSQL to multiplex connections. Use transaction-mode
pooling (not session-mode) since rio-build does not use prepared statements
across transaction boundaries.

#info[
  *Note:* The scheduler's @leader-election uses a *Kubernetes Lease*
  (`coordination.k8s.io/v1`), not PostgreSQL. PgBouncer mode has no effect on
  leader election. See
  #cross-link("/spec/components/scheduler.typ")[scheduler: Leader
    Election] for details.
]

= gRPC

#table(
  columns: 4,
  table.header([Parameter], [Type], [Default], [Description]),
  [`RIO_GRPC_MAX_MESSAGE_SIZE`],
  [usize (env var)],
  [268435456 (256 MiB)],
  [Maximum gRPC message size in bytes. Sized for `MAX_DAG_NODES`-scale
    `SubmitBuild` requests (\~120 MB at \~150k nodes). Applies to all gRPC
    services. Read from the environment, not a config-file key.],
)

== High Availability

- *Development:* Single PostgreSQL instance is sufficient.
- *Production:* Use a managed HA service (RDS Multi-@az, Cloud SQL HA, or
  Patroni on self-hosted). The store and scheduler tolerate brief leader
  failovers (connection retry with backoff).

== Schema Migration

Migrations are managed via `sqlx migrate` with numbered migration files in
the `rio-migrations` crate's `migrations/` directory.

- *Forward-compatible:* New columns use `ADD COLUMN ... DEFAULT` so old code
  tolerates new schema.
- *Blue-green safe:* During rolling deployments, both old and new code
  versions may run simultaneously. Migrations must be compatible with both.
- *Forward-only:* Migrations have no `down.sql`. Rollback is by deploying the
  previous binary version (it ignores unknown columns/tables).
- *Deploy-time, not startup:* The `rio-migrate` Job (k8s) or the
  `rio-migrate` systemd oneshot (standalone NixOS) runs `rio-store migrate`
  before components start; an advisory lock serializes concurrent runs.
  Components verify the schema at startup and fail with an error naming the
  runner if it is missing or stale.

= Configuration via CRD (Runtime)

The `Pool` CRD provides runtime-configurable parameters that the controller
reconciles without component restarts. See
#cross-link("/spec/components/controller.typ")[Controller] for the full CRD spec.

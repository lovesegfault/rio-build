# rio-cli

> Admin CLI for rio-build. Thin wrapper over `AdminService` (scheduler) and
> `StoreAdminService` (store) gRPC. Not packaged as a separate image — the
> binary is bundled into the scheduler container so the operator path is
> `kubectl exec deploy/rio-scheduler -- rio-cli …`.

## Connection model

`--scheduler-addr` (default `localhost:9001`) and `--store-addr` (default
`rio-store.rio-store:9002`) target the in-pod case. TLS is env-only
(`RIO_TLS__*`). gRPC connect is **per-subcommand** — `bps` (kube-only) and
`upstream`/`verify-chunks` (store-only) MUST work when the scheduler is
unreachable (e.g., to diagnose why).

`--json` makes handlers print exactly one JSON document to stdout (for `jq`
pipelines). Prost-generated types don't derive `Serialize`; each subcommand
projects to module-local structs so the JSON surface is decoupled from proto
evolution. Streaming subcommands (`logs`, `gc`, `verify-chunks` stdout) ignore
the flag.

r[cli.rpc-timeout]

Unary admin RPCs are wrapped in a 120s deadline.

r[cli.rpc-retry]

Unary admin RPCs retry up to twice on `UNAVAILABLE` (1s/2s backoff) before surfacing the error — covers a standby-replica hit or leader-election flip without masking a genuinely-down scheduler.
All unary admin RPCs go through `rpc()` which applies the retry above and a per-attempt `RPC_TIMEOUT` deadline. `RPC_TIMEOUT` is 120s — NOT 30s — because actor-routed RPCs (`InspectBuildDag`, `ClusterSnapshot`) queue behind the actor mailbox, and the operator needs the dump precisely when the actor is saturated (I-163: ~9.5k mailbox commands under load). `connect_admin` has a separate 10s connect timeout (TCP/handshake bound). Streaming RPCs (`TriggerGC`, `GetBuildLogs`, `VerifyChunks`) wrap only the initial call; per-message progress drains without a whole-call deadline.

## Subcommands

| Subcommand | Service | Purpose |
|---|---|---|
| `status` | `AdminService.ClusterStatus` | One-screen workers/builds/queue summary |
| `workers` | `AdminService.ListExecutors` (PG) / `DebugListExecutors` (actor) | Detailed executor list; `--actor`/`--diff` modes |
| `builds` | `AdminService.ListBuilds` | Paginated build list with `--status` filter |
| `derivations` | `AdminService.InspectBuildDag` | Live actor DAG snapshot for a build |
| `logs` | `AdminService.GetBuildLogs` | Stream a derivation's log (ring-buffer or S3) |
| `gc` | `AdminService.TriggerGC` | Streamed mark/sweep; `--dry-run` |
| `poison-list` / `poison-clear` | `AdminService.{ListPoisoned,ClearPoison}` | Show/clear derivation poison roots |
| `cancel-build` | `SchedulerService.CancelBuild` | Force-cancel an active build |
| `drain-executor` | `AdminService.DrainExecutor` | Mark a worker draining (`--force` reassigns) |
| `bps` | k8s apiserver (no gRPC) | `BuilderPoolSet` CR get/describe |
| `verify-chunks` | `StoreAdminService.VerifyChunks` | PG↔backend chunk audit |
| `upstream` | `StoreAdminService.{List,Add,Remove}Upstream` | Per-tenant upstream cache CRUD |
| `create-tenant` / `list-tenants` | `AdminService.{CreateTenant,ListTenants}` | Tenant CRUD |

r[cli.cmd.derivations]
`rio-cli derivations [BUILD_ID] [--all-active] [--status S] [--stuck]` calls `InspectBuildDag` (`r[proto.admin.diag-rpc]`) — the live actor view, not PG. `--all-active` iterates `ListBuilds(status=active)` first. `--stuck` filters to derivations assigned to dead-stream executors (`!assigned_executor.is_empty() && !executor_has_stream` — the I-025 smoking gun). Per-derivation output surfaces `system`/`required_features`/`failed_builders` (the `hard_filter` inputs) and per-executor `rejections` so "why won't it dispatch" is one look. Sort: stuck first, then status, then name.

r[cli.cmd.cancel-build]
`rio-cli cancel-build BUILD_ID [--reason R]` calls `SchedulerService.CancelBuild` (NOT `AdminService` — cancel lives next to submit). Idempotent: returns `cancelled=false` for already-terminal/unknown. The operator lever for orphaned builds (gateway crash mid-disconnect cleanup, I-112); the scheduler's orphan-watcher sweep is the automatic counterpart.

r[cli.cmd.verify-chunks]
`rio-cli verify-chunks [--batch-size N]` server-streams `StoreAdminService.VerifyChunks`. Missing chunk hashes go to **stdout** (one hex BLAKE3 per line — pipeable into `xargs aws s3api head-object`); progress goes to **stderr** so `verify-chunks | tee missing.txt` captures hashes while progress scrolls. Warns on stream-closed-without-`done` (store disconnected mid-scan). I-040 diagnostic.

r[cli.cmd.sla]
`rio-cli sla {override|list|clear|reset|status|explain|export-corpus|import-corpus}` calls the ADR-023 `AdminService.*SlaOverride` / `ResetSlaModel` / `SlaStatus` / `SlaExplain` / `*SlaCorpus` RPCs. `override PNAME [--system S] [--tenant T] [--tier NAME] [--cores N] [--mem 8Gi] [--ttl 7d]` pins a key (NULL system/tenant are wildcards, `r[sched.sla.override-precedence]`); `reset PNAME` drops the key's `build_samples` and evicts the cached fit so the next dispatch re-probes; `status PNAME` dumps the cached fit + any active override; `explain PNAME` shows the per-tier candidate table with rejection reasons; `export-corpus -o PATH` / `import-corpus PATH` move fitted curves between clusters (ref-second rescaled).

r[cli.cmd.upstream]
`rio-cli upstream {list|add|remove} --tenant T …` is the only subcommand that talks gRPC to the store directly (`StoreAdminService`). `--tenant` accepts either a UUID (passed through, scheduler-free) or a name (resolved via `AdminService.ListTenants` — only requires scheduler reachability when the operator passes a name, I-093). `add` takes `--url`, `--priority` (default 50), repeatable `--trusted-key`, and `--sig-mode {keep|add|replace}`.

r[cli.workers.actor-diff]
`rio-cli workers --actor` calls `DebugListExecutors` (in-memory `self.executors` map). `--diff` calls BOTH `ListExecutors` (PG) and `DebugListExecutors` and joins by `executor_id` with per-row `⚠` divergence markers: `pg-only` = stream not connected to this leader (I-048c symptom), `actor-only` = PG `last_seen` stale (transient), `both` + `has_stream=false` = I-048b zombie (heartbeat-created entry, no `stream_tx`). `--actor` header line shows `ZOMBIE`/`DRAINING`/`DEGRADED` markers — all three gate `has_capacity()`, so any present means dispatch can't reach this worker even if stream/registered/warm all show Y.

#import "/lib/rio.typ": *
#show: rio.with(domains: ("cli",))


Admin CLI for rio-build. Thin wrapper over `AdminService` (scheduler) and
`StoreAdminService` (store) gRPC. Run locally via
`cargo xtask k8s cli -- <cmd>` (port-forwards + service-HMAC key fetch). NOT
bundled into control-plane images (#rref("sec.image.control-plane-minimal")).

= Connection model

`--scheduler-addr` (default `localhost:9001`) and `--store-addr` (default
`rio-store.rio-store:9002`) target the in-pod case. gRPC connect is
*per-subcommand* --- `pool` (kube-only) and `upstream`/`verify-chunks`
(store-only) MUST work when the scheduler is unreachable (e.g., to diagnose
why).

`--json` makes handlers print exactly one JSON document to stdout (for `jq`
pipelines). Prost-generated types don't derive `Serialize`; each subcommand
projects to module-local structs so the JSON surface is decoupled from proto
evolution. Streaming subcommands (`logs`, `gc`, `verify-chunks` stdout) ignore
the flag.

#r("cli.rpc-timeout")[
  Unary admin RPCs are wrapped in a 120s deadline.
]

#r("cli.rpc-retry")[
  Unary admin RPCs retry up to twice on `UNAVAILABLE` (1s/2s backoff) before
  surfacing the error --- covers a standby-replica hit or leader-election flip
  without masking a genuinely-down scheduler.
]

All unary admin RPCs go through `rpc()` which applies the retry above and a
per-attempt `RPC_TIMEOUT` deadline. `RPC_TIMEOUT` is 120s --- NOT 30s ---
because actor-routed RPCs (`InspectBuildDag`, `ClusterSnapshot`) queue behind
the actor mailbox, and the operator needs the dump precisely when the actor is
saturated (I-163: \~9.5k mailbox commands under load). `connect_admin` has a
separate 10s connect timeout (TCP/handshake bound). Streaming RPCs
(`TriggerGC`, `LogService.TailLog`, `VerifyChunks`) wrap only the initial call;
per-message progress drains without a whole-call deadline.

= Subcommands

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Subcommand], [Service], [Purpose]),
  [`status`],
  [`AdminService.ClusterStatus`],
  [One-screen workers/builds/queue summary],

  [`workers`],
  [`AdminService.ListExecutors`],
  [Busy-executor list (one entry per open pull-mode attempt)],

  [`builds`],
  [`AdminService.ListBuilds`],
  [Paginated build list with `--status` filter],

  [`derivations`],
  [`AdminService.InspectBuildDag`],
  [Live actor @dag snapshot for a build],

  [`logs`],
  [`LogService.TailLog` (rio-store)],
  [Stream a derivation's log from the store's chunk manifest; `--exec-id` pins
    an execution, default latest],

  [`gc`], [`AdminService.TriggerGC`], [Streamed mark/sweep; `--dry-run`],

  [`poison-list` / `poison-clear`],
  [`AdminService.{ListPoisoned,ClearPoison}`],
  [Show/clear @poison-derivation roots],

  [`cancel-build`],
  [`AdminService.CancelBuild`],
  [Force-cancel an active build (operator override)],

  [`drain-executor`],
  [the removed `AdminService.DrainExecutor`],
  [Retired no-op --- surfaces the scheduler's error naming the successor
    procedures (cordon + exclusion / cancel + Job delete / pool pause)],

  [`pool`], [k8s apiserver (no gRPC)], [`Pool` CR get/describe],

  [`verify-chunks`],
  [`StoreAdminService.VerifyChunks`],
  [PG↔backend chunk audit],

  [`upstream`],
  [`StoreAdminService.{List,Add,Remove}Upstream`],
  [Per-tenant upstream cache CRUD],

  [`create-tenant` / `list-tenants`],
  [`AdminService.{CreateTenant,ListTenants}`],
  [Tenant CRUD],
)

#r("cli.cmd.derivations")[
  `rio-cli derivations [BUILD_ID] [--all-active] [--status S] [--stuck]` calls
  `InspectBuildDag` (#rref("proto.admin.diag-rpc")) --- the live actor view,
  not PG. `--all-active` iterates `ListBuilds(status=active)` first. `--stuck`
  filters to derivations assigned to dead-stream executors
  (`!assigned_executor.is_empty() && !executor_has_stream` --- the I-025
  smoking gun). Per-derivation output surfaces
  `system`/`required_features`/`failed_builders` (the `hard_filter` inputs)
  and per-executor `rejections` so "why won't it dispatch" is one look. Sort:
  stuck first, then status, then name.
]

#r("cli.cmd.cancel-build+2")[
  `rio-cli cancel-build BUILD_ID [--reason R]` calls `AdminService.CancelBuild`
  (service-token gated, `caller_tenant=None` operator override ---
  #rref("admin.rpc.cancel-build")). `SchedulerService.CancelBuild` remains the
  tenant-scoped path used by the gateway and is unreachable from the CLI in
  JWT mode (#rref("sched.tenant.authz")). Idempotent: returns
  `cancelled=false` for already-terminal/unknown. The operator lever for
  orphaned builds (gateway crash mid-disconnect cleanup, I-112); the
  scheduler's orphan-watcher sweep is the automatic counterpart.
]

#r("cli.cmd.verify-chunks")[
  `rio-cli verify-chunks [--batch-size N]` server-streams
  `StoreAdminService.VerifyChunks`. Missing chunk hashes go to *stdout* (one
  hex BLAKE3 per line --- pipeable into `xargs aws s3api head-object`);
  progress goes to *stderr* so `verify-chunks | tee missing.txt` captures
  hashes while progress scrolls. Warns on stream-closed-without-`done` (store
  disconnected mid-scan). I-040 diagnostic.
]

#r("cli.stream.drain-bound")[
  The shared CLI drain law (`drain_until_done`) MUST treat the server's
  terminal sentinel as end-of-stream by construction --- the loop stops at
  the sentinel and never polls again, so a post-sentinel transport error
  (replica restart after sealing, RST before trailers) cannot fail a
  complete audit --- and MUST bound per-message inactivity at 120 s,
  converting the half-open-connection truncation class (peer death without
  FIN/RST on the keepalive-free eager CLI channel) into a nonzero PARTIAL
  exit instead of an unbounded hang.
]
Interactive follow streams (`rio-cli logs`) stay outside this law: an
hour-quiet build log is legitimate idle there (the streaming chain's
1 h silence tolerance), and `logs.rs` remains the documented exit-0
disclosure exception. The 120 s figure matches the CLI's `RPC_TIMEOUT`
budget; `VerifyChunks` emits a progress frame per batch, so one bound
covers every batch shape.

#r("cli.cmd.sla")[
  `rio-cli sla {override|list|clear|reset|status|explain|export-corpus|import-corpus}`
  calls the ADR-023 `AdminService.*SlaOverride` / `ResetSlaModel` /
  `SlaStatus` / `SlaExplain` / `*SlaCorpus` RPCs.
  `override PNAME [--system S] [--tenant T] [--tier NAME] [--cores N] [--mem 8Gi] [--ttl 7d]`
  pins a key (NULL system/tenant are wildcards,
  #rref("sched.sla.override-precedence")); `reset PNAME` drops the key's
  `build_samples` and evicts the cached fit so the next dispatch re-probes;
  `status PNAME` dumps the cached fit + any active override; `explain PNAME`
  shows the per-tier candidate table with rejection reasons;
  `export-corpus -o PATH` / `import-corpus PATH` move fitted curves between
  clusters (ref-second rescaled).
]

#r("cli.cmd.upstream")[
  `rio-cli upstream {list|add|remove} --tenant T …` is the only subcommand
  that talks gRPC to the store directly (`StoreAdminService`). `--tenant`
  accepts either a UUID (passed through, scheduler-free) or a name (resolved
  via `AdminService.ListTenants` --- only requires scheduler reachability when
  the operator passes a name, I-093). `add` takes `--url`, `--priority`
  (default 50), repeatable `--trusted-key`, and
  `--sig-mode {keep|add|replace}`.
]

*Retired (1c' deletion commit C --- the operator surfaces):*
`cli.workers.actor-diff`. The `--actor`/`--diff` modes existed to diff the
scheduler's in-memory executor map against PG (the I-048b/c zombie-stream
diagnostics). That map --- and the divergence class it exposed --- is gone
with the stream session; `rio-cli workers` now reads the durable
open-attempt view directly, and per-pod liveness questions belong to the
Job/pod census (`kubectl get jobs/pods`).

//! rio-cli — thin admin CLI over the scheduler's `AdminService`.
//!
//! Intended for LOCAL use via port-forward (`cargo xtask k8s cli --
//! <cmd>`): xtask opens tunnels to scheduler:9001 + store:9002, fetches
//! the mTLS client cert from the cluster Secret, and execs THIS binary
//! with `RIO_SCHEDULER_ADDR`/`RIO_STORE_ADDR` set.
//!
//! In-pod exec (`kubectl exec deploy/rio-scheduler -- rio-cli <cmd>`)
//! also works.

use std::time::Duration;

use anyhow::anyhow;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use rio_common::backoff::{self, Backoff, Jitter, RetryError};
use rio_common::signal::Token;

use rio_proto::{AdminServiceClient, StoreAdminServiceClient};
use tonic::transport::Channel;

// Subcommand handlers. Each module owns its `#[derive(Args)]` struct
// and `run*` fn so `main.rs` deltas for a new subcommand stay at enum
// variant + match arm + mod decl.
mod bps;
mod builds;
mod derivations;
mod gc;
mod logs;
mod poison;
mod sla;
mod status;
mod tenants;
mod upstream;
mod verify_chunks;
mod workers;

/// Per-RPC deadline. AdminService RPCs used by rio-cli are all unary
/// and cheap (ClusterStatus, ListExecutors, ListBuilds, CreateTenant,
/// ListTenants) — 30s covers a scheduler under load. Unbounded `.await?`
/// here hangs the CLI forever when the scheduler is wedged (recovery
/// stuck, lease contention). Observed: `vm-test-run-rio-cli` 10-min
/// hang after scheduler startup timing changes.
///
/// 120s not 30s (I-163): under heavy actor saturation `ClusterSnapshot`
/// queues behind ~9.5k mailbox commands. The actual fix is decoupling
/// dispatch from Heartbeat (`r[sched.actor.dispatch-decoupled]`) +
/// the cached-snapshot path (`r[sched.admin.snapshot-cached]`) — but
/// `InspectBuildDag` and other actor-routed admin RPCs still queue,
/// and the operator needs the dump precisely when the actor is busy.
///
/// `connect_single` already has a 10s CONNECT timeout (rio-proto
/// client/mod.rs CONNECT_TIMEOUT) — that bounds TCP SYN / handshake.
/// This bounds the RPC itself (scheduler accepted the connection but
/// the handler is blocked on something).
// r[impl cli.rpc-timeout]
pub(crate) const RPC_TIMEOUT: Duration = Duration::from_secs(120);

/// Bounded retry on `UNAVAILABLE`: standby scheduler replica returns
/// it (leader-only RPCs), as does a leader mid-recovery. Three tries
/// with 1s/2s backoff covers a leader-election flip without making a
/// genuinely-down scheduler hang the CLI for minutes.
// r[impl cli.rpc-retry]
const RPC_BACKOFF: Backoff = Backoff {
    base: Duration::from_secs(1),
    mult: 2.0,
    cap: Duration::from_secs(4),
    jitter: Jitter::None,
};
const RPC_MAX_ATTEMPTS: u32 = 3;

/// Wrap an AdminService RPC call with timeout, `UNAVAILABLE` retry,
/// and error context.
///
/// Takes a closure so the call can be re-issued on transient
/// `UNAVAILABLE` (standby replica, leader mid-recovery). Each attempt
/// gets the full `RPC_TIMEOUT`; non-`UNAVAILABLE` status and deadline-
/// exceeded surface immediately.
///
/// **Unary only.** The `T: Default` bound rejects `Streaming<_>` at
/// compile time (prost-generated messages and `()` derive `Default`;
/// `tonic::Streaming` does not). Server-streaming RPCs (TriggerGC,
/// GetBuildLogs) need per-message progress, not a whole-call deadline,
/// and re-issuing the call on retry would silently drop already-
/// received messages — wrap the stream-drain loop instead.
pub(crate) async fn rpc<T: Default>(
    what: &'static str,
    mut op: impl AsyncFnMut() -> Result<tonic::Response<T>, tonic::Status>,
) -> anyhow::Result<T> {
    backoff::retry(
        &RPC_BACKOFF,
        RPC_MAX_ATTEMPTS,
        &Token::new(),
        |s: &tonic::Status| s.code() == tonic::Code::Unavailable,
        |n, s| {
            eprintln!(
                "{what}: UNAVAILABLE ({}); retry {n}/{}",
                s.message(),
                RPC_MAX_ATTEMPTS - 1
            );
        },
        async || {
            tokio::time::timeout(RPC_TIMEOUT, op()).await.map_err(|_| {
                tonic::Status::deadline_exceeded(format!("deadline exceeded after {RPC_TIMEOUT:?}"))
            })?
        },
    )
    .await
    .map(|r| r.into_inner())
    .map_err(|e| match e {
        RetryError::Exhausted { last, .. } => {
            anyhow!("{what}: {} ({:?})", last.message(), last.code())
        }
        RetryError::Cancelled => unreachable!("never-cancelled token"),
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    scheduler_addr: String,
    /// Store gRPC address. Only used by the `upstream` subcommand —
    /// StoreAdminService is hosted on the store's port, not the
    /// scheduler's. In-pod default targets the rio-store Service DNS
    /// (scheduler pod is in rio-system; store pod is in rio-store
    /// namespace since P0454's four-namespace split).
    store_addr: String,
}

impl rio_common::config::ValidateConfig for Config {
    fn validate(&self) -> anyhow::Result<()> {
        rio_common::config::ensure_required(&self.scheduler_addr, "scheduler_addr", "cli")
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // In-pod default. `kubectl exec` into the scheduler pod →
            // AdminService is on the same port as SchedulerService.
            scheduler_addr: "localhost:9001".into(),
            // Cross-namespace DNS. From the scheduler pod (the common
            // `kubectl exec` target), the store is reachable at its
            // Service FQDN. Override via `RIO_STORE_ADDR` for
            // port-forward / standalone use.
            store_addr: "rio-store.rio-store:9002".into(),
        }
    }
}

impl Config {
    /// Connect to the scheduler's `AdminService`. Called once for the
    /// admin-dispatch arm of `main()`; non-admin subcommands (kube /
    /// store / SchedulerService) are matched first so they never `?`
    /// on an unreachable scheduler — `rio-cli bps describe` must work
    /// when the scheduler is down (e.g., to diagnose why).
    async fn connect_admin(&self) -> anyhow::Result<AdminServiceClient<Channel>> {
        rio_proto::client::connect_single(&self.scheduler_addr)
            .await
            .map_err(|e| anyhow!("connect to scheduler at {}: {e}", self.scheduler_addr))
    }

    /// Connect to the store's `StoreAdminService`. Only `upstream` /
    /// `verify-chunks` need this; called per-arm so scheduler-only
    /// subcommands don't fail on an unreachable store.
    async fn connect_store_admin(&self) -> anyhow::Result<StoreAdminServiceClient<Channel>> {
        rio_proto::client::connect_single(&self.store_addr)
            .await
            .map_err(|e| anyhow!("connect to store at {}: {e}", self.store_addr))
    }
}

// rio-cli's CLI surface is the subcommand enum; the cross-cutting
// flags are `--scheduler-addr` and `--json`. TLS is env-only
// sets it via env anyway and three cert-path flags would clutter --help.
#[derive(Parser, Serialize, Default)]
#[command(name = "rio-cli", about = "Admin CLI for rio-build")]
struct CliArgs {
    /// Scheduler gRPC address (host:port). Defaults to localhost:9001
    /// for in-pod use via `kubectl exec`.
    #[arg(long, global = true)]
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduler_addr: Option<String>,

    /// Store gRPC address (host:port). Only used by `upstream` — the
    /// StoreAdminService lives on the store's port, not the
    /// scheduler's.
    #[arg(long, global = true)]
    #[serde(skip_serializing_if = "Option::is_none")]
    store_addr: Option<String>,

    /// Machine-readable JSON output (one document per invocation). With
    /// `--json`, handlers print a single JSON object/array to stdout
    /// and nothing else — `| jq` pipelines are the intended consumer.
    /// Streaming subcommands (`logs`, `gc`) ignore this flag: logs are
    /// raw bytes anyway, and gc progress is line-oriented.
    #[arg(long, global = true)]
    #[serde(skip)]
    json: bool,

    #[command(subcommand)]
    #[serde(skip)]
    cmd: Option<Cmd>,
}

// TODO: rio keys subcommand — blocked on rio-store signing-key admin RPCs
// (Add/List/Retire/RotateClusterKey); see docs/src/components/store.md "Signing keys" future-work note
// and the paired TODO in rio-store/src/grpc/admin.rs
// TODO: rio submit — server-side .drv parse approach pending; cli-A03
// dependency-surface tension (rio-cli must not depend on rio-nix derivation parser)
#[derive(Subcommand, Clone)]
enum Cmd {
    /// Create a tenant. The name maps to the SSH authorized_keys comment
    /// field — builds from keys with this comment are attributed here.
    CreateTenant(tenants::CreateArgs),
    /// List all tenants.
    ListTenants,
    /// Cluster status summary: workers, builds, queue depth.
    Status,
    /// Detailed worker list. `Status` shows a one-line summary per
    /// worker; this shows everything `ListExecutors` returns — features,
    /// size class, current build — for operational drill-down.
    ///
    /// `--actor` reads the scheduler's IN-MEMORY map instead of PG —
    /// what `dispatch_ready()` sees, not what `last_seen` claims.
    /// I-048b/c diagnostic: PG showed fetchers `[alive]` (heartbeat
    /// unary RPC reached the new leader), actor map was empty (bidi
    /// stream stuck on TCP keepalive to old leader). `--diff` shows
    /// both side-by-side with per-row `⚠` divergence markers.
    Workers(workers::Args),
    /// List builds with optional filtering.
    Builds(builds::ListArgs),
    /// Actor in-memory DAG snapshot for a build. Unlike builds (PG
    /// summary) or GetBuildGraph (PG graph), this queries the LIVE
    /// actor — exactly what dispatch_ready() sees. The I-025
    /// diagnostic: if a derivation is Assigned to an executor whose
    /// stream is dead (⚠ no-stream), dispatch is stuck forever.
    ///
    /// Status filter: "Ready" shows queued-not-dispatched (why?),
    /// "Assigned" + ⚠ shows the freeze, "Queued" shows blocked-on-deps.
    Derivations(derivations::Args),
    /// Stream build logs for a derivation.
    ///
    /// The server keys its ring buffer on `derivation_path`, not
    /// `build_id` — the positional here is the drv path. `--build-id`
    /// is needed only for completed builds (S3 lookup path); for
    /// active builds the ring buffer serves logs by drv path alone.
    Logs(logs::Args),
    /// Trigger garbage collection. Scheduler proxies to the store
    /// after populating `extra_roots` from live builds, so in-flight
    /// outputs aren't swept. Progress is streamed line-by-line.
    Gc(gc::Args),
    /// Clear poison state for a derivation so it can be re-scheduled.
    /// Idempotent: exits 0 on a non-poisoned path (cleared=false),
    /// but exits nonzero on a bare hash or invalid path (server
    /// rejects — use `poison-list` to find the right path).
    PoisonClear(poison::ClearArgs),
    /// List poisoned derivations. These are the ROOTS that cascade
    /// DependencyFailed — a single poisoned FOD can block hundreds of
    /// downstream derivations. Output shows the full .drv path (what
    /// `poison-clear` takes), which workers failed, and age (TTL 24h).
    PoisonList,
    /// Cancel an active build. Transitions in-flight derivations to
    /// Cancelled (workers receive CancelSignal → cgroup.kill) and frees
    /// their slots. Idempotent: returns cancelled=false for an already-
    /// terminal or unknown build_id.
    ///
    /// I-112: the operator lever for orphaned builds — a client that
    /// disconnected mid-build SHOULD trigger gateway-side CancelBuild
    /// (P0331), but a gateway crash or gateway→scheduler timeout during
    /// the disconnect cleanup leaves the build Active forever. This is
    /// the manual escape hatch; the scheduler's orphan-watcher sweep is
    /// the automatic one.
    // r[impl cli.cmd.cancel-build]
    CancelBuild(builds::CancelArgs),
    /// Mark a worker draining. The scheduler stops dispatching new
    /// builds to it; in-flight builds complete (or, with --force, are
    /// reassigned). Same RPC the controller fires on SIGTERM/eviction —
    /// this is the manual operator lever for the same path.
    DrainExecutor(workers::DrainArgs),
    /// Inspect BuilderPoolSet CRs via the K8s apiserver (not gRPC).
    /// `get` lists BPSes; `describe` joins spec classes with live
    /// child BuilderPool replica counts + effective-cutoff status —
    /// the spec→child→replica chain kubectl can't show in one place.
    Bps {
        #[command(subcommand)]
        cmd: bps::BpsCmd,
    },
    /// ADR-023 SLA-driven sizing: per-pname overrides, model reset,
    /// cached-fit status, candidate-table explain, seed-corpus export/import.
    Sla {
        #[command(subcommand)]
        cmd: sla::SlaCmd,
    },
    /// PG↔backend chunk consistency audit. HeadObject every non-deleted
    /// chunk; report PG-says-exists-but-S3-says-no. Missing hashes go
    /// to stdout (one hex BLAKE3 per line, pipeable); progress to stderr.
    /// I-040 diagnostic — catches chunks stranded by key-format changes.
    /// Talks to StoreAdminService directly — `--store-addr` required.
    VerifyChunks(verify_chunks::Args),
    /// Manage per-tenant upstream binary-cache substitution config.
    /// Talks to StoreAdminService directly (not scheduler-proxied) —
    /// `--store-addr` or `RIO_STORE_ADDR` must reach the store.
    Upstream {
        #[command(subcommand)]
        cmd: upstream::UpstreamCmd,
    },
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let cmd = cli
        .cmd
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no subcommand given (try --help)"))?;
    let as_json = cli.json;

    // Config load + TLS init are local-only (env/file read, no network)
    // and have working defaults, so they run unconditionally even for
    // kube-only subcommands that ignore them. gRPC CONNECT is per-arm
    // (see `Config::connect_admin` / `connect_store_admin`) — that's
    // the part that must not gate `bps` / `upstream` on an unreachable
    // scheduler.
    let cfg: Config = rio_common::config::load("cli", cli)?;
    {
        use rio_common::config::ValidateConfig as _;
        cfg.validate()?;
    }

    match cmd {
        // kube-only — talks to the K8s apiserver via KUBECONFIG /
        // in-cluster config; no gRPC connect at all.
        Cmd::Bps { cmd } => bps::run(as_json, cmd).await,
        // store-admin only — don't fail on an unreachable scheduler
        // when the operator just wants to add a cache URL or audit
        // chunk consistency.
        Cmd::Upstream { cmd } => {
            let mut sc = cfg.connect_store_admin().await?;
            upstream::run(as_json, &mut sc, &cfg.scheduler_addr, cmd).await
        }
        Cmd::VerifyChunks(a) => verify_chunks::run(&mut cfg.connect_store_admin().await?, a).await,
        // SchedulerService, not AdminService — same address, separate
        // client (see `builds::run_cancel`).
        Cmd::CancelBuild(a) => builds::run_cancel(as_json, &cfg.scheduler_addr, a).await,
        // Everything else talks to AdminService — connect once.
        admin => {
            let mut c = cfg.connect_admin().await?;
            match admin {
                Cmd::CreateTenant(a) => tenants::run_create(as_json, &mut c, a).await,
                Cmd::ListTenants => tenants::run_list(as_json, &mut c).await,
                Cmd::Status => status::run(as_json, &mut c).await,
                Cmd::Workers(a) => workers::run(as_json, &mut c, a).await,
                Cmd::Builds(a) => builds::run_list(as_json, &mut c, a).await,
                Cmd::Derivations(a) => derivations::run(as_json, &mut c, a).await,
                Cmd::Logs(a) => logs::run(&mut c, a).await,
                Cmd::Gc(a) => gc::run(&mut c, a).await,
                Cmd::PoisonClear(a) => poison::run_clear(as_json, &mut c, a).await,
                Cmd::PoisonList => poison::run_list(as_json, &mut c).await,
                Cmd::DrainExecutor(a) => workers::run_drain(as_json, &mut c, a).await,
                Cmd::Sla { cmd } => sla::run(as_json, &mut c, cmd).await,
                Cmd::Bps { .. }
                | Cmd::Upstream { .. }
                | Cmd::VerifyChunks(_)
                | Cmd::CancelBuild(_) => unreachable!("handled above"),
            }
        }
    }
}

// ===========================================================================
// JSON output
// ===========================================================================
//
// The admin-facing proto response types derive `serde::Serialize`
// (targeted `type_attribute` in `rio-proto/build.rs`), so `--json`
// emits the proto value directly. `prost_types::Timestamp` and nested
// `ResourceUsage` fields are `#[serde(skip)]`ed there rather than
// pulling in `prost-wkt-types`; enum fields serialize as their wire
// `i32` (pre-prod — no JSON-shape stability contract yet).

/// Print a serde value as pretty JSON. Single emission point so all
/// `--json` output is consistent (pretty, trailing newline, errors
/// propagate).
pub(crate) fn json<T: Serialize + ?Sized>(v: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Shared `--json` / human list-output triad used by `list-tenants` and
/// other simple list subcommands: JSON array if `as_json`, `empty`
/// placeholder line if no items, else `each` per item.
pub(crate) fn emit<T: Serialize>(
    as_json: bool,
    items: &[T],
    empty: &str,
    each: impl Fn(&T),
) -> anyhow::Result<()> {
    if as_json {
        return json(items);
    }
    if items.is_empty() {
        println!("{empty}");
    } else {
        items.iter().for_each(each);
    }
    Ok(())
}

// ===========================================================================
// Human-readable output helpers
// ===========================================================================

/// Human "Nh Nm ago" from a seconds-ago delta. Shared by `PoisonList`
/// (server returns `poisoned_secs_ago`) and [`fmt_ts_ago`] (epoch input).
pub(crate) fn fmt_secs_ago(ago: u64) -> String {
    if ago < 60 {
        format!("{ago}s ago")
    } else if ago < 3600 {
        format!("{}m {}s ago", ago / 60, ago % 60)
    } else {
        format!("{}h {}m ago", ago / 3600, (ago % 3600) / 60)
    }
}

/// Unix-epoch seconds → human "Nh Nm ago" relative to now.
pub(crate) fn fmt_ts_ago(epoch_secs: Option<i64>) -> String {
    let Some(secs) = epoch_secs else {
        return "—".into();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    fmt_secs_ago((now - secs).max(0) as u64)
}

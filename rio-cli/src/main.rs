//! rio-cli — thin admin CLI over the scheduler's `AdminService`.
//!
//! Intended for LOCAL use via port-forward (`cargo xtask k8s cli --
//! <cmd>`): xtask opens tunnels to scheduler:9001 + store:9002, fetches
//! the mTLS client cert from the cluster Secret, and execs THIS binary
//! with `RIO_SCHEDULER_ADDR`/`RIO_STORE_ADDR`/`RIO_TLS__*` set.
//!
//! In-pod exec (`kubectl exec deploy/rio-scheduler -- rio-cli <cmd>`)
//! also works — the scheduler pod has `RIO_TLS__*` env + certs at
//! `/etc/rio/tls/` — but forces the scheduler image to bundle rio-cli
//! and every runtime dep (jq for `--json`, column for tables, …). See
//! `r[sec.image.control-plane-minimal]`.

use std::time::Duration;

use anyhow::{anyhow, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use rio_common::backoff::{Backoff, Jitter};

use rio_proto::{AdminServiceClient, StoreAdminServiceClient};
use tonic::Code;
use tonic::transport::Channel;

mod bps;
mod builds;
mod cutoffs;
mod derivations;
mod estimator;
mod gc;
mod logs;
mod poison;
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
/// `connect_admin` already has a 10s CONNECT timeout (rio-proto
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
const RPC_RETRY_ATTEMPTS: u32 = 3;
const RPC_RETRY_BACKOFF: Backoff = Backoff {
    base: Duration::from_secs(1),
    mult: 2.0,
    cap: Duration::from_secs(4),
    jitter: Jitter::None,
};

/// Wrap an AdminService RPC call with timeout, `UNAVAILABLE` retry,
/// and error context.
///
/// Takes a closure so the call can be re-issued on transient
/// `UNAVAILABLE` (standby replica, leader mid-recovery). Each attempt
/// gets the full `RPC_TIMEOUT`; non-`UNAVAILABLE` status and deadline-
/// exceeded surface immediately.
///
/// NOT used for streaming AdminService RPCs (TriggerGC, GetBuildLogs)
/// — those need per-message progress, not a whole-call deadline. If
/// a future subcommand adds one, wrap the stream-drain loop instead.
pub(crate) async fn rpc<T>(
    what: &'static str,
    mut op: impl AsyncFnMut() -> Result<tonic::Response<T>, tonic::Status>,
) -> anyhow::Result<T> {
    let mut attempt = 0u32;
    loop {
        match tokio::time::timeout(RPC_TIMEOUT, op()).await {
            Err(_) => bail!("{what}: deadline exceeded after {RPC_TIMEOUT:?}"),
            Ok(Ok(r)) => return Ok(r.into_inner()),
            Ok(Err(s)) if s.code() == Code::Unavailable && attempt + 1 < RPC_RETRY_ATTEMPTS => {
                eprintln!(
                    "{what}: UNAVAILABLE ({}); retry {}/{}",
                    s.message(),
                    attempt + 1,
                    RPC_RETRY_ATTEMPTS - 1
                );
                tokio::time::sleep(RPC_RETRY_BACKOFF.duration(attempt)).await;
                attempt += 1;
            }
            Ok(Err(s)) => bail!("{what}: {} ({:?})", s.message(), s.code()),
        }
    }
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
    tls: rio_common::tls::TlsConfig,
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
            tls: rio_common::tls::TlsConfig::default(),
        }
    }
}

impl Config {
    /// Connect to the scheduler's `AdminService`. Called per-arm so a
    /// subcommand that talks only to the store (or only to kube) never
    /// `?`s on an unreachable scheduler — `rio-cli bps describe` must
    /// work when the scheduler is down (e.g., to diagnose why).
    async fn connect_admin(&self) -> anyhow::Result<AdminServiceClient<Channel>> {
        rio_proto::client::connect_admin(&self.scheduler_addr)
            .await
            .map_err(|e| anyhow!("connect to scheduler at {}: {e}", self.scheduler_addr))
    }

    /// Connect to the store's `StoreAdminService`. Only `upstream` /
    /// `verify-chunks` need this; called per-arm so scheduler-only
    /// subcommands don't fail on an unreachable store.
    async fn connect_store_admin(&self) -> anyhow::Result<StoreAdminServiceClient<Channel>> {
        rio_proto::client::connect_store_admin(&self.store_addr)
            .await
            .map_err(|e| anyhow!("connect to store at {}: {e}", self.store_addr))
    }
}

// rio-cli's CLI surface is the subcommand enum; the cross-cutting
// flags are `--scheduler-addr` and `--json`. TLS is env-only
// (RIO_TLS__*) — no flags, because the in-pod case (the common case)
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
    CreateTenant {
        /// Tenant name (unique, non-empty after trim).
        name: String,
        /// GC retention period in hours. Build artifacts for this tenant
        /// are eligible for sweep after this many hours without access.
        #[arg(long)]
        gc_retention_hours: Option<u32>,
        /// Storage cap in bytes. Soft limit — GC targets this tenant
        /// more aggressively when exceeded.
        #[arg(long)]
        gc_max_store_bytes: Option<u64>,
        /// Bearer token for binary-cache HTTP access. Unset = no cache
        /// access for this tenant.
        #[arg(long)]
        cache_token: Option<String>,
    },
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
    Workers {
        /// Filter by worker status: "alive", "draining", or empty for all.
        /// Ignored with `--actor`/`--diff` (those read the full map).
        #[arg(long)]
        status: Option<String>,
        /// Read the scheduler actor's in-memory executor map instead of
        /// PG. Surfaces `has_stream`/`warm`/`kind` — the dispatch
        /// filter inputs. PG `last_seen` can't tell you if the stream
        /// to THIS leader is dead.
        #[arg(long, conflicts_with = "diff")]
        actor: bool,
        /// Join PG view (`ListExecutors`) with actor view
        /// (`DebugListExecutors`). `⚠` marks rows where they disagree:
        /// PG-only = stream not connected, actor-only = PG stale,
        /// both-but-no-stream = I-048b zombie.
        #[arg(long, conflicts_with = "actor")]
        diff: bool,
    },
    /// List builds with optional filtering.
    Builds {
        /// Filter by build status: "pending", "active", "succeeded",
        /// "failed", "cancelled".
        #[arg(long)]
        status: Option<String>,
        /// Max results (server capped).
        #[arg(long, default_value = "50")]
        limit: u32,
        /// Opaque keyset cursor from a prior page's `next_cursor`.
        /// Stable under concurrent inserts, unlike offset.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Actor in-memory DAG snapshot for a build. Unlike builds (PG
    /// summary) or GetBuildGraph (PG graph), this queries the LIVE
    /// actor — exactly what dispatch_ready() sees. The I-025
    /// diagnostic: if a derivation is Assigned to an executor whose
    /// stream is dead (⚠ no-stream), dispatch is stuck forever.
    ///
    /// Status filter: "Ready" shows queued-not-dispatched (why?),
    /// "Assigned" + ⚠ shows the freeze, "Queued" shows blocked-on-deps.
    Derivations {
        /// Build UUID. Required unless --all-active.
        build_id: Option<String>,
        /// Iterate ALL active builds (ListBuilds status=active). Useful
        /// when you don't know WHICH build is stuck — the I-025 QA
        /// scenario had 4 builds frozen simultaneously.
        #[arg(long, conflicts_with = "build_id")]
        all_active: bool,
        /// Filter by status ("Ready", "Assigned", "Running", "Queued", ...).
        #[arg(long)]
        status: Option<String>,
        /// Only show derivations assigned to dead-stream executors
        /// (the I-025 smoking gun).
        #[arg(long)]
        stuck: bool,
    },
    /// Stream build logs for a derivation.
    ///
    /// The server keys its ring buffer on `derivation_path`, not
    /// `build_id` — the positional here is the drv path. `--build-id`
    /// is needed only for completed builds (S3 lookup path); for
    /// active builds the ring buffer serves logs by drv path alone.
    Logs {
        /// Full derivation store path (e.g. `/nix/store/abc-foo.drv`).
        drv_path: String,
        /// Build UUID. Only needed if the derivation is no longer in
        /// the active-build ring buffer (S3 archive lookup).
        #[arg(long)]
        build_id: Option<String>,
    },
    /// Trigger garbage collection. Scheduler proxies to the store
    /// after populating `extra_roots` from live builds, so in-flight
    /// outputs aren't swept. Progress is streamed line-by-line.
    Gc {
        /// Report what would be collected without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Override the per-tenant retention floor for this sweep.
        /// Paths younger than this are protected even if otherwise
        /// unreachable. Unset = use each tenant's configured retention.
        #[arg(long)]
        grace_hours: Option<u32>,
    },
    /// Clear poison state for a derivation so it can be re-scheduled.
    /// Idempotent: exits 0 on a non-poisoned path (cleared=false),
    /// but exits nonzero on a bare hash or invalid path (server
    /// rejects — use `poison-list` to find the right path).
    PoisonClear {
        /// Full .drv store path (e.g. `/nix/store/abc...-foo.drv`).
        /// Bare hashes are rejected — a silent no-match would look
        /// like "not poisoned" when it's actually "wrong key format".
        drv_path: String,
    },
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
    CancelBuild {
        /// Build UUID (as shown by `rio-cli builds` or `status`).
        build_id: String,
        /// Free-form reason (recorded in the BuildCancelled event +
        /// scheduler logs). Defaults to "operator_request".
        #[arg(long, default_value = "operator_request")]
        reason: String,
    },
    /// Mark a worker draining. The scheduler stops dispatching new
    /// builds to it; in-flight builds complete (or, with --force, are
    /// reassigned). Same RPC the controller fires on SIGTERM/eviction —
    /// this is the manual operator lever for the same path.
    DrainExecutor {
        /// Worker ID (as shown by `rio-cli workers`).
        executor_id: String,
        /// Cancel running builds on the worker (reassign elsewhere)
        /// instead of waiting for them to complete.
        #[arg(long)]
        force: bool,
    },
    /// Size-class cutoff status: configured vs effective (post-rebalancer
    /// EMA drift), per-class queue/running counts, sample-window size.
    /// Surfaces how far SITA-E has drifted from the static TOML config
    /// and whether each class has enough samples to trust its cutoff.
    Cutoffs,
    /// Dump the scheduler's in-memory build-history estimator: per-
    /// `(pname, system)` EMA duration/memory + the size-class that
    /// `classify()` picks under current effective cutoffs. I-124
    /// diagnostic — "why is X routed to large? is its EMA plausible?".
    /// Sorted by sample_count desc (most-observed first).
    Estimator {
        /// Substring match on pname. Omit for all entries.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Inspect BuilderPoolSet CRs via the K8s apiserver (not gRPC).
    /// `get` lists BPSes; `describe` joins spec classes with live
    /// child BuilderPool replica counts + effective-cutoff status —
    /// the spec→child→replica chain kubectl can't show in one place.
    Bps {
        #[command(subcommand)]
        cmd: bps::BpsCmd,
    },
    /// PG↔backend chunk consistency audit. HeadObject every non-deleted
    /// chunk; report PG-says-exists-but-S3-says-no. Missing hashes go
    /// to stdout (one hex BLAKE3 per line, pipeable); progress to stderr.
    /// I-040 diagnostic — catches chunks stranded by key-format changes.
    /// Talks to StoreAdminService directly — `--store-addr` required.
    VerifyChunks {
        /// Chunks per backend exists_batch. 0 = server default (1000).
        #[arg(long, default_value_t = 0)]
        batch_size: u32,
    },
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
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

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
    rio_common::grpc::init_client_tls(rio_common::tls::load_client_tls(&cfg.tls)?);

    match cmd {
        // kube-only — talks to the K8s apiserver via KUBECONFIG /
        // in-cluster config; no gRPC connect at all.
        Cmd::Bps { cmd } => bps::run(as_json, cmd).await?,
        // store-admin only — don't fail on an unreachable scheduler
        // when the operator just wants to add a cache URL or audit
        // chunk consistency.
        Cmd::Upstream { cmd } => {
            let mut sc = cfg.connect_store_admin().await?;
            upstream::run(as_json, &mut sc, &cfg.scheduler_addr, cmd).await?;
        }
        Cmd::VerifyChunks { batch_size } => {
            let mut sc = cfg.connect_store_admin().await?;
            verify_chunks::run(&mut sc, batch_size).await?;
        }
        Cmd::CreateTenant {
            name,
            gc_retention_hours,
            gc_max_store_bytes,
            cache_token,
        } => {
            let mut client = cfg.connect_admin().await?;
            tenants::run_create(
                as_json,
                &mut client,
                name,
                gc_retention_hours,
                gc_max_store_bytes,
                cache_token,
            )
            .await?;
        }
        Cmd::ListTenants => {
            tenants::run_list(as_json, &mut cfg.connect_admin().await?).await?;
        }
        Cmd::Status => status::run(as_json, &mut cfg.connect_admin().await?).await?,
        Cmd::Workers {
            status,
            actor,
            diff,
        } => {
            let mut client = cfg.connect_admin().await?;
            if actor {
                workers::run_actor(as_json, &mut client).await?;
            } else if diff {
                workers::run_diff(as_json, &mut client).await?;
            } else {
                workers::run_pg(as_json, &mut client, status).await?;
            }
        }
        Cmd::Builds {
            status,
            limit,
            cursor,
        } => {
            builds::run_list(
                as_json,
                &mut cfg.connect_admin().await?,
                status,
                limit,
                cursor,
            )
            .await?;
        }
        Cmd::Derivations {
            build_id,
            all_active,
            status,
            stuck,
        } => {
            let mut client = cfg.connect_admin().await?;
            derivations::run(as_json, &mut client, build_id, all_active, status, stuck).await?;
        }
        Cmd::Logs { drv_path, build_id } => {
            let mut client = cfg.connect_admin().await?;
            logs::run(&mut client, drv_path, build_id).await?;
        }
        Cmd::Gc {
            dry_run,
            grace_hours,
        } => gc::run(&mut cfg.connect_admin().await?, dry_run, grace_hours).await?,
        Cmd::PoisonClear { drv_path } => {
            poison::run_clear(as_json, &mut cfg.connect_admin().await?, drv_path).await?;
        }
        Cmd::PoisonList => {
            poison::run_list(as_json, &mut cfg.connect_admin().await?).await?;
        }
        Cmd::CancelBuild { build_id, reason } => {
            builds::run_cancel(as_json, &cfg.scheduler_addr, build_id, reason).await?;
        }
        Cmd::DrainExecutor { executor_id, force } => {
            workers::run_drain(as_json, &mut cfg.connect_admin().await?, executor_id, force)
                .await?;
        }
        Cmd::Cutoffs => cutoffs::run(as_json, &mut cfg.connect_admin().await?).await?,
        Cmd::Estimator { filter } => {
            estimator::run(as_json, filter, &mut cfg.connect_admin().await?).await?;
        }
    }
    Ok(())
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
pub(crate) fn json<T: Serialize>(v: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
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

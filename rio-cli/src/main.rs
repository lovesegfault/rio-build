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

use std::future::Future;
use std::time::Duration;

use anyhow::anyhow;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use rio_common::grpc::with_timeout;

use rio_proto::types::{
    BuildInfo, ClearPoisonRequest, ClusterStatusResponse, CreateTenantRequest,
    DrainExecutorRequest, ExecutorInfo, InspectBuildDagRequest, ListBuildsRequest,
    ListBuildsResponse, ListExecutorsRequest, ListExecutorsResponse, TenantInfo,
};

mod cutoffs;
mod estimator;
mod gc;
mod logs;
mod status;
mod upstream;
mod verify_chunks;
mod workers;
mod wps;

/// Per-RPC deadline. AdminService RPCs used by rio-cli are all unary
/// and cheap (ClusterStatus, ListExecutors, ListBuilds, CreateTenant,
/// ListTenants) — 30s covers a scheduler under load. Unbounded `.await?`
/// here hangs the CLI forever when the scheduler is wedged (recovery
/// stuck, lease contention). Observed: `vm-test-run-rio-cli` 10-min
/// hang after remediations 01/08 shifted scheduler startup timing.
///
/// `connect_admin` already has a 10s CONNECT timeout (rio-proto
/// client/mod.rs CONNECT_TIMEOUT) — that bounds TCP SYN / handshake.
/// This bounds the RPC itself (scheduler accepted the connection but
/// the handler is blocked on something).
pub(crate) const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Wrap an AdminService RPC call with timeout + error context.
///
/// Thin wrapper over [`rio_common::grpc::with_timeout`] that hoists
/// `.into_inner()` so callers get `T` directly. The common helper
/// handles the RPC deadline (`RPC_TIMEOUT`) and tonic::Status → anyhow
/// conversion with the RPC name in the error.
///
/// NOT used for streaming AdminService RPCs (TriggerGC, GetBuildLogs)
/// — those need per-message progress, not a whole-call deadline. If
/// a future subcommand adds one, wrap the stream-drain loop instead.
pub(crate) async fn rpc<T>(
    what: &'static str,
    fut: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
) -> anyhow::Result<T> {
    with_timeout(what, RPC_TIMEOUT, fut)
        .await
        .map(tonic::Response::into_inner)
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
    /// size class, running/max slots — for operational drill-down.
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
    /// `get` lists WPSes; `describe` joins spec classes with live
    /// child BuilderPool replica counts + effective-cutoff status —
    /// the spec→child→replica chain kubectl can't show in one place.
    Wps(wps::WpsArgs),
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
    Upstream(upstream::UpstreamArgs),
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

    // kube-only subcommands — dispatched BEFORE the gRPC connect
    // below. `wps` talks to the K8s apiserver directly (via
    // KUBECONFIG / in-cluster config) and has no dependency on
    // the scheduler being reachable. Running `rio-cli wps describe`
    // when the scheduler is down (e.g., to diagnose why) must
    // work — it would not if we'd already `?`'d on connect_admin.
    //
    // Single-variant match so the `other => other` fallthrough
    // is exhaustive (adding a second kube-only subcommand = add
    // another arm here; doesn't disturb the gRPC match below).
    let cmd = match cmd {
        Cmd::Wps(args) => return wps::run(as_json, args).await,
        other => other,
    };

    let cfg: Config = rio_common::config::load("cli", cli)?;
    {
        use rio_common::config::ValidateConfig as _;
        cfg.validate()?;
    }

    rio_proto::client::init_client_tls(rio_common::tls::load_client_tls(&cfg.tls)?);

    // Store-admin subcommands — dispatched AFTER config load (need
    // store_addr + TLS) but BEFORE the scheduler connect. `upstream`
    // talks only to the store; don't fail on an unreachable scheduler
    // when the operator just wants to add a cache URL.
    let cmd = match cmd {
        Cmd::Upstream(args) => {
            let mut sc = rio_proto::client::connect_store_admin(&cfg.store_addr)
                .await
                .map_err(|e| anyhow!("connect to store at {}: {e}", cfg.store_addr))?;
            return upstream::run(as_json, &mut sc, &cfg.scheduler_addr, args.cmd).await;
        }
        Cmd::VerifyChunks { batch_size } => {
            let mut sc = rio_proto::client::connect_store_admin(&cfg.store_addr)
                .await
                .map_err(|e| anyhow!("connect to store at {}: {e}", cfg.store_addr))?;
            return verify_chunks::run(&mut sc, batch_size).await;
        }
        other => other,
    };

    let mut client = rio_proto::client::connect_admin(&cfg.scheduler_addr)
        .await
        .map_err(|e| anyhow::anyhow!("connect to scheduler at {}: {e}", cfg.scheduler_addr))?;

    match cmd {
        Cmd::CreateTenant {
            name,
            gc_retention_hours,
            gc_max_store_bytes,
            cache_token,
        } => {
            let resp = rpc(
                "CreateTenant",
                client.create_tenant(CreateTenantRequest {
                    tenant_name: name,
                    gc_retention_hours,
                    gc_max_store_bytes,
                    cache_token,
                }),
            )
            .await?;
            let t = resp
                .tenant
                .ok_or_else(|| anyhow!("CreateTenant returned no TenantInfo"))?;
            if as_json {
                json(&TenantJson::from(&t))?;
            } else {
                print_tenant(&t);
            }
        }
        Cmd::ListTenants => {
            let resp = rpc("ListTenants", client.list_tenants(())).await?;
            if as_json {
                json(
                    &resp
                        .tenants
                        .iter()
                        .map(TenantJson::from)
                        .collect::<Vec<_>>(),
                )?;
            } else if resp.tenants.is_empty() {
                println!("(no tenants)");
            } else {
                for t in &resp.tenants {
                    print_tenant(t);
                }
            }
        }
        Cmd::Status => status::run(as_json, &mut client).await?,
        Cmd::Workers {
            status,
            actor,
            diff,
        } => {
            // --actor and --diff delegate to the workers module; the
            // PG-only path stays inline (unchanged behavior — no
            // accidental output-format drift for existing callers).
            if actor {
                workers::run_actor(as_json, &mut client).await?;
            } else if diff {
                workers::run_diff(as_json, &mut client).await?;
            } else {
                let resp = rpc(
                    "ListExecutors",
                    client.list_executors(ListExecutorsRequest {
                        status_filter: status.unwrap_or_default(),
                    }),
                )
                .await?;
                if as_json {
                    json(&WorkersJson::from(&resp))?;
                } else if resp.executors.is_empty() {
                    println!("(no executors)");
                } else {
                    for w in &resp.executors {
                        print_worker(w);
                    }
                }
            }
        }
        Cmd::Builds { status, limit } => {
            let resp = rpc(
                "ListBuilds",
                client.list_builds(ListBuildsRequest {
                    status_filter: status.unwrap_or_default(),
                    limit,
                    ..Default::default()
                }),
            )
            .await?;
            if as_json {
                json(&BuildsJson::from(&resp))?;
            } else if resp.builds.is_empty() {
                println!("(no builds — {} total matching filter)", resp.total_count);
            } else {
                println!("{} builds ({} total):", resp.builds.len(), resp.total_count);
                for b in &resp.builds {
                    print_build(b);
                }
            }
        }
        Cmd::Derivations {
            build_id,
            all_active,
            status,
            stuck,
        } => {
            // Resolve build_id(s): either the one given, or all active.
            let build_ids: Vec<String> = if all_active {
                rpc(
                    "ListBuilds",
                    client.list_builds(ListBuildsRequest {
                        status_filter: "active".into(),
                        limit: 50,
                        ..Default::default()
                    }),
                )
                .await?
                .builds
                .into_iter()
                .map(|b| b.build_id)
                .collect()
            } else {
                vec![build_id.ok_or_else(|| anyhow!("BUILD_ID required (or use --all-active)"))?]
            };

            if build_ids.is_empty() {
                if as_json {
                    json(&Vec::<()>::new())?;
                } else {
                    println!("(no active builds)");
                }
                return Ok(());
            }

            // Prost types don't derive Serialize (see JSON output section
            // below) — project to an inline struct. Includes the raw
            // live_executor_ids list for "this executor is in PG but
            // not here" scripting.
            #[derive(Serialize)]
            struct DrvRow<'a> {
                drv_path: &'a str,
                drv_hash: &'a str,
                status: &'a str,
                is_fod: bool,
                assigned_executor: &'a str,
                executor_has_stream: bool,
                retry_count: u32,
                infra_retry_count: u32,
                backoff_remaining_secs: u64,
                interested_build_count: u32,
                system: &'a str,
                required_features: &'a [String],
                failed_builders: &'a [String],
                #[serde(skip_serializing_if = "Vec::is_empty")]
                rejections: Vec<Rejection<'a>>,
            }
            #[derive(Serialize)]
            struct Rejection<'a> {
                executor_id: &'a str,
                reason: &'a str,
            }
            #[derive(Serialize)]
            struct Out<'a> {
                // Present only in --all-active mode so single-build JSON
                // shape is unchanged (back-compat for existing scripts).
                #[serde(skip_serializing_if = "Option::is_none")]
                build_id: Option<&'a str>,
                derivations: Vec<DrvRow<'a>>,
                live_executor_ids: &'a [String],
            }

            // Collect JSON rows across builds; print text per-build inline.
            // Responses held for the lifetime-borrowing DrvRow projection.
            let mut json_out: Vec<Out<'_>> = Vec::with_capacity(build_ids.len());
            let mut resps = Vec::with_capacity(build_ids.len());
            for id in &build_ids {
                resps.push(
                    rpc(
                        "InspectBuildDag",
                        client.inspect_build_dag(InspectBuildDagRequest {
                            build_id: id.clone(),
                        }),
                    )
                    .await?,
                );
            }

            for (i, (id, resp)) in build_ids.iter().zip(&resps).enumerate() {
                let mut drvs: Vec<_> = resp
                    .derivations
                    .iter()
                    .filter(|d| status.as_ref().is_none_or(|s| d.status == *s))
                    .filter(|d| {
                        !stuck || (!d.assigned_executor.is_empty() && !d.executor_has_stream)
                    })
                    .collect();
                // Sort: stuck first, then by status, then by name.
                drvs.sort_by(|a, b| {
                    let a_stuck = !a.assigned_executor.is_empty() && !a.executor_has_stream;
                    let b_stuck = !b.assigned_executor.is_empty() && !b.executor_has_stream;
                    b_stuck
                        .cmp(&a_stuck)
                        .then(a.status.cmp(&b.status))
                        .then(a.drv_path.cmp(&b.drv_path))
                });

                if as_json {
                    json_out.push(Out {
                        build_id: all_active.then_some(id.as_str()),
                        derivations: drvs
                            .iter()
                            .map(|d| DrvRow {
                                drv_path: &d.drv_path,
                                drv_hash: &d.drv_hash,
                                status: &d.status,
                                is_fod: d.is_fod,
                                assigned_executor: &d.assigned_executor,
                                executor_has_stream: d.executor_has_stream,
                                retry_count: d.retry_count,
                                infra_retry_count: d.infra_retry_count,
                                backoff_remaining_secs: d.backoff_remaining_secs,
                                interested_build_count: d.interested_build_count,
                                system: &d.system,
                                required_features: &d.required_features,
                                failed_builders: &d.failed_builders,
                                rejections: d
                                    .rejections
                                    .iter()
                                    .map(|r| Rejection {
                                        executor_id: &r.executor_id,
                                        reason: &r.reason,
                                    })
                                    .collect(),
                            })
                            .collect(),
                        live_executor_ids: &resp.live_executor_ids,
                    });
                } else {
                    // Per-build header (only when iterating multiple).
                    if build_ids.len() > 1 {
                        if i > 0 {
                            println!();
                        }
                        println!("═══ build {id} ═══");
                    }
                    // Summary header.
                    let by_status: std::collections::BTreeMap<&str, usize> =
                        drvs.iter().fold(Default::default(), |mut m, d| {
                            *m.entry(d.status.as_str()).or_default() += 1;
                            m
                        });
                    let stuck_count = drvs
                        .iter()
                        .filter(|d| !d.assigned_executor.is_empty() && !d.executor_has_stream)
                        .count();
                    println!(
                        "{} derivations ({} live executors in stream pool)",
                        drvs.len(),
                        resp.live_executor_ids.len()
                    );
                    for (s, n) in &by_status {
                        println!("  {s}: {n}");
                    }
                    if stuck_count > 0 {
                        println!("  ⚠ {stuck_count} assigned to dead-stream executors (I-025)");
                    }
                    println!();
                    // Per-derivation lines.
                    for d in &drvs {
                        let name = d
                            .drv_path
                            .rsplit_once('-')
                            .map(|(_, n)| n)
                            .unwrap_or(&d.drv_path);
                        let name = name.strip_suffix(".drv").unwrap_or(name);
                        let fod = if d.is_fod { " [FOD]" } else { "" };
                        // I-062: surface hard_filter inputs that would
                        // explain a stuck-Ready. system always shown
                        // (cheap, distinguishes "builtin" FODs from
                        // arch-specific); features/failed-on only when
                        // non-empty (rare — when present, THE answer).
                        let sys = format!(" [{}]", d.system);
                        let feats = if d.required_features.is_empty() {
                            String::new()
                        } else {
                            format!(" feat={}", d.required_features.join(","))
                        };
                        let failed_on = if d.failed_builders.is_empty() {
                            String::new()
                        } else {
                            format!(" ⚠ failed-on:{}", d.failed_builders.join(","))
                        };
                        let stream = if !d.assigned_executor.is_empty() {
                            if d.executor_has_stream {
                                format!(" → {}", d.assigned_executor)
                            } else {
                                format!(" → {} ⚠ no-stream", d.assigned_executor)
                            }
                        } else {
                            String::new()
                        };
                        let backoff = if d.backoff_remaining_secs > 0 {
                            format!(" (backoff {}s)", d.backoff_remaining_secs)
                        } else {
                            String::new()
                        };
                        let retries = if d.retry_count > 0 || d.infra_retry_count > 0 {
                            format!(" r={}/i={}", d.retry_count, d.infra_retry_count)
                        } else {
                            String::new()
                        };
                        println!(
                            "  [{:<9}]{fod}{sys} {name}{stream}{backoff}{retries}{feats}{failed_on}",
                            d.status
                        );
                        // I-062: per-executor rejection reasons. Only
                        // populated for Ready (server-side gate). When
                        // present, this IS the answer to "why won't it
                        // dispatch" — every executor named with the
                        // first hard_filter clause that rejects, or
                        // ACCEPT (which means the rejection is OUTSIDE
                        // hard_filter — that's the load-bearing finding).
                        if !d.rejections.is_empty() {
                            let rj = d
                                .rejections
                                .iter()
                                .map(|r| format!("{}={}", r.executor_id, r.reason))
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!("      rejected-by: {rj}");
                        }
                    }
                }
            }

            if as_json {
                // --all-active: array of per-build objects.
                // Single build: the one object directly (back-compat).
                if all_active {
                    json(&json_out)?;
                } else {
                    json(&json_out[0])?;
                }
            }
        }
        Cmd::Logs { drv_path, build_id } => logs::run(&mut client, drv_path, build_id).await?,
        Cmd::Gc { dry_run } => gc::run(&mut client, dry_run).await?,
        Cmd::PoisonClear { drv_path } => {
            // Validate BEFORE the RPC. The scheduler's DAG is keyed on
            // the full store path, so a bare hash silently no-matches
            // and cleared=false looks like "not poisoned" when it's
            // actually "you gave me the wrong key".
            if !drv_path.starts_with("/nix/store/") || !drv_path.ends_with(".drv") {
                anyhow::bail!(
                    "expected full .drv store path (e.g. /nix/store/abc...-foo.drv), got '{drv_path}'.\n\
                     Run `rio-cli poison-list` to find the right path."
                );
            }
            let resp = rpc(
                "ClearPoison",
                client.clear_poison(ClearPoisonRequest {
                    derivation_hash: drv_path.clone(),
                }),
            )
            .await?;
            if as_json {
                #[derive(Serialize)]
                struct ClearedJson<'a> {
                    drv_path: &'a str,
                    cleared: bool,
                }
                json(&ClearedJson {
                    drv_path: &drv_path,
                    cleared: resp.cleared,
                })?;
            } else if resp.cleared {
                println!("cleared poison for {drv_path}");
            } else {
                println!("{drv_path}: not poisoned (nothing to clear)");
            }
        }
        Cmd::PoisonList => {
            let resp = rpc("ListPoisoned", client.list_poisoned(())).await?;
            if as_json {
                #[derive(Serialize)]
                struct Row<'a> {
                    drv_path: &'a str,
                    failed_executors: &'a [String],
                    poisoned_secs_ago: u64,
                }
                json(
                    &resp
                        .derivations
                        .iter()
                        .map(|d| Row {
                            drv_path: &d.drv_path,
                            failed_executors: &d.failed_executors,
                            poisoned_secs_ago: d.poisoned_secs_ago,
                        })
                        .collect::<Vec<_>>(),
                )?;
            } else if resp.derivations.is_empty() {
                println!("no poisoned derivations");
            } else {
                for d in &resp.derivations {
                    let age_h = d.poisoned_secs_ago / 3600;
                    let age_m = (d.poisoned_secs_ago % 3600) / 60;
                    println!(
                        "{}\n  failed on: {}\n  poisoned:  {age_h}h{age_m}m ago (TTL 24h)",
                        d.drv_path,
                        d.failed_executors.join(", ")
                    );
                }
            }
        }
        Cmd::CancelBuild { build_id, reason } => {
            // CancelBuild lives on SchedulerService, not AdminService —
            // it's the same RPC the gateway calls on client disconnect.
            // Same address (scheduler hosts both services on one port),
            // separate client. Unary — rpc() helper applies.
            let mut sched = rio_proto::client::connect_scheduler(&cfg.scheduler_addr)
                .await
                .map_err(|e| anyhow!("connect to scheduler at {}: {e}", cfg.scheduler_addr))?;
            let resp = rpc(
                "CancelBuild",
                sched.cancel_build(rio_proto::types::CancelBuildRequest {
                    build_id: build_id.clone(),
                    reason,
                }),
            )
            .await?;
            if as_json {
                #[derive(Serialize)]
                struct CancelJson<'a> {
                    build_id: &'a str,
                    cancelled: bool,
                }
                json(&CancelJson {
                    build_id: &build_id,
                    cancelled: resp.cancelled,
                })?;
            } else if resp.cancelled {
                println!("cancelled build {build_id}");
            } else {
                // Server returns false for already-terminal AND
                // BuildNotFound (the latter as a tonic NotFound error,
                // which `rpc()` would have surfaced above — so false
                // here means "found but already terminal").
                println!("{build_id}: already terminal (nothing to cancel)");
            }
        }
        Cmd::DrainExecutor { executor_id, force } => {
            // Unary — rpc() helper applies. Server returns accepted=true
            // for known workers (idempotent on already-draining) and
            // accepted=false for unknown ids (per admin/tests.rs, not
            // an error — just a no-op). Empty id is InvalidArgument.
            let resp = rpc(
                "DrainExecutor",
                client.drain_executor(DrainExecutorRequest {
                    executor_id: executor_id.clone(),
                    force,
                }),
            )
            .await?;
            if as_json {
                #[derive(Serialize)]
                struct DrainJson<'a> {
                    executor_id: &'a str,
                    accepted: bool,
                    running_builds: u32,
                }
                json(&DrainJson {
                    executor_id: &executor_id,
                    accepted: resp.accepted,
                    running_builds: resp.running_builds,
                })?;
            } else if resp.accepted {
                println!(
                    "draining {executor_id} ({} build{} {})",
                    resp.running_builds,
                    if resp.running_builds == 1 { "" } else { "s" },
                    if force { "reassigned" } else { "in flight" }
                );
            } else {
                println!("{executor_id}: not found (nothing to drain)");
            }
        }
        Cmd::Cutoffs => cutoffs::run(as_json, &mut client).await?,
        Cmd::Estimator { filter } => estimator::run(as_json, filter, &mut client).await?,
        // Dispatched above (before gRPC connect). The early match
        // consumes the Wps variant and returns; this arm is reached
        // only if the dispatch order is broken — fail loud.
        //
        // NOT splitting Cmd into KubeCmd|GrpcCmd yet: single kube-
        // only variant, and the unreachable! fails loud on first
        // invocation (not silent). When a SECOND kube-only
        // subcommand lands (e.g., a BuilderPool describe that
        // doesn't need admin RPC), reconsider the split.
        Cmd::Wps(_) => unreachable!("Wps handled before gRPC connect"),
        Cmd::Upstream(_) => unreachable!("Upstream handled before scheduler connect"),
        Cmd::VerifyChunks { .. } => unreachable!("VerifyChunks handled before scheduler connect"),
    }
    Ok(())
}

// ===========================================================================
// JSON output
// ===========================================================================
//
// Prost-generated types don't derive `serde::Serialize` (enabling it
// workspace-wide would be a blast-radius change: every rio crate's
// serialization surface shifts, `google.protobuf.Timestamp` needs
// `prost-wkt-types`, enum repr changes). Thin per-subcommand wrappers
// project just the fields a CLI consumer cares about — stable JSON
// surface under our control, not coupled to proto evolution.
//
// Timestamps (`prost_types::Timestamp`) and nested `ResourceUsage`
// are flattened / stringified rather than projected structurally —
// `rio-cli --json | jq` consumers want readable scalars, not
// `{seconds: N, nanos: M}` objects.

/// Print a serde value as pretty JSON. Single emission point so all
/// `--json` output is consistent (pretty, trailing newline, errors
/// propagate).
pub(crate) fn json<T: Serialize>(v: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

#[derive(Serialize)]
struct TenantJson<'a> {
    tenant_id: &'a str,
    tenant_name: &'a str,
    gc_retention_hours: u32,
    gc_max_store_bytes: Option<u64>,
    has_cache_token: bool,
}
impl<'a> From<&'a TenantInfo> for TenantJson<'a> {
    fn from(t: &'a TenantInfo) -> Self {
        Self {
            tenant_id: &t.tenant_id,
            tenant_name: &t.tenant_name,
            gc_retention_hours: t.gc_retention_hours,
            gc_max_store_bytes: t.gc_max_store_bytes,
            has_cache_token: t.has_cache_token,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct StatusJson {
    total_executors: u32,
    active_executors: u32,
    draining_executors: u32,
    pending_builds: u32,
    active_builds: u32,
    queued_derivations: u32,
    running_derivations: u32,
    store_size_bytes: u64,
}
impl From<&ClusterStatusResponse> for StatusJson {
    fn from(s: &ClusterStatusResponse) -> Self {
        Self {
            total_executors: s.total_executors,
            active_executors: s.active_executors,
            draining_executors: s.draining_executors,
            pending_builds: s.pending_builds,
            active_builds: s.active_builds,
            queued_derivations: s.queued_derivations,
            running_derivations: s.running_derivations,
            store_size_bytes: s.store_size_bytes,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct ExecutorJson<'a> {
    executor_id: &'a str,
    status: &'a str,
    systems: &'a [String],
    supported_features: &'a [String],
    running_builds: u32,
    size_class: &'a str,
}
impl<'a> From<&'a ExecutorInfo> for ExecutorJson<'a> {
    fn from(w: &'a ExecutorInfo) -> Self {
        Self {
            executor_id: &w.executor_id,
            status: &w.status,
            systems: &w.systems,
            supported_features: &w.supported_features,
            running_builds: w.running_builds,
            size_class: &w.size_class,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct BuildJson<'a> {
    build_id: &'a str,
    tenant_id: &'a str,
    /// `BuildState` enum stringified via prost's `as_str_name` — gives
    /// `"BUILD_STATE_ACTIVE"` etc. Proto-native naming so consumers
    /// can round-trip it through the proto enum if they need to.
    state: &'a str,
    priority_class: &'a str,
    total_derivations: u32,
    completed_derivations: u32,
    cached_derivations: u32,
    error_summary: &'a str,
}
impl<'a> From<&'a BuildInfo> for BuildJson<'a> {
    fn from(b: &'a BuildInfo) -> Self {
        Self {
            build_id: &b.build_id,
            tenant_id: &b.tenant_id,
            state: b.state().as_str_name(),
            priority_class: &b.priority_class,
            total_derivations: b.total_derivations,
            completed_derivations: b.completed_derivations,
            cached_derivations: b.cached_derivations,
            error_summary: &b.error_summary,
        }
    }
}

/// Top-level wrapper so `rio-cli workers --json | jq '.executors | length'`
/// works. A bare array would work too, but the named key future-proofs
/// for adding metadata (e.g. snapshot time) without a breaking change.
#[derive(Serialize)]
struct WorkersJson<'a> {
    executors: Vec<ExecutorJson<'a>>,
}
impl<'a> From<&'a ListExecutorsResponse> for WorkersJson<'a> {
    fn from(r: &'a ListExecutorsResponse) -> Self {
        Self {
            executors: r.executors.iter().map(ExecutorJson::from).collect(),
        }
    }
}

#[derive(Serialize)]
struct BuildsJson<'a> {
    builds: Vec<BuildJson<'a>>,
    total_count: u32,
}
impl<'a> From<&'a ListBuildsResponse> for BuildsJson<'a> {
    fn from(r: &'a ListBuildsResponse) -> Self {
        Self {
            builds: r.builds.iter().map(BuildJson::from).collect(),
            total_count: r.total_count,
        }
    }
}

// ===========================================================================
// Human-readable output
// ===========================================================================

fn print_tenant(t: &TenantInfo) {
    println!(
        "tenant {} ({})  gc_retention={}h  max_store={}  cache_token={}",
        t.tenant_name,
        t.tenant_id,
        t.gc_retention_hours,
        t.gc_max_store_bytes
            .map(|b| format!("{b}B"))
            .unwrap_or_else(|| "unlimited".into()),
        if t.has_cache_token { "yes" } else { "no" }
    );
}

/// Detailed per-worker view for `rio-cli workers`. `Status` prints a
/// compact one-liner; this is the "show me everything" form for
/// debugging a specific worker's registration or feature advertisement.
fn print_worker(w: &ExecutorInfo) {
    println!("worker {} [{}]", w.executor_id, w.status);
    println!(
        "  state:    {}",
        if w.running_builds > 0 { "busy" } else { "idle" }
    );
    println!("  systems:  {}", w.systems.join(", "));
    if !w.supported_features.is_empty() {
        println!("  features: {}", w.supported_features.join(", "));
    }
    if !w.size_class.is_empty() {
        println!("  size:     {}", w.size_class);
    }
    if let Some(r) = &w.resources {
        println!(
            "  cpu={:.2}  mem={}/{}  disk={}/{}",
            r.cpu_fraction,
            r.memory_used_bytes,
            r.memory_total_bytes,
            r.disk_used_bytes,
            r.disk_total_bytes
        );
    }
}

fn print_build(b: &BuildInfo) {
    println!(
        "  build {} [{:?}] {}/{} drv ({} cached) tenant={} prio={}",
        b.build_id,
        b.state(),
        b.completed_derivations,
        b.total_derivations,
        b.cached_derivations,
        b.tenant_id,
        b.priority_class,
    );
    if !b.error_summary.is_empty() {
        println!("    error: {}", b.error_summary);
    }
}

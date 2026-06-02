//! `cargo xtask replay dev` — local engine run and the offline dry-run.
//!
//! Two dev affordances, both deliberately small:
//!
//! - **Live dev run** (default): wrap the engine's existing local input path
//!   (`rio-replay run --spec … --archive <local> --no-s3`) in-process against
//!   a real gateway — either an explicit `--store ssh-ng://…` endpoint or a
//!   `--provider k3s` port-forward to `svc/rio-gateway`. No Job, no
//!   ConfigMap, no S3: the spec and every campaign artifact live under a
//!   local state directory. This is **not a measurement surface** — results
//!   and comparability from a dev run are never publishable, and the command
//!   says so in its output.
//! - **Offline dry-run** (`--dry-run`): build the timed schedule and resolve
//!   the supply ladder fully offline via the engine's dry-run planner — no
//!   cluster, no AWS, no network, no spec file. CI exercises this path
//!   against the committed fixture archive so the operator entry point can
//!   never silently drift from the engine's planner.
//!
//! The engine transport mandates SSH host-key pinning (the campaign spec is
//! rejected without `cluster.gateway_host_key`), so live dev runs must pass
//! the gateway's host key explicitly via `--ssh-host-key`; only `--dry-run`
//! works without one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use rio_replay::archive::reader::{ArchiveFormat, ReplayArchive};
use rio_replay::archive::schema::Capability;
use rio_replay::run::RunArgs;
use rio_replay::run::spec::{
    ArchiveRef, CampaignSpec, ClusterEndpoints, Filters, Knobs, Mode as EngineMode, S3Target,
    SchedulingBlock, TenantBlock,
};
use rio_replay::run::timeline::{TimedDryRunPlan, plan_timed_dry_run};

use super::TENANT_WARM;
use super::launch::{Mode, Schedule};
use crate::k8s::shared::{self, ProcessGuard};
use crate::ui;

#[derive(Args)]
pub struct DevArgs {
    /// Local replay archive: a directory-form archive or a packed .dwarfs
    /// image. Live runs need a v1 archive (the spec pins its
    /// content-addressed identity); v0 archives are accepted for --dry-run
    /// only.
    #[arg(long)]
    pub archive: PathBuf,
    /// Plan fully offline and print the schedule/workload/supply counts as
    /// JSON — no cluster, no AWS, no spec file is written.
    #[arg(long)]
    pub dry_run: bool,
    /// Explicit ssh-ng:// gateway store URL (e.g. against a manual port
    /// forward or a local gateway). Mutually exclusive with --provider.
    #[arg(long, conflicts_with = "provider")]
    pub store: Option<String>,
    /// Port-forward the dev cluster's gateway/scheduler/store instead of
    /// naming an endpoint with --store. Requires --ssh-key.
    #[arg(long, value_enum, requires = "ssh_key")]
    pub provider: Option<DevProvider>,
    /// Tenant SSH private key the engine dials the gateway with. Appended
    /// to the store URL as its `ssh-key=` query parameter when the URL
    /// lacks one; required with --provider.
    #[arg(long)]
    pub ssh_key: Option<PathBuf>,
    /// Pinned gateway SSH host key: an OpenSSH public-key line
    /// (`ssh-ed25519 AAAA… comment`) or a `SHA256:…` fingerprint. Required
    /// for live runs — the engine transport mandates host-key pinning and
    /// refuses a spec without it. Read it from the deployed gateway's
    /// host-key Secret, or `ssh-keyscan` the forwarded port.
    #[arg(long)]
    pub ssh_host_key: Option<String>,
    /// Dependency mode: leaf = dependencies substituted from upstream
    /// caches and roots force-built; self-hosted = full closure built by
    /// rio. Dev runs default to self-hosted so they need no upstream
    /// configuration.
    #[arg(long, value_enum, default_value_t = Mode::SelfHosted)]
    pub mode: Mode,
    /// When submissions happen: timeless = queue-driven dispatch; timed =
    /// recorded request offsets divided by --speedup (requires an archive
    /// with the `timed` capability; the engine validates).
    #[arg(long, value_enum, default_value_t = Schedule::Timeless)]
    pub schedule: Schedule,
    /// Divisor applied to recorded request offsets when building the timed
    /// schedule. Only meaningful with --schedule timed.
    #[arg(long, default_value_t = 1.0)]
    pub speedup: f64,
    /// Cap on attempted jobs (dev runs are fixture/smoke scale).
    #[arg(long)]
    pub limit: Option<usize>,
    /// Scheduler AdminService address (host:port). With --provider this is
    /// replaced by the bound port-forward address.
    #[arg(long, default_value = "127.0.0.1:9001")]
    pub scheduler_addr: String,
    /// Store StoreService address (host:port). With --provider this is
    /// replaced by the bound port-forward address.
    #[arg(long, default_value = "127.0.0.1:9002")]
    pub store_addr: String,
    /// Local state directory; the campaign-id subdirectory (`dev-<archive
    /// id prefix>`) is appended, so re-running the same archive resumes the
    /// same state.
    #[arg(long, default_value = "target/replay-dev")]
    pub state_dir: PathBuf,
}

/// Dev-cluster provider for the port-forward path. Only k3s exists here on
/// purpose: EKS clusters are measurement clusters and campaigns against them
/// go through `replay launch`, never through dev mode.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[value(rename_all = "lower")]
pub enum DevProvider {
    K3s,
}

/// Printed before and after every live dev run so its artifacts can never be
/// mistaken for campaign results.
const NOT_A_MEASUREMENT: &str = "dev run — not a measurement surface; results and comparability from this run are not publishable";

/// Open a local archive (directory form or .dwarfs image), wrapping the
/// error with the path so a typo'd --archive names itself.
fn open_archive(path: &Path) -> Result<ReplayArchive> {
    ReplayArchive::open(path).with_context(|| format!("open replay archive {}", path.display()))
}

/// Campaign knobs for a dev run: the speedup from the CLI, every other knob
/// at the engine default.
fn dev_knobs(a: &DevArgs) -> Knobs {
    Knobs {
        speedup: a.speedup,
        ..Knobs::default()
    }
}

/// One-line archive identity header printed by both dev paths: path, format,
/// short id (`v0` for archives with no content-addressed identity), and the
/// capability flags that decide what a replay of this archive can do.
fn archive_header(path: &Path, archive: &ReplayArchive) -> String {
    let format = match archive.format() {
        ArchiveFormat::V0 => "v0",
        ArchiveFormat::V1 => "v1",
    };
    let id = archive
        .archive_id_short()
        .unwrap_or_else(|| "v0".to_string());
    let caps = archive.capabilities();
    // One `flag=bool` pair per capability, derived from the closed enum so
    // a new flag can never be silently missing from this header.
    let flags = Capability::ALL
        .iter()
        .map(|capability| format!("{}={}", capability.flag(), capability.enabled_in(caps)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "archive {} — format {format}, id {id}, capabilities: {flags}",
        path.display(),
    )
}

/// Build the fully offline dry-run plan: open the archive and run the
/// engine's timed dry-run planner over it. Pure library calls — no tokio,
/// no network, no cluster, no AWS — which is exactly the offline contract
/// the unit test over the committed fixture archive pins.
fn dry_run_plan(a: &DevArgs) -> Result<TimedDryRunPlan> {
    let archive = open_archive(&a.archive)?;
    plan_timed_dry_run(&archive, &dev_knobs(a), a.limit).context("plan the timed dry run offline")
}

/// Apply --ssh-key to a gateway store URL that lacks the `ssh-key=` query
/// parameter (the engine's endpoint parser requires one — it names the
/// tenant private key the transport dials with). A URL that already carries
/// the parameter is used verbatim.
fn store_url_with_key(store: &str, ssh_key: Option<&Path>) -> Result<String> {
    if store.contains("ssh-key=") {
        return Ok(store.to_string());
    }
    let key = ssh_key.context(
        "the gateway store URL carries no ssh-key= query parameter and no --ssh-key was given — \
         the engine dials the gateway with the tenant's private key; pass --ssh-key <path> (or \
         embed ssh-key=<path> in --store)",
    )?;
    let separator = if store.contains('?') { '&' } else { '?' };
    Ok(format!("{store}{separator}ssh-key={}", key.display()))
}

/// Build the local-only campaign spec for a live dev run. Pure (no cluster,
/// no AWS): the gateway endpoint and gRPC addresses come from the (resolved)
/// CLI flags, S3 sync is disabled, and the archive is pinned by the digest
/// computed from the local archive itself.
fn build_dev_spec(a: &DevArgs, archive_digest: &str) -> Result<CampaignSpec> {
    let store = a.store.as_deref().context(
        "the dev run needs a gateway endpoint: pass --store ssh-ng://… or --provider k3s \
         (offline planning needs neither — use --dry-run)",
    )?;
    let gateway_store_url = store_url_with_key(store, a.ssh_key.as_deref())?;
    // The engine transport mandates host-key pinning and the spec is
    // rejected without one, so the refusal happens here with the dev flag
    // named instead of failing later inside engine validation.
    let gateway_host_key = a.ssh_host_key.clone().context(
        "live dev runs must pin the gateway SSH host key: pass --ssh-host-key <openssh public \
         key line | SHA256:… fingerprint> (read it from the deployed gateway's host-key Secret, \
         or ssh-keyscan the forwarded port); only --dry-run works without one",
    )?;
    let mode = a.mode.engine();
    let spec = CampaignSpec {
        // Deterministic id derived from the archive identity so re-running
        // the same archive resumes the same local state dir.
        campaign_id: Some(format!("dev-{}", &archive_digest[..12])),
        mode,
        // Local pin only: no S3 location, just the digest the engine
        // re-verifies against the locally opened archive.
        archive: ArchiveRef {
            s3_bucket: None,
            s3_prefix: None,
            digest: archive_digest.to_string(),
        },
        // S3 sync disabled — every artifact of a dev run stays on local
        // disk under the state dir.
        s3: S3Target {
            bucket: None,
            ..S3Target::default()
        },
        cluster: ClusterEndpoints {
            gateway_store_url,
            // The supply prefetch arm (leaf mode) dials the gateway as the
            // warm tenant with `<ssh_key_dir>/<warm tenant>`; self-hosted
            // mode never prefetches, so the directory is wired only for
            // leaf-mode dev runs.
            ssh_key_dir: match (mode, &a.ssh_key) {
                (EngineMode::Leaf, Some(key)) => key.parent().map(Path::to_path_buf),
                _ => None,
            },
            scheduler_addr: a.scheduler_addr.clone(),
            store_addr: a.store_addr.clone(),
            // Dev clusters run verifier-less schedulers; Admin reads are
            // tokenless.
            service_hmac_key_path: None,
            gateway_host_key: Some(gateway_host_key),
        },
        tenants: TenantBlock {
            build_tenant: mode.expected_build_tenant().to_owned(),
            warm_tenant: TENANT_WARM.to_owned(),
            // No launch pre-flight runs for dev: the tenants are never
            // verified and the engine is always started with
            // --allow-unverified-tenants.
            upstreams_verified: false,
            ..TenantBlock::default()
        },
        filters: Filters {
            limit: a.limit,
            ..Filters::default()
        },
        scheduling: SchedulingBlock {
            mode: a.schedule.engine(),
        },
        knobs: dev_knobs(a),
        ..CampaignSpec::default()
    };
    // The engine's own validation is the contract: a non-default --speedup
    // without --schedule timed, a timed schedule with a contradictory
    // supply policy, etc. are all refused here, before anything dials.
    spec.validate()
        .context("constructed dev campaign spec failed engine validation")?;
    Ok(spec)
}

/// The engine `run` arguments for a live dev run: the spec written into the
/// state dir, the local archive (no S3 fetch), no S3 sync, no deadline, and
/// --allow-unverified-tenants always set (no launch pre-flight runs for dev,
/// so the spec always records the tenants as unverified).
fn engine_run_args(a: &DevArgs, state_dir: &Path) -> RunArgs {
    RunArgs {
        spec: state_dir.join("spec.json"),
        state_dir: state_dir.to_path_buf(),
        archive: Some(a.archive.clone()),
        limit: a.limit,
        deadline: None,
        allow_unverified_tenants: true,
        no_s3: true,
    }
}

pub async fn run(a: DevArgs) -> Result<()> {
    if a.dry_run {
        return dry_run(&a);
    }
    live_run(a).await
}

/// The fully offline dry-run: archive identity header plus the engine's
/// dry-run plan, printed as pretty JSON. No cluster, no AWS, no spec file.
#[allow(clippy::print_stdout)]
fn dry_run(a: &DevArgs) -> Result<()> {
    // The header needs the archive's identity and capability flags;
    // dry_run_plan opens the archive again behind its pure path-in /
    // counts-out signature. Dev archives are fixture/smoke scale, so the
    // second open is cheaper than threading an open handle through the
    // unit-tested signature.
    {
        let archive = open_archive(&a.archive)?;
        println!("{}", archive_header(&a.archive, &archive));
    }
    let plan = dry_run_plan(a)?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}

/// Live dev run: resolve the gateway endpoint (explicit --store or a
/// --provider port-forward), write the local spec, and run the engine
/// in-process over its local-archive/no-S3 path.
#[allow(clippy::print_stdout)]
async fn live_run(mut a: DevArgs) -> Result<()> {
    // Open the archive first: a live run needs a v1 archive (the spec pins
    // its content-addressed identity), and a bad --archive should fail
    // before any port-forward is spawned.
    let open_path = a.archive.clone();
    let archive = tokio::task::spawn_blocking(move || ReplayArchive::open(&open_path))
        .await
        .context("archive open task panicked or was cancelled")?
        .with_context(|| format!("open replay archive {}", a.archive.display()))?;
    println!("{}", archive_header(&a.archive, &archive));
    let Some(digest) = archive.archive_id().map(str::to_string) else {
        bail!(
            "{} is a v0 archive with no content-addressed identity, so it cannot pin a campaign \
             spec — use --dry-run for offline planning, or re-record it as a v1 archive",
            a.archive.display()
        );
    };
    // The engine re-opens the archive itself (`RunArgs.archive`); this
    // handle was only needed for the identity check and header.
    drop(archive);

    // Resolve the gateway endpoint. The port-forward guards must stay alive
    // until the engine returns: dropping a ProcessGuard kills its kubectl
    // child and severs the tunnel.
    let mut guards: Vec<ProcessGuard> = Vec::new();
    if a.provider.is_some() {
        let ssh_key = a.ssh_key.clone().context(
            "--provider needs --ssh-key <tenant private key> to build the gateway store URL",
        )?;
        let (gateway_port, gateway_guard) = ui::step("port-forward svc/rio-gateway", || {
            shared::port_forward(crate::k8s::NS, "svc/rio-gateway", 0, 22)
        })
        .await?;
        let ((scheduler_port, scheduler_guard), (store_port, store_guard)) =
            ui::step("port-forward scheduler/store gRPC", || {
                shared::tunnel_grpc(0, 0)
            })
            .await?;
        guards.extend([gateway_guard, scheduler_guard, store_guard]);
        a.store = Some(format!(
            "ssh-ng://rio@127.0.0.1:{gateway_port}?compress=true&ssh-key={}",
            ssh_key.display()
        ));
        a.scheduler_addr = format!("127.0.0.1:{scheduler_port}");
        a.store_addr = format!("127.0.0.1:{store_port}");
    }

    // Local spec + state dir. The campaign id is deterministic per archive,
    // so re-running the same archive resumes the same state dir instead of
    // starting over.
    let spec = build_dev_spec(&a, &digest)?;
    let campaign_id = spec
        .campaign_id
        .clone()
        .expect("build_dev_spec always pins a campaign id");
    let state_dir = a.state_dir.join(&campaign_id);
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("create dev state dir {}", state_dir.display()))?;
    let spec_path = state_dir.join("spec.json");
    std::fs::write(&spec_path, serde_json::to_vec_pretty(&spec)?)
        .with_context(|| format!("write {}", spec_path.display()))?;

    println!("{NOT_A_MEASUREMENT}");
    if !ui::is_verbose() {
        println!(
            "(engine progress logs are hidden at the default verbosity — re-run with -v or \
             RUST_LOG=rio_replay=info to stream them)"
        );
    }
    // In-process engine run over the same code path the in-cluster Job
    // executes (`rio-replay run --spec … --archive <local> --no-s3`), so a
    // dev run exercises the real plan/supply/submit/collect/report pipeline.
    rio_replay::run::run(engine_run_args(&a, &state_dir)).await?;
    drop(guards);

    println!(
        "dev run finished: {campaign_id}\n  results: {}\n  summary: {}",
        state_dir.join("results.jsonl").display(),
        state_dir.join("report").join("summary.md").display(),
    );
    println!("{NOT_A_MEASUREMENT}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use rio_replay::run::spec::ScheduleMode;

    use super::*;

    /// Fixture gateway host-key pin. The engine transport mandates host-key
    /// pinning, so every live-run spec needs one; tests use an obviously
    /// fake but well-formed OpenSSH line.
    const HOST_KEY_PIN: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholder rio-gateway";

    /// Every `DevArgs` field at its CLI default (spelled out so the tests
    /// never need to parse argv).
    fn dev_args_for_tests() -> DevArgs {
        DevArgs {
            archive: PathBuf::new(),
            dry_run: false,
            store: None,
            provider: None,
            ssh_key: None,
            ssh_host_key: None,
            mode: Mode::SelfHosted,
            schedule: Schedule::Timeless,
            speedup: 1.0,
            limit: None,
            scheduler_addr: "127.0.0.1:9001".into(),
            store_addr: "127.0.0.1:9002".into(),
            state_dir: PathBuf::from("target/replay-dev"),
        }
    }

    #[test]
    fn dev_dry_run_plans_the_committed_fixture_offline() {
        // The committed v1 fixture lives in the engine crate; resolve it from
        // the runtime CARGO_MANIFEST_DIR so nextest workspace remapping keeps
        // working. No network, no cluster, no AWS — this is the CI-side
        // offline dry-run gate for the operator entry point (the exact-count
        // golden lives in rio-replay/tests/timed_dry_run.rs).
        let fixture = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("../rio-replay/tests/fixtures/archive/v1-basic");
        let mut a = dev_args_for_tests();
        a.archive = fixture;
        a.dry_run = true;
        let plan = dry_run_plan(&a).unwrap();
        assert!(plan.requests >= 1);
        assert_eq!(plan.schedule_len, plan.requests);
        assert!(plan.workload_units >= 1);
        assert_eq!(plan.demoted_impure, 0);
        let rendered = serde_json::to_string_pretty(&plan).unwrap();
        assert!(
            rendered.contains("\"scheduleLen\"") && rendered.contains("\"unresolvedOffline\""),
            "{rendered}"
        );
    }

    #[test]
    fn dev_spec_is_local_only_and_validates() {
        let mut a = dev_args_for_tests();
        a.store = Some("ssh-ng://rio@127.0.0.1:2222?compress=true&ssh-key=/tmp/dev-key".into());
        // The engine transport mandates a pinned gateway host key (the spec
        // is rejected without one), so live dev runs always pass it.
        a.ssh_host_key = Some(HOST_KEY_PIN.into());
        a.schedule = Schedule::Timed;
        a.speedup = 100.0;
        a.limit = Some(5);
        let digest = "ab".repeat(32);
        let spec = build_dev_spec(&a, &digest).unwrap();
        assert!(spec.campaign_id.as_deref().unwrap().starts_with("dev-"));
        assert_eq!(spec.s3.bucket, None);
        assert_eq!(
            spec.cluster.gateway_store_url,
            "ssh-ng://rio@127.0.0.1:2222?compress=true&ssh-key=/tmp/dev-key"
        );
        assert_eq!(spec.cluster.scheduler_addr, "127.0.0.1:9001");
        assert_eq!(spec.cluster.store_addr, "127.0.0.1:9002");
        assert_eq!(spec.cluster.gateway_host_key.as_deref(), Some(HOST_KEY_PIN));
        assert_eq!(spec.archive.digest, digest);
        assert_eq!(spec.archive.s3_prefix, None);
        assert_eq!(spec.scheduling.mode, ScheduleMode::Timed);
        assert_eq!(spec.knobs.speedup, 100.0);
        assert_eq!(spec.filters.limit, Some(5));
        assert_eq!(spec.mode, EngineMode::SelfHosted);
        assert!(!spec.tenants.upstreams_verified);
        spec.validate().unwrap();
    }

    #[test]
    fn dev_run_args_wrap_the_engine_local_path() {
        let state_dir = PathBuf::from("target/replay-dev/dev-abababababab");
        let mut a = dev_args_for_tests();
        a.archive = PathBuf::from("/tmp/some-archive");
        a.limit = Some(3);
        let run = engine_run_args(&a, &state_dir);
        assert_eq!(run.spec, state_dir.join("spec.json"));
        assert_eq!(run.state_dir, state_dir);
        assert_eq!(run.archive.as_deref(), Some(Path::new("/tmp/some-archive")));
        assert_eq!(run.limit, Some(3));
        assert!(run.no_s3);
        assert!(run.allow_unverified_tenants);
        assert_eq!(run.deadline, None);
    }

    #[test]
    fn dev_spec_requires_an_endpoint_and_a_host_key() {
        let digest = "ab".repeat(32);

        // No --store and no --provider resolution: refused naming both
        // flags before anything dials.
        let a = dev_args_for_tests();
        let err = build_dev_spec(&a, &digest).unwrap_err().to_string();
        assert!(
            err.contains("--store") && err.contains("--provider"),
            "{err}"
        );

        // The engine mandates host-key pinning; a live dev run without
        // --ssh-host-key is refused with the flag named (and --dry-run
        // suggested as the no-pin alternative).
        let mut a = dev_args_for_tests();
        a.store = Some("ssh-ng://rio@127.0.0.1:2222?compress=true&ssh-key=/tmp/dev-key".into());
        let err = build_dev_spec(&a, &digest).unwrap_err().to_string();
        assert!(
            err.contains("--ssh-host-key") && err.contains("--dry-run"),
            "{err}"
        );
    }

    #[test]
    fn dev_store_url_gains_the_ssh_key_parameter_when_missing() {
        // A URL that already names a key is used verbatim (--ssh-key, when
        // also given, is for the warm-tenant key directory, not the URL).
        assert_eq!(
            store_url_with_key(
                "ssh-ng://rio@127.0.0.1:2222?ssh-key=/tmp/dev-key",
                Some(Path::new("/tmp/other-key"))
            )
            .unwrap(),
            "ssh-ng://rio@127.0.0.1:2222?ssh-key=/tmp/dev-key"
        );
        // Existing query string: the key is appended with '&'.
        assert_eq!(
            store_url_with_key(
                "ssh-ng://rio@127.0.0.1:2222?compress=true",
                Some(Path::new("/tmp/dev-key"))
            )
            .unwrap(),
            "ssh-ng://rio@127.0.0.1:2222?compress=true&ssh-key=/tmp/dev-key"
        );
        // No query string at all: the key starts one with '?'.
        assert_eq!(
            store_url_with_key(
                "ssh-ng://rio@127.0.0.1:2222",
                Some(Path::new("/tmp/dev-key"))
            )
            .unwrap(),
            "ssh-ng://rio@127.0.0.1:2222?ssh-key=/tmp/dev-key"
        );
        // Neither the URL nor --ssh-key names a key: refused naming both.
        let err = store_url_with_key("ssh-ng://rio@127.0.0.1:2222", None)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--ssh-key") && err.contains("ssh-key="),
            "{err}"
        );
    }

    #[test]
    fn dev_spec_wires_the_warm_key_dir_for_leaf_mode_only() {
        let digest = "ab".repeat(32);
        // Leaf mode + --ssh-key: the key's parent directory becomes
        // cluster.ssh_key_dir so the supply prefetch arm can dial the
        // gateway as the warm tenant (key file `<dir>/<warm tenant>`).
        let mut a = dev_args_for_tests();
        a.store = Some("ssh-ng://rio@127.0.0.1:2222".into());
        a.ssh_host_key = Some(HOST_KEY_PIN.into());
        a.ssh_key = Some(PathBuf::from("/tmp/dev-keys/replay-leaf"));
        a.mode = Mode::Leaf;
        let spec = build_dev_spec(&a, &digest).unwrap();
        assert_eq!(
            spec.cluster.ssh_key_dir.as_deref(),
            Some(Path::new("/tmp/dev-keys"))
        );
        assert_eq!(spec.tenants.build_tenant, "replay-leaf");
        assert_eq!(spec.tenants.warm_tenant, "replay-warm");
        // The URL lacked ssh-key=, so the same --ssh-key was applied to it.
        assert_eq!(
            spec.cluster.gateway_store_url,
            "ssh-ng://rio@127.0.0.1:2222?ssh-key=/tmp/dev-keys/replay-leaf"
        );

        // Self-hosted mode never prefetches: no key directory is wired even
        // when --ssh-key is given.
        let mut a = dev_args_for_tests();
        a.store = Some("ssh-ng://rio@127.0.0.1:2222".into());
        a.ssh_host_key = Some(HOST_KEY_PIN.into());
        a.ssh_key = Some(PathBuf::from("/tmp/dev-keys/replay-selfhosted"));
        let spec = build_dev_spec(&a, &digest).unwrap();
        assert_eq!(spec.cluster.ssh_key_dir, None);
        assert_eq!(spec.tenants.build_tenant, "replay-selfhosted");
    }
}

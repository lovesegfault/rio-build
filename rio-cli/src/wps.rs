//! `rio-cli wps get|describe` — WorkerPoolSet inspection.
//!
//! Unlike every other subcommand in rio-cli, this talks to the
//! Kubernetes apiserver directly (kube-rs `Api<T>`), not to the
//! scheduler's gRPC AdminService. `kubectl get wps` already works
//! (the CRD has printcolumns); this adds the cross-join that
//! kubectl can't do — fetch each child WorkerPool's live replica
//! status alongside the parent WPS spec, so `wps describe` shows
//! the spec→child→replica chain in one place.
//!
//! Separate module (not inline in `main.rs`) — same convention as
//! `cutoffs.rs`: keep `main.rs` deltas to enum variant + match arm
//! + mod decl.

use clap::{Args, Subcommand};
use kube::{Api, Client, ResourceExt};
use serde::Serialize;

use rio_crds::workerpool::{WorkerPool, WorkerPoolStatus};
use rio_crds::workerpoolset::{ClassStatus, SizeClassSpec, WorkerPoolSet};

#[derive(Args, Clone)]
pub struct WpsArgs {
    #[command(subcommand)]
    pub cmd: WpsCmd,
}

#[derive(Subcommand, Clone)]
pub enum WpsCmd {
    /// List WorkerPoolSets in a namespace.
    ///
    /// `kubectl get wps` shows the same thing (the CRD has printer
    /// columns) — this variant exists for completeness and so `--json`
    /// gives a stable machine-readable shape that doesn't change when
    /// the CRD's printcolumns do.
    Get {
        /// Namespace to list. Defaults to "default" — rio-cli has no
        /// kubeconfig-context awareness (it reads KUBECONFIG/in-cluster
        /// but doesn't parse the current-context's namespace). Operators
        /// in non-default namespaces pass `-n <ns>` explicitly.
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
    /// Describe one WorkerPoolSet: spec classes + live child WorkerPool
    /// replica counts + WPS status (effective cutoffs, queue depth).
    ///
    /// The cross-join is the value-add over `kubectl get wps <name>
    /// -o yaml`: this fetches each child `WorkerPool` by derived name
    /// (`{wps}-{class.name}`) and shows its `.status.{replicas,
    /// readyReplicas}` inline — the spec→child→replica chain without
    /// the operator having to know the child-naming convention.
    Describe {
        /// WorkerPoolSet name.
        name: String,
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
}

/// Run the `wps` subcommand. Called from `main.rs` BEFORE the gRPC
/// admin client connect — this subcommand doesn't touch the scheduler,
/// so it must work even when the scheduler is down (e.g., operator
/// diagnosing why no workers are Ready).
pub(crate) async fn run(as_json: bool, args: WpsArgs) -> anyhow::Result<()> {
    // try_default reads in-cluster config (service account token at
    // /var/run/secrets/kubernetes.io/serviceaccount/) or KUBECONFIG
    // for local dev — same precedence kubectl uses. `?` — no kube
    // client = can't inspect CRs; fail loud with the kube-rs error
    // (it says which config source it tried and why it failed).
    let client = Client::try_default()
        .await
        .map_err(|e| anyhow::anyhow!("kube client: {e}"))?;

    match args.cmd {
        WpsCmd::Get { namespace } => get(as_json, client, &namespace).await,
        WpsCmd::Describe { name, namespace } => describe(as_json, client, &namespace, &name).await,
    }
}

// ---------------------------------------------------------------------------
// get — list WorkerPoolSets
// ---------------------------------------------------------------------------

async fn get(as_json: bool, client: Client, ns: &str) -> anyhow::Result<()> {
    let api: Api<WorkerPoolSet> = Api::namespaced(client, ns);
    let list = api
        .list(&Default::default())
        .await
        .map_err(|e| anyhow::anyhow!("list WorkerPoolSets in {ns}: {e}"))?;

    if as_json {
        return crate::json(&GetJson {
            items: list.items.iter().map(GetRowJson::from).collect(),
        });
    }

    // Header always prints — exit criterion says "empty with headers
    // if none", so operators see the table frame even when the
    // namespace is wrong / no WPS applied yet (distinguishes "no
    // output = tool failed" from "no output = nothing to show").
    println!("{:<24} {:<8} CHILDREN", "NAME", "CLASSES");
    for wps in &list.items {
        let name = wps.name_any();
        // Child names derived, not fetched from status — status may
        // not exist yet (fresh WPS, reconciler hasn't populated
        // .status.classes[].child_pool). The naming convention
        // (`{wps}-{class.name}`) is canonical (P0233's reconciler
        // uses it; workerpoolset.rs ClassStatus.child_pool just
        // memoizes the same formula).
        let children: Vec<String> = wps
            .spec
            .classes
            .iter()
            .map(|c| format!("{name}-{}", c.name))
            .collect();
        println!(
            "{:<24} {:<8} {}",
            name,
            wps.spec.classes.len(),
            children.join(",")
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// describe — one WPS + child WorkerPool replica counts + status
// ---------------------------------------------------------------------------

async fn describe(as_json: bool, client: Client, ns: &str, name: &str) -> anyhow::Result<()> {
    let wps_api: Api<WorkerPoolSet> = Api::namespaced(client.clone(), ns);
    let wp_api: Api<WorkerPool> = Api::namespaced(client, ns);

    let wps = wps_api
        .get(name)
        .await
        .map_err(|e| anyhow::anyhow!("get WorkerPoolSet {ns}/{name}: {e}"))?;

    // Per-class child fetch. Sequential (not join_all) — a WPS has
    // ~2-4 classes in practice (small/medium/large + maybe huge);
    // concurrent fetch would save milliseconds at the cost of error
    // interleaving. Each class's child is fetched separately so a
    // missing/not-yet-reconciled child (`get_opt` → Ok(None)) degrades
    // that ONE class's row to "-/-" rather than failing the whole
    // describe.
    let mut rows = Vec::with_capacity(wps.spec.classes.len());
    for class in &wps.spec.classes {
        let child_name = format!("{}-{}", wps.name_any(), class.name);
        // `get_opt` — child may not exist yet (Ok(None) → -/-);
        // 403 → warn to stderr but still render the row (RBAC
        // misconfig is per-verb, the WPS get above already worked);
        // 500/network → bail (apiserver degraded means all remaining
        // rows would be equally misleading).
        //
        // Previously a Result-to-Option coercion here swallowed 403
        // and 500 into the same silent `-/-` as a genuine 404 — an
        // operator diagnosing a misbehaving autoscaler couldn't tell
        // "child not reconciled yet" from "my ServiceAccount can't
        // `get workerpools`."
        let child_status = match wp_api.get_opt(&child_name).await {
            Ok(Some(wp)) => wp.status,
            Ok(None) => {
                // Child not created yet — reconciler lag, or the
                // child create failed (check WPS .status.conditions
                // for the latter). Render as -/-.
                None
            }
            Err(kube::Error::Api(ae)) if ae.code == 403 => {
                // RBAC denied. Warn but don't bail — the operator
                // might still want the spec-side of the describe
                // (class names, cutoffs, derived child names) even
                // if the child status is unavailable. 403 on a
                // per-resource get is common post-deploy when the
                // helm chart's RBAC covers `get workerpoolsets` but
                // forgot `get workerpools`.
                eprintln!(
                    "warning: RBAC denied `get workerpools/{child_name}` ({}). \
                     Child status unavailable — check service account permissions.",
                    ae.message
                );
                None
            }
            Err(e) => {
                // 500, network blip, transport error — surface it.
                // Continuing with `-/-` for this class would be
                // misleading: if the apiserver is degraded for one
                // child it's degraded for the rest, and the operator
                // would see a table of `-/-` that looks like "nothing
                // reconciled yet" when the real signal is "apiserver
                // is down."
                anyhow::bail!("get WorkerPool {ns}/{child_name}: {e}");
            }
        };
        rows.push(ClassRow {
            class,
            child_name,
            child_status,
        });
    }

    if as_json {
        return crate::json(&DescribeJson {
            name: &wps.name_any(),
            namespace: ns,
            classes: rows.iter().map(ClassJson::from).collect(),
            status: wps
                .status
                .as_ref()
                .map(|s| s.classes.iter().map(StatusClassJson::from).collect()),
        });
    }

    println!("Name:      {}", wps.name_any());
    println!("Namespace: {ns}");
    println!("Classes:");
    for row in &rows {
        // `ready/replicas` from the child WorkerPool's `.status`.
        // WorkerPoolStatus fields are non-Option i32 (zero-value is
        // meaningful — 0 replicas is a valid observed state). The
        // `-/-` fallback is only for "child not found at all."
        let replicas = row
            .child_status
            .as_ref()
            .map(|s| format!("{}/{}", s.ready_replicas, s.replicas))
            .unwrap_or_else(|| "-/-".into());
        println!(
            "  {:<12} cutoff={:>6.1}s  replicas={:<8}  pool={}",
            row.class.name, row.class.cutoff_secs, replicas, row.child_name
        );
    }
    // Status block — may be absent (reconciler hasn't populated yet).
    // Distinct from the Classes block above: Classes shows SPEC cutoffs
    // + child REPLICA counts; Status shows EFFECTIVE cutoffs (post-
    // rebalancer EMA) + queue depth. An operator reading both sees
    // "configured=60s, effective=47.2s" → the rebalancer has drifted.
    if let Some(st) = &wps.status {
        println!("Status:");
        for cs in &st.classes {
            println!(
                "  {:<12} effective={:>6.1}s  queued={:<6}  ready={}/{}",
                cs.name, cs.effective_cutoff_secs, cs.queued, cs.ready_replicas, cs.replicas
            );
        }
    } else {
        // Explicit message so operators don't wonder if the tool
        // dropped the status section — it genuinely hasn't been
        // written yet (fresh apply, reconciler not caught up).
        println!("Status: (not yet populated — reconciler pending)");
    }
    Ok(())
}

/// One spec class + its fetched child WorkerPool status. Internal
/// join row — built once per class, consumed by both the human and
/// JSON output paths.
struct ClassRow<'a> {
    class: &'a SizeClassSpec,
    child_name: String,
    child_status: Option<WorkerPoolStatus>,
}

// ---------------------------------------------------------------------------
// JSON projection
//
// Same pattern as cutoffs.rs / main.rs JSON block — thin wrappers
// over the CRD types so `--json | jq` consumers get a stable shape
// that doesn't churn when the CRD schema grows a field.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct GetJson<'a> {
    items: Vec<GetRowJson<'a>>,
}

#[derive(Serialize)]
struct GetRowJson<'a> {
    name: String,
    classes: Vec<&'a str>,
    children: Vec<String>,
}

impl<'a> From<&'a WorkerPoolSet> for GetRowJson<'a> {
    fn from(wps: &'a WorkerPoolSet) -> Self {
        let name = wps.name_any();
        let children = wps
            .spec
            .classes
            .iter()
            .map(|c| format!("{name}-{}", c.name))
            .collect();
        Self {
            name,
            classes: wps.spec.classes.iter().map(|c| c.name.as_str()).collect(),
            children,
        }
    }
}

#[derive(Serialize)]
struct DescribeJson<'a> {
    name: &'a str,
    namespace: &'a str,
    classes: Vec<ClassJson<'a>>,
    /// `None` = WPS has no `.status` yet (reconciler not caught up).
    /// `Some([])` would mean status exists but with zero classes —
    /// distinct from `None` so `jq '.status == null'` works as the
    /// reconciler-liveness check.
    status: Option<Vec<StatusClassJson<'a>>>,
}

#[derive(Serialize)]
struct ClassJson<'a> {
    name: &'a str,
    cutoff_secs: f64,
    child_pool: &'a str,
    /// `ready/replicas` from the child WorkerPool's `.status`, or
    /// `null` if the child doesn't exist yet.
    ready_replicas: Option<i32>,
    replicas: Option<i32>,
}

impl<'a> From<&'a ClassRow<'a>> for ClassJson<'a> {
    fn from(r: &'a ClassRow<'a>) -> Self {
        Self {
            name: &r.class.name,
            cutoff_secs: r.class.cutoff_secs,
            child_pool: &r.child_name,
            ready_replicas: r.child_status.as_ref().map(|s| s.ready_replicas),
            replicas: r.child_status.as_ref().map(|s| s.replicas),
        }
    }
}

#[derive(Serialize)]
struct StatusClassJson<'a> {
    name: &'a str,
    effective_cutoff_secs: f64,
    queued: u64,
    child_pool: &'a str,
    replicas: i32,
    ready_replicas: i32,
}

impl<'a> From<&'a ClassStatus> for StatusClassJson<'a> {
    fn from(s: &'a ClassStatus) -> Self {
        Self {
            name: &s.name,
            effective_cutoff_secs: s.effective_cutoff_secs,
            queued: s.queued,
            child_pool: &s.child_pool,
            replicas: s.replicas,
            ready_replicas: s.ready_replicas,
        }
    }
}

# Plan 0237: rio-cli wps subcommand

phase4c.md:36 — `rio-cli wps get|describe` via kube-rs `Api<WorkerPoolSet>`. Shows spec + child WorkerPool status. This is a kube client, not a gRPC client — it talks directly to the k8s API.

**Per A8:** new file `rio-cli/src/wps.rs`, match arms in `main.rs`. Serial after [P0236](plan-0236-rio-cli-cutoffs.md) (both add variants to the same enum).

## Entry criteria

- [P0232](plan-0232-wps-crd-struct-crdgen.md) merged (`WorkerPoolSet` types exist — `rio-cli` imports them)
- [P0236](plan-0236-rio-cli-cutoffs.md) merged (`main.rs` serialized — `Cutoffs` variant in place)
- [P0216](plan-0216-rio-cli-subcommands.md) merged (`main.rs` serialized — subcommand scaffold)

## Tasks

### T1 — `feat(cli):` wps.rs handler

NEW `rio-cli/src/wps.rs`:

```rust
// Design adjusted 2026-03-18: P0232 T0 extracts rio-crds crate.
// CLI deps on rio-crds (CRD types only), not rio-controller (reconcilers).
use rio_crds::workerpoolset::WorkerPoolSet;
use rio_crds::workerpool::WorkerPool;
use kube::{Api, Client, ResourceExt};
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct WpsArgs {
    #[command(subcommand)]
    pub cmd: WpsCmd,
}

#[derive(Subcommand)]
pub enum WpsCmd {
    /// List all WorkerPoolSets
    Get {
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
    /// Show a WorkerPoolSet's spec + child pool status
    Describe {
        name: String,
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
}

pub async fn run(args: WpsArgs) -> anyhow::Result<()> {
    let client = Client::try_default().await?;  // check: P0216 set this up?

    match args.cmd {
        WpsCmd::Get { namespace } => {
            let api: Api<WorkerPoolSet> = Api::namespaced(client, &namespace);
            let list = api.list(&Default::default()).await?;
            println!("{:<20} {:<8} {:<20}", "NAME", "CLASSES", "CHILDREN");
            for wps in list {
                let children: Vec<_> = wps.spec.classes.iter()
                    .map(|c| format!("{}-{}", wps.name_any(), c.name))
                    .collect();
                println!("{:<20} {:<8} {}",
                         wps.name_any(), wps.spec.classes.len(), children.join(","));
            }
        }
        WpsCmd::Describe { name, namespace } => {
            let wps_api: Api<WorkerPoolSet> = Api::namespaced(client.clone(), &namespace);
            let wp_api: Api<WorkerPool> = Api::namespaced(client, &namespace);
            let wps = wps_api.get(&name).await?;

            println!("Name: {}", wps.name_any());
            println!("Classes:");
            for class in &wps.spec.classes {
                let child_name = format!("{}-{}", wps.name_any(), class.name);
                let child = wp_api.get(&child_name).await.ok();
                let replicas = child.as_ref()
                    .and_then(|c| c.status.as_ref())
                    .map(|s| format!("{}/{}", s.ready_replicas.unwrap_or(0), s.replicas.unwrap_or(0)))
                    .unwrap_or_else(|| "-/-".into());
                println!("  {:<12} cutoff={:.1}s replicas={} pool={}",
                         class.name, class.cutoff_secs, replicas, child_name);
            }
            if let Some(status) = &wps.status {
                println!("Status:");
                for cs in &status.classes {
                    println!("  {:<12} effective={:.1}s queued={}",
                             cs.name, cs.effective_cutoff_secs, cs.queued);
                }
            }
        }
    }
    Ok(())
}
```

**Hidden check:** `grep try_default rio-cli/src/` — if P0216 set up `kube::Client::try_default()`, reuse it. If not, add the setup here (it's ~3 lines).

### T2 — `feat(cli):` main.rs wire

MODIFY [`rio-cli/src/main.rs`](../../rio-cli/src/main.rs):

```rust
// enum variant:
Wps(wps::WpsArgs),

// match arm:
Commands::Wps(args) => wps::run(args).await?,

// mod decl:
mod wps;
```

**Cargo.toml check:** `rio-cli` needs `rio-controller` and `kube` as deps. If `rio-controller` isn't already a dep (it may not be — CLI historically talks gRPC only), add it. Alternatively, re-export the CRD types from `rio-proto` or a shared crate to avoid pulling in all of `rio-controller`.

## Exit criteria

- `/nbr .#ci` green
- `rio-cli wps get` lists WPS resources (or empty with headers if none)
- `rio-cli wps describe <name>` shows classes + child replica counts + status

## Tracey

No markers — CLI is operator tooling.

## Files

```json files
[
  {"path": "rio-cli/src/wps.rs", "action": "NEW", "note": "T1: wps get/describe via kube-rs Api<WorkerPoolSet>"},
  {"path": "rio-cli/src/main.rs", "action": "MODIFY", "note": "T2: Wps enum variant + match arm + mod decl (serial after P0236)"},
  {"path": "rio-cli/Cargo.toml", "action": "MODIFY", "note": "T2: rio-controller + kube deps if not already present"}
]
```

```
rio-cli/
├── src/
│   ├── wps.rs      # T1 (NEW)
│   └── main.rs     # T2: variant + match arm (serial after P0236)
└── Cargo.toml      # T2: deps if needed
```

## Dependencies

```json deps
{"deps": [232, 236, 216], "soft_deps": [], "note": "deps:[P0232(WPS types), P0236(main.rs serial), P0216(scaffold)]. Verify kube::Client setup exists (P0216 may or may not have added it)."}
```

**Depends on:** [P0232](plan-0232-wps-crd-struct-crdgen.md) — `WorkerPoolSet` types. [P0236](plan-0236-rio-cli-cutoffs.md) — `main.rs` serial (both add enum variants). [P0216](plan-0216-rio-cli-subcommands.md) — subcommand scaffold.
**Conflicts with:** `main.rs` — serial after P0236 via dep.

**Hidden check at dispatch:** `grep try_default rio-cli/src/` — add `kube::Client::try_default()` if P0216 didn't.

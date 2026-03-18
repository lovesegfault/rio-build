# Plan 0236: rio-cli cutoffs subcommand

phase4c.md:36 — `rio-cli cutoffs` calls `GetSizeClassStatus` and prints a table of `name | configured | effective | queued | running | samples`. `--json` flag for machine consumption.

**Per A8:** new file `rio-cli/src/cutoffs.rs`, two match arms in `main.rs` (subcommand enum variant + dispatch). Minimizes conflict with [P0216](plan-0216-rio-cli-subcommands.md) which also modifies `main.rs` in 4b.

## Entry criteria

- [P0231](plan-0231-get-sizeclass-status-rpc-hub.md) merged (`GetSizeClassStatus` RPC + generated proto types)
- [P0216](plan-0216-rio-cli-subcommands.md) merged (`main.rs` serialized — subcommand enum + gRPC client setup in place)

## Tasks

### T1 — `feat(cli):` cutoffs.rs handler

NEW `rio-cli/src/cutoffs.rs`:

```rust
use rio_proto::admin::{AdminServiceClient, GetSizeClassStatusRequest};
use clap::Args;

#[derive(Args)]
pub struct CutoffsArgs {
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: CutoffsArgs, admin: &mut AdminServiceClient<Channel>) -> anyhow::Result<()> {
    let resp = admin.get_size_class_status(GetSizeClassStatusRequest {}).await?.into_inner();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&resp.classes)?);
        return Ok(());
    }

    // Table output
    println!("{:<12} {:>10} {:>10} {:>8} {:>8} {:>8}",
             "CLASS", "CONFIGURED", "EFFECTIVE", "QUEUED", "RUNNING", "SAMPLES");
    for c in &resp.classes {
        println!("{:<12} {:>10.1} {:>10.1} {:>8} {:>8} {:>8}",
                 c.name, c.configured_cutoff_secs, c.effective_cutoff_secs,
                 c.queued, c.running, c.sample_count);
    }
    Ok(())
}
```

### T2 — `feat(cli):` main.rs wire

MODIFY [`rio-cli/src/main.rs`](../../rio-cli/src/main.rs) — at `:96` (or wherever P0216 left the subcommand enum):

```rust
// enum variant:
Cutoffs(cutoffs::CutoffsArgs),

// match arm:
Commands::Cutoffs(args) => cutoffs::run(args, &mut admin_client).await?,

// mod decl at top:
mod cutoffs;
```

## Exit criteria

- `/nbr .#ci` green
- `rio-cli cutoffs --help` shows the subcommand
- `rio-cli cutoffs --json` against a running scheduler outputs valid JSON (`| jq .` parses)

## Tracey

No markers — CLI is operator tooling, not normative spec.

## Files

```json files
[
  {"path": "rio-cli/src/cutoffs.rs", "action": "NEW", "note": "T1: GetSizeClassStatus call + table/JSON output"},
  {"path": "rio-cli/src/main.rs", "action": "MODIFY", "note": "T2: Cutoffs enum variant + match arm + mod decl near :96"}
]
```

```
rio-cli/src/
├── cutoffs.rs    # T1 (NEW)
└── main.rs       # T2: variant + match arm
```

## Dependencies

```json deps
{"deps": [231, 216], "soft_deps": [], "note": "deps:[P0231(proto), P0216(main.rs serial)]. P0237 deps this (same main.rs, both add variants)."}
```

**Depends on:** [P0231](plan-0231-get-sizeclass-status-rpc-hub.md) — `GetSizeClassStatus` RPC + generated types. [P0216](plan-0216-rio-cli-subcommands.md) — `main.rs` serialization (subcommand scaffold + admin gRPC client setup).
**Conflicts with:** `main.rs` count=2 — serialized after P0216. [P0237](plan-0237-rio-cli-wps.md) also touches main.rs, deps on this plan.

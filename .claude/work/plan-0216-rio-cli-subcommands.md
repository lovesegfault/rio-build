# Plan 0216: rio-cli — remaining subcommands + `--json` + cli.nix extension

Wave 2. Completes the rio-cli scaffolding from its current 257L (CreateTenant/ListTenants/Status only at [`rio-cli/src/main.rs:97-120`](../../rio-cli/src/main.rs)) to cover all admin RPCs. Adds `--json` global flag for machine-readable output.

**All RPCs already exist.** The `rpc()` helper at `main.rs:43-45` handles unary RPCs; `GetBuildLogs` and `TriggerGC` are **STREAMING** — they need a stream-drain loop, NOT the helper (comment at `:43-45` already warns about this).

rio-cli is already packaged in the scheduler image at [`nix/docker.nix:177-181`](../../nix/docker.nix) — `kubectl exec` path works today. Standalone `dockerImages.rio-cli` is **not in scope** (A6).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged (no new markers — CLI is tooling under existing `r[sched.admin.*]` markers)

## Tasks

### T1 — `feat(cli):` `--json` global flag

On `CliArgs` near `scheduler_addr`:

```rust
#[arg(long, global = true)]
json: bool,
```

Each existing handler: `if cli.json { println!("{}", serde_json::to_string_pretty(&resp)?) } else { /* existing table */ }`.

Verify prost-generated types have `serde::Serialize` — if not, either add thin wrapper structs per subcommand OR enable the `prost-wkt-types` serde feature. Prefer thin wrappers (less blast radius).

### T2 — `feat(cli):` new `Cmd` variants at `:97-120`

```rust
/// Detailed worker list (keeps Status as summary view)
Workers,

/// List builds, optionally filtered
Builds {
    #[arg(long)] status: Option<String>,
    #[arg(long, default_value = "50")] limit: u32,
},

/// Stream build logs (GetBuildLogs is STREAMING — drain loop, NOT rpc() helper)
Logs { build_id: String },

/// Trigger GC (TriggerGC is STREAMING — drain GcProgress to stdout)
Gc {
    #[arg(long)] dry_run: bool,
},

/// Clear poison state for a derivation hash
PoisonClear { drv_hash: String },
```

**Handlers:**

- `Workers`: `ListWorkers` RPC → detailed view per worker (vs `Status` summary).
- `Builds`: `ListBuilds` RPC, filtered by status if provided.
- `Logs`: `GetBuildLogs` — `while let Some(chunk) = stream.message().await? { print!("{}", chunk.text); }` — drain the stream, print each chunk to stdout directly.
- `Gc`: `TriggerGC` — `while let Some(progress) = stream.message().await? { println!("{:?}", progress); }` — drain `GcProgress`.
- `PoisonClear`: `ClearPoison` RPC (unary — can use the `rpc()` helper).

### T3 — `test(cli):` smoke test via ephemeral scheduler

Smoke test via `rio-test-support` ephemeral scheduler (per `phase4b.md:37`): each subcommand connects and returns non-error against an empty scheduler.

### T4 — `test(vm):` extend cli.nix

At [`nix/tests/scenarios/cli.nix`](../../nix/tests/scenarios/cli.nix) (TAIL append after `:140`):

```python
# Workers JSON is parseable and non-empty
machine.succeed("rio-cli workers --json | jq -e '.workers | length >= 1'")

# Builds returns at least the one we just submitted
machine.succeed("rio-cli builds | grep -q ...")

# GC dry-run exits 0 (streams GcProgress but doesn't actually delete)
machine.succeed("rio-cli gc --dry-run")

# PoisonClear on nonexistent hash returns NotFound (error-path)
machine.fail("rio-cli poison-clear 0000000000000000000000000000000000000000000000000000000000000000")
```

## Exit criteria

- `/nbr .#ci` green
- All 5 new subcommands covered in `cli.nix`
- `rio-cli workers --json | jq -e .` parses (valid JSON)

## Tracey

No new markers — CLI is tooling. Extends verify coverage for existing `r[sched.admin.list-workers]` and `r[sched.admin.clear-poison]` markers (add `// r[verify ...]` annotations to the cli.nix header if those markers need more verify coverage per `tracey query untested`).

## Files

```json files
[
  {"path": "rio-cli/src/main.rs", "action": "MODIFY", "note": "T1: --json global flag; T2: Workers/Builds/Logs/Gc/PoisonClear variants + handlers (streaming for Logs/Gc)"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T4: 5 new subcommand assertions (TAIL append after :140)"}
]
```

```
rio-cli/src/main.rs                # T1+T2: --json + 5 new subcommands
nix/tests/scenarios/cli.nix        # T4: VM assertions
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "Fully disjoint — only plan in 4b touching rio-cli/src/main.rs and cli.nix."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md) — for the `phase4b.md` corrections marking rio-cli packaging `[x]`.

**Conflicts with:** **none.** Only plan in 4b touching `rio-cli/src/main.rs` and `cli.nix`. Fully parallel with everything.

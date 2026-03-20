# Plan 0076: Figment layered config (TOML + RIO_ env + CLI)

## Design

Before this plan, each binary's config was pure clap: `--flag=value` or `env=` attribute, defaults baked into `#[derive(Parser)]`. No TOML file support, no layering. `configuration.md:3-5` specifies precedence `CLI > env > TOML > defaults` — this plan implements it.

**`rio-common::config::load`** (`b9dd69d`): figment-based provider chain.
```
Serialized::defaults(Config::default())
  .merge(Toml::file("/etc/rio/{component}.toml"))
  .merge(Toml::file("./{component}.toml"))
  .merge(Env::prefixed("RIO_").split("__"))
  .merge(Serialized::defaults(cli))
```
Env uses `__` for nesting: `RIO_STORE__S3_BUCKET` → `store.s3_bucket`.

**Two-struct contract** (critical — documented in module header):
- `Config`: concrete `T` fields, `#[serde(default)]`, `Default` impl gives the same values the old clap `default_value=` did.
- `CliArgs`: `Option<T>` fields, NO `env=` (figment's Env provider replaces it), NO `default_value`, every field has `#[serde(skip_serializing_if = "Option::is_none")]`.

That `skip_serializing_if` is **load-bearing**: without it, an unset CLI flag serializes as `null` and overwrites the env/TOML value. `cli_none_does_not_override_lower_layers` guards this specifically.

`figment::Jail` (dev-dep feature `test`) manipulates process-global env/cwd under an internal mutex and restores on drop. Safe under `cargo test` single-process concurrency because all env-touching tests go through Jail; nextest per-test-process sidesteps it entirely.

**Migration** (`fa38e1d`): all 4 `main.rs` converted. `main()` becomes: `CliArgs::parse()` → `config::load(component, cli)` → `ensure!(required non-empty)` → rest unchanged. Per-binary regression test `config_defaults_match_phase2a()` asserts `Config::default()` exactly matches old hardcoded defaults. (2ab2d22e/P0097 later renamed test → `config_defaults_are_stable`, fixed 3 dangerous defaults. fuse_passthrough guard remains.) Key guard: worker `fuse_passthrough` defaults to `true`, not the serde bool default `false` — a silent drift to `false` adds a userspace copy per FUSE read (~2× per-build latency) and only shows as a `vm-phase2a` timing regression, not a hard failure.

Worker bool-CLI subtlety: clap's bare `bool` is a flag (presence=true, absence=false), which would make `--fuse-passthrough` always set from CLI, defeating layering. `Option<bool>` + explicit `value_parser` makes clap accept `--fuse-passthrough=true|false` and leaves it `None` when absent. `cli_fuse_passthrough_explicit_bool` guards this.

Required fields (`scheduler_addr`, `store_addr`, `database_url`) can't use serde's "missing field" error because `#[serde(default)]` gives `String::new()` = silent success. Post-merge `ensure!()` checks with a human-readable message.

NixOS modules (`nix/modules/*.nix`) updated to write `/etc/rio/{component}.toml` instead of passing env vars directly. 12 tests: each precedence layer overrides the one below, `__` nesting, CLI-none-doesn't-override, required-field ensure.

## Files

```json files
[
  {"path": "rio-common/src/config.rs", "action": "NEW", "note": "figment provider chain; two-struct contract documented in header"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "mod config"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "figment, serde, clap features"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "Config/CliArgs split; config::load; ensure!; config_defaults_match_phase2a"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "Config/CliArgs split; config::load; database_url ensure!"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "Config/CliArgs split; config::load"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "Config/CliArgs split; fuse_passthrough Option<bool>+value_parser; cli_fuse_passthrough_explicit_bool"},
  {"path": "nix/modules/scheduler.nix", "action": "MODIFY", "note": "write /etc/rio/scheduler.toml"},
  {"path": "nix/modules/store.nix", "action": "MODIFY", "note": "write /etc/rio/store.toml"},
  {"path": "nix/modules/worker.nix", "action": "MODIFY", "note": "write /etc/rio/worker.toml"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive: `r[cfg.layer.precedence]`, `r[cfg.layer.env-split]`.

## Entry

- Depends on **P0074** (unwrap sweep): all 4 `main.rs` were last touched by the Result sweep; this rebases on that.

## Exit

Merged as `b9dd69d..fa38e1d` (2 commits). `.#ci` green at merge; `vm-phase2a` still passes with NixOS modules writing TOML instead of env vars.

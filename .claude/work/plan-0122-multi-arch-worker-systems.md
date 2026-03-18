# Plan 0122: Multi-arch worker — HeartbeatRequest systems repeated + features wiring

## Design

Single-commit plan that wired multi-architecture worker support end-to-end across 4 crates. Before: `HeartbeatRequest.system` was a singular `string`; a worker declared exactly one platform. `requiredSystemFeatures` in derivations was silently dropped — the worker's `supported_features` was hardcoded `Vec::new()` despite the CRD having a `features` field.

`HeartbeatRequest.system` → `systems` (repeated). `WorkerInfo` follows suit. Same field number (no deployments exist, wire-compat not needed). `rio-common` grew a `comma_vec` deserialize helper that accepts **either** a comma-separated string (env layer, `RIO_SYSTEMS=x86_64-linux,aarch64-linux`) **or** a TOML array (config file layer). Figment's providers give different types; this visitor bridges both without external crates.

`rio-worker`: `Config.system` → `systems Vec<String>` + `features`. `build_heartbeat_request` takes slice refs, populates both proto fields. `rio-scheduler`: `WorkerState.system Option` → `systems Vec`. `can_build()` does **any-match** on systems (multi-arch worker builds derivations for any declared system), **all-match** on features (unchanged). `is_registered()` checks `!systems.is_empty()` — empty-systems worker is misconfigured, not a wildcard. `rio-controller`: `env("RIO_SYSTEMS", join(","))` + `RIO_FEATURES` in `build_container`. The scheduler's `can_build()` already checked `supported_features.contains()`; it just never received non-empty features until now.

Net effect: `requiredSystemFeatures` derivations dispatch correctly. Multi-arch worker pools work without one pool per arch — a cluster with mixed x86/arm nodes can share a single WorkerPool.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "HeartbeatRequest.system → systems repeated; WorkerInfo follows"},
  {"path": "rio-common/src/config.rs", "action": "MODIFY", "note": "comma_vec deserialize: string OR TOML array; figment layer bridge"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "build_heartbeat_request takes &[String] systems + features"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "Config.systems Vec + features from figment"},
  {"path": "rio-scheduler/src/state/worker.rs", "action": "MODIFY", "note": "systems Vec; can_build any-match systems, all-match features; is_registered checks !empty"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "handle_heartbeat stores Vec"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "dispatch eligibility uses new can_build"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "ClusterStatus reports systems Vec"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "proto translation systems plural"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "RIO_SYSTEMS + RIO_FEATURES env vars in container spec"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[sched.worker.systems-any-match]`, `r[sched.worker.features-all-match]`. P0127's docs audit fixed `proto.md` systems plural (`6d3b24a`) and `scheduler.md` systems plural (`ada0f76`).

## Entry

- Depends on P0112: `RIO_SYSTEMS`/`RIO_FEATURES` env vars go in the controller-generated STS.
- Depends on P0106: `ClusterStatus` reports systems; this plan changed the field shape.

## Exit

Merged as `77961c7` (1 commit). `.#ci` green at merge. 8 test sites migrated from singular `system` to `systems`. `can_build` tests: any-match systems (x86 drv dispatches to x86+arm worker), all-match features, empty-systems rejected.

# Plan 0218: store config — plumb `nar_buffer_budget_bytes`

Wave 3 filler. Config plumbing — 1 struct field + 1 startup call + 1 roundtrip test. The builder `StoreServer::with_nar_budget()` already exists (per [`grpc/mod.rs:141`](../../rio-store/src/grpc/mod.rs) comment); `DEFAULT_NAR_BUDGET` at `:153` is a hardcode. This plan threads the config value through.

Closes `TODO(phase4b)` at [`rio-store/src/grpc/mod.rs:142`](../../rio-store/src/grpc/mod.rs).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync-markers.md) merged

## Tasks

### T1 — `feat(store):` config field + builder call

At [`rio-store/src/main.rs:50`](../../rio-store/src/main.rs) `Config` struct (uses `rio_common::config::load`):

```rust
/// NAR reassembly buffer budget in bytes. None → DEFAULT_NAR_BUDGET.
#[serde(default)]
pub nar_buffer_budget_bytes: Option<u64>,
```

At startup, where `StoreServer` is constructed:

```rust
let server = if let Some(budget) = cfg.nar_buffer_budget_bytes {
    StoreServer::with_nar_budget(budget)  // builder at grpc/mod.rs:141
} else {
    StoreServer::default()  // DEFAULT_NAR_BUDGET at :153
};
```

Delete `TODO(phase4b)` at `grpc/mod.rs:142`.

### T2 — `test(store):` config roundtrip

Write `nar_buffer_budget_bytes = 12345` to a temp `store.toml`, load via `rio_common::config::load`, assert value reaches `StoreServer` (expose via a getter or assert on the constructed server's internal field).

## Exit criteria

- `/nbr .#ci` green
- `grep 'TODO(phase4b)' rio-store/src/grpc/mod.rs` returns empty

## Tracey

None — config plumbing.

## Files

```json files
[
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T1: Config.nar_buffer_budget_bytes at :50; call with_nar_budget at startup"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T1: delete TODO(phase4b) at :142"}
]
```

```
rio-store/src/
├── main.rs                        # T1: config field + builder call
└── grpc/mod.rs                    # T1: close TODO
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "grpc/mod.rs:142 distant from P0213's :84 — parallel OK."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync-markers.md).

**Conflicts with:** [`grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) — P0213 touches `:84` (ResourceExhausted status mapping), this touches `:142` (TODO delete). Distant, different functions — file-disjoint in practice, parallel OK.

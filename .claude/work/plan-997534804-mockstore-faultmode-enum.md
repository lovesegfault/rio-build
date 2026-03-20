# Plan 997534804: MockStore fault-injection — FaultMode enum + method-keyed map

consol-mc235 feature. [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) `MockStore` has **8 AtomicBool flags** + 1 AtomicU32 at [`:53-82`](../../rio-test-support/src/grpc.rs), each following the same 3-site pattern:

| Flag | Field | `Default::default()` in `new()` | Handler check |
|---|---|---|---|
| `fail_next_puts` | [`:53`](../../rio-test-support/src/grpc.rs) | [`:97`](../../rio-test-support/src/grpc.rs) | [`:154`, `:250`](../../rio-test-support/src/grpc.rs) |
| `fail_find_missing` | [`:56`](../../rio-test-support/src/grpc.rs) | [`:98`](../../rio-test-support/src/grpc.rs) | [`:425`](../../rio-test-support/src/grpc.rs) |
| `fail_query_path_info` | [`:59`](../../rio-test-support/src/grpc.rs) | [`:99`](../../rio-test-support/src/grpc.rs) | [`:405`](../../rio-test-support/src/grpc.rs) |
| `fail_get_path` | [`:61`](../../rio-test-support/src/grpc.rs) | [`:100`](../../rio-test-support/src/grpc.rs) | [`:337`](../../rio-test-support/src/grpc.rs) |
| `get_path_garbage` | [`:64`](../../rio-test-support/src/grpc.rs) | — | handler-side |
| `get_path_gate_armed` | [`:72`](../../rio-test-support/src/grpc.rs) | — | handler-side |
| `content_lookup_hang` | [`:78`](../../rio-test-support/src/grpc.rs) | [`:104`](../../rio-test-support/src/grpc.rs) | [`:443`](../../rio-test-support/src/grpc.rs) |
| `fail_content_lookup` | [`:82`](../../rio-test-support/src/grpc.rs) | [`:105`](../../rio-test-support/src/grpc.rs) | [`:449`](../../rio-test-support/src/grpc.rs) |

[P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) added the last 2 (`content_lookup_hang`, `fail_content_lookup`) this window. The growth pattern is **3N**: each (RPC method × {fail,hang,garbage} mode) adds a struct field + Default init + handler-entry guard. Future candidates: `query_path_info_hang`, `put_path_hang` (for timeout tests). A 9th flag hits the proposed threshold.

**Refactor:** `enum FaultMode { Fail, Hang, Garbage }` + `DashMap<&'static str, FaultMode>` keyed by method name. One `self.check_fault("content_lookup")?` call at each handler entry replaces per-flag `if self.flag.load(SeqCst)`. Next fault = 1-line `.insert()` in the test, not a 3-site add.

Absorbs ~50L boilerplate. The `get_path_gate_armed` (holds-then-succeeds, distinct from fail/hang) stays as a separate flag — it's a stateful gate, not a simple fault mode.

**CAUTION:** [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) is **collision count=23** (top-20 hot file). Many in-flight plans touch it — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T19, [P997534803](plan-997534803-ca-compare-test-fixture-extract.md), and any future `spawn_mock_*` helper additions. Sequence AFTER the current impl wave drains.

## Tasks

### T1 — `refactor(test-support):` FaultMode enum + faults DashMap

MODIFY [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs). Add before `MockStore` struct at ~`:50`:

```rust
/// Fault injection mode for a MockStore method. Arm via
/// `store.faults.insert("content_lookup", FaultMode::Hang)`.
///
/// Replaces the per-method AtomicBool flag pattern (8 flags pre-P997534804,
/// each a 3-site add: struct field + Default::default + handler guard). The
/// enum+DashMap makes the NEXT fault a 1-line .insert() in the test, not a
/// 3-site add in rio-test-support (P997534804 consolidation).
#[derive(Clone, Copy, Debug)]
pub enum FaultMode {
    /// Return `Err(Status::unavailable("MockStore fault-injected"))`.
    Fail,
    /// `pending::<()>().await` — never returns. Tests use an outer
    /// `tokio::time::timeout` to observe the hang.
    Hang,
    /// Return syntactically-valid but semantically-garbage response
    /// (e.g., GetPath returns random bytes instead of a NAR).
    Garbage,
}

/// Method-name constants for `faults.insert()`. Using consts instead of
/// bare strings catches typos at compile time via unused-import warning.
pub mod fault_key {
    pub const PUT_PATH: &str = "put_path";
    pub const FIND_MISSING: &str = "find_missing";
    pub const QUERY_PATH_INFO: &str = "query_path_info";
    pub const GET_PATH: &str = "get_path";
    pub const CONTENT_LOOKUP: &str = "content_lookup";
}
```

Replace the 6 simple AtomicBool fields (`fail_find_missing`, `fail_query_path_info`, `fail_get_path`, `get_path_garbage`, `content_lookup_hang`, `fail_content_lookup`) in `MockStore` with:

```rust
/// Method → fault mode. Insert to arm, remove to disarm. Checked at
/// each handler entry via `check_fault`.
pub faults: Arc<dashmap::DashMap<&'static str, FaultMode>>,
```

Keep `fail_next_puts: Arc<AtomicU32>` (counted decrement, not a simple bool — different shape). Keep `get_path_gate_armed: Arc<AtomicBool>` (holds-then-succeeds stateful gate — distinct semantics from Fail/Hang/Garbage). Keep `watch_fail_count: Arc<AtomicU32>` at `:612` (counted).

### T2 — `refactor(test-support):` check_fault helper + handler-entry calls

MODIFY [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs). Add method on `MockStore`:

```rust
impl MockStore {
    /// Fault-injection gate. Call at handler entry. Returns `Err`
    /// for `Fail`, never returns for `Hang`, returns `Ok(true)` for
    /// `Garbage` (handler should synthesize garbage response), returns
    /// `Ok(false)` when no fault armed.
    async fn check_fault(&self, method: &str) -> Result<bool, tonic::Status> {
        match self.faults.get(method).map(|e| *e.value()) {
            Some(FaultMode::Fail) => Err(tonic::Status::unavailable(
                format!("MockStore fault-injected: {method}"),
            )),
            Some(FaultMode::Hang) => {
                std::future::pending::<()>().await;
                unreachable!()
            }
            Some(FaultMode::Garbage) => Ok(true),
            None => Ok(false),
        }
    }
}
```

Replace the 8 handler-entry checks (at `:154`, `:250`, `:337`, `:405`, `:425`, `:443`, `:449`, and `get_path_garbage` site) with:

```rust
// At handler entry, e.g. content_lookup:
if self.check_fault(fault_key::CONTENT_LOOKUP).await? {
    // Garbage branch — synthesize garbage response here
    return Ok(Response::new(ContentLookupResponse { ... garbage ... }));
}
// ... normal handler body
```

For methods that currently have separate fail vs hang flags (`content_lookup` has both `:443` and `:449`), the single `check_fault` call handles both modes via the enum.

### T3 — `refactor(scheduler):` migrate test callers to faults.insert()

MODIFY test files that arm the old flags. Grep `fail_content_lookup\|content_lookup_hang\|fail_query_path_info\|fail_get_path\|get_path_garbage\|fail_find_missing` across `rio-scheduler/src/actor/tests/`, `rio-worker/src/`, `rio-gateway/tests/`. Each:

```rust
// OLD
store.content_lookup_hang.store(true, SeqCst);
// NEW
store.faults.insert(fault_key::CONTENT_LOOKUP, FaultMode::Hang);
```

Disarming:
```rust
// OLD
store.fail_query_path_info.store(false, SeqCst);
// NEW
store.faults.remove(fault_key::QUERY_PATH_INFO);
```

Expected migration sites: the 8 `r[verify sched.ca.*]` tests in completion.rs (P0311-T54/T61/T67), the `r[verify worker.upload.*]` tests in upload.rs (P0311-T19), and any older fault-injection tests from the P0208/P0251 era. Count at dispatch.

### T4 — `test(test-support):` fault mode self-test

ADD a `#[cfg(test)]` test in [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) proving each FaultMode behaves as spec'd:

```rust
#[tokio::test]
async fn faultmode_fail_returns_unavailable() {
    let store = MockStore::new();
    store.faults.insert(fault_key::CONTENT_LOOKUP, FaultMode::Fail);
    let result = store.check_fault(fault_key::CONTENT_LOOKUP).await;
    assert!(matches!(result, Err(s) if s.code() == tonic::Code::Unavailable));
}

#[tokio::test]
async fn faultmode_hang_never_returns() {
    let store = MockStore::new();
    store.faults.insert(fault_key::CONTENT_LOOKUP, FaultMode::Hang);
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        store.check_fault(fault_key::CONTENT_LOOKUP),
    ).await;
    assert!(result.is_err(), "Hang mode must never return");
}

#[tokio::test]
async fn faultmode_none_returns_false() {
    let store = MockStore::new();
    assert!(matches!(
        store.check_fault(fault_key::CONTENT_LOOKUP).await,
        Ok(false)
    ));
}
```

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'enum FaultMode\|pub faults:\|check_fault' rio-test-support/src/grpc.rs` → ≥3 hits (T1+T2)
- `grep 'fail_find_missing\|fail_query_path_info\|fail_get_path\|get_path_garbage\|content_lookup_hang\|fail_content_lookup' rio-test-support/src/grpc.rs` → 0 hits as struct fields (T1: all 6 simple bools removed; may appear in migration-guide comment)
- `grep 'fail_next_puts\|get_path_gate_armed\|watch_fail_count' rio-test-support/src/grpc.rs` → ≥3 hits (T1: these three STAY — counted + stateful, different shape)
- `cargo nextest run -p rio-test-support faultmode` → 3 passed (T4)
- `cargo nextest run -p rio-scheduler 'actor::tests'` → all pass, same count as before (T3 migration preserves behavior)
- `cargo nextest run -p rio-worker` → all pass (T3)
- `wc -l rio-test-support/src/grpc.rs` → net -30..-50L (T1+T2: 6 fields × 3 sites removed, enum + check_fault + 5 consts added)
- `grep 'dashmap' rio-test-support/Cargo.toml` → ≥1 hit (T1: new dep; check workspace Cargo.toml — dashmap may already be there via rio-store/rio-scheduler)

## Tracey

No markers. Test-support refactor; not spec-behavior. The migrated tests retain their `r[verify sched.ca.*]` / `r[verify worker.upload.*]` annotations.

## Files

```json files
[
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "T1: +FaultMode enum + fault_key consts + faults: DashMap field, -6 AtomicBool fields (:53-105 struct + Default). T2: +check_fault() method, replace 8 handler-entry guards (:154,:250,:337,:405,:425,:443,:449). T4: +3 FaultMode self-tests"},
  {"path": "rio-test-support/Cargo.toml", "action": "MODIFY", "note": "T1: +dashmap dep (check workspace Cargo.toml first — may already be there)"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T3: migrate flag.store(true) → faults.insert() (P0311-T54/T61/T67 CA-compare tests + pre-existing)"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T3: migrate fault flag usage (P0311-T19 fallthrough test + older)"}
]
```

```
rio-test-support/src/
└── grpc.rs          # T1+T2+T4: FaultMode enum + DashMap + check_fault + tests
rio-test-support/Cargo.toml  # T1: +dashmap (if absent)
rio-scheduler/src/actor/tests/
└── completion.rs    # T3: migrate .store(true) → .insert()
rio-worker/src/
└── upload.rs        # T3: migrate fault flag usage
```

## Dependencies

```json deps
{"deps": [311], "soft_deps": [997534803, 419, 251], "note": "HARD-dep P0311 (DONE — content_lookup_hang + fail_content_lookup fields at grpc.rs:78,:82 exist; discovered_from=consolidator). Soft-dep P997534803 (CA-compare fixture extraction — T3 there migrates completion.rs CA tests to setup_ca_fixture; T3 HERE migrates the same tests' fault-arming. If P997534803 lands first, T3 here migrates the post-fixture-extraction shape: `f.store.faults.insert(...)` instead of `store.content_lookup_hang.store(...)`. Sequence P997534803 FIRST so both migrations compose in one pass over completion.rs). Soft-dep P0419 (grpc_timeout plumbing — ca_cutoff_compare_slow_store arms content_lookup_hang; P0419 shrinks the timeout, this plan changes the arming call. Non-overlapping edits). Soft-dep P0251 (DONE — the CA-compare hook that content_lookup_hang tests). COLLISION CAUTION: rio-test-support/src/grpc.rs count=23 (top-20 hot). T1+T2 is a STRUCT-LEVEL rewrite (replace 6 fields, add enum+map+method). SEQUENCE AFTER impl wave drains — check at dispatch for in-flight grpc.rs touchers. P0311-T19 adds fail_batch_precondition (one more flag this plan would absorb if it lands first). P997534803 adds no grpc.rs changes but depends on the MockStoreHandle shape. THRESHOLD NOTE: consolidator said 'worth it if 9th flag appears'. fail_batch_precondition (P0311-T19) would be the 9th; query_path_info_hang would be the 10th. Dispatch this plan if either is in-flight."}
```

**Depends on:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) — `content_lookup_hang` + `fail_content_lookup` fields (the 7th + 8th flags that tipped the threshold).

**Conflicts with:** [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) count=23 HOT — struct-level rewrite. Sequence AFTER current wave. [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T19 adds `fail_batch_precondition` field — if unlanded, absorb into T1's FaultMode migration. [P997534803](plan-997534803-ca-compare-test-fixture-extract.md) migrates completion.rs CA tests — T3 here touches the same tests' fault-arming lines.

# Plan 0221: rio-bench crate + Hydra comparison doc

phase4c.md:54-55 — benchmarking infrastructure for SubmitBuild latency and dispatch throughput. The milestone target (p99<50ms @ N=100) is a **target, not a gate** — the deliverable here is the bench infrastructure itself, not hitting the number. Criterion gives stable measurements; `nix run .#bench` wires it into the dev loop; the Hydra comparison doc gives operators a baseline.

This plan touches `Cargo.toml` (workspace member) and `flake.nix` (app alias), both of which are tail-append operations. No 4b plan adds a crate (verified by grep of `.claude/work/plan-02{04..19}-*.md` for `Cargo.toml`), so no serialization needed beyond "don't dispatch concurrent with another crate-add."

## Tasks

### T1 — `feat(bench):` rio-bench crate skeleton

NEW crate `rio-bench/`:

```toml
# rio-bench/Cargo.toml
[package]
name = "rio-bench"
version = "0.1.0"
edition = "2024"
publish = false

[dependencies]
rio-proto = { path = "../rio-proto" }
rio-scheduler = { path = "../rio-scheduler" }
rio-test-support = { path = "../rio-test-support" }
tokio = { workspace = true, features = ["full"] }
tonic = { workspace = true }
rand = "0.8"

[dev-dependencies]
criterion = { version = "0.5", features = ["async_tokio", "html_reports"] }

[[bench]]
name = "submit_build"
harness = false   # REQUIRED — criterion provides its own main()

[[bench]]
name = "dispatch"
harness = false
```

`rio-bench/src/lib.rs` — DAG generators as pure functions returning `Vec<DerivationNode>`:

```rust
/// Linear chain: node[i] depends on node[i-1]. Stresses critical-path
/// dispatch latency (no parallelism possible).
pub fn linear(n: usize) -> Vec<DerivationNode> { ... }

/// Complete binary tree of given depth. 2^depth-1 nodes. Stresses
/// fan-out dispatch (all leaves ready at once).
pub fn binary_tree(depth: usize) -> Vec<DerivationNode> { ... }

/// N nodes where `frac` share a common dependency (diamond root).
/// Stresses de-duplication in the scheduler's ready-set scan.
pub fn shared_diamond(n: usize, frac: f64) -> Vec<DerivationNode> { ... }
```

Each generator produces valid-shaped `DerivationNode`s with synthetic `drv_path` (e.g., `/nix/store/{random-hash}-bench-{i}.drv`) and synthetic `output_paths`. They do NOT need to correspond to real derivations — the scheduler's SubmitBuild path validates DAG shape, not store validity.

### T2 — `feat(bench):` SubmitBuild latency benchmark

`rio-bench/benches/submit_build.rs`:

```rust
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use rio_bench::{linear, binary_tree, shared_diamond};

fn bench_submit_build(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    // Ephemeral PG via rio-test-support (same pattern as integration tests)
    let (scheduler_addr, _pg) = rt.block_on(rio_test_support::spawn_scheduler_with_pg());

    let mut group = c.benchmark_group("submit_build_latency");
    for n in [1, 10, 100, 1000] {
        let dag = linear(n);
        group.bench_with_input(BenchmarkId::new("linear", n), &dag, |b, dag| {
            b.to_async(&rt).iter(|| async {
                let mut client = connect(scheduler_addr).await;
                client.submit_build(dag.clone()).await.unwrap();
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_submit_build);
criterion_main!(benches);
```

Measure: wall-clock from gRPC call start to `SubmitBuildResponse` received. The scheduler's BFS + DB insert dominates at high N — p99<50ms@N=100 is the target.

### T3 — `feat(bench):` dispatch throughput benchmark

`rio-bench/benches/dispatch.rs` — measure how fast the scheduler can drain a ready queue when workers are infinitely fast (mocked worker channel that instantly acks). This isolates the scheduler's ready-scan + DB update from actual build execution.

Mock worker: `tokio::sync::mpsc` channel that receives `AssignDerivation` and immediately sends back `BuildComplete`. Seed with `binary_tree(10)` = 1023 nodes, all leaves ready; measure time-to-drain.

### T4 — `feat(bench):` nix run .#bench alias

MODIFY `flake.nix` — add to `apps.${system}`:

```nix
bench = {
  type = "app";
  program = "${pkgs.writeShellScript "rio-bench" ''
    cd ${self}
    exec ${craneLib.cargoBin}/bin/cargo bench -p rio-bench "$@"
  ''}";
};
```

Tail-append near existing `apps` (grep for `apps = {` or `apps.${system}`).

MODIFY root `Cargo.toml` workspace members: add `"rio-bench"` to the `members` array (tail of the list).

### T5 — `docs:` Hydra comparison procedure

MODIFY [`docs/src/capacity-planning.md`](../../docs/src/capacity-planning.md) — add a `## Benchmarking against Hydra` section documenting:

1. **Setup:** identical hardware (or same EC2 instance type), same PostgreSQL config, same build workload (nixpkgs subset).
2. **Metrics to compare:** SubmitBuild latency (rio) vs. `hydra-queue-runner` enqueue latency; time-to-first-dispatch; queue-drain throughput.
3. **Expected differences:** rio-build does DAG reconstruction gateway-side (not scheduler-side); Hydra does drv parsing on the queue-runner. Apples-to-apples requires measuring from "client sends drv" to "worker receives assignment."
4. **Caveat:** Hydra's queue runner is single-threaded; rio-scheduler is async but single-actor. Throughput comparison is meaningful; latency comparison needs care around what's being measured.

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c cargo bench -p rio-bench -- --test` smoke-passes (criterion's `--test` mode runs 1 iteration without timing — verifies benches compile and don't panic)
- `nix run .#bench` launches criterion
- `docs/src/capacity-planning.md` has the Hydra section

## Tracey

No markers — benchmarking infrastructure is not normative spec behavior.

## Files

```json files
[
  {"path": "rio-bench/Cargo.toml", "action": "NEW", "note": "T1: crate manifest with criterion dev-dep, harness=false benches"},
  {"path": "rio-bench/src/lib.rs", "action": "NEW", "note": "T1: DAG generators linear/binary_tree/shared_diamond"},
  {"path": "rio-bench/benches/submit_build.rs", "action": "NEW", "note": "T2: SubmitBuild latency N={1,10,100,1000}"},
  {"path": "rio-bench/benches/dispatch.rs", "action": "NEW", "note": "T3: dispatch throughput with mocked infinitely-fast worker"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "T4: add rio-bench to workspace members (tail)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T4: apps.bench alias (tail-append to apps attrset)"},
  {"path": "docs/src/capacity-planning.md", "action": "MODIFY", "note": "T5: Hydra comparison procedure section"}
]
```

```
rio-bench/                    # NEW crate
├── Cargo.toml
├── src/lib.rs
└── benches/
    ├── submit_build.rs
    └── dispatch.rs
Cargo.toml                    # T4: workspace member
flake.nix                     # T4: apps.bench
docs/src/capacity-planning.md # T5
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "No intra-4c deps. Cargo.toml + flake.nix tail-append — verify no concurrent crate-add before dispatch."}
```

**Depends on:** none. DAG generators use existing `rio-proto::DerivationNode`; ephemeral PG via existing `rio-test-support`.
**Conflicts with:** `Cargo.toml` and `flake.nix` are tail-append; `flake.nix` collision count = 23 but no other 4c plan touches it. Safe for Wave-1 parallel dispatch.

**Hidden check at dispatch:** verify `criterion` 0.5 is edition-2024 compatible (`cargo add criterion --dev -p rio-bench` will tell you fast). `harness = false` is REQUIRED — criterion provides its own `main()`; forgetting this gives a cryptic "duplicate lang item" error.

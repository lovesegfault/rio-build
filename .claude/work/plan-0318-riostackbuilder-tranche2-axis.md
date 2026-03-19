# Plan 0318: RioStackBuilder — third axis for tranche-2 functional tests

[P0305](plan-0305-functional-tests-tranche1.md) shipped `RioStack` with a 2×2 named-constructor matrix at [`functional/mod.rs:66-113`](../../rio-gateway/tests/functional/mod.rs): `new`/`new_chunked` × bare/`_ready`. The P0305 reviewer flagged that tranche-2 (survey at [`plan-0305:477`](plan-0305-functional-tests-tranche1.md)) needs a real `SchedulerActor` (leader-election stub, worker-registration mock) — a **third axis**. 2³ = 8 named constructors is the point where the matrix stops scaling and a builder starts paying off.

Additionally: `Option<Arc<MemoryChunkBackend>>` at [`mod.rs:60`](../../rio-gateway/tests/functional/mod.rs) is a pub field that every chunked test `.expect("new_chunked provides it")`-s. Fine at 6 tests, noise at ~28 (tranche-2's estimate). The builder can return the backend from `with_chunked()` directly — no optional-on-the-struct.

**Not a P0305 defect.** Four constructors for four cases was the right call for tranche-1 scope. This plan is forward-prep for tranche-2's ~22 scenarios so they don't land on a constructor explosion.

## Entry criteria

- [P0305](plan-0305-functional-tests-tranche1.md) merged (`RioStack` exists at `functional/mod.rs:66`) — DONE at [`196fdc5b`](https://github.com/search?q=196fdc5b&type=commits)

## Tasks

### T1 — `refactor(gateway):` RioStackBuilder struct with chain methods

MODIFY [`rio-gateway/tests/functional/mod.rs`](../../rio-gateway/tests/functional/mod.rs).

Replace the 4-constructor matrix at `:72-113` with a builder. The builder owns the axis state; `.build()` spawns; `.ready()` does handshake + `wopSetOptions`. The chunk backend is returned from `with_chunked()` — no `Option` field on the final struct.

```rust
/// Builder for [`RioStack`]. Three orthogonal axes:
/// - **chunked:** inline-only (default) vs FastCDC + MemoryChunkBackend
/// - **scheduler:** mock (default) vs real SchedulerActor (tranche-2)
/// - **ready:** bare stream vs handshake + wopSetOptions done
///
/// The chunk backend is returned from `.with_chunked()`, not stored on
/// `RioStack` — tests that don't chunk don't carry an `Option::None`.
///
/// Tranche-1 (6 tests, 2 axes) used 4 named constructors. Tranche-2
/// (~22 tests, 3 axes) would need 8. The builder collapses to one
/// entry point + chain.
#[derive(Default)]
pub struct RioStackBuilder {
    chunked: bool,
    real_scheduler: bool,
    // Captured backend — returned to caller from with_chunked(),
    // not stored on RioStack.
    chunk_backend: Option<Arc<MemoryChunkBackend>>,
}

impl RioStackBuilder {
    pub fn new() -> Self { Self::default() }

    /// Use FastCDC chunking (NARs ≥ INLINE_THRESHOLD go through
    /// chunk+reassembly). Returns the backend so tests can assert
    /// chunk counts — proving NARs round-tripped through chunk→PG
    /// manifest→reassembly, not the inline-blob shortcut.
    ///
    /// Chainable: `let (builder, backend) = RioStackBuilder::new().with_chunked();`
    pub fn with_chunked(mut self) -> (Self, Arc<MemoryChunkBackend>) {
        let backend = Arc::new(MemoryChunkBackend::new());
        self.chunked = true;
        self.chunk_backend = Some(Arc::clone(&backend));
        (self, backend)
    }

    /// Use a real `SchedulerActor` instead of `MockScheduler`. Needs a
    /// leader-election stub (always-leader) and worker-registration mock.
    /// Tranche-2's CA/cutoff scenarios and `wopBuildDerivation` need
    /// real scheduler state machine (job queue, assignment, completion).
    ///
    /// Stub shape: spawn `SchedulerActor` with an in-memory `LeaseLock`
    /// that immediately grants leadership + a mock worker channel that
    /// auto-accepts assignments. Details at tranche-2 plan time — this
    /// builder method is the interface seam.
    pub fn with_real_scheduler(mut self) -> Self {
        self.real_scheduler = true;
        self
    }

    /// Spawn the stack. Stream is BARE — caller handshakes.
    pub async fn build(self) -> anyhow::Result<RioStack> {
        let db = TestDb::new(&MIGRATOR).await;
        let service = if self.chunked {
            let cache = Arc::new(ChunkCache::new(
                Arc::clone(self.chunk_backend.as_ref().unwrap())
                    as Arc<dyn ChunkBackend>
            ));
            StoreServiceImpl::with_chunk_cache(db.pool.clone(), cache)
        } else {
            StoreServiceImpl::new(db.pool.clone())
        };
        // Scheduler branch (mock vs real) — tranche-2 fills real_scheduler arm:
        if self.real_scheduler {
            // TODO(tranche-2-plan): spawn real SchedulerActor with
            // always-leader stub + mock-worker channel. Until then:
            unimplemented!("real SchedulerActor stub lands with tranche-2 plan");
        }
        RioStack::build_inner(db, service).await
    }

    /// Spawn + handshake + wopSetOptions. Ready for opcodes.
    pub async fn ready(self) -> anyhow::Result<RioStack> {
        let mut stack = self.build().await?;
        do_handshake(&mut stack.stream).await?;
        send_set_options(&mut stack.stream).await?;
        Ok(stack)
    }
}
```

**Rename `RioStack::build` → `build_inner`** (private, at `:116-165`) — `build` is now the builder's terminal method. Drop the `chunk_backend: Option<Arc<MemoryChunkBackend>>` param from `build_inner` signature (the builder handles it).

**Drop `pub chunk_backend: Option<_>` field at `:60`.** Tests that need it now get it from `with_chunked()`.

### T2 — `refactor(gateway):` migrate tranche-1 callsites to builder

MODIFY [`rio-gateway/tests/functional/nar_roundtrip.rs`](../../rio-gateway/tests/functional/nar_roundtrip.rs), [`references.rs`](../../rio-gateway/tests/functional/references.rs), [`store_roundtrip.rs`](../../rio-gateway/tests/functional/store_roundtrip.rs).

| Old | New |
|---|---|
| `RioStack::new().await?` | `RioStackBuilder::new().build().await?` |
| `RioStack::new_ready().await?` | `RioStackBuilder::new().ready().await?` |
| `RioStack::new_chunked().await?` | `let (b, backend) = RioStackBuilder::new().with_chunked(); b.build().await?` |
| `RioStack::new_chunked_ready().await?` | `let (b, backend) = RioStackBuilder::new().with_chunked(); b.ready().await?` |
| `stack.chunk_backend.as_ref().expect(...)` | `backend` (local, returned from `with_chunked()`) |

Grep for callsites: `grep -rn 'RioStack::new\|chunk_backend' rio-gateway/tests/functional/`.

**Delete the 4 named constructors at `:72-113`** after migration. The builder is the only entry.

### T3 — `docs:` update tranche-2 future-ref in P0305 survey

MODIFY [`.claude/work/plan-0305-functional-tests-tranche1.md`](plan-0305-functional-tests-tranche1.md) at `:363` and `:477` — the survey table's tranche-2 row says "`RioStack` + real `SchedulerActor` (leader election stub, worker registration mock)". Append: "(builder seam landed [P0318](plan-0318-riostackbuilder-tranche2-axis.md) — tranche-2 plan uses `.with_real_scheduler()` and fills the stub)."

The tranche-2 plan author reads this, finds the seam already cut, writes only the stub body.

## Exit criteria

- `/nbr .#ci` green — tranche-1 tests pass under the builder
- `grep -c 'RioStack::new\b' rio-gateway/tests/functional/` → 0 (old constructors gone)
- `grep -c 'RioStackBuilder' rio-gateway/tests/functional/mod.rs` → ≥1 (builder defined)
- `grep 'chunk_backend: Option' rio-gateway/tests/functional/mod.rs` → 0 (field dropped)
- `grep '\.expect.*new_chunked provides' rio-gateway/tests/functional/` → 0 (expect()s gone — backend is a local)
- `grep 'with_real_scheduler' rio-gateway/tests/functional/mod.rs` → ≥1 (tranche-2 seam exists, even if `unimplemented!`)
- `grep 'P0318\|RioStackBuilder' .claude/work/plan-0305-*.md` → ≥1 (T3: tranche-2 row points at the seam)

## Tracey

No new markers. Test-infrastructure refactor — no spec behavior. `r[gw.opcode.query-path-info]` and `r[store.nar.reassembly]` are what the migrated tests verify; their `r[verify ...]` annotations move with the callsites but don't change.

## Files

```json files
[
  {"path": "rio-gateway/tests/functional/mod.rs", "action": "MODIFY", "note": "T1: RioStackBuilder struct + chain methods; drop 4 constructors :72-113; drop chunk_backend field :60; rename build→build_inner :116"},
  {"path": "rio-gateway/tests/functional/nar_roundtrip.rs", "action": "MODIFY", "note": "T2: migrate RioStack::new_* → RioStackBuilder chain"},
  {"path": "rio-gateway/tests/functional/references.rs", "action": "MODIFY", "note": "T2: migrate RioStack::new_* → RioStackBuilder chain"},
  {"path": "rio-gateway/tests/functional/store_roundtrip.rs", "action": "MODIFY", "note": "T2: migrate RioStack::new_* → RioStackBuilder chain"},
  {"path": ".claude/work/plan-0305-functional-tests-tranche1.md", "action": "MODIFY", "note": "T3: tranche-2 survey row points at builder seam"}
]
```

```
rio-gateway/tests/functional/
├── mod.rs                # T1: +RioStackBuilder, -4 constructors, -Option field
├── nar_roundtrip.rs      # T2: callsite migration
├── references.rs         # T2: callsite migration
└── store_roundtrip.rs    # T2: callsite migration
```

## Dependencies

```json deps
{"deps": [305], "soft_deps": [], "note": "Hard dep on P0305 (DONE 196fdc5b) — RioStack exists. discovered_from=305 (reviewer flagged during P0305 review). NOT blocking tranche-2's plan doc (that plan can be written in parallel — it depends on this plan's MERGE, not its existence). Prio 45: infrastructure-for-future, not urgent. When tranche-2's plan is written, it will dep on this plan and fill the with_real_scheduler() stub."}
```

**Depends on:** [P0305](plan-0305-functional-tests-tranche1.md) — DONE at [`196fdc5b`](https://github.com/search?q=196fdc5b&type=commits). `RioStack` and the 4 constructors exist.

**Conflicts with:** [`functional/mod.rs`](../../rio-gateway/tests/functional/mod.rs) has no other UNIMPL-plan touches (collisions check: not in top-50). [`references.rs`](../../rio-gateway/tests/functional/references.rs) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T18 adds `read_path_info` helper calls at `:53-60`, `:149-156`; this T2 touches the `RioStack::new` call at the top of each `#[test]` fn. Different sections — trivial merge. If P0304 T18 lands first (likely — P0304 is higher-priority), rebase picks up the helper calls cleanly.

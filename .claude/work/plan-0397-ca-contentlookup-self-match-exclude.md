# Plan 397: CA ContentLookup self-match exclusion (URGENT — blocks P0252 cascade)

**Bughunt-mc196 CRITICAL finding.** The CA cutoff-compare hook at [`completion.rs:289-337`](../../rio-scheduler/src/actor/completion.rs) has a time-of-insert vs time-of-compare ordering bug that makes the **first-ever build** of any CA derivation report `ca_output_unchanged = true`, triggering [P0252](plan-0252-ca-cutoff-propagate-skipped.md)'s cascade to skip ALL downstream builds WITHOUT building them.

**The sequence:**
1. Worker builds CA drv `D` (no prior `content_index` row for this `nar_hash`)
2. Worker uploads output → `PutPath` at [`put_path.rs:590`](../../rio-store/src/grpc/put_path.rs) inserts into `content_index(nar_hash, store_path_hash)`
3. Worker sends `BuildComplete` → scheduler `handle_success_completion` fires
4. [`completion.rs:305-315`](../../rio-scheduler/src/actor/completion.rs): `ContentLookup(output_hash)` queries `content_index` → finds the row **from step 2** → `matched = true`
5. [`completion.rs:335`](../../rio-scheduler/src/actor/completion.rs): `all_matched = true` → `ca_output_unchanged = true`
6. P0252 cascade: downstream `Queued → Skipped` — **NEVER BUILT**

The comparison asks "have we seen this content before?" — but "before" includes **this build's own upload**. The PutPath runs on the worker; the ContentLookup runs on the scheduler; the worker's PutPath always completes before BuildComplete fires (worker code sequencing). So the self-match is deterministic, not a race.

**Why this wasn't caught by P0251's tests:** [P0251-T4](plan-0251-ca-cutoff-compare.md) `ca_completion_with_matching_hash_sets_unchanged` mocks `content_lookup → found=true`. It doesn't exercise the full PutPath→ContentLookup→completion sequence. The VM demo [P0254-T2](plan-0254-ca-metrics-vm-demo.md) would catch it (build-1 asserts `saves ≥ 2`, but on first build saves SHOULD be 0; the assert at `:54` checks `after - before ≥ 2` which would trivially pass because EVERY first-build falsely matches).

**Fix: self-exclusion.** Add the just-built `store_path` to the lookup so the query excludes it. `BuiltOutput.output_path` at [`build_types.proto:221`](../../rio-proto/proto/build_types.proto) is already populated with the realized store path; wire it through.

## Entry criteria

- [P0251](plan-0251-ca-cutoff-compare.md) merged (CA-compare hook at [`completion.rs:289-337`](../../rio-scheduler/src/actor/completion.rs) exists)

## Tasks

### T1 — `fix(proto):` ContentLookupRequest — add exclude_store_path field

MODIFY [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto) at `:181-187`. Add field 3 (NOT reusing reserved 2, which was `output_name`):

```proto
message ContentLookupRequest {
  bytes content_hash = 1;
  reserved 2;
  reserved "output_name";
  // Store path to EXCLUDE from the lookup. The CA cutoff-compare
  // hook sets this to the just-built output's path so the lookup
  // answers "have we seen this content in a DIFFERENT path before?"
  // (not "have we seen it at all?" — the just-uploaded path always
  // matches itself). Empty = no exclusion (legacy callers unchanged).
  string exclude_store_path = 3;
}
```

### T2 — `fix(store):` content_index::lookup — exclude_store_path parameter + query WHERE

MODIFY [`rio-store/src/content_index.rs`](../../rio-store/src/content_index.rs) at `:78-96`. Add optional exclusion:

```rust
// r[impl store.content.self-exclude]
pub async fn lookup(
    pool: &PgPool,
    content_hash: &[u8],
    exclude_store_path: Option<&str>,  // ← NEW
) -> crate::metadata::Result<Option<ValidatedPathInfo>> {
    let row: Option<crate::metadata::NarinfoRow> = sqlx::query_as(concat!(
        "SELECT ",
        crate::narinfo_cols!(),
        " FROM content_index ci \
         INNER JOIN narinfo n ON ci.store_path_hash = n.store_path_hash \
         INNER JOIN manifests m ON n.store_path_hash = m.store_path_hash \
         WHERE ci.content_hash = $1 \
           AND m.status = 'complete' \
           AND ($2::text IS NULL OR n.store_path != $2) \
         LIMIT 1"
    ))
    .bind(content_hash)
    .bind(exclude_store_path)
    .fetch_optional(pool)
    .await?;
    crate::metadata::validate_row(row)
}
```

The `$2::text IS NULL OR n.store_path != $2` is the standard nullable-filter idiom — when `exclude_store_path` is `None`, PG treats the predicate as true (no filter).

### T3 — `fix(store):` gRPC handler — pass exclude_store_path through

MODIFY [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) at `:577-605`:

```rust
async fn content_lookup(
    &self,
    request: Request<ContentLookupRequest>,
) -> Result<Response<ContentLookupResponse>, Status> {
    let req = request.into_inner();
    // ... existing 32-byte guard ...
    let exclude = if req.exclude_store_path.is_empty() {
        None
    } else {
        Some(req.exclude_store_path.as_str())
    };
    match crate::content_index::lookup(&self.pool, &req.content_hash, exclude).await {
        // ... existing match arms ...
    }
}
```

### T4 — `fix(scheduler):` completion.rs CA-compare — set exclude_store_path

MODIFY [`rio-scheduler/src/actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs) at `:305-307`:

```rust
let mut req = tonic::Request::new(rio_proto::types::ContentLookupRequest {
    content_hash: output.output_hash.clone(),
    exclude_store_path: output.output_path.clone(),  // ← self-exclusion
});
```

That's the entire scheduler-side change — `output.output_path` is already available from `BuiltOutput` at [`build_types.proto:221`](../../rio-proto/proto/build_types.proto).

### T5 — `test(store):` lookup excludes self, still finds sibling

MODIFY [`rio-store/src/content_index.rs`](../../rio-store/src/content_index.rs) `#[cfg(test)] mod tests` after `:204`:

```rust
// r[verify store.content.self-exclude]
#[tokio::test]
async fn lookup_exclude_own_path_returns_none() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    // Seed ONE narinfo+manifest+content_index row for path P with hash H.
    let sp = /* ... */;
    let nar_hash = [0xaau8; 32];
    // (same setup as test_insert_and_lookup_roundtrip)
    insert(&db.pool, &nar_hash, &sp_hash).await?;

    // Lookup WITHOUT exclusion → finds it.
    let found = lookup(&db.pool, &nar_hash, None).await?;
    assert!(found.is_some(), "no-exclusion lookup should find P");

    // Lookup EXCLUDING P → None. This is the self-match fix:
    // "have we seen H in a DIFFERENT path?" → no, P is the only one.
    let found = lookup(&db.pool, &nar_hash, Some(sp.as_str())).await?;
    assert!(found.is_none(), "excluding the only match → None");
    Ok(())
}

#[tokio::test]
async fn lookup_exclude_one_finds_sibling() -> anyhow::Result<()> {
    let db = TestDb::new(&crate::MIGRATOR).await;
    // Seed TWO paths P1, P2 both with the SAME nar_hash (same content,
    // different input-addressed names — the PK is (content_hash,
    // store_path_hash) so both rows coexist).
    // Lookup excluding P1 → MUST find P2. This proves the exclusion
    // is per-path, not "any match → None".
    // (setup: two complete narinfo+manifest pairs, both content_index
    //  rows with the same nar_hash.)
    // ...
    let found = lookup(&db.pool, &nar_hash, Some(p1.as_str())).await?;
    assert_eq!(found.unwrap().store_path.as_str(), p2.as_str());
    Ok(())
}
```

### T6 — `test(scheduler):` completion CA-compare — first-build doesn't self-match

MODIFY [`rio-scheduler/src/actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs). Extend the mock-store fixture so `content_lookup` respects `exclude_store_path`:

```rust
// r[verify sched.ca.cutoff-compare]
#[tokio::test]
async fn ca_compare_first_build_excludes_own_upload() {
    // MockStore seeded with ONE content_index row: (hash_H, path_P).
    // (Simulates: worker uploaded P via PutPath before BuildComplete.)
    // CA completion for drv D with built_outputs = [(name=out,
    //   output_path=P, output_hash=H)].
    //
    // BEFORE this fix: ContentLookup(H) → finds P → matched=true →
    //   ca_output_unchanged=true → P0252 would skip downstream. WRONG.
    //
    // AFTER this fix: ContentLookup(H, exclude=P) → None → matched=false
    //   → ca_output_unchanged=false. CORRECT: this is the FIRST build.
    //
    // Assert: state.ca_output_unchanged == false.
    // Assert: counter{outcome="miss"} == 1 (not "match").
}

#[tokio::test]
async fn ca_compare_second_build_matches_prior() {
    // MockStore seeded with TWO content_index rows: (H, P_prior) from
    // a previous build, AND (H, P_new) from THIS build's upload.
    // CA completion with output_path=P_new, output_hash=H.
    //
    // ContentLookup(H, exclude=P_new) → finds P_prior → matched=true.
    // This is the INTENDED positive case: a prior build with the same
    // content exists → downstream CAN be skipped.
    //
    // Assert: state.ca_output_unchanged == true.
}
```

These two tests together prove the exclusion is "hide self, find sibling" — not "always miss" (over-correction) or "always match" (the bug).

## Exit criteria

- `/nbr .#ci` green
- `cargo nextest run -p rio-store lookup_exclude_own_path_returns_none lookup_exclude_one_finds_sibling` → 2 passed
- `cargo nextest run -p rio-scheduler ca_compare_first_build_excludes_own_upload ca_compare_second_build_matches_prior` → 2 passed
- `grep 'exclude_store_path' rio-proto/proto/types.proto rio-store/src/content_index.rs rio-store/src/grpc/mod.rs rio-scheduler/src/actor/completion.rs` → ≥4 hits (field wired end-to-end)
- **Mutation:** remove the `exclude_store_path: output.output_path.clone()` line at T4's site → `ca_compare_first_build_excludes_own_upload` FAILS with `ca_output_unchanged == true` (the bug reproduced)
- **Blocker for [P0252](plan-0252-ca-cutoff-propagate-skipped.md):** if P0252 is in-flight or merged, steer coordinator — the cascade at [`completion.rs:T3`](plan-0252-ca-cutoff-propagate-skipped.md) acting on a false `ca_output_unchanged = true` skips EVERY first-time CA downstream. This plan must merge before (or alongside) P0252 goes live.

## Tracey

References existing markers:
- `r[sched.ca.cutoff-compare]` — T4 refines the existing implementation at [`completion.rs:267`](../../rio-scheduler/src/actor/completion.rs); T6 verifies the self-exclusion edge
- `r[store.content.self-exclude]` — **NEW marker** (see Spec additions below); T2 implements, T5 verifies

## Spec additions

Add to [`docs/src/components/store.md`](../../docs/src/components/store.md) after `r[store.hash.domain-sep]` (after `:31`):

```
r[store.content.self-exclude]

The `ContentLookup` RPC MUST accept an optional `exclude_store_path` filter. When set, the lookup returns a match ONLY if the content exists under a DIFFERENT store path. This prevents a just-uploaded output from matching its own `content_index` row during the CA cutoff-compare (which would falsely report "seen before" on the first-ever build).
```

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T1: :181-187 ContentLookupRequest +exclude_store_path field 3"},
  {"path": "rio-store/src/content_index.rs", "action": "MODIFY", "note": "T2: lookup() +exclude_store_path param + WHERE clause; T5: +2 tests after :204"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T3: :577-605 content_lookup handler — pass exclude through"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "T4: :305-307 ContentLookupRequest — set exclude_store_path = output.output_path"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T6: +2 tests (first-build-miss, second-build-match)"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "T6: MockStore content_lookup — respect exclude_store_path (fixture update)"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "Spec-addition: +r[store.content.self-exclude] marker after :31"}
]
```

```
rio-proto/proto/
└── types.proto                   # T1: +exclude_store_path field
rio-store/src/
├── content_index.rs              # T2: lookup() param + WHERE; T5: tests
└── grpc/mod.rs                   # T3: handler pass-through
rio-scheduler/src/actor/
├── completion.rs                 # T4: set exclude_store_path
└── tests/completion.rs           # T6: first-build/second-build tests
rio-test-support/src/grpc.rs      # T6: MockStore fixture
docs/src/components/store.md      # Spec addition
```

## Dependencies

```json deps
{"deps": [251], "soft_deps": [252, 393, 254], "note": "discovered_from=bughunt-mc196 (CRITICAL). Hard-dep P0251 (DONE — the CA-compare hook this plan fixes). Soft-dep P0252 (UNIMPL/in-flight — THE CONSUMER: P0252's find_cutoff_eligible reads ca_output_unchanged; if P0252 merges before this fix, every first-time CA build cascades Skipped WRONGLY. Coordinator MUST sequence this plan BEFORE or ALONGSIDE P0252 going live. p252 worktree exists — steer.). Soft-dep P0393 (CA ContentLookup serial-timeout — same completion.rs hook at :289-337; P0393-T3's optional ContentLookupBatch proto also needs exclude_store_path semantics (repeated field or per-entry exclude); coordinate at T3 dispatch. T1+T4 here are additive to P0393's breaker/short-circuit — non-overlapping). Soft-dep P0254 (VM demo — its T2 testScript at :38-56 would falsely pass 'saves >= 2' on first-build without this fix; the regression this plan catches IS P0254's intended demo scenario). types.proto class-3 weak (field-add to existing msg, no new RPC) — nextest post-rebase sufficient. completion.rs count=26 HOT — T4 is 1-line field-set inside existing Request::new, low conflict. content_index.rs count=3, grpc/mod.rs count=16 — both additive."}
```

**Depends on:** [P0251](plan-0251-ca-cutoff-compare.md) — CA-compare hook at [`completion.rs:289-337`](../../rio-scheduler/src/actor/completion.rs) exists.

**Conflicts with:** [`completion.rs`](../../rio-scheduler/src/actor/completion.rs) count=26 — [P0393](plan-0393-ca-contentlookup-serial-timeout.md) rewrites the SAME loop body at `:289-337` (breaker-gate + short-circuit + 2s timeout). **T4 here is a 1-line field-add inside `Request::new(..)`; P0393's changes are outside that block.** Sequence either order; if P0393's T3 (ContentLookupBatch) lands, the batch variant ALSO needs `exclude_store_path` semantics (document in P0393 conflict note). [`types.proto`](../../rio-proto/proto/types.proto) class-3-weak (field-only additive). [P0304-T148](plan-0304-trivial-batch-p0222-harness.md)/T157 add counter labels in the same match body — additive, non-overlapping.

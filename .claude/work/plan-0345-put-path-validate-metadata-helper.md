# Plan 0345: Extract validate_put_metadata + apply_trailer — PutPath/PutPathBatch dedup

Consolidator finding (mc=84). Six-step metadata validation and four-step trailer-apply are duplicated verbatim between [`put_path.rs:282-331`](../../rio-store/src/grpc/put_path.rs) and [`put_path_batch.rs:125-155/:219-233`](../../rio-store/src/grpc/put_path_batch.rs). The code **admits it**: [`put_path_batch.rs:219`](../../rio-store/src/grpc/put_path_batch.rs) comment says "same shape as put_path.rs step 5 prelude."

~60 lines of duplication. Net save is smaller (~40L after the helper signatures + call sites), but the **real win** is that a future `PutPathBatch v2` (chunked blobs, multipart resume) or a third entry point won't copy the validation dance a third time. The `path_not_in_claims` metric gap in [P0344](plan-0344-put-path-batch-content-index-parity.md) T3 is an example of what drifts: `put_path.rs` emits the counter, `put_path_batch.rs` doesn't — both have the check, one forgot the side-effect.

**Scope is EXTRACTION ONLY.** No behavior change. Tests should pass unchanged. The `hmac_rejected` counter gap is [P0344](plan-0344-put-path-batch-content-index-parity.md)'s concern; this plan just makes the *future* gap impossible by having one copy of the check.

## Entry criteria

- [P0267](plan-0267-atomic-multi-output-tx.md) merged — **DONE** (both `put_path.rs` and `put_path_batch.rs` exist with the duplication)

## Tasks

### T1 — `refactor(store):` extract validate_put_metadata helper

NEW fn in [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) (near `maybe_sign` at [`:273`](../../rio-store/src/grpc/mod.rs)). The six steps, from both [`put_path.rs:282-331`](../../rio-store/src/grpc/put_path.rs) and [`put_path_batch.rs:125-155`](../../rio-store/src/grpc/put_path_batch.rs):

```rust
/// Validate a raw PathInfo message for PutPath/PutPathBatch.
/// Shared validation: (1) nar_hash-empty enforcement (trailer-only mode),
/// (2) references bound, (3) signatures bound, (4) placeholder hash fill,
/// (5) ValidatedPathInfo::try_from, (6) HMAC path-in-claims check.
///
/// `ctx_label` goes into error messages for client-side disambiguation
/// ("PutPath" vs "output N").
///
/// Returns the validated info; on HMAC path-not-in-claims failure,
/// increments the `hmac_rejected_total{reason=path_not_in_claims}`
/// counter before erroring.
// r[impl sec.boundary.grpc-hmac]
pub(super) fn validate_put_metadata(
    mut raw_info: rio_proto::types::PathInfo,
    hmac_claims: Option<&AssignmentClaims>,
    ctx_label: &str,
) -> Result<ValidatedPathInfo, Status> {
    // Step 1: trailer-only enforcement.
    if !raw_info.nar_hash.is_empty() {
        return Err(Status::invalid_argument(format!(
            "{ctx_label}: metadata.nar_hash must be empty (trailer-only mode)"
        )));
    }
    // Step 4: placeholder fill (before TryFrom, which validates 32-byte).
    raw_info.nar_hash = vec![0u8; 32];

    // Steps 2-3: bound repeated fields BEFORE per-element validation.
    rio_common::grpc::check_bound(
        "references", raw_info.references.len(), rio_common::limits::MAX_REFERENCES,
    )?;
    rio_common::grpc::check_bound(
        "signatures", raw_info.signatures.len(), rio_common::limits::MAX_SIGNATURES,
    )?;

    // Step 5: centralized validation.
    let info = ValidatedPathInfo::try_from(raw_info)
        .map_err(|e| Status::invalid_argument(format!("{ctx_label}: {e}")))?;

    // Step 6: HMAC path-in-claims.
    if let Some(claims) = hmac_claims {
        let path_str = info.store_path.as_str();
        if !claims.expected_outputs.iter().any(|o| o == path_str) {
            warn!(
                store_path = %path_str,
                worker_id = %claims.worker_id,
                drv_hash = %claims.drv_hash,
                "{ctx_label}: path not in assignment's expected_outputs",
            );
            metrics::counter!(
                "rio_store_hmac_rejected_total",
                "reason" => "path_not_in_claims"
            ).increment(1);
            return Err(Status::permission_denied(format!(
                "{ctx_label}: path not authorized by assignment token"
            )));
        }
    }

    Ok(info)
}
```

**Call sites:**
- [`put_path.rs:282-331`](../../rio-store/src/grpc/put_path.rs) → `let mut info = validate_put_metadata(raw_info, hmac_claims.as_ref(), "PutPath")?;`
- [`put_path_batch.rs:125-155`](../../rio-store/src/grpc/put_path_batch.rs) → `let info = validate_put_metadata(raw_info, hmac_claims.as_ref(), &format!("output {idx}"))?;` — **but see bail!-vs-? note below.**

**bail!-vs-? in put_path_batch:** the batch version's `:148-155` uses `bail!` (calls `abort_batch`). If T1 replaces that with `validate_put_metadata(...)?`, the `?` bypasses `bail!` — which is **fine in phase-1** (no placeholders inserted yet) but violates [P0342](plan-0342-put-path-batch-bail-on-placeholder-insert.md)'s "no `?` in phase-2/3" discipline. Phase-1's metadata handling at `:125-155` is pre-placeholder, so `?` is safe there. The static `?`-grep test from P0342-T2 slices on "after `--- Phase 2:`" marker, so phase-1 `?`s are allowed. **Check at dispatch:** if the phase-2 marker string drifts, verify the slice boundary still excludes phase-1.

### T2 — `refactor(store):` extract apply_trailer helper

NEW fn in [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs). Four steps from [`put_path.rs:515-537`](../../rio-store/src/grpc/put_path.rs) and [`put_path_batch.rs:220-233`](../../rio-store/src/grpc/put_path_batch.rs):

```rust
/// Apply a PutPathTrailer to a ValidatedPathInfo: (1) 32-byte hash
/// check, (2) nar_size bound, (3) hash overwrite, (4) size overwrite.
/// Returns the applied hash as `[u8; 32]` for callers that need it
/// separately (put_path_batch pre-collects for content-index).
pub(super) fn apply_trailer(
    info: &mut ValidatedPathInfo,
    t: &PutPathTrailer,
    ctx_label: &str,
) -> Result<[u8; 32], Status> {
    let hash: [u8; 32] = t.nar_hash.as_slice().try_into().map_err(|_| {
        Status::invalid_argument(format!(
            "{ctx_label}: trailer nar_hash must be 32 bytes, got {}",
            t.nar_hash.len()
        ))
    })?;
    if t.nar_size > MAX_NAR_SIZE {
        return Err(Status::invalid_argument(format!(
            "{ctx_label}: trailer nar_size {} exceeds MAX_NAR_SIZE",
            t.nar_size
        )));
    }
    info.nar_hash = hash;
    info.nar_size = t.nar_size;
    Ok(hash)
}
```

**Call sites:**
- [`put_path.rs:515-537`](../../rio-store/src/grpc/put_path.rs) → `apply_trailer(&mut info, &t, "PutPath").map_err(|e| { self.abort_upload(&store_path_hash).await; e })?` — **but** `abort_upload` is async and `?` doesn't let you run async cleanup. Keep the explicit `match`/early-return in put_path.rs; just call the helper for the hash+size logic:
  ```rust
  let hash = match apply_trailer(&mut info, &t, "PutPath") {
      Ok(h) => h,
      Err(e) => {
          self.abort_upload(&store_path_hash).await;
          return Err(e);
      }
  };
  // `hash` is unused here (put_path reads info.nar_hash directly);
  // let _ = hash;
  ```
- [`put_path_batch.rs:220-233`](../../rio-store/src/grpc/put_path_batch.rs) → `let _hash = match apply_trailer(info, t, &format!("output {idx}")) { Ok(h) => h, Err(e) => bail!(e) };`

### T3 — `test(store):` extraction no-behavior-change proof

NO new test — the existing `gt13_batch_rpc_atomic` at [`chunked.rs:287`](../../rio-store/tests/grpc/chunked.rs) + `PutPath` happy-path tests at `rio-store/tests/grpc/core.rs` cover the validation flow. T1+T2 are pure extraction; all tests passing after = proof of no-behavior-change.

**At dispatch:** run `cargo nextest run -p rio-store grpc` before AND after the extraction; diff test-count and pass-count. Should be identical.

## Exit criteria

- `/nbr .#ci` green
- T1: `grep -c 'fn validate_put_metadata' rio-store/src/grpc/mod.rs` → 1
- T1: `grep -c 'validate_put_metadata' rio-store/src/grpc/put_path.rs rio-store/src/grpc/put_path_batch.rs` → ≥2 (both call sites migrated)
- T1: `grep 'nar_hash must be empty' rio-store/src/grpc/put_path.rs` → 0 (inlined error string moved to helper)
- T1: `grep 'nar_hash must be empty' rio-store/src/grpc/put_path_batch.rs` → 0 (ditto)
- T2: `grep -c 'fn apply_trailer' rio-store/src/grpc/mod.rs` → 1
- T2: `grep -c 'apply_trailer' rio-store/src/grpc/put_path.rs rio-store/src/grpc/put_path_batch.rs` → ≥2
- T3 proof: `cargo nextest run -p rio-store grpc` test-count unchanged pre/post
- Net line delta: `git diff --shortstat` on `put_path.rs` + `put_path_batch.rs` + `mod.rs` → negative net (extraction saves lines)
- `nix develop -c tracey query rule sec.boundary.grpc-hmac` shows ≥2 `impl` sites (existing + T1's consolidated annotation — the helper IS the impl)

## Tracey

References existing markers:
- `r[sec.boundary.grpc-hmac]` — T1 consolidates the HMAC path-in-claims check into one function; the `r[impl]` annotation moves from two sites to one (the helper). The `put_path.rs:316-331` and `put_path_batch.rs:148-155` duplicated checks both implement the same spec clause; after extraction, one annotation on `validate_put_metadata` suffices
- `r[store.put.wal-manifest]` — T2's `apply_trailer` is step-2 of the WAL pattern ("verify against declared NarHash"); no annotation change, the verify-call stays in each handler

No new markers. Pure extraction.

## Files

```json files
[
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T1: +validate_put_metadata(raw_info, claims, ctx) after maybe_sign :273; T2: +apply_trailer(info, trailer, ctx)"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "T1: :282-331 → validate_put_metadata call; T2: :515-537 → apply_trailer call (keep explicit abort_upload match)"},
  {"path": "rio-store/src/grpc/put_path_batch.rs", "action": "MODIFY", "note": "T1: :125-155 → validate_put_metadata call; T2: :219-233 → apply_trailer call (bail! on Err)"}
]
```

```
rio-store/src/grpc/
├── mod.rs                  # T1+T2: new helpers
├── put_path.rs             # T1+T2: call sites migrated
└── put_path_batch.rs       # T1+T2: call sites migrated
```

## Dependencies

```json deps
{"deps": [267], "soft_deps": [342, 304, 0344], "note": "P0267 (DONE) — both files exist with duplication. discovered_from=consolidator(mc84). Soft-dep P0342: T1 here replaces put_path_batch.rs :125-155 with a ? call in phase-1 — P0342's static ?-grep test slices on 'Phase 2:' marker, so phase-1 ? is allowed. If P0342 lands first, no conflict. If this lands first, verify P0342-T2's slice boundary doesn't trip on T1's new ? at :125. Soft-dep P0304-T37/T38: edits :302/:258 — non-overlapping with T1's :125-155 or T2's :219-233. Soft-dep P0344: its T3 adds hmac_rejected counter at :148-155 — if P0344 lands FIRST, T1's extraction absorbs the counter into validate_put_metadata (the helper already has it in the snippet above). If THIS lands first, P0344-T3 is a no-op (the helper has the counter). Either ordering works; both plans' T3/T1 describe the same target state."}
```

**Depends on:** [P0267](plan-0267-atomic-multi-output-tx.md) — merged. Both handlers exist with the duplication at the cited lines.

**Conflicts with:** [`put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs) — [P0342](plan-0342-put-path-batch-bail-on-placeholder-insert.md) T1 edits `:268-275`; [P0304](plan-0304-trivial-batch-p0222-harness.md) T37-T38 edits `:302/:258`; [P0344](plan-0344-put-path-batch-content-index-parity.md) edits post-`:326` + `:148-155`. T1 here edits `:125-155` (overlaps P0344-T3's `:148-155` — **same target state**, either ordering works); T2 edits `:219-233`. [`put_path.rs`](../../rio-store/src/grpc/put_path.rs) — T1 edits `:282-331`, T2 edits `:515-537`; no other UNIMPL plan touches those ranges. [`mod.rs`](../../rio-store/src/grpc/mod.rs) — not in collisions top-50 as `rio-store/src/grpc/mod.rs` specifically; `rio-store/src/grpc.rs` is count=18 (different file).

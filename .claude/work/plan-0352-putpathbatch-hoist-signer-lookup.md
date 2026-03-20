# Plan 0352: PutPathBatch — hoist tenant-signer lookup outside phase-3 loop

> **ERRATUM (line-cite drift post-P0345):** Line cites below were accurate at plan-write time but drifted after P0345's `validate_put_metadata`/`apply_trailer` extraction shifted put_path_batch.rs by dozens of lines. The referenced concepts are fn-name-stable: `maybe_sign` (grpc/mod.rs), `tenant_id` extraction at the request preamble, `pool.begin()` at the phase-3 tx open, the `for (idx, accum)` loop in phase-3. Re-grep at dispatch.

bughunter-mc98 perf finding. [P0338](plan-0338-tenant-signer-wiring-putpath.md) made `maybe_sign` async + DB-hitting (grpc/mod.rs `sign_for_tenant(Some(tid))` → `get_active_signer` pool query). put_path_batch.rs calls `self.maybe_sign(tenant_id, &mut info).await` **inside** the `for (idx, accum) in outputs.iter_mut()` loop, **inside** the open tx at `pool.begin()`.

`tenant_id` is set **once** at `:67` (one JWT per batch → same `Claims.sub` across all outputs → same tenant). For a 10-output batch with `tenant_id.is_some()`: 10 identical `SELECT ... FROM tenant_keys WHERE tenant_id=$1` queries while the tx at `:313` holds row locks on `manifests`. Queries are read-only, tables disjoint (`tenant_keys` vs `manifests`) — no deadlock. But N roundtrips (~1ms each) inside an open tx = N extra ms of lock-hold. At 10 outputs that's negligible; at 100 (possible for a many-output derivation) it's 100ms of avoidable lock-hold.

[`put_path.rs:596`](../../rio-store/src/grpc/put_path.rs) has the same `maybe_sign(tenant_id, ...)` pattern but is single-output → N=1, non-issue there.

Two fix shapes:
- **(a)** cache `get_active_signer` result once before the loop, pass cached `Option<Signer>` to a new `maybe_sign_with_resolved`
- **(b)** resolve to `Option<Signer>` once at `:67` (where `tenant_id` is extracted), pass the resolved signer down through phase-3 instead of the UUID

(b) is cleaner: it eliminates `async` + `Result` from the inner loop entirely. `sign_for_tenant_with_resolved(Option<&Signer>, fp)` becomes sync + infallible. But (b) changes the error-surfacing timing — a `TenantKeyLookup` failure currently warns + falls back per-output inside the loop; with (b) it would warn + fall back once at extraction time. That's strictly better (one warn instead of N) but changes observable log-count.

(a) keeps the existing `maybe_sign` interface, adds an `Option<Signer>` parameter that bypasses the lookup when `Some`. Simpler diff. Chosen here.

## Entry criteria

- [P0338](plan-0338-tenant-signer-wiring-putpath.md) merged (`maybe_sign` is async + calls `sign_for_tenant` — DONE)

## Tasks

### T1 — `perf(store):` TenantSigner::resolve_once — signer lookup helper

MODIFY [`rio-store/src/signing.rs`](../../rio-store/src/signing.rs) after `sign_for_tenant` (~`:238`):

```rust
    /// Resolve the signer for a tenant once — cluster-fallback applied.
    ///
    /// For callers that sign N times with the same tenant_id (e.g.,
    /// PutPathBatch signs each output with the same tenant's key).
    /// Returns (cloned Signer, was_tenant_key). The Signer is cheap
    /// to clone (String + 32-byte seed). Calling `Signer::sign()` on
    /// the returned value is sync + infallible — no further DB hits.
    ///
    /// Same fallback semantics as [`sign_for_tenant`]: `None` tenant,
    /// no active key, or revoked-only → cluster key. DB failure
    /// (`TenantKeyLookup`) propagates — caller decides whether to
    /// fall back (PutPathBatch does, matching maybe_sign's behavior).
    pub async fn resolve_once(
        &self,
        tenant_id: Option<uuid::Uuid>,
    ) -> Result<(Signer, bool), SignerError> {
        if let Some(tid) = tenant_id {
            if let Some(tenant_signer) = crate::metadata::get_active_signer(&self.pool, tid)
                .await
                .map_err(|e| SignerError::TenantKeyLookup(e.to_string()))?
            {
                return Ok((tenant_signer, true));
            }
        }
        Ok((self.cluster.clone(), false))
    }
```

`Signer` needs `Clone` — verify at dispatch it derives or can derive (`String + SigningKey`; `SigningKey` is `Clone`). If not already `#[derive(Clone)]`, add it.

### T2 — `perf(store):` PutPathBatch — resolve signer once before phase-3

MODIFY [`rio-store/src/grpc/put_path_batch.rs`](../../rio-store/src/grpc/put_path_batch.rs). After `tenant_id` extraction at `:67-70` and before `request.into_inner()` at `:72`, OR at the top of phase-3 before the tx begin at `:313`:

```rust
        // Resolve the tenant's signer ONCE. All outputs in the batch
        // share the same tenant (one JWT → one Claims.sub). Without
        // this, maybe_sign(tenant_id, ...) inside the phase-3 loop
        // does N identical get_active_signer queries while the tx at
        // :313 holds manifests row locks. N=10 → 10ms extra lock-hold;
        // N=100 (possible for a many-output derivation) → 100ms.
        //
        // Fallback on TenantKeyLookup Err matches maybe_sign: warn +
        // cluster key. One warn instead of N — same end state.
        let resolved_signer: Option<(crate::signing::Signer, bool)> =
            match self.signer() {
                None => None,
                Some(ts) => match ts.resolve_once(tenant_id).await {
                    Ok(pair) => Some(pair),
                    Err(e) => {
                        warn!(error = %e, ?tenant_id,
                            "tenant-key lookup failed; batch will sign with cluster key");
                        // Same counter as maybe_sign's fallback path
                        // (P0304-T50 if landed; otherwise omit).
                        Some((ts.cluster().clone(), false))
                    }
                },
            };
```

Then replace `:326` `self.maybe_sign(tenant_id, &mut info).await` with a sync inline sign using `resolved_signer`:

```rust
            // Sign with the pre-resolved signer (see resolve_once above).
            // Sync — no DB hit inside the tx.
            if let Some((ref signer, was_tenant)) = resolved_signer {
                self.sign_with_resolved(signer, *was_tenant, &mut info);
            }
```

Where `sign_with_resolved` is a new sync helper in [`grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) extracted from `maybe_sign`'s body (fingerprint computation + `signer.sign(fp)` + `info.signatures.push(sig)` + the empty-refs warn + the `key_label` debug line). `maybe_sign` becomes: `resolve_once` → `sign_with_resolved`. Shared logic, one codepath.

### T3 — `refactor(store):` extract sign_with_resolved from maybe_sign

MODIFY [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) around `:270-345`. Split `maybe_sign` into:

```rust
    /// Sync signing given a pre-resolved Signer. No DB hit.
    /// Extracted so PutPathBatch can resolve once + sign N times
    /// without N roundtrips inside its phase-3 tx.
    fn sign_with_resolved(
        &self,
        signer: &crate::signing::Signer,
        was_tenant: bool,
        info: &mut ValidatedPathInfo,
    ) {
        // [current maybe_sign body: empty-refs warn, refs→String,
        //  fingerprint(), signer.sign(fp), key_label debug, push sig]
    }

    /// Sign narinfo with tenant-or-cluster key. Async (DB lookup).
    /// For single-output paths (PutPath); batch callers should use
    /// [`TenantSigner::resolve_once`] + [`sign_with_resolved`].
    pub(super) async fn maybe_sign(
        &self,
        tenant_id: Option<uuid::Uuid>,
        info: &mut ValidatedPathInfo,
    ) {
        let Some(ts) = self.signer() else { return };
        let (signer, was_tenant) = match ts.resolve_once(tenant_id).await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, ?tenant_id,
                    "tenant-key lookup failed; falling back to cluster key");
                (ts.cluster().clone(), false)
            }
        };
        self.sign_with_resolved(&signer, was_tenant, info);
    }
```

Net: `maybe_sign` retains its interface; `sign_for_tenant` at [`signing.rs:219`](../../rio-store/src/signing.rs) stays ~~(used by [`admin.rs`](../../rio-store/src/grpc/admin.rs))~~. `resolve_once` + `sign_with_resolved` are the new batch-friendly entry.

> **ERRATUM (post-merge):** `sign_for_tenant` has no admin.rs caller — admin.rs uses `cluster()` for ResignPaths (historical paths have no tenant attribution). `sign_for_tenant` has zero production callers post-P0352; only unit tests call it. Dead-code collapse tracked at [P0304-T91](plan-0304-trivial-batch-p0222-harness.md).

**Interaction with [P0304](plan-0304-trivial-batch-p0222-harness.md)-T49:** T49 plans to replace `sign_for_tenant(None, fp).expect("infallible")` with `cluster().sign()` in `maybe_sign`'s fallback. T3 here rewrites `maybe_sign` entirely via `resolve_once` — `resolve_once` already does `self.cluster.clone()` on the `None` path, so T49's change is subsumed. If P0304 lands first, T3 rebases clean (the `.expect()` it removes is gone). If this lands first, P0304-T49 is OBE — note at dispatch.

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'resolve_once\|fn resolve_once' rio-store/src/signing.rs` → ≥1 hit (T1)
- `grep 'sign_with_resolved\|fn sign_with_resolved' rio-store/src/grpc/mod.rs` → ≥1 hit (T3)
- `grep 'maybe_sign(tenant_id' rio-store/src/grpc/put_path_batch.rs` → 0 hits (T2: loop no longer calls async maybe_sign)
- `grep 'resolved_signer\|resolve_once' rio-store/src/grpc/put_path_batch.rs` → ≥1 hit (T2: pre-resolved)
- `cargo nextest run -p rio-store grpc::signing:: put_path_batch` — existing signing + batch tests green (behavior preserved)
- `cargo nextest run -p rio-store put_path_with_tenant_jwt_signs_with_tenant_key` → pass (T3: end-to-end tenant-sign chain unchanged)
- [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T23 (`batch_outputs_signed_with_tenant_key`) → pass if already landed; proves T2 didn't break batch-tenant-sign
- **No-regression on `maybe_sign` fallback:** seed corrupt-seed tenant (reuse [`tenant_keys.rs:227`](../../rio-store/src/metadata/tenant_keys.rs) pattern), call `maybe_sign` → signature verifies under cluster pubkey + upload succeeds. If [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T26 has landed, it covers this; otherwise informal check

## Tracey

References existing marker:
- `r[store.tenant.sign-key]` — [`store.md:188`](../../docs/src/components/store.md). T1's `resolve_once` is a second entry point to the same `// r[impl store.tenant.sign-key]` behavior at [`signing.rs:204`](../../rio-store/src/signing.rs). No new annotation — same marker, same spec clause ("MUST use the tenant's active signing key ... falling back to the cluster key"). Perf refactor doesn't add verify.

No new markers. Pure perf — the spec doesn't constrain query count per batch.

## Files

```json files
[
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T1: +resolve_once after :238; may need #[derive(Clone)] on Signer"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T3: split maybe_sign into resolve+sign_with_resolved ~:270-345"},
  {"path": "rio-store/src/grpc/put_path_batch.rs", "action": "MODIFY", "note": "T2: resolve signer once before phase-3 tx; :326 maybe_sign → sync sign_with_resolved"}
]
```

```
rio-store/src/
├── signing.rs                   # T1: +resolve_once
└── grpc/
    ├── mod.rs                   # T3: maybe_sign split
    └── put_path_batch.rs        # T2: hoist resolution
```

## Dependencies

```json deps
{"deps": [338], "soft_deps": [304, 344, 345, 311], "note": "Bughunter-mc98 perf finding. P0338 (DONE) made maybe_sign async+DB-hitting — the premise. rio-store/src/grpc/mod.rs count=15 (moderate); P0304-T49+T50 edit same fn (:323-339 fallback arm) — T3 here REWRITES maybe_sign, subsumes T49 (cluster().sign() direct) + T50 (tenant_key_lookup_failed_total counter — re-add in resolve_once if desired). Sequence: if P0304 lands first, T3 rebases on its changes; if this lands first, P0304-T49 is OBE (note at dispatch). P0344/P0345 touch put_path_batch.rs but different sections (content-index parity, validate_metadata extraction — not phase-3 sign). P0311-T23 adds batch-tenant-sign test to signing.rs — tests this fix's code; sequence-independent. discovered_from=bughunter."}
```

**Depends on:** [P0338](plan-0338-tenant-signer-wiring-putpath.md) — `maybe_sign` is async + DB-hitting; `tenant_id` extraction at `:67` exists; `sign_for_tenant` exists.

**Conflicts with:** [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) count=15 — [P0304](plan-0304-trivial-batch-p0222-harness.md) T49+T50 edit `:323-339`; T3 rewrites the same region. `put_path_batch.rs` — [P0344](plan-0344-put-path-batch-content-index-parity.md) (content-index) + [P0345](plan-0345-put-path-validate-metadata-helper.md) (validate_put_metadata extraction) touch the preamble and phase-1/2, not phase-3's `:326`. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T37 edits `:302` `.unwrap()→.expect()`, [P0304](plan-0304-trivial-batch-p0222-harness.md)-T38 adds metrics — both non-overlapping with T2's `:326` + pre-phase-3 resolve. [`signing.rs`](../../rio-store/src/signing.rs) — [P0304](plan-0304-trivial-batch-p0222-harness.md)-T48 edits `:202-206` doc-comment; T1 here adds after `:238` — non-overlapping.

# Plan 992487504: malformed cluster_key_history entries fail silently at the gate

[`cluster_key_history.rs:29-38`](../../rio-store/src/metadata/cluster_key_history.rs) returns raw `TEXT` from the DB — no parse, no validate. No admin-CLI write path exists ([`:7-8`](../../rio-store/src/metadata/cluster_key_history.rs) says "admin-CLI territory" but [P992487505](plan-992487505-admin-cli-key-tables.md) hasn't landed), so manual `psql INSERT` is the only populate method. [`any_sig_trusted` at signing.rs:61-66](../../rio-store/src/signing.rs) `filter_map`s each entry through `split_once(':')?` → `b64.decode(..).ok()?` → `[u8; 32].try_into().ok()?` → `VerifyingKey::from_bytes(..).ok()?` — every `?` is a silent discard.

**Failure mode:** operator typos a pubkey during rotation (`cache.example.com-2:VGhpcyBpcyBub3QgMzIgYnl0ZXM=` — wrong length). Startup logs `[loaded N prior cluster keys]` (N includes the bad entry — it's just a string count). Old-key paths go dark for cross-tenant reads (`sig_visibility_gate` has no matching key). **Zero signal.** Operator debugs for hours wondering why paths "disappeared" after rotation.

discovered_from=521. origin=reviewer.

## Entry criteria

- [P0521](plan-0521-cluster-key-rotation-contradiction.md) merged (`cluster_key_history` table + `load_cluster_key_history` query exist) — **DONE**

## Tasks

### T1 — `fix(store):` try-parse each entry at load, warn+skip malformed

The parse chain [`any_sig_trusted` uses at :61-66](../../rio-store/src/signing.rs) is correct — just needs to run ONCE at load time with logging, instead of silently per-verify. Extract the per-key parse into a helper:

```rust
// signing.rs — new helper near any_sig_trusted (:~55), or in a
// pubkey_entry submodule if the crate has one:

/// Parse a `name:base64(pubkey)` trusted-key entry. This is the
/// format Nix's `trusted-public-keys` uses, what `Signer::trusted_key_entry`
/// emits, and what `cluster_key_history.pubkey` stores.
///
/// Returns `None` with a distinguishable error reason for each
/// failure point — load-time validation logs this; hot-path
/// `any_sig_trusted` discards it (perf: don't allocate error
/// strings per-verify-per-key).
pub(crate) fn parse_trusted_key_entry(entry: &str) -> Result<(&str, VerifyingKey), &'static str> {
    let (name, pk_b64) = entry.split_once(':')
        .ok_or("missing ':' separator (expected name:base64(pubkey))")?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let pk_bytes = b64.decode(pk_b64)
        .map_err(|_| "pubkey is not valid base64")?;
    let pk: [u8; 32] = pk_bytes.try_into()
        .map_err(|_| "pubkey is not 32 bytes (ed25519 public key length)")?;
    VerifyingKey::from_bytes(&pk)
        .map(|vk| (name, vk))
        .map_err(|_| "pubkey is not a valid ed25519 point")
}
```

Then at [`load_prior_cluster` (signing.rs:307-311)](../../rio-store/src/signing.rs) or its caller in `main.rs`:

```rust
pub async fn load_prior_cluster(pool: &sqlx::PgPool) -> Result<Vec<String>, SignerError> {
    let entries = crate::metadata::load_cluster_key_history(pool)
        .await
        .map_err(|e| SignerError::TenantKeyLookup(e.to_string()))?;

    // r[impl store.key.rotation-cluster-history]
    // Validate at load. any_sig_trusted's filter_map silently discards
    // malformed entries — an operator typo during rotation would make
    // old-key paths go dark with ZERO signal. Parse here, once, loudly.
    let mut valid = Vec::with_capacity(entries.len());
    for entry in entries {
        match parse_trusted_key_entry(&entry) {
            Ok((name, _)) => {
                tracing::debug!(key_name = name, "prior cluster key loaded");
                valid.push(entry);
            }
            Err(reason) => {
                tracing::warn!(
                    entry = %entry, reason,
                    "malformed cluster_key_history entry — SKIPPED. \
                     Paths signed under this key will fail sig-visibility gate. \
                     Fix the cluster_key_history.pubkey column or retire the row."
                );
                // warn+skip, not fail-startup. A malformed OLD key
                // shouldn't block store boot — but the warn is loud,
                // and the skip is now OBSERVABLE (log + count mismatch).
            }
        }
    }
    if valid.len() != entries.len() {
        tracing::warn!(
            loaded = valid.len(), total = entries.len(),
            skipped = entries.len() - valid.len(),
            "cluster_key_history: some entries malformed — see above"
        );
    }
    Ok(valid)
}
```

**warn+skip vs fail-startup:** warn+skip. A malformed OLD key (rotation history) shouldn't block store boot — the current key is fine, current-key sigs verify. But the warn is loud (`tracing::warn!` goes to structured logs → alertable), and the summary `loaded != total` makes the skip explicit. If the operator WANTS fail-fast, `RIO_STORE_STRICT_KEY_HISTORY=1` env could flip it — but that's scope creep; the warn is enough signal.

### T2 — `refactor(store):` any_sig_trusted uses parse_trusted_key_entry

Replace the inline `filter_map` chain at [`:61-66`](../../rio-store/src/signing.rs) with the helper — same behavior (silent discard at verify-time), but now the parse is in one place:

```rust
let keys: Vec<(&str, VerifyingKey)> = trusted_keys
    .iter()
    .filter_map(|k| parse_trusted_key_entry(k).ok())  // .ok() discards reason — hot path
    .collect();
```

`Result → Option` via `.ok()` keeps the hot-path silent (don't log per-verify-per-key). The point is: load-time validates LOUDLY with the same function that verify-time discards SILENTLY. Divergence is impossible.

### T3 — `test(store):` malformed entry is warned+skipped, valid entries still loaded

```rust
// r[verify store.key.rotation-cluster-history]
#[tokio::test]
async fn load_prior_cluster_skips_malformed_warns_loudly() {
    let db = TestDb::new(&crate::MIGRATOR).await;

    // 3 entries: valid, malformed (bad b64), valid.
    sqlx::query(
        "INSERT INTO cluster_key_history (pubkey, created_at) VALUES \
         ('cache-v1:+xowaKa72B8f4byIFY0F+Zp5dOxlcjvW3TDGZR48rPM=', now()), \
         ('cache-bad:!!!not base64!!!', now()), \
         ('cache-v2:YHbVOPZg1LDoTUj+TTzgAkYg/FPUcr0N5VVqmhTucQ4=', now())"
    ).execute(&db.pool).await.unwrap();

    // Capture tracing output — tracing-test crate or a layer subscriber.
    let loaded = TenantSigner::load_prior_cluster(&db.pool).await.unwrap();

    assert_eq!(loaded.len(), 2, "malformed entry skipped, 2 valid loaded");
    assert!(loaded.iter().any(|e| e.starts_with("cache-v1:")));
    assert!(loaded.iter().any(|e| e.starts_with("cache-v2:")));
    assert!(!loaded.iter().any(|e| e.starts_with("cache-bad:")));

    // assert! on captured warn: "malformed cluster_key_history entry"
    // assert! on captured warn: "loaded = 2, total = 3, skipped = 1"
}

#[test]
fn parse_trusted_key_entry_error_reasons() {
    // Each failure point gets a distinct reason — operator can tell
    // WHAT's wrong without comparing against a spec.
    assert_eq!(parse_trusted_key_entry("no-colon").unwrap_err(),
               "missing ':' separator (expected name:base64(pubkey))");
    assert_eq!(parse_trusted_key_entry("name:!!!").unwrap_err(),
               "pubkey is not valid base64");
    assert_eq!(parse_trusted_key_entry("name:dGVzdA==").unwrap_err(),  // "test" → 4 bytes
               "pubkey is not 32 bytes (ed25519 public key length)");
    // Valid 32-byte b64 but not a valid curve point — harder to construct,
    // may skip. ed25519-dalek rejects low-order points; all-zero is one.
}
```

## Exit criteria

- `/nbr .#ci` green
- `grep 'parse_trusted_key_entry' rio-store/src/signing.rs` → ≥3 hits (definition + load_prior_cluster caller + any_sig_trusted caller)
- `grep 'malformed cluster_key_history' rio-store/src/signing.rs` → ≥1 hit (the warn message)
- `cargo nextest run -p rio-store load_prior_cluster_skips_malformed` → passes
- `cargo nextest run -p rio-store parse_trusted_key_entry_error_reasons` → passes
- Mutation: change T1's `Err(reason) => warn!` arm back to silent `Err(_) => {}` → T3's warn-capture assert FAILS
- `grep 'filter_map.*split_once.*decode.*try_into.*from_bytes' rio-store/src/signing.rs` → 0 hits (inline chain replaced by helper — no second copy to drift)

## Tracey

References existing markers:
- `r[store.key.rotation-cluster-history]` ([`store.md:218`](../../docs/src/components/store.md)) — T1's validation and T3's test serve this marker. Spec says "Prior keys are loaded from `cluster_key_history`" — this plan makes "loaded" mean "loaded AND valid". T1 adds a second `r[impl]` site (the validation loop); T3 adds a `r[verify]`.

No new markers. The validation requirement is implied by the existing spec ("MUST remain in the trusted set" can't mean "silently ignored if you typo").

## Files

```json files
[
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T1: NEW parse_trusted_key_entry helper near :55. load_prior_cluster :307-311 adds validate loop. T2: :61-66 filter_map → parse_trusted_key_entry().ok(). P0304-T992487505 (SignerError docstring) touches :131-139 — diff section, clean rebase"},
  {"path": "rio-store/src/metadata/cluster_key_history.rs", "action": "MODIFY", "note": "T3: new test load_prior_cluster_skips_malformed_warns_loudly in the #[cfg(test)] mod at :42+. No production change — load query stays raw TEXT (validation is caller-side at load_prior_cluster)"}
]
```

```
rio-store/src/
├── signing.rs                         # T1+T2: parse helper, load validation, any_sig_trusted refactor
└── metadata/cluster_key_history.rs    # T3: test
```

## Dependencies

```json deps
{"deps": [521], "soft_deps": [992487505], "note": "P0521 (DONE) created the table + load query. P992487505 (admin-CLI) closes the write-side gap — validation-at-write is better than validation-at-read, but this plan's read-side check is defense regardless. Ships independently of P992487505."}
```

**Depends on:** [P0521](plan-0521-cluster-key-rotation-contradiction.md) (DONE) — created `cluster_key_history` table (migration 027) and `load_cluster_key_history` query.

**Soft-dep:** [P992487505](plan-992487505-admin-cli-key-tables.md) — adds admin-CLI write path with validation-at-write. That's BETTER than validation-at-read (reject before it's in the DB). But manual `psql INSERT` is always possible, so this plan's read-side check is defense regardless. Ships independently.

**Conflicts with:** `signing.rs` — [P0304](plan-0304-trivial-batch-p0222-harness.md) T992487505 (SignerError docstring at `:131-139`) touches the same file, DIFFERENT section (`:55`/`:307` vs `:131-139`), clean rebase. `cluster_key_history.rs` is low-traffic — P992487505 may add a write fn here; T3 adds a test, both additive.

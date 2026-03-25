# Plan 0429: Manifest::total_size — wire into GetPath reassembly sanity-check

sprint-1 cleanup finding. [`Manifest::total_size`](../../rio-store/src/manifest.rs) at `:149` is `#[cfg(test)]`-gated — it sums chunk sizes (u64, handles 200k × 256 KiB = 50 GiB > u32::MAX) and was intended for GetPath reassembly to sanity-check against `narinfo.nar_size` before streaming chunks back. Currently test-only; the GetPath handler never calls it. Promoted from `TODO(P0429)` at `manifest.rs:149`.

The check catches manifest/narinfo drift: if PutPath wrote a manifest whose summed chunk sizes don't match the narinfo's `nar_size`, GetPath would stream corrupt data. Better to fail fast with a clear error than deliver garbage.

## Entry criteria

- [`rio-store/src/manifest.rs`](../../rio-store/src/manifest.rs) `Manifest::total_size` exists (it does, test-gated)
- [`rio-store/src/grpc/get_path.rs`](../../rio-store/src/grpc/get_path.rs) or equivalent GetPath handler exists and loads both manifest and narinfo

## Tasks

### T1 — `feat(store):` drop #[cfg(test)] on Manifest::total_size

MODIFY [`rio-store/src/manifest.rs`](../../rio-store/src/manifest.rs) at `:151`. Remove the `#[cfg(test)]` attribute so `total_size()` is callable from production code. Keep the doc-comment explaining the u64 rationale.

### T2 — `feat(store):` GetPath — sanity-check manifest.total_size() against narinfo.nar_size

MODIFY the GetPath handler (grep `fn get_path` in `rio-store/src/grpc/`). After loading both manifest and narinfo, before streaming chunks:

```rust
let manifest_size = manifest.total_size();
if manifest_size != narinfo.nar_size {
    return Err(Status::data_loss(format!(
        "manifest/narinfo size mismatch for {}: manifest sums to {} bytes, narinfo says {} bytes",
        store_path, manifest_size, narinfo.nar_size
    )));
}
```

`Status::data_loss` is the right gRPC code — the store's persistent state is inconsistent, not a transient failure.

### T3 — `test(store):` GetPath rejects manifest/narinfo size mismatch

NEW test alongside existing GetPath tests. Seed a narinfo with `nar_size = 100` but a manifest whose chunks sum to `99`. Assert GetPath returns `Status::data_loss` with the mismatch message, and does NOT stream any chunk bytes.

## Exit criteria

- `/nixbuild .#ci` green
- `grep '#\[cfg(test)\]' rio-store/src/manifest.rs` around `total_size` → 0 hits (gate removed)
- `grep 'total_size()' rio-store/src/grpc/` → ≥1 hit (wired into GetPath)
- New mismatch test passes; existing GetPath happy-path tests still pass

## Tracey

References existing `r[store.get.reassembly]` if present (check `docs/src/components/store.md`). If no marker exists for the sanity-check behavior, add `r[store.get.size-sanity-check]` to store.md and annotate T2 with `// r[impl store.get.size-sanity-check]`, T3 with `// r[verify store.get.size-sanity-check]`.

## Files

```json files
[
  {"path": "rio-store/src/manifest.rs", "action": "MODIFY", "note": "T1: remove #[cfg(test)] on total_size at :151"},
  {"path": "rio-store/src/grpc/get_path.rs", "action": "MODIFY", "note": "T2: +size sanity-check before chunk streaming. T3: +mismatch test. Re-grep exact filename at dispatch (may be grpc/mod.rs or grpc/store.rs)"}
]
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "sprint-1 cleanup finding (discovered_from=rio-store cleanup worker). Standalone — manifest.rs and GetPath handler both exist today. No hard deps. Low-traffic files."}
```

**Depends on:** nothing — both files exist.
**Conflicts with:** [`manifest.rs`](../../rio-store/src/manifest.rs) — [P0311-T65](plan-0311-test-gap-batch-cli-recovery-dash.md) adds mutants-triage tests to manifest.rs `cfg(test)` mod; T1 here touches the `total_size` fn above it. Non-overlapping.

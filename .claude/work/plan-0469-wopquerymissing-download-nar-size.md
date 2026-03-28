# Plan 469: wopQueryMissing downloadSize/narSize — wire real sizes from FindMissingPathsResponse

[P0463](plan-0463-upstream-substitution-surface.md) review finding: [`opcodes_read.rs:779-780`](../../rio-gateway/src/handler/opcodes_read.rs) still hardcodes `downloadSize` and `narSize` to `0` in the `wopQueryMissing` response, even though `willSubstitute` is now populated. Result: `nix build` shows "0.00 MiB download" regardless of actual substitution payload — misleading UX. No `TODO` tag exists on the hardcoded zeros.

`FindMissingPathsResponse` (from [P0462](plan-0462-upstream-substitution-core.md)) SHOULD carry per-path size info from the upstream narinfo; if it doesn't, extend the proto. This plan wires the sizes through.

## Entry criteria

- [P0463](plan-0463-upstream-substitution-surface.md) merged (`willSubstitute` populated in `wopQueryMissing` response) — **DONE**

## Tasks

### T1 — `fix(proto):` extend FindMissingPathsResponse with size fields (if absent)

Check [`rio-proto/src/types.proto`](../../rio-proto/src/types.proto) — does `FindMissingPathsResponse` have `download_size_bytes` / `nar_size_bytes` fields? If not, add:

```proto
message FindMissingPathsResponse {
  repeated string missing_paths = 1;
  repeated string substitutable_paths = 2;
  // Aggregate sizes across substitutable_paths. Populated from
  // upstream narinfo FileSize/NarSize. Zero if unknown.
  uint64 download_size_bytes = 3;
  uint64 nar_size_bytes = 4;
}
```

Aggregate (not per-path) is what `wopQueryMissing` needs — the Nix wire protocol sends two `u64`s total, not per-path. If the proto already has per-path sizes, sum them gateway-side.

### T2 — `fix(store):` populate size fields in FindMissingPaths handler

MODIFY the `FindMissingPaths` gRPC handler in [`rio-store/src/grpc/`](../../rio-store/src/grpc/) (grep for `find_missing_paths` impl). When the substituter reports a path as available, it has the narinfo — extract `FileSize` (compressed download) and `NarSize` (uncompressed), accumulate into the response fields.

If the narinfo lacks size fields (rare but valid), contribute `0` for that path — don't fail the whole request.

### T3 — `fix(gateway):` wire response sizes into wopQueryMissing

MODIFY [`rio-gateway/src/handler/opcodes_read.rs`](../../rio-gateway/src/handler/opcodes_read.rs) at `:779-780`:

```diff
-    wire::write_u64(w, 0).await?; // downloadSize
-    wire::write_u64(w, 0).await?; // narSize
+    wire::write_u64(w, download_size).await?;
+    wire::write_u64(w, nar_size).await?;
```

Where `download_size` / `nar_size` come from the `FindMissingPathsResponse` captured at `:724-730`. Currently that match arm extracts only `missing_paths` and `substitutable_paths` into HashSets — extend to also capture the two size `u64`s.

## Exit criteria

- `/nixbuild .#ci` → green
- `grep 'write_u64(w, 0).*downloadSize\|write_u64(w, 0).*narSize' rio-gateway/src/handler/opcodes_read.rs` → 0 hits (hardcoded zeros gone)
- `nix-store --query --size ssh-ng://<gateway> /nix/store/<known-cached-path>` via the VM test → non-zero size reported
- `grep 'download_size_bytes\|nar_size_bytes' rio-proto/src/types.proto` → ≥2 hits (proto fields exist)

## Tracey

References existing markers:
- `r[gw.opcode.query-missing]` — T3 completes the response wire-up ([`gateway.md:303`](../../docs/src/components/gateway.md)); the marker's spec text should mention downloadSize/narSize if it doesn't already — check at dispatch, `tracey bump` if the spec text needs amending
- `r[store.substitute.upstream]` — T2 extends the substituter's response surface ([`store.md:212`](../../docs/src/components/store.md))

No new markers — this completes an existing opcode's response, doesn't introduce new behavior.

## Files

```json files
[
  {"path": "rio-proto/src/types.proto", "action": "MODIFY", "note": "T1: CONDITIONAL — add download_size_bytes/nar_size_bytes to FindMissingPathsResponse if absent"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "T2: populate size fields in find_missing_paths handler (path may be substitute.rs — grep at dispatch)"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "T3: wire response sizes at :779-780; extend match arm at :724-730 to capture sizes"}
]
```

```
rio-proto/src/
└── types.proto                 # T1: size fields (conditional)
rio-store/src/grpc/
└── admin.rs or substitute.rs   # T2: populate from narinfo
rio-gateway/src/handler/
└── opcodes_read.rs             # T3: wire to response :779-780
```

## Dependencies

```json deps
{"deps": [463], "soft_deps": [465, 462], "note": "P0463 review finding — hardcoded zeros at :779-780 now that willSubstitute is populated. Soft-dep P0465 (JWT propagation) — both touch opcodes_read.rs handle_query_missing; P0465 edits request construction ~:722, this edits response tail :779-780. Non-overlapping hunks in same fn. Soft-dep P0462 — FindMissingPathsResponse proto originated there; T1 may be no-op if P0462 already added size fields. discovered_from=463."}
```

**Depends on:** [P0463](plan-0463-upstream-substitution-surface.md) — `willSubstitute` populated. **DONE**.
**Soft-dep:** [P0465](plan-0465-gateway-jwt-propagation-read-opcodes.md) — same fn, different hunks. [P0462](plan-0462-upstream-substitution-core.md) — proto message origin; check if size fields already exist.
**Conflicts with:** [`opcodes_read.rs`](../../rio-gateway/src/handler/opcodes_read.rs) — P0465 T1 touches `:722` request head; this plan's T3 touches `:779-780` response tail + `:724-730` match arm. Middle of same fn. Prefer landing P0465 first.

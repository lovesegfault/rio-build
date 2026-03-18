# Plan 0181: Rem-02 — Empty references: NAR Boyer-Moore scanner + GC gate + ResignPaths (P0, 6-PR series)

## Design

**The phase's single largest fix.** P0 blast radius: GC sweeps live paths past `grace_hours=2`; signatures permanently commit to `refs=""`; sandbox ENOENT on transitive deps. Every path uploaded since phase 1b had empty `references` — the worker's `upload_all_outputs` never populated the field. A 6-PR series because each component has independent review/test surfaces.

**PR 1/6 (`002c95a`) — `rio-nix/src/refscan.rs`:** `RefScanSink` implementing Nix C++ `src/libstore/references.cc`'s Boyer-Moore skip-scan. NOT Aho-Corasick — a purpose-built scan that exploits the restricted nixbase32 alphabet: scan each 32-byte window backwards, skip j+1 positions on non-nixbase32 byte at offset j. Binary NAR sections traverse at ~O(n/32). Seam buffer (≤31 bytes rolling tail) handles chunk-boundary straddling — **zero-alloc** via take/truncate/restore. Proptest caught a duplicate-byte bug in the seam logic on first draft (Nix's `auto s = tail;` is a copy, the Rust mistranslation wasn't). `CandidateSet` hash→full-path map with sorted `resolve()` (PathInfo.references order affects narinfo signature fingerprint). 22 unit tests + 2 proptests + fuzz target.

**PR 2/6 (`9165dc2`) — worker integration:** pre-scan pass before the retry loop. One extra disk read through the scanner only (no hash, no network). `PathInfo` is gRPC message 0 (trailer-mode protocol) so can't know refs until dump finishes — chose pre-scan over proto change. Candidate set = `resolved_input_srcs ∪ drv.outputs()` (direct inputs only — Nix itself doesn't scan transitive). `deriver` field populated from `drv_path`.

**PR 3/6 (`ba2d6f0`) — store safety:** `maybe_sign` warns + increments `rio_store_sign_empty_refs_total` on zero-ref non-CA sign. `check_empty_refs_gate`: before mark phase, count sweep-eligible rows with empty refs + no CA; if >10%, refuse GC with `FailedPrecondition`. Pre-fix stores are ~100% empty-ref → gate prevents catastrophic sweep.

**PR 4/6 (`522711b`) + 4b (`9b68274`) — backfill:** migration 010 `refs_backfilled` flag column + idx. `ResignPaths` admin RPC: wet-run fetches NAR from chunk store, scans for refs, updates `references` + re-signs, sets `refs_backfilled=true`.

**PR 5/6 (`9f38ea2`) — VM e2e:** build → scan → PG → GC survival.

**PR 6/6 (`4bb8aab`) — runbook:** `docs/src/runbooks/gc-enablement.md` checklist.

Supporting: `6663434` spec markers; `578289e` tracey fix; `4bdcb76` executor candidate-set integration + refscan alphabet dedup.

## Files

```json files
[
  {"path": "rio-nix/src/refscan.rs", "action": "NEW", "note": "RefScanSink Boyer-Moore skip-scan; CandidateSet; seam buffer; is_exhausted fast path"},
  {"path": "rio-nix/fuzz/fuzz_targets/refscan.rs", "action": "NEW", "note": "fuzz target + corpus seeds"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "pre-scan pass; UploadResult.references; deriver populated"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "plumb drv_path + resolved_input_srcs ∪ drv.outputs() candidate set"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "maybe_sign: warn + metric on zero-ref non-CA"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "check_empty_refs_gate before mark; ResignPaths RPC wet-run"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "wire ResignPaths handler"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "GcGateResult; ResignPathsRequest/Response"},
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "ResignPaths RPC"},
  {"path": "migrations/010_refscan_backfill.sql", "action": "NEW", "note": "refs_backfilled flag column"},
  {"path": "migrations/011_refscan_backfill_idx.sql", "action": "NEW", "note": "partial idx on refs_backfilled=false"},
  {"path": "docs/src/runbooks/gc-enablement.md", "action": "NEW", "note": "operator checklist for safe GC enable post-backfill"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "rewrite §upload; 2 spec markers; drop Phase-deferral"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "spec markers for gate + empty-ref warn"},
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "e2e refs subtest: build → scan → PG → GC survival"},
  {"path": "nix/fuzz.nix", "action": "MODIFY", "note": "refscan fuzz target"}
]
```

## Tracey

- `r[impl worker.upload.references-scanned]` — `9165dc2`
- `r[impl worker.upload.deriver-populated]` — `9165dc2`
- `r[verify worker.upload.references-scanned]` — `9165dc2` (unit) + `4bdcb76` (integration) + `9f38ea2` (VM e2e)
- `r[verify worker.upload.deriver-populated]` — `9165dc2` + `9f38ea2`
- `r[impl store.gc.empty-refs-gate]` ×2 — `ba2d6f0`
- `r[verify store.gc.empty-refs-gate]` ×4 — `ba2d6f0`
- `r[impl store.signing.empty-refs-warn]` — `ba2d6f0`
- `r[verify store.signing.empty-refs-warn]` ×3 — `ba2d6f0`
- `# r[verify store.gc.two-phase]` — `9f38ea2` (VM)

17 marker annotations (6 impl, 11 verify — largest tracey footprint of any phase-4a plan).

## Entry

- Depends on P0148: phase 3b complete (worker upload path from 2c, store GC from 3b)

## Exit

Merged as `35171c7` (plan §2) + 10-commit fix series: `002c95a`, `9165dc2`, `ba2d6f0`, `522711b`, `9b68274`, `4bb8aab`, `9f38ea2`, `6663434`, `578289e`, `4bdcb76`. `.#ci` green. Remediation doc: `docs/src/remediations/phase4a/02-empty-references-nar-scanner.md` (1358 lines). **`TODO(phase4b)` at `upload.rs:166`:** trailer-refs protocol extension if pre-scan cost measurable.

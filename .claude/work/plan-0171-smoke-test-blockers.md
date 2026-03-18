# Plan 0171: Smoke-test blockers — 4 pre-existing bugs (willBuild!out, Realisation basename, builtin system, lease targeting)

## Design

Four bugs uncovered while validating the gateway streaming changes on a cold EKS store. All pre-date the streaming work — the smoke test had never passed end-to-end on a fresh cluster.

**`wopQueryMissing` echoes DerivedPath in `willBuild` (`rio-gateway`):** `handle_query_missing` pushed the raw `...drv!out` DerivedPath string into `willBuild`. Nix expects a `StorePathSet` — client fails `StorePath::parse` on `!`. Fixed: push `drv.as_str()` for Built paths. Test was `.contains()` which masked the bug; tightened to exact equality.

**`BuildResult` Realisation JSON `outPath` must be basename (`rio-nix`):** `write_build_result` sent full `/nix/store/...` path. Nix's `StorePath::to_string()` is basename-only; `Realisation::fromJSON` parses via that, failing with `illegal base-32 character '/'`. Fixed: strip `STORE_PREFIX` on write, prepend on read for consistent internal full-path representation. (Rem-06 later found the same bug class in `opcodes_read.rs` — see P0185.)

**Workers must advertise `system="builtin"` (`rio-worker`):** bootstrap fetchurl drvs (busybox, bootstrap-tools) have `system="builtin"` `builder="builtin:fetchurl"`. Workers only advertised `x86_64-linux` → `can_build()` always false → 57 DAG leaves permanently undispatchable on cold store. Every `nix-daemon` supports `builtin:fetchurl` internally; workers now always append `"builtin"`.

**Smoke test targets leader pod via Lease:** `kubectl exec deploy/` picks an arbitrary pod → 50% standby "not leader". `sched_leader`/`sched_exec` helpers read the Lease `holderIdentity`.

Riders: `661e539` pinned a proptest regression for BuildResult outPath roundtrip; `d12cdfa` bumped nix 2.33.3→2.34.1 / nixpkgs 25.05→25.11; `395a27c` deferred connection log/metrics to first auth callback (reduced noise); `6b8ba28` added `global.logLevel` helm knob.

## Files

```json files
[
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "handle_query_missing: push drv.as_str() not DerivedPath for willBuild"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "write_build_result: strip STORE_PREFIX on outPath write; prepend on read"},
  {"path": "rio-worker/src/config.rs", "action": "MODIFY", "note": "always append \"builtin\" to configured systems"},
  {"path": "infra/eks/smoke-test.sh", "action": "MODIFY", "note": "sched_leader/sched_exec read Lease holderIdentity"},
  {"path": "rio-nix/proptest-regressions/protocol/build.txt", "action": "NEW", "note": "pinned regression seed for BuildResult outPath roundtrip"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "defer connection log/metrics to first auth callback"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "global.logLevel knob"}
]
```

## Tracey

No new markers. The Realisation basename fix is the `protocol/build.rs` instance of what rem-06 later fixed for `opcodes_read.rs`.

## Entry

- Depends on P0170: gateway streaming (smoke test validated it, these blocked)
- Depends on P0169: Karpenter + NLB (smoke test runs against the EKS deployment)

## Exit

Merged as `5786f82`, `661e539`, `d12cdfa`, `395a27c`, `6b8ba28` (5 commits). `.#ci` green. **Smoke test passes end-to-end on cold EKS store for the first time.**

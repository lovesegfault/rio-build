# ADR-018: CA Resolution and `dependentRealisations` Schema

## Status
Accepted (Spike — 2026-03-19, P0247)

## Context

[ADR-004](./004-ca-ready-design.md) committed to CA-ready schema from Phase 2c. [`opcodes_read.rs`](../../../rio-gateway/src/handler/opcodes_read.rs) discards `dependentRealisations` from `wopRegisterDrvOutput` and hardcodes `{}` for `wopQueryRealisation`. [phase5.md:19](../phases-archive/phase5.md) claimed "resolution will require persisting it." [`r[sched.ca.resolve]`](../components/scheduler.md) claims the junction table is "populated by `wopRegisterDrvOutput`."

This spike was planned as a live-daemon wire capture (strace a CA-on-CA build chain in a VM). Source-level analysis of the locked flake input `github:NixOS/Nix@26842787` made live capture unnecessary: **Nix removed `dependentRealisations` from the data model upstream**. Live capture of any CA chain against current Nix would only confirm the source — it's always `{}`.

**USER Q3 already decided:** `realisation_deps` junction table, not JSONB. This ADR records the wire evidence and corrects the population source.

## Finding

**`dependentRealisations` is dead upstream.** In the locked Nix source:

| Evidence | Source (`NixOS/Nix@26842787`) |
|---|---|
| Struct has no such field | `src/libstore/include/nix/store/realisation.hh:53-74` — `UnkeyedRealisation` holds only `outPath` + `signatures` |
| Serializer writes `{}` stub | `src/libstore/realisation.cc:105` — `{"dependentRealisations", json::object()}` with comment `// back-compat` |
| Deserializer ignores it | `src/libstore/realisation.cc:85-97` — reads only `outPath`, `signatures` |
| Schema doc records removal | `doc/manual/source/protocols/json/schema/build-trace-entry-v2.yaml:19` — `"Version 2: Remove dependentRealisations"` |
| Test comment confirms intent | `src/libstore-tests/realisation.cc:82` — *"We no longer have a notion of 'dependent realisations'"* (read-only back-compat test) |

On the wire, `dependentRealisations` is **always `{}`** for any Nix client built from this revision or later. Older clients may still send populated maps (historical shape documented in Appendix A) — rio-gateway should accept-and-discard defensively, which is what it already does.

### Why Nix removed it

Nix's [build-trace model](https://github.com/NixOS/Nix/blob/26842787/doc/manual/source/store/build-trace.md) split Realisations into:

- **Base build trace**: resolved-derivation-hash → output-path. Coherent by construction.
- **Derived build trace**: unresolved-derivation-hash → output-path. A local memoization cache that MUST be backed by underlying base entries.

`dependentRealisations` was an attempt to bundle derived-trace provenance into a single Realisation record. The new model keeps them separate: base entries are the unit of exchange (what goes over the wire); derived entries are a local-store optimization each implementation manages for itself. Bundling provenance into the wire record conflated the two.

## Decision

### 1. `realisation_deps` junction table stands — but population source changes

USER Q3 remains: `realisation_deps(realisation_id, dep_realisation_id)` junction table (migration 015, P0249).

**Population source:** the scheduler populates it during its own `tryResolve`-equivalent pass, NOT the gateway from wire payloads. When `r[sched.ca.resolve]` rewrites a CA derivation's `inputDrvs` by querying the realisations table for each input's `(drv_hash, output_name)`, each successful lookup is a dependency edge. Insert into `realisation_deps` at resolution time.

This is rio's **derived build trace** per Nix's model — a local cache that rio computes from its own base entries. It never crosses the wire.

### 2. Gateway handlers stay as-is for `dependentRealisations`

`handle_register_drv_output`: continue ignoring the field. Parsing it would yield `{}` for all current clients.

`handle_query_realisation`: continue emitting `{}`. The gateway could emit from `realisation_deps`, but current Nix's deserializer ignores it (`realisation.cc:85-97`), so the bytes are wasted. Revisit only if a future Nix reintroduces the field.

### 3. Resolution logic belongs in the scheduler

Per Nix's `Derivation::tryResolve` (`derivations.cc:1215-1239`) — see Appendix B. The scheduler's resolve step:

1. For each `(input_drv_path, output_names)` in `inputDrvs`:
   - Compute the input drv's hash modulo (same as the `drv_hash` half of the `realisations` PK)
   - Query `realisations` for `(drv_hash, output_name)` → `output_path`
   - Insert `output_path` into the resolved derivation's `inputSrcs`
   - Record the rewrite: `DownstreamPlaceholder(input_drv, output)` → `output_path`
   - (Side effect: insert edge into `realisation_deps`)
2. String-replace all placeholder renderings throughout env/args/builder
3. Drop `inputDrvs` — the resolved derivation is a `BasicDerivation` with only `inputSrcs`

## CA-on-CA Frequency (Informational)

Per USER A10, P0253 is T0 regardless. Sampled for context.

In the locked nixpkgs (flake input, 37,187 `.nix` files under `pkgs/`), **3 files** set `__contentAddressed = true`:

- `pkgs/stdenv/linux/bootstrap-tools/default.nix:18`
- `pkgs/stdenv/generic/default.nix:99-103`
- `pkgs/stdenv/darwin/default.nix:73`
- plus `pkgs/stdenv/generic/make-derivation.nix:367` (`mkDerivation` itself)

**All four are gated behind `config.contentAddressedByDefault`** (defined `pkgs/top-level/config.nix:195`, default `false`, `mkMassRebuild`-tagged). No individual package opts into CA piecemeal.

| Config | CA-on-CA frequency | Resolution path |
|---|---|---|
| Default nixpkgs | ~0% | Cold |
| `config.contentAddressedByDefault = true` | ~100% — every `mkDerivation` chain | Hot |

**Bimodal, no middle ground.** A tenant either runs default nixpkgs (resolution never fires) or a full-CA config (resolution fires on every dispatch). P0253's resolve step is either dead code or critical-path; there is no "occasional" case.

## Downstream Impact

| Item | Was | Now |
|---|---|---|
| **P0249 migration 015** | Schema unchanged | Schema unchanged — column sizes confirmed (64-byte hex + output name per key, store path per value). No FK to wire data. |
| **P0253 T1** | "parse `dependentRealisations` JSON, INSERT into `realisation_deps`" | **Drop T1 gateway-parse.** Gateway stays as-is. `realisation_deps` populated by scheduler during resolve (P0253 T2/T3 scope). |
| **P0253 T2/T3** | Scheduler resolve step | Unchanged — now also the population source for `realisation_deps`. |
| **phase5.md:19** | "resolution will require persisting it" | Stale — corrected by this ADR. Resolution requires persisting *rio's own lookups*, not the wire field. |
| **`r[sched.ca.resolve]`** | "junction table populated by `wopRegisterDrvOutput`" | Stale — needs `tracey bump`. Populated by scheduler during resolve, not by gateway from wire. |

## Consequences

- **Positive:** Gateway stays thin. Resolution is a scheduler concern anyway — this aligns with ADR-007 (scheduler owns PG).
- **Positive:** No coupling to a deprecated wire format. Upstream Nix won't break us by changing a field that no longer exists.
- **Positive:** rio's `realisation_deps` is purely derived from rio's own `realisations` table — coherence is trivial (single source of truth).
- **Negative:** `realisation_deps` population is delayed until resolve-time. A Realisation registered via `wopRegisterDrvOutput` but never used as a dependency input has no entry. This is fine — the junction table's purpose is resolve-time lookup acceleration, not audit.
- **Negative:** If Nix reintroduces a wire-level dependency format, we'll need a separate pass to reconcile. Low risk — upstream explicitly moved away from this.

---

## Appendix A — Wire Format (Captured)

### `wopRegisterDrvOutput` (opcode 42) request payload

One standard Nix wire string: `[u64 LE len][bytes][pad-to-8]`. Content is JSON produced by `nlohmann::json(Realisation).dump()` (`src/libstore/common-protocol.cc:61-65`).

rio-gateway requires protocol ≥ 1.37 (`rio-nix/src/protocol/handshake.rs:27`), so the pre-1.31 format (`DrvOutput` + `StorePath` separately, `daemon.cc:960-963`) is never seen.

**JSON shape** (`src/libstore/realisation.cc:99-123`):

```json
{
  "id": "sha256:<64-hex>!<output_name>",
  "outPath": "<store-path-basename>",
  "signatures": ["<name>:<base64-sig>", ...],
  "dependentRealisations": {}
}
```

- `id`: `DrvOutput::to_string()` — `drvHash.to_string(Base16, true) + "!" + outputName`. The `sha256:` prefix is literal (`realisation.hh:44`). Regex per Nix's own schema (`build-trace-entry-v2.yaml:51`): `^sha256:[0-9a-f]{64}![a-zA-Z_][a-zA-Z0-9_-]*$`
- `outPath`: `StorePath` serialization — **basename only**, no `/nix/store/` prefix. Already handled at [`opcodes_read.rs:459-473`](../../../rio-gateway/src/handler/opcodes_read.rs).
- `signatures`: `std::set<Signature>` as JSON array. May be absent (older clients) — `optionalValueAt` (`realisation.cc:92`).
- `dependentRealisations`: **Always `{}`** from current Nix. Always present (back-compat). Always ignored on read.

**Hex dump** — see [`rio-gateway/tests/golden/corpus/ca-register-2deep.bin`](../../../rio-gateway/tests/golden/corpus/README.md) offset table. First frame decoded:

```
00000000: b000 0000 0000 0000 7b22 6465 7065 6e64  ........{"depend
00000010: 656e 7452 6561 6c69 7361 7469 6f6e 7322  entRealisations"
00000020: 3a7b 7d2c 2269 6422 3a22 7368 6132 3536  :{},"id":"sha256
00000030: 3a31 3565 3363 3536 3038 3934 6362 6232  :15e3c560894cbb2
00000040: 3730 3835 6366 3635 6235 6132 6563 6231  7085cf65b5a2ecb1
00000050: 3834 3838 6339 3939 3439 3766 3435 3331  8488c999497f4531
00000060: 6236 3930 3761 3735 3831 6365 3664 3532  b6907a7581ce6d52
00000070: 3721 6261 7a22 2c22 6f75 7450 6174 6822  7!baz","outPath"
00000080: 3a22 6731 7737 6879 3371 6731 7737 6879  :"g1w7hy3qg1w7hy
00000090: 3371 6731 7737 6879 3371 6731 7737 6879  3qg1w7hy3qg1w7hy
000000a0: 3371 2d66 6f6f 222c 2273 6967 6e61 7475  3q-foo","signatu
000000b0: 7265 7322 3a5b 5d7d                      res":[]}
```

### `wopQueryRealisation` (opcode 43) request payload

One standard Nix wire string containing `DrvOutput::to_string()` — same `sha256:<hex>!<name>` format as the `id` field above. See [`ca-query-2deep.bin`](../../../rio-gateway/tests/golden/corpus/README.md).

### `wopQueryRealisation` response payload

`std::set<Realisation>` serialization: `[u64 count][per-entry: u64 len + json + pad]`. Count is 0 (miss) or 1 (found) — Nix's store returns at most one per `(drv_hash, output_name)` key (`daemon.cc:983-986`). rio-gateway already correct at [`opcodes_read.rs:589-596`](../../../rio-gateway/src/handler/opcodes_read.rs).

### Historical `dependentRealisations` shape (defensive parsing only)

From Nix's read-only back-compat fixture (`src/libstore-tests/data/realisation/with-dependent-realisations.json`):

```json
{
  "dependentRealisations": {
    "sha256:6f869f9ea2823bda165e06076fd0de4366dead2c0e8d2dbbad277d4f15c373f5!quux": "g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo"
  },
  "id": "sha256:15e3c560894cbb27085cf65b5a2ecb18488c999497f4531b6907a7581ce6d527!baz",
  "outPath": "g1w7hy3qg1w7hy3qg1w7hy3qg1w7hy3q-foo",
  "signatures": []
}
```

- **Flat** map, not nested. `{DrvOutput_string: StorePath_basename}`.
- Key format: identical to `id`.
- Value format: identical to `outPath` (basename).
- Historical cardinality: 1 entry per CA input-derivation output that was resolved for this build.

rio-gateway's current `serde_json::Value` parse already tolerates arbitrary shapes here — no action needed.

---

## Appendix B — Resolved vs Unresolved Derivation (`tryResolve`)

From `src/libstore/derivations.cc:1215-1239` in the locked Nix source.

### Unresolved `Derivation`

Full ATerm with `inputDrvs` populated. Conceptual shape (not literal ATerm — `inputDrvs` is a map in memory; ATerm serializes it as the second positional argument to `Derive`):

```
inputDrvs = { "/nix/store/xxx-dep.drv" -> {"out"} }
inputSrcs = { "/nix/store/yyy-fixed-input" }
env = { "DEP" = "/1ril1qzj2i7wz3kfxrf11mbqh5qyvpa6c8v7p5jfnpgkxg0hbzb8" }
                 ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
                 DownstreamPlaceholder — "/" + nixbase32(sha256("nix-upstream-output:<hashPart>:<outputPathName>"))
```

Placeholder construction (`src/libstore/downstream-placeholder.cc:7-21`):

- Clear-text: `"nix-upstream-output:" + drvPath.hashPart() + ":" + outputPathName(drvName, outputName)` where `outputPathName` is `name` for `out`, `name + "-" + outputName` otherwise
- Hash: SHA-256 of clear-text
- Render: `"/" + hash.to_string(Nix32, false)` — a 53-char string starting with `/`

### Resolved `BasicDerivation`

`BasicDerivation resolved{*this}` — slice-copy drops `inputDrvs` (it's a `Derivation`-only field).

```
inputSrcs = { "/nix/store/yyy-fixed-input", "/nix/store/zzz-resolved-dep-out" }
env = { "DEP" = "/nix/store/zzz-resolved-dep-out" }
```

Transformation (`tryResolveInput`, `derivations.cc:1170-1213`):

1. Per `(inputDrv, outputNames)`: for each output name, call `queryResolutionChain(drvPath, outputName)` — this bottoms out in a `realisations` table lookup (`store-api.cc resolveDerivedPath`).
2. Resolved path → insert into `resolved.inputSrcs` (`derivations.cc:1197`).
3. `inputRewrites` map: `placeholder.render()` → `store.printStorePath(actualPath)` (`derivations.cc:1195`).
4. After all inputs: `rewriteDerivation(store, resolved, inputRewrites)` (`derivations.cc:1236`) — string-replace every placeholder occurrence throughout `env`, `args`, `builder`.

Whether to resolve (`Derivation::shouldResolve`, `derivations.cc:1125-1156`):

| Derivation type | Resolve? |
|---|---|
| Input-addressed | Only if deferred (`ia.deferred`) |
| Content-addressed, fixed | Only if `ca-derivations` experimental feature enabled — optional optimization |
| Content-addressed, floating | **Always** (`ca.fixed ? ... : true`) |
| Impure | Always |
| Any type with dynamic-derivation inputs (`childMap` non-empty) | Always |

For rio's purposes (P0253): `is_ca && !is_fixed_output` is the gate.

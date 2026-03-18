# Plan 0284: Dashboard doc sweep + r[dash.*] markers + phase5 closeout

**USER A9: dashboard gets a first-class spec doc with `r[dash.*]` markers.** [`docs/src/components/dashboard.md`](../../docs/src/components/dashboard.md) exists but is preliminary. This plan rewrites it to match the shipped architecture (Envoy per A1, Svelte per A7) and seeds 4 normative markers covering the killer journey + degrade threshold + streaming + Envoy translation.

Also absorbs the **phase5 core closeout** responsibilities that would have been a separate plan: remove `introduction.md:50` warning (SAFETY GATE gate), retag `cgroup.rs:301`, mark phase5.md `[x]`.

## Entry criteria

- [P0278](plan-0278-dashboard-build-list-drawer.md), [P0279](plan-0279-dashboard-streaming-log-viewer.md), [P0280](plan-0280-dashboard-dag-viz-xyflow.md), [P0281](plan-0281-dashboard-management-actions.md), [P0283](plan-0283-dashboard-vm-smoke-curl.md) merged (dashboard feature-complete)
- [P0254](plan-0254-ca-metrics-vm-demo.md), [P0255](plan-0255-quota-reject-submitbuild.md), [P0256](plan-0256-per-tenant-signing-output-hash.md), [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md), [P0261](plan-0261-governor-lru-eviction.md), [P0263](plan-0263-worker-client-side-chunker.md), [P0267](plan-0267-atomic-multi-output-tx.md), [P0268](plan-0268-chaos-harness-toxiproxy.md), [P0269](plan-0269-fuse-is-file-guard.md), [P0270](plan-0270-buildstatus-critpath-workers.md), [P0271](plan-0271-cursor-pagination-admin-builds.md), [P0272](plan-0272-per-tenant-narinfo-filter.md) merged (core feature-complete)

## Tasks

### T1 — `docs:` rewrite dashboard.md + seed 4 r[dash.*] markers

MODIFY [`docs/src/components/dashboard.md`](../../docs/src/components/dashboard.md) — replace preliminary content with shipped architecture. Seed markers (standalone paragraphs, blank line before, col 0):

```markdown
r[dash.envoy.grpc-web-translate]

The dashboard pod's Envoy sidecar translates gRPC-Web (HTTP/1.1 POST from browser fetch) to gRPC over HTTP/2 with mTLS client cert presented to the scheduler. The scheduler is never aware of gRPC-Web — it sees a normal mTLS client. CORS preflight and the `grpc-web` filter are Envoy-side.

r[dash.journey.build-to-logs]

The killer journey: click build (Builds page) → DAG renders (Graph page) → click running node (DrvNode) → log stream renders (LogViewer). The nginx→Envoy→scheduler chain MUST support server-streaming end-to-end (verified by the 0x80 trailer-frame byte in curl).

r[dash.graph.degrade-threshold]

Graph rendering MUST degrade to a sortable table when the node count exceeds 2000. dagre layout on >2000 nodes freezes the main thread. Above 500 nodes, dagre runs in a Web Worker. The server separately caps responses at 5000 nodes (`GetBuildGraphResponse.truncated`).

r[dash.stream.log-tail]

`GetBuildLogs` server-stream consumption MUST use `TextDecoder('utf-8', {fatal: false})` — build output can contain non-UTF-8 bytes (compiler locale garbage). Lossy decode to `U+FFFD`, never throw. nginx `proxy_buffering off` is required or the stream buffers entirely before reaching the browser.
```

Rewrite architecture section: Envoy sidecar (not tonic-web), Svelte 5 (not React), `@xyflow/svelte`, port-forward only (no Ingress).

### T2 — `docs:` phase5.md dashboard scope correction

MODIFY [`docs/src/phases/phase5.md`](../../docs/src/phases/phase5.md) `:32` — **strike** "Worker utilization graphs, cache hit rate analytics" (Grafana P0222 owns those). Replace with "Interactive DAG visualization, live log streaming, management actions (drain/poison/GC)". Mark `:30-34` bullets `[x]`.

### T3 — `docs:` crate-structure.md note

MODIFY [`docs/src/crate-structure.md`](../../docs/src/crate-structure.md) — note `rio-dashboard/` is a sibling, built by `nix/dashboard.nix` not Cargo, Svelte 5.

### T4 — `docs:` remove introduction.md:50 warning (THE SAFETY GATE)

MODIFY [`docs/src/introduction.md`](../../docs/src/introduction.md) — **remove the `:50` warning** ("Multi-tenant deployments with untrusted tenants are unsafe before Phase 5").

**Precondition (per GT16 + R16):** [P0255](plan-0255-quota-reject-submitbuild.md) + [P0256](plan-0256-per-tenant-signing-output-hash.md) + [P0272](plan-0272-per-tenant-narinfo-filter.md) merged AND their VM tests green in the last `.#ci` run. Not just "dag says DONE" — check the actual CI artifact.

### T5 — `docs:` residual deferral-block sweep

```bash
grep -rn 'Phase 5 deferral' docs/src/
```

Expected closures: `data-flows.md:58` (P0254), `integration.md:19` (P0260), `multi-tenancy.md:19,64,82` (P0259,P0256,P0255), `security.md:69` (P0272). Each implementing plan should have closed its own — this catches stragglers.

IF [P0264](plan-0264-findmissingchunks-tenant-scope.md) was cut: `store.md` gets `> Phase 6 deferral:` block for tenant-scoped chunks.

### T6 — `refactor(worker):` cgroup.rs:301 retag

MODIFY [`rio-worker/src/cgroup.rs`](../../rio-worker/src/cgroup.rs) at `:301` — retag `TODO(phase5)` → `TODO(adr-012)` per A9 (device-plugin track, NOT absorbed by phase5).

### T7 — `docs:` phase5.md all [x]

```bash
sed -i 's/- \[ \]/- [x]/g' docs/src/phases/phase5.md
grep '\[ \]' docs/src/phases/phase5.md  # → empty
```

### T8 — `test:` TODO audit + tracey validate

```bash
rg 'TODO[^(]' rio-dashboard/ nix/dashboard.nix  # untagged → tag or resolve
rg 'TODO\(phase5\)' rio-*/src/                   # → empty (only adr-012 retag survives)
nix develop -c tracey query validate             # 0 errors
nix develop -c tracey query status               # 14 core + 4 dash markers resolved
```

## Exit criteria

- `/nbr .#ci` green (includes `tracey-validate`)
- `rg 'unsafe before Phase 5' docs/src/` → 0 (THE milestone)
- `rg 'TODO\(phase5\)' rio-*/src/` → 0 (only `adr-012` retag survives)
- `rg 'Phase 5 deferral' docs/src/` → 0 (or only Phase-6-deferred if P0264 cut)
- `nix develop -c tracey query validate` → 0 errors
- 4 `r[dash.*]` markers all show `impl` + `verify` in `tracey query status`

## Tracey

**Seeds 4 domain markers** (T1 — dashboard.md):

| Marker | impl plan | verify plan |
|---|---|---|
| `r[dash.envoy.grpc-web-translate]` | [P0273](plan-0273-envoy-sidecar-grpc-web.md) | [P0273](plan-0273-envoy-sidecar-grpc-web.md) T4 curl |
| `r[dash.journey.build-to-logs]` | [P0278](plan-0278-dashboard-build-list-drawer.md)+[P0279](plan-0279-dashboard-streaming-log-viewer.md)+[P0280](plan-0280-dashboard-dag-viz-xyflow.md) | [P0283](plan-0283-dashboard-vm-smoke-curl.md) VM |
| `r[dash.graph.degrade-threshold]` | [P0280](plan-0280-dashboard-dag-viz-xyflow.md) T2 | [P0280](plan-0280-dashboard-dag-viz-xyflow.md) T7 |
| `r[dash.stream.log-tail]` | [P0279](plan-0279-dashboard-streaming-log-viewer.md) T1 | [P0279](plan-0279-dashboard-streaming-log-viewer.md) T4 |

**Note:** these markers are seeded by THIS plan but referenced by already-merged plans P0273-P0283. The `// r[impl ...]` annotations in those plans point to markers that don't exist yet — `tracey validate` will show dangling refs until this plan merges. That's the expected ordering for a closeout plan that seeds markers for already-shipped code. Acceptable because P0284 is the LAST plan to merge.

## Files

```json files
[
  {"path": "docs/src/components/dashboard.md", "action": "MODIFY", "note": "T1: rewrite + seed 4 r[dash.*] markers (USER A9)"},
  {"path": "docs/src/phases/phase5.md", "action": "MODIFY", "note": "T2: strike Grafana-owned, mark [x]; T7: all [x]"},
  {"path": "docs/src/crate-structure.md", "action": "MODIFY", "note": "T3: rio-dashboard sibling note"},
  {"path": "docs/src/introduction.md", "action": "MODIFY", "note": "T4: REMOVE :50 warning (SAFETY GATE — precondition P0255+P0256+P0272 green)"},
  {"path": "docs/src/data-flows.md", "action": "MODIFY", "note": "T5: verify :58 closed by P0254"},
  {"path": "docs/src/integration.md", "action": "MODIFY", "note": "T5: verify :19 closed by P0260"},
  {"path": "docs/src/multi-tenancy.md", "action": "MODIFY", "note": "T5: verify :19/:64/:82 closed"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T5: verify :69 closed by P0272"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "T6: retag :301 phase5→adr-012"}
]
```

```
docs/src/
├── components/dashboard.md       # T1: rewrite + 4 r[dash.*] markers
├── phases/phase5.md              # T2+T7
├── crate-structure.md            # T3
├── introduction.md               # T4: REMOVE WARNING
└── ...                           # T5: deferral sweep
rio-worker/src/cgroup.rs          # T6: retag
```

## Dependencies

```json deps
{"deps": [278, 279, 280, 281, 283, 254, 255, 256, 260, 261, 263, 267, 268, 269, 270, 271, 272], "soft_deps": [264, 266], "note": "PHASE CLOSEOUT — LAST plan. Deps on all leaf-tips (transitive covers P0245-P0283). R16: intro.md:50 removal PRECONDITION = P0255+P0256+P0272 VM tests green in last .#ci (not just dag DONE). P0264/P0266 soft — if cut, add Phase 6 deferral blocks."}
```

**Depends on:** ALL leaf-tips. Dashboard: P0278-P0281, P0283. Core: P0254, P0255, P0256, P0260, P0261, P0263, P0267, P0268, P0269, P0270, P0271, P0272. Transitive closure covers P0245-P0283.
**Conflicts with:** none — LAST plan, everything merged.

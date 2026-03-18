# Plan 0245: Prologue — phase5.md corrections + 14 marker seeds + GT-verify

Phase 5's `phase5.md` was written against an imagined codebase. Ground-truth reconciliation at `6b5d4f4` (see `.claude/notes/phase5-partition.md` §1, 16 GT rows) found: the "per-edge cutoff logic" at `:10` does not exist (it's `find_newly_ready()` dependency-unblocking with zero CA awareness); `dependentRealisations` is discarded at [`opcodes_read.rs:418`](../../rio-gateway/src/handler/opcodes_read.rs); `proto.md:252` claims JWT propagation in the present tense while `multi-tenancy.md:19` says "completely unimplemented."

This plan is the frontier root — pure docs, no code-file collision. It corrects phase5.md, seeds 14 domain markers into component specs (definitions only — implementing plans add `r[impl]`/`r[verify]`), adds spec caveats, and runs the GT13 multi-output-atomicity verify task whose outcome gates [P0267](plan-0267-atomic-multi-output-tx.md)'s scope.

**USER DECISIONS applied:** Per Q1 reframe, `sched.preempt.oom-migrate` is NOT seeded — proactive-ema ([P0265](plan-0265-worker-resource-usage-emit.md)/[P0266](plan-0266-scheduler-ema-update-midbuild.md)) does no kill and needs no preemption marker. `r[sched.preempt.never-running]` stands unbumped. 14 markers, not 15.

## Entry criteria

- [P0244](plan-0244-doc-sync-sweep-phase-x.md) merged (4c closeout — `admin/builds.rs:22` retagged to phase5, all 4c deferral blocks closed)

## Tasks

### T1 — `docs:` correct phase5.md GT1/GT4

MODIFY [`docs/src/phases/phase5.md`](../../docs/src/phases/phase5.md) `:10`:

> Before: "connected to the scheduler's per-edge cutoff logic"
> After: "connects to the scheduler's `find_newly_ready()` dependency-unblocking loop; Phase 5 **adds** a hash-comparison branch (no pre-existing CA cutoff infrastructure exists — `rg cutoff rio-scheduler/src/` returns only size-class duration routing)."

Add GT4 note near the same section:

> `has_ca_floating_outputs()` exists at [`rio-nix/src/derivation/mod.rs:222`](../../rio-nix/src/derivation/mod.rs) — detection is plumbing, not parsing.

### T2 — `docs:` seed 14 domain markers

Add standalone `r[...]` paragraphs to component specs (blank line before, col 0, definition text only — no `r[impl]`/`r[verify]` yet). Each implementing plan will annotate code.

**[`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md)** — 4 markers (near the existing `r[sched.preempt.never-running]` at `:246` for the CA family, or near `:185` sched.classify for conceptual adjacency):

```markdown
r[sched.ca.detect]

The scheduler MUST distinguish content-addressed derivations from input-addressed at DAG merge time. The `is_ca` flag is set from `has_ca_floating_outputs() || is_fixed_output()` at gateway translate, propagated via proto `DerivationInfo.is_content_addressed`, persisted on `DerivationState`.

r[sched.ca.cutoff-compare]

When a CA derivation completes successfully, the scheduler MUST compare the output `nar_hash` against the content index. A match means the output is byte-identical to a prior build — downstream builds depending only on this output can be skipped.

r[sched.ca.cutoff-propagate]

On hash match, the scheduler MUST transition downstream derivations whose only incomplete dependency was the matched CA output from `Queued` to `Skipped` without running them. The transition cascades recursively (depth-capped at 1000). Running derivations are NEVER killed — cutoff applies to `Queued` only (see `r[sched.preempt.never-running]`).

r[sched.ca.resolve]

When a CA derivation's inputs are themselves CA (CA-depends-on-CA), the scheduler MUST rewrite `inputDrvs` placeholder paths to realized store paths before dispatch. Resolution queries the `realisation_deps` junction table populated by `wopRegisterDrvOutput`.
```

**[`docs/src/components/store.md`](../../docs/src/components/store.md)** — 6 markers:

```markdown
r[store.gc.tenant-quota-enforce]

(Sibling to `r[store.gc.tenant-quota]`.) The gateway MUST reject `SubmitBuild` with `STDERR_ERROR` when `tenant_store_bytes(tenant_id)` exceeds `tenants.gc_max_store_bytes`. Enforcement is eventually-consistent — `tenant_store_bytes` may be cached with ≤30s TTL. The connection stays open; the user can retry after GC.

r[store.tenant.sign-key]

narinfo signing MUST use the tenant's active signing key from `tenant_keys` when present, falling back to the cluster key otherwise. A tenant with its own key produces narinfo that `nix store verify --trusted-public-keys tenant:<pk>` accepts for that tenant's paths only.

r[store.tenant.narinfo-filter]

Authenticated narinfo requests MUST filter results by `path_tenants.tenant_id = auth.tenant_id`. Anonymous (unauthenticated) requests return unfiltered results for backward compatibility.

r[store.chunk.put-standalone]

(Sibling to `r[store.chunk.refcount-txn]`.) The `PutChunk` RPC MUST accept chunks independent of any NAR manifest. A chunk with no manifest reference is held for the grace-TTL before GC eligibility.

r[store.chunk.grace-ttl]

(Sibling to `r[store.chunk.refcount-txn]`.) Chunks with zero manifest references AND `created_at < now() - grace_seconds` are GC-eligible. The grace period prevents a race where a worker's `PutChunk` arrives before its `PutPath` manifest.

r[store.atomic.multi-output]

Multi-output derivation registration MUST be atomic at the DB level: all output rows commit in one transaction, or none do. Blob-store writes are NOT rolled back (orphaned blobs are refcount-zero and GC-eligible on the next sweep). The bound is ≤1 NAR-size per failure.
```

**[`docs/src/components/gateway.md`](../../docs/src/components/gateway.md)** — 4 markers (near `r[gw.auth.tenant-from-key-comment]` at `:481`):

```markdown
r[gw.jwt.claims]

JWT claims: `sub` = tenant_id UUID (server-resolved at mint time), `iat`, `exp` (SSH session duration + grace), `jti` (unique token ID for revocation). Signed ed25519, public key distributed via ConfigMap.

r[gw.jwt.issue]

On successful SSH authentication, the gateway MUST mint a JWT with `sub` set to the resolved tenant UUID and store it on the session context. The gateway forwards `jti` in `SubmitBuildRequest.jwt_jti` so the scheduler can check revocation.

r[gw.jwt.verify]

The tonic interceptor on scheduler, store, and controller MUST extract `x-rio-tenant-token`, verify signature+expiry, attach `Claims` to request extensions, and reject invalid tokens with `Status::unauthenticated`. The scheduler ADDITIONALLY checks `jti NOT IN jwt_revoked` (PG lookup — gateway stays PG-free).

r[gw.jwt.dual-mode]

(Does NOT bump `r[gw.auth.tenant-from-key-comment]`.) Gateway auth is two-branched PERMANENTLY: `x-rio-tenant-token` header present → JWT verify; absent → SSH-comment fallback. Operator chooses per-deployment via `gateway.toml auth_mode`. Both paths stay maintained.
```

### T2b — `docs:` seed 4 r[dash.*] markers (pulled from P0284)

**Pulled forward from [P0284](plan-0284-dashboard-docs-sweep-markers.md)** — 6 dashboard plans (P0273, P0277-P0280, P0283) reference `r[dash.*]` markers in `r[verify]` annotations. Without this, `.#ci` tracey-validate fails on their merges until P0284 lands (which deps on 17 plans and merges last).

NEW `docs/src/components/dashboard.md`:

```markdown
# rio-dashboard

> **Phase 5:** Web dashboard for operational visibility. Svelte 5 SPA,
> Envoy-sidecar gRPC-Web translation, DAG visualization via @xyflow/svelte.

## Architecture

See `infra/helm/rio-build/templates/dashboard-*.yaml`.

## Normative requirements

r[dash.envoy.grpc-web-translate]

The dashboard pod's Envoy sidecar translates gRPC-Web (HTTP/1.1 POST from browser fetch) to gRPC over HTTP/2 with mTLS client cert presented to the scheduler. The scheduler is never aware of gRPC-Web — it sees a normal mTLS client. CORS preflight and the `grpc-web` filter are Envoy-side.

r[dash.journey.build-to-logs]

The killer journey: click build (Builds page) → DAG renders (Graph page) → click running node (DrvNode) → log stream renders (LogViewer). The nginx→Envoy→scheduler chain MUST support server-streaming end-to-end (verified by the 0x80 trailer-frame byte in curl).

r[dash.graph.degrade-threshold]

Graph rendering MUST degrade to a sortable table when the node count exceeds 2000. dagre layout on >2000 nodes freezes the main thread. Above 500 nodes, dagre runs in a Web Worker. The server separately caps responses at 5000 nodes (`GetBuildGraphResponse.truncated`).

r[dash.stream.log-tail]

`GetBuildLogs` server-stream consumption MUST use `TextDecoder('utf-8', {fatal: false})` — build output can contain non-UTF-8 bytes (compiler locale garbage). Lossy decode to `U+FFFD`, never throw. nginx `proxy_buffering off` is required or the stream buffers entirely before reaching the browser.
```

Run `nix develop -c tracey query validate` — MUST show 0 errors, 18 new uncovered expected (14 core + 4 dash).

### T3 — `docs:` GT15 fix — proto.md present-tense caveat

MODIFY [`docs/src/components/proto.md`](../../docs/src/components/proto.md) near `:252`:

```markdown
> **Phase 5 deferral:** JWT propagation via `x-rio-tenant-token` is aspirational until P0259 lands. At HEAD, `tenant_id` is an empty string in all gRPC metadata.
```

[P0259](plan-0259-jwt-verify-middleware.md) removes this caveat when it makes the claim true.

### T4 — `test:` GT13 verify — multi-output atomicity

**Scope-gating task for [P0267](plan-0267-atomic-multi-output-tx.md).** Construct a 2-output derivation, inject a fault between output-1 and output-2 `PutPath`, check for orphan rows. Write outcome to a comment in `.claude/notes/phase5-partition.md` §1:

```markdown
<!-- GT13-OUTCOME: real -->   # OR: false-alarm (single-statement atomic)
```

If `false-alarm`: P0267 becomes tracey-annotate-only (4c P0226 pattern). If `real`: P0267 wraps in `sqlx::Transaction`.

Approach: unit test in `rio-store` with `rio-test-support` ephemeral PG. Mock the blob-store `put` to fail on the second call. Assert zero rows in `paths` table after — if a row survives, the bug is real.

### T5 — `docs:` TODO(phase5) audit

```bash
rg -n 'TODO\(phase5\)' -- 'rio-*/src/**/*.rs'
```

Confirm the 4 known at `6b5d4f4`:
- [`opcodes_read.rs:493`](../../rio-gateway/src/handler/opcodes_read.rs) → absorbed by [P0256](plan-0256-per-tenant-signing-output-hash.md) (GT5: signing concern, not CA-cutoff)
- [`chunk.rs:62`](../../rio-store/src/grpc/chunk.rs) → [P0262](plan-0262-putchunk-impl-grace-ttl.md)
- [`fuse/ops.rs:361`](../../rio-worker/src/fuse/ops.rs) → [P0269](plan-0269-fuse-is-file-guard.md)
- [`cgroup.rs:301`](../../rio-worker/src/cgroup.rs) → ADR-012 track per A9, NOT absorbed; core-phase closeout retags

Capture any NEW `TODO(phase5)` from P0207/P0213 (4b) that landed since `6b5d4f4`. Each must map to a phase-5 plan or get a followup row.

### T6 — `docs:` re-verify disk state at dispatch

The partition note was written against `6b5d4f4`. At dispatch, re-run:

```bash
rg -n 'governor|DefaultKeyedRateLimiter' rio-gateway/src/        # GT11 — should NOW exist post-P0213
ls migrations/ | sort -V | tail -1                               # expect 013; P0249 starts at 014
rg -n 'r\[gw.rate' docs/src/components/gateway.md                # check if P0213 touched marker
git log --oneline -5 -- rio-scheduler/src/actor/completion.rs    # P0228 anchor for CA spine
```

## Exit criteria

- `/nbr .#ci` green
- `rg 'per-edge cutoff' docs/src/phases/phase5.md` → 0 matches
- `nix develop -c tracey query uncovered | wc -l` ≥ baseline + 18 (14 core + 4 dash)
- `nix develop -c tracey query validate` → 0 errors
- GT13 outcome recorded in `.claude/notes/phase5-partition.md` §1
- T5 audit produces a table mapping every `TODO(phase5)` to a plan number

## Tracey

**Seeds 18 domain markers** (14 core + 4 dash, definitions only — zero `r[impl]`/`r[verify]`):

| Marker | File | Implementing plan |
|---|---|---|
| `r[sched.ca.detect]` | scheduler.md | [P0250](plan-0250-ca-detect-plumb-is-ca.md) |
| `r[sched.ca.cutoff-compare]` | scheduler.md | [P0251](plan-0251-ca-cutoff-compare.md) |
| `r[sched.ca.cutoff-propagate]` | scheduler.md | [P0252](plan-0252-ca-cutoff-propagate-skipped.md) + P0254 VM verify |
| `r[sched.ca.resolve]` | scheduler.md | [P0253](plan-0253-ca-resolution-dependentrealisations.md) |
| `r[store.gc.tenant-quota-enforce]` | store.md | [P0255](plan-0255-quota-reject-submitbuild.md) |
| `r[store.tenant.sign-key]` | store.md | [P0256](plan-0256-per-tenant-signing-output-hash.md) |
| `r[store.tenant.narinfo-filter]` | store.md | [P0272](plan-0272-per-tenant-narinfo-filter.md) |
| `r[store.chunk.put-standalone]` | store.md | [P0262](plan-0262-putchunk-impl-grace-ttl.md) |
| `r[store.chunk.grace-ttl]` | store.md | [P0262](plan-0262-putchunk-impl-grace-ttl.md) |
| `r[store.atomic.multi-output]` | store.md | [P0267](plan-0267-atomic-multi-output-tx.md) |
| `r[gw.jwt.claims]` | gateway.md | [P0257](plan-0257-jwt-lib-claims-sign-verify.md) |
| `r[gw.jwt.issue]` | gateway.md | [P0258](plan-0258-jwt-issuance-gateway.md) |
| `r[gw.jwt.verify]` | gateway.md | [P0259](plan-0259-jwt-verify-middleware.md) |
| `r[gw.jwt.dual-mode]` | gateway.md | [P0260](plan-0260-jwt-dual-mode-k8s-sighup.md) |

**NOT seeded** (per USER Q1 reframe): no `sched.preempt.oom-migrate`. `r[sched.preempt.never-running]` stands unbumped.

## Files

```json files
[
  {"path": "docs/src/phases/phase5.md", "action": "MODIFY", "note": "T1: GT1+GT4 corrections at :10"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T2: seed 4 sched.ca.* markers near :246"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T2: seed 6 store.* markers"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T2: seed 4 gw.jwt.* markers near :481"},
  {"path": "docs/src/components/dashboard.md", "action": "NEW", "note": "T2b: seed 4 r[dash.*] markers (pulled from P0284 — 6 plans reference these)"},
  {"path": "docs/src/components/proto.md", "action": "MODIFY", "note": "T3: GT15 deferral caveat at :252"},
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "T4: GT13 verify test ONLY — no production code touch (or tests/ file)"},
  {"path": ".claude/notes/phase5-partition.md", "action": "MODIFY", "note": "T4: write GT13-OUTCOME comment"}
]
```

```
docs/src/
├── phases/phase5.md              # T1: GT1/GT4 corrections
├── components/
│   ├── scheduler.md              # T2: 4 sched.ca.* markers
│   ├── store.md                  # T2: 6 store.* markers
│   ├── gateway.md                # T2: 4 gw.jwt.* markers
│   ├── dashboard.md              # T2b: 4 r[dash.*] markers (NEW)
│   └── proto.md                  # T3: GT15 caveat
.claude/notes/phase5-partition.md # T4: GT13-OUTCOME
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "FRONTIER ROOT. Docs-only — no code-file collision with 4b/4c work. Was deps=[244] (wait for 4c close) but that blocked P0273 which needs dash markers seeded here. Dispatch alongside 4b/4c."}
```

**Depends on:** [P0244](plan-0244-doc-sync-sweep-phase-x.md) — 4c closeout ensures `admin/builds.rs:22` retagged, all 4c deferral blocks closed, disk state matches partition-note GT assumptions.
**Conflicts with:** none. Docs-only. No `collisions.jsonl` entries for these paths at high count.

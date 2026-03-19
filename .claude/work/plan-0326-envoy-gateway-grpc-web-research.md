# Plan 0326: RESEARCH — Envoy Gateway gRPC-Web support (P0273 scope resolution)

**Research plan. No code ships.** [P0273](plan-0273-envoy-sidecar-grpc-web.md) was scope-changed at [`plan-0273-envoy-sidecar-grpc-web.md:5-16`](plan-0273-envoy-sidecar-grpc-web.md) on 2026-03-18 (Audit C #25): Envoy Gateway via nixhelm chart supersedes the hand-configured sidecar. The scope-change block ends with "Implementer: rewrite tasks after verifying Gateway API gRPC-Web support path" — but that verification never happened. P0273's implementer at worktree [`p273` @ `25173c72`](https://github.com/search?q=25173c72&type=commits) built the sidecar per the original tasks. Good reference code (Envoy v3 API modernization, scheduler-port-9001-not-8443 fix), but potentially the wrong architecture.

**The open question** (verbatim from P0273:13): does Envoy Gateway support gRPC-Web natively via `GRPCRoute`, or does it need the `EnvoyPatchPolicy` escape hatch? The answer determines whether Gateway API is simpler-than-sidecar (native support → just write CRDs) or more-complex (escape hatch → you're hand-writing envoy config ANYWAY, now with a Kubernetes abstraction layer on top).

**Blocks [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) + [P0283](plan-0283-dashboard-vm-smoke-curl.md).** Both depend on P0273 for the gRPC-Web translation layer. Until P0273's architecture is decided, neither can dispatch against a stable interface.

**Spec impact:** [`r[dash.envoy.grpc-web-translate]`](../../docs/src/components/dashboard.md) at `dashboard.md:26` explicitly says "sidecar". If this research concludes Gateway API, that marker text is wrong and needs `tracey bump` — the architectural fact it asserts changes.

## Tasks

### T1 — `docs(research):` investigate Envoy Gateway gRPC-Web support surface

**Web research. No code.** Answer these questions with cited primary sources (Envoy Gateway docs, GitHub issues, CRD schemas):

1. **Does `GRPCRoute` (Gateway API standard) handle gRPC-Web?** `GRPCRoute` is for native gRPC (HTTP/2). gRPC-Web is HTTP/1.1 POST with a custom wire framing. Check: does Envoy Gateway's `GRPCRoute` implementation include the `envoy.filters.http.grpc_web` filter, or does it expect native gRPC only?

   Primary sources to check:
   - [Envoy Gateway `GRPCRoute` docs](https://gateway.envoyproxy.io/) — search for "grpc-web"
   - [Gateway API `GRPCRoute` spec](https://gateway-api.sigs.k8s.io/api-types/grpcroute/) — does the upstream spec mention gRPC-Web at all?
   - Envoy Gateway GitHub — search issues for "grpc-web" + "GRPCRoute"

2. **If not native: does `EnvoyPatchPolicy` or `BackendTrafficPolicy` support injecting the grpc_web filter?** `EnvoyPatchPolicy` is the JSONPatch escape hatch — you can inject arbitrary xDS. `BackendTrafficPolicy` is the structured extension point. Check which (if either) can add `envoy.filters.http.grpc_web` to the filter chain.

   Primary sources:
   - [`EnvoyPatchPolicy` docs](https://gateway.envoyproxy.io/latest/api/extension_types/#envoypatchpolicy) — is HTTP filter injection a supported operation?
   - [`ClientTrafficPolicy`](https://gateway.envoyproxy.io/latest/api/extension_types/#clienttrafficpolicy) — gRPC-Web is a CLIENT-facing concern (browser → gateway), so this may be the right extension point
   - [`EnvoyExtensionPolicy`](https://gateway.envoyproxy.io/latest/api/extension_types/#envoyextensionpolicy) — external processing / WASM filters

3. **CORS handling.** The sidecar config at P0273:63-67 configures CORS (allow_origin, allow_methods, allow_headers, expose_headers with grpc-status/grpc-message). Does Envoy Gateway's `SecurityPolicy` or `HTTPRoute` CORS support expose grpc-specific headers, or does it need the escape hatch too?

4. **mTLS upstream to scheduler.** The sidecar config at P0273:75-80 sets up mTLS client cert presentation to `rio-scheduler-svc:8443`. `BackendTLSPolicy` (Gateway API standard) handles upstream TLS — does it support client-cert presentation (mTLS), or TLS-verify-only?

5. **nixhelm chart availability.** P0273:7 says "`charts.envoyproxy.gateway` or similar — verify at dispatch". Check:
   - Does [nixhelm](https://github.com/farcaller/nixhelm) actually carry the Envoy Gateway chart?
   - If not, what's the flake-input path? (envoyproxy publishes a Helm chart; `helm2nix` or manual `fetchHelm` are fallbacks)

**Deliverable:** a decision matrix section (see T3) with cited answers to all 5 questions.

### T2 — `docs(research):` decision — Gateway API or keep sidecar

Based on T1 findings, write the decision. Three possible outcomes:

| Outcome | Condition | P0273 rewrite scope |
|---|---|---|
| **A: Gateway API native** | gRPC-Web supported via `GRPCRoute` or first-class policy (no EnvoyPatchPolicy) | Full rewrite — tasks become CRD authoring (Gateway, GRPCRoute, SecurityPolicy, BackendTLSPolicy). ~4-5 new T-items. Sidecar code at `25173c72` becomes reference-only. |
| **B: Gateway API + escape hatch** | gRPC-Web needs `EnvoyPatchPolicy` to inject the filter | Hybrid — Gateway + HTTPRoute for routing/CORS/mTLS, EnvoyPatchPolicy for grpc_web filter injection. Complexity ≈ sidecar (you're writing envoy xDS either way) + Kubernetes CRD layer. Only worth it if nixhelm-pinning wins on drift. |
| **C: Keep sidecar** | Escape hatch is fragile (JSONPatch against generated xDS names) OR nixhelm chart doesn't exist OR BackendTLSPolicy doesn't do client-cert mTLS | Strike the Audit C scope-change block. P0273 tasks stand as-is. `25173c72` is the implementation. |

**Bias toward honesty about complexity.** Outcome B is the trap: it LOOKS like "using the platform" but you end up writing MORE config (CRDs + xDS patches) than the sidecar's single `envoy-dashboard.yaml`. If T1 finds gRPC-Web needs EnvoyPatchPolicy AND the patch targets are generated names (like `default/eg/http`), outcome C is probably correct — the sidecar's static config is more maintainable than a JSONPatch against a moving target.

### T3 — `docs:` write decision + rewrite P0273 tasks (or strike scope-change)

MODIFY [`.claude/work/plan-0273-envoy-sidecar-grpc-web.md`](plan-0273-envoy-sidecar-grpc-web.md):

**Add a `## Research resolution (P0326)` section** below the Audit C scope-change block at `:16`, containing:

- T1's decision matrix (5 questions × cited answers)
- T2's outcome (A/B/C) with one-paragraph rationale
- If outcome A or B: the rewritten `## Tasks` section (replace the sidecar T1-T4 with Gateway API CRDs)
- If outcome C: a `STRUCK — sidecar stands` note at the top of the Audit C block, and a rationale paragraph explaining why Gateway API was evaluated and rejected

**If outcome A or B** — also MODIFY [`docs/src/components/dashboard.md`](../../docs/src/components/dashboard.md) at `:26-28`:

The current marker text:

> r[dash.envoy.grpc-web-translate]
>
> The dashboard pod's Envoy sidecar translates gRPC-Web (HTTP/1.1 POST from browser fetch) to gRPC over HTTP/2 with mTLS client cert presented to the scheduler. ...

Becomes (outcome A example — adjust for actual findings):

> r[dash.envoy.grpc-web-translate]
>
> Envoy Gateway (deployed via nixhelm chart) translates gRPC-Web (HTTP/1.1 POST from browser fetch) to gRPC over HTTP/2 with mTLS client cert presented to the scheduler via `BackendTLSPolicy`. The scheduler is never aware of gRPC-Web — it sees a normal mTLS client. The `GRPCRoute` CRD configures routing; `SecurityPolicy` configures CORS with grpc-status/grpc-message expose-headers.

Then run `tracey bump` — the marker's architectural assertion changed, so existing `// r[impl dash.envoy.grpc-web-translate]` annotations (if any) are stale until reviewed. Check `tracey query rule dash.envoy.grpc-web-translate` at dispatch to see if any impl annotations exist yet (probably zero — P0273 hasn't merged).

**If outcome C** — no `dashboard.md` edit. The marker says "sidecar" and sidecar is correct.

### T4 — `docs:` update P0273 dag.jsonl row (if outcome A or B)

If T3 rewrites P0273's tasks, the `## Files` fence changes (Gateway API CRDs in `infra/helm/` instead of `infra/helm/rio-build/files/envoy-dashboard.yaml`), and the `tracey_total`/`tracey_covered` counts change. Update the P0273 row via:

```bash
.claude/bin/onibus dag set-field 273 tracey_total <new-count>
.claude/bin/onibus dag set-field 273 crate "<new-crates-csv>"
```

Check at dispatch whether `onibus dag` has a `set-field` verb or if it's `onibus dag append` with override semantics. If neither, edit `dag.jsonl` directly (find the `"plan": 273` row, update fields, preserve JSONL format).

**If outcome C** — no dag.jsonl change. P0273's row is already correct for the sidecar tasks.

## Exit criteria

- T1: all 5 research questions answered with primary-source citations (URLs that resolve, not "I think the docs say...")
- T2: outcome A/B/C decided with one-paragraph rationale written in the P0273 doc
- T3: `grep -A5 'Research resolution' .claude/work/plan-0273-envoy-sidecar-grpc-web.md` → decision matrix + outcome present
- **If outcome A or B:** `grep 'GRPCRoute\|BackendTLSPolicy\|EnvoyPatchPolicy' .claude/work/plan-0273-envoy-sidecar-grpc-web.md` → ≥3 hits in the rewritten Tasks section (CRD names appear)
- **If outcome A or B:** `nix develop -c tracey query rule dash.envoy.grpc-web-translate` → marker version bumped (e.g. `+2`), spec text mentions Gateway API not sidecar
- **If outcome C:** `grep 'STRUCK.*sidecar stands' .claude/work/plan-0273-envoy-sidecar-grpc-web.md` → 1 hit at the top of the Audit C block; `git diff sprint-1 -- docs/src/components/dashboard.md` → empty (no marker change)
- Coordinator can dispatch P0273 against a stable task list — no more "rewrite tasks after verifying" blockers in the doc
- **No code compiles or tests run.** This plan's exit is a DECISION DOCUMENT. The curl gate (0x80 trailer-frame byte) that P0273:13 mentions is P0273's exit criterion, not this plan's.

## Tracey

References existing marker (conditionally bumped):
- `r[dash.envoy.grpc-web-translate]` at [`dashboard.md:26`](../../docs/src/components/dashboard.md) — T3 bumps if outcome A or B (architectural assertion "sidecar" → "Envoy Gateway")

No new markers. This plan is research + doc-rewrite; the spec marker already exists and describes the correct contract (gRPC-Web → gRPC+mTLS translation). Only the MECHANISM (sidecar vs Gateway API) changes, and that's an implementation detail the marker TEXT happens to mention.

## Files

```json files
[
  {"path": ".claude/work/plan-0273-envoy-sidecar-grpc-web.md", "action": "MODIFY", "note": "T3: add Research resolution section (decision matrix + outcome). If outcome A/B: rewrite Tasks section for Gateway API CRDs. If outcome C: add STRUCK note to Audit C block."},
  {"path": "docs/src/components/dashboard.md", "action": "MODIFY", "note": "T3 CONDITIONAL (outcome A/B only): update r[dash.envoy.grpc-web-translate] text sidecar→Gateway API, then tracey bump. Outcome C: no edit."},
  {"path": ".claude/dag.jsonl", "action": "MODIFY", "note": "T4 CONDITIONAL (outcome A/B only): update P0273 row tracey_total + crate fields for new task list. Outcome C: no edit."}
]
```

```
.claude/work/
└── plan-0273-envoy-sidecar-grpc-web.md  # T3: decision + rewritten tasks (or STRUCK)
docs/src/components/
└── dashboard.md                          # T3 (A/B only): marker text update + bump
.claude/dag.jsonl                         # T4 (A/B only): P0273 row update
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [273], "note": "RESEARCH plan — no entry criteria. Web research + doc rewrite, no code. discovered_from: 273 (the scope-change block IS the followup). REVERSE-blocks: P0273 cannot re-dispatch until this resolves (coordinator should hold P0273 worktree at 25173c72 as reference, not dispatch it). P0277+P0283 transitively blocked via P0273. Soft-dep P0273: if the coordinator wants to dispatch this researcher WITH read access to the p273 worktree (25173c72 sidecar code as reference for Envoy v3 API patterns), that's helpful but not required — the research questions are answerable from public Envoy Gateway docs alone. Priority: 50 — blocks two plans but is pure research (cheap, fast, no CI gate)."}
```

**Depends on:** none. Research is self-contained — Envoy Gateway docs are public, nixhelm is a public flake, Gateway API spec is public.

**Blocks (reverse):** [P0273](plan-0273-envoy-sidecar-grpc-web.md) — cannot re-dispatch until outcome decided. Transitively blocks [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) + [P0283](plan-0283-dashboard-vm-smoke-curl.md).

**Conflicts with:** [`plan-0273-envoy-sidecar-grpc-web.md`](plan-0273-envoy-sidecar-grpc-web.md) — no other UNIMPL plan edits it. [`dashboard.md`](../../docs/src/components/dashboard.md) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T5 references `r[dash.graph.degrade-threshold]` at `:34` in prose (doesn't edit the file). No collision. The worktree `p273` at `25173c72` should be KEPT as reference (Envoy v3 API, scheduler-port-9001 fix) — do NOT `git worktree remove` it; the research may conclude the sidecar code is correct (outcome C), in which case it's the implementation.

# Phase-5 Dashboard Partition — FINAL Plan (P0273–P0284)

---

# USER DECISIONS (2026-03-18) — OVERRIDE SYNTHESIS DEFAULTS BELOW

| Item | Synthesis default | **USER DECISION** | Impact on plans |
|---|---|---|---|
| **A1** | tonic-web THIRD plaintext port (Rust, scheduler code) | **Envoy as DASHBOARD's sidecar** | **P0273 complete rewrite.** No Rust. No tonic-web dep. Scheduler COMPLETELY untouched. P0273 becomes: Envoy config YAML + mTLS client cert gen + sidecar container in dashboard Pod spec. Dashboard pod = nginx (static) + envoy (gRPC-Web→gRPC, presents mTLS cert to scheduler-svc). R5 (`.accept_http1`) **disappears** — Envoy handles HTTP/1.1 natively. R3 (streaming) moves to known-good territory — Envoy gRPC-Web streaming is battle-tested. |
| **A2** | PG-backed thin GraphNode | **Confirmed** — no GraphQL | Unchanged. One API surface (gRPC AdminService). If R3 fails at P0273, GraphQL subscriptions over WebSocket is a FALLBACK, not a pre-build. |
| **A3** | Committed gen + drift check | **buf CLI first, buildNpmPackage fallback** | P0275 reshaped: T0 tries `buf generate` (buf IS in nixpkgs; check if bundled plugins cover connect-es without network). If buf works in-sandbox → done. If buf needs remote plugins (network) → T1 packages `@connectrpc/protoc-gen-connect-es` via buildNpmPackage with vendored binary. Committed-gen is LAST resort, not expected. |
| **A6** | port-forward only | **Confirmed** | Unchanged. Matches Grafana. |
| **A7** | React 18 | **Svelte 5 + @xyflow/svelte** | **Every TS plan reshapes.** P0274: SvelteKit (or vite+svelte) not Vite+React. P0277-P0281: `.svelte` components not `.tsx`. @xyflow/svelte API is similar to @xyflow/react but Svelte idioms (stores not hooks). R6 (500+ node freeze) is LESS of a risk — Svelte's compile-time reactivity avoids VDOM reconciliation bottleneck. Bundle ~3KB runtime vs ~45KB React+ReactDOM. |
| **A9** | Zero `r[ui.*]` markers | **Dashboard spec doc with `r[dash.*]`** | NEW: `docs/src/components/dashboard.md` with `r[dash.*]` markers. `_lib.py` `_DOMAIN_MARKER_RE` adds `dash` prefix. P0284 (doc sweep) seeds the markers. Treats dashboard as a first-class component like scheduler/worker. Markers cover the killer journey (build→DAG→logs) as normative requirements, not per-component styling. |
| **Open Q** | P0276 dispatch timing | **Dispatch immediately, advisory-serialize** | `deps=[]`. EOF-append to types.proto is textually parallel-safe. P0280 (DAG viz) hard-deps on P0276 regardless — landing P0276 early unblocks the differentiator. |

## Architecture shift: Envoy sidecar (A1)

**Before (synthesis):** scheduler grows a third listener with tonic-web.

**After (user decision):** scheduler is unchanged. The dashboard pod is:

> **A1/A6 interaction note (clarified post-decision):** with port-forward-only (A6),
> the "browsers can't mTLS" problem doesn't actually bite — port-forward tunnels
> past any Service-level TLS, and the health-port precedent proves the scheduler
> CAN serve plaintext alongside mTLS. tonic-web-on-a-plaintext-port would have
> been simpler infra. **Envoy is kept anyway for future-proofing**: scheduler's
> surface area stays minimal, and if Ingress ever happens (A6 revisited later),
> the translation layer is already in place. Present complexity is the price of
> not touching the scheduler.

```
┌─────────────────────────────── rio-dashboard Pod ─────────────────────────────┐
│  ┌─────────────────┐      ┌─────────────────────────────────────────────┐  │
│  │ nginx           │      │ envoy                                       │  │
│  │ :80 static SPA  │      │ :8080 gRPC-Web listener (HTTP/1.1)          │  │
│  │ /api/* → :8080  │─────▶│   grpc_web filter → router → upstream       │  │
│  └─────────────────┘      │   cluster: scheduler-svc (mTLS client cert) │  │
│                           └─────────────────────┬───────────────────────┘  │
└─────────────────────────────────────────────────┼──────────────────────────┘
                                                  │ gRPC/mTLS (HTTP/2)
                                                  ▼
                                     ┌────────────────────┐
                                     │ rio-scheduler-svc  │  (UNCHANGED)
                                     │ :8443 mTLS         │
                                     └────────────────────┘
```

P0273 tasks become:
- Envoy bootstrap config (`infra/helm/rio-build/files/envoy-dashboard.yaml`): grpc_web filter + CORS + mTLS upstream cluster
- mTLS client cert: reuse the existing scheduler CA? Or cert-manager? Check how rio-cli/rio-controller already present certs to scheduler.
- Dashboard Pod spec: +envoy sidecar container, cert Secret volume
- P0273's curl gate still applies: `curl -X POST localhost:8080/rio.admin.AdminService/GetBuildLogs ... | xxd | grep '^00000000: 80'` (0x80 = gRPC-Web trailer frame)
- `nix/docker-pulled.nix`: +envoy image (for VM tests)

## Framework shift: Svelte 5 (A7)

Every code sample in P0274-P0281 changes. Key translations:

| React idiom | Svelte 5 equivalent |
|---|---|
| `useState` | `$state()` rune |
| `useEffect` | `$effect()` rune |
| `useSyncExternalStore` (log stream) | Custom store + `$state` wrapper |
| `.tsx` | `.svelte` (with `<script lang="ts">`) |
| `@xyflow/react` | `@xyflow/svelte` |
| `React.memo` + `nodeTypes` const | Svelte compiles this away — less R6 mitigation needed |

P0274's scaffold: `pnpm create vite rio-dashboard --template svelte-ts` (or SvelteKit if routing is wanted — check if SPA-mode SvelteKit adds value over plain Vite).

## r[dash.*] domain (A9)

`docs/src/components/dashboard.md` — NEW. Example markers:
- `r[dash.journey.build-to-logs]` — click build → DAG shows → click node → log stream renders (the killer journey)
- `r[dash.graph.degrade-threshold]` — >2000 nodes → degrade to table (R6 mitigation is normative)
- `r[dash.envoy.grpc-web-translate]` — sidecar translates gRPC-Web → gRPC+mTLS (the A1 architecture is normative)
- `r[dash.stream.log-tail]` — GetBuildLogs server-stream renders as virtual-scroll append

`_lib.py` `_DOMAIN_MARKER_RE` updated to include `dash` prefix.

---


## Assumptions & open questions (confirm before dispatch)

| # | Assumption | Evidence | Risk if wrong |
|---|---|---|---|
| A1 | **tonic-web on a THIRD plaintext port** (not layered on main port). Browsers cannot present mTLS client certs; `main.rs:595` loads mandatory server TLS. | `rio-scheduler/src/main.rs:595-646` applies `ServerTlsConfig`; health-plaintext precedent at `:604-624` proves pattern. | If someone insists on single-port: blocked entirely. No config toggle makes browsers do mTLS-to-tonic. |
| A2 | **`GetBuildGraph` is PG-backed, NOT actor-snapshot.** Schema confirmed at `migrations/001_scheduler.sql:39-100` (`derivations` / `derivation_edges` / `build_derivations` / `assignments`). Actor retention of proto `DerivationNode[]` post-merge is unverified and carries `drv_content` (≤64KB/node). | `grep DerivationNode rio-scheduler/src/actor/` shows usage in merge.rs only; no evidence of retention. PG schema is certain. | Actor approach: "retain graph" refactor blows out scope; thin message avoids 128MB responses. |
| A3 | **Proto TS codegen is COMMITTED** (`rio-dashboard/src/gen/*.ts` in git), regenerated by a script, drift-checked in CI. In-sandbox codegen is attempted in P0275 but the fallback is the expected path. | `nix/tracey.nix:53-56` proves `fetchPnpmDeps --ignore-scripts` blocks postinstall binary fetches. `@bufbuild/protoc-gen-es` ships a native binary. `protoc-gen-connect-es` is NOT in nixpkgs. | If in-sandbox works: bonus. If committed-gen rejected: P0275 becomes a nixpkgs-packaging detour (days). |
| A4 | **P0276 serializes behind P0231** on `types.proto` (count=29). P0231 is UNIMPL with dep chain `[230, 205, 214]` → `[229, 204]`. **Minimum 5 merges before P0276 can touch types.proto.** | `.claude/dag.jsonl`: P0231 `status=UNIMPL deps=[230,205,214]`; all deps UNIMPL. | If P0245-P0272 (phase5 core, unplanned) also touch types.proto, P0276's real dep extends. Run `collisions-regen` at dispatch. |
| A5 | **`AdminServiceImpl` needs `#[derive(Clone)]` added.** Struct at `admin/mod.rs:52-74` has no derive today. All fields (`Arc<LogBuffers>`, `Option<(S3Client,String)>`, `PgPool`, `ActorHandle`, `Instant`, …) are cheaply `Clone`. | `grep Clone admin/mod.rs` → no match on struct. `main.rs:661` moves it into `AdminServiceServer::new()`. | Mechanical — unless a field added between now and P0273 is `!Clone`. Verify at dispatch. |
| A6 | **No `kind: Ingress` in MVP.** Chart has zero Ingress resources today (`ls templates/` confirms). Access via `kubectl port-forward svc/rio-dashboard`. Adding Ingress means choosing controller class, TLS source, hostname — all environment-specific. | `infra/helm/rio-build/templates/` contains no Ingress. Grafana (P0222) uses same port-forward model. | If operator demands Ingress: followup plan, ~1 day, but needs environment decisions outside this partition's scope. |
| A7 | **React 18, not Preact.** tracey uses Preact (`nix/tracey.nix:7`) but `@xyflow/react` is React-first; Preact compat requires aliasing with known hook-timing issues. | `@xyflow/react` docs; DAG viz is the differentiator, don't fight the library. | Preact saves ~30KB gzipped. Not worth the risk for the one component that matters. |
| A8 | **`pnpmDeps` hash bumps on every dep-adding plan.** P0274, P0275, P0277, P0278, P0279, P0280 each change `package.json` → each does one `lib.fakeHash` → `/nbr` → paste-real-hash cycle. | `nix/tracey.nix:44` — single FOD hash for entire offline store. | Per-plan tax (~2min each). Documented in each plan's task list, not eliminated. |
| A9 | **Zero `r[ui.*]` tracey markers.** UI presentation is not protocol-normative. One possible exception: if P0276's `GetBuildGraph` gets spec text in `docs/src/components/scheduler.md`, it gets `r[sched.admin.build-graph]` — but that's `sched.*` not `ui.*`. | Constraints section. `tracey query status` should show zero `ui.*` at partition close. | — |

**Open question for user:** Should P0276 (GetBuildGraph RPC) block on P0231's 5-plan dep chain, or dispatch immediately with a git-rebase-at-merge-time expectation? EOF-append to `types.proto` is textually parallel-safe (both append distinct messages), but the `admin.proto` RPC line is also at a fixed position. **Recommendation: dispatch immediately (`deps=[]`), document as advisory-serialize, merge-queue resolves.** P0280 (DAG viz UI) hard-deps on P0276 regardless.

---

## Risk ranking — what kills this partition if discovered late

| # | Risk | Kills | De-risked by | Wave |
|---|---|---|---|---|
| **R1** | mTLS on scheduler main port blocks browsers entirely | Everything | P0273 spawns a THIRD listener (plaintext, http1, GrpcWebLayer, AdminService-only) cloning the proven health-port pattern at `main.rs:604-624` | 0a |
| **R2** | `@bufbuild/protoc-gen-es` postinstall fetches native binary → `--ignore-scripts` blocks it → in-sandbox codegen impossible | P0277–P0281 (all TS feature plans) | P0275 is a two-exit spike: try in-sandbox, fall back to committed-gen-with-drift-check (the expected path per A3) | 0b |
| **R3** | gRPC-Web server-streaming through fetch+tonic-web+k8s Service unproven — logs + GC progress both dead if broken | 2 of 3 product differentiators | P0273's exit gate includes a curl streaming test against `GetBuildLogs` asserting the `0x80` trailer-frame byte — proves streaming BEFORE any TypeScript | 0a |
| **R4** | `types.proto` at count=29 — P0276 serializes behind P0231's 5-plan UNIMPL chain; P0245-P0272 (unplanned) may add more touchers | DAG viz blocked indefinitely | P0276 uses EOF-append + DB-backed impl (no actor dep) → can dispatch early with advisory-serialize; P0280 (UI) can land with transport-mocked vitest independently | Documented |
| **R5** | No `.accept_http1(true)` on tonic-web builder → **curl succeeds (h2c-capable) but every browser fetch fails with `PROTOCOL_ERROR`** | Everything, silently — P0273 curl passes, P0277 browser doesn't | P0273 task list explicitly includes `.accept_http1(true)` + `CorsLayer`; P0277 is the first browser-verification point with explicit "if P0273 curl passed but this fails, check R5" note | 0a + 1 |
| **R6** | `@xyflow/react` freezes browser at ~500 nodes with naive per-node React component (inline `nodeTypes`) | DAG viz product value | P0280 mandates `nodeTypes` memoized OUTSIDE component + explicit `>2000`-node degrade-to-table; dagre layout in Web Worker for `>500` nodes | 2 |
| **R7** | `GetBuildGraphResponse` reusing `DerivationNode` → `drv_content` (≤64KB/node) × 2000 nodes = 128MB → exceeds `max_encoding_message_size` | DAG viz | P0276 defines **NEW thin `GraphNode`** (5 strings, ~200 bytes) — does NOT reuse the fat submit-path message | 0a |
| **R8** | Non-UTF-8 bytes in `BuildLogChunk.lines` crash naive `TextDecoder` | Log viewer | P0279 mandates `new TextDecoder('utf-8', {fatal: false})`; vitest with `Uint8Array.of(0xff, 0xfe)` → assert `\ufffd`, no throw | 2 |
| **R9** | Playwright-in-Nix = ~500MB chromium FOD + font-config sandbox issues + version-lock brittleness; no precedent in `nix/tests/` | E2E gate blocks merge forever on infra | P0283 uses **curl-only smoke** — asserts `<div id="root">` in HTML + `0x00` gRPC-Web DATA frame byte through nginx proxy. Playwright is a followup, not a gate | 3 |
| **R10** | `pnpmDeps` hash churn — if two Wave-2 plans merge same window, second gets FOD hash conflict | Throughput | Documented per-plan tax (A8); merge-queue naturally serializes; trivial resolution (`/nbr` → paste hash) | Per-plan |

---

## The killer user journey (what justifies this partition)

> *"Build `7f3a` has been running 40 minutes, what's happening?"*
> → Click build (P0278) → see DAG, 47 green nodes, 1 yellow (P0280) → click yellow node → log tail streams (P0279), "oh, compiling LLVM" → done in 30 seconds.

That journey touches three features in sequence. The spine P0277→P0278→P0280→P0279 is optimized to get *that one path* working end-to-end before layering write-actions.

**Explicit non-scope (Grafana P0222 owns these — `phase5.md:32` is struck):**
- Worker CPU time-series — Prometheus
- Cache hit rate trending — Prometheus

---

## Wave 0a — Foundation spikes (PARALLEL — disjoint hot files)

### P0273 — `feat(scheduler):` tonic-web dedicated port — THE GATEKEEPER

**De-risks R1, R3, R5.** If this plan's exit gate fails, the entire partition is dead. Dispatch first, verify hardest.

**Why a third port, not a layer on the main port:** `rio-scheduler/src/main.rs:595` loads mTLS via `load_server_tls`; `:645-646` applies it unconditionally when present. Browsers cannot present client certs to a tonic mTLS endpoint. The ONLY path is a separate plaintext listener. The repo already proves this pattern at `main.rs:604-624` (plaintext health port sharing `HealthReporter` with the mTLS port via `Clone`). We clone that pattern, swapping health service → `GrpcWebLayer`-wrapped `AdminServiceServer`.

**Leader-election interplay (load-bearing comment):** Both scheduler replicas expose `:9201`. The grpc-web listener uses the same `serve_shutdown` child token → shuts down on step-down. The K8s Service only routes to the leader because standby pods fail readiness (shared `HealthReporter` toggle, `main.rs:587-594`). **P0273 MUST add a comment documenting this** so a future reader doesn't "fix" it by making grpc-web listen unconditionally.

**Tasks:**
- **T1** `feat(deps):` Add `tonic-web = { workspace = true }` and `tower-http = { workspace = true, features = ["cors"] }` to `/root/src/rio-build/main/rio-scheduler/Cargo.toml`. Add both to `/root/src/rio-build/main/Cargo.toml` `[workspace.dependencies]` (check if `tower-http` already present for other crates — grep first).
- **T2** `feat(scheduler):` `/root/src/rio-build/main/rio-scheduler/src/admin/mod.rs:52` — add `#[derive(Clone)]` to `AdminServiceImpl`. Verify all fields are `Clone` (A5: `Arc<LogBuffers>`, `Option<(S3Client,String)>`, `PgPool`, `ActorHandle`, `Instant` — all cheap-Clone). If any field added since 6b5d4f4 is `!Clone`, wrap the whole struct in `Arc` instead.
- **T3** `feat(scheduler):` `/root/src/rio-build/main/rio-scheduler/src/main.rs` `Config` struct (near `:24-82`): add `grpc_web_addr: std::net::SocketAddr`, default `"0.0.0.0:9201".parse().unwrap()` (follows `health_addr` pattern). Extend `config_defaults_are_stable` test (near `:689`).
- **T4** `feat(scheduler):` After `main.rs:624` (health-plaintext spawn), BEFORE `main.rs:644` (main builder):
  ```rust
  // gRPC-Web listener: plaintext HTTP/1.1, AdminService ONLY.
  // Browsers can't present mTLS client certs, so this port serves the
  // dashboard. Same AdminServiceImpl instance (Clone via T2) — reads
  // see the same actor_tx/pool/log_buffers as the mTLS port.
  //
  // Leader-election: uses serve_shutdown (child token), so this
  // listener stops on step-down. K8s Service only routes to the
  // leader because standby fails readiness (shared HealthReporter
  // toggle above). If you make this listen unconditionally, the
  // dashboard will split-brain between leader and standby.
  let grpc_web_addr = cfg.grpc_web_addr;
  let admin_service_web = admin_service.clone();
  let grpc_web_shutdown = serve_shutdown.clone();
  info!(addr = %grpc_web_addr, "spawning gRPC-Web listener (AdminService, plaintext)");
  rio_common::task::spawn_monitored("grpc-web", async move {
      // CorsLayer::permissive — TODO(phase5): origin allowlist once
      // dashboard Ingress has a stable hostname. Permissive is OK for
      // MVP: NetworkPolicy restricts :9201 to cluster-internal.
      // grpc-status/grpc-message MUST be in expose_headers or
      // connect-web can't read error trailers.
      let cors = tower_http::cors::CorsLayer::permissive()
          .expose_headers([
              http::header::HeaderName::from_static("grpc-status"),
              http::header::HeaderName::from_static("grpc-message"),
              http::header::HeaderName::from_static("grpc-status-details-bin"),
          ]);
      if let Err(e) = tonic::transport::Server::builder()
          .accept_http1(true)              // R5: browsers use HTTP/1.1 fetch, NOT h2c
          .layer(cors)                     // R5: preflight OPTIONS
          .layer(tonic_web::GrpcWebLayer::new())
          .add_service(AdminServiceServer::new(admin_service_web)
              .max_decoding_message_size(max_message_size)
              .max_encoding_message_size(max_message_size))
          .serve_with_shutdown(grpc_web_addr, grpc_web_shutdown.cancelled_owned())
          .await
      { tracing::error!(error = %e, "grpc-web server failed"); }
  });
  ```
- **T5** `feat(helm):` `/root/src/rio-build/main/infra/helm/rio-build/templates/scheduler.yaml` — add `containerPort: 9201` named `grpc-web` to the container ports list + add port to the Service. ~4 lines total.
- **T6** `feat(helm):` `/root/src/rio-build/main/infra/helm/rio-build/templates/networkpolicy.yaml` — near the `rio-scheduler-ingress` rule, add port 9201 with `from: [{namespaceSelector: {}}]` (cluster-internal). `# TODO(P0282): tighten to dashboard pod selector` comment.
- **T7** `feat(nixos):` `/root/src/rio-build/main/nix/modules/scheduler.nix` — add `RIO_GRPC_WEB_ADDR` env (pattern-match existing `RIO_LISTEN_ADDR`). Default `0.0.0.0:9201`.
- **T8** `test(vm):` In `/root/src/rio-build/main/nix/tests/scenarios/cli.nix` (or whichever standalone-fixture scenario is lightest), add TWO curl assertions:
  ```python
  # R1 de-risk: gRPC-Web framing works (unary ClusterStatus)
  # 5-byte frame header: [compression=0x00][length=0x00000000] = empty proto message
  machine.succeed(
      "curl -sf -X POST http://scheduler:9201/rio.admin.AdminService/ClusterStatus "
      "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
      r"--data-binary $'\x00\x00\x00\x00\x00' "
      "| xxd | head -1 | grep -q '^00000000: 00'"  # response DATA frame, compression=0
  )
  # R3 de-risk: server-streaming works through tonic-web BEFORE any TS exists.
  # GetBuildLogs for nonexistent build → scheduler streams is_complete=true
  # immediately. gRPC-Web streaming MUST end with a trailers frame whose
  # frame-type byte has MSB set (0x80). If tonic-web's streaming is broken,
  # this grep fails.
  machine.succeed(
      "printf '\\x00\\x00\\x00\\x00\\x0a\\x0a\\x08nonexist' > /tmp/req.bin && "  # GetBuildLogsRequest{build_id:"nonexist"}
      "curl -sf -X POST http://scheduler:9201/rio.admin.AdminService/GetBuildLogs "
      "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
      "--data-binary @/tmp/req.bin | xxd | tail -5 | grep -q ' 80'"  # trailer frame marker
  )
  ```

**Exit gate:** `/nbr .#ci` green. Both curl assertions pass. The `0x80` grep is the single most important assertion in Wave 0 — it proves server-streaming works end-to-end before a single line of TypeScript.

**Files:**
| Path | Action | Collision | Note |
|---|---|---|---|
| `Cargo.toml` | MODIFY | low (workspace deps EOF) | T1 |
| `rio-scheduler/Cargo.toml` | MODIFY | low | T1 |
| `rio-scheduler/src/admin/mod.rs` | MODIFY | low | T2 — one-line `#[derive(Clone)]` at `:52` |
| `rio-scheduler/src/main.rs` | MODIFY | **count=27 HOT** | T3+T4 — config field near `:46`, spawn block near `:625`. Disjoint from P0227/P0230 (size_classes region ~:699) |
| `infra/helm/rio-build/templates/scheduler.yaml` | MODIFY | count=4 | T5 |
| `infra/helm/rio-build/templates/networkpolicy.yaml` | MODIFY | low | T6 |
| `nix/modules/scheduler.nix` | MODIFY | low | T7 |
| `nix/tests/scenarios/cli.nix` | MODIFY | low | T8 |

**Deps:** `[]` (Wave 0 root). **Parallel with P0274, P0276** — P0273 touches `main.rs`/helm, P0274 touches `flake.nix`/`rio-dashboard/`, P0276 touches proto/db. Zero file overlap.

---

### P0274 — `feat(dashboard):` pnpm/Vite/React scaffold + `nix/dashboard.nix` — THE BUILD-CHAIN PROOF

**De-risks:** proves `fetchPnpmDeps` + `vite build` produce a non-empty `dist/` inside the Nix sandbox BEFORE any feature code. Clones `nix/tracey.nix:32-68` with one critical deviation: we run `tsc --noEmit` in buildPhase (tracey skipped it due to `@typescript/native-preview` postinstall trap at `:53-56` — we use stock `typescript`, no postinstall).

**Tasks:**
- **T1** `feat(dashboard):` Create `/root/src/rio-build/main/rio-dashboard/`:
  - `package.json` — **audit every dep for postinstall scripts before committing lockfile.** Runtime deps: `react@18`, `react-dom@18`, `react-router-dom@6`, `@connectrpc/connect`, `@connectrpc/connect-web`, `@bufbuild/protobuf`. Dev deps: `typescript@5` (NOT `@typescript/native-preview`), `vite@6`, `@vitejs/plugin-react`, `vitest@2`, `@testing-library/react`, `jsdom`, `eslint@9`, `@typescript-eslint/eslint-plugin`, `@typescript-eslint/parser`. Scripts: `build: "tsc --noEmit && vite build"`, `test: "vitest run"`, `lint: "eslint src --max-warnings 0"`, `dev: "vite"`.
  - `pnpm-lock.yaml` — generated via `nix develop -c bash -c 'cd rio-dashboard && pnpm install --lockfile-only'` after T3 adds nodejs/pnpm to devshell.
  - `tsconfig.json` — `strict: true`, `jsx: "react-jsx"`, `moduleResolution: "bundler"`, `target: "ES2022"`, `paths: {"@/*": ["./src/*"]}`.
  - `vite.config.ts` — `plugins: [react()]`, `build.outDir: 'dist'`, `server.proxy: { '/rio.admin.AdminService': { target: 'http://localhost:9201', changeOrigin: true } }` (dev-mode proxy — prod uses `baseUrl: '/'` through nginx, one build artifact for both).
  - `vitest.config.ts` — `environment: 'jsdom'`, `globals: true`.
  - `eslint.config.js` — flat config, `@typescript-eslint/recommended`.
  - `index.html` — `<div id="root"></div>` + `<script type="module" src="/src/main.tsx">`.
  - `src/main.tsx` — `createRoot(document.getElementById('root')!).render(<App />)`.
  - `src/App.tsx` — `export function App() { return <h1>rio-dashboard</h1> }`.
  - `src/App.test.tsx` — `render(<App />); expect(screen.getByText('rio-dashboard')).toBeInTheDocument()`. Proves vitest+jsdom+testing-library wiring.
  - `.gitignore` — `node_modules`, `dist`.
- **T2** `feat(nix):` NEW `/root/src/rio-build/main/nix/dashboard.nix`:
  ```nix
  { pkgs }:
  let
    # Repo root — P0275 will need rio-proto/proto/ visible for codegen.
    # cleanSource, NOT crane's cleanCargoSource (drops .ts/.json/.html).
    src = pkgs.lib.cleanSource ../.;
    dashboardRoot = "rio-dashboard";
  in
  pkgs.stdenvNoCC.mkDerivation rec {
    pname = "rio-dashboard";
    version = "0.1.0";
    inherit src;
    sourceRoot = "source/${dashboardRoot}"; # crane convention, tracey.nix:36

    pnpmDeps = pkgs.fetchPnpmDeps {
      inherit pname version src;
      sourceRoot = "source/${dashboardRoot}";
      pnpm = pkgs.pnpm_10;
      fetcherVersion = 3;
      hash = pkgs.lib.fakeHash; # replace after first /nbr
    };

    nativeBuildInputs = with pkgs; [ nodejs pnpm_10 pnpmConfigHook ];

    # Unlike tracey.nix:57-61 (which skips tsc due to @typescript/native-
    # preview's postinstall trap), we use stock `typescript` — no
    # postinstall — so tsc+lint+test run inside the sandbox.
    # The derivation succeeding IS the check.
    buildPhase = ''
      runHook preBuild
      pnpm run lint
      pnpm run test
      pnpm run build   # = tsc --noEmit && vite build
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      cp -r dist $out
      test -f $out/index.html       # vite produced output
      test -n "$(ls $out/assets/*.js 2>/dev/null)"  # at least one bundle
      runHook postInstall
    '';
  }
  ```
- **T3** `feat(flake):` `/root/src/rio-build/main/flake.nix` — THREE edits, all additive at distinct anchors:
  1. Near `:287` (tracey import): `rioDashboard = import ./nix/dashboard.nix { inherit pkgs; };`
  2. In `checks` attrset: `dashboard = rioDashboard;` — derivation succeeding = tsc+eslint+vitest+vite-build all pass.
  3. In `ciParts` list (near `:667`): add `cargoChecks.dashboard` (or however the aggregate references checks — pattern-match `tracey-validate`).
  4. Dev shell `nativeBuildInputs` (near `:844`): add `nodejs`, `pnpm_10`.
- **T4** `.gitignore` — add `rio-dashboard/node_modules/`, `rio-dashboard/dist/`.
- **T5** First `/nbr .#checks.x86_64-linux.dashboard` fails with hash mismatch → paste real hash into `nix/dashboard.nix`. Per memory `tracey-adoption.md`: "bump = ONE fakeHash (pnpmDeps)".

**Exit gate:** `/nbr .#checks.x86_64-linux.dashboard` → `result/index.html` contains `<div id="root">`. `ls result/assets/*.js` non-empty. `/nbr .#ci` green (dashboard now a member).

**Files:**
| Path | Action | Collision | Note |
|---|---|---|---|
| `rio-dashboard/` (11 files) | NEW | zero | T1 — greenfield |
| `nix/dashboard.nix` | NEW | zero | T2 |
| `flake.nix` | MODIFY | **count=24 HOT** | T3 — 4 additive edits at distinct anchors. Disjoint from P0273. |
| `.gitignore` | MODIFY | low | T4 |

**Deps:** `[]`. **Parallel with P0273, P0276.**

---

### P0276 — `feat(proto,scheduler):` GetBuildGraph RPC — PG-BACKED, THIN MESSAGE

**De-risks R4, R7.** DAG viz needs graph structure. **This plan does NOT reuse `DerivationNode`** — that message carries `bytes drv_content` (`types.proto:32`, ≤64KB/node, 16MB DAG budget) and submit-path fields irrelevant to rendering. 2000 nodes × 64KB = 128MB, exceeds `max_encoding_message_size`. All three runner-up plans independently converged on thin new messages; the PG schema at `migrations/001_scheduler.sql:39-100` has everything needed for a 3-table JOIN.

**Why Wave 0 not Wave 2:** Pure Rust, no TS deps, no `flake.nix` touch, no `main.rs` touch. Dispatching early unblocks P0280 and clears the `types.proto` slot before P0245-P0272 (unplanned) potentially add more touchers.

**types.proto serialization (A4):** P0231 is the last planned toucher, UNIMPL with 5-plan dep chain. **Recommendation: dispatch P0276 with `deps=[]` and advisory-serialize** — both P0231 and P0276 are EOF-appends of distinct messages, textually parallel-safe. At merge time, if P0231 landed first, trivial rebase (appends don't conflict). If P0276 lands first, P0231 rebases trivially. **At dispatch, run `git log --oneline -5 -- rio-proto/proto/types.proto` to confirm current tip.**

**Tasks:**
- **T1** `feat(proto):` EOF-append to `/root/src/rio-build/main/rio-proto/proto/types.proto`:
  ```protobuf
  // Dashboard DAG visualization — per-node snapshot.
  // Deliberately THIN: DerivationNode carries drv_content (≤64KB) and
  // submit-path sizing fields. 2000 nodes × 64KB = 128MB > max message
  // size. GraphNode is ~200 bytes — what the dashboard actually needs.
  message GraphNode {
    string drv_path = 1;
    string pname = 2;
    string system = 3;
    // derivations.status CHECK constraint values:
    // created|queued|ready|assigned|running|completed|failed|poisoned|dependency_failed
    string status = 4;
    // Populated from assignments WHERE status IN ('pending','acknowledged')
    // (the active-assignment unique index). Empty if unassigned.
    // Dashboard uses this to link a running (yellow) node → worker detail.
    string assigned_worker_id = 5;
  }
  message GraphEdge {
    string parent_drv_path = 1;
    string child_drv_path = 2;
    bool is_cutoff = 3;  // CA early-cutoff edge (phase5 core wires this)
  }
  message GetBuildGraphRequest {
    string build_id = 1;
  }
  message GetBuildGraphResponse {
    repeated GraphNode nodes = 1;
    repeated GraphEdge edges = 2;
    // Server caps response at DASHBOARD_GRAPH_NODE_LIMIT (default 5000).
    // If truncated, dashboard degrades to table view. Better than a
    // 50MB response crashing the tab on a 50k-node chromium build.
    bool truncated = 3;
    uint32 total_nodes = 4;  // actual count even if truncated
  }
  ```
- **T2** `feat(proto):` `/root/src/rio-build/main/rio-proto/proto/admin.proto` — after `CreateTenant` (`:36`):
  ```protobuf
  // Full derivation DAG for one build with per-node status. Snapshot,
  // not streaming — dashboard polls for live updates (5s interval).
  // Backed by PG (derivations + derivation_edges + build_derivations
  // JOIN), not actor state.
  rpc GetBuildGraph(rio.types.GetBuildGraphRequest) returns (rio.types.GetBuildGraphResponse);
  ```
- **T3** `feat(scheduler):` `/root/src/rio-build/main/rio-scheduler/src/db.rs` — new function at EOF of impl block (no edit to existing queries):
  ```rust
  /// Load the derivation DAG for one build, scoped via build_derivations.
  /// Subgraph projection: the global derivations/derivation_edges tables
  /// span all builds; we filter to nodes this build requested. Edges are
  /// included iff BOTH endpoints are in the build's node set — a build
  /// doesn't "see" edges to derivations only other builds requested.
  ///
  /// Returns (nodes, edges, total_nodes). If total_nodes > limit, nodes
  /// is truncated (edges naturally follow since they filter on node set).
  pub async fn load_build_graph(
      &self,
      build_id: Uuid,
      limit: u32,
  ) -> sqlx::Result<(Vec<GraphNodeRow>, Vec<GraphEdgeRow>, u32)> {
      // Count first (cheap, indexed on build_id)
      let total: i64 = sqlx::query_scalar!(
          "SELECT COUNT(*) FROM build_derivations WHERE build_id = $1",
          build_id
      ).fetch_one(&self.pool).await?.unwrap_or(0);

      // Nodes: derivations JOIN build_derivations LEFT JOIN active assignment.
      // assigned_worker_id comes from derivations.assigned_worker_id (set
      // during dispatch) — assignments table has richer history but for
      // a snapshot the derivations column is authoritative.
      let nodes = sqlx::query_as!(GraphNodeRow, r#"
          SELECT d.drv_path, d.pname, d.system, d.status,
                 COALESCE(d.assigned_worker_id, '') AS "assigned_worker_id!"
          FROM derivations d
          JOIN build_derivations bd ON bd.derivation_id = d.derivation_id
          WHERE bd.build_id = $1
          LIMIT $2
      "#, build_id, limit as i64).fetch_all(&self.pool).await?;

      // Edges: both endpoints must be in this build's node set (subquery
      // filter on build_derivations). If nodes were truncated, edges are
      // naturally truncated too — no orphan edges.
      let edges = sqlx::query_as!(GraphEdgeRow, r#"
          SELECT dp.drv_path AS parent_drv_path,
                 dc.drv_path AS child_drv_path,
                 e.is_cutoff
          FROM derivation_edges e
          JOIN derivations dp ON dp.derivation_id = e.parent_id
          JOIN derivations dc ON dc.derivation_id = e.child_id
          WHERE e.parent_id IN (SELECT derivation_id FROM build_derivations WHERE build_id = $1)
            AND e.child_id  IN (SELECT derivation_id FROM build_derivations WHERE build_id = $1)
      "#, build_id).fetch_all(&self.pool).await?;

      Ok((nodes, edges, total as u32))
  }
  ```
  Define `GraphNodeRow`/`GraphEdgeRow` structs nearby. **Regenerate `.sqlx/` offline query cache:** `nix develop -c cargo sqlx prepare --workspace` — forgetting this fails the offline build.
- **T4** `feat(scheduler):` NEW `/root/src/rio-build/main/rio-scheduler/src/admin/graph.rs`:
  ```rust
  use super::*;
  const DASHBOARD_GRAPH_NODE_LIMIT: u32 = 5000;

  pub(super) async fn get_build_graph(
      pool: &PgPool,
      build_id: &str,
  ) -> Result<GetBuildGraphResponse, Status> {
      let build_uuid = Uuid::parse_str(build_id)
          .map_err(|_| Status::invalid_argument("build_id is not a valid UUID"))?;
      let (nodes, edges, total) = /* db.load_build_graph */
          .await.map_err(|e| Status::internal(format!("db: {e}")))?;
      Ok(GetBuildGraphResponse {
          nodes: nodes.into_iter().map(Into::into).collect(),
          edges: edges.into_iter().map(Into::into).collect(),
          truncated: total > DASHBOARD_GRAPH_NODE_LIMIT,
          total_nodes: total,
      })
  }
  ```
- **T5** `feat(scheduler):` `/root/src/rio-build/main/rio-scheduler/src/admin/mod.rs` — add `mod graph;` + wire `get_build_graph` into the `AdminService` trait impl (near `:339`). **Check: different lines from P0273's `#[derive(Clone)]` at `:52`** — no collision.
- **T6** `test(scheduler):` `/root/src/rio-build/main/rio-scheduler/src/admin/tests.rs` — two tests using `rio-test-support` ephemeral PG:
  - 3 nodes, 2 edges, one build → assert shape, assert `truncated=false`
  - Two builds sharing one derivation → assert each sees only its own subgraph (the `build_derivations` filter)
  - One test with limit=2 on a 3-node build → assert `truncated=true`, `total_nodes=3`
- **T7** `test(proto):` Roundtrip encode/decode `GetBuildGraphResponse` with 2 nodes, 1 edge.
- **T8** (after merge) `scripts/regen-proto-ts.sh` (created by P0275) must be re-run and `rio-dashboard/src/gen/` re-committed. If P0275 hasn't landed yet, this is a no-op; P0275 picks up the new RPC when it generates.

**Exit gate:** `/nbr .#ci` green. `grpcurl -plaintext scheduler:9001 rio.admin.AdminService/GetBuildGraph -d '{"build_id":"<uuid>"}'` returns nodes + edges for a real build.

**Files:**
| Path | Action | Collision | Note |
|---|---|---|---|
| `rio-proto/proto/types.proto` | MODIFY | **count=29 HOTTEST** | T1 EOF-append. Advisory-serialize vs P0231 (also EOF-append, textually parallel-safe). |
| `rio-proto/proto/admin.proto` | MODIFY | count=3 | T2 — one line after `:36` |
| `rio-scheduler/src/db.rs` | MODIFY | count=34 | T3 — new fn at EOF of impl. `.sqlx/` regen required. |
| `rio-scheduler/src/admin/graph.rs` | NEW | zero | T4 |
| `rio-scheduler/src/admin/mod.rs` | MODIFY | low | T5 — `mod` decl + trait wire. Disjoint from P0273's `:52`. |
| `rio-scheduler/src/admin/tests.rs` | MODIFY | low | T6 |
| `.sqlx/` | MODIFY | low (regenerated) | T3 side-effect |

**Deps:** `[]` (advisory-serialize after P0231 — see A4 open question). **Parallel with P0273, P0274.**

---

## Wave 0b — Codegen spike (serialized after P0274)

### P0275 — `feat(dashboard):` Proto TS codegen — TWO-EXIT SPIKE

**De-risks R2.** `@bufbuild/protoc-gen-es` ships a native binary via postinstall; `fetchPnpmDeps` uses `--ignore-scripts` (`nix/tracey.nix:54-56` proves this trap is real). We cannot know until we try whether buf's `local: node_modules/.bin/...` plugin resolution works with the offline pnpm store.

**Exit path A (attempted first):** `buf generate` runs inside the nix buildPhase using `pkgs.buf` + plugins from offline `node_modules/.bin/`. Generated files land in `src/gen/` at build time. Clean, no drift.

**Exit path B (expected per A3):** Generate locally via `scripts/regen-proto-ts.sh`, commit `src/gen/*.ts`, buildPhase runs a drift check. Drift-prone but sandbox-safe. **This is what tracey does** (`nix/tracey.nix:29-31`: "the checked-in file is the source of truth for the npm build").

**Tasks:**
- **T1** `feat(dashboard):` Add to `/root/src/rio-build/main/rio-dashboard/package.json` devDeps: `@bufbuild/buf`, `@bufbuild/protoc-gen-es`, `@connectrpc/protoc-gen-connect-es`. Regenerate `pnpm-lock.yaml`. **Audit postinstalls:** `cd rio-dashboard && pnpm ls --depth=0 --json | jq -r '.[] | .devDependencies | keys[]' | xargs -I{} sh -c 'jq -r ".scripts.postinstall // empty" node_modules/{}/package.json | grep -q . && echo "{}"'` — list any package with a postinstall. If any `@bufbuild/*` or `@connectrpc/*` appears → Exit Path B is mandatory.
- **T2** `feat(dashboard):` NEW `/root/src/rio-build/main/rio-dashboard/buf.gen.yaml`:
  ```yaml
  version: v2
  inputs:
    - directory: ../rio-proto/proto
  plugins:
    - local: node_modules/.bin/protoc-gen-es
      out: src/gen
      opt: [target=ts]
    - local: node_modules/.bin/protoc-gen-connect-es
      out: src/gen
      opt: [target=ts]
  ```
  Hidden check: `buf` needs well-known-types (`google/protobuf/timestamp.proto`, `empty.proto`). `@bufbuild/protobuf` bundles them. If buf can't find them, add `pkgs.protobuf` to `nativeBuildInputs` with explicit `-I`.
- **T3** `feat(dashboard):` `package.json` script: `"generate": "buf generate"`.
- **T4** `feat(nix):` `/root/src/rio-build/main/nix/dashboard.nix` — **Exit Path A attempt:**
  ```nix
  nativeBuildInputs = with pkgs; [ nodejs pnpm_10 pnpmConfigHook buf protobuf ];
  buildPhase = ''
    runHook preBuild
    export PROTOC=${pkgs.protobuf}/bin/protoc
    pnpm run generate
    test -f src/gen/admin_connect.ts || (echo "codegen silently no-op'd"; exit 1)
    pnpm run lint
    pnpm run test
    pnpm run build
    runHook postBuild
  '';
  ```
  `pnpmDeps.hash` changes → one more `fakeHash` cycle.
- **T5** **DECISION POINT:** `/nbr .#checks.x86_64-linux.dashboard`.
  - **Passes** → Exit Path A, done.
  - **Fails** with `protoc-gen-es: not found`, `ENOENT node_modules/.bin/...`, or postinstall-missing-binary → **Exit Path B:**
    1. NEW `/root/src/rio-build/main/scripts/regen-proto-ts.sh`:
       ```bash
       #!/usr/bin/env bash
       # Regenerates rio-dashboard/src/gen/ from rio-proto/proto/*.proto.
       # Run after touching admin.proto or types.proto. Commit the output.
       # CI drift-checks via nix/dashboard.nix buildPhase.
       set -euo pipefail
       cd "$(dirname "$0")/../rio-dashboard"
       pnpm run generate
       ```
    2. Run it locally: `nix develop -c scripts/regen-proto-ts.sh`. Commit `rio-dashboard/src/gen/*.ts`.
    3. Revert `nix/dashboard.nix` buildPhase to remove `pnpm run generate`; ADD drift check:
       ```nix
       # Exit Path B: codegen is committed. Drift-check against fresh
       # generation. No .git in sandbox — diff against a tempdir copy.
       buildPhase = ''
         runHook preBuild
         cp -r src/gen /tmp/committed-gen
         export PROTOC=${pkgs.protobuf}/bin/protoc
         pnpm run generate
         diff -r /tmp/committed-gen src/gen || (
           echo "ERROR: proto codegen drift. Run scripts/regen-proto-ts.sh and commit."
           exit 1
         )
         pnpm run lint
         pnpm run test
         pnpm run build
         runHook postBuild
       '';
       ```
    4. Remove `@bufbuild/buf`/`protoc-gen-*` from `package.json` if they're purely postinstall-driven and the devshell has `pkgs.buf` — reduces `pnpmDeps` size.
- **T6** `feat(dashboard):` `tsconfig.json` `include: ["src"]` (gen/ is under src/). `eslint.config.js` ignore `src/gen/**` (generated code has its own lint style). `flake.nix` `pre-commit.settings.excludes`: add `"^rio-dashboard/src/gen/"`.

**Exit gate:** `/nbr .#checks.x86_64-linux.dashboard` passes (either exit path). `src/gen/admin_connect.ts` exports `AdminService`. Add one import in `src/App.tsx` (`import { AdminService } from './gen/admin_connect'`) to verify tree-shaking doesn't break and types resolve.

**Files:**
| Path | Action | Collision | Note |
|---|---|---|---|
| `rio-dashboard/package.json` | MODIFY | collides P0274 | T1 — serialized by dep |
| `rio-dashboard/pnpm-lock.yaml` | MODIFY | collides P0274 | T1 regen |
| `rio-dashboard/buf.gen.yaml` | NEW | zero | T2 |
| `nix/dashboard.nix` | MODIFY | collides P0274 | T4 — `pnpmDeps.hash` bump + buildPhase |
| `scripts/regen-proto-ts.sh` | NEW (path B) | zero | T5 |
| `rio-dashboard/src/gen/*.ts` | NEW (path B) | zero | T5 |
| `flake.nix` | MODIFY | **count=24** | T6 pre-commit exclude — one line, additive |

**Deps:** `[274]` hard (same files). **Parallel with P0273, P0276.**

---

## Wave 1 — First browser contact (serialized after P0273 + P0275)

### P0277 — `feat(dashboard):` App shell + ClusterStatus — THE INTEGRATION PROOF

**De-risks R5 (browser-specific).** First plan that makes a real network call from React. P0273's curl test proved the server works; curl speaks h2c and doesn't care about CORS. **Browsers use HTTP/1.1 `fetch()` and DO preflight OPTIONS.** If P0273's `.accept_http1(true)` or `CorsLayer` was wrong, this plan finds it.

**Tasks:**
- **T1** `feat(dashboard):` `/root/src/rio-build/main/rio-dashboard/src/api/transport.ts`:
  ```typescript
  import { createConnectTransport } from "@connectrpc/connect-web";
  // baseUrl '/' — prod: nginx reverse-proxy /rio.admin.AdminService/* → scheduler:9201.
  // dev: vite.config.ts server.proxy forwards to http://localhost:9201.
  // ONE build artifact for both; no VITE_* build-time parameterization.
  export const transport = createConnectTransport({
    baseUrl: "/",
    useBinaryFormat: true, // grpc-web+proto, not grpc-web+json
  });
  ```
- **T2** `feat(dashboard):` `/root/src/rio-build/main/rio-dashboard/src/api/admin.ts`:
  ```typescript
  import { createClient } from "@connectrpc/connect";
  import { AdminService } from "../gen/admin_connect";
  import { transport } from "./transport";
  export const admin = createClient(AdminService, transport);
  ```
- **T3** `feat(dashboard):` `package.json` — add `react-router-dom@6`, `@tanstack/react-query@5`. `pnpmDeps.hash` bump.
- **T4** `feat(dashboard):` Rewrite `src/App.tsx` — `<QueryClientProvider>` → `<RouterProvider>`. Routes: `/` → `<Cluster/>`, `/builds`, `/builds/:id`, `/workers`. Layout with nav sidebar + `<Outlet/>`.
- **T5** `feat(dashboard):` `/root/src/rio-build/main/rio-dashboard/src/pages/Cluster.tsx`:
  ```typescript
  import { useQuery } from "@tanstack/react-query";
  import { admin } from "../api/admin";

  export function Cluster() {
    const { data, error, isLoading } = useQuery({
      queryKey: ["cluster-status"],
      queryFn: () => admin.clusterStatus({}),
      refetchInterval: 5000,
    });
    if (error) return <div role="alert">scheduler unreachable: {String(error)}</div>;
    if (isLoading) return <div>loading…</div>;
    // types.proto:659-669 — all 9 ClusterStatusResponse fields
    return <dl data-testid="cluster-status">
      <dt>Workers</dt><dd>{data!.activeWorkers} active / {data!.totalWorkers} total</dd>
      <dt>Builds</dt><dd>{data!.pendingBuilds} pending / {data!.activeBuilds} active</dd>
      <dt>Derivations</dt><dd>{data!.queuedDerivations} queued / {data!.runningDerivations} running</dd>
      {/* ... remaining fields */}
    </dl>;
  }
  ```
- **T6** `test(dashboard):` `/root/src/rio-build/main/rio-dashboard/src/pages/__tests__/Cluster.test.tsx` — `vi.mock("../../api/admin")`:
  - Mock `admin.clusterStatus` → resolve `{activeWorkers: 3, ...}` → assert `screen.getByText(/3 active/)`.
  - Mock reject → assert `role="alert"`.

**Exit gate:** `/nbr .#ci` green. **Manual (documented, not gating):** `kubectl port-forward svc/rio-scheduler 9201` + `cd rio-dashboard && pnpm dev` → `http://localhost:5173` shows cluster counts. **If this fails but P0273's curl passed → R5 is the culprit.** Check: browser devtools Network tab → OPTIONS preflight returned 200 with `access-control-allow-*` headers? POST returned 200 with `grpc-status: 0` in exposed response headers?

**Files:** all NEW under `rio-dashboard/src/`. `package.json`/`nix/dashboard.nix` MODIFY (hash bump — collides P0274/P0275, serialized by dep).

**Deps:** `[273, 275]` hard.

---

## Wave 2 — Feature verticals (PARALLEL after P0277)

### P0278 — `feat(dashboard):` Build list + detail drawer

**Tasks:**
- **T1** `package.json` — add `@tanstack/react-table`. Hash bump.
- **T2** `src/pages/Builds.tsx` — `useQuery(['builds', statusFilter, page])` → `admin.listBuilds({statusFilter, limit: 100, offset: page*100})`. TanStack Table columns: `build_id` (mono, click-to-copy), `state` (colored pill), `tenant`, progress bar (`completed_derivations/total_derivations`), `submitted_at` (relative: "3 min ago"), duration. Status filter pills above table. Offset pagination using `total_count` for page count. **Scheduler-side cap check:** `rio-scheduler/src/admin/builds.rs:33` clamps `limit` to 1000 — 100/page is safe.
- **T3** `src/components/BuildDrawer.tsx` — click row → slide-over drawer showing all `BuildInfo` fields (`types.proto:706-718`). Two tab placeholders: "Logs" (P0279 fills), "Graph" (P0280 fills). Deep-link fallback: if navigated directly to `/builds/:id`, no `GetBuild` RPC exists — `listBuilds({limit:1000})` client-side `.find()` (acceptable for MVP; `GetBuild` is a followup).
- **T4** `src/components/BuildStatePill.tsx` — `BuildState` enum → color + label. ONE source of truth for "what color is Failed."
- **T5** Vitest: mock `listBuilds` with 3 builds mixed states → assert 3 rows, click → drawer opens. Filter pill → assert `statusFilter` in queryFn call.

**Exit gate:** `/nbr .#ci` green.
**Deps:** `[277]`.

---

### P0279 — `feat(dashboard):` Streaming log viewer — THE R3 CONSUMER

**Highest TS-side risk.** P0273's curl proved the server STREAMS. This plan proves the browser CONSUMES it.

**Tasks:**
- **T1** `package.json` — add `@tanstack/react-virtual`. Hash bump.
- **T2** `src/hooks/useLogStream.ts`:
  ```typescript
  import { useEffect, useRef, useState } from "react";
  import { admin } from "../api/admin";

  // R8: lossy decode. Build output CAN be non-UTF-8 (compiler locale
  // garbage, binary accidentally cat'd). {fatal:false} → U+FFFD, no throw.
  const decoder = new TextDecoder("utf-8", { fatal: false });

  export function useLogStream(buildId: string, drvPath?: string) {
    const [lines, setLines] = useState<string[]>([]);
    const [done, setDone] = useState(false);
    const [err, setErr] = useState<Error | null>(null);
    const lastLineRef = useRef(0n); // for resume via since_line

    useEffect(() => {
      setLines([]); setDone(false); setErr(null); lastLineRef.current = 0n;
      const ctrl = new AbortController();
      (async () => {
        try {
          const stream = admin.getBuildLogs(
            { buildId, derivationPath: drvPath ?? "", sinceLine: lastLineRef.current },
            { signal: ctrl.signal }
          );
          for await (const chunk of stream) {
            // types.proto:727-732 — BuildLogChunk.lines is repeated bytes → Uint8Array[]
            const decoded = chunk.lines.map((b: Uint8Array) => decoder.decode(b));
            setLines(prev => [...prev, ...decoded]);
            lastLineRef.current = chunk.firstLineNumber + BigInt(chunk.lines.length);
            if (chunk.isComplete) { setDone(true); return; }
          }
        } catch (e) {
          if (!ctrl.signal.aborted) setErr(e as Error);
        }
      })();
      return () => ctrl.abort(); // unmount/deps-change → cancel stream
    }, [buildId, drvPath]);

    return { lines, done, err };
  }
  ```
  **Perf note:** `setLines(prev => [...prev, ...decoded])` is O(n) per chunk. `types.proto:178` says "64 lines or 100ms" batching → chunks are small, but a 10-minute build = ~6000 chunks. If this becomes a bottleneck: switch to `useRef<string[]>` + `useSyncExternalStore`. Decide at implement time based on profiling — don't prematurely optimize.
- **T3** `src/components/LogViewer.tsx` — `useVirtualizer({count: lines.length, estimateSize: () => 20})`. Follow-tail: `scrollToIndex(lines.length-1)` on append unless user scrolled up (`scrollTop + clientHeight < scrollHeight - 100`). `<pre>` per row, `white-space: pre`, mono font.
- **T4** Embed in `BuildDrawer.tsx` (P0278) Logs tab.
- **T5** Vitest — `src/hooks/__tests__/useLogStream.test.ts`:
  - Mock `admin.getBuildLogs` as async generator yielding 2 chunks → assert `lines` accumulates 2×64.
  - **R8 test:** yield `{lines: [Uint8Array.of(0x48,0x69), Uint8Array.of(0xff,0xfe,0x21)]}` → assert no throw, `lines[1].includes('\ufffd')`.
  - Mock `{isComplete: true}` → assert `done` flips.
  - Unmount mid-stream → assert generator's `signal.aborted` flipped (mock generator checks it).

**Exit gate:** `/nbr .#ci` green. R8 vitest passes.
**Deps:** `[277]`. Soft `[278]` (embeds in drawer — merge-trivial collision).

---

### P0280 — `feat(dashboard):` DAG visualization — THE DIFFERENTIATOR

**De-risks R6 via mandatory escape hatches.**

**Tasks:**
- **T1** `package.json` — add `@xyflow/react`, `@dagrejs/dagre`. Hash bump.
- **T2** `src/lib/graphLayout.ts`:
  ```typescript
  import dagre from "@dagrejs/dagre";
  import type { GraphNode, GraphEdge } from "../gen/types_pb";
  import type { Node, Edge } from "@xyflow/react";

  // Server truncates at 5000 (P0276). UI additionally degrades to table
  // at 2000 — reactflow can render 5000, but dagre layout takes ~8s
  // on 5000 nodes main-thread. 2000 is the interactive ceiling.
  export const DEGRADE_THRESHOLD = 2000;
  export const WORKER_THRESHOLD = 500; // dagre in WebWorker above this

  export type LayoutResult =
    | { degraded: false; nodes: Node[]; edges: Edge[] }
    | { degraded: true; reason: string };

  export function layoutGraph(gn: GraphNode[], ge: GraphEdge[]): LayoutResult {
    if (gn.length > DEGRADE_THRESHOLD) {
      return { degraded: true, reason: `${gn.length} nodes > ${DEGRADE_THRESHOLD}` };
    }
    const g = new dagre.graphlib.Graph();
    g.setGraph({ rankdir: "TB", nodesep: 40, ranksep: 80 });
    g.setDefaultEdgeLabel(() => ({}));
    for (const n of gn) g.setNode(n.drvPath, { width: 180, height: 40 });
    for (const e of ge) g.setEdge(e.childDrvPath, e.parentDrvPath); // child→parent = build order
    dagre.layout(g);
    return {
      degraded: false,
      nodes: gn.map(n => {
        const { x, y } = g.node(n.drvPath);
        return {
          id: n.drvPath, position: { x, y }, type: "drvNode",
          data: { pname: n.pname, status: n.status, workerId: n.assignedWorkerId },
        };
      }),
      edges: ge.map(e => ({
        id: `${e.childDrvPath}→${e.parentDrvPath}`,
        source: e.childDrvPath, target: e.parentDrvPath,
        style: e.isCutoff ? { strokeDasharray: "5,5" } : undefined,
      })),
    };
  }
  ```
- **T3** `src/lib/graphLayout.worker.ts` — WebWorker entry for `>500` nodes. `onmessage` → run dagre → `postMessage({positions})`. Main thread: `new Worker(new URL('./graphLayout.worker.ts', import.meta.url))` (Vite handles this).
- **T4** `src/pages/Graph.tsx` — `useQuery(['build-graph', buildId])` → `admin.getBuildGraph({buildId})`. If `response.truncated || nodes.length > DEGRADE_THRESHOLD` → `<GraphTable nodes={...}/>` (sortable, failed/poisoned float to top). Else → `layoutGraph` (worker if >500) → `<ReactFlow>`.
  **R6 critical:** `const nodeTypes = useMemo(() => ({drvNode: DrvNode}), [])` — memoize OUTSIDE the render body or at module-level. Inline `nodeTypes={{drvNode: DrvNode}}` causes full remount every render (reactflow docs warn this explicitly).
  **5s poll** via `refetchInterval: 5000` — status colors update live.
- **T5** `src/components/DrvNode.tsx` — custom reactflow node. `pname` top, `drvPath` hash-prefix mono below. Status → className → CSS: green=completed, yellow=running (with `animate-pulse`), red=failed/poisoned, gray=queued/created. Running nodes show `workerId` on hover. `onClick` → navigate `/builds/:id` with `?drv=<path>` — P0279's LogViewer filters by it.
- **T6** Embed in `BuildDrawer.tsx` Graph tab.
- **T7** Vitest: `layoutGraph` 3 nodes/2 edges → positions non-zero, edges mapped. 2001 nodes → `degraded:true`. Status className mapping.

**Exit gate:** `/nbr .#ci` green.
**Deps:** `[276, 277]` hard.

---

### P0281 — `feat(dashboard):` Management actions (Drain / ClearPoison / TriggerGC)

**The Grafana-can't value.** Three write RPCs, all already exist server-side. `TriggerGC` is a second server-stream — implicitly validates P0279's pattern.

**Tasks:**
- **T1** `src/pages/Workers.tsx` — `useQuery(['workers'])` → `admin.listWorkers({})`. Columns from `WorkerInfo` (`types.proto:679-691`): `worker_id`, status pill, `running_builds/max_builds` bar, `size_class`, `last_heartbeat` relative (**>30s ago → red, dead worker walking**). Per-row `<DrainButton/>`.
- **T2** `src/components/DrainButton.tsx` — `useMutation` from TanStack Query:
  ```typescript
  const qc = useQueryClient();
  const drain = useMutation({
    mutationFn: (workerId: string) => admin.drainWorker({ workerId, force: false }),
    onMutate: async (workerId) => {
      // Optimistic: mark row as draining immediately
      await qc.cancelQueries({queryKey: ['workers']});
      const prev = qc.getQueryData(['workers']);
      qc.setQueryData(['workers'], (old: any) => /* set worker.status='draining' */);
      return { prev };
    },
    onError: (_e, _id, ctx) => qc.setQueryData(['workers'], ctx!.prev), // revert
    onSettled: () => qc.invalidateQueries({queryKey: ['workers']}),
  });
  ```
  Confirm dialog via `window.confirm` (MVP — real modal is polish). Error → toast.
- **T3** `src/components/ClearPoisonButton.tsx` — embedded in `DrvNode.tsx` (P0280) context menu when `status==='poisoned'`. `admin.clearPoison({derivationHash})` → invalidate `['build-graph', buildId]` → node re-renders as queued.
- **T4** `src/pages/GC.tsx` — form: `dryRun` checkbox, `gracePeriodHours` number input. Submit → `for await (const progress of admin.triggerGC({dryRun, gracePeriodHours}))` → progress bar (`pathsScanned`/`pathsCollected`/`bytesFreed` from `types.proto:756-762`). `isComplete: true` → close, success toast. Reuses P0279's AbortController-in-useEffect pattern.
- **T5** `src/components/Toast.tsx` — portal + auto-dismiss. Hand-rolled context, ~40 lines. No library.
- **T6** Integrate: GC button on Cluster page (P0277). ClearPoison in DrvNode context menu (P0280).
- **T7** Vitest: mock each RPC, assert optimistic state transitions. GC: mock stream yielding 3 progress chunks → assert bar updates.

**Exit gate:** `/nbr .#ci` green.
**Deps:** `[277]` hard. `[278, 279, 280]` soft (integrates into their components — merge-trivial).

---

### P0282 — `feat(infra):` Docker image + Helm deploy — CAN DISPATCH EARLY

**Only deps on `nix/dashboard.nix` producing a `dist/`** — parallel with Wave 0b/1/2.

**Tasks:**
- **T1** `feat(nix):` `/root/src/rio-build/main/nix/docker.nix` — add dashboard image following existing `buildZstd` pattern:
  ```nix
  dashboardNginxConf = pkgs.writeText "nginx.conf" ''
    daemon off;
    error_log /dev/stderr info;
    events { worker_connections 1024; }
    http {
      include ${pkgs.nginx}/conf/mime.types;
      access_log /dev/stdout;
      server {
        listen 8080;
        # SPA: all routes serve index.html, client-side router handles path
        location / {
          root ${rioDashboard};
          try_files $uri /index.html;
        }
        # gRPC-Web is HTTP/1.1 POST — standard proxy_pass works.
        # DNS name matches the helm Service (P0282 T3).
        location /rio.admin.AdminService/ {
          proxy_pass http://rio-scheduler:9201;
          proxy_http_version 1.1;  # gRPC-Web requires 1.1
          # Disable buffering for server-streaming (GetBuildLogs, TriggerGC)
          # or nginx buffers the entire response before sending to browser.
          proxy_buffering off;
        }
      }
    }
  '';
  dashboard = buildZstd {
    name = "rio-dashboard"; tag = "dev";
    contents = [ pkgs.nginx rioDashboard ];
    config = {
      Cmd = [ "${pkgs.nginx}/bin/nginx" "-c" "${dashboardNginxConf}" ];
      ExposedPorts."8080/tcp" = {};
    };
  };
  ```
  **`proxy_buffering off` is load-bearing** — without it nginx buffers the entire server-stream before flushing to the browser, defeating live log tailing.
- **T2** `feat(flake):` `/root/src/rio-build/main/flake.nix` — pass `rioDashboard` to `nix/docker.nix` import. Expose `docker-dashboard` in `packages`. **Second flake.nix touch** (P0274 was first). Different section (packages vs checks) — low conflict.
- **T3** `feat(helm):` NEW `/root/src/rio-build/main/infra/helm/rio-build/templates/dashboard.yaml`:
  ```yaml
  {{- if .Values.dashboard.enabled }}
  apiVersion: apps/v1
  kind: Deployment
  metadata:
    name: rio-dashboard
    labels: {{- include "rio-build.labels" . | nindent 4 }}
  spec:
    replicas: {{ .Values.dashboard.replicas }}
    selector: { matchLabels: { app.kubernetes.io/name: rio-dashboard } }
    template:
      metadata:
        labels: { app.kubernetes.io/name: rio-dashboard }
      spec:
        securityContext: { runAsNonRoot: true, runAsUser: 101 }  # nginx
        containers:
        - name: nginx
          image: {{ .Values.dashboard.image.repository }}:{{ .Values.dashboard.image.tag }}
          ports: [{containerPort: 8080, name: http}]
          securityContext:
            readOnlyRootFilesystem: true
            allowPrivilegeEscalation: false
          volumeMounts:
          - { name: tmp, mountPath: /var/cache/nginx }
          - { name: tmp, mountPath: /var/run }
        volumes: [{name: tmp, emptyDir: {}}]
  ---
  apiVersion: v1
  kind: Service
  metadata: { name: rio-dashboard }
  spec:
    selector: { app.kubernetes.io/name: rio-dashboard }
    ports: [{name: http, port: 80, targetPort: 8080}]
  {{- end }}
  ```
  **No Ingress** (A6). Port-forward is the MVP access model.
- **T4** `feat(helm):` `/root/src/rio-build/main/infra/helm/rio-build/values.yaml` — `dashboard: { enabled: false, replicas: 1, image: { repository: rio-dashboard, tag: dev } }`. Default `enabled: false` so existing deploys don't change.
- **T5** `feat(helm):` `/root/src/rio-build/main/infra/helm/rio-build/templates/networkpolicy.yaml` — tighten P0273's permissive port-9201 rule to `from: [{podSelector: {matchLabels: {app.kubernetes.io/name: rio-dashboard}}}]`.

**Exit gate:** `/nbr .#docker-dashboard` produces `.tar.zst`. `helm template infra/helm/rio-build --set dashboard.enabled=true` renders cleanly. `/nbr .#ci` green (includes helm-lint if it exists).

**Files:**
| Path | Action | Collision | Note |
|---|---|---|---|
| `nix/docker.nix` | MODIFY | medium | T1 |
| `flake.nix` | MODIFY | **count=24** | T2 — packages section, disjoint from P0274's checks section |
| `infra/helm/rio-build/templates/dashboard.yaml` | NEW | zero | T3 |
| `infra/helm/rio-build/values.yaml` | MODIFY | low | T4 |
| `infra/helm/rio-build/templates/networkpolicy.yaml` | MODIFY | low | T5 — same rule P0273 added, tighten |

**Deps:** `[274]` hard. **Parallel with P0275–P0281.**

---

## Wave 3 — Verification + close-out

### P0283 — `test(vm):` Dashboard smoke — CURL-ONLY, NO PLAYWRIGHT

**De-risks R9 by not taking the risk.** Playwright = ~500MB chromium FOD + font-config sandbox issues + version-lock brittleness + zero precedent in `nix/tests/`. curl proves: (a) nginx serves SPA, (b) nginx proxies gRPC-Web to scheduler, (c) SPA routing fallback works. That IS the integration surface. Rendering is covered by vitest.

**Tasks:**
- **T1** `test(vm):` `/root/src/rio-build/main/nix/tests/fixtures/standalone.nix` — add optional `dashboard` node parameter. Node runs nginx as a systemd service serving `rioDashboard`, with `proxy_pass http://scheduler:9201` (matching P0282's conf but with VM-internal hostname).
- **T2** `test(vm):` NEW `/root/src/rio-build/main/nix/tests/scenarios/dashboard.nix`:
  ```nix
  { fixture, ... }: {
    name = "dashboard-smoke";
    testScript = ''
      start_all()
      scheduler.wait_for_unit("rio-scheduler")
      dashboard.wait_for_unit("nginx")
      dashboard.wait_for_open_port(8080)

      # SPA served: index.html has the React root mount point
      dashboard.succeed("curl -sf http://localhost:8080/ | grep -q 'id=\"root\"'")

      # SPA routing fallback: /builds/xyz returns index.html (try_files $uri /index.html)
      dashboard.succeed("curl -sf http://localhost:8080/builds/nonexistent | grep -q 'id=\"root\"'")

      # gRPC-Web through nginx proxy → scheduler: ClusterStatus returns DATA frame
      dashboard.succeed(
          "curl -sf -X POST http://localhost:8080/rio.admin.AdminService/ClusterStatus "
          "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
          r"--data-binary $'\x00\x00\x00\x00\x00' "
          "| xxd | head -1 | grep -q '^00000000: 00'"
      )

      # Server-streaming through nginx proxy: trailer frame present
      # (proxy_buffering off → stream flushes, doesn't buffer entire response)
      dashboard.succeed(
          "printf '\\x00\\x00\\x00\\x00\\x0a\\x0a\\x08nonexist' > /tmp/req.bin && "
          "curl -sf -X POST http://localhost:8080/rio.admin.AdminService/GetBuildLogs "
          "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
          "--data-binary @/tmp/req.bin | xxd | tail -5 | grep -q ' 80'"
      )
    '';
  }
  ```
- **T3** `test(vm):` `/root/src/rio-build/main/nix/tests/default.nix` — register `vm-dashboard-standalone` scenario on `standalone` fixture (not k3s-full — lighter, no ingress controller needed).

**Exit gate:** `/nbr .#checks.x86_64-linux.vm-dashboard-standalone` passes. `/nbr .#ci` green (scenario auto-joins if `default.nix` wires it into the vmTests set).

**Files:**
| Path | Action | Collision | Note |
|---|---|---|---|
| `nix/tests/fixtures/standalone.nix` | MODIFY | medium | T1 — optional dashboard node |
| `nix/tests/scenarios/dashboard.nix` | NEW | zero | T2 |
| `nix/tests/default.nix` | MODIFY | medium | T3 — one line. Soft conflict with P0241/P0243 (also touch default.nix). |

**Deps:** `[273, 282]` hard.

---

### P0284 — `docs:` Doc sweep + phase5.md scope correction

**Tasks:**
- **T1** `/root/src/rio-build/main/docs/src/phases/phase5.md:32` — **strike** "Worker utilization graphs, cache hit rate analytics" (P0222 Grafana owns it). Replace with "Interactive DAG visualization, live log streaming, management actions (drain/poison/GC)". Mark `:30-34` bullets `[x]`.
- **T2** NEW `/root/src/rio-build/main/docs/src/components/dashboard.md` (~40 lines):
  - Architecture: React SPA → nginx (`/rio.admin.AdminService/*` proxy) → scheduler `:9201` (tonic-web, plaintext, AdminService-only)
  - Why third port (mTLS on main port blocks browsers)
  - Dev workflow: `kubectl port-forward svc/rio-scheduler 9201` + `cd rio-dashboard && pnpm dev`
  - Regen workflow: `scripts/regen-proto-ts.sh` after proto changes, commit `src/gen/`
  - **No `r[ui.*]` markers** — UI presentation is not protocol-normative
- **T3** `/root/src/rio-build/main/docs/src/crate-structure.md` — note `rio-dashboard/` is a sibling, built by `nix/dashboard.nix` not Cargo.
- **T4** **TODO audit:** `rg 'TODO[^(]' rio-dashboard/ nix/dashboard.nix` — tag or resolve any untagged. `rg 'TODO\(P027[3-9]\|P028[0-3]\)' --type rust --type ts` — resolve any that reference completed plans.
- **T5** **Tracey audit:** `tracey query validate` → 0 errors. `tracey query status` → confirm no `r[ui.*]` entries appeared. If P0276 got a spec paragraph in `scheduler.md`, verify `r[sched.admin.build-graph]` has matching `impl`/`verify` annotations.

**Exit gate:** `/nbr .#ci` green (includes `tracey-validate`).
**Files:** docs only, low collision.
**Deps:** `[278, 279, 280, 281, 283]` — last to merge.

---

## Summary table

| Plan | Wave | Title | Hard deps | Hot files | Risk addressed |
|---|---|---|---|---|---|
| **P0273** | 0a | tonic-web dedicated port | — | `main.rs`(27), `scheduler.yaml`(4) | R1, R3, R5 |
| **P0274** | 0a | pnpm/Vite scaffold + `nix/dashboard.nix` | — | `flake.nix`(24) | build-chain proof |
| **P0276** | 0a | GetBuildGraph RPC — **PG-backed, thin message** | — (advisory: P0231) | `types.proto`(29), `db.rs`(34) | R4, R7 |
| **P0275** | 0b | Proto TS codegen (two-exit spike) | 274 | `nix/dashboard.nix`, `package.json` | R2 |
| **P0277** | 1 | App shell + ClusterStatus | 273, 275 | — | R5 browser-verify |
| **P0278** | 2 | Build list + drawer | 277 | — | — |
| **P0279** | 2 | Streaming log viewer | 277 | — | R3 consumer, R8 |
| **P0280** | 2 | DAG visualization | 276, 277 | — | R6 |
| **P0281** | 2 | Management actions | 277 (soft: 278,279,280) | — | — |
| **P0282** | 2† | Helm deploy + docker | 274 | `docker.nix`, `flake.nix`(24) | — |
| **P0283** | 3 | VM smoke (curl, no Playwright) | 273, 282 | `tests/default.nix` | R9 |
| **P0284** | 3 | Doc sweep | 278,279,280,281,283 | `phase5.md` | — |

† P0282 is nominally Wave 2 but dispatches at Wave 0b timing (only deps P0274).

## Dispatch order (risk-first)

```
Frontier @ T0:      [P0273, P0274, P0276]           — 3 parallel, disjoint hot files
After P0274:        + [P0275, P0282]                — 5 in flight
After P0273+P0275:  + [P0277]                       — Y-join
After P0277:        + [P0278, P0279, P0281]         — 4 parallel (+P0280 if P0276 landed)
After P0276+P0277:  + [P0280]
After P0273+P0282:  + [P0283]
After everything:   + [P0284]
```

**Max parallelism:** ~5-6 plans mid-stream.
**Critical path:** P0274 → P0275 → P0277 → P0280 → P0284 (5 hops).
**`flake.nix` touches:** P0274, P0275 (pre-commit excludes), P0282 — wave-sequenced, disjoint sections.
**`types.proto` touches:** P0276 only — EOF-append, advisory-serialize vs P0231.

---

## Collision map

| File | Count | Plans | Mitigation |
|---|---|---|---|
| `rio-proto/proto/types.proto` | 29 | P0276 | EOF-append. Advisory-serialize vs P0231 (also EOF-append — textually parallel-safe). |
| `rio-scheduler/src/db.rs` | 34 | P0276 | New fn at EOF. `.sqlx/` regen required. |
| `rio-scheduler/src/main.rs` | 27 | P0273 | Spawn block `:625` + config `:46`. Disjoint from phase-4b P0227/P0230 (size_classes ~`:699`). |
| `flake.nix` | 24 | P0274, P0275, P0282 | Wave-sequenced. Checks vs pre-commit vs packages — disjoint sections. |
| `rio-scheduler/src/admin/mod.rs` | low | P0273 (`:52` derive), P0276 (`mod graph;` + trait wire ~`:339`) | Disjoint lines. |
| `infra/helm/.../networkpolicy.yaml` | low | P0273 (add permissive rule), P0282 (tighten same rule) | Sequential by design. |
| `rio-dashboard/package.json` + `pnpmDeps.hash` | new | P0274 creates; P0275,P0277,P0278,P0279,P0280 bump | **Per-plan tax (A8).** Each does one `fakeHash` cycle. Merge-queue serializes. |
| `rio-dashboard/src/components/BuildDrawer.tsx` | new | P0278 creates; P0279, P0280 add tabs | Serial by dep. Merge-trivial (distinct tab slots). |
| `nix/tests/default.nix` | medium | P0283 | Soft conflict with phase-4b P0241/P0243 — one-line additions each. |

---

## Verification matrix

Every plan gates on `/nbr .#ci` (CLAUDE.md: single gate). Fast iteration targets:

| Plan | Fast check | Full gate |
|---|---|---|
| P0273 | `/nbr .#checks.x86_64-linux.clippy` then `nextest` | `/nbr .#ci` |
| P0274, P0275, P0277–P0281 | `/nbr .#checks.x86_64-linux.dashboard` | `/nbr .#ci` |
| P0276 | `/nbr .#checks.x86_64-linux.nextest` (after `.sqlx/` regen) | `/nbr .#ci` |
| P0282 | `/nbr .#docker-dashboard` + helm-template render | `/nbr .#ci` |
| P0283 | `/nbr .#checks.x86_64-linux.vm-dashboard-standalone` | `/nbr .#ci` |
| P0284 | `/nbr .#checks.x86_64-linux.tracey-validate` | `/nbr .#ci` |

**NEVER `nix build` locally** — `/nbr` via remote builder only (memory: 3 prior crashes).

---

## Grafted improvements vs. original winning plan

| From | Change | Why |
|---|---|---|
| MVP/Dep/User (unanimous) | **P0276 uses thin `GraphNode` (5 strings) NOT `DerivationNode`** | `drv_content` at `types.proto:32` is ≤64KB/node — 2000 nodes = 128MB > `max_encoding_message_size`. All three runner-ups caught this; winning plan's R6 was about rendering, not response size. |
| MVP/Dep/User (unanimous) | **P0276 is PG-backed (3-table JOIN) NOT actor-snapshot** | `migrations/001_scheduler.sql:39-100` schema is certain; actor retention of proto nodes is unverified. `db.rs` already has sibling queries (`:814` edge insert). |
| User-first | `GraphNode.assigned_worker_id` + `truncated`/`total_nodes` fields | Cross-links running DAG node → worker; graceful server-side cap. |
| Dep-first | Leader-election comment in P0273, `.sqlx/` regen reminder in P0276, vite proxy (one build artifact) in P0277 | Load-bearing details the winning plan mentioned but didn't codify. |
| Dep-first | `expose_headers` for `grpc-status`/`grpc-message` in P0273 CORS | connect-web can't read error trailers without this — silent error-handling failure. |
| MVP-first | `proxy_buffering off` in P0282 nginx conf | Without it nginx buffers entire server-stream → defeats live log tailing. |
| User-first | `last_heartbeat >30s → red` in P0281 worker table | Operator's first signal of a dead worker. |
| User-first | Web Worker for dagre layout >500 nodes in P0280 | Main-thread dagre on 1000 nodes = ~2s freeze. |

---

## Files referenced

- `/root/src/rio-build/main/rio-scheduler/src/main.rs` — `:595-666` mTLS + health-plaintext + main builder
- `/root/src/rio-build/main/rio-scheduler/src/admin/mod.rs` — `:52-74` `AdminServiceImpl` (no Clone derive today)
- `/root/src/rio-build/main/rio-proto/proto/types.proto` — `:12-58` `DerivationNode`/`Edge` (fat, submit-path)
- `/root/src/rio-build/main/migrations/001_scheduler.sql` — `:39-100` graph schema (confirmed)
- `/root/src/rio-build/main/nix/tracey.nix` — `:32-68` `fetchPnpmDeps` template, `:53-56` postinstall trap
- `/root/src/rio-build/main/.claude/dag.jsonl` — P0231 `UNIMPL deps=[230,205,214]`, all UNIMPL
- `/root/src/rio-build/main/.claude/collisions.jsonl` — `types.proto` count=29, `admin.proto` count=3

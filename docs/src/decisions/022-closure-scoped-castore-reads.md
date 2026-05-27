# ADR-022: Closure-scoped castore reads for assignment tokens

Status: **Accepted** (owner decisions recorded 2026-05-27).

**Scope:** what a per-build assignment token may read from the castore RPC surface (`GetDirectory`, `HasDirectories`, `HasBlobs`, `ReadBlob`, `StatBlob`). Sequencing is [implementation plan §P0591](./022-implementation-plan.md). Three candidate designs (each with a red-team review) and a separate evaluation of store-mediated delivery alternatives were developed off-repo; this document records the decisions, the chosen architecture, and the rejected alternatives in condensed form. It resolves the "assignment-token scope" follow-up that the §P0560 cutover review deferred to the untrusted-tenant milestone.

---

## 1. Problem and the hard constraint

A build's assignment token (HMAC, minted by the scheduler at dispatch, TTL = 2× build_timeout clamped to [4 h, 48 h], revoked ≤10 s after the drv reaches a terminal state) is the builder's credential for the five castore read RPCs, enforced at the `castore_tenant_id` chokepoint in `rio-store/src/grpc/directory.rs`. Authorization there is **tenant-wide**: any digest contained in any path attributed to the token's tenant (`path_tenants`) is readable. A leaked token therefore grants reads over the entire tenant store for its remaining lifetime. The goal: a build's token must only read that build's own input closure — the byte set the build itself mounts.

Hard constraint (owner decision, made before the candidate evaluation): the naive fix — writing the full closure (10³–10⁴ rows) into a scope table at every dispatch — is rejected. **No per-dispatch O(closure) database writes.** Shipped properties must not regress: tenant isolation, verified re-upload attribution, FindMissingPaths semantics, GC correctness, CA-derivation support, the JIT/streaming fetch path, and existing breaker/timeout budgets.

The raw material already exists: the scheduler computes the input closure at every dispatch, ships it in `WorkAssignment.input_closure`, signs a commitment to it in the token (`AssignmentClaims.input_closure_digest = blake3(sorted closure)`, with `digest_input_closure` as the shared helper in `rio-auth/src/hmac.rs`), and the store already verifies exactly this presented-list-vs-signed-digest pair on the upload path (`PutPathChunked` Begin). This design reuses that verification at read time instead of inventing new state.

## 2. Decision record (2026-05-27)

5. **Closure scoping Phase 4** (closing the anonymous in-cluster name-keyed read surface with an internal-caller credential) **is committed**; the credential choice happens in that phase's ADR.
6. **Under enforce, tokens without an attested closure digest are denied** (no tenant-wide fallback). Operational context: there are currently no long-lived clusters — only wipeable dev deploys — so greenfield cutovers are acceptable across both this design and the mountd-credential redesign.
7. **`HasDirectories`/`HasBlobs` are scoped for assignment tokens too** (uniform rule across all castore read RPCs).
8. **The server-side ScopeSet derivation fallback** (rebuild from pins + the `references` table) **is required before enforcing on clusters with more than one store replica** — and because of decision 9 it therefore **ships with v1**, not as a contingency.
9. **Enforce is the default from the start** (no log-only soak phase); log-only remains available as the rollback flag value, and the would-deny/deny metrics still ship for observability.
10. **Out-of-scope reads return `NOT_FOUND`** (no existence oracle); the store-side deny log/metric carries the real reason for triage.
11. **Worst-case ~2% overhead at the closure cap is accepted for v1**; the L1/L2 optimization levers stay specified as follow-ups gated on real measurements.

(Decisions 1–4 of the same date concern the mountd admission credential and are recorded in [its ADR](./022-mountd-admission-credentials.md).)

## 3. Architecture

### 3.1 Authorization model — presented, attested closure scope

At mount time (and on every new store channel), the builder presents its `WorkAssignment.input_closure` once via a new `DirectoryService.PresentClosure` unary. The store recomputes `blake3(sorted raw strings)` exactly as `digest_input_closure` does, requires equality with the token's signed `input_closure_digest`, enforces the existing `MAX_INPUT_CLOSURE` cap (65 536), keys each entry as `StorePath::sha256_digest()` (the junction-table key used by `path_tenants`/`directory_paths`/`file_blobs`/`narinfo`), and caches the resulting **ScopeSet** in RAM keyed by the closure digest itself. From then on, every castore read carrying an assignment token must resolve through a containing store path that is **both** attributed to the token's tenant (today's rule, unchanged) **and** a member of that attested closure. JWT (user/gateway) callers keep today's tenant-wide behavior; the chunk RPCs keep digest-as-capability; uploads, attribution, FindMissingPaths, GC, and terminal revocation are untouched.

The authorization statement in one sentence: **a token may read exactly the closure the scheduler signed for it and the builder mounted.** Leaked-token property: the holder can read at most the byte set the build itself mounts, until terminal state (+≤10 s) or expiry — and nothing else of the tenant's store via that token.

### 3.2 RPC-by-RPC behavior (assignment-token callers; JWT callers unchanged)

- **PresentClosure (new):** HMAC verify → digest equality → cap → cache insert. Idempotent, no PG. Mismatch ⇒ `INVALID_ARGUMENT` regardless of mode.
- **Chokepoint:** `castore_tenant_id` becomes a typed `castore_authz()` returning tenant + optional scope claim (JWT ⇒ no scope), so the compiler forces every query site to consume the scope decision. Existing order is preserved: HMAC → expiry/role → terminal-revocation probe → scope resolution.
- **GetDirectory (non-recursive), ReadBlob, StatBlob:** existing query + one shared predicate `AND <junction>.store_path_hash = ANY($scope)` (NULL bind for JWT callers). Out of scope ⇒ `NOT_FOUND` — same status as tenant-unreachable today (decision 10).
- **GetDirectory recursive (DAG prefetch):** scope-filter the seed frontier only; the descent keeps today's per-batch tenant join and inherits scope by containment (children discovered from authorized parents belong to the same containing path; pinned by a PG-backed test).
- **HasDirectories / HasBlobs:** same predicate (decision 7) — a presence bit means present AND tenant-visible AND in-closure. The gateway's delta-sync calls these with a JWT; that path is mode-independent and gets a regression test.
- **GetChunk/GetChunks/HasChunks:** unchanged (chunk digests remain learnable only via the now-scoped StatBlob/ReadBlob or prior possession).
- **Name-keyed StoreService reads, uploads, FindMissingPaths:** unchanged in this round (Phase 4 owns the name-keyed surface).

### 3.3 Data, caches, hot-path budget

New state: one in-RAM cache per store replica, `closure_digest → ScopeSet` (sorted digest vector); ~26 KB for a typical 800-path closure, ~2 MiB at the cap; capacity-capped (default 256 MiB) and idle-TTL evicted; eviction is harmless (re-present on demand); identical closures and re-dispatches dedupe by content address. **Zero Postgres tables, zero rows, zero migrations, zero per-dispatch or per-build writes. No token, proto-claims, or scheduler changes.**

Budget: warm FUSE reads and metadata ops add nothing (no RPC today, none after). Mount adds one unary. Cold-miss reads add one array bind (~0.3–2 ms against 2–8 ms observed RTT and 30/60 s budgets, <1% typical). Worst case — a cap-sized closure with ~25 k cold opens — is ~2% cumulative overhead, accepted for v1 (decision 11). Two levers are specified behind the same ScopeSet abstraction if real measurements demand them: **L1** a per-(closure, digest) verdict memo; **L2** establish-time expansion to object digests so per-RPC SQL is byte-identical to today (with rate-limited re-expansion for paths that land via substitution after establish). Neither changes the wire protocol; neither ships in v1.

### 3.4 Failure-mode policy

Config `[castore_read_scope] mode = "off" | "log" | "enforce"` (+ cache size, idle TTL), modeled on `[assignment_revocation]`. **The shipped default is `enforce`** (decision 9); `log` is the rollback value, `off` exists for dev parity. *Phase-1 staging note:* Phase 1 ships with `mode = log` (the builder does not present yet, so enforce would lean every read on the derivation fallback alone); the `enforce` default flips with the Phase-2 builder presentation — `log` remains the rollback value thereafter, and decision 9 (enforce as the end-state default, no soak phase once both halves exist) is unchanged.

| Situation | log | enforce (default) |
|---|---|---|
| Digest in scope | serve | serve |
| Digest out of scope | serve + `would_deny` metric + sampled structured log (drv, executor, digest, closure digest) | `NOT_FOUND` + the same structured log (the log carries the real reason; the status does not — decision 10) |
| Scope absent on this replica (not yet presented / evicted) | serve + `scope_absent` metric | first try the server-side derivation fallback (§3.5); if still unknown, `FAILED_PRECONDITION` + reason `CASTORE_SCOPE_REQUIRED`; the builder presents and retries within the outer fetch budget (presentation is idempotent and cheap) and presents proactively on every new channel; excluded from breaker/transient accounting |
| Presented list ≠ signed digest | `INVALID_ARGUMENT` | `INVALID_ARGUMENT` |
| Unattested token (empty `input_closure_digest`: degraded dispatch / pre-P0589 token) | serve + metric | **deny** (`PERMISSION_DENIED`) — no tenant-wide fallback (decision 6); safe because such assignments carry empty roots and the builder fails fast pre-mount |
| Revocation/HMAC failures | unchanged | unchanged (revocation stays as specced; scope never widens access) |

Never fail open in enforce mode; never silently fall back to tenant-wide.

### 3.5 Server-side derivation fallback (ships with v1)

Because enforce is the default and the store is a leaderless autoscaled replica set behind per-request L7 balancing, a replica may legitimately receive a scoped read before that build ever presented to it. Decision 8 therefore makes the derivation fallback part of v1 rather than a contingency: on scope-miss the store may rebuild the ScopeSet itself from `scheduler_live_pins` (the dispatch-time seeds) + a bounded walk of `narinfo.references`, RAM-only and TTL'd, used only on miss — the presented closure remains the primary, attested path and the digest check still gates what the builder presents. A scheduler-written scope table stays the fallback of last resort and is explicitly not planned (it reintroduces the migration, janitor, and rollout coupling this design avoids).

### 3.6 Config and observability surface

| Surface | Addition |
|---|---|
| store config | `[castore_read_scope] mode = "off" \| "log" \| "enforce"` (default `enforce` once both halves exist; Phase 1 ships `log` — see §3.4), plus cache capacity and idle TTL; modeled on `[assignment_revocation]` |
| metrics | `rio_store_castore_scope_{established,absent,mismatch,denied,would_deny}_total` + an establish-latency histogram |
| logs | sampled structured deny/would-deny log carrying drv hash, executor, requested digest, and closure digest — the triage source, since the wire status is a deliberately uninformative `NOT_FOUND` |
| status codes | out-of-scope ⇒ `NOT_FOUND`; digest mismatch ⇒ `INVALID_ARGUMENT`; scope unresolvable on this replica ⇒ `FAILED_PRECONDITION` + `CASTORE_SCOPE_REQUIRED`; unattested token under enforce ⇒ `PERMISSION_DENIED` |
| builder | a singleflight scope presenter on the fetch path; presents at mount, on every new channel, and on `CASTORE_SCOPE_REQUIRED`; tolerates `UNIMPLEMENTED` from old stores |

### 3.7 Interplay with shipped properties (deltas only)

Tenant isolation: scope is ANDed with, never replaces, the `path_tenants` join. Attribution / verified re-upload / FindMissingPaths: untouched code paths. GC: the scope is an immutable set of names — sweep neither reads nor invalidates it; live pins + grace protect in-flight builds exactly as today. One documented narrowing: an object shared between a swept in-closure path and a surviving out-of-closure path was readable by accident before and is `NOT_FOUND` now — reaching that state requires a pin-write failure plus retention expiry mid-build, the same infrastructure-failure family as losing an unshared input. CA derivations: closures contain realized child outputs at dispatch; nothing changes. JIT/streaming/breaker: `NOT_FOUND` stays terminal, `UNAVAILABLE` stays transient, `CASTORE_SCOPE_REQUIRED` is a new non-breaker branch; no budget constants change. Terminal revocation: unchanged, still evaluated before scope.

## 4. Considered alternatives

Full candidate matrices and red-team reviews were performed off-repo; summarised:

- **D1 — roots in the credential + server-side reachability + lazily materialized PG closure cache.** Meets the property but reintroduces persistent closure state on the store side (two tables, millions of rows, janitor, repair machinery); its two high-severity findings (permanent false-DENY from truncated lazy population; pins absent on the sparse-node dispatch path) mean the simple version is not the correct version, and its current-reachability semantics can drift from what the builder actually mounted. Its observability ideas (structured deny log, "explain scope" tooling) are kept; its pins+references resolver survives as the §3.5 fallback mechanism.
- **D2 — digest possession as capability** (drop the tenant join for builder tokens). Does not meet the bar: the token's formal grant *widens* to "any digest you know, any tenant"; only its marginal authority over the already-anonymous in-cluster surface shrinks. Rejected as the authorization basis; retained as the unchanged contract of the chunk layer, and its honest framing (the anonymous name-keyed surface is the binding in-cluster constraint until Phase 4) is adopted in §6.
- **Store-mediated delivery (S1: scheduler pushes scope to the store at dispatch; S2: assignments flow through the store).** Evaluated on "total moving parts and failure modes", both lose to the presented-closure design: S1 needs a migration, an O(closure-bytes) PG write inside the single-threaded dispatch actor for every build, a delete/sweep lifecycle, a scheduler↔store rollout ordering, and loses self-healing (a missed write is unrecoverable in-build, whereas a missing presentation is fixed by an idempotent re-present); S2 additionally relocates the most failure-hardened path in the system (assign → record → send → reap/redispatch) into a service that has never held per-build control-plane state, makes the store a hard dependency of every build start, and still needs all of the enforcement code. Ranking: D3 (chosen) > S1b > S1a > S2.

## 5. Residual risks

- **Enforce-by-default with no soak:** a presentation/coverage bug denies reads instead of logging them. Mitigations: the derivation fallback (§3.5), the idempotent re-present path, the `log` rollback value, and the would-deny/deny metrics + structured log shipping from day one. Acceptable per decision 6's operational context (wipeable dev deploys only).
- **Per-replica presentation churn** under L7 per-request balancing (≤ N_replicas presents per build, plus re-presents after eviction); bounded by the fallback and visible via `scope_absent`.
- **Mega-closure builds** pay the worst-case ~2% until/unless L1/L2 land (decision 11).
- **The leaked-token property is only as strong as the in-cluster surface around it**: until Phase 4, an attacker with in-cluster network reach can still use the anonymous name-keyed StoreService reads without any token (§6).

## 6. Explicit non-goals

- No change to the chunk layer: `GetChunks`/`HasChunks`/`GetChunk` stay unauthenticated, digest-as-capability.
- No change to JWT (user/gateway) castore reads: tenant-wide by design.
- No closing of the anonymous name-keyed StoreService surface (GetPath, BatchQueryPathInfo, GetNarIndex*, BatchGetManifest, QueryPathFromHashPart, non-attribution FindMissingPaths) **in this round** — that is Phase 4, now committed (decision 5) with its own ADR for the internal-caller credential. Until then the network policy remains the in-cluster boundary, and assignment tokens MUST NOT be accepted as a generic credential for name-keyed reads (prevents re-widening).
- No per-dispatch or per-build database writes, no new tables, no token-format or WorkAssignment changes, no scheduler changes.
- No change to upload gating, attribution, FindMissingPaths semantics, GC algorithms, CA handling, or any timeout/breaker/batch constant.

## 7. Phased delivery

Sequenced as [implementation plan §P0591](./022-implementation-plan.md); spec rules and tracey annotations land with the implementation phases, not with this ADR.

1. **Phase 1 — store side, enforce by default**: PresentClosure RPC + reason constant; `grpc/scope.rs` (ScopeSet, cache, establish/verify, derivation fallback); typed `castore_authz` refactor consumed at all eight query sites incl. Has\*; the shared scope predicate + recursive seed pre-filter; `[castore_read_scope]` config block (default `enforce`); metrics + structured deny log; unattested-token deny. Unit + PG-backed tests (same-tenant disjoint-closure allow/deny matrix over all five RPCs, shared-digest case, recursive containment, JWT/gateway-Has\* unaffected, unattested policy, mode matrix). *As landed:* Phase 1 ships `mode = log` (the builder does not present yet); the `enforce` default flips with the Phase-2 builder presentation — `log` remains the rollback value thereafter (§3.4 staging note).
2. **Phase 2 — builder side + negative VM coverage**: carry `input_closure` into MountInputs, present at mount and on new channels, singleflight presenter on the fetch path, `CASTORE_SCOPE_REQUIRED` present-then-retry within the outer budget (no breaker record), `UNIMPLEMENTED` tolerance for old stores. VM: extend the castore-e2e faults scenario — two builds, one tenant, disjoint closures; replaying build A's token against a B-only digest returns `NOT_FOUND` while A's build still completes. Skew note: with the enforce default, a not-yet-presenting builder is carried by the §3.5 derivation fallback; Phases 1–2 are nonetheless expected to land back-to-back, and the only deployments that exist are wipeable dev clusters (decision 6's context), so there is no brownfield ordering constraint.
3. **Phase 3 — observability follow-through and lever decision**: dashboards/alerts on the scope metrics, a mega-closure measurement run, and the L1/L2 ship/no-ship decision gated on those measurements (decision 11). Rollback posture stays "flip mode to `log`" — there is no enforce-flip step because enforce is the default.
4. **Phase 4 — follow-up ADR (committed)**: bring the anonymous name-keyed StoreService reads under credentials; choose the internal-caller credential there; when the presented credential is an assignment token, apply the same ScopeSet by name. Optional items (L1/L2 levers if not already shipped, "explain scope" admin tooling) live there too.

## 8. Cross-references

- [Implementation plan §P0591](./022-implementation-plan.md) — file-level sequencing and exit criteria; the §P0560 "assignment-token scope/lifetime" follow-up note now resolves here.
- [Mount-admission credentials ADR](./022-mountd-admission-credentials.md) — the companion decision for the node-local mountd credential (separate key family; decisions 1–4 of the same record).
- [Design Overview §5/§8](./022-design-overview.md#5-store-side-metadata-the-nar-index) — the castore metadata model and read RPC surface this scopes.
- [ADR-022 §2.2/§2.6](./022-lazy-store-fs-erofs-vs-riofs.md) — the mount-time DAG prefetch and `open()` fetch paths that consume the scoped RPCs.

# Phase-Removal Mapping — 2026-03-19

Phases are dead indirection. The plan DAG is the structure. Every `phase` reference maps to a plan number or gets removed.

## Source TODOs (14) → plan owners

| File:line | Was | → | Owner |
|---|---|---|---|
| `rio-scheduler/src/admin/builds.rs:22` | `TODO(phase4c)` cursor | `TODO(P0271)` | cursor-pagination-admin-builds |
| `rio-store/src/cache_server/auth.rs:36` | `TODO(phase4b)` narinfo scoping | `TODO(P0272)` | per-tenant-narinfo-filter |
| `rio-store/src/gc/drain.rs:109` | `TODO(phase4b)` xmax | `TODO(P0208)` | upsert-inserted refactor |
| `rio-worker/src/cgroup.rs:301` | `TODO(phase5)` device-plugin | `TODO(P0286)` | privileged hardening (retro) |
| `rio-store/src/cas.rs:564` | `TODO(phase4c)` scopeguard | `TODO(P0225)` | scopeguard-assess |
| `rio-store/src/cache_server/mod.rs:77` | `TODO(phase4c)` nix-cache-info | `TODO(P0225)` | same plan, second T |
| `rio-store/src/grpc/chunk.rs:62` | `TODO(phase5)` client chunking | `TODO(P0262)` | putchunk-impl |
| `rio-worker/src/upload.rs:166` | `TODO(phase4c)` trailer-refs | `TODO(P0263)` | worker-client-chunker (may close — check at dispatch) |
| `rio-worker/src/fuse/ops.rs:361` | `TODO(phase5)` is_file guard | `TODO(P0269)` | fuse-is-file-guard |
| `rio-store/src/metadata/queries.rs:225` | `TODO(phase4b)` RESOURCE_EXHAUSTED | `TODO(P0213)` | ratelimit-conn-cap (closes this TODO) |
| `rio-store/src/grpc/mod.rs:142` | `TODO(phase4b)` nar_buffer_budget | `TODO(P0218)` | nar-buffer-config |
| `rio-gateway/src/handler/opcodes_read.rs:493` | `TODO(phase5)` output_hash | `TODO(P0253)` | CA dependentrealisations |
| `rio-scheduler/src/db.rs:9` | `TODO(phase4c)` query→query! | **`TODO(P0297)`** | NEW: sqlx-macro-conversion |
| `rio-scheduler/src/grpc/mod.rs:189` | `TODO(phase4b)` NormalizedName | **`TODO(P0298)`** | NEW: normalized-name-newtype |

## Deferral blocks (24) → resolution

### Scheduled (17) — rewrite to "Scheduled: [P0NNN]"

| Location | Was | → |
|---|---|---|
| `controller.md:100` | WPS deferral | Scheduled: P0232 |
| `controller.md:180` | GC+WPS reconcilers | Scheduled: P0212 + P0233 |
| `data-flows.md:58` | CA cutoff | Scheduled: P0251+P0252 |
| `capacity-planning.md:64` | WPS | Scheduled: P0232 |
| `challenges.md:100` | circuit-breaker | Already implemented-ref, not deferral — just drop "Phase 4b" prefix |
| `challenges.md:138` | WPS | Scheduled: P0232 |
| `multi-tenancy.md:19` | JWT flow | Scheduled: P0257-P0260 |
| `multi-tenancy.md:64` | per-tenant signing | Scheduled: P0256 |
| `multi-tenancy.md:70` | GC policy | Scheduled: P0206+P0207 |
| `multi-tenancy.md:82` | quotas | Scheduled: P0255 |
| `worker.md:238` | atomic multi-output | Scheduled: P0267 |
| `integration.md:19` | tenant annotation | Scheduled: P0258 |
| `security.md:69` | bearer/narinfo | Scheduled: P0272 |
| `scheduler.md:215` | CutoffRebalancer | Scheduled: P0229 |
| `verification.md:83` | chaos/toxiproxy | Scheduled: P0268 |
| `verification.md:68` | security tests | Mixed — see §F below |
| `verification.md:92` | criterion/mutants | criterion→P0221; mutants→**P0301** |

### CRD-dead (2) — P0294 deletes the whole Build section

| Location | Fate |
|---|---|
| `controller.md:30` criticalPathRemaining | P0294 T4 deletes §Build CRD Lifecycle |
| `controller.md:43` conditions | Same — whole section gone |

### User-decided (5) → new plans or delete

| Location | Decision | Plan |
|---|---|---|
| `challenges.md:73` staggered scheduling | **Plan it** (P0296 ephemeral makes cold-start hot-path) | **P0299** |
| `security.md:25(a)` ed25519-only filter | **Delete** (operator's authorized_keys, operator's problem) | — |
| `verification.md:10` multi-Nix matrix | **Plan it** (protocol compat matters; Lix diverges) | **P0300** |
| `verification.md:92` cargo-mutants | **Plan it** | **P0301** |
| `verification.md:68` __noChroot precheck | **Plan it** (tiny; early reject, better UX) | **P0302** |

### Non-deferral prose (2) — just reword

| Location | Was | → |
|---|---|---|
| `data-flows.md:163` orphan timeout | "Phase deferral: not currently planned" | "Not implemented, not planned" (it's already a non-deferral) |
| `dependencies.md:3` | "Phase 1 deps are minimal" | Delete the line — historical fluff |

### §F verification.md:68 security-test breakdown

| Test | Maps to |
|---|---|
| `__noChroot` gateway reject | **P0302** (new) |
| JWT validation (expired/invalid sig) | P0259 (jwt-verify-middleware) tests |
| mTLS client cert rejection | P0242 (VM section I security) |
| FOD proxy allowlist | P0243 (fod-proxy scenario) |
| Binary cache auth | P0242 |

## New plans (P0297-P0302)

| # | Title | Cx | From | Deps |
|---|---|---|---|---|
| P0297 | sqlx query→query! macro conversion | MED | `db.rs:9` TODO | 204 |
| P0298 | NormalizedName newtype (4th ad-hoc trim dedup) | LOW | `grpc/mod.rs:189` TODO | 204 |
| P0299 | Staggered scheduling — cold-start prefetch gate | MED | `challenges.md:73` | 296 (ephemeral makes it relevant) |
| P0300 | Multi-Nix compatibility matrix (2.20+/unstable/Lix) | MED | `verification.md:10` | 204 |
| P0301 | cargo-mutants CI wiring | LOW | `verification.md:92` | 221 (criterion infra first) |
| P0302 | __noChroot gateway precheck | LOW | `verification.md:68` | 204 |

## Structural changes

| Item | Action |
|---|---|
| `docs/src/phases/` → `docs/src/phases-archive/` | `git mv` — 13 files, citations stay valid |
| `docs/src/SUMMARY.md` | Drop `phases/*` entries |
| `CLAUDE.md` `TODO(phaseXY)` convention | Rewrite → `TODO(P0NNN)` convention |
| CLAUDE.md line 109 "keep phase plan docs in sync" | Delete — no more phase docs |
| CLAUDE.md line 168 "Phase plan" in design-book list | Delete |
| Git tags `phase-1a`..`phase-4a` | **Keep** — commit-range archaeology |
| 113 `phaseXY.md:NN` citations in DONE backfill plans | **Leave** — archaeology; archive move keeps them valid |

## Sweep sequencing

1. Archive move + SUMMARY.md + CLAUDE.md rewrite + deferral-block edits — NOW (all reference existing plans)
2. TODO rewrites for the 12 with existing owners — NOW (sed-able)
3. Feed P0297-P0302 to `/plan` — background
4. TODO rewrites for P0297/P0298 — after plans land

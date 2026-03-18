# Audit Batch B1 Decisions — 2026-03-18 (spec-markers)

Items 8-14 from `.claude/notes/plan-decision-audit.md` — all spec-marker blast radius.

| # | Plan | Issue | **Decision** | Delta |
|---|---|---|---|---|
| 8 | **P0208** + `store.md` | xmax misses refcount=0 resurrection (CONFLICT fired → inserted=false → skip upload, but S3 may be gone) | **Switch: refcount=1 primary.** OVERRIDES Batch A #4. No spike. `store.md:222` marker rewrite + tracey bump. | T0 spike DROPPED; marker `r[store.cas.xmax-inserted]` → rename + rewrite |
| 9 | **P0245** + **P0259** | Controller has NO gRPC server (kube reconcile + raw-TCP /healthz only). Spec says 3 services. | **Drop controller.** P0245 `r[gw.jwt.verify]` text: "scheduler and store" not "scheduler, store, and controller". P0259 exit criterion wc -l → 2. | Marker text edit (P0245 unmerged, no tracey bump needed) |
| 10 | **P0233** | `workerpoolset.rio.build/finalizer` forks convention (existing 2 use `rio.build/<resource>-<purpose>`) | **Retrofit ALL 3 to Kubebuilder style.** Live migration: reconcile loop PATCHes add-new+remove-old idempotently on startup. Both names recognized during transition. | P0233 gets T0 migration task; existing workerpool.rs + build.rs finalizer consts change |
| 11 | **P0261** + `gateway.md` | jti-keying → user mints tokens → escapes limit. `sub`=tenant_id is the true per-tenant key. | **Key on `sub` (tenant_id).** Bounded keyspace → NO LRU eviction needed → **P0261 purpose eliminated.** P0261 → RESERVED. gateway.md:668-670 (the "jti makes keyspace unbounded" sentences) DELETED. P0213's default key becomes `Claims.sub` not `tenant_name`. | P0261 status→RESERVED; P0284 dep updated; gateway.md cleanup |
| 12 | **P0223** | `defaultAction: ALLOW` re-enables ~40 RuntimeDefault-blocked syscalls (kexec_load, userfaultfd). k8s Localhost REPLACES not LAYERS. | **Clone containerd RuntimeDefault JSON (~350 syscall allowlist, defaultAction:ERRNO) + append the 5 denials.** ADR-012:14 already says allowlist. | Profile JSON rewrite (~350 lines); `security.md:56` marker text fix |
| 13 | **P0212** | `stats.chunks_enqueued` doesn't exist (field is `s3_keys_enqueued`). Plural names fork convention. | **`rio_store_gc_path_swept_total` + `rio_store_gc_s3_key_enqueued_total`.** Singular, correct fields. Add to `observability.md`. | Code sample fix; observability.md entry |
| 14 | **P0232** + **P0234** | `target_queue_per_replica` has no source — SizeClassSpec lacks the field. 3-arg compute_desired call won't compile (real sig takes 4). | **Add `target_queue_per_replica: Option<u32>` (default 5) to SizeClassSpec.** P0232→P0233→P0234 dep chain already exists; CRD regen in P0235. | P0232 CRD struct gets field + default fn; P0234 code sample fixed |

## #8 rationale — why refcount=1 beats xmax

The upsert: `INSERT ... ON CONFLICT DO UPDATE SET refcount = refcount + 1`.

| Scenario | xmax | refcount after | xmax says | refcount=1 says | Correct answer |
|---|---|---|---|---|---|
| Fresh insert | 0 | 1 | inserted=true → upload | inserted=true → upload | upload |
| Existed at refcount≥1 | txid | ≥2 | inserted=false → skip | inserted=false → skip | skip |
| **Resurrected from refcount=0** (soft-deleted, GC drain pending) | **txid** (CONFLICT fired) | **1** | **inserted=false → skip** | **inserted=true → upload** | **upload** (S3 may be gone) |

xmax's "inserted=false" in row 3 is WRONG. The chunk is present in PG but GC may have already deleted it from S3. `refcount=1` catches this. Also: no system-column dependency, one fewer thing to explain.

## #11 rationale — why P0261 dies

The entire plan's premise was "jti keyspace is unbounded → LRU mandatory." With `sub`-keying:
- Keyspace = number of tenants (operator-created, bounded)
- `governor`'s dashmap grows to `|tenants|` and stops
- No eviction ever needed

P0213 already keys on `tenant_name` (SSH comment). Change that to `Claims.sub` (the tenant UUID from the JWT) — same boundedness, just the JWT-native source instead of the SSH fallback. P0261's T1 ("change key to jti") and T2 ("LRU wrapper") are both moot.

## Batch B2 pending — multi-plan items #15-23 (9 items)

Lower blast radius but still cascade across 2-3 plans each. Separate round.

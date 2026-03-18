# Audit Batch A Decisions — 2026-03-18

Proto/migration tier (hardest to undo). 7 decisions from `.claude/notes/plan-decision-audit.md` §Batch A.

| # | Plan | Issue | **Decision** | Delta |
|---|---|---|---|---|
| 1 | **P0263** | New `PutPathManifest` RPC proposed but files fence omits proto+store; `r[store.integrity.verify-on-put]` requires store to SHA-256 the stream | **No proto change.** FindMissingPaths + stream full NAR. Worker is NOT trusted → manifest-mode needs NAR reconstruction (ChunkCache fetch) → only a win if hit rate high. `TODO(phase6)`: measure cache hit rate first. | Plan scope shrinks; exit criterion already met via idempotency |
| 2 | **P0245** | `SubmitBuildRequest.jwt_jti` field 10 redundant — interceptor already parses Claims | **Drop proto field.** Handler reads `req.extensions().get::<Claims>().jti` → INSERT into `builds.jwt_jti` column. Add column to migration 016. Zero wire surface, full audit. | Drop `r[gw.jwt.jti-forward]` proto shape; keep audit via Claims→DB |
| 3 | **P0271** | Cursor as RFC3339 string: "opaque" is a lie, chrono not a dep, ListBuildsRequest is at types.proto:693 not admin.proto | **`base64(version_byte \|\| submitted_at_micros_i64_be \|\| build_id_bytes)`.** Actually opaque, version prefix = change later without wire break, room for tiebreaker. No chrono (PG gives micros via `EXTRACT(EPOCH)*1e6`). | Fix files fence (types.proto not admin.proto); cursor encode/decode ~20 lines |
| 4 | **P0208** | `updated_at` fallback doesn't fix the bug (it's at UPLOAD not DRAIN); wrong migration number (013 vs 012) | **Spike xmax first as planned; fallback is `RETURNING (refcount = 1)`** NOT updated_at. Zero migrations, same atomicity. | Replace fallback section |
| 5 | **P0264** | BUG: `CREATE INDEX ON chunks(chunk_hash)` — column is `blake3_hash`. DEEPER: single column can't model many-to-many for content-addressed dedup | **Junction table `chunk_tenants(blake3_hash, tenant_id)`.** Matches `path_tenants` precedent (migration 009). PutChunk does `INSERT ON CONFLICT DO NOTHING` into junction. | Migration becomes junction table, not ALTER COLUMN |
| 6 | **P0219** | `failure_count` field redundant? `retry_count` exists at db.rs:201 | **Keep `failure_count` — clean separation.** `retry_count` has other consumers (metrics, admin UI). Coupling poison to it means retry_count semantic changes cascade to poison. Explicit field. | No change — my rewrite stands |
| 7 | **P0249** | `REFERENCES realisations(id)` — no `id` column exists, PK is composite | **Composite FK `(drv_hash, output_name)` pairs + `ON DELETE RESTRICT`.** 4-column junction, wider rows but small count. No ALTER to realisations. RESTRICT (not CASCADE) protects against accidental orphaning. | Rewrite migration 015 schema |

## Context: threat model resolved (P0263)

**Worker is NOT fully trusted.** Container escape, supply-chain on worker binary. Store must independently SHA-256 the NAR → manifest-mode requires reconstruction from ChunkCache/S3 → bandwidth "win" is net positive only if cache hit rate is high. Defer to phase6 with measurement-first.

## P0219 note

The assessor was RIGHT to flag `retry_count` exists, but user chose clean separation. `failure_count` stays. The rationale: `retry_count` might feed metrics/admin displays that expect "all retry attempts" semantics; mixing poison logic in means a future "don't count X toward poison" change would silently alter those displays.

## Next: Batch B (16 items — spec-markers + multi-plan)

Lighter weight per item. User decision pending on whether to continue through all 29 or sample.

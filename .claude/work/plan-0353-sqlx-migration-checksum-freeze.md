# Plan 0353: sqlx migration checksum freeze — comment-only edits break persistent-DB deploys

[P0350](plan-0350-chunk-tenants-junction-cleanup-on-gc.md) review found the second instance of a comment-only migration edit changing the sqlx checksum. [`76ba3999`](https://github.com/search?q=76ba3999&type=commits) was the first (renumber comment in [`migrations/018_chunk_tenants.sql`](../../migrations/018_chunk_tenants.sql)); P0350-T2 is the second (CASCADE dead-code honest comment at `:17-18`). [`sqlx::migrate!()`](../../rio-store/src/main.rs) at `:209-212` validates checksums against the `_sqlx_migrations` table — any Aurora or persistent DB that already applied the OLD 018 checksum will fail with `VersionMismatch` on next deploy.

This is pre-production safe (TestDb is ephemeral — every test run drops and re-creates). It bricks persistent-DB deploys the moment we have one. Two prior edits means it will recur — migrations are the only SQL file class that gets "archaeological" comment updates as reality diverges from initial intent.

**Three remediation options from the followup:**
- **(a)** Freeze migration SQL after first prod apply + move docs to Rust doc-comments on the migration-loading module. Comment churn happens in Rust; `.sql` stays bit-frozen.
- **(b)** sqlx checksum fixup migration — a meta-migration that UPDATEs `_sqlx_migrations.checksum` for known-benign comment edits. Per-edit maintenance burden; detectable-only-at-deploy.
- **(c)** `.set_ignore_missing(true)` on the `Migrator`. **Dangerous** — masks real drift (missing migration, reordered migrations, content-changed-with-behavior migrations).

**Decision: option (a).** One-time structural fix; no ongoing maintenance; comment updates stay useful. The Rust doc-comment is the "living commentary" home; the `.sql` becomes an immutable artifact.

## Entry criteria

- [P0350](plan-0350-chunk-tenants-junction-cleanup-on-gc.md) merged (its T2 edits `:17-18` — T2 here moves that comment OUT of the `.sql` into Rust, so sequence P0350→P0353 to avoid double-edit churn)

## Tasks

### T1 — `refactor(store):` extract migration commentary to Rust doc-module

NEW [`rio-store/src/migrations.rs`](../../rio-store/src/migrations.rs) — per-migration doc-comments keyed by migration number. Not executed; exists purely for `cargo doc` + grep. Structure:

```rust
//! Migration commentary — the "living" half of migrations/*.sql.
//!
//! sqlx checksums migration files by content. Editing a comment in a
//! .sql file changes the checksum → any persistent DB that already
//! applied the old checksum fails with VersionMismatch on next deploy.
//! We hit this twice (76ba3999 renumber comment, P0350 CASCADE note).
//!
//! POLICY: migrations/*.sql are frozen after they ship to any
//! persistent DB. Commentary, rationale, "why we chose X over Y",
//! dead-code notes — all go HERE, keyed by migration number. The .sql
//! files carry only the minimal SQL + a one-line "-- see
//! rio-store/src/migrations.rs m_NNN" pointer.
//!
//! When you need to explain a migration's behavior: add or extend the
//! m_NNN const below. Do NOT edit the .sql.

/// migration 018_chunk_tenants.sql
///
/// Adds `chunk_tenants` junction (blake3_hash, tenant_id) for
/// tenant-scoped FindMissingChunks dedup.
///
/// **CASCADE dead code:** the `chunks.blake3_hash` FK has `ON DELETE
/// CASCADE`, but chunks are soft-deleted (`UPDATE SET deleted=TRUE`,
/// never `DELETE`) — CASCADE never fires. The junction-row cleanup
/// happens in `enqueue_chunk_deletes` (P0350) in the same tx as the
/// soft-delete.
///
/// **No same-txn race:** the "No race" claim (removed from .sql by
/// P0295-T40) was TRUE but for a different reason — sequential commit,
/// not txn atomicity. sqlx runs each migration in autocommit mode
/// (two separate statements, not one txn).
///
/// **Renumber history:** shipped as 017 in plan, renumbered to 018
/// after migration-number collision with `017_tenant_keys_fk_cascade`
/// (the P0332 incident). 76ba3999 fixed the stale "017" refs in code
/// comments.
pub const M_018: () = ();

// Add m_NNN consts for other migrations as commentary accumulates.
// Not all migrations need one — only those with non-obvious history.
```

MODIFY [`rio-store/src/lib.rs`](../../rio-store/src/lib.rs) — add `pub mod migrations;` so `cargo doc` renders it.

### T2 — `refactor(migrations):` strip commentary from .sql, add pointer stub

MODIFY [`migrations/018_chunk_tenants.sql`](../../migrations/018_chunk_tenants.sql) at `:15-21` (the comment block P0350-T2 + P0295-T40 both touch):

Replace the comment block with a single pointer line:

```sql
-- Commentary: see rio-store/src/migrations.rs M_018
```

**This is the LAST checksum-changing edit to 018.** After this lands, 018 is frozen — any future commentary goes to `migrations.rs::M_018`.

**Coordinate with [P0295](plan-0295-doc-rot-batch-sweep.md)-T40 and [P0350](plan-0350-chunk-tenants-junction-cleanup-on-gc.md)-T2:** both edit `:15-18` of the same file. Sequence: P0350-T2 lands its honest-CASCADE note → P0295-T40 lands its autocommit correction → this T2 replaces BOTH with the pointer stub. If this dispatches before P0295, fold T40's intent into `M_018`'s doc-comment (already drafted above) and skip P0295-T40.

### T3 — `test(store):` checksum-freeze regression guard

NEW test in [`rio-store/tests/migrations.rs`](../../rio-store/tests/migrations.rs) (or extend existing migration test module):

```rust
/// sqlx checksums migration files by content — editing a comment
/// changes the checksum and bricks persistent-DB deploys. This test
/// pins the checksum of each migration after it ships.
///
/// When adding a NEW migration: add its checksum here. The test will
/// fail once with "unknown migration NNN" — grab the sha384 from the
/// panic message, add the row, commit.
///
/// When a checksum CHANGES for an existing migration: the edit is
/// wrong. Move commentary to rio-store/src/migrations.rs instead.
/// Only legitimate reason to update a pinned checksum: pre-production,
/// the migration itself needs a behavior change, and you've verified
/// no persistent DB has applied it.
#[test]
fn migration_checksums_frozen() {
    let migrator = sqlx::migrate!("../migrations");
    let pinned: std::collections::HashMap<i64, [u8; 48]> = [
        // (version, sha384) — grab from `migrator.iter()` output on
        // first run. sqlx uses sha384 internally.
        // 001 through 018 pinned here. Impl: run once, copy from
        // panic output.
    ].into_iter().collect();

    for m in migrator.iter() {
        match pinned.get(&m.version) {
            Some(expected) => assert_eq!(
                m.checksum.as_slice(), expected,
                "migration {} checksum changed — move commentary to \
                 rio-store/src/migrations.rs, do NOT edit the .sql",
                m.version
            ),
            None => panic!(
                "unpinned migration {}: add to pinned map. checksum: {:?}",
                m.version, m.checksum
            ),
        }
    }
}
```

**Impl note:** sqlx's `Migration.checksum` is a `Cow<[u8]>` of the sha384. The test's job is to make checksum drift a CI failure, not a deploy-time surprise.

### T4 — `docs:` CLAUDE.md migration-freeze policy

MODIFY [`CLAUDE.md`](../../CLAUDE.md) — add a sub-section under `## Development Notes` or `## Plan-driven development`:

```markdown
### Migration files are frozen after they ship

`sqlx::migrate!()` checksums `.sql` files by content. Editing a comment
changes the checksum → persistent-DB deploys fail with `VersionMismatch`.

- **Commentary, rationale, history:** goes in `rio-store/src/migrations.rs`
  (per-migration `M_NNN` doc-consts). NOT in the `.sql`.
- **New migration:** add the SQL, add its checksum to
  `rio-store/tests/migrations.rs::migration_checksums_frozen`.
- **Behavior change to shipped migration:** write a NEW migration. Never
  edit shipped ones.
```

(Root-level file — outside Files-fence validator pattern, see P0304-T1.)

## Exit criteria

- `/nbr .#ci` green
- `cargo nextest run -p rio-store migration_checksums_frozen` → pass (T3: all 18 pinned)
- `grep -c 'M_018\|M_0' rio-store/src/migrations.rs` → ≥1 (T1: doc-module exists with ≥1 entry)
- `grep 'pub mod migrations' rio-store/src/lib.rs` → 1 hit (T1: module declared)
- `wc -l migrations/018_chunk_tenants.sql` → reduced vs pre-T2 (commentary stripped)
- `grep 'see rio-store/src/migrations.rs' migrations/018_chunk_tenants.sql` → 1 hit (T2: pointer stub present)
- `grep 'Migration files are frozen' CLAUDE.md` → 1 hit (T4: policy doc'd)
- **T3 mutation check:** edit a comment in `migrations/018_chunk_tenants.sql` → `migration_checksums_frozen` fails with "checksum changed — move commentary" message

## Tracey

No spec markers. Migration checksum validation is a tooling/deploy invariant, not a spec'd behavior. `r[store.*]` markers cover store RPC semantics; this is dev-workflow hygiene.

## Files

```json files
[
  {"path": "rio-store/src/migrations.rs", "action": "NEW", "note": "T1: per-migration doc-consts; M_018 seeded with CASCADE/autocommit/renumber commentary"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "T1: pub mod migrations;"},
  {"path": "migrations/018_chunk_tenants.sql", "action": "MODIFY", "note": "T2: strip :15-21 comment block, replace with 'see migrations.rs M_018' pointer. LAST checksum-changing edit to 018."},
  {"path": "rio-store/tests/migrations.rs", "action": "NEW", "note": "T3: migration_checksums_frozen test — pins sha384 of all 18 migrations"}
]
```

**Root-level file (outside Files-fence validator pattern — see P0304-T1):** `CLAUDE.md` MODIFY — T4 migration-freeze policy section.

```
rio-store/src/
├── migrations.rs                 # T1: NEW — doc-only module
└── lib.rs                        # T1: +mod migrations
migrations/018_chunk_tenants.sql  # T2: comment strip + pointer
rio-store/tests/migrations.rs     # T3: NEW — checksum freeze test
CLAUDE.md                         # T4: policy section
```

## Dependencies

```json deps
{"deps": [350], "soft_deps": [295], "note": "discovered_from=350. P0350-T2 edits migrations/018_chunk_tenants.sql:17-18 (CASCADE honest comment); this plan's T2 strips ALL commentary from that region. Sequence P0350→this so T2 here subsumes P0350-T2's edit into migrations.rs::M_018 instead of re-editing the .sql. Soft-dep P0295-T40 (edits :15-16 autocommit note — same region, same subsumption: T2 here replaces both). If this dispatches BEFORE P0295, mark P0295-T40 OBE — its intent is captured in M_018's doc-comment. 76ba3999 was the first checksum-break instance; P0350 is the second; this plan prevents the third. Option (a) chosen over (b) fixup-migration (per-edit burden, deploy-time-only detection) and (c) set_ignore_missing (dangerous — masks real drift)."}
```

**Depends on:** [P0350](plan-0350-chunk-tenants-junction-cleanup-on-gc.md) — its T2 edits the same `:17-18` lines T2 here strips. Land P0350 first; this subsumes the comment into Rust.
**Soft-dep:** [P0295](plan-0295-doc-rot-batch-sweep.md)-T40 — edits `:15-16` adjacent lines. If this lands first, T40 becomes OBE (intent captured in `M_018`).
**Conflicts with:** [`migrations/018_chunk_tenants.sql`](../../migrations/018_chunk_tenants.sql) — P0350-T2 (`:17-18`), P0295-T40 (`:15-16`), P0304-T41 (`:17-21` — `OBE` per T41 text, already fixed by 76ba3999). All in the comment region T2 strips; sequence P0350→this, mark others OBE. `rio-store/src/lib.rs` low-traffic for mod declarations.

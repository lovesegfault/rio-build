//! Per-tenant store accounting. Phase 4b: accounting only.
//! Phase 5: enforcement (reject PutPath above quota, or tenant-scoped GC).

use sqlx::PgPool;
use uuid::Uuid;

// r[impl store.gc.tenant-quota]
/// Sum of `narinfo.nar_size` over all paths this tenant has referenced.
///
/// `COALESCE(..., 0)` so a tenant with zero paths returns 0 not NULL —
/// `SUM()` over an empty set is NULL in SQL, and sqlx would return a
/// decode error for `i64` (NOT NULL expected). The `::bigint` cast is
/// belt-and-suspenders: `SUM(bigint)` already produces `numeric` in PG,
/// which sqlx would decode as `BigDecimal` not `i64`.
///
/// A path referenced by N tenants counts its full `nar_size` against
/// each tenant's total — this is intentional. Per-tenant accounting
/// measures "how many bytes does this tenant keep alive", not "how many
/// bytes would be freed if this tenant vanished". Dedup across tenants
/// is a storage win, not a quota credit.
pub async fn tenant_store_bytes(pool: &PgPool, tenant_id: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        r#"SELECT COALESCE(SUM(n.nar_size), 0)::bigint
           FROM narinfo n
           JOIN path_tenants pt USING (store_path_hash)
           WHERE pt.tenant_id = $1"#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
}

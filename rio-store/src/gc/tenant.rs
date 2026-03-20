//! Per-tenant store accounting and quota lookup.
//!
//! Phase 4b: accounting only. Phase 5: enforcement at the gateway
//! (reject SubmitBuild above quota — `r[store.gc.tenant-quota-enforce]`).

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

/// `(used_bytes, limit_bytes)` for a tenant looked up BY NAME.
///
/// Backs the `TenantQuota` RPC. The gateway only knows `tenant_name`
/// (authorized_keys comment) in dual-mode fallback; joining on name
/// here keeps the gateway PG-free per `r[sched.tenant.resolve]`.
///
/// `None` → unknown tenant (gateway passes through — single-tenant
/// mode or a tenant that was never seeded). `Some((used, None))` →
/// known tenant with no configured limit (`gc_max_store_bytes IS
/// NULL`). `Some((used, Some(limit)))` → enforceable quota.
///
/// Two queries, not one JOIN: the usage SUM is already isolated in
/// [`tenant_store_bytes`] (one source of truth — the accounting
/// number the admin sees is the same number the gate uses). The
/// extra round-trip is inside a 30s-cached path, so latency is a
/// non-concern.
pub async fn tenant_quota_by_name(
    pool: &PgPool,
    tenant_name: &str,
) -> Result<Option<(i64, Option<i64>)>, sqlx::Error> {
    let row: Option<(Uuid, Option<i64>)> =
        sqlx::query_as("SELECT tenant_id, gc_max_store_bytes FROM tenants WHERE tenant_name = $1")
            .bind(tenant_name)
            .fetch_optional(pool)
            .await?;

    let Some((tenant_id, limit)) = row else {
        return Ok(None);
    };

    let used = tenant_store_bytes(pool, tenant_id).await?;
    Ok(Some((used, limit)))
}

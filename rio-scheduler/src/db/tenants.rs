//! Tenant CRUD + auth queries — `tenants` and `jwt_revoked` tables.

use uuid::Uuid;

use super::{SchedulerDb, TenantRow};

impl SchedulerDb {
    /// Resolve a tenant name to its UUID. `None` if no such tenant.
    /// Used by SubmitBuild / ResolveTenant / ListBuilds — the gateway
    /// sends the tenant NAME (from the `authorized_keys` comment
    /// field); the scheduler resolves it here.
    pub(crate) async fn lookup_tenant_id(&self, name: &str) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar("SELECT tenant_id FROM tenants WHERE tenant_name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await
    }

    /// Check if a JWT `jti` is in the revocation table. EXISTS —
    /// short-circuits at first match, no row data transferred. PK
    /// index on `jti` makes this O(log n); the table is small
    /// (revocations are rare events) so this is ~1 index page hit.
    pub(crate) async fn is_jwt_revoked(&self, jti: &str) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jwt_revoked WHERE jti = $1)")
            .bind(jti)
            .fetch_one(&self.pool)
            .await
    }

    /// List all tenants (for AdminService.ListTenants).
    pub(crate) async fn list_tenants(&self) -> Result<Vec<TenantRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT tenant_id, tenant_name, gc_retention_hours, gc_max_store_bytes,
                   cache_token IS NOT NULL AS has_cache_token,
                   EXTRACT(EPOCH FROM created_at)::bigint AS created_at
            FROM tenants ORDER BY created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Default `gc_retention_hours` for new tenants: 168h = 7 days.
    /// Used as the COALESCE fallback in `Self::create_tenant` when
    /// the CreateTenant request omits retention (proto3 default 0 →
    /// `None` here → this value).
    pub const DEFAULT_GC_RETENTION_HOURS: i32 = 168;

    /// Create a tenant. Returns `None` on conflict (tenant_name OR
    /// cache_token already exists) — caller maps to `AlreadyExists`.
    ///
    /// `gc_retention_hours=None` → [`DEFAULT_GC_RETENTION_HOURS`] via SQL COALESCE.
    ///
    /// [`DEFAULT_GC_RETENTION_HOURS`]: Self::DEFAULT_GC_RETENTION_HOURS
    pub(crate) async fn create_tenant(
        &self,
        name: &str,
        gc_retention_hours: Option<i32>,
        gc_max_store_bytes: Option<i64>,
        cache_token: Option<&str>,
    ) -> Result<Option<TenantRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            INSERT INTO tenants (tenant_name, gc_retention_hours, gc_max_store_bytes, cache_token)
            VALUES ($1, COALESCE($2, 168), $3, $4)
            ON CONFLICT DO NOTHING
            RETURNING tenant_id, tenant_name, gc_retention_hours, gc_max_store_bytes,
                      cache_token IS NOT NULL AS has_cache_token,
                      EXTRACT(EPOCH FROM created_at)::bigint AS created_at
            "#,
        )
        .bind(name)
        .bind(gc_retention_hours)
        .bind(gc_max_store_bytes)
        .bind(cache_token)
        .fetch_optional(&self.pool)
        .await
    }
}

//! Tenant CRUD queries — `tenants` table.

use super::{SchedulerDb, TenantRow};

impl SchedulerDb {
    /// List all tenants (for AdminService.ListTenants).
    pub async fn list_tenants(&self) -> Result<Vec<TenantRow>, sqlx::Error> {
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

    /// Create a tenant. Returns `None` on conflict (tenant_name OR
    /// cache_token already exists) — caller maps to `AlreadyExists`.
    pub async fn create_tenant(
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

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
    ///
    /// `query_as!` (not runtime `query_as`): compile-checks the
    /// projection against [`TenantRow`] — the cross-service contract
    /// struct in `rio_migrations::schema`. The `!` overrides on
    /// `has_cache_token`/`created_at` tell sqlx the expressions are
    /// non-NULL (PG can't infer that for `IS NOT NULL` / `EXTRACT`).
    pub(crate) async fn list_tenants(&self) -> Result<Vec<TenantRow>, sqlx::Error> {
        sqlx::query_as!(
            TenantRow,
            r#"
            SELECT tenant_id, tenant_name, gc_retention_hours, gc_max_store_bytes,
                   cache_token IS NOT NULL AS "has_cache_token!",
                   EXTRACT(EPOCH FROM created_at)::bigint AS "created_at!"
            FROM tenants ORDER BY created_at
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Default `gc_retention_hours` for new tenants: 168h = 7 days.
    /// Applied via `unwrap_or` in `Self::create_tenant` when the
    /// CreateTenant request omits retention (proto3 default 0 →
    /// `None` here → this value).
    pub const DEFAULT_GC_RETENTION_HOURS: i32 = 168;

    /// Create a tenant. Returns `None` on conflict (tenant_name OR
    /// cache_token already exists) — caller maps to `AlreadyExists`.
    ///
    /// `gc_retention_hours=None` → [`DEFAULT_GC_RETENTION_HOURS`] via `unwrap_or`.
    ///
    /// [`DEFAULT_GC_RETENTION_HOURS`]: Self::DEFAULT_GC_RETENTION_HOURS
    pub(crate) async fn create_tenant(
        &self,
        name: &str,
        gc_retention_hours: Option<i32>,
        gc_max_store_bytes: Option<i64>,
        cache_token: Option<&str>,
    ) -> Result<Option<TenantRow>, sqlx::Error> {
        sqlx::query_as!(
            TenantRow,
            r#"
            INSERT INTO tenants (tenant_name, gc_retention_hours, gc_max_store_bytes, cache_token)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT DO NOTHING
            RETURNING tenant_id, tenant_name, gc_retention_hours, gc_max_store_bytes,
                      cache_token IS NOT NULL AS "has_cache_token!",
                      EXTRACT(EPOCH FROM created_at)::bigint AS "created_at!"
            "#,
            name,
            gc_retention_hours.unwrap_or(Self::DEFAULT_GC_RETENTION_HOURS),
            gc_max_store_bytes,
            cache_token,
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Delete a tenant by name, DISPOSITIONING gc-hold evidence FIRST
    /// (bug_095 / migration 104): `gc_holds.tenant_id` is `ON DELETE
    /// RESTRICT` — hold rows are KeepForever audit evidence, so a
    /// hold-bearing tenant refuses deletion at the schema layer and
    /// this fn turns each face into a typed outcome BEFORE issuing
    /// the DELETE (the operator gets the doctrine, not an FK error):
    /// active holds → release first (the heal edge stays witnessed);
    /// released history → the tenant is permanently archival (its
    /// rows anchor the audit's WHO). FK CASCADE
    /// (tenant_keys/upstreams/path_tenants/chunk_tenants) and SET NULL
    /// (builds/derivations) handle the rest — see migrations 009/012/
    /// 017/018/026.
    pub(crate) async fn delete_tenant(
        &self,
        name: &str,
    ) -> Result<TenantDeleteOutcome, sqlx::Error> {
        let Some(row) = sqlx::query!(
            r#"SELECT t.tenant_id,
                      COUNT(h.hold_id) FILTER (
                          WHERE h.released_at IS NULL
                            AND (h.expires_at IS NULL OR h.expires_at > now())
                      ) AS "active_holds!",
                      COUNT(h.hold_id) AS "total_holds!"
                 FROM tenants t
                 LEFT JOIN gc_holds h ON h.tenant_id = t.tenant_id
                WHERE t.tenant_name = $1
                GROUP BY t.tenant_id"#,
            name
        )
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(TenantDeleteOutcome::NotFound);
        };
        if row.active_holds > 0 {
            return Ok(TenantDeleteOutcome::ActiveHolds {
                count: row.active_holds,
            });
        }
        if row.total_holds > 0 {
            return Ok(TenantDeleteOutcome::HoldHistory {
                count: row.total_holds,
            });
        }
        let r = sqlx::query!("DELETE FROM tenants WHERE tenant_name = $1", name)
            .execute(&self.pool)
            .await?;
        Ok(if r.rows_affected() > 0 {
            TenantDeleteOutcome::Deleted
        } else {
            // Raced with a concurrent delete (or a hold inserted
            // between the check and the DELETE — RESTRICT would then
            // error; the race window is the operator-RPC plane).
            TenantDeleteOutcome::NotFound
        })
    }
}

/// Typed offboarding outcome (bug_095): the gc-hold doctrine's faces,
/// dispositioned BEFORE the DELETE — see [`SchedulerDb::delete_tenant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TenantDeleteOutcome {
    /// No hold rows; the tenant row was deleted.
    Deleted,
    /// No tenant by that name.
    NotFound,
    /// Unreleased, unexpired hold(s): release them first — the heal
    /// edge stays witnessed (holds are RELEASED, never deleted).
    ActiveHolds {
        /// Active hold rows referencing the tenant.
        count: i64,
    },
    /// Released hold history pins the tenant anchor: a hold-bearing
    /// tenant is permanently archival by doctrine (M_104).
    HoldHistory {
        /// Total hold rows referencing the tenant.
        count: i64,
    },
}

//! `AdminService.ListTenants` / `CreateTenant` implementation.

use tonic::Status;

use rio_common::tenant::NormalizedName;
use rio_proto::types::{
    CreateTenantRequest, CreateTenantResponse, ListTenantsResponse, TenantInfo,
};

use crate::db::{SchedulerDb, TenantRow};

/// Load all tenants from PG and convert to proto.
pub(super) async fn list_tenants(db: &SchedulerDb) -> Result<ListTenantsResponse, Status> {
    let rows = db
        .list_tenants()
        .await
        .map_err(|e| Status::internal(format!("db: {e}")))?;
    Ok(ListTenantsResponse {
        tenants: rows.into_iter().map(tenant_row_to_proto).collect(),
    })
}

/// Validate the request and insert a new tenant row.
///
/// `NormalizedName` trims + rejects empty AND interior whitespace.
/// The same normalization runs at every read path (gateway server.rs,
/// cache auth.rs, SubmitBuild) — storing the normalized form here means
/// every `WHERE tenant_name = $1` lookup matches. Without it,
/// `" team-a "` never matches `'team-a'` and the operator spends an
/// afternoon staring at invisible whitespace. Interior-whitespace
/// rejection (`"team a"` → `InteriorWhitespace`) catches the typo class
/// where an authorized_keys comment has a space instead of a dash — the
/// tenant would be unreachable otherwise.
pub(super) async fn create_tenant(
    db: &SchedulerDb,
    req: CreateTenantRequest,
) -> Result<CreateTenantResponse, Status> {
    let tenant_name = NormalizedName::new(&req.tenant_name)
        .map_err(|e| Status::invalid_argument(format!("invalid tenant_name: {e}")))?;
    let cache_token = req.cache_token.as_deref().map(str::trim);
    if cache_token.is_some_and(str::is_empty) {
        return Err(Status::invalid_argument(
            "cache_token must not be empty string (omit field for no-cache-auth)",
        ));
    }
    // u32→i32 / u64→i64 would wrap to negative for out-of-range
    // values (PG stores INTEGER/BIGINT signed). GC with negative
    // retention is undefined downstream.
    let gc_retention_hours = req
        .gc_retention_hours
        .map(|h| {
            i32::try_from(h).map_err(|_| {
                Status::invalid_argument("gc_retention_hours out of range (max 2^31-1)")
            })
        })
        .transpose()?;
    let gc_max_store_bytes = req
        .gc_max_store_bytes
        .map(|b| {
            i64::try_from(b).map_err(|_| {
                Status::invalid_argument("gc_max_store_bytes out of range (max 2^63-1)")
            })
        })
        .transpose()?;
    let row = db
        .create_tenant(
            &tenant_name,
            gc_retention_hours,
            gc_max_store_bytes,
            cache_token,
        )
        .await
        .map_err(|e| Status::internal(format!("db: {e}")))?
        .ok_or_else(|| {
            Status::already_exists(format!(
                "tenant '{tenant_name}' already exists (or cache_token collision)"
            ))
        })?;
    Ok(CreateTenantResponse {
        tenant: Some(tenant_row_to_proto(row)),
    })
}

pub(super) fn tenant_row_to_proto(row: TenantRow) -> TenantInfo {
    TenantInfo {
        tenant_id: row.tenant_id.to_string(),
        tenant_name: row.tenant_name,
        gc_retention_hours: row.gc_retention_hours as u32,
        gc_max_store_bytes: row.gc_max_store_bytes.map(|b| b as u64),
        created_at: Some(prost_types::Timestamp {
            seconds: row.created_at,
            nanos: 0,
        }),
        has_cache_token: row.has_cache_token,
    }
}

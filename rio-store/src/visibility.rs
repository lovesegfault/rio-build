//! Tenant sig-visibility: the ONE body behind every visibility decision
//! (bug_115).
//!
//! Factored out of `grpc/sign.rs::sig_visibility_gate` so the
//! materialization walk's local-presence probe and the gRPC read gates
//! decide visibility through the SAME code path. The verdict itself —
//! the I-217 table over `(owned, any_built, sig_trusted)` — lives in
//! [`rio_evidence_kernel::visibility::visibility_verdict`] (kani-swept,
//! K4); this module owns the PG/projection work that feeds it and the
//! [`TenantVisible`] witness that proves a caller consulted it.
//!
//! ## The witness
//!
//! [`TenantVisible`] is mintable ONLY by [`visible_to_tenant`]'s
//! Visible verdict. The walk's `LocalPresence::Present` arm carries one
//! structurally, so a tenant-blind "physically present ⇒ serve it"
//! probe no longer compiles (the pre-fix walk laundered paths that the
//! gate hides — substitution-only rows signed by keys the interested
//! tenants don't trust, or other tenants' built outputs per I-217 —
//! into the job's per-tenant ownership via
//! `upsert_path_tenants_for_batch`).
//!
//! ## Caller-side policy exemptions
//!
//! Anonymous requests (`tenant_id = None`) are unfiltered
//! (`r[store.tenant.narinfo-filter]`) and `.drv` paths are build
//! inputs, exempt from tenant scoping (`r[gw.jwt.anon-drv-lookup]`).
//! Both short-circuit BEFORE the kernel table — they are policy about
//! whether visibility applies, not visibility cells.

use std::collections::HashMap;
use std::sync::Arc;

use rio_evidence_kernel::visibility::{VisibilityVerdict, visibility_verdict};
use rio_proto::validated::ValidatedPathInfo;
use uuid::Uuid;

use crate::error::MetadataError;
use crate::metadata;
use crate::signing::TenantSigner;

/// Witness: this path passed the tenant sig-visibility verdict for one
/// interested tenant. Sole mint: [`visible_to_tenant`]'s Visible arm.
/// `LocalPresence::Present` requires one — the walk cannot treat a
/// local row as servable without having consulted the gate.
#[must_use]
#[derive(Debug)]
pub(crate) struct TenantVisible(());

/// Per-job memo of `tenant → trusted-key entries` (bug_115: the
/// trusted set costs two PG queries; a closure walk consults it for
/// every locally-present path × interested tenant, so it is cached for
/// the job's lifetime — key material changes do not need to land
/// mid-walk).
#[derive(Default)]
pub(crate) struct TrustedSetCache {
    by_tenant: HashMap<Uuid, Arc<Vec<String>>>,
}

/// The requesting tenant's signature trust set: upstream
/// `trusted_keys` ∪ cluster key (current + prior history) ∪ the
/// tenant's own `tenant_keys` pubkeys. Memoized in `cache`.
///
/// The cluster + tenant-own union covers the PutPath→scheduler timing
/// window: `maybe_sign` signs with the cluster key OR (when
/// `r[store.tenant.sign-key]` applies) the tenant's own key —
/// `path_tenants` count=0 → gate fires → without BOTH unioned in, a
/// freshly-built path is invisible to its own tenant.
pub(crate) async fn trusted_set(
    pool: &sqlx::PgPool,
    signer: Option<&TenantSigner>,
    tid: Uuid,
    cache: &mut TrustedSetCache,
) -> Result<Arc<Vec<String>>, MetadataError> {
    if let Some(hit) = cache.by_tenant.get(&tid) {
        return Ok(Arc::clone(hit));
    }
    let mut trusted = metadata::upstreams::tenant_trusted_keys(pool, tid).await?;
    if let Some(ts) = signer {
        trusted.push(ts.cluster().trusted_key_entry());
        // r[impl store.key.rotation-cluster-history]
        // Prior cluster keys: paths signed under a rotated-out key stay
        // visible after CASCADE drops their path_tenants rows.
        trusted.extend_from_slice(ts.prior_cluster_entries());
    }
    // r[impl store.tenant.sign-key]
    // The tenant's OWN signing pubkey(s) — see the window note above.
    let own = metadata::tenant_keys::trusted_key_entries(pool, tid).await?;
    trusted.extend(own);
    let entry = Arc::new(trusted);
    cache.by_tenant.insert(tid, Arc::clone(&entry));
    Ok(entry)
}

/// May `tenant_id` see `info`? `Ok(Some(_))` = visible (witness
/// minted), `Ok(None)` = hidden (treat as absent for this tenant).
///
/// This is the FULL former body of `sig_visibility_gate`: policy
/// exemptions, the one-round-trip `(owned, any_built)` projection, the
/// lazy signature check, and the kernel verdict. `sig_trusted` is only
/// computed for substitution-only rows — sound because the kernel's K4
/// harness proves the verdict is independent of `sig_trusted` once
/// `owned || any_built` holds.
// r[impl store.substitute.tenant-sig-visibility+2]
// r[impl store.materialize.local-visibility]
pub(crate) async fn visible_to_tenant(
    pool: &sqlx::PgPool,
    signer: Option<&TenantSigner>,
    tenant_id: Option<Uuid>,
    info: &ValidatedPathInfo,
    cache: &mut TrustedSetCache,
) -> Result<Option<TenantVisible>, MetadataError> {
    let Some(tid) = tenant_id else {
        // Anonymous → unfiltered (r[store.tenant.narinfo-filter]).
        return Ok(Some(TenantVisible(())));
    };
    // r[impl gw.jwt.anon-drv-lookup]
    // .drv files are build INPUTS, not tenant-owned outputs — exempt
    // from tenant-scoped visibility (store-side mirror of the gateway's
    // `jwt_unless_drv`).
    if info.store_path.is_derivation() {
        return Ok(Some(TenantVisible(())));
    }

    let path_hash = info.store_path.sha256_digest();

    // Two facts in one round-trip: does this tenant own it, and has
    // ANY tenant ever built it? (`path_tenants` is populated at
    // build-completion by the scheduler; the Substituter does not
    // populate it — zero rows ⇒ substitution-only or the fresh-PutPath
    // window.)
    let (owned, any_built): (bool, bool) = sqlx::query_as(
        "SELECT \
           bool_or(tenant_id = $2), \
           count(*) > 0 \
         FROM path_tenants WHERE store_path_hash = $1",
    )
    .bind(path_hash.as_slice())
    .bind(tid)
    .fetch_one(pool)
    .await
    .map(|(o, a): (Option<bool>, bool)| (o.unwrap_or(false), a))?;

    // Lazy signature cell: only substitution-only rows need it (K4
    // proves the verdict ignores it otherwise).
    let sig_trusted = if !owned && !any_built {
        let trusted = trusted_set(pool, signer, tid, cache).await?;
        if trusted.is_empty() {
            // Tenant trusts no keys AND no signer configured → any
            // substituted path is invisible.
            false
        } else {
            let fp = rio_nix::narinfo::fingerprint(
                info.store_path.as_str(),
                &info.nar_hash,
                info.nar_size,
                &info
                    .references
                    .iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>(),
            );
            crate::signing::any_sig_trusted(&info.signatures, &trusted, &fp).is_some()
        }
    } else {
        false
    };

    Ok(match visibility_verdict(owned, any_built, sig_trusted) {
        VisibilityVerdict::Visible => Some(TenantVisible(())),
        VisibilityVerdict::Hidden => None,
    })
}

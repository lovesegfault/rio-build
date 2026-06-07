//! Tenant sig-visibility: the ONE body behind every visibility decision
//! (bug_115; batch entrypoint unified by bug_061 —
//! `r[store.visibility.one-body]`).
//!
//! Factored out of `grpc/sign.rs::sig_visibility_gate` so the
//! materialization walk's local-presence probe and the gRPC read gates
//! — single-path AND batch (`FindMissingPaths`) — decide visibility
//! through the SAME code path: one `(owned, any_built)` projection
//! ([`own_built_projection`]), one `.drv` exemption ([`drv_exempt`]),
//! one signature cell ([`sig_cell`]), one malformed-row disposition
//! (DB-egress validation errors propagate on every entrypoint). The
//! verdict itself — the I-217 table over
//! `(owned, any_built, sig_trusted)` — lives in
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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rio_evidence_kernel::visibility::{VisibilityVerdict, visibility_verdict};
use rio_proto::validated::ValidatedPathInfo;
use uuid::Uuid;

use crate::error::MetadataError;
use crate::metadata;
use crate::signing::TenantSigner;

/// The `.drv` exemption test shared by BOTH visibility entrypoints:
/// parsed [`rio_nix::store_path::StorePath::is_derivation`] semantics
/// (the name component ends in `.drv`). An unparseable request string
/// is NOT exempt — it cannot name a derivation the store knows, and it
/// can never correspond to a complete narinfo row (paths are validated
/// at ingest). Pre-bug_061 the batch gate tested the RAW string's
/// suffix while the single-path gate tested the parsed name; for every
/// path that can actually be locally present the two agree, but the
/// policy now exists exactly once.
pub(crate) fn drv_exempt(path: &str) -> bool {
    rio_nix::store_path::StorePath::parse(path)
        .map(|sp| sp.is_derivation())
        .unwrap_or(false)
}

/// The ONE `(owned, any_built)` projection both visibility entrypoints
/// read (bug_061): per input hash — does `tid` own it, and has ANY
/// tenant built it. A hash with zero `path_tenants` rows
/// (substitution-only, or the fresh-PutPath window) is absent from the
/// returned map.
async fn own_built_projection(
    pool: &sqlx::PgPool,
    tid: Uuid,
    hashes: &[Vec<u8>],
) -> Result<HashMap<Vec<u8>, (bool, bool)>, sqlx::Error> {
    if hashes.is_empty() {
        return Ok(HashMap::new());
    }
    #[derive(sqlx::FromRow)]
    struct OwnBuilt {
        h: Vec<u8>,
        owned: bool,
    }
    let rows: Vec<OwnBuilt> = sqlx::query_as(
        "SELECT pt.store_path_hash AS h, \
                bool_or(pt.tenant_id = $2) AS owned \
         FROM path_tenants pt \
         JOIN UNNEST($1::bytea[]) AS k(h) ON pt.store_path_hash = k.h \
         GROUP BY pt.store_path_hash",
    )
    .bind(hashes)
    .bind(tid)
    .fetch_all(pool)
    .await?;
    // A returned group has ≥1 row by construction → any_built = true.
    Ok(rows.into_iter().map(|r| (r.h, (r.owned, true))).collect())
}

/// The ONE signature cell both visibility entrypoints evaluate for a
/// substitution-only row (bug_061): does any stored signature verify
/// against `trusted` over the row's fingerprint. An empty trusted set
/// is `false` (tenant trusts no keys → any substituted path is
/// invisible).
fn sig_cell(info: &ValidatedPathInfo, trusted: &[String]) -> bool {
    if trusted.is_empty() {
        return false;
    }
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
    crate::signing::any_sig_trusted(&info.signatures, trusted, &fp).is_some()
}

/// Witness: this path passed the tenant sig-visibility verdict — and
/// CARRIES the tenants it passed for (bug_139 / signed Q2: the
/// quantifier travels with the evidence; a consumer cannot widen a
/// one-tenant verdict into all-tenant ownership because the set is
/// part of the witness). Sole mints: [`visible_to_tenant`]'s Visible
/// arms (the consulted tenant's singleton set — empty only in
/// anonymous/single-tenant mode, where no per-tenant ownership
/// exists to stamp) and [`merge`](Self::merge). `LocalPresence::Present`
/// requires one — the walk cannot treat a local row as servable
/// without having consulted the gate.
#[must_use]
#[derive(Debug)]
pub(crate) struct TenantVisible(Vec<Uuid>);

impl TenantVisible {
    /// The tenants whose view validated this path (non-empty).
    pub(crate) fn tenants(&self) -> &[Uuid] {
        &self.0
    }

    /// Union two witnesses (both are mints, so the union is one).
    pub(crate) fn merge(&mut self, other: TenantVisible) {
        for t in other.0 {
            if !self.0.contains(&t) {
                self.0.push(t);
            }
        }
    }
}

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
        return Ok(Some(TenantVisible(tenant_id.into_iter().collect())));
    };
    // r[impl gw.jwt.anon-drv-lookup]
    // .drv files are build INPUTS, not tenant-owned outputs — exempt
    // from tenant-scoped visibility (store-side mirror of the gateway's
    // `jwt_unless_drv`).
    if info.store_path.is_derivation() {
        return Ok(Some(TenantVisible(tenant_id.into_iter().collect())));
    }

    // The shared projection (bug_061: the SAME query the batch
    // entrypoint runs — `path_tenants` is populated at build-completion
    // by the scheduler; the Substituter does not populate it — zero
    // rows ⇒ substitution-only or the fresh-PutPath window).
    let hashes = vec![info.store_path.sha256_digest().to_vec()];
    let own_built = own_built_projection(pool, tid, &hashes).await?;
    let (owned, any_built) = own_built.get(&hashes[0]).copied().unwrap_or((false, false));

    // Lazy signature cell: only substitution-only rows need it (K4
    // proves the verdict ignores it otherwise). The cell itself is the
    // shared [`sig_cell`] — empty trusted set ⇒ false.
    let sig_trusted = if !owned && !any_built {
        let trusted = trusted_set(pool, signer, tid, cache).await?;
        sig_cell(info, &trusted)
    } else {
        false
    };

    Ok(match visibility_verdict(owned, any_built, sig_trusted) {
        VisibilityVerdict::Visible => Some(TenantVisible(tenant_id.into_iter().collect())),
        VisibilityVerdict::Hidden => None,
    })
}

/// Batch entrypoint of the ONE visibility body (bug_061): given the
/// locally-present subset of a `FindMissingPaths` request, return the
/// subset visible to `tenant_id`. Anonymous (`None`) is unfiltered
/// (`r[store.tenant.narinfo-filter]`); `.drv` paths are exempt
/// ([`drv_exempt`] — same policy as the single-path body); built
/// paths take the kernel verdict over the shared
/// [`own_built_projection`]; substitution-only paths evaluate the
/// shared [`sig_cell`] over rows fetched through
/// [`metadata::query_path_info_batch`] — which validates rows at DB
/// egress, so a malformed row surfaces as
/// [`MetadataError::MalformedRow`] on this path EXACTLY as it does on
/// the single-path read (ONE disposition; the pre-fix batch silently
/// hid the row, answering "missing" where the single-path RPC answered
/// Internal). Rows with no complete manifest are hidden — the
/// single-path flow's callers hit NotFound at `query_path_info` before
/// the gate runs, so the entrypoints agree there too.
///
/// ≤3 PG round-trips regardless of `present.len()` (one projection
/// GROUP BY, one batched narinfo fetch, trusted set via
/// [`trusted_set`]'s two queries) — the I-110 batching contract.
// r[impl store.visibility.one-body]
// r[impl store.substitute.tenant-sig-visibility+2]
pub(crate) async fn visible_subset(
    pool: &sqlx::PgPool,
    signer: Option<&TenantSigner>,
    tenant_id: Option<Uuid>,
    present: &[String],
    cache: &mut TrustedSetCache,
) -> Result<HashSet<String>, MetadataError> {
    let Some(tid) = tenant_id else {
        // Anonymous → unfiltered (r[store.tenant.narinfo-filter]).
        return Ok(present.iter().cloned().collect());
    };
    if present.is_empty() {
        return Ok(HashSet::new());
    }

    use sha2::Digest;
    let hashes: Vec<Vec<u8>> = present
        .iter()
        .map(|p| sha2::Sha256::digest(p.as_bytes()).to_vec())
        .collect();
    let own_built = own_built_projection(pool, tid, &hashes).await?;

    let mut visible: HashSet<String> = HashSet::with_capacity(present.len());
    let mut subst_only: Vec<String> = Vec::new();
    for (p, h) in present.iter().zip(&hashes) {
        // r[impl gw.jwt.anon-drv-lookup]
        // .drv files are build inputs, not tenant-owned outputs —
        // exempt per the same (now shared) policy as the single-path
        // body. Without this, `wopQueryValidPaths` reports a .drv
        // missing while `wopIsValidPath` reports it valid for the same
        // path/JWT.
        if drv_exempt(p) {
            visible.insert(p.clone());
            continue;
        }
        match own_built.get(h) {
            Some(&(owned, any_built)) => {
                // The kernel's I-217 table (sig_trusted=false is sound
                // here — K4 proves the verdict ignores it once
                // owned || any_built).
                if matches!(
                    visibility_verdict(owned, any_built, false),
                    VisibilityVerdict::Visible
                ) {
                    visible.insert(p.clone());
                }
            }
            None => subst_only.push(p.clone()),
        }
    }
    if subst_only.is_empty() {
        return Ok(visible);
    }

    let trusted = trusted_set(pool, signer, tid, cache).await?;
    if trusted.is_empty() {
        // Tenant trusts nothing → all substitution-only paths hidden.
        return Ok(visible);
    }
    for (_path, info) in metadata::query_path_info_batch(pool, &subst_only).await? {
        let Some(info) = info else {
            // No complete manifest — hidden (single-path agreement;
            // see the doc comment).
            continue;
        };
        let sig_trusted = sig_cell(&info, &trusted);
        if matches!(
            visibility_verdict(false, false, sig_trusted),
            VisibilityVerdict::Visible
        ) {
            visible.insert(info.store_path.to_string());
        }
    }
    Ok(visible)
}

//! Tenant sig-visibility: the ONE body behind every visibility decision
//! (bug_115; batch entrypoint unified by bug_061 —
//! `r[store.visibility.one-body]`).
//!
//! Factored out of `grpc/sign.rs::sig_visibility_gate` so the
//! materialization walk's local-presence probe and the gRPC read gates
//! — single-path AND batch (`FindMissingPaths`) — decide visibility
//! through the SAME code path: one `(owned, any_built)` projection
//! ([`own_built_projection`]),
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
//! (`r[store.tenant.narinfo-filter]`) — short-circuits BEFORE the
//! kernel table; it's policy about whether visibility applies, not a
//! visibility cell.
//!
//! No `.drv` exemption (`r[store.tenant.valid-paths-filter]`): `.drv`
//! paths go through the same ownership/sig checks as outputs.
//! Exempting them made a cross-tenant `.drv` *valid but unreadable* —
//! the client skipped the upload, then the builder's castore-FUSE read
//! (strict `path_tenants` join, `r[store.castore.tenant-scope+2]`) got
//! NotFound → EIO → infra-retries exhausted. Reporting it invalid
//! instead makes the client re-upload, and the idempotent-skip
//! junction write (`r[store.put.tenant-junction]`) grants this tenant
//! read access.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rio_evidence_kernel::visibility::{VisibilityVerdict, visibility_verdict};
use rio_proto::validated::ValidatedPathInfo;
use uuid::Uuid;

use crate::error::MetadataError;
use crate::metadata;
use crate::signing::TenantSigner;

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

/// Shared per-job memo of `tenant → trusted-key entries` (bug_115
/// economy: the trusted set costs two PG queries; a closure walk
/// consults it for every locally-present path × interested tenant, so
/// it is cached for the job's lifetime — key material changes do not
/// land mid-walk).
///
/// bug_073 — the guard-scope law is a property of this TYPE, not of
/// call-site discipline: the internal `std::sync::Mutex` wraps only
/// pure map lookup/insert, is taken and released inside each method,
/// and never escapes — so no caller can hold it across a foreign
/// await. A reintroduced cross-await hold inside this module is a
/// COMPILE ERROR in the walk: path futures are `Send` by the window's
/// `BoxFuture` bound and `std::sync::MutexGuard` is `!Send`. Per-
/// tenant loads single-flight through a `tokio::sync::OnceCell`
/// (initialized OUTSIDE the map lock): F concurrent siblings missing
/// the same tenant COALESCE on one load — exactly one trusted-set
/// load per (job, tenant), and two paths of one job can never observe
/// different trust sets across a mid-walk key rotation. A failed load
/// leaves the cell empty (a PG blip never poisons the job's memo).
#[derive(Default)]
pub(crate) struct SharedTrustCache {
    by_tenant: std::sync::Mutex<HashMap<Uuid, TrustCell>>,
    /// Test-only loader-invocation counter (the single-flight economy
    /// witness: loads ≤ 1 per (job, tenant)).
    #[cfg(test)]
    pub(crate) loads: std::sync::atomic::AtomicUsize,
}

/// One tenant's single-flight trusted-set cell: cloned OUT of the map
/// lock, initialized (2 PG queries) outside it.
type TrustCell = Arc<tokio::sync::OnceCell<Arc<Vec<String>>>>;

/// The requesting tenant's signature trust set: upstream
/// `trusted_keys` ∪ cluster key (current + prior history) ∪ the
/// tenant's own `tenant_keys` pubkeys. Memoized in `cache`
/// (per-operation locking + per-tenant single-flight — bug_073; the
/// 2-query loader runs OUTSIDE the map lock, inside the tenant's
/// cell).
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
    cache: &SharedTrustCache,
) -> Result<Arc<Vec<String>>, MetadataError> {
    // Map op under the internal guard: lookup-or-insert the tenant's
    // cell, guard released before ANY await (a hold across the loader
    // would be `!Send` in the walk's path futures — compile-checked).
    let cell = {
        let mut map = cache
            .by_tenant
            .lock()
            .expect("trust-cache map lock is never poisoned (no panics under it)");
        Arc::clone(map.entry(tid).or_default())
    };
    cell.get_or_try_init(|| async {
        #[cfg(test)]
        cache
            .loads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        Ok(Arc::new(trusted))
    })
    .await
    .map(Arc::clone)
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
    cache: &SharedTrustCache,
) -> Result<Option<TenantVisible>, MetadataError> {
    let Some(tid) = tenant_id else {
        // Anonymous → unfiltered (r[store.tenant.narinfo-filter]).
        return Ok(Some(TenantVisible(tenant_id.into_iter().collect())));
    };
    // r[impl store.tenant.valid-paths-filter]
    // No `.drv` exemption — `.drv` paths take the same ownership/sig
    // gate as outputs (see module doc for the castore-FUSE rationale).

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
/// (`r[store.tenant.narinfo-filter]`); `.drv` paths get NO exemption
/// (`r[store.tenant.valid-paths-filter]` — same policy as the
/// single-path body); built
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
    cache: &SharedTrustCache,
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
        // r[impl store.tenant.valid-paths-filter]
        // Same policy as the single-path gate above (see module doc):
        // no `.drv` exemption — owned → visible,
        // built-by-another-tenant → hidden, else sig-gated.
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

    // bug_189: the DB-egress fetch runs BEFORE any trust verdict —
    // `query_path_info_batch` validates rows at egress, so a corrupt
    // row surfaces `MalformedRow` for a trust-nothing tenant EXACTLY
    // as it does for a trusting one (one disposition with the
    // single-path lane). The binding is the validated-row witness
    // every branch below must consume; the empty-trust optimization
    // may skip the sig cell, never the egress check. (Pre-fix the
    // empty-trust early return preceded the fetch, answering the
    // corrupt row "missing" on the batch lane while the single lane
    // surfaced Internal.)
    let rows = metadata::query_path_info_batch(pool, &subst_only).await?;
    let trusted = trusted_set(pool, signer, tid, cache).await?;
    if trusted.is_empty() {
        // Tenant trusts nothing → all substitution-only paths hidden
        // (the rows above were still validated).
        return Ok(visible);
    }
    for (_path, info) in rows {
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

/// Batched sig-visibility for SUBSTITUTION-ONLY paths, keyed by
/// `narinfo.store_path_hash`: returns the subset of `hashes` whose
/// stored narinfo carries a signature `tid` trusts (the SAME
/// predicate as [`sig_cell`], over the SAME [`trusted_set`]).
///
/// Callers own the substitution-only precondition (zero
/// `path_tenants` rows for each hash) — this answers only the
/// signature half of the predicate. Hashes absent from `narinfo` are
/// hidden (same as the single-path gate's NotFound). The castore
/// read fallback (`grpc/directory.rs`) consults THIS body so a path
/// the validity gates report visible is always READABLE —
/// `r[store.tenant.valid-paths-filter]` requires "valid ⇒ readable"
/// to hold per caller, which only stays true if both surfaces
/// evaluate the SAME predicate (`r[store.visibility.one-body]`).
///
/// PK lookup — never a narinfo seq scan, so the fallback arm stays
/// index-only even for large batches.
// r[impl store.substitute.tenant-sig-visibility+2]
// r[impl store.castore.tenant-scope+2]
pub(crate) async fn sig_visible_path_hashes(
    pool: &sqlx::PgPool,
    signer: Option<&TenantSigner>,
    tid: Uuid,
    hashes: &[Vec<u8>],
    cache: &SharedTrustCache,
) -> Result<HashSet<Vec<u8>>, MetadataError> {
    if hashes.is_empty() {
        return Ok(HashSet::new());
    }
    let trusted = trusted_set(pool, signer, tid, cache).await?;
    if trusted.is_empty() {
        // Tenant trusts no keys at all → every substitution-only path
        // is invisible.
        return Ok(HashSet::new());
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        store_path_hash: Vec<u8>,
        store_path: String,
        nar_hash: Vec<u8>,
        nar_size: i64,
        references: Vec<String>,
        signatures: Vec<String>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT store_path_hash, store_path, nar_hash, nar_size, \"references\", signatures \
         FROM narinfo WHERE store_path_hash = ANY($1::bytea[])",
    )
    .bind(hashes)
    .fetch_all(pool)
    .await?;
    let mut visible = HashSet::with_capacity(rows.len());
    for r in rows {
        let Ok(nar_hash): Result<[u8; 32], _> = r.nar_hash.as_slice().try_into() else {
            // Malformed row — hide (defensive; query_path_info would
            // MalformedRow on it).
            continue;
        };
        // The [`sig_cell`] body over raw row fields (no
        // ValidatedPathInfo at this layer — the castore fallback
        // works in store_path_hash space).
        let fp = rio_nix::narinfo::fingerprint(
            &r.store_path,
            &nar_hash,
            r.nar_size as u64,
            &r.references,
        );
        if crate::signing::any_sig_trusted(&r.signatures, &trusted, &fp).is_some() {
            visible.insert(r.store_path_hash);
        }
    }
    Ok(visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R2-073 / W-073c — certifies: the memo's economy and consistency
    /// survive the per-operation granularity change — exactly ONE
    /// trusted-set load per (job, tenant) under F concurrent misses;
    /// siblings COALESCE on the in-flight load and observe the same
    /// set. GREEN-SIDE BY CONSTRUCTION (disclosed rationale, the
    /// round-6 class): this pins the close's ADDED property, not the
    /// defect — the single-flight cell does not exist pre-fix (the
    /// job-wide mutex serialized the second caller into a map hit,
    /// also count 1), and the defect itself is pinned by the
    /// probe-rendezvous red in executor.rs
    /// (`probe_phase_admits_concurrent_paths`).
    #[tokio::test]
    async fn trusted_set_loads_once_per_tenant_under_concurrent_misses() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let tenant = crate::test_helpers::seed_tenant(&db.pool, "trust-singleflight").await;
        metadata::upstreams::insert(
            &db.pool,
            tenant,
            "http://127.0.0.1:1/unreachable-never-contacted",
            50,
            &["cache.sf:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()],
            crate::metadata::upstreams::SigMode::Keep,
        )
        .await
        .expect("upstream row seeded (trusted_set reads the table only)");

        let cache = SharedTrustCache::default();
        // F = 2 concurrent misses on the SAME tenant: both callers
        // race to the cell; one runs the loader, the sibling awaits
        // the in-flight init.
        let (a, b) = tokio::join!(
            trusted_set(&db.pool, None, tenant, &cache),
            trusted_set(&db.pool, None, tenant, &cache),
        );
        let (a, b) = (a.expect("load a"), b.expect("load b"));
        assert!(
            Arc::ptr_eq(&a, &b),
            "siblings coalesce on ONE memoized entry (same Arc)"
        );
        assert!(
            a.iter().any(|k| k.starts_with("cache.sf:")),
            "the loaded set carries the seeded upstream key"
        );
        assert_eq!(
            cache.loads.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "exactly one loader run for two concurrent same-tenant misses"
        );
    }
}

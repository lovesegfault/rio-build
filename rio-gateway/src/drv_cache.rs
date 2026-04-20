//! Per-session derivation cache and the I/O helpers that fill it.
//!
//! Extracted to break the `handler` ↔ `translate` import cycle:
//! `translate::reconstruct_dag` needs `resolve_derivations_batch` for
//! its BFS; the cache helpers need `max_transitive_inputs` for the
//! cap. Both used to live in the other module's file. Now both live
//! here; `handler` and `translate` each depend on this module and not
//! on each other.

use std::collections::HashMap;
use std::sync::OnceLock;

use futures_util::{StreamExt, stream};
use rio_nix::derivation::Derivation;
use rio_nix::store_path::StorePath;
use rio_proto::StoreServiceClient;
use tonic::transport::Channel;
use tracing::{debug, warn};

use crate::handler::GatewayError;
use crate::handler::grpc::grpc_get_path;

/// Default cap on transitive input derivations resolved per build (DoS guard).
/// Matches `rio_nix::protocol::wire::MAX_COLLECTION_COUNT` — the wire layer
/// already bounds at 1M, so a tighter cap here only rejects DAGs the wire
/// admitted (I-016: 10k→100k; I-135: 100k→1M after hello-deep-1024x's >100k
/// .drv closure). Memory: 1M parsed `Derivation` ≈ 1–3 GB/session at the cap;
/// gateway pod limit raised to 4 GiB (I-134) to accommodate. Override via
/// `RIO_MAX_TRANSITIVE_INPUTS`.
pub const DEFAULT_MAX_TRANSITIVE_INPUTS: usize = 1_048_576;

/// Process-global limit. Set once via [`init_max_transitive_inputs`] in main()
/// AFTER config load. Same OnceLock pattern as `rio_common::grpc::CLIENT_TLS`
/// — threading this through `reconstruct_dag` + `resolve_derivation` +
/// `try_cache_drv` (~18 call sites including tests) is invasive for a value
/// that IS process-global (one DoS guard, not per-session policy).
static MAX_TRANSITIVE_INPUTS: OnceLock<usize> = OnceLock::new();

/// Set the process-wide transitive-input cap. Call ONCE in main(), after
/// loading config but before any SSH session can call `reconstruct_dag`.
/// Calling twice is a silent no-op (OnceLock semantics — first wins).
pub fn init_max_transitive_inputs(n: usize) {
    let _ = MAX_TRANSITIVE_INPUTS.set(n);
}

#[cfg(test)]
thread_local! {
    /// Per-test override. OnceLock above is process-global (first test
    /// to call `init_` wins, all others see its value); this lets a
    /// single test set a small cap without poisoning siblings.
    /// Thread-local is safe under `#[tokio::test]` default
    /// `current_thread` runtime — `reconstruct_dag` runs on the test
    /// task, no `spawn` crosses threads.
    static TEST_CAP: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Per-test override of [`max_transitive_inputs()`]. Scoped to the
/// calling thread; subsequent tests on the same nextest worker see the
/// default again (each `#[test]` is a fresh thread).
#[cfg(test)]
pub(crate) fn override_max_transitive_inputs(n: usize) {
    TEST_CAP.with(|c| c.set(Some(n)));
}

/// Current cap. Falls back to [`DEFAULT_MAX_TRANSITIVE_INPUTS`] if main()
/// never called init (tests, or a future binary that forgot to wire it).
pub(crate) fn max_transitive_inputs() -> usize {
    #[cfg(test)]
    if let Some(n) = TEST_CAP.with(|c| c.get()) {
        return n;
    }
    *MAX_TRANSITIVE_INPUTS
        .get()
        .unwrap_or(&DEFAULT_MAX_TRANSITIVE_INPUTS)
}

/// Maximum NAR size to buffer for `.drv` caching. Above this, the NAR
/// streams directly to the store and [`try_cache_drv`] is skipped —
/// `resolve_derivation` fetches from the store later during DAG
/// reconstruction (one extra round-trip, correctness unchanged).
/// 16 MiB covers observed outliers (`options.json.drv` ≈ 9.7MB) with
/// headroom; typical `.drv` NARs are <10KB.
pub(crate) const DRV_NAR_BUFFER_LIMIT: u64 = 16 * 1024 * 1024;

/// Max in-flight `GetPath` calls during BFS .drv resolution. The store's
/// `inline_blob` reads are tiny (.drv NARs are KB-range) so the bound is
/// mostly to cap connection-pool fan-out, same rationale as the
/// scheduler's `DEFAULT_SUBSTITUTE_CONCURRENCY`. 32 matches I-052's
/// `wopAddMultipleToStore` pipeline depth.
pub(crate) const BFS_FETCH_CONCURRENCY: usize = 32;

/// If `path` is a `.drv`, parse the ATerm from NAR data and cache it.
/// Cap drv_cache at [`max_transitive_inputs()`]. The cache
/// is session-scoped; a client uploading >cap .drv files would consume
/// cap * (avg drv size ~1KB parsed) per session. The cap matches the
/// BFS limit in translate::reconstruct_dag — a DAG bigger than that
/// would be rejected anyway, so caching more .drvs is wasted.
///
/// Returns true if inserted, false if cap hit. Caller decides what
/// to do (try_cache_drv logs + continues; resolve_derivation
/// returns an error to the client).
fn insert_drv_bounded(
    drv_cache: &mut HashMap<StorePath, Derivation>,
    path: StorePath,
    drv: Derivation,
) -> bool {
    if drv_cache.len() >= max_transitive_inputs() && !drv_cache.contains_key(&path) {
        return false;
    }
    drv_cache.insert(path, drv);
    true
}

/// Map a `grpc_get_path` result to a parsed [`Derivation`]. Shared
/// fetch→parse tail of [`resolve_derivation`] / [`resolve_derivations_batch`].
fn parse_fetched_drv(
    path: &StorePath,
    fetched: Option<(rio_proto::validated::ValidatedPathInfo, Vec<u8>)>,
) -> anyhow::Result<Derivation> {
    let (_info, nar) = fetched.ok_or_else(|| GatewayError::DerivationNotFound(path.to_string()))?;
    Derivation::parse_from_nar(&nar).map_err(|e| {
        GatewayError::DerivationParse {
            path: path.to_string(),
            msg: e.to_string(),
        }
        .into()
    })
}

/// Best-effort: if `path` is a `.drv`, parse the ATerm from NAR data and
/// cache it. Logs and continues on parse error or cap hit — the upload
/// itself still proceeds.
pub(crate) fn try_cache_drv(
    path: &StorePath,
    nar_data: &[u8],
    drv_cache: &mut HashMap<StorePath, Derivation>,
) {
    if !path.is_derivation() {
        return;
    }
    match Derivation::parse_from_nar(nar_data) {
        Ok(drv) => {
            if insert_drv_bounded(drv_cache, path.clone(), drv) {
                debug!(path = %path, "cached parsed derivation");
            } else {
                // Log at warn (not per-insert spam): every subsequent insert
                // at cap also fails. The upload itself still succeeds.
                warn!(
                    path = %path,
                    cap = max_transitive_inputs(),
                    "drv_cache at cap; not caching (upload still proceeds)"
                );
            }
        }
        Err(e) => {
            warn!(path = %path, error = %e, "failed to parse .drv from NAR");
        }
    }
}

/// Look up a derivation from session cache, or fetch from store via gRPC,
/// parse the ATerm, and cache it.
///
/// NOTE: `.drv` lookups use ANONYMOUS store access (no JWT). A `.drv`
/// is a build INPUT — it may have been uploaded via a different tenant
/// context (e.g., `nix copy` with default key, then `nix build` with
/// tenant key). Tenant-scoping input resolution breaks cross-context
/// build flows: the store's `path_tenants` table has no row for the
/// `.drv` under the building tenant, so tenant-filtered `GetPath`
/// returns NotFound → "not a valid store path". JWT propagation is
/// for OUTPUT reads (`handle_query_path_info`, `handle_nar_from_path`,
/// etc.) where tenant-scoped visibility is the correct semantics.
pub(crate) async fn resolve_derivation(
    drv_path: &StorePath,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<Derivation> {
    if let Some(cached) = drv_cache.get(drv_path) {
        return Ok(cached.clone());
    }

    let drv = parse_fetched_drv(
        drv_path,
        grpc_get_path(store_client, None, drv_path.as_str()).await?,
    )?;

    // Bound drv_cache. resolve_derivation is called from BFS in
    // translate::reconstruct_dag — cap hit means the DAG is too large
    // (the BFS enforces the same cap, but the cache could grow beyond
    // it across multiple builds in one session). Error propagates as
    // DAG failure.
    if !insert_drv_bounded(drv_cache, drv_path.clone(), drv.clone()) {
        return Err(GatewayError::DrvCacheFull {
            count: drv_cache.len(),
            cap: max_transitive_inputs(),
        }
        .into());
    }
    Ok(drv)
}

/// Batch counterpart to [`resolve_derivation`] for the BFS in
/// `translate::reconstruct_dag`. Fires up to [`BFS_FETCH_CONCURRENCY`]
/// concurrent `GetPath` calls for the cache MISSES in `paths`, parses
/// each NAR, inserts into `drv_cache`, and returns `(StorePath,
/// Derivation)` for EVERY requested path (hits and freshly fetched) so
/// the caller can enqueue the next BFS level without re-probing the
/// cache. Order is NOT preserved (buffer_unordered) — the BFS only needs
/// the set.
///
/// P0539: the per-child `resolve_derivation().await` in the old BFS was
/// ~1085 sequential RTTs to rio-store for a hello-shallow closure
/// (~210s). Level-batching collapses that to roughly DAG-depth ×
/// ceil(level-width / 32) RTTs.
///
/// Same anonymous-lookup semantics as [`resolve_derivation`] (no JWT —
/// `.drv`s are build inputs).
pub(crate) async fn resolve_derivations_batch(
    paths: Vec<StorePath>,
    store_client: &StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<Vec<(StorePath, Derivation)>> {
    let mut resolved = Vec::with_capacity(paths.len());
    let mut to_fetch = Vec::new();
    for p in paths {
        match drv_cache.get(&p) {
            Some(d) => resolved.push((p, d.clone())),
            None => to_fetch.push(p),
        }
    }
    if to_fetch.is_empty() {
        return Ok(resolved);
    }
    // Defensive self-contained gate. The caller (`reconstruct_dag`) now
    // checks `count + frontier.len()` BEFORE calling us, but this
    // function is the chokepoint where an unbounded `to_fetch` becomes
    // an unbounded `Vec<Derivation>` in memory — keep the check local
    // so a future caller can't bypass the budget. Gate on
    // `drv_cache.len() + to_fetch.len()` so the pre-fetch check rejects
    // exactly what `insert_drv_bounded` would reject post-fetch — with
    // `to_fetch.len()` alone, a session that fills `drv_cache` to ~cap
    // via `try_cache_drv` then issues a disjoint ~cap-miss build would
    // pass here, fully materialize `fetched` (~cap parsed Derivations),
    // and only then error: peak ≈ 2× the budget the 4 GiB pod limit was
    // sized for.
    let cap = max_transitive_inputs();
    if drv_cache.len() + to_fetch.len() > cap {
        return Err(GatewayError::DrvCacheFull {
            count: drv_cache.len() + to_fetch.len(),
            cap,
        }
        .into());
    }

    // tonic clients are cheap clones over a shared Channel; each
    // concurrent task gets its own clone so calls don't serialize on a
    // &mut. Results stream back unordered.
    let mut fetched: Vec<(StorePath, Derivation)> = stream::iter(to_fetch)
        .map(|sp| {
            let mut client = store_client.clone();
            async move {
                let drv =
                    parse_fetched_drv(&sp, grpc_get_path(&mut client, None, sp.as_str()).await?)?;
                Ok::<_, anyhow::Error>((sp, drv))
            }
        })
        .buffer_unordered(BFS_FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<anyhow::Result<_>>()?;

    for (sp, drv) in &fetched {
        if !insert_drv_bounded(drv_cache, sp.clone(), drv.clone()) {
            return Err(GatewayError::DrvCacheFull {
                count: drv_cache.len(),
                cap: max_transitive_inputs(),
            }
            .into());
        }
    }
    resolved.append(&mut fetched);
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_drv(tag: &str) -> (StorePath, Derivation) {
        let path = format!("/nix/store/{}-{tag}.drv", "a".repeat(32));
        let aterm = format!(
            r#"Derive([("out","/nix/store/{}-{tag}-out","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#,
            "b".repeat(32)
        );
        (
            StorePath::parse(&path).unwrap(),
            Derivation::parse(&aterm).unwrap(),
        )
    }

    /// Pre-fetch gate must account for existing `drv_cache` occupancy,
    /// not just `to_fetch.len()`. With cap=4, drv_cache pre-filled to 3,
    /// and 2 cache-miss paths requested: gate rejects with count=5
    /// BEFORE any GetPath fires. Pre-fix: `to_fetch.len()=2 <= 4` →
    /// gate passed → both fetched → `insert_drv_bounded` errored AFTER
    /// the Vec<Derivation> was fully materialized (peak ≈ 2× budget).
    #[tokio::test]
    async fn resolve_derivations_batch_gate_accounts_for_existing_cache() {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        override_max_transitive_inputs(4);

        let (store, store_client, _h) = spawn_mock_store_with_client().await.unwrap();

        // Pre-fill drv_cache with 3 entries (distinct keys).
        let mut drv_cache = HashMap::new();
        for i in 0..3 {
            let (sp, drv) = parse_drv(&format!("cached{i}"));
            drv_cache.insert(sp, drv);
        }

        // Two cache-miss paths. Neither is in drv_cache nor in MockStore
        // — but the gate must fire before any fetch.
        let (miss_a, _) = parse_drv("miss-a");
        let (miss_b, _) = parse_drv("miss-b");

        let err = resolve_derivations_batch(vec![miss_a, miss_b], &store_client, &mut drv_cache)
            .await
            .expect_err("3 cached + 2 miss > cap=4 must reject pre-fetch");

        let gw_err = err.downcast_ref::<GatewayError>().expect("GatewayError");
        match gw_err {
            GatewayError::DrvCacheFull { count, cap } => {
                assert_eq!(*count, 5, "count = drv_cache.len() + to_fetch.len()");
                assert_eq!(*cap, 4);
            }
            other => panic!("expected DrvCacheFull, got {other:?}"),
        }

        // Structural proof: rejection happened BEFORE fetch. MockStore
        // records every GetPath's manifest_hint — empty ⇒ never called.
        assert!(
            store.calls.get_path_hints.read().unwrap().is_empty(),
            "gate must reject BEFORE any GetPath — pre-fix this was non-empty \
             (fetch happened, then insert_drv_bounded errored)"
        );
    }
}

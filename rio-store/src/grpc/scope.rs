//! Closure-scoped castore reads (ADR-022 P0591).
//!
//! A build's assignment token is the builder's credential on the
//! castore read surface (`DirectoryService`). Tenant scoping alone
//! (`r[store.castore.tenant-scope+2]`) makes a leaked token a
//! tenant-wide read capability for its remaining lifetime; this module
//! narrows it to **exactly the input closure the scheduler signed for
//! that build** — the byte set the build itself mounts.
//!
//! Mechanism: the builder presents `WorkAssignment.input_closure` once
//! per mount / new channel via `PresentClosure`; [`CastoreScope::establish`]
//! recomputes `blake3(sorted closure)` exactly as the upload path's
//! `Begin` validation does, requires equality with the token's signed
//! `input_closure_digest`, keys every entry as
//! `StorePath::sha256_digest()` (the `path_tenants`/`directory_paths`/
//! `file_blobs` junction key) and caches the resulting [`ScopeSet`] in
//! RAM keyed by the closure digest itself. Every castore read carrying
//! an assignment token then resolves through [`CastoreScope::resolve`],
//! which yields a [`ReadScope`] the query sites consume: the membership
//! predicate is ANDed with (never replaces) the existing tenant join.
//!
//! Zero Postgres tables, zero per-dispatch writes: the only state is a
//! per-replica capacity-capped, idle-TTL'd cache, plus a short-TTL
//! cache of server-side *derived* scopes (rebuilt from
//! `scheduler_live_pins` + `narinfo."references"`) used only when a
//! read arrives before any presentation reached this replica.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tonic::Status;
use tracing::{info, warn};

use rio_auth::hmac::AssignmentClaims;
use rio_common::limits::MAX_INPUT_CLOSURE;
use rio_nix::store_path::StorePath;

/// Default capacity (in bytes of cached digests) for the presented
/// ScopeSet cache. A typical 800-path closure is ~26 KB; the
/// `MAX_INPUT_CLOSURE` cap is ~2 MiB — 256 MiB holds thousands of
/// concurrent distinct closures, and identical closures dedupe by
/// content address.
pub const DEFAULT_CACHE_CAPACITY_BYTES: u64 = 256 * 1024 * 1024;

/// Default idle TTL for presented ScopeSets. An in-flight build touches
/// its scope on every cold read, so only genuinely idle entries expire;
/// eviction is harmless either way (the builder re-presents on demand,
/// and the derivation fallback covers the gap).
pub const DEFAULT_CACHE_IDLE_TTL_SECS: u64 = 3600;

/// TTL for server-side *derived* scopes (`scheduler_live_pins` +
/// `narinfo."references"` walk). Deliberately short: a derived scope is
/// not attested and can under-cover paths that land via substitution
/// after the walk, so it must be recomputed often enough to self-heal.
const DERIVED_SCOPE_TTL_SECS: u64 = 60;

/// Byte budget for the derived-scope cache. Small on purpose — derived
/// scopes exist only to carry reads that arrive before a presentation
/// reaches this replica.
const DERIVED_SCOPE_CAPACITY_BYTES: u64 = 64 * 1024 * 1024;

/// Closure-scope enforcement mode (`[castore_read_scope].mode`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum ScopeMode {
    /// No scope handling at all: behavior identical to tenant scoping
    /// alone. Dev parity / emergency escape hatch.
    Off,
    /// Resolve and compare scopes but never reject: out-of-scope reads
    /// are served and counted (`would_deny`), absent scopes are served
    /// and counted (`scope_absent`). The rollback value now that
    /// `enforce` is the shipped default (it was the interim default for
    /// the store-only Phase 1, before the builder presented closures).
    Log,
    /// Reject out-of-scope reads with `NOT_FOUND`, unattested tokens
    /// with `PERMISSION_DENIED`, and unresolvable scopes with
    /// `FAILED_PRECONDITION` + [`rio_proto::CASTORE_SCOPE_REQUIRED_MSG`].
    ///
    /// The shipped default (ADR-022 closure-scoped reads, decision 9)
    /// since the builder-side presentation landed (P0591 Phase 2): the
    /// builder presents `WorkAssignment.input_closure` at mount and
    /// re-presents on `CASTORE_SCOPE_REQUIRED`, and the pins+references
    /// derivation fallback carries replicas a presentation has not
    /// reached.
    #[default]
    Enforce,
}

/// An established closure read scope: the deduplicated, sorted set of
/// `store_path_hash` keys (SHA-256 of the full store-path string) named
/// by one input closure.
///
/// ~32 bytes per path: ~26 KB for a typical 800-path closure, ~2 MiB at
/// the [`MAX_INPUT_CLOSURE`] cap.
#[derive(Debug, PartialEq, Eq)]
pub struct ScopeSet {
    /// Sorted + deduped so membership is a binary search and the SQL
    /// bind is deterministic.
    hashes: Vec<[u8; 32]>,
}

impl ScopeSet {
    /// Key every closure entry as `StorePath::sha256_digest()` — the
    /// junction-table key `path_tenants`/`directory_paths`/`file_blobs`/
    /// `narinfo` all share. Errors on the first entry that is not a
    /// parseable store path (the attested closure is scheduler-produced,
    /// so this only fires on a hand-rolled presentation).
    pub fn from_closure(closure: &[String]) -> Result<Self, String> {
        let mut hashes = Vec::with_capacity(closure.len());
        for p in closure {
            let parsed = StorePath::parse(p)
                .map_err(|e| format!("closure entry is not a valid store path: {e}"))?;
            hashes.push(parsed.sha256_digest());
        }
        Ok(Self::from_hashes(hashes))
    }

    /// Build from already-keyed `store_path_hash` values (the
    /// derivation fallback reads them straight from Postgres).
    pub fn from_hashes(mut hashes: Vec<[u8; 32]>) -> Self {
        hashes.sort_unstable();
        hashes.dedup();
        Self { hashes }
    }

    /// Membership test for one containing-path hash.
    pub fn contains(&self, store_path_hash: &[u8; 32]) -> bool {
        self.hashes.binary_search(store_path_hash).is_ok()
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// The sorted hashes, for the `= ANY($scope)` SQL bind.
    pub fn hashes(&self) -> &[[u8; 32]] {
        &self.hashes
    }

    /// Approximate resident size, used as the moka cache weight.
    fn weight(&self) -> u32 {
        u32::try_from(self.hashes.len().saturating_mul(32) + 64).unwrap_or(u32::MAX)
    }
}

/// Identity of the scoped caller, carried alongside the decision so
/// deny/would-deny logging can name the build without re-parsing the
/// token.
#[derive(Debug, Clone)]
pub struct ScopeCtx {
    pub drv_hash: String,
    pub executor_id: String,
    /// Hex closure digest from the token claim (empty for unattested
    /// tokens observed in `log` mode).
    pub closure_digest: String,
}

/// Per-request scope decision the castore query sites consume.
///
/// Error outcomes (unattested token under enforce, unresolvable scope
/// under enforce, presentation mismatch) never reach the query sites —
/// [`CastoreScope::resolve`] returns them as `Err(Status)`.
#[derive(Debug, Clone)]
pub enum ReadScope {
    /// No membership filtering and no accounting: JWT caller (tenant-
    /// wide by design), `mode = "off"`, or a `log`-mode situation that
    /// only gets counted (absent scope / unattested token).
    Unscoped,
    /// `enforce`: the serving SQL binds the scope array; a digest whose
    /// containing paths are all outside the scope yields no rows.
    Enforce(Arc<ScopeSet>, ScopeCtx),
    /// `log` with a resolvable scope: serve exactly as today (NULL
    /// bind) but probe membership and count would-denies.
    Audit(Arc<ScopeSet>, ScopeCtx),
}

impl ReadScope {
    /// The `$scope` SQL bind for the *serving* query: `Some(hashes)`
    /// only under enforce, `None` (SQL NULL — predicate passes) for
    /// JWT/off/log callers.
    pub fn sql_bind(&self) -> Option<Vec<Vec<u8>>> {
        match self {
            ReadScope::Enforce(set, _) => Some(set.hashes().iter().map(|h| h.to_vec()).collect()),
            ReadScope::Audit(..) | ReadScope::Unscoped => None,
        }
    }
}

/// Per-replica scope state: mode + the presented and derived ScopeSet
/// caches. One instance per `DirectoryServiceImpl`, shared by all
/// castore read RPCs and the `PresentClosure` handler.
pub struct CastoreScope {
    mode: ScopeMode,
    /// Verified presentations, keyed by the hex closure digest (the
    /// content address of the closure itself — identical closures and
    /// re-dispatches dedupe naturally).
    presented: moka::future::Cache<String, Arc<ScopeSet>>,
    /// Server-side derived scopes (`§3.5` fallback), keyed by
    /// `drv_hash`. Short TTL; used only on presentation miss under
    /// `enforce`. The key is deliberately the build, not the attested
    /// closure digest: the derived set reflects the current
    /// pins+references state, so concurrent dispatches of the same drv
    /// share one entry for at most the TTL — at worst a slightly
    /// stale-but-still-build-bounded scope until re-derivation.
    derived: moka::future::Cache<String, Arc<ScopeSet>>,
    /// Deny/would-deny log sampler sequence (logs are sampled; the
    /// counters are exact).
    log_seq: AtomicU64,
}

impl CastoreScope {
    /// Build from config. `cache_capacity_bytes` bounds the presented
    /// cache by total digest bytes; `idle_ttl` evicts scopes no read
    /// has touched for that long.
    pub fn new(mode: ScopeMode, cache_capacity_bytes: u64, idle_ttl: Duration) -> Self {
        let weigher = |_k: &String, v: &Arc<ScopeSet>| v.weight().saturating_add(64);
        Self {
            mode,
            presented: moka::future::Cache::builder()
                .max_capacity(cache_capacity_bytes)
                .weigher(weigher)
                .time_to_idle(idle_ttl.max(Duration::from_secs(1)))
                .build(),
            derived: moka::future::Cache::builder()
                .max_capacity(DERIVED_SCOPE_CAPACITY_BYTES)
                .weigher(weigher)
                .time_to_live(Duration::from_secs(DERIVED_SCOPE_TTL_SECS))
                .build(),
            log_seq: AtomicU64::new(0),
        }
    }

    /// A `mode = off` instance with default cache sizes — what
    /// `DirectoryServiceImpl::new` starts with so test fixtures and
    /// callers that never configure scoping keep today's behavior.
    pub fn disabled() -> Self {
        Self::new(
            ScopeMode::Off,
            DEFAULT_CACHE_CAPACITY_BYTES,
            Duration::from_secs(DEFAULT_CACHE_IDLE_TTL_SECS),
        )
    }

    pub fn mode(&self) -> ScopeMode {
        self.mode
    }

    /// `PresentClosure` core: cap → digest equality → keying → cache
    /// insert. Idempotent, no Postgres. Mismatch is `INVALID_ARGUMENT`
    /// in every mode and never echoes stored data.
    ///
    /// Returns the number of distinct paths in the established scope.
    // r[impl store.castore.scope-establish]
    pub async fn establish(
        &self,
        claims: &AssignmentClaims,
        closure: &[String],
    ) -> Result<usize, Status> {
        let started = Instant::now();
        if claims.input_closure_digest.is_empty() {
            // Nothing was attested at dispatch, so nothing can be
            // verified here. Same status class as a mismatch.
            metrics::counter!("rio_store_castore_scope_mismatch_total").increment(1);
            return Err(Status::invalid_argument(
                "assignment token carries no input_closure_digest; nothing to present",
            ));
        }
        if closure.len() > MAX_INPUT_CLOSURE {
            // Counted with the mismatches: every PresentClosure
            // verification rejection lands in the same counter.
            metrics::counter!("rio_store_castore_scope_mismatch_total").increment(1);
            return Err(Status::invalid_argument(format!(
                "closure has {} entries, exceeds MAX_INPUT_CLOSURE {MAX_INPUT_CLOSURE}",
                closure.len()
            )));
        }
        // Same check the upload path runs on `Begin.input_closure`:
        // sort the raw strings, then digest. The builder's wire order
        // is irrelevant; the digest is over the canonical (sorted) form.
        let mut sorted = closure.to_vec();
        sorted.sort_unstable();
        let computed = AssignmentClaims::digest_input_closure(&sorted);
        if computed != claims.input_closure_digest {
            metrics::counter!("rio_store_castore_scope_mismatch_total").increment(1);
            warn!(
                drv_hash = %claims.drv_hash,
                executor_id = %claims.executor_id,
                presented_entries = closure.len(),
                "PresentClosure rejected: presented closure does not match the signed digest"
            );
            return Err(Status::invalid_argument(
                "presented closure does not match the assignment's input_closure_digest",
            ));
        }
        // Idempotent re-present: the scope is content-addressed by the
        // digest, so a hit is necessarily the same set.
        if let Some(existing) = self.presented.get(&claims.input_closure_digest).await {
            return Ok(existing.len());
        }
        let set = Arc::new(ScopeSet::from_closure(&sorted).map_err(|e| {
            // An entry that cannot be keyed is a verification rejection
            // like any other — count it with the mismatches.
            metrics::counter!("rio_store_castore_scope_mismatch_total").increment(1);
            Status::invalid_argument(e)
        })?);
        self.presented
            .insert(claims.input_closure_digest.clone(), Arc::clone(&set))
            .await;
        metrics::counter!("rio_store_castore_scope_established_total").increment(1);
        metrics::histogram!("rio_store_castore_scope_establish_seconds")
            .record(started.elapsed().as_secs_f64());
        info!(
            drv_hash = %claims.drv_hash,
            executor_id = %claims.executor_id,
            closure_digest = %claims.input_closure_digest,
            paths = set.len(),
            "established castore read scope"
        );
        Ok(set.len())
    }

    /// Resolve the scope decision for one castore read by an
    /// assignment-token caller, per the failure-mode policy:
    ///
    /// | situation                  | log                        | enforce                                  |
    /// |----------------------------|----------------------------|------------------------------------------|
    /// | scope presented            | `Audit` (serve + compare)  | `Enforce` (predicate in SQL)             |
    /// | scope absent               | serve + `scope_absent`     | derive from pins+references, else `FAILED_PRECONDITION` + `CASTORE_SCOPE_REQUIRED` |
    /// | unattested token           | serve + `would_deny`       | `PERMISSION_DENIED` (no tenant-wide fallback) |
    ///
    /// Never widens access and never silently falls back to tenant-wide
    /// under enforce.
    ///
    /// `rpc` names the calling read RPC for the deny log/metrics.
    // r[impl store.castore.closure-scope]
    pub async fn resolve(
        &self,
        claims: &AssignmentClaims,
        pool: &PgPool,
        rpc: &'static str,
    ) -> Result<ReadScope, Status> {
        if self.mode == ScopeMode::Off {
            return Ok(ReadScope::Unscoped);
        }
        let ctx = ScopeCtx {
            drv_hash: claims.drv_hash.clone(),
            executor_id: claims.executor_id.clone(),
            closure_digest: claims.input_closure_digest.clone(),
        };
        // Unattested token: the scheduler signed no closure digest at
        // dispatch (degraded dispatch / pre-P0589 token). Under enforce
        // there is deliberately no tenant-wide fallback.
        if claims.input_closure_digest.is_empty() {
            return match self.mode {
                ScopeMode::Enforce => {
                    metrics::counter!(
                        "rio_store_castore_scope_denied_total",
                        "reason" => "unattested"
                    )
                    .increment(1);
                    self.log_denied(true, rpc, "unattested", &ctx, None);
                    // Same opaque phrasing as every other token
                    // rejection on this surface.
                    Err(Status::permission_denied("assignment token rejected"))
                }
                ScopeMode::Log => {
                    metrics::counter!(
                        "rio_store_castore_scope_would_deny_total",
                        "reason" => "unattested"
                    )
                    .increment(1);
                    self.log_denied(false, rpc, "unattested", &ctx, None);
                    Ok(ReadScope::Unscoped)
                }
                ScopeMode::Off => unreachable!("handled above"),
            };
        }
        // Presented scope, content-addressed by the signed digest.
        if let Some(set) = self.presented.get(&claims.input_closure_digest).await {
            return Ok(match self.mode {
                ScopeMode::Enforce => ReadScope::Enforce(set, ctx),
                ScopeMode::Log => ReadScope::Audit(set, ctx),
                ScopeMode::Off => unreachable!("handled above"),
            });
        }
        // Scope absent on this replica (never presented here, or evicted).
        match self.mode {
            ScopeMode::Log => {
                metrics::counter!(
                    "rio_store_castore_scope_absent_total",
                    "resolution" => "served"
                )
                .increment(1);
                Ok(ReadScope::Unscoped)
            }
            ScopeMode::Enforce => {
                // §3.5 derivation fallback: rebuild the scope from the
                // build's dispatch-time pins + the references DAG the
                // store already holds. Required for a leaderless
                // replica set behind per-request balancing — a replica
                // may legitimately see a read before any presentation.
                if let Some(set) = self.derive(pool, &claims.drv_hash).await {
                    metrics::counter!(
                        "rio_store_castore_scope_absent_total",
                        "resolution" => "derived"
                    )
                    .increment(1);
                    Ok(ReadScope::Enforce(set, ctx))
                } else {
                    metrics::counter!(
                        "rio_store_castore_scope_absent_total",
                        "resolution" => "denied"
                    )
                    .increment(1);
                    warn!(
                        rpc,
                        drv_hash = %ctx.drv_hash,
                        executor_id = %ctx.executor_id,
                        closure_digest = %ctx.closure_digest,
                        "castore read scope not resolvable on this replica; \
                         asking the builder to present"
                    );
                    Err(Status::failed_precondition(
                        rio_proto::CASTORE_SCOPE_REQUIRED_MSG,
                    ))
                }
            }
            ScopeMode::Off => unreachable!("handled above"),
        }
    }

    /// Rebuild a ScopeSet server-side from `scheduler_live_pins` (the
    /// dispatch-time seeds for this `drv_hash`) plus a recursive
    /// `narinfo."references"` walk — the same reachability data GC mark
    /// uses. RAM-only, TTL'd, used only on presentation miss; the
    /// presented closure remains the primary, attested path.
    ///
    /// Returns `None` (⇒ scope stays unresolved) when the build has no
    /// pins on this Postgres, when the walk exceeds the closure cap
    /// (a truncated scope would cause false denies), or on query error
    /// (never fail open).
    // r[impl store.castore.closure-scope]
    async fn derive(&self, pool: &PgPool, drv_hash: &str) -> Option<Arc<ScopeSet>> {
        if let Some(set) = self.derived.get(drv_hash).await {
            return Some(set);
        }
        // `"references"` quoted: PG reserved keyword. The LIMIT bounds
        // the materialized rows; the walk itself is bounded by the
        // build's reachable narinfo rows (a strict subset of what GC
        // mark walks store-wide).
        let rows: Result<Vec<(Vec<u8>,)>, sqlx::Error> = sqlx::query_as(
            r#"
            WITH RECURSIVE reachable(store_path) AS (
                SELECT n.store_path
                  FROM scheduler_live_pins p
                  JOIN narinfo n USING (store_path_hash)
                 WHERE p.drv_hash = $1
                UNION
                SELECT unnest(n."references")
                  FROM narinfo n
                  JOIN reachable r ON n.store_path = r.store_path
            )
            SELECT n.store_path_hash
              FROM narinfo n
              JOIN reachable r ON n.store_path = r.store_path
             LIMIT $2
            "#,
        )
        .bind(drv_hash)
        .bind(i64::try_from(MAX_INPUT_CLOSURE + 1).expect("cap fits i64"))
        .fetch_all(pool)
        .await;
        match rows {
            Ok(rows) if rows.is_empty() => None,
            Ok(rows) if rows.len() > MAX_INPUT_CLOSURE => {
                warn!(
                    drv_hash,
                    rows = rows.len(),
                    "derived closure exceeds MAX_INPUT_CLOSURE; refusing the truncated scope"
                );
                None
            }
            Ok(rows) => {
                let hashes: Vec<[u8; 32]> = rows
                    .into_iter()
                    .filter_map(|(h,)| h.try_into().ok())
                    .collect();
                if hashes.is_empty() {
                    return None;
                }
                let set = Arc::new(ScopeSet::from_hashes(hashes));
                self.derived
                    .insert(drv_hash.to_string(), Arc::clone(&set))
                    .await;
                Some(set)
            }
            Err(e) => {
                warn!(
                    error = %e,
                    drv_hash,
                    "scope derivation query failed; treating the scope as unresolved"
                );
                None
            }
        }
    }

    /// Record an out-of-scope verdict for one digest: `denied` under
    /// enforce (the read returned `NOT_FOUND`), `would_deny` under log
    /// (the read was served). The structured log — not the wire status
    /// — carries the real reason.
    pub fn record_out_of_scope(
        &self,
        enforced: bool,
        rpc: &'static str,
        ctx: &ScopeCtx,
        digest: &[u8; 32],
    ) {
        if enforced {
            metrics::counter!(
                "rio_store_castore_scope_denied_total",
                "reason" => "out_of_scope"
            )
            .increment(1);
        } else {
            metrics::counter!(
                "rio_store_castore_scope_would_deny_total",
                "reason" => "out_of_scope"
            )
            .increment(1);
        }
        self.log_denied(
            enforced,
            rpc,
            "out_of_scope",
            ctx,
            Some(hex::encode(digest)),
        );
    }

    /// Like [`Self::record_out_of_scope`] but for batch presence RPCs,
    /// where `count` request digests were (or would be) reported absent
    /// because every containing path is outside the scope.
    pub fn record_out_of_scope_batch(
        &self,
        enforced: bool,
        rpc: &'static str,
        ctx: &ScopeCtx,
        count: usize,
    ) {
        if count == 0 {
            return;
        }
        let n = count as u64;
        if enforced {
            metrics::counter!(
                "rio_store_castore_scope_denied_total",
                "reason" => "out_of_scope"
            )
            .increment(n);
        } else {
            metrics::counter!(
                "rio_store_castore_scope_would_deny_total",
                "reason" => "out_of_scope"
            )
            .increment(n);
        }
        self.log_denied(enforced, rpc, "out_of_scope", ctx, None);
    }

    /// Sampled structured deny/would-deny log: the counters are exact,
    /// the log is the triage detail (rpc, reason, drv, executor, digest,
    /// closure digest) and is sampled so a hot out-of-scope loop cannot
    /// flood the collector.
    fn log_denied(
        &self,
        enforced: bool,
        rpc: &'static str,
        reason: &'static str,
        ctx: &ScopeCtx,
        digest: Option<String>,
    ) {
        let seq = self.log_seq.fetch_add(1, Ordering::Relaxed);
        if seq >= 32 && !seq.is_multiple_of(64) {
            return;
        }
        warn!(
            rpc,
            reason,
            enforced,
            drv_hash = %ctx.drv_hash,
            executor_id = %ctx.executor_id,
            closure_digest = %ctx.closure_digest,
            digest = digest.as_deref().unwrap_or(""),
            "castore read denied (or would be denied) by the closure scope"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;
    use rio_test_support::fixtures::test_store_path;

    fn claims(drv: &str, closure_digest: &str) -> AssignmentClaims {
        AssignmentClaims {
            executor_id: "scope-unit".into(),
            drv_hash: drv.into(),
            expected_outputs: vec![],
            is_ca: false,
            expiry_unix: 9_999_999_999,
            tenant: Some(uuid::Uuid::nil().to_string()),
            role: rio_auth::hmac::TokenRole::Builder,
            input_closure_digest: closure_digest.into(),
        }
    }

    fn attested(drv: &str, closure: &[String]) -> AssignmentClaims {
        let mut sorted = closure.to_vec();
        sorted.sort_unstable();
        claims(drv, &AssignmentClaims::digest_input_closure(&sorted))
    }

    fn scope(mode: ScopeMode) -> CastoreScope {
        CastoreScope::new(
            mode,
            DEFAULT_CACHE_CAPACITY_BYTES,
            Duration::from_secs(DEFAULT_CACHE_IDLE_TTL_SECS),
        )
    }

    /// ScopeSet keys entries as SHA-256 of the FULL path string — the
    /// same `store_path_hash` the junction tables use — and answers
    /// membership over the sorted, deduped set.
    // r[verify store.castore.scope-establish]
    #[test]
    fn scope_set_keys_by_full_path_sha256() {
        let a = test_store_path("scope-a");
        let b = test_store_path("scope-b");
        let set = ScopeSet::from_closure(&[a.clone(), b.clone(), a.clone()]).expect("valid paths");
        assert_eq!(set.len(), 2, "duplicates collapse");
        let key_a: [u8; 32] = {
            use sha2::Digest as _;
            sha2::Sha256::digest(a.as_bytes()).into()
        };
        assert!(set.contains(&key_a), "keyed by sha256(full path string)");
        let other: [u8; 32] = {
            use sha2::Digest as _;
            sha2::Sha256::digest(test_store_path("scope-c").as_bytes()).into()
        };
        assert!(!set.contains(&other));

        // Entries that are not store paths are rejected, not silently
        // dropped (a silently narrowed scope would deny in-closure reads).
        assert!(ScopeSet::from_closure(&["not-a-store-path".into()]).is_err());
    }

    /// Establish: matching digest is accepted (and idempotent); a
    /// mismatched digest, an unattested token, an over-cap list, and an
    /// unparseable entry are all INVALID_ARGUMENT regardless of mode —
    /// and every one of those rejections lands in the mismatch counter.
    // r[verify store.castore.scope-establish]
    #[tokio::test]
    async fn establish_verifies_digest_cap_and_attestation() {
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let closure = vec![test_store_path("est-a"), test_store_path("est-b")];
        let s = scope(ScopeMode::Enforce);

        let ok = attested("drv-est", &closure);
        assert_eq!(s.establish(&ok, &closure).await.expect("verifies"), 2);
        // Idempotent re-present (and order-independent: digest is over
        // the sorted form).
        let reversed: Vec<String> = closure.iter().rev().cloned().collect();
        assert_eq!(s.establish(&ok, &reversed).await.expect("idempotent"), 2);

        // Mismatch: token attests a different closure.
        let other = attested("drv-est", &[test_store_path("est-other")]);
        let err = s.establish(&other, &closure).await.expect_err("mismatch");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Unattested token has nothing to verify against.
        let unattested = claims("drv-est", "");
        let err = s
            .establish(&unattested, &closure)
            .await
            .expect_err("unattested");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // Over the closure cap.
        let oversized: Vec<String> = (0..=MAX_INPUT_CLOSURE)
            .map(|i| test_store_path(&format!("p{i}")))
            .collect();
        let huge = attested("drv-est", &oversized);
        let err = s.establish(&huge, &oversized).await.expect_err("over cap");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // An entry that cannot be keyed as a store path (the token
        // attests it, so the digest matches — keying is what fails).
        let bogus = vec!["not-a-store-path".to_string()];
        let bogus_claims = attested("drv-est", &bogus);
        let err = s
            .establish(&bogus_claims, &bogus)
            .await
            .expect_err("unparseable entry");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        // One establishment (the idempotent re-present is not counted),
        // four rejections (mismatch, unattested, over-cap, unparseable).
        assert_eq!(
            recorder.get("rio_store_castore_scope_established_total{}"),
            1
        );
        assert_eq!(
            recorder.get("rio_store_castore_scope_mismatch_total{}"),
            4,
            "every PresentClosure verification rejection is counted; saw: {:?}",
            recorder.all_keys()
        );
    }

    /// Mode matrix for `resolve` with a presented scope and with an
    /// unattested token: off never scopes; log audits/serves; enforce
    /// scopes and denies unattested tokens outright.
    // r[verify store.castore.closure-scope]
    #[tokio::test]
    async fn resolve_mode_matrix() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let closure = vec![test_store_path("mode-a")];
        let c = attested("drv-mode", &closure);

        // off → Unscoped without touching anything.
        let off = scope(ScopeMode::Off);
        assert!(matches!(
            off.resolve(&c, &db.pool, "ScopeUnitTest")
                .await
                .expect("off never errors"),
            ReadScope::Unscoped
        ));

        // log + presented → Audit; enforce + presented → Enforce.
        for (mode, want_enforce) in [(ScopeMode::Log, false), (ScopeMode::Enforce, true)] {
            let s = scope(mode);
            s.establish(&c, &closure).await.expect("establish");
            match s
                .resolve(&c, &db.pool, "ScopeUnitTest")
                .await
                .expect("resolves")
            {
                ReadScope::Enforce(set, ctx) => {
                    assert!(want_enforce, "Enforce only under enforce mode");
                    assert_eq!(set.len(), 1);
                    assert_eq!(ctx.drv_hash, "drv-mode");
                }
                ReadScope::Audit(set, _) => {
                    assert!(!want_enforce, "Audit only under log mode");
                    assert_eq!(set.len(), 1);
                }
                ReadScope::Unscoped => panic!("presented scope must not resolve to Unscoped"),
            }
        }

        // Unattested token: log serves (unscoped), enforce denies.
        let unattested = claims("drv-unattested", "");
        let log = scope(ScopeMode::Log);
        assert!(matches!(
            log.resolve(&unattested, &db.pool, "ScopeUnitTest")
                .await
                .expect("served"),
            ReadScope::Unscoped
        ));
        let enforce = scope(ScopeMode::Enforce);
        let err = enforce
            .resolve(&unattested, &db.pool, "ScopeUnitTest")
            .await
            .expect_err("denied under enforce");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        assert_eq!(
            err.message(),
            "assignment token rejected",
            "must not say why (attestation state is an oracle)"
        );
    }

    /// Scope absent under log mode is served; under enforce with no
    /// pins to derive from it is FAILED_PRECONDITION carrying the
    /// CASTORE_SCOPE_REQUIRED reason for the builder's present-and-retry.
    // r[verify store.castore.closure-scope]
    #[tokio::test]
    async fn absent_scope_serves_in_log_and_asks_for_presentation_in_enforce() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let c = attested("drv-absent", &[test_store_path("absent-a")]);

        let log = scope(ScopeMode::Log);
        assert!(matches!(
            log.resolve(&c, &db.pool, "ScopeUnitTest")
                .await
                .expect("served"),
            ReadScope::Unscoped
        ));

        let enforce = scope(ScopeMode::Enforce);
        let err = enforce
            .resolve(&c, &db.pool, "ScopeUnitTest")
            .await
            .expect_err("unresolvable under enforce");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(
            err.message()
                .contains(rio_proto::CASTORE_SCOPE_REQUIRED_MSG),
            "message must carry the wire-contract reason: {err:?}"
        );
    }

    /// The §3.5 derivation fallback rebuilds the scope from
    /// `scheduler_live_pins` seeds plus the transitive
    /// `narinfo."references"` walk, so a never-presented replica can
    /// still authorize an in-closure read under enforce.
    // r[verify store.castore.closure-scope]
    #[tokio::test]
    async fn derivation_fallback_walks_pins_and_references() {
        use crate::test_helpers::StoreSeed;
        let db = TestDb::new(&crate::MIGRATOR).await;

        // root → mid → leaf reference chain; only root is pinned.
        let leaf_path = test_store_path("derive-leaf");
        let mid_path = test_store_path("derive-mid");
        let root_path = test_store_path("derive-root");
        let leaf = StoreSeed::raw_path(&leaf_path).seed(&db.pool).await;
        let _mid = StoreSeed::raw_path(&mid_path)
            .with_refs(&[leaf_path.as_str()])
            .seed(&db.pool)
            .await;
        let root = StoreSeed::raw_path(&root_path)
            .with_refs(&[mid_path.as_str()])
            .seed(&db.pool)
            .await;
        // An unrelated path that must NOT appear in the derived scope.
        let stranger = StoreSeed::path("derive-stranger").seed(&db.pool).await;

        sqlx::query("INSERT INTO scheduler_live_pins (store_path_hash, drv_hash) VALUES ($1, $2)")
            .bind(&root)
            .bind("drv-derive")
            .execute(&db.pool)
            .await
            .expect("seed pin");

        let s = scope(ScopeMode::Enforce);
        let c = attested("drv-derive", std::slice::from_ref(&root_path));
        let resolved = s
            .resolve(&c, &db.pool, "ScopeUnitTest")
            .await
            .expect("derived");
        let ReadScope::Enforce(set, _) = resolved else {
            panic!("enforce + derivable must yield ReadScope::Enforce");
        };
        let as_arr = |v: &[u8]| -> [u8; 32] { v.try_into().expect("32-byte hash") };
        assert!(set.contains(&as_arr(&root)), "pinned seed in scope");
        assert!(
            set.contains(&as_arr(&leaf)),
            "transitive reference in scope"
        );
        assert_eq!(set.len(), 3, "root + mid + leaf, nothing else");
        assert!(!set.contains(&as_arr(&stranger)));

        // A drv with no pins stays unresolved (and is NOT cached as
        // empty): FAILED_PRECONDITION asks the builder to present.
        let no_pins = attested("drv-no-pins", &[root_path]);
        let err = s
            .resolve(&no_pins, &db.pool, "ScopeUnitTest")
            .await
            .expect_err("no pins");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }
}

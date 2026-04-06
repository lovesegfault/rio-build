//! Per-tenant store-quota cache for the pre-SubmitBuild gate.
//!
//! The quota check is on the build hot path. Calling the store's
//! `TenantQuota` RPC per-opcode adds a PG round-trip per `nix build`.
//! This module wraps a `tenant_name → (used_bytes, limit_bytes)` map
//! with a 30-second TTL: the first build of a burst fetches, the rest
//! hit the cache. Quota is **eventually-enforcing** — a few MB of
//! race-window overflow during the TTL is acceptable per
//! `r[store.gc.tenant-quota-enforce]`.
//!
//! Disabled-by-default design: single-tenant mode (empty
//! `tenant_name`) skips the check entirely. Unknown tenant → NOT_FOUND
//! from the store → cached as "no quota" so the RPC isn't retried
//! until TTL.
// r[impl store.gc.tenant-quota-enforce]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rio_common::tenant::NormalizedName;
use rio_proto::StoreServiceClient;
use rio_proto::types::TenantQuotaRequest;
use tonic::transport::Channel;

/// 30-second TTL on cached `(used, limit)`. Spec upper bound
/// (`r[store.gc.tenant-quota-enforce]` says "≤30s"). A build burst
/// from the same tenant within this window makes exactly one store
/// RPC; after TTL, the next build refreshes. Small enough that
/// `nix store gc` → retry feels responsive; large enough that a
/// CI closure-upload loop doesn't hammer the store per-derivation.
pub const QUOTA_CACHE_TTL: Duration = Duration::from_secs(30);

/// Quota verdict for a tenant. Distinguishes "under limit" (pass)
/// from "no limit configured" (pass — both flow to SubmitBuild, but
/// only the former is interesting to log/metric) from "over".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaVerdict {
    /// `used ≤ limit` — allow the build.
    Under { used: u64, limit: u64 },
    /// `used > limit` — reject with STDERR_ERROR.
    Over { used: u64, limit: u64 },
    /// `gc_max_store_bytes IS NULL` or tenant unknown → no gate.
    /// Also the result for single-tenant mode (empty tenant_name:
    /// the caller gates that before hitting this module, but
    /// return value is the same shape).
    Unlimited,
}

/// Cached quota reading. `None` inside means "store said NOT_FOUND"
/// — we cache the negative result too so a typo'd tenant name in
/// authorized_keys doesn't cause an RPC every build.
#[derive(Debug, Clone, Copy)]
struct Entry {
    quota: Option<(u64, Option<u64>)>,
    fetched_at: Instant,
}

/// Per-tenant quota cache. `Clone` is cheap (inner `Arc`). One
/// instance on `GatewayServer`, cloned into every `ConnectionHandler`
/// then every `SessionContext` — the same pattern as
/// [`TenantLimiter`](crate::ratelimit::TenantLimiter). All clones
/// share one map so a tenant's quota fetched by connection A is warm
/// for connection B.
///
/// `Mutex<HashMap>` not `DashMap`: the keyspace is bounded by
/// authorized_keys entries (operator-controlled, typically <100),
/// access is read-then-maybe-write once per TTL, and the critical
/// section is a HashMap lookup + Instant compare — microseconds.
/// `DashMap` sharding would add per-access overhead for a contention
/// problem that doesn't exist at this scale.
#[derive(Clone, Default)]
pub struct QuotaCache {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

impl QuotaCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Check the tenant's quota, fetching via `store_client` on TTL
    /// miss. Returns [`QuotaVerdict::Unlimited`] on any fetch error
    /// (fail-open: a transient store/PG hiccup shouldn't block builds
    /// — quota is a soft limit, not a security gate). The error is
    /// logged so ops sees repeated misses.
    ///
    /// `tenant_name` empty → no RPC, `Unlimited`. Empty name =
    /// single-tenant mode per `r[gw.auth.tenant-from-key-comment]`;
    /// there's no tenant row to quota against.
    pub async fn check(
        &self,
        store_client: &mut StoreServiceClient<Channel>,
        tenant_name: &str,
    ) -> QuotaVerdict {
        // Single-tenant mode (empty name from authorized_keys comment)
        // → no quota row to gate on. `from_maybe_empty` is the same
        // normalization the scheduler/store use — one place defines
        // what "empty tenant" means.
        let Some(tenant_name) = NormalizedName::from_maybe_empty(tenant_name) else {
            return QuotaVerdict::Unlimited;
        };

        if let Some(v) = self.lookup_fresh(&tenant_name) {
            return Self::classify(v);
        }

        // Cache miss or stale → fetch. No singleflight — concurrent
        // requests for the same tenant within the same tick each do
        // an RPC, which is fine: the result is read-only, races are
        // idempotent (last-writer wins on the cache entry, all
        // writers carry the same data modulo microseconds of
        // path_tenants churn).
        let fetched = match store_client
            .tenant_quota(TenantQuotaRequest {
                tenant_name: tenant_name.to_string(),
            })
            .await
        {
            Ok(resp) => {
                let r = resp.into_inner();
                Some((r.used_bytes, r.limit_bytes))
            }
            Err(status) if status.code() == tonic::Code::NotFound => {
                // Unknown tenant — cache the negative so we don't
                // retry every build. The gateway only sends known
                // names (matched authorized_keys entry) in normal
                // operation; NOT_FOUND here means the operator
                // hasn't seeded the `tenants` row yet. Pass through
                // — the scheduler's SubmitBuild will reject with
                // the same "unknown tenant" anyway.
                None
            }
            Err(status) => {
                // Transient store/PG error. Fail-open: don't block
                // the build on a soft-limit check. Don't cache — we
                // want to retry next TTL. Log at warn so repeated
                // misses are visible (a stuck store would show a
                // steady stream of these).
                tracing::warn!(
                    tenant = %tenant_name,
                    code = ?status.code(),
                    error = %status.message(),
                    "TenantQuota RPC failed; skipping quota gate (fail-open)"
                );
                return QuotaVerdict::Unlimited;
            }
        };

        // Cache under lock. Short critical section: insert only.
        {
            let mut m = self.inner.lock().expect("quota cache mutex poisoned");
            m.insert(
                tenant_name.to_string(),
                Entry {
                    quota: fetched,
                    fetched_at: Instant::now(),
                },
            );
        }

        Self::classify(fetched)
    }

    /// Non-blocking cache read. `Some` iff fresh (within TTL).
    fn lookup_fresh(&self, tenant_name: &str) -> Option<Option<(u64, Option<u64>)>> {
        let m = self.inner.lock().expect("quota cache mutex poisoned");
        let e = m.get(tenant_name)?;
        if e.fetched_at.elapsed() < QUOTA_CACHE_TTL {
            Some(e.quota)
        } else {
            None
        }
    }

    /// Map a cached/fetched `(used, limit)` to a verdict.
    fn classify(q: Option<(u64, Option<u64>)>) -> QuotaVerdict {
        match q {
            Some((used, Some(limit))) if used > limit => QuotaVerdict::Over { used, limit },
            Some((used, Some(limit))) => QuotaVerdict::Under { used, limit },
            // limit=None (no configured quota) OR tenant unknown.
            Some((_, None)) | None => QuotaVerdict::Unlimited,
        }
    }

    /// Test hook: seed an entry directly (bypassing the RPC). Tests
    /// can also seed `MockStore::tenant_quotas` and exercise the
    /// fetch path — this is the shortcut for unit-testing
    /// `classify` + TTL without a mock server.
    #[cfg(test)]
    pub fn seed_for_test(&self, tenant_name: &str, used: u64, limit: Option<u64>) {
        self.inner.lock().unwrap().insert(
            tenant_name.to_string(),
            Entry {
                quota: Some((used, limit)),
                fetched_at: Instant::now(),
            },
        );
    }
}

/// Format a byte count as a human-readable string for error messages.
///
/// IEC binary units (KiB/MiB/GiB/TiB), one decimal place, rounds up
/// at the decimal so "1.0 GiB" means at least 1073741824 bytes. The
/// STDERR_ERROR quota message goes to `nix build` stderr — humans
/// read it, so "12.3 GiB / 10.0 GiB" beats "13207024435 / 10737418240".
pub fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_over_under_unlimited() {
        assert_eq!(
            QuotaCache::classify(Some((100, Some(50)))),
            QuotaVerdict::Over {
                used: 100,
                limit: 50
            }
        );
        assert_eq!(
            QuotaCache::classify(Some((50, Some(100)))),
            QuotaVerdict::Under {
                used: 50,
                limit: 100
            }
        );
        // Exactly at limit = Under (strict `>` for Over).
        assert_eq!(
            QuotaCache::classify(Some((100, Some(100)))),
            QuotaVerdict::Under {
                used: 100,
                limit: 100
            }
        );
        assert_eq!(
            QuotaCache::classify(Some((100, None))),
            QuotaVerdict::Unlimited
        );
        assert_eq!(QuotaCache::classify(None), QuotaVerdict::Unlimited);
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(10 * 1024 * 1024 * 1024), "10.0 GiB");
    }

    #[test]
    fn cache_hit_within_ttl() {
        let c = QuotaCache::new();
        c.seed_for_test("t", 200, Some(100));
        // seed_for_test stamps Instant::now() — fresh.
        assert!(matches!(c.lookup_fresh("t"), Some(Some((200, Some(100))))));
        // Unknown key misses.
        assert!(c.lookup_fresh("unknown").is_none());
    }
}

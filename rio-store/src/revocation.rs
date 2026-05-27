//! Terminal-build revocation for assignment-token castore reads
//! (`r[store.castore.terminal-revocation]`).
//!
//! The per-build assignment token is the builder's tenant credential on
//! the castore read surface (`DirectoryService`). Its expiry is bounded
//! at mint time (`r[common.hmac.expiry-cap]`), but a leaked token would
//! otherwise stay usable until that expiry even after its build
//! finished. The scheduler already persists the needed liveness state
//! in the shared Postgres (`derivations.status`, written at every
//! transition), so the store can probe it: once the status is terminal,
//! reads with that token are rejected.
//!
//! The probe is deliberately **permissive**:
//! - only a row that exists *and* is terminal revokes; a missing row
//!   (hand-minted dev/test tokens, a drv GC'd long after completion)
//!   stays allowed — the check narrows access, never widens it;
//! - a PG error fails open (the read's own data query will surface the
//!   outage anyway);
//! - uploads and `GetChunks` are not gated here at all, so the
//!   tolerated late-upload-after-redispatch behavior is unchanged.
//!
//! Cost: the verdict is cached per `drv_hash` (both answers) for
//! [`DEFAULT_CACHE_TTL_SECS`]-scale TTLs, so steady state adds at most
//! one indexed PG lookup per active drv per TTL on RPCs that already do
//! PG work per call.

use std::time::Duration;

use sqlx::PgPool;
use tracing::warn;

/// Default per-`drv_hash` verdict cache TTL (seconds).
///
/// 10 s sits in the middle of the 5-15 s band that keeps both costs
/// negligible: revocation latency (a terminal build's token keeps
/// working for at most this long after the scheduler records the
/// status) and PG load (≤ one `derivations.status` PK-style lookup per
/// active drv per TTL — noise next to the per-read tenant-scope query
/// the castore RPCs already issue).
pub const DEFAULT_CACHE_TTL_SECS: u64 = 10;

/// Bound on cached verdicts. Keys are live-ish `drv_hash`es seen within
/// one TTL — worst case a few thousand concurrent builds; 64 Ki entries
/// of `String → bool` is a trivially small ceiling that exists only so
/// a flood of forged tokens cannot grow the map unboundedly.
const CACHE_MAX_ENTRIES: u64 = 65_536;

/// Probes whether an assignment token's build has reached a terminal
/// state, with a short-TTL in-memory cache keyed by `drv_hash`.
///
/// Construct once per service (`DirectoryServiceImpl::with_revocation`)
/// so the cache is shared across all castore read RPCs.
pub struct BuildTerminalProbe {
    /// `drv_hash` → "is terminal" verdict. Both the positive and the
    /// negative answer are cached (a non-terminal build's reads must
    /// not pay a PG probe per call either). Same `moka::future` flavor
    /// as the chunk cache (`cas.rs`) — the workspace enables only that
    /// feature.
    cache: moka::future::Cache<String, bool>,
}

impl BuildTerminalProbe {
    /// `cache_ttl` is how long a verdict (either way) is served without
    /// re-consulting Postgres — i.e. the worst-case revocation latency.
    /// Clamped to ≥ 1 ms defensively; config validation already rejects
    /// a zero TTL.
    pub fn new(cache_ttl: Duration) -> Self {
        Self {
            cache: moka::future::Cache::builder()
                .max_capacity(CACHE_MAX_ENTRIES)
                .time_to_live(cache_ttl.max(Duration::from_millis(1)))
                .build(),
        }
    }

    /// True iff `derivations.status` for `drv_hash` exists and is in
    /// the shared terminal set
    /// ([`rio_migrations::schema::DERIVATION_TERMINAL_STATUSES`]).
    ///
    /// Missing row → `false` (cannot prove terminal — permissive).
    /// Lookup error → `false` and **not cached** (fail open; the next
    /// call retries).
    // r[impl store.castore.terminal-revocation]
    pub async fn is_terminal(&self, pool: &PgPool, drv_hash: &str) -> bool {
        if let Some(cached) = self.cache.get(drv_hash).await {
            return cached;
        }
        let status: Result<Option<String>, sqlx::Error> =
            sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = $1")
                .bind(drv_hash)
                .fetch_optional(pool)
                .await;
        match status {
            Ok(status) => {
                let terminal = status.as_deref().is_some_and(|s| {
                    rio_migrations::schema::DERIVATION_TERMINAL_STATUSES.contains(&s)
                });
                self.cache.insert(drv_hash.to_string(), terminal).await;
                terminal
            }
            Err(e) => {
                // Fail open: this is a narrowing defense-in-depth check;
                // if PG is actually down the read's own tenant-scope
                // query fails right after this anyway.
                warn!(
                    error = %e,
                    drv_hash,
                    "terminal-build revocation probe failed; allowing the read"
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// Seed a minimal `derivations` row (the columns the scheduler
    /// always writes) with the given status.
    async fn seed_derivation(pool: &PgPool, drv_hash: &str, status: &str) {
        sqlx::query(
            "INSERT INTO derivations (drv_hash, drv_path, system, status) \
             VALUES ($1, $2, 'x86_64-linux', $3)",
        )
        .bind(drv_hash)
        .bind(format!("/nix/store/{drv_hash}-revocation-test.drv"))
        .bind(status)
        .execute(pool)
        .await
        .expect("seed derivations row");
    }

    async fn set_status(pool: &PgPool, drv_hash: &str, status: &str) {
        sqlx::query("UPDATE derivations SET status = $2 WHERE drv_hash = $1")
            .bind(drv_hash)
            .bind(status)
            .execute(pool)
            .await
            .expect("update derivations.status");
    }

    /// Verdicts: running → not terminal; completed → terminal; missing
    /// row → not terminal (permissive).
    // r[verify store.castore.terminal-revocation]
    #[tokio::test]
    async fn classifies_terminal_vs_live_vs_missing() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        seed_derivation(&db.pool, "rev-live", "running").await;
        seed_derivation(&db.pool, "rev-done", "completed").await;

        let probe = BuildTerminalProbe::new(Duration::from_secs(30));
        assert!(!probe.is_terminal(&db.pool, "rev-live").await);
        assert!(probe.is_terminal(&db.pool, "rev-done").await);
        // No derivations row at all: cannot prove terminal → allow.
        assert!(!probe.is_terminal(&db.pool, "rev-unknown").await);
    }

    /// The negative (non-terminal) verdict is served from the cache
    /// within the TTL: flipping the status does not change the answer
    /// until the entry expires. TTL is generous (30 s) so the
    /// assertion is structural, not a timing race.
    // r[verify store.castore.terminal-revocation]
    #[tokio::test]
    async fn negative_verdict_is_cached_within_ttl() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        seed_derivation(&db.pool, "rev-cache", "running").await;

        let probe = BuildTerminalProbe::new(Duration::from_secs(30));
        assert!(!probe.is_terminal(&db.pool, "rev-cache").await);

        set_status(&db.pool, "rev-cache", "completed").await;
        assert!(
            !probe.is_terminal(&db.pool, "rev-cache").await,
            "within the TTL the cached non-terminal verdict must be served \
             (no per-read PG probe)"
        );
    }

    /// After the TTL the probe re-consults PG and observes the status
    /// flip. Poll-until-true (no upper-bound timing assertion) so
    /// builder-load stalls can't flake it; the integration-level
    /// allow/deny pair lives in tests/grpc/revocation.rs.
    // r[verify store.castore.terminal-revocation]
    #[tokio::test]
    async fn verdict_refreshes_after_ttl() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        seed_derivation(&db.pool, "rev-flip", "running").await;

        let probe = BuildTerminalProbe::new(Duration::from_millis(200));
        assert!(!probe.is_terminal(&db.pool, "rev-flip").await);

        set_status(&db.pool, "rev-flip", "completed").await;
        let mut flipped = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if probe.is_terminal(&db.pool, "rev-flip").await {
                flipped = true;
                break;
            }
        }
        assert!(
            flipped,
            "after the cache TTL the probe must observe the terminal status"
        );
    }

    /// A Postgres error on the probe fails open (allow) and the error
    /// verdict is NOT cached: the very next probe against a healthy
    /// pool sees the real (terminal) status.
    // r[verify store.castore.terminal-revocation]
    #[tokio::test]
    async fn probe_error_fails_open_and_is_not_cached() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        seed_derivation(&db.pool, "rev-err", "completed").await;

        // A pool whose connections can never be established: nothing
        // speaks Postgres on 127.0.0.1:1. `connect_lazy` defers the
        // failure to the first acquire, i.e. inside `is_terminal`; the
        // short acquire_timeout bounds the test even if the connect
        // attempt doesn't fail fast.
        let broken = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_secs(2))
            .connect_lazy("postgres://rio:rio@127.0.0.1:1/rio")
            .expect("lazy pool construction never connects");

        let probe = BuildTerminalProbe::new(Duration::from_secs(30));
        assert!(
            !probe.is_terminal(&broken, "rev-err").await,
            "a failed status probe must fail open (allow)"
        );
        assert!(
            probe.is_terminal(&db.pool, "rev-err").await,
            "the fail-open verdict must not be cached — the next probe against a \
             healthy pool must see the terminal status"
        );
    }
}

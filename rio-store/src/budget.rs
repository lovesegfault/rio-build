//! The shared NAR-budget cost-axis home (merged_bug_005's close).
//!
//! One pool (`nar_bytes_budget`, 32 GiB default) is consumed by two
//! reservation PRICING modes:
//!
//! - **Delivery-priced** (PutPath trailer mode): permits accrue
//!   chunk-by-chunk as bytes actually arrive — an attacker's cost is
//!   real bandwidth, so the per-tenant cost axis is priced by the
//!   transfer itself.
//! - **Declaration-priced** (substitute legs and PutPath declared
//!   mode): the WHOLE charge is granted up front from a wire-supplied
//!   size — a hostile declaration is free, so the cost axis MUST be
//!   carried by the acquisition itself: (tenant, ledger, cap) bind
//!   BEFORE any grant.
//!
//! `DeclaredCharge` is the ONE constructor for declaration-priced
//! acquisition — both reservation modes (the substitute leg's
//! `substitute::NarBudgetReservation` and PutPath's
//! `reserve_declared`) construct it, and its signature REQUIRES the
//! cost axis. A new declaration-priced consumer structurally cannot
//! acquire without consulting the ledger (wave-9's `reserve_declared`
//! shipped exactly that bare sibling — eight ~4 GiB declarations from
//! one worker pinned the full pool at zero bandwidth, renewable for
//! the whole hold envelope).
// r[impl store.budget.cost-axis]

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use rio_common::limits::{MAX_NAR_SIZE, MIN_NAR_CHUNK_CHARGE};

/// Per-tenant cap on AGGREGATE outstanding reservation charge — the
/// COST axis of the budget law (merged_bug_021's blast radius; R17
/// all-axes). Declaration-priced charge is free to mint (a hostile
/// upstream's or worker's lies cost no bandwidth) — so without this
/// cap one tenant could pin the whole pool by declaration alone
/// (8 legs × ~4 GiB against the 32 GiB default), in EITHER
/// declaration-priced mode: the substitute leg (upstream-supplied
/// NarSize) and PutPath declared mode (worker-supplied
/// `declared_nar_size`) consult the same law. PutPath's trailer mode
/// stays delivery-priced (per-chunk accrual) and is deliberately
/// outside this cap. `2 × MAX_NAR_SIZE` (8 GiB = ¼ of the default
/// pool): ≥ 4 tenants must collude to fill the pool by declaration,
/// while a single tenant's parallel warm of two max-size closures is
/// preserved. Refusal, never queueing: over-cap returns the typed
/// [`DeclaredRefusal::TenantBudgetExhausted`] (retryable — both
/// planes' existing retry machinery absorbs it); a tenant-axis queue
/// would mint new lock-order proof obligations for zero benefit.
/// Violable per R17 via each plane's builder override.
pub(crate) const TENANT_RESERVATION_CAP: u64 = 2 * MAX_NAR_SIZE;

// The cap admits at least one whole-NAR reservation (so a single
// honest tenant is never structurally refused). Each plane's pool
// pins its own `cap <= pool` relation beside its pool const.
const _: () = assert!(TENANT_RESERVATION_CAP >= MAX_NAR_SIZE);

// Charge-cast losslessness: every admitted declaration is
// `< MAX_NAR_SIZE`, so the u32 charge cast below cannot truncate.
const _: () = assert!(MAX_NAR_SIZE - 1 <= u32::MAX as u64);

/// The cost-axis charge bucket for declaration-priced acquisitions
/// whose verified authority carries NO tenant attribution: dev-mode
/// callers, allowlisted service callers, and builder tokens minted
/// for orphaned/recovered nodes (`AssignmentClaims.tenant = None`).
/// One SHARED capped bucket — fail-closed on the cost axis: an
/// unattributed caller population can pin at most
/// [`TENANT_RESERVATION_CAP`] in aggregate, never the pool. The nil
/// UUID cannot collide with a real tenant (`tenant_id` is
/// `gen_random_uuid()`-minted; v4 UUIDs are never nil).
pub(crate) const UNATTRIBUTED_DECLARED_BUCKET: Uuid = Uuid::nil();

/// Derive the declared-mode charge tenant from the
/// HMAC-VERIFIED assignment claims — never from the request body
/// (the same trust rule as `hw_perf_samples.submitting_tenant`:
/// the scheduler signs the attribution; a compromised worker cannot
/// choose its own bucket). `None` claims (dev mode / service bypass)
/// and tenant-less claims land in the shared
/// [`UNATTRIBUTED_DECLARED_BUCKET`]. A malformed signed tenant is our
/// own scheduler's bug: warn + the unattributed bucket (fail-closed
/// on the COST axis — the bucket is capped — without failing the
/// upload availability axis on a non-adversarial defect).
pub(crate) fn declared_charge_tenant(claims: Option<&rio_auth::hmac::AssignmentClaims>) -> Uuid {
    match claims.and_then(|c| c.tenant.as_deref()) {
        Some(t) => match Uuid::parse_str(t) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(
                    tenant = %t,
                    error = %e,
                    "declared charge: malformed signed tenant attribution; \
                     charging the unattributed bucket"
                );
                UNATTRIBUTED_DECLARED_BUCKET
            }
        },
        None => UNATTRIBUTED_DECLARED_BUCKET,
    }
}

// r[impl store.budget.cost-axis]
/// THE sealed NAR-byte budget home (merged_bug_005's R24 seal — the
/// module boundary, named): the raw `tokio::sync::Semaphore` is a
/// MODULE-PRIVATE field of this type, so `add_permits`/`forget`/bare
/// `acquire_many`/`acquire_many_owned` against the pool are
/// UNWRITABLE outside `crate::budget`. The only debit paths:
///
/// - `DeclaredCharge::new` — declaration-priced (whole charge up
///   front from a wire-supplied size); the cost axis `(tenant, cap)`
///   is REQUIRED by the signature and the ledger consult is fused
///   into the acquisition (this type OWNS the ledger — a caller
///   cannot pair the pool with the wrong accounting).
/// - `NarBudget::acquire_chunk` — delivery-priced (trailer mode's
///   per-chunk accrual; permits track bytes actually received). Its
///   one chokepoint (`accumulate_chunk`) times every wait with
///   `BUDGET_WAIT_GRACE` and sheds typed.
///
/// Reads are not debits: [`Self::available_permits`] is the
/// test/gauge face. `Clone` shares IDENTITY (one `Arc` semaphore,
/// one `Arc`-backed ledger) — main.rs wires ONE instance into both
/// ingest planes so PutPath and substitution draw from one pool
/// under one cost accounting.
#[derive(Debug, Clone)]
pub struct NarBudget {
    /// The raw pool. PRIVATE — see the type doc: the seal is this
    /// field's visibility.
    semaphore: Arc<Semaphore>,
    /// The cost-axis accounting (one instance per pool, by
    /// construction).
    ledger: TenantReservationLedger,
}

impl NarBudget {
    /// A fresh pool of `permits` byte-permits with its own (empty)
    /// tenant ledger.
    pub fn new(permits: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(permits)),
            ledger: TenantReservationLedger::default(),
        }
    }

    /// The delivery-priced per-chunk debit face (trailer mode): a
    /// plain `acquire_many` future for `charge` permits. The caller's
    /// chokepoint (`accumulate_chunk`) bounds the wait with
    /// `BUDGET_WAIT_GRACE` and sheds typed on elapse — pricing is the
    /// delivered bytes themselves, so no per-tenant ledger consult
    /// (an attacker pays bandwidth; the cost axis is real).
    pub(crate) fn acquire_chunk(
        &self,
        charge: u32,
    ) -> impl Future<Output = Result<tokio::sync::SemaphorePermit<'_>, tokio::sync::AcquireError>>
    {
        self.semaphore.acquire_many(charge)
    }

    /// Read-only pool headroom (tests/gauges). Reads are not debits.
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

// r[impl store.put.nar-bytes-budget+6]
/// Tenant-keyed outstanding reservation charge — the cost-axis
/// accounting behind [`TENANT_RESERVATION_CAP`]. Consulted inside
/// [`DeclaredCharge::new`] BEFORE the semaphore park (a refused
/// tenant never queues, so the wait edge gains no new population);
/// released when the charge drops (the [`TenantChargeGuard`] rides
/// the charge, so every abort path — completion, deadline expiry,
/// cancellation — releases through the ONE `Drop` impl).
#[derive(Debug, Clone, Default)]
pub(crate) struct TenantReservationLedger {
    outstanding: Arc<std::sync::Mutex<std::collections::HashMap<Uuid, u64>>>,
}

impl TenantReservationLedger {
    /// Charge `charge` against `tenant_id`'s outstanding total, or
    /// refuse typed if the aggregate would exceed `cap`. The critical
    /// section is sync and await-free.
    pub(crate) fn checked_charge(
        &self,
        tenant_id: Uuid,
        charge: u64,
        cap: u64,
    ) -> Result<TenantChargeGuard, DeclaredRefusal> {
        let mut map = self.outstanding.lock().expect("ledger lock poisoned");
        let entry = map.entry(tenant_id).or_insert(0);
        if entry.saturating_add(charge) > cap {
            return Err(DeclaredRefusal::TenantBudgetExhausted { cap });
        }
        *entry += charge;
        Ok(TenantChargeGuard {
            outstanding: Arc::clone(&self.outstanding),
            tenant_id,
            charge,
        })
    }
}

/// RAII release of one tenant charge — see [`TenantReservationLedger`].
#[derive(Debug)]
pub(crate) struct TenantChargeGuard {
    outstanding: Arc<std::sync::Mutex<std::collections::HashMap<Uuid, u64>>>,
    tenant_id: Uuid,
    charge: u64,
}

impl Drop for TenantChargeGuard {
    fn drop(&mut self) {
        let mut map = self.outstanding.lock().expect("ledger lock poisoned");
        if let Some(e) = map.get_mut(&self.tenant_id) {
            *e = e.saturating_sub(self.charge);
            if *e == 0 {
                map.remove(&self.tenant_id);
            }
        }
    }
}

/// Typed refusal surface of [`DeclaredCharge::new`]. Each consuming
/// plane maps these onto its own error alphabet (the substitute leg's
/// `SubstituteError`, PutPath's `Status`) — the LAW is shared, the
/// vocabulary stays per-plane.
#[derive(Debug)]
pub(crate) enum DeclaredRefusal {
    /// `declared >= MAX_NAR_SIZE` — the size axis (also what makes
    /// the u32 charge cast lossless).
    TooLarge {
        /// The violated bound ([`MAX_NAR_SIZE`]).
        limit: u64,
    },
    /// The tenant's aggregate outstanding declared charge would
    /// exceed its cap — the cost axis. Retryable; never queued.
    TenantBudgetExhausted {
        /// The violated aggregate cap.
        cap: u64,
    },
    /// The budget semaphore is closed (shutdown).
    BudgetClosed,
}

/// A declaration-priced acquisition against the shared NAR-byte pool:
/// the granted permits FUSED to the tenant's cost-axis charge in one
/// RAII value — the only way to hold declaration-priced permits is to
/// have paid the ledger. Dropping releases both through the permit
/// and guard `Drop` impls (every abort path — completion, deadline
/// expiry, cancellation — releases the pool AND restores the
/// tenant's headroom).
///
/// THE constructor for both reservation modes (the budget law's cost
/// axis, R17/R24): the signature requires `(ledger, tenant, cap,
/// declared)` — a declaration-priced consumer without the cost axis
/// does not typecheck.
#[derive(Debug)]
pub(crate) struct DeclaredCharge {
    /// `charge` permits in one `OwnedSemaphorePermit` (acquired via
    /// `acquire_many_owned`); credited back on drop.
    _permits: OwnedSemaphorePermit,
    /// The tenant's cost-axis charge: released by the same drop that
    /// credits the semaphore back.
    _tenant_charge: TenantChargeGuard,
}

impl DeclaredCharge {
    /// Acquire `declared.max(MIN_NAR_CHUNK_CHARGE)` permits in one
    /// shot, charging `tenant` on the budget's OWN ledger against
    /// `cap` FIRST: an over-cap tenant is REFUSED typed, never
    /// queued — the wait edge gains no new population. `on_park`
    /// runs after the ledger admits and before the (possibly
    /// parking) semaphore acquire — the substitute leg stamps
    /// `ClaimPhase::BudgetParked` there; the park itself is the
    /// lawful unbounded zero-holding wait (waiters park free;
    /// holders expire). The ledger is the budget's module-private
    /// field (one pool = one accounting; a caller cannot pair the
    /// pool with a wrong or fresh ledger), `cap` stays explicit —
    /// the R17 violability lane each plane owns.
    pub(crate) async fn new(
        budget: &NarBudget,
        tenant: Uuid,
        cap: u64,
        declared: u64,
        on_park: impl FnOnce(),
    ) -> Result<Self, DeclaredRefusal> {
        // The size axis (defense in depth at the constructor: both
        // planes also gate earlier; this also makes the u32 cast
        // lossless — the const pin above).
        if declared >= MAX_NAR_SIZE {
            return Err(DeclaredRefusal::TooLarge {
                limit: MAX_NAR_SIZE,
            });
        }
        let charge = declared.max(u64::from(MIN_NAR_CHUNK_CHARGE)) as u32;
        let tenant_charge = budget
            .ledger
            .checked_charge(tenant, u64::from(charge), cap)?;
        on_park();
        let permits = Arc::clone(&budget.semaphore)
            .acquire_many_owned(charge)
            .await
            .map_err(|_| DeclaredRefusal::BudgetClosed)?;
        Ok(Self {
            _permits: permits,
            _tenant_charge: tenant_charge,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W10-A core (the law at its own quantifier — AGGREGATE, not
    /// per-charge): a single tenant's outstanding declared charges
    /// cannot exceed `TENANT_RESERVATION_CAP`, regardless of
    /// per-charge size; a DIFFERENT tenant still has its own
    /// headroom; releases restore headroom.
    #[tokio::test]
    async fn declared_charges_aggregate_per_tenant() {
        let budget = NarBudget::new(8 * MAX_NAR_SIZE as usize);
        let t = Uuid::from_u128(1);
        let u = Uuid::from_u128(2);
        let big = MAX_NAR_SIZE - 1;

        let c1 = DeclaredCharge::new(&budget, t, TENANT_RESERVATION_CAP, big, || {})
            .await
            .expect("charge 1 within cap");
        let _c2 = DeclaredCharge::new(&budget, t, TENANT_RESERVATION_CAP, big, || {})
            .await
            .expect("charge 2 within cap (2 x (4GiB-1) < 8GiB)");

        // Charge 3 refuses at the AGGREGATE even at minimum size —
        // the per-charge axis cannot launder the cost axis.
        let refused = DeclaredCharge::new(&budget, t, TENANT_RESERVATION_CAP, 1, || {}).await;
        assert!(
            matches!(
                refused,
                Err(DeclaredRefusal::TenantBudgetExhausted { cap })
                    if cap == TENANT_RESERVATION_CAP
            ),
            "aggregate over-cap must refuse typed, got {refused:?}"
        );

        // The cap is per-tenant: another tenant's first charge grants.
        let _u1 = DeclaredCharge::new(&budget, u, TENANT_RESERVATION_CAP, big, || {})
            .await
            .expect("a different tenant has its own headroom");

        // Release restores headroom (the RAII edge).
        drop(c1);
        let _c3 = DeclaredCharge::new(&budget, t, TENANT_RESERVATION_CAP, big, || {})
            .await
            .expect("released charge restores tenant headroom");
    }

    /// The size axis refuses before the ledger is consulted (no
    /// charge is leaked for a refused declaration).
    #[tokio::test]
    async fn too_large_refuses_without_charging() {
        let budget = NarBudget::new(8 * MAX_NAR_SIZE as usize);
        let t = Uuid::from_u128(3);
        let refused =
            DeclaredCharge::new(&budget, t, TENANT_RESERVATION_CAP, MAX_NAR_SIZE, || {}).await;
        assert!(matches!(
            refused,
            Err(DeclaredRefusal::TooLarge { limit }) if limit == MAX_NAR_SIZE
        ));
        // Full cap still available after the refusal.
        let _ok = DeclaredCharge::new(&budget, t, TENANT_RESERVATION_CAP, MAX_NAR_SIZE - 1, || {})
            .await
            .expect("no charge leaked by the size refusal");
    }
}

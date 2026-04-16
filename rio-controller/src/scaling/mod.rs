//! Scaling helpers — queue-depth lookups and the [`component`]
//! scaler.
//!
//! Shared surface the Job reconcilers and the ComponentScaler
//! both consume. Condition helpers live in
//! `reconcilers::common::conditions`; the BPS ownership predicate
//! lives with its sole caller in `reconcilers::builderpoolset`.

pub mod component;

/// Queue depth relevant to a pool, given its `spec.systems`.
///
/// I-107: with multi-arch BuilderPools (one-arch-per-pool), the global
/// `queued_derivations` over-scales every pool to the cluster-wide
/// backlog — an aarch64 pool sits at ceiling while the queue is
/// x86-only. The scheduler now returns `queued_by_system` (Ready-only,
/// sum == scalar). We sum the entries whose key is in `systems`.
///
/// Backward compat: empty map (old scheduler) → fall back to the
/// scalar so a controller upgrade ahead of the scheduler still scales.
/// Empty `systems` (operator omitted it) → also fall back to the
/// scalar; nothing to filter on.
pub(crate) fn queued_for_systems(
    status: &rio_proto::types::ClusterStatusResponse,
    systems: &[String],
) -> u32 {
    if status.queued_by_system.is_empty() || systems.is_empty() {
        return status.queued_derivations;
    }
    systems
        .iter()
        .map(|s| status.queued_by_system.get(s).copied().unwrap_or(0))
        .sum()
}

#[cfg(test)]
mod tests;

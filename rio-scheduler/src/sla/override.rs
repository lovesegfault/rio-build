//! ADR-023 phase-6 operator override resolution.
//!
//! An override row pins a `(pname, system?, tenant?)` key to a forced
//! tier / `cores` / `mem`. NULL `system`/`tenant` are wildcards.
//! [`resolve`] picks the most-specific matching row;
//! [`super::solve::intent_for`] consults the result BEFORE the
//! fit/explore branch — `forced_cores` short-circuits the model;
//! `forced_mem` overrides mem in any branch; `tier` filters the
//! solve ladder.

use crate::db::SlaOverrideRow;

use super::types::ModelKey;

/// Resolved override for one [`ModelKey`]. All fields `Option`: a row
/// may force only a tier (solve still runs against that tier's targets)
/// OR only `cores` (solve bypassed) OR only `mem` (solve runs for
/// cores, mem is forced), in any combination.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedTarget {
    pub tier: Option<String>,
    pub forced_cores: Option<f64>,
    pub forced_mem: Option<u64>,
}

impl From<&SlaOverrideRow> for ResolvedTarget {
    fn from(r: &SlaOverrideRow) -> Self {
        Self {
            tier: r.tier.clone(),
            forced_cores: r.cores,
            // i64 → u64: column is operator-written; a negative value
            // is a config error, clamp to 0 rather than panic in the
            // dispatch path.
            forced_mem: r.mem_bytes.map(|b| b.max(0) as u64),
        }
    }
}

/// Specificity rank for precedence ordering. Higher = more specific.
/// `pname` is required (every row has it), so the range is 0..=3:
/// `pname`-only (0) < +1 each for `cluster`/`system`/`tenant` set.
/// All three are treated symmetrically — none is "more specific" than
/// another; ties break on `created_at` (newest wins).
///
/// `cluster` participates in the rank so a `{pname, cluster:"east"}`
/// row beats a global `{pname}` row regardless of `created_at` — under
/// the shared-PG topology a per-cluster pin must not be silently
/// shadowed by a later all-clusters pin. Rows for OTHER clusters are
/// already filtered out at SQL read time
/// ([`SchedulerDb::read_sla_overrides`]).
///
/// [`SchedulerDb::read_sla_overrides`]: crate::db::SchedulerDb::read_sla_overrides
fn specificity(r: &SlaOverrideRow) -> u8 {
    u8::from(r.cluster.is_some()) + u8::from(r.system.is_some()) + u8::from(r.tenant.is_some())
}

// r[impl sched.sla.override-precedence]
/// Most-specific non-expired override matching `key`, or `None`.
///
/// Match rule: `pname` exact; `system`/`tenant` exact-or-NULL (NULL is
/// a wildcard). Expiry is filtered at read time
/// ([`SchedulerDb::read_sla_overrides`]); this function does not read
/// the clock.
///
/// Precedence: highest `specificity` wins. Ties (e.g. two
/// `pname`-only rows) break on `created_at` — newest wins, so a fresh
/// `rio-cli sla override` shadows a stale one without the operator
/// having to `clear` first.
///
/// O(n) over the cached override slice; n is operator-written (tens of
/// rows), refreshed once per estimator tick.
///
/// [`SchedulerDb::read_sla_overrides`]: crate::db::SchedulerDb::read_sla_overrides
pub fn resolve(key: &ModelKey, overrides: &[SlaOverrideRow]) -> Option<ResolvedTarget> {
    overrides
        .iter()
        .filter(|r| {
            r.pname == key.pname
                && r.system.as_deref().is_none_or(|s| s == key.system)
                && r.tenant.as_deref().is_none_or(|t| t == key.tenant)
        })
        .max_by(|a, b| {
            specificity(a)
                .cmp(&specificity(b))
                .then(a.created_at.total_cmp(&b.created_at))
        })
        .map(ResolvedTarget::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(pname: &str, system: &str, tenant: &str) -> ModelKey {
        ModelKey {
            pname: pname.into(),
            system: system.into(),
            tenant: tenant.into(),
        }
    }
    fn row(
        pname: &str,
        system: Option<&str>,
        tenant: Option<&str>,
        cores: f64,
        created_at: f64,
    ) -> SlaOverrideRow {
        SlaOverrideRow {
            pname: pname.into(),
            system: system.map(Into::into),
            tenant: tenant.map(Into::into),
            cores: Some(cores),
            created_at,
            ..Default::default()
        }
    }

    // r[verify sched.sla.override-precedence]
    #[test]
    fn most_specific_wins() {
        let overrides = vec![
            row("hello", None, None, 4.0, 1.0),
            row("hello", Some("x86_64-linux"), None, 8.0, 2.0),
            row("hello", Some("x86_64-linux"), Some("t1"), 12.0, 3.0),
        ];
        // Full match → most specific.
        assert_eq!(
            resolve(&key("hello", "x86_64-linux", "t1"), &overrides)
                .unwrap()
                .forced_cores,
            Some(12.0)
        );
        // Tenant mismatch → falls back to pname+system.
        assert_eq!(
            resolve(&key("hello", "x86_64-linux", "t2"), &overrides)
                .unwrap()
                .forced_cores,
            Some(8.0)
        );
        // System mismatch → falls back to pname-only.
        assert_eq!(
            resolve(&key("hello", "aarch64-linux", "t2"), &overrides)
                .unwrap()
                .forced_cores,
            Some(4.0)
        );
    }

    #[test]
    fn pname_mismatch_is_none() {
        let overrides = vec![row("hello", None, None, 4.0, 1.0)];
        assert!(resolve(&key("gcc", "x86_64-linux", "t1"), &overrides).is_none());
    }

    #[test]
    fn tie_breaks_on_newest() {
        // Two pname-only rows; newer (created_at=5) shadows older.
        let overrides = vec![
            row("hello", None, None, 4.0, 1.0),
            row("hello", None, None, 16.0, 5.0),
        ];
        assert_eq!(
            resolve(&key("hello", "x", "t"), &overrides)
                .unwrap()
                .forced_cores,
            Some(16.0)
        );
    }

    #[test]
    fn system_and_tenant_symmetric_specificity() {
        // pname+system vs pname+tenant: same specificity rank → newest wins.
        let overrides = vec![
            row("hello", Some("x86_64-linux"), None, 8.0, 2.0),
            row("hello", None, Some("t1"), 6.0, 3.0),
        ];
        assert_eq!(
            resolve(&key("hello", "x86_64-linux", "t1"), &overrides)
                .unwrap()
                .forced_cores,
            Some(6.0)
        );
    }

    #[test]
    fn empty_overrides_is_none() {
        assert!(resolve(&key("hello", "x", "t"), &[]).is_none());
    }

    #[test]
    fn cluster_scoped_beats_global_same_shape() {
        // {pname, cluster:"east"} (older) vs {pname} (newer): before
        // `cluster` joined the specificity rank, both were rank 0 and
        // the NEWER global row shadowed the per-cluster pin. With
        // cluster in the rank, 1 > 0 → east wins. Rows for OTHER
        // clusters are filtered at SQL read time, so `resolve()` only
        // ever sees this-cluster + NULL.
        let east = SlaOverrideRow {
            cluster: Some("east".into()),
            ..row("hello", None, None, 8.0, 1.0)
        };
        let global = row("hello", None, None, 4.0, 5.0);
        assert_eq!(
            resolve(&key("hello", "x", "t"), &[east, global])
                .unwrap()
                .forced_cores,
            Some(8.0),
            "cluster-scoped (specificity 1) beats global (0) despite older"
        );
    }
}

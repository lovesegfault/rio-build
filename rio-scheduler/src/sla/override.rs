//! ADR-023 phase-6 operator override resolution.
//!
//! An override row pins a `(pname, system?, tenant?)` key to a forced
//! tier / `cores` / `mem`. NULL `system`/`tenant` are wildcards.
//! [`resolve`] picks the most-specific matching row;
//! [`super::solve::intent_for`] consults the result BEFORE the
//! fit/explore branch. All three (`forced_cores`/`forced_mem`/`tier`)
//! route the hw-agnostic `intent_for` path: the operator pinned a value
//! → bypass the menu-fit solve. (bug_033: a `forced_mem` larger than
//! every admitted cell's menu-max would otherwise emit a
//! permanently-Pending pod.)

use std::borrow::Cow;

use crate::db::SlaOverrideRow;

use super::config::CapacityType;
use super::solve::Tier;
use super::types::ModelKey;

/// Resolved override for one [`ModelKey`]. All fields `Option`: a row
/// may force only a tier (solve still runs against that tier's targets)
/// OR only `cores` (solve bypassed) OR only `mem` (solve runs for
/// cores, mem is forced), in any combination. `p*_secs` build a one-off
/// tier (config ladder ignored); `capacity` filters the admissible set
/// post-solve and is the ONLY field that does NOT bypass `solve_full`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedTarget {
    pub tier: Option<String>,
    pub forced_cores: Option<f64>,
    pub forced_mem: Option<u64>,
    pub p50_secs: Option<f64>,
    pub p90_secs: Option<f64>,
    pub p99_secs: Option<f64>,
    pub capacity: Option<CapacityType>,
}

impl ResolvedTarget {
    /// True iff any field that routes the hw-agnostic [`intent_for`]
    /// path is set. `capacity` is excluded — it filters `solve_full`'s
    /// cells post-memo and must NOT gate the hw-aware path off.
    ///
    /// [`intent_for`]: super::solve::intent_for
    pub fn bypasses_solve_full(&self) -> bool {
        self.forced_cores.is_some()
            || self.tier.is_some()
            || self.forced_mem.is_some()
            || self.p50_secs.is_some()
            || self.p90_secs.is_some()
            || self.p99_secs.is_some()
    }

    /// The tier ladder this override resolves against. Single source of
    /// the `ResolvedTarget → effective tiers` projection — `intent_for`,
    /// `explain`, and the `SlaPrediction` recorder all call this so a
    /// new override field shows up everywhere or nowhere (mb_053:
    /// `p*_secs` was only handled in `intent_for`; explain showed the
    /// config ladder with "Override: (none)"; SlaPrediction recorded
    /// the config-ladder tier → false `envelope_result_total{miss}`).
    ///
    /// - Named `tier` → config ladder filtered to that tier (unknown
    ///   name → empty → BestEffort, surfaces via `sla explain`).
    /// - `p*_secs` → a single ad-hoc `Tier{name:"override"}` carrying
    ///   the operator's targets; config ladder ignored.
    /// - Neither → borrowed config ladder unchanged.
    ///
    /// Named `tier` wins if both are set (more specific operator intent).
    /// `forced_cores` is NOT consulted — that short-circuits before any
    /// solve, so callers handle it separately.
    pub fn effective_tiers<'a>(&self, cfg: &'a [Tier]) -> Cow<'a, [Tier]> {
        if let Some(name) = self.tier.as_deref() {
            return Cow::Owned(cfg.iter().filter(|t| t.name == name).cloned().collect());
        }
        if self.p50_secs.is_some() || self.p90_secs.is_some() || self.p99_secs.is_some() {
            return Cow::Owned(vec![Tier {
                name: "override".into(),
                p50: self.p50_secs,
                p90: self.p90_secs,
                p99: self.p99_secs,
            }]);
        }
        Cow::Borrowed(cfg)
    }

    /// Human-readable summary of every override field that is set, for
    /// `sla explain`'s `Override:` header. `None` iff no field is set.
    /// Single source so `explain` doesn't open-code the field list
    /// (mb_053: `p*_secs` + `capacity` were silently omitted).
    pub fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(c) = self.forced_cores {
            parts.push(format!("forced cores={c}"));
        }
        if let Some(m) = self.forced_mem {
            parts.push(format!(
                "forced mem={:.1}Gi",
                m as f64 / (1u64 << 30) as f64
            ));
        }
        if let Some(t) = self.tier.as_deref() {
            parts.push(format!("tier pinned to {t:?}"));
        }
        if let Some(p) = self.p50_secs {
            parts.push(format!("p50={p:.0}s"));
        }
        if let Some(p) = self.p90_secs {
            parts.push(format!("p90={p:.0}s"));
        }
        if let Some(p) = self.p99_secs {
            parts.push(format!("p99={p:.0}s"));
        }
        if let Some(c) = self.capacity {
            parts.push(format!("capacity={}", c.label()));
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
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
            p50_secs: r.p50_secs,
            p90_secs: r.p90_secs,
            p99_secs: r.p99_secs,
            // Unparseable cap (operator typo via raw SQL) → drop silently;
            // the CLI already constrains to spot|on-demand.
            capacity: r.capacity_type.as_deref().and_then(CapacityType::parse),
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
    resolve_row(key, overrides).map(ResolvedTarget::from)
}

/// [`resolve`] but returns the matching ROW (with `id`/`created_by`/
/// `expires_at`) instead of the projected [`ResolvedTarget`]. Single
/// implementation of the filter+rank so callers that need the full row
/// (`AdminQuery::SlaStatus`) cannot drift from dispatch's resolution.
pub fn resolve_row<'a>(
    key: &ModelKey,
    overrides: &'a [SlaOverrideRow],
) -> Option<&'a SlaOverrideRow> {
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

    fn ladder() -> Vec<Tier> {
        vec![
            Tier {
                name: "fast".into(),
                p50: None,
                p90: Some(60.0),
                p99: None,
            },
            Tier {
                name: "normal".into(),
                p50: None,
                p90: Some(300.0),
                p99: None,
            },
        ]
    }

    /// mb_053 architectural close: the `ResolvedTarget → effective tier
    /// ladder` projection has a single implementation. Three call sites
    /// (`intent_for`, `explain`, snapshot SlaPrediction) consume this;
    /// the test asserts ALL override field-shapes route correctly so a
    /// new field that bypasses the ladder cannot be added without
    /// touching this one fn.
    #[test]
    fn effective_tiers_covers_p_and_named() {
        let cfg = ladder();
        // No override → borrowed config ladder.
        let none = ResolvedTarget::default().effective_tiers(&cfg);
        assert!(matches!(none, Cow::Borrowed(_)));
        assert_eq!(none.len(), 2);
        // Named tier → filtered to that tier only.
        let named = ResolvedTarget {
            tier: Some("fast".into()),
            ..Default::default()
        };
        assert_eq!(
            named
                .effective_tiers(&cfg)
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["fast"]
        );
        // Unknown named tier → empty (BestEffort; surfaces via explain).
        let unknown = ResolvedTarget {
            tier: Some("nope".into()),
            ..Default::default()
        };
        assert!(unknown.effective_tiers(&cfg).is_empty());
        // p* → ad-hoc tier carrying the operator's targets.
        let p = ResolvedTarget {
            p90_secs: Some(1200.0),
            ..Default::default()
        };
        let pt = p.effective_tiers(&cfg);
        assert_eq!(pt.len(), 1);
        assert_eq!(pt[0].name, "override");
        assert_eq!(pt[0].p90, Some(1200.0));
        assert_eq!(pt[0].binding_bound(), Some(1200.0));
        // Named wins over p*: more specific operator intent.
        let both = ResolvedTarget {
            tier: Some("normal".into()),
            p90_secs: Some(1200.0),
            ..Default::default()
        };
        assert_eq!(both.effective_tiers(&cfg)[0].name, "normal");
        // capacity-only does NOT change the ladder (it filters cells
        // post-solve, not the solve target).
        let cap_only = ResolvedTarget {
            capacity: Some(CapacityType::Od),
            ..Default::default()
        };
        assert!(matches!(cap_only.effective_tiers(&cfg), Cow::Borrowed(_)));
    }

    #[test]
    fn describe_covers_all_fields() {
        assert_eq!(ResolvedTarget::default().describe(), None);
        let full = ResolvedTarget {
            tier: Some("fast".into()),
            forced_cores: Some(8.0),
            forced_mem: Some(2 << 30),
            p90_secs: Some(600.0),
            capacity: Some(CapacityType::Od),
            ..Default::default()
        };
        let s = full.describe().unwrap();
        for needle in [
            "cores=8",
            "mem=2.0Gi",
            "tier pinned",
            "p90=600s",
            "capacity=on-demand",
        ] {
            assert!(s.contains(needle), "describe() missing {needle:?}: {s}");
        }
    }

    #[test]
    fn resolve_carries_p_targets_and_capacity() {
        let row = SlaOverrideRow {
            pname: "hello".into(),
            p90_secs: Some(600.0),
            capacity_type: Some("on-demand".into()),
            ..Default::default()
        };
        let r = resolve(&key("hello", "x", "t"), std::slice::from_ref(&row)).unwrap();
        assert_eq!(r.p90_secs, Some(600.0));
        assert_eq!(r.p50_secs, None);
        assert_eq!(r.capacity, Some(CapacityType::Od));
        // p* gates solve_full off; capacity alone does NOT.
        assert!(r.bypasses_solve_full());
        let cap_only = ResolvedTarget {
            capacity: Some(CapacityType::Spot),
            ..Default::default()
        };
        assert!(!cap_only.bypasses_solve_full());
        // Unparseable cap → dropped, not propagated as garbage.
        let bad = SlaOverrideRow {
            pname: "hello".into(),
            capacity_type: Some("preemptible".into()),
            ..Default::default()
        };
        assert_eq!(ResolvedTarget::from(&bad).capacity, None);
    }

    #[test]
    fn resolve_row_matches_resolve_with_cluster_scoped() {
        // Regression for the SlaStatus inline-reimplementation drift:
        // it ranked on `system+tenant` only (omitting `cluster`), so a
        // newer global row beat a cluster-scoped row at the tie-break
        // while dispatch (via `resolve`) picked the cluster row. With
        // `resolve_row` as the single shared body, both agree.
        let east = SlaOverrideRow {
            cluster: Some("east".into()),
            ..row("hello", None, None, 8.0, 1.0)
        };
        let global = row("hello", None, None, 4.0, 5.0);
        let rows = [east.clone(), global];
        let k = key("hello", "x86_64-linux", "acme");
        let by_row = resolve_row(&k, &rows).unwrap();
        assert_eq!(by_row.cores, Some(8.0), "resolve_row picks cluster-scoped");
        assert_eq!(
            resolve(&k, &rows).unwrap().forced_cores,
            by_row.cores,
            "resolve and resolve_row agree (shared body)"
        );
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

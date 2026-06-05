//! The I-217 tenant-visibility verdict table (bug_115).
//!
//! One pure function answers "may THIS tenant see THIS path" from the
//! three per-(tenant, path) facts every caller already gathers:
//!
//! - `owned` — `path_tenants` carries a row for the requesting tenant;
//! - `any_built` — `path_tenants` carries ≥1 row for ANY tenant;
//! - `sig_trusted` — some narinfo signature verifies against the
//!   requesting tenant's trusted set (upstream `trusted_keys` ∪
//!   cluster key + history ∪ the tenant's own `tenant_keys`).
//!
//! The table IS I-217: `owned` ⇒ visible; built-by-another-only ⇒
//! hidden (isolation beats signature trust — a substituted-then-built
//! path is tenant-owned, not "still public via sig"); substitution-only
//! ⇒ visible iff a signature verifies. Both production deciders — the
//! gRPC read gate (`sig_visibility_gate{,_batch}` via
//! `rio-store/src/visibility.rs`) and the materialization walk's
//! local-presence probe — call THIS function, so the walk can never
//! re-derive a divergent notion of visibility (the pre-fix walk had no
//! notion at all: raw physical presence was sufficient, laundering
//! gate-hidden rows into per-tenant ownership).
//!
//! Caller-side POLICY exemptions (anonymous requests are unfiltered;
//! `.drv` build inputs are tenant-exempt) are deliberately NOT cells of
//! this table: they decide whether to consult the table, not what the
//! table says.

/// The verdict for one (tenant, path): may the tenant see the path?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityVerdict {
    /// The tenant may see the path (serve / count / extend from it).
    Visible,
    /// The path must be treated as absent for this tenant.
    Hidden,
}

/// The I-217 visibility law. See the module docs for the cell
/// semantics; the kani harness `check_visibility_verdict_i217_table`
/// pins all eight cells and the dominance facts the lazy callers rely
/// on (`sig_trusted` is irrelevant when `owned || any_built`, so
/// callers may skip computing it for those rows).
// r[impl store.materialize.local-visibility]
#[must_use]
pub fn visibility_verdict(owned: bool, any_built: bool, sig_trusted: bool) -> VisibilityVerdict {
    if owned {
        VisibilityVerdict::Visible
    } else if any_built {
        // I-217: built by another tenant ONLY → hidden, regardless of
        // signature trust.
        VisibilityVerdict::Hidden
    } else if sig_trusted {
        // Substitution-only path with a signature the tenant trusts.
        VisibilityVerdict::Visible
    } else {
        VisibilityVerdict::Hidden
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eight-cell I-217 table, literal.
    #[test]
    fn verdict_table_is_i217() {
        use VisibilityVerdict::{Hidden, Visible};
        // (owned, any_built, sig_trusted) → verdict
        let table = [
            ((false, false, false), Hidden), // substitution-only, untrusted
            ((false, false, true), Visible), // substitution-only, trusted sig
            ((false, true, false), Hidden),  // built by another only
            ((false, true, true), Hidden),   // I-217: isolation beats sig
            ((true, false, false), Visible), // owned (fresh window)
            ((true, false, true), Visible),  // owned + trusted
            ((true, true, false), Visible),  // owned (normal built row)
            ((true, true, true), Visible),   // owned, everything
        ];
        for ((o, b, s), want) in table {
            assert_eq!(
                visibility_verdict(o, b, s),
                want,
                "cell (owned={o}, any_built={b}, sig_trusted={s})"
            );
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// K4 (bug_115): `visibility_verdict` ≡ the I-217 table, swept over
    /// all eight cells, PLUS the two dominance facts the lazy callers
    /// rely on:
    ///
    /// 1. `owned` dominates: the verdict is Visible regardless of the
    ///    other two cells;
    /// 2. when `owned || any_built`, the verdict is INDEPENDENT of
    ///    `sig_trusted` — so a caller that skips the signature
    ///    computation for those rows (both production callers do, to
    ///    avoid the trusted-set queries) cannot change the verdict by
    ///    passing `false`.
    #[kani::proof]
    fn check_visibility_verdict_i217_table() {
        let owned: bool = kani::any();
        let any_built: bool = kani::any();
        let sig_trusted: bool = kani::any();

        let v = visibility_verdict(owned, any_built, sig_trusted);

        // The table, re-derived independently.
        let want = if owned {
            VisibilityVerdict::Visible
        } else if any_built {
            VisibilityVerdict::Hidden
        } else if sig_trusted {
            VisibilityVerdict::Visible
        } else {
            VisibilityVerdict::Hidden
        };
        assert_eq!(v, want);

        // Dominance fact 1: owned ⇒ Visible.
        if owned {
            assert_eq!(v, VisibilityVerdict::Visible);
        }
        // Dominance fact 2: sig_trusted is irrelevant once owned or
        // any_built holds (the lazy-caller contract).
        if owned || any_built {
            assert_eq!(v, visibility_verdict(owned, any_built, !sig_trusted));
        }
    }
}

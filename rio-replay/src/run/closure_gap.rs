//! THE missing-derivation policy for closure walks.
//!
//! Several walkers traverse the same derivation-closure data of an open
//! replay archive — the supply planner's truth walks (adjacency records
//! or embedded ATerm texts), the plan stage's per-unit closure expansion,
//! and the submitter's import walk. Each historically open-coded its own
//! answer to the one question they share: *what happens when the walk
//! reaches a derivation its backing source does not have?* The answers
//! diverged (hard error / silent empty closure / `debug!` skip), so the
//! same defective archive aborted one stage loudly while another stage
//! silently computed wrong accounting over it. The decision now lives
//! here, once, with each walker naming its policy at the call site.
//!
//! There are exactly two legitimate policies, because the walkers answer
//! two different questions with two different contracts:
//!
//! - **Closure truth** — "what does this unit need?" The archive format
//!   guarantees the answer is complete: "The full requisite `.drv`
//!   closure of every workload unit MUST be embedded" and, under the
//!   `dependency_closures` capability, `closures.jsonl` carries "direct
//!   dependency adjacency for every derivation in the union requisite
//!   closure of the workload (workload units included)"
//!   (`docs/dev/2026-05-28-build-replay-design.md`, §"Archive format
//!   v1"). A gap means the archive violates that contract, and every
//!   downstream consumer of the answer (warm sets, closure-overlap
//!   dispositions, supply planning, dry-run reports) would silently
//!   compute over a truncated closure — so the walk fails, naming the
//!   missing derivation and the root that needed it.
//!
//! - **Import offer** — "which embedded derivation texts can this archive
//!   offer the target?" Thin archives legitimately embed "only content no
//!   configured substituter can provide" (same doc, §"Thin archive"), so
//!   a non-embedded interior input is an expected condition: the target
//!   resolves it from its own store or substituters. The walk skips it —
//!   loudly (`warn!`), and callers surface the skipped set in their batch
//!   records — because when the target CANNOT resolve it, the resulting
//!   per-root build failure must be attributable to the archive, not
//!   charged to the unit as a regression. A missing ROOT is still an
//!   error: roots come from this archive's own workload, so absence is
//!   archive damage, never thinness.

use anyhow::{Result, bail};

/// Which contract governs a derivation that a closure walk reaches but
/// its backing source does not have. See the module docs for the two
/// policies and the format contracts they cite.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ClosureGapPolicy<'a> {
    /// The walk's answer is dependency-closure truth: a gap is an archive
    /// contract violation and aborts the walk, naming `root` (the
    /// workload root whose closure was being answered).
    Truth { root: &'a str },
    /// The walk enumerates importable bytes: a non-root gap is the thin
    /// archive shape, skipped at `warn!`; a root gap is archive damage.
    Offer { is_root: bool },
}

/// THE missing-derivation decision shared by every closure walker.
///
/// `gap` describes the miss in the backing source's own terms (e.g.
/// `"has no dependency-closure record in the archive"`, `"is not
/// embedded in the archive"`), so errors stay as actionable as the walk
/// that produced them.
///
/// Returns `Ok(())` exactly when the gap is tolerated — the caller skips
/// the derivation and continues the walk; under [`ClosureGapPolicy::Truth`]
/// it never returns `Ok`.
pub(crate) fn closure_gap(
    policy: ClosureGapPolicy<'_>,
    missing_drv: &str,
    gap: &str,
) -> Result<()> {
    match policy {
        ClosureGapPolicy::Truth { root } => bail!(
            "derivation {missing_drv} {gap} (closure of replay root {root}); the archive must \
             carry the full requisite derivation closure of every workload unit, so this \
             closure cannot be answered truthfully"
        ),
        ClosureGapPolicy::Offer { is_root: true } => bail!(
            "root {missing_drv} is not embedded in the replay archive (incomplete or corrupted \
             archive)"
        ),
        ClosureGapPolicy::Offer { is_root: false } => {
            tracing::warn!(
                path = %missing_drv,
                "input derivation not embedded in the archive; skipping (a thin archive only \
                 offers what it embeds — the target must resolve this input itself, and the \
                 skip is surfaced on the batch record)"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truth_always_errors_naming_drv_root_and_gap() {
        let err = format!(
            "{:#}",
            closure_gap(
                ClosureGapPolicy::Truth {
                    root: "/nix/store/r-root.drv"
                },
                "/nix/store/m-missing.drv",
                "has no dependency-closure record in the archive",
            )
            .unwrap_err()
        );
        assert!(err.contains("m-missing.drv"), "{err}");
        assert!(err.contains("r-root.drv"), "{err}");
        assert!(err.contains("has no dependency-closure record"), "{err}");
    }

    #[test]
    fn offer_tolerates_interiors_and_refuses_roots() {
        closure_gap(
            ClosureGapPolicy::Offer { is_root: false },
            "/nix/store/m-missing.drv",
            "is not embedded in the archive",
        )
        .expect("a non-root gap is the thin-archive shape");

        let err = format!(
            "{:#}",
            closure_gap(
                ClosureGapPolicy::Offer { is_root: true },
                "/nix/store/m-missing.drv",
                "is not embedded in the archive",
            )
            .unwrap_err()
        );
        assert!(err.contains("m-missing.drv"), "{err}");
        assert!(err.contains("incomplete or corrupted"), "{err}");
    }
}

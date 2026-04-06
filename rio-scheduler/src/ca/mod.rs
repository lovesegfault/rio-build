//! Content-addressed derivation scheduling logic.
//!
//! CA derivations differ from input-addressed ones in that their output
//! paths are NOT known until build completion. Two mechanisms handle this:
//!
//! - **Resolution** ([`resolve`]): before dispatching a CA-depends-on-CA
//!   derivation, rewrite its `inputDrvs` placeholder paths to realized
//!   store paths by querying the `realisations` table. See ADR-018
//!   Appendix B for the Nix `tryResolve` equivalent.
//!
//! - **Cutoff** (`r[sched.ca.cutoff-compare]` in `actor/completion.rs`):
//!   on completion, compare the output's nar_hash against the content
//!   index. A match means downstream builds can be skipped (the output
//!   is identical to a prior build's).

pub mod resolve;

pub use resolve::{
    CaResolveInput, RealisationLookup, ResolveError, ResolvedDerivation, downstream_placeholder,
    resolve_ca_inputs,
};

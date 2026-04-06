//! String-backed newtypes for scheduler identifiers.
//!
//! Definitions live in `rio-common`. Currently the scheduler is the
//! only consumer — rio-worker and rio-proto use raw strings. Re-
//! exported here for `use crate::state::DrvHash` compat so the
//! scheduler's 80+ import sites don't churn if the upstream path
//! changes.

pub use rio_common::newtype::{DrvHash, WorkerId};

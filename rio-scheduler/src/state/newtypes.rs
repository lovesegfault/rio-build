//! String-backed newtypes for scheduler identifiers.
//!
//! Moved to `rio-common` so rio-worker and rio-proto can share the same
//! types. Re-exported here for `use crate::state::DrvHash` compat — the
//! scheduler's 80+ import sites don't need to churn.

pub use rio_common::newtype::{DrvHash, WorkerId};

//! Scenario registry. One module per scenario; `ALL` is the manual slice
//! the scheduler iterates.
//!
//! Seeded from `.stress-test/issues/` — each scenario is "the check that
//! would have caught I-NNN on the live cluster". See
//! `~/tmp/rio-qa/i-nnn-scenarios.md` for the full candidate list.

use super::Scenario;

mod i048c_blackhole_self_test;

pub static ALL: &[&dyn Scenario] = &[
    &i048c_blackhole_self_test::BlackholeSelfTest,
    // top-10 seeds (i095, i163, i056, i043, i165, i201, i032, i078,
    // i114) follow once the scheduler compiles end-to-end.
];

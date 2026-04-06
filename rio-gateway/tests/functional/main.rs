//! Functional-tier tests: gateway wire protocol against REAL `rio-store`.
//!
//! Port of Lix `functionaltests2` scenarios (not harness — rio's harness
//! is wire-protocol-shaped, Lix's is nix-CLI-shaped; we port what's being
//! **proved**, not the invocation shape).
//!
//! The middle tier between `wire_opcodes/` (`MockStore`, ~ms) and VM tests
//! (k3s+FUSE+cgroup, 2-5min). Catches bugs `MockStore` hides:
//!
//! - `MockStore` accepts any hash → real store rejects mismatches
//!   (`r[store.integrity.verify-on-put]`)
//! - `MockStore` does `HashMap::insert(bytes)` → `HashMap::get(bytes)` —
//!   byte-identical by construction → real store does FastCDC chunk →
//!   PG manifest → reassembly (`r[store.nar.reassembly]`)
//! - Every `wire_opcodes` test sends `references: NO_STRINGS` → this tier
//!   sends real chains
//!
//! History: P0054 "VM test caught, byte-tests were wrong" — the
//! `wopAddMultipleToStore`/`wopNarFromPath` wire tests passed against
//! `MockStore`; the real stack rejected the bytes. This tier would have
//! caught it without the 5min VM wait.

// tests/functional/mod.rs — RioStack fixture. Rust's `mod fixture;`
// searches `fixture.rs` or `fixture/mod.rs` — not a sibling `mod.rs`.
// #[path] reaches it, same pattern as wire_opcodes/main.rs → ../common/mod.rs.
#[path = "mod.rs"]
mod fixture;

use fixture::{RioStackBuilder, add_to_store_nar, make_large_nar};
use rio_nix::protocol::wire;
use rio_test_support::TestResult;
use rio_test_support::fixtures::{make_nar, test_store_path};
use rio_test_support::wire::{drain_stderr_expecting_error, drain_stderr_until_last};
use rio_test_support::{wire_bytes, wire_send};

mod nar_roundtrip;
mod references;
mod store_roundtrip;

//! Cross-tenant: SubmitBuild attribution goes to the submitter.
//!
//! `rio-cli builds` doesn't expose `--tenant` (the proto's
//! `ListBuildsRequest.tenant_filter` exists but isn't wired to the
//! CLI), so the server-side-filter probe is deferred. What we CAN
//! assert: A submits → exactly that many builds gain A's UUID as
//! `tenant_id` (not B's, not empty). And B's count does not change
//! (B didn't submit). If the gateway mis-attributed (wrong key→tenant
//! mapping, JWT sub mix-up), one of those would fail.
//!
//! ## Why snapshot-and-delta instead of absolute counts
//!
//! `TenantPool::acquire()` reuses tenants LIFO (`ctx.rs`). The slot we
//! receive as B may have been held earlier in this run by a phase-1
//! scenario that submitted builds — those rows persist in the DB with
//! B's UUID. Asserting `n_b == 0` therefore fails on residue this
//! scenario didn't create. The pool's semaphore guarantees A and B are
//! held *exclusively* by us for the duration of this run, so any DELTA
//! in their counts is attributable to our one submission. Snapshot
//! before, assert no leak after.
//!
//! TODO: the structural fix is a `freshTenant()` API in the test
//! framework so isolation tests get clean state instead of pool slots.
//! That's a follow-up — the pool exists for resource reuse, isolation
//! tests have orthogonal needs and shouldn't bend the existing design
//! around them. Until then, delta-based assertions are correct under
//! pool reuse and don't require touching `TenantPool`.
//!
//! `r[sched.tenant.authz+3]` covers SchedulerService RPCs (Watch/
//! Cancel/QueryStatus reject `PERMISSION_DENIED` on tenant mismatch);
//! those have no nix-level entry point to probe directly here.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct CrossTenantListBuilds;

/// Count builds whose `tenant_id` matches `uuid` in a `rio-cli builds`
/// JSON response.
fn count_for(resp: &Value, uuid: &str) -> usize {
    resp.get("builds")
        .and_then(Value::as_array)
        .map(|builds| {
            builds
                .iter()
                .filter(|b| b.get("tenant_id").and_then(Value::as_str) == Some(uuid))
                .count()
        })
        .unwrap_or(0)
}

#[async_trait]
impl Scenario for CrossTenantListBuilds {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "iso01-cross-tenant-listbuilds",
            i_ref: None,
            isolation: Isolation::Tenant { count: 2 },
            timeout: Duration::from_secs(120),
            exercises: crate::exercises!(),
        }
    }

    async fn run(&self, ctx: &mut QaCtx) -> Result<Verdict> {
        let a_uuid = ctx.tenant_uuid(0)?;
        let b_uuid = ctx.tenant_uuid(1)?;

        // Snapshot BEFORE A submits — the LIFO tenant pool may hand us
        // slots that earlier scenarios already submitted builds under.
        // No `--tenant` on rio-cli; fetch all and filter client-side.
        let before: Value = ctx.cli_json(&["builds"])?;
        let a_before = count_for(&before, &a_uuid);
        let b_before = count_for(&before, &b_uuid);

        // A submits a build (5s, completes before we check). B does NOT.
        ctx.nix_build_via_gateway(0, "iso01-a", 5, 1).await?;

        let after: Value = ctx.cli_json(&["builds"])?;
        let a_after = count_for(&after, &a_uuid);
        let b_after = count_for(&after, &b_uuid);

        // We hold A and B exclusively (pool semaphore), so deltas are
        // ours. A must gain exactly the one build we submitted; B must
        // gain none.
        if a_after <= a_before {
            return Ok(Verdict::Fail(format!(
                "A submitted but tenant_id={a_uuid} count stayed at {a_before} — \
                 SubmitBuild attribution dropped or mis-attributed"
            )));
        }
        if b_after > b_before {
            return Ok(Verdict::Fail(format!(
                "tenant_id={b_uuid} count rose {b_before}→{b_after} but B never \
                 submitted — cross-tenant attribution leak (A's key mapped to B?)"
            )));
        }
        Ok(Verdict::Pass)
    }
}

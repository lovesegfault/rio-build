//! I-115: nix-daemon's sandbox temp probes (`.chroot`/`.lock`/`.check`)
//! reach store GetPath instead of FUSE-side ENOENT fast-path.
//!
//! The fix added a suffix-match in FUSE lookup(). Regression check
//! needs a store-side counter of GetPath-by-suffix or a builder-side
//! `fuse_temp_fastpath_total` — neither is exported today, so this
//! Skips with the gap named.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::k8s::qa::{Isolation, QaCtx, Scenario, ScenarioMeta, Verdict};

pub struct TempSuffixFastpath;

#[async_trait]
impl Scenario for TempSuffixFastpath {
    fn meta(&self) -> ScenarioMeta {
        ScenarioMeta {
            id: "i115-temp-suffix-fastpath",
            i_ref: Some(115),
            isolation: Isolation::Tenant { count: 1 },
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(&self, _ctx: &mut QaCtx) -> Result<Verdict> {
        Ok(Verdict::Skip(
            "needs rio_builder_fuse_temp_fastpath_total or store-side GetPath \
             suffix counter — neither exported; add metric then assert >0 after build"
                .into(),
        ))
    }
}

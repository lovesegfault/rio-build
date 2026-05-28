//! `cargo xtask parity status` — render campaign progress and Job state.
//!
//! Progress comes from the campaign's progress.json in S3: the engine
//! rewrites it on every watchdog tick and its poller syncs it (with the
//! rest of the state dir) to `parity/campaigns/<id>/progress.json`, so
//! the document is at most a few minutes stale. The campaign Job's
//! active/succeeded/failed counts come from the cluster, and `--watch`
//! re-reads both every 30s, stamping each block with the local poll
//! time. Once one poll has succeeded, `--watch` treats kube/S3 errors
//! as transient — they are printed and polling continues, so a single
//! 503 cannot kill an hours-long watch. Whether a finished campaign
//! drained completely or stopped at its deadline is the `partial` flag
//! in summary.md — `cargo xtask parity report` renders that; the
//! engine's exit code does not distinguish the two.

use anyhow::Result;
use clap::Args;
use k8s_openapi::api::batch::v1::Job;
use kube::api::Api;

use super::NS_PARITY;
use crate::k8s::client as kclient;

#[derive(Args)]
pub struct StatusArgs {
    /// Campaign id (the Job name printed by `parity launch`).
    pub campaign: String,
    /// Poll every 30s until interrupted.
    #[arg(long)]
    pub watch: bool,
}

/// One human-readable line per interesting progress.json field; falls
/// back to the raw document when the shape is unknown. The engine owns
/// the schema (`rio_parity::run::report::Progress`, camelCase): stage,
/// per-bucket counts, attempted (with its inScope/attemptable
/// denominators from the comparability block), infra / hydra-unknown
/// rates, throughput, ETA, suspension windows, completeness.
pub fn summarize_progress(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.to_owned();
    };
    let mut lines = Vec::new();
    for key in ["stage", "updatedAt"] {
        if let Some(s) = v.get(key).and_then(|s| s.as_str()) {
            lines.push(format!("{key}: {s}"));
        }
    }
    for key in [
        "attempted",
        "bucketCounts",
        "infraRatePct",
        "hydraUnknownRatePct",
        "jobsPerHour",
        "etaHours",
    ] {
        if let Some(val) = v.get(key).filter(|val| !val.is_null()) {
            lines.push(format!("{key}: {val}"));
        }
    }
    if let Some(windows) = v
        .get("suspension")
        .and_then(|s| s.get("windows"))
        .and_then(|w| w.as_array())
    {
        lines.push(format!("suspension windows: {}", windows.len()));
    }
    // Comparability block: the denominators the attempted count is
    // measured against, and how much of the in-scope set is terminal.
    if let Some(c) = v.get("comparability") {
        let in_scope = c.get("inScope").filter(|x| !x.is_null());
        let attemptable = c.get("attemptable").filter(|x| !x.is_null());
        if let (Some(in_scope), Some(attemptable)) = (in_scope, attemptable) {
            lines.push(format!("inScope/attemptable: {in_scope} / {attemptable}"));
        }
        if let Some(pct) = c.get("completenessPct").filter(|p| !p.is_null()) {
            lines.push(format!("completenessPct: {pct}"));
        }
    }
    if lines.is_empty() {
        return serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_owned());
    }
    lines.join("\n")
}

/// Local wall-clock stamp for one `--watch` block, second precision with
/// the UTC offset so a pasted scrollback stays unambiguous.
fn poll_stamp(now: &jiff::Zoned) -> String {
    now.strftime("%Y-%m-%d %H:%M:%S %:z").to_string()
}

#[allow(clippy::print_stdout)]
pub async fn run(a: StatusArgs) -> Result<()> {
    let store = super::s3::CampaignStore::discover()?;
    let client = kclient::client().await?;
    let jobs_api: Api<Job> = Api::namespaced(client, NS_PARITY);

    // Fail fast until one poll has succeeded (a bad kubeconfig or AWS
    // credential chain should abort immediately); after that, --watch
    // treats poll errors as transient: print them and keep polling.
    let mut succeeded_once = false;
    loop {
        if a.watch {
            println!("[{}]", poll_stamp(&jiff::Zoned::now()));
        }
        match poll_once(&a, &jobs_api, &store).await {
            Ok(()) => succeeded_once = true,
            Err(e) if a.watch && succeeded_once => {
                println!("poll failed (will retry in 30s): {e:#}");
            }
            Err(e) => return Err(e),
        }

        if !a.watch {
            break;
        }
        println!("---");
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
    Ok(())
}

/// One status poll: campaign Job state from the cluster, then the
/// progress.json summary from S3.
#[allow(clippy::print_stdout)]
async fn poll_once(
    a: &StatusArgs,
    jobs_api: &Api<Job>,
    store: &super::s3::CampaignStore,
) -> Result<()> {
    // Job state (an absent Job is fine — abort/cleanup may have
    // deleted it while the S3 artifacts remain the source of truth).
    match jobs_api.get_opt(&a.campaign).await? {
        Some(job) => {
            let st = job.status.unwrap_or_default();
            println!(
                "job {NS_PARITY}/{}: active={} succeeded={} failed={}",
                a.campaign,
                st.active.unwrap_or(0),
                st.succeeded.unwrap_or(0),
                st.failed.unwrap_or(0),
            );
        }
        None => println!(
            "job {NS_PARITY}/{}: not found (deleted or never launched)",
            a.campaign
        ),
    }
    match store
        .fetch_campaign_doc(&a.campaign, "progress.json")
        .await?
    {
        Some(progress) => println!("{}", summarize_progress(&progress)),
        None => println!(
            "{}: not written yet (engine still in plan/warm — or wrong campaign id?)",
            store.uri(&a.campaign, "progress.json")
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rio_parity::run::report::Progress;
    use rio_parity::run::spec::ComparabilityBlock;
    use rio_parity::run::watchdog::SuspensionSummary;

    use super::*;

    #[test]
    fn summarize_known_fields_and_fall_back_to_raw() {
        // Build the fixture with the engine's own Progress type so a field
        // rename on the engine side fails this test instead of silently
        // degrading status output to the raw-JSON fallback.
        let progress = Progress {
            campaign_id: "parity-leaf-20260601-ab12".into(),
            stage: "submit+collect".into(),
            updated_at: "2026-06-02T03:04:05Z".into(),
            bucket_counts: BTreeMap::from([
                ("match-built".to_string(), 12),
                ("queued".to_string(), 38),
            ]),
            attempted: 50,
            infra_rate_pct: Some(1.5),
            hydra_unknown_rate_pct: None,
            jobs_per_hour: Some(120.0),
            eta_hours: Some(2.0),
            suspension: SuspensionSummary::default(),
            comparability: ComparabilityBlock {
                in_scope: 100,
                attemptable: 80,
                completeness_pct: 12.5,
                ..Default::default()
            },
        };
        let raw = serde_json::to_string(&progress).unwrap();
        let s = summarize_progress(&raw);
        assert!(s.contains("stage: submit+collect"), "{s}");
        assert!(s.contains("updatedAt: 2026-06-02T03:04:05Z"), "{s}");
        assert!(s.contains("\"match-built\":12"), "{s}");
        assert!(s.contains("attempted: 50"), "{s}");
        assert!(s.contains("infraRatePct: 1.5"), "{s}");
        assert!(s.contains("jobsPerHour: 120"), "{s}");
        assert!(s.contains("etaHours: 2"), "{s}");
        assert!(s.contains("suspension windows: 0"), "{s}");
        // The attempted count renders next to its denominators and the
        // terminal-completeness figure from the comparability block.
        assert!(s.contains("inScope/attemptable: 100 / 80"), "{s}");
        assert!(s.contains("completenessPct: 12.5"), "{s}");
        // Null fields are dropped, not rendered as "null".
        assert!(!s.contains("hydraUnknownRatePct"), "{s}");
        assert!(!s.contains("null"), "{s}");

        // Unknown shape → pretty JSON; non-JSON → verbatim.
        let unknown = r#"{"something":"else"}"#;
        assert!(summarize_progress(unknown).contains("something"));
        assert_eq!(summarize_progress("not json"), "not json");
    }

    #[test]
    fn poll_stamp_renders_local_time_with_offset() {
        // Fixed instant in a fixed zone so the strftime spec itself is
        // exercised deterministically (a bad spec panics here, not at
        // the first live --watch poll).
        let zoned = jiff::civil::date(2026, 6, 2)
            .at(3, 4, 5, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap();
        assert_eq!(poll_stamp(&zoned), "2026-06-02 03:04:05 +00:00");
    }
}

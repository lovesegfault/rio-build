//! `rio-cli derivations` — actor in-memory DAG snapshot for a build.
//!
//! Unlike `builds` (PG summary) or `GetBuildGraph` (PG graph), this
//! queries the LIVE actor — exactly what `dispatch_ready()` sees. The
//! I-025 diagnostic: if a derivation is `Assigned` to an executor whose
//! stream is dead (`⚠ no-stream`), dispatch is stuck forever.
//!
//! Separate module (not inline in `main.rs`) — same convention as
//! `cutoffs.rs`/`wps.rs`: keep `main.rs` deltas to enum variant +
//! match arm + mod decl only.

use anyhow::anyhow;
use rio_proto::AdminServiceClient;
use rio_proto::types::{InspectBuildDagRequest, ListBuildsRequest};
use serde::Serialize;
use tonic::transport::Channel;

use crate::{json, rpc};

/// Run the `derivations` subcommand.
pub(crate) async fn run(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
    build_id: Option<String>,
    all_active: bool,
    status: Option<String>,
    stuck: bool,
) -> anyhow::Result<()> {
    // Resolve build_id(s): either the one given, or all active.
    let build_ids: Vec<String> = if all_active {
        rpc(
            "ListBuilds",
            client.list_builds(ListBuildsRequest {
                status_filter: "active".into(),
                limit: 50,
                ..Default::default()
            }),
        )
        .await?
        .builds
        .into_iter()
        .map(|b| b.build_id)
        .collect()
    } else {
        vec![build_id.ok_or_else(|| anyhow!("BUILD_ID required (or use --all-active)"))?]
    };

    if build_ids.is_empty() {
        if as_json {
            json(&Vec::<()>::new())?;
        } else {
            println!("(no active builds)");
        }
        return Ok(());
    }

    // Collect JSON rows across builds; print text per-build inline.
    // Responses held for the lifetime-borrowing DrvRow projection.
    let mut json_out: Vec<Out<'_>> = Vec::with_capacity(build_ids.len());
    let mut resps = Vec::with_capacity(build_ids.len());
    for id in &build_ids {
        resps.push(
            rpc(
                "InspectBuildDag",
                client.inspect_build_dag(InspectBuildDagRequest {
                    build_id: id.clone(),
                }),
            )
            .await?,
        );
    }

    for (i, (id, resp)) in build_ids.iter().zip(&resps).enumerate() {
        let mut drvs: Vec<_> = resp
            .derivations
            .iter()
            .filter(|d| status.as_ref().is_none_or(|s| d.status == *s))
            .filter(|d| !stuck || (!d.assigned_executor.is_empty() && !d.executor_has_stream))
            .collect();
        // Sort: stuck first, then by status, then by name.
        drvs.sort_by(|a, b| {
            let a_stuck = !a.assigned_executor.is_empty() && !a.executor_has_stream;
            let b_stuck = !b.assigned_executor.is_empty() && !b.executor_has_stream;
            b_stuck
                .cmp(&a_stuck)
                .then(a.status.cmp(&b.status))
                .then(a.drv_path.cmp(&b.drv_path))
        });

        if as_json {
            json_out.push(Out {
                build_id: all_active.then_some(id.as_str()),
                derivations: drvs
                    .iter()
                    .map(|d| DrvRow {
                        drv_path: &d.drv_path,
                        drv_hash: &d.drv_hash,
                        status: &d.status,
                        is_fod: d.is_fod,
                        assigned_executor: &d.assigned_executor,
                        executor_has_stream: d.executor_has_stream,
                        retry_count: d.retry_count,
                        infra_retry_count: d.infra_retry_count,
                        backoff_remaining_secs: d.backoff_remaining_secs,
                        interested_build_count: d.interested_build_count,
                        system: &d.system,
                        required_features: &d.required_features,
                        failed_builders: &d.failed_builders,
                        rejections: d
                            .rejections
                            .iter()
                            .map(|r| Rejection {
                                executor_id: &r.executor_id,
                                reason: &r.reason,
                            })
                            .collect(),
                    })
                    .collect(),
                live_executor_ids: &resp.live_executor_ids,
            });
        } else {
            // Per-build header (only when iterating multiple).
            if build_ids.len() > 1 {
                if i > 0 {
                    println!();
                }
                println!("═══ build {id} ═══");
            }
            // Summary header.
            let by_status: std::collections::BTreeMap<&str, usize> =
                drvs.iter().fold(Default::default(), |mut m, d| {
                    *m.entry(d.status.as_str()).or_default() += 1;
                    m
                });
            let stuck_count = drvs
                .iter()
                .filter(|d| !d.assigned_executor.is_empty() && !d.executor_has_stream)
                .count();
            println!(
                "{} derivations ({} live executors in stream pool)",
                drvs.len(),
                resp.live_executor_ids.len()
            );
            for (s, n) in &by_status {
                println!("  {s}: {n}");
            }
            if stuck_count > 0 {
                println!("  ⚠ {stuck_count} assigned to dead-stream executors (I-025)");
            }
            println!();
            // Per-derivation lines.
            for d in &drvs {
                let name = d
                    .drv_path
                    .rsplit_once('-')
                    .map(|(_, n)| n)
                    .unwrap_or(&d.drv_path);
                let name = name.strip_suffix(".drv").unwrap_or(name);
                let fod = if d.is_fod { " [FOD]" } else { "" };
                // I-062: surface hard_filter inputs that would
                // explain a stuck-Ready. system always shown
                // (cheap, distinguishes "builtin" FODs from
                // arch-specific); features/failed-on only when
                // non-empty (rare — when present, THE answer).
                let sys = format!(" [{}]", d.system);
                let feats = if d.required_features.is_empty() {
                    String::new()
                } else {
                    format!(" feat={}", d.required_features.join(","))
                };
                let failed_on = if d.failed_builders.is_empty() {
                    String::new()
                } else {
                    format!(" ⚠ failed-on:{}", d.failed_builders.join(","))
                };
                let stream = if !d.assigned_executor.is_empty() {
                    if d.executor_has_stream {
                        format!(" → {}", d.assigned_executor)
                    } else {
                        format!(" → {} ⚠ no-stream", d.assigned_executor)
                    }
                } else {
                    String::new()
                };
                let backoff = if d.backoff_remaining_secs > 0 {
                    format!(" (backoff {}s)", d.backoff_remaining_secs)
                } else {
                    String::new()
                };
                let retries = if d.retry_count > 0 || d.infra_retry_count > 0 {
                    format!(" r={}/i={}", d.retry_count, d.infra_retry_count)
                } else {
                    String::new()
                };
                println!(
                    "  [{:<9}]{fod}{sys} {name}{stream}{backoff}{retries}{feats}{failed_on}",
                    d.status
                );
                // I-062: per-executor rejection reasons. Only
                // populated for Ready (server-side gate). When
                // present, this IS the answer to "why won't it
                // dispatch" — every executor named with the
                // first hard_filter clause that rejects, or
                // ACCEPT (which means the rejection is OUTSIDE
                // hard_filter — that's the load-bearing finding).
                if !d.rejections.is_empty() {
                    let rj = d
                        .rejections
                        .iter()
                        .map(|r| format!("{}={}", r.executor_id, r.reason))
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!("      rejected-by: {rj}");
                }
            }
        }
    }

    if as_json {
        // --all-active: array of per-build objects.
        // Single build: the one object directly (back-compat).
        if all_active {
            json(&json_out)?;
        } else {
            json(&json_out[0])?;
        }
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// JSON projection
// ───────────────────────────────────────────────────────────────────────────
//
// Prost types don't derive Serialize (see JSON output section in
// `main.rs`) — project to module-local structs. Includes the raw
// live_executor_ids list for "this executor is in PG but not here"
// scripting.

#[derive(Serialize)]
struct DrvRow<'a> {
    drv_path: &'a str,
    drv_hash: &'a str,
    status: &'a str,
    is_fod: bool,
    assigned_executor: &'a str,
    executor_has_stream: bool,
    retry_count: u32,
    infra_retry_count: u32,
    backoff_remaining_secs: u64,
    interested_build_count: u32,
    system: &'a str,
    required_features: &'a [String],
    failed_builders: &'a [String],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rejections: Vec<Rejection<'a>>,
}

#[derive(Serialize)]
struct Rejection<'a> {
    executor_id: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct Out<'a> {
    // Present only in --all-active mode so single-build JSON
    // shape is unchanged (back-compat for existing scripts).
    #[serde(skip_serializing_if = "Option::is_none")]
    build_id: Option<&'a str>,
    derivations: Vec<DrvRow<'a>>,
    live_executor_ids: &'a [String],
}

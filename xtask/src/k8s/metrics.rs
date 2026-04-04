//! `xtask k8s metrics` — one-shot Prometheus scrape of key gauges.
//!
//! Stress sessions I-128→I-148 were debugged via log-grep and ad-hoc
//! `cli builds` polls. Every component emits `rio_*` on :9091/:9092
//! but until P0539a lands no Prometheus scrapes them. This subcommand
//! is the stop-gap: port-forward (in-process, kube-rs) to the
//! scheduler leader + each store replica, GET /metrics, pretty-print
//! the handful of gauges that answer "is the actor wedged?".
//!
//! Targets <5s wall-clock — one scrape per pod, no polling.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use console::style;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::k8s::{NS, NS_STORE};
use crate::kube as k;

/// Scheduler metrics container port (scheduler.yaml `name: metrics`).
/// The Service spec only exposes 9001 (gRPC) — must target the pod.
const SCHED_METRICS_PORT: u16 = 9091;
/// Store metrics container port (store.yaml).
const STORE_METRICS_PORT: u16 = 9092;

/// Per-scrape timeout. Anything slower than this and the cluster has
/// bigger problems than a missing gauge.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(3);

/// Scheduler gauges to print as-is (name → display label). Labelled
/// gauges (per-pool, per-class) print every series; scalars print one.
/// `actor_mailbox_depth` lands in P0539c — absent until then, rendered
/// as `-`.
const SCHED_GAUGES: &[(&str, &str)] = &[
    ("rio_scheduler_actor_mailbox_depth", "mailbox_depth"),
    ("rio_scheduler_derivations_queued", "derivations_queued"),
    ("rio_scheduler_derivations_running", "derivations_running"),
    ("rio_scheduler_workers_active", "workers_active"),
    ("rio_scheduler_builds_active", "builds_active"),
    ("rio_scheduler_fod_queue_depth", "fod_queue_depth"),
    ("rio_scheduler_class_queue_depth", "class_queue_depth"),
];

/// Store gauges, summarized per replica.
const STORE_GAUGES: &[(&str, &str)] = &[
    ("rio_store_pg_pool_utilization", "pg_pool_util"),
    ("rio_store_s3_deletes_pending", "s3_del_pending"),
    ("rio_store_chunk_dedup_ratio", "dedup_ratio"),
];

#[allow(clippy::print_stdout, clippy::print_stderr)]
pub async fn run(dry_run: bool) -> Result<()> {
    let client = k::client().await?;

    let leader = k::scheduler_leader(&client, NS)
        .await
        .context("scheduler leader Lease — is rio deployed?")?;
    let store_pods = list_store_pods(&client).await?;

    if dry_run {
        println!("would scrape:");
        println!("  scheduler  {NS}/{leader}:{SCHED_METRICS_PORT}/metrics (leader)");
        for p in &store_pods {
            println!("  store      {NS_STORE}/{p}:{STORE_METRICS_PORT}/metrics");
        }
        println!(
            "gauges: {}",
            SCHED_GAUGES
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!(
            "        {}",
            STORE_GAUGES
                .iter()
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!("        rio_scheduler_actor_cmd_seconds_{{sum,count}} → mean by cmd");
        return Ok(());
    }

    // Scheduler leader.
    eprintln!(
        "{} {} {}",
        style("▸").blue(),
        style("scheduler").bold(),
        style(format!("({leader})")).dim()
    );
    match scrape(&client, NS, &leader, SCHED_METRICS_PORT).await {
        Ok(body) => {
            let scrape = Scrape::parse(&body);
            for &(metric, label) in SCHED_GAUGES {
                print_gauge(&scrape, metric, label);
            }
            print_actor_cmd_means(&scrape);
        }
        Err(e) => eprintln!("  {} scrape failed: {e:#}", style("✗").red()),
    }

    // Store replicas.
    eprintln!(
        "{} {} {}",
        style("▸").blue(),
        style("store").bold(),
        style(format!("({} replicas)", store_pods.len())).dim()
    );
    if store_pods.is_empty() {
        eprintln!("  {} no rio-store pods", style("·").dim());
    }
    for pod in &store_pods {
        match scrape(&client, NS_STORE, pod, STORE_METRICS_PORT).await {
            Ok(body) => {
                let scrape = Scrape::parse(&body);
                let line: Vec<String> = STORE_GAUGES
                    .iter()
                    .map(|&(metric, label)| {
                        let v = scrape
                            .first(metric)
                            .map(fmt_val)
                            .unwrap_or_else(|| "-".into());
                        format!("{label}={v}")
                    })
                    .collect();
                eprintln!("  {} {:<28} {}", style("·").dim(), pod, line.join("  "));
            }
            Err(e) => eprintln!("  {} {:<28} scrape failed: {e:#}", style("✗").red(), pod),
        }
    }

    Ok(())
}

async fn list_store_pods(client: &k::Client) -> Result<Vec<String>> {
    let api: Api<Pod> = Api::namespaced(client.clone(), NS_STORE);
    let lp = ListParams::default().labels("app.kubernetes.io/name=rio-store");
    let mut names: Vec<String> = api
        .list(&lp)
        .await?
        .items
        .into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .filter(|p| {
            p.status
                .as_ref()
                .and_then(|s| s.phase.as_deref())
                .map(|ph| ph == "Running")
                .unwrap_or(false)
        })
        .filter_map(|p| p.metadata.name)
        .collect();
    names.sort();
    Ok(names)
}

/// In-process port-forward + minimal HTTP/1.0 GET /metrics. Same
/// approach as `status::gather_scheduler_metrics` and
/// `smoke::sched_metric` — pulling hyper for one request is heavier
/// than 10 lines, and kube's portforward hands back a duplex stream.
async fn scrape(client: &k::Client, ns: &str, pod: &str, port: u16) -> Result<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let fut = async {
        let mut pf = pods.portforward(pod, &[port]).await?;
        let mut stream = pf.take_stream(port).context("no portforward stream")?;
        stream
            .write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
            .await?;
        let mut body = String::new();
        stream.read_to_string(&mut body).await?;
        anyhow::Ok(body)
    };
    tokio::time::timeout(SCRAPE_TIMEOUT, fut)
        .await
        .context("scrape timed out")?
}

#[allow(clippy::print_stderr)]
fn print_gauge(scrape: &Scrape, metric: &str, label: &str) {
    match scrape.series(metric) {
        [] => eprintln!("  {:<24} {}", label, style("-").dim()),
        [(labels, v)] if labels.is_empty() => {
            eprintln!("  {:<24} {}", label, style(fmt_val(*v)).cyan())
        }
        many => {
            let parts: Vec<String> = many
                .iter()
                .map(|(l, v)| format!("{l}={}", fmt_val(*v)))
                .collect();
            eprintln!("  {:<24} {}", label, parts.join("  "));
        }
    }
}

/// `actor_cmd_seconds` is a histogram; the cheap "p99-ish" without a
/// real quantile is sum/count per `cmd` label — i.e. mean latency.
/// Good enough to spot the one cmd that's 100× the rest (the I-139
/// signature). Real p99 lands with P0539a's Prometheus.
#[allow(clippy::print_stderr)]
fn print_actor_cmd_means(scrape: &Scrape) {
    let sums = scrape.labelled("rio_scheduler_actor_cmd_seconds_sum");
    let counts = scrape.labelled("rio_scheduler_actor_cmd_seconds_count");
    if sums.is_empty() {
        eprintln!("  {:<24} {}", "actor_cmd_mean", style("-").dim());
        return;
    }
    let mut rows: Vec<(String, f64, f64)> = sums
        .iter()
        .filter_map(|(labels, sum)| {
            let cmd = label_value(labels, "cmd")?;
            let count = *counts.get(labels)?;
            (count > 0.0).then(|| (cmd, sum / count, count))
        })
        .collect();
    // Slowest first — that's the one you're looking for.
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));
    eprintln!(
        "  {:<24} {}",
        "actor_cmd_mean",
        style("(sum/count by cmd, slowest first)").dim()
    );
    for (cmd, mean, count) in rows.iter().take(8) {
        eprintln!(
            "    {:<22} {:>10}  {}",
            cmd,
            style(format!("{:.3}s", mean)).cyan(),
            style(format!("n={count:.0}")).dim()
        );
    }
}

fn fmt_val(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

/// Extract `key="value"` from a prom label set `{a="x",b="y"}`.
fn label_value(labels: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = labels.find(&needle)? + needle.len();
    let end = labels[start..].find('"')?;
    Some(labels[start..start + end].to_string())
}

/// Parsed prometheus text exposition. Keyed by metric name → list of
/// `(label-set, value)`. Label set is the raw `{…}` string (empty for
/// unlabelled scalars) — good enough for display and for matching
/// `_sum` against `_count` of the same series.
struct Scrape {
    by_name: BTreeMap<String, Vec<(String, f64)>>,
}

impl Scrape {
    fn parse(body: &str) -> Self {
        let mut by_name: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // `name{labels} value` or `name value`. Split on the LAST
            // whitespace — label values can't contain unescaped spaces
            // in the metrics crate's exposition, but this is robust to
            // a HTTP status line sneaking through (body includes the
            // response headers; harmlessly parsed-and-ignored).
            let Some((head, val)) = line.rsplit_once(char::is_whitespace) else {
                continue;
            };
            let Ok(val) = val.parse::<f64>() else {
                continue;
            };
            let (name, labels) = match head.find('{') {
                Some(i) => (&head[..i], head[i..].to_string()),
                None => (head, String::new()),
            };
            by_name
                .entry(name.to_string())
                .or_default()
                .push((labels, val));
        }
        Self { by_name }
    }

    fn series(&self, name: &str) -> &[(String, f64)] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    fn first(&self, name: &str) -> Option<f64> {
        self.series(name).first().map(|(_, v)| *v)
    }

    fn labelled(&self, name: &str) -> BTreeMap<String, f64> {
        self.series(name).iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"HTTP/1.0 200 OK
Content-Type: text/plain

# HELP rio_scheduler_derivations_queued queued
# TYPE rio_scheduler_derivations_queued gauge
rio_scheduler_derivations_queued 42
rio_scheduler_workers_active{pool="x86-64-tiny"} 3
rio_scheduler_workers_active{pool="x86-64-small"} 1
rio_scheduler_actor_cmd_seconds_sum{cmd="Dispatch"} 12.5
rio_scheduler_actor_cmd_seconds_count{cmd="Dispatch"} 50
rio_scheduler_actor_cmd_seconds_sum{cmd="Heartbeat"} 0.2
rio_scheduler_actor_cmd_seconds_count{cmd="Heartbeat"} 200
"#;

    #[test]
    fn parse_scalar() {
        let s = Scrape::parse(SAMPLE);
        assert_eq!(s.first("rio_scheduler_derivations_queued"), Some(42.0));
        assert_eq!(s.first("rio_scheduler_absent"), None);
    }

    #[test]
    fn parse_labelled() {
        let s = Scrape::parse(SAMPLE);
        let series = s.series("rio_scheduler_workers_active");
        assert_eq!(series.len(), 2);
        assert_eq!(
            label_value(&series[0].0, "pool").as_deref(),
            Some("x86-64-tiny")
        );
    }

    #[test]
    fn parse_ignores_headers_and_help() {
        let s = Scrape::parse(SAMPLE);
        // HTTP status line: "HTTP/1.0 200 OK" → rsplit gives "OK", not f64.
        assert!(!s.by_name.contains_key("HTTP/1.0"));
        // Content-Type header: "text/plain" not f64.
        assert!(!s.by_name.contains_key("Content-Type:"));
        // # HELP / # TYPE lines skipped.
        assert!(s.by_name.keys().all(|k| !k.starts_with('#')));
    }

    #[test]
    fn cmd_mean() {
        let s = Scrape::parse(SAMPLE);
        let sums = s.labelled("rio_scheduler_actor_cmd_seconds_sum");
        let counts = s.labelled("rio_scheduler_actor_cmd_seconds_count");
        let dispatch_labels = r#"{cmd="Dispatch"}"#;
        assert_eq!(
            sums.get(dispatch_labels).unwrap() / counts.get(dispatch_labels).unwrap(),
            0.25
        );
    }

    #[test]
    fn fmt_val_integral() {
        assert_eq!(fmt_val(42.0), "42");
        assert_eq!(fmt_val(0.123456), "0.123");
    }
}

//! `rio-cli workers --actor` / `--diff` — scheduler in-memory executor state.
//!
//! `ListExecutors` (the default `workers` path) reads PG `last_seen`.
//! After scheduler failover, heartbeats (unary RPC) reach the new
//! leader's PG immediately while the worker's BuildExecution stream
//! (long-lived bidi) is still stuck in TCP keepalive against the old
//! leader. PG says `[alive]`; the actor's `self.executors` HashMap
//! has no entry, so dispatch never reaches it. I-048b/c took four
//! wrong hypotheses to find — this RPC would have made it one look.
//!
//! `--diff` calls BOTH RPCs and shows divergence per-row.
//! `pg-only`: worker heartbeating but no stream to this leader.
//! `actor-only`: stream open, PG `last_seen` not updated yet (rare —
//! heartbeat is faster than stream-open). In both but
//! `has_stream=false`: I-048b zombie (entry exists, stream gone — the
//! pre-fix heartbeat-creates-entry path).

use std::collections::BTreeSet;

use rio_proto::AdminServiceClient;
use rio_proto::types::{
    DebugExecutorState, DrainExecutorRequest, ExecutorInfo, ExecutorKind, ListExecutorsRequest,
};
use serde::Serialize;
use tonic::transport::Channel;

use crate::{json, rpc};

// r[impl cli.workers.actor-diff]
/// `--actor` path: in-memory snapshot only.
#[derive(clap::Args, Clone)]
pub(crate) struct Args {
    /// Filter by worker status: "alive", "draining", or empty for all.
    /// Ignored with `--actor`/`--diff` (those read the full map).
    #[arg(long)]
    status: Option<String>,
    /// Read the scheduler actor's in-memory executor map instead of
    /// PG. Surfaces `has_stream`/`warm`/`kind` — the dispatch
    /// filter inputs. PG `last_seen` can't tell you if the stream
    /// to THIS leader is dead.
    #[arg(long, conflicts_with = "diff")]
    actor: bool,
    /// Join PG view (`ListExecutors`) with actor view
    /// (`DebugListExecutors`). `⚠` marks rows where they disagree:
    /// PG-only = stream not connected, actor-only = PG stale,
    /// both-but-no-stream = I-048b zombie.
    #[arg(long, conflicts_with = "actor")]
    diff: bool,
}

#[derive(clap::Args, Clone)]
pub(crate) struct DrainArgs {
    /// Worker ID (as shown by `rio-cli workers`).
    executor_id: String,
    /// Cancel running builds on the worker (reassign elsewhere)
    /// instead of waiting for them to complete.
    #[arg(long)]
    force: bool,
}

pub(crate) async fn run(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
    a: Args,
) -> anyhow::Result<()> {
    if a.actor {
        run_actor(as_json, client).await
    } else if a.diff {
        run_diff(as_json, client).await
    } else {
        run_pg(as_json, client, a.status).await
    }
}

pub(crate) async fn run_actor(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
) -> anyhow::Result<()> {
    let resp = rpc("DebugListExecutors", async || {
        client.debug_list_executors(()).await
    })
    .await?;
    if as_json {
        return json(&resp);
    }
    if resp.executors.is_empty() {
        println!("(actor map empty — no streams connected to this scheduler instance)");
        return Ok(());
    }
    for w in &resp.executors {
        print_actor_worker(w);
    }
    Ok(())
}

/// `--diff` path: PG ∪ actor, diverge markers per row.
pub(crate) async fn run_diff(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
) -> anyhow::Result<()> {
    // Gather both views before printing — same all-or-nothing
    // discipline as status::run.
    let pg = rpc("ListExecutors", async || {
        client.list_executors(ListExecutorsRequest::default()).await
    })
    .await?;
    let actor = rpc("DebugListExecutors", async || {
        client.debug_list_executors(()).await
    })
    .await?;

    // Index by executor_id. BTreeSet for stable output order.
    let pg_ids: BTreeSet<&str> = pg
        .executors
        .iter()
        .map(|e| e.executor_id.as_str())
        .collect();
    let actor_ids: BTreeSet<&str> = actor
        .executors
        .iter()
        .map(|e| e.executor_id.as_str())
        .collect();
    let all_ids: BTreeSet<&str> = pg_ids.union(&actor_ids).copied().collect();

    let pg_by_id: std::collections::HashMap<&str, &ExecutorInfo> = pg
        .executors
        .iter()
        .map(|e| (e.executor_id.as_str(), e))
        .collect();
    let actor_by_id: std::collections::HashMap<&str, &DebugExecutorState> = actor
        .executors
        .iter()
        .map(|e| (e.executor_id.as_str(), e))
        .collect();

    let rows: Vec<DiffRow<'_>> = all_ids
        .iter()
        .map(|id| DiffRow::new(id, pg_by_id.get(id).copied(), actor_by_id.get(id).copied()))
        .collect();
    let diverge_count = rows.iter().filter(|r| r.diverges()).count();

    if as_json {
        #[derive(Serialize)]
        struct DiffJson<'a> {
            diverge_count: usize,
            rows: &'a [DiffRow<'a>],
        }
        return json(&DiffJson {
            diverge_count,
            rows: &rows,
        });
    }

    for r in &rows {
        r.print();
    }
    if diverge_count > 0 {
        println!("\n⚠ {diverge_count} worker(s) diverge between PG and actor map");
    } else {
        println!("\n✓ PG and actor map agree ({} workers)", rows.len());
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// Diff row
// ───────────────────────────────────────────────────────────────────────────

/// One executor's PG view vs actor view, joined by ID. Either side may
/// be absent. Serde-serializable for `--json` so the divergence is
/// machine-readable too (e.g. for an alert that polls `--diff --json`
/// and pages on `diverge_count > 0`).
#[derive(Serialize)]
struct DiffRow<'a> {
    executor_id: &'a str,
    /// `pg-only` / `actor-only` / `both`. The marker the human output
    /// keys off; serialized so jq filters can do `select(.presence !=
    /// "both")`.
    presence: &'static str,
    /// PG `status` field (e.g. `"alive"`). None if PG-only-absent.
    pg_status: Option<&'a str>,
    /// Actor's `has_stream`. None if actor-only-absent. The I-048b
    /// signal: `presence == "both" && !actor_has_stream` = zombie.
    actor_has_stream: Option<bool>,
    actor_is_registered: Option<bool>,
    actor_warm: Option<bool>,
    actor_kind: Option<&'static str>,
    actor_heartbeat_ago_secs: Option<u64>,
    actor_busy: Option<bool>,
    /// I-056b: gate `has_capacity()`. Either true → dispatch can't
    /// reach this worker even if stream/registered/warm all show Y.
    /// Surfaced here so `--diff` matches `--actor`'s header markers.
    actor_draining: Option<bool>,
    actor_store_degraded: Option<bool>,
}

impl<'a> DiffRow<'a> {
    fn new(
        id: &'a str,
        pg: Option<&'a ExecutorInfo>,
        actor: Option<&'a DebugExecutorState>,
    ) -> Self {
        let presence = match (pg.is_some(), actor.is_some()) {
            (true, true) => "both",
            (true, false) => "pg-only",
            (false, true) => "actor-only",
            (false, false) => unreachable!("id came from union of both sets"),
        };
        Self {
            executor_id: id,
            presence,
            pg_status: pg.map(|p| p.status.as_str()),
            actor_has_stream: actor.map(|a| a.has_stream),
            actor_is_registered: actor.map(|a| a.is_registered),
            actor_warm: actor.map(|a| a.warm),
            actor_kind: actor.map(kind_str),
            actor_heartbeat_ago_secs: actor.map(|a| a.last_heartbeat_ago_secs),
            actor_busy: actor.map(|a| a.running_build.is_some()),
            actor_draining: actor.map(|a| a.draining),
            actor_store_degraded: actor.map(|a| a.store_degraded),
        }
    }

    /// I-056b suffix markers (`⚠ DRAINING` / `⚠ DEGRADED`). Same
    /// strings as `print_actor_worker` so `--actor` and `--diff` scan
    /// identically. NOT a divergence (draining is operator-intent, not
    /// PG-vs-actor drift) so it doesn't bump `diverge_count`.
    fn capacity_markers(&self) -> String {
        let mut m = String::new();
        if self.actor_draining == Some(true) {
            m.push_str("  ⚠ DRAINING");
        }
        if self.actor_store_degraded == Some(true) {
            m.push_str("  ⚠ DEGRADED");
        }
        m
    }

    /// Whether this row should get the `⚠` marker. Three failure
    /// shapes: PG knows about it but actor doesn't (stream not
    /// connected to leader), actor knows but PG doesn't (rare — fresh
    /// stream, no heartbeat written yet), or both know but the stream
    /// is dead (I-048b zombie: heartbeat-created entry, no stream_tx).
    fn diverges(&self) -> bool {
        self.presence != "both" || self.actor_has_stream == Some(false)
    }

    fn print(&self) {
        let mark = if self.diverges() { "⚠" } else { " " };
        match self.presence {
            "pg-only" => {
                // Stream not connected to this leader. PG says it's
                // there; the actor never sees it. The I-048c symptom.
                println!(
                    "{mark} {}  pg=[{}]  actor=ABSENT  (stream not connected to leader)",
                    self.executor_id,
                    self.pg_status.unwrap_or("?"),
                );
            }
            "actor-only" => {
                // Fresh stream, PG last_seen not updated yet. Benign
                // and transient — usually clears on next heartbeat.
                println!(
                    "{mark} {}  pg=ABSENT  actor=[stream={} reg={} warm={} {} hb={}s busy={}]  (PG last_seen stale){}",
                    self.executor_id,
                    yn(self.actor_has_stream.unwrap()),
                    yn(self.actor_is_registered.unwrap()),
                    yn(self.actor_warm.unwrap()),
                    self.actor_kind.unwrap(),
                    self.actor_heartbeat_ago_secs.unwrap(),
                    yn(self.actor_busy.unwrap()),
                    self.capacity_markers(),
                );
            }
            "both" => {
                let mut suffix = String::new();
                if self.actor_has_stream == Some(false) {
                    suffix.push_str("  (ZOMBIE: entry exists, no stream)");
                }
                suffix.push_str(&self.capacity_markers());
                println!(
                    "{mark} {}  pg=[{}]  actor=[stream={} reg={} warm={} {} hb={}s busy={}]{suffix}",
                    self.executor_id,
                    self.pg_status.unwrap_or("?"),
                    yn(self.actor_has_stream.unwrap()),
                    yn(self.actor_is_registered.unwrap()),
                    yn(self.actor_warm.unwrap()),
                    self.actor_kind.unwrap(),
                    self.actor_heartbeat_ago_secs.unwrap(),
                    yn(self.actor_busy.unwrap()),
                );
            }
            _ => unreachable!(),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// --actor printing + JSON
// ───────────────────────────────────────────────────────────────────────────

fn print_actor_worker(w: &DebugExecutorState) {
    // ZOMBIE marker on the header line so it scans without reading
    // the field breakdown — the I-048b signature is `entry exists +
    // has_stream=false`, so `has_stream=N` here is the only thing an
    // operator needs to see to know dispatch can't reach this worker.
    // I-056b: draining/store_degraded gate has_capacity() — either true
    // means dispatch can't reach this worker even if stream/registered/
    // warm all show Y. Surface BOTH on the header line so they scan
    // without reading the field breakdown (same rationale as ZOMBIE).
    let mut markers = String::new();
    if !w.has_stream {
        markers.push_str("  ⚠ ZOMBIE (no stream)");
    }
    if w.draining {
        markers.push_str("  ⚠ DRAINING");
    }
    if w.store_degraded {
        markers.push_str("  ⚠ DEGRADED");
    }
    println!("worker {}  [{}]{markers}", w.executor_id, kind_str(w));
    println!(
        "  stream={}  registered={}  warm={}  hb={}s ago",
        yn(w.has_stream),
        yn(w.is_registered),
        yn(w.warm),
        w.last_heartbeat_ago_secs,
    );
    println!(
        "  systems:  {}",
        if w.systems.is_empty() {
            "(none — no heartbeat yet)".into()
        } else {
            w.systems.join(", ")
        }
    );
    match &w.running_build {
        Some(h) => println!("  running:  {h}"),
        None => println!("  running:  (idle)"),
    }
}

/// `kind` is wire-i32. `try_from` so unknown variants (proto added a
/// kind, this binary is stale) print `kind=?` instead of misreporting
/// as Builder (the i32 default).
fn kind_str(w: &DebugExecutorState) -> &'static str {
    match ExecutorKind::try_from(w.kind) {
        Ok(ExecutorKind::Builder) => "builder",
        Ok(ExecutorKind::Fetcher) => "fetcher",
        Err(_) => "kind=?",
    }
}

fn yn(b: bool) -> &'static str {
    if b { "Y" } else { "N" }
}

/// PG-backed worker list (`ListExecutors`). Default `rio-cli workers`
/// path — `--actor`/`--diff` use [`run_actor`]/[`run_diff`] instead.
pub(crate) async fn run_pg(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
    status: Option<String>,
) -> anyhow::Result<()> {
    let req = ListExecutorsRequest {
        status_filter: status.unwrap_or_default(),
    };
    let resp = rpc("ListExecutors", async || {
        client.list_executors(req.clone()).await
    })
    .await?;
    if as_json {
        json(&resp)?;
    } else if resp.executors.is_empty() {
        println!("(no executors)");
    } else {
        for w in &resp.executors {
            print_worker(w);
        }
    }
    Ok(())
}

/// `rio-cli drain-executor` — mark a worker draining. Same RPC the
/// controller fires on SIGTERM/eviction; this is the manual operator
/// lever for the same path.
pub(crate) async fn run_drain(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
    a: DrainArgs,
) -> anyhow::Result<()> {
    let DrainArgs { executor_id, force } = a;
    // Unary — rpc() helper applies. Server returns accepted=true
    // for known workers (idempotent on already-draining) and
    // accepted=false for unknown ids (per admin/tests.rs, not
    // an error — just a no-op). Empty id is InvalidArgument.
    let req = DrainExecutorRequest {
        executor_id: executor_id.clone(),
        force,
    };
    let resp = rpc("DrainExecutor", async || {
        client.drain_executor(req.clone()).await
    })
    .await?;
    if as_json {
        #[derive(Serialize)]
        struct DrainJson<'a> {
            executor_id: &'a str,
            accepted: bool,
            busy: bool,
        }
        json(&DrainJson {
            executor_id: &executor_id,
            accepted: resp.accepted,
            busy: resp.busy,
        })?;
    } else if resp.accepted {
        let action = if force { "reassigned" } else { "in flight" };
        if resp.busy {
            println!("draining {executor_id} (build {action})");
        } else {
            println!("draining {executor_id} (idle)");
        }
    } else {
        println!("{executor_id}: not found (nothing to drain)");
    }
    Ok(())
}

/// Detailed per-worker view for `rio-cli workers`. `Status` prints a
/// compact one-liner; this is the "show me everything" form for
/// debugging a specific worker's registration or feature advertisement.
fn print_worker(w: &ExecutorInfo) {
    println!("worker {} [{}]", w.executor_id, w.status);
    println!("  state:    {}", if w.busy { "busy" } else { "idle" });
    println!(
        "  hb:       {}   up: {}",
        crate::fmt_ts_ago(w.last_heartbeat.as_ref().map(|t| t.seconds)),
        crate::fmt_ts_ago(w.connected_since.as_ref().map(|t| t.seconds))
    );
    println!("  systems:  {}", w.systems.join(", "));
    if !w.supported_features.is_empty() {
        println!("  features: {}", w.supported_features.join(", "));
    }
    if let Some(r) = &w.resources {
        println!(
            "  cpu={:.2}  mem={}/{}  disk={}/{}",
            r.cpu_fraction,
            r.memory_used_bytes,
            r.memory_total_bytes,
            r.disk_used_bytes,
            r.disk_total_bytes
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(draining: bool, store_degraded: bool) -> DebugExecutorState {
        DebugExecutorState {
            executor_id: "ex-1".into(),
            has_stream: true,
            is_registered: true,
            warm: true,
            kind: ExecutorKind::Builder as i32,
            draining,
            store_degraded,
            ..Default::default()
        }
    }

    /// I-056b: `--diff` must surface DRAINING/DEGRADED markers (gate
    /// `has_capacity()`). Before the fix, DiffRow lacked the fields
    /// entirely — operator saw a clean row for a worker dispatch
    /// rejects.
    // r[verify cli.workers.actor-diff]
    #[test]
    fn diff_row_surfaces_draining_degraded() {
        let a = actor(true, false);
        let row = DiffRow::new("ex-1", None, Some(&a));
        assert_eq!(row.actor_draining, Some(true));
        assert_eq!(row.actor_store_degraded, Some(false));
        assert!(row.capacity_markers().contains("DRAINING"));
        assert!(!row.capacity_markers().contains("DEGRADED"));

        let a = actor(false, true);
        let row = DiffRow::new("ex-1", None, Some(&a));
        assert!(row.capacity_markers().contains("DEGRADED"));

        // draining/degraded are NOT divergence (operator-intent, not
        // PG-vs-actor drift).
        let pg = ExecutorInfo {
            executor_id: "ex-1".into(),
            status: "alive".into(),
            ..Default::default()
        };
        let a = actor(true, true);
        let row = DiffRow::new("ex-1", Some(&pg), Some(&a));
        assert!(!row.diverges(), "draining must not bump diverge_count");
        assert!(row.capacity_markers().contains("DRAINING"));
        assert!(row.capacity_markers().contains("DEGRADED"));

        // pg-only → no actor → no markers.
        let row = DiffRow::new("ex-1", Some(&pg), None);
        assert_eq!(row.actor_draining, None);
        assert!(row.capacity_markers().is_empty());
    }
}

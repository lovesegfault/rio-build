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
use rio_proto::types::{DebugExecutorState, ExecutorInfo, ExecutorKind, ListExecutorsRequest};
use serde::Serialize;
use tonic::transport::Channel;

use crate::{json, rpc};

/// `--actor` path: in-memory snapshot only.
pub(crate) async fn run_actor(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
) -> anyhow::Result<()> {
    let resp = rpc("DebugListExecutors", client.debug_list_executors(())).await?;
    if as_json {
        return json(&ActorWorkersJson::from(&resp.executors[..]));
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
    let pg = rpc(
        "ListExecutors",
        client.list_executors(ListExecutorsRequest::default()),
    )
    .await?;
    let actor = rpc("DebugListExecutors", client.debug_list_executors(())).await?;

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
    actor_running_count: Option<u32>,
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
            actor_running_count: actor.map(|a| a.running_count),
        }
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
                    "{mark} {}  pg=ABSENT  actor=[stream={} reg={} warm={} {} hb={}s run={}]  (PG last_seen stale)",
                    self.executor_id,
                    yn(self.actor_has_stream.unwrap()),
                    yn(self.actor_is_registered.unwrap()),
                    yn(self.actor_warm.unwrap()),
                    self.actor_kind.unwrap(),
                    self.actor_heartbeat_ago_secs.unwrap(),
                    self.actor_running_count.unwrap(),
                );
            }
            "both" => {
                let zombie = if self.actor_has_stream == Some(false) {
                    "  (ZOMBIE: entry exists, no stream)"
                } else {
                    ""
                };
                println!(
                    "{mark} {}  pg=[{}]  actor=[stream={} reg={} warm={} {} hb={}s run={}]{zombie}",
                    self.executor_id,
                    self.pg_status.unwrap_or("?"),
                    yn(self.actor_has_stream.unwrap()),
                    yn(self.actor_is_registered.unwrap()),
                    yn(self.actor_warm.unwrap()),
                    self.actor_kind.unwrap(),
                    self.actor_heartbeat_ago_secs.unwrap(),
                    self.actor_running_count.unwrap(),
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
    let zombie = if !w.has_stream {
        "  ⚠ ZOMBIE (no stream)"
    } else {
        ""
    };
    println!("worker {}  [{}]{zombie}", w.executor_id, kind_str(w));
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
    println!("  running:  {} build(s)", w.running_count);
    for h in &w.running_builds {
        println!("    {h}");
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

/// Top-level JSON wrapper. Same `executors` key as `WorkersJson` so
/// `jq '.executors | length'` works for both `workers --json` and
/// `workers --actor --json`. The shape inside differs (proto fields
/// vs PG fields), but the envelope matches.
#[derive(Serialize)]
struct ActorWorkersJson<'a> {
    executors: Vec<ActorExecutorJson<'a>>,
}
impl<'a> From<&'a [DebugExecutorState]> for ActorWorkersJson<'a> {
    fn from(ws: &'a [DebugExecutorState]) -> Self {
        Self {
            executors: ws.iter().map(ActorExecutorJson::from).collect(),
        }
    }
}

#[derive(Serialize)]
struct ActorExecutorJson<'a> {
    executor_id: &'a str,
    has_stream: bool,
    is_registered: bool,
    warm: bool,
    kind: &'static str,
    systems: &'a [String],
    last_heartbeat_ago_secs: u64,
    running_count: u32,
    running_builds: &'a [String],
}
impl<'a> From<&'a DebugExecutorState> for ActorExecutorJson<'a> {
    fn from(w: &'a DebugExecutorState) -> Self {
        Self {
            executor_id: &w.executor_id,
            has_stream: w.has_stream,
            is_registered: w.is_registered,
            warm: w.warm,
            kind: kind_str(w),
            systems: &w.systems,
            last_heartbeat_ago_secs: w.last_heartbeat_ago_secs,
            running_count: w.running_count,
            running_builds: &w.running_builds,
        }
    }
}

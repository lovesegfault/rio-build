//! Stage 5: build-log rendering.
//!
//! One spawned renderer task drains a [`RenderEvent`] channel fed by
//! every per-root `watch_stream` task and the coordinator main loop,
//! `select!`ing that against a redraw tick. All renderers write to
//! **stderr**; stdout is reserved for the final result-path lines
//! (the machine-readable surface).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use rio_proto::types::{
    BuildEvent, BuildLogBatch, BuildPhase, DerivationEvent, DerivationEventKind,
    SubstituteProgress, build_event::Event,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub mod ci;
pub mod plain;
pub mod term;
pub mod tty;

/// What the coordinator hands the renderer.
#[derive(Debug)]
pub enum RenderEvent {
    /// A `BuildEvent` from one of the per-root watch streams.
    Build(BuildEvent),
    /// One non-build line (eval-parent stderr, coordinator messages).
    /// Anything written to stderr behind the TTY display's back lands
    /// inside the ephemeral region and corrupts it.
    Note(String),
    /// Cumulative pre-build pipeline counters (eval → upload → submit).
    /// The coordinator sends a fresh snapshot whenever a counter
    /// changes; each renderer decides how to show it (live status line
    /// on the TTY, milestone lines for plain/CI).
    Prebuild(PrebuildSnapshot),
}

/// Cumulative counters for the pre-build window of one `rio build`
/// invocation: evaluation, digest negotiation/upload, and submission.
/// All counts only ever grow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrebuildSnapshot {
    /// Requested attrs whose evaluation has not reported yet.
    pub attrs_pending: usize,
    /// Submit roots produced by evaluation so far.
    pub roots_found: usize,
    /// Derivations discovered by evaluation so far.
    pub drvs_found: usize,
    /// Source roots discovered by evaluation so far.
    pub sources_found: usize,
    /// Drv digests the cluster acknowledged (uploaded or already held).
    pub drvs_acked: usize,
    /// Of [`drvs_acked`](Self::drvs_acked), how many were actually uploaded.
    pub drvs_uploaded: usize,
    /// Source roots the cluster acknowledged.
    pub sources_acked: usize,
    /// Of [`sources_acked`](Self::sources_acked), how many were actually uploaded.
    pub sources_uploaded: usize,
    /// Submitted builds whose event stream has started.
    pub builds_accepted: usize,
}

impl PrebuildSnapshot {
    /// Every requested attr has evaluated and at least one produced a
    /// build root.
    pub fn eval_done(&self) -> bool {
        self.attrs_pending == 0 && self.roots_found > 0
    }

    /// Every object discovered so far has been acknowledged by the
    /// cluster.
    pub fn uploads_done(&self) -> bool {
        self.drvs_acked >= self.drvs_found && self.sources_acked >= self.sources_found
    }

    /// Objects the cluster already held (no bytes transferred).
    pub fn already_present(&self) -> usize {
        (self.drvs_acked + self.sources_acked)
            .saturating_sub(self.drvs_uploaded + self.sources_uploaded)
    }

    /// Nothing left to narrate: builds are accepted and streaming.
    pub fn done(&self) -> bool {
        self.eval_done() && self.uploads_done() && self.builds_accepted >= self.roots_found
    }
}

/// Milestone gate shared by the line-oriented renderers (plain, CI):
/// yields the "evaluation finished" and "uploads finished" lines at
/// most once each, so both renderers print identical text.
#[derive(Default)]
pub(crate) struct PrebuildMilestones {
    eval_reported: bool,
    upload_reported: bool,
}

impl PrebuildMilestones {
    pub(crate) fn lines(&mut self, s: &PrebuildSnapshot) -> Vec<String> {
        let mut out = Vec::new();
        if !self.eval_reported && s.eval_done() {
            self.eval_reported = true;
            out.push(format!(
                "evaluated {} root(s): {} derivations, {} sources",
                s.roots_found, s.drvs_found, s.sources_found
            ));
        }
        if self.eval_reported && !self.upload_reported && s.uploads_done() {
            self.upload_reported = true;
            out.push(format!(
                "uploaded {} drv blobs, {} sources ({} already present)",
                s.drvs_uploaded,
                s.sources_uploaded,
                s.already_present()
            ));
        }
        out
    }
}

/// Cloneable sender into the renderer task. `send` never blocks; a
/// dropped receiver makes it a no-op (the [`null`](Self::null) handle).
#[derive(Clone)]
pub struct RenderHandle(Option<mpsc::UnboundedSender<RenderEvent>>);

impl RenderHandle {
    /// A handle whose sends are dropped (tests).
    pub fn null() -> Self {
        Self(None)
    }

    pub fn send(&self, ev: RenderEvent) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(ev);
        }
    }

    pub fn note(&self, msg: impl Into<String>) {
        self.send(RenderEvent::Note(msg.into()));
    }
}

/// Which renderer drives the terminal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum RenderMode {
    /// Pick from the environment ([`select`]).
    #[default]
    Auto,
    /// One status line per state edge (script-compatible).
    Plain,
    /// Buffered per-drv blocks with `::group::` folds and a heartbeat.
    Ci,
    /// Live ephemeral region + `f` log browser.
    Tty,
}

#[derive(Clone, Debug, Default)]
pub struct RenderOpts {
    pub mode: RenderMode,
    /// Don't wrap successful build logs in `::group::` folds.
    pub no_fold: bool,
    /// Seconds a build may produce no output before its tail is dumped
    /// and further output is streamed live (CI). 0 disables.
    pub stall_timeout: u64,
}

// r[impl bc.render.select]
/// Auto-selection: `Tty` when stderr+stdin are a tty and `TERM` isn't
/// `dumb`; else `Ci` when `GITHUB_ACTIONS` is set; else `Plain`.
pub fn select(
    mode: RenderMode,
    env: &HashMap<String, String>,
    stderr_tty: bool,
    stdin_tty: bool,
) -> RenderMode {
    if mode != RenderMode::Auto {
        return mode;
    }
    let term_ok = env.get("TERM").is_none_or(|t| !t.is_empty() && t != "dumb");
    if stderr_tty && stdin_tty && term_ok {
        RenderMode::Tty
    } else if term::fold_markers(env) {
        RenderMode::Ci
    } else {
        RenderMode::Plain
    }
}

/// Spawn the renderer task. Returns the send handle plus a `stop`
/// future: drop every clone of the handle, then await the future to
/// drain the channel and clear any live region. `pager_gate` is set
/// while a pager owns the terminal — the caller's signal handler
/// should swallow Ctrl-C while it's high.
pub fn spawn(
    opts: RenderOpts,
    pager_gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> (RenderHandle, JoinHandle<()>) {
    let env: HashMap<String, String> = std::env::vars().collect();
    let isatty = console::Term::stderr().is_term();
    // SAFETY: isatty(0) is a pure read.
    let stdin_tty = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
    let mode = select(opts.mode, &env, isatty, stdin_tty);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let mut out = std::io::stderr();
        match mode {
            RenderMode::Auto | RenderMode::Plain => {
                let mut r = plain::PlainRenderer::default();
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                tick.tick().await;
                loop {
                    tokio::select! {
                        ev = rx.recv() => match ev {
                            Some(ev) => r.on_event(&mut out, &ev),
                            None => { r.tick(&mut out); break; }
                        },
                        _ = tick.tick() => r.tick(&mut out),
                    }
                }
            }
            RenderMode::Ci => {
                let mut r = ci::CiRenderer::new(
                    out,
                    ci::CiOpts {
                        color: term::want_color(&env, isatty),
                        fold: !opts.no_fold && term::fold_markers(&env),
                        stall_timeout: Duration::from_secs(opts.stall_timeout),
                        ..Default::default()
                    },
                );
                let mut tick = tokio::time::interval(r.heartbeat_interval());
                tick.tick().await;
                loop {
                    tokio::select! {
                        ev = rx.recv() => match ev {
                            Some(ev) => r.on_event(&ev),
                            None => break,
                        },
                        _ = tick.tick() => r.tick(),
                    }
                }
            }
            RenderMode::Tty => tty::drive(rx, pager_gate).await,
        }
    });
    (RenderHandle(Some(tx)), task)
}

/// Monotonic-clock seam: tests use a `Cell<Instant>`.
pub type Clock = Box<dyn Fn() -> Instant + Send>;

pub(crate) fn wall_clock() -> Clock {
    Box::new(Instant::now)
}

/// `/nix/store/<hash>-foo.drv` → `foo.drv`.
pub(crate) fn short_drv(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.split_once('-') {
        Some((hash, rest)) if hash.len() == 32 => rest,
        _ => base,
    }
}

pub(crate) fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Per-derivation log sink shared by the CI and TTY renderers (≈ PR
/// `BuildOutput`). The renderer that owns it decides what `streaming`
/// means and where added lines go.
#[derive(Clone)]
pub struct DrvRow {
    /// Display label (`short_drv` of the drv path).
    pub label: String,
    pub drv_path: String,
    pub build_id: String,
    pub started_at: Instant,
    pub last_output_at: Instant,
    /// Ring buffer of sanitized lines.
    pub lines: VecDeque<String>,
    pub phase: Option<String>,
    /// Stall-escalated to live streaming (CI).
    pub streaming: bool,
    /// Highest `first_line_number + len` seen — dedup on resume.
    pub next_line_no: u64,
    /// Ring-buffer cap.
    pub max_lines: usize,
}

impl DrvRow {
    pub fn new(drv_path: &str, build_id: &str, now: Instant, max_lines: usize) -> Self {
        Self {
            label: short_drv(drv_path).to_string(),
            drv_path: drv_path.to_string(),
            build_id: build_id.to_string(),
            started_at: now,
            last_output_at: now,
            lines: VecDeque::new(),
            phase: None,
            streaming: false,
            next_line_no: 0,
            max_lines,
        }
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        now.duration_since(self.started_at)
    }

    /// Append one already-sanitized line.
    pub fn push_line(&mut self, line: String, now: Instant) {
        self.last_output_at = now;
        if self.lines.len() == self.max_lines {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }
}

/// What [`route`] tells the renderer to do for one `BuildEvent`.
pub(crate) enum DrvEdge<'a> {
    Open(&'a DerivationEvent),
    Log(&'a BuildLogBatch),
    Phase(&'a BuildPhase),
    Substitute(&'a SubstituteProgress),
    Close {
        kind: DerivationEventKind,
        drv: &'a DerivationEvent,
    },
    /// Build-level Failed/Cancelled: drop this build's still-open rows
    /// (no per-drv terminal arrives for them). Scoped to one build_id —
    /// with attrset expansion, several roots run concurrently.
    Drain {
        build_id: &'a str,
    },
    Ignore,
}

/// Common `BuildEvent` → row-edge classification (open/close/log/phase).
/// Renderer-specific behaviour (what an open row LOOKS like) lives in
/// the renderer; this is just "which row, what edge".
pub(crate) fn route(ev: &BuildEvent) -> DrvEdge<'_> {
    match ev.event.as_ref() {
        Some(Event::Derivation(d)) => {
            match DerivationEventKind::try_from(d.kind).unwrap_or(DerivationEventKind::Queued) {
                DerivationEventKind::Started
                | DerivationEventKind::Substituting
                | DerivationEventKind::Queued => DrvEdge::Open(d),
                k @ (DerivationEventKind::Completed
                | DerivationEventKind::Cached
                | DerivationEventKind::Failed) => DrvEdge::Close { kind: k, drv: d },
            }
        }
        Some(Event::Log(b)) => DrvEdge::Log(b),
        Some(Event::Phase(p)) => DrvEdge::Phase(p),
        Some(Event::SubstituteProgress(p)) => DrvEdge::Substitute(p),
        // Build-level Completed is *not* a drain: every drv already got
        // its per-drv Close, and with multiple roots one build's
        // Completed must not drop another's still-running rows.
        Some(Event::Failed(_) | Event::Cancelled(_)) => DrvEdge::Drain {
            build_id: &ev.build_id,
        },
        _ => DrvEdge::Ignore,
    }
}

/// Ingest a log batch into `row` (sanitized, dedup'd by line number),
/// returning the lines that were actually added (for live streaming).
pub(crate) fn ingest_log(row: &mut DrvRow, batch: &BuildLogBatch, now: Instant) -> Vec<String> {
    // Dedup on resume: a re-delivered batch (`first_line_number` not
    // past what we've seen) is dropped.
    if batch.first_line_number < row.next_line_no {
        return Vec::new();
    }
    row.next_line_no = batch.first_line_number + batch.lines.len() as u64;
    let mut added = Vec::with_capacity(batch.lines.len());
    for raw in &batch.lines {
        // Display path, not parse: build output is arbitrary bytes
        // (build_types.proto explicitly says non-UTF-8); U+FFFD is the
        // correct rendering for an undecodable byte.
        #[allow(clippy::disallowed_methods)]
        let line = term::sanitize_line(&String::from_utf8_lossy(raw));
        row.push_line(line.clone(), now);
        added.push(line);
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_duration_units() {
        assert_eq!(fmt_duration(Duration::from_secs(5)), "5s");
        assert_eq!(fmt_duration(Duration::from_secs(65)), "1m05s");
        assert_eq!(fmt_duration(Duration::from_secs(3700)), "1h01m40s");
    }

    // r[verify bc.render.select]
    #[test]
    fn select_mode() {
        let env = |k: &[(&str, &str)]| -> HashMap<String, String> {
            k.iter().map(|(a, b)| ((*a).into(), (*b).into())).collect()
        };
        assert_eq!(
            select(RenderMode::Auto, &env(&[]), false, false),
            RenderMode::Plain
        );
        assert_eq!(
            select(
                RenderMode::Auto,
                &env(&[("GITHUB_ACTIONS", "true")]),
                false,
                false
            ),
            RenderMode::Ci
        );
        assert_eq!(
            select(RenderMode::Auto, &env(&[]), true, true),
            RenderMode::Tty
        );
        assert_eq!(
            select(RenderMode::Auto, &env(&[("TERM", "dumb")]), true, true),
            RenderMode::Plain
        );
        // stdin not a tty (pipe, </dev/null) → no tty mode.
        assert_eq!(
            select(RenderMode::Auto, &env(&[]), true, false),
            RenderMode::Plain
        );
        // --render overrides everything.
        assert_eq!(
            select(
                RenderMode::Plain,
                &env(&[("GITHUB_ACTIONS", "true")]),
                true,
                true
            ),
            RenderMode::Plain
        );
    }
}

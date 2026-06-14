//! Non-interactive (CI) renderer.
//!
//! Each derivation's log is buffered and emitted as one contiguous
//! block when it finishes, so concurrent builds never interleave. On
//! Actions-style CI, successful builds are folded with `::group::`
//! markers; failed builds are never fully folded. A periodic heartbeat
//! names the running builds so the CI log shows liveness, and a build
//! that produces no output for `stall_timeout` is escalated to live
//! streaming.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use rio_proto::types::DerivationEventKind;

use super::term::sanitize_line;
use super::{Clock, DrvEdge, DrvRow, RenderEvent, fmt_duration, ingest_log, route, wall_clock};

/// Unfolded tail of a failure log; earlier output gets folded.
const FAILURE_TAIL_LINES: usize = 200;
const STALL_TAIL_LINES: usize = 5;

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

pub struct CiOpts {
    pub color: bool,
    pub fold: bool,
    pub heartbeat_interval: Duration,
    pub stall_timeout: Duration,
    pub buffer_lines: usize,
    pub clock: Clock,
}

impl Default for CiOpts {
    fn default() -> Self {
        Self {
            color: false,
            fold: false,
            heartbeat_interval: Duration::from_secs(30),
            stall_timeout: Duration::from_secs(300),
            buffer_lines: 10_000,
            clock: wall_clock(),
        }
    }
}

pub struct CiRenderer<W: Write> {
    out: W,
    opts: CiOpts,
    /// drv_path → row.
    running: HashMap<String, DrvRow>,
}

impl<W: Write> CiRenderer<W> {
    pub fn new(out: W, opts: CiOpts) -> Self {
        Self {
            out,
            opts,
            running: HashMap::new(),
        }
    }

    pub fn heartbeat_interval(&self) -> Duration {
        self.opts.heartbeat_interval
    }

    fn sgr(&self, code: &str, s: &str) -> String {
        sgr(self.opts.color, code, s)
    }

    fn print(&mut self, lines: &[String]) {
        for line in lines {
            let _ = writeln!(self.out, "{line}");
        }
        let _ = self.out.flush();
    }

    pub fn on_event(&mut self, ev: &RenderEvent) {
        let ev = match ev {
            RenderEvent::Build(ev) => ev,
            RenderEvent::Note(s) => {
                // Notes carry eval-error text and attr names from user
                // input — sanitize and prefix like log lines so they
                // can't smuggle ::group::/::error:: at line start.
                self.print(&[format!("· {}", sanitize_line(s))]);
                return;
            }
        };
        let now = (self.opts.clock)();
        match route(ev) {
            DrvEdge::Open(d) => {
                let kind = DerivationEventKind::try_from(d.kind).ok();
                if let Some(row) = self.running.get_mut(&d.derivation_path) {
                    // Substituting → Queued/Started: reset the row, do
                    // not no-op (the fetch failed/downgraded; the build
                    // really starts now).
                    row.started_at = now;
                    row.last_output_at = now;
                    row.phase = None;
                    return;
                }
                if kind == Some(DerivationEventKind::Queued) {
                    // Queued without an open row is just dispatch noise.
                    return;
                }
                let mut row = DrvRow::new(
                    &d.derivation_path,
                    &ev.build_id,
                    now,
                    self.opts.buffer_lines,
                );
                if kind == Some(DerivationEventKind::Substituting) {
                    row.phase = Some("fetching".into());
                }
                let label = row.label.clone();
                self.running.insert(d.derivation_path.clone(), row);
                self.print(&[format!("▶ {label} started")]);
            }
            DrvEdge::Log(batch) => {
                let color = self.opts.color;
                if let Some(row) = self.running.get_mut(&batch.derivation_path) {
                    let added = ingest_log(row, batch, now);
                    if row.streaming {
                        let prefix = sgr(color, DIM, &format!("{}>", row.label));
                        let lines: Vec<String> =
                            added.iter().map(|l| format!("{prefix} {l}")).collect();
                        self.print(&lines);
                    }
                }
            }
            DrvEdge::Phase(p) => {
                if let Some(row) = self.running.get_mut(&p.derivation_path) {
                    let phase = sanitize_line(&p.phase);
                    row.push_line(format!("@ phase {phase}"), now);
                    row.phase = Some(phase);
                }
            }
            DrvEdge::Substitute(p) => {
                if let Some(row) = self.running.get_mut(&p.derivation_path) {
                    row.last_output_at = now;
                    row.phase = Some(format!(
                        "fetching {:.1}/{:.1} MiB",
                        p.bytes_done as f64 / (1 << 20) as f64,
                        p.bytes_expected as f64 / (1 << 20) as f64
                    ));
                }
            }
            DrvEdge::Close { kind, drv } => {
                if let Some(row) = self.running.remove(&drv.derivation_path) {
                    self.finish(row, kind, &sanitize_line(&drv.error_message));
                }
            }
            DrvEdge::Drain { build_id } => {
                // This build was cancelled/failed: drop its still-open
                // rows so they don't haunt the heartbeat. Other roots
                // keep theirs.
                self.running.retain(|_, row| row.build_id != build_id);
            }
            DrvEdge::Ignore => {}
        }
    }

    fn finish(&mut self, row: DrvRow, kind: DerivationEventKind, error: &str) {
        let duration = fmt_duration(row.elapsed((self.opts.clock)()));
        if row.streaming {
            // Log already on screen; just print the verdict.
            self.print(&[self.verdict(&row, kind, &duration, error)]);
            return;
        }
        match kind {
            DerivationEventKind::Failed => self.emit_failure(&row, &duration, error),
            _ => self.emit_success(&row, kind, &duration),
        }
    }

    fn verdict(&self, row: &DrvRow, kind: DerivationEventKind, dur: &str, error: &str) -> String {
        match kind {
            DerivationEventKind::Failed => {
                let suffix = if error.is_empty() {
                    String::new()
                } else {
                    format!(": {error}")
                };
                self.sgr(RED, &format!("✘  {} failed after {dur}{suffix}", row.label))
            }
            DerivationEventKind::Cached => self.sgr(GREEN, &format!("✔  {} (cached)", row.label)),
            _ => self.sgr(GREEN, &format!("✔  {} ({dur})", row.label)),
        }
    }

    fn emit_success(&mut self, row: &DrvRow, kind: DerivationEventKind, dur: &str) {
        let verdict = self.verdict(row, kind, dur, "");
        let mut lines = if self.opts.fold {
            vec![format!("::group::{verdict}")]
        } else {
            vec![verdict]
        };
        lines.extend(self.prefixed(row));
        if self.opts.fold {
            lines.push("::endgroup::".into());
        }
        self.print(&lines);
    }

    fn emit_failure(&mut self, row: &DrvRow, dur: &str, error: &str) {
        // The error is almost always at the end: keep the tail visible,
        // fold the rest so multi-failure CI pages stay scrollable.
        let dropped = if row.lines.len() == row.max_lines {
            format!(" (oldest lines dropped, buffer={})", row.max_lines)
        } else {
            String::new()
        };
        let body = self.prefixed(row);
        let mut lines = vec![format!(
            "{}{dropped}",
            self.verdict(row, DerivationEventKind::Failed, dur, error)
        )];
        if self.opts.fold && body.len() > FAILURE_TAIL_LINES {
            let head = body.len() - FAILURE_TAIL_LINES;
            lines.push(format!("::group::earlier output ({head} lines)"));
            lines.extend_from_slice(&body[..head]);
            lines.push("::endgroup::".into());
            lines.extend_from_slice(&body[head..]);
        } else {
            lines.extend(body);
        }
        lines.push(self.sgr(
            RED,
            &format!(
                "✘ end of log for {} · rio build --attach {}",
                row.label, row.build_id
            ),
        ));
        self.print(&lines);
    }

    fn prefixed(&self, row: &DrvRow) -> Vec<String> {
        let prefix = self.sgr(DIM, &format!("{}>", row.label));
        row.lines.iter().map(|l| format!("{prefix} {l}")).collect()
    }

    pub fn tick(&mut self) {
        self.check_stalls();
        if self.running.is_empty() {
            return;
        }
        let now = (self.opts.clock)();
        let mut rows: Vec<&DrvRow> = self.running.values().collect();
        rows.sort_by_key(|r| r.started_at);
        let parts: Vec<String> = rows
            .iter()
            .map(|r| {
                let mut detail = fmt_duration(r.elapsed(now));
                if let Some(p) = &r.phase {
                    detail.push_str(", ");
                    detail.push_str(p);
                }
                let flag = if r.streaming { " ⚠stalled" } else { "" };
                format!("{} ({detail}){flag}", r.label)
            })
            .collect();
        let line = self.sgr(
            DIM,
            &format!("⏵ {} building: {}", rows.len(), parts.join(", ")),
        );
        self.print(&[line]);
    }

    fn check_stalls(&mut self) {
        if self.opts.stall_timeout.is_zero() {
            return;
        }
        let now = (self.opts.clock)();
        let color = self.opts.color;
        let mut out = Vec::new();
        for row in self.running.values_mut() {
            if row.streaming || now.duration_since(row.last_output_at) < self.opts.stall_timeout {
                continue;
            }
            let silent = fmt_duration(now.duration_since(row.last_output_at));
            let prefix = sgr(color, DIM, &format!("{}>", row.label));
            out.push(sgr(
                color,
                BOLD,
                &format!("⚠ {}: no output for {silent}, last lines:", row.label),
            ));
            let tail: Vec<_> = row.lines.iter().rev().take(STALL_TAIL_LINES).collect();
            for line in tail.into_iter().rev() {
                out.push(format!("{prefix} {line}"));
            }
            out.push(sgr(
                color,
                BOLD,
                &format!("⚠ {}: streaming further output live", row.label),
            ));
            // From now on lines appear immediately; the final block is
            // skipped for this build.
            row.streaming = true;
        }
        if !out.is_empty() {
            self.print(&out);
        }
    }
}

fn sgr(color: bool, code: &str, s: &str) -> String {
    if color {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use rio_proto::types::{
        BuildEvent, BuildLogBatch, BuildPhase, DerivationEvent, build_event::Event,
    };

    use super::*;

    const DRV_A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-pkg-a.drv";
    const DRV_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-pkg-b.drv";

    type Buf = Arc<Mutex<Vec<u8>>>;
    struct BufWrite(Buf);
    impl Write for BufWrite {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    type FakeClock = Arc<Mutex<Instant>>;

    fn make(opts: CiOpts) -> (CiRenderer<BufWrite>, Buf, FakeClock) {
        let buf: Buf = Arc::new(Mutex::new(Vec::new()));
        let now: FakeClock = Arc::new(Mutex::new(Instant::now()));
        let now2 = now.clone();
        let r = CiRenderer::new(
            BufWrite(buf.clone()),
            CiOpts {
                clock: Box::new(move || *now2.lock().unwrap()),
                ..opts
            },
        );
        (r, buf, now)
    }

    fn ev(event: Event) -> RenderEvent {
        RenderEvent::Build(BuildEvent {
            build_id: "build-abc123".into(),
            sequence: 1,
            timestamp: None,
            event: Some(event),
        })
    }

    fn drv(path: &str, kind: DerivationEventKind) -> RenderEvent {
        ev(Event::Derivation(DerivationEvent {
            derivation_path: path.into(),
            kind: kind as i32,
            ..Default::default()
        }))
    }

    fn log(path: &str, first: u64, lines: &[&str]) -> RenderEvent {
        ev(Event::Log(BuildLogBatch {
            derivation_path: path.into(),
            lines: lines.iter().map(|s| s.as_bytes().to_vec()).collect(),
            first_line_number: first,
            executor_id: String::new(),
        }))
    }

    fn text(buf: &Buf) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    fn advance(now: &FakeClock, secs: u64) {
        let mut n = now.lock().unwrap();
        *n += Duration::from_secs(secs);
    }

    #[test]
    fn success_block_contiguous() {
        let (mut r, buf, now) = make(CiOpts::default());
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        r.on_event(&drv(DRV_B, DerivationEventKind::Started));
        r.on_event(&log(DRV_A, 0, &["a1"]));
        r.on_event(&log(DRV_B, 0, &["b1"]));
        r.on_event(&log(DRV_A, 1, &["a2"]));
        advance(&now, 65);
        r.on_event(&drv(DRV_A, DerivationEventKind::Completed));
        r.on_event(&drv(DRV_B, DerivationEventKind::Completed));
        let t = text(&buf);
        // Logs of concurrent builds don't interleave.
        assert!(t.contains("pkg-a.drv> a1\npkg-a.drv> a2\n"), "{t}");
        assert!(t.contains("✔  pkg-a.drv (1m05s)"), "{t}");
        assert!(r.running.is_empty());
    }

    #[test]
    fn success_folded_failure_not() {
        let (mut r, buf, _now) = make(CiOpts {
            fold: true,
            ..Default::default()
        });
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        r.on_event(&log(DRV_A, 0, &["fine"]));
        r.on_event(&drv(DRV_A, DerivationEventKind::Completed));
        r.on_event(&drv(DRV_B, DerivationEventKind::Started));
        r.on_event(&log(DRV_B, 0, &["boom"]));
        r.on_event(&ev(Event::Derivation(DerivationEvent {
            derivation_path: DRV_B.into(),
            kind: DerivationEventKind::Failed as i32,
            error_message: "rc=1".into(),
            ..Default::default()
        })));
        let t = text(&buf);
        assert!(
            t.contains("::group::✔  pkg-a.drv (0s)\npkg-a.drv> fine\n::endgroup::\n"),
            "{t}"
        );
        // Failure block has no fold markers around it.
        let fail = t.split("✘  pkg-b.drv failed").nth(1).unwrap();
        assert!(!fail.contains("::group::"), "{fail}");
        assert!(
            t.contains("✘ end of log for pkg-b.drv · rio build --attach build-abc123"),
            "{t}"
        );
    }

    #[test]
    fn failure_buffer_cap_noted() {
        let (mut r, buf, _now) = make(CiOpts {
            buffer_lines: 3,
            ..Default::default()
        });
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        r.on_event(&log(DRV_A, 0, &["l1", "l2", "l3", "l4", "l5"]));
        r.on_event(&drv(DRV_A, DerivationEventKind::Failed));
        let t = text(&buf);
        assert!(!t.contains("pkg-a.drv> l1"), "{t}");
        assert!(t.contains("pkg-a.drv> l5"), "{t}");
        assert!(t.contains("oldest lines dropped"), "{t}");
    }

    #[test]
    fn phase_recorded_and_logged() {
        let (mut r, buf, _now) = make(CiOpts::default());
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        r.on_event(&ev(Event::Phase(BuildPhase {
            derivation_path: DRV_A.into(),
            phase: "buildPhase".into(),
        })));
        assert_eq!(r.running[DRV_A].phase.as_deref(), Some("buildPhase"));
        r.on_event(&drv(DRV_A, DerivationEventKind::Failed));
        assert!(text(&buf).contains("pkg-a.drv> @ phase buildPhase"));
    }

    #[test]
    fn sanitize_at_ingestion_neutralizes_injection() {
        let (mut r, buf, _now) = make(CiOpts {
            fold: true,
            ..Default::default()
        });
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        r.on_event(&log(DRV_A, 0, &["::endgroup::", "10%\r100%", "a\x1b[2Jb"]));
        // Phase, error_message and Note carry build/eval-derived strings
        // too — same sanitize+prefix discipline.
        r.on_event(&ev(Event::Phase(BuildPhase {
            derivation_path: DRV_A.into(),
            phase: "x\r::error::evil".into(),
        })));
        r.on_event(&ev(Event::Derivation(DerivationEvent {
            derivation_path: DRV_A.into(),
            kind: DerivationEventKind::Failed as i32,
            error_message: "boom\r::add-mask::secret".into(),
            ..Default::default()
        })));
        r.on_event(&RenderEvent::Note("eval: \r::group::leaked".into()));
        let t = text(&buf);
        // Short failure log → renderer emits no fold markers of its own;
        // any line starting :: would be an injection.
        for line in t.lines() {
            assert!(!line.starts_with("::"), "injected line start: {line:?}");
        }
        // Prefix neutralizes CI command injection from log lines.
        assert!(t.contains("pkg-a.drv> ::endgroup::"), "{t}");
        assert!(t.contains("pkg-a.drv> 100%"), "{t}");
        assert!(t.contains("pkg-a.drv> ab"), "{t}");
        // CR-overwrite ate "eval: " — fine, the prefix is what matters.
        assert!(t.contains("· ::group::leaked"), "{t}");
    }

    #[test]
    fn heartbeat_lists_running_longest_first_and_silent_when_idle() {
        let (mut r, buf, now) = make(CiOpts::default());
        r.tick();
        assert_eq!(text(&buf), "");
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        advance(&now, 100);
        r.on_event(&drv(DRV_B, DerivationEventKind::Started));
        r.on_event(&ev(Event::Phase(BuildPhase {
            derivation_path: DRV_A.into(),
            phase: "buildPhase".into(),
        })));
        advance(&now, 20);
        r.tick();
        let t = text(&buf);
        let line = t.lines().last().unwrap();
        assert!(line.contains("2 building"), "{line}");
        let a = line.find("pkg-a.drv (2m00s, buildPhase)").unwrap();
        let b = line.find("pkg-b.drv (20s)").unwrap();
        assert!(a < b, "{line}");
    }

    #[test]
    fn stall_escalates_to_streaming_once() {
        let (mut r, buf, now) = make(CiOpts {
            stall_timeout: Duration::from_secs(300),
            ..Default::default()
        });
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        r.on_event(&log(DRV_A, 0, &["t1", "t2", "t3", "t4", "t5", "t6"]));
        advance(&now, 301);
        r.tick();
        let t = text(&buf);
        assert!(t.contains("⚠ pkg-a.drv: no output for 5m01s"), "{t}");
        assert!(t.contains("pkg-a.drv> t2"), "{t}");
        assert!(!t.contains("pkg-a.drv> t1"), "{t}");
        assert!(t.contains("streaming further output live"), "{t}");
        assert!(r.running[DRV_A].streaming);
        // Subsequent lines appear immediately.
        r.on_event(&log(DRV_A, 6, &["after-stall"]));
        assert!(text(&buf).contains("pkg-a.drv> after-stall"));
        // Stall reported only once.
        r.tick();
        assert_eq!(text(&buf).matches("no output for").count(), 1);
        // Final block not re-emitted; only verdict.
        let before = text(&buf);
        r.on_event(&drv(DRV_A, DerivationEventKind::Failed));
        let delta = &text(&buf)[before.len()..];
        assert!(!delta.contains("pkg-a.drv> t6"), "{delta}");
        assert!(delta.contains("✘  pkg-a.drv failed"), "{delta}");
    }

    #[test]
    fn color_applied() {
        let (mut r, buf, _now) = make(CiOpts {
            color: true,
            ..Default::default()
        });
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        r.on_event(&drv(DRV_A, DerivationEventKind::Completed));
        assert!(text(&buf).contains("\x1b[32m✔  pkg-a.drv"));
    }

    #[test]
    fn long_failure_folds_head() {
        let (mut r, buf, _now) = make(CiOpts {
            fold: true,
            ..Default::default()
        });
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        let lines: Vec<String> = (0..FAILURE_TAIL_LINES + 50)
            .map(|i| format!("line {i}"))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        r.on_event(&log(DRV_A, 0, &refs));
        r.on_event(&drv(DRV_A, DerivationEventKind::Failed));
        let t = text(&buf);
        assert!(t.contains("::group::earlier output (50 lines)"), "{t}");
        let (head, tail) = t.split_once("::endgroup::").unwrap();
        assert!(head.contains("pkg-a.drv> line 0"));
        assert!(tail.contains(&format!("pkg-a.drv> line {}", FAILURE_TAIL_LINES + 49)));
    }

    #[test]
    fn substituting_row_reset_on_started_and_drained_on_terminal() {
        let (mut r, buf, now) = make(CiOpts::default());
        r.on_event(&drv(DRV_A, DerivationEventKind::Substituting));
        assert_eq!(r.running[DRV_A].phase.as_deref(), Some("fetching"));
        advance(&now, 30);
        // Fetch failed → scheduler re-queues then starts the build.
        r.on_event(&drv(DRV_A, DerivationEventKind::Started));
        assert_eq!(r.running[DRV_A].phase, None);
        assert_eq!(
            r.running[DRV_A].elapsed(*now.lock().unwrap()),
            Duration::ZERO
        );
        r.on_event(&log(DRV_A, 0, &["building"]));
        r.on_event(&drv(DRV_A, DerivationEventKind::Completed));
        assert!(text(&buf).contains("pkg-a.drv> building"));
        // Build-level Cancelled drains this build's open rows but leaves
        // a concurrently running root's rows alone.
        r.on_event(&drv(DRV_B, DerivationEventKind::Substituting));
        r.on_event(&RenderEvent::Build(BuildEvent {
            build_id: "build-other".into(),
            sequence: 1,
            timestamp: None,
            event: Some(Event::Derivation(DerivationEvent {
                derivation_path: "/nix/store/cccccccccccccccccccccccccccccccc-pkg-c.drv".into(),
                kind: DerivationEventKind::Started as i32,
                ..Default::default()
            })),
        }));
        r.on_event(&ev(Event::Cancelled(rio_proto::types::BuildCancelled {
            reason: "user".into(),
        })));
        assert!(!r.running.contains_key(DRV_B));
        assert_eq!(r.running.len(), 1, "other root's row survives");
    }
}

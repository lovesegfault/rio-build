//! The status-line renderer: one line per state edge (cached drvs are
//! counted in summaries, not listed), no log/phase events.
//! Script-compatible — the output format is frozen (the VM test
//! asserts on it). Written to stderr; stdout is the result-path
//! surface only.

use std::collections::HashSet;
use std::io::Write;

use rio_proto::types::{BuildEvent, BuildProgress, DerivationEventKind, build_event::Event};

use super::{PrebuildMilestones, PrebuildSnapshot, RenderEvent, short_drv};

/// Progress lines after this many events without one. The cluster
/// sends a `Progress` after every derivation state edge; rendering
/// every one made a 4121-drv warm run produce ~7500 lines (2.5/drv).
/// The interval tick in [`super::spawn`] flushes at 1s regardless.
const PROGRESS_EVERY_N_EVENTS: u32 = 50;

/// Coalescing state across events. The renderer is otherwise
/// stateless; this holds only what's needed to (a) emit at most one
/// `Progress` line per `PROGRESS_EVERY_N_EVENTS` events / 1s tick,
/// and (b) split the cluster's `running` count into
/// substituting-vs-building from the `DerivationEvent` edges seen.
#[derive(Default)]
pub(super) struct PlainRenderer {
    /// Latest Progress received but not yet rendered.
    pending: Option<(String, BuildProgress)>,
    /// Events since the last Progress line.
    since_progress: u32,
    /// drv paths currently in `Substituting` (entered on the edge,
    /// left on Cached/Completed/Failed). The cluster's
    /// `BuildProgress.running` covers BOTH substituting and building;
    /// subtracting this set's size yields actually-building.
    substituting: HashSet<String>,
    /// Pre-build milestones already announced (one line each, ever —
    /// snapshots arrive per counter change but this is a status
    /// surface, not a progress bar).
    milestones: PrebuildMilestones,
}

impl PlainRenderer {
    pub(super) fn on_event(&mut self, out: &mut impl Write, ev: &RenderEvent) {
        match ev {
            RenderEvent::Build(ev) => self.on_build(out, ev),
            RenderEvent::Note(s) => {
                let _ = writeln!(out, "{}", super::term::sanitize_line(s));
            }
            RenderEvent::Prebuild(s) => self.on_prebuild(out, s),
        }
    }

    /// Low-frequency pre-build milestones: one line when evaluation
    /// finishes, one when every discovered object is acked. The
    /// build-accepted line comes from the coordinator as a Note.
    fn on_prebuild(&mut self, out: &mut impl Write, s: &PrebuildSnapshot) {
        for line in self.milestones.lines(s) {
            let _ = writeln!(out, "{line}");
        }
    }

    /// Flush the held-back Progress line if any (1s tick).
    pub(super) fn tick(&mut self, out: &mut impl Write) {
        self.flush_progress(out);
    }

    fn on_build(&mut self, out: &mut impl Write, ev: &BuildEvent) {
        if let Some(Event::Derivation(d)) = ev.event.as_ref() {
            match DerivationEventKind::try_from(d.kind) {
                Ok(DerivationEventKind::Substituting) => {
                    self.substituting.insert(d.derivation_path.clone());
                }
                // Any non-Substituting edge closes a substituting row.
                // Substituting → Queued/Started is the RC-A reset (the
                // fetch downgraded; the build really starts now) — same
                // handling as the ci/tty renderers. Cached/Completed/
                // Failed are terminal.
                Ok(_) => {
                    self.substituting.remove(&d.derivation_path);
                }
                Err(_) => {}
            }
        }
        if let Some(Event::Progress(p)) = ev.event.as_ref() {
            self.pending = Some((ev.build_id.clone(), p.clone()));
            self.since_progress += 1;
            if self.since_progress >= PROGRESS_EVERY_N_EVENTS {
                self.flush_progress(out);
            }
            return;
        }
        // Non-Progress events flush any pending Progress first when
        // the line is terminal — keeps the final status line ordered
        // before "completed:" / "FAILED".
        if matches!(
            ev.event.as_ref(),
            Some(Event::Completed(_) | Event::Failed(_) | Event::Cancelled(_))
        ) {
            self.flush_progress(out);
        }
        self.since_progress += 1;
        if let Some(s) = line(ev) {
            let _ = writeln!(out, "{s}");
        }
    }

    fn flush_progress(&mut self, out: &mut impl Write) {
        if let Some((id, p)) = self.pending.take() {
            let id_short = id.get(..8).unwrap_or(&id);
            let sub = self.substituting.len() as u32;
            let building = p.running.saturating_sub(sub);
            let _ = writeln!(
                out,
                "[{id_short}] {}/{} done, {} substituting, {} building, {} queued",
                p.completed, p.total, sub, building, p.queued
            );
        }
        self.since_progress = 0;
    }
}

// r[impl bc.render.plain-default]
/// Render one event to a status line. `None` = not a status edge
/// (log/phase/progress are deliberately not rendered — this is a
/// status surface, not a log pipe).
pub fn line(ev: &BuildEvent) -> Option<String> {
    let id_short = ev.build_id.get(..8).unwrap_or(&ev.build_id);
    match ev.event.as_ref()? {
        Event::Started(s) => Some(format!(
            "[{id_short}] started: {} derivations ({} cached)",
            s.total_derivations, s.cached_derivations
        )),
        Event::InputsResolved(_) => Some(format!("[{id_short}] inputs resolved")),
        Event::Derivation(d) => {
            let state = match DerivationEventKind::try_from(d.kind).ok()? {
                DerivationEventKind::Queued => "queued",
                DerivationEventKind::Started => "building",
                DerivationEventKind::Completed => "built",
                // No per-drv line: a warm cluster caches thousands of
                // drvs; the started summary and the coalesced Progress
                // line already carry the cached/done counts.
                DerivationEventKind::Cached => return None,
                DerivationEventKind::Failed => "FAILED",
                DerivationEventKind::Substituting => "substituting",
            };
            let mut s = format!(
                "[{id_short}] {:<12} {}",
                state,
                short_drv(&d.derivation_path)
            );
            if !d.error_message.is_empty() {
                s.push_str(": ");
                s.push_str(&d.error_message);
            }
            Some(s)
        }
        // Progress is coalesced by `PlainRenderer` (held back until
        // the every-N-events / 1s tick flush) and rendered with the
        // substituting/building split — never one line per edge here.
        Event::Progress(_) => None,
        Event::Completed(c) => Some(format!(
            "[{id_short}] completed: {}",
            c.output_paths.join(" ")
        )),
        Event::Failed(f) => Some(format!(
            "[{id_short}] FAILED ({}): {}",
            f.failed_derivation, f.error_message
        )),
        Event::Cancelled(c) => Some(format!("[{id_short}] cancelled: {}", c.reason)),
        Event::Log(_) | Event::Phase(_) | Event::SubstituteProgress(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::{BuildCompleted, DerivationEvent};

    fn ev(event: Event) -> BuildEvent {
        BuildEvent {
            build_id: "0123456789abcdef".into(),
            sequence: 1,
            timestamp: None,
            event: Some(event),
        }
    }

    #[test]
    fn derivation_edges_render_and_logs_do_not() {
        let started = ev(Event::Derivation(DerivationEvent {
            derivation_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-1.0.drv".into(),
            kind: DerivationEventKind::Started as i32,
            ..Default::default()
        }));
        let line = line(&started).unwrap();
        assert!(line.contains("building"), "{line}");
        assert!(line.contains("hello-1.0.drv"), "{line}");
        assert!(line.starts_with("[01234567]"), "{line}");

        let log = ev(Event::Log(rio_proto::types::BuildLogBatch::default()));
        assert!(super::line(&log).is_none());
    }

    #[test]
    fn completed_lists_outputs() {
        let done = ev(Event::Completed(BuildCompleted {
            output_paths: vec!["/nix/store/x-out".into()],
        }));
        assert!(line(&done).unwrap().contains("/nix/store/x-out"));
    }

    #[test]
    fn on_event_writes_to_sink() {
        let mut buf = Vec::new();
        PlainRenderer::default().on_event(
            &mut buf,
            &RenderEvent::Build(ev(Event::Derivation(DerivationEvent {
                derivation_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
                kind: DerivationEventKind::Queued as i32,
                ..Default::default()
            }))),
        );
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("queued"), "{s}");
        assert!(s.ends_with('\n'));
    }

    fn drv(path: &str, kind: DerivationEventKind) -> RenderEvent {
        RenderEvent::Build(ev(Event::Derivation(DerivationEvent {
            derivation_path: path.into(),
            kind: kind as i32,
            ..Default::default()
        })))
    }

    fn progress(completed: u32, running: u32, queued: u32, total: u32) -> RenderEvent {
        RenderEvent::Build(ev(Event::Progress(BuildProgress {
            completed,
            running,
            queued,
            total,
            ..Default::default()
        })))
    }

    #[test]
    fn progress_coalesced_and_split_substituting() {
        let mut r = PlainRenderer::default();
        let mut buf = Vec::new();
        // Substituting edges + Progress events: nothing emitted until
        // the tick (or the 50-event threshold) flushes.
        r.on_event(
            &mut buf,
            &drv("/nix/store/a", DerivationEventKind::Substituting),
        );
        r.on_event(
            &mut buf,
            &drv("/nix/store/b", DerivationEventKind::Substituting),
        );
        r.on_event(&mut buf, &progress(0, 5, 10, 100));
        r.on_event(&mut buf, &progress(1, 5, 9, 100));
        let s = std::str::from_utf8(&buf).unwrap();
        // Only the two derivation-edge lines so far; Progress held back.
        assert_eq!(s.lines().count(), 2, "{s}");
        assert!(!s.contains("done"), "{s}");
        // Tick flushes the LATEST Progress with the substituting split.
        r.tick(&mut buf);
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(
            s.contains("1/100 done, 2 substituting, 3 building, 9 queued"),
            "{s}"
        );
        // Cached closes a substituting drv.
        r.on_event(&mut buf, &drv("/nix/store/a", DerivationEventKind::Cached));
        r.on_event(&mut buf, &progress(2, 4, 9, 100));
        r.tick(&mut buf);
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(
            s.contains("2/100 done, 1 substituting, 3 building, 9 queued"),
            "{s}"
        );
        // RC-A: Substituting → Started resets (the fetch downgraded;
        // the drv counts as building now, not substituting).
        r.on_event(&mut buf, &drv("/nix/store/b", DerivationEventKind::Started));
        r.on_event(&mut buf, &progress(2, 4, 9, 100));
        r.tick(&mut buf);
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(
            s.contains("2/100 done, 0 substituting, 4 building, 9 queued"),
            "{s}"
        );
    }

    #[test]
    fn cached_edge_emits_no_line_but_closes_substituting() {
        let mut r = PlainRenderer::default();
        let mut buf = Vec::new();
        r.on_event(
            &mut buf,
            &drv("/nix/store/a", DerivationEventKind::Substituting),
        );
        r.on_event(&mut buf, &drv("/nix/store/a", DerivationEventKind::Cached));
        let s = std::str::from_utf8(&buf).unwrap();
        // The substituting edge prints; the cached edge does not.
        assert_eq!(s.lines().count(), 1, "{s}");
        assert!(!s.contains("cached"), "{s}");
        // The drv still left the substituting set, so the next Progress
        // counts it as done rather than substituting.
        r.on_event(&mut buf, &progress(1, 0, 0, 1));
        r.tick(&mut buf);
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(
            s.contains("1/1 done, 0 substituting, 0 building, 0 queued"),
            "{s}"
        );
    }

    #[test]
    fn prebuild_milestones_print_once() {
        let mut r = PlainRenderer::default();
        let mut buf = Vec::new();
        // Mid-eval snapshots are silent — this is a status surface, not
        // a progress bar.
        r.on_event(
            &mut buf,
            &RenderEvent::Prebuild(PrebuildSnapshot {
                attrs_pending: 1,
                drvs_found: 10,
                sources_found: 2,
                ..Default::default()
            }),
        );
        assert!(buf.is_empty(), "{}", std::str::from_utf8(&buf).unwrap());
        // Eval finished: one summary line with the discovered counts.
        r.on_event(
            &mut buf,
            &RenderEvent::Prebuild(PrebuildSnapshot {
                attrs_pending: 0,
                roots_found: 1,
                drvs_found: 42,
                sources_found: 3,
                drvs_acked: 10,
                drvs_uploaded: 4,
                ..Default::default()
            }),
        );
        let s = std::str::from_utf8(&buf).unwrap();
        assert_eq!(s.lines().count(), 1, "{s}");
        assert!(
            s.contains("evaluated 1 root(s): 42 derivations, 3 sources"),
            "{s}"
        );
        // Everything acked: one upload summary with the cached split.
        let done = PrebuildSnapshot {
            attrs_pending: 0,
            roots_found: 1,
            drvs_found: 42,
            sources_found: 3,
            drvs_acked: 42,
            drvs_uploaded: 12,
            sources_acked: 3,
            sources_uploaded: 1,
            ..Default::default()
        };
        r.on_event(&mut buf, &RenderEvent::Prebuild(done));
        let s = std::str::from_utf8(&buf).unwrap();
        assert_eq!(s.lines().count(), 2, "{s}");
        assert!(
            s.contains("uploaded 12 drv blobs, 1 sources (32 already present)"),
            "{s}"
        );
        // Later snapshots (builds accepted) add nothing more.
        r.on_event(
            &mut buf,
            &RenderEvent::Prebuild(PrebuildSnapshot {
                builds_accepted: 1,
                ..done
            }),
        );
        assert_eq!(std::str::from_utf8(&buf).unwrap().lines().count(), 2);
    }

    #[test]
    fn progress_flushed_on_threshold_and_before_terminal() {
        let mut r = PlainRenderer::default();
        let mut buf = Vec::new();
        for i in 0..PROGRESS_EVERY_N_EVENTS {
            r.on_event(&mut buf, &progress(i, 1, 0, 100));
        }
        // The Nth Progress event flushes (no tick needed).
        let s = std::str::from_utf8(&buf).unwrap();
        assert_eq!(s.lines().count(), 1, "{s}");
        // Terminal event flushes any pending Progress first.
        let mut r = PlainRenderer::default();
        let mut buf = Vec::new();
        r.on_event(&mut buf, &progress(99, 0, 0, 100));
        r.on_event(
            &mut buf,
            &RenderEvent::Build(ev(Event::Completed(BuildCompleted {
                output_paths: vec!["/nix/store/x-out".into()],
            }))),
        );
        let s = std::str::from_utf8(&buf).unwrap();
        let lines: Vec<_> = s.lines().collect();
        assert_eq!(lines.len(), 2, "{s}");
        assert!(lines[0].contains("99/100 done"), "{s}");
        assert!(lines[1].contains("completed:"), "{s}");
    }
}

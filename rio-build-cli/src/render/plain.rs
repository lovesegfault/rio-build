//! The status-line renderer: one line per state edge, no log/phase
//! events. Script-compatible — the output format is frozen (the VM
//! test asserts on it). Written to stderr; stdout is the result-path
//! surface only.

use std::io::Write;

use rio_proto::types::{BuildEvent, DerivationEventKind, build_event::Event};

use super::{RenderEvent, short_drv};

pub(super) fn on_event(out: &mut impl Write, ev: &RenderEvent) {
    match ev {
        RenderEvent::Build(ev) => {
            if let Some(s) = line(ev) {
                let _ = writeln!(out, "{s}");
            }
        }
        RenderEvent::Note(s) => {
            let _ = writeln!(out, "{}", super::term::sanitize_line(s));
        }
    }
}

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
                DerivationEventKind::Cached => "cached",
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
        Event::Progress(p) => Some(format!(
            "[{id_short}] {}/{} done, {} building, {} queued",
            p.completed, p.total, p.running, p.queued
        )),
        Event::Completed(c) => Some(format!(
            "[{id_short}] completed: {}",
            c.output_paths.join(" ")
        )),
        Event::Failed(f) => Some(format!(
            "[{id_short}] FAILED ({}): {}",
            f.failed_derivation, f.error_message
        )),
        Event::Cancelled(c) => Some(format!("[{id_short}] cancelled: {}", c.reason)),
        Event::Snapshot(_)
        | Event::ResyncRequired(_)
        | Event::Phase(_)
        | Event::SubstituteProgress(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::{BuildCompleted, DerivationEvent};

    fn ev(event: Event) -> BuildEvent {
        BuildEvent {
            build_id: "0123456789abcdef".into(),
            timestamp: None,
            event: Some(event),
        }
    }

    #[test]
    fn derivation_edges_render() {
        let started = ev(Event::Derivation(DerivationEvent {
            derivation_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-1.0.drv".into(),
            kind: DerivationEventKind::Started as i32,
            ..Default::default()
        }));
        let line = line(&started).unwrap();
        assert!(line.contains("building"), "{line}");
        assert!(line.contains("hello-1.0.drv"), "{line}");
        assert!(line.starts_with("[01234567]"), "{line}");
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
        on_event(
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
}

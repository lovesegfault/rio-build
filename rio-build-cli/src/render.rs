//! Stage 5: per-drv status lines from `BuildEvent` streams.
//!
//! Pure formatting — the coordinator decides where lines go (stdout in
//! the CLI, nowhere in tests). One line per state edge; log batches
//! and byte-progress events are deliberately not rendered (this is a
//! status surface, not a log pipe — `--attach` against a chatty build
//! must not flood the terminal).

use rio_proto::types::{BuildEvent, DerivationEventKind, build_event::Event};

/// Short display name for a drv path (`/nix/store/<hash>-foo.drv` →
/// `foo.drv`).
fn short_drv(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.split_once('-') {
        Some((hash, rest)) if hash.len() == 32 => rest,
        _ => base,
    }
}

/// Render one event to a status line. `None` = not a status edge.
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
}

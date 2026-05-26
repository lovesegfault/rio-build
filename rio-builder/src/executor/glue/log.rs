//! The `@nix` log side-channel filter.
//!
//! nixpkgs' stdenv writes `@nix {"action":"setPhase","phase":"buildPhase"}`
//! to `$NIX_LOG_FD` (which the request glue points at fd 2 = the build's
//! pty) at every phase transition. Under the daemon executor those lines
//! were consumed by nix-daemon and surfaced as `STDERR_RESULT{SetPhase}`
//! frames, which `stderr_loop.rs` forwarded as [`BuildPhase`] messages
//! (`r[builder.stderr.forward-set-phase]`). With the native executor the
//! raw lines arrive directly in the captured log stream, so this filter
//! reproduces the daemon's classification: `@nix `-prefixed lines are
//! consumed (never forwarded to the log batcher, the persisted log, or
//! the client), `setPhase` actions become phase events, everything else
//! `@nix` is dropped, and ordinary lines pass through untouched.
//!
//! The activation milestone wires this between `rio_exec::ExecEvent::Log`
//! and the existing `LogBatcher`, preserving the daemon-era semantics
//! documented on [`LineAction::Phase`]: flush the batcher before sending
//! the phase frame (line-number ordering stays monotone), play no part
//! in silence accounting (the max-silent deadline lives in rio-exec and
//! is reset by the builder's raw output — including these frames —
//! exactly as the daemon-era path reset it on any stderr output), and
//! count every line against the message cap so a build cannot flood the
//! scheduler with phase frames (`r[builder.stderr.msg-cap]` successor).
//!
//! [`BuildPhase`]: rio_proto::types::BuildPhase

/// Maximum lines (log + `@nix` frames) accepted from one build before it
/// is aborted with `LogLimitExceeded`.
///
/// Same value as the daemon-era `MAX_BUILD_STDERR_MESSAGES` choke point
/// in `stderr_loop.rs`: the cap exists so a build emitting frames in a
/// tight loop cannot occupy the executor/scheduler for the full build
/// timeout. Counting *every* line (not just phase frames) at a single
/// choke point mirrors the old dispatch() behavior.
pub(crate) const MAX_BUILD_LOG_LINES: u64 = 10_000_000;

/// Classification of one captured log line.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LineAction {
    /// An ordinary log line: forward to the `LogBatcher` unchanged.
    Forward(Vec<u8>),
    /// A `@nix {"action":"setPhase",...}` frame: emit the existing
    /// `BuildPhase` proto message carrying this phase name.
    ///
    /// Daemon-era semantics the caller must preserve (they lived in
    /// `stderr_loop.rs::forward_phase` and are normative under
    /// `builder.stderr.forward-set-phase`):
    /// - flush the batcher first if it has pending lines, so the phase
    ///   frame cannot overtake buffered log lines on the channel;
    /// - send the frame directly on `log_tx`, NOT through the batcher;
    /// - leave silence accounting alone — the max-silent deadline lives
    ///   in rio-exec and was already reset by the frame's raw pty bytes,
    ///   like any other builder output.
    Phase(String),
    /// A `@nix ` frame that is not a well-formed `setPhase` (other
    /// actions like `msg`/`start`/`stop`/`result`, or malformed JSON):
    /// consumed silently, exactly like rio's daemon-era dispatch dropped
    /// the equivalent STDERR_RESULT types it didn't forward.
    Consumed,
    /// The per-build line cap was exceeded: the caller must abort the
    /// build with `LogLimitExceeded`.
    CapExceeded,
}

/// Stateful filter for one build's log stream.
///
/// One instance per build; `handle` is called for every captured line in
/// arrival order.
pub(crate) struct NixLogFilter {
    lines_seen: u64,
    cap: u64,
}

impl NixLogFilter {
    pub(crate) fn new() -> Self {
        Self {
            lines_seen: 0,
            cap: MAX_BUILD_LOG_LINES,
        }
    }

    /// Construct with a custom cap (tests).
    #[cfg(test)]
    pub(crate) fn with_cap(cap: u64) -> Self {
        Self { lines_seen: 0, cap }
    }

    /// Classify one captured line (without its trailing newline).
    pub(crate) fn handle(&mut self, line: &[u8]) -> LineAction {
        self.lines_seen += 1;
        // `>` (not `>=`): the cap is the maximum number of ACCEPTED
        // lines, so the cap-th line still passes and the cap+1-th trips.
        if self.lines_seen > self.cap {
            return LineAction::CapExceeded;
        }

        // Only a *prefix* match consumes the line: `@nix ` mid-line is
        // ordinary build output (e.g. a build echoing its own log).
        let Some(rest) = line.strip_prefix(b"@nix ") else {
            return LineAction::Forward(line.to_vec());
        };

        // The frame is consumed regardless of whether we understand it —
        // it is a machine side-channel, not user-visible output. Only a
        // well-formed setPhase with a string phase becomes a Phase event.
        match serde_json::from_slice::<serde_json::Value>(rest) {
            Ok(v) if v.get("action").and_then(|a| a.as_str()) == Some("setPhase") => {
                match v.get("phase").and_then(|p| p.as_str()) {
                    Some(phase) => LineAction::Phase(phase.to_owned()),
                    None => LineAction::Consumed,
                }
            }
            _ => LineAction::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_lines_pass_through() {
        let mut f = NixLogFilter::new();
        assert_eq!(
            f.handle(b"building /nix/store/x"),
            LineAction::Forward(b"building /nix/store/x".to_vec())
        );
        // Empty lines are ordinary output too.
        assert_eq!(f.handle(b""), LineAction::Forward(Vec::new()));
    }

    #[test]
    fn set_phase_is_extracted() {
        let mut f = NixLogFilter::new();
        assert_eq!(
            f.handle(br#"@nix {"action":"setPhase","phase":"buildPhase"}"#),
            LineAction::Phase("buildPhase".to_owned())
        );
        assert_eq!(
            f.handle(br#"@nix {"phase":"installPhase","action":"setPhase"}"#),
            LineAction::Phase("installPhase".to_owned())
        );
    }

    #[test]
    fn other_nix_actions_are_consumed() {
        let mut f = NixLogFilter::new();
        for line in [
            br#"@nix {"action":"start","id":1,"type":105}"#.as_slice(),
            br#"@nix {"action":"stop","id":1}"#.as_slice(),
            br#"@nix {"action":"msg","level":1,"msg":"hi"}"#.as_slice(),
            br#"@nix {"action":"setExpected","id":1}"#.as_slice(),
            br#"@nix {"action":"result","id":1}"#.as_slice(),
        ] {
            assert_eq!(f.handle(line), LineAction::Consumed, "line: {line:?}");
        }
    }

    #[test]
    fn malformed_nix_frames_are_consumed_not_forwarded() {
        let mut f = NixLogFilter::new();
        assert_eq!(f.handle(b"@nix {not json"), LineAction::Consumed);
        assert_eq!(f.handle(b"@nix "), LineAction::Consumed);
        // setPhase without a string phase is malformed → consumed.
        assert_eq!(
            f.handle(br#"@nix {"action":"setPhase","phase":42}"#),
            LineAction::Consumed
        );
        assert_eq!(
            f.handle(br#"@nix {"action":"setPhase"}"#),
            LineAction::Consumed
        );
    }

    #[test]
    fn mid_line_at_nix_is_not_consumed() {
        let mut f = NixLogFilter::new();
        let line = br#"echo '@nix {"action":"setPhase","phase":"x"}'"#;
        assert_eq!(f.handle(line), LineAction::Forward(line.to_vec()));
        // Prefix without the trailing space is also NOT a frame.
        assert_eq!(
            f.handle(b"@nixos rebuild"),
            LineAction::Forward(b"@nixos rebuild".to_vec())
        );
    }

    #[test]
    fn cap_counts_every_line_including_phases() {
        let mut f = NixLogFilter::with_cap(4);
        assert!(matches!(f.handle(b"one"), LineAction::Forward(_)));
        assert!(matches!(
            f.handle(br#"@nix {"action":"setPhase","phase":"p"}"#),
            LineAction::Phase(_)
        ));
        assert!(matches!(f.handle(b"three"), LineAction::Forward(_)));
        // The cap is the number of ACCEPTED lines: the fourth (cap-th)
        // line still passes…
        assert!(matches!(f.handle(b"four"), LineAction::Forward(_)));
        // …and the cap+1-th trips, regardless of its content, and stays
        // exceeded.
        assert_eq!(f.handle(b"five"), LineAction::CapExceeded);
        assert_eq!(f.handle(b"six"), LineAction::CapExceeded);
    }
}

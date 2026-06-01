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
//! Lines longer than rio-exec's pending-buffer cap arrive as multiple
//! events; the executor marks every fragment except the `\n`-bearing
//! one `terminated: false`. Classification is decided by the logical
//! line's HEAD and inherited by its continuations, per stream: the tail
//! of an oversized `@nix ` frame is consumed with its head (it is
//! side-channel payload, not user output — forwarding it would leak
//! the frame body into the tenant-visible log), and the tail of an
//! oversized ordinary line is forwarded with its head (consuming it
//! would silently truncate user output). A `setPhase` head emits its
//! phase exactly once; continuations of any `@nix` head are consumed.
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

/// The classification a logical line's head bequeaths to its
/// continuation fragments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingClass {
    /// The head was ordinary output: forward continuations verbatim.
    Forward,
    /// The head was an `@nix ` frame (any action, including a
    /// `setPhase` that already emitted its Phase): consume
    /// continuations.
    Consume,
}

/// Stateful filter for one build's log stream.
///
/// One instance per build; `handle` is called for every captured line in
/// arrival order.
pub(crate) struct NixLogFilter {
    lines_seen: u64,
    cap: u64,
    /// Per-stream pending classification: present while the previous
    /// fragment on that stream was un-terminated, i.e. the next event
    /// on the stream is a continuation of the same logical line.
    /// Streams are independent — a fragmented stdout line must not
    /// re-classify interleaved stderr lines (SeparatePipes capture).
    pending: Vec<(rio_exec::LogStream, PendingClass)>,
}

impl NixLogFilter {
    pub(crate) fn new() -> Self {
        Self {
            lines_seen: 0,
            cap: MAX_BUILD_LOG_LINES,
            pending: Vec::new(),
        }
    }

    /// Construct with a custom cap (tests).
    #[cfg(test)]
    pub(crate) fn with_cap(cap: u64) -> Self {
        Self {
            lines_seen: 0,
            cap,
            pending: Vec::new(),
        }
    }

    /// Classify one captured line event (without its trailing newline).
    ///
    /// `terminated` is rio-exec's framing flag: `false` means this
    /// event is a fragment (cap force-emit or EOF flush) and the
    /// logical line continues — or ends unterminated — on the same
    /// stream. The cap counts every fragment (`builder.stderr.msg-cap`:
    /// counted before filtering), so an oversized frame cannot dodge
    /// accounting by being split.
    pub(crate) fn handle(
        &mut self,
        stream: rio_exec::LogStream,
        line: &[u8],
        terminated: bool,
    ) -> LineAction {
        self.lines_seen += 1;
        // `>` (not `>=`): the cap is the maximum number of ACCEPTED
        // lines, so the cap-th line still passes and the cap+1-th trips.
        if self.lines_seen > self.cap {
            return LineAction::CapExceeded;
        }

        // Continuation of a fragmented logical line: inherit the head's
        // classification; never re-classify tail bytes (an oversized
        // `@nix` frame's body could otherwise leak as ordinary output,
        // and an ordinary line's tail could be eaten by a spurious
        // `@nix ` match at a fragment boundary).
        if let Some(idx) = self.pending.iter().position(|(s, _)| *s == stream) {
            let (_, class) = self.pending[idx];
            if terminated {
                self.pending.swap_remove(idx);
            }
            return match class {
                PendingClass::Forward => LineAction::Forward(line.to_vec()),
                PendingClass::Consume => LineAction::Consumed,
            };
        }

        // Head of a logical line: classify by content. Only a *prefix*
        // match consumes the line: `@nix ` mid-line is ordinary build
        // output (e.g. a build echoing its own log).
        let (action, class) = match line.strip_prefix(b"@nix ") {
            None => (LineAction::Forward(line.to_vec()), PendingClass::Forward),
            Some(rest) => {
                // The frame is consumed regardless of whether we
                // understand it — it is a machine side-channel, not
                // user-visible output. Only a well-formed setPhase with
                // a string phase becomes a Phase event, emitted ONCE at
                // the head; continuations are consumed like any other
                // frame bytes.
                let action = match serde_json::from_slice::<serde_json::Value>(rest) {
                    Ok(v) if v.get("action").and_then(|a| a.as_str()) == Some("setPhase") => {
                        match v.get("phase").and_then(|p| p.as_str()) {
                            Some(phase) => LineAction::Phase(phase.to_owned()),
                            None => LineAction::Consumed,
                        }
                    }
                    _ => LineAction::Consumed,
                };
                (action, PendingClass::Consume)
            }
        };
        if !terminated {
            self.pending.push((stream, class));
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_exec::LogStream;

    /// Most tests exercise whole (terminated) merged-stream lines.
    fn one(f: &mut NixLogFilter, line: &[u8]) -> LineAction {
        f.handle(LogStream::Merged, line, true)
    }

    #[test]
    fn ordinary_lines_pass_through() {
        let mut f = NixLogFilter::new();
        assert_eq!(
            one(&mut f, b"building /nix/store/x"),
            LineAction::Forward(b"building /nix/store/x".to_vec())
        );
        // Empty lines are ordinary output too.
        assert_eq!(one(&mut f, b""), LineAction::Forward(Vec::new()));
    }

    #[test]
    fn set_phase_is_extracted() {
        let mut f = NixLogFilter::new();
        assert_eq!(
            one(
                &mut f,
                br#"@nix {"action":"setPhase","phase":"buildPhase"}"#
            ),
            LineAction::Phase("buildPhase".to_owned())
        );
        assert_eq!(
            one(
                &mut f,
                br#"@nix {"phase":"installPhase","action":"setPhase"}"#
            ),
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
            assert_eq!(one(&mut f, line), LineAction::Consumed, "line: {line:?}");
        }
    }

    #[test]
    fn malformed_nix_frames_are_consumed_not_forwarded() {
        let mut f = NixLogFilter::new();
        assert_eq!(one(&mut f, b"@nix {not json"), LineAction::Consumed);
        assert_eq!(one(&mut f, b"@nix "), LineAction::Consumed);
        // setPhase without a string phase is malformed → consumed.
        assert_eq!(
            one(&mut f, br#"@nix {"action":"setPhase","phase":42}"#),
            LineAction::Consumed
        );
        assert_eq!(
            one(&mut f, br#"@nix {"action":"setPhase"}"#),
            LineAction::Consumed
        );
    }

    #[test]
    fn mid_line_at_nix_is_not_consumed() {
        let mut f = NixLogFilter::new();
        let line = br#"echo '@nix {"action":"setPhase","phase":"x"}'"#;
        assert_eq!(one(&mut f, line), LineAction::Forward(line.to_vec()));
        // Prefix without the trailing space is also NOT a frame.
        assert_eq!(
            one(&mut f, b"@nixos rebuild"),
            LineAction::Forward(b"@nixos rebuild".to_vec())
        );
    }

    #[test]
    fn cap_counts_every_line_including_phases() {
        let mut f = NixLogFilter::with_cap(4);
        assert!(matches!(one(&mut f, b"one"), LineAction::Forward(_)));
        assert!(matches!(
            one(&mut f, br#"@nix {"action":"setPhase","phase":"p"}"#),
            LineAction::Phase(_)
        ));
        assert!(matches!(one(&mut f, b"three"), LineAction::Forward(_)));
        // The cap is the number of ACCEPTED lines: the fourth (cap-th)
        // line still passes…
        assert!(matches!(one(&mut f, b"four"), LineAction::Forward(_)));
        // …and the cap+1-th trips, regardless of its content, and stays
        // exceeded.
        assert_eq!(one(&mut f, b"five"), LineAction::CapExceeded);
        assert_eq!(one(&mut f, b"six"), LineAction::CapExceeded);
    }

    /// An oversized `@nix` frame: the head fragment classifies (and is
    /// consumed), and every continuation — including the terminated
    /// final piece — is consumed with it. None of the frame body may
    /// reach the forwarded log.
    // r[verify builder.stderr.forward-set-phase+2]
    #[test]
    fn oversized_atnix_frame_tails_are_consumed() {
        let mut f = NixLogFilter::new();
        let head = br#"@nix {"action":"msg","level":0,"msg":"AAAA"#;
        assert_eq!(
            f.handle(LogStream::Merged, head, false),
            LineAction::Consumed
        );
        assert_eq!(
            f.handle(LogStream::Merged, b"AAAAAAAA", false),
            LineAction::Consumed,
            "mid fragment inherits the head's consume"
        );
        assert_eq!(
            f.handle(LogStream::Merged, br#"AAAA"}"#, true),
            LineAction::Consumed,
            "final fragment inherits too"
        );
        // The logical line ended: the next line classifies fresh.
        assert_eq!(
            f.handle(LogStream::Merged, b"ordinary", true),
            LineAction::Forward(b"ordinary".to_vec())
        );
    }

    /// The symmetric case: an oversized ORDINARY line's tail must keep
    /// flowing to the log — even when a fragment boundary makes a tail
    /// piece start with `@nix ` (classifying tails by content would
    /// consume user output).
    #[test]
    fn oversized_ordinary_line_tails_are_forwarded() {
        let mut f = NixLogFilter::new();
        assert_eq!(
            f.handle(LogStream::Merged, b"cat huge-file: AAAA", false),
            LineAction::Forward(b"cat huge-file: AAAA".to_vec())
        );
        let tricky_tail = br#"@nix {"action":"setPhase","phase":"x"}"#;
        assert_eq!(
            f.handle(LogStream::Merged, tricky_tail, true),
            LineAction::Forward(tricky_tail.to_vec()),
            "a tail that happens to start with @nix is still user output"
        );
    }

    /// A fragmented setPhase frame emits its phase exactly once, at the
    /// head; the continuation produces no second Phase event.
    #[test]
    fn split_phase_frame_emits_phase_once() {
        let mut f = NixLogFilter::new();
        // Head parses as a complete setPhase (the splitter cap split the
        // logical line right after the JSON).
        assert_eq!(
            f.handle(
                LogStream::Merged,
                br#"@nix {"action":"setPhase","phase":"buildPhase"}"#,
                false,
            ),
            LineAction::Phase("buildPhase".to_owned())
        );
        assert_eq!(
            f.handle(LogStream::Merged, b"   ", true),
            LineAction::Consumed,
            "continuation of the frame is consumed, not a second phase"
        );
    }

    /// Streams are independent: a fragmented stdout line must not
    /// re-classify interleaved stderr lines and vice versa.
    #[test]
    fn continuation_state_is_per_stream() {
        let mut f = NixLogFilter::new();
        // stdout: oversized @nix frame, head consumed, still open.
        assert_eq!(
            f.handle(LogStream::Stdout, br#"@nix {"action":"msg""#, false),
            LineAction::Consumed
        );
        // stderr interleaves an ordinary (whole) line: forwarded.
        assert_eq!(
            f.handle(LogStream::Stderr, b"err output", true),
            LineAction::Forward(b"err output".to_vec())
        );
        // stderr opens its own oversized ORDINARY line…
        assert_eq!(
            f.handle(LogStream::Stderr, b"big err: AAAA", false),
            LineAction::Forward(b"big err: AAAA".to_vec())
        );
        // …stdout's tail still consumes…
        assert_eq!(
            f.handle(LogStream::Stdout, b"tail}", true),
            LineAction::Consumed
        );
        // …and stderr's tail still forwards.
        assert_eq!(
            f.handle(LogStream::Stderr, b"BBBB", true),
            LineAction::Forward(b"BBBB".to_vec())
        );
    }

    /// An EOF flush can deliver an unterminated HEAD (the build died
    /// mid-line): it classifies by content like any head — the flag
    /// only matters for what would have come after.
    #[test]
    fn eof_partial_head_classified_by_content() {
        let mut f = NixLogFilter::new();
        assert_eq!(
            f.handle(LogStream::Merged, b"partial user line", false),
            LineAction::Forward(b"partial user line".to_vec())
        );
        let mut f = NixLogFilter::new();
        assert_eq!(
            f.handle(LogStream::Merged, br#"@nix {"action":"m"#, false),
            LineAction::Consumed
        );
    }
}

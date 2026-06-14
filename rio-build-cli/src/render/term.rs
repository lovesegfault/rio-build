//! Terminal/CI output helpers shared by the build-log renderers.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

/// ANSI escape sequences: CSI, OSC (incl. unterminated), other ESC forms.
fn ansi_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)?|[@-Z\\-_])").unwrap()
    })
}

/// ESC that does not start an SGR sequence (used after [`ansi_re`]
/// filtering). Matches exactly what the substitution keeps: a CSI
/// sequence (incl. intermediate bytes) with final byte `m`. `regex`
/// has no lookahead, so [`sanitize_line`] does the negative test.
fn sgr_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\x1b\[[0-?]*[ -/]*m").unwrap())
}

fn trailing_partial_sgr_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\x1b[^m]*$").unwrap())
}

/// Mega-lines (minified JS etc.) choke terminals and CI web UIs.
pub const MAX_LINE_LEN: usize = 4096;
pub const SGR_RESET: &str = "\x1b[0m";

/// Make one captured log line safe to re-emit.
///
/// - emulate carriage-return overwrite (progress bars: keep final state)
/// - keep SGR colour sequences, strip everything else ANSI (cursor moves,
///   clear-screen, OSC title changes — a build log must not be able to
///   take over the terminal)
/// - if any SGR was kept, append a reset so unbalanced colour can't bleed
///   into subsequent lines
/// - expand tabs, drop other control chars
/// - cap length
///
/// CI command injection: GitHub/Forgejo only interpret `::commands::` at
/// the start of a line. Renderers prefix every build line with `"label> "`,
/// and this function guarantees no CR/LF survives, so build output can
/// never fabricate a line start to smuggle `::endgroup::`/`::add-mask::`.
pub fn sanitize_line(s: &str) -> String {
    // CR overwrite: keep the last non-empty segment.
    let s = if s.contains('\r') {
        s.rsplit('\r').find(|p| !p.is_empty()).unwrap_or("")
    } else {
        s
    };
    let mut kept_sgr = false;
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    // Walk the string, passing kept-SGR ANSI through, stripping the rest.
    while i < bytes.len() {
        if bytes[i] == 0x1b
            && let Some(m) = ansi_re().find_at(s, i)
            && m.start() == i
        {
            let seq = m.as_str();
            if seq.starts_with("\x1b[") && seq.ends_with('m') {
                kept_sgr = true;
                out.push_str(seq);
            }
            i = m.end();
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        match ch {
            '\t' => {
                // expandtabs(4): pad to the next multiple of 4 display cells.
                let col = console::measure_text_width(&out);
                for _ in 0..(4 - col % 4) {
                    out.push(' ');
                }
            }
            '\x1b' => out.push(ch),
            c if c < ' ' => {}
            c => out.push(c),
        }
        i += ch.len_utf8();
    }
    // After the substitution, every legitimate ESC starts a kept SGR
    // (`\x1b[...m`). A stray or partial ESC would pair with our own
    // output at display time, so drop it.
    let mut out = strip_stray_esc(&out);
    if out.len() > MAX_LINE_LEN {
        // Don't cut an SGR in half: an unterminated CSI makes the
        // terminal swallow the text that follows.
        let mut head = &out[..safe_truncation_point(&out, MAX_LINE_LEN)];
        if let Some(m) = trailing_partial_sgr_re().find(head) {
            head = &head[..m.start()];
        }
        out = format!("{head} …[line truncated]");
    }
    if kept_sgr {
        out.push_str(SGR_RESET);
    }
    out
}

/// Drop any ESC that does not start a kept SGR sequence.
fn strip_stray_esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(off) = s[i..].find('\x1b') {
        out.push_str(&s[i..i + off]);
        let at = i + off;
        if let Some(m) = sgr_re().find_at(s, at)
            && m.start() == at
        {
            out.push_str(m.as_str());
            i = m.end();
        } else {
            i = at + 1;
        }
    }
    out.push_str(&s[i..]);
    out
}

/// Largest byte index ≤ `n` that is a char boundary.
fn safe_truncation_point(s: &str, n: usize) -> usize {
    let mut n = n.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    n
}

/// Clip to display cells, passing ANSI sequences through unbroken.
pub fn clip_ansi(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut cells = 0usize;
    let mut i = 0;
    while i < s.len() {
        if let Some(m) = ansi_re().find_at(s, i)
            && m.start() == i
        {
            out.push_str(m.as_str());
            i = m.end();
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        let mut buf = [0u8; 4];
        let w = console::measure_text_width(ch.encode_utf8(&mut buf));
        if cells + w > width {
            break;
        }
        out.push(ch);
        cells += w;
        i += ch.len_utf8();
    }
    out
}

/// fzf-style subsequence match, case-insensitive.
pub fn subseq_match(query: &str, candidate: &str) -> bool {
    let mut it = candidate.chars().flat_map(|c| c.to_lowercase());
    query
        .chars()
        .flat_map(|c| c.to_lowercase())
        .all(|q| it.any(|c| c == q))
}

/// Truncate the middle of `s` to fit `n` chars (not display cells).
pub fn trunc_middle(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    let half = (n - 1) / 2;
    let mut out: String = chars[..half].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - (n - 1 - half)..]);
    out
}

/// Colour decision: `NO_COLOR` > `CLICOLOR_FORCE`/`FORCE_COLOR` > isatty.
///
/// Actions CI counts as colour-capable: its log viewer renders ANSI,
/// but the job's stderr is a pipe, so isatty alone would disable it.
pub fn want_color(env: &HashMap<String, String>, isatty: bool) -> bool {
    if env.get("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if env.get("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty())
        || env.get("FORCE_COLOR").is_some_and(|v| !v.is_empty())
    {
        return true;
    }
    isatty || fold_markers(env)
}

/// Whether to emit `::group::` fold markers for collapsible log
/// sections.
///
/// GitHub Actions, Forgejo Actions and Gitea Actions all set
/// `GITHUB_ACTIONS=true` and are indistinguishable from inside a job.
/// GitHub and Forgejo (v7+) render the markers as folds; Gitea prints
/// them as text, which is harmless.
pub fn fold_markers(env: &HashMap<String, String>) -> bool {
    env.get("GITHUB_ACTIONS").is_some_and(|v| v == "true")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_passthrough() {
        assert_eq!(sanitize_line("plain text"), "plain text");
        assert_eq!(sanitize_line("ünïcode 漢字"), "ünïcode 漢字");
    }

    #[test]
    fn sanitize_cr_overwrite() {
        // Progress bars rewrite the line; keep the final state.
        assert_eq!(sanitize_line("10%\r50%\r100%"), "100%");
        // Trailing CR (e.g. CRLF already split): keep last non-empty segment.
        assert_eq!(sanitize_line("done\r"), "done");
    }

    #[test]
    fn sanitize_keeps_sgr_appends_reset() {
        let out = sanitize_line("\x1b[31merror\x1b[0m rest");
        assert!(out.contains("\x1b[31m"));
        assert!(out.ends_with("\x1b[0m"));
    }

    #[test]
    fn sanitize_strips_dangerous_escapes() {
        // Cursor movement, clear screen, OSC title: stripped.
        assert_eq!(sanitize_line("a\x1b[2Jb"), "ab");
        assert_eq!(sanitize_line("a\x1b[3Ab"), "ab");
        assert_eq!(sanitize_line("a\x1b]0;evil title\x07b"), "ab");
        // Unterminated OSC must not swallow output of later lines.
        assert_eq!(sanitize_line("a\x1b]0;unterminated"), "a");
    }

    #[test]
    fn sanitize_stray_esc_dropped() {
        // Lone or partial ESC not matched as a full sequence must not
        // leak: the terminal would pair it with following output.
        assert_eq!(sanitize_line("tail\x1b"), "tail");
        assert!(!sanitize_line("a\x1b").contains('\x1b'));
        // Kept SGR still intact, including intermediate bytes before m.
        assert!(sanitize_line("x\x1b[31my\x1b").contains("\x1b[31m"));
        assert!(sanitize_line("x\x1b[0 my").contains("\x1b[0 m"));
    }

    #[test]
    fn sanitize_truncation_no_partial_sgr() {
        // Truncation must not cut an SGR sequence in half.
        let partial = Regex::new(r"\x1b[^m]*$").unwrap();
        for offset in 1..5 {
            let line = format!(
                "{}{}",
                "x".repeat(MAX_LINE_LEN - offset),
                "\x1b[31mred\x1b[0m"
            );
            let out = sanitize_line(&line);
            let head = out.split(" …[line truncated]").next().unwrap();
            assert!(
                !partial.is_match(head),
                "offset {offset}: {:?}",
                &head[head.len().saturating_sub(8)..]
            );
        }
    }

    #[test]
    fn sanitize_controls_and_tabs() {
        assert_eq!(sanitize_line("a\tb"), "a   b");
        assert_eq!(sanitize_line("a\x07\x08b"), "ab");
    }

    #[test]
    fn sanitize_caps_length() {
        let out = sanitize_line(&"x".repeat(MAX_LINE_LEN + 100));
        assert!(out.len() < MAX_LINE_LEN + 50);
        assert!(out.ends_with("[line truncated]"));
    }

    #[test]
    fn clip_ansi_respects_cjk_and_passes_ansi() {
        // CJK count as 2 cells.
        assert_eq!(clip_ansi("漢字abc", 5), "漢字a");
        // ANSI passes through unbroken and uncounted.
        assert_eq!(clip_ansi("\x1b[31mab\x1b[0mcd", 3), "\x1b[31mab\x1b[0mc");
    }

    #[test]
    fn subseq_match_case_insensitive() {
        assert!(subseq_match("ddnx", "checks.DeadNix"));
        assert!(subseq_match("", "anything"));
        assert!(!subseq_match("xz", "checks.deadnix"));
    }

    #[test]
    fn trunc_middle_works() {
        assert_eq!(trunc_middle("short", 10), "short");
        assert_eq!(trunc_middle("0123456789", 5), "01…89");
    }

    fn env(kvs: &[(&str, &str)]) -> HashMap<String, String> {
        kvs.iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn want_color_precedence() {
        for (e, isatty, expected) in [
            (vec![], true, true),
            (vec![], false, false),
            (vec![("NO_COLOR", "1")], true, false),
            (vec![("FORCE_COLOR", "1")], false, true),
            (vec![("CLICOLOR_FORCE", "1")], false, true),
            // NO_COLOR wins over force.
            (vec![("NO_COLOR", "1"), ("FORCE_COLOR", "1")], true, false),
            // Actions CI renders ANSI even though stderr is a pipe.
            (vec![("GITHUB_ACTIONS", "true")], false, true),
            (
                vec![("GITHUB_ACTIONS", "true"), ("NO_COLOR", "1")],
                false,
                false,
            ),
        ] {
            assert_eq!(
                want_color(&env(&e), isatty),
                expected,
                "{e:?} isatty={isatty}"
            );
        }
    }

    #[test]
    fn fold_markers_detects_actions() {
        assert!(!fold_markers(&env(&[])));
        assert!(fold_markers(&env(&[("GITHUB_ACTIONS", "true")])));
        assert!(!fold_markers(&env(&[("GITHUB_ACTIONS", "false")])));
    }
}

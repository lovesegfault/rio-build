//! Minimal glob matching for job-name filters (`*` = any run of bytes,
//! `?` = exactly one byte).
//!
//! Deliberately tiny instead of pulling `globset`: campaign filters only
//! need `*`/`?` over job names like
//! `python3Packages.requests.x86_64-linux`.
//!
//! Matching uses the iterative "last star" two-pointer algorithm, which is
//! O(pattern.len() * name.len()) worst case. The naive recursive
//! formulation is exponential in the number of `*`s on non-matching names;
//! patterns are operator-supplied and evaluated against every manifest
//! entry at plan time, so a pathological pattern must degrade
//! polynomially, not stall the plan stage.

/// Returns true when `name` matches `pattern` (`*` matches any run of
/// bytes including the empty one, `?` matches exactly one byte, everything
/// else is literal).
///
/// Matching is byte-wise: a multi-byte UTF-8 character needs one `?` per
/// byte of its encoding. Job names are ASCII in practice, where bytes and
/// characters coincide.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    glob_match_counted(pattern, name).0
}

/// [`glob_match`] plus the number of loop iterations taken, so tests can
/// pin the polynomial complexity bound structurally (count steps) instead
/// of with a load-sensitive wall-clock budget.
fn glob_match_counted(pattern: &str, name: &str) -> (bool, u64) {
    let (p, n) = (pattern.as_bytes(), name.as_bytes());
    let (mut pi, mut ni) = (0, 0);
    // Most recent `*`: (pattern index after it, name index its current
    // attempt starts at). On a dead end, resume there with the star
    // consuming one more byte. Remembering only the last star is
    // sufficient: an earlier star re-expanding can always be reproduced by
    // expanding the later one instead.
    let mut star: Option<(usize, usize)> = None;
    let mut steps: u64 = 0;
    while ni < n.len() {
        steps += 1;
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi + 1, ni));
            pi += 1;
        } else if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return (false, steps);
        }
    }
    // Pattern must be exhausted too; trailing `*`s match the empty suffix.
    while pi < p.len() && p[pi] == b'*' {
        steps += 1;
        pi += 1;
    }
    (pi == p.len(), steps)
}

#[cfg(test)]
mod tests {
    use super::{glob_match, glob_match_counted};

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", "anything.x86_64-linux"));
        assert!(glob_match(
            "python3Packages.*",
            "python3Packages.requests.x86_64-linux"
        ));
        assert!(glob_match("*.x86_64-linux", "hello.x86_64-linux"));
        assert!(glob_match("hello.?86_64-linux", "hello.x86_64-linux"));
        assert!(!glob_match(
            "python3Packages.*",
            "haskellPackages.lens.x86_64-linux"
        ));
        assert!(!glob_match("hello", "hello.x86_64-linux"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    /// Reference implementation: the obviously-correct recursive
    /// specification of the same semantics (`*` in the pattern is always a
    /// wildcard, even against a literal `*` in the name). Exponential in
    /// the number of `*`s, so only usable on tiny inputs.
    fn reference_match(pattern: &str, name: &str) -> bool {
        fn inner(p: &[u8], n: &[u8]) -> bool {
            match (p.first(), n.first()) {
                (None, None) => true,
                (Some(b'*'), _) => inner(&p[1..], n) || (!n.is_empty() && inner(p, &n[1..])),
                (Some(b'?'), Some(_)) => inner(&p[1..], &n[1..]),
                (Some(c), Some(d)) if c == d => inner(&p[1..], &n[1..]),
                _ => false,
            }
        }
        inner(pattern.as_bytes(), name.as_bytes())
    }

    /// Exhaustive differential check against the recursive reference over
    /// every short pattern/name combination, including literal `*` and `?`
    /// bytes in the name (the corner where naive arm ordering in the
    /// iterative matcher would treat a pattern `*` as a literal).
    #[test]
    fn matches_reference_on_exhaustive_small_inputs() {
        const PATTERN_ALPHABET: &[u8] = b"ab*?";
        const NAME_ALPHABET: &[u8] = b"ab*?";
        const MAX_LEN: usize = 4;

        fn strings(alphabet: &[u8], max_len: usize) -> Vec<String> {
            let mut out = vec![String::new()];
            let mut frontier = vec![String::new()];
            for _ in 0..max_len {
                let mut next = Vec::new();
                for s in &frontier {
                    for &c in alphabet {
                        let mut t = s.clone();
                        t.push(c as char);
                        next.push(t);
                    }
                }
                out.extend(next.iter().cloned());
                frontier = next;
            }
            out
        }

        for pattern in strings(PATTERN_ALPHABET, MAX_LEN) {
            for name in strings(NAME_ALPHABET, MAX_LEN) {
                assert_eq!(
                    glob_match(&pattern, &name),
                    reference_match(&pattern, &name),
                    "pattern={pattern:?} name={name:?}"
                );
            }
        }
    }

    /// Perf regression gate on adversarial inputs: non-matching many-star
    /// patterns over names where every literal segment matches at every
    /// position (worst case for backtracking matchers). The bound is
    /// structural — loop iterations capped at O(pattern * name), not wall
    /// clock — so it cannot flake under builder load. The smallest case
    /// runs first: if exponential backtracking is ever reintroduced, it
    /// still completes in well under a second and trips the step budget
    /// with a clear message, before the larger cases (which an exponential
    /// matcher would never finish) are reached.
    #[test]
    fn adversarial_star_pattern_is_polynomial() {
        let cases = [
            // Six stars over 40 bytes: ~100ms per name with the recursive
            // formulation, growing ~6x per added star.
            ("*a*a*a*a*a*b".to_string(), "a".repeat(40), false),
            // 25 stars over 200 bytes: unreachable for an exponential
            // matcher (would run for years), trivial for a polynomial one.
            (format!("{}*b", "*a".repeat(24)), "a".repeat(200), false),
            // Long literal segment re-walked on every star re-expansion:
            // the iterative algorithm's own worst-case shape.
            (format!("*{}b", "a".repeat(20)), "a".repeat(100), false),
            // Matching variant stays polynomial too.
            ("*a".repeat(30), "a".repeat(30), true),
        ];
        for (pattern, name, expected) in cases {
            // Sound upper bound for the last-star algorithm: at most
            // name.len() backtracks, each followed by at most pattern.len()
            // forward steps, plus the trailing-star sweep.
            let budget =
                ((pattern.len() as u64) + 1) * ((name.len() as u64) + 1) + pattern.len() as u64;
            let (matched, steps) = glob_match_counted(&pattern, &name);
            assert_eq!(matched, expected, "pattern={pattern:?} name={name:?}");
            assert!(
                steps <= budget,
                "pattern={pattern:?} name={name:?}: steps={steps} exceeds \
                 O(pattern * name) budget={budget}"
            );
        }
    }
}

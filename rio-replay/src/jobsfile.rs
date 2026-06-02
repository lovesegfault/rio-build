//! The jobs-file format — the single owner of its line grammar.
//!
//! One job name per line; `#` starts a comment, whole-line or trailing;
//! blank lines are ignored. Job names can never contain `#` (every
//! recorded name passes the
//! [`recipe::validate_job_name`](crate::evalset::recipe::validate_job_name)
//! charset rule), so splitting at the first `#` never splits a
//! legitimate name.
//!
//! Both consumers of the format parse through
//! [`parse_jobs_file_lines`]: the recorder's `--scope jobs-file:`
//! parser ([`crate::cmd::eval`]) and the campaign engine's
//! `filters.jobs_file` allowlist ([`crate::run::plan`]). They once
//! carried independent open-coded copies of the grammar, and the
//! copies drifted: the recorder accepted trailing comments while the
//! engine glued them onto the job name, so the one jobs file an
//! operator naturally maintains for both record and replay was
//! accepted by the recorder and then refused by the plan stage as
//! "not present in the archive's workload units". With one parse
//! function the grammar cannot fork; a future consumer of the format
//! has nothing left to re-derive.

/// Parse jobs-file text into its job-name entries: split each line at
/// the first `#`, trim ASCII whitespace, and drop empties. Entries are
/// returned in file order with duplicates preserved — sorting,
/// deduplication, and name validation are consumer policy, not part of
/// the format.
pub fn parse_jobs_file_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split_once('#').map_or(line, |(name, _comment)| name))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_strips_comments_blanks_and_whitespace() {
        let text = "# whole-line comment\n\
                    nixpkgs.hello.x86_64-linux  # trailing comment\n\
                    nixpkgs.jq.x86_64-linux#no-space\n\
                    \n\
                    \t  \n\
                       # indented whole-line comment\n\
                    nixpkgs.hello.x86_64-linux\n";
        // File order, duplicates preserved: consumers decide policy.
        assert_eq!(
            parse_jobs_file_lines(text),
            vec![
                "nixpkgs.hello.x86_64-linux".to_string(),
                "nixpkgs.jq.x86_64-linux".to_string(),
                "nixpkgs.hello.x86_64-linux".to_string(),
            ]
        );
        assert!(parse_jobs_file_lines("").is_empty());
        assert!(parse_jobs_file_lines("# only comments\n  # here\n").is_empty());
    }
}

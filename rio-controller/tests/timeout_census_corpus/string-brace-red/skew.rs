//! R22 plant (brace-integrity axis, bug_151): a `{` inside a STRING
//! within a cfg(test) module must not extend the skip over the
//! production item below. The wave-9 scanner counted braces on RAW
//! lines (strings and comments included, before its comment skip), so
//! the unbalanced brace in the test string swallowed everything after
//! the module — the untagged production call below was invisible.
//! Brace counting now runs over comment/string-stripped text (the
//! shared lexer's semantics); the call below is the red.
#[cfg(test)]
mod tests {
    fn t() -> &'static str {
        "{" // an unbalanced brace, safely inside a string
    }
}

pub async fn poll() {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {}).await;
}

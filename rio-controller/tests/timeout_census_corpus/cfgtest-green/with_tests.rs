// Behavior pin (not an evasion axis): sites inside #[cfg(test)] code
// are NOT production population — D2 quantifies over production Err
// arms. This tree must scan EMPTY.
pub fn production_no_timeout() {}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn test_only_timeout() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {}).await;
    }
}

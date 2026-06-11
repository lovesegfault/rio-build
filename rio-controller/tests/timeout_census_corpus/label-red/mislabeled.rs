// R22 planted red — LABEL-KEY axis: the site IS tagged, but with a
// class outside the closed {delay, refusal, irreversible} vocabulary.
// A generator that only checks tag PRESENCE admits invented classes
// (the merged_bug_109 label evasion shape).
pub async fn mislabeled_call() {
    // timeout-census: benign — not a real class
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {}).await;
}

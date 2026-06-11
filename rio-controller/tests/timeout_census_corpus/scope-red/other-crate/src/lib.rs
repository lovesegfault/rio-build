// R22 planted red — SCOPE axis: an untagged production timeout site in
// a sibling-crate-shaped tree. The generator must detect sites in ANY
// root handed to it; the production default (this crate's src/) is a
// declared argument, not a structural blindness. Widening the default
// scope is the round-9 tier-2 census plane's burn-down item.
pub async fn other_crate_call() {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {}).await;
}

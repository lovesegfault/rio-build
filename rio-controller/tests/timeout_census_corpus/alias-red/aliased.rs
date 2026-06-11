// R22 planted red — ALIAS axis: the import renames the timeout symbol
// and the call site carries NO classification tag. A generator that
// greps only the canonical spelling misses this site (the
// merged_bug_001 evasion shape, applied to this census at birth).
use tokio::time::timeout as tmo;

pub async fn aliased_call() {
    let _ = tmo(std::time::Duration::from_secs(1), async {}).await;
}

//! R22 plant (use-plain axis, bug_151): the PLAIN import production
//! (`use tokio::time::timeout;`) must enable the bare needle — the
//! untagged call below is the red. The wave-9 scanner enabled this
//! form; the plant pins it against regression while the table grows.
use tokio::time::timeout;

pub async fn poll() {
    let _ = timeout(std::time::Duration::from_secs(1), async {}).await;
}

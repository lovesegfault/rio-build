//! R22 plant (use-module-path axis, bug_151): the module-path import
//! production (`use tokio::time;` + `time::timeout(...)`) enabled no
//! needle under the wave-9 scanner — an untagged site in this form
//! passed the Totality law green. The call below is the red.
use tokio::time;

pub async fn poll() {
    let _ = time::timeout(std::time::Duration::from_secs(1), async {}).await;
}

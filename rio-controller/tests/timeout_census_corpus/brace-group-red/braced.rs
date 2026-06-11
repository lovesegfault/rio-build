//! R22 plant (use-brace-group axis, bug_151): the brace-group import
//! production — multi-line, with an alias INSIDE the group — enabled
//! no needle under the wave-9 scanner (exact-line matching), so an
//! untagged call here passed the Totality law green. The call below
//! is the red.
#[rustfmt::skip]
use tokio::time::{
    sleep,
    timeout as bounded,
};

pub async fn poll() {
    let _ = bounded(std::time::Duration::from_secs(1), async {
        sleep(std::time::Duration::ZERO).await;
    })
    .await;
}

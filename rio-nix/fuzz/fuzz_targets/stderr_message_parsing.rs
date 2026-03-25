#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::protocol::client::read_stderr_message;

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut cursor = std::io::Cursor::new(data);
        // Loop: a single wire buffer can contain multiple STDERR
        // messages back-to-back (the daemon streams them until
        // STDERR_LAST). Parse until error/EOF to exercise the
        // trailing-bytes paths.
        while read_stderr_message(&mut cursor).await.is_ok() {}
    });
});

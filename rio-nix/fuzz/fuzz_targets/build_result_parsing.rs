#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::protocol::build::{read_basic_derivation, read_build_result};

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut cursor = std::io::Cursor::new(data);
        let _ = read_build_result(&mut cursor).await;

        let mut cursor = std::io::Cursor::new(data);
        let _ = read_basic_derivation(&mut cursor).await;
    });
});

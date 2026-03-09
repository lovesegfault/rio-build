#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::protocol::wire;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();

    rt.block_on(async {
        {
            let mut cursor = Cursor::new(data);
            let _ = wire::read_u64(&mut cursor).await;
        }

        {
            let mut cursor = Cursor::new(data);
            let _ = wire::read_bool(&mut cursor).await;
        }

        {
            let mut cursor = Cursor::new(data);
            let _ = wire::read_bytes(&mut cursor).await;
        }

        {
            let mut cursor = Cursor::new(data);
            let _ = wire::read_string(&mut cursor).await;
        }

        {
            let mut cursor = Cursor::new(data);
            let _ = wire::read_strings(&mut cursor).await;
        }

        {
            let mut cursor = Cursor::new(data);
            let _ = wire::read_string_pairs(&mut cursor).await;
        }

        {
            let mut cursor = Cursor::new(data);
            let _ = wire::read_framed_stream(&mut cursor).await;
        }
    });
});

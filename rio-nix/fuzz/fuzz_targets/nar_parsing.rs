#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::nar;

fuzz_target!(|data: &[u8]| {
    // Try parsing as a NAR archive
    let mut cursor = std::io::Cursor::new(data);
    let _ = nar::parse(&mut cursor);

    // Try extracting a single file from it
    let _ = nar::extract_single_file(data);
});

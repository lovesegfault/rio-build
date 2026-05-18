#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::narinfo::NarInfo;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let _ = NarInfo::parse(input);
    }
});

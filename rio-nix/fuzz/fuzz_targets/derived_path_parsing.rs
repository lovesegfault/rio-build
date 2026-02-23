#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::protocol::derived_path::DerivedPath;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let _ = DerivedPath::parse(input);
    }
});

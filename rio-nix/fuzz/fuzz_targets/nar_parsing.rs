#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::nar;

fuzz_target!(|data: &[u8]| {
    // Try parsing as a NAR archive
    let mut cursor = std::io::Cursor::new(data);
    if let Ok(node) = nar::parse(&mut cursor) {
        // Parse succeeded → extract_to_path MUST stay confined to the
        // tempdir. Any panic/crash here is a fuzz finding; any write
        // outside `root` would surface as an OS error on a sandboxed
        // fuzz runner. parse_directory's entry-name validation
        // (r[worker.nar.entry-name-safety]) is what guarantees this.
        let dst = tempfile::TempDir::new().expect("tempdir");
        let root = dst.path().join("extracted");
        let _ = nar::extract_to_path(&node, &root);
    }

    // Try extracting a single file from it
    let _ = nar::extract_single_file(data);
});

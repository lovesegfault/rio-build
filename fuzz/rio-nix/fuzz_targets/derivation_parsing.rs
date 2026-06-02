#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::derivation::Derivation;

// Repr-faithfulness oracle: any input the typed parse boundary ACCEPTS
// must survive a write→re-parse round trip and re-serialize to the
// same bytes. A divergence between the typed representation and the
// ATerm writer (lossy accessor, classification drift, masking bug)
// becomes a fuzz crash instead of a silent corruption. The rejected
// seeds (seed-reject-*) keep the boundary's error paths in the corpus
// so coverage tracks both sides of the classification.
fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data)
        && let Ok(drv) = Derivation::parse(input)
    {
        let written = drv.to_aterm();
        let reparsed = Derivation::parse(&written)
            .expect("writer output of an accepted derivation must re-parse");
        assert_eq!(
            written,
            reparsed.to_aterm(),
            "typed repr must be byte-faithful across write->parse->write"
        );
    }
});

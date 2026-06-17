//! Store-corpus acceptance gate for the canonical Derivation codec
//! (ADR-024, the DRVPROTO verification rerun in-tree).
//!
//! Walks every `*.drv` in `/nix/store` and pushes each one through the
//! FULL gateway pipeline: ATerm parse → `to_proto` → canonical encode
//! → digest → `verify_drv_blob` (blake3 + decode + validate +
//! canonical re-encode byte-compare + ATerm reconstruction + drv_path
//! recompute + static FOD output-path recompute), then asserts the
//! reconstructed ATerm is byte-identical to the on-disk file.
//!
//! The drv-path identity is an *external* anchor: the on-disk store
//! path was minted by Nix itself, so byte-parity + path-parity proves
//! the round-tripped bytes are exactly the bytes Nix hashed.
//!
//! `#[ignore]`d: depends on the machine's store contents (and its
//! size). Run explicitly:
//!
//! ```sh
//! cargo nextest run -p rio-proto store_corpus --run-ignored all
//! # or: cargo test -p rio-proto --test derivation_store_corpus -- --ignored --nocapture
//! ```

use std::sync::Mutex;

use rio_nix::derivation::Derivation as NixDerivation;
use rio_proto::derivation_util::{derivation_digest, to_proto, verify_drv_blob};

#[derive(Default)]
struct Tally {
    total: usize,
    structured_attrs: usize,
    non_ascii: usize,
    failures: Vec<(String, String)>,
}

fn check_one(path: &str, tally: &Mutex<Tally>) {
    let fail = |stage: &str, detail: String| {
        tally
            .lock()
            .unwrap()
            .failures
            .push((path.to_string(), format!("{stage}: {detail}")));
    };

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return fail("read", e.to_string()),
    };
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => return fail("utf8", e.to_string()),
    };
    let drv = match NixDerivation::parse(&text) {
        Ok(d) => d,
        Err(e) => return fail("aterm-parse", e.to_string()),
    };

    let proto = to_proto(&drv);
    let blob = prost::Message::encode_to_vec(&proto);
    let digest = derivation_digest(&proto);

    // The claimed path is the real on-disk path — verify recomputes
    // its hash part from the round-tripped content.
    let verified = match verify_drv_blob(&blob, &digest, path) {
        Ok(v) => v,
        Err(e) => return fail("verify", e.to_string()),
    };
    if verified.aterm != text {
        let n = text
            .as_bytes()
            .iter()
            .zip(verified.aterm.as_bytes())
            .take_while(|(a, b)| a == b)
            .count();
        return fail(
            "byte-parity",
            format!(
                "lens {}/{}, first diff at {n}",
                text.len(),
                verified.aterm.len()
            ),
        );
    }

    let mut t = tally.lock().unwrap();
    t.total += 1;
    if drv.env().contains_key("__json") {
        t.structured_attrs += 1;
    }
    if !text.is_ascii() {
        t.non_ascii += 1;
    }
}

#[test]
#[ignore = "walks every .drv in /nix/store — machine-dependent, run explicitly (see module docs)"]
fn store_corpus_round_trip() {
    let mut paths: Vec<String> = std::fs::read_dir("/nix/store")
        .expect("read /nix/store")
        .filter_map(|e| {
            let name = e.ok()?.file_name();
            let name = name.to_str()?;
            name.ends_with(".drv").then(|| format!("/nix/store/{name}"))
        })
        .collect();
    paths.sort();

    let tally = Mutex::new(Tally::default());
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let chunk = paths.len().div_ceil(workers).max(1);
    std::thread::scope(|s| {
        for slice in paths.chunks(chunk) {
            let tally = &tally;
            s.spawn(move || {
                for p in slice {
                    check_one(p, tally);
                }
            });
        }
    });

    let t = tally.into_inner().unwrap();
    println!(
        "store corpus: {}/{} ok ({} structuredAttrs, {} non-ASCII, {} failures)",
        t.total,
        paths.len(),
        t.structured_attrs,
        t.non_ascii,
        t.failures.len()
    );
    for (p, why) in t.failures.iter().take(20) {
        println!("  FAIL {p}: {why}");
    }
    assert!(
        t.failures.is_empty(),
        "{} of {} drvs failed the round-trip gate",
        t.failures.len(),
        paths.len()
    );
    assert!(
        t.total >= 1000,
        "corpus too small to be meaningful: {} drvs (need >= 1000)",
        t.total
    );
}

#![no_main]

use libfuzzer_sys::fuzz_target;
use rio_nix::nar;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    // r[verify store.index.nar-ls-streaming]
    // nar_ls and parse share the wire grammar. nar_ls accepts a strict
    // SUPERSET of what parse() accepts: it streams (no MAX_CONTENT_SIZE
    // cap) and is byte-faithful (Vec<u8> path/target — no UTF-8
    // requirement). Anything parse() accepts, nar_ls() must accept and
    // agree on offsets/digests. Anything nar_ls() rejects, parse()
    // must reject. The two known one-way gaps are matched explicitly
    // so a NEW gap (a real grammar bug) still trips the catch-all.
    let parse_result = nar::parse(&mut Cursor::new(data));
    let ls_result = nar::nar_ls(Cursor::new(data));

    match (&parse_result, &ls_result) {
        (Ok(_), Ok(entries)) => {
            // r[verify store.index.nar-ls-offset]
            // r[verify store.index.file-digest]
            // Offsets must point into the input and slice to the
            // recorded size; file_digest must hash that slice.
            for e in entries {
                if e.kind != nar::NarEntryKind::Regular {
                    continue;
                }
                let off = e.nar_offset as usize;
                let end = off.checked_add(e.size as usize).expect("overflow");
                let slice = data.get(off..end).expect("nar_offset out of bounds");
                assert_eq!(
                    e.file_digest,
                    *blake3::hash(slice).as_bytes(),
                    "file_digest mismatch at {}..{}",
                    off,
                    end
                );
            }
        }
        (Err(nar::NarError::ContentTooLarge(_)), Ok(_)) => {
            // parse() buffers whole and caps at MAX_CONTENT_SIZE;
            // nar_ls streams and accepts any size. Expected divergence.
        }
        (Err(nar::NarError::InvalidUtf8 { .. }), Ok(_)) => {
            // parse()'s String-based NarNode rejects non-UTF-8 entry
            // names and symlink targets; nar_ls is byte-faithful.
            // (Other parse() InvalidUtf8 sites — magic, keywords, node
            // type — are structural mismatches nar_ls also rejects, so
            // they never reach this arm.) Expected divergence.
        }
        (Ok(node), Err(e)) => panic!("parse ok but nar_ls failed: {e:?}\nnode: {node:?}"),
        (Err(e), Ok(_)) => panic!("parse failed ({e:?}) but nar_ls ok — undocumented gap"),
        (Err(_), Err(_)) => {}
    }
});

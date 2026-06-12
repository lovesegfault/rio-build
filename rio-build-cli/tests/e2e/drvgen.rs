//! Real canonical drv fixtures: ATerm → rio-nix parse → canonical
//! proto encode, with the drv path Nix would mint for that content
//! (`make_text(name, sha256(aterm), refs)`). Same construction as the
//! rio-store DrvBlobService tests, extended with inputDrvs/inputSrcs
//! so chains and wide graphs pass the store's full server-side
//! cross-check (digest, canonical re-encode, ATerm reconstruction,
//! drv_path recompute).

use rio_nix::derivation::Derivation as NixDerivation;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::StorePath;
use rio_proto::derivation_util::{canonical_encode, derivation_digest, to_proto};
use rio_proto::types::{DerivationNode, DrvBlob};

/// One fixture derivation, ready for both the worker channel
/// (skeleton node + blob) and assertions (digest, paths).
#[derive(Clone)]
pub struct DrvFixture {
    pub node: DerivationNode,
    pub blob: DrvBlob,
    pub digest: [u8; 32],
    pub drv_path: String,
    pub out_path: String,
}

/// Deterministic fake-but-parseable output path for `tag`.
pub fn fake_out_path(tag: &str) -> String {
    // nixbase32 alphabet (no e/o/u/t).
    const ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let h = blake3::hash(tag.as_bytes());
    let part: String = h.as_bytes()[..32]
        .iter()
        .map(|b| ALPHABET[(*b as usize) % 32] as char)
        .collect();
    format!("/nix/store/{part}-{tag}")
}

/// Build a real canonical drv whose inputDrvs are `inputs` and whose
/// inputSrcs are `srcs` (full store paths).
pub fn make_drv(tag: &str, inputs: &[&DrvFixture], srcs: &[&str]) -> DrvFixture {
    let out_path = fake_out_path(tag);

    let mut input_drvs: Vec<String> = inputs
        .iter()
        .map(|f| format!(r#"("{}",["out"])"#, f.drv_path))
        .collect();
    input_drvs.sort();
    let mut input_srcs: Vec<String> = srcs.iter().map(|s| format!("\"{s}\"")).collect();
    input_srcs.sort();

    let aterm = format!(
        r#"Derive([("out","{out_path}","","")],[{drvs}],[{srcs}],"x86_64-linux","/bin/sh",["-c","echo {tag}"],[("name","{tag}")])"#,
        drvs = input_drvs.join(","),
        srcs = input_srcs.join(","),
    );
    let drv = NixDerivation::parse(&aterm).expect("fixture ATerm parses");
    let proto = to_proto(&drv);
    let body = canonical_encode(&proto);
    let digest = derivation_digest(&proto);

    let h = NixHash::compute(HashAlgo::SHA256, aterm.as_bytes());
    let refs: Vec<StorePath> = inputs
        .iter()
        .map(|f| f.drv_path.as_str())
        .chain(srcs.iter().copied())
        .map(|p| StorePath::parse(p).expect("fixture refs parse"))
        .collect();
    let drv_path = StorePath::make_text(&format!("{tag}.drv"), &h, &refs)
        .expect("make_text")
        .to_string();

    let mut node = rio_test_support::fixtures::make_derivation_node(tag, "x86_64-linux");
    node.drv_path = drv_path.clone();
    node.pname = tag.to_string();
    node.output_names = vec!["out".into()];
    node.expected_output_paths = vec![out_path.clone()];
    node.drv_digest = digest.to_vec();
    node.input_drv_digests = inputs.iter().map(|f| f.digest.to_vec()).collect();
    node.drv_content = vec![];

    DrvFixture {
        blob: DrvBlob {
            digest: digest.to_vec(),
            drv_path: drv_path.clone(),
            body,
        },
        node,
        digest,
        drv_path,
        out_path,
    }
}

/// A linear chain `tags[0] ← tags[1] ← … ← tags[n-1]` (last is root).
pub fn chain(tags: &[&str]) -> Vec<DrvFixture> {
    let mut out: Vec<DrvFixture> = Vec::with_capacity(tags.len());
    for (i, tag) in tags.iter().enumerate() {
        let prev: Vec<&DrvFixture> = if i == 0 { vec![] } else { vec![&out[i - 1]] };
        out.push(make_drv(tag, &prev, &[]));
    }
    out
}

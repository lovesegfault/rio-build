//! `exportReferencesGraph` materialization.
//!
//! A derivation can request the reference graph of one or more of its
//! inputs as a build-time file (`exportReferencesGraph = ["closure"
//! some-path]`, or the structured-attrs object form). CppNix writes:
//!
//! - non-structured-attrs: the legacy "validity registration" text
//!   format (`nix-store --register-validity` input) to
//!   `<build dir>/<name>` — per path: the path, its deriver (always
//!   empty here: registrations are written with `showDerivers = false`),
//!   the reference count, then one reference per line; paths and
//!   references in sorted order;
//! - structured-attrs: the JSON path-info array substituted for the
//!   graph key inside `.attrs.json` (see [`super::attrs`]).
//!
//! Both forms need the same two computations, which live here: the
//! closure of the requested paths *within the build's input closure*
//! (a path outside it is an input-rejection, matching CppNix
//! `exportReferences`), and the per-path info rendering.
//!
//! The closure data comes from the input metadata rio already fetched
//! for the synthetic-DB path (`ValidatedPathInfo`: references, NAR hash
//! and size) — no additional store round-trips.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rio_nix::hash::{HashAlgo, NixHash};
use rio_proto::validated::ValidatedPathInfo;
use serde_json::Value;

use super::GlueError;

/// Index over the build's input closure metadata.
pub(crate) struct ClosureIndex<'a> {
    by_path: HashMap<&'a str, &'a ValidatedPathInfo>,
    /// The full input closure (every store path the build may read).
    /// Membership here is the "is it in the input closure" gate.
    input_closure: BTreeSet<&'a str>,
}

impl<'a> ClosureIndex<'a> {
    pub(crate) fn new(metadata: &'a [ValidatedPathInfo], input_paths: &'a [String]) -> Self {
        Self {
            by_path: metadata
                .iter()
                .map(|i| (i.store_path.as_str(), i))
                .collect(),
            input_closure: input_paths.iter().map(String::as_str).collect(),
        }
    }

    /// The closure of `targets` (BFS over references), all of which must
    /// lie inside the build's input closure. Returned sorted (store-path
    /// order), which is also the order CppNix's `StorePathSet` iterates.
    fn closure_of(&self, targets: &[String]) -> Result<BTreeSet<&'a str>, GlueError> {
        let mut closure: BTreeSet<&'a str> = BTreeSet::new();
        let mut queue: Vec<&str> = Vec::new();

        for t in targets {
            if !self.input_closure.contains(t.as_str()) {
                return Err(GlueError::ExportRefsOutsideClosure { path: t.clone() });
            }
            queue.push(t.as_str());
        }

        while let Some(p) = queue.pop() {
            let Some(info) = self.by_path.get(p) else {
                // Every input-closure path has metadata (the same data
                // fed the synthetic DB); a miss means the caller passed
                // inconsistent inputs.
                return Err(GlueError::ExportRefsMissingMetadata { path: p.to_owned() });
            };
            if !closure.insert(info.store_path.as_str()) {
                continue;
            }
            for r in &info.references {
                // Self-references are common (path referencing itself);
                // the insert() above already de-duplicates cycles.
                queue.push(r.as_str());
            }
        }

        // CppNix expands the closure with the outputs of any .drv files
        // it contains (the closureInfo / NixOS-image pattern). That
        // expansion needs the .drv *contents* (to learn its outputs) and
        // those outputs' own closures — neither of which is part of the
        // build's input metadata. Today the daemon-based path fails the
        // same way (the synthetic DB doesn't contain those outputs
        // either), so rather than half-implement it we reject loudly.
        // TODO: support .drv expansion by parsing the .drv from the
        // merged store and requiring its outputs to be present in the
        // input closure.
        if let Some(drv) = closure.iter().find(|p| p.ends_with(".drv")) {
            return Err(GlueError::ExportRefsDrvExpansionUnsupported {
                path: (*drv).to_owned(),
            });
        }

        Ok(closure)
    }

    /// The legacy registration text for the closure of `targets`.
    pub(crate) fn registration_text(&self, targets: &[String]) -> Result<Vec<u8>, GlueError> {
        let closure = self.closure_of(targets)?;
        let mut out = String::new();
        for p in &closure {
            let info = self.by_path[p];
            out.push_str(p);
            out.push('\n');
            // Deriver line: always empty (showDerivers = false), but the
            // line itself is present.
            out.push('\n');
            let mut refs: Vec<&str> = info.references.iter().map(|r| r.as_str()).collect();
            refs.sort_unstable();
            out.push_str(&refs.len().to_string());
            out.push('\n');
            for r in refs {
                out.push_str(r);
                out.push('\n');
            }
        }
        Ok(out.into_bytes())
    }

    /// The structured-attrs JSON form: an array of path-info objects
    /// (path, narHash (SRI), narSize, references, closureSize) for the
    /// closure of `targets`, sorted by path.
    ///
    /// Field set note: this matches the long-standing
    /// `Store::pathInfoToJSON(..., includeImpureInfo = false,
    /// showClosureSize = true)` shape. The differential harness compares
    /// the rendered `.attrs.json` against the deployed Nix oracle and is
    /// the authority if the deployed version's field set differs.
    pub(crate) fn closure_info_json(&self, targets: &[String]) -> Result<Value, GlueError> {
        let closure = self.closure_of(targets)?;
        // Per-path closure SETS are memoized across the member loop:
        // closureInfo-style graphs (NixOS images) have thousands of
        // members whose closures overlap almost entirely, and the naive
        // per-member BFS is O(N²) traversals. set(p) = {p} ∪ ⋃ set(ref)
        // computed once per path in dependency-first order; sizes are
        // then a sum over the memoized set.
        let mut memo: BTreeMap<&str, std::rc::Rc<BTreeSet<&str>>> = BTreeMap::new();
        let arr: Vec<Value> = closure
            .iter()
            .map(|p| {
                let info = self.by_path[p];
                let mut refs: Vec<&str> = info.references.iter().map(|r| r.as_str()).collect();
                refs.sort_unstable();
                let nar_hash = NixHash::new(HashAlgo::SHA256, info.nar_hash.to_vec())
                    .expect("32-byte sha256 digest is always valid");
                serde_json::json!({
                    "path": p,
                    "narHash": nar_hash.to_sri(),
                    "narSize": info.nar_size,
                    "references": refs,
                    "closureSize": self.closure_size_memo(p, &mut memo),
                })
            })
            .collect();
        Ok(Value::Array(arr))
    }

    /// Sum of `narSize` over the closure of one path (which is fully
    /// inside the index by construction — `closure_of` validated it),
    /// using `memo` to share already-computed closure sets between
    /// calls. Self-references are tolerated (a path is always in its
    /// own set); store-path reference graphs are otherwise acyclic.
    fn closure_size_memo(
        &'a self,
        path: &'a str,
        memo: &mut BTreeMap<&'a str, std::rc::Rc<BTreeSet<&'a str>>>,
    ) -> u64 {
        // Iterative post-order: a path is finalized only after all its
        // references are, so each set is built exactly once.
        let mut stack: Vec<(&str, bool)> = vec![(path, false)];
        while let Some((p, children_done)) = stack.pop() {
            if memo.contains_key(p) {
                continue;
            }
            let Some(info) = self.by_path.get(p) else {
                // Outside the index (cannot happen for closure_of
                // output); treat as empty so the sum stays defined.
                memo.insert(p, std::rc::Rc::new(BTreeSet::new()));
                continue;
            };
            if children_done {
                let mut set: BTreeSet<&str> = BTreeSet::new();
                set.insert(info.store_path.as_str());
                for r in &info.references {
                    if let Some(child) = memo.get(r.as_str()) {
                        set.extend(child.iter().copied());
                    }
                }
                memo.insert(p, std::rc::Rc::new(set));
            } else {
                stack.push((p, true));
                for r in &info.references {
                    if r.as_str() != p && !memo.contains_key(r.as_str()) {
                        stack.push((r.as_str(), false));
                    }
                }
            }
        }
        memo[path]
            .iter()
            .map(|p| self.by_path.get(p).map_or(0, |i| i.nar_size))
            .sum()
    }
}

/// Validate an `exportReferencesGraph` graph name.
///
/// The name is tenant-controlled (straight from the derivation env /
/// `__json`) and becomes a file name under `/build` in the flat form,
/// so it must never be able to traverse paths. The accepted set is
/// CppNix's own check — first char `[A-Za-z_]`, rest
/// `[A-Za-z0-9_.-]` — which inherently rejects empty names, `.`, `..`,
/// anything containing `/`, and NUL. Real-world names like
/// `closure-info` (dashes, dots) remain accepted; do not tighten
/// further or drvs Nix builds would be rejected here.
pub(crate) fn validate_graph_name(name: &str) -> Result<(), GlueError> {
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(GlueError::ExportRefsInvalidName {
            name: name.to_owned(),
        })
    }
}

/// Parse the flat-env `exportReferencesGraph` value: alternating
/// `name path name path …` whitespace-separated pairs. Graph names are
/// validated (see [`validate_graph_name`]) so a hostile name is
/// rejected as the tenant's input error before any path is built from
/// it.
pub(crate) fn parse_flat_export_refs(value: &str) -> Result<Vec<(String, String)>, GlueError> {
    let words: Vec<&str> = value.split_whitespace().collect();
    if !words.len().is_multiple_of(2) {
        return Err(GlueError::ExportRefsMalformed {
            value: value.to_owned(),
        });
    }
    words
        .chunks_exact(2)
        .map(|c| {
            validate_graph_name(c[0])?;
            Ok((c[0].to_owned(), c[1].to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::store_path::StorePath;

    fn info(path: &str, size: u64, refs: &[&str]) -> ValidatedPathInfo {
        ValidatedPathInfo {
            store_path: StorePath::parse(path).unwrap(),
            store_path_hash: vec![],
            deriver: None,
            nar_hash: [7u8; 32],
            nar_size: size,
            references: refs.iter().map(|r| StorePath::parse(r).unwrap()).collect(),
            registration_time: 0,
            ultimate: false,
            signatures: vec![],
            content_address: None,
        }
    }

    const A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a";
    const B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b";
    const C: &str = "/nix/store/cccccccccccccccccccccccccccccccc-c";
    const OUTSIDE: &str = "/nix/store/oooooooooooooooooooooooooooooooo-outside";

    fn fixture() -> (Vec<ValidatedPathInfo>, Vec<String>) {
        // a → b → c, with a also → c (diamond-ish) and c self-referencing.
        let infos = vec![info(A, 10, &[B, C]), info(B, 20, &[C]), info(C, 40, &[C])];
        let paths = vec![A.to_string(), B.to_string(), C.to_string()];
        (infos, paths)
    }

    #[test]
    fn registration_text_format() {
        let (infos, paths) = fixture();
        let index = ClosureIndex::new(&infos, &paths);
        let text = String::from_utf8(index.registration_text(&[A.to_string()]).unwrap()).unwrap();
        // Sorted by store path (a, b, c); per entry: path, empty deriver
        // line, ref count, refs sorted.
        let expected = format!("{A}\n\n2\n{B}\n{C}\n{B}\n\n1\n{C}\n{C}\n\n1\n{C}\n");
        assert_eq!(text, expected);
    }

    #[test]
    fn closure_is_transitive_and_deduplicated() {
        let (infos, paths) = fixture();
        let index = ClosureIndex::new(&infos, &paths);
        // Starting from B: closure = {B, C} (C's self-ref doesn't loop).
        let text = String::from_utf8(index.registration_text(&[B.to_string()]).unwrap()).unwrap();
        assert!(text.contains(B));
        assert!(text.contains(C));
        assert!(!text.contains(A));
    }

    #[test]
    fn target_outside_input_closure_is_rejected() {
        let (infos, paths) = fixture();
        let index = ClosureIndex::new(&infos, &paths);
        let err = index.registration_text(&[OUTSIDE.to_string()]).unwrap_err();
        assert!(matches!(err, GlueError::ExportRefsOutsideClosure { path } if path == OUTSIDE));
    }

    #[test]
    fn drv_in_closure_is_rejected_for_now() {
        let drv_path = "/nix/store/dddddddddddddddddddddddddddddddd-x.drv";
        let infos = vec![info(A, 10, &[drv_path]), info(drv_path, 5, &[])];
        let paths = vec![A.to_string(), drv_path.to_string()];
        let index = ClosureIndex::new(&infos, &paths);
        let err = index.registration_text(&[A.to_string()]).unwrap_err();
        assert!(matches!(
            err,
            GlueError::ExportRefsDrvExpansionUnsupported { .. }
        ));
    }

    #[test]
    fn closure_size_is_per_path() {
        let (infos, paths) = fixture();
        let index = ClosureIndex::new(&infos, &paths);
        let v = index.closure_info_json(&[A.to_string()]).unwrap();
        let arr = v.as_array().unwrap();
        // Sorted: a, b, c. closureSize: a=10+20+40, b=20+40, c=40.
        assert_eq!(arr[0]["path"], serde_json::json!(A));
        assert_eq!(arr[0]["closureSize"], serde_json::json!(70));
        assert_eq!(arr[1]["closureSize"], serde_json::json!(60));
        assert_eq!(arr[2]["closureSize"], serde_json::json!(40));
        assert_eq!(arr[2]["narSize"], serde_json::json!(40));
    }

    #[test]
    fn flat_export_refs_parsing() {
        assert_eq!(
            parse_flat_export_refs("closure /nix/store/x graph2 /nix/store/y").unwrap(),
            vec![
                ("closure".to_string(), "/nix/store/x".to_string()),
                ("graph2".to_string(), "/nix/store/y".to_string()),
            ]
        );
        assert!(parse_flat_export_refs("name-without-path /x odd").is_err());
        assert_eq!(parse_flat_export_refs("").unwrap(), vec![]);
    }

    #[test]
    fn graph_names_are_validated() {
        // Real-world names pass: dashes and dots are legal past the
        // first character (e.g. nixpkgs' `closure-info`).
        assert!(parse_flat_export_refs("closure-info /nix/store/x").is_ok());
        assert!(validate_graph_name("registration_v2.txt").is_ok());

        // Path traversal and separators are rejected as the tenant's
        // input error, naming the offending graph name.
        let err = parse_flat_export_refs("../escape /nix/store/x").unwrap_err();
        assert!(
            matches!(err, GlueError::ExportRefsInvalidName { ref name } if name == "../escape"),
            "{err}"
        );
        let err = parse_flat_export_refs("sub/dir /nix/store/x").unwrap_err();
        assert!(
            matches!(err, GlueError::ExportRefsInvalidName { ref name } if name == "sub/dir"),
            "{err}"
        );
        // `.`/`..`/empty/leading-digit/NUL are all outside CppNix's
        // accepted set too.
        for bad in [".", "..", "", "0day", "with\0nul"] {
            assert!(
                validate_graph_name(bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }
}

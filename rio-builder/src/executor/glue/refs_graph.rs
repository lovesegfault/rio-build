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
//! `exportReferences`), and the per-path info rendering. Like CppNix,
//! the closure is then expanded with the output closures of any `.drv`
//! files it contains (see `closure_of`). One deliberate strictness
//! difference: CppNix resolves those output closures from whatever
//! happens to be valid in the builder-local store, so the same
//! derivation can succeed on one machine and fail on another; rio only
//! consults the build's declared input metadata, so the outcome is the
//! same on every worker.
//!
//! The closure data comes from the input metadata that accompanies the
//! build's input manifest (`ValidatedPathInfo`: references, NAR hash
//! and size) — no store round-trips; the only file access is reading
//! `.drv` text from the already-materialized input store during the
//! expansion above.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use rio_nix::derivation::Derivation;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::basename;
use rio_proto::validated::ValidatedPathInfo;
use serde_json::Value;

use super::GlueError;

/// Index over the build's input closure metadata.
pub(crate) struct ClosureIndex<'a> {
    by_path: HashMap<&'a str, &'a ValidatedPathInfo>,
    /// The full input closure (every store path the build may read).
    /// Membership here is the "is it in the input closure" gate.
    input_closure: BTreeSet<&'a str>,
    /// Host directory holding the materialized inputs (the merged-store
    /// bind source), used to read `.drv` contents when a graph closure
    /// needs derivation-output expansion. `None` only in callers whose
    /// graphs cannot contain `.drv` paths (unit fixtures).
    store_dir: Option<&'a Path>,
}

impl<'a> ClosureIndex<'a> {
    pub(crate) fn new(metadata: &'a [ValidatedPathInfo], input_paths: &'a [String]) -> Self {
        Self {
            by_path: metadata
                .iter()
                .map(|i| (i.store_path.as_str(), i))
                .collect(),
            input_closure: input_paths.iter().map(String::as_str).collect(),
            store_dir: None,
        }
    }

    /// Provide the host directory the input closure is materialized in,
    /// enabling `.drv` closure expansion (reading the drv text is the
    /// only file access this module performs).
    pub(crate) fn with_store_dir(mut self, dir: &'a Path) -> Self {
        self.store_dir = Some(dir);
        self
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

        // CppNix (`LocalDerivationGoal::exportReferences`) post-processes
        // the closure: every `.drv` file in it is parsed and the closure
        // of each of its *outputs* is unioned in — the pattern used to
        // hand a build the full build-time dependency graph of something
        // (installer images, `closureInfo`-style registration sets). Two
        // details mirrored exactly:
        //   - the expansion runs over a snapshot of the closure, so a
        //     `.drv` that only appears inside an expanded output closure
        //     is not itself expanded;
        //   - an output without a statically-known path (content-
        //     addressed derivation) is an error, as in CppNix.
        // CppNix reads output closures from the global store DB; the
        // worker's view of the store is the input metadata index, so
        // every expanded path must be present there. Whenever the
        // deriving expression used `drvPath`-style context the resolver
        // has already pulled those outputs into the input set, so this
        // is the common case rather than an extra requirement.
        let drvs: Vec<&'a str> = closure
            .iter()
            .copied()
            .filter(|p| p.ends_with(".drv"))
            .collect();
        for drv_path in drvs {
            for (output, out_path) in self.drv_outputs(drv_path)? {
                if out_path.is_empty() {
                    return Err(GlueError::ExportRefsDrvFloatingOutput {
                        drv: drv_path.to_owned(),
                        output,
                    });
                }
                // BFS the output's closure out of the index. Unlike the
                // requested graph targets, the output is not required to
                // be inside the *input closure* (the registration file
                // may name paths the sandbox cannot read — same as
                // CppNix), but it must be known to the index.
                let mut queue: Vec<&str> = vec![out_path.as_str()];
                while let Some(p) = queue.pop() {
                    let Some(info) = self.by_path.get(p) else {
                        return Err(GlueError::ExportRefsDrvOutputMissing {
                            drv: drv_path.to_owned(),
                            path: p.to_owned(),
                        });
                    };
                    if !closure.insert(info.store_path.as_str()) {
                        continue;
                    }
                    for r in &info.references {
                        queue.push(r.as_str());
                    }
                }
            }
        }

        Ok(closure)
    }

    /// Read and parse a `.drv` from the materialized input store and
    /// return its `(output name, declared store path)` pairs (the
    /// declared path is empty for floating content-addressed outputs).
    fn drv_outputs(&self, drv_store_path: &str) -> Result<Vec<(String, String)>, GlueError> {
        let unreadable = |reason: String| GlueError::ExportRefsDrvUnreadable {
            path: drv_store_path.to_owned(),
            reason,
        };
        let Some(dir) = self.store_dir else {
            return Err(unreadable(
                "no materialized input store is available to this caller".to_owned(),
            ));
        };
        let Some(base) = basename(drv_store_path) else {
            return Err(unreadable("not a valid store path".to_owned()));
        };
        let host_path = dir.join(base);
        let text = std::fs::read_to_string(&host_path)
            .map_err(|e| unreadable(format!("reading {}: {e}", host_path.display())))?;
        let drv = Derivation::parse(&text).map_err(|e| unreadable(format!("parsing: {e}")))?;
        Ok(drv
            .outputs()
            .iter()
            .map(|o| (o.name().to_owned(), o.path().to_owned()))
            .collect())
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

    const DRV: &str = "/nix/store/dddddddddddddddddddddddddddddddd-probe.drv";
    const DOUT: &str = "/nix/store/ffffffffffffffffffffffffffffffff-probe-out";
    const DRV2: &str = "/nix/store/gggggggggggggggggggggggggggggggg-inner.drv";
    const OUT2: &str = "/nix/store/hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh-inner-out";
    const FDRV: &str = "/nix/store/jjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjj-fetch.drv";
    const FOUT: &str = "/nix/store/kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk-fetched";

    /// Write a minimal input-addressed `.drv` (ATerm) into `dir` under
    /// the store-path basename, declaring the given
    /// `(name, path, hashAlgo, hash)` outputs: empty hash fields =
    /// input-addressed, empty path + hashAlgo = floating CA, path +
    /// hash fields = fixed-output.
    fn write_drv(
        dir: &std::path::Path,
        drv_store_path: &str,
        outputs: &[(&str, &str, &str, &str)],
    ) {
        let outs: Vec<String> = outputs
            .iter()
            .map(|(n, p, algo, hash)| format!("(\"{n}\",\"{p}\",\"{algo}\",\"{hash}\")"))
            .collect();
        let env: Vec<String> = outputs
            .iter()
            .map(|(n, p, _, _)| format!("(\"{n}\",\"{p}\")"))
            .collect();
        let aterm = format!(
            "Derive([{}],[],[],\"x86_64-linux\",\"/bin/sh\",[],[{}])",
            outs.join(","),
            env.join(",")
        );
        std::fs::write(dir.join(basename(drv_store_path).unwrap()), aterm).unwrap();
    }

    #[test]
    fn drv_in_closure_expands_to_its_output_closure() {
        // A (the requested target) → DRV; DRV declares output DOUT → C.
        // The graph must contain {A, DRV, DOUT, C}: the derivation
        // closure plus each output's closure, exactly CppNix's
        // `exportReferences` expansion. DOUT and C are deliberately NOT
        // part of the input closure — expanded outputs only need
        // metadata, not sandbox visibility.
        let infos = vec![
            info(A, 10, &[DRV]),
            info(DRV, 5, &[]),
            info(DOUT, 30, &[C]),
            info(C, 40, &[]),
        ];
        let paths = vec![A.to_string(), DRV.to_string()];
        let tmp = tempfile::tempdir().unwrap();
        write_drv(tmp.path(), DRV, &[("out", DOUT, "", "")]);
        let index = ClosureIndex::new(&infos, &paths).with_store_dir(tmp.path());

        let text = String::from_utf8(index.registration_text(&[A.to_string()]).unwrap()).unwrap();
        for p in [A, DRV, DOUT, C] {
            assert!(text.contains(p), "{p} missing from registration:\n{text}");
        }

        // The structured-attrs JSON form sees the same expansion, with
        // closure sizes computed over the expanded set.
        let v = index.closure_info_json(&[A.to_string()]).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        let dout = arr.iter().find(|e| e["path"] == DOUT).unwrap();
        assert_eq!(dout["closureSize"], serde_json::json!(70));
    }

    #[test]
    fn drv_floating_output_is_rejected() {
        let infos = vec![info(A, 10, &[DRV]), info(DRV, 5, &[])];
        let paths = vec![A.to_string(), DRV.to_string()];
        let tmp = tempfile::tempdir().unwrap();
        // Floating-CA output: empty declared path, hash algo set.
        write_drv(tmp.path(), DRV, &[("out", "", "r:sha256", "")]);
        let index = ClosureIndex::new(&infos, &paths).with_store_dir(tmp.path());
        let err = index.registration_text(&[A.to_string()]).unwrap_err();
        assert!(
            matches!(err, GlueError::ExportRefsDrvFloatingOutput { ref output, .. } if output == "out"),
            "{err}"
        );
    }

    #[test]
    fn drv_reached_via_expanded_output_is_not_reexpanded() {
        // Snapshot rule (CppNix iterates a snapshot of the closure): DRV's
        // output DOUT references DRV2 — a derivation that only enters the
        // graph through that expanded output closure. DRV2 itself must NOT
        // be expanded: it is readable and well-formed, but its declared
        // output OUT2 has no metadata in the index, so a (wrong) fixpoint
        // expansion would fail with ExportRefsDrvOutputMissing instead of
        // succeeding.
        let infos = vec![
            info(A, 10, &[DRV]),
            info(DRV, 5, &[]),
            info(DOUT, 30, &[DRV2]),
            info(DRV2, 5, &[]),
        ];
        let paths = vec![A.to_string(), DRV.to_string()];
        let tmp = tempfile::tempdir().unwrap();
        write_drv(tmp.path(), DRV, &[("out", DOUT, "", "")]);
        write_drv(tmp.path(), DRV2, &[("out", OUT2, "", "")]);
        let index = ClosureIndex::new(&infos, &paths).with_store_dir(tmp.path());

        let text = String::from_utf8(index.registration_text(&[A.to_string()]).unwrap()).unwrap();
        assert!(
            text.contains(DRV2),
            "DRV2 belongs to DOUT's closure:\n{text}"
        );
        assert!(
            !text.contains(OUT2),
            "OUT2 must not appear: DRV2 was reached only via an expanded output closure\n{text}"
        );
    }

    #[test]
    fn fixed_output_drv_in_graph_expands_like_any_other() {
        // A fixed-output derivation has a declared path AND hash fields;
        // it must expand normally, not trip the floating-CA rejection
        // (which is keyed on an *empty* declared path).
        let infos = vec![
            info(A, 10, &[FDRV]),
            info(FDRV, 5, &[]),
            info(FOUT, 30, &[]),
        ];
        let paths = vec![A.to_string(), FDRV.to_string()];
        let tmp = tempfile::tempdir().unwrap();
        write_drv(
            tmp.path(),
            FDRV,
            &[(
                "out",
                FOUT,
                "r:sha256",
                "1111111111111111111111111111111111111111111111111111111111111111",
            )],
        );
        let index = ClosureIndex::new(&infos, &paths).with_store_dir(tmp.path());

        let text = String::from_utf8(index.registration_text(&[A.to_string()]).unwrap()).unwrap();
        assert!(text.contains(FOUT), "{text}");
    }

    #[test]
    fn drv_output_without_metadata_is_rejected() {
        // DRV declares DOUT but the index has no metadata for it.
        let infos = vec![info(A, 10, &[DRV]), info(DRV, 5, &[])];
        let paths = vec![A.to_string(), DRV.to_string()];
        let tmp = tempfile::tempdir().unwrap();
        write_drv(tmp.path(), DRV, &[("out", DOUT, "", "")]);
        let index = ClosureIndex::new(&infos, &paths).with_store_dir(tmp.path());
        let err = index.registration_text(&[A.to_string()]).unwrap_err();
        assert!(
            matches!(err, GlueError::ExportRefsDrvOutputMissing { ref path, .. } if path == DOUT),
            "{err}"
        );
    }

    #[test]
    fn unreadable_drv_is_rejected() {
        let infos = vec![info(A, 10, &[DRV]), info(DRV, 5, &[])];
        let paths = vec![A.to_string(), DRV.to_string()];

        // No store dir at all.
        let index = ClosureIndex::new(&infos, &paths);
        let err = index.registration_text(&[A.to_string()]).unwrap_err();
        assert!(
            matches!(err, GlueError::ExportRefsDrvUnreadable { .. }),
            "{err}"
        );

        // Store dir present but the .drv file is missing from it.
        let tmp = tempfile::tempdir().unwrap();
        let index = ClosureIndex::new(&infos, &paths).with_store_dir(tmp.path());
        let err = index.registration_text(&[A.to_string()]).unwrap_err();
        assert!(
            matches!(err, GlueError::ExportRefsDrvUnreadable { .. }),
            "{err}"
        );
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

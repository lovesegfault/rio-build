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
//! and size) — no store round-trips and zero file access of any kind:
//! `.drv` text for the expansion above arrives in a caller-supplied
//! table assembled at input resolution (`builder.glue.pure`).
//!
//! All graph traversal (closure expansion, cycle detection, closure
//! sizing) is delegated to [`rio_nix::closure`] — this module contains
//! zero hand-rolled graph loops. Cyclic reference metadata, which
//! rio-store can represent but CppNix's local store cannot, is rejected
//! fail-closed as the tenant's input error (never worker-transient): see
//! `closure_of`'s cycle gate.

use std::collections::{BTreeSet, HashMap};

use rio_nix::derivation::Derivation;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::{STORE_DIR, StorePath};
use rio_proto::validated::ValidatedPathInfo;
use serde_json::Value;

use super::GlueError;

/// CppNix `toStorePath()`: a graph target may name any path *inside* a
/// store path (`"${pkg}/bin/tool"` — the shape `writeReferencesToFile`
/// and NixOS initrd builders produce); the exported closure is that of
/// the containing store path. Truncate to the `/nix/store/<hash>-<name>`
/// root and validate it is a well-formed store path; paths outside the
/// store are rejected exactly as CppNix's `toStorePath` rejects them.
fn to_store_path_root(target: &str) -> Result<String, GlueError> {
    let rel = target
        .strip_prefix(STORE_DIR)
        .and_then(|r| r.strip_prefix('/'))
        .ok_or_else(|| GlueError::ExportRefsNotAStorePath {
            path: target.to_owned(),
        })?;
    let base = rel.split('/').next().unwrap_or("");
    let root = format!("{STORE_DIR}/{base}");
    if StorePath::parse(&root).is_err() {
        return Err(GlueError::ExportRefsNotAStorePath {
            path: target.to_owned(),
        });
    }
    Ok(root)
}

/// Index over the build's input closure metadata.
pub(crate) struct ClosureIndex<'a> {
    by_path: HashMap<&'a str, &'a ValidatedPathInfo>,
    /// The full input closure (every store path the build may read).
    /// Membership here is the "is it in the input closure" gate.
    input_closure: BTreeSet<&'a str>,
    /// ATerm text per `.drv` store path, supplied by the caller (the
    /// executor's input resolution, or the differential harness), used
    /// when a graph closure needs derivation-output expansion. `None`
    /// only in callers whose graphs cannot contain `.drv` paths (unit
    /// fixtures). The glue performs no file access of any kind
    /// (`builder.glue.pure`): every derivation byte arrives here.
    drv_table: Option<&'a std::collections::BTreeMap<String, String>>,
}

impl<'a> ClosureIndex<'a> {
    pub(crate) fn new(metadata: &'a [ValidatedPathInfo], input_paths: &'a [String]) -> Self {
        Self {
            by_path: metadata
                .iter()
                .map(|i| (i.store_path.as_str(), i))
                .collect(),
            input_closure: input_paths.iter().map(String::as_str).collect(),
            drv_table: None,
        }
    }

    /// Provide the derivation-text table, enabling `.drv` closure
    /// expansion. The table is assembled upstream (input resolution
    /// retains every fetched ATerm text); this module never touches
    /// the filesystem.
    pub(crate) fn with_drv_table(
        mut self,
        table: &'a std::collections::BTreeMap<String, String>,
    ) -> Self {
        self.drv_table = Some(table);
        self
    }

    /// The references of an index entry, as `&'a str` borrowed from the
    /// metadata (the resolver shape `rio_nix::closure` walks over).
    fn refs_of_known(info: &'a ValidatedPathInfo) -> impl Iterator<Item = &'a str> {
        info.references.iter().map(|r| r.as_str())
    }

    /// Phase 1 of the graph materialization: the reference closure of
    /// `targets` — CppNix `toStorePath()` normalization, the
    /// input-closure containment gate, and the BFS over references.
    /// This is the SHARED walk: [`closure_of`](Self::closure_of) (the
    /// glue's expansion consumer) builds on its result, and
    /// [`DrvTextDemand::from_declaration`] (the input resolver's demand
    /// producer) reads exactly its `.drv` members — one function, so
    /// the producer and the consumer cannot diverge on which paths a
    /// declaration reaches.
    // r[impl builder.glue.drv-table-demand]
    fn reference_closure_of(
        &self,
        targets: &[String],
    ) -> Result<rio_nix::closure::ClosureSet<'a>, GlueError> {
        let mut set = rio_nix::closure::ClosureSet::new();

        // CppNix normalizes each target with `toStorePath()` before the
        // closure walk, so sub-paths inside a store path are valid
        // targets; the containment gate then applies to the containing
        // store path.
        let mut roots: Vec<&'a str> = Vec::new();
        for t in targets {
            let root = to_store_path_root(t)?;
            let Some(canonical) = self.input_closure.get(root.as_str()) else {
                return Err(GlueError::ExportRefsOutsideClosure { path: t.clone() });
            };
            roots.push(canonical);
        }

        // BFS over references. Every reached path must have an entry in
        // the build's input metadata index; a miss means the caller
        // passed inconsistent inputs.
        set.extend(roots, |p| match self.by_path.get(p) {
            Some(info) => Ok(Self::refs_of_known(info)),
            None => Err(GlueError::ExportRefsMissingMetadata { path: p.to_owned() }),
        })?;
        Ok(set)
    }

    /// The closure of `targets` (BFS over references), all of which must
    /// lie inside the build's input closure. Returned sorted (store-path
    /// order), which is also the order CppNix's `StorePathSet` iterates.
    ///
    /// All graph traversal is delegated to [`rio_nix::closure`]
    /// (cycle-safe by construction); this function owns only the
    /// CppNix-mirroring policy: target normalization, the input-closure
    /// containment gate, `.drv` output expansion, and the fail-closed
    /// cycle rejection.
    fn closure_of(&self, targets: &[String]) -> Result<BTreeSet<&'a str>, GlueError> {
        let mut set = self.reference_closure_of(targets)?;

        // CppNix (`LocalDerivationGoal::exportReferences`) post-processes
        // the closure: every `.drv` file in it is parsed and the closure
        // of each of its *outputs* is unioned in — the pattern used to
        // hand a build the full build-time dependency graph of something
        // (installer images, `closureInfo`-style registration sets). Two
        // details mirrored exactly:
        //   - the expansion runs over a snapshot of the closure, so a
        //     `.drv` that only appears inside an expanded output closure
        //     is not itself expanded (`ClosureSet::extend` additionally
        //     guarantees an already-visited path is never re-resolved);
        //   - an output without a statically-known path (content-
        //     addressed derivation) is an error, as in CppNix.
        // CppNix reads output closures from the global store DB; the
        // worker's view of the store is the input metadata index, so
        // every expanded path must be present there. Whenever the
        // deriving expression used `drvPath`-style context the resolver
        // has already pulled those outputs into the input set, so this
        // is the common case rather than an extra requirement.
        // TODO: F3 (round-15 C4 follow-up) — demand derivation keys on
        // the `.drv` name suffix; a source path named `*.drv` in a graph
        // closure is treated as derivation text (parse fails downstream).
        // Part of the suffix-keyed family F3 unifies on a typed PathKind.
        // See store.put.drv-text-ca+3.
        let drvs: Vec<&'a str> = set.members().filter(|p| p.ends_with(".drv")).collect();
        for drv_path in drvs {
            for (output, out_path) in self.drv_outputs(drv_path)? {
                if out_path.is_empty() {
                    return Err(GlueError::ExportRefsDrvFloatingOutput {
                        drv: drv_path.to_owned(),
                        output,
                    });
                }
                // The declared output and its whole closure must be known
                // to the index. Unlike the requested graph targets, they
                // are not required to be inside the *input closure* (the
                // registration file may name paths the sandbox cannot
                // read — same as CppNix).
                let Some(out_info) = self.by_path.get(out_path.as_str()) else {
                    return Err(GlueError::ExportRefsDrvOutputMissing {
                        drv: drv_path.to_owned(),
                        path: out_path.clone(),
                    });
                };
                set.extend([out_info.store_path.as_str()], |p| {
                    match self.by_path.get(p) {
                        Some(info) => Ok(Self::refs_of_known(info)),
                        None => Err(GlueError::ExportRefsDrvOutputMissing {
                            drv: drv_path.to_owned(),
                            path: p.to_owned(),
                        }),
                    }
                })?;
            }
        }

        // Fail-closed cycle gate, covering BOTH render forms (the flat
        // registration text and the structured-attrs JSON call through
        // here). Cyclic reference metadata has no defined CppNix
        // equivalent — the oracle's local store cannot represent it — so
        // reject it as the tenant-visible input error rather than emit
        // bytes no Nix toolchain could have produced (or hang computing
        // them, which is what the pre-rio_nix::closure implementation
        // did).
        // r[impl builder.exec.refs-graph-acyclic]
        let cyclic = rio_nix::closure::find_cycle(set.members(), |p| {
            self.by_path
                .get(p)
                .into_iter()
                .flat_map(|info| Self::refs_of_known(info))
        });
        if !cyclic.is_empty() {
            return Err(GlueError::ExportRefsCyclicMetadata {
                paths: cyclic.iter().map(|p| (*p).to_owned()).collect(),
            });
        }

        Ok(set.members().collect())
    }

    /// Read and parse a `.drv` from the materialized input store and
    /// return its `(output name, declared store path)` pairs (the
    /// declared path is empty for floating content-addressed outputs).
    fn drv_outputs(&self, drv_store_path: &str) -> Result<Vec<(String, String)>, GlueError> {
        let unreadable = |reason: String| GlueError::ExportRefsDrvUnreadable {
            path: drv_store_path.to_owned(),
            reason,
        };
        let Some(table) = self.drv_table else {
            return Err(unreadable(
                "no derivation table is available to this caller".to_owned(),
            ));
        };
        // A miss is defensively unreachable in production: the table is
        // assembled from the SAME closure this index walks (every .drv
        // member's text is retained or prefetched at input resolution),
        // so a missing entry means the caller's table and closure
        // diverged — a structural fault of the inputs, never transient.
        let Some(text) = table.get(drv_store_path) else {
            return Err(unreadable(
                "derivation text missing from the resolved input table".to_owned(),
            ));
        };
        let drv = Derivation::parse(text).map_err(|e| unreadable(format!("parsing: {e}")))?;
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

    /// The structured-attrs JSON form: an array of path-info objects for
    /// the closure of `targets`, sorted by path. Each element carries
    /// exactly the fields CppNix's `Store::pathInfoToJSON(...,
    /// includeImpureInfo = false, showClosureSize = true)` emits when
    /// `writeStructuredAttrs` expands `exportReferencesGraph`:
    /// `closureSize`, `narHash` (Nix colon format `sha256:<nixbase32>`,
    /// NOT SRI), `narSize`, `path`, `references` (sorted, including any
    /// self-reference recorded in the path's metadata), `"valid": true`,
    /// and — only for content-addressed paths — `ca` with the
    /// renderContentAddress string carried by the path's metadata
    /// (non-CA paths omit the key). Verified against Nix 2.34.7 output
    /// and pinned byte-for-byte by the `erg-structured`
    /// differential-corpus entry; key order inside `.attrs.json` is
    /// normalized by `sort_json_keys` in `attrs.rs` to match nlohmann's
    /// sorted maps.
    pub(crate) fn closure_info_json(&self, targets: &[String]) -> Result<Value, GlueError> {
        let closure = self.closure_of(targets)?;
        // Per-member closure sizes, computed by rio_nix::closure with one
        // reusable scratch set: closureInfo-style graphs (NixOS images)
        // have thousands of members whose closures overlap almost
        // entirely, and holding a memoized closure SET per member is
        // O(N²) memory. Paths outside the index contribute size 0 and no
        // references (cannot happen for closure_of output, but keeps the
        // sum defined).
        let sizes = rio_nix::closure::closure_sizes(
            closure.iter().copied(),
            |p| {
                self.by_path
                    .get(p)
                    .into_iter()
                    .flat_map(|info| Self::refs_of_known(info))
            },
            |p| self.by_path.get(p).map_or(0, |info| info.nar_size),
        );
        let arr: Vec<Value> = closure
            .iter()
            .map(|p| {
                let info = self.by_path[p];
                let mut refs: Vec<&str> = info.references.iter().map(|r| r.as_str()).collect();
                refs.sort_unstable();
                let nar_hash = NixHash::new(HashAlgo::SHA256, info.nar_hash.to_vec())
                    .expect("32-byte sha256 digest is always valid");
                let mut el = serde_json::json!({
                    "path": p,
                    // CppNix renders narHash in colon/nixbase32 form here
                    // (pathInfoToJSON), not SRI.
                    "narHash": nar_hash.to_colon(),
                    "narSize": info.nar_size,
                    "references": refs,
                    "closureSize": sizes[p],
                    // pathInfoToJSON marks every existing path as valid;
                    // every closure member here exists by construction.
                    "valid": true,
                });
                // pathInfoToJSON adds the optional `ca` key only when the
                // path is content-addressed, with the renderContentAddress
                // string (`fixed:[r:]<algo>:<nixbase32>` / `text:…`) —
                // exactly the descriptor the store metadata already
                // carries, so it passes through verbatim. Non-CA paths
                // omit the key entirely (verified against Nix 2.34.7
                // `.attrs.json` output for a flat-added file alongside an
                // input-addressed path).
                if let Some(ca) = &info.content_address {
                    el["ca"] = Value::String(ca.clone());
                }
                el
            })
            .collect();
        Ok(Value::Array(arr))
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

/// The set of `.drv` texts the request glue's `exportReferencesGraph`
/// expansion will read for THIS build — derived from the build's own
/// declaration, which is the only demand origin (the glue's `.drv`
/// expansion only ever parses `.drv` members of a declared graph's
/// reference closure; see [`ClosureIndex::closure_of`]).
///
/// [`DrvTextDemand::from_declaration`] is the ONLY constructor: a
/// resolver consuming this type structurally cannot fetch derivation
/// texts the glue has no reader for, and "no declaration ⇒ zero
/// graph-purpose fetches at any closure depth" is a type fact, not a
/// heuristic.
// r[impl builder.glue.drv-table-demand]
#[derive(Debug)]
pub(crate) struct DrvTextDemand {
    paths: BTreeSet<String>,
}

impl DrvTextDemand {
    /// Derive the demand from the derivation's declaration (both forms:
    /// the flat `exportReferencesGraph` env value and the
    /// structured-attrs `__json` object), walking each declared graph
    /// with the SAME `reference_closure_of` the glue's expansion uses.
    ///
    /// Per-graph failures contribute NO demand, deliberately: a
    /// malformed declaration, an out-of-closure target, or missing
    /// metadata is the GLUE's rejection to make, with its canonical
    /// `GlueError` — the demand model neither duplicates that error
    /// nor pre-empts it with a resolver-flavored one. The build still
    /// fails identically; it just fails in the component that owns the
    /// message. (A demand miss can also never manifest as a missing
    /// table entry at expansion time for a VALID graph: validity
    /// implies the same walk succeeds here.)
    pub(crate) fn from_declaration(
        drv: &impl rio_nix::derivation::DerivationLike,
        index: &ClosureIndex<'_>,
    ) -> Self {
        let mut paths = BTreeSet::new();

        // Graph target lists, by declaration form. Mirrors the glue's
        // dispatch in `prepare_request`: structured-attrs builds read
        // the `__json` object's `exportReferencesGraph` (values
        // flattened recursively, oracle `flatten`), flat builds read
        // the alternating `name path` env value.
        let mut target_lists: Vec<Vec<String>> = Vec::new();
        if let Some(json_text) = drv.env().get("__json") {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(json_text)
                && let Some(Value::Object(erg)) = map.get("exportReferencesGraph")
            {
                for val in erg.values() {
                    let mut set = std::collections::BTreeSet::new();
                    if rio_nix::derivation::typed::flatten_strings(val, &mut set).is_ok() {
                        target_lists.push(set.into_iter().collect());
                    }
                }
            }
        } else if let Some(flat) = drv.env().get("exportReferencesGraph")
            && let Ok(pairs) = parse_flat_export_refs(flat)
        {
            for (_, target) in pairs {
                target_lists.push(vec![target]);
            }
        }

        for targets in target_lists {
            if let Ok(set) = index.reference_closure_of(&targets) {
                paths.extend(
                    set.members()
                        .filter(|p| p.ends_with(".drv"))
                        .map(str::to_owned),
                );
            }
        }

        Self { paths }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().map(String::as_str)
    }

    /// Test-only seam for loop-level consumers (`inputs.rs` wiring
    /// tests): production code has exactly one constructor,
    /// [`from_declaration`](Self::from_declaration) — the demand
    /// derivation itself is pinned by this module's tests.
    #[cfg(test)]
    pub(crate) fn from_paths_for_tests(paths: impl IntoIterator<Item = String>) -> Self {
        Self {
            paths: paths.into_iter().collect(),
        }
    }
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
    // Well-formed store path (valid nixbase32 hash) that is simply not a
    // member of the fixture's input closure.
    const OUTSIDE: &str = "/nix/store/wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww-outside";

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

    /// CppNix `toStorePath()` semantics: a target naming a path *inside*
    /// a store path exports the closure of the containing store path.
    #[test]
    fn sub_store_path_target_normalizes_to_its_root() {
        let (infos, paths) = fixture();
        let index = ClosureIndex::new(&infos, &paths);
        let direct = index.registration_text(&[B.to_string()]).unwrap();
        let via_sub = index.registration_text(&[format!("{B}/bin/tool")]).unwrap();
        assert_eq!(
            via_sub, direct,
            "a sub-path target must export exactly its containing store path's closure"
        );
        // The structured form goes through the same normalization.
        let json = index
            .closure_info_json(&[format!("{B}/share/doc/readme")])
            .unwrap();
        assert_eq!(json, index.closure_info_json(&[B.to_string()]).unwrap());
    }

    /// Pins the closure-info element format byte-for-byte against what
    /// Nix 2.34.7's `writeStructuredAttrs` produced for the
    /// `erg-structured` differential-corpus entry (static busybox with a
    /// recorded self-reference): colon/nixbase32 `narHash`, `valid: true`,
    /// `closureSize`, and the self-reference kept in `references`.
    #[test]
    fn closure_info_matches_the_cppnix_oracle_element() {
        const BUSYBOX: &str = "/nix/store/y7fhmxcdbfyslfgkclgf4263wy6bhp3j-busybox-static-x86_64-unknown-linux-musl-1.37.0";
        // sha256 NAR hash of that path, as raw bytes (oracle rendered it
        // as sha256:0azg5qlpqyf49b29ffdqabgrwhbn4k9blsanb8h7yizwwk52b9cc).
        let digest: [u8; 32] = [
            0x8c, 0xa5, 0x25, 0xca, 0xe4, 0xfc, 0x47, 0x7f, 0x20, 0x5a, 0x56, 0x69, 0xba, 0xd2,
            0x24, 0x76, 0x41, 0x9e, 0xdf, 0x52, 0xb8, 0x39, 0x97, 0xc4, 0x4a, 0xc4, 0x79, 0x7c,
            0x29, 0x2e, 0xef, 0x2b,
        ];
        let mut bb = info(BUSYBOX, 1_495_048, &[BUSYBOX]);
        bb.nar_hash = digest;
        let infos = vec![bb];
        let paths = vec![BUSYBOX.to_string()];
        let index = ClosureIndex::new(&infos, &paths);
        let json = index.closure_info_json(&[BUSYBOX.to_string()]).unwrap();
        let expected = serde_json::json!([{
            "closureSize": 1_495_048u64,
            "narHash": "sha256:0azg5qlpqyf49b29ffdqabgrwhbn4k9blsanb8h7yizwwk52b9cc",
            "narSize": 1_495_048u64,
            "path": BUSYBOX,
            "references": [BUSYBOX],
            "valid": true,
        }]);
        assert_eq!(json, expected);
    }

    /// pathInfoToJSON adds the optional `ca` key only for
    /// content-addressed paths, passing the renderContentAddress string
    /// through verbatim; non-CA paths omit the key entirely (not
    /// `null`). Verified against Nix 2.34.7 `.attrs.json` output for a
    /// flat-added file alongside an input-addressed path.
    #[test]
    fn closure_info_emits_ca_only_for_content_addressed_paths() {
        const CA_DESC: &str = "fixed:sha256:08b59by0b0ga7j2yfhl64hx6alsd7cvcwkllvxixhj3fws5kx1zw";
        let mut ca_info = info(A, 144, &[]);
        ca_info.content_address = Some(CA_DESC.to_string());
        let plain = info(B, 10, &[]);
        let infos = vec![ca_info, plain];
        let paths = vec![A.to_string(), B.to_string()];
        let index = ClosureIndex::new(&infos, &paths);
        let json = index
            .closure_info_json(&[A.to_string(), B.to_string()])
            .unwrap();
        let arr = json.as_array().unwrap();
        let by_path: std::collections::HashMap<&str, &Value> = arr
            .iter()
            .map(|el| (el["path"].as_str().unwrap(), el))
            .collect();
        assert_eq!(by_path[A]["ca"], Value::String(CA_DESC.to_string()));
        assert!(
            by_path[B].as_object().unwrap().get("ca").is_none(),
            "non-CA path must omit the ca key entirely (not null)"
        );
    }

    /// …and anything not under the store dir at all is rejected the way
    /// CppNix's `toStorePath` rejects it, before the closure walk.
    #[test]
    fn target_outside_the_store_is_rejected() {
        let (infos, paths) = fixture();
        let index = ClosureIndex::new(&infos, &paths);
        for bad in [
            "/etc/passwd",
            "relative/path",
            "/nix/store/not-a-valid-name",
        ] {
            let err = index.registration_text(&[bad.to_string()]).unwrap_err();
            assert!(
                matches!(&err, GlueError::ExportRefsNotAStorePath { path } if path == bad),
                "{bad}: expected ExportRefsNotAStorePath, got {err:?}"
            );
        }
    }

    const DRV: &str = "/nix/store/dddddddddddddddddddddddddddddddd-probe.drv";
    const DOUT: &str = "/nix/store/ffffffffffffffffffffffffffffffff-probe-out";
    const DRV2: &str = "/nix/store/gggggggggggggggggggggggggggggggg-inner.drv";
    const OUT2: &str = "/nix/store/hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh-inner-out";
    const FDRV: &str = "/nix/store/jjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjj-fetch.drv";
    const FOUT: &str = "/nix/store/kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk-fetched";

    /// Build a minimal input-addressed `.drv` (ATerm) table entry,
    /// declaring the given `(name, path, hashAlgo, hash)` outputs:
    /// empty hash fields = input-addressed, empty path + hashAlgo =
    /// floating CA, path + hash fields = fixed-output.
    fn drv_entry(
        table: &mut std::collections::BTreeMap<String, String>,
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
        table.insert(drv_store_path.to_owned(), aterm);
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
        let mut table = std::collections::BTreeMap::new();
        drv_entry(&mut table, DRV, &[("out", DOUT, "", "")]);
        let index = ClosureIndex::new(&infos, &paths).with_drv_table(&table);

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
        let mut table = std::collections::BTreeMap::new();
        // Floating-CA output: empty declared path, hash algo set.
        drv_entry(&mut table, DRV, &[("out", "", "r:sha256", "")]);
        let index = ClosureIndex::new(&infos, &paths).with_drv_table(&table);
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
        let mut table = std::collections::BTreeMap::new();
        drv_entry(&mut table, DRV, &[("out", DOUT, "", "")]);
        drv_entry(&mut table, DRV2, &[("out", OUT2, "", "")]);
        let index = ClosureIndex::new(&infos, &paths).with_drv_table(&table);

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
        let mut table = std::collections::BTreeMap::new();
        drv_entry(
            &mut table,
            FDRV,
            &[(
                "out",
                FOUT,
                "r:sha256",
                "1111111111111111111111111111111111111111111111111111111111111111",
            )],
        );
        let index = ClosureIndex::new(&infos, &paths).with_drv_table(&table);

        let text = String::from_utf8(index.registration_text(&[A.to_string()]).unwrap()).unwrap();
        assert!(text.contains(FOUT), "{text}");
    }

    #[test]
    fn drv_output_without_metadata_is_rejected() {
        // DRV declares DOUT but the index has no metadata for it.
        let infos = vec![info(A, 10, &[DRV]), info(DRV, 5, &[])];
        let paths = vec![A.to_string(), DRV.to_string()];
        let mut table = std::collections::BTreeMap::new();
        drv_entry(&mut table, DRV, &[("out", DOUT, "", "")]);
        let index = ClosureIndex::new(&infos, &paths).with_drv_table(&table);
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

        // No derivation table at all: structural — the caller cannot
        // expand .drvs here. Permanent.
        let index = ClosureIndex::new(&infos, &paths);
        let err = index.registration_text(&[A.to_string()]).unwrap_err();
        assert!(
            matches!(err, GlueError::ExportRefsDrvUnreadable { .. }),
            "{err}"
        );

        // Table present but the .drv text is unparseable: a property
        // of the derivation, not of this worker. Permanent.
        let mut table = std::collections::BTreeMap::new();
        table.insert(DRV.to_owned(), "not an aterm derivation".to_owned());
        let index = ClosureIndex::new(&infos, &paths).with_drv_table(&table);
        let err = index.registration_text(&[A.to_string()]).unwrap_err();
        assert!(
            matches!(err, GlueError::ExportRefsDrvUnreadable { .. }),
            "{err}"
        );
    }

    #[test]
    fn drv_text_missing_from_table_is_rejected() {
        // The closure names DRV but the supplied table has no entry for
        // it. Defensively unreachable in production (the table is
        // assembled from the same closure) — and structurally permanent:
        // by glue time every byte the glue could ask for has already
        // been resolved, so a miss is a fault of the inputs, never a
        // transient I/O condition (the glue performs no I/O at all).
        let infos = vec![info(A, 10, &[DRV]), info(DRV, 5, &[])];
        let paths = vec![A.to_string(), DRV.to_string()];
        let table = std::collections::BTreeMap::new();
        let index = ClosureIndex::new(&infos, &paths).with_drv_table(&table);
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

    /// The merged_bug_009 reproducer: mutually-referencing metadata
    /// (A↔B, NOT a self-reference) must be rejected by both render
    /// forms, naming the cyclic paths. On the pre-`rio_nix::closure`
    /// implementation this test does not fail — it HANGS (the
    /// closure-size memo recursed forever), which is why the cycle gate
    /// and this test land in the same commit.
    // r[verify builder.exec.refs-graph-acyclic]
    #[test]
    fn cyclic_reference_metadata_is_rejected_not_hung() {
        let infos = vec![info(A, 10, &[B]), info(B, 20, &[A])];
        let paths = vec![A.to_string(), B.to_string()];
        let index = ClosureIndex::new(&infos, &paths);

        let text_err = index.registration_text(&[A.to_string()]).unwrap_err();
        assert!(
            matches!(
                &text_err,
                GlueError::ExportRefsCyclicMetadata { paths }
                    if paths.contains(&A.to_string()) && paths.contains(&B.to_string())
            ),
            "registration_text: expected ExportRefsCyclicMetadata naming A and B, got {text_err}"
        );

        let json_err = index.closure_info_json(&[A.to_string()]).unwrap_err();
        assert!(
            matches!(&json_err, GlueError::ExportRefsCyclicMetadata { .. }),
            "closure_info_json: expected ExportRefsCyclicMetadata, got {json_err}"
        );
    }

    /// Cycle rejection MUST stay a permanent input rejection. If it ever
    /// becomes worker-transient, one hostile registration turns into an
    /// unbounded retry storm — the exact failure mode being fixed.
    // r[verify builder.exec.refs-graph-acyclic]
    #[test]
    fn cycle_rejection_is_permanent_not_transient() {
        let infos = vec![info(A, 10, &[B]), info(B, 20, &[A])];
        let paths = vec![A.to_string(), B.to_string()];
        let index = ClosureIndex::new(&infos, &paths);
        let err = index.registration_text(&[A.to_string()]).unwrap_err();
        // Permanence is structural now: GlueError carries no transient
        // class at all (`builder.glue.pure` deleted the transient query with
        // the glue's last I/O capability), so the cyclic rejection can
        // only ever map to InputRejected.
        assert!(matches!(err, GlueError::ExportRefsCyclicMetadata { .. }));
    }

    /// A cycle that is only reachable through a `.drv` output-closure
    /// expansion is rejected identically — the gate runs after the full
    /// expansion, not just over the directly-requested targets.
    // r[verify builder.exec.refs-graph-acyclic]
    #[test]
    fn cycle_via_drv_expansion_is_rejected() {
        // A → DRV; DRV's output DOUT → C; C ↔ OUT2 form the cycle.
        let infos = vec![
            info(A, 10, &[DRV]),
            info(DRV, 5, &[]),
            info(DOUT, 30, &[C]),
            info(C, 40, &[OUT2]),
            info(OUT2, 15, &[C]),
        ];
        let paths = vec![A.to_string(), DRV.to_string()];
        let mut table = std::collections::BTreeMap::new();
        drv_entry(&mut table, DRV, &[("out", DOUT, "", "")]);
        let index = ClosureIndex::new(&infos, &paths).with_drv_table(&table);

        let err = index.registration_text(&[A.to_string()]).unwrap_err();
        assert!(
            matches!(
                &err,
                GlueError::ExportRefsCyclicMetadata { paths }
                    if paths.contains(&C.to_string()) && paths.contains(&OUT2.to_string())
            ),
            "expected cycle naming C and OUT2, got {err}"
        );
    }

    /// Diamond-shaped closures (shared dependencies) are not cycles and
    /// must keep rendering — the false-positive trap for cycle gates.
    #[test]
    fn diamond_closure_is_not_misreported_as_cycle() {
        // A → {B, DOUT}, B → C, DOUT → C, C → (nothing but itself).
        let infos = vec![
            info(A, 10, &[B, DOUT]),
            info(B, 20, &[C]),
            info(DOUT, 30, &[C]),
            info(C, 40, &[C]),
        ];
        let paths = vec![
            A.to_string(),
            B.to_string(),
            DOUT.to_string(),
            C.to_string(),
        ];
        let index = ClosureIndex::new(&infos, &paths);
        let v = index.closure_info_json(&[A.to_string()]).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        // A's closure: 10 + 20 + 30 + 40; C counted once despite the
        // diamond and its self-reference.
        assert_eq!(arr[0]["closureSize"], serde_json::json!(100));
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

    // ───────────────────── DrvTextDemand (C9c1) ─────────────────────

    /// Helper: a BasicDerivation with the given env (the demand model
    /// only reads env + the shared walk).
    fn demand_drv(env: &[(&str, &str)]) -> rio_nix::derivation::BasicDerivation {
        rio_nix::derivation::BasicDerivation::new(
            vec![
                rio_nix::derivation::DerivationOutput::new(
                    "out",
                    "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-demand-out",
                    "",
                    "",
                )
                .unwrap(),
            ],
            std::collections::BTreeSet::new(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            env.iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap()
    }

    const D1: &str = "/nix/store/d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1-one.drv";
    const D2: &str = "/nix/store/d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2-two.drv";
    const D3: &str = "/nix/store/d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3d3-three.drv";

    /// A 3-deep .drv chain in the input closure: D1 → D2 → D3, with a
    /// plain source S hanging off D3.
    fn drv_chain_fixture() -> (Vec<ValidatedPathInfo>, Vec<String>) {
        let infos = vec![
            info(D1, 10, &[D2]),
            info(D2, 10, &[D3]),
            info(D3, 10, &[C]),
            info(C, 40, &[]),
        ];
        let paths = vec![
            D1.to_string(),
            D2.to_string(),
            D3.to_string(),
            C.to_string(),
        ];
        (infos, paths)
    }

    /// "No declaration ⇒ zero graph-purpose fetches at any depth" is a
    /// type fact: with a 3-deep .drv chain in the closure and NO
    /// exportReferencesGraph declaration, the demand is empty.
    // r[verify builder.glue.drv-table-demand]
    #[test]
    fn demand_empty_without_declaration_at_depth_3() {
        let (infos, paths) = drv_chain_fixture();
        let index = ClosureIndex::new(&infos, &paths);
        let drv = demand_drv(&[("name", "x")]);
        let demand = DrvTextDemand::from_declaration(&drv, &index);
        assert!(demand.is_empty(), "no declaration, no demand: {demand:?}");
    }

    /// A .drv graph target demands exactly its transitive .drv closure
    /// — every member the glue's expansion will parse, nothing else
    /// (the plain source C is in the walk but is not a .drv).
    // r[verify builder.glue.drv-table-demand]
    #[test]
    fn drv_target_demands_its_transitive_drv_closure_exactly() {
        let (infos, paths) = drv_chain_fixture();
        let index = ClosureIndex::new(&infos, &paths);
        let drv = demand_drv(&[("exportReferencesGraph", &format!("graph {D1}"))]);
        let demand = DrvTextDemand::from_declaration(&drv, &index);
        let got: Vec<&str> = demand.iter().collect();
        assert_eq!(got, vec![D1, D2, D3], "exact transitive .drv closure");
        assert!(!demand.contains(C), "non-.drv members are not demanded");
    }

    /// CppNix snapshot lockstep: a .drv that appears only inside an
    /// expanded OUTPUT closure is not itself expanded by the glue
    /// (the expansion runs over a snapshot), so the demand model must
    /// not demand its text either — producer and consumer agree
    /// because they share the same phase-1 walk.
    // r[verify builder.glue.drv-table-demand]
    #[test]
    fn output_expansion_drvs_are_not_demanded() {
        // Graph target D1 (a .drv) whose OUTPUT's closure contains D2.
        // Phase 1 reaches only D1 + its refs (none). The output
        // closure (B → D2) is expansion territory: D2's text is read
        // never — the snapshot rule — so it must not be demanded.
        let out_path = B;
        let infos = vec![
            info(D1, 10, &[]),
            info(out_path, 20, &[D2]),
            info(D2, 10, &[]),
        ];
        let paths = vec![D1.to_string(), out_path.to_string(), D2.to_string()];
        let index = ClosureIndex::new(&infos, &paths);
        let drv = demand_drv(&[("exportReferencesGraph", &format!("graph {D1}"))]);
        let demand = DrvTextDemand::from_declaration(&drv, &index);
        let got: Vec<&str> = demand.iter().collect();
        assert_eq!(got, vec![D1], "only the phase-1 .drv member");
        assert!(
            !demand.contains(D2),
            "output-closure .drvs are never parsed by the glue (snapshot \
             rule) and therefore never demanded"
        );
    }

    /// The structured-attrs and flat declarations of the same graphs
    /// produce identical demand.
    // r[verify builder.glue.drv-table-demand]
    #[test]
    fn structured_and_flat_declarations_demand_identically() {
        let (infos, paths) = drv_chain_fixture();
        let index = ClosureIndex::new(&infos, &paths);

        let flat = demand_drv(&[("exportReferencesGraph", &format!("g {D2}"))]);
        let flat_demand = DrvTextDemand::from_declaration(&flat, &index);

        let json = format!(r#"{{"exportReferencesGraph":{{"g":["{D2}"]}}}}"#);
        let structured = demand_drv(&[("__json", &json)]);
        let structured_demand = DrvTextDemand::from_declaration(&structured, &index);

        let f: Vec<&str> = flat_demand.iter().collect();
        let s: Vec<&str> = structured_demand.iter().collect();
        assert_eq!(f, s, "both forms walk the same closure");
        assert_eq!(f, vec![D2, D3]);
    }

    /// Canonical-error ordering: a malformed declaration (or an
    /// out-of-closure target) contributes NO demand — and the GLUE
    /// still rejects with its canonical error when it runs. The demand
    /// model never pre-empts the glue's message with a resolver-
    /// flavored one.
    // r[verify builder.glue.drv-table-demand]
    #[test]
    fn invalid_graphs_contribute_no_demand_and_glue_owns_the_error() {
        let (infos, paths) = drv_chain_fixture();
        let index = ClosureIndex::new(&infos, &paths);

        // Malformed flat value (odd word count).
        let malformed = demand_drv(&[("exportReferencesGraph", "lonely-name")]);
        assert!(DrvTextDemand::from_declaration(&malformed, &index).is_empty());
        let glue_err = parse_flat_export_refs("lonely-name").unwrap_err();
        assert!(matches!(glue_err, GlueError::ExportRefsMalformed { .. }));

        // Out-of-closure target: no demand; the glue's walk raises the
        // canonical containment error.
        let outside = demand_drv(&[("exportReferencesGraph", &format!("g {OUTSIDE}"))]);
        assert!(DrvTextDemand::from_declaration(&outside, &index).is_empty());
        let glue_err = index.registration_text(&[OUTSIDE.to_string()]).unwrap_err();
        assert!(
            matches!(glue_err, GlueError::ExportRefsOutsideClosure { .. }),
            "{glue_err}"
        );
    }
}

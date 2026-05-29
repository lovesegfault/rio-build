//! Per-target dependency closures from `nix derivation show -r` output.
//!
//! The dep-closure.jsonl artifact records, for every manifest target,
//! the target's requisite closure in adjacency form: each dependency
//! derivation together with its statically-declared output paths. The
//! campaign runner needs that output-path → producing-derivation
//! mapping to decide which dependency outputs can be fetched up front
//! and which targets cannot be attempted at all. Enumeration is static
//! — one `nix derivation show -r` subprocess per target, never one per
//! dependency, and no builds.
//!
//! The same show output also feeds the replay archive's view of the
//! closure: [`closure_adjacency_from_show_json`] extracts one
//! direct-adjacency record per derivation (`inputs`/`srcs`/`outputs`,
//! the `closures.jsonl` shape) together with each derivation's declared
//! impure environment variables (the `impure-env.json` member) and its
//! `requiredSystemFeatures`.

use std::collections::BTreeMap;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use crate::archive::schema::{ClosureRecord, ImpureEnv};

/// dep-closure.jsonl record (adjacency form):
/// `{job, drvPath, deps: [{drvPath, outputPaths[]}], caOutputs[]}` —
/// `deps` has one entry per drv in the target's requisite closure
/// (the target itself excluded), each with its statically-declared
/// output paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepClosureRecord {
    pub job: String,
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    pub deps: Vec<DepDrv>,
    /// Content-addressed (floating) outputs with no statically-declared
    /// output path. Listed separately so consumers can tell "this
    /// output cannot be resolved to a store path before it is built"
    /// apart from "this dependency has no outputs".
    #[serde(rename = "caOutputs", default, skip_serializing_if = "Vec::is_empty")]
    pub ca_outputs: Vec<CaOutput>,
}

/// One dependency derivation and its declared output paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepDrv {
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    #[serde(rename = "outputPaths")]
    pub output_paths: Vec<String>,
}

/// One output of a dependency derivation that has no static store path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaOutput {
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    pub output: String,
}

/// The slice of one `nix derivation show` entry this module consumes;
/// unknown fields (`name`, `system`, `args`, …) are ignored.
#[derive(Debug, Deserialize)]
struct ShowDrv {
    #[serde(default)]
    outputs: BTreeMap<String, ShowOutputEntry>,
    #[serde(default)]
    env: BTreeMap<String, serde_json::Value>,
    /// Nested input form emitted by nix ≥ 2.32: `inputs.drvs` /
    /// `inputs.srcs`.
    #[serde(default)]
    inputs: ShowInputs,
    /// Legacy flat spelling of the direct input derivations (older nix
    /// emits it at the top level instead of under `inputs`).
    #[serde(default, rename = "inputDrvs")]
    input_drvs: BTreeMap<String, serde_json::Value>,
    /// Legacy flat spelling of the direct input sources.
    #[serde(default, rename = "inputSrcs")]
    input_srcs: Vec<String>,
}

/// The nested `inputs` object of a `nix derivation show` entry
/// (nix ≥ 2.32): direct input derivations keyed by drv path — the
/// per-input requested-output lists are not needed here — plus direct
/// non-drv input sources.
#[derive(Debug, Default, Deserialize)]
struct ShowInputs {
    #[serde(default)]
    drvs: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    srcs: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ShowOutputEntry {
    path: Option<String>,
}

/// Prefix a bare store-path basename with `/nix/store/`. Assumes the
/// standard store dir: paths from a relocated store (`--store
/// /other/root`) would come out wrong, which is acceptable because the
/// eval-set builder always evaluates against the standard store.
fn normalize_store_path(p: &str) -> String {
    if p.starts_with("/nix/store/") {
        p.to_string()
    } else {
        format!("/nix/store/{p}")
    }
}

/// Resolve one output of one drv to a static store path:
/// 1. `outputs.<name>.path` (input-addressed drvs; basenames normalized);
/// 2. else `env.<name>` when it is a real `/nix/store/` path
///    (fixed-output drvs: nix 2.34's JSON puts hash/method in `outputs`
///    and the real path only in the builder env);
/// 3. else None — floating CA output.
///
/// Known mis-fire: `__structuredAttrs` fixed-output drvs have no
/// per-output `env` key, so their outputs degrade to `caOutputs` (and
/// the target-level `requiredSystemFeatures` extraction returns None
/// for structuredAttrs targets). Both degrade conservatively — an
/// output is reported as not statically resolvable rather than
/// resolved to a wrong path — and structuredAttrs derivations are rare
/// in the Hydra jobsets this targets. The `/nix/store/` prefix check
/// shares `normalize_store_path`'s standard-store-dir assumption.
fn output_path(drv: &ShowDrv, output_name: &str) -> Option<String> {
    if let Some(p) = drv.outputs.get(output_name).and_then(|o| o.path.as_deref()) {
        return Some(normalize_store_path(p));
    }
    match drv.env.get(output_name) {
        Some(serde_json::Value::String(s)) if s.starts_with("/nix/store/") => Some(s.clone()),
        _ => None,
    }
}

/// Parse `nix derivation show -r` JSON into a full-path-keyed drv map.
/// nix ≥ 2.32 wraps the map as `{"version": …, "derivations": {…}}`;
/// older versions emit the map at the top level. Two-step parse: read
/// the document as a `serde_json::Value`, take `derivations` when
/// present (else treat the whole object as the map), then deserialize
/// each entry — unrelated top-level keys (`version`, future additions)
/// never break parsing. Entries are deserialized straight from the
/// borrowed `Value` (no per-entry clone): each entry carries a
/// multi-kilobyte `env` map and a closure can hold 10^5+ entries.
fn parse_show_drvs(show_json: &str) -> anyhow::Result<BTreeMap<String, ShowDrv>> {
    let value: serde_json::Value =
        serde_json::from_str(show_json).context("parse `nix derivation show -r` JSON")?;
    let map_value = value.get("derivations").unwrap_or(&value);
    let map = map_value
        .as_object()
        .context("`nix derivation show -r` output is not a JSON object")?;
    let mut drvs = BTreeMap::new();
    for (key, drv_value) in map {
        let drv = ShowDrv::deserialize(drv_value)
            .with_context(|| format!("parse derivation show entry {key}"))?;
        // Keys may be basenames (nix 2.34) or full paths (older nix).
        drvs.insert(normalize_store_path(key), drv);
    }
    Ok(drvs)
}

/// Build the dep-closure record for `target_drv` from a
/// `nix derivation show -r <target_drv>` JSON blob. Also returns the
/// target's `requiredSystemFeatures` so the caller can backfill the
/// manifest record's `requiredFeatures`. `target_drv` may be spelled
/// as a full `/nix/store/…` path or a bare basename; the emitted
/// record always carries the full path.
pub fn dep_closure_from_show_json(
    show_json: &str,
    target_drv: &str,
    job: &str,
) -> anyhow::Result<(DepClosureRecord, Option<Vec<String>>)> {
    // Normalize the target to the same canonical form as the map keys
    // so lookup and the self-exclusion below work for either spelling.
    let target_drv = normalize_store_path(target_drv);
    let normalized = parse_show_drvs(show_json)?;
    let target = normalized
        .get(&target_drv)
        .with_context(|| format!("target {target_drv} not present in derivation show output"))?;

    let required_features = target
        .env
        .get("requiredSystemFeatures")
        .and_then(|v| match v {
            serde_json::Value::String(s) if !s.trim().is_empty() => {
                Some(s.split_whitespace().map(String::from).collect::<Vec<_>>())
            }
            _ => None,
        });

    let mut deps = Vec::new();
    let mut ca_outputs = Vec::new();
    // BTreeMap iteration ⇒ deps come out sorted by drvPath
    // (deterministic artifacts).
    for (drv_path, drv) in &normalized {
        if *drv_path == target_drv {
            continue;
        }
        let mut output_paths = Vec::new();
        for output_name in drv.outputs.keys() {
            match output_path(drv, output_name) {
                Some(p) => output_paths.push(p),
                None => ca_outputs.push(CaOutput {
                    drv_path: drv_path.clone(),
                    output: output_name.clone(),
                }),
            }
        }
        output_paths.sort();
        output_paths.dedup();
        deps.push(DepDrv {
            drv_path: drv_path.clone(),
            output_paths,
        });
    }

    Ok((
        DepClosureRecord {
            job: job.to_string(),
            drv_path: target_drv,
            deps,
            ca_outputs,
        },
        required_features,
    ))
}

/// Everything the recorder extracts from one `nix derivation show -r`
/// blob for the replay archive: direct dependency adjacency for every
/// derivation in the output, plus the impure-environment and
/// required-system-feature declarations found along the way. All three
/// maps are keyed by full drv store path, so per-target results merge
/// key-by-key into one union view — a derivation reached from several
/// targets yields identical entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClosureAdjacency {
    /// One direct-adjacency record (the `closures.jsonl` shape) per
    /// derivation in the show output — workload targets included —
    /// keyed by the derivation's full `/nix/store/` path.
    pub records: BTreeMap<String, ClosureRecord>,
    /// Derivation store path → the impure environment variable names it
    /// declares via `env.impureEnvVars`, sorted and deduplicated. Only
    /// derivations declaring at least one variable are present.
    pub impure_env: ImpureEnv,
    /// Derivation store path → its declared `requiredSystemFeatures`,
    /// in declaration order. Only derivations declaring at least one
    /// feature are present. Collected here so callers can recover a
    /// unit's required features without re-parsing the show output.
    pub required_system_features: BTreeMap<String, Vec<String>>,
}

/// Extract the replay archive's closure view from a
/// `nix derivation show -r` JSON blob: one direct-adjacency record per
/// derivation in the output (the target itself included), plus per-drv
/// impure-environment and required-system-feature declarations.
///
/// Direct inputs and sources come from the nested `inputs.drvs` /
/// `inputs.srcs` form nix ≥ 2.32 emits, falling back to the legacy
/// top-level `inputDrvs` / `inputSrcs` spellings when the nested form
/// is empty; either way they are normalized to full `/nix/store/`
/// paths, sorted, and deduplicated. Outputs resolve through the same
/// rules as the per-target closure records: statically-declared and
/// env-recovered fixed-output paths become `Some`, floating
/// content-addressed outputs `None`.
pub fn closure_adjacency_from_show_json(show_json: &str) -> anyhow::Result<ClosureAdjacency> {
    let drvs = parse_show_drvs(show_json)?;
    let mut adjacency = ClosureAdjacency::default();
    for (drv_path, drv) in &drvs {
        // Effective direct inputs/sources: the nested form when it has
        // content, else the legacy flat spelling. Entries may be bare
        // basenames (nix 2.34) or full paths (older nix); both
        // normalize to full /nix/store/ paths.
        let input_drvs = if drv.inputs.drvs.is_empty() {
            &drv.input_drvs
        } else {
            &drv.inputs.drvs
        };
        let mut inputs: Vec<String> = input_drvs
            .keys()
            .map(|key| normalize_store_path(key))
            .collect();
        inputs.sort();
        inputs.dedup();
        let input_srcs = if drv.inputs.srcs.is_empty() {
            &drv.input_srcs
        } else {
            &drv.inputs.srcs
        };
        let mut srcs: Vec<String> = input_srcs
            .iter()
            .map(|src| normalize_store_path(src))
            .collect();
        srcs.sort();
        srcs.dedup();
        let outputs: BTreeMap<String, Option<String>> = drv
            .outputs
            .keys()
            .map(|name| (name.clone(), output_path(drv, name)))
            .collect();
        adjacency.records.insert(
            drv_path.clone(),
            ClosureRecord {
                drv: drv_path.clone(),
                inputs,
                srcs,
                outputs,
            },
        );

        // env.impureEnvVars is an ASCII-whitespace-separated name list;
        // store it sorted and deduplicated, and only when non-empty.
        if let Some(serde_json::Value::String(raw)) = drv.env.get("impureEnvVars") {
            let mut names: Vec<String> = raw.split_ascii_whitespace().map(String::from).collect();
            names.sort();
            names.dedup();
            if !names.is_empty() {
                adjacency.impure_env.insert(drv_path.clone(), names);
            }
        }
        // env.requiredSystemFeatures gets the same whitespace-split
        // treatment but keeps declaration order (it backfills per-unit
        // metadata, not an archive member of its own).
        if let Some(serde_json::Value::String(raw)) = drv.env.get("requiredSystemFeatures") {
            let features: Vec<String> = raw.split_ascii_whitespace().map(String::from).collect();
            if !features.is_empty() {
                adjacency
                    .required_system_features
                    .insert(drv_path.clone(), features);
            }
        }
    }
    Ok(adjacency)
}

/// Run `nix derivation show -r <drv>` and return its stdout. Requires
/// a `nix` binary and the drv (plus its closure) already in the local
/// store — i.e. run after the evaluation phase — so the offline unit
/// suite never calls it; it is exercised end-to-end when an eval set is
/// actually built.
pub async fn run_derivation_show(nix_bin: &str, drv_path: &str) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(nix_bin)
        .args([
            "--extra-experimental-features",
            "nix-command",
            "derivation",
            "show",
            "-r",
            drv_path,
        ])
        // Cancelling the caller (e.g. a failure elsewhere in the build)
        // drops this future; kill_on_drop keeps that from orphaning a
        // still-running nix process.
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawn {nix_bin} derivation show -r {drv_path}"))?;
    anyhow::ensure!(
        out.status.success(),
        "nix derivation show -r {drv_path} failed ({}): {}",
        out.status,
        crate::body_snippet(std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")),
    );
    String::from_utf8(out.stdout).context("derivation show output is not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Six real records extracted verbatim from a recorded
    /// `nix derivation show -r` run (nix 2.34): a target derivation,
    /// four input-addressed dependency derivations, and one
    /// fixed-output dependency, with the top-level `version` wrapper
    /// key preserved.
    fn fixture() -> String {
        std::fs::read_to_string(
            crate::test_manifest_dir().join("tests/fixtures/derivation-show/strncpy-subset.json"),
        )
        .unwrap()
    }

    const TARGET: &str = "/nix/store/00d529rs5cfj1kwz79sm79qackf9gppk-strncpy.drv";

    #[test]
    fn deps_list_every_dep_drv_with_outputs_excluding_the_target() {
        let (rec, features) =
            dep_closure_from_show_json(&fixture(), TARGET, "bootstrap.strncpy.x86_64-linux")
                .unwrap();
        assert_eq!(rec.job, "bootstrap.strncpy.x86_64-linux");
        assert_eq!(rec.drv_path, TARGET);
        // Adjacency form: one entry per dep drv in the requisite
        // closure, the target itself excluded.
        assert_eq!(rec.deps.len(), 5);
        assert!(
            !rec.deps.iter().any(|d| d.drv_path == TARGET),
            "the target drv must not appear in its own deps"
        );
        // Own output excluded everywhere.
        assert!(
            !rec.deps.iter().any(|d| d
                .output_paths
                .contains(&"/nix/store/wh601bizr79g0x5h9r7m06hcahr07skz-strncpy".to_string())),
            "target's own output must not appear in deps[].outputPaths"
        );
        // Each dep drv maps to its declared output: the four input drvs
        // (input-addressed: outputs.path, basename-normalized) and the
        // fixed-output fetchurl drv (recovered from env, since FOD
        // outputs carry hash/method instead of path in nix 2.34's JSON).
        let by_drv: std::collections::BTreeMap<&str, &DepDrv> =
            rec.deps.iter().map(|d| (d.drv_path.as_str(), d)).collect();
        for (dep_drv, expected_out) in [
            (
                "/nix/store/8y8aadripzcsxfvg4hsdacwcgw6ypm50-strncpy-builder.drv",
                "/nix/store/vrza83f5pwasbz4rl0773zzpmfqmsmy6-strncpy-builder",
            ),
            (
                "/nix/store/pz3m6g7iabylyazm40p363y8795vybf9-mescc-tools-1.9.1.drv",
                "/nix/store/53zd871rgpcvi7qmqkffxhzaiq0is7k4-mescc-tools-1.9.1",
            ),
            (
                "/nix/store/yjp7zwabw9xcm6g42736mbhlvcddw410-mescc-tools-extra-1.9.1.drv",
                "/nix/store/nf45b5s985mn73828jb6pfq0g85j0hvg-mescc-tools-extra-1.9.1",
            ),
            (
                "/nix/store/zlsfxdxsnzp1nzzw113avl2v0s5mgjpr-kaem-1.9.1.drv",
                "/nix/store/n3mfz1lj8rbz0cspkfrvxiy9h5hm0vzg-kaem-1.9.1",
            ),
            (
                "/nix/store/013mqc5ymx4cih72blz21l6ync49i3jg-expr-strcmp.patch.drv",
                "/nix/store/h24jwyqgava9fwnq8rxfk56n2gi25wgs-expr-strcmp.patch",
            ),
        ] {
            let dep = by_drv
                .get(dep_drv)
                .unwrap_or_else(|| panic!("missing dep entry for {dep_drv} in {:?}", rec.deps));
            assert_eq!(
                dep.output_paths,
                vec![expected_out.to_string()],
                "outputs of {dep_drv}"
            );
        }
        assert!(rec.ca_outputs.is_empty());
        // strncpy has no requiredSystemFeatures.
        assert_eq!(features, None);
        // Deterministic artifacts: deps sorted by drvPath.
        let drv_order: Vec<&str> = rec.deps.iter().map(|d| d.drv_path.as_str()).collect();
        let mut sorted = drv_order.clone();
        sorted.sort();
        assert_eq!(sorted, drv_order);
    }

    #[test]
    fn floating_ca_outputs_without_any_path_are_listed_separately() {
        // Synthetic: a dependency whose output has neither outputs.path
        // nor a /nix/store env value (true floating-CA shape).
        let json = r#"{
          "derivations": {
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-target.drv": {
              "outputs": {"out": {"path": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-target"}},
              "env": {"out": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-target"},
              "name": "target"
            },
            "cccccccccccccccccccccccccccccccc-floating.drv": {
              "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}},
              "env": {"out": "/0c6rn30q4frawknapgwq386zq9m8z4j"},
              "name": "floating"
            }
          }
        }"#;
        let (rec, _) = dep_closure_from_show_json(
            json,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-target.drv",
            "job",
        )
        .unwrap();
        // The floating dep is still part of the closure (one deps entry)
        // but contributes no static output path; the unresolvable output
        // is listed under caOutputs instead.
        assert_eq!(rec.deps.len(), 1);
        assert_eq!(
            rec.deps[0].drv_path,
            "/nix/store/cccccccccccccccccccccccccccccccc-floating.drv"
        );
        assert!(rec.deps[0].output_paths.is_empty());
        assert_eq!(rec.ca_outputs.len(), 1);
        assert_eq!(
            rec.ca_outputs[0].drv_path,
            "/nix/store/cccccccccccccccccccccccccccccccc-floating.drv"
        );
        assert_eq!(rec.ca_outputs[0].output, "out");
    }

    #[test]
    fn wrapped_output_with_version_key_is_parsed() {
        // nix ≥ 2.32 wraps the map and adds a top-level "version" key
        // (the committed strncpy fixture preserves it); the parser must
        // tolerate it and still find the map under "derivations".
        let json = r#"{
          "version": 4,
          "derivations": {
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-target.drv": {
              "outputs": {"out": {"path": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-target"}},
              "env": {"out": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-target"},
              "name": "target"
            },
            "dddddddddddddddddddddddddddddddd-dep.drv": {
              "outputs": {"out": {"path": "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-dep"}},
              "env": {"out": "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-dep"},
              "name": "dep"
            }
          }
        }"#;
        let (rec, _) = dep_closure_from_show_json(
            json,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-target.drv",
            "job",
        )
        .unwrap();
        assert_eq!(rec.deps.len(), 1);
        assert_eq!(
            rec.deps[0].drv_path,
            "/nix/store/dddddddddddddddddddddddddddddddd-dep.drv"
        );
        assert_eq!(
            rec.deps[0].output_paths,
            vec!["/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-dep".to_string()]
        );
    }

    #[test]
    fn required_system_features_are_extracted_from_the_target_env() {
        let json = r#"{
          "derivations": {
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-vm-test.drv": {
              "outputs": {"out": {"path": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-vm-test"}},
              "env": {"out": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-vm-test",
                      "requiredSystemFeatures": "kvm nixos-test"},
              "name": "vm-test"
            }
          }
        }"#;
        let (_, features) = dep_closure_from_show_json(
            json,
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-vm-test.drv",
            "job",
        )
        .unwrap();
        assert_eq!(
            features,
            Some(vec!["kvm".to_string(), "nixos-test".to_string()])
        );
    }

    #[test]
    fn missing_target_is_an_error() {
        let err = dep_closure_from_show_json(&fixture(), "/nix/store/zzz-missing.drv", "job")
            .unwrap_err();
        assert!(format!("{err:#}").contains("zzz-missing"), "got: {err:#}");
    }

    #[test]
    fn target_drv_basename_spelling_is_accepted() {
        // Callers may pass the target as a full store path or as the
        // bare basename `nix derivation show` keys its map with; both
        // spellings resolve to the same record, carrying the full path.
        let (rec, _) = dep_closure_from_show_json(
            &fixture(),
            "00d529rs5cfj1kwz79sm79qackf9gppk-strncpy.drv",
            "bootstrap.strncpy.x86_64-linux",
        )
        .unwrap();
        assert_eq!(rec.drv_path, TARGET);
        assert_eq!(rec.deps.len(), 5);
        assert!(
            !rec.deps.iter().any(|d| d.drv_path == TARGET),
            "the target drv must not appear in its own deps"
        );
    }

    #[test]
    fn dep_closure_record_serializes_with_design_field_names() {
        let rec = DepClosureRecord {
            job: "j".into(),
            drv_path: "/nix/store/aaa-x.drv".into(),
            deps: vec![DepDrv {
                drv_path: "/nix/store/bbb-y.drv".into(),
                output_paths: vec!["/nix/store/bbb-y".into()],
            }],
            ca_outputs: vec![],
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert!(v.get("drvPath").is_some());
        assert!(v.get("deps").is_some());
        assert_eq!(v["deps"][0]["drvPath"], "/nix/store/bbb-y.drv");
        assert_eq!(v["deps"][0]["outputPaths"][0], "/nix/store/bbb-y");
        assert!(
            v.get("depOutputPaths").is_none(),
            "flat depOutputPaths replaced by the adjacency form"
        );
        assert!(v.get("caOutputs").is_none(), "empty caOutputs omitted");
    }

    #[test]
    fn closure_adjacency_extracts_inputs_srcs_outputs_and_impure_env() {
        let adj = closure_adjacency_from_show_json(&fixture()).unwrap();
        // One record per drv in the show output, keyed by full /nix/store path.
        assert_eq!(adj.records.len(), 6);
        for rec in adj.records.values() {
            assert!(
                rec.drv.starts_with("/nix/store/"),
                "drv keys are full store paths"
            );
            for i in &rec.inputs {
                assert!(i.starts_with("/nix/store/") && i.ends_with(".drv"));
            }
            for s in &rec.srcs {
                assert!(s.starts_with("/nix/store/") && !s.ends_with(".drv"));
            }
        }
        // Direct adjacency from the nested `inputs.drvs` map: strncpy depends
        // on its builder.
        let strncpy = adj
            .records
            .values()
            .find(|r| r.drv.ends_with("-strncpy.drv"))
            .unwrap();
        assert!(
            strncpy
                .inputs
                .iter()
                .any(|i| i.ends_with("-strncpy-builder.drv"))
        );
        // `inputs.srcs` survive as non-drv store paths (the kaem wrapper script).
        let kaem = adj
            .records
            .values()
            .find(|r| r.drv.ends_with("-kaem-1.9.1.drv"))
            .unwrap();
        assert!(kaem.srcs.iter().any(|s| s.ends_with("-kaem-wrapper.kaem")));
        // Output resolution reuses the existing output_path helper:
        // input-addressed outputs come from outputs.<name>.path, the
        // fixed-output fetchurl's out path is recovered from env — both end
        // up Some(/nix/store/…).
        assert_eq!(
            strncpy.outputs.get("out").unwrap().as_deref(),
            Some("/nix/store/wh601bizr79g0x5h9r7m06hcahr07skz-strncpy")
        );
        assert!(adj.records.values().all(|r| {
            r.outputs
                .values()
                .all(|v| v.as_ref().is_none_or(|p| p.starts_with("/nix/store/")))
        }));
        // impure-env capture: the fetchurl patch drv declares the proxy variables.
        let (impure_drv, vars) = adj
            .impure_env
            .iter()
            .find(|(d, _)| d.ends_with("-expr-strcmp.patch.drv"))
            .unwrap();
        assert!(impure_drv.starts_with("/nix/store/"));
        assert_eq!(
            vars,
            &vec![
                "all_proxy".to_string(),
                "ftp_proxy".to_string(),
                "http_proxy".to_string(),
                "https_proxy".to_string(),
                "no_proxy".to_string()
            ]
        );
        assert_eq!(
            adj.impure_env.len(),
            1,
            "only the fetchurl drv declares impureEnvVars"
        );
        // None of the fixture derivations declares requiredSystemFeatures.
        assert!(adj.required_system_features.is_empty());
    }

    #[test]
    fn impure_env_names_are_sorted_deduped_and_floating_outputs_map_to_none() {
        // Hand-written two-entry show output in the wrapped (nix ≥ 2.32)
        // shape: one derivation declares impureEnvVars with duplicate,
        // unsorted names, and its `lib` output is floating — neither
        // outputs.lib.path nor an env.lib store path exists.
        let show = serde_json::json!({
            "version": 4,
            "derivations": {
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-impure-1.0.drv": {
                    "name": "impure-1.0",
                    "outputs": {
                        "out": {"path": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-impure-1.0"},
                        "lib": {"hashAlgo": "sha256", "method": "nar"}
                    },
                    "env": {
                        "impureEnvVars": "NIX_SECRET A_TOKEN NIX_SECRET",
                        "out": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-impure-1.0"
                    },
                    "inputs": {
                        "drvs": {
                            "cccccccccccccccccccccccccccccccc-dep-1.0.drv": {
                                "dynamicOutputs": {},
                                "outputs": ["out"]
                            }
                        },
                        "srcs": []
                    }
                },
                "cccccccccccccccccccccccccccccccc-dep-1.0.drv": {
                    "name": "dep-1.0",
                    "outputs": {"out": {"path": "dddddddddddddddddddddddddddddddd-dep-1.0"}},
                    "env": {"out": "/nix/store/dddddddddddddddddddddddddddddddd-dep-1.0"},
                    "inputs": {"drvs": {}, "srcs": []}
                }
            }
        })
        .to_string();
        let adj = closure_adjacency_from_show_json(&show).unwrap();
        assert_eq!(adj.records.len(), 2);
        let impure = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-impure-1.0.drv";
        // Variable names are split on ASCII whitespace, sorted, and deduplicated.
        assert_eq!(
            adj.impure_env[impure],
            vec!["A_TOKEN".to_string(), "NIX_SECRET".to_string()]
        );
        assert_eq!(adj.impure_env.len(), 1);
        // The floating output resolves to no static path; the declared
        // sibling keeps its store path.
        let rec = &adj.records[impure];
        assert!(rec.outputs.get("lib").unwrap().is_none());
        assert_eq!(
            rec.outputs.get("out").unwrap().as_deref(),
            Some("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-impure-1.0")
        );
        assert_eq!(
            rec.inputs,
            vec!["/nix/store/cccccccccccccccccccccccccccccccc-dep-1.0.drv".to_string()]
        );
        assert!(adj.required_system_features.is_empty());
    }

    #[test]
    fn legacy_flat_input_spellings_and_required_features_are_extracted() {
        // Older nix emits the unwrapped top-level map with flat
        // `inputDrvs`/`inputSrcs` keys (full store paths, not basenames);
        // the adjacency extraction falls back to them when the nested
        // `inputs` object is absent. requiredSystemFeatures declarations
        // are collected per derivation, in declaration order.
        let show = serde_json::json!({
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-vm-test.drv": {
                "name": "vm-test",
                "outputs": {"out": {"path": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-vm-test"}},
                "env": {
                    "out": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-vm-test",
                    "requiredSystemFeatures": "kvm nixos-test"
                },
                "inputDrvs": {
                    "/nix/store/cccccccccccccccccccccccccccccccc-dep-1.0.drv": ["out"]
                },
                "inputSrcs": ["/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-builder.sh"]
            }
        })
        .to_string();
        let adj = closure_adjacency_from_show_json(&show).unwrap();
        let rec = &adj.records["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-vm-test.drv"];
        assert_eq!(
            rec.inputs,
            vec!["/nix/store/cccccccccccccccccccccccccccccccc-dep-1.0.drv".to_string()]
        );
        assert_eq!(
            rec.srcs,
            vec!["/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-builder.sh".to_string()]
        );
        assert_eq!(
            adj.required_system_features["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-vm-test.drv"],
            vec!["kvm".to_string(), "nixos-test".to_string()]
        );
        assert!(adj.impure_env.is_empty());
    }

    #[test]
    fn dep_closure_record_round_trips_through_jsonl() {
        use crate::evalset::artifacts::{DEP_CLOSURE_FILE, EvalSetDir};

        let rec = DepClosureRecord {
            job: "j".into(),
            drv_path: "/nix/store/aaa-x.drv".into(),
            deps: vec![DepDrv {
                drv_path: "/nix/store/bbb-y.drv".into(),
                output_paths: vec!["/nix/store/bbb-y".into()],
            }],
            ca_outputs: vec![CaOutput {
                drv_path: "/nix/store/ccc-z.drv".into(),
                output: "out".into(),
            }],
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = EvalSetDir::create(tmp.path()).unwrap();
        let path = dir
            .write_jsonl(DEP_CLOSURE_FILE, std::slice::from_ref(&rec))
            .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);

        let back: DepClosureRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(back.job, rec.job);
        assert_eq!(back.drv_path, rec.drv_path);
        assert_eq!(back.deps[0].drv_path, rec.deps[0].drv_path);
        assert_eq!(back.deps[0].output_paths, rec.deps[0].output_paths);
        assert_eq!(back.ca_outputs[0].drv_path, rec.ca_outputs[0].drv_path);
        assert_eq!(back.ca_outputs[0].output, rec.ca_outputs[0].output);

        // A record written without caOutputs (omitted when empty) must
        // deserialize back to an empty list, not fail.
        let no_ca: DepClosureRecord =
            serde_json::from_str(r#"{"job":"j","drvPath":"/nix/store/aaa-x.drv","deps":[]}"#)
                .unwrap();
        assert!(no_ca.ca_outputs.is_empty());
    }
}

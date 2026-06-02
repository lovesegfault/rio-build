//! Closure adjacency extraction from `nix derivation show -r` output.
//!
//! The recorder runs one `nix derivation show -r` per workload unit and
//! [`closure_adjacency_from_show_json`] extracts from each blob the
//! replay archive's view of the closure: one direct-adjacency record
//! per derivation (`inputs`/`srcs`/`outputs`, the `closures.jsonl`
//! shape) together with each derivation's declared impure environment
//! variables (the `impure-env.json` member) and its
//! `requiredSystemFeatures` (backfilled into the unit metadata).
//! Per-target results merge key-by-key into one union over the whole
//! scope. Enumeration is static — one subprocess per target, never one
//! per dependency, and no builds.

use std::collections::BTreeMap;

use anyhow::Context as _;
use rio_nix::derivation::structured_attrs::{self, StructuredAttrsEnv, string_list_attr};
use serde::Deserialize;

use crate::archive::schema::{ClosureRecord, ImpureEnv};

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
    /// The `__structuredAttrs` user-attr payload as nix ≥ 2.34 show
    /// JSON emits it: lifted to a top-level object (the env then
    /// carries only output placeholder keys, never the user attrs).
    /// Older show output mirrors the raw derivation env instead, where
    /// the same payload is the `env.__json` string — the [`ShowEnv`]
    /// adapter handles that fallback.
    #[serde(default, rename = "structuredAttrs")]
    structured_attrs: Option<serde_json::Value>,
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
///    and the real path only in the builder env). `__structuredAttrs`
///    derivations keep their per-output env keys — nix writes output
///    placeholders into the env separately from the user attrs it
///    serializes into the structured payload — so fixed-output paths
///    recover the same way for both encodings (pinned by the
///    structured-attrs fixture test);
/// 3. else None — floating CA output.
///
/// The `/nix/store/` prefix check shares `normalize_store_path`'s
/// standard-store-dir assumption.
fn output_path(drv: &ShowDrv, output_name: &str) -> Option<String> {
    if let Some(p) = drv.outputs.get(output_name).and_then(|o| o.path.as_deref()) {
        return Some(normalize_store_path(p));
    }
    match drv.env.get(output_name) {
        Some(serde_json::Value::String(s)) if s.starts_with("/nix/store/") => Some(s.clone()),
        _ => None,
    }
}

/// [`StructuredAttrsEnv`] view of one `nix derivation show` entry — the
/// recorder-side carrier adapter for the canonical structured-attrs read
/// (`rio_nix::derivation::structured_attrs`). Deliberately dumb: it only
/// knows where THIS carrier keeps the two sources — the flat env is a
/// JSON map (string values only count as flat attrs), and the structured
/// payload is either the top-level `structuredAttrs` object the show
/// format lifts it to (nix ≥ 2.34, format version 4) or the raw
/// `env.__json` string of older show output (the same encoding
/// ATerm-derived envs carry). All precedence lives in the shared rule, so
/// the recorder can never silently disagree with the gateway or the
/// replay engine about what one derivation declares.
///
/// A malformed `__json` string yields no payload (flat fallback) — the
/// payload is machine-written by nix, so that arm is never expected to
/// fire. The `__json` arm parses once per view (one per derivation); the
/// lifted form is borrowed.
struct ShowEnv<'a> {
    env: &'a BTreeMap<String, serde_json::Value>,
    payload: Option<std::borrow::Cow<'a, serde_json::Value>>,
}

impl<'a> ShowEnv<'a> {
    fn new(drv: &'a ShowDrv) -> Self {
        use std::borrow::Cow;
        let payload = if let Some(value) = &drv.structured_attrs {
            Some(Cow::Borrowed(value))
        } else {
            drv.env
                .get("__json")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .map(Cow::Owned)
        };
        Self {
            env: &drv.env,
            payload,
        }
    }
}

impl StructuredAttrsEnv for ShowEnv<'_> {
    fn flat(&self, key: &str) -> Option<&str> {
        self.env.get(key).and_then(|value| value.as_str())
    }

    fn structured_payload(&self) -> Option<&serde_json::Value> {
        self.payload.as_deref()
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
    /// declares via `impureEnvVars` (read through the canonical
    /// structured-payload-first rule, see
    /// `rio_nix::derivation::structured_attrs`), sorted and
    /// deduplicated. Only derivations declaring at least one variable
    /// are present.
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

        // impureEnvVars / requiredSystemFeatures are read through THE
        // canonical structured-payload-first rule (`string_list_attr` in
        // rio_nix::derivation::structured_attrs, via the [`ShowEnv`]
        // adapter): for `__structuredAttrs` derivations the flat env
        // never carries them, so an env-only read would silently record
        // no impure declarations (the unit is then built without its
        // recorded impure env and charged as a false rio regression) and
        // no required features (the plan-stage exclusion filter cannot
        // fire while the gateway still extracts the real features at
        // submit time).
        let view = ShowEnv::new(drv);
        // impureEnvVars is stored sorted and deduplicated, and only
        // when non-empty.
        let mut names = string_list_attr(&view, structured_attrs::IMPURE_ENV_VARS_ATTR);
        names.sort();
        names.dedup();
        if !names.is_empty() {
            adjacency.impure_env.insert(drv_path.clone(), names);
        }
        // requiredSystemFeatures keeps declaration order (it backfills
        // per-unit metadata, not an archive member of its own).
        let features = string_list_attr(&view, structured_attrs::REQUIRED_SYSTEM_FEATURES_ATTR);
        if !features.is_empty() {
            adjacency
                .required_system_features
                .insert(drv_path.clone(), features);
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

    /// Two real records captured verbatim from a `nix derivation show
    /// -r` run (nix 2.34.7, JSON format version 4) over a
    /// `__structuredAttrs = true` closure: an input-addressed target
    /// declaring `requiredSystemFeatures` and a fixed-output dependency
    /// declaring `impureEnvVars`. In this format the user attrs live in
    /// the top-level `structuredAttrs` object — the env carries only
    /// output placeholder keys.
    fn structured_attrs_fixture() -> String {
        std::fs::read_to_string(
            crate::test_manifest_dir().join("tests/fixtures/derivation-show/structured-attrs.json"),
        )
        .unwrap()
    }

    #[test]
    fn structured_attrs_declarations_are_extracted() {
        // The flat-env-only reader extracted nothing from either record
        // (structuredAttrs derivations carry no flat user-attr env
        // keys), so this fixture samples exactly the population the
        // structured-first read exists for.
        let adj = closure_adjacency_from_show_json(&structured_attrs_fixture()).unwrap();
        assert_eq!(adj.records.len(), 2);
        // impureEnvVars from the structured payload, sorted and
        // deduplicated (declared order in the fixture: https_proxy,
        // http_proxy, no_proxy).
        let fod = "/nix/store/h37kzbmx226b0i2gb7bhcc668wwgxkgs-structured-fetch-1.0.drv";
        assert_eq!(
            adj.impure_env[fod],
            vec![
                "http_proxy".to_string(),
                "https_proxy".to_string(),
                "no_proxy".to_string()
            ]
        );
        assert_eq!(adj.impure_env.len(), 1);
        // requiredSystemFeatures from the structured payload, in
        // declaration order.
        let target = "/nix/store/2s7zynm430f86wwgv80pg59dpmhqc449-structured-vm-test.drv";
        assert_eq!(
            adj.required_system_features[target],
            vec!["kvm".to_string(), "nixos-test".to_string()]
        );
        assert_eq!(adj.required_system_features.len(), 1);
        // The structuredAttrs fixed-output drv still resolves its out
        // path through the per-output env key: nix writes output
        // placeholders into the env separately from the user attrs it
        // serializes into the structured payload.
        assert_eq!(
            adj.records[fod].outputs["out"].as_deref(),
            Some("/nix/store/rsrhap460vd96m7fwffigi23cprl38r2-structured-fetch-1.0")
        );
    }

    #[test]
    fn env_json_payload_is_read_when_structured_attrs_is_not_lifted() {
        // Older `nix derivation show` mirrors the raw derivation env,
        // where `__structuredAttrs` serializes the user attrs into
        // `env.__json` only. Both `__json` strings here are verbatim
        // raw-ATerm env values of the same two derivations the
        // structured-attrs fixture records (read from the .drv files).
        // The decoy flat `requiredSystemFeatures` key on the first
        // entry can never co-occur with `__json` in real output; it
        // pins the precedence order — the structured payload wins over
        // the flat env, mirroring the gateway's `StructuredEnv`.
        let show = serde_json::json!({
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-structured-vm-test.drv": {
                "name": "structured-vm-test",
                "outputs": {"out": {"path": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-structured-vm-test"}},
                "env": {
                    "__json": "{\"builder\":\"/bin/sh\",\"fetched\":\"/nix/store/rsrhap460vd96m7fwffigi23cprl38r2-structured-fetch-1.0\",\"name\":\"structured-vm-test\",\"requiredSystemFeatures\":[\"kvm\",\"nixos-test\"],\"system\":\"x86_64-linux\"}",
                    "out": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-structured-vm-test",
                    "requiredSystemFeatures": "decoy-flat-feature"
                },
                "inputDrvs": {
                    "/nix/store/cccccccccccccccccccccccccccccccc-structured-fetch-1.0.drv": ["out"]
                },
                "inputSrcs": []
            },
            "/nix/store/cccccccccccccccccccccccccccccccc-structured-fetch-1.0.drv": {
                "name": "structured-fetch-1.0",
                "outputs": {"out": {"hash": "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=", "method": "flat"}},
                "env": {
                    "__json": "{\"builder\":\"/bin/sh\",\"impureEnvVars\":[\"https_proxy\",\"http_proxy\",\"no_proxy\"],\"name\":\"structured-fetch-1.0\",\"outputHash\":\"sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=\",\"outputHashAlgo\":\"sha256\",\"outputHashMode\":\"flat\",\"system\":\"x86_64-linux\"}",
                    "out": "/nix/store/dddddddddddddddddddddddddddddddd-structured-fetch-1.0"
                },
                "inputDrvs": {},
                "inputSrcs": []
            }
        })
        .to_string();
        let adj = closure_adjacency_from_show_json(&show).unwrap();
        assert_eq!(adj.records.len(), 2);
        // The __json payload wins over the decoy flat key.
        assert_eq!(
            adj.required_system_features["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-structured-vm-test.drv"],
            vec!["kvm".to_string(), "nixos-test".to_string()]
        );
        // impureEnvVars parsed out of __json, sorted and deduplicated.
        assert_eq!(
            adj.impure_env["/nix/store/cccccccccccccccccccccccccccccccc-structured-fetch-1.0.drv"],
            vec![
                "http_proxy".to_string(),
                "https_proxy".to_string(),
                "no_proxy".to_string()
            ]
        );
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
}

//! Output policy checks: `allowedReferences` / `disallowedReferences` /
//! `allowedRequisites` / `disallowedRequisites` / `maxSize` /
//! `maxClosureSize` / `unsafeDiscardReferences`.
//!
//! nix-daemon enforces these silently today; rio has never had code that
//! even mentions them, so dropping the daemon without reimplementing them
//! would silently change build semantics (stdenv uses
//! `disallowedRequisites` to keep bootstrap tools out of final outputs).
//!
//! Two disjoint sources, mirroring Nix's `DerivationOptions`:
//!
//! * **Legacy env attrs** (non-`__structuredAttrs` derivations): the four
//!   reference/requisite lists as whitespace-separated env values. They
//!   apply to *every* output, and the `*Requisites` checks exclude the
//!   output's own path from its closure.
//! * **`__structuredAttrs` derivations**: the per-output
//!   `outputChecks.<name>.{…}` object inside `env["__json"]` — the only
//!   form that can express `maxSize`/`maxClosureSize` — plus
//!   `unsafeDiscardReferences.<name>`. Self-references are *not* excluded.
//!
//! A derivation in structuredAttrs mode has only `__json` in its env, so
//! a parser that read only the legacy keys would silently skip every
//! check for exactly the derivations nixpkgs is migrating toward.
//
// TODO(M7): the __json navigation here should move to the shared
// StructuredEnv parser introduced by the request-glue work so the
// gateway, the request glue, and this module read structured attrs
// through one implementation.

use std::collections::{BTreeMap, HashMap, HashSet};

/// Per-output checks (the structuredAttrs `outputChecks.<name>` object,
/// or the legacy global lists applied to every output).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct OutputChecks {
    /// `Some(list)` = the output may only reference paths in the list
    /// (entries are store paths or sibling output names). `None` = no
    /// restriction. An empty `Some` list means "no references allowed".
    pub(crate) allowed_references: Option<Vec<String>>,
    pub(crate) disallowed_references: Vec<String>,
    /// Like `allowed_references` but over the output's full runtime
    /// closure rather than its direct references.
    pub(crate) allowed_requisites: Option<Vec<String>>,
    pub(crate) disallowed_requisites: Vec<String>,
    /// Maximum NAR size of the output itself, bytes.
    pub(crate) max_size: Option<u64>,
    /// Maximum total NAR size of the output's closure (including the
    /// output itself), bytes.
    pub(crate) max_closure_size: Option<u64>,
}

impl OutputChecks {
    fn is_empty(&self) -> bool {
        self.allowed_references.is_none()
            && self.disallowed_references.is_empty()
            && self.allowed_requisites.is_none()
            && self.disallowed_requisites.is_empty()
            && self.max_size.is_none()
            && self.max_closure_size.is_none()
    }
}

/// The parsed output policy of one derivation.
#[derive(Debug, Default, Clone)]
pub(crate) struct OutputPolicy {
    /// Legacy (non-structuredAttrs) global checks, applied to every
    /// output. Empty when the derivation uses structuredAttrs.
    legacy: OutputChecks,
    /// structuredAttrs per-output checks, keyed by output name. Empty
    /// for legacy derivations.
    per_output: BTreeMap<String, OutputChecks>,
    /// structuredAttrs `unsafeDiscardReferences.<name>` — outputs whose
    /// scanned reference set is deliberately recorded as empty (bootstrap
    /// tarballs, disk images that legitimately embed copies of inputs).
    discard_references: BTreeMap<String, bool>,
    /// Whether the derivation used structuredAttrs (changes the
    /// self-reference exclusion rule for `*Requisites`).
    structured: bool,
}

impl OutputPolicy {
    /// Parse the policy from a derivation's environment.
    ///
    /// structuredAttrs detection follows Nix: the presence of a parseable
    /// `__json` key. In that mode the legacy env keys are ignored (they
    /// do not exist as separate keys anyway — everything lives in the
    /// JSON blob).
    pub(crate) fn parse(env: &BTreeMap<String, String>) -> Self {
        if let Some(json) = env.get("__json")
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(json)
        {
            return Self::from_structured(&value);
        }

        let list = |key: &str| -> Option<Vec<String>> {
            env.get(key)
                .map(|v| v.split_whitespace().map(String::from).collect())
        };
        OutputPolicy {
            legacy: OutputChecks {
                allowed_references: list("allowedReferences"),
                disallowed_references: list("disallowedReferences").unwrap_or_default(),
                allowed_requisites: list("allowedRequisites"),
                disallowed_requisites: list("disallowedRequisites").unwrap_or_default(),
                // maxSize/maxClosureSize have no legacy env form.
                max_size: None,
                max_closure_size: None,
            },
            per_output: BTreeMap::new(),
            discard_references: BTreeMap::new(),
            structured: false,
        }
    }

    fn from_structured(json: &serde_json::Value) -> Self {
        let str_list = |v: &serde_json::Value| -> Vec<String> {
            v.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut per_output = BTreeMap::new();
        if let Some(checks) = json.get("outputChecks").and_then(|v| v.as_object()) {
            for (output, spec) in checks {
                let get = |key: &str| spec.get(key);
                per_output.insert(
                    output.clone(),
                    OutputChecks {
                        allowed_references: get("allowedReferences").map(&str_list),
                        disallowed_references: get("disallowedReferences")
                            .map(&str_list)
                            .unwrap_or_default(),
                        allowed_requisites: get("allowedRequisites").map(&str_list),
                        disallowed_requisites: get("disallowedRequisites")
                            .map(&str_list)
                            .unwrap_or_default(),
                        max_size: get("maxSize").and_then(serde_json::Value::as_u64),
                        max_closure_size: get("maxClosureSize").and_then(serde_json::Value::as_u64),
                    },
                );
            }
        }

        let mut discard_references = BTreeMap::new();
        if let Some(discard) = json
            .get("unsafeDiscardReferences")
            .and_then(|v| v.as_object())
        {
            for (output, flag) in discard {
                discard_references.insert(output.clone(), flag.as_bool().unwrap_or(false));
            }
        }

        OutputPolicy {
            legacy: OutputChecks::default(),
            per_output,
            discard_references,
            structured: true,
        }
    }

    /// Whether the scanned references of `output` should be discarded
    /// (recorded as empty) per `unsafeDiscardReferences`.
    pub(crate) fn discard_references_for(&self, output: &str) -> bool {
        self.discard_references
            .get(output)
            .copied()
            .unwrap_or(false)
    }

    /// The checks that apply to `output`, if any.
    fn checks_for(&self, output: &str) -> Option<&OutputChecks> {
        if self.structured {
            self.per_output.get(output).filter(|c| !c.is_empty())
        } else if self.legacy.is_empty() {
            None
        } else {
            Some(&self.legacy)
        }
    }

    /// True when no output has any check and nothing is discarded —
    /// lets the caller skip closure computation entirely.
    pub(crate) fn is_empty(&self) -> bool {
        self.discard_references.values().all(|v| !v)
            && self.legacy.is_empty()
            && self.per_output.values().all(OutputChecks::is_empty)
    }
}

/// One output's view for policy checking, supplied by the result
/// pipeline after scanning.
#[derive(Debug, Clone)]
pub(crate) struct OutputForPolicy {
    pub(crate) name: String,
    pub(crate) store_path: String,
    /// Direct (post-discard) references, full store paths.
    pub(crate) references: Vec<String>,
    pub(crate) nar_size: u64,
}

/// A policy violation. The message is tenant-facing.
#[derive(Debug, thiserror::Error)]
#[error("output '{output}' violates {rule}: {detail}")]
pub(crate) struct PolicyViolation {
    pub(crate) output: String,
    pub(crate) rule: &'static str,
    pub(crate) detail: String,
}

/// Check every output against the policy.
///
/// `outputs` are this build's outputs (post reference-scan, post CA
/// finalization once that exists). `closure_info` maps every *input*
/// closure path to its `(references, nar_size)` — the data needed to
/// extend an output's direct references into its full runtime closure
/// for the `*Requisites` / `maxClosureSize` checks. Sibling outputs are
/// resolved against `outputs` itself.
pub(crate) fn check_outputs(
    outputs: &[OutputForPolicy],
    policy: &OutputPolicy,
    closure_info: &HashMap<String, (Vec<String>, u64)>,
) -> Result<(), PolicyViolation> {
    if policy.is_empty() {
        return Ok(());
    }

    let by_path: HashMap<&str, &OutputForPolicy> =
        outputs.iter().map(|o| (o.store_path.as_str(), o)).collect();
    let name_to_path: HashMap<&str, &str> = outputs
        .iter()
        .map(|o| (o.name.as_str(), o.store_path.as_str()))
        .collect();

    for output in outputs {
        let Some(checks) = policy.checks_for(&output.name) else {
            continue;
        };

        // Resolve an entry that may be a sibling output name into its
        // store path; anything else is taken to already be a store path.
        let resolve = |entry: &str| -> String {
            name_to_path
                .get(entry)
                .map(|p| (*p).to_string())
                .unwrap_or_else(|| entry.to_string())
        };

        // --- direct-reference checks --------------------------------
        if let Some(allowed) = &checks.allowed_references {
            let allowed: HashSet<String> = allowed.iter().map(|e| resolve(e)).collect();
            if let Some(bad) = output.references.iter().find(|r| !allowed.contains(*r)) {
                return Err(PolicyViolation {
                    output: output.name.clone(),
                    rule: "allowedReferences",
                    detail: format!("references {bad}, which is not in the allowed list"),
                });
            }
        }
        if !checks.disallowed_references.is_empty() {
            let disallowed: HashSet<String> = checks
                .disallowed_references
                .iter()
                .map(|e| resolve(e))
                .collect();
            if let Some(bad) = output.references.iter().find(|r| disallowed.contains(*r)) {
                return Err(PolicyViolation {
                    output: output.name.clone(),
                    rule: "disallowedReferences",
                    detail: format!("references {bad}, which is explicitly disallowed"),
                });
            }
        }

        // --- closure (requisite / size) checks -----------------------
        let needs_closure = checks.allowed_requisites.is_some()
            || !checks.disallowed_requisites.is_empty()
            || checks.max_closure_size.is_some();
        if needs_closure {
            let closure = compute_closure(output, &by_path, closure_info).map_err(|missing| {
                PolicyViolation {
                    output: output.name.clone(),
                    rule: "requisite closure",
                    detail: format!(
                        "cannot verify: no closure metadata for referenced path {missing} \
                             (not an input of this derivation and not a sibling output)"
                    ),
                }
            })?;

            // Legacy semantics exclude the output's own path from its
            // requisite set; structuredAttrs semantics do not.
            let requisites: HashSet<&String> = if policy.structured {
                closure.iter().collect()
            } else {
                closure
                    .iter()
                    .filter(|p| **p != output.store_path)
                    .collect()
            };

            if let Some(allowed) = &checks.allowed_requisites {
                let allowed: HashSet<String> = allowed.iter().map(|e| resolve(e)).collect();
                if let Some(bad) = requisites.iter().find(|r| !allowed.contains(**r)) {
                    return Err(PolicyViolation {
                        output: output.name.clone(),
                        rule: "allowedRequisites",
                        detail: format!(
                            "runtime closure contains {bad}, which is not in the allowed list"
                        ),
                    });
                }
            }
            if !checks.disallowed_requisites.is_empty() {
                let disallowed: HashSet<String> = checks
                    .disallowed_requisites
                    .iter()
                    .map(|e| resolve(e))
                    .collect();
                if let Some(bad) = requisites.iter().find(|r| disallowed.contains(**r)) {
                    return Err(PolicyViolation {
                        output: output.name.clone(),
                        rule: "disallowedRequisites",
                        detail: format!(
                            "runtime closure contains {bad}, which is explicitly disallowed"
                        ),
                    });
                }
            }
            if let Some(max) = checks.max_closure_size {
                let total: u64 = closure
                    .iter()
                    .map(|p| {
                        by_path
                            .get(p.as_str())
                            .map(|o| o.nar_size)
                            .or_else(|| closure_info.get(p).map(|(_, sz)| *sz))
                            .unwrap_or(0)
                    })
                    .sum();
                if total > max {
                    return Err(PolicyViolation {
                        output: output.name.clone(),
                        rule: "maxClosureSize",
                        detail: format!("closure size {total} bytes exceeds the limit of {max}"),
                    });
                }
            }
        }

        if let Some(max) = checks.max_size
            && output.nar_size > max
        {
            return Err(PolicyViolation {
                output: output.name.clone(),
                rule: "maxSize",
                detail: format!(
                    "NAR size {} bytes exceeds the limit of {max}",
                    output.nar_size
                ),
            });
        }
    }
    Ok(())
}

/// Compute the full runtime closure of `output` (including itself):
/// transitive references through sibling outputs (whose reference sets
/// were just scanned) and through input paths (whose reference sets come
/// from the resolved input-closure metadata).
///
/// Returns `Err(path)` when a reachable path has no metadata anywhere —
/// the check cannot be performed soundly, so the caller rejects rather
/// than silently passing.
fn compute_closure(
    output: &OutputForPolicy,
    siblings_by_path: &HashMap<&str, &OutputForPolicy>,
    closure_info: &HashMap<String, (Vec<String>, u64)>,
) -> Result<HashSet<String>, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = vec![output.store_path.clone()];
    while let Some(path) = stack.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        // Every sibling output — including the one under check, where
        // the walk starts — is in `siblings_by_path`; anything else must
        // be covered by the input-closure metadata or the check cannot
        // be performed soundly.
        let refs: &[String] = if let Some(sib) = siblings_by_path.get(path.as_str()) {
            &sib.references
        } else if let Some((refs, _)) = closure_info.get(&path) {
            refs
        } else {
            return Err(path);
        };
        for r in refs {
            if !seen.contains(r) {
                stack.push(r.clone());
            }
        }
    }
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(hash32: &str, name: &str) -> String {
        format!("/nix/store/{hash32}-{name}")
    }

    fn out_path() -> String {
        p("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "thing")
    }
    fn dev_path() -> String {
        p("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "thing-dev")
    }
    fn input_path() -> String {
        p("cccccccccccccccccccccccccccccccc", "glibc")
    }
    fn bootstrap_path() -> String {
        p("dddddddddddddddddddddddddddddddd", "bootstrap-tools")
    }

    fn legacy_env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn structured_env(json: serde_json::Value) -> BTreeMap<String, String> {
        BTreeMap::from([("__json".to_string(), json.to_string())])
    }

    fn output(name: &str, path: &str, refs: &[&str], nar_size: u64) -> OutputForPolicy {
        OutputForPolicy {
            name: name.into(),
            store_path: path.into(),
            references: refs.iter().map(|s| s.to_string()).collect(),
            nar_size,
        }
    }

    /// Closure metadata for the standard test input: glibc has no refs.
    fn closure_info() -> HashMap<String, (Vec<String>, u64)> {
        HashMap::from([
            (input_path(), (vec![], 1000)),
            (bootstrap_path(), (vec![], 5000)),
        ])
    }

    #[test]
    fn empty_policy_passes_everything() {
        let policy = OutputPolicy::parse(&legacy_env(&[]));
        assert!(policy.is_empty());
        let outs = [output("out", &out_path(), &[&input_path()], 10)];
        check_outputs(&outs, &policy, &closure_info()).unwrap();
    }

    #[test]
    fn legacy_disallowed_references_rejects() {
        let env = legacy_env(&[("disallowedReferences", &bootstrap_path())]);
        let policy = OutputPolicy::parse(&env);
        let outs = [output("out", &out_path(), &[&bootstrap_path()], 10)];
        let err = check_outputs(&outs, &policy, &closure_info()).unwrap_err();
        assert_eq!(err.rule, "disallowedReferences");
        assert!(err.detail.contains("bootstrap-tools"), "{err}");
    }

    #[test]
    fn legacy_allowed_references_rejects_unlisted() {
        let env = legacy_env(&[("allowedReferences", "")]);
        let policy = OutputPolicy::parse(&env);
        // Empty allowed list = no references allowed at all.
        let outs = [output("out", &out_path(), &[&input_path()], 10)];
        let err = check_outputs(&outs, &policy, &closure_info()).unwrap_err();
        assert_eq!(err.rule, "allowedReferences");

        // No references → passes.
        let outs = [output("out", &out_path(), &[], 10)];
        check_outputs(&outs, &policy, &closure_info()).unwrap();
    }

    #[test]
    fn legacy_allowed_references_accepts_sibling_output_names() {
        // `allowedReferences = [ "out" ]` style: the entry is an output
        // NAME, resolved to its store path.
        let env = legacy_env(&[("allowedReferences", "out")]);
        let policy = OutputPolicy::parse(&env);
        let outs = [
            output("out", &out_path(), &[], 10),
            output("dev", &dev_path(), &[&out_path()], 10),
        ];
        check_outputs(&outs, &policy, &closure_info()).unwrap();
    }

    #[test]
    fn legacy_disallowed_requisites_rejects_transitively() {
        // out → dev (sibling) → bootstrap-tools: the bootstrap path is
        // nowhere in out's DIRECT references but is in its closure.
        let env = legacy_env(&[("disallowedRequisites", &bootstrap_path())]);
        let policy = OutputPolicy::parse(&env);
        let mut info = closure_info();
        info.insert(input_path(), (vec![bootstrap_path()], 1000));
        let outs = [output("out", &out_path(), &[&input_path()], 10)];
        let err = check_outputs(&outs, &policy, &info).unwrap_err();
        assert_eq!(err.rule, "disallowedRequisites");
    }

    #[test]
    fn legacy_requisites_exclude_self() {
        // A self-referencing output with allowedRequisites that does NOT
        // list itself: legacy semantics exclude self, so it passes.
        let env = legacy_env(&[("allowedRequisites", &input_path())]);
        let policy = OutputPolicy::parse(&env);
        let outs = [output(
            "out",
            &out_path(),
            &[&out_path(), &input_path()],
            10,
        )];
        check_outputs(&outs, &policy, &closure_info()).unwrap();
    }

    #[test]
    fn structured_requisites_include_self() {
        // Same situation under structuredAttrs: self is NOT excluded, so
        // the unlisted self-reference fails allowedRequisites.
        let env = structured_env(serde_json::json!({
            "outputChecks": { "out": { "allowedRequisites": [input_path()] } }
        }));
        let policy = OutputPolicy::parse(&env);
        let outs = [output(
            "out",
            &out_path(),
            &[&out_path(), &input_path()],
            10,
        )];
        let err = check_outputs(&outs, &policy, &closure_info()).unwrap_err();
        assert_eq!(err.rule, "allowedRequisites");
    }

    #[test]
    fn structured_per_output_checks_only_apply_to_named_output() {
        let env = structured_env(serde_json::json!({
            "outputChecks": { "dev": { "disallowedReferences": [input_path()] } }
        }));
        let policy = OutputPolicy::parse(&env);
        // "out" references the disallowed path but the check names "dev".
        let outs = [
            output("out", &out_path(), &[&input_path()], 10),
            output("dev", &dev_path(), &[], 10),
        ];
        check_outputs(&outs, &policy, &closure_info()).unwrap();

        // The same reference on "dev" fails.
        let outs = [
            output("out", &out_path(), &[], 10),
            output("dev", &dev_path(), &[&input_path()], 10),
        ];
        let err = check_outputs(&outs, &policy, &closure_info()).unwrap_err();
        assert_eq!(err.rule, "disallowedReferences");
        assert_eq!(err.output, "dev");
    }

    #[test]
    fn structured_max_size_and_closure_size() {
        let env = structured_env(serde_json::json!({
            "outputChecks": { "out": { "maxSize": 100, "maxClosureSize": 500 } }
        }));
        let policy = OutputPolicy::parse(&env);

        // Over maxSize.
        let outs = [output("out", &out_path(), &[], 200)];
        let err = check_outputs(&outs, &policy, &closure_info()).unwrap_err();
        assert_eq!(err.rule, "maxSize");

        // Under maxSize but over maxClosureSize via the input's 1000-byte
        // NAR (closure = self 50 + glibc 1000).
        let outs = [output("out", &out_path(), &[&input_path()], 50)];
        let err = check_outputs(&outs, &policy, &closure_info()).unwrap_err();
        assert_eq!(err.rule, "maxClosureSize");

        // Under both.
        let outs = [output("out", &out_path(), &[], 50)];
        check_outputs(&outs, &policy, &closure_info()).unwrap();
    }

    #[test]
    fn unknown_closure_path_rejects_instead_of_passing() {
        let env = legacy_env(&[("disallowedRequisites", &bootstrap_path())]);
        let policy = OutputPolicy::parse(&env);
        // Reference to a path with no metadata anywhere.
        let ghost = p("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", "ghost");
        let outs = [output("out", &out_path(), &[&ghost], 10)];
        let err = check_outputs(&outs, &policy, &closure_info()).unwrap_err();
        assert!(err.detail.contains("no closure metadata"), "{err}");
        assert!(err.detail.contains("ghost"), "{err}");
    }

    #[test]
    fn unsafe_discard_references_parsed() {
        let env = structured_env(serde_json::json!({
            "unsafeDiscardReferences": { "out": true, "dev": false }
        }));
        let policy = OutputPolicy::parse(&env);
        assert!(policy.discard_references_for("out"));
        assert!(!policy.discard_references_for("dev"));
        assert!(!policy.discard_references_for("lib"));
    }

    #[test]
    fn legacy_keys_ignored_in_structured_mode() {
        // A structuredAttrs drv whose JSON happens to contain top-level
        // allowedReferences: per Nix semantics only outputChecks counts.
        let mut env = structured_env(serde_json::json!({
            "allowedReferences": [],
        }));
        // Even a stray legacy env key (impossible in practice) is ignored.
        env.insert("disallowedReferences".into(), input_path());
        let policy = OutputPolicy::parse(&env);
        let outs = [output("out", &out_path(), &[&input_path()], 10)];
        check_outputs(&outs, &policy, &closure_info()).unwrap();
    }
}

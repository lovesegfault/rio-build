//! `xtask regen helm-obs` — render the canonical `describe_*!` HELP
//! strings into the operator chart (bug_330 / merged_bug_353 class:
//! dashboard panel descriptions and PrometheusRule semantics sentences
//! restated metric meaning by hand and drifted from the code).
//!
//! Two outputs, one scrape:
//!
//! 1. `infra/helm/rio-build/generated/metric-help.json` — `{name: help}`
//!    for every described metric. The PrometheusRule template reads it
//!    via `$.Files.Get` so rendered descriptions OPEN with the canonical
//!    HELP sentence (hand-written text in the chart is operator ACTION
//!    only).
//! 2. `infra/helm/rio-build/dashboards/*.json` rewritten in place: any
//!    panel carrying the marker key `"rioMetric": "<name>"` gets
//!    `description := help[name]`, suffixed with ` — <panel.rioNote>`
//!    when present. Grafana ignores the unknown marker keys;
//!    serde_json's `preserve_order` feature keeps the diff minimal.
//!
//! `helm-obs-drift` (nix/misc-checks.nix) runs this hermetically and
//! diffs, so editing a HELP string without regenerating fails the gate.

use std::fs;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::sh::repo_root;

pub async fn run() -> Result<()> {
    let helm = repo_root().join("infra/helm/rio-build");
    let help = super::docs_data::metrics_help_map()?;

    fs::create_dir_all(helm.join("generated"))?;
    fs::write(
        helm.join("generated/metric-help.json"),
        serde_json::to_string_pretty(&help)? + "\n",
    )?;

    let mut rewritten = 0usize;
    for entry in fs::read_dir(helm.join("dashboards"))? {
        let p = entry?.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let body = fs::read_to_string(&p)?;
        let mut v: Value =
            serde_json::from_str(&body).with_context(|| format!("parse {}", p.display()))?;
        // merged_bug_098: markers are counted over the WHOLE document
        // and the rewrite recurses into nested `panels` arrays
        // (collapsed Grafana rows nest their children) — a marker the
        // walk cannot reach is a hard error, not a silent skip.
        // merged_bug_056: the count reads through rio_metric_marker,
        // so a present-but-non-string marker errors HERE instead of
        // being counted-but-unrewritable.
        let markers = count_rio_metric_markers(&v)
            .with_context(|| format!("marker census in {}", p.display()))?;
        let mut changed = false;
        let mut visited = 0usize;
        if let Some(panels) = v.get_mut("panels").and_then(Value::as_array_mut) {
            for panel in panels {
                if rewrite_panel_tree(panel, &help, &mut visited)
                    .with_context(|| format!("panel in {}", p.display()))?
                {
                    changed = true;
                }
            }
        }
        anyhow::ensure!(
            visited == markers,
            "{}: {} rioMetric marker(s) in the document but the panel \
             walk visited {} — marker outside panels[]/nested panels[]?",
            p.display(),
            markers,
            visited,
        );
        if changed {
            fs::write(&p, serde_json::to_string_pretty(&v)? + "\n")?;
            rewritten += 1;
        }
    }
    println!("wrote generated/metric-help.json; rewrote {rewritten} dashboard file(s)");
    Ok(())
}

/// THE marker-extraction predicate (merged_bug_056): `Ok(None)` when
/// the key is absent, `Ok(Some(name))` for a string marker, and a hard
/// error for a present-but-non-string value. Census, walk, and rewrite
/// all read markers through THIS function, so the three sites cannot
/// disagree on what a marker is — pre-fix the census counted by key
/// presence and the walk by `get().is_some()` while the rewrite
/// silently `Ok(false)`-skipped non-string values, re-opening the
/// silent description-rot lane one axis over (value type instead of
/// reachability).
fn rio_metric_marker(obj: &Value) -> Result<Option<&str>> {
    match obj.get("rioMetric") {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(other) => bail!("rioMetric marker must be a string metric name, got: {other}"),
    }
}

/// Recurse one panel AND its nested `panels` array (collapsed rows),
/// applying the marker rewrite to every level (merged_bug_098 — the
/// old walk was top-level only, so markers inside collapsed rows were
/// silently skipped and their descriptions rotted).
fn rewrite_panel_tree(
    panel: &mut Value,
    help: &std::collections::BTreeMap<String, String>,
    visited: &mut usize,
) -> Result<bool> {
    let mut changed = rewrite_panel_description(panel, help)?;
    if rio_metric_marker(panel)?.is_some() {
        *visited += 1;
    }
    if let Some(children) = panel.get_mut("panels").and_then(Value::as_array_mut) {
        for child in children {
            if rewrite_panel_tree(child, help, visited)? {
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// Count every `rioMetric` key anywhere in the document tree — the
/// census the walk is asserted against. Reads through
/// [`rio_metric_marker`], so a non-string marker is a hard error here
/// too, never a count the rewrite cannot act on.
fn count_rio_metric_markers(v: &Value) -> Result<usize> {
    Ok(match v {
        Value::Object(m) => {
            let own = usize::from(rio_metric_marker(v)?.is_some());
            own + m
                .values()
                .map(count_rio_metric_markers)
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .sum::<usize>()
        }
        Value::Array(a) => a
            .iter()
            .map(count_rio_metric_markers)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .sum(),
        _ => 0,
    })
}

/// Apply the marker-keyed description rewrite to one panel. Returns
/// whether the panel changed. Idempotent: a second run is a no-op.
fn rewrite_panel_description(
    panel: &mut Value,
    help: &std::collections::BTreeMap<String, String>,
) -> Result<bool> {
    let Some(name) = rio_metric_marker(panel)?.map(str::to_owned) else {
        return Ok(false);
    };
    let Some(h) = help.get(&name) else {
        bail!("rioMetric {name:?} is not described by any describe_*! callsite");
    };
    let mut desc = h.clone();
    if let Some(note) = panel.get("rioNote").and_then(Value::as_str) {
        desc.push_str(" — ");
        desc.push_str(note);
    }
    let new = Value::String(desc);
    if panel.get("description") == Some(&new) {
        return Ok(false);
    }
    panel["description"] = new;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rewrite_is_marker_keyed_and_idempotent() {
        let mut help = std::collections::BTreeMap::new();
        help.insert("rio_x_y".to_string(), "Canonical help.".to_string());

        // No marker → untouched even with a stale description.
        let mut plain = json!({"title": "t", "description": "stale"});
        assert!(!rewrite_panel_description(&mut plain, &help).unwrap());
        assert_eq!(plain["description"], "stale");

        // Marker → canonical help + note suffix; second run no-op.
        let mut marked = json!({
            "rioMetric": "rio_x_y",
            "rioNote": "operator action.",
            "description": "stale"
        });
        assert!(rewrite_panel_description(&mut marked, &help).unwrap());
        assert_eq!(marked["description"], "Canonical help. — operator action.");
        assert!(!rewrite_panel_description(&mut marked, &help).unwrap());

        // Unknown marker → hard error (a typo'd marker must not pass).
        let mut unknown = json!({"rioMetric": "rio_nope"});
        assert!(rewrite_panel_description(&mut unknown, &help).is_err());
    }

    // merged_bug_098 red (pre-fix): the walk was top-level only — a
    // marker inside a collapsed row's nested panels[] kept its stale
    // description and nothing complained. The tree walk now reaches
    // it and the census counts every marker in the document.
    #[test]
    fn nested_row_panels_are_rewritten_and_censused() {
        let mut help = std::collections::BTreeMap::new();
        help.insert("rio_x_y".to_string(), "Canonical help.".to_string());

        let mut row = json!({
            "type": "row", "collapsed": true,
            "panels": [
                {"rioMetric": "rio_x_y", "description": "stale"},
                {"title": "unmarked"}
            ]
        });
        let mut visited = 0;
        assert!(rewrite_panel_tree(&mut row, &help, &mut visited).unwrap());
        assert_eq!(visited, 1);
        assert_eq!(row["panels"][0]["description"], "Canonical help.");

        let doc = json!({"panels": [row]});
        assert_eq!(count_rio_metric_markers(&doc).unwrap(), 1);
        // A marker the panel walk cannot reach (outside panels[]) is
        // visible to the census — the caller's ensure! catches it.
        let stray = json!({"templating": {"rioMetric": "rio_x_y"}, "panels": []});
        assert_eq!(count_rio_metric_markers(&stray).unwrap(), 1);
    }

    /// merged_bug_056 red: a present-but-non-string marker is a hard
    /// error at EVERY reader — census, walk, and rewrite — never a
    /// counted-but-silently-skipped panel (pre-fix the rewrite
    /// returned Ok(false) for it while census and walk both counted
    /// it, so the visited==markers gate passed with no rewrite and no
    /// error).
    #[test]
    fn non_string_marker_is_a_hard_error_everywhere() {
        let help = std::collections::BTreeMap::from([("rio_x_y".to_owned(), "H.".to_owned())]);
        let mut bad = json!({"rioMetric": 42, "description": "stale"});
        assert!(rewrite_panel_description(&mut bad, &help).is_err());
        let mut visited = 0;
        assert!(rewrite_panel_tree(&mut bad, &help, &mut visited).is_err());
        let doc = json!({"panels": [{"rioMetric": 42}]});
        assert!(count_rio_metric_markers(&doc).is_err());
    }
}

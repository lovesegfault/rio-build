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
        let mut changed = false;
        if let Some(panels) = v.get_mut("panels").and_then(Value::as_array_mut) {
            for panel in panels {
                if rewrite_panel_description(panel, &help)
                    .with_context(|| format!("panel in {}", p.display()))?
                {
                    changed = true;
                }
            }
        }
        if changed {
            fs::write(&p, serde_json::to_string_pretty(&v)? + "\n")?;
            rewritten += 1;
        }
    }
    println!("wrote generated/metric-help.json; rewrote {rewritten} dashboard file(s)");
    Ok(())
}

/// Apply the marker-keyed description rewrite to one panel. Returns
/// whether the panel changed. Idempotent: a second run is a no-op.
fn rewrite_panel_description(
    panel: &mut Value,
    help: &std::collections::BTreeMap<String, String>,
) -> Result<bool> {
    let Some(name) = panel
        .get("rioMetric")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
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
}

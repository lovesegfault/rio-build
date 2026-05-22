//! Regenerate infra/eks/generated.auto.tfvars.json from nix/pins.toml.
//!
//! The pins are plain TOML, so this is a pure parse → sorted-JSON dump
//! with no nix evaluation involved. The output matches `jq -S .` of the
//! flake's `.#tfvars` package (recursively sorted keys, two-space
//! indent, trailing newline), so either path produces identical bytes;
//! the `tfvars-fresh` check compares against the nix-rendered side,
//! which also guards the two parsers (builtins.fromTOML vs the toml
//! crate) against ever disagreeing.

use anyhow::{Context, Result};
use serde_json::Value;

use crate::sh::repo_root;
use crate::ui;

/// Rebuild every JSON object with its keys in sorted order (arrays and
/// scalars pass through unchanged). serde_json preserves insertion
/// order, so inserting in sorted order reproduces `jq -S` exactly.
fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, sort_keys(v)))
                .collect::<std::collections::BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        scalar => scalar,
    }
}

pub async fn run() -> Result<()> {
    ui::step(
        "nix/pins.toml → infra/eks/generated.auto.tfvars.json",
        || async {
            let pins_path = repo_root().join("nix/pins.toml");
            let out_path = repo_root().join("infra/eks/generated.auto.tfvars.json");

            let pins = std::fs::read_to_string(&pins_path)
                .with_context(|| format!("read {}", pins_path.display()))?;
            let pins: toml::Value =
                toml::from_str(&pins).with_context(|| format!("parse {}", pins_path.display()))?;
            let pins = sort_keys(serde_json::to_value(&pins).context("convert pins TOML to JSON")?);

            let mut json =
                serde_json::to_string_pretty(&pins).context("serialize tfvars to JSON")?;
            json.push('\n');
            std::fs::write(&out_path, json)
                .with_context(|| format!("write {}", out_path.display()))?;
            Ok(())
        },
    )
    .await
}

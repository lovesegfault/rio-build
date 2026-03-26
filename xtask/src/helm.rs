//! Helm wrapper with a builder for `upgrade --install`.

use std::fmt;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::sh::{self, cmd, shell};

/// One entry from `helm ls -o json`, enriched with the image tag from
/// `helm get values`. `revision` is a string in helm's output
/// (unlike `helm history` which emits it as a number).
#[derive(Serialize, Deserialize)]
pub struct ReleaseStatus {
    pub name: String,
    pub revision: String,
    pub status: String,
    pub chart: String,
    pub app_version: String,
    #[serde(skip_deserializing)]
    pub image_tag: Option<String>,
}

/// `helm ls -n NS -f ^RELEASE$ -o json`, enriched with the deployed
/// image tag. `None` if the release isn't installed.
pub fn release_status(release: &str, ns: &str) -> Result<Option<ReleaseStatus>> {
    let sh = shell()?;
    let pat = format!("^{release}$");
    let json = sh::read(cmd!(sh, "helm ls -n {ns} -f {pat} -o json"))?;
    let mut list: Vec<ReleaseStatus> = serde_json::from_str(&json)?;
    let Some(mut rel) = list.pop() else {
        return Ok(None);
    };
    // Same best-effort tag enrichment as history_json.
    if let Ok(values) = sh::read(cmd!(sh, "helm get values {release} -n {ns} -o json")) {
        rel.image_tag = serde_json::from_str::<serde_json::Value>(&values)
            .ok()
            .and_then(|v| {
                v.pointer("/global/image/tag")
                    .and_then(|t| t.as_str())
                    .map(String::from)
            });
    }
    Ok(Some(rel))
}

/// One entry from `helm history -o json`, enriched with the image tag
/// from `helm get values --revision`. Display impl formats for the
/// rollback picker: `5 · Upgrade complete · rio-build-0.3.1 · 2c491f0 · 2h ago`.
#[derive(Deserialize)]
pub struct Revision {
    pub revision: u32,
    updated: jiff::Timestamp,
    chart: String,
    description: String,
    #[serde(skip)]
    image_tag: Option<String>,
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use jiff::fmt::friendly::{Designator, SpanPrinter};
        use jiff::{SpanRound, Timestamp, Unit};

        write!(
            f,
            "{} · {} · {}",
            self.revision, self.description, self.chart
        )?;
        if let Some(tag) = &self.image_tag {
            write!(f, " · {tag}")?;
        }
        // Negative span (past→now) so SpanPrinter's Direction::Auto
        // adds the "ago" suffix. Rounded to minutes, hours as largest
        // — Timestamp spans can't use calendar units without a tz.
        let span = self
            .updated
            .since((Unit::Hour, Timestamp::now()))
            .and_then(|s| s.round(SpanRound::new().smallest(Unit::Minute).largest(Unit::Hour)))
            .expect("Unit::Hour is valid for Timestamp spans");
        let age = SpanPrinter::new()
            .designator(Designator::Compact)
            .span_to_string(&span);
        write!(f, " · {age}")
    }
}

/// `helm history -o json`, newest-first, enriched with image tags.
pub fn history_json(release: &str, ns: &str) -> Result<Vec<Revision>> {
    let sh = shell()?;
    let json = sh::read(cmd!(sh, "helm history {release} -n {ns} -o json"))?;
    let mut revs: Vec<Revision> = serde_json::from_str(&json)?;

    // Enrich with image tag (global.image.tag from values). Best-effort
    // — if helm get values fails (revision pruned, network blip), leave
    // the tag blank rather than bailing the whole picker.
    for r in &mut revs {
        let rev = r.revision.to_string();
        if let Ok(values) = sh::read(cmd!(
            sh,
            "helm get values {release} -n {ns} --revision {rev} -o json"
        )) {
            r.image_tag = serde_json::from_str::<serde_json::Value>(&values)
                .ok()
                .and_then(|v| {
                    v.pointer("/global/image/tag")
                        .and_then(|t| t.as_str())
                        .map(String::from)
                });
        }
    }

    revs.reverse(); // newest first — most likely rollback target at top
    Ok(revs)
}

pub struct Helm {
    release: String,
    chart: String,
    namespace: Option<String>,
    create_ns: bool,
    values: Vec<String>,
    sets: Vec<(String, String)>,
    set_jsons: Vec<(String, String)>,
    wait: Option<Duration>,
}

impl Helm {
    pub fn upgrade_install(release: impl Into<String>, chart: impl Into<String>) -> Self {
        Self {
            release: release.into(),
            chart: chart.into(),
            namespace: None,
            create_ns: false,
            values: vec![],
            sets: vec![],
            set_jsons: vec![],
            wait: None,
        }
    }

    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = Some(ns.into());
        self
    }

    pub fn create_namespace(mut self) -> Self {
        self.create_ns = true;
        self
    }

    pub fn values(mut self, file: impl Into<String>) -> Self {
        self.values.push(file.into());
        self
    }

    pub fn set(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.sets.push((key.into(), val.into()));
        self
    }

    pub fn set_json(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.set_jsons.push((key.into(), val.into()));
        self
    }

    pub fn wait(mut self, timeout: Duration) -> Self {
        self.wait = Some(timeout);
        self
    }

    pub fn run(self) -> Result<()> {
        let sh = shell()?;
        let mut args = vec![
            "upgrade".into(),
            "--install".into(),
            self.release,
            self.chart,
        ];
        if let Some(ns) = self.namespace {
            args.extend(["--namespace".into(), ns]);
            if self.create_ns {
                args.push("--create-namespace".into());
            }
        }
        for v in self.values {
            args.extend(["-f".into(), v]);
        }
        for (k, v) in self.sets {
            args.extend(["--set".into(), format!("{k}={v}")]);
        }
        for (k, v) in self.set_jsons {
            args.extend(["--set-json".into(), format!("{k}={v}")]);
        }
        if let Some(t) = self.wait {
            args.extend([
                "--wait".into(),
                "--timeout".into(),
                format!("{}s", t.as_secs()),
            ]);
        }
        sh::run_sync(cmd!(sh, "helm {args...}"))
    }
}

pub fn uninstall(release: &str, namespace: &str) -> Result<()> {
    let sh = shell()?;
    sh::run_sync(cmd!(
        sh,
        "helm uninstall {release} -n {namespace} --ignore-not-found"
    ))
}

pub fn rollback(release: &str, namespace: &str, rev: u32) -> Result<()> {
    let sh = shell()?;
    let rev = rev.to_string();
    sh::run_sync(cmd!(
        sh,
        "helm rollback {release} {rev} --namespace {namespace} --wait --timeout 5m"
    ))
}

pub fn history(release: &str, namespace: &str) -> Result<()> {
    let sh = shell()?;
    // Output IS the deliverable — always show it.
    sh::run_interactive(cmd!(sh, "helm history {release} --namespace {namespace}"))
}

#[cfg(test)]
mod tests {
    use super::Revision;

    fn rev(secs_ago: i64) -> Revision {
        Revision {
            revision: 5,
            updated: jiff::Timestamp::now() - jiff::SignedDuration::from_secs(secs_ago),
            chart: "rio-build-0.3.1".into(),
            description: "Upgrade complete".into(),
            image_tag: Some("2c491f0".into()),
        }
    }

    #[test]
    fn revision_display_friendly_age() {
        assert_eq!(
            rev(9000).to_string(),
            "5 · Upgrade complete · rio-build-0.3.1 · 2c491f0 · 2h 30m ago"
        );
        assert_eq!(
            rev(172_800).to_string(),
            "5 · Upgrade complete · rio-build-0.3.1 · 2c491f0 · 48h ago"
        );
    }
}

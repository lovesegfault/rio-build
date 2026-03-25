//! Helm wrapper with a builder for `upgrade --install`.

use std::time::Duration;

use anyhow::Result;

use crate::sh::{self, cmd, shell};

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

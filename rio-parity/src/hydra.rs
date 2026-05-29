//! hydra.nixos.org API client.
//!
//! Politeness contract: every request carries `Accept: application/json`
//! and a descriptive User-Agent; requests are counted against a hard
//! cap and spaced by a minimum interval. Mass per-path data comes from
//! cache.nixos.org, never from Hydra.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;

/// `GET /eval/<id>` — note: there are NO `project`/`jobset` keys in
/// this response (verified on eval 1824219); they come from a sampled
/// build or an explicit `--jobset` flag.
#[derive(Debug, Clone, Deserialize)]
pub struct HydraEval {
    pub id: u64,
    #[serde(default)]
    pub builds: Vec<u64>,
    #[serde(default)]
    pub jobsetevalinputs: BTreeMap<String, HydraEvalInput>,
    #[serde(default)]
    pub timestamp: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HydraEvalInput {
    #[serde(rename = "type")]
    pub input_type: Option<String>,
    pub revision: Option<String>,
    pub uri: Option<String>,
    pub value: Option<String>,
}

/// `GET /jobset/<project>/<jobset>` — declared inputs + entry point.
#[derive(Debug, Clone, Deserialize)]
pub struct HydraJobset {
    pub project: String,
    pub name: String,
    pub nixexprinput: Option<String>,
    pub nixexprpath: Option<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, HydraJobsetInput>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HydraJobsetInput {
    #[serde(rename = "type")]
    pub input_type: String,
    pub value: Option<String>,
}

/// `GET /build/<id>` and `GET /eval/<id>/job/<name>` share this shape.
/// `buildstatus` is null while the build is still running and an
/// integer code (0 = success) once it has finished.
#[derive(Debug, Clone, Deserialize)]
pub struct HydraBuild {
    pub id: u64,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub jobset: Option<String>,
    pub job: String,
    #[serde(default)]
    pub system: Option<String>,
    pub drvpath: String,
    #[serde(default)]
    pub buildoutputs: BTreeMap<String, HydraBuildOutput>,
    #[serde(default)]
    pub buildstatus: Option<i64>,
    #[serde(default)]
    pub finished: Option<i64>,
    #[serde(default)]
    pub nixname: Option<String>,
    #[serde(default)]
    pub releasename: Option<String>,
    #[serde(default)]
    pub jobsetevals: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HydraBuildOutput {
    pub path: Option<String>,
}

/// Default hard cap on hydra.nixos.org requests per eval-set build. A
/// scoped build needs only a handful of structural requests (the eval,
/// the jobset, a bounded number of sampled per-job lookups), so 150
/// leaves margin while still stopping a runaway loop. The eval command
/// raises it automatically when an explicit job list is larger.
pub const DEFAULT_HYDRA_REQUEST_CAP: u32 = 150;
/// Default spacing between consecutive hydra.nixos.org requests.
pub const DEFAULT_HYDRA_MIN_INTERVAL: Duration = Duration::from_millis(500);

struct BudgetState {
    used: u32,
    last: Option<tokio::time::Instant>,
}

/// hydra.nixos.org client with an enforced politeness budget.
pub struct HydraClient {
    http: reqwest::Client,
    base: reqwest::Url,
    cap: u32,
    min_interval: Duration,
    state: tokio::sync::Mutex<BudgetState>,
}

impl HydraClient {
    pub fn new(
        base_url: &str,
        user_agent: &str,
        cap: u32,
        min_interval: Duration,
    ) -> anyhow::Result<Self> {
        let mut base = base_url.to_string();
        if !base.ends_with('/') {
            base.push('/');
        }
        let http = crate::http_client(user_agent, Duration::from_secs(120))
            .context("build hydra HTTP client")?;
        Ok(Self {
            http,
            base: reqwest::Url::parse(&base).with_context(|| format!("parse hydra URL {base}"))?,
            cap,
            min_interval,
            state: tokio::sync::Mutex::new(BudgetState {
                used: 0,
                last: None,
            }),
        })
    }

    /// Requests issued so far (recorded into the archive provenance's
    /// `stats.hydra_requests_used` audit field).
    pub async fn requests_used(&self) -> u32 {
        self.state.lock().await.used
    }

    /// Charge one request against the budget, enforcing the cap and the
    /// min spacing. The refused request is NOT counted.
    async fn charge(&self) -> anyhow::Result<()> {
        let mut st = self.state.lock().await;
        if st.used >= self.cap {
            anyhow::bail!(
                "hydra politeness budget exhausted ({} requests used, cap {}); \
                 narrow the scope, or raise --hydra-request-cap only if the extra \
                 load on hydra.nixos.org is justified",
                st.used,
                self.cap
            );
        }
        if let Some(last) = st.last {
            let since = last.elapsed();
            if since < self.min_interval {
                tokio::time::sleep(self.min_interval - since).await;
            }
        }
        st.used += 1;
        st.last = Some(tokio::time::Instant::now());
        Ok(())
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        self.charge().await?;
        let url = self
            .base
            .join(path)
            .with_context(|| format!("join {path}"))?;
        tracing::debug!(%url, "hydra GET");
        let resp = self
            .http
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!(
                "GET {url}: HTTP {status}: {}",
                crate::body_snippet(&resp.text().await.unwrap_or_default())
            );
        }
        resp.json::<T>()
            .await
            .with_context(|| format!("parse JSON from {url}"))
    }

    pub async fn get_eval(&self, id: u64) -> anyhow::Result<HydraEval> {
        self.get_json(&format!("eval/{id}")).await
    }

    pub async fn get_jobset(&self, project: &str, jobset: &str) -> anyhow::Result<HydraJobset> {
        self.get_json(&format!("jobset/{project}/{jobset}")).await
    }

    /// `GET /eval/<id>/job/<name>` — the per-job lookup the politeness
    /// pattern allows for single jobs (fidelity samples, scoped sets).
    ///
    /// Job names are Nix attribute paths (e.g.
    /// `nixpkgs.hello.x86_64-linux`) with no characters that need
    /// percent-encoding, so the name is interpolated into the URL path
    /// as-is.
    pub async fn get_eval_job(&self, eval_id: u64, job: &str) -> anyhow::Result<HydraBuild> {
        self.get_json(&format!("eval/{eval_id}/job/{job}")).await
    }

    pub async fn get_build(&self, id: u64) -> anyhow::Result<HydraBuild> {
        self.get_json(&format!("build/{id}")).await
    }

    /// `GET /build/<id>/constituents` for an aggregate build.
    ///
    /// TODO: the response shape (a JSON array of build objects) is the
    /// one endpoint here without a recorded fixture; confirm it against
    /// real hydra.nixos.org before the first constituents-scoped eval
    /// set is built.
    pub async fn get_constituents(&self, build_id: u64) -> anyhow::Result<Vec<HydraBuild>> {
        self.get_json(&format!("build/{build_id}/constituents"))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::http::HeaderMap;

    /// Recorded-fixture directory, resolved through the runtime
    /// manifest dir (see [`crate::test_manifest_dir`]).
    fn fixture_dir() -> std::path::PathBuf {
        crate::test_manifest_dir().join("tests/fixtures/hydra")
    }

    fn fixture(name: &str) -> String {
        let p = fixture_dir().join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read fixture {p:?}: {e}"))
    }

    /// The ~1.6 MB eval response is committed zstd-compressed (the
    /// check-added-large-files pre-commit hook caps files at 500 KB);
    /// decompress at runtime.
    fn fixture_zst(name: &str) -> Vec<u8> {
        let p = fixture_dir().join(name);
        let compressed = std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {p:?}: {e}"));
        zstd::decode_all(compressed.as_slice())
            .unwrap_or_else(|e| panic!("zstd-decompress fixture {p:?}: {e}"))
    }

    #[test]
    fn parses_eval_json() {
        let eval: HydraEval =
            serde_json::from_slice(&fixture_zst("eval-1824219.json.zst")).unwrap();
        assert_eq!(eval.id, 1824219);
        assert_eq!(eval.builds.len(), 161_643);
        let nixpkgs = &eval.jobsetevalinputs["nixpkgs"];
        assert_eq!(
            nixpkgs.revision.as_deref(),
            Some("68d8aa3d661f0e6bd5862291b5bb263b2a6595c9")
        );
        assert_eq!(nixpkgs.input_type.as_deref(), Some("git"));
        assert_eq!(
            nixpkgs.uri.as_deref(),
            Some("https://github.com/nixos/nixpkgs.git")
        );
        // The eval JSON has NO project/jobset keys at all (verified on
        // the recorded eval 1824219) — the type must not require them.
        let stable = &eval.jobsetevalinputs["stableBranch"];
        assert_eq!(stable.value.as_deref(), Some("false"));
    }

    #[test]
    fn parses_jobset_json() {
        let js: HydraJobset = serde_json::from_str(&fixture("jobset-nixos-unstable.json")).unwrap();
        assert_eq!(js.project, "nixos");
        assert_eq!(js.name, "unstable");
        assert_eq!(js.nixexprinput.as_deref(), Some("nixpkgs"));
        assert_eq!(
            js.nixexprpath.as_deref(),
            Some("nixos/release-combined.nix")
        );
        assert_eq!(js.inputs["stableBranch"].input_type, "boolean");
        assert_eq!(js.inputs["stableBranch"].value.as_deref(), Some("false"));
        assert_eq!(js.inputs["nixpkgs"].input_type, "git");
    }

    #[test]
    fn parses_build_json_finished_and_unfinished_fields() {
        let b: HydraBuild =
            serde_json::from_str(&fixture("job-nixpkgs.hello.x86_64-linux.json")).unwrap();
        assert_eq!(b.id, 324433458);
        assert_eq!(b.job, "nixpkgs.hello.x86_64-linux");
        assert_eq!(b.system.as_deref(), Some("x86_64-linux"));
        assert_eq!(
            b.drvpath,
            "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv"
        );
        assert_eq!(b.buildstatus, Some(0));
        assert_eq!(b.finished, Some(1));
        assert_eq!(b.releasename, None, "hello has no releasename");
        assert_eq!(
            b.buildoutputs["out"].path.as_deref(),
            Some("/nix/store/10s5j3mfdg22k1597x580qrhprnzcjwb-hello-2.12.3")
        );

        let chan: HydraBuild = serde_json::from_str(&fixture("job-nixos.channel.json")).unwrap();
        assert_eq!(
            chan.releasename.as_deref(),
            Some("nixos-26.05pre975402.68d8aa3d661f")
        );
        assert_eq!(
            chan.nixname.as_deref(),
            Some("nixos-channel-26.05pre975402.68d8aa3d661f")
        );
    }

    #[test]
    fn unfinished_build_null_buildstatus_is_none() {
        // Hydra serves `"buildstatus": null` while a build is still
        // running; the recorded fixtures only cover finished builds, so
        // pin the null handling with an inline sample.
        let b: HydraBuild = serde_json::from_str(
            r#"{
                "id": 325000000,
                "job": "nixpkgs.hello.x86_64-linux",
                "drvpath": "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv",
                "buildstatus": null,
                "finished": 0
            }"#,
        )
        .unwrap();
        assert_eq!(b.buildstatus, None);
        assert_eq!(b.finished, Some(0));
    }

    /// Loopback fake Hydra serving the recorded fixtures. Returns 406
    /// when the politeness headers are missing so a header regression
    /// fails these tests.
    async fn fixture_response(file: String, headers: HeaderMap) -> axum::response::Response {
        use axum::response::IntoResponse;
        let accept_ok = headers
            .get(axum::http::header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            == Some("application/json");
        let ua_ok = headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ua| ua.starts_with("rio-parity/"));
        if !accept_ok || !ua_ok {
            return (
                axum::http::StatusCode::NOT_ACCEPTABLE,
                "missing Accept: application/json or rio-parity User-Agent",
            )
                .into_response();
        }
        let dir = fixture_dir();
        // The large eval fixture is committed zstd-compressed; serve it
        // decompressed when the plain file is absent (the real Hydra
        // serves plain JSON, so the client never sees zstd).
        let body = match tokio::fs::read(dir.join(&file)).await {
            Ok(body) => Some(body),
            Err(_) => match tokio::fs::read(dir.join(format!("{file}.zst"))).await {
                Ok(z) => Some(zstd::decode_all(z.as_slice()).expect("decompress fixture")),
                Err(_) => None,
            },
        };
        match body {
            Some(body) => (
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response(),
            None => (
                axum::http::StatusCode::NOT_FOUND,
                format!("no fixture {file}"),
            )
                .into_response(),
        }
    }

    async fn h_eval(
        axum::extract::Path(id): axum::extract::Path<u64>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        fixture_response(format!("eval-{id}.json"), headers).await
    }
    async fn h_jobset(
        axum::extract::Path((p, j)): axum::extract::Path<(String, String)>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        fixture_response(format!("jobset-{p}-{j}.json"), headers).await
    }
    async fn h_job(
        axum::extract::Path((_id, job)): axum::extract::Path<(u64, String)>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        fixture_response(format!("job-{job}.json"), headers).await
    }

    async fn spawn_fake_hydra() -> (String, tokio::task::JoinHandle<()>) {
        use axum::routing::get;
        let app = axum::Router::new()
            .route("/eval/{id}", get(h_eval))
            .route("/jobset/{project}/{jobset}", get(h_jobset))
            .route("/eval/{id}/job/{job}", get(h_job));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/"), handle)
    }

    fn test_client(base: &str, cap: u32) -> HydraClient {
        // min_interval=0 so tests don't sleep.
        HydraClient::new(
            base,
            &crate::user_agent(None),
            cap,
            std::time::Duration::ZERO,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn client_fetches_eval_jobset_and_job() {
        let (base, _srv) = spawn_fake_hydra().await;
        let c = test_client(&base, 10);

        let eval = c.get_eval(1824219).await.unwrap();
        assert_eq!(eval.id, 1824219);

        let js = c.get_jobset("nixos", "unstable").await.unwrap();
        assert_eq!(
            js.nixexprpath.as_deref(),
            Some("nixos/release-combined.nix")
        );

        let b = c.get_eval_job(1824219, "nixos.channel").await.unwrap();
        assert_eq!(
            b.drvpath,
            "/nix/store/bim7019bg00n745ycf1zkyk0acchv76b-nixos-channel-26.05pre975402.68d8aa3d661f.drv"
        );
        assert_eq!(c.requests_used().await, 3);
    }

    #[tokio::test]
    async fn budget_cap_is_enforced() {
        let (base, _srv) = spawn_fake_hydra().await;
        let c = test_client(&base, 2);
        c.get_eval(1824219).await.unwrap();
        c.get_jobset("nixos", "unstable").await.unwrap();
        let err = c.get_eval(1824219).await.unwrap_err();
        assert!(
            err.to_string().contains("politeness budget"),
            "expected budget error, got: {err:#}"
        );
        assert_eq!(
            c.requests_used().await,
            2,
            "the refused call must not count"
        );
    }

    /// The default 500 ms min interval must actually delay the second
    /// request. Exercises `charge()` (the hook `get_json` runs before
    /// every request — `client_fetches_eval_jobset_and_job` proves that
    /// wiring) directly under a paused clock, so the assertion is on
    /// virtual time and never entangles auto-advance with real socket
    /// I/O.
    #[tokio::test(start_paused = true)]
    async fn min_interval_delays_second_request() {
        // The base URL is never contacted; only budget bookkeeping runs.
        let c = HydraClient::new(
            "http://hydra.invalid/",
            &crate::user_agent(None),
            10,
            DEFAULT_HYDRA_MIN_INTERVAL,
        )
        .unwrap();
        let start = tokio::time::Instant::now();
        c.charge().await.unwrap();
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "the first request must not be delayed"
        );
        c.charge().await.unwrap();
        assert!(
            start.elapsed() >= DEFAULT_HYDRA_MIN_INTERVAL,
            "the second request must wait out the min interval, elapsed only {:?}",
            start.elapsed()
        );
        assert_eq!(c.requests_used().await, 2);
    }

    #[tokio::test]
    async fn http_errors_name_the_url_and_include_a_body_snippet() {
        let (base, _srv) = spawn_fake_hydra().await;
        let c = test_client(&base, 10);
        // No fixture for this job → fake server returns 404.
        let err = c.get_eval_job(1824219, "no.such.job").await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("404") && msg.contains("no.such.job"),
            "got: {msg}"
        );
        // The response body ("no fixture …") is carried as a snippet so
        // Hydra-side error text is visible without re-running the request.
        assert!(msg.contains("no fixture"), "got: {msg}");
    }
}

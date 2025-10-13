// Nix Store trait implementation for dispatcher

use crate::async_progress::AsyncProgress;
use crate::build_queue::{BuildJob, BuildQueue};
use crate::builder_pool::BuilderPool;
use crate::scheduler::Scheduler;
use anyhow::Context;
use nix_daemon::{
    BuildMode, BuildResult, ClientSettings, Missing, PathInfo, Progress, Stderr, Store,
};
use std::collections::HashMap;
use std::fmt::Debug;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

/// Simple Progress implementation that returns a value immediately
struct SimpleProgress<T, E> {
    result: Option<Result<T, E>>,
}

impl<T: Send, E: From<nix_daemon::Error> + Send + Sync> Progress for SimpleProgress<T, E> {
    type T = T;
    type Error = E;

    async fn next(&mut self) -> Result<Option<Stderr>, Self::Error> {
        // No intermediate progress messages
        Ok(None)
    }

    async fn result(mut self) -> Result<Self::T, Self::Error> {
        self.result.take().expect("result called twice")
    }
}

fn ok<T: Send, E: From<nix_daemon::Error> + Send + Sync>(value: T) -> SimpleProgress<T, E> {
    SimpleProgress {
        result: Some(Ok(value)),
    }
}

fn err<T: Send, E: From<nix_daemon::Error> + Send + Sync>(error: E) -> SimpleProgress<T, E> {
    SimpleProgress {
        result: Some(Err(error)),
    }
}

/// Dispatcher's Nix store implementation
///
/// This implements the Store trait to handle Nix protocol requests from SSH clients.
/// It dispatches build requests to the worker fleet.
#[allow(dead_code)]
pub struct DispatcherStore {
    build_queue: BuildQueue,
    scheduler: Scheduler,
    builder_pool: BuilderPool,
}

#[allow(dead_code)]
impl DispatcherStore {
    pub fn new(build_queue: BuildQueue, scheduler: Scheduler, builder_pool: BuilderPool) -> Self {
        Self {
            build_queue,
            scheduler,
            builder_pool,
        }
    }
}

/// Get path info using nix path-info command
async fn get_path_info(path: &str) -> anyhow::Result<PathInfo> {
    use tokio::process::Command;

    let output = Command::new("nix")
        .args(["path-info", "--json", path])
        .output()
        .await
        .context("Failed to run nix path-info")?;

    if !output.status.success() {
        anyhow::bail!("nix path-info failed for {}", path);
    }

    // Parse JSON output
    let json_str = String::from_utf8(output.stdout).context("Invalid UTF-8 from nix path-info")?;

    #[derive(serde::Deserialize)]
    struct NixPathInfo {
        #[serde(rename = "narSize")]
        nar_size: u64,
        #[serde(rename = "narHash")]
        nar_hash: String,
        #[serde(default)]
        references: Vec<String>,
    }

    let path_infos: Vec<NixPathInfo> =
        serde_json::from_str(&json_str).context("Failed to parse nix path-info JSON")?;

    let path_info = path_infos
        .first()
        .ok_or_else(|| anyhow::anyhow!("No path info returned"))?;

    Ok(PathInfo {
        references: path_info.references.clone(),
        nar_size: path_info.nar_size,
        nar_hash: path_info.nar_hash.clone(),
        ca: None,
        signatures: Vec::new(),
        deriver: None,
        registration_time: chrono::Utc::now(),
        ultimate: false,
    })
}

impl Store for DispatcherStore {
    type Error = anyhow::Error;

    fn is_valid_path<P: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: P,
    ) -> impl Progress<T = bool, Error = Self::Error> {
        let path = path.as_ref().to_string();
        debug!("is_valid_path: {}", path);

        AsyncProgress::new(async move {
            // Check if path exists in local /nix/store
            match tokio::fs::metadata(&path).await {
                Ok(_) => {
                    debug!("Path {} exists in local store", path);
                    Ok(true)
                }
                Err(_) => {
                    debug!("Path {} not found in local store", path);
                    Ok(false)
                }
            }
        })
    }

    fn has_substitutes<P: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: P,
    ) -> impl Progress<T = bool, Error = Self::Error> {
        let path = path.as_ref().to_string();
        debug!("has_substitutes: {}", path);

        // TODO: Check binary caches
        ok(false)
    }

    fn add_to_store<
        SN: AsRef<str> + Send + Sync + Debug,
        SC: AsRef<str> + Send + Sync + Debug,
        Refs,
        R,
    >(
        &mut self,
        name: SN,
        cam_str: SC,
        _refs: Refs,
        repair: bool,
        mut source: R,
    ) -> impl Progress<T = (String, PathInfo), Error = Self::Error>
    where
        Refs: IntoIterator + Send + Debug,
        Refs::IntoIter: ExactSizeIterator + Send,
        Refs::Item: AsRef<str> + Send + Sync,
        R: tokio::io::AsyncRead + Unpin + Send + Debug,
    {
        let name = name.as_ref().to_string();
        let cam_str = cam_str.as_ref().to_string();
        info!(
            "add_to_store: name={}, cam_str={}, repair={}",
            name, cam_str, repair
        );

        AsyncProgress::new(async move {
            use tokio::io::AsyncReadExt;

            // Read NAR data from source
            let mut nar_data = Vec::new();
            source
                .read_to_end(&mut nar_data)
                .await
                .context("Failed to read NAR data from source")?;

            info!("Read {} bytes of NAR data for {}", nar_data.len(), name);

            // Import using nix-store --import
            let mut child = tokio::process::Command::new("nix-store")
                .arg("--import")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .context("Failed to spawn nix-store --import")?;

            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to get stdin"))?;

            stdin
                .write_all(&nar_data)
                .await
                .context("Failed to write NAR data to nix-store")?;
            stdin.flush().await.context("Failed to flush stdin")?;
            drop(stdin);

            let output = child
                .wait_with_output()
                .await
                .context("Failed to wait for nix-store --import")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("nix-store --import failed: {}", stderr);
            }

            // Parse output to get the imported store path
            let stdout = String::from_utf8(output.stdout)
                .context("Invalid UTF-8 from nix-store --import")?;
            let imported_path = stdout.trim().to_string();

            info!("Successfully imported {} to {}", name, imported_path);

            // Get path info for the imported path
            let path_info = get_path_info(&imported_path).await.unwrap_or_else(|e| {
                warn!("Could not get path info for {}: {}", imported_path, e);
                PathInfo {
                    references: Vec::new(),
                    nar_size: nar_data.len() as u64,
                    nar_hash: String::new(),
                    ca: None,
                    signatures: Vec::new(),
                    deriver: None,
                    registration_time: chrono::Utc::now(),
                    ultimate: false,
                }
            });

            Ok((imported_path, path_info))
        })
    }

    fn build_paths<Paths>(
        &mut self,
        paths: Paths,
        mode: BuildMode,
    ) -> impl Progress<T = (), Error = Self::Error>
    where
        Paths: IntoIterator + Send + Debug,
        Paths::IntoIter: ExactSizeIterator + Send,
        Paths::Item: AsRef<str> + Send + Sync,
    {
        info!("build_paths: mode={:?}", mode);

        let paths: Vec<String> = paths.into_iter().map(|p| p.as_ref().to_string()).collect();

        let build_queue = self.build_queue.clone();

        AsyncProgress::new(async move {
            use crate::build_queue::JobStatus;

            let mut job_ids = Vec::new();

            // Enqueue all paths
            for path in paths {
                info!("Enqueuing build for path: {}", path);

                // For now, default to x86_64-linux
                // TODO: Parse derivation file to extract actual platform
                let platform = "x86_64-linux".to_string();

                let job = BuildJob::new(path.clone(), platform);
                let job_id = build_queue.enqueue(job).await;

                info!("Enqueued job {} for {}", job_id, path);
                job_ids.push(job_id);
            }

            // Wait for all jobs to complete
            info!("Waiting for {} job(s) to complete", job_ids.len());
            for job_id in &job_ids {
                match build_queue.wait_for_completion(job_id).await {
                    Some(JobStatus::Completed) => {
                        info!("Job {} completed successfully", job_id);
                    }
                    Some(JobStatus::Failed) => {
                        anyhow::bail!("Job {} failed", job_id);
                    }
                    _ => {
                        anyhow::bail!("Job {} timed out or disappeared", job_id);
                    }
                }
            }

            info!("All {} job(s) completed", job_ids.len());
            Ok(())
        })
    }

    fn ensure_path<Path: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: Path,
    ) -> impl Progress<T = (), Error = Self::Error> {
        let path = path.as_ref().to_string();
        debug!("ensure_path: {}", path);

        // TODO: Ensure path exists
        err(anyhow::anyhow!("ensure_path not yet implemented"))
    }

    fn add_temp_root<Path: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: Path,
    ) -> impl Progress<T = (), Error = Self::Error> {
        let path = path.as_ref().to_string();
        debug!("add_temp_root: {}", path);

        // TODO: Add temp GC root
        ok(())
    }

    fn add_indirect_root<Path: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: Path,
    ) -> impl Progress<T = (), Error = Self::Error> {
        let path = path.as_ref().to_string();
        debug!("add_indirect_root: {}", path);

        // TODO: Add indirect GC root
        ok(())
    }

    fn find_roots(&mut self) -> impl Progress<T = HashMap<String, String>, Error = Self::Error> {
        debug!("find_roots");

        // TODO: Find GC roots
        ok(HashMap::new())
    }

    fn set_options(&mut self, opts: ClientSettings) -> impl Progress<T = (), Error = Self::Error> {
        debug!("set_options: {:?}", opts);

        // TODO: Store client options
        ok(())
    }

    fn query_pathinfo<S: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: S,
    ) -> impl Progress<T = Option<PathInfo>, Error = Self::Error> {
        let path = path.as_ref().to_string();
        debug!("query_pathinfo: {}", path);

        AsyncProgress::new(async move {
            // Check if path exists in local /nix/store
            if tokio::fs::metadata(&path).await.is_ok() {
                // Get path info using nix path-info
                match get_path_info(&path).await {
                    Ok(info) => {
                        debug!("Path {} exists in local store", path);
                        Ok(Some(info))
                    }
                    Err(e) => {
                        debug!("Path {} exists but couldn't get info: {}", path, e);
                        // Path exists but we can't get full info - return minimal PathInfo
                        Ok(Some(PathInfo {
                            references: Vec::new(),
                            nar_size: 0,
                            nar_hash: String::new(),
                            ca: None,
                            signatures: Vec::new(),
                            deriver: None,
                            registration_time: chrono::Utc::now(),
                            ultimate: false,
                        }))
                    }
                }
            } else {
                debug!("Path {} not found in local store", path);
                Ok(None)
            }
        })
    }

    fn query_valid_paths<Paths>(
        &mut self,
        paths: Paths,
        use_substituters: bool,
    ) -> impl Progress<T = Vec<String>, Error = Self::Error>
    where
        Paths: IntoIterator + Send + Debug,
        Paths::IntoIter: ExactSizeIterator + Send,
        Paths::Item: AsRef<str> + Send + Sync,
    {
        debug!("query_valid_paths: use_substituters={}", use_substituters);

        let paths: Vec<String> = paths.into_iter().map(|p| p.as_ref().to_string()).collect();

        AsyncProgress::new(async move {
            let mut valid_paths = Vec::new();

            for path in paths {
                // Check if path exists in local /nix/store
                if tokio::fs::metadata(&path).await.is_ok() {
                    debug!("Path {} is valid", path);
                    valid_paths.push(path);
                } else {
                    debug!("Path {} is not valid", path);
                }
            }

            debug!("Found {} valid paths out of query", valid_paths.len());
            Ok(valid_paths)
        })
    }

    fn query_substitutable_paths<Paths>(
        &mut self,
        _paths: Paths,
    ) -> impl Progress<T = Vec<String>, Error = Self::Error>
    where
        Paths: IntoIterator + Send + Debug,
        Paths::IntoIter: ExactSizeIterator + Send,
        Paths::Item: AsRef<str> + Send + Sync,
    {
        debug!("query_substitutable_paths");

        // TODO: Query substitutable paths
        ok(Vec::new())
    }

    fn query_valid_derivers<S: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: S,
    ) -> impl Progress<T = Vec<String>, Error = Self::Error> {
        let path = path.as_ref().to_string();
        debug!("query_valid_derivers: {}", path);

        // TODO: Query valid derivers
        ok(Vec::new())
    }

    fn query_missing<Ps>(&mut self, _paths: Ps) -> impl Progress<T = Missing, Error = Self::Error>
    where
        Ps: IntoIterator + Send + Debug,
        Ps::IntoIter: ExactSizeIterator + Send,
        Ps::Item: AsRef<str> + Send + Sync,
    {
        debug!("query_missing");

        // TODO: Determine what needs to be built/downloaded
        ok(Missing {
            will_build: Vec::new(),
            will_substitute: Vec::new(),
            unknown: Vec::new(),
            download_size: 0,
            nar_size: 0,
        })
    }

    fn query_derivation_output_map<P: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: P,
    ) -> impl Progress<T = HashMap<String, String>, Error = Self::Error> {
        let path = path.as_ref().to_string();
        debug!("query_derivation_output_map: {}", path);

        // TODO: Parse derivation and return output map
        ok(HashMap::new())
    }

    fn build_paths_with_results<Ps>(
        &mut self,
        _paths: Ps,
        mode: BuildMode,
    ) -> impl Progress<T = HashMap<String, BuildResult>, Error = Self::Error>
    where
        Ps: IntoIterator + Send + Debug,
        Ps::IntoIter: ExactSizeIterator + Send,
        Ps::Item: AsRef<str> + Send + Sync,
    {
        info!("build_paths_with_results: mode={:?}", mode);

        // TODO: Build paths and return detailed results
        err(anyhow::anyhow!(
            "build_paths_with_results not yet implemented"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_queue::BuildQueue;
    use crate::builder_pool::BuilderPool;
    use crate::scheduler::Scheduler;

    #[tokio::test]
    async fn test_is_valid_path_nonexistent() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        let result = store
            .is_valid_path("/nix/store/nonexistent-path-12345")
            .result()
            .await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn test_is_valid_path_existing() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        // Create a temporary file to test
        let test_path = "/tmp/rio-test-valid-path";
        tokio::fs::write(test_path, b"test").await.unwrap();

        let result = store.is_valid_path(test_path).result().await;

        // Cleanup
        let _ = tokio::fs::remove_file(test_path).await;

        assert!(result.unwrap(), "Existing path should be valid");
    }

    #[tokio::test]
    async fn test_has_substitutes_returns_false() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        let result = store.has_substitutes("/nix/store/test").result().await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn test_set_options_succeeds() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        let opts = ClientSettings::default();
        let result = store.set_options(opts).result().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_query_pathinfo_returns_none() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        let result = store.query_pathinfo("/nix/store/test").result().await;
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn test_query_missing_returns_empty() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        let paths = vec!["/nix/store/test1", "/nix/store/test2"];
        let result = store.query_missing(paths).result().await;
        let missing = result.unwrap();

        assert_eq!(missing.will_build.len(), 0);
        assert_eq!(missing.will_substitute.len(), 0);
        assert_eq!(missing.unknown.len(), 0);
    }

    #[tokio::test]
    async fn test_build_paths_returns_ok() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        let paths = vec!["/nix/store/test.drv"];
        let result = store.build_paths(paths, BuildMode::Normal).result().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_query_valid_paths_mixed() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        // Create some test files
        let test_path1 = "/tmp/rio-valid-1";
        let test_path2 = "/tmp/rio-valid-2";
        tokio::fs::write(test_path1, b"test1").await.unwrap();
        tokio::fs::write(test_path2, b"test2").await.unwrap();

        let paths = vec![
            test_path1,
            "/tmp/nonexistent-1",
            test_path2,
            "/tmp/nonexistent-2",
        ];

        let result = store.query_valid_paths(paths, false).result().await;

        // Cleanup
        let _ = tokio::fs::remove_file(test_path1).await;
        let _ = tokio::fs::remove_file(test_path2).await;

        let valid = result.unwrap();
        assert_eq!(valid.len(), 2, "Should find 2 valid paths");
        assert!(valid.contains(&test_path1.to_string()));
        assert!(valid.contains(&test_path2.to_string()));
    }

    #[tokio::test]
    async fn test_add_to_store_with_nar() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        // Create a test derivation and build it to get a real store path
        let test_nix = r#"
derivation {
  name = "rio-add-test";
  system = builtins.currentSystem;
  builder = "/bin/sh";
  args = [ "-c" "echo test > $out" ];
}
"#;

        let nix_file = "/tmp/rio-add-store.nix";
        tokio::fs::write(nix_file, test_nix).await.unwrap();

        // Instantiate and build
        let inst_output = tokio::process::Command::new("nix-instantiate")
            .arg(nix_file)
            .output()
            .await
            .unwrap();

        if !inst_output.status.success() {
            // Skip test if nix not available
            let _ = tokio::fs::remove_file(nix_file).await;
            return;
        }

        let drv_path = String::from_utf8(inst_output.stdout)
            .unwrap()
            .trim()
            .to_string();

        let build_output = tokio::process::Command::new("nix-build")
            .arg(&drv_path)
            .output()
            .await
            .unwrap();

        if !build_output.status.success() {
            let _ = tokio::fs::remove_file(nix_file).await;
            return;
        }

        let store_path = String::from_utf8(build_output.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Export using nix-store --export (not --dump)
        let export_output = tokio::process::Command::new("nix-store")
            .arg("--export")
            .arg(&store_path)
            .output()
            .await
            .unwrap();

        assert!(
            export_output.status.success(),
            "nix-store --export should succeed"
        );

        let nar_export_data = export_output.stdout;

        // Create AsyncRead source from NAR export data
        let source = tokio::io::BufReader::new(&nar_export_data[..]);

        // Try to add to store (should re-import the same path)
        let result = store
            .add_to_store(
                "rio-add-test",
                "sha256",
                Vec::<String>::new(),
                false,
                source,
            )
            .result()
            .await;

        // Cleanup
        let _ = tokio::fs::remove_file(nix_file).await;

        // Should succeed
        if let Err(e) = &result {
            eprintln!("add_to_store error: {}", e);
        }
        assert!(result.is_ok(), "add_to_store should succeed");

        let (imported_path, _path_info) = result.unwrap();
        assert!(!imported_path.is_empty(), "Should return imported path");
        assert!(
            imported_path.starts_with("/nix/store/"),
            "Should be a store path"
        );
    }

    #[tokio::test]
    async fn test_add_to_store_invalid_nar() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());
        let mut store = DispatcherStore::new(queue, scheduler, pool);

        // Create invalid NAR data
        let invalid_nar = b"not valid NAR data";
        let source = tokio::io::BufReader::new(&invalid_nar[..]);

        let result = store
            .add_to_store("test", "sha256", Vec::<String>::new(), false, source)
            .result()
            .await;

        // Should fail
        assert!(result.is_err(), "Invalid NAR data should fail import");
    }
}

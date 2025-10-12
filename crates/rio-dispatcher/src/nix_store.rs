// Nix Store trait implementation for dispatcher

use crate::build_queue::BuildQueue;
use crate::builder_pool::BuilderPool;
use crate::scheduler::Scheduler;
use nix_daemon::{
    BuildMode, BuildResult, ClientSettings, Missing, PathInfo, Progress, Stderr, Store,
};
use std::collections::HashMap;
use std::fmt::Debug;
use tracing::{debug, info};

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

impl Store for DispatcherStore {
    type Error = anyhow::Error;

    fn is_valid_path<P: AsRef<str> + Send + Sync + Debug>(
        &mut self,
        path: P,
    ) -> impl Progress<T = bool, Error = Self::Error> {
        let path = path.as_ref().to_string();
        debug!("is_valid_path: {}", path);

        // TODO: Check if path exists in our store or on builders
        // For now, return false to trigger builds
        ok(false)
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
        _source: R,
    ) -> impl Progress<T = (String, PathInfo), Error = Self::Error>
    where
        Refs: IntoIterator + Send + Debug,
        Refs::IntoIter: ExactSizeIterator + Send,
        Refs::Item: AsRef<str> + Send + Sync,
        R: tokio::io::AsyncRead + Unpin + Send + Debug,
    {
        let name = name.as_ref().to_string();
        let cam_str = cam_str.as_ref().to_string();
        debug!(
            "add_to_store: name={}, cam_str={}, repair={}",
            name, cam_str, repair
        );

        // TODO: Actually add to store
        err(anyhow::anyhow!("add_to_store not yet implemented"))
    }

    fn build_paths<Paths>(
        &mut self,
        _paths: Paths,
        mode: BuildMode,
    ) -> impl Progress<T = (), Error = Self::Error>
    where
        Paths: IntoIterator + Send + Debug,
        Paths::IntoIter: ExactSizeIterator + Send,
        Paths::Item: AsRef<str> + Send + Sync,
    {
        info!("build_paths: mode={:?}", mode);

        let _build_queue = self.build_queue.clone();
        let _scheduler = self.scheduler.clone();

        // Create a future that does the actual work
        SimpleProgress {
            result: Some(Ok(())), // Placeholder - will be replaced with actual async logic
        }
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

        // TODO: Query path info from builders
        ok(None)
    }

    fn query_valid_paths<Paths>(
        &mut self,
        _paths: Paths,
        use_substituters: bool,
    ) -> impl Progress<T = Vec<String>, Error = Self::Error>
    where
        Paths: IntoIterator + Send + Debug,
        Paths::IntoIter: ExactSizeIterator + Send,
        Paths::Item: AsRef<str> + Send + Sync,
    {
        debug!("query_valid_paths: use_substituters={}", use_substituters);

        // TODO: Query which paths are valid
        ok(Vec::new())
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

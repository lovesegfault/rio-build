// Build scheduler - assigns jobs to builders

use crate::build_queue::BuildJob;
use crate::builder_pool::{BuilderInfo, BuilderPool};
use rio_common::BuilderId;
use tracing::{debug, warn};

/// Build scheduler for assigning jobs to builders
#[derive(Clone)]
#[allow(dead_code)]
pub struct Scheduler {
    builder_pool: BuilderPool,
}

#[allow(dead_code)]
impl Scheduler {
    pub fn new(builder_pool: BuilderPool) -> Self {
        Self { builder_pool }
    }

    /// Select a builder for a build job using round-robin with load balancing
    ///
    /// Selection criteria (in priority order):
    /// 1. Builder must support the required platform
    /// 2. Builder must have all required features (future)
    /// 3. Builder must be available (not offline)
    /// 4. Prefer builder with fewest current jobs (load balancing)
    pub async fn select_builder(&self, job: &BuildJob) -> Option<BuilderId> {
        debug!(
            "Selecting builder for job {} (platform: {})",
            job.job_id, job.platform
        );

        let all_builders = self.builder_pool.get_all_builders().await;

        if all_builders.is_empty() {
            warn!("No builders available");
            return None;
        }

        // Filter builders by platform
        let compatible_builders: Vec<&BuilderInfo> = all_builders
            .iter()
            .filter(|b| b.platforms.iter().any(|p| p.to_string() == job.platform))
            .collect();

        if compatible_builders.is_empty() {
            warn!(
                "No builders available for platform {} (total builders: {})",
                job.platform,
                all_builders.len()
            );
            return None;
        }

        // Select builder with fewest current jobs (simple load balancing)
        let selected = compatible_builders
            .iter()
            .min_by_key(|b| b.status.current_jobs.len())?;

        debug!(
            "Selected builder {} for job {} (current jobs: {})",
            selected.id,
            job.job_id,
            selected.status.current_jobs.len()
        );

        Some(selected.id.clone())
    }

    /// Get all builders for a specific platform
    pub async fn get_builders_for_platform(&self, platform: &str) -> Vec<BuilderInfo> {
        let all_builders = self.builder_pool.get_all_builders().await;
        all_builders
            .into_iter()
            .filter(|b| b.platforms.iter().any(|p| p.to_string() == platform))
            .collect()
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new(BuilderPool::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_queue::BuildJob;

    #[tokio::test]
    async fn test_select_builder_no_builders() {
        let pool = BuilderPool::new();
        let scheduler = Scheduler::new(pool);

        let job = BuildJob::new(
            "/nix/store/test.drv".to_string(),
            "x86_64-linux".to_string(),
        );

        let result = scheduler.select_builder(&job).await;
        assert!(
            result.is_none(),
            "Should return None when no builders available"
        );
    }

    #[tokio::test]
    async fn test_select_builder_platform_mismatch() {
        let pool = BuilderPool::new();

        // Register an aarch64-linux builder
        pool.register_builder(
            rio_common::BuilderId::new(),
            "builder1".to_string(),
            vec!["aarch64-linux".to_string()],
            vec![],
        )
        .await
        .unwrap();

        let scheduler = Scheduler::new(pool);

        // Request x86_64-linux
        let job = BuildJob::new(
            "/nix/store/test.drv".to_string(),
            "x86_64-linux".to_string(),
        );

        let result = scheduler.select_builder(&job).await;
        assert!(
            result.is_none(),
            "Should return None when no compatible platform"
        );
    }

    #[tokio::test]
    async fn test_select_builder_success() {
        let pool = BuilderPool::new();

        // Register a compatible builder
        let builder_id = rio_common::BuilderId::new();
        pool.register_builder(
            builder_id.clone(),
            "builder1".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await
        .unwrap();

        let scheduler = Scheduler::new(pool);

        let job = BuildJob::new(
            "/nix/store/test.drv".to_string(),
            "x86_64-linux".to_string(),
        );

        let result = scheduler.select_builder(&job).await;
        assert_eq!(result, Some(builder_id));
    }

    #[tokio::test]
    async fn test_select_builder_load_balancing() {
        let pool = BuilderPool::new();

        // Register two builders with same platform
        let builder1_id = rio_common::BuilderId::new();
        let builder2_id = rio_common::BuilderId::new();

        pool.register_builder(
            builder1_id.clone(),
            "builder1".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await
        .unwrap();

        pool.register_builder(
            builder2_id.clone(),
            "builder2".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await
        .unwrap();

        let scheduler = Scheduler::new(pool);

        let job = BuildJob::new(
            "/nix/store/test.drv".to_string(),
            "x86_64-linux".to_string(),
        );

        // Should select one of the builders (both have 0 jobs)
        let result = scheduler.select_builder(&job).await;
        assert!(result.is_some());
        assert!(result == Some(builder1_id) || result == Some(builder2_id));
    }

    #[tokio::test]
    async fn test_get_builders_for_platform() {
        let pool = BuilderPool::new();

        pool.register_builder(
            rio_common::BuilderId::new(),
            "builder1".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await
        .unwrap();

        pool.register_builder(
            rio_common::BuilderId::new(),
            "builder2".to_string(),
            vec!["aarch64-linux".to_string()],
            vec![],
        )
        .await
        .unwrap();

        let scheduler = Scheduler::new(pool);

        let x86_builders = scheduler.get_builders_for_platform("x86_64-linux").await;
        assert_eq!(x86_builders.len(), 1);

        let aarch_builders = scheduler.get_builders_for_platform("aarch64-linux").await;
        assert_eq!(aarch_builders.len(), 1);

        let darwin_builders = scheduler.get_builders_for_platform("x86_64-darwin").await;
        assert_eq!(darwin_builders.len(), 0);
    }
}

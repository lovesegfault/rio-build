// Build queue management

use rio_common::JobId;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info};

/// Status of a build job
#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum JobStatus {
    Queued,
    Dispatched,
    Building,
    Completed,
    Failed,
}

/// A build job in the queue
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BuildJob {
    pub job_id: JobId,
    pub derivation_path: String,
    pub platform: String,
    pub status: JobStatus,
}

#[allow(dead_code)]
impl BuildJob {
    pub fn new(derivation_path: String, platform: String) -> Self {
        Self {
            job_id: JobId::new(),
            derivation_path,
            platform,
            status: JobStatus::Queued,
        }
    }
}

/// Thread-safe build queue
#[derive(Clone)]
#[allow(dead_code)]
pub struct BuildQueue {
    queue: Arc<Mutex<VecDeque<BuildJob>>>,
    jobs: Arc<RwLock<HashMap<JobId, BuildJob>>>,
}

#[allow(dead_code)]
impl BuildQueue {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            jobs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Add a job to the queue
    #[tracing::instrument(skip(self, job), fields(job_id = %job.job_id, derivation = %job.derivation_path, platform = %job.platform))]
    pub async fn enqueue(&self, job: BuildJob) -> JobId {
        let job_id = job.job_id.clone();
        info!(
            "Enqueuing job {} for derivation {}",
            job_id, job.derivation_path
        );

        // Add to queue
        let mut queue = self.queue.lock().await;
        queue.push_back(job.clone());

        // Track in jobs map
        let mut jobs = self.jobs.write().await;
        jobs.insert(job_id.clone(), job);

        debug!("Queue size: {}", queue.len());
        job_id
    }

    /// Dequeue the next job
    #[tracing::instrument(skip(self))]
    pub async fn dequeue(&self) -> Option<BuildJob> {
        let mut queue = self.queue.lock().await;
        let job = queue.pop_front()?;

        info!(
            "Dequeued job {} for derivation {}",
            job.job_id, job.derivation_path
        );
        debug!("Queue size: {}", queue.len());

        Some(job)
    }

    /// Get current queue size
    pub async fn size(&self) -> usize {
        let queue = self.queue.lock().await;
        queue.len()
    }

    /// Update job status
    #[tracing::instrument(skip(self), fields(job_id = %job_id, status = ?status))]
    pub async fn update_status(&self, job_id: &JobId, status: JobStatus) {
        let mut jobs = self.jobs.write().await;
        if let Some(job) = jobs.get_mut(job_id) {
            debug!(
                "Updating job {} status: {:?} -> {:?}",
                job_id, job.status, status
            );
            job.status = status;
        }
    }

    /// Get job by ID
    pub async fn get_job(&self, job_id: &JobId) -> Option<BuildJob> {
        let jobs = self.jobs.read().await;
        jobs.get(job_id).cloned()
    }

    /// Get all jobs
    pub async fn get_all_jobs(&self) -> Vec<BuildJob> {
        let jobs = self.jobs.read().await;
        jobs.values().cloned().collect()
    }

    /// Wait for a job to reach a terminal status (Completed or Failed)
    ///
    /// Polls the job status with exponential backoff until it reaches a terminal state.
    /// Returns the final job status.
    #[tracing::instrument(skip(self), fields(job_id = %job_id))]
    pub async fn wait_for_completion(&self, job_id: &JobId) -> Option<JobStatus> {
        use tokio::time::{Duration, sleep};

        let mut delay = Duration::from_millis(50);
        let max_delay = Duration::from_secs(1);
        let max_wait = Duration::from_secs(300); // 5 minute timeout
        let start = tokio::time::Instant::now();

        loop {
            // Check if job exists and its status
            if let Some(job) = self.get_job(job_id).await {
                match job.status {
                    JobStatus::Completed | JobStatus::Failed => {
                        debug!("Job {} reached terminal status: {:?}", job_id, job.status);
                        return Some(job.status);
                    }
                    _ => {
                        // Job still in progress
                        debug!(
                            "Job {} still in progress (status: {:?}), waiting...",
                            job_id, job.status
                        );
                    }
                }
            } else {
                // Job not found
                debug!("Job {} not found", job_id);
                return None;
            }

            // Check timeout
            if start.elapsed() > max_wait {
                info!("Job {} wait timed out after {:?}", job_id, max_wait);
                return None;
            }

            // Sleep with exponential backoff
            sleep(delay).await;
            delay = (delay * 2).min(max_delay);
        }
    }
}

impl Default for BuildQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_enqueue_and_dequeue() {
        let queue = BuildQueue::new();

        // Enqueue a job
        let job = BuildJob::new(
            "/nix/store/test.drv".to_string(),
            "x86_64-linux".to_string(),
        );
        let job_id = queue.enqueue(job.clone()).await;

        assert_eq!(queue.size().await, 1);

        // Dequeue the job
        let dequeued = queue.dequeue().await.expect("Should have a job");
        assert_eq!(dequeued.job_id, job_id);
        assert_eq!(dequeued.derivation_path, "/nix/store/test.drv");
        assert_eq!(queue.size().await, 0);
    }

    #[tokio::test]
    async fn test_fifo_order() {
        let queue = BuildQueue::new();

        // Enqueue multiple jobs
        let job1 = BuildJob::new(
            "/nix/store/job1.drv".to_string(),
            "x86_64-linux".to_string(),
        );
        let job2 = BuildJob::new(
            "/nix/store/job2.drv".to_string(),
            "x86_64-linux".to_string(),
        );
        let job3 = BuildJob::new(
            "/nix/store/job3.drv".to_string(),
            "x86_64-linux".to_string(),
        );

        let id1 = queue.enqueue(job1).await;
        let id2 = queue.enqueue(job2).await;
        let id3 = queue.enqueue(job3).await;

        assert_eq!(queue.size().await, 3);

        // Dequeue in FIFO order
        assert_eq!(queue.dequeue().await.unwrap().job_id, id1);
        assert_eq!(queue.dequeue().await.unwrap().job_id, id2);
        assert_eq!(queue.dequeue().await.unwrap().job_id, id3);
        assert_eq!(queue.size().await, 0);
    }

    #[tokio::test]
    async fn test_update_status() {
        let queue = BuildQueue::new();

        let job = BuildJob::new(
            "/nix/store/test.drv".to_string(),
            "x86_64-linux".to_string(),
        );
        let job_id = queue.enqueue(job).await;

        // Initial status should be Queued
        let retrieved = queue.get_job(&job_id).await.unwrap();
        assert_eq!(retrieved.status, JobStatus::Queued);

        // Update status
        queue.update_status(&job_id, JobStatus::Building).await;

        let updated = queue.get_job(&job_id).await.unwrap();
        assert_eq!(updated.status, JobStatus::Building);
    }

    #[tokio::test]
    async fn test_get_all_jobs() {
        let queue = BuildQueue::new();

        let job1 = BuildJob::new(
            "/nix/store/job1.drv".to_string(),
            "x86_64-linux".to_string(),
        );
        let job2 = BuildJob::new(
            "/nix/store/job2.drv".to_string(),
            "aarch64-linux".to_string(),
        );

        queue.enqueue(job1).await;
        queue.enqueue(job2).await;

        let all_jobs = queue.get_all_jobs().await;
        assert_eq!(all_jobs.len(), 2);
    }

    #[tokio::test]
    async fn test_empty_dequeue() {
        let queue = BuildQueue::new();
        assert_eq!(queue.size().await, 0);
        assert!(queue.dequeue().await.is_none());
    }
}

use rio_common::{proto::BuilderStatus, BuilderId, Platform};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

#[derive(Clone)]
pub struct BuilderInfo {
    pub id: BuilderId,
    pub endpoint: String,
    pub platforms: Vec<Platform>,
    pub features: Vec<String>,
    pub status: BuilderStatus,
}

/// Manages the pool of connected builders
#[derive(Clone)]
pub struct BuilderPool {
    builders: Arc<RwLock<HashMap<BuilderId, BuilderInfo>>>,
}

impl BuilderPool {
    pub fn new() -> Self {
        Self {
            builders: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a new builder
    pub async fn register_builder(
        &self,
        id: BuilderId,
        endpoint: String,
        platforms: Vec<String>,
        features: Vec<String>,
    ) -> Result<(), String> {
        let platforms: Vec<Platform> = platforms.iter().map(|s| Platform::from_str(s)).collect();

        info!(
            "Registering builder {} with platforms: {:?}",
            id, platforms
        );

        let builder_info = BuilderInfo {
            id: id.clone(),
            endpoint,
            platforms,
            features,
            status: BuilderStatus::default(),
        };

        let mut builders = self.builders.write().await;
        builders.insert(id.clone(), builder_info);

        info!("Builder {} registered successfully", id);
        Ok(())
    }

    /// Remove a builder from the pool
    pub async fn unregister_builder(&self, id: &BuilderId) {
        let mut builders = self.builders.write().await;
        if builders.remove(id).is_some() {
            info!("Builder {} unregistered", id);
        } else {
            warn!("Attempted to unregister unknown builder {}", id);
        }
    }

    /// Get information about a specific builder
    pub async fn get_builder(&self, id: &BuilderId) -> Option<BuilderInfo> {
        let builders = self.builders.read().await;
        builders.get(id).cloned()
    }

    /// Get all registered builders
    pub async fn get_all_builders(&self) -> Vec<BuilderInfo> {
        let builders = self.builders.read().await;
        builders.values().cloned().collect()
    }

    /// Get builders that support a specific platform
    pub async fn get_builders_for_platform(&self, platform: &Platform) -> Vec<BuilderInfo> {
        let builders = self.builders.read().await;
        builders
            .values()
            .filter(|b| b.platforms.contains(platform))
            .cloned()
            .collect()
    }

    /// Get count of registered builders
    pub async fn builder_count(&self) -> usize {
        let builders = self.builders.read().await;
        builders.len()
    }
}

impl Default for BuilderPool {
    fn default() -> Self {
        Self::new()
    }
}

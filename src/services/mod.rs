//! Services: hyperforge's child activations (repo, workspace, build,
//! images, releases — leaves under the root `hyperforge` service) and the
//! symmetric sync service they share.

pub mod symmetric_sync;

pub mod build;
pub mod images;
pub mod releases;
pub mod repo;
pub mod utils;
pub mod workspace;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::adapters::LocalForge;

pub use build::BuildService;
pub use repo::RepoService;
pub use workspace::WorkspaceService;

/// Shared state for all hyperforge sub-services
#[derive(Clone)]
pub struct HyperforgeState {
    pub sync_service: Arc<SymmetricSyncService>,
    /// Cached `LocalForge` instances per org
    pub local_forges: Arc<RwLock<HashMap<String, Arc<LocalForge>>>>,
    /// Base config directory (~/.config/hyperforge)
    pub config_dir: PathBuf,
}

impl HyperforgeState {
    pub fn new() -> Self {
        let config_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("hyperforge");

        Self::new_with_config_dir(config_dir)
    }

    /// Create a state rooted at an arbitrary config directory.
    ///
    /// Tests use this to isolate disk I/O under a tempdir instead of the
    /// real `~/.config/hyperforge`.
    pub fn new_with_config_dir(config_dir: PathBuf) -> Self {
        Self {
            sync_service: Arc::new(SymmetricSyncService::new()),
            local_forges: Arc::new(RwLock::new(HashMap::new())),
            config_dir,
        }
    }

    /// Get or create `LocalForge` for an org with file persistence
    pub async fn get_local_forge(&self, org: &str) -> Arc<LocalForge> {
        // Try to get existing
        {
            let forges = self.local_forges.read().unwrap();
            if let Some(forge) = forges.get(org) {
                return forge.clone();
            }
        }

        // Create new with persistence
        let yaml_path = self.config_dir.join("orgs").join(org).join("repos.yaml");
        let forge = Arc::new(LocalForge::with_config_path(org, yaml_path));

        // Try to load existing state
        let _ = forge.load_from_yaml().await;

        // Cache it
        {
            let mut forges = self.local_forges.write().unwrap();
            forges.insert(org.to_string(), forge.clone());
        }

        forge
    }

    /// Reload all cached `LocalForge` instances from disk
    pub async fn reload(&self) -> Vec<String> {
        let org_names: Vec<String> = {
            let forges = self.local_forges.read().unwrap();
            forges.keys().cloned().collect()
        };

        let mut reloaded = Vec::new();
        for org in &org_names {
            let yaml_path = self.config_dir.join("orgs").join(org).join("repos.yaml");
            let forge = Arc::new(LocalForge::with_config_path(org, yaml_path));
            let _ = forge.load_from_yaml().await;
            {
                let mut forges = self.local_forges.write().unwrap();
                forges.insert(org.clone(), forge);
            }
            reloaded.push(org.clone());
        }
        reloaded
    }

    /// Evict a cached `LocalForge` instance for an org
    pub async fn evict_org(&self, org: &str) {
        let mut forges = self.local_forges.write().unwrap();
        forges.remove(org);
    }
}

impl Default for HyperforgeState {
    fn default() -> Self {
        Self::new()
    }
}

pub use symmetric_sync::{SymmetricSyncService, SyncDiff, SyncOp};

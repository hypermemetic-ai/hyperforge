use async_trait::async_trait;
use async_stream::stream;
use futures::Stream;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

use plexus_core::plexus::{
    Activation, ChildRouter, PlexusStream, PlexusError,
};
use plexus_macros::activation;

use crate::bridge::{GitRemoteBridge, PulumiBridge, create_registry, create_secret_store};
use crate::storage::{HyperforgePaths, OrgStorage, GlobalConfig, OrgConfig};
use crate::events::{RepoEvent, PackageEvent};
use crate::types::{RepoDetails, PackageSummary, VersionBump};

/// Child router for a specific repository (e.g., org.hypermemetic.repos.substrate)
/// Receives org-level configuration from parent ReposActivation.
pub struct RepoChildRouter {
    paths: Arc<HyperforgePaths>,
    org_name: String,
    repo_name: String,
    /// Organization config passed from parent - avoids reloading from disk
    org_config: OrgConfig,
}

impl RepoChildRouter {
    pub fn new(paths: Arc<HyperforgePaths>, org_name: String, repo_name: String, org_config: OrgConfig) -> Self {
        Self { paths, org_name, repo_name, org_config }
    }

    fn storage(&self) -> OrgStorage {
        OrgStorage::new((*self.paths).clone(), self.org_name.clone())
    }
}

#[activation(
    namespace = "repo",
    version = "1.0.0",
    description = "Repository operations",
    crate_path = "plexus_core"
)]
impl RepoChildRouter {
    /// Show repository details
    #[method(description = "Show repository details")]
    pub async fn show(&self) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let repo_name = self.repo_name.clone();

        stream! {
            match storage.load_repos().await {
                Ok(repos) => {
                    if let Some(config) = repos.repos.get(&repo_name) {
                        let details = RepoDetails {
                            name: repo_name.clone(),
                            description: config.description.clone(),
                            visibility: config.visibility.unwrap_or_default(),
                            forge_urls: std::collections::HashMap::new(),
                        };

                        yield RepoEvent::Details {
                            org_name,
                            repo: details,
                        };
                    } else {
                        yield RepoEvent::Error {
                            org_name,
                            repo_name: Some(repo_name),
                            message: "Repository not found".to_string(),
                        };
                    }
                }
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name,
                        repo_name: Some(repo_name),
                        message: e.to_string(),
                    };
                }
            }
        }
    }

    /// Sync repository to forges
    #[method(
        description = "Sync repository to forges",
        params(
            dry_run = "Preview changes without applying",
            yes = "Skip confirmation prompts"
        )
    )]
    pub async fn sync(&self, dry_run: Option<bool>, yes: Option<bool>) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let org_name = self.org_name.clone();
        let repo_name = self.repo_name.clone();
        let org_config = self.org_config.clone();
        let paths = self.paths.clone();
        let is_dry_run = dry_run.unwrap_or(false);
        let auto_yes = yes.unwrap_or(false);

        stream! {
            yield RepoEvent::SyncStarted {
                org_name: org_name.clone(),
                repo_count: 1,
            };

            // Load global config for workspace bindings (org config comes from parent)
            let global_config = match GlobalConfig::load(&paths).await {
                Ok(cfg) => cfg,
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Failed to load config: {}", e),
                    };
                    return;
                }
            };

            // Org config comes from parent - no need to look it up

            // Load repos config to get this repo's settings
            let storage = OrgStorage::new((*paths).clone(), org_name.clone());
            let repos_config = match storage.load_repos().await {
                Ok(cfg) => cfg,
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Failed to load repos: {}", e),
                    };
                    return;
                }
            };

            let repo_config = match repos_config.repos.get(&repo_name) {
                Some(cfg) => cfg.clone(),
                None => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Repository not found: {}", repo_name),
                    };
                    return;
                }
            };

            // Find workspace paths bound to this org
            let workspace_paths: Vec<PathBuf> = global_config.workspaces
                .iter()
                .filter(|(_, org)| *org == &org_name)
                .map(|(path, _)| path.clone())
                .collect();

            // Get forges for this repo (use repo-specific or org default)
            let org_forges = org_config.forges.all_forges();
            let forges = repo_config.forges.as_ref()
                .unwrap_or(&org_forges);

            // Find local repo path in any workspace
            let local_repo_path = workspace_paths
                .iter()
                .map(|ws| ws.join(&repo_name))
                .find(|path| path.join(".git").exists());

            // Validate git remotes if local repo exists
            if let Some(repo_path) = local_repo_path {
                let git_bridge = GitRemoteBridge::new(
                    repo_path,
                    org_name.clone(),
                    org_config.owner.clone(),
                );

                // Validate/setup remotes
                match git_bridge.setup_forge_remotes(forges, &repo_name).await {
                    Ok(added_remotes) => {
                        // Emit events for any remotes that were added
                        for remote_info in &added_remotes {
                            // Format is "name=url"
                            if let Some((name, url)) = remote_info.split_once('=') {
                                yield RepoEvent::RemoteAdded {
                                    org_name: org_name.clone(),
                                    repo_name: repo_name.clone(),
                                    remote: name.to_string(),
                                    url: url.to_string(),
                                };
                            }
                        }

                        // Emit validation event with all configured remotes
                        let all_remotes: Vec<String> = forges
                            .iter()
                            .map(|f| f.to_string())
                            .collect();

                        yield RepoEvent::RemotesValidated {
                            org_name: org_name.clone(),
                            repo_name: repo_name.clone(),
                            remotes: all_remotes,
                        };
                    }
                    Err(e) => {
                        yield RepoEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message: format!("Failed to setup git remotes: {}", e),
                        };
                        return;
                    }
                }
            }

            // Call Pulumi bridge
            let bridge = PulumiBridge::new(&paths);
            let repos_file = paths.repos_file(&org_name);
            let staged_file = paths.staged_repos_file(&org_name);

            // Select/create stack for this org
            if let Err(e) = bridge.select_stack(&org_name).await {
                yield RepoEvent::Error {
                    org_name: org_name.clone(),
                    repo_name: Some(repo_name.clone()),
                    message: format!("Failed to select Pulumi stack: {}", e),
                };
                return;
            }

            yield RepoEvent::SyncProgress {
                org_name: org_name.clone(),
                repo_name: repo_name.clone(),
                stage: "pulumi".to_string(),
            };

            // Run pulumi preview or up
            use futures::StreamExt;
            use std::pin::Pin;

            let mut pulumi_stream: Pin<Box<dyn Stream<Item = crate::events::PulumiEvent> + Send>> = if is_dry_run {
                Box::pin(bridge.preview(&org_name, &repos_file, &staged_file))
            } else {
                Box::pin(bridge.up(&org_name, &repos_file, &staged_file, auto_yes))
            };

            // Process Pulumi events and convert to RepoEvents
            while let Some(event) = pulumi_stream.next().await {
                match event {
                    crate::events::PulumiEvent::ResourcePlanned { resource_name, .. } |
                    crate::events::PulumiEvent::ResourceApplied { resource_name, .. } => {
                        yield RepoEvent::SyncProgress {
                            org_name: org_name.clone(),
                            repo_name: resource_name,
                            stage: "pulumi".into(),
                        };
                    }
                    crate::events::PulumiEvent::PreviewComplete { creates, .. } |
                    crate::events::PulumiEvent::UpComplete { creates, .. } => {
                        yield RepoEvent::SyncComplete {
                            org_name: org_name.clone(),
                            success: true,
                            synced_count: creates,
                        };
                    }
                    crate::events::PulumiEvent::Error { message } => {
                        yield RepoEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message,
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    /// List packages configured for this repository
    #[method(description = "List packages configured for this repository")]
    pub async fn packages_list(&self) -> impl Stream<Item = PackageEvent> + Send + 'static {
        let org_name = self.org_name.clone();
        let repo_name = self.repo_name.clone();
        let storage = self.storage();

        stream! {
            // Load repos config
            let repos_config = match storage.load_repos().await {
                Ok(cfg) => cfg,
                Err(e) => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: format!("Failed to load repos: {}", e),
                    };
                    return;
                }
            };

            let repo_config = match repos_config.repos.get(&repo_name) {
                Some(cfg) => cfg,
                None => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: format!("Repository not found: {}", repo_name),
                    };
                    return;
                }
            };

            if repo_config.packages.is_empty() {
                yield PackageEvent::NoPackages {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                };
                return;
            }

            // Create package summaries
            let summaries: Vec<PackageSummary> = repo_config.packages
                .iter()
                .map(|pkg| PackageSummary {
                    name: pkg.name.clone(),
                    package_type: pkg.package_type.clone(),
                    path: pkg.path.clone(),
                    registry: pkg.registry().to_string(),
                    local_version: None,
                    published_version: None,
                    needs_publish: false,
                })
                .collect();

            yield PackageEvent::PackageList {
                org_name,
                repo_name,
                packages: summaries,
            };
        }
    }

    /// Show package status (local vs published versions)
    #[method(description = "Show package status (local vs published versions)")]
    pub async fn packages_status(&self) -> impl Stream<Item = PackageEvent> + Send + 'static {
        let org_name = self.org_name.clone();
        let repo_name = self.repo_name.clone();
        let storage = self.storage();
        let paths = self.paths.clone();

        stream! {
            // Load global config for secret provider and workspaces
            let global_config = match GlobalConfig::load(&paths).await {
                Ok(cfg) => cfg,
                Err(e) => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: format!("Failed to load config: {}", e),
                    };
                    return;
                }
            };

            // Load repos config
            let repos_config = match storage.load_repos().await {
                Ok(cfg) => cfg,
                Err(e) => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: format!("Failed to load repos: {}", e),
                    };
                    return;
                }
            };

            let repo_config = match repos_config.repos.get(&repo_name) {
                Some(cfg) => cfg,
                None => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: format!("Repository not found: {}", repo_name),
                    };
                    return;
                }
            };

            if repo_config.packages.is_empty() {
                yield PackageEvent::NoPackages {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                };
                return;
            }

            // Find local repo path in any workspace
            let workspace_paths: Vec<PathBuf> = global_config.workspaces
                .iter()
                .filter(|(_, org)| *org == &org_name)
                .map(|(path, _)| path.clone())
                .collect();

            let local_repo_path = workspace_paths
                .iter()
                .map(|ws| ws.join(&repo_name))
                .find(|path| path.join(".git").exists());

            let repo_path = match local_repo_path {
                Some(p) => p,
                None => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: "Local repository not found in any workspace".to_string(),
                    };
                    return;
                }
            };

            // Read manifest and published versions for each package
            let mut summaries = Vec::new();
            for pkg in &repo_config.packages {
                let registry = create_registry(&pkg.package_type, create_secret_store(&global_config.secret_provider, &org_name));

                let summary = match registry.read_manifest(&repo_path, pkg).await {
                    Ok(s) => s,
                    Err(_e) => {
                        // Create a partial summary with the error noted
                        PackageSummary {
                            name: pkg.name.clone(),
                            package_type: pkg.package_type.clone(),
                            path: pkg.path.clone(),
                            registry: pkg.registry().to_string(),
                            local_version: None,
                            published_version: None,
                            needs_publish: false,
                        }
                    }
                };
                summaries.push(summary);
            }

            yield PackageEvent::PackageStatus {
                org_name,
                repo_name,
                packages: summaries,
            };
        }
    }

    /// Publish packages to registries
    #[method(
        description = "Publish packages to registries",
        params(
            dry_run = "Preview what would be published without actually publishing",
            package_name = "Specific package to publish (publishes all if not specified)",
            bump = "Version bump type: 'patch', 'minor', or 'major' (increments version before publishing)"
        )
    )]
    pub async fn packages_publish(&self, dry_run: Option<bool>, package_name: Option<String>, bump: Option<String>) -> impl Stream<Item = PackageEvent> + Send + 'static {
        let org_name = self.org_name.clone();
        let repo_name = self.repo_name.clone();
        let storage = self.storage();
        let paths = self.paths.clone();
        let is_dry_run = dry_run.unwrap_or(false);
        let version_bump: Option<VersionBump> = bump.as_ref().and_then(|b| b.parse().ok());

        stream! {
            // Load global config for secret provider and workspaces
            let global_config = match GlobalConfig::load(&paths).await {
                Ok(cfg) => cfg,
                Err(e) => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: format!("Failed to load config: {}", e),
                    };
                    return;
                }
            };

            // Load repos config
            let repos_config = match storage.load_repos().await {
                Ok(cfg) => cfg,
                Err(e) => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: format!("Failed to load repos: {}", e),
                    };
                    return;
                }
            };

            let repo_config = match repos_config.repos.get(&repo_name) {
                Some(cfg) => cfg,
                None => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: format!("Repository not found: {}", repo_name),
                    };
                    return;
                }
            };

            if repo_config.packages.is_empty() {
                yield PackageEvent::NoPackages {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                };
                return;
            }

            // Filter packages if a specific one was requested
            let packages_to_publish: Vec<_> = if let Some(ref name) = package_name {
                repo_config.packages
                    .iter()
                    .filter(|p| &p.name == name && p.publish)
                    .collect()
            } else {
                repo_config.packages
                    .iter()
                    .filter(|p| p.publish)
                    .collect()
            };

            if packages_to_publish.is_empty() {
                yield PackageEvent::Error {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                    message: if package_name.is_some() {
                        format!("Package '{}' not found or not configured for publishing", package_name.unwrap())
                    } else {
                        "No packages configured for publishing".to_string()
                    },
                };
                return;
            }

            // Find local repo path
            let workspace_paths: Vec<PathBuf> = global_config.workspaces
                .iter()
                .filter(|(_, org)| *org == &org_name)
                .map(|(path, _)| path.clone())
                .collect();

            let local_repo_path = workspace_paths
                .iter()
                .map(|ws| ws.join(&repo_name))
                .find(|path| path.join(".git").exists());

            let repo_path = match local_repo_path {
                Some(p) => p,
                None => {
                    yield PackageEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        message: "Local repository not found in any workspace".to_string(),
                    };
                    return;
                }
            };

            // Publish each package
            for pkg in packages_to_publish {
                yield PackageEvent::PublishStarted {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                    package_name: pkg.name.clone(),
                    package_type: pkg.package_type.clone(),
                    dry_run: is_dry_run,
                };

                let registry = create_registry(&pkg.package_type, create_secret_store(&global_config.secret_provider, &org_name));

                let result = match registry.publish(&repo_path, pkg, version_bump, is_dry_run).await {
                    Ok(r) => r,
                    Err(e) => {
                        yield PackageEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: repo_name.clone(),
                            message: format!("Failed to publish {}: {}", pkg.name, e),
                        };
                        continue;
                    }
                };

                yield PackageEvent::PublishComplete {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                    result,
                };
            }
        }
    }
}

#[async_trait]
impl ChildRouter for RepoChildRouter {
    fn router_namespace(&self) -> &str {
        &self.repo_name
    }

    async fn router_call(&self, method: &str, params: Value) -> Result<PlexusStream, PlexusError> {
        Activation::call(self, method, params).await
    }

    async fn get_child(&self, _name: &str) -> Option<Box<dyn ChildRouter>> {
        None  // Repos have no children
    }
}

use async_trait::async_trait;
use async_stream::stream;
use futures::Stream;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use plexus_core::plexus::{
    Activation, ChildRouter, PlexusStream, PlexusError,
    ChildSummary,
};
use plexus_macros::activation;

use crate::bridge::{GitRemoteBridge, KeychainBridge};
use crate::storage::{HyperforgePaths, OrgStorage, GlobalConfig, OrgConfig};
use crate::events::{ConvergeResult, DiffStatus, PulumiEvent};
use crate::types::{RepoSummary, RepoConfig, Visibility, Forge};

use super::events::{
    RepoListEvent, RepoCreateEvent, RepoAdoptEvent, RepoSyncEvent,
    RepoRemoveEvent, RepoRefreshEvent, RepoDiffEvent, RepoConvergeEvent,
    RepoCloneEvent,
};

use super::RepoChildRouter;

/// Repository info discovered from a forge API
struct ForgeRepo {
    name: String,
    #[allow(dead_code)]
    url: String,
}

/// Query forge API for list of repositories
async fn query_forge_repos(
    forge: &Forge,
    owner: &str,
    token: &str,
) -> Result<Vec<ForgeRepo>, String> {
    let client = reqwest::Client::new();

    match forge {
        Forge::GitHub => {
            let url = format!("https://api.github.com/users/{}/repos?per_page=100", owner);
            let response = client
                .get(&url)
                .header("Authorization", format!("Bearer {}", token))
                .header("User-Agent", "hyperforge")
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|e| e.to_string())?;

            if !response.status().is_success() {
                return Err(format!("API returned {}", response.status()));
            }

            let repos: Vec<serde_json::Value> = response
                .json()
                .await
                .map_err(|e| e.to_string())?;

            Ok(repos.iter().filter_map(|r| {
                Some(ForgeRepo {
                    name: r.get("name")?.as_str()?.to_string(),
                    url: r.get("html_url")?.as_str()?.to_string(),
                })
            }).collect())
        }
        Forge::Codeberg => {
            let url = format!("https://codeberg.org/api/v1/users/{}/repos", owner);
            let response = client
                .get(&url)
                .header("Authorization", format!("token {}", token))
                .send()
                .await
                .map_err(|e| e.to_string())?;

            if !response.status().is_success() {
                return Err(format!("API returned {}", response.status()));
            }

            let repos: Vec<serde_json::Value> = response
                .json()
                .await
                .map_err(|e| e.to_string())?;

            Ok(repos.iter().filter_map(|r| {
                Some(ForgeRepo {
                    name: r.get("name")?.as_str()?.to_string(),
                    url: r.get("html_url")?.as_str()?.to_string(),
                })
            }).collect())
        }
        Forge::GitLab => {
            Err("GitLab refresh not yet implemented".into())
        }
    }
}

pub struct ReposActivation {
    paths: Arc<HyperforgePaths>,
    org_name: String,
    /// Organization configuration passed from parent - avoids reloading from disk
    org_config: OrgConfig,
}

impl ReposActivation {
    pub fn new(paths: Arc<HyperforgePaths>, org_name: String, org_config: OrgConfig) -> Self {
        Self { paths, org_name, org_config }
    }

    fn storage(&self) -> OrgStorage {
        OrgStorage::new((*self.paths).clone(), self.org_name.clone())
    }
}

#[activation(
    namespace = "repos",
    version = "1.0.0",
    description = "Repository management",
    crate_path = "plexus_core",
    service
)]
impl ReposActivation {
    /// List repositories in this organization
    #[method(
        description = "List repositories",
        params(staged = "Show staged repos instead of committed")
    )]
    pub async fn list(&self, staged: Option<bool>) -> impl Stream<Item = RepoListEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let show_staged = staged.unwrap_or(false);

        stream! {
            let config = if show_staged {
                storage.load_staged().await
            } else {
                storage.load_repos().await
            };

            match config {
                Ok(repos_config) => {
                    let repos: Vec<RepoSummary> = repos_config.repos
                        .iter()
                        .filter(|(_, cfg)| !cfg.delete)
                        .map(|(name, cfg)| RepoSummary {
                            name: name.clone(),
                            visibility: cfg.visibility.unwrap_or_default(),
                            forges: cfg.forges.clone().unwrap_or_default(),
                            synced: !show_staged,
                        })
                        .collect();

                    yield RepoListEvent::Listed {
                        org_name,
                        repos,
                        staged: show_staged,
                    };
                }
                Err(e) => {
                    yield RepoListEvent::Error {
                        org_name,
                        repo_name: None,
                        message: e.to_string(),
                    };
                }
            }
        }
    }

    /// Create/update a repository configuration
    #[method(
        description = "Create or update a repository",
        params(
            repo_name = "Repository name",
            description = "Repository description",
            visibility = "public or private",
            forges = "Comma-separated forge list",
            init_local = "Initialize local git repo with .gitignore",
            path = "Local path for init_local (defaults to workspace path)"
        )
    )]
    pub async fn create(
        &self,
        repo_name: String,
        description: Option<String>,
        visibility: Option<String>,
        forges: Option<String>,
        init_local: Option<bool>,
        path: Option<String>,
    ) -> impl Stream<Item = RepoCreateEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let org_config = self.org_config.clone();
        let paths = self.paths.clone();
        let should_init = init_local.unwrap_or(false);
        let custom_path = path;

        stream! {
            // Parse visibility
            let vis = match visibility.as_deref() {
                Some("private") => Some(Visibility::Private),
                Some("public") => Some(Visibility::Public),
                None => None,
                Some(v) => {
                    yield RepoCreateEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Invalid visibility: {}", v),
                    };
                    return;
                }
            };

            // Parse forges
            let forge_list: Option<Vec<Forge>> = match forges {
                Some(f) => {
                    let parsed: Result<Vec<Forge>, _> = f
                        .split(',')
                        .map(|s| s.trim().parse())
                        .collect();

                    match parsed {
                        Ok(list) => Some(list),
                        Err(e) => {
                            yield RepoCreateEvent::Error {
                                org_name: org_name.clone(),
                                repo_name: Some(repo_name.clone()),
                                message: e,
                            };
                            return;
                        }
                    }
                }
                None => {
                    // Use org defaults from parent-injected config
                    Some(org_config.forges.all_forges())
                }
            };

            let config = RepoConfig {
                description: description.clone(),
                visibility: vis,
                forges: forge_list.clone(),
                protected: false,
                delete: false,
                synced: None,
                discovered: None,
                packages: vec![],
                build: None,
            };

            match storage.stage_repo(repo_name.clone(), config).await {
                Ok(()) => {
                    yield RepoCreateEvent::Staged {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                    };
                }
                Err(e) => {
                    yield RepoCreateEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: e.to_string(),
                    };
                    return;
                }
            }

            // Initialize local repo if requested
            if should_init {
                // Determine the local path
                let local_path = if let Some(p) = custom_path {
                    PathBuf::from(p).join(&repo_name)
                } else {
                    // Find workspace path for this org
                    let global_config = match GlobalConfig::load(&paths).await {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            yield RepoCreateEvent::Error {
                                org_name: org_name.clone(),
                                repo_name: Some(repo_name.clone()),
                                message: format!("Failed to load config: {}", e),
                            };
                            return;
                        }
                    };

                    // Find first workspace bound to this org
                    let workspace_path = global_config.workspaces
                        .iter()
                        .find(|(_, org)| *org == &org_name)
                        .map(|(path, _)| path.clone());

                    match workspace_path {
                        Some(ws) => ws.join(&repo_name),
                        None => {
                            yield RepoCreateEvent::Error {
                                org_name: org_name.clone(),
                                repo_name: Some(repo_name.clone()),
                                message: "No workspace bound to this org. Use --path to specify location.".to_string(),
                            };
                            return;
                        }
                    }
                };

                // Check if directory already exists
                if local_path.exists() {
                    yield RepoCreateEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Directory already exists: {}", local_path.display()),
                    };
                    return;
                }

                // Create directory
                if let Err(e) = tokio::fs::create_dir_all(&local_path).await {
                    yield RepoCreateEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Failed to create directory: {}", e),
                    };
                    return;
                }

                // Run git init
                let git_init = tokio::process::Command::new("git")
                    .current_dir(&local_path)
                    .args(["init"])
                    .output()
                    .await;

                match git_init {
                    Ok(output) if output.status.success() => {
                        yield RepoCreateEvent::LocalInitialized {
                            org_name: org_name.clone(),
                            repo_name: repo_name.clone(),
                            path: local_path.clone(),
                        };
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        yield RepoCreateEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message: format!("git init failed: {}", stderr),
                        };
                        return;
                    }
                    Err(e) => {
                        yield RepoCreateEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message: format!("Failed to run git: {}", e),
                        };
                        return;
                    }
                }

                // Write .gitignore
                let gitignore_path = local_path.join(".gitignore");
                if let Err(e) = tokio::fs::write(&gitignore_path, crate::templates::DEFAULT_GITIGNORE).await {
                    yield RepoCreateEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Failed to write .gitignore: {}", e),
                    };
                    return;
                }

                yield RepoCreateEvent::GitignoreCreated {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                    path: gitignore_path,
                };

                // Set up hyperforge.org and core.sshCommand
                let git_bridge = GitRemoteBridge::new(
                    local_path.clone(),
                    org_name.clone(),
                    org_config.owner.clone(),
                );

                if let Err(e) = git_bridge.ensure_ssh_config().await {
                    yield RepoCreateEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Failed to configure SSH: {}", e),
                    };
                    // Non-fatal, continue
                }

                // Set up remotes for configured forges
                let forges_to_setup = forge_list.unwrap_or_else(|| org_config.forges.all_forges());
                match git_bridge.setup_forge_remotes(&forges_to_setup, &repo_name).await {
                    Ok(added) => {
                        for remote_info in added {
                            if let Some((name, url)) = remote_info.split_once('=') {
                                yield RepoCreateEvent::RemoteAdded {
                                    org_name: org_name.clone(),
                                    repo_name: repo_name.clone(),
                                    remote: name.to_string(),
                                    url: url.to_string(),
                                };
                            }
                        }
                    }
                    Err(e) => {
                        yield RepoCreateEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message: format!("Failed to setup remotes: {}", e),
                        };
                        // Non-fatal, continue
                    }
                }

                yield RepoCreateEvent::LocalSetupComplete {
                    org_name,
                    repo_name,
                    path: local_path,
                };
            }
        }
    }

    /// Adopt an existing directory as a hyperforge-managed repository
    #[method(
        description = "Adopt an existing directory as a hyperforge repository",
        params(
            repo_name = "Repository name",
            path = "Path to the existing directory",
            git_init = "Initialize as git repository if not already one",
            scan_packages = "Automatically detect and configure packages",
            description = "Repository description",
            visibility = "Repository visibility: 'public' or 'private'",
            forges = "Comma-separated list of forges (e.g., 'github,codeberg')"
        )
    )]
    pub async fn adopt(
        &self,
        repo_name: String,
        path: String,
        git_init: Option<bool>,
        scan_packages: Option<bool>,
        description: Option<String>,
        visibility: Option<String>,
        forges: Option<String>,
    ) -> impl Stream<Item = RepoAdoptEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let org_config = self.org_config.clone();
        let should_git_init = git_init.unwrap_or(false);
        let should_scan = scan_packages.unwrap_or(true);

        stream! {
            // Parse path
            let repo_path = PathBuf::from(&path);

            // Check if path exists
            if !repo_path.exists() {
                yield RepoAdoptEvent::Error {
                    org_name: org_name.clone(),
                    repo_name: Some(repo_name.clone()),
                    message: format!("Path does not exist: {}", path),
                };
                return;
            }

            if !repo_path.is_dir() {
                yield RepoAdoptEvent::Error {
                    org_name: org_name.clone(),
                    repo_name: Some(repo_name.clone()),
                    message: format!("Path is not a directory: {}", path),
                };
                return;
            }

            yield RepoAdoptEvent::Started {
                org_name: org_name.clone(),
                repo_name: repo_name.clone(),
                path: repo_path.clone(),
            };

            let git_dir = repo_path.join(".git");
            let is_git_repo = git_dir.exists();

            // Initialize git if requested and not already a repo
            if should_git_init && !is_git_repo {
                use tokio::process::Command;

                let output = Command::new("git")
                    .arg("init")
                    .current_dir(&repo_path)
                    .output()
                    .await;

                match output {
                    Ok(out) if out.status.success() => {
                        yield RepoAdoptEvent::GitInitialized {
                            org_name: org_name.clone(),
                            repo_name: repo_name.clone(),
                            path: repo_path.clone(),
                        };
                    }
                    Ok(out) => {
                        yield RepoAdoptEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message: format!("git init failed: {}", String::from_utf8_lossy(&out.stderr)),
                        };
                        return;
                    }
                    Err(e) => {
                        yield RepoAdoptEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message: format!("Failed to run git init: {}", e),
                        };
                        return;
                    }
                }
            }

            // Detect git remotes if it's a git repo
            let mut detected_forges = Vec::new();
            if is_git_repo || should_git_init {
                use tokio::process::Command;

                if let Ok(output) = Command::new("git")
                    .args(["remote", "-v"])
                    .current_dir(&repo_path)
                    .output()
                    .await
                {
                    if output.status.success() {
                        let remotes = String::from_utf8_lossy(&output.stdout);
                        for line in remotes.lines() {
                            if line.contains("github.com") && !detected_forges.contains(&Forge::GitHub) {
                                detected_forges.push(Forge::GitHub);
                            } else if line.contains("codeberg.org") && !detected_forges.contains(&Forge::Codeberg) {
                                detected_forges.push(Forge::Codeberg);
                            }
                        }

                        if !detected_forges.is_empty() {
                            yield RepoAdoptEvent::RemotesDetected {
                                org_name: org_name.clone(),
                                repo_name: repo_name.clone(),
                                forges: detected_forges.clone(),
                            };
                        }
                    }
                }
            }

            // Scan for packages if requested
            let mut packages = Vec::new();
            if should_scan {
                use crate::types::{PackageConfig, PackageType};

                // Check for Cargo.toml
                if repo_path.join("Cargo.toml").exists() {
                    if let Ok(content) = tokio::fs::read_to_string(repo_path.join("Cargo.toml")).await {
                        if let Ok(toml) = content.parse::<toml_edit::DocumentMut>() {
                            if let Some(name) = toml.get("package")
                                .and_then(|p: &toml_edit::Item| p.get("name"))
                                .and_then(|n: &toml_edit::Item| n.as_str())
                            {
                                packages.push(PackageConfig {
                                    name: name.to_string(),
                                    package_type: PackageType::Crate,
                                    path: PathBuf::from("."),
                                    registry: None,
                                    publish: true,
                                    publish_command: None,
                                });
                            }
                        }
                    }
                }

                // Check for package.json
                if repo_path.join("package.json").exists() {
                    if let Ok(content) = tokio::fs::read_to_string(repo_path.join("package.json")).await {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                            if let Some(name) = json.get("name").and_then(|n| n.as_str()) {
                                packages.push(PackageConfig {
                                    name: name.to_string(),
                                    package_type: PackageType::Npm,
                                    path: PathBuf::from("."),
                                    registry: None,
                                    publish: true,
                                    publish_command: None,
                                });
                            }
                        }
                    }
                }

                // Check for mix.exs
                if repo_path.join("mix.exs").exists() {
                    if let Ok(content) = tokio::fs::read_to_string(repo_path.join("mix.exs")).await {
                        // Simple regex to extract app name
                        if let Ok(re) = regex::Regex::new(r#"app:\s*:(\w+)"#) {
                            if let Some(caps) = re.captures(&content) {
                                if let Some(name) = caps.get(1) {
                                    packages.push(PackageConfig {
                                        name: name.as_str().to_string(),
                                        package_type: PackageType::Hex,
                                        path: PathBuf::from("."),
                                        registry: None,
                                        publish: true,
                                        publish_command: None,
                                    });
                                }
                            }
                        }
                    }
                }

                // Check for pyproject.toml or setup.py
                let has_pyproject = repo_path.join("pyproject.toml").exists();
                let has_setup_py = repo_path.join("setup.py").exists();

                if has_pyproject {
                    if let Ok(content) = tokio::fs::read_to_string(repo_path.join("pyproject.toml")).await {
                        if let Ok(toml) = content.parse::<toml_edit::DocumentMut>() {
                            let name = toml.get("project")
                                .and_then(|p: &toml_edit::Item| p.get("name"))
                                .and_then(|n: &toml_edit::Item| n.as_str())
                                .or_else(|| {
                                    toml.get("tool")
                                        .and_then(|t: &toml_edit::Item| t.get("poetry"))
                                        .and_then(|p: &toml_edit::Item| p.get("name"))
                                        .and_then(|n: &toml_edit::Item| n.as_str())
                                });

                            if let Some(n) = name {
                                packages.push(PackageConfig {
                                    name: n.to_string(),
                                    package_type: PackageType::PyPi,
                                    path: PathBuf::from("."),
                                    registry: None,
                                    publish: true,
                                    publish_command: None,
                                });
                            }
                        }
                    }
                } else if has_setup_py {
                    if let Ok(content) = tokio::fs::read_to_string(repo_path.join("setup.py")).await {
                        if let Ok(re) = regex::Regex::new(r#"name\s*=\s*["']([^"']+)["']"#) {
                            if let Some(caps) = re.captures(&content) {
                                if let Some(name) = caps.get(1) {
                                    packages.push(PackageConfig {
                                        name: name.as_str().to_string(),
                                        package_type: PackageType::PyPi,
                                        path: PathBuf::from("."),
                                        registry: None,
                                        publish: true,
                                        publish_command: None,
                                    });
                                }
                            }
                        }
                    }
                }

                if !packages.is_empty() {
                    yield RepoAdoptEvent::PackagesDetected {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        packages: packages.clone(),
                    };
                }
            }

            // Parse visibility
            let vis = match visibility.as_deref() {
                Some("private") => Some(Visibility::Private),
                Some("public") => Some(Visibility::Public),
                None => None,
                Some(v) => {
                    yield RepoAdoptEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Invalid visibility: {}", v),
                    };
                    return;
                }
            };

            // Parse forges - use detected if not specified
            let forge_list: Option<Vec<Forge>> = match forges {
                Some(f) => {
                    let parsed: Result<Vec<Forge>, _> = f
                        .split(',')
                        .map(|s| s.trim().parse())
                        .collect();

                    match parsed {
                        Ok(list) => Some(list),
                        Err(e) => {
                            yield RepoAdoptEvent::Error {
                                org_name: org_name.clone(),
                                repo_name: Some(repo_name.clone()),
                                message: e,
                            };
                            return;
                        }
                    }
                }
                None if !detected_forges.is_empty() => {
                    Some(detected_forges)
                }
                None => {
                    // Use org defaults
                    Some(org_config.forges.all_forges())
                }
            };

            // Create repo config
            let config = RepoConfig {
                description: description.clone(),
                visibility: vis,
                forges: forge_list,
                protected: false,
                delete: false,
                synced: None,
                discovered: None,
                packages,
                build: None,
            };

            // Stage the repo
            match storage.stage_repo(repo_name.clone(), config).await {
                Ok(()) => {
                    yield RepoAdoptEvent::Complete {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        path: repo_path,
                    };
                }
                Err(e) => {
                    yield RepoAdoptEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Failed to adopt repo: {}", e),
                    };
                }
            }
        }
    }

    /// Sync repositories to forges
    #[method(
        description = "Sync repositories to forges via Pulumi",
        params(
            repo_name = "Specific repo to sync (optional, syncs all if omitted)",
            dry_run = "Preview changes without applying",
            yes = "Skip confirmation prompts"
        )
    )]
    pub async fn sync(
        &self,
        repo_name: Option<String>,
        dry_run: Option<bool>,
        yes: Option<bool>,
    ) -> impl Stream<Item = RepoSyncEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let org_config = self.org_config.clone();
        let paths = self.paths.clone();
        let is_dry_run = dry_run.unwrap_or(false);
        let auto_yes = yes.unwrap_or(false);
        let target_repo = repo_name;

        stream! {
            // Load global config for workspace bindings (org config comes from parent)
            let global_config = match GlobalConfig::load(&paths).await {
                Ok(cfg) => cfg,
                Err(e) => {
                    yield RepoSyncEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: None,
                        message: format!("Failed to load config: {}", e),
                    };
                    return;
                }
            };

            // Merge staged into committed
            let repos_config = match storage.merge_staged().await {
                Ok(cfg) => cfg,
                Err(e) => {
                    yield RepoSyncEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: None,
                        message: e.to_string(),
                    };
                    return;
                }
            };

            // Filter repos if target specified
            let repos_to_sync: Vec<_> = repos_config.repos
                .iter()
                .filter(|(name, _)| {
                    match &target_repo {
                        Some(target) => name.as_str() == target.as_str(),
                        None => true,
                    }
                })
                .collect();

            // Validate target exists if specified
            if let Some(ref target) = target_repo {
                if repos_to_sync.is_empty() {
                    yield RepoSyncEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(target.clone()),
                        message: format!("Repository not found: {}", target),
                    };
                    return;
                }
            }

            let repo_count = repos_to_sync.len();

            // Determine which forges should be synced vs skipped
            // Origin forge is always synced regardless of the sync flag
            let all_forges = org_config.forges.all_forges();
            let synced_forges = org_config.forges.synced_forges();
            let origin_forge = &org_config.origin;

            // Emit warnings for skipped forges (except origin which is always synced)
            for forge in &all_forges {
                if !synced_forges.contains(forge) && forge != origin_forge {
                    yield RepoSyncEvent::ForgeSkipped {
                        org_name: org_name.clone(),
                        forge: forge.clone(),
                        reason: "sync: false in org config".to_string(),
                    };
                }
            }

            // Calculate effective synced forges (synced + origin)
            let mut effective_synced_forges: Vec<Forge> = synced_forges.clone();
            if !effective_synced_forges.contains(origin_forge) {
                effective_synced_forges.push(origin_forge.clone());
            }

            yield RepoSyncEvent::Started {
                org_name: org_name.clone(),
                repo_count,
            };

            // Find workspace paths bound to this org
            let workspace_paths: Vec<PathBuf> = global_config.workspaces
                .iter()
                .filter(|(_, org)| *org == &org_name)
                .map(|(path, _)| path.clone())
                .collect();

            // Validate git remotes for each repo that exists locally
            for (repo_name, repo_config) in &repos_to_sync {
                if repo_config.delete {
                    continue;
                }

                // Find local repo path in any workspace
                let local_repo_path = workspace_paths
                    .iter()
                    .map(|ws| ws.join(repo_name))
                    .find(|path| path.join(".git").exists());

                if let Some(repo_path) = local_repo_path {
                    // Get forges for this repo (use repo-specific or org default)
                    let org_forges = org_config.forges.all_forges();
                    let forges = repo_config.forges.as_ref()
                        .unwrap_or(&org_forges);

                    let git_bridge = GitRemoteBridge::new(
                        repo_path,
                        org_name.clone(),
                        org_config.owner.clone(),
                    );

                    // Validate/setup remotes
                    match git_bridge.setup_forge_remotes(forges, repo_name).await {
                        Ok(added_remotes) => {
                            // Emit events for any remotes that were added
                            for remote_info in &added_remotes {
                                // Format is "name=url"
                                if let Some((name, url)) = remote_info.split_once('=') {
                                    yield RepoSyncEvent::RemoteAdded {
                                        org_name: org_name.clone(),
                                        repo_name: repo_name.to_string(),
                                        remote: name.to_string(),
                                        url: url.to_string(),
                                    };
                                }
                            }

                            // Always emit validation event with all configured remotes
                            let all_remotes: Vec<String> = forges
                                .iter()
                                .map(|f| f.to_string())
                                .collect();

                            yield RepoSyncEvent::RemotesValidated {
                                org_name: org_name.clone(),
                                repo_name: repo_name.to_string(),
                                remotes: all_remotes,
                            };
                        }
                        Err(e) => {
                            yield RepoSyncEvent::Error {
                                org_name: org_name.clone(),
                                repo_name: Some(repo_name.to_string()),
                                message: format!("Failed to setup git remotes: {}", e),
                            };
                            // Continue with other repos even if one fails
                        }
                    }
                }
            }

            let repos_file = paths.repos_file(&org_name);
            let staged_file = paths.staged_repos_file(&org_name);

            let bridge = crate::bridge::PulumiBridge::new(&paths);

            // Select/create stack for this org
            if let Err(e) = bridge.select_stack(&org_name).await {
                yield RepoSyncEvent::Error {
                    org_name: org_name.clone(),
                    repo_name: None,
                    message: format!("Failed to select Pulumi stack: {}", e),
                };
                return;
            }

            // Convert PulumiEvent to RepoEvent
            use futures::StreamExt;
            use std::pin::Pin;

            let mut pulumi_stream: Pin<Box<dyn Stream<Item = crate::events::PulumiEvent> + Send>> = if is_dry_run {
                Box::pin(bridge.preview(&org_name, &repos_file, &staged_file))
            } else {
                Box::pin(bridge.up(&org_name, &repos_file, &staged_file, auto_yes))
            };

            let mut up_success = false;

            while let Some(event) = pulumi_stream.next().await {
                match event {
                    crate::events::PulumiEvent::PreviewStarted { .. } |
                    crate::events::PulumiEvent::UpStarted { .. } => {
                        yield RepoSyncEvent::Started {
                            org_name: org_name.clone(),
                            repo_count: 0, // Unknown at this point
                        };
                    }
                    crate::events::PulumiEvent::ResourcePlanned { resource_name, .. } |
                    crate::events::PulumiEvent::ResourceApplied { resource_name, .. } => {
                        yield RepoSyncEvent::Progress {
                            org_name: org_name.clone(),
                            repo_name: resource_name,
                            stage: "pulumi".into(),
                        };
                    }
                    crate::events::PulumiEvent::PreviewComplete { creates, .. } => {
                        yield RepoSyncEvent::Complete {
                            org_name: org_name.clone(),
                            success: true,
                            synced_count: creates,
                        };
                    }
                    crate::events::PulumiEvent::UpComplete { success, creates, .. } => {
                        up_success = success;
                        yield RepoSyncEvent::Complete {
                            org_name: org_name.clone(),
                            success,
                            synced_count: creates,
                        };
                    }
                    crate::events::PulumiEvent::Error { message } => {
                        yield RepoSyncEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: None,
                            message,
                        };
                    }
                    _ => {}
                }
            }

            // Capture outputs after successful apply (not dry_run)
            if !is_dry_run && up_success {
                match bridge.get_outputs(&org_name).await {
                    Ok(outputs) => {
                        for (repo_name, repo_output) in outputs.repos {
                            if let Some(url) = repo_output.github_url.clone() {
                                // Persist to storage
                                let _ = storage.update_synced(
                                    &repo_name,
                                    Forge::GitHub,
                                    url.clone(),
                                    repo_output.github_id.clone(),
                                ).await;

                                yield RepoSyncEvent::OutputsCaptured {
                                    org_name: org_name.clone(),
                                    repo_name: repo_name.clone(),
                                    forge: Forge::GitHub,
                                    url,
                                    id: repo_output.github_id,
                                };
                            }
                            if let Some(url) = repo_output.codeberg_url.clone() {
                                // Persist to storage
                                let _ = storage.update_synced(
                                    &repo_name,
                                    Forge::Codeberg,
                                    url.clone(),
                                    repo_output.codeberg_id.clone(),
                                ).await;

                                yield RepoSyncEvent::OutputsCaptured {
                                    org_name: org_name.clone(),
                                    repo_name: repo_name.clone(),
                                    forge: Forge::Codeberg,
                                    url,
                                    id: repo_output.codeberg_id,
                                };
                            }
                        }
                    }
                    Err(e) => {
                        yield RepoSyncEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: None,
                            message: format!("Failed to capture outputs: {}", e),
                        };
                    }
                }
            }
        }
    }

    /// Mark a repository for deletion
    #[method(
        description = "Remove a repository",
        params(
            repo_name = "Repository to remove",
            force = "Force removal of protected repos"
        )
    )]
    pub async fn remove(
        &self,
        repo_name: String,
        force: Option<bool>,
    ) -> impl Stream<Item = RepoRemoveEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let force_delete = force.unwrap_or(false);

        stream! {
            // Load repos to check protection status
            let repos = match storage.load_repos().await {
                Ok(r) => r,
                Err(e) => {
                    yield RepoRemoveEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: e.to_string(),
                    };
                    return;
                }
            };

            // Check if repo exists and is protected
            if let Some(config) = repos.repos.get(&repo_name) {
                if config.protected && !force_delete {
                    yield RepoRemoveEvent::ProtectionError {
                        org_name,
                        repo_name,
                        message: "Repository is protected. Use --force true to delete.".into(),
                    };
                    return;
                }
            }

            // Proceed with deletion
            match storage.stage_deletion(repo_name.clone()).await {
                Ok(()) => {
                    yield RepoRemoveEvent::MarkedForDeletion {
                        org_name,
                        repo_name,
                    };
                }
                Err(e) => {
                    yield RepoRemoveEvent::Error {
                        org_name,
                        repo_name: Some(repo_name),
                        message: e.to_string(),
                    };
                }
            }
        }
    }

    /// Refresh local state from forge APIs
    #[method(
        description = "Query forges and update local discovered state",
        params(
            force = "Refresh even if recently updated"
        )
    )]
    pub async fn refresh(
        &self,
        _force: Option<bool>,
    ) -> impl Stream<Item = RepoRefreshEvent> + Send + 'static {
        let org_name = self.org_name.clone();
        let org_config = self.org_config.clone();
        let storage = self.storage();

        stream! {
            // Org config comes from parent - no need to reload

            yield RepoRefreshEvent::Started {
                org_name: org_name.clone(),
                forges: org_config.forges.all_forges(),
            };

            // Track all discovered repo names across forges
            let mut discovered_repos: HashSet<String> = HashSet::new();

            // Query each forge
            for forge in org_config.forges.all_forges() {
                let keychain = KeychainBridge::new(&org_name);
                let token_key = match &forge {
                    Forge::GitHub => "github-token",
                    Forge::Codeberg => "codeberg-token",
                    Forge::GitLab => "gitlab-token",
                };

                let token = match keychain.get(token_key).await {
                    Ok(Some(t)) => t,
                    Ok(None) => {
                        yield RepoRefreshEvent::Error {
                            org_name: org_name.clone(),
                            message: format!("No token configured for {}", forge),
                        };
                        continue;
                    }
                    Err(e) => {
                        yield RepoRefreshEvent::Error {
                            org_name: org_name.clone(),
                            message: e,
                        };
                        continue;
                    }
                };

                // Query forge API
                let repos = query_forge_repos(&forge, &org_config.owner, &token).await;

                match repos {
                    Ok(forge_repos) => {
                        yield RepoRefreshEvent::Progress {
                            org_name: org_name.clone(),
                            forge: forge.clone(),
                            repos_found: forge_repos.len(),
                        };

                        for repo in forge_repos {
                            discovered_repos.insert(repo.name);
                        }
                    }
                    Err(e) => {
                        yield RepoRefreshEvent::Error {
                            org_name: org_name.clone(),
                            message: format!("{} query failed: {}", forge, e),
                        };
                    }
                }
            }

            // Load current repos to calculate matched vs untracked
            let repos_config = match storage.load_repos().await {
                Ok(r) => r,
                Err(e) => {
                    yield RepoRefreshEvent::Error {
                        org_name: org_name.clone(),
                        message: e.to_string(),
                    };
                    return;
                }
            };

            let local_repo_names: HashSet<String> = repos_config.repos.keys().cloned().collect();

            let matched = discovered_repos.intersection(&local_repo_names).count();
            let untracked = discovered_repos.difference(&local_repo_names).count();

            yield RepoRefreshEvent::Complete {
                org_name,
                discovered: discovered_repos.len(),
                matched,
                untracked,
            };
        }
    }

    /// Compare local desired state vs remote actual state
    #[method(
        description = "Show differences between local and remote state",
        params(
            refresh = "Refresh from forges before diffing"
        )
    )]
    pub async fn diff(
        &self,
        refresh: Option<bool>,
    ) -> impl Stream<Item = RepoDiffEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let org_config = self.org_config.clone();
        let should_refresh = refresh.unwrap_or(false);

        // If refresh is requested, we need to capture refresh events first
        // For simplicity, we just do the diff without inline refresh for now
        // (User can call refresh() first if they want fresh data)
        let _ = should_refresh; // TODO: implement inline refresh

        stream! {
            // Load repos config (committed)
            let mut repos = match storage.load_repos().await {
                Ok(r) => r,
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: None,
                        message: e.to_string(),
                    };
                    return;
                }
            };

            // Also include staged repos (pending changes)
            if let Ok(staged) = storage.load_staged().await {
                for (name, config) in staged.repos {
                    if config.delete {
                        // Mark for deletion in existing entry
                        if let Some(existing) = repos.repos.get_mut(&name) {
                            existing.delete = true;
                        }
                    } else {
                        // Add or replace with staged version
                        repos.repos.insert(name, config);
                    }
                }
            }

            // Org config comes from parent - no need to reload
            // Determine which forges should be synced vs skipped
            // Origin forge is always synced regardless of the sync flag
            let all_forges = org_config.forges.all_forges();
            let synced_forges_list = org_config.forges.synced_forges();
            let origin_forge = &org_config.origin;

            // Emit warnings for skipped forges (except origin which is always synced)
            for forge in &all_forges {
                if !synced_forges_list.contains(forge) && forge != origin_forge {
                    yield RepoEvent::ForgeSkipped {
                        org_name: org_name.clone(),
                        forge: forge.clone(),
                        reason: "sync: false in org config (excluded from diff)".to_string(),
                    };
                }
            }

            // Calculate effective synced forges set (synced + origin)
            let mut effective_synced_set: HashSet<Forge> = synced_forges_list.into_iter().collect();
            effective_synced_set.insert(origin_forge.clone());

            let mut to_create = 0;
            let mut to_update = 0;
            let mut to_delete = 0;
            let mut in_sync = 0;

            for (name, repo_config) in &repos.repos {
                // Get desired forges from repo config, filtered by effective synced forges
                let repo_forges: HashSet<_> = repo_config.forges
                    .as_ref()
                    .map(|f| f.iter().cloned().collect())
                    .unwrap_or_else(|| org_config.forges.all_forges().into_iter().collect());

                // Filter to only forges that are actually synced (desired & effective_synced)
                let desired_forges: HashSet<_> = repo_forges
                    .intersection(&effective_synced_set)
                    .cloned()
                    .collect();

                // Get actually synced forges, filtered to only those we're tracking
                let actual_synced: HashSet<_> = repo_config.synced
                    .as_ref()
                    .map(|s| s.forges.keys().cloned().collect())
                    .unwrap_or_default();

                // Only count forges in our effective sync set
                let synced_forges: HashSet<_> = actual_synced
                    .intersection(&effective_synced_set)
                    .cloned()
                    .collect();

                // Determine status and build details
                let (status, details) = if repo_config.delete {
                    to_delete += 1;
                    let forges_list: Vec<_> = synced_forges.iter().map(|f| f.to_string()).collect();
                    (DiffStatus::ToDelete, vec![
                        format!("Marked for deletion on: [{}]", forges_list.join(", ")),
                    ])
                } else if synced_forges.is_empty() {
                    to_create += 1;
                    let forges_list: Vec<_> = desired_forges.iter().map(|f| f.to_string()).collect();
                    (DiffStatus::ToCreate, vec![
                        format!("Will create on: [{}]", forges_list.join(", ")),
                    ])
                } else if desired_forges != synced_forges {
                    to_update += 1;
                    let missing: Vec<_> = desired_forges.difference(&synced_forges)
                        .map(|f| f.to_string())
                        .collect();
                    let extra: Vec<_> = synced_forges.difference(&desired_forges)
                        .map(|f| f.to_string())
                        .collect();
                    let mut details = vec![];
                    if !missing.is_empty() {
                        details.push(format!("Missing on: [{}]", missing.join(", ")));
                    }
                    if !extra.is_empty() {
                        details.push(format!("Extra on: [{}]", extra.join(", ")));
                    }
                    (DiffStatus::ToUpdate, details)
                } else {
                    in_sync += 1;
                    let forges_list: Vec<_> = synced_forges.iter().map(|f| f.to_string()).collect();
                    (DiffStatus::InSync, vec![
                        format!("Synced on: [{}]", forges_list.join(", ")),
                    ])
                };

                yield RepoEvent::RepoDiff {
                    org_name: org_name.clone(),
                    repo_name: name.clone(),
                    status,
                    details,
                };
            }

            // Note: Untracked repos would require scanning _discovered state
            // across all repos or doing a fresh forge query. For now we report 0
            // and users can run refresh() to populate _discovered state.
            let untracked = 0;

            yield RepoEvent::DiffSummary {
                org_name,
                to_create,
                to_update,
                to_delete,
                in_sync,
                untracked,
            };
        }
    }

    /// Full bidirectional sync with convergence verification
    #[method(
        description = "Full bidirectional sync: refresh, diff, apply, capture, verify",
        params(
            dry_run = "Preview all phases without applying",
            yes = "Skip confirmation prompts"
        )
    )]
    pub async fn converge(
        &self,
        dry_run: Option<bool>,
        yes: Option<bool>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let org_config = self.org_config.clone();
        let paths = self.paths.clone();
        let is_dry_run = dry_run.unwrap_or(false);
        let auto_yes = yes.unwrap_or(false);

        stream! {
            let phases = vec![
                "refresh".to_string(),
                "diff".to_string(),
                "apply".to_string(),
                "capture".to_string(),
                "verify".to_string(),
            ];

            yield RepoEvent::ConvergeStarted {
                org_name: org_name.clone(),
                phases: phases.clone(),
            };

            // ============================================================
            // Phase 1: REFRESH - Query forges for current state
            // ============================================================
            yield RepoEvent::ConvergePhase {
                org_name: org_name.clone(),
                phase: "refresh".into(),
                status: "started".into(),
            };

            // Org config comes from parent - no need to reload

            // Track discovered repos across forges
            let mut discovered_repos: HashSet<String> = HashSet::new();

            for forge in org_config.forges.all_forges() {
                let keychain = KeychainBridge::new(&org_name);
                let token_key = match &forge {
                    Forge::GitHub => "github-token",
                    Forge::Codeberg => "codeberg-token",
                    Forge::GitLab => "gitlab-token",
                };

                let token = match keychain.get(token_key).await {
                    Ok(Some(t)) => t,
                    Ok(None) => {
                        yield RepoEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: None,
                            message: format!("No token configured for {}", forge),
                        };
                        continue;
                    }
                    Err(e) => {
                        yield RepoEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: None,
                            message: e,
                        };
                        continue;
                    }
                };

                // Query forge API
                let repos = query_forge_repos(&forge, &org_config.owner, &token).await;

                match repos {
                    Ok(forge_repos) => {
                        for repo in forge_repos {
                            discovered_repos.insert(repo.name);
                        }
                    }
                    Err(e) => {
                        yield RepoEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: None,
                            message: format!("{} query failed: {}", forge, e),
                        };
                    }
                }
            }

            yield RepoEvent::ConvergePhase {
                org_name: org_name.clone(),
                phase: "refresh".into(),
                status: format!("discovered {} repos", discovered_repos.len()),
            };

            // ============================================================
            // Phase 2: DIFF - Compare local vs remote
            // ============================================================
            yield RepoEvent::ConvergePhase {
                org_name: org_name.clone(),
                phase: "diff".into(),
                status: "started".into(),
            };

            // Load repos config and compute diff
            let repos_config = match storage.load_repos().await {
                Ok(r) => r,
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: None,
                        message: e.to_string(),
                    };
                    return;
                }
            };

            // Also load staged repos to include pending changes
            let staged_config = match storage.load_staged().await {
                Ok(s) => s,
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: None,
                        message: e.to_string(),
                    };
                    return;
                }
            };

            // Calculate effective synced forges (synced + origin)
            let synced_forges_list = org_config.forges.synced_forges();
            let origin_forge = &org_config.origin;
            let mut effective_synced_set: HashSet<Forge> = synced_forges_list.into_iter().collect();
            effective_synced_set.insert(origin_forge.clone());

            let mut to_create = 0usize;
            let mut to_update = 0usize;
            let mut to_delete = 0usize;

            // Check committed repos
            for (_, repo_config) in &repos_config.repos {
                // Get desired forges, filtered by effective synced set
                let repo_forges: HashSet<_> = repo_config.forges
                    .as_ref()
                    .map(|f| f.iter().cloned().collect())
                    .unwrap_or_else(|| org_config.forges.all_forges().into_iter().collect());

                let desired_forges: HashSet<_> = repo_forges
                    .intersection(&effective_synced_set)
                    .cloned()
                    .collect();

                // Get actually synced forges, filtered to effective set
                let actual_synced: HashSet<_> = repo_config.synced
                    .as_ref()
                    .map(|s| s.forges.keys().cloned().collect())
                    .unwrap_or_default();

                let synced_forges: HashSet<_> = actual_synced
                    .intersection(&effective_synced_set)
                    .cloned()
                    .collect();

                if repo_config.delete {
                    to_delete += 1;
                } else if synced_forges.is_empty() {
                    to_create += 1;
                } else if desired_forges != synced_forges {
                    to_update += 1;
                }
            }

            // Check staged repos (new or modified)
            for (name, repo_config) in &staged_config.repos {
                if repo_config.delete {
                    // Only count if it exists in committed (would be a deletion)
                    if repos_config.repos.contains_key(name) {
                        to_delete += 1;
                    }
                } else if !repos_config.repos.contains_key(name) {
                    // New repo being created
                    to_create += 1;
                } else {
                    // Update to existing repo
                    to_update += 1;
                }
            }

            yield RepoEvent::ConvergePhase {
                org_name: org_name.clone(),
                phase: "diff".into(),
                status: format!("create={}, update={}, delete={}", to_create, to_update, to_delete),
            };

            // If nothing to do, we're converged
            if to_create == 0 && to_update == 0 && to_delete == 0 {
                yield RepoEvent::ConvergeComplete {
                    org_name,
                    success: true,
                    changes_applied: 0,
                    final_state: ConvergeResult {
                        converged: true,
                        repos_synced: 0,
                        repos_created: 0,
                        repos_deleted: 0,
                        drift_detected: false,
                    },
                };
                return;
            }

            if is_dry_run {
                yield RepoEvent::ConvergePhase {
                    org_name: org_name.clone(),
                    phase: "apply".into(),
                    status: "skipped (dry run)".into(),
                };

                yield RepoEvent::ConvergeComplete {
                    org_name,
                    success: true,
                    changes_applied: 0,
                    final_state: ConvergeResult {
                        converged: false,
                        repos_synced: to_create + to_update,
                        repos_created: to_create,
                        repos_deleted: to_delete,
                        drift_detected: false,
                    },
                };
                return;
            }

            // ============================================================
            // Phase 3: APPLY - Run Pulumi to make changes
            // ============================================================
            yield RepoEvent::ConvergePhase {
                org_name: org_name.clone(),
                phase: "apply".into(),
                status: "started".into(),
            };

            // Merge staged into committed
            if let Err(e) = storage.merge_staged().await {
                yield RepoEvent::Error {
                    org_name: org_name.clone(),
                    repo_name: None,
                    message: e.to_string(),
                };
                return;
            }

            let bridge = crate::bridge::PulumiBridge::new(&paths);

            // Select/create stack for this org
            if let Err(e) = bridge.select_stack(&org_name).await {
                yield RepoEvent::Error {
                    org_name: org_name.clone(),
                    repo_name: None,
                    message: format!("Failed to select Pulumi stack: {}", e),
                };
                return;
            }

            let repos_file = paths.repos_file(&org_name);
            let staged_file = paths.staged_repos_file(&org_name);

            let pulumi_stream = bridge.up(&org_name, &repos_file, &staged_file, auto_yes);

            let mut changes_applied = 0usize;

            use futures::StreamExt;
            let mut pulumi_stream = Box::pin(pulumi_stream);

            while let Some(event) = pulumi_stream.next().await {
                match event {
                    PulumiEvent::UpComplete { success, creates, updates, deletes } => {
                        changes_applied = creates + updates + deletes;

                        if !success {
                            yield RepoEvent::Error {
                                org_name: org_name.clone(),
                                repo_name: None,
                                message: "Pulumi apply failed".into(),
                            };
                            return;
                        }

                        yield RepoEvent::ConvergePhase {
                            org_name: org_name.clone(),
                            phase: "apply".into(),
                            status: format!("applied {} changes", changes_applied),
                        };
                    }
                    PulumiEvent::Error { message } => {
                        yield RepoEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: None,
                            message,
                        };
                        return;
                    }
                    _ => {}
                }
            }

            // ============================================================
            // Phase 4: CAPTURE - Store outputs in local state
            // ============================================================
            yield RepoEvent::ConvergePhase {
                org_name: org_name.clone(),
                phase: "capture".into(),
                status: "started".into(),
            };

            match bridge.get_outputs(&org_name).await {
                Ok(outputs) => {
                    for (repo_name, repo_output) in outputs.repos {
                        if let Some(url) = repo_output.github_url {
                            let _ = storage.update_synced(
                                &repo_name,
                                Forge::GitHub,
                                url,
                                repo_output.github_id,
                            ).await;
                        }
                        if let Some(url) = repo_output.codeberg_url {
                            let _ = storage.update_synced(
                                &repo_name,
                                Forge::Codeberg,
                                url,
                                repo_output.codeberg_id,
                            ).await;
                        }
                    }

                    yield RepoEvent::ConvergePhase {
                        org_name: org_name.clone(),
                        phase: "capture".into(),
                        status: "outputs captured".into(),
                    };
                }
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: None,
                        message: format!("Capture failed: {}", e),
                    };
                    // Continue to verify phase even if capture failed
                }
            }

            // ============================================================
            // Phase 5: VERIFY - Re-diff to confirm convergence
            // ============================================================
            yield RepoEvent::ConvergePhase {
                org_name: org_name.clone(),
                phase: "verify".into(),
                status: "started".into(),
            };

            // Re-load repos to check for drift
            let final_repos = match storage.load_repos().await {
                Ok(r) => r,
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: None,
                        message: format!("Failed to verify: {}", e),
                    };
                    // Return with drift detected since we can't verify
                    yield RepoEvent::ConvergeComplete {
                        org_name,
                        success: false,
                        changes_applied,
                        final_state: ConvergeResult {
                            converged: false,
                            repos_synced: to_create + to_update,
                            repos_created: to_create,
                            repos_deleted: to_delete,
                            drift_detected: true,
                        },
                    };
                    return;
                }
            };

            // Re-compute diff to check for remaining drift
            // effective_synced_set is already defined above
            let mut verify_create = 0usize;
            let mut verify_update = 0usize;
            let mut verify_delete = 0usize;

            for (_, repo_config) in &final_repos.repos {
                // Get desired forges, filtered by effective synced set
                let repo_forges: HashSet<_> = repo_config.forges
                    .as_ref()
                    .map(|f| f.iter().cloned().collect())
                    .unwrap_or_else(|| org_config.forges.all_forges().into_iter().collect());

                let desired_forges: HashSet<_> = repo_forges
                    .intersection(&effective_synced_set)
                    .cloned()
                    .collect();

                // Get actually synced forges, filtered to effective set
                let actual_synced: HashSet<_> = repo_config.synced
                    .as_ref()
                    .map(|s| s.forges.keys().cloned().collect())
                    .unwrap_or_default();

                let synced_forges: HashSet<_> = actual_synced
                    .intersection(&effective_synced_set)
                    .cloned()
                    .collect();

                if repo_config.delete {
                    verify_delete += 1;
                } else if synced_forges.is_empty() {
                    verify_create += 1;
                } else if desired_forges != synced_forges {
                    verify_update += 1;
                }
            }

            let drift_detected = verify_create > 0 || verify_update > 0 || verify_delete > 0;

            yield RepoEvent::ConvergePhase {
                org_name: org_name.clone(),
                phase: "verify".into(),
                status: if drift_detected {
                    "drift detected!".into()
                } else {
                    "converged".into()
                },
            };

            // ============================================================
            // Complete
            // ============================================================
            yield RepoEvent::ConvergeComplete {
                org_name,
                success: !drift_detected,
                changes_applied,
                final_state: ConvergeResult {
                    converged: !drift_detected,
                    repos_synced: to_create + to_update,
                    repos_created: to_create,
                    repos_deleted: to_delete,
                    drift_detected,
                },
            };
        }
    }

    /// Clone a repository from forges and configure remotes
    #[method(
        description = "Clone a repository from forges and configure all remotes",
        params(
            repo_name = "Repository to clone",
            target = "Target directory (defaults to current dir / repo_name)"
        )
    )]
    pub async fn clone(
        &self,
        repo_name: String,
        target: Option<String>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let org_config = self.org_config.clone();

        stream! {
            // Org config comes from parent - no need to reload

            // Load repos config to get synced URLs
            let repos = match storage.load_repos().await {
                Ok(r) => r,
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: e.to_string(),
                    };
                    return;
                }
            };

            // Find the repo config
            let repo_config = match repos.repos.get(&repo_name) {
                Some(cfg) => cfg.clone(),
                None => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Repository not found in config: {}", repo_name),
                    };
                    return;
                }
            };

            // Determine target path
            let target_path = match target {
                Some(t) => PathBuf::from(t),
                None => std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(&repo_name),
            };

            yield RepoEvent::CloneStarted {
                org_name: org_name.clone(),
                repo_name: repo_name.clone(),
                target_path: target_path.clone(),
            };

            // Determine which forge to clone from (prefer origin/primary)
            let org_forges = org_config.forges.all_forges();
            let forges = repo_config.forges.as_ref()
                .unwrap_or(&org_forges);

            let origin_forge = &org_config.origin;
            let origin_url = org_config.origin_url(&org_name, &repo_name);

            yield RepoEvent::CloneProgress {
                org_name: org_name.clone(),
                repo_name: repo_name.clone(),
                forge: origin_forge.clone(),
                stage: "cloning".into(),
            };

            // Run git clone
            let clone_output = tokio::process::Command::new("git")
                .args(["clone", &origin_url, target_path.to_str().unwrap_or(".")])
                .output()
                .await;

            match clone_output {
                Ok(output) if output.status.success() => {
                    yield RepoEvent::CloneProgress {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        forge: origin_forge.clone(),
                        stage: "cloned".into(),
                    };
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("git clone failed: {}", stderr),
                    };
                    return;
                }
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: Some(repo_name.clone()),
                        message: format!("Failed to run git: {}", e),
                    };
                    return;
                }
            }

            // Add remotes for other forges
            let mut remotes = vec!["origin".to_string()];

            for forge in forges {
                if forge == origin_forge {
                    continue; // Already the origin
                }

                let forge_name = forge.to_string().to_lowercase();
                let remote_url = org_config.ssh_url(forge, &org_name, &repo_name);

                yield RepoEvent::CloneProgress {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                    forge: forge.clone(),
                    stage: format!("adding remote '{}'", forge_name),
                };

                let add_remote = tokio::process::Command::new("git")
                    .current_dir(&target_path)
                    .args(["remote", "add", &forge_name, &remote_url])
                    .output()
                    .await;

                match add_remote {
                    Ok(output) if output.status.success() => {
                        remotes.push(forge_name.clone());
                        yield RepoEvent::CloneRemoteAdded {
                            org_name: org_name.clone(),
                            repo_name: repo_name.clone(),
                            remote_name: forge_name,
                            url: remote_url,
                        };
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        // Non-fatal: remote might already exist
                        yield RepoEvent::CloneProgress {
                            org_name: org_name.clone(),
                            repo_name: repo_name.clone(),
                            forge: forge.clone(),
                            stage: format!("remote add warning: {}", stderr.trim()),
                        };
                    }
                    Err(e) => {
                        yield RepoEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message: format!("Failed to add remote: {}", e),
                        };
                    }
                }
            }

            // Fetch all remotes
            yield RepoEvent::CloneProgress {
                org_name: org_name.clone(),
                repo_name: repo_name.clone(),
                forge: origin_forge.clone(),
                stage: "fetching all remotes".into(),
            };

            let _ = tokio::process::Command::new("git")
                .current_dir(&target_path)
                .args(["fetch", "--all"])
                .output()
                .await;

            // Check sync status across remotes
            let mut sync_details = Vec::new();
            let mut all_in_sync = true;

            // Get default branch
            let default_branch_output = tokio::process::Command::new("git")
                .current_dir(&target_path)
                .args(["symbolic-ref", "--short", "HEAD"])
                .output()
                .await;

            let default_branch = default_branch_output
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "main".to_string());

            // Compare refs for each remote
            for remote in &remotes {
                let ref_output = tokio::process::Command::new("git")
                    .current_dir(&target_path)
                    .args(["rev-parse", &format!("{}/{}", remote, default_branch)])
                    .output()
                    .await;

                if let Ok(output) = ref_output {
                    if output.status.success() {
                        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        let short_commit = commit.chars().take(7).collect::<String>();
                        sync_details.push(format!("{}/{} = {}", remote, default_branch, short_commit));
                    } else {
                        sync_details.push(format!("{}/{} = (not found)", remote, default_branch));
                        all_in_sync = false;
                    }
                }
            }

            // Check if all refs match
            if remotes.len() > 1 {
                let first_ref = tokio::process::Command::new("git")
                    .current_dir(&target_path)
                    .args(["rev-parse", &format!("origin/{}", default_branch)])
                    .output()
                    .await
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string());

                for remote in &remotes[1..] {
                    let remote_ref = tokio::process::Command::new("git")
                        .current_dir(&target_path)
                        .args(["rev-parse", &format!("{}/{}", remote, default_branch)])
                        .output()
                        .await
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .map(|s| s.trim().to_string());

                    if first_ref != remote_ref {
                        all_in_sync = false;
                    }
                }
            }

            yield RepoEvent::RemoteSyncStatus {
                org_name: org_name.clone(),
                repo_name: repo_name.clone(),
                branch: default_branch,
                in_sync: all_in_sync,
                details: sync_details,
            };

            yield RepoEvent::CloneComplete {
                org_name,
                repo_name,
                target_path,
                remotes,
            };
        }
    }

    /// Clone all repositories for an organization
    #[method(
        description = "Clone all repositories for this org, configuring remotes for each",
        params(
            target = "Target directory (repos will be cloned as subdirectories)"
        )
    )]
    pub async fn clone_all(
        &self,
        target: Option<String>,
    ) -> impl Stream<Item = RepoEvent> + Send + 'static {
        let storage = self.storage();
        let org_name = self.org_name.clone();
        let org_config = self.org_config.clone();

        stream! {
            // Org config comes from parent - no need to reload

            // Determine target directory
            let target_dir = match target {
                Some(t) => PathBuf::from(t),
                None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            };

            // Ensure target directory exists
            if !target_dir.exists() {
                if let Err(e) = std::fs::create_dir_all(&target_dir) {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: None,
                        message: format!("Failed to create target directory: {}", e),
                    };
                    return;
                }
            }

            // Load repos config
            let repos = match storage.load_repos().await {
                Ok(r) => r,
                Err(e) => {
                    yield RepoEvent::Error {
                        org_name: org_name.clone(),
                        repo_name: None,
                        message: e.to_string(),
                    };
                    return;
                }
            };

            let origin_forge = &org_config.origin;

            // Clone each repo
            for (repo_name, repo_config) in &repos.repos {
                if repo_config.delete {
                    continue; // Skip repos marked for deletion
                }

                let repo_path = target_dir.join(repo_name);

                // Skip if already exists
                if repo_path.exists() {
                    yield RepoEvent::CloneProgress {
                        org_name: org_name.clone(),
                        repo_name: repo_name.clone(),
                        forge: origin_forge.clone(),
                        stage: "skipped (already exists)".into(),
                    };
                    continue;
                }

                yield RepoEvent::CloneStarted {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                    target_path: repo_path.clone(),
                };

                // Get forges for this repo
                let org_forges = org_config.forges.all_forges();
                let forges = repo_config.forges.as_ref()
                    .unwrap_or(&org_forges);

                let origin_url = org_config.origin_url(&org_name, repo_name);

                yield RepoEvent::CloneProgress {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                    forge: origin_forge.clone(),
                    stage: "cloning".into(),
                };

                // Run git clone
                let clone_output = tokio::process::Command::new("git")
                    .args(["clone", &origin_url, repo_path.to_str().unwrap_or(".")])
                    .output()
                    .await;

                match clone_output {
                    Ok(output) if output.status.success() => {
                        yield RepoEvent::CloneProgress {
                            org_name: org_name.clone(),
                            repo_name: repo_name.clone(),
                            forge: origin_forge.clone(),
                            stage: "cloned".into(),
                        };
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        yield RepoEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message: format!("git clone failed: {}", stderr),
                        };
                        continue; // Skip to next repo
                    }
                    Err(e) => {
                        yield RepoEvent::Error {
                            org_name: org_name.clone(),
                            repo_name: Some(repo_name.clone()),
                            message: format!("Failed to run git: {}", e),
                        };
                        continue;
                    }
                }

                // Add remotes for other forges
                let mut remotes = vec!["origin".to_string()];

                for forge in forges {
                    if forge == origin_forge {
                        continue;
                    }

                    let forge_name = forge.to_string().to_lowercase();
                    let remote_url = org_config.ssh_url(forge, &org_name, repo_name);

                    let add_remote = tokio::process::Command::new("git")
                        .current_dir(&repo_path)
                        .args(["remote", "add", &forge_name, &remote_url])
                        .output()
                        .await;

                    if let Ok(output) = add_remote {
                        if output.status.success() {
                            remotes.push(forge_name.clone());
                            yield RepoEvent::CloneRemoteAdded {
                                org_name: org_name.clone(),
                                repo_name: repo_name.clone(),
                                remote_name: forge_name,
                                url: remote_url,
                            };
                        }
                    }
                }

                // Fetch all remotes
                let _ = tokio::process::Command::new("git")
                    .current_dir(&repo_path)
                    .args(["fetch", "--all"])
                    .output()
                    .await;

                yield RepoEvent::CloneComplete {
                    org_name: org_name.clone(),
                    repo_name: repo_name.clone(),
                    target_path: repo_path,
                    remotes,
                };
            }
        }
    }

    pub fn plugin_children(&self) -> Vec<ChildSummary> {
        vec![]  // Dynamic children loaded from config
    }
}

#[async_trait]
impl ChildRouter for ReposActivation {
    fn router_namespace(&self) -> &str {
        "repos"
    }

    async fn router_call(&self, method: &str, params: Value) -> Result<PlexusStream, PlexusError> {
        Activation::call(self, method, params).await
    }

    async fn get_child(&self, name: &str) -> Option<Box<dyn ChildRouter>> {
        // Check if repo exists in config
        let storage = self.storage();
        let repos = storage.load_repos().await.ok()?;

        if repos.repos.contains_key(name) {
            Some(Box::new(RepoChildRouter::new(
                self.paths.clone(),
                self.org_name.clone(),
                name.to_string(),
                self.org_config.clone(),
            )))
        } else {
            None
        }
    }
}

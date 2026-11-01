//! `ImagesService` — Container image management subactivation
//!
//! A child plugin under `RepoService`, accessible as:
//!   synapse lforge hyperforge repo images list --org foo --name bar
//!   synapse lforge hyperforge repo images delete --org foo --name bar --tag v1.0

use async_stream::stream;
use futures::Stream;
use std::sync::Arc;

use crate::adapters::forge_port::ForgePort;
use crate::adapters::registry::codeberg::CodebergRegistryAdapter;
use crate::adapters::registry::github::GitHubRegistryAdapter;
use crate::adapters::registry::RegistryPort;
use crate::auth::YamlAuthProvider;
use crate::service::HyperforgeEvent;
use crate::services::HyperforgeState;
use crate::types::Forge;

/// Sub-service for container image operations
#[derive(Clone)]
pub struct ImagesService {
    state: HyperforgeState,
}

impl ImagesService {
    pub const fn new(state: HyperforgeState) -> Self {
        Self { state }
    }
}

fn make_registry_adapter(
    forge: &str,
    auth: Arc<YamlAuthProvider>,
    org: &str,
) -> Result<Box<dyn RegistryPort>, String> {
    match forge {
        "github" => GitHubRegistryAdapter::new(auth, org)
            .map(|a| Box::new(a) as Box<dyn RegistryPort>)
            .map_err(|e| format!("github: {e}")),
        "codeberg" => CodebergRegistryAdapter::new(auth, org)
            .map(|a| Box::new(a) as Box<dyn RegistryPort>)
            .map_err(|e| format!("codeberg: {e}")),
        other => Err(format!("Container registry not supported for forge: {other}")),
    }
}

fn make_auth() -> Result<Arc<YamlAuthProvider>, String> {
    YamlAuthProvider::new()
        .map(Arc::new)
        .map_err(|e| format!("Failed to create auth provider: {e}"))
}

#[plexus_macros::activation(
    trait = "ImagesApi",
    methods = marked,
    namespace = "images",
    description = "Container image management: list, delete, push",
    crate_path = "plexus_core"
)]
impl ImagesService {
    /// List container image tags for a repo on its configured forges
    #[plexus_macros::method(
        description = "List container image tags for a repository",
        params(
            org = "Organization name",
            name = "Repository name",
            forge = "Forge to query: github, codeberg, or gitlab (optional, defaults to all configured forges)",
            filter = "Regex pattern to filter tags (optional)"
        )
    )]
    pub async fn list(
        &self,
        org: String,
        name: String,
        forge: Option<Forge>,
        filter: Option<String>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let re = match &filter {
                Some(pattern) => match regex::Regex::new(pattern) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Invalid regex '{pattern}': {e}"),
                        };
                        return;
                    }
                },
                None => None,
            };

            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            // Determine which forges to query
            let target_forges: Vec<String> = if let Some(f) = forge {
                vec![f.as_str().to_string()]
            } else {
                let local = state.get_local_forge(&org).await;
                match local.get_repo(&org, &name).await {
                    Ok(repo) => {
                        let origin_str = serde_json::to_string(&repo.origin)
                            .unwrap_or_default().trim_matches('"').to_string();
                        let mut forges = vec![origin_str];
                        for m in &repo.mirrors {
                            let s = serde_json::to_string(m)
                                .unwrap_or_default().trim_matches('"').to_string();
                            if !forges.contains(&s) {
                                forges.push(s);
                            }
                        }
                        forges
                    }
                    Err(_) => {
                        // Not in LocalForge, try github as default
                        vec!["github".to_string()]
                    }
                }
            };

            for forge_name in &target_forges {
                let adapter = match make_registry_adapter(forge_name, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        yield HyperforgeEvent::Info {
                            message: format!("Skipping {forge_name} ({e})"),
                        };
                        continue;
                    }
                };

                match adapter.list_images(&org, &name).await {
                    Ok(tags) => {
                        if tags.is_empty() {
                            yield HyperforgeEvent::Info {
                                message: format!("No images found for {org}/{name} on {forge_name}"),
                            };
                            continue;
                        }

                        for tag in &tags {
                            if let Some(ref re) = re {
                                if !re.is_match(&tag.tag) {
                                    continue;
                                }
                            }
                            yield HyperforgeEvent::ImageTag {
                                repo_name: name.clone(),
                                forge: forge_name.clone(),
                                tag: tag.tag.clone(),
                                digest: tag.digest.clone(),
                                size_bytes: tag.size_bytes,
                                created_at: tag.created_at.to_rfc3339(),
                            };
                        }
                    }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to list images on {forge_name}: {e}"),
                        };
                    }
                }
            }
        }
    }

    /// List all container packages for an org across its configured forges
    #[plexus_macros::method(
        description = "List all container packages for an organization across forges",
        params(
            org = "Organization name",
            forge = "Forge to query: github, codeberg, or gitlab (optional, queries all known forges)",
            filter = "Regex pattern to filter package names (optional)"
        )
    )]
    pub async fn list_all(
        &self,
        org: String,
        forge: Option<Forge>,
        filter: Option<String>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let re = match &filter {
                Some(pattern) => match regex::Regex::new(pattern) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Invalid regex '{pattern}': {e}"),
                        };
                        return;
                    }
                },
                None => None,
            };

            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            // Determine forges to query
            let target_forges: Vec<String> = if let Some(f) = forge {
                vec![f.as_str().to_string()]
            } else {
                // Get forges from org config (SSH keys configured = forge is known)
                let org_config = crate::config::OrgConfig::load(&state.config_dir, &org);
                let mut forges: Vec<String> = org_config.ssh.keys().cloned().collect();
                if forges.is_empty() {
                    forges.push("github".to_string());
                }
                forges
            };

            let mut total = 0usize;
            for forge_name in &target_forges {
                let adapter = match make_registry_adapter(forge_name, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        yield HyperforgeEvent::Info {
                            message: format!("Skipping {forge_name} ({e})"),
                        };
                        continue;
                    }
                };

                match adapter.list_packages(&org).await {
                    Ok(packages) => {
                        if packages.is_empty() {
                            yield HyperforgeEvent::Info {
                                message: format!("No packages found for {org} on {forge_name}"),
                            };
                            continue;
                        }

                        for pkg in &packages {
                            if let Some(ref re) = re {
                                if !re.is_match(&pkg.name) {
                                    continue;
                                }
                            }
                            let created = pkg.created_at.map_or_else(|| "unknown".to_string(), |dt| dt.to_rfc3339());
                            yield HyperforgeEvent::Info {
                                message: format!(
                                    "  {}/{} on {} — {} tag(s), created {}",
                                    org, pkg.name, forge_name, pkg.tag_count, created,
                                ),
                            };
                            total += 1;
                        }
                    }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to list packages on {forge_name}: {e}"),
                        };
                    }
                }
            }

            yield HyperforgeEvent::Info {
                message: format!("{total} package(s) found."),
            };
        }
    }

    /// Build and push a container image to forge registries
    #[plexus_macros::method(
        description = "Build a Docker image and push to configured forge registries. Uses native Docker API via bollard.",
        params(
            org = "Organization name",
            name = "Image name (defaults to repo directory name)",
            path = "Path to build context (directory containing Dockerfile)",
            tag = "Image tag (default: latest)",
            dockerfile = "Dockerfile path relative to build context (optional, auto-detected)",
            forge = "Target forge: github, codeberg, or gitlab (optional, pushes to all configured forges)",
            dry_run = "Preview without building or pushing (optional, default: false)"
        )
    )]
    pub async fn push(
        &self,
        org: String,
        name: Option<String>,
        path: String,
        tag: Option<String>,
        dockerfile: Option<String>,
        forge: Option<Forge>,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();
        let is_dry_run = dry_run.unwrap_or(false);

        stream! {
            use crate::types::registry::{ContainerRegistry, ImageRef, RegistryAuth};

            let dry_prefix = if is_dry_run { "[dry-run] " } else { "" };
            let build_path = std::path::PathBuf::from(&path);

            if !build_path.exists() {
                yield HyperforgeEvent::Error {
                    message: format!("Build path does not exist: {path}"),
                };
                return;
            }

            // Detect Dockerfile
            let df_relative = if let Some(ref df) = dockerfile {
                df.clone()
            } else {
                let candidates = ["Dockerfile", "Containerfile", "docker/Dockerfile"];
                if let Some(c) = candidates.iter().find(|c| build_path.join(c).exists()) { c.to_string() } else {
                    yield HyperforgeEvent::Error {
                        message: format!("No Dockerfile found in {}. Tried: {}", path, candidates.join(", ")),
                    };
                    return;
                }
            };

            if !build_path.join(&df_relative).exists() {
                yield HyperforgeEvent::Error {
                    message: format!("Dockerfile not found: {}", build_path.join(&df_relative).display()),
                };
                return;
            }

            // Resolve image name and tag
            let image_name = name.unwrap_or_else(|| {
                build_path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image")
                    .to_string()
            });
            let image_tag = tag.unwrap_or_else(|| "latest".to_string());
            let local_tag = format!("{image_name}:{image_tag}");

            // Resolve target registries from forge names
            let target_registries: Vec<ContainerRegistry> = if let Some(f) = forge {
                vec![match f {
                    Forge::GitHub => ContainerRegistry::Ghcr,
                    Forge::Codeberg => ContainerRegistry::Codeberg,
                    Forge::GitLab => ContainerRegistry::GitLab,
                }]
            } else {
                let org_config = crate::config::OrgConfig::load(&state.config_dir, &org);
                let mut regs: Vec<ContainerRegistry> = org_config.ssh.keys().map(|k| {
                    match k.as_str() {
                        "github" => ContainerRegistry::Ghcr,
                        "codeberg" => ContainerRegistry::Codeberg,
                        "gitlab" => ContainerRegistry::GitLab,
                        other => ContainerRegistry::Custom(other.to_string()),
                    }
                }).collect();
                if regs.is_empty() {
                    regs.push(ContainerRegistry::Ghcr);
                }
                regs
            };

            // Connect to Docker daemon
            let docker = match crate::docker::connect() {
                Ok(d) => d,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Docker unavailable: {e}. Is Docker/Colima running?"),
                    };
                    return;
                }
            };

            // Verify Docker is responsive
            match crate::docker::check_state(&docker).await {
                crate::docker::DockerState::Available { version } => {
                    yield HyperforgeEvent::Info {
                        message: format!("Docker {version} connected"),
                    };
                }
                crate::docker::DockerState::NotRunning => {
                    yield HyperforgeEvent::Error {
                        message: "Docker daemon is not running. Start Docker or Colima first.".to_string(),
                    };
                    return;
                }
                crate::docker::DockerState::NotInstalled => {
                    yield HyperforgeEvent::Error {
                        message: "Docker is not installed.".to_string(),
                    };
                    return;
                }
            }

            // Build
            yield HyperforgeEvent::Info {
                message: format!("{}Building {} from {}", dry_prefix, local_tag, build_path.join(&df_relative).display()),
            };

            if !is_dry_run {
                match crate::docker::build_image(&docker, &build_path, &df_relative, &local_tag).await {
                    Ok(_id) => {
                        yield HyperforgeEvent::Info {
                            message: format!("Build succeeded: {local_tag}"),
                        };
                    }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Build failed: {e}"),
                        };
                        return;
                    }
                }
            }

            // Resolve auth and push to each registry
            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            for registry in &target_registries {
                let image_ref = ImageRef::new(registry.clone(), &org, &image_name, &image_tag);

                yield HyperforgeEvent::Info {
                    message: format!("{dry_prefix}Pushing {image_ref}"),
                };

                if is_dry_run {
                    yield HyperforgeEvent::ImagePush {
                        repo_name: image_name.clone(),
                        forge: registry.token_forge_name().to_string(),
                        tag: image_tag.clone(),
                        image: image_ref.full_name(),
                        success: true,
                        error: None,
                    };
                    continue;
                }

                // Resolve registry credentials
                let reg_auth = match RegistryAuth::resolve(registry, &org, auth.as_ref()).await {
                    Ok(a) => a,
                    Err(e) => {
                        yield HyperforgeEvent::ImagePush {
                            repo_name: image_name.clone(),
                            forge: registry.token_forge_name().to_string(),
                            tag: image_tag.clone(),
                            image: image_ref.full_name(),
                            success: false,
                            error: Some(e),
                        };
                        continue;
                    }
                };

                let credentials = crate::docker::to_docker_credentials(&reg_auth, registry, &org);

                // Tag for remote registry
                let remote_repo = format!("{}/{}/{}", registry.host(), org, image_name);
                if let Err(e) = crate::docker::tag_image(&docker, &local_tag, &remote_repo, &image_tag).await {
                    yield HyperforgeEvent::ImagePush {
                        repo_name: image_name.clone(),
                        forge: registry.token_forge_name().to_string(),
                        tag: image_tag.clone(),
                        image: image_ref.full_name(),
                        success: false,
                        error: Some(e),
                    };
                    continue;
                }

                // Push via bollard
                match crate::docker::push_image(&docker, &remote_repo, &image_tag, credentials).await {
                    Ok(()) => {
                        yield HyperforgeEvent::ImagePush {
                            repo_name: image_name.clone(),
                            forge: registry.token_forge_name().to_string(),
                            tag: image_tag.clone(),
                            image: image_ref.full_name(),
                            success: true,
                            error: None,
                        };
                    }
                    Err(e) => {
                        yield HyperforgeEvent::ImagePush {
                            repo_name: image_name.clone(),
                            forge: registry.token_forge_name().to_string(),
                            tag: image_tag.clone(),
                            image: image_ref.full_name(),
                            success: false,
                            error: Some(e),
                        };
                    }
                }
            }
        }
    }

    /// Delete a container image tag
    #[plexus_macros::method(
        description = "Delete a container image tag from a forge registry. Requires --confirm true.",
        params(
            org = "Organization name",
            name = "Repository name",
            forge = "Forge: github, codeberg, or gitlab",
            tag = "Image tag to delete",
            confirm = "Actually delete (default: false — dry-run unless confirmed)"
        )
    )]
    pub async fn delete(
        &self,
        org: String,
        name: String,
        forge: Forge,
        tag: String,
        confirm: Option<bool>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let is_dry_run = !confirm.unwrap_or(false);
        let forge_str = forge.as_str().to_string();

        stream! {
            let dry_prefix = if is_dry_run { "[dry-run] " } else { "" };

            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            let adapter = match make_registry_adapter(&forge_str, auth, &org) {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            if is_dry_run {
                yield HyperforgeEvent::Info {
                    message: format!("{dry_prefix}Would delete {name}:{tag} from {org}/{name} on {forge_str}"),
                };
                return;
            }

            match adapter.delete_image(&org, &name, &tag).await {
                Ok(()) => {
                    yield HyperforgeEvent::ImageDelete {
                        repo_name: name,
                        forge: forge_str.clone(),
                        tag,
                        success: true,
                        error: None,
                    };
                }
                Err(e) => {
                    yield HyperforgeEvent::ImageDelete {
                        repo_name: name,
                        forge: forge_str.clone(),
                        tag,
                        success: false,
                        error: Some(e.to_string()),
                    };
                }
            }
        }
    }
}

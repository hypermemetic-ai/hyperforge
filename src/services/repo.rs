//! `RepoService` - Single-repo operations and registry CRUD

use async_stream::stream;
use futures::Stream;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::adapters::{CodebergAdapter, ForgePort, GitHubAdapter, GitLabAdapter};
use crate::auth::YamlAuthProvider;
use crate::commands::materialize::{materialize, MaterializeOpts, MaterializeReport};
use crate::commands::{push, status};
use crate::config::HyperforgeConfig;
use crate::git::{build_remote_url_with, Git, Transport};
use crate::service::{HyperforgeEvent, TransportChangeFailureReason};
use crate::services::images::ImagesService;
use crate::services::releases::ReleasesService;
use crate::services::HyperforgeState;
use crate::types::{Forge, Repo, RepoRecord, Visibility};

/// Create a forge adapter for the given forge, org, and auth provider.
fn make_repo_adapter(
    forge: &Forge,
    auth: Arc<YamlAuthProvider>,
    org: &str,
) -> Result<Box<dyn ForgePort>, String> {
    match forge {
        Forge::GitHub => GitHubAdapter::new(auth, org)
            .map(|a| Box::new(a) as Box<dyn ForgePort>)
            .map_err(|e| format!("{forge:?}: {e}")),
        Forge::Codeberg => CodebergAdapter::new(auth, org)
            .map(|a| Box::new(a) as Box<dyn ForgePort>)
            .map_err(|e| format!("{forge:?}: {e}")),
        Forge::GitLab => GitLabAdapter::new(auth, org)
            .map(|a| Box::new(a) as Box<dyn ForgePort>)
            .map_err(|e| format!("{forge:?}: {e}")),
    }
}

/// Create an auth provider, returning a formatted error on failure.
fn make_auth() -> Result<Arc<YamlAuthProvider>, String> {
    YamlAuthProvider::new()
        .map(Arc::new)
        .map_err(|e| format!("Failed to create auth provider: {e}"))
}

/// Build a `HyperforgeEvent::Repo` from a `Repo` struct.
fn repo_event(repo: &crate::types::Repo) -> HyperforgeEvent {
    HyperforgeEvent::Repo {
        name: repo.name.clone(),
        description: repo.description.clone(),
        visibility: format!("{:?}", repo.visibility).to_lowercase(),
        origin: format!("{:?}", repo.origin).to_lowercase(),
        mirrors: repo
            .mirrors
            .iter()
            .map(|f| format!("{f:?}").to_lowercase())
            .collect(),
        protected: repo.protected,
        staged_for_deletion: repo.staged_for_deletion,
    }
}

/// Convert a `MaterializeReport` into a list of info events.
fn materialize_events(report: &MaterializeReport) -> Vec<HyperforgeEvent> {
    let mut events = Vec::new();
    if report.config_written {
        events.push(HyperforgeEvent::Info {
            message: "Updated on-disk config".to_string(),
        });
    }
    for remote in &report.remotes_added {
        events.push(HyperforgeEvent::Info {
            message: format!("Added remote: {remote}"),
        });
    }
    for remote in &report.remotes_updated {
        events.push(HyperforgeEvent::Info {
            message: format!("Updated remote: {remote}"),
        });
    }
    if report.hooks_installed {
        events.push(HyperforgeEvent::Info {
            message: "Installed pre-push hook".to_string(),
        });
    }
    events
}

/// Sub-service for single-repo operations and registry CRUD
#[derive(Clone)]
pub struct RepoService {
    pub(crate) state: HyperforgeState,
}

impl RepoService {
    pub const fn new(state: HyperforgeState) -> Self {
        Self { state }
    }
}

#[plexus_macros::activation(
    namespace = "repo",
    description = "Single-repo operations and registry CRUD",
    crate_path = "plexus_core"
)]
impl RepoService {
    /// Container image operations (list/pull/push/delete images in forge registries).
    #[plexus_macros::child]
    fn images(&self) -> ImagesService {
        ImagesService::new(self.state.clone())
    }

    /// Release operations (list/create/upload/download release artifacts).
    #[plexus_macros::child]
    fn releases(&self) -> ReleasesService {
        ReleasesService::new(self.state.clone())
    }

    /// List repositories for an organization (from `LocalForge`)
    #[plexus_macros::method(
        description = "List all repositories in the local forge for an organization",
        params(
            org = "Organization name",
            filter = "Regex pattern to filter repo names (optional)"
        )
    )]
    pub async fn list(
        &self,
        org: String,
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

            let local = state.get_local_forge(&org).await;

            match local.list_repos(&org).await {
                Ok(repos) => {
                    for repo in repos {
                        if let Some(ref re) = re {
                            if !re.is_match(&repo.name) {
                                continue;
                            }
                        }
                        yield repo_event(&repo);
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to list repos: {e}"),
                    };
                }
            }
        }
    }

    /// Create a new repository in `LocalForge`
    #[plexus_macros::method(
        description = "Create a new repository configuration",
        params(
            org = "Organization name",
            name = "Repository name",
            description = "Repository description (optional)",
            visibility = "Repository visibility: public or private",
            origin = "Origin forge: github, codeberg, or gitlab",
            mirrors = "Mirror forges (optional, comma-separated)"
        )
    )]
    pub async fn create(
        &self,
        org: String,
        name: String,
        description: Option<String>,
        visibility: String,
        origin: String,
        mirrors: Option<String>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            // Parse forge from string
            let origin_forge = if let Some(f) = HyperforgeConfig::parse_forge(&origin) { f } else {
                yield HyperforgeEvent::Error {
                    message: format!("Invalid origin forge: {origin}. Must be github, codeberg, or gitlab"),
                };
                return;
            };

            // Parse visibility
            let vis = match Visibility::parse(&visibility) {
                Ok(v) => v,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            // Parse mirrors
            let mirror_forges: Vec<Forge> = if let Some(m) = mirrors {
                m.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .filter_map(HyperforgeConfig::parse_forge)
                    .collect()
            } else {
                Vec::new()
            };

            // Build repo
            let mut repo = Repo::new(name, origin_forge).with_visibility(vis);
            if let Some(desc) = description {
                repo = repo.with_description(desc);
            }
            repo = repo.with_mirrors(mirror_forges);

            // Get or create LocalForge with persistence
            let local = state.get_local_forge(&org).await;

            match local.create_repo(&org, &repo).await {
                Ok(()) => {
                    // Save to YAML
                    if let Err(e) = local.save_to_yaml().await {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to save repos.yaml: {e}"),
                        };
                        return;
                    }

                    yield HyperforgeEvent::Info {
                        message: format!("Created repository: {}", repo.name),
                    };
                    yield repo_event(&repo);
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to create repo: {e}"),
                    };
                }
            }
        }
    }

    /// Update an existing repository
    #[plexus_macros::method(
        description = "Update repository configuration",
        params(
            org = "Organization name",
            name = "Repository name",
            description = "New repository description (optional)",
            visibility = "New visibility: public or private (optional)"
        )
    )]
    pub async fn update(
        &self,
        org: String,
        name: String,
        description: Option<String>,
        visibility: Option<String>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let local = state.get_local_forge(&org).await;

            // Get existing repo
            let mut repo = match local.get_repo(&org, &name).await {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to get repo: {e}"),
                    };
                    return;
                }
            };

            // Update fields
            if let Some(desc) = description {
                repo.description = Some(desc);
            }

            if let Some(vis) = visibility {
                repo.visibility = match Visibility::parse(&vis) {
                    Ok(v) => v,
                    Err(e) => {
                        yield HyperforgeEvent::Error { message: e };
                        return;
                    }
                };
            }

            match local.update_repo(&org, &repo).await {
                Ok(()) => {
                    if let Err(e) = local.save_to_yaml().await {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to save repos.yaml: {e}"),
                        };
                        return;
                    }

                    yield HyperforgeEvent::Info {
                        message: format!("Updated repository: {}", repo.name),
                    };

                    // Materialize to disk if the record has a local_path
                    if let Ok(record) = local.get_record(&repo.name) {
                        if let Some(ref local_path) = record.local_path {
                            match materialize(&org, &record, local_path, MaterializeOpts::default()) {
                                Ok(report) => {
                                    for event in materialize_events(&report) {
                                        yield event;
                                    }
                                }
                                Err(e) => {
                                    yield HyperforgeEvent::Error {
                                        message: format!("Failed to materialize config: {e}"),
                                    };
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to update repo: {e}"),
                    };
                }
            }
        }
    }

    /// Soft-delete a repository: privatize on remote forges, then mark dismissed locally
    #[plexus_macros::method(
        description = "Soft-delete a repository: privatize on remotes, mark dismissed locally (record preserved in repos.yaml)",
        params(
            org = "Organization name",
            name = "Repository name"
        )
    )]
    pub async fn delete(
        &self,
        org: String,
        name: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let local = state.get_local_forge(&org).await;

            // Get the record to find which forges it's on
            let record = match local.get_record(&name) {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repository not found: {e}"),
                    };
                    return;
                }
            };

            // Protected repos cannot be deleted
            if record.protected {
                yield HyperforgeEvent::Error {
                    message: format!("Cannot delete '{name}': repo is protected. Remove protection first with: repo update --org {org} --name {name} --protected false"),
                };
                return;
            }

            // Privatize on each remote forge
            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            let mut privatize_errors = Vec::new();
            let mut privatized_forges = Vec::new();
            for forge in &record.present_on {
                let adapter = match make_repo_adapter(forge, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        privatize_errors.push(e);
                        continue;
                    }
                };

                // Make private on remote
                let private_repo = Repo::new(&name, forge.clone())
                    .with_visibility(Visibility::Private);
                match adapter.update_repo(&org, &private_repo).await {
                    Ok(()) => {
                        privatized_forges.push(forge.clone());
                        yield HyperforgeEvent::Info {
                            message: format!("Made private on {forge:?}"),
                        };
                    }
                    Err(e) => {
                        privatize_errors.push(format!("{forge:?}: {e}"));
                    }
                }
            }

            for error in &privatize_errors {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to privatize - {error}"),
                };
            }

            // Soft-delete locally (always, even if remote privatization had errors)
            match local.delete_repo(&org, &name).await {
                Ok(()) => {
                    // Record which forges were successfully privatized + update visibility
                    if let Ok(mut rec) = local.get_record(&name) {
                        for f in &privatized_forges {
                            rec.privatized_on.insert(f.clone());
                        }
                        if !privatized_forges.is_empty() {
                            rec.visibility = Visibility::Private;
                        }
                        let _ = local.update_record(&rec);
                    }

                    if let Err(e) = local.save_to_yaml().await {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to save repos.yaml: {e}"),
                        };
                        return;
                    }

                    yield HyperforgeEvent::Info {
                        message: format!(
                            "Soft-deleted repository: {name} (privatized on remotes, record preserved in repos.yaml)"
                        ),
                    };
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to delete repo: {e}"),
                    };
                }
            }

            if !privatize_errors.is_empty() {
                yield HyperforgeEvent::Error {
                    message: format!("Completed with {} privatization error(s)", privatize_errors.len()),
                };
            }
        }
    }

    /// Purge a soft-deleted repository: hard-delete from remote forges and remove from repos.yaml
    ///
    /// Only works on dismissed (soft-deleted) repos that have been privatized.
    /// Protected repos cannot be purged — remove protection first.
    #[plexus_macros::method(
        description = "Hard-delete a dismissed repo from remote forges and remove from repos.yaml. Requires repo to be dismissed and not protected.",
        params(
            org = "Organization name",
            name = "Repository name (must be dismissed)"
        )
    )]
    pub async fn purge(
        &self,
        org: String,
        name: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let local = state.get_local_forge(&org).await;

            let record = match local.get_record(&name) {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repository not found: {e}"),
                    };
                    return;
                }
            };

            // Must be dismissed first
            if !record.dismissed {
                yield HyperforgeEvent::Error {
                    message: format!("Cannot purge '{name}': repo is not dismissed. Run 'repo delete' first."),
                };
                return;
            }

            // Protected repos cannot be purged
            if record.protected {
                yield HyperforgeEvent::Error {
                    message: format!("Cannot purge '{name}': repo is protected. Remove protection first with: repo update --org {org} --name {name} --protected false"),
                };
                return;
            }

            // Delete from each remote forge
            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            let mut delete_errors = Vec::new();
            let mut deleted_forges = Vec::new();
            let forges_to_delete: Vec<_> = record.present_on.iter().cloned().collect();
            for forge in &forges_to_delete {
                let adapter = match make_repo_adapter(forge, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        delete_errors.push(e);
                        continue;
                    }
                };

                match adapter.delete_repo(&org, &name).await {
                    Ok(()) => {
                        deleted_forges.push(forge.clone());
                        yield HyperforgeEvent::Info {
                            message: format!("Deleted from {forge:?}"),
                        };
                    }
                    Err(e) => {
                        delete_errors.push(format!("{forge:?}: {e}"));
                    }
                }
            }

            // Track partial progress in LocalForge: update present_on and deleted_from
            if !deleted_forges.is_empty() {
                if let Ok(mut rec) = local.get_record(&name) {
                    for f in &deleted_forges {
                        rec.present_on.remove(f);
                        if !rec.deleted_from.contains(f) {
                            rec.deleted_from.push(f.clone());
                        }
                    }
                    let _ = local.update_record(&rec);
                    let _ = local.save_to_yaml().await;
                }
            }

            for error in &delete_errors {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to delete - {error}"),
                };
            }

            // Hard-delete locally (remove record from repos.yaml entirely)
            if delete_errors.is_empty() {
                if let Err(e) = local.remove_repo(&name) {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to remove local record: {e}"),
                    };
                } else {
                    if let Err(e) = local.save_to_yaml().await {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to save repos.yaml: {e}"),
                        };
                        return;
                    }
                    yield HyperforgeEvent::Info {
                        message: format!("Purged repository: {name} (deleted from all remotes, removed from repos.yaml)"),
                    };
                }
            } else {
                yield HyperforgeEvent::Error {
                    message: format!("Purge incomplete: {} remote deletion error(s). Local record preserved.", delete_errors.len()),
                };
            }
        }
    }

    /// Rename a repository on remote forge(s) and in local config
    #[plexus_macros::method(
        description = "Rename a repository on remote forge(s) and update local configuration",
        params(
            org = "Organization name",
            old_name = "Current repository name",
            new_name = "New repository name",
            forges = "Comma-separated forges to rename on (optional, defaults to origin only)"
        )
    )]
    pub async fn rename(
        &self,
        org: String,
        old_name: String,
        new_name: String,
        forges: Option<String>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            // Get local forge and verify repo exists
            let local = state.get_local_forge(&org).await;

            let repo = match local.get_repo(&org, &old_name).await {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repository not found in local config: {e}"),
                    };
                    return;
                }
            };

            // Determine which forges to rename on
            let target_forges: Vec<Forge> = if let Some(forge_list) = forges {
                forge_list
                    .split(',')
                    .filter_map(|f| HyperforgeConfig::parse_forge(f.trim()))
                    .collect()
            } else {
                // Default to origin only
                vec![repo.origin.clone()]
            };

            // Get auth provider
            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            // Rename on each target forge
            let mut errors = Vec::new();
            for forge in &target_forges {
                let adapter = match make_repo_adapter(forge, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                };

                match adapter.rename_repo(&org, &old_name, &new_name).await {
                    Ok(()) => {
                        yield HyperforgeEvent::Info {
                            message: format!("Renamed on {forge:?}: {old_name} -> {new_name}"),
                        };
                    }
                    Err(e) => {
                        errors.push(format!("{forge:?}: {e}"));
                    }
                }
            }

            // Report any errors from remote renames
            for error in &errors {
                yield HyperforgeEvent::Error {
                    message: format!("Remote rename failed - {error}"),
                };
            }

            // Update local config regardless of remote errors (user may want to fix manually)
            match local.rename_repo(&org, &old_name, &new_name).await {
                Ok(()) => {
                    if let Err(e) = local.save_to_yaml().await {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to save repos.yaml: {e}"),
                        };
                        return;
                    }

                    yield HyperforgeEvent::Info {
                        message: format!("Local config updated: {old_name} -> {new_name}"),
                    };

                    // Materialize to disk if the renamed record has a local_path
                    if let Ok(record) = local.get_record(&new_name) {
                        if let Some(ref local_path) = record.local_path {
                            match materialize(&org, &record, local_path, MaterializeOpts::default()) {
                                Ok(report) => {
                                    for event in materialize_events(&report) {
                                        yield event;
                                    }
                                }
                                Err(e) => {
                                    yield HyperforgeEvent::Error {
                                        message: format!("Failed to materialize config: {e}"),
                                    };
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to update local config: {e}"),
                    };
                }
            }

            if errors.is_empty() {
                yield HyperforgeEvent::Info {
                    message: format!("Renamed repository: {old_name} -> {new_name}"),
                };
            } else {
                yield HyperforgeEvent::Error {
                    message: format!("Rename completed with {} error(s)", errors.len()),
                };
            }
        }
    }

    /// Set or unset archive status on remote forges
    #[plexus_macros::method(
        description = "Archive or unarchive a repository on its remote forges",
        params(
            org = "Organization name",
            name = "Repository name",
            archived = "Set archived (true) or unarchived (false)"
        )
    )]
    pub async fn set_archived(
        &self,
        org: String,
        name: String,
        archived: bool,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let local = state.get_local_forge(&org).await;

            let repo = match local.get_repo(&org, &name).await {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repository not found in local config: {e}"),
                    };
                    return;
                }
            };

            let mut target_forges = vec![repo.origin.clone()];
            for mirror in &repo.mirrors {
                if !target_forges.contains(mirror) {
                    target_forges.push(mirror.clone());
                }
            }

            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            let action = if archived { "Archived" } else { "Unarchived" };
            let mut errors = Vec::new();
            for forge in &target_forges {
                let adapter = match make_repo_adapter(forge, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                };

                match adapter.set_archived(&org, &name, archived).await {
                    Ok(()) => {
                        yield HyperforgeEvent::Info {
                            message: format!("{action} '{name}' on {forge:?}"),
                        };
                    }
                    Err(e) => {
                        errors.push(format!("{forge:?}: {e}"));
                    }
                }
            }

            for error in &errors {
                yield HyperforgeEvent::Error {
                    message: format!("  Error: {error}"),
                };
            }

            // Update local state
            if let Err(e) = local.set_archived(&org, &name, archived).await {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to update local state: {e}"),
                };
            }
        }
    }

    /// Set the default branch on remote forges and optionally checkout locally
    #[plexus_macros::method(
        description = "Set the default branch on remote forges for a repository, and optionally git checkout locally",
        params(
            org = "Organization name",
            name = "Repository name",
            branch = "Branch to set as default",
            checkout = "Also run git checkout locally (optional, default: false)",
            path = "Local repo path for checkout (required if --checkout is true)"
        )
    )]
    pub async fn set_default_branch(
        &self,
        org: String,
        name: String,
        branch: String,
        checkout: Option<bool>,
        path: Option<String>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            // Get local forge to find repo config (forges)
            let local = state.get_local_forge(&org).await;

            let repo = match local.get_repo(&org, &name).await {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repository not found in local config: {e}"),
                    };
                    return;
                }
            };

            // Collect all forges (origin + mirrors)
            let mut target_forges = vec![repo.origin.clone()];
            for mirror in &repo.mirrors {
                if !target_forges.contains(mirror) {
                    target_forges.push(mirror.clone());
                }
            }

            // Get auth provider
            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            // Set default branch on each forge
            let mut errors = Vec::new();
            for forge in &target_forges {
                let adapter = match make_repo_adapter(forge, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                };

                match adapter.set_default_branch(&org, &name, &branch).await {
                    Ok(()) => {
                        yield HyperforgeEvent::Info {
                            message: format!("Set default branch to '{branch}' on {forge:?}"),
                        };
                    }
                    Err(e) => {
                        errors.push(format!("{forge:?}: {e}"));
                    }
                }
            }

            // Report errors
            for error in &errors {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to set default branch - {error}"),
                };
            }

            // Update LocalForge record with the new default branch
            if errors.is_empty() {
                if let Err(e) = local.set_default_branch(&org, &name, &branch).await {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to update LocalForge default_branch: {e}"),
                    };
                } else if let Err(e) = local.save_to_yaml().await {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to save repos.yaml: {e}"),
                    };
                }
            }

            // Optionally checkout locally
            if checkout.unwrap_or(false) {
                if let Some(ref repo_path) = path {
                    let repo_path = std::path::Path::new(repo_path);
                    match crate::git::Git::checkout(repo_path, &branch) {
                        Ok(()) => {
                            yield HyperforgeEvent::Info {
                                message: format!("Checked out '{branch}' locally"),
                            };
                        }
                        Err(e) => {
                            yield HyperforgeEvent::Error {
                                message: format!("Git checkout failed: {e}"),
                            };
                        }
                    }
                } else {
                    yield HyperforgeEvent::Error {
                        message: "--path is required when --checkout is true".to_string(),
                    };
                }
            }

            if errors.is_empty() {
                yield HyperforgeEvent::Info {
                    message: format!("Default branch set to '{branch}' on all forges"),
                };
            } else {
                yield HyperforgeEvent::Error {
                    message: format!("Completed with {} error(s)", errors.len()),
                };
            }
        }
    }

    /// Import repositories from a remote forge
    #[plexus_macros::method(
        description = "Import repository configurations from a remote forge (GitHub, Codeberg, GitLab)",
        params(
            org = "Organization name",
            forge = "Source forge: github, codeberg, or gitlab"
        )
    )]
    pub async fn import(
        &self,
        org: String,
        forge: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            // Parse forge
            let source_forge = if let Some(f) = HyperforgeConfig::parse_forge(&forge) { f } else {
                yield HyperforgeEvent::Error {
                    message: format!("Invalid forge: {forge}. Must be github, codeberg, or gitlab"),
                };
                return;
            };

            // Get forge adapter
            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };
            let adapter: Arc<dyn ForgePort> = match make_repo_adapter(&source_forge, auth, &org) {
                Ok(a) => Arc::from(a),
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            yield HyperforgeEvent::Info {
                message: format!("Fetching repositories from {forge} for {org}..."),
            };

            // List repos from remote forge
            let repos = match adapter.list_repos(&org).await {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to list repos from {forge}: {e}"),
                    };
                    return;
                }
            };

            yield HyperforgeEvent::Info {
                message: format!("Found {} repositories", repos.len()),
            };

            // Get local forge
            let local = state.get_local_forge(&org).await;

            // Import each repo
            let mut imported = 0;
            let mut skipped = 0;
            let mut errors = 0;

            for repo in repos {
                // Check if already exists
                let exists = match local.repo_exists(&org, &repo.name).await {
                    Ok(exists) => exists,
                    Err(e) => {
                        errors += 1;
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to check if {} exists: {}", repo.name, e),
                        };
                        continue;
                    }
                };

                if exists {
                    skipped += 1;
                    continue;
                }

                // Create in local forge
                match local.create_repo(&org, &repo).await {
                    Ok(()) => {
                        imported += 1;
                        yield repo_event(&repo);
                    }
                    Err(e) => {
                        errors += 1;
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to import {}: {}", repo.name, e),
                        };
                    }
                }
            }

            // Save to YAML
            if let Err(e) = local.save_to_yaml().await {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to save repos.yaml: {e}"),
                };
                return;
            }

            yield HyperforgeEvent::Info {
                message: format!(
                    "Import complete: {imported} imported, {skipped} skipped (already exist), {errors} errors"
                ),
            };
        }
    }

    /// Initialize hyperforge for a git repository
    #[plexus_macros::method(
        description = "Initialize hyperforge configuration for a repository",
        params(
            path = "Repository path (absolute)",
            forges = "Comma-separated list of forges (github,codeberg,gitlab)",
            org = "Organization/username on forges",
            repo_name = "Repository name (optional, defaults to directory name)",
            visibility = "Repository visibility: public or private (optional, default: public)",
            description = "Repository description (optional)",
            ssh_keys = "SSH keys per forge in format 'forge:path,forge:path' (optional)",
            force = "Force reinitialize even if config exists (optional, default: false)",
            dry_run = "Preview changes without applying (optional, default: false)",
            no_hooks = "Skip installing pre-push hook (optional, default: false)",
            no_ssh_wrapper = "Skip configuring SSH wrapper (optional, default: false)",
            transport = "Remote URL transport: ssh or https (optional, default: ssh). Controls the shape of `git remote` URLs written during init."
        )
    )]
    pub async fn init(
        &self,
        path: String,
        forges: String,
        org: String,
        repo_name: Option<String>,
        visibility: Option<String>,
        description: Option<String>,
        ssh_keys: Option<String>,
        force: Option<bool>,
        dry_run: Option<bool>,
        no_hooks: Option<bool>,
        no_ssh_wrapper: Option<bool>,
        transport: Option<Transport>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let repo_path = PathBuf::from(&path);
            let is_dry_run = dry_run.unwrap_or(false);

            // Parse forges
            let forge_list: Vec<String> = forges.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            if forge_list.is_empty() {
                yield HyperforgeEvent::Error {
                    message: "At least one forge required".to_string(),
                };
                return;
            }

            // Parse visibility
            let vis = match visibility.as_deref() {
                None => Visibility::Public,
                Some(s) => match Visibility::parse(s) {
                    Ok(v) => v,
                    Err(e) => {
                        yield HyperforgeEvent::Error { message: e };
                        return;
                    }
                },
            };

            // Parse SSH keys into a HashMap
            let mut ssh: HashMap<String, String> = HashMap::new();
            if let Some(keys_str) = ssh_keys {
                for pair in keys_str.split(',') {
                    let parts: Vec<&str> = pair.trim().split(':').collect();
                    if parts.len() == 2 {
                        ssh.insert(parts[0].to_string(), parts[1].to_string());
                    }
                }
            }

            // Derive repo name from dir name or explicit param
            let name = repo_name.unwrap_or_else(|| {
                repo_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            });

            // Parse forge strings into Forge enums for present_on
            let mut present_on = HashSet::new();
            for f in &forge_list {
                if let Some(forge_enum) = HyperforgeConfig::parse_forge(f) {
                    present_on.insert(forge_enum);
                }
            }

            // Build or update a RepoRecord
            let local = state.get_local_forge(&org).await;

            let mut record = match local.get_record(&name) {
                Ok(existing) => existing,
                Err(_) => RepoRecord {
                    name: name.clone(),
                    description: None,
                    visibility: Visibility::Public,
                    default_branch: "main".to_string(),
                    present_on: HashSet::new(),
                    protected: false,
                    managed: false,
                    dismissed: false,
                    deleted_from: Vec::new(),
                    deleted_at: None,
                    privatized_on: HashSet::new(),
                    previous_names: Vec::new(),
                    local_path: None,
                    forges: Vec::new(),
                    ssh: HashMap::new(),
                    forge_config: HashMap::new(),
                    ci: None,
                    dist: None,
                },
            };

            // Set config-first fields
            record.local_path = Some(repo_path.clone());
            record.forges = forge_list.clone();
            record.visibility = vis;
            record.ssh = ssh;
            for forge in &present_on {
                record.present_on.insert(forge.clone());
            }
            if let Some(desc) = description {
                record.description = Some(desc);
            }

            // Check if config already exists (unless --force)
            let config_exists = HyperforgeConfig::exists(&repo_path);
            if config_exists && !force.unwrap_or(false) {
                yield HyperforgeEvent::Error {
                    message: "Config already exists. Use --force to reinitialize.".to_string(),
                };
                return;
            }

            // Register in LocalForge
            if !is_dry_run {
                if let Err(e) = local.upsert_record(record.clone()) {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to register in LocalForge: {e}"),
                    };
                    return;
                }

                if let Err(e) = local.save_to_yaml().await {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to save repos.yaml: {e}"),
                    };
                    return;
                }

                yield HyperforgeEvent::Info {
                    message: format!("Registered {name} in LocalForge for org {org}"),
                };
            }

            // Materialize config + remotes + hooks onto disk
            let opts = MaterializeOpts {
                config: true,
                remotes: true,
                hooks: !no_hooks.unwrap_or(false),
                ssh_wrapper: !no_ssh_wrapper.unwrap_or(false),
                dry_run: is_dry_run,
                auto_commit: true,
                transport,
            };

            match materialize(&org, &record, &repo_path, opts) {
                Ok(report) => {
                    if is_dry_run {
                        yield HyperforgeEvent::Info {
                            message: "[DRY RUN] Would initialize hyperforge".to_string(),
                        };
                    }

                    // Override config_written message for init (shows path)
                    if report.config_written {
                        yield HyperforgeEvent::Info {
                            message: format!("Created config at {}", repo_path.join(".hyperforge/config.toml").display()),
                        };
                    }

                    for remote in &report.remotes_added {
                        yield HyperforgeEvent::Info {
                            message: format!("Added remote: {remote}"),
                        };
                    }

                    for remote in &report.remotes_updated {
                        yield HyperforgeEvent::Info {
                            message: format!("Updated remote: {remote}"),
                        };
                    }

                    if report.hooks_installed {
                        yield HyperforgeEvent::Info {
                            message: "Installed pre-push hook".to_string(),
                        };
                    }

                    if report.ssh_configured {
                        yield HyperforgeEvent::Info {
                            message: "Configured SSH wrapper".to_string(),
                        };
                    }

                    if report.auto_committed {
                        yield HyperforgeEvent::Info {
                            message: "Auto-committed .hyperforge/ config".to_string(),
                        };
                    }

                    for warning in &report.warnings {
                        yield HyperforgeEvent::Info {
                            message: format!("⚠ {warning}"),
                        };
                    }

                    yield HyperforgeEvent::Info {
                        message: "Hyperforge initialized successfully".to_string(),
                    };
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Materialize failed: {e}"),
                    };
                }
            }
        }
    }

    /// Switch the remote URL transport of a hyperforge-registered repo.
    ///
    /// Idempotent: running the same call twice in a row is a no-op on the
    /// second call — no `git remote set-url` is invoked and no failure is
    /// emitted. Current transport is read live from `git remote`, so repos
    /// that were initialized before this method existed also work.
    #[plexus_macros::method(
        description = "Switch the remote URL transport (ssh<->https) of a hyperforge-registered repo. Idempotent: a second call with the same transport is a success no-op.",
        params(
            path = "Repository path (absolute)",
            transport = "Target transport: ssh or https"
        )
    )]
    pub async fn set_transport(
        &self,
        path: String,
        transport: Transport,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        stream! {
            let repo_path = std::path::Path::new(&path);

            // 1. The path must exist and be a directory.
            if !repo_path.exists() {
                yield HyperforgeEvent::TransportChangeFailed {
                    path: path.clone(),
                    reason: TransportChangeFailureReason::PathNotFound,
                    message: format!("Path does not exist: {path}"),
                };
                return;
            }

            // 2. The path must be a hyperforge-registered repo — same
            //    class of rejection as `repo status` when it errors with
            //    `NotInitialized`. Callers who haven't run `repo init`
            //    yet get this failure, matching the acceptance criterion
            //    for "target repo is not hyperforge-registered".
            if !HyperforgeConfig::exists(repo_path) {
                yield HyperforgeEvent::TransportChangeFailed {
                    path: path.clone(),
                    reason: TransportChangeFailureReason::NotInitialized,
                    message: format!(
                        "Not a hyperforge repository at {path}. Run `repo init` first.",
                    ),
                };
                return;
            }

            // 3. Must be a git repo so we can read and rewrite remotes.
            if !Git::is_repo(repo_path) {
                yield HyperforgeEvent::TransportChangeFailed {
                    path: path.clone(),
                    reason: TransportChangeFailureReason::NotAGitRepo,
                    message: format!("Not a git repository at {path}"),
                };
                return;
            }

            // 4. Load the config to know which forges to touch. Each
            //    forge maps to a named remote (`origin` for the first,
            //    forge-name for the others), respecting any
            //    `forge_config.remote` override.
            let config = match HyperforgeConfig::load(repo_path) {
                Ok(c) => c,
                Err(e) => {
                    yield HyperforgeEvent::TransportChangeFailed {
                        path: path.clone(),
                        reason: TransportChangeFailureReason::ConfigError,
                        message: format!("Failed to load .hyperforge/config.toml: {e}"),
                    };
                    return;
                }
            };

            let current_remotes = match Git::list_remotes(repo_path) {
                Ok(rs) => rs,
                Err(e) => {
                    yield HyperforgeEvent::TransportChangeFailed {
                        path: path.clone(),
                        reason: TransportChangeFailureReason::GitError,
                        message: format!("Failed to list git remotes: {e}"),
                    };
                    return;
                }
            };

            // 5. For each configured forge, compute the desired URL at
            //    the requested transport and compare to what's on disk.
            //    Record the remotes whose URL actually changes. Remotes
            //    that the config declares but `git remote` doesn't have
            //    yet are silently skipped — those never got through
            //    `repo init`'s add-remote step, so there's nothing to
            //    rewrite; the next init or manual add picks up the
            //    transport from whatever is requested then.
            let mut changed: Vec<String> = Vec::new();

            for forge_str in &config.forges {
                let remote_name = config.remote_for_forge(forge_str);
                let org_for_forge = config
                    .forge_config
                    .get(forge_str)
                    .and_then(|fc| fc.org.as_deref())
                    .or(config.org.as_deref())
                    .unwrap_or("");
                let repo_name = config.get_repo_name(repo_path);

                if org_for_forge.is_empty() {
                    // Without an org we cannot compute a valid URL.
                    // Treat this as a config error.
                    yield HyperforgeEvent::TransportChangeFailed {
                        path: path.clone(),
                        reason: TransportChangeFailureReason::ConfigError,
                        message: format!(
                            "No org configured for forge '{forge_str}' in {path}",
                        ),
                    };
                    return;
                }

                let desired_url = build_remote_url_with(
                    forge_str,
                    org_for_forge,
                    &repo_name,
                    transport,
                );

                // Remotes declared in config but absent from `git remote`
                // are left alone — `repo init`'s add-remote step never
                // got them up, and the next init or manual add picks up
                // whichever transport is requested then.
                if let Some(existing) = current_remotes.iter().find(|r| r.name == remote_name) {
                    if existing.fetch_url == desired_url {
                        // Already at requested transport (and URL) — idempotent no-op path.
                        continue;
                    }
                    if let Err(e) = Git::set_remote_url(repo_path, &remote_name, &desired_url) {
                        yield HyperforgeEvent::TransportChangeFailed {
                            path: path.clone(),
                            reason: TransportChangeFailureReason::GitError,
                            message: format!(
                                "Failed to set remote '{remote_name}' URL: {e}",
                            ),
                        };
                        return;
                    }
                    changed.push(remote_name.clone());
                }
            }

            // 6. Report outcome. If nothing actually changed on disk the
            //    call is a pure no-op — emit TransportUnchanged without
            //    ever invoking `git remote set-url`. (If every configured
            //    remote was missing from `git remote` that's still a
            //    no-op for transport purposes — the caller hasn't set
            //    those remotes up yet; the next `repo init` or manual
            //    add will use whatever transport is passed to it.)
            if changed.is_empty() {
                yield HyperforgeEvent::TransportUnchanged {
                    path: path.clone(),
                    transport,
                };
            } else {
                yield HyperforgeEvent::TransportChanged {
                    path: path.clone(),
                    transport,
                    remotes_changed: changed,
                };
            }
        }
    }

    /// Show git repository status
    #[plexus_macros::method(
        description = "Show git repository sync status across all configured forges",
        params(
            path = "Repository path (absolute)"
        )
    )]
    pub async fn status(
        &self,
        path: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        stream! {
            let repo_path = std::path::Path::new(&path);
            let path_str = path.clone();

            match status::status(repo_path) {
                Ok(report) => {
                    // Emit one RepoTransport per forge so callers can read
                    // the current transport without parsing `git remote -v`.
                    // Transport is detected live from the stored URL, so
                    // pre-TRANSPORT repos report correctly.
                    for forge_status in &report.forges {
                        let transport = forge_status
                            .remote_url
                            .as_deref()
                            .and_then(Transport::detect);
                        yield HyperforgeEvent::RepoTransport {
                            path: path_str.clone(),
                            remote: forge_status.remote_name.clone(),
                            forge: forge_status.forge.clone(),
                            transport,
                            url: forge_status.remote_url.clone(),
                        };
                    }
                    // Current branch
                    yield HyperforgeEvent::Info {
                        message: format!("On branch: {}", report.branch),
                    };

                    // Working tree status
                    if report.has_changes || report.has_staged {
                        yield HyperforgeEvent::Info {
                            message: "Working tree has changes".to_string(),
                        };
                    } else {
                        yield HyperforgeEvent::Info {
                            message: "Working tree clean".to_string(),
                        };
                    }

                    // Forge status
                    for forge_status in report.forges {
                        let symbol = if forge_status.is_up_to_date() {
                            "✓"
                        } else if forge_status.ahead > 0 && forge_status.behind > 0 {
                            "↕"
                        } else if forge_status.ahead > 0 {
                            "↑"
                        } else if forge_status.behind > 0 {
                            "↓"
                        } else {
                            "✗"
                        };

                        let mut msg = format!("{} {} ({})",
                            symbol,
                            forge_status.forge,
                            forge_status.remote_name
                        );

                        if forge_status.ahead > 0 || forge_status.behind > 0 {
                            msg.push_str(&format!(" ↑{} ↓{}", forge_status.ahead, forge_status.behind));
                        }

                        if let Some(err) = forge_status.error {
                            msg.push_str(&format!(" - {err}"));
                        }

                        yield HyperforgeEvent::Info { message: msg };
                    }

                    // SSH configuration health
                    match (&report.ssh_command, &report.hyperforge_org) {
                        (Some(cmd), Some(org)) if cmd == "hyperforge-ssh" => {
                            yield HyperforgeEvent::Info {
                                message: format!("SSH: hyperforge-ssh (org: {org})"),
                            };
                        }
                        (Some(cmd), None) if cmd == "hyperforge-ssh" => {
                            yield HyperforgeEvent::Error {
                                message: "SSH: hyperforge-ssh configured but hyperforge.org NOT SET — pushes will use wrong key".to_string(),
                            };
                        }
                        (Some(cmd), _) => {
                            yield HyperforgeEvent::Info {
                                message: format!("SSH: custom ({cmd})"),
                            };
                        }
                        (None, _) => {
                            yield HyperforgeEvent::Info {
                                message: "SSH: system default".to_string(),
                            };
                        }
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Status failed: {e}"),
                    };
                }
            }
        }
    }

    /// Push to configured forges
    #[plexus_macros::method(
        description = "Push current branch to all configured forges",
        params(
            path = "Repository path (absolute)",
            set_upstream = "Set upstream tracking (optional, default: false)",
            force = "Force push (optional, default: false)",
            dry_run = "Preview push without executing (optional, default: false)",
            only_forges = "Only push to specific forges, comma-separated (optional)"
        )
    )]
    pub async fn push(
        &self,
        path: String,
        set_upstream: Option<bool>,
        force: Option<bool>,
        dry_run: Option<bool>,
        only_forges: Option<String>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        stream! {
            let repo_path = std::path::Path::new(&path);

            // Build options
            let mut options = push::PushOptions::new();

            if set_upstream.unwrap_or(false) {
                options = options.set_upstream();
            }

            if force.unwrap_or(false) {
                options = options.force();
            }

            if dry_run.unwrap_or(false) {
                options = options.dry_run();
            }

            if let Some(forges_str) = only_forges {
                let forges: Vec<String> = forges_str.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                options = options.only(forges);
            }

            // Execute push
            match push::push(repo_path, options) {
                Ok(report) => {
                    if report.dry_run {
                        yield HyperforgeEvent::Info {
                            message: "[DRY RUN] Would push to forges".to_string(),
                        };
                    }

                    for result in report.results {
                        if result.success {
                            yield HyperforgeEvent::Info {
                                message: format!("✓ Pushed {} to {} ({})",
                                    result.branch,
                                    result.forge,
                                    result.remote_name
                                ),
                            };
                        } else {
                            yield HyperforgeEvent::Error {
                                message: format!("✗ Failed to push to {}: {}",
                                    result.forge,
                                    result.error.as_deref().unwrap_or("unknown error")
                                ),
                            };
                        }
                    }

                    if report.all_success {
                        yield HyperforgeEvent::Info {
                            message: "All pushes succeeded".to_string(),
                        };
                    } else {
                        yield HyperforgeEvent::Error {
                            message: "Some pushes failed".to_string(),
                        };
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Push failed: {e}"),
                    };
                }
            }
        }
    }

    /// Clone a repository from `LocalForge`
    #[plexus_macros::method(
        description = "Clone a repository by name from LocalForge, auto-initialize with hyperforge config",
        params(
            org = "Organization name",
            name = "Repository name (must exist in LocalForge)",
            path = "Target directory path (optional, defaults to ./<name>)",
            forge = "Preferred forge to clone from (optional, defaults to first in present_on)"
        )
    )]
    pub async fn clone(
        &self,
        org: String,
        name: String,
        path: Option<String>,
        forge: Option<String>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            // 1. Lookup repo in LocalForge
            let local = state.get_local_forge(&org).await;

            let record = match local.get_record(&name) {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repository not found in LocalForge: {e}"),
                    };
                    return;
                }
            };

            // 2. Pick clone forge
            let clone_forge = if let Some(ref forge_str) = forge {
                if let Some(f) = HyperforgeConfig::parse_forge(forge_str) {
                    if !record.present_on.contains(&f) {
                        yield HyperforgeEvent::Error {
                            message: format!("Repository not present on forge: {forge_str}"),
                        };
                        return;
                    }
                    f
                } else {
                    yield HyperforgeEvent::Error {
                        message: format!("Invalid forge: {forge_str}. Must be github, codeberg, or gitlab"),
                    };
                    return;
                }
            } else {
                // Use first forge from present_on
                if let Some(f) = record.present_on.iter().next() { f.clone() } else {
                    yield HyperforgeEvent::Error {
                        message: "Repository has no forges in present_on".to_string(),
                    };
                    return;
                }
            };

            // 3. Build clone URL
            let forge_str = format!("{clone_forge:?}").to_lowercase();
            let clone_url = crate::git::build_remote_url(&forge_str, &org, &name);

            // 4. Determine target path
            let target_path = path.unwrap_or_else(|| name.clone());

            yield HyperforgeEvent::Info {
                message: format!("Cloning {name} from {forge_str} into {target_path}"),
            };

            // 5. Clone
            if let Err(e) = crate::git::Git::clone(&clone_url, &target_path) {
                yield HyperforgeEvent::Error {
                    message: format!("Git clone failed: {e}"),
                };
                return;
            }

            yield HyperforgeEvent::Info {
                message: "Clone successful".to_string(),
            };

            // 6. Set local_path on the record and materialize
            let clone_path = PathBuf::from(&target_path);

            // Update record with local_path and ensure forges list is populated
            let mut updated_record = record.clone();
            updated_record.local_path = Some(clone_path.clone());
            if updated_record.forges.is_empty() {
                updated_record.forges = updated_record.present_on.iter()
                    .map(|f| format!("{f:?}").to_lowercase())
                    .collect();
            }

            // Materialize config + remotes onto disk
            match materialize(&org, &updated_record, &clone_path, MaterializeOpts::default()) {
                Ok(report) => {
                    // Override config_written message for clone (shows provenance)
                    if report.config_written {
                        yield HyperforgeEvent::Info {
                            message: "Generated .hyperforge/config.toml from LocalForge metadata".to_string(),
                        };
                    }
                    for remote in &report.remotes_added {
                        yield HyperforgeEvent::Info {
                            message: format!("Added remote: {remote}"),
                        };
                    }
                    for remote in &report.remotes_updated {
                        yield HyperforgeEvent::Info {
                            message: format!("Updated remote: {remote}"),
                        };
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to materialize config: {e}"),
                    };
                    // Continue anyway - clone succeeded
                }
            }

            // 7. Update LocalForge with local_path
            if let Err(e) = local.update_record(&updated_record) {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to update LocalForge record: {e}"),
                };
            } else if let Err(e) = local.save_to_yaml().await {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to save repos.yaml: {e}"),
                };
            }

            yield HyperforgeEvent::Info {
                message: format!("Repository {name} cloned and configured"),
            };
        }
    }

    /// Sync a repo from `LocalForge` to its remote forges
    #[plexus_macros::method(
        description = "Sync a repo from LocalForge to its remote forges (create if missing, update if drifted)",
        params(
            org = "Organization name",
            name = "Repository name",
            dry_run = "Preview changes without applying (optional, default: false)"
        )
    )]
    pub async fn sync(
        &self,
        org: String,
        name: String,
        dry_run: Option<bool>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();
        let is_dry_run = dry_run.unwrap_or(false);

        stream! {
            let dry_prefix = if is_dry_run { "[DRY RUN] " } else { "" };
            let local = state.get_local_forge(&org).await;

            // Get repo record from LocalForge
            let record = match local.get_record(&name) {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repo '{name}' not found in LocalForge: {e}"),
                    };
                    return;
                }
            };

            let repo = record.to_repo();

            if record.forges.is_empty() {
                yield HyperforgeEvent::Error {
                    message: format!("Repo '{name}' has no target forges configured"),
                };
                return;
            }

            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            yield HyperforgeEvent::Info {
                message: format!(
                    "{}Syncing repo '{}' to forges: [{}]",
                    dry_prefix, name, record.forges.join(", "),
                ),
            };

            let mut created = 0usize;
            let mut updated = 0usize;
            let mut in_sync = 0usize;
            let mut errors = 0usize;
            let mut record = record;

            for forge_name in &record.forges.clone() {
                let forge = if let Some(f) = HyperforgeConfig::parse_forge(forge_name) { f } else {
                    yield HyperforgeEvent::Error {
                        message: format!("Invalid forge: {forge_name}"),
                    };
                    errors += 1;
                    continue;
                };

                let adapter = match make_repo_adapter(&forge, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("{forge_name}: {e}"),
                        };
                        errors += 1;
                        continue;
                    }
                };

                // Check if repo exists on this forge
                let exists = match adapter.repo_exists(&org, &name).await {
                    Ok(v) => v,
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("{forge_name}: failed to check existence: {e}"),
                        };
                        errors += 1;
                        continue;
                    }
                };

                if exists {
                    // Check for drift
                    let remote = match adapter.get_repo(&org, &name).await {
                        Ok(r) => r,
                        Err(e) => {
                            yield HyperforgeEvent::Error {
                                message: format!("{forge_name}: failed to fetch remote: {e}"),
                            };
                            errors += 1;
                            continue;
                        }
                    };

                    let norm_desc = |d: &Option<String>| -> Option<String> {
                        match d.as_deref() {
                            None | Some("") => None,
                            Some(s) => Some(s.to_string()),
                        }
                    };
                    let desc_drifted = norm_desc(&repo.description) != norm_desc(&remote.description);
                    let vis_drifted = repo.visibility != remote.visibility;

                    if desc_drifted || vis_drifted {
                        let mut diffs = Vec::new();
                        if desc_drifted { diffs.push("description"); }
                        if vis_drifted { diffs.push("visibility"); }

                        yield HyperforgeEvent::Info {
                            message: format!(
                                "  {}Updating {} on {} (drifted: {})",
                                dry_prefix, name, forge_name, diffs.join(", "),
                            ),
                        };

                        if is_dry_run {
                            updated += 1;
                        } else {
                            match adapter.update_repo(&org, &repo).await {
                                Ok(()) => {
                                    updated += 1;
                                    record.present_on.insert(forge.clone());
                                }
                                Err(e) => {
                                    yield HyperforgeEvent::Error {
                                        message: format!("{forge_name}: update failed: {e}"),
                                    };
                                    errors += 1;
                                }
                            }
                        }
                    } else {
                        record.present_on.insert(forge.clone());
                        in_sync += 1;
                    }
                } else {
                    // Create
                    yield HyperforgeEvent::Info {
                        message: format!("  {dry_prefix}Creating {name} on {forge_name}"),
                    };
                    if is_dry_run {
                        created += 1;
                    } else {
                        match adapter.create_repo(&org, &repo).await {
                            Ok(()) => {
                                created += 1;
                                record.present_on.insert(forge.clone());
                            }
                            Err(e) => {
                                yield HyperforgeEvent::Error {
                                    message: format!("{forge_name}: create failed: {e}"),
                                };
                                errors += 1;
                            }
                        }
                    }
                }
            }

            // Persist present_on updates to LocalForge
            if !is_dry_run && (created > 0 || updated > 0 || in_sync > 0) {
                if let Err(e) = local.update_record(&record) {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to update LocalForge record: {e}"),
                    };
                } else if let Err(e) = local.save_to_yaml().await {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to save repos.yaml: {e}"),
                    };
                }
            }

            yield HyperforgeEvent::Info {
                message: format!(
                    "{dry_prefix}Sync complete: {created} created, {updated} updated, {in_sync} in sync, {errors} errors",
                ),
            };
        }
    }

    /// Find large tracked files in a repository
    #[plexus_macros::method(
        description = "Find large tracked files in a repository",
        params(
            org = "Organization name",
            name = "Repository name",
            threshold_kb = "Size threshold in KB (optional, default: 100)"
        )
    )]
    pub async fn large_files(
        &self,
        org: String,
        name: String,
        threshold_kb: Option<u64>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();
        let threshold = threshold_kb.unwrap_or(100) * 1024;

        stream! {
            let local = state.get_local_forge(&org).await;

            let record = match local.get_record(&name) {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repo '{name}' not found in LocalForge: {e}"),
                    };
                    return;
                }
            };

            let repo_path = if let Some(p) = &record.local_path { PathBuf::from(p) } else {
                yield HyperforgeEvent::Error {
                    message: format!("Repo '{name}' has no local_path set in LocalForge"),
                };
                return;
            };

            if !repo_path.exists() {
                yield HyperforgeEvent::Error {
                    message: format!("Repo path does not exist: {}", repo_path.display()),
                };
                return;
            }

            // Scan tracked files + git history
            let scan = crate::services::build::large_files::scan_repo(&repo_path, threshold);

            match scan {
                Ok(results) if results.is_empty() => {
                    yield HyperforgeEvent::Info {
                        message: format!(
                            "No files over {}KB in '{}' (tracked or history)",
                            threshold / 1024,
                            name,
                        ),
                    };
                }
                Ok(results) => {
                    for entry in &results {
                        yield HyperforgeEvent::LargeFile {
                            repo_name: name.clone(),
                            file_path: entry.path.clone(),
                            size_bytes: entry.size,
                            history_only: entry.history_only,
                        };
                    }
                    let history_count = results.iter().filter(|e| e.history_only).count();
                    let tracked_count = results.len() - history_count;
                    yield HyperforgeEvent::Info {
                        message: format!(
                            "Found {} large file(s) over {}KB in '{}' ({} tracked, {} history-only)",
                            results.len(),
                            threshold / 1024,
                            name,
                            tracked_count,
                            history_count,
                        ),
                    };
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Scan failed for '{name}': {e}"),
                    };
                }
            }
        }
    }

    /// Show total size of tracked files in a repository
    #[plexus_macros::method(
        description = "Show total size of git-tracked files in a repository",
        params(
            org = "Organization name",
            name = "Repository name"
        )
    )]
    pub async fn size(
        &self,
        org: String,
        name: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let local = state.get_local_forge(&org).await;

            let record = match local.get_record(&name) {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repo '{name}' not found in LocalForge: {e}"),
                    };
                    return;
                }
            };

            let repo_path = if let Some(p) = &record.local_path { PathBuf::from(p) } else {
                yield HyperforgeEvent::Error {
                    message: format!("Repo '{name}' has no local_path set in LocalForge"),
                };
                return;
            };

            if !repo_path.exists() {
                yield HyperforgeEvent::Error {
                    message: format!("Repo path does not exist: {}", repo_path.display()),
                };
                return;
            }

            let output = match std::process::Command::new("git")
                .args(["ls-files"])
                .current_dir(&repo_path)
                .output()
            {
                Ok(o) => o,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to run git ls-files: {e}"),
                    };
                    return;
                }
            };

            if !output.status.success() {
                yield HyperforgeEvent::Error {
                    message: format!(
                        "git ls-files failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ),
                };
                return;
            }

            let files_str = String::from_utf8_lossy(&output.stdout);
            let mut total_bytes: u64 = 0;
            let mut tracked_files: usize = 0;

            for line in files_str.lines() {
                if line.is_empty() {
                    continue;
                }
                let full_path = repo_path.join(line);
                if let Ok(meta) = std::fs::metadata(&full_path) {
                    total_bytes += meta.len();
                    tracked_files += 1;
                }
            }

            yield HyperforgeEvent::RepoSize {
                repo_name: name,
                tracked_files,
                total_bytes,
            };
        }
    }

    /// Count lines of code in a repository
    #[plexus_macros::method(
        description = "Count lines of code in a repository, broken down by file extension",
        params(
            org = "Organization name",
            name = "Repository name"
        )
    )]
    pub async fn loc(
        &self,
        org: String,
        name: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let local = state.get_local_forge(&org).await;

            let record = match local.get_record(&name) {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repo '{name}' not found in LocalForge: {e}"),
                    };
                    return;
                }
            };

            let repo_path = if let Some(p) = &record.local_path { PathBuf::from(p) } else {
                yield HyperforgeEvent::Error {
                    message: format!("Repo '{name}' has no local_path set in LocalForge"),
                };
                return;
            };

            if !repo_path.exists() {
                yield HyperforgeEvent::Error {
                    message: format!("Repo path does not exist: {}", repo_path.display()),
                };
                return;
            }

            let output = match std::process::Command::new("git")
                .args(["ls-files"])
                .current_dir(&repo_path)
                .output()
            {
                Ok(o) => o,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to run git ls-files: {e}"),
                    };
                    return;
                }
            };

            if !output.status.success() {
                yield HyperforgeEvent::Error {
                    message: format!(
                        "git ls-files failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ),
                };
                return;
            }

            let files_str = String::from_utf8_lossy(&output.stdout);
            let mut total_lines: usize = 0;
            let mut total_files: usize = 0;
            let mut by_extension: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

            for line in files_str.lines() {
                if line.is_empty() {
                    continue;
                }
                let full_path = repo_path.join(line);
                let ext = std::path::Path::new(line)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("(none)")
                    .to_string();

                if let Ok(file) = std::fs::File::open(&full_path) {
                    let reader = std::io::BufReader::new(file);
                    let count = std::io::BufRead::lines(reader).count();
                    total_lines += count;
                    total_files += 1;
                    *by_extension.entry(ext).or_insert(0) += count;
                }
            }

            yield HyperforgeEvent::RepoLoc {
                repo_name: name,
                total_lines,
                total_files,
                by_extension,
            };
        }
    }

    /// Check if a repository has uncommitted changes
    #[plexus_macros::method(
        description = "Check if a repository has staged, unstaged, or untracked changes",
        params(
            org = "Organization name",
            name = "Repository name"
        )
    )]
    pub async fn dirty(
        &self,
        org: String,
        name: String,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let local = state.get_local_forge(&org).await;

            let record = match local.get_record(&name) {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Repo '{name}' not found in LocalForge: {e}"),
                    };
                    return;
                }
            };

            let repo_path = if let Some(p) = &record.local_path { PathBuf::from(p) } else {
                yield HyperforgeEvent::Error {
                    message: format!("Repo '{name}' has no local_path set in LocalForge"),
                };
                return;
            };

            if !repo_path.exists() {
                yield HyperforgeEvent::Error {
                    message: format!("Repo path does not exist: {}", repo_path.display()),
                };
                return;
            }

            match crate::git::Git::repo_status(&repo_path) {
                Ok(s) => {
                    yield HyperforgeEvent::RepoDirty {
                        repo_name: name,
                        has_staged: s.has_staged,
                        has_changes: s.has_changes,
                        has_untracked: s.has_untracked,
                        branch: s.branch,
                    };
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to get status for '{name}': {e}"),
                    };
                }
            }
        }
    }
}

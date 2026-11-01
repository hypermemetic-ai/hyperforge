//! `ReleasesService` — Release management subactivation
//!
//! A child plugin under `RepoService`, accessible as:
//!   synapse lforge hyperforge repo releases list --org foo --name bar
//!   synapse lforge hyperforge repo releases create --org foo --name bar --tag v1.0.0
//!   synapse lforge hyperforge repo releases upload --org foo --name bar --tag v1.0.0 --file ./dist/app.tar.gz

use async_stream::stream;
use futures::Stream;
use std::sync::Arc;

use crate::adapters::forge_port::ForgePort;
use crate::adapters::releases::codeberg::CodebergReleaseAdapter;
use crate::adapters::releases::github::GitHubReleaseAdapter;
use crate::adapters::releases::ReleasePort;
use crate::auth::YamlAuthProvider;
use crate::service::HyperforgeEvent;
use crate::services::HyperforgeState;
use crate::types::Forge;

/// Sub-service for release operations
#[derive(Clone)]
pub struct ReleasesService {
    state: HyperforgeState,
}

impl ReleasesService {
    pub const fn new(state: HyperforgeState) -> Self {
        Self { state }
    }
}

fn make_release_adapter(
    forge: &str,
    auth: Arc<YamlAuthProvider>,
    org: &str,
) -> Result<Box<dyn ReleasePort>, String> {
    match forge {
        "github" => GitHubReleaseAdapter::new(auth, org)
            .map(|a| Box::new(a) as Box<dyn ReleasePort>)
            .map_err(|e| format!("github: {e}")),
        "codeberg" => CodebergReleaseAdapter::new(auth, org)
            .map(|a| Box::new(a) as Box<dyn ReleasePort>)
            .map_err(|e| format!("codeberg: {e}")),
        other => Err(format!("Releases not supported for forge: {other}")),
    }
}

fn make_auth() -> Result<Arc<YamlAuthProvider>, String> {
    YamlAuthProvider::new()
        .map(Arc::new)
        .map_err(|e| format!("Failed to create auth provider: {e}"))
}

/// Guess content type from filename extension
fn guess_content_type(filename: &str) -> &'static str {
    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        "application/gzip"
    } else if filename.ends_with(".zip") {
        "application/zip"
    } else if filename.ends_with(".tar.xz") || filename.ends_with(".txz") {
        "application/x-xz"
    } else if filename.ends_with(".tar.bz2") {
        "application/x-bzip2"
    } else if filename.ends_with(".deb") {
        "application/vnd.debian.binary-package"
    } else if filename.ends_with(".rpm") {
        "application/x-rpm"
    } else if filename.ends_with(".dmg") {
        "application/x-apple-diskimage"
    } else if filename.ends_with(".exe") || filename.ends_with(".msi") {
        "application/octet-stream"
    } else if filename.ends_with(".txt") || filename.ends_with(".md") {
        "text/plain"
    } else if filename.ends_with(".json") {
        "application/json"
    } else if filename.ends_with(".sha256") || filename.ends_with(".sha512") {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

#[plexus_macros::activation(
    trait = "ReleasesApi",
    methods = marked,
    namespace = "releases",
    description = "Release management: list, create, upload, delete, assets",
    crate_path = "plexus_core"
)]
impl ReleasesService {
    /// List releases for a repo across configured forges
    #[plexus_macros::method(
        description = "List releases for a repository across its configured forges",
        params(
            org = "Organization name",
            name = "Repository name",
            forge = "Forge to query: github, codeberg, or gitlab (optional, defaults to all configured forges)"
        )
    )]
    pub async fn list(
        &self,
        org: String,
        name: String,
        forge: Option<Forge>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            let target_forges = resolve_target_forges(&state, &org, &name, forge).await;

            for forge_name in &target_forges {
                let adapter = match make_release_adapter(forge_name, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        yield HyperforgeEvent::Info {
                            message: format!("Skipping {forge_name} ({e})"),
                        };
                        continue;
                    }
                };

                match adapter.list_releases(&org, &name).await {
                    Ok(releases) => {
                        if releases.is_empty() {
                            yield HyperforgeEvent::Info {
                                message: format!("No releases found for {org}/{name} on {forge_name}"),
                            };
                            continue;
                        }

                        for release in &releases {
                            yield HyperforgeEvent::ReleaseInfo {
                                repo_name: name.clone(),
                                forge: forge_name.clone(),
                                tag: release.tag_name.clone(),
                                title: release.name.clone(),
                                asset_count: release.assets.len(),
                                draft: release.draft,
                                prerelease: release.prerelease,
                                created_at: release.created_at.to_rfc3339(),
                            };
                        }
                    }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to list releases on {forge_name}: {e}"),
                        };
                    }
                }
            }
        }
    }

    /// Create a tagged release on forge(s)
    #[plexus_macros::method(
        description = "Create a tagged release on one or more forges",
        params(
            org = "Organization name",
            name = "Repository name",
            tag = "Git tag for the release (e.g. v1.0.0)",
            title = "Release title (optional, defaults to tag name)",
            body = "Release description/notes (optional)",
            draft = "Create as draft release (optional, default: false)",
            prerelease = "Mark as pre-release (optional, default: false)",
            forge = "Target forge: github, codeberg, or gitlab (optional, defaults to all configured forges)"
        )
    )]
    pub async fn create(
        &self,
        org: String,
        name: String,
        tag: String,
        title: Option<String>,
        body: Option<String>,
        draft: Option<bool>,
        prerelease: Option<bool>,
        forge: Option<Forge>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();
        let release_title = title.unwrap_or_else(|| tag.clone());
        let release_body = body.unwrap_or_default();
        let is_draft = draft.unwrap_or(false);
        let is_prerelease = prerelease.unwrap_or(false);

        stream! {
            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            let target_forges = resolve_target_forges(&state, &org, &name, forge).await;

            for forge_name in &target_forges {
                let adapter = match make_release_adapter(forge_name, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        yield HyperforgeEvent::ReleaseCreate {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            success: false,
                            error: Some(e),
                        };
                        continue;
                    }
                };

                match adapter
                    .create_release(&org, &name, &tag, &release_title, &release_body, is_draft, is_prerelease)
                    .await
                {
                    Ok(_release) => {
                        yield HyperforgeEvent::ReleaseCreate {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            success: true,
                            error: None,
                        };
                    }
                    Err(e) => {
                        yield HyperforgeEvent::ReleaseCreate {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            success: false,
                            error: Some(e.to_string()),
                        };
                    }
                }
            }
        }
    }

    /// Upload a file as a release asset
    #[plexus_macros::method(
        description = "Upload a file as an asset to an existing release. Reads file from disk and uploads to the forge.",
        params(
            org = "Organization name",
            name = "Repository name",
            tag = "Release tag (e.g. v1.0.0)",
            file = "Path to file to upload",
            forge = "Target forge: github, codeberg, or gitlab (optional, defaults to all configured forges)"
        )
    )]
    pub async fn upload(
        &self,
        org: String,
        name: String,
        tag: String,
        file: String,
        forge: Option<Forge>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            // Read the file from disk
            let file_path = std::path::Path::new(&file);
            if !file_path.exists() {
                yield HyperforgeEvent::Error {
                    message: format!("File not found: {file}"),
                };
                return;
            }

            let filename = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("upload")
                .to_string();

            let data = match tokio::fs::read(&file).await {
                Ok(d) => d,
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to read file {file}: {e}"),
                    };
                    return;
                }
            };

            let size_bytes = data.len() as u64;
            let content_type = guess_content_type(&filename);

            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            let target_forges = resolve_target_forges(&state, &org, &name, forge).await;

            for forge_name in &target_forges {
                let adapter = match make_release_adapter(forge_name, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        yield HyperforgeEvent::ReleaseUpload {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            asset_name: filename.clone(),
                            size_bytes,
                            success: false,
                            error: Some(e),
                        };
                        continue;
                    }
                };

                // Find the release by tag to get its ID
                let release = match adapter.get_release_by_tag(&org, &name, &tag).await {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        yield HyperforgeEvent::ReleaseUpload {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            asset_name: filename.clone(),
                            size_bytes,
                            success: false,
                            error: Some(format!("Release with tag '{tag}' not found on {forge_name}")),
                        };
                        continue;
                    }
                    Err(e) => {
                        yield HyperforgeEvent::ReleaseUpload {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            asset_name: filename.clone(),
                            size_bytes,
                            success: false,
                            error: Some(format!("Failed to find release: {e}")),
                        };
                        continue;
                    }
                };

                match adapter
                    .upload_asset(&org, &name, release.id, &filename, content_type, data.clone())
                    .await
                {
                    Ok(_asset) => {
                        yield HyperforgeEvent::ReleaseUpload {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            asset_name: filename.clone(),
                            size_bytes,
                            success: true,
                            error: None,
                        };
                    }
                    Err(e) => {
                        yield HyperforgeEvent::ReleaseUpload {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            asset_name: filename.clone(),
                            size_bytes,
                            success: false,
                            error: Some(e.to_string()),
                        };
                    }
                }
            }
        }
    }

    /// Delete a release by tag
    #[plexus_macros::method(
        description = "Delete a release by tag. Dry-run by default; pass --confirm true to actually delete.",
        params(
            org = "Organization name",
            name = "Repository name",
            tag = "Release tag to delete (e.g. v1.0.0)",
            forge = "Target forge: github, codeberg, or gitlab (optional, defaults to all configured forges)",
            confirm = "Actually delete (default: false — dry-run unless confirmed)"
        )
    )]
    pub async fn delete(
        &self,
        org: String,
        name: String,
        tag: String,
        forge: Option<Forge>,
        confirm: Option<bool>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();
        let is_dry_run = !confirm.unwrap_or(false);

        stream! {
            let dry_prefix = if is_dry_run { "[dry-run] " } else { "" };

            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            let target_forges = resolve_target_forges(&state, &org, &name, forge).await;

            for forge_name in &target_forges {
                if is_dry_run {
                    yield HyperforgeEvent::Info {
                        message: format!("{dry_prefix}Would delete release {tag} for {org}/{name} on {forge_name}"),
                    };
                    continue;
                }

                let adapter = match make_release_adapter(forge_name, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        yield HyperforgeEvent::ReleaseDelete {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            success: false,
                            error: Some(e),
                        };
                        continue;
                    }
                };

                // Find the release by tag to get its ID
                let release = match adapter.get_release_by_tag(&org, &name, &tag).await {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        yield HyperforgeEvent::ReleaseDelete {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            success: false,
                            error: Some(format!("Release with tag '{tag}' not found on {forge_name}")),
                        };
                        continue;
                    }
                    Err(e) => {
                        yield HyperforgeEvent::ReleaseDelete {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            success: false,
                            error: Some(format!("Failed to find release: {e}")),
                        };
                        continue;
                    }
                };

                match adapter.delete_release(&org, &name, release.id).await {
                    Ok(()) => {
                        yield HyperforgeEvent::ReleaseDelete {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            success: true,
                            error: None,
                        };
                    }
                    Err(e) => {
                        yield HyperforgeEvent::ReleaseDelete {
                            repo_name: name.clone(),
                            forge: forge_name.clone(),
                            tag: tag.clone(),
                            success: false,
                            error: Some(e.to_string()),
                        };
                    }
                }
            }
        }
    }

    /// List assets attached to a specific release
    #[plexus_macros::method(
        description = "List assets attached to a specific release by tag",
        params(
            org = "Organization name",
            name = "Repository name",
            tag = "Release tag (e.g. v1.0.0)",
            forge = "Forge to query: github, codeberg, or gitlab (optional, defaults to all configured forges)"
        )
    )]
    pub async fn assets(
        &self,
        org: String,
        name: String,
        tag: String,
        forge: Option<Forge>,
    ) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
        let state = self.state.clone();

        stream! {
            let auth = match make_auth() {
                Ok(a) => a,
                Err(e) => {
                    yield HyperforgeEvent::Error { message: e };
                    return;
                }
            };

            let target_forges = resolve_target_forges(&state, &org, &name, forge).await;

            for forge_name in &target_forges {
                let adapter = match make_release_adapter(forge_name, auth.clone(), &org) {
                    Ok(a) => a,
                    Err(e) => {
                        yield HyperforgeEvent::Info {
                            message: format!("Skipping {forge_name} ({e})"),
                        };
                        continue;
                    }
                };

                // Find the release by tag to get its ID
                let release = match adapter.get_release_by_tag(&org, &name, &tag).await {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        yield HyperforgeEvent::Info {
                            message: format!("Release '{tag}' not found for {org}/{name} on {forge_name}"),
                        };
                        continue;
                    }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to find release on {forge_name}: {e}"),
                        };
                        continue;
                    }
                };

                // The release already has assets embedded from get_release_by_tag
                if release.assets.is_empty() {
                    yield HyperforgeEvent::Info {
                        message: format!("No assets for release {tag} on {forge_name}"),
                    };
                    continue;
                }

                for asset in &release.assets {
                    yield HyperforgeEvent::AssetInfo {
                        repo_name: name.clone(),
                        forge: forge_name.clone(),
                        tag: tag.clone(),
                        asset_name: asset.name.clone(),
                        size_bytes: asset.size_bytes,
                        content_type: asset.content_type.clone(),
                        download_url: asset.download_url.clone(),
                        created_at: asset.created_at.to_rfc3339(),
                    };
                }
            }
        }
    }
}

/// Resolve which forges to target, same pattern as `ImagesService`
async fn resolve_target_forges(
    state: &HyperforgeState,
    org: &str,
    name: &str,
    forge: Option<Forge>,
) -> Vec<String> {
    if let Some(f) = forge {
        vec![f.as_str().to_string()]
    } else {
        let local = state.get_local_forge(org).await;
        match local.get_repo(org, name).await {
            Ok(repo) => {
                let origin_str = serde_json::to_string(&repo.origin)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string();
                let mut forges = vec![origin_str];
                for m in &repo.mirrors {
                    let s = serde_json::to_string(m)
                        .unwrap_or_default()
                        .trim_matches('"')
                        .to_string();
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
    }
}

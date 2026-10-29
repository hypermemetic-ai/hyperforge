//! Build release orchestrator: cross-compile, package, create forge releases, upload assets.

use async_stream::stream;
use futures::Stream;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::adapters::releases::codeberg::CodebergReleaseAdapter;
use crate::adapters::releases::github::GitHubReleaseAdapter;
use crate::adapters::releases::ReleasePort;
use crate::auth::credentials::preflight_check;
use crate::auth::YamlAuthProvider;
use crate::build_system::cross_compile::{compile_and_package, host_triple, TargetTriple};
use crate::build_system::{self, BinaryTarget};
use crate::commands::runner::discover_or_bail;
use crate::commands::workspace::{build_publish_dep_graph, DiscoveredRepo};
use crate::config::HyperforgeConfig;
use crate::git::Git;
use crate::service::HyperforgeEvent;
use crate::services::utils::RepoFilter;
use crate::types::config::DistChannel;
use crate::types::Forge;

pub(crate) fn make_auth() -> Result<Arc<YamlAuthProvider>, String> {
    YamlAuthProvider::new()
        .map(Arc::new)
        .map_err(|e| format!("Failed to create auth provider: {e}"))
}

pub(crate) fn make_release_adapter(
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

/// Guess content type from filename extension
fn guess_content_type(filename: &str) -> &'static str {
    if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
        "application/gzip"
    } else if filename.ends_with(".zip") {
        "application/zip"
    } else {
        "application/octet-stream"
    }
}

/// Collect binary targets and version for a discovered repo.
/// Returns None if the repo has no binary targets.
fn repo_binary_info(repo: &DiscoveredRepo) -> Option<(Vec<BinaryTarget>, String)> {
    let targets = build_system::binary_targets(&repo.path);
    if targets.is_empty() {
        return None;
    }
    let version = repo.package_version.clone()?;
    Some((targets, version))
}

/// Resolve which forges to target from a repo's config.
fn resolve_forges(repo: &DiscoveredRepo, forge_override: &Option<String>) -> Vec<String> {
    if let Some(ref f) = forge_override {
        f.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    } else {
        repo.forges().iter().map(std::string::ToString::to_string).collect()
    }
}

/// Resolve the org from a discovered repo's config
fn resolve_org(repo: &DiscoveredRepo) -> Option<String> {
    repo.org().map(std::string::ToString::to_string)
}

/// Parse comma-separated target triples, defaulting to native
#[allow(dead_code)]
fn parse_targets(targets: &Option<String>) -> Vec<TargetTriple> {
    match targets {
        Some(s) if !s.is_empty() => s
            .split(',')
            .map(|t| TargetTriple::new(t.trim()))
            .collect(),
        _ => vec![TargetTriple::new(host_triple())],
    }
}

/// Determine if a path is itself a single repo (has Cargo.toml or *.cabal)
fn is_single_repo(path: &std::path::Path) -> bool {
    path.join("Cargo.toml").exists()
        || std::fs::read_dir(path)
            .ok()
            .is_some_and(|entries| {
                entries
                    .filter_map(std::result::Result::ok)
                    .any(|e| {
                        e.path()
                            .extension()
                            .is_some_and(|ext| ext == "cabal")
                    })
            })
}

/// Build a `DiscoveredRepo` from a single repo path (not a workspace)
fn discover_single_repo(path: &std::path::Path) -> Result<DiscoveredRepo, String> {
    let path = path
        .canonicalize()
        .map_err(|e| format!("Cannot resolve path: {e}"))?;

    let dir_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let primary_bs = build_system::detect_build_system(&path);
    let all_bs = build_system::detect_all_build_systems(&path);
    let deps = build_system::parse_dependencies(&path, &primary_bs);
    let pkg_name = build_system::package_name(&path, &primary_bs);
    let pkg_version = build_system::package_version(&path, &primary_bs);
    let config = crate::config::HyperforgeConfig::load(&path).ok();
    let is_git = Git::is_repo(&path);

    Ok(DiscoveredRepo {
        path,
        dir_name,
        config,
        is_git_repo: is_git,
        is_hyperforge_repo: crate::config::HyperforgeConfig::exists(&std::path::PathBuf::from(".")),
        build_system: primary_bs,
        build_systems: all_bs,
        dependencies: deps,
        package_name: pkg_name,
        package_version: pkg_version,
    })
}

/// Accumulated counters from releasing a single repo.
struct SingleRepoReleaseCounts {
    /// Whether this repo had binary targets (counted as a release repo)
    has_binaries: bool,
    targets: usize,
    forges: HashSet<String>,
    assets_uploaded: usize,
    failed: usize,
}

/// Load the dist config for a discovered repo from its .hyperforge/config.toml.
fn load_dist_config(repo: &DiscoveredRepo) -> Option<crate::types::config::DistConfig> {
    HyperforgeConfig::load(&repo.path)
        .ok()
        .and_then(|c| c.dist)
}

/// Resolve target triples for a repo, consulting dist config when no CLI override is provided.
fn resolve_targets_with_dist(
    cli_targets: &Option<String>,
    repo: &DiscoveredRepo,
) -> Vec<TargetTriple> {
    // CLI override takes priority
    if let Some(ref s) = cli_targets {
        if !s.is_empty() {
            return s.split(',').map(|t| TargetTriple::new(t.trim())).collect();
        }
    }
    // Check dist config
    if let Some(dist) = load_dist_config(repo) {
        if !dist.targets.is_empty() {
            return dist.targets.iter().map(TargetTriple::new).collect();
        }
    }
    // Fallback to native host
    vec![TargetTriple::new(host_triple())]
}

/// Resolve forges for a repo, consulting dist config channels when no CLI override is provided.
fn resolve_forges_with_dist(
    cli_forge: &Option<String>,
    repo: &DiscoveredRepo,
) -> Vec<String> {
    // CLI override takes priority
    if let Some(ref f) = cli_forge {
        let parsed: Vec<String> = f.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    // Check dist config: if forge-release is not in channels, return empty (skip)
    if let Some(dist) = load_dist_config(repo) {
        if !dist.channels.is_empty() {
            if !dist.channels.contains(&DistChannel::ForgeRelease) {
                // Dist config exists but doesn't include forge-release — skip
                return Vec::new();
            }
            // forge-release is in channels — use repo's configured forges
            return repo.forges().iter().map(std::string::ToString::to_string).collect();
        }
    }
    // No dist config — fall back to repo's configured forges
    repo.forges().iter().map(std::string::ToString::to_string).collect()
}

/// Collect org/forge/channel data from repos and run pre-flight auth.
/// Accepts an iterator of repo references to work with both owned and borrowed slices.
async fn run_release_preflight(
    repos: &[&DiscoveredRepo],
    forge_override: &Option<Forge>,
) -> Vec<HyperforgeEvent> {
    use std::collections::HashSet;

    let forge_override_str: Option<String> = forge_override.as_ref().map(|f| f.as_str().to_string());

    let mut org_forges: std::collections::HashMap<String, HashSet<String>> =
        std::collections::HashMap::new();
    let mut org_channels: std::collections::HashMap<String, Vec<DistChannel>> =
        std::collections::HashMap::new();

    for repo in repos {
        let org = match repo.org() {
            Some(o) => o.to_string(),
            None => continue,
        };

        // Collect forges
        let forges: Vec<String> = if let Some(ref f) = forge_override_str {
            vec![f.clone()]
        } else {
            repo.forges().iter().map(std::string::ToString::to_string).collect()
        };

        let forge_set = org_forges.entry(org.clone()).or_default();
        for f in forges {
            forge_set.insert(f);
        }

        // Collect dist channels
        if let Some(dist) = load_dist_config(repo) {
            let ch = org_channels.entry(org.clone()).or_default();
            for c in dist.channels {
                if !ch.contains(&c) {
                    ch.push(c);
                }
            }
        }
    }

    // Run preflight for each org
    let auth = match make_auth() {
        Ok(a) => a,
        Err(e) => {
            return vec![HyperforgeEvent::Error {
                message: format!("Pre-flight: {e}"),
            }];
        }
    };

    let mut all_errors = Vec::new();
    for (org, forge_set) in &org_forges {
        let forges: Vec<String> = forge_set.iter().cloned().collect();
        let channels = org_channels.get(org).cloned().unwrap_or_default();
        let errors = preflight_check(&forges, &channels, org, auth.as_ref()).await;
        all_errors.extend(errors);
    }

    all_errors
}

/// Release a single repo: detect binaries, compile for all targets, create git tag,
/// create forge releases, upload assets. Returns events and summary counters.
///
/// Shared by both `release` (single/workspace) and `release_all` (workspace-only).
async fn release_single_repo(
    repo: &DiscoveredRepo,
    tag: &str,
    target_triples: &[TargetTriple],
    forge_override: &Option<String>,
    release_title: &str,
    release_body: &str,
    is_draft: bool,
    is_dry_run: bool,
) -> (Vec<HyperforgeEvent>, SingleRepoReleaseCounts) {
    let mut events = Vec::new();
    let mut counts = SingleRepoReleaseCounts {
        has_binaries: false,
        targets: 0,
        forges: HashSet::new(),
        assets_uploaded: 0,
        failed: 0,
    };

    let dry_prefix = if is_dry_run { "[dry-run] " } else { "" };

    let (bin_targets, version) = if let Some(info) = repo_binary_info(repo) { info } else {
        events.push(HyperforgeEvent::Info {
            message: format!("Skipping {} (no binary targets or no version)", repo.dir_name),
        });
        return (events, counts);
    };

    counts.has_binaries = true;
    let binary_names: Vec<String> = bin_targets.iter().map(|b| b.name.clone()).collect();
    let bs_kind = &bin_targets[0].build_system;
    let repo_name = repo.dir_name.clone();

    events.push(HyperforgeEvent::Info {
        message: format!(
            "{}  {} v{} -- {} binary target(s): {}",
            dry_prefix,
            repo_name,
            version,
            binary_names.len(),
            binary_names.join(", "),
        ),
    });

    // Output dir for archives
    let output_dir = repo.path.join("target").join("dist");

    // Compile and package for each target triple
    let mut archives: Vec<PathBuf> = Vec::new();

    for triple in target_triples {
        events.push(HyperforgeEvent::ReleaseBuildStep {
            repo_name: repo_name.clone(),
            target: triple.triple.clone(),
            status: "compiling".to_string(),
            detail: Some(format!("{} binary(ies)", binary_names.len())),
        });

        if is_dry_run {
            let stem = format!(
                "{}-{}-v{}{}",
                repo_name,
                triple.triple,
                version,
                triple.archive_format().extension()
            );
            events.push(HyperforgeEvent::ReleaseBuildStep {
                repo_name: repo_name.clone(),
                target: triple.triple.clone(),
                status: "packaging".to_string(),
                detail: Some(format!("would create {stem}")),
            });
            counts.targets += 1;
            continue;
        }

        let results = compile_and_package(
            &repo.path,
            bs_kind,
            std::slice::from_ref(triple),
            &binary_names,
            &version,
            &output_dir,
        )
        .await;

        for result in results {
            if result.success {
                if let Some(ref archive_path) = result.archive_path {
                    events.push(HyperforgeEvent::ReleaseBuildStep {
                        repo_name: repo_name.clone(),
                        target: triple.triple.clone(),
                        status: "packaging".to_string(),
                        detail: Some(format!(
                            "created {}",
                            archive_path.file_name().unwrap_or_default().to_string_lossy()
                        )),
                    });
                    archives.push(archive_path.clone());
                    counts.targets += 1;
                }
            } else {
                let err = result.error.unwrap_or_else(|| "unknown error".to_string());
                events.push(HyperforgeEvent::Error {
                    message: format!(
                        "{}: compile/package failed for {}: {}",
                        repo_name, triple.triple, err
                    ),
                });
                counts.failed += 1;
            }
        }
    }

    // Create git tag if needed
    if !is_dry_run && !Git::tag_exists(&repo.path, tag) {
        events.push(HyperforgeEvent::Info {
            message: format!("  Creating git tag {tag} in {repo_name}"),
        });
        let tag_output = tokio::process::Command::new("git")
            .args(["tag", "-a", tag, "-m", &format!("Release {tag}")])
            .current_dir(&repo.path)
            .output()
            .await;
        match tag_output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                events.push(HyperforgeEvent::Error {
                    message: format!("  Failed to create tag {tag}: {stderr}"),
                });
            }
            Err(e) => {
                events.push(HyperforgeEvent::Error {
                    message: format!("  Failed to run git tag: {e}"),
                });
            }
        }
    } else if is_dry_run {
        if Git::tag_exists(&repo.path, tag) {
            events.push(HyperforgeEvent::Info {
                message: format!("{dry_prefix}  Tag {tag} already exists in {repo_name}"),
            });
        } else {
            events.push(HyperforgeEvent::Info {
                message: format!("{dry_prefix}  Would create git tag {tag} in {repo_name}"),
            });
        }
    }

    // Upload to forges
    let target_forges = resolve_forges(repo, forge_override);
    if target_forges.is_empty() {
        events.push(HyperforgeEvent::Info {
            message: format!("  {repo_name} has no configured forges -- skipping release upload"),
        });
        return (events, counts);
    }

    let org = if let Some(o) = resolve_org(repo) { o } else {
        events.push(HyperforgeEvent::Info {
            message: format!("  {repo_name} has no org configured -- skipping release upload"),
        });
        return (events, counts);
    };

    if is_dry_run {
        for forge_name in &target_forges {
            events.push(HyperforgeEvent::Info {
                message: format!(
                    "{}  Would create release {} on {}/{} ({}), upload {} asset(s)",
                    dry_prefix,
                    tag,
                    org,
                    repo_name,
                    forge_name,
                    target_triples.len(),
                ),
            });
            counts.forges.insert(forge_name.clone());
        }
        return (events, counts);
    }

    // Real upload path
    let auth = match make_auth() {
        Ok(a) => a,
        Err(e) => {
            events.push(HyperforgeEvent::Error { message: e });
            counts.failed += 1;
            return (events, counts);
        }
    };

    for forge_name in &target_forges {
        let adapter = match make_release_adapter(forge_name, auth.clone(), &org) {
            Ok(a) => a,
            Err(e) => {
                events.push(HyperforgeEvent::ReleaseCreate {
                    repo_name: repo_name.clone(),
                    forge: forge_name.clone(),
                    tag: tag.to_string(),
                    success: false,
                    error: Some(e),
                });
                counts.failed += 1;
                continue;
            }
        };

        // Create release
        match adapter
            .create_release(
                &org,
                &repo_name,
                tag,
                release_title,
                release_body,
                is_draft,
                false,
            )
            .await
        {
            Ok(release_info) => {
                events.push(HyperforgeEvent::ReleaseCreate {
                    repo_name: repo_name.clone(),
                    forge: forge_name.clone(),
                    tag: tag.to_string(),
                    success: true,
                    error: None,
                });
                counts.forges.insert(forge_name.clone());

                // Upload each archive
                for archive_path in &archives {
                    let filename = archive_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_string();

                    events.push(HyperforgeEvent::ReleaseBuildStep {
                        repo_name: repo_name.clone(),
                        target: "all".to_string(),
                        status: "uploading".to_string(),
                        detail: Some(format!("{filename} to {forge_name}")),
                    });

                    let data = match tokio::fs::read(archive_path).await {
                        Ok(d) => d,
                        Err(e) => {
                            events.push(HyperforgeEvent::ReleaseUpload {
                                repo_name: repo_name.clone(),
                                forge: forge_name.clone(),
                                tag: tag.to_string(),
                                asset_name: filename.clone(),
                                size_bytes: 0,
                                success: false,
                                error: Some(format!("Failed to read archive: {e}")),
                            });
                            counts.failed += 1;
                            continue;
                        }
                    };

                    let size_bytes = data.len() as u64;
                    let content_type = guess_content_type(&filename);

                    match adapter
                        .upload_asset(
                            &org,
                            &repo_name,
                            release_info.id,
                            &filename,
                            content_type,
                            data,
                        )
                        .await
                    {
                        Ok(_) => {
                            events.push(HyperforgeEvent::ReleaseUpload {
                                repo_name: repo_name.clone(),
                                forge: forge_name.clone(),
                                tag: tag.to_string(),
                                asset_name: filename,
                                size_bytes,
                                success: true,
                                error: None,
                            });
                            counts.assets_uploaded += 1;
                        }
                        Err(e) => {
                            events.push(HyperforgeEvent::ReleaseUpload {
                                repo_name: repo_name.clone(),
                                forge: forge_name.clone(),
                                tag: tag.to_string(),
                                asset_name: filename,
                                size_bytes,
                                success: false,
                                error: Some(e.to_string()),
                            });
                            counts.failed += 1;
                        }
                    }
                }
            }
            Err(e) => {
                events.push(HyperforgeEvent::ReleaseCreate {
                    repo_name: repo_name.clone(),
                    forge: forge_name.clone(),
                    tag: tag.to_string(),
                    success: false,
                    error: Some(e.to_string()),
                });
                counts.failed += 1;
            }
        }
    }

    (events, counts)
}

pub fn release(
    path: String,
    tag: String,
    targets: Option<String>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    forge: Option<Forge>,
    title: Option<String>,
    body: Option<String>,
    draft: Option<bool>,
    dry_run: Option<bool>,
    skip_auth_check: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let filter = RepoFilter::new(include, exclude);
    let is_dry_run = dry_run.unwrap_or(false);
    let is_draft = draft.unwrap_or(false);
    let is_skip_auth = skip_auth_check.unwrap_or(false);
    let release_title = title.unwrap_or_else(|| tag.clone());
    let release_body = body.unwrap_or_default();
    let forge_str: Option<String> = forge.as_ref().map(|f| f.as_str().to_string());

    stream! {
        let dry_prefix = if is_dry_run { "[dry-run] " } else { "" };
        let workspace_path = PathBuf::from(&path);

        // Determine repos: single repo or workspace
        let repos: Vec<DiscoveredRepo> = if is_single_repo(&workspace_path) {
            match discover_single_repo(&workspace_path) {
                Ok(r) => vec![r],
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to discover repo: {e}"),
                    };
                    return;
                }
            }
        } else {
            let ctx = match discover_or_bail(&workspace_path) {
                Ok(ctx) => ctx,
                Err(event) => {
                    yield event;
                    return;
                }
            };
            ctx.repos
                .into_iter()
                .filter(|r| filter.matches(&r.dir_name))
                .collect()
        };

        if repos.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No repos matched filter.".to_string(),
            };
            return;
        }

        // ── Pre-flight auth check ──
        if !is_skip_auth && !is_dry_run {
            let repo_refs: Vec<&DiscoveredRepo> = repos.iter().collect();
            let preflight_errors = run_release_preflight(&repo_refs, &forge).await;
            if !preflight_errors.is_empty() {
                for event in preflight_errors {
                    yield event;
                }
                return;
            }
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{}Release {} -- {} repo(s)",
                dry_prefix,
                tag,
                repos.len(),
            ),
        };

        let mut summary_repos = 0usize;
        let mut summary_targets = 0usize;
        let mut summary_forges_set = HashSet::new();
        let mut summary_assets = 0usize;
        let mut summary_failed = 0usize;

        for repo in &repos {
            // Resolve targets and forges per-repo, consulting dist config
            let repo_targets = resolve_targets_with_dist(&targets, repo);
            let repo_forges = resolve_forges_with_dist(&forge_str, repo);

            // If dist config says no forge-release, skip
            if repo_forges.is_empty() && forge_str.is_none() {
                yield HyperforgeEvent::Info {
                    message: format!(
                        "Skipping {} (no forge-release channel in dist config)",
                        repo.dir_name,
                    ),
                };
                continue;
            }

            let forge_override_for_repo: Option<String> = if forge_str.is_some() {
                forge_str.clone()
            } else if !repo_forges.is_empty() {
                Some(repo_forges.join(","))
            } else {
                None
            };

            let (repo_events, repo_counts) = release_single_repo(
                repo,
                &tag,
                &repo_targets,
                &forge_override_for_repo,
                &release_title,
                &release_body,
                is_draft,
                is_dry_run,
            ).await;

            for event in repo_events {
                yield event;
            }

            if repo_counts.has_binaries {
                summary_repos += 1;
            }
            summary_targets += repo_counts.targets;
            summary_forges_set.extend(repo_counts.forges);
            summary_assets += repo_counts.assets_uploaded;
            summary_failed += repo_counts.failed;
        }

        yield HyperforgeEvent::ReleaseSummary {
            repos: summary_repos,
            targets: summary_targets,
            forges: summary_forges_set.len(),
            assets_uploaded: summary_assets,
            failed: summary_failed,
        };
    }
}

/// Workspace-wide release: release all binary-producing packages in dependency order.
///
/// Unlike `release`, this always treats the path as a workspace (never single-repo mode).
/// Repos are processed sequentially in dependency order (from `topo_order`), with
/// cross-compilation across targets running in parallel within each repo.
pub fn release_all(
    path: String,
    tag: String,
    targets: Option<String>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    forge: Option<Forge>,
    title: Option<String>,
    body: Option<String>,
    draft: Option<bool>,
    dry_run: Option<bool>,
    skip_auth_check: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let filter = RepoFilter::new(include, exclude);
    let is_dry_run = dry_run.unwrap_or(false);
    let is_draft = draft.unwrap_or(false);
    let is_skip_auth = skip_auth_check.unwrap_or(false);
    let release_title = title.unwrap_or_else(|| tag.clone());
    let release_body = body.unwrap_or_default();
    let forge_str: Option<String> = forge.as_ref().map(|f| f.as_str().to_string());

    stream! {
        let dry_prefix = if is_dry_run { "[dry-run] " } else { "" };
        let workspace_path = PathBuf::from(&path);

        // Always workspace mode
        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => {
                yield event;
                return;
            }
        };

        let all_repos = ctx.repos;

        // ── Pre-flight auth check ──
        if !is_skip_auth && !is_dry_run {
            let filtered_for_preflight: Vec<_> = all_repos
                .iter()
                .filter(|r| filter.matches(&r.dir_name))
                .collect::<Vec<_>>();
            let preflight_errors = run_release_preflight(&filtered_for_preflight, &forge).await;
            if !preflight_errors.is_empty() {
                for event in preflight_errors {
                    yield event;
                }
                return;
            }
        }

        // Build dependency graph for ordering
        let dep_graph = build_publish_dep_graph(&all_repos);

        // Compute topological order (dependencies first)
        let ordered_indices = match dep_graph.topo_order() {
            Ok(order) => order,
            Err(e) => {
                yield HyperforgeEvent::Error {
                    message: format!("Dependency cycle detected: {e}"),
                };
                // Fall back to original order
                (0..all_repos.len()).collect()
            }
        };

        // Build ordered repo list, applying filter
        let repos: Vec<&DiscoveredRepo> = ordered_indices
            .iter()
            .filter_map(|&idx| {
                let repo = &all_repos[idx];
                if filter.matches(&repo.dir_name) {
                    Some(repo)
                } else {
                    None
                }
            })
            .collect();

        if repos.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No repos matched filter.".to_string(),
            };
            return;
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{}Release all {} -- {} repo(s) in dependency order",
                dry_prefix,
                tag,
                repos.len(),
            ),
        };

        let mut summary_repos = 0usize;
        let mut summary_targets = 0usize;
        let mut summary_forges_set = HashSet::new();
        let mut summary_assets = 0usize;
        let mut summary_failed = 0usize;

        for (i, repo) in repos.iter().enumerate() {
            // Resolve targets and forges per-repo, consulting dist config
            let repo_targets = resolve_targets_with_dist(&targets, repo);
            let repo_forges = resolve_forges_with_dist(&forge_str, repo);

            // If dist config says no forge-release, skip
            if repo_forges.is_empty() && forge_str.is_none() {
                yield HyperforgeEvent::Info {
                    message: format!(
                        "{}[{}/{}] Skipping {} (no forge-release channel in dist config)",
                        dry_prefix,
                        i + 1,
                        repos.len(),
                        repo.dir_name,
                    ),
                };
                continue;
            }

            yield HyperforgeEvent::Info {
                message: format!(
                    "{}[{}/{}] Processing {}...",
                    dry_prefix,
                    i + 1,
                    repos.len(),
                    repo.dir_name,
                ),
            };

            let forge_override_for_repo: Option<String> = if forge_str.is_some() {
                forge_str.clone()
            } else if !repo_forges.is_empty() {
                Some(repo_forges.join(","))
            } else {
                None
            };

            let (repo_events, repo_counts) = release_single_repo(
                repo,
                &tag,
                &repo_targets,
                &forge_override_for_repo,
                &release_title,
                &release_body,
                is_draft,
                is_dry_run,
            ).await;

            for event in repo_events {
                yield event;
            }

            if repo_counts.has_binaries {
                summary_repos += 1;
            }
            summary_targets += repo_counts.targets;
            summary_forges_set.extend(repo_counts.forges);
            summary_assets += repo_counts.assets_uploaded;
            summary_failed += repo_counts.failed;
        }

        yield HyperforgeEvent::ReleaseSummary {
            repos: summary_repos,
            targets: summary_targets,
            forges: summary_forges_set.len(),
            assets_uploaded: summary_assets,
            failed: summary_failed,
        };
    }
}

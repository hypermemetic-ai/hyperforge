//! Package registry operations: `package_diff`, publish, bump.

use async_stream::stream;
use futures::Stream;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::build_system::BuildSystemKind;
use crate::commands::runner::{discover_or_bail, run_batch};
use crate::commands::workspace::build_publish_dep_graph;
use crate::git::Git;
use crate::service::HyperforgeEvent;
use crate::services::utils::{dry_prefix, RepoFilter};
use crate::package::DriftResult;

/// Result of a version bump + commit + tag operation.
struct BumpResult {
    events: Vec<HyperforgeEvent>,
    success: bool,
}

/// Check that a repo is clean, bump the version, commit (manifest + lockfile), and create a git tag.
///
/// Returns events to yield and whether the operation succeeded.
fn bump_commit_tag(
    repo_path: &Path,
    package_name: &str,
    new_version: &str,
    build_system: &BuildSystemKind,
    skip_commit: bool,
    skip_tag: bool,
) -> BumpResult {
    let mut events = Vec::new();

    // Check repo is clean before bumping
    match Git::repo_status(repo_path) {
        Ok(status) => {
            if status.has_changes || status.has_staged {
                events.push(HyperforgeEvent::PublishStep {
                    package_name: package_name.to_string(),
                    version: new_version.to_string(),
                    registry: registry_kind_for(build_system),
                    action: crate::service::PublishActionKind::Failed,
                    success: false,
                    error: Some("repo has uncommitted changes — commit or stash first".to_string()),
                });
                return BumpResult { events, success: false };
            }
        }
        Err(e) => {
            events.push(HyperforgeEvent::PublishStep {
                package_name: package_name.to_string(),
                version: new_version.to_string(),
                registry: registry_kind_for(build_system),
                action: crate::service::PublishActionKind::Failed,
                success: false,
                error: Some(format!("git status failed: {e}")),
            });
            return BumpResult { events, success: false };
        }
    }

    // Write the new version to the manifest
    if let Err(e) = crate::build_system::version::set_package_version(
        repo_path,
        build_system,
        new_version,
    ) {
        events.push(HyperforgeEvent::PublishStep {
            package_name: package_name.to_string(),
            version: new_version.to_string(),
            registry: registry_kind_for(build_system),
            action: crate::service::PublishActionKind::Failed,
            success: false,
            error: Some(format!("version bump failed: {e}")),
        });
        return BumpResult { events, success: false };
    }

    if !skip_commit {
        // Stage manifest file
        match build_system {
            BuildSystemKind::Cargo => {
                let _ = Git::add(repo_path, "Cargo.toml");
                // Also stage Cargo.lock if it exists and changed
                if repo_path.join("Cargo.lock").exists() {
                    let _ = Git::add(repo_path, "Cargo.lock");
                }
            }
            BuildSystemKind::Cabal => {
                let _ = Git::add(repo_path, "*.cabal");
            }
            _ => {
                let _ = Git::add(repo_path, "package.json");
                if repo_path.join("package-lock.json").exists() {
                    let _ = Git::add(repo_path, "package-lock.json");
                }
            }
        }

        let commit_msg = format!("chore: bump {package_name} to {new_version}");
        let _ = Git::commit(repo_path, &commit_msg);
    }

    if !skip_tag {
        let tag_name = format!("{package_name}-v{new_version}");
        let tag_msg = format!("Release {package_name} v{new_version}");
        match Git::tag(repo_path, &tag_name, Some(&tag_msg)) {
            Ok(()) => {
                events.push(HyperforgeEvent::PublishStep {
                    package_name: package_name.to_string(),
                    version: new_version.to_string(),
                    registry: registry_kind_for(build_system),
                    action: crate::service::PublishActionKind::Tag,
                    success: true,
                    error: None,
                });
            }
            Err(e) => {
                events.push(HyperforgeEvent::Info {
                    message: format!("  Warning: failed to create tag {tag_name}: {e}"),
                });
            }
        }
    }

    BumpResult { events, success: true }
}

const fn registry_kind_for(bs: &BuildSystemKind) -> crate::service::PackageRegistry {
    match bs {
        BuildSystemKind::Cargo => crate::service::PackageRegistry::CratesIo,
        BuildSystemKind::Cabal => crate::service::PackageRegistry::Hackage,
        _ => crate::service::PackageRegistry::Npm,
    }
}

pub fn package_diff(
    path: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    stream! {
        let filter = RepoFilter::new(include, exclude);
        let workspace_path = PathBuf::from(&path);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        // Build dep graph (excluding dev deps for publish ordering)
        let graph = build_publish_dep_graph(&ctx.repos);

        if graph.nodes.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No packages found in workspace.".to_string(),
            };
            return;
        }

        // Filter packages
        let indices: Vec<usize> = graph.nodes.iter().enumerate()
            .filter(|(_, node)| filter.matches(&node.name))
            .map(|(i, _)| i)
            .collect();

        if indices.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No packages matched filter.".to_string(),
            };
            return;
        }

        yield HyperforgeEvent::Info {
            message: format!("Checking {} packages against registries...", indices.len()),
        };

        // Build work items — skip packages without registries or versions up front
        struct DiffItem {
            name: String,
            repo_path: PathBuf,
            build_system: crate::build_system::BuildSystemKind,
            local_version: String,
        }

        let mut work_items = Vec::new();
        let mut skip_events = Vec::new();

        for &idx in &indices {
            let node = &graph.nodes[idx];
            let build_system = match node.build_system.as_str() {
                "cargo" => crate::build_system::BuildSystemKind::Cargo,
                "cabal" => crate::build_system::BuildSystemKind::Cabal,
                "node" => crate::build_system::BuildSystemKind::Node,
                _ => crate::build_system::BuildSystemKind::Unknown,
            };

            if crate::package::registry_for(&build_system).is_none() {
                skip_events.push(HyperforgeEvent::Info {
                    message: format!("  {}: skipped (no registry for {})", node.name, node.build_system),
                });
                continue;
            }

            match &node.version {
                Some(v) => {
                    work_items.push(DiffItem {
                        name: node.name.clone(),
                        repo_path: workspace_path.join(&node.path),
                        build_system,
                        local_version: v.clone(),
                    });
                }
                None => {
                    skip_events.push(HyperforgeEvent::Info {
                        message: format!("  {}: skipped (no version)", node.name),
                    });
                }
            }
        }

        // Yield skip messages
        for event in skip_events {
            yield event;
        }

        // Run all registry queries + drift detection in parallel
        let results = run_batch(work_items, 8, |item| async move {
            let registry = match crate::package::registry_for(&item.build_system) {
                Some(r) => r,
                None => return None,
            };

            let registry_kind = registry.registry_kind();

            let published = match registry.published_version(&item.name).await {
                Ok(pv) => pv,
                Err(e) => {
                    return Some(Err(format!("  {}: registry query failed: {}", item.name, e)));
                }
            };

            let published_version = published.as_ref().map(|p| p.version.clone());

            let mut status = match &published_version {
                None => crate::service::PackageStatus::Unpublished,
                Some(pub_v) => {
                    match crate::build_system::version::compare_versions(&item.local_version, pub_v) {
                        Some(std::cmp::Ordering::Greater) => crate::service::PackageStatus::Ahead,
                        Some(std::cmp::Ordering::Equal) => crate::service::PackageStatus::UpToDate,
                        Some(std::cmp::Ordering::Less) => crate::service::PackageStatus::Stale,
                        None => crate::service::PackageStatus::Stale,
                    }
                }
            };

            let mut changed_files = None;
            if matches!(status, crate::service::PackageStatus::UpToDate) {
                match registry.detect_drift(&item.repo_path, &item.name, &item.local_version).await {
                    Ok(DriftResult::Drifted { changed_files: files }) => {
                        status = crate::service::PackageStatus::Drifted;
                        if !files.is_empty() {
                            changed_files = Some(files);
                        }
                    }
                    Ok(DriftResult::Identical | DriftResult::Unknown) => {}
                    Err(_) => {}
                }
            }

            Some(Ok(HyperforgeEvent::PackageDiff {
                package_name: item.name,
                build_system: item.build_system,
                local_version: item.local_version,
                published_version,
                registry: registry_kind,
                status,
                changed_files,
            }))
        }).await;

        // Yield results
        for result in results {
            match result {
                Ok(Some(Ok(event))) => yield event,
                Ok(Some(Err(msg))) => yield HyperforgeEvent::Error { message: msg },
                Ok(None) => {}
                Err(e) => yield HyperforgeEvent::Error { message: e },
            }
        }
    }
}

pub fn publish(
    path: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    execute: Option<bool>,
    no_tag: Option<bool>,
    no_commit: Option<bool>,
    bump: Option<String>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let is_dry_run = !execute.unwrap_or(false);
    let skip_tags = no_tag.unwrap_or(false);
    let skip_commits = no_commit.unwrap_or(false);
    let bump_kind = crate::types::VersionBump::from_str_or_patch(bump.as_deref());
    let dry_prefix = dry_prefix(is_dry_run);
    let filter = RepoFilter::new(include, exclude);

    stream! {
        let workspace_path = PathBuf::from(&path);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        // Build dep graph (excluding dev deps for publish ordering)
        let graph = build_publish_dep_graph(&ctx.repos);

        if graph.nodes.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No packages found in workspace.".to_string(),
            };
            return;
        }

        // Resolve targets from filter
        let targets: Vec<usize> = graph.nodes.iter().enumerate()
            .filter(|(_, node)| {
                if filter.is_empty() {
                    matches!(node.build_system.as_str(), "cargo" | "cabal")
                } else {
                    filter.matches(&node.name)
                }
            })
            .map(|(i, _)| i)
            .collect();

        if targets.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No publishable packages matched filter.".to_string(),
            };
            return;
        }

        // Build publish plan (queries registries, computes transitive closure)
        let plan = match crate::build_system::publish::build_publish_plan(
            &graph,
            &targets,
            &workspace_path,
            &bump_kind,
        ).await {
            Ok(p) => p,
            Err(e) => {
                yield HyperforgeEvent::Error {
                    message: format!("Failed to build publish plan: {e}"),
                };
                return;
            }
        };

        // Report exclusions
        for (name, reason) in &plan.excluded {
            yield HyperforgeEvent::Info {
                message: format!("{dry_prefix}Excluded {name}: {reason}"),
            };
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{}Publish plan: {} packages in dependency order",
                dry_prefix,
                plan.steps.len()
            ),
        };

        // Track failed nodes to skip dependents
        let mut failed_nodes: HashSet<usize> = HashSet::new();
        let mut published_count = 0usize;
        let mut auto_bumped_count = 0usize;
        let mut skipped_count = 0usize;
        let mut failed_count = 0usize;
        let mut tags_created = 0usize;

        for step in &plan.steps {
            // Check if any dependency failed
            let dep_failed = graph.direct_deps(step.node_idx)
                .iter()
                .any(|dep_idx| failed_nodes.contains(dep_idx));

            if dep_failed {
                failed_nodes.insert(step.node_idx);
                failed_count += 1;
                yield HyperforgeEvent::PublishStep {
                    package_name: step.name.clone(),
                    version: step.target_version.clone(),
                    registry: crate::service::PackageRegistry::CratesIo, // placeholder
                    action: crate::service::PublishActionKind::Failed,
                    success: false,
                    error: Some("dependency failed to publish".to_string()),
                };
                continue;
            }

            let build_system = &step.build_system;
            let registry = match crate::package::registry_for(build_system) {
                Some(r) => r,
                None => continue,
            };
            let registry_kind = registry.registry_kind();

            match &step.action {
                crate::build_system::publish::PublishAction::Skip => {
                    skipped_count += 1;
                    yield HyperforgeEvent::PublishStep {
                        package_name: step.name.clone(),
                        version: step.target_version.clone(),
                        registry: registry_kind,
                        action: crate::service::PublishActionKind::Skip,
                        success: true,
                        error: None,
                    };
                }
                crate::build_system::publish::PublishAction::Error(msg) => {
                    failed_nodes.insert(step.node_idx);
                    failed_count += 1;
                    yield HyperforgeEvent::PublishStep {
                        package_name: step.name.clone(),
                        version: step.target_version.clone(),
                        registry: registry_kind,
                        action: crate::service::PublishActionKind::Failed,
                        success: false,
                        error: Some(msg.clone()),
                    };
                }
                action => {
                    let is_auto_bump = matches!(action, crate::build_system::publish::PublishAction::AutoBump);
                    let action_kind = match action {
                        crate::build_system::publish::PublishAction::Publish => crate::service::PublishActionKind::Publish,
                        crate::build_system::publish::PublishAction::AutoBump => crate::service::PublishActionKind::AutoBump,
                        crate::build_system::publish::PublishAction::InitialPublish => crate::service::PublishActionKind::InitialPublish,
                        _ => unreachable!(),
                    };

                    // Auto-bump: check clean, bump, commit, tag
                    if is_auto_bump && !is_dry_run {
                        let bump_result = bump_commit_tag(
                            &step.path,
                            &step.name,
                            &step.target_version,
                            build_system,
                            skip_commits,
                            true, // skip tag here — tag after successful publish
                        );
                        for event in bump_result.events {
                            yield event;
                        }
                        if !bump_result.success {
                            failed_nodes.insert(step.node_idx);
                            failed_count += 1;
                            continue;
                        }
                        auto_bumped_count += 1;
                    } else if is_auto_bump {
                        // Dry run auto-bump
                        auto_bumped_count += 1;
                    }

                    // Publish
                    let result = registry.publish(&step.path, &step.name, is_dry_run).await;

                    match result {
                        Ok(pr) if pr.success => {
                            published_count += 1;

                            yield HyperforgeEvent::PublishStep {
                                package_name: step.name.clone(),
                                version: step.target_version.clone(),
                                registry: registry_kind.clone(),
                                action: action_kind,
                                success: true,
                                error: None,
                            };

                            // Git tag after successful publish
                            if !skip_tags && !is_dry_run {
                                let tag_name = format!("{}-v{}", step.name, step.target_version);
                                let tag_msg = format!("Release {} v{}", step.name, step.target_version);
                                if let Err(e) = Git::tag(&step.path, &tag_name, Some(&tag_msg)) {
                                    yield HyperforgeEvent::Info {
                                        message: format!("  Warning: failed to create tag {tag_name}: {e}"),
                                    };
                                } else {
                                    tags_created += 1;
                                    yield HyperforgeEvent::PublishStep {
                                        package_name: step.name.clone(),
                                        version: step.target_version.clone(),
                                        registry: registry_kind.clone(),
                                        action: crate::service::PublishActionKind::Tag,
                                        success: true,
                                        error: None,
                                    };
                                }
                            }
                        }
                        Ok(pr) => {
                            // Publish returned but was not successful
                            failed_nodes.insert(step.node_idx);
                            failed_count += 1;
                            yield HyperforgeEvent::PublishStep {
                                package_name: step.name.clone(),
                                version: step.target_version.clone(),
                                registry: registry_kind,
                                action: crate::service::PublishActionKind::Failed,
                                success: false,
                                error: pr.error,
                            };
                        }
                        Err(e) => {
                            failed_nodes.insert(step.node_idx);
                            failed_count += 1;
                            yield HyperforgeEvent::PublishStep {
                                package_name: step.name.clone(),
                                version: step.target_version.clone(),
                                registry: registry_kind,
                                action: crate::service::PublishActionKind::Failed,
                                success: false,
                                error: Some(format!("{e}")),
                            };
                        }
                    }
                }
            }
        }

        yield HyperforgeEvent::PublishSummary {
            total: plan.steps.len(),
            published: published_count,
            auto_bumped: auto_bumped_count,
            skipped: skipped_count,
            failed: failed_count,
            tags_created,
        };
    }
}

pub fn bump(
    path: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    bump: Option<String>,
    commit: Option<bool>,
    dry_run: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let bump_kind = crate::types::VersionBump::from_str_or_patch(bump.as_deref());
    let auto_commit = commit.unwrap_or(false);
    let is_dry_run = dry_run.unwrap_or(false);
    let dry_prefix = dry_prefix(is_dry_run);
    let filter = RepoFilter::new(include, exclude);

    stream! {
        let workspace_path = PathBuf::from(&path);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        let repos: Vec<_> = ctx.repos.iter().filter(|r| {
            let name = r.package_name.as_deref().unwrap_or(&r.dir_name);
            filter.matches(name)
        }).collect();

        if repos.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No packages matched filter.".to_string(),
            };
            return;
        }

        yield HyperforgeEvent::Info {
            message: format!("{}Bumping {} packages ({:?})...", dry_prefix, repos.len(), bump_kind),
        };

        let mut bumped = 0usize;
        let mut failed = 0usize;

        for repo in &repos {
            let name = repo.package_name.as_deref().unwrap_or(&repo.dir_name);
            let current_version = if let Some(v) = &repo.package_version { v.clone() } else {
                yield HyperforgeEvent::Info {
                    message: format!("  {name}: skipped (no version)"),
                };
                continue;
            };

            let parsed = if let Some(v) = crate::build_system::version::SemVer::parse(&current_version) { v } else {
                yield HyperforgeEvent::Info {
                    message: format!("  {name}: skipped (unparseable version: {current_version})"),
                };
                continue;
            };

            let new_version = parsed.bump(&bump_kind).to_string();

            if is_dry_run {
                bumped += 1;
            } else {
                let result = bump_commit_tag(
                    &repo.path,
                    name,
                    &new_version,
                    &repo.build_system,
                    !auto_commit,  // skip_commit = !auto_commit
                    !auto_commit,  // skip_tag = !auto_commit (tags go with commits)
                );
                for event in result.events {
                    yield event;
                }
                if result.success {
                    bumped += 1;
                } else {
                    failed += 1;
                    continue;
                }
            }

            yield HyperforgeEvent::PublishStep {
                package_name: name.to_string(),
                version: new_version.clone(),
                registry: registry_kind_for(&repo.build_system),
                action: crate::service::PublishActionKind::AutoBump,
                success: true,
                error: None,
            };
        }

        yield HyperforgeEvent::Info {
            message: format!("{dry_prefix}Bump complete: {bumped} bumped, {failed} failed"),
        };
    }
}

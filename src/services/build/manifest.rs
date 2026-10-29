//! Manifest generation and analysis: unify, analyze, `detect_name_mismatches`.

use async_stream::stream;
use futures::Stream;
use std::path::PathBuf;

use crate::commands::runner::discover_or_bail;
use crate::commands::workspace::build_dep_graph;
use crate::service::HyperforgeEvent;
use crate::services::utils::dry_prefix;

pub fn unify(
    path: String,
    dry_run: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let is_dry_run = dry_run.unwrap_or(false);

    stream! {
        let workspace_path = PathBuf::from(&path);
        let dry_prefix = dry_prefix(is_dry_run);

        // Discover workspace
        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        yield HyperforgeEvent::Info {
            message: format!("{}Workspace unify: {} repos discovered", dry_prefix, ctx.repos.len()),
        };

        // Collect Rust crates
        let rust_repos = ctx.repos_for_build_system(&crate::build_system::BuildSystemKind::Cargo);
        if !rust_repos.is_empty() {
            yield HyperforgeEvent::Info {
                message: format!("  Found {} Rust crates", rust_repos.len()),
            };

            let crates: Vec<crate::build_system::cargo_config::CrateInfo> = rust_repos
                .iter()
                .map(|repo| {
                    let name = repo.effective_name();
                    let version = repo.package_version.clone().unwrap_or_else(|| "0.0.0".to_string());
                    let rel_path = repo.dir_name.clone();
                    crate::build_system::cargo_config::CrateInfo {
                        name,
                        version,
                        path: rel_path,
                        dependencies: repo.dependencies.clone(),
                    }
                })
                .collect();

            match crate::build_system::cargo_config::generate_cargo_config(
                &ctx.root,
                &crates,
                is_dry_run,
            ) {
                Ok(report) => {
                    let action_str = match report.action {
                        crate::build_system::cargo_config::FileAction::Created => "created",
                        crate::build_system::cargo_config::FileAction::Updated => "updated",
                        crate::build_system::cargo_config::FileAction::Unchanged => "unchanged",
                        crate::build_system::cargo_config::FileAction::Removed => "removed",
                    };

                    yield HyperforgeEvent::UnifyResult {
                        language: "rust".to_string(),
                        file_path: ctx.root.join(".cargo/config.toml").to_string_lossy().to_string(),
                        action: action_str.to_string(),
                    };

                    yield HyperforgeEvent::Info {
                        message: format!(
                            "{}.cargo/config.toml: {} patches [{}]",
                            dry_prefix,
                            report.patches.len(),
                            action_str
                        ),
                    };

                    if !report.patches.is_empty() {
                        for (name, path) in &report.patches {
                            yield HyperforgeEvent::Info {
                                message: format!("  patch: {name} -> {path}"),
                            };
                        }
                    }

                    for (desc, cleanup_action) in &report.cleanup {
                        let cleanup_str = match cleanup_action {
                            crate::build_system::cargo_config::FileAction::Removed => "removed",
                            crate::build_system::cargo_config::FileAction::Updated => "updated",
                            crate::build_system::cargo_config::FileAction::Created => "created",
                            crate::build_system::cargo_config::FileAction::Unchanged => "unchanged",
                        };
                        yield HyperforgeEvent::UnifyResult {
                            language: "rust".to_string(),
                            file_path: desc.clone(),
                            action: cleanup_str.to_string(),
                        };
                        yield HyperforgeEvent::Info {
                            message: format!("  {dry_prefix}{desc} [{cleanup_str}]"),
                        };
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to generate .cargo/config.toml: {e}"),
                    };
                }
            }
        }

        // Collect Haskell packages
        let cabal_repos = ctx.repos_for_build_system(&crate::build_system::BuildSystemKind::Cabal);
        if !cabal_repos.is_empty() {
            yield HyperforgeEvent::Info {
                message: format!("  Found {} Haskell packages", cabal_repos.len()),
            };

            let packages: Vec<crate::build_system::cabal_project::CabalPackageInfo> = cabal_repos
                .iter()
                .map(|repo| crate::build_system::cabal_project::CabalPackageInfo {
                    name: repo.effective_name(),
                    path: repo.dir_name.clone(),
                })
                .collect();

            match crate::build_system::cabal_project::generate_cabal_project(
                &ctx.root,
                &packages,
                is_dry_run,
            ) {
                Ok(report) => {
                    let action_str = match report.action {
                        crate::build_system::cabal_project::FileAction::Created => "created",
                        crate::build_system::cabal_project::FileAction::Updated => "updated",
                        crate::build_system::cabal_project::FileAction::Unchanged => "unchanged",
                    };

                    yield HyperforgeEvent::UnifyResult {
                        language: "haskell".to_string(),
                        file_path: ctx.root.join("cabal.project").to_string_lossy().to_string(),
                        action: action_str.to_string(),
                    };

                    yield HyperforgeEvent::Info {
                        message: format!(
                            "{}cabal.project: {} packages [{}]",
                            dry_prefix,
                            report.packages.len(),
                            action_str
                        ),
                    };
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Failed to generate cabal.project: {e}"),
                    };
                }
            }
        }

        if rust_repos.is_empty() && cabal_repos.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No Rust or Haskell projects found — nothing to unify.".to_string(),
            };
        }
    }
}

pub fn analyze(
    path: String,
    format: Option<String>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let output_format = format.unwrap_or_else(|| "summary".to_string());

    stream! {
        let workspace_path = PathBuf::from(&path);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        // Build dep graph from all discovered repos
        let graph = build_dep_graph(&ctx.repos);

        if graph.nodes.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No packages found in workspace.".to_string(),
            };
            return;
        }

        match output_format.as_str() {
            "summary" => {
                // Build tiers
                match graph.build_tiers() {
                    Ok(tiers) => {
                        yield HyperforgeEvent::Info {
                            message: format!(
                                "Workspace: {} packages, {} internal deps, {} build tiers",
                                graph.nodes.len(),
                                graph.edges.len(),
                                tiers.len()
                            ),
                        };

                        for (tier_idx, tier) in tiers.iter().enumerate() {
                            let names: Vec<&str> = tier
                                .iter()
                                .map(|&i| graph.nodes[i].name.as_str())
                                .collect();
                            yield HyperforgeEvent::Info {
                                message: format!("  Tier {}: {}", tier_idx, names.join(", ")),
                            };
                        }
                    }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Cycle detected: {e}"),
                        };
                    }
                }

                // Show mismatches summary
                let mismatches = graph.version_mismatches();
                if mismatches.is_empty() {
                    yield HyperforgeEvent::Info {
                        message: "No version mismatches detected.".to_string(),
                    };
                } else {
                    yield HyperforgeEvent::Info {
                        message: format!("\n{} version mismatches:", mismatches.len()),
                    };
                    for m in &mismatches {
                        yield HyperforgeEvent::DepMismatch {
                            repo: m.repo_name.clone(),
                            dependency: m.dependency.clone(),
                            pinned_version: m.pinned_version.clone(),
                            local_version: m.local_version.clone(),
                        };
                    }
                }
            }

            "graph" => {
                for (i, node) in graph.nodes.iter().enumerate() {
                    let deps = graph.direct_deps(i);
                    let rdeps = graph.reverse_deps(i);

                    let dep_names: Vec<&str> = deps
                        .iter()
                        .map(|&j| graph.nodes[j].name.as_str())
                        .collect();
                    let rdep_names: Vec<&str> = rdeps
                        .iter()
                        .map(|&j| graph.nodes[j].name.as_str())
                        .collect();

                    let version_str = node
                        .version
                        .as_deref()
                        .unwrap_or("?");

                    yield HyperforgeEvent::Info {
                        message: format!(
                            "{} v{} [{}] deps=[{}] rdeps=[{}]",
                            node.name,
                            version_str,
                            node.build_system,
                            dep_names.join(", "),
                            rdep_names.join(", ")
                        ),
                    };
                }
            }

            "mismatches" => {
                let mismatches = graph.version_mismatches();
                if mismatches.is_empty() {
                    yield HyperforgeEvent::Info {
                        message: "No version mismatches detected.".to_string(),
                    };
                } else {
                    yield HyperforgeEvent::Info {
                        message: format!("{} version mismatches:", mismatches.len()),
                    };
                    for m in &mismatches {
                        yield HyperforgeEvent::DepMismatch {
                            repo: m.repo_name.clone(),
                            dependency: m.dependency.clone(),
                            pinned_version: m.pinned_version.clone(),
                            local_version: m.local_version.clone(),
                        };
                    }
                }
            }

            other => {
                yield HyperforgeEvent::Error {
                    message: format!(
                        "Unknown format '{other}'. Valid: summary, graph, mismatches"
                    ),
                };
            }
        }
    }
}

pub fn detect_name_mismatches(
    path: String,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    stream! {
        let workspace_path = PathBuf::from(&path);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        // Report unconfigured repos (git repos without .hyperforge/config.toml)
        for unconfigured in &ctx.unconfigured_repos {
            let name = unconfigured.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?");
            yield HyperforgeEvent::Info {
                message: format!(
                    "UNCONFIGURED: {} — git repo without .hyperforge/config.toml (run: hyperforge repo init --path {})",
                    name, unconfigured.display()
                ),
            };
        }

        // Report name mismatches
        let mut mismatches = 0usize;
        let mut checked = 0usize;

        for repo in &ctx.repos {
            let pkg_name = match &repo.package_name {
                Some(n) => n,
                None => continue,
            };
            checked += 1;

            if *pkg_name != repo.dir_name {
                mismatches += 1;
                yield HyperforgeEvent::Info {
                    message: format!(
                        "MISMATCH: dir={} package={} ({})",
                        repo.dir_name, pkg_name, repo.build_system
                    ),
                };
            }
        }

        let unconfigured_count = ctx.unconfigured_repos.len();
        if mismatches == 0 && unconfigured_count == 0 {
            yield HyperforgeEvent::Info {
                message: format!("All {checked} packages match their directory names. No unconfigured repos."),
            };
        } else {
            yield HyperforgeEvent::Info {
                message: format!(
                    "{mismatches} name mismatches, {unconfigured_count} unconfigured repos (across {checked} configured packages)."
                ),
            };
        }
    }
}

//! Distribution config management: show and initialize [dist] sections in .hyperforge/config.toml.

use async_stream::stream;
use futures::Stream;
use std::path::PathBuf;

use crate::build_system::BuildSystemKind;
use crate::commands::runner::discover_or_bail;
use crate::config::HyperforgeConfig;
use crate::service::HyperforgeEvent;
use crate::services::utils::{dry_prefix, RepoFilter};
use crate::types::config::{DistChannel, DistConfig};

/// Common cross-compilation targets for Rust binary releases.
const COMMON_RUST_TARGETS: &[&str] = &[
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

/// Default channels for a Rust binary project.
fn default_rust_channels() -> Vec<DistChannel> {
    vec![
        DistChannel::ForgeRelease,
        DistChannel::CratesIo,
        DistChannel::Binstall,
    ]
}

/// Default channels for a Haskell project.
fn default_haskell_channels() -> Vec<DistChannel> {
    vec![
        DistChannel::ForgeRelease,
        DistChannel::Hackage,
        DistChannel::Brew,
    ]
}

/// Parse comma-separated target triples.
fn parse_target_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Show distribution config for repos in a workspace.
pub fn dist_show(
    path: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let filter = RepoFilter::new(include, exclude);

    stream! {
        let workspace_path = PathBuf::from(&path);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        let mut configured = 0usize;
        let mut unconfigured = 0usize;

        for repo in &ctx.repos {
            if !filter.matches(&repo.dir_name) {
                continue;
            }

            let name = repo.effective_name();

            let dist = repo.config.as_ref().and_then(|c| c.dist.as_ref());

            if let Some(dist) = dist {
                configured += 1;
                let channels: Vec<String> = dist.channels.iter().map(std::string::ToString::to_string).collect();
                let targets_str = if dist.targets.is_empty() {
                    "none".to_string()
                } else {
                    dist.targets.join(", ")
                };

                let mut parts = vec![
                    format!("channels: [{}]", channels.join(", ")),
                    format!("targets: [{}]", targets_str),
                ];
                if let Some(ref tap) = dist.brew_tap {
                    parts.push(format!("brew_tap: {tap}"));
                }
                if let Some(ref tap_path) = dist.brew_tap_path {
                    parts.push(format!("brew_tap_path: {tap_path}"));
                }

                yield HyperforgeEvent::Info {
                    message: format!("  {} — {}", name, parts.join(", ")),
                };
            } else {
                unconfigured += 1;
                yield HyperforgeEvent::Info {
                    message: format!("  {name} — not configured"),
                };
            }
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "Distribution config: {configured} configured, {unconfigured} not configured",
            ),
        };
    }
}

/// Populate [dist] config for repos in a workspace.
pub fn dist_init(
    path: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    channels: Option<Vec<DistChannel>>,
    targets: Option<String>,
    brew_tap: Option<String>,
    force: Option<bool>,
    dry_run: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let filter = RepoFilter::new(include, exclude);
    let is_dry_run = dry_run.unwrap_or(false);
    let is_force = force.unwrap_or(false);

    stream! {
        let workspace_path = PathBuf::from(&path);
        let dry = dry_prefix(is_dry_run);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        let mut written = 0usize;
        let mut skipped_existing = 0usize;
        let mut skipped_no_bs = 0usize;

        for repo in &ctx.repos {
            if !filter.matches(&repo.dir_name) {
                continue;
            }

            let name = repo.effective_name();

            // Skip repos that already have [dist] unless --force
            let has_existing = repo.config.as_ref().and_then(|c| c.dist.as_ref()).is_some();
            if has_existing && !is_force {
                skipped_existing += 1;
                continue;
            }

            // Determine build system for defaults
            let bs = &repo.build_system;
            let (default_channels, default_targets) = match bs {
                BuildSystemKind::Cargo => (
                    default_rust_channels(),
                    COMMON_RUST_TARGETS.iter().map(std::string::ToString::to_string).collect::<Vec<_>>(),
                ),
                BuildSystemKind::Cabal => (
                    default_haskell_channels(),
                    vec!["native".to_string()],
                ),
                _ => {
                    skipped_no_bs += 1;
                    continue;
                }
            };

            // Apply overrides from CLI
            let final_channels = match &channels {
                Some(ch) => ch.clone(),
                None => default_channels,
            };
            let final_targets = match &targets {
                Some(s) => parse_target_list(s),
                None => default_targets,
            };

            let dist = DistConfig {
                channels: final_channels.clone(),
                targets: final_targets.clone(),
                brew_tap: brew_tap.clone(),
                brew_tap_path: None,
            };

            let channels_str: Vec<String> = dist.channels.iter().map(std::string::ToString::to_string).collect();
            yield HyperforgeEvent::Info {
                message: format!(
                    "{}{}: channels=[{}], targets=[{}]{}",
                    dry,
                    name,
                    channels_str.join(", "),
                    dist.targets.join(", "),
                    dist.brew_tap.as_ref().map(|t| format!(", brew_tap={t}")).unwrap_or_default(),
                ),
            };

            if is_dry_run {
                written += 1;
            } else {
                // Load existing config or create minimal one
                let mut config = repo.config.clone().unwrap_or_else(HyperforgeConfig::default);
                config.dist = Some(dist);

                match config.save(&repo.path) {
                    Ok(()) => { written += 1; }
                    Err(e) => {
                        yield HyperforgeEvent::Error {
                            message: format!("Failed to write config for {name}: {e}"),
                        };
                    }
                }
            }
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{dry}Dist init: {written} written, {skipped_existing} already configured, {skipped_no_bs} no build system",
            ),
        };
    }
}

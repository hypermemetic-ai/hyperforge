//! Binstall metadata injection: add `[package.metadata.binstall]` to Cargo.toml files.

use async_stream::stream;
use futures::Stream;
use std::path::{Path, PathBuf};

use crate::commands::runner::discover_or_bail;
use crate::config::HyperforgeConfig;
use crate::service::HyperforgeEvent;
use crate::services::utils::{dry_prefix, RepoFilter};
use crate::types::Forge;

// ---------------------------------------------------------------------------
// Forge URL base
// ---------------------------------------------------------------------------

fn forge_url_base(forge: &str, org: &str, repo_name: &str) -> String {
    match forge {
        "codeberg" => format!("https://codeberg.org/{org}/{repo_name}"),
        "gitlab" => format!("https://gitlab.com/{org}/{repo_name}"),
        // Default to GitHub
        _ => format!("https://github.com/{org}/{repo_name}"),
    }
}

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

enum BinstallResult {
    /// Already had binstall metadata — skipped
    Skipped,
    /// Not a Cargo project — skipped
    NotCargo,
    /// Successfully injected binstall metadata
    Injected { pkg_url: String },
}

fn inject_binstall(
    repo_path: &Path,
    dir_name: &str,
    forge_hint: Option<&str>,
    dry_run: bool,
) -> Result<BinstallResult, String> {
    let cargo_path = repo_path.join("Cargo.toml");
    if !cargo_path.exists() {
        return Ok(BinstallResult::NotCargo);
    }

    let content = std::fs::read_to_string(&cargo_path)
        .map_err(|e| format!("failed to read Cargo.toml: {e}"))?;

    let mut doc: toml_edit::DocumentMut = content
        .parse()
        .map_err(|e| format!("failed to parse Cargo.toml: {e}"))?;

    // Check if binstall metadata already exists
    if doc
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("binstall"))
        .is_some()
    {
        return Ok(BinstallResult::Skipped);
    }

    // Ensure [package] exists
    if doc.get("package").is_none() {
        return Err("Cargo.toml has no [package] section".to_string());
    }

    // Determine org and forge
    let forge = forge_hint.unwrap_or("github");
    let org = match HyperforgeConfig::load(repo_path) {
        Ok(cfg) => cfg.org.unwrap_or_default(),
        Err(_) => String::new(),
    };

    if org.is_empty() {
        return Err(
            "no org found — set org in .hyperforge/config.toml or pass --forge".to_string(),
        );
    }

    let repo_url = forge_url_base(forge, &org, dir_name);

    // Binstall template variables are literal single-brace tokens like { version }.
    // We must NOT interpolate them — they go verbatim into the TOML.
    let pkg_url = [
        &repo_url,
        "/releases/download/v{ version }/{ name }-{ target }-v{ version }{ archive-suffix }",
    ]
    .concat();
    let bin_dir = "{ name }-{ target }-v{ version }/{ bin }{ binary-ext }";
    let pkg_fmt = "tgz";

    if !dry_run {
        doc["package"]["metadata"]["binstall"]["pkg-url"] = toml_edit::value(&pkg_url);
        doc["package"]["metadata"]["binstall"]["bin-dir"] = toml_edit::value(bin_dir);
        doc["package"]["metadata"]["binstall"]["pkg-fmt"] = toml_edit::value(pkg_fmt);

        std::fs::write(&cargo_path, doc.to_string())
            .map_err(|e| format!("failed to write Cargo.toml: {e}"))?;
    }

    Ok(BinstallResult::Injected { pkg_url })
}

// ---------------------------------------------------------------------------
// Streaming function
// ---------------------------------------------------------------------------

pub fn binstall_init(
    path: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    forge: Option<Forge>,
    dry_run: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let is_dry_run = dry_run.unwrap_or(false);
    let forge_hint: Option<String> = forge.map(|f| f.as_str().to_string());
    let filter = RepoFilter::new(include, exclude);

    stream! {
        let workspace_path = PathBuf::from(&path);
        let prefix = dry_prefix(is_dry_run);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        let mut items: Vec<(String, PathBuf)> = Vec::new();

        for repo in &ctx.repos {
            if !filter.matches(&repo.dir_name) {
                continue;
            }
            items.push((repo.dir_name.clone(), repo.path.clone()));
        }

        // Also include unconfigured repos
        for repo_path in &ctx.unconfigured_repos {
            let dir_name = repo_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !filter.matches(&dir_name) {
                continue;
            }
            items.push((dir_name, repo_path.clone()));
        }

        if items.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No repos matched filter.".to_string(),
            };
            return;
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{}Injecting binstall metadata across {} repos...",
                prefix,
                items.len()
            ),
        };

        let mut injected = 0usize;
        let mut skipped = 0usize;
        let mut not_cargo = 0usize;
        let mut failed = 0usize;

        for (dir_name, repo_path) in &items {
            let result = inject_binstall(repo_path, dir_name, forge_hint.as_deref(), is_dry_run);
            match result {
                Ok(BinstallResult::Skipped) => {
                    skipped += 1;
                    yield HyperforgeEvent::Info {
                        message: format!("  {dir_name}: already has binstall metadata, skipped"),
                    };
                }
                Ok(BinstallResult::NotCargo) => {
                    not_cargo += 1;
                }
                Ok(BinstallResult::Injected { pkg_url }) => {
                    injected += 1;
                    yield HyperforgeEvent::Info {
                        message: format!("{prefix}  {dir_name}: injected binstall metadata (pkg-url: {pkg_url})"),
                    };
                }
                Err(e) => {
                    failed += 1;
                    yield HyperforgeEvent::Error {
                        message: format!("  {dir_name}: {e}"),
                    };
                }
            }
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{prefix}Binstall init complete: {injected} injected, {skipped} already configured, {not_cargo} non-Cargo, {failed} failed"
            ),
        };
    }
}

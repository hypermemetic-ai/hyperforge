//! Workspace-wide .gitignore management: ensure sane patterns across all repos.

use async_stream::stream;
use futures::Stream;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::build_system::BuildSystemKind;
use crate::commands::runner::{discover_or_bail, run_batch_blocking};
use crate::service::HyperforgeEvent;
use crate::services::utils::{dry_prefix, RepoFilter};

// ---------------------------------------------------------------------------
// Default pattern groups
// ---------------------------------------------------------------------------

const OS_PATTERNS: &[&str] = &[
    ".DS_Store",
    "Thumbs.db",
    "Desktop.ini",
    ".Spotlight-V100",
    ".Trashes",
];

const EDITOR_PATTERNS: &[&str] = &[
    "*.swp",
    "*.swo",
    "*~",
    ".vscode/",
    ".idea/",
    "*.iml",
];

const BUILD_ARTIFACT_PATTERNS: &[&str] = &[
    "*.o",
    "*.a",
    "*.so",
    "*.dylib",
    "*.dll",
    "*.exe",
    "*.out",
];

const DATA_PATTERNS: &[&str] = &["*.db", "*.sqlite", "*.sqlite3"];

const LOG_PATTERNS: &[&str] = &["*.log"];

const ENV_PATTERNS: &[&str] = &[".env", ".env.local", ".env.*.local"];

fn default_patterns() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("OS files", OS_PATTERNS),
        ("Editor / IDE", EDITOR_PATTERNS),
        ("Build artifacts", BUILD_ARTIFACT_PATTERNS),
        ("Data files", DATA_PATTERNS),
        ("Logs", LOG_PATTERNS),
        ("Env / secrets", ENV_PATTERNS),
    ]
}

// ---------------------------------------------------------------------------
// Build-system-aware patterns
// ---------------------------------------------------------------------------

const fn patterns_for_build_system(kind: &BuildSystemKind) -> Option<(&'static str, &'static [&'static str])> {
    match kind {
        BuildSystemKind::Cargo => Some(("Cargo", &["target/"])),
        BuildSystemKind::Cabal => Some(("Cabal", &[
            "dist-newstyle/",
            ".cabal-sandbox/",
            "*.hi",
            "*.dyn_hi",
            "*.dyn_o",
        ])),
        BuildSystemKind::Node => Some(("Node", &[
            "node_modules/",
            ".npm/",
            ".yarn/",
        ])),
        BuildSystemKind::Unknown => None,
    }
}

// ---------------------------------------------------------------------------
// Core logic
// ---------------------------------------------------------------------------

enum GitignoreResult {
    Unchanged,
    Updated { added: usize },
    Created { added: usize },
}

fn ensure_patterns(
    repo_path: &Path,
    build_systems: &[BuildSystemKind],
    extra: &[String],
    dry_run: bool,
) -> Result<GitignoreResult, String> {
    let gitignore_path = repo_path.join(".gitignore");

    let existing_content = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)
            .map_err(|e| format!("failed to read .gitignore: {e}"))?
    } else {
        String::new()
    };

    let existed = gitignore_path.exists();

    // Parse existing lines into a set (trimmed, skip comments/blanks)
    let existing: HashSet<String> = existing_content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();

    // Collect all desired groups
    let mut groups: Vec<(&str, Vec<&str>)> = default_patterns()
        .into_iter()
        .map(|(label, pats)| (label, pats.to_vec()))
        .collect();

    // Add build-system-specific patterns
    let mut seen_bs: HashSet<String> = HashSet::new();
    for bs in build_systems {
        let key = format!("{bs:?}");
        if seen_bs.contains(&key) {
            continue;
        }
        seen_bs.insert(key);
        if let Some((label, pats)) = patterns_for_build_system(bs) {
            groups.push((label, pats.to_vec()));
        }
    }

    // Add user-supplied extra patterns
    if !extra.is_empty() {
        // We'll store refs differently for extras
        // Just handle them as a separate group below
    }

    // Filter each group to only patterns not already present
    let mut additions: Vec<(String, Vec<String>)> = Vec::new();

    for (label, pats) in &groups {
        let missing: Vec<String> = pats
            .iter()
            .filter(|p| !existing.contains(**p))
            .map(std::string::ToString::to_string)
            .collect();
        if !missing.is_empty() {
            additions.push((label.to_string(), missing));
        }
    }

    // Extra user patterns
    let missing_extra: Vec<String> = extra
        .iter()
        .filter(|p| !existing.contains(p.trim()))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if !missing_extra.is_empty() {
        additions.push(("User patterns".to_string(), missing_extra));
    }

    if additions.is_empty() {
        return Ok(GitignoreResult::Unchanged);
    }

    // Build the text to append
    let total_added: usize = additions.iter().map(|(_, pats)| pats.len()).sum();

    let mut appendix = String::new();
    if !existing_content.is_empty() && !existing_content.ends_with('\n') {
        appendix.push('\n');
    }
    appendix.push_str("\n# --- Added by hyperforge ---\n");

    for (label, pats) in &additions {
        appendix.push_str(&format!("\n# {label}\n"));
        for p in pats {
            appendix.push_str(p);
            appendix.push('\n');
        }
    }

    if !dry_run {
        let mut full = existing_content;
        full.push_str(&appendix);
        std::fs::write(&gitignore_path, full)
            .map_err(|e| format!("failed to write .gitignore: {e}"))?;
    }

    if existed {
        Ok(GitignoreResult::Updated { added: total_added })
    } else {
        Ok(GitignoreResult::Created { added: total_added })
    }
}

// ---------------------------------------------------------------------------
// Streaming function
// ---------------------------------------------------------------------------

pub fn gitignore_sync(
    path: String,
    patterns: Option<Vec<String>>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    dry_run: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let is_dry_run = dry_run.unwrap_or(false);
    let extra_patterns = patterns.unwrap_or_default();
    let filter = RepoFilter::new(include, exclude);

    stream! {
        let workspace_path = PathBuf::from(&path);
        let prefix = dry_prefix(is_dry_run);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        // Build work items from both configured and unconfigured repos
        struct RepoItem {
            dir_name: String,
            path: PathBuf,
            build_systems: Vec<BuildSystemKind>,
        }

        let mut items: Vec<RepoItem> = Vec::new();

        // Configured repos
        for repo in &ctx.repos {
            if !filter.matches(&repo.dir_name) {
                continue;
            }
            items.push(RepoItem {
                dir_name: repo.dir_name.clone(),
                path: repo.path.clone(),
                build_systems: repo.build_systems.clone(),
            });
        }

        // Unconfigured repos (git but no .hyperforge)
        for repo_path in &ctx.unconfigured_repos {
            let dir_name = repo_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if !filter.matches(&dir_name) {
                continue;
            }

            let build_systems = crate::build_system::detect_all_build_systems(repo_path);
            items.push(RepoItem {
                dir_name,
                path: repo_path.clone(),
                build_systems,
            });
        }

        if items.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No repos matched filter.".to_string(),
            };
            return;
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{}Syncing .gitignore across {} repos...",
                prefix,
                items.len()
            ),
        };

        // Run ensure_patterns in parallel
        type GitignoreWorkItem = (String, PathBuf, Vec<BuildSystemKind>, Vec<String>, bool);
        let work: Vec<GitignoreWorkItem> = items
            .into_iter()
            .map(|item| {
                (
                    item.dir_name,
                    item.path,
                    item.build_systems,
                    extra_patterns.clone(),
                    is_dry_run,
                )
            })
            .collect();

        let results = run_batch_blocking(work, 8, |(dir_name, repo_path, build_systems, extra, dry)| {
            let result = ensure_patterns(&repo_path, &build_systems, &extra, dry);
            (dir_name, result)
        })
        .await;

        let mut unchanged = 0usize;
        let mut updated = 0usize;
        let mut created = 0usize;
        let mut failed = 0usize;

        for result in results {
            match result {
                Ok((name, Ok(GitignoreResult::Unchanged))) => {
                    unchanged += 1;
                    yield HyperforgeEvent::Info {
                        message: format!("  {name}: unchanged"),
                    };
                }
                Ok((name, Ok(GitignoreResult::Updated { added }))) => {
                    updated += 1;
                    yield HyperforgeEvent::Info {
                        message: format!("{prefix}  {name}: added {added} patterns"),
                    };
                }
                Ok((name, Ok(GitignoreResult::Created { added }))) => {
                    created += 1;
                    yield HyperforgeEvent::Info {
                        message: format!("{prefix}  {name}: created .gitignore ({added} patterns)"),
                    };
                }
                Ok((name, Err(e))) => {
                    failed += 1;
                    yield HyperforgeEvent::Error {
                        message: format!("  {name}: {e}"),
                    };
                }
                Err(e) => {
                    failed += 1;
                    yield HyperforgeEvent::Error {
                        message: format!("Task error: {e}"),
                    };
                }
            }
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{prefix}Gitignore sync complete: {created} created, {updated} updated, {unchanged} unchanged, {failed} failed"
            ),
        };
    }
}

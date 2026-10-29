//! Workspace-level dirty check: find repos with uncommitted changes.

use async_stream::stream;
use futures::Stream;
use std::path::PathBuf;

use crate::commands::runner::discover_or_bail;
use crate::git::Git;
use crate::service::HyperforgeEvent;
use crate::services::utils::RepoFilter;

/// Check dirty status for a single repo path.
fn check_dirty(repo_path: &std::path::Path) -> Result<(String, bool, bool, bool), String> {
    let status = Git::repo_status(repo_path).map_err(|e| format!("{e}"))?;
    Ok((
        status.branch,
        status.has_staged,
        status.has_changes,
        status.has_untracked,
    ))
}

pub fn dirty(
    path: String,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    all_git: Option<bool>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let filter = RepoFilter::new(include, exclude);
    let scan_all = all_git.unwrap_or(false);

    stream! {
        let workspace_path = PathBuf::from(&path);

        let ctx = match discover_or_bail(&workspace_path) {
            Ok(ctx) => ctx,
            Err(event) => { yield event; return; }
        };

        // Build list of (name, path) to check
        let mut targets: Vec<(String, PathBuf)> = ctx.repos.iter()
            .filter(|r| filter.matches(&r.dir_name))
            .map(|r| (r.dir_name.clone(), r.path.clone()))
            .collect();

        if scan_all {
            for unconfigured_path in &ctx.unconfigured_repos {
                if let Some(name) = unconfigured_path.file_name().and_then(|n| n.to_str()) {
                    if filter.matches(name) {
                        targets.push((name.to_string(), unconfigured_path.clone()));
                    }
                }
            }
            targets.sort_by(|a, b| a.0.cmp(&b.0));
        }

        if targets.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No repos matched filter.".to_string(),
            };
            return;
        }

        let results = crate::commands::runner::run_batch_blocking(
            targets,
            8,
            |(repo_name, repo_path)| {
                match check_dirty(&repo_path) {
                    Ok((branch, has_staged, has_changes, has_untracked)) => {
                        (repo_name, branch, has_staged, has_changes, has_untracked, None)
                    }
                    Err(e) => (repo_name, String::new(), false, false, false, Some(e)),
                }
            },
        ).await;

        let mut dirty_count = 0usize;
        let mut clean_count = 0usize;
        let mut total = 0usize;

        // Collect and sort by name for deterministic output
        let mut entries: Vec<(String, String, bool, bool, bool, Option<String>)> = Vec::new();
        for result in results {
            match result {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Task error: {e}"),
                    };
                }
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        for (repo_name, branch, has_staged, has_changes, has_untracked, error) in entries {
            if let Some(e) = error {
                yield HyperforgeEvent::Error {
                    message: format!("{repo_name}: {e}"),
                };
                continue;
            }

            total += 1;
            let is_dirty = has_staged || has_changes || has_untracked;
            if is_dirty {
                dirty_count += 1;
                yield HyperforgeEvent::RepoDirty {
                    repo_name,
                    has_staged,
                    has_changes,
                    has_untracked,
                    branch,
                };
            } else {
                clean_count += 1;
            }
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "{dirty_count} dirty, {clean_count} clean ({total} repos checked)",
            ),
        };
    }
}

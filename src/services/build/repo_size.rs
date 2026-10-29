//! Workspace-level repo size reporting.

use async_stream::stream;
use futures::Stream;
use std::path::PathBuf;

use crate::commands::runner::discover_or_bail;
use crate::service::HyperforgeEvent;
use crate::services::utils::RepoFilter;

/// Measure total tracked-file size for a single repo.
fn measure_repo(repo_path: &std::path::Path) -> Result<(usize, u64), String> {
    let output = std::process::Command::new("git")
        .args(["ls-files"])
        .current_dir(repo_path)
        .output()
        .map_err(|e| format!("git ls-files failed: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
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

    Ok((tracked_files, total_bytes))
}

pub fn repo_sizes(
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

        let repos: Vec<_> = ctx.repos.iter()
            .filter(|r| filter.matches(&r.dir_name))
            .collect();

        if repos.is_empty() {
            yield HyperforgeEvent::Info {
                message: "No repos matched filter.".to_string(),
            };
            return;
        }

        let inputs: Vec<(String, PathBuf)> = repos.iter()
            .map(|r| (r.dir_name.clone(), r.path.clone()))
            .collect();

        let results = crate::commands::runner::run_batch_blocking(
            inputs,
            8,
            |(repo_name, repo_path)| {
                match measure_repo(&repo_path) {
                    Ok((tracked_files, total_bytes)) => (repo_name, tracked_files, total_bytes, None),
                    Err(e) => (repo_name, 0, 0, Some(e)),
                }
            },
        ).await;

        let mut entries: Vec<(String, usize, u64)> = Vec::new();

        for result in results {
            match result {
                Ok((repo_name, tracked_files, total_bytes, error)) => {
                    if let Some(e) = error {
                        yield HyperforgeEvent::Error {
                            message: format!("{repo_name}: {e}"),
                        };
                    } else {
                        entries.push((repo_name, tracked_files, total_bytes));
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Task error: {e}"),
                    };
                }
            }
        }

        // Sort by size descending
        entries.sort_by(|a, b| b.2.cmp(&a.2));

        let mut workspace_total: u64 = 0;
        let mut workspace_files: usize = 0;

        for (repo_name, tracked_files, total_bytes) in &entries {
            workspace_total += total_bytes;
            workspace_files += tracked_files;
            yield HyperforgeEvent::RepoSize {
                repo_name: repo_name.clone(),
                tracked_files: *tracked_files,
                total_bytes: *total_bytes,
            };
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "Workspace total: {} files, {:.1}MB across {} repos",
                workspace_files,
                workspace_total as f64 / (1024.0 * 1024.0),
                entries.len(),
            ),
        };
    }
}

//! Large file detection: find oversized files in tracked tree and git history.

use async_stream::stream;
use futures::Stream;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::commands::runner::discover_or_bail;
use crate::service::HyperforgeEvent;
use crate::services::utils::RepoFilter;

/// A large file found in a repo.
pub struct LargeFileEntry {
    pub path: String,
    pub size: u64,
    pub history_only: bool,
}

/// Scan a single repo for large files in both the working tree and git history.
/// Returns entries sorted by size descending.
pub fn scan_repo(repo_path: &std::path::Path, threshold: u64) -> Result<Vec<LargeFileEntry>, String> {
    let mut results: Vec<LargeFileEntry> = Vec::new();
    let mut tracked_paths: HashSet<String> = HashSet::new();

    // Phase 1: Currently tracked files (git ls-files + stat)
    let output = std::process::Command::new("git")
        .args(["ls-files"])
        .current_dir(repo_path)
        .output()
        .map_err(|e| format!("git ls-files failed: {e}"))?;

    if output.status.success() {
        let files_str = String::from_utf8_lossy(&output.stdout);
        for line in files_str.lines() {
            if line.is_empty() {
                continue;
            }
            tracked_paths.insert(line.to_string());
            let full_path = repo_path.join(line);
            if let Ok(meta) = std::fs::metadata(&full_path) {
                let size = meta.len();
                if size >= threshold {
                    results.push(LargeFileEntry {
                        path: line.to_string(),
                        size,
                        history_only: false,
                    });
                }
            }
        }
    }

    // Phase 2: Git history blobs (catches deleted large files still in pack)
    let history_output = std::process::Command::new("sh")
        .args([
            "-c",
            "git rev-list --objects --all | git cat-file --batch-check='%(objecttype) %(objectname) %(objectsize) %(rest)'"
        ])
        .current_dir(repo_path)
        .output();

    if let Ok(output) = history_output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let mut history_max: HashMap<String, u64> = HashMap::new();

            for line in stdout.lines() {
                let parts: Vec<&str> = line.splitn(4, ' ').collect();
                if parts.len() >= 4 && parts[0] == "blob" {
                    if let Ok(size) = parts[2].parse::<u64>() {
                        let path = parts[3];
                        if size >= threshold && !tracked_paths.contains(path) {
                            let entry = history_max.entry(path.to_string()).or_insert(0);
                            if size > *entry {
                                *entry = size;
                            }
                        }
                    }
                }
            }

            for (path, size) in history_max {
                results.push(LargeFileEntry {
                    path,
                    size,
                    history_only: true,
                });
            }
        }
    }

    results.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(results)
}

pub fn large_files(
    path: String,
    threshold_kb: Option<u64>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
) -> impl Stream<Item = HyperforgeEvent> + Send + 'static {
    let filter = RepoFilter::new(include, exclude);
    let threshold = threshold_kb.unwrap_or(100) * 1024;

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

        yield HyperforgeEvent::Info {
            message: format!(
                "Scanning {} repos for files over {}KB (tracked + history)...",
                repos.len(),
                threshold / 1024,
            ),
        };

        let inputs: Vec<(String, PathBuf, u64)> = repos.iter()
            .map(|r| (r.dir_name.clone(), r.path.clone(), threshold))
            .collect();

        let results = crate::commands::runner::run_batch_blocking(
            inputs,
            8,
            |(repo_name, repo_path, thresh)| {
                match scan_repo(&repo_path, thresh) {
                    Ok(entries) => (repo_name, entries, None),
                    Err(e) => (repo_name, Vec::new(), Some(e)),
                }
            },
        ).await;

        let mut total_large = 0usize;
        let mut total_history = 0usize;
        let mut repos_with_large = 0usize;

        for result in results {
            match result {
                Ok((repo_name, entries, error)) => {
                    if let Some(e) = error {
                        yield HyperforgeEvent::Error {
                            message: format!("{repo_name}: {e}"),
                        };
                        continue;
                    }
                    if !entries.is_empty() {
                        repos_with_large += 1;
                        for entry in &entries {
                            total_large += 1;
                            if entry.history_only {
                                total_history += 1;
                            }
                            yield HyperforgeEvent::LargeFile {
                                repo_name: repo_name.clone(),
                                file_path: entry.path.clone(),
                                size_bytes: entry.size,
                                history_only: entry.history_only,
                            };
                        }
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Task error: {e}"),
                    };
                }
            }
        }

        yield HyperforgeEvent::Info {
            message: format!(
                "Scan complete: {} large file(s) across {} repo(s) ({} tracked, {} history-only, threshold: {}KB)",
                total_large,
                repos_with_large,
                total_large - total_history,
                total_history,
                threshold / 1024,
            ),
        };
    }
}

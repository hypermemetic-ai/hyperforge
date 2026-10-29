//! Workspace-level lines-of-code counting.

use async_stream::stream;
use futures::Stream;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::PathBuf;

use crate::commands::runner::discover_or_bail;
use crate::service::HyperforgeEvent;
use crate::services::utils::RepoFilter;

/// Count lines of code for a single repo, broken down by file extension.
fn measure_loc(repo_path: &std::path::Path) -> Result<(usize, usize, HashMap<String, usize>), String> {
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
    let mut total_lines: usize = 0;
    let mut total_files: usize = 0;
    let mut by_extension: HashMap<String, usize> = HashMap::new();

    for line in files_str.lines() {
        if line.is_empty() {
            continue;
        }
        let full_path = repo_path.join(line);
        let ext = std::path::Path::new(line)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("(none)")
            .to_string();

        if let Ok(file) = std::fs::File::open(&full_path) {
            let reader = std::io::BufReader::new(file);
            let count = reader.lines().count();
            total_lines += count;
            total_files += 1;
            *by_extension.entry(ext).or_insert(0) += count;
        }
    }

    Ok((total_lines, total_files, by_extension))
}

pub fn loc(
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
                match measure_loc(&repo_path) {
                    Ok((total_lines, total_files, by_extension)) =>
                        (repo_name, total_lines, total_files, by_extension, None),
                    Err(e) =>
                        (repo_name, 0, 0, HashMap::new(), Some(e)),
                }
            },
        ).await;

        let mut entries: Vec<(String, usize, usize, HashMap<String, usize>)> = Vec::new();

        for result in results {
            match result {
                Ok((repo_name, total_lines, total_files, by_extension, error)) => {
                    if let Some(e) = error {
                        yield HyperforgeEvent::Error {
                            message: format!("{repo_name}: {e}"),
                        };
                    } else {
                        entries.push((repo_name, total_lines, total_files, by_extension));
                    }
                }
                Err(e) => {
                    yield HyperforgeEvent::Error {
                        message: format!("Task error: {e}"),
                    };
                }
            }
        }

        // Sort by total lines descending
        entries.sort_by(|a, b| b.1.cmp(&a.1));

        let mut workspace_lines: usize = 0;
        let mut workspace_files: usize = 0;
        let mut workspace_ext: HashMap<String, usize> = HashMap::new();

        for (repo_name, total_lines, total_files, by_extension) in &entries {
            workspace_lines += total_lines;
            workspace_files += total_files;
            for (ext, count) in by_extension {
                *workspace_ext.entry(ext.clone()).or_insert(0) += count;
            }
            yield HyperforgeEvent::RepoLoc {
                repo_name: repo_name.clone(),
                total_lines: *total_lines,
                total_files: *total_files,
                by_extension: by_extension.clone(),
            };
        }

        // Top extensions summary
        let mut ext_list: Vec<_> = workspace_ext.into_iter().collect();
        ext_list.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = ext_list.iter().take(5)
            .map(|(ext, lines)| format!(".{ext}: {lines}"))
            .collect();

        yield HyperforgeEvent::Info {
            message: format!(
                "Workspace total: {} lines across {} files in {} repos. Top: {}",
                workspace_lines,
                workspace_files,
                entries.len(),
                top.join(", "),
            ),
        };
    }
}

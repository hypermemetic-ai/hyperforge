//! Push command - Push to all configured forges
//!
//! `hyperforge push --path .`
//!
//! This command:
//! 1. Reads the hyperforge config
//! 2. Pushes the current branch to all configured forges
//! 3. Reports success/failure for each forge
//! 4. Retries transient SSH/network failures automatically
//! 5. Continues to remaining forges even if one fails

use std::path::Path;
use thiserror::Error;

use crate::config::HyperforgeConfig;
use crate::git::{Git, GitError};

/// Default large file threshold (500KB)
const LARGE_FILE_THRESHOLD: u64 = 500 * 1024;

/// Errors that can occur during push
#[derive(Debug, Error)]
pub enum PushError {
    #[error("Not a hyperforge repository. Run 'hyperforge init' first.")]
    NotInitialized,

    #[error("Not a git repository: {path}")]
    NotAGitRepo { path: String },

    #[error("Push to {forge} ({remote}) failed: {message}")]
    PushFailed {
        forge: String,
        remote: String,
        message: String,
    },

    #[error("Remote not found: {remote} (forge: {forge})")]
    RemoteNotFound { forge: String, remote: String },

    #[error("Git error: {0}")]
    GitError(#[from] GitError),

    #[error("Config error: {0}")]
    ConfigError(#[from] crate::config::ConfigError),

    #[error("No branch to push. Create a commit first.")]
    NoBranch,

    #[error("Large files detected — push blocked:\n{details}")]
    LargeFilesBlocked { details: String },
}

pub type PushResult<T> = Result<T, PushError>;

/// Options for the push command
#[derive(Debug, Clone, Default)]
pub struct PushOptions {
    /// Set upstream tracking when pushing
    pub set_upstream: bool,

    /// Dry run - don't actually push
    pub dry_run: bool,

    /// Force push (use with caution)
    pub force: bool,

    /// Only push to specific forges (empty = all)
    pub only_forges: Vec<String>,

    /// Override branch to push (None = current branch)
    pub branch: Option<String>,
}

impl PushOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn set_upstream(mut self) -> Self {
        self.set_upstream = true;
        self
    }

    pub const fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    pub const fn force(mut self) -> Self {
        self.force = true;
        self
    }

    pub fn only(mut self, forges: Vec<String>) -> Self {
        self.only_forges = forges;
        self
    }

    pub fn with_branch(mut self, branch: impl Into<String>) -> Self {
        self.branch = Some(branch.into());
        self
    }
}

/// Result of pushing to a single forge
#[derive(Debug, Clone)]
pub struct ForgePushResult {
    /// Forge name
    pub forge: String,

    /// Git remote name
    pub remote_name: String,

    /// Branch that was pushed
    pub branch: String,

    /// Whether push succeeded
    pub success: bool,

    /// Error message if failed
    pub error: Option<String>,

    /// Whether this was a dry run
    pub dry_run: bool,
}

/// Overall push report
#[derive(Debug)]
pub struct PushReport {
    /// Path to the repository
    pub repo_path: String,

    /// Branch that was pushed
    pub branch: String,

    /// Results for each forge
    pub results: Vec<ForgePushResult>,

    /// Whether all pushes succeeded
    pub all_success: bool,

    /// Whether this was a dry run
    pub dry_run: bool,
}

impl PushReport {
    /// Format as human-readable string
    pub fn format(&self) -> String {
        let mut lines = Vec::new();

        if self.dry_run {
            lines.push("Dry run - no changes made".to_string());
            lines.push(String::new());
        }

        lines.push(format!("Pushing {} from {}", self.branch, self.repo_path));
        lines.push(String::new());

        for result in &self.results {
            let symbol = if result.success { "✓" } else { "✗" };
            let status = if result.success {
                "pushed".to_string()
            } else {
                result.error.clone().unwrap_or_else(|| "failed".to_string())
            };

            lines.push(format!(
                "  {} {} ({}): {}",
                symbol, result.forge, result.remote_name, status
            ));
        }

        if self.all_success {
            lines.push(String::new());
            lines.push("All forges pushed successfully.".to_string());
        }

        lines.join("\n")
    }
}

/// Push to all configured forges
///
/// # Arguments
/// * `path` - Path to the repository
/// * `options` - Push options
///
/// # Returns
/// `PushReport` with results for each forge
///
/// # Behavior
/// Attempts all configured forges even if one fails. This ensures that a
/// transient failure on one forge (e.g. SSH connection reset on Codeberg)
/// doesn't prevent pushing to other forges (e.g. GitHub).
pub fn push(path: &Path, options: PushOptions) -> PushResult<PushReport> {
    // Check if hyperforge config exists
    if !HyperforgeConfig::exists(path) {
        return Err(PushError::NotInitialized);
    }

    // Check if it's a git repo
    if !Git::is_repo(path) {
        return Err(PushError::NotAGitRepo {
            path: path.display().to_string(),
        });
    }

    // Load config
    let config = HyperforgeConfig::load(path)?;

    // Get branch — use override if specified, otherwise current branch
    let branch = if let Some(ref b) = options.branch {
        b.clone()
    } else {
        let b = Git::current_branch(path)?;
        if b.is_empty() {
            return Err(PushError::NoBranch);
        }
        b
    };

    // Check for large tracked files before pushing
    // Use per-repo threshold from config, falling back to default (100KB).
    // Set to 0 in config to disable the guard entirely.
    let threshold = config
        .large_file_threshold_kb
        .map_or(LARGE_FILE_THRESHOLD, |kb| kb * 1024);
    if !options.dry_run && threshold > 0 {
        if let Ok(entries) = crate::services::build::large_files::scan_repo(path, threshold)
        {
            let blocked: Vec<_> = entries
                .iter()
                .filter(|e| !e.history_only)
                .collect();
            if !blocked.is_empty() {
                let details = blocked
                    .iter()
                    .map(|e| {
                        format!(
                            "  {} ({:.1}KB)",
                            e.path,
                            e.size as f64 / 1024.0,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(PushError::LargeFilesBlocked { details });
            }
        }
    }

    // Determine which forges to push to
    let forges_to_push: Vec<&String> = if options.only_forges.is_empty() {
        config.forges.iter().collect()
    } else {
        config
            .forges
            .iter()
            .filter(|f| options.only_forges.contains(f))
            .collect()
    };

    let mut results = Vec::new();

    for forge in forges_to_push {
        let remote_name = config.remote_for_forge(forge);

        // Check if remote exists
        if Git::get_remote(path, &remote_name).is_err() {
            results.push(ForgePushResult {
                forge: forge.clone(),
                remote_name: remote_name.clone(),
                branch: branch.clone(),
                success: false,
                error: Some(format!("Remote not found: {remote_name}")),
                dry_run: options.dry_run,
            });
            continue;
        }

        let mut result = ForgePushResult {
            forge: forge.clone(),
            remote_name: remote_name.clone(),
            branch: branch.clone(),
            success: true,
            error: None,
            dry_run: options.dry_run,
        };

        if !options.dry_run {
            // Perform the actual push (Git::push already retries transient errors)
            let push_result = if options.set_upstream {
                Git::push_set_upstream(path, &remote_name, &branch)
            } else if options.force {
                // Force push — also use retry via push_with_retry pattern
                crate::git::force_push_with_retry(path, &remote_name, &branch)
            } else {
                Git::push(path, &remote_name, Some(&branch))
            };

            match push_result {
                Ok(()) => {
                    result.success = true;
                }
                Err(e) => {
                    result.success = false;
                    result.error = Some(e.to_string());
                }
            }
        }

        results.push(result);
    }

    let all_success = results.iter().all(|r| r.success);

    Ok(PushReport {
        repo_path: path.display().to_string(),
        branch,
        results,
        all_success,
        dry_run: options.dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::init::{init, InitOptions};
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn setup_repo_with_commit(path: &Path) {
        // Configure git user
        Git::config_set(path, "user.email", "test@test.com").unwrap();
        Git::config_set(path, "user.name", "Test").unwrap();

        // Create initial commit
        fs::write(path.join("README.md"), "# Test").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(path)
            .output()
            .unwrap();
    }

    #[test]
    fn test_push_not_initialized() {
        let temp = TempDir::new().unwrap();
        Git::init(temp.path()).unwrap();

        let result = push(temp.path(), PushOptions::new());
        assert!(matches!(result, Err(PushError::NotInitialized)));
    }

    #[test]
    fn test_push_no_commits() {
        let temp = TempDir::new().unwrap();

        // Initialize but don't create any commits
        let options = InitOptions::new(vec!["github".to_string()])
            .with_org("alice");
        init(temp.path(), options).unwrap();

        // In a fresh repo with no commits, pushing will fail
        // because there's nothing to push. This tests error handling.
        // Dry run should still work though.
        let result = push(temp.path(), PushOptions::new().dry_run());
        // Dry run succeeds even without commits (we just report what we would do)
        assert!(result.is_ok());
    }

    #[test]
    fn test_push_dry_run() {
        let temp = TempDir::new().unwrap();

        let options = InitOptions::new(vec!["github".to_string(), "codeberg".to_string()])
            .with_org("alice");
        init(temp.path(), options).unwrap();
        setup_repo_with_commit(temp.path());

        // Dry run should succeed
        let report = push(temp.path(), PushOptions::new().dry_run()).unwrap();

        assert!(report.dry_run);
        assert_eq!(report.results.len(), 2);
        assert!(report.all_success);

        // All results should be marked as dry run
        for result in &report.results {
            assert!(result.dry_run);
            assert!(result.success);
        }
    }

    #[test]
    fn test_push_only_specific_forges() {
        let temp = TempDir::new().unwrap();

        let options = InitOptions::new(vec!["github".to_string(), "codeberg".to_string()])
            .with_org("alice");
        init(temp.path(), options).unwrap();
        setup_repo_with_commit(temp.path());

        // Only push to github
        let report = push(
            temp.path(),
            PushOptions::new()
                .dry_run()
                .only(vec!["github".to_string()]),
        )
        .unwrap();

        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].forge, "github");
    }

    #[test]
    fn test_push_report_format() {
        let report = PushReport {
            repo_path: "/test/repo".to_string(),
            branch: "main".to_string(),
            results: vec![
                ForgePushResult {
                    forge: "github".to_string(),
                    remote_name: "origin".to_string(),
                    branch: "main".to_string(),
                    success: true,
                    error: None,
                    dry_run: false,
                },
                ForgePushResult {
                    forge: "codeberg".to_string(),
                    remote_name: "codeberg".to_string(),
                    branch: "main".to_string(),
                    success: true,
                    error: None,
                    dry_run: false,
                },
            ],
            all_success: true,
            dry_run: false,
        };

        let formatted = report.format();
        assert!(formatted.contains("Pushing main from /test/repo"));
        assert!(formatted.contains("✓ github"));
        assert!(formatted.contains("✓ codeberg"));
        assert!(formatted.contains("All forges pushed successfully"));
    }

    #[test]
    fn test_push_options_builder() {
        let options = PushOptions::new()
            .set_upstream()
            .dry_run()
            .force()
            .only(vec!["github".to_string()]);

        assert!(options.set_upstream);
        assert!(options.dry_run);
        assert!(options.force);
        assert_eq!(options.only_forges, vec!["github"]);
    }

    #[test]
    fn test_push_remote_not_found() {
        let temp = TempDir::new().unwrap();

        // Initialize hyperforge
        let options = InitOptions::new(vec!["github".to_string()])
            .with_org("alice");
        init(temp.path(), options).unwrap();
        setup_repo_with_commit(temp.path());

        // Remove the remote
        Git::remove_remote(temp.path(), "origin").unwrap();

        // Push returns Ok but with failed result for the missing remote
        let report = push(temp.path(), PushOptions::new()).unwrap();
        assert!(!report.all_success);
        assert_eq!(report.results.len(), 1);
        assert!(!report.results[0].success);
        assert!(report.results[0].error.as_ref().unwrap().contains("Remote not found"));
    }
}

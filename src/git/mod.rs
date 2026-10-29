//! Git operations and remote management
//!
//! This module provides git command execution and parsing for hyperforge.
//! It uses git as the source of truth for repository state.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during git operations
#[derive(Debug, Error)]
pub enum GitError {
    #[error("Git command failed: {message}")]
    CommandFailed { message: String },

    #[error("Not a git repository: {path}")]
    NotARepo { path: String },

    #[error("Remote not found: {name}")]
    RemoteNotFound { name: String },

    #[error("Remote already exists: {name}")]
    RemoteAlreadyExists { name: String },

    #[error("Failed to parse git output: {message}")]
    ParseError { message: String },

    #[error("Git not installed or not in PATH")]
    GitNotFound,

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

pub type GitResult<T> = Result<T, GitError>;

/// Build an error message from a failed command's stderr and stdout.
/// Includes stdout when non-empty (e.g. pre-push hook output).
pub(crate) fn command_error_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = stderr.trim();
    let stdout = stdout.trim();
    if stdout.is_empty() {
        stderr.to_string()
    } else if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("{stderr}\n{stdout}")
    }
}

/// Information about a git remote
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
}

/// Branch tracking information
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchStatus {
    pub name: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// Repository status
#[derive(Debug, Clone)]
pub struct RepoStatus {
    pub branch: String,
    pub tracking: Option<BranchStatus>,
    pub has_changes: bool,
    pub has_staged: bool,
    pub has_untracked: bool,
}

/// Maximum number of retries for transient push errors
const PUSH_MAX_RETRIES: u32 = 3;

/// Base delay between retries (doubles each attempt)
const PUSH_RETRY_BASE_MS: u64 = 1000;

/// Check if a git push error message indicates a transient network/SSH failure
/// that may succeed on retry.
fn is_transient_push_error(msg: &str) -> bool {
    let msg_lower = msg.to_lowercase();
    msg_lower.contains("connection reset by peer")
        || msg_lower.contains("connection timed out")
        || msg_lower.contains("connection refused")
        || msg_lower.contains("broken pipe")
        || msg_lower.contains("kex_exchange_identification")
        || msg_lower.contains("ssh_msg_disconnect")
        || msg_lower.contains("could not read from remote repository")
        || msg_lower.contains("the remote end hung up unexpectedly")
        || msg_lower.contains("ssl_error_syscall")
        || msg_lower.contains("network is unreachable")
}

/// Run a git push command with retry logic for transient failures.
/// Retries up to `PUSH_MAX_RETRIES` times with exponential backoff.
fn push_with_retry<F>(mut attempt: F) -> GitResult<()>
where
    F: FnMut() -> GitResult<()>,
{
    let mut last_err = None;

    for retry in 0..=PUSH_MAX_RETRIES {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if retry < PUSH_MAX_RETRIES && is_transient_push_error(&msg) {
                    let delay = std::time::Duration::from_millis(
                        PUSH_RETRY_BASE_MS * 2u64.pow(retry),
                    );
                    std::thread::sleep(delay);
                    last_err = Some(e);
                } else {
                    return Err(e);
                }
            }
        }
    }

    Err(last_err.unwrap())
}

/// Git operations helper
pub struct Git;

impl Git {
    /// Check if a path is a git repository
    pub fn is_repo(path: &Path) -> bool {
        path.join(".git").exists() || path.join(".git").is_file()
    }

    /// Initialize a new git repository
    pub fn init(path: &Path) -> GitResult<()> {
        let output = Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// Clone a git repository
    pub fn clone(url: &str, target_path: &str) -> GitResult<()> {
        let output = Command::new("git")
            .args(["clone", url, target_path])
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// List all remotes with their URLs
    pub fn list_remotes(path: &Path) -> GitResult<Vec<RemoteInfo>> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["remote", "-v"])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(Self::parse_remotes(&stdout))
    }

    /// Get a specific remote's info
    pub fn get_remote(path: &Path, name: &str) -> GitResult<RemoteInfo> {
        let remotes = Self::list_remotes(path)?;
        remotes
            .into_iter()
            .find(|r| r.name == name)
            .ok_or_else(|| GitError::RemoteNotFound {
                name: name.to_string(),
            })
    }

    /// Add a new remote
    pub fn add_remote(path: &Path, name: &str, url: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["remote", "add", name, url])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("already exists") {
                return Err(GitError::RemoteAlreadyExists {
                    name: name.to_string(),
                });
            }
            return Err(GitError::CommandFailed {
                message: stderr.to_string(),
            });
        }

        Ok(())
    }

    /// Remove a remote
    pub fn remove_remote(path: &Path, name: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["remote", "remove", name])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No such remote") {
                return Err(GitError::RemoteNotFound {
                    name: name.to_string(),
                });
            }
            return Err(GitError::CommandFailed {
                message: stderr.to_string(),
            });
        }

        Ok(())
    }

    /// Set a remote's URL
    pub fn set_remote_url(path: &Path, name: &str, url: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["remote", "set-url", name, url])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No such remote") {
                return Err(GitError::RemoteNotFound {
                    name: name.to_string(),
                });
            }
            return Err(GitError::CommandFailed {
                message: stderr.to_string(),
            });
        }

        Ok(())
    }

    /// Configure SSH key for the repository using core.sshCommand
    pub fn configure_ssh(path: &Path, key_path: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        // Expand ~ in key path
        let expanded_path = if key_path.starts_with("~/") {
            dirs::home_dir().map_or_else(|| key_path.into(), |h| h.join(&key_path[2..]))
        } else {
            key_path.into()
        };

        let ssh_command = format!(
            "ssh -i {} -o IdentitiesOnly=yes",
            expanded_path.display()
        );

        let output = Command::new("git")
            .args(["config", "core.sshCommand", &ssh_command])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// Get current branch name
    pub fn current_branch(path: &Path) -> GitResult<String> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Get branch tracking status (ahead/behind)
    pub fn branch_status(path: &Path) -> GitResult<Option<BranchStatus>> {
        Self::ensure_repo(path)?;

        // Get verbose branch info
        let output = Command::new("git")
            .args(["branch", "-vv", "--no-color"])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(Self::parse_branch_status(&stdout))
    }

    /// Get repository status (changes, staged, untracked)
    pub fn repo_status(path: &Path) -> GitResult<RepoStatus> {
        Self::ensure_repo(path)?;

        let branch = Self::current_branch(path)?;
        let tracking = Self::branch_status(path)?;

        // Get porcelain status for parsing
        let output = Command::new("git")
            .args(["status", "--porcelain=v1"])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut has_changes = false;
        let mut has_staged = false;
        let mut has_untracked = false;

        for line in stdout.lines() {
            if line.len() < 2 {
                continue;
            }
            let index_status = line.chars().next().unwrap_or(' ');
            let worktree_status = line.chars().nth(1).unwrap_or(' ');

            if index_status == '?' {
                has_untracked = true;
            } else {
                if index_status != ' ' {
                    has_staged = true;
                }
                if worktree_status != ' ' {
                    has_changes = true;
                }
            }
        }

        Ok(RepoStatus {
            branch,
            tracking,
            has_changes,
            has_staged,
            has_untracked,
        })
    }

    /// Push to a remote (retries transient SSH/network failures)
    pub fn push(path: &Path, remote: &str, branch: Option<&str>) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let path = path.to_path_buf();
        let remote = remote.to_string();
        let branch = branch.map(std::string::ToString::to_string);

        push_with_retry(move || {
            let mut args = vec!["push".to_string(), remote.clone()];
            if let Some(ref b) = branch {
                args.push(b.clone());
            }

            let output = Command::new("git")
                .args(&args)
                .current_dir(&path)
                .output()?;

            if !output.status.success() {
                return Err(GitError::CommandFailed {
                    message: command_error_message(&output),
                });
            }

            Ok(())
        })
    }

    /// Push with upstream tracking (retries transient SSH/network failures)
    pub fn push_set_upstream(path: &Path, remote: &str, branch: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let path = path.to_path_buf();
        let remote = remote.to_string();
        let branch = branch.to_string();

        push_with_retry(move || {
            let output = Command::new("git")
                .args(["push", "-u", &remote, &branch])
                .current_dir(&path)
                .output()?;

            if !output.status.success() {
                return Err(GitError::CommandFailed {
                    message: command_error_message(&output),
                });
            }

            Ok(())
        })
    }

    /// Pull from a remote
    pub fn pull(path: &Path, remote: Option<&str>) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let mut args = vec!["pull"];
        if let Some(r) = remote {
            args.push(r);
        }

        let output = Command::new("git")
            .args(&args)
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// Checkout a branch
    pub fn checkout(path: &Path, branch: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["checkout", branch])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// Fetch from all remotes
    pub fn fetch_all(path: &Path) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["fetch", "--all"])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// Fetch from a specific remote
    pub fn fetch(path: &Path, remote: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["fetch", remote])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// Get ahead/behind count for a specific remote
    pub fn ahead_behind(path: &Path, remote: &str, branch: &str) -> GitResult<(u32, u32)> {
        Self::ensure_repo(path)?;

        let upstream = format!("{remote}/{branch}");
        let output = Command::new("git")
            .args(["rev-list", "--left-right", "--count", &format!("{upstream}...HEAD")])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            // Remote branch might not exist yet
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("unknown revision") {
                return Ok((0, 0));
            }
            return Err(GitError::CommandFailed {
                message: stderr.to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parts: Vec<&str> = stdout.split_whitespace().collect();
        if parts.len() != 2 {
            return Ok((0, 0));
        }

        let behind = parts[0].parse().unwrap_or(0);
        let ahead = parts[1].parse().unwrap_or(0);

        Ok((ahead, behind))
    }

    /// Get git config value
    pub fn config_get(path: &Path, key: &str) -> GitResult<Option<String>> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["config", "--get", key])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            // Key not found is not an error
            return Ok(None);
        }

        Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
    }

    /// Set git config value
    pub fn config_set(path: &Path, key: &str, value: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["config", key, value])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).to_string(),
            });
        }

        Ok(())
    }

    /// Stage a file for commit
    pub fn add(path: &Path, file: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["add", file])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: command_error_message(&output),
            });
        }

        Ok(())
    }

    /// Create a commit with the given message
    pub fn commit(path: &Path, message: &str) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: command_error_message(&output),
            });
        }

        Ok(())
    }

    /// Check if a tag exists in the repo.
    pub fn tag_exists(path: &Path, tag: &str) -> bool {
        Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/tags/{tag}")])
            .current_dir(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Count commits since a tag.
    /// Returns 0 if tag is at HEAD. Errors if tag doesn't exist.
    pub fn commits_since_tag(path: &Path, tag: &str) -> GitResult<usize> {
        let output = Command::new("git")
            .args(["rev-list", &format!("{tag}..HEAD"), "--count"])
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<usize>()
            .map_err(|e| GitError::ParseError {
                message: format!("failed to parse commit count: {e}"),
            })
    }

    /// Count commits since a tag that touch any of the given files.
    ///
    /// More precise than `commits_since_tag` — only counts commits where
    /// at least one of the specified paths was modified.
    pub fn commits_touching_paths_since_tag(
        path: &Path,
        tag: &str,
        files: &[String],
    ) -> GitResult<usize> {
        let mut args = vec![
            "rev-list".to_string(),
            "--count".to_string(),
            format!("{}..HEAD", tag),
            "--".to_string(),
        ];
        args.extend(files.iter().cloned());

        let output = Command::new("git")
            .args(&args)
            .current_dir(path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<usize>()
            .map_err(|e| GitError::ParseError {
                message: format!("failed to parse commit count: {e}"),
            })
    }

    /// Create an annotated or lightweight tag
    pub fn tag(path: &Path, name: &str, message: Option<&str>) -> GitResult<()> {
        Self::ensure_repo(path)?;

        let output = if let Some(msg) = message {
            Command::new("git")
                .args(["tag", "-a", name, "-m", msg])
                .current_dir(path)
                .output()?
        } else {
            Command::new("git")
                .args(["tag", name])
                .current_dir(path)
                .output()?
        };

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: command_error_message(&output),
            });
        }

        Ok(())
    }

    // --- Private helper methods ---

    fn ensure_repo(path: &Path) -> GitResult<()> {
        if !Self::is_repo(path) {
            return Err(GitError::NotARepo {
                path: path.display().to_string(),
            });
        }
        Ok(())
    }

    fn parse_remotes(output: &str) -> Vec<RemoteInfo> {
        let mut remotes: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();

        for line in output.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                continue;
            }

            let name = parts[0].to_string();
            let url = parts[1].to_string();
            let kind = parts[2]; // (fetch) or (push)

            let entry = remotes.entry(name).or_insert((None, None));
            if kind.contains("fetch") {
                entry.0 = Some(url);
            } else if kind.contains("push") {
                entry.1 = Some(url);
            }
        }

        remotes
            .into_iter()
            .filter_map(|(name, (fetch, push))| {
                let fetch_url = fetch?;
                let push_url = push.unwrap_or_else(|| fetch_url.clone());
                Some(RemoteInfo {
                    name,
                    fetch_url,
                    push_url,
                })
            })
            .collect()
    }

    fn parse_branch_status(output: &str) -> Option<BranchStatus> {
        // Find the current branch (marked with *)
        for line in output.lines() {
            if !line.starts_with('*') {
                continue;
            }

            // Format: * branch_name hash [upstream: ahead N, behind M] commit message
            let line = line.trim_start_matches('*').trim();
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                continue;
            }

            let branch_name = parts[0].to_string();

            // Look for tracking info in brackets
            let mut upstream = None;
            let mut ahead = 0u32;
            let mut behind = 0u32;

            if let Some(start) = line.find('[') {
                if let Some(end) = line.find(']') {
                    let tracking_info = &line[start + 1..end];

                    // Parse upstream name (before :)
                    if let Some(colon_pos) = tracking_info.find(':') {
                        upstream = Some(tracking_info[..colon_pos].to_string());
                        let status_part = &tracking_info[colon_pos + 1..];

                        // Parse ahead/behind
                        if status_part.contains("ahead") {
                            if let Some(n) = Self::extract_number(status_part, "ahead") {
                                ahead = n;
                            }
                        }
                        if status_part.contains("behind") {
                            if let Some(n) = Self::extract_number(status_part, "behind") {
                                behind = n;
                            }
                        }
                    } else {
                        // No colon means just the upstream name (up to date)
                        upstream = Some(tracking_info.trim().to_string());
                    }
                }
            }

            return Some(BranchStatus {
                name: branch_name,
                upstream,
                ahead,
                behind,
            });
        }

        None
    }

    fn extract_number(s: &str, prefix: &str) -> Option<u32> {
        let idx = s.find(prefix)?;
        let rest = &s[idx + prefix.len()..];
        // Take only consecutive digits after skipping whitespace
        let num_str: String = rest
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        num_str.parse().ok()
    }
}

/// Force push with retry logic for transient SSH/network failures.
pub fn force_push_with_retry(path: &Path, remote: &str, branch: &str) -> GitResult<()> {
    let path = path.to_path_buf();
    let remote = remote.to_string();
    let branch = branch.to_string();

    push_with_retry(move || {
        let output = Command::new("git")
            .args(["push", "--force", &remote, &branch])
            .current_dir(&path)
            .output()?;

        if !output.status.success() {
            return Err(GitError::CommandFailed {
                message: command_error_message(&output),
            });
        }

        Ok(())
    })
}

/// Git transport for remote URLs.
///
/// `Deserialize` + `JsonSchema` are present so Synapse can accept
/// `--transport ssh|https` as a closed-set enum flag: unknown values are
/// rejected at CLI parse time, before the call reaches the service.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Ssh,
    Https,
}

impl Transport {
    /// Default transport: reads `HYPERFORGE_TRANSPORT=https|ssh`, falls back to Ssh.
    pub fn from_env() -> Self {
        match std::env::var("HYPERFORGE_TRANSPORT").ok().as_deref() {
            Some(v) if v.eq_ignore_ascii_case("https") => Self::Https,
            _ => Self::Ssh,
        }
    }

    /// Lowercase string form used in events and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            Self::Https => "https",
        }
    }

    /// Classify a git remote URL by transport shape. Returns `None` for
    /// URLs that don't look like either SSH (`git@host:org/repo`) or
    /// HTTPS (`https://host/org/repo`). Callers that still need to
    /// compare treat `None` as "unrecognized — not equal to anything".
    pub fn detect(url: &str) -> Option<Self> {
        if url.starts_with("git@") {
            Some(Self::Ssh)
        } else if url.starts_with("https://") || url.starts_with("http://") {
            Some(Self::Https)
        } else {
            None
        }
    }
}

/// Build a git remote URL for a forge, picking transport from env (ssh by default).
pub fn build_remote_url(forge: &str, org: &str, repo: &str) -> String {
    build_remote_url_with(forge, org, repo, Transport::from_env())
}

/// Build a git remote URL for a forge with an explicit transport.
pub fn build_remote_url_with(forge: &str, org: &str, repo: &str, transport: Transport) -> String {
    let host = match forge.to_lowercase().as_str() {
        "github" => "github.com",
        "codeberg" => "codeberg.org",
        "gitlab" => "gitlab.com",
        other => return match transport {
            Transport::Ssh => format!("git@{other}:{org}/{repo}.git"),
            Transport::Https => format!("https://{other}/{org}/{repo}.git"),
        },
    };
    match transport {
        Transport::Ssh => format!("git@{host}:{org}/{repo}.git"),
        Transport::Https => format!("https://{host}/{org}/{repo}.git"),
    }
}

/// Extract org and repo from a git remote URL
pub fn parse_remote_url(url: &str) -> Option<(String, String, String)> {
    // SSH format: git@github.com:org/repo.git
    if url.starts_with("git@") {
        let rest = url.trim_start_matches("git@");
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() != 2 {
            return None;
        }

        let host = parts[0];
        let forge = match host {
            "github.com" => "github",
            "codeberg.org" => "codeberg",
            "gitlab.com" => "gitlab",
            _ => host,
        };

        let path = parts[1].trim_end_matches(".git");
        let path_parts: Vec<&str> = path.splitn(2, '/').collect();
        if path_parts.len() != 2 {
            return None;
        }

        return Some((forge.to_string(), path_parts[0].to_string(), path_parts[1].to_string()));
    }

    // HTTPS format: https://github.com/org/repo.git
    if url.starts_with("https://") || url.starts_with("http://") {
        let url = url.trim_start_matches("https://").trim_start_matches("http://");
        let parts: Vec<&str> = url.splitn(2, '/').collect();
        if parts.len() != 2 {
            return None;
        }

        let host = parts[0];
        let forge = match host {
            "github.com" => "github",
            "codeberg.org" => "codeberg",
            "gitlab.com" => "gitlab",
            _ => host,
        };

        let path = parts[1].trim_end_matches(".git");
        let path_parts: Vec<&str> = path.splitn(2, '/').collect();
        if path_parts.len() != 2 {
            return None;
        }

        return Some((forge.to_string(), path_parts[0].to_string(), path_parts[1].to_string()));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_build_remote_url_ssh() {
        assert_eq!(
            build_remote_url_with("github", "alice", "my-repo", Transport::Ssh),
            "git@github.com:alice/my-repo.git"
        );
        assert_eq!(
            build_remote_url_with("codeberg", "bob", "tool", Transport::Ssh),
            "git@codeberg.org:bob/tool.git"
        );
        assert_eq!(
            build_remote_url_with("gitlab", "org", "project", Transport::Ssh),
            "git@gitlab.com:org/project.git"
        );
    }

    #[test]
    fn test_build_remote_url_https() {
        assert_eq!(
            build_remote_url_with("github", "alice", "my-repo", Transport::Https),
            "https://github.com/alice/my-repo.git"
        );
        assert_eq!(
            build_remote_url_with("codeberg", "bob", "tool", Transport::Https),
            "https://codeberg.org/bob/tool.git"
        );
        assert_eq!(
            build_remote_url_with("gitlab", "org", "project", Transport::Https),
            "https://gitlab.com/org/project.git"
        );
    }

    #[test]
    fn test_parse_remote_url_ssh() {
        let result = parse_remote_url("git@github.com:alice/my-repo.git");
        assert_eq!(
            result,
            Some(("github".to_string(), "alice".to_string(), "my-repo".to_string()))
        );

        let result = parse_remote_url("git@codeberg.org:bob/tool.git");
        assert_eq!(
            result,
            Some(("codeberg".to_string(), "bob".to_string(), "tool".to_string()))
        );
    }

    #[test]
    fn test_parse_remote_url_https() {
        let result = parse_remote_url("https://github.com/alice/my-repo.git");
        assert_eq!(
            result,
            Some(("github".to_string(), "alice".to_string(), "my-repo".to_string()))
        );
    }

    #[test]
    fn test_parse_remotes() {
        let output = "origin\tgit@github.com:alice/repo.git (fetch)\n\
                      origin\tgit@github.com:alice/repo.git (push)\n\
                      upstream\tgit@github.com:bob/repo.git (fetch)\n\
                      upstream\tgit@github.com:bob/repo.git (push)";

        let remotes = Git::parse_remotes(output);
        assert_eq!(remotes.len(), 2);

        let origin = remotes.iter().find(|r| r.name == "origin").unwrap();
        assert_eq!(origin.fetch_url, "git@github.com:alice/repo.git");
    }

    #[test]
    fn test_parse_branch_status_with_tracking() {
        let output = "* main abc1234 [origin/main: ahead 2, behind 1] Latest commit";
        let status = Git::parse_branch_status(output).unwrap();

        assert_eq!(status.name, "main");
        assert_eq!(status.upstream, Some("origin/main".to_string()));
        assert_eq!(status.ahead, 2);
        assert_eq!(status.behind, 1);
    }

    #[test]
    fn test_parse_branch_status_up_to_date() {
        let output = "* main abc1234 [origin/main] Latest commit";
        let status = Git::parse_branch_status(output).unwrap();

        assert_eq!(status.name, "main");
        assert_eq!(status.upstream, Some("origin/main".to_string()));
        assert_eq!(status.ahead, 0);
        assert_eq!(status.behind, 0);
    }

    #[test]
    fn test_parse_branch_status_no_tracking() {
        let output = "* feature abc1234 Work in progress";
        let status = Git::parse_branch_status(output).unwrap();

        assert_eq!(status.name, "feature");
        assert_eq!(status.upstream, None);
    }

    #[test]
    fn test_is_repo_false() {
        let temp = TempDir::new().unwrap();
        assert!(!Git::is_repo(temp.path()));
    }

    #[test]
    fn test_init_and_is_repo() {
        let temp = TempDir::new().unwrap();
        Git::init(temp.path()).unwrap();
        assert!(Git::is_repo(temp.path()));
    }

    #[test]
    fn test_remote_operations() {
        let temp = TempDir::new().unwrap();
        Git::init(temp.path()).unwrap();

        // Add remote
        Git::add_remote(temp.path(), "origin", "git@github.com:test/repo.git").unwrap();

        // List remotes
        let remotes = Git::list_remotes(temp.path()).unwrap();
        assert_eq!(remotes.len(), 1);
        assert_eq!(remotes[0].name, "origin");

        // Get specific remote
        let origin = Git::get_remote(temp.path(), "origin").unwrap();
        assert_eq!(origin.fetch_url, "git@github.com:test/repo.git");

        // Set URL
        Git::set_remote_url(temp.path(), "origin", "git@github.com:test/new-repo.git").unwrap();
        let origin = Git::get_remote(temp.path(), "origin").unwrap();
        assert_eq!(origin.fetch_url, "git@github.com:test/new-repo.git");

        // Remove remote
        Git::remove_remote(temp.path(), "origin").unwrap();
        let remotes = Git::list_remotes(temp.path()).unwrap();
        assert!(remotes.is_empty());
    }

    #[test]
    fn test_add_remote_already_exists() {
        let temp = TempDir::new().unwrap();
        Git::init(temp.path()).unwrap();

        Git::add_remote(temp.path(), "origin", "git@github.com:test/repo.git").unwrap();
        let result = Git::add_remote(temp.path(), "origin", "git@github.com:test/other.git");

        assert!(matches!(result, Err(GitError::RemoteAlreadyExists { .. })));
    }

    #[test]
    fn test_remove_remote_not_found() {
        let temp = TempDir::new().unwrap();
        Git::init(temp.path()).unwrap();

        let result = Git::remove_remote(temp.path(), "nonexistent");
        assert!(matches!(result, Err(GitError::RemoteNotFound { .. })));
    }

    #[test]
    fn test_config_operations() {
        let temp = TempDir::new().unwrap();
        Git::init(temp.path()).unwrap();

        // Set config
        Git::config_set(temp.path(), "user.name", "Test User").unwrap();

        // Get config
        let value = Git::config_get(temp.path(), "user.name").unwrap();
        assert_eq!(value, Some("Test User".to_string()));

        // Get non-existent config
        let value = Git::config_get(temp.path(), "nonexistent.key").unwrap();
        assert_eq!(value, None);
    }

    #[test]
    fn test_current_branch() {
        let temp = TempDir::new().unwrap();
        Git::init(temp.path()).unwrap();

        // Configure git user for commit
        Git::config_set(temp.path(), "user.email", "test@test.com").unwrap();
        Git::config_set(temp.path(), "user.name", "Test").unwrap();

        // Create initial commit to have a branch
        fs::write(temp.path().join("README.md"), "# Test").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(temp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial"])
            .current_dir(temp.path())
            .output()
            .unwrap();

        let branch = Git::current_branch(temp.path()).unwrap();
        // Could be "main" or "master" depending on git config
        assert!(!branch.is_empty());
    }

    #[test]
    fn test_add_commit_tag() {
        let temp = TempDir::new().unwrap();
        Git::init(temp.path()).unwrap();
        Git::config_set(temp.path(), "user.email", "test@test.com").unwrap();
        Git::config_set(temp.path(), "user.name", "Test").unwrap();

        // Create a file and stage it
        fs::write(temp.path().join("hello.txt"), "world").unwrap();
        Git::add(temp.path(), "hello.txt").unwrap();

        // Commit
        Git::commit(temp.path(), "initial commit").unwrap();

        // Lightweight tag
        Git::tag(temp.path(), "v0.1.0", None).unwrap();

        // Annotated tag
        Git::tag(temp.path(), "my-crate-v0.1.0", Some("release 0.1.0")).unwrap();

        // Verify tags exist
        let output = Command::new("git")
            .args(["tag", "-l"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        let tags = String::from_utf8_lossy(&output.stdout);
        assert!(tags.contains("v0.1.0"));
        assert!(tags.contains("my-crate-v0.1.0"));
    }
}

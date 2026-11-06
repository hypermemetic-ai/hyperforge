//! `ops::git` — typed wrapper over git operations (V5PARITY-3, V5PARITY-15).
//!
//! V5PARITY-3 introduced this module as subprocess wrappers over the
//! user's `git` CLI so the user's SSH agent / credential helper /
//! hooks / config flow through transparently.
//!
//! V5PARITY-15 split the module into two backends behind the same
//! public API. **Network and hook-bearing ops** (clone/fetch/pull/push)
//! stay subprocess-only — they need the user's environment. **Pure
//! local ops** (status, add, commit, tag, checkout, config edits, show,
//! origin URL) route through `git2` for an order-of-magnitude latency
//! win on workspace iteration.
//!
//! Per-op routing is fixed at compile time below; nothing to choose at
//! runtime except the `HF_GIT_FORCE_SUBPROCESS=1` escape hatch, which
//! routes every op through subprocess (regression-isolation aid).
//!
//! D13 invariant: this is the only module that spawns `git` processes
//! or links against `git2`. Services route every git op through here.

use std::path::Path;

use thiserror::Error;

mod local;
mod subprocess;

/// Git operation error. Variant set unchanged from V5PARITY-3 plus a
/// new `Local` for git2-backed failures.
#[derive(Debug, Error, Clone)]
pub enum GitError {
    #[error("git not found on PATH")]
    GitNotFound,
    #[error("target path is not a git working tree: {0}")]
    NotAGitRepo(String),
    #[error("working tree is dirty: {0}")]
    DirtyTree(String),
    #[error("destination already exists: {0}")]
    DestExists(String),
    #[error("non-fast-forward — branch has diverged")]
    NonFastForward,
    #[error("git command failed ({code}): {stderr}")]
    CommandFailed { code: i32, stderr: String },
    #[error("git2 {code}: {message}")]
    Local { code: &'static str, message: String },
    #[error("io error: {0}")]
    Io(String),
}

impl GitError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::GitNotFound => "git_not_found",
            Self::NotAGitRepo(_) => "not_a_git_repo",
            Self::DirtyTree(_) => "dirty_tree",
            Self::DestExists(_) => "dest_exists",
            Self::NonFastForward => "non_ff",
            Self::CommandFailed { .. } => "git_failed",
            Self::Local { code, .. } => code,
            Self::Io(_) => "io",
        }
    }
}

/// Parsed git status snapshot. Identical between subprocess and git2
/// backends (acceptance-tested via dual-run under
/// `HF_GIT_FORCE_SUBPROCESS=1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
}

impl StatusSnapshot {
    #[must_use]
    pub const fn dirty(&self) -> bool {
        self.staged > 0 || self.unstaged > 0 || self.untracked > 0
    }
}

/// `core.sshCommand` template string. Both backends write the same
/// value into `.git/config` so a switch between backends doesn't
/// rewrite configs unnecessarily.
#[must_use]
pub fn format_ssh_command(key_path: &Path) -> String {
    format!("ssh -i {} -o IdentitiesOnly=yes", key_path.display())
}

fn force_subprocess() -> bool {
    std::env::var_os("HF_GIT_FORCE_SUBPROCESS")
        .map(|v| v != "0" && !v.is_empty())
        .unwrap_or(false)
}

// ===================================================================
// Subprocess-only ops — network / hooks / GIT_SSH_COMMAND forwarding.
// ===================================================================

pub fn clone_repo(url: &str, dest: &Path) -> Result<(), GitError> {
    subprocess::clone_repo(url, dest)
}

pub fn clone_repo_with_env(
    url: &str,
    dest: &Path,
    env: &[(&str, &str)],
) -> Result<(), GitError> {
    subprocess::clone_repo_with_env(url, dest, env)
}

/// Run git in `dir`, its stdout or a message naming the failure. The door
/// for build flows whose commands have no typed op here.
pub fn run_in(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

// ===================================================================
// HYPERFORGE-64: hyperforge's own `.hyperforge/config.toml`.
// ===================================================================

/// hyperforge's own per-repo file, which hyperforge itself rewrites (an org
/// move, `repo_name`, `forges`). One rule holds for it everywhere: when it is
/// tracked and hyperforge changes it, the change is committed by itself, so
/// it never shows as somebody's uncommitted work. The writer commits
/// ([`crate::v5::ops::fs::write_hyperforge_config`]); a pull or a publish
/// that meets it changed (an older hyperforge, a hand edit) commits it too.
pub const OWN_CONFIG: &str = ".hyperforge/config.toml";

/// `git status` entries (`XY`, path) in `dir`, every untracked file listed
/// by itself. Paths are unquoted (`-z`).
fn status_entries(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let out = run_in(dir, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let mut entries = Vec::new();
    let mut parts = out.split('\0');
    while let Some(e) = parts.next() {
        if e.len() < 4 {
            continue;
        }
        let xy = e[..2].to_string();
        // A rename or copy: `XY new\0old\0`.
        if xy.starts_with(['R', 'C']) {
            parts.next();
        }
        entries.push((xy, e[3..].to_string()));
    }
    Ok(entries)
}

/// A tracked, changed (not deleted) `.hyperforge/config.toml` entry.
fn is_own_config_change(xy: &str, path: &str) -> bool {
    path == OWN_CONFIG && xy != "??" && !xy.contains('D')
}

/// Is `.hyperforge/config.toml` tracked in `dir` and changed (staged or
/// not, not deleted)? `false` outside a git repo.
#[must_use]
pub fn own_config_changed(dir: &Path) -> bool {
    status_entries(dir).is_ok_and(|es| es.iter().any(|(xy, p)| is_own_config_change(xy, p)))
}

/// The uncommitted changes in `dir` (`XY path`, untracked included) other
/// than a tracked, changed `.hyperforge/config.toml`.
pub fn changes_besides_own_config(dir: &Path) -> Result<Vec<String>, String> {
    Ok(status_entries(dir)?
        .into_iter()
        .filter(|(xy, p)| !is_own_config_change(xy, p))
        .map(|(xy, p)| format!("{xy} {p}"))
        .collect())
}

/// Commit `.hyperforge/config.toml` by itself when it is tracked and
/// changed: `chore: .hyperforge/config.toml (hyperforge <by>)`. Anything
/// else staged stays staged and out of the commit. `Ok(Some(short sha))`
/// when it committed, `Ok(None)` when there was nothing to commit.
pub fn commit_own_config(dir: &Path, by: &str) -> Result<Option<String>, String> {
    if !own_config_changed(dir) {
        return Ok(None);
    }
    let msg = format!("chore: {OWN_CONFIG} (hyperforge {by})");
    run_in(dir, &["commit", "-q", "-m", &msg, "--only", "--", OWN_CONFIG])?;
    run_in(dir, &["rev-parse", "--short", "HEAD"]).map(|h| Some(h.trim().to_string()))
}

/// HYPERFORGE-64 (2): run blocking git (a subprocess, a network round
/// trip) off the async runtime. A service stream that runs git inline holds
/// its runtime worker for the whole call without yielding, and the socket's
/// task, woken by that stream into the same worker's LIFO slot (which no
/// other worker steals), waits behind it: the connection's answers and its
/// pongs go out only when the stream next returns `Pending`.
pub async fn off_runtime<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// Create a process for `git`, for callers that stream its stdin.
#[must_use]
pub fn git_command() -> std::process::Command {
    std::process::Command::new("git")
}

/// Does `refs/heads/<branch>` exist here?
#[must_use]
pub fn branch_exists(dir: &Path, branch: &str) -> bool {
    run_in(dir, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok()
}

/// Add a worktree at `dest` on `branch`: created from `base` (default HEAD),
/// or the existing branch as it stands, which refuses a `base`.
pub fn worktree_add(dir: &Path, dest: &Path, branch: &str, base: Option<&str>) -> Result<(), String> {
    let dest_s = dest.to_str().ok_or("worktree path is not UTF-8")?;
    if branch_exists(dir, branch) {
        if base.is_some() {
            return Err(format!("branch {branch} exists; it keeps its own base"));
        }
        run_in(dir, &["worktree", "add", "-q", dest_s, branch]).map(|_| ())
    } else {
        let mut args = vec!["worktree", "add", "-q", "-b", branch, dest_s];
        args.extend(base);
        run_in(dir, &args).map(|_| ())
    }
}

/// Remove a clean worktree. Git refuses one with uncommitted work.
pub fn worktree_remove(dir: &Path, dest: &Path) -> Result<(), String> {
    run_in(dir, &["worktree", "remove", dest.to_str().ok_or("worktree path is not UTF-8")?]).map(|_| ())
}

/// Every checkout of this repo: the main one first, then its worktrees.
pub fn worktree_list(dir: &Path) -> Result<Vec<std::path::PathBuf>, String> {
    Ok(run_in(dir, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(std::path::PathBuf::from)
        .collect())
}

pub fn fetch(dir: &Path, remote: Option<&str>) -> Result<(), GitError> {
    subprocess::fetch(dir, remote)
}

pub fn pull_ff(dir: &Path, remote: &str, branch: &str) -> Result<(), GitError> {
    subprocess::pull_ff(dir, remote, branch)
}

pub fn push_refs(dir: &Path, remote: &str, branch: Option<&str>) -> Result<(), GitError> {
    subprocess::push_refs(dir, remote, branch)
}

pub fn push_ref(dir: &Path, remote: &str, refspec: &str) -> Result<(), GitError> {
    subprocess::push_ref(dir, remote, refspec)
}

/// HYPERFORGE-60: an unrun `git -C <dir>` command with extra `-c` settings
/// and environment (credentials go in the environment, never the args).
pub fn command_with(
    dir: &Path,
    config: &[&str],
    envs: &[(String, std::ffi::OsString)],
    args: &[&str],
) -> std::process::Command {
    subprocess::command_with(dir, config, envs, args)
}

/// HYPERFORGE-60: does `refname` resolve in `dir`?
pub fn ref_exists(dir: &Path, refname: &str) -> bool {
    subprocess::ref_exists(dir, refname)
}

/// HYPERFORGE-60: `(refname, sha)` for every ref under `pattern`.
pub fn refs_under(dir: &Path, pattern: &str) -> Vec<(String, String)> {
    subprocess::refs_under(dir, pattern)
}

// ===================================================================
// Routed ops — git2 by default, subprocess via HF_GIT_FORCE_SUBPROCESS.
// ===================================================================

pub fn status(dir: &Path) -> Result<StatusSnapshot, GitError> {
    if force_subprocess() {
        subprocess::status(dir)
    } else {
        local::status(dir)
    }
}

pub fn is_dirty(dir: &Path) -> Result<bool, GitError> {
    if force_subprocess() {
        subprocess::is_dirty(dir)
    } else {
        local::is_dirty(dir)
    }
}

pub fn add(dir: &Path, paths: &[&str]) -> Result<(), GitError> {
    if force_subprocess() {
        subprocess::add(dir, paths)
    } else {
        local::add(dir, paths)
    }
}

pub fn commit(dir: &Path, message: &str) -> Result<(), GitError> {
    if force_subprocess() {
        subprocess::commit(dir, message)
    } else {
        local::commit(dir, message)
    }
}

pub fn commit_with(dir: &Path, message: &str, allow_empty: bool) -> Result<(), GitError> {
    if force_subprocess() {
        subprocess::commit_with(dir, message, allow_empty)
    } else {
        local::commit_with(dir, message, allow_empty)
    }
}

pub fn tag(dir: &Path, name: &str) -> Result<(), GitError> {
    if force_subprocess() {
        subprocess::tag(dir, name)
    } else {
        local::tag(dir, name)
    }
}

pub fn tag_annotated(dir: &Path, name: &str, message: &str) -> Result<(), GitError> {
    if force_subprocess() {
        subprocess::tag_annotated(dir, name, message)
    } else {
        local::tag_annotated(dir, name, message)
    }
}

pub fn checkout(dir: &Path, branch: &str, create: bool) -> Result<(), GitError> {
    if force_subprocess() {
        subprocess::checkout(dir, branch, create)
    } else {
        local::checkout(dir, branch, create)
    }
}

pub fn show(dir: &Path, rev: &str, path: &str) -> Result<String, GitError> {
    if force_subprocess() {
        subprocess::show(dir, rev, path)
    } else {
        local::show(dir, rev, path)
    }
}

pub fn set_remote_url(dir: &Path, name: &str, url: &str) -> Result<(), GitError> {
    if force_subprocess() {
        subprocess::set_remote_url(dir, name, url)
    } else {
        local::set_remote_url(dir, name, url)
    }
}

pub fn set_ssh_command(dir: &Path, key_path: &Path) -> Result<(), GitError> {
    if force_subprocess() {
        subprocess::set_ssh_command(dir, key_path)
    } else {
        local::set_ssh_command(dir, key_path)
    }
}

pub fn clear_ssh_command(dir: &Path) -> Result<(), GitError> {
    if force_subprocess() {
        subprocess::clear_ssh_command(dir)
    } else {
        local::clear_ssh_command(dir)
    }
}

pub fn get_ssh_command(dir: &Path) -> Result<Option<String>, GitError> {
    if force_subprocess() {
        subprocess::get_ssh_command(dir)
    } else {
        local::get_ssh_command(dir)
    }
}

/// Read the `origin` remote URL. New in V5PARITY-15 — replaces the
/// hand-rolled `.git/config` INI parser that lived in `workspaces.rs`.
pub fn read_origin_url(dir: &Path) -> Result<Option<String>, GitError> {
    if force_subprocess() {
        subprocess::read_origin_url(dir)
    } else {
        local::read_origin_url(dir)
    }
}

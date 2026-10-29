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

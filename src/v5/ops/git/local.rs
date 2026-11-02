//! `ops::git::local` — git2-backed local ops (V5PARITY-15).
//!
//! Used by `mod.rs` for ops that don't need the user's SSH agent /
//! credential helper / pre-receive hooks. Open the repo once with
//! `Repository::open` and run all reads/writes in-process. Cuts
//! per-call latency from ~1–3ms (subprocess spawn) to <100µs.
//!
//! Behavior parity is checked against subprocess via
//! `HF_GIT_FORCE_SUBPROCESS=1` regression runs (V5PARITY-15 acceptance).

use std::path::Path;

use git2::{Repository, StatusOptions};

use super::{GitError, StatusSnapshot};

fn open(dir: &Path) -> Result<Repository, GitError> {
    Repository::open(dir).map_err(map_open_error(dir))
}

fn map_open_error(dir: &Path) -> impl Fn(git2::Error) -> GitError + '_ {
    move |e: git2::Error| {
        if matches!(e.code(), git2::ErrorCode::NotFound) {
            GitError::NotAGitRepo(dir.display().to_string())
        } else {
            GitError::Local { code: "open", message: e.message().to_string() }
        }
    }
}

fn map_err(op: &'static str) -> impl Fn(git2::Error) -> GitError {
    move |e: git2::Error| GitError::Local { code: op, message: e.message().to_string() }
}

pub(super) fn status(dir: &Path) -> Result<StatusSnapshot, GitError> {
    let repo = open(dir)?;

    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);

    let mut staged = 0u32;
    let mut unstaged = 0u32;
    let mut untracked = 0u32;

    let statuses = repo.statuses(Some(&mut opts)).map_err(map_err("status"))?;
    for entry in statuses.iter() {
        let s = entry.status();
        if s.contains(git2::Status::WT_NEW) {
            untracked += 1;
            continue;
        }
        let is_staged = s.intersects(
            git2::Status::INDEX_NEW
                | git2::Status::INDEX_MODIFIED
                | git2::Status::INDEX_DELETED
                | git2::Status::INDEX_RENAMED
                | git2::Status::INDEX_TYPECHANGE,
        );
        let is_unstaged = s.intersects(
            git2::Status::WT_MODIFIED
                | git2::Status::WT_DELETED
                | git2::Status::WT_TYPECHANGE
                | git2::Status::WT_RENAMED,
        );
        if is_staged {
            staged += 1;
        }
        if is_unstaged {
            unstaged += 1;
        }
    }

    let (branch, upstream, ahead, behind) = branch_tracking(&repo);

    Ok(StatusSnapshot {
        branch,
        upstream,
        ahead,
        behind,
        staged,
        unstaged,
        untracked,
    })
}

pub(super) fn is_dirty(dir: &Path) -> Result<bool, GitError> {
    status(dir).map(|s| s.dirty())
}

fn branch_tracking(repo: &Repository) -> (Option<String>, Option<String>, u32, u32) {
    let head = match repo.head() {
        Ok(h) => h,
        Err(_) => return (None, None, 0, 0),
    };
    let branch = head.shorthand().map(String::from);
    let local_oid = match head.target() {
        Some(o) => o,
        None => return (branch, None, 0, 0),
    };

    if !head.is_branch() {
        return (branch, None, 0, 0);
    }
    let local_branch = match git2::Branch::wrap(head).upstream() {
        Ok(b) => b,
        Err(_) => return (branch, None, 0, 0),
    };
    let upstream_name = local_branch
        .name()
        .ok()
        .flatten()
        .map(String::from);
    let upstream_oid = match local_branch.get().target() {
        Some(o) => o,
        None => return (branch, upstream_name, 0, 0),
    };
    match repo.graph_ahead_behind(local_oid, upstream_oid) {
        Ok((a, b)) => (
            branch,
            upstream_name,
            u32::try_from(a).unwrap_or(u32::MAX),
            u32::try_from(b).unwrap_or(u32::MAX),
        ),
        Err(_) => (branch, upstream_name, 0, 0),
    }
}

pub(super) fn add(dir: &Path, paths: &[&str]) -> Result<(), GitError> {
    let repo = open(dir)?;
    let mut index = repo.index().map_err(map_err("add"))?;
    for p in paths {
        index.add_path(Path::new(p)).map_err(map_err("add"))?;
    }
    index.write().map_err(map_err("add"))?;
    Ok(())
}

pub(super) fn commit(dir: &Path, message: &str) -> Result<(), GitError> {
    commit_with(dir, message, false)
}

pub(super) fn commit_with(dir: &Path, message: &str, allow_empty: bool) -> Result<(), GitError> {
    let repo = open(dir)?;
    let sig = repo.signature().map_err(map_err("commit"))?;

    let mut index = repo.index().map_err(map_err("commit"))?;
    let tree_oid = index.write_tree().map_err(map_err("commit"))?;
    let tree = repo.find_tree(tree_oid).map_err(map_err("commit"))?;

    let parent_commits: Vec<git2::Commit> = match repo.head().and_then(|h| h.peel_to_commit()) {
        Ok(c) => vec![c],
        Err(_) => Vec::new(),
    };

    if !allow_empty && !parent_commits.is_empty() {
        let parent_tree = parent_commits[0].tree().map_err(map_err("commit"))?;
        if parent_tree.id() == tree.id() {
            return Err(GitError::CommandFailed {
                code: 1,
                stderr: "nothing to commit".into(),
            });
        }
    }

    let parent_refs: Vec<&git2::Commit> = parent_commits.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
        .map_err(map_err("commit"))?;
    Ok(())
}

pub(super) fn tag(dir: &Path, name: &str) -> Result<(), GitError> {
    let repo = open(dir)?;
    let head = repo.head().map_err(map_err("tag"))?;
    let target = head
        .resolve()
        .and_then(|r| r.peel(git2::ObjectType::Any))
        .map_err(map_err("tag"))?;
    repo.tag_lightweight(name, &target, false)
        .map_err(map_err("tag"))?;
    Ok(())
}

pub(super) fn tag_annotated(dir: &Path, name: &str, message: &str) -> Result<(), GitError> {
    let repo = open(dir)?;
    let head = repo.head().map_err(map_err("tag"))?;
    let target = head
        .resolve()
        .and_then(|r| r.peel(git2::ObjectType::Any))
        .map_err(map_err("tag"))?;
    let sig = repo.signature().map_err(map_err("tag"))?;
    repo.tag(name, &target, &sig, message, false)
        .map_err(map_err("tag"))?;
    Ok(())
}

pub(super) fn checkout(dir: &Path, branch: &str, create: bool) -> Result<(), GitError> {
    let repo = open(dir)?;
    let head_commit = match repo.head().and_then(|h| h.peel_to_commit()) {
        Ok(c) => Some(c),
        Err(_) => None,
    };

    if create {
        // `checkout -b`: a branch that exists is refused, never moved.
        if let Some(commit) = &head_commit {
            repo.branch(branch, commit, false).map_err(map_err("checkout"))?;
        } else {
            return Err(GitError::Local {
                code: "checkout",
                message: "no HEAD to base new branch on".into(),
            });
        }
    }

    // Resolve refs/heads/<branch> and check it out.
    let ref_name = format!("refs/heads/{branch}");
    let obj = repo.revparse_single(&ref_name).map_err(map_err("checkout"))?;
    repo.checkout_tree(&obj, None).map_err(map_err("checkout"))?;
    repo.set_head(&ref_name).map_err(map_err("checkout"))?;
    Ok(())
}

pub(super) fn show(dir: &Path, rev: &str, path: &str) -> Result<String, GitError> {
    let repo = open(dir)?;
    let spec = format!("{rev}:{path}");
    let obj = repo.revparse_single(&spec).map_err(map_err("show"))?;
    let blob = obj.peel_to_blob().map_err(map_err("show"))?;
    Ok(String::from_utf8_lossy(blob.content()).into_owned())
}

pub(super) fn set_remote_url(dir: &Path, name: &str, url: &str) -> Result<(), GitError> {
    let repo = open(dir)?;
    repo.remote_set_url(name, url).map_err(map_err("set_remote_url"))?;
    Ok(())
}

pub(super) fn set_ssh_command(dir: &Path, key_path: &Path) -> Result<(), GitError> {
    let repo = open(dir)?;
    let mut config = repo.config().map_err(map_err("config"))?;
    let cmd = super::format_ssh_command(key_path);
    config.set_str("core.sshCommand", &cmd).map_err(map_err("config"))?;
    Ok(())
}

pub(super) fn clear_ssh_command(dir: &Path) -> Result<(), GitError> {
    let repo = open(dir)?;
    let mut config = repo.config().map_err(map_err("config"))?;
    match config.remove("core.sshCommand") {
        Ok(()) => Ok(()),
        Err(e) if matches!(e.code(), git2::ErrorCode::NotFound) => Ok(()),
        Err(e) => Err(GitError::Local { code: "config", message: e.message().to_string() }),
    }
}

pub(super) fn get_ssh_command(dir: &Path) -> Result<Option<String>, GitError> {
    let repo = open(dir)?;
    let config = repo.config().map_err(map_err("config"))?;
    match config.get_string("core.sshCommand") {
        Ok(s) => Ok(Some(s)),
        Err(e) if matches!(e.code(), git2::ErrorCode::NotFound) => Ok(None),
        Err(e) => Err(GitError::Local { code: "config", message: e.message().to_string() }),
    }
}

/// Read the `origin` remote URL via libgit2. Replaces the hand-rolled
/// `.git/config` INI parser that lived in `workspaces.rs`.
///
/// Tries `Repository::open` first (canonical path). Falls back to
/// reading `.git/config` directly via `git2::Config::open` when the
/// repo can't be opened — typically a test fixture that wrote a config
/// without initializing the repo state, or a partial clone in flight.
pub(super) fn read_origin_url(dir: &Path) -> Result<Option<String>, GitError> {
    if !dir.join(".git").exists() {
        return Ok(None);
    }
    if let Ok(repo) = Repository::open(dir) {
        let url = match repo.find_remote("origin") {
            Ok(remote) => remote.url().map(String::from),
            Err(e) if matches!(e.code(), git2::ErrorCode::NotFound) => None,
            Err(e) => return Err(GitError::Local { code: "remote", message: e.message().to_string() }),
        };
        return Ok(url);
    }
    // Resolve `.git` whether it's a directory or a `gitdir:` pointer file.
    let git_dir = dir.join(".git");
    let cfg_path = if git_dir.is_file() {
        let txt = std::fs::read_to_string(&git_dir).map_err(|e| GitError::Io(e.to_string()))?;
        let rest = txt.trim().strip_prefix("gitdir:").map(str::trim).ok_or_else(|| {
            GitError::Local { code: "config", message: "malformed .git pointer file".into() }
        })?;
        std::path::PathBuf::from(rest).join("config")
    } else {
        git_dir.join("config")
    };
    if !cfg_path.is_file() {
        return Ok(None);
    }
    let cfg = git2::Config::open(&cfg_path)
        .map_err(|e| GitError::Local { code: "config", message: e.message().to_string() })?;
    match cfg.get_string("remote.origin.url") {
        Ok(u) => Ok(Some(u)),
        Err(e) if matches!(e.code(), git2::ErrorCode::NotFound) => Ok(None),
        Err(e) => Err(GitError::Local { code: "config", message: e.message().to_string() }),
    }
}

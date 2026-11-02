//! `ops::git::subprocess` — subprocess-backed git ops (V5PARITY-3).
//!
//! Per ticket R1: shells out to the user's `git` CLI so SSH agent /
//! credential helper / hooks / `core.sshCommand` flow through
//! transparently. This is the ONLY module under `src/v5/` that spawns
//! `git` processes (D13 + V5LIFECYCLE-11's `command-git` invariant).
//!
//! V5PARITY-15 split this out from a single-file `ops::git` module —
//! `mod.rs` now owns the public API and routes per-op between
//! subprocess (network/hook ops) and `local` (git2-backed local ops).

use std::path::Path;
use std::process::Command;

use super::{GitError, StatusSnapshot};

pub(super) fn clone_repo(url: &str, dest: &Path) -> Result<(), GitError> {
    clone_repo_with_env(url, dest, &[])
}

pub(super) fn clone_repo_with_env(
    url: &str,
    dest: &Path,
    env: &[(&str, &str)],
) -> Result<(), GitError> {
    if dest.exists() {
        return Err(GitError::DestExists(dest.display().to_string()));
    }
    run_git_with_env(
        None,
        &["clone", url, dest.to_str().unwrap_or("")],
        env,
    )
}

pub(super) fn fetch(dir: &Path, remote: Option<&str>) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    let mut args: Vec<&str> = vec!["-C", dir.to_str().unwrap_or(""), "fetch"];
    if let Some(r) = remote {
        args.push(r);
    } else {
        args.push("--all");
    }
    run_git(None, &args)
}

pub(super) fn pull_ff(dir: &Path, remote: &str, branch: &str) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    if is_dirty(dir)? {
        return Err(GitError::DirtyTree(dir.display().to_string()));
    }
    match run_git(
        None,
        &["-C", dir.to_str().unwrap_or(""), "pull", "--ff-only", remote, branch],
    ) {
        Ok(()) => Ok(()),
        Err(GitError::CommandFailed { stderr, .. }) if stderr.contains("Not possible to fast-forward") => {
            Err(GitError::NonFastForward)
        }
        Err(e) => Err(e),
    }
}

pub(super) fn push_refs(dir: &Path, remote: &str, branch: Option<&str>) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    let mut args: Vec<&str> = vec!["-C", dir.to_str().unwrap_or(""), "push", remote];
    if let Some(b) = branch {
        args.push(b);
    }
    run_git(None, &args)
}

/// HYPERFORGE-60: a `git -C <dir>` command with extra `-c` settings and
/// environment, unrun, for a caller that passes a credential through the
/// environment (never the arguments) and reads git's output itself.
pub(super) fn command_with(
    dir: &Path,
    config: &[&str],
    envs: &[(String, std::ffi::OsString)],
    args: &[&str],
) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    for c in config {
        cmd.arg("-c").arg(c);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(args);
    cmd
}

/// HYPERFORGE-60: does `refname` resolve in `dir`?
pub(super) fn ref_exists(dir: &Path, refname: &str) -> bool {
    run_git_capture(None, &["-C", dir.to_str().unwrap_or(""), "rev-parse", "--verify", "--quiet", refname]).is_ok()
}

/// HYPERFORGE-60: `(refname, sha)` for every ref under `pattern`.
pub(super) fn refs_under(dir: &Path, pattern: &str) -> Vec<(String, String)> {
    run_git_capture(None, &["-C", dir.to_str().unwrap_or(""), "for-each-ref", "--format=%(objectname) %(refname)", pattern])
        .map(|out| {
            out.lines()
                .filter_map(|l| l.split_once(' ').map(|(sha, name)| (name.to_string(), sha.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn push_ref(dir: &Path, remote: &str, refspec: &str) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    run_git(
        None,
        &["-C", dir.to_str().unwrap_or(""), "push", remote, refspec],
    )
}

pub(super) fn status(dir: &Path) -> Result<StatusSnapshot, GitError> {
    ensure_git_repo(dir)?;
    let out = run_git_capture(
        None,
        &["-C", dir.to_str().unwrap_or(""), "status", "--porcelain=v2", "--branch"],
    )?;
    Ok(parse_status(&out))
}

pub(super) fn is_dirty(dir: &Path) -> Result<bool, GitError> {
    status(dir).map(|s| s.dirty())
}

pub(super) fn set_remote_url(dir: &Path, name: &str, url: &str) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    run_git(
        None,
        &["-C", dir.to_str().unwrap_or(""), "remote", "set-url", name, url],
    )
}

pub(super) fn add(dir: &Path, paths: &[&str]) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    let mut args: Vec<&str> = vec!["-C", dir.to_str().unwrap_or(""), "add"];
    args.extend_from_slice(paths);
    run_git(None, &args)
}

pub(super) fn commit(dir: &Path, message: &str) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    run_git(
        None,
        &["-C", dir.to_str().unwrap_or(""), "commit", "-m", message],
    )
}

pub(super) fn commit_with(dir: &Path, message: &str, allow_empty: bool) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    let dir_s = dir.to_str().unwrap_or("");
    if allow_empty {
        run_git(None, &["-C", dir_s, "commit", "--allow-empty", "-m", message])
    } else {
        run_git(None, &["-C", dir_s, "commit", "-m", message])
    }
}

pub(super) fn tag(dir: &Path, name: &str) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    run_git(
        None,
        &["-C", dir.to_str().unwrap_or(""), "tag", name],
    )
}

pub(super) fn tag_annotated(dir: &Path, name: &str, message: &str) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    run_git(
        None,
        &["-C", dir.to_str().unwrap_or(""), "tag", "-a", name, "-m", message],
    )
}

pub(super) fn checkout(dir: &Path, branch: &str, create: bool) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    let dir_s = dir.to_str().unwrap_or("");
    if create {
        return run_git(None, &["-C", dir_s, "checkout", "-b", branch]);
    }
    run_git(None, &["-C", dir_s, "checkout", branch])
}

pub(super) fn show(dir: &Path, rev: &str, path: &str) -> Result<String, GitError> {
    ensure_git_repo(dir)?;
    run_git_capture(
        None,
        &["-C", dir.to_str().unwrap_or(""), "show", &format!("{rev}:{path}")],
    )
}

pub(super) fn set_ssh_command(dir: &Path, key_path: &Path) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    let cmd = super::format_ssh_command(key_path);
    run_git(
        None,
        &["-C", dir.to_str().unwrap_or(""), "config", "core.sshCommand", &cmd],
    )
}

pub(super) fn clear_ssh_command(dir: &Path) -> Result<(), GitError> {
    ensure_git_repo(dir)?;
    match run_git(
        None,
        &["-C", dir.to_str().unwrap_or(""), "config", "--unset", "core.sshCommand"],
    ) {
        Ok(()) => Ok(()),
        Err(GitError::CommandFailed { code: 5, .. }) => Ok(()),
        Err(e) => Err(e),
    }
}

pub(super) fn get_ssh_command(dir: &Path) -> Result<Option<String>, GitError> {
    ensure_git_repo(dir)?;
    match run_git_capture(
        None,
        &["-C", dir.to_str().unwrap_or(""), "config", "--get", "core.sshCommand"],
    ) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(GitError::CommandFailed { code: 1, .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read the `origin` remote URL via subprocess. Used as the fallback
/// when `local::read_origin_url` (git2-backed) is disabled via
/// `HF_GIT_FORCE_SUBPROCESS=1`.
///
/// Uses `git config --file <path> --get` to read directly from the
/// config file without requiring repo state — `git -C <dir> config`
/// would fail on test fixtures that wrote a bare `.git/config`. The
/// `local` backend handles that case via `git2::Config::open`; this
/// keeps behavior parity.
pub(super) fn read_origin_url(dir: &Path) -> Result<Option<String>, GitError> {
    let git_dir = dir.join(".git");
    if !git_dir.exists() {
        return Ok(None);
    }
    let cfg_path = if git_dir.is_file() {
        let txt = std::fs::read_to_string(&git_dir).map_err(|e| GitError::Io(e.to_string()))?;
        let rest = txt.trim().strip_prefix("gitdir:").map(str::trim)
            .ok_or_else(|| GitError::Io("malformed .git pointer file".into()))?;
        std::path::PathBuf::from(rest).join("config")
    } else {
        git_dir.join("config")
    };
    if !cfg_path.is_file() {
        return Ok(None);
    }
    match run_git_capture(
        None,
        &["config", "--file", cfg_path.to_str().unwrap_or(""), "--get", "remote.origin.url"],
    ) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(GitError::CommandFailed { code: 1, .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------
// Internals.
// ---------------------------------------------------------------------

fn ensure_git_repo(dir: &Path) -> Result<(), GitError> {
    let git_dir = dir.join(".git");
    if !git_dir.exists() {
        return Err(GitError::NotAGitRepo(dir.display().to_string()));
    }
    Ok(())
}

fn run_git(cwd: Option<&Path>, args: &[&str]) -> Result<(), GitError> {
    run_git_with_env(cwd, args, &[])
}

fn run_git_with_env(cwd: Option<&Path>, args: &[&str], env: &[(&str, &str)]) -> Result<(), GitError> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    match cmd.output() {
        Ok(out) => {
            if out.status.success() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                let code = out.status.code().unwrap_or(-1);
                Err(GitError::CommandFailed { code, stderr })
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(GitError::GitNotFound),
        Err(e) => Err(GitError::Io(e.to_string())),
    }
}

fn run_git_capture(cwd: Option<&Path>, args: &[&str]) -> Result<String, GitError> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    match cmd.output() {
        Ok(out) => {
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                let code = out.status.code().unwrap_or(-1);
                Err(GitError::CommandFailed { code, stderr })
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(GitError::GitNotFound),
        Err(e) => Err(GitError::Io(e.to_string())),
    }
}

/// Parse `git status --porcelain=v2 --branch` output into a snapshot.
/// Porcelain v2 format:
///   # branch.oid <oid>
///   # branch.head <branch>
///   # branch.upstream <upstream>
///   # branch.ab +N -M
///   1 <status> ... (staged/unstaged)
///   ? <path>      (untracked)
fn parse_status(raw: &str) -> StatusSnapshot {
    let mut branch = None;
    let mut upstream = None;
    let mut ahead = 0u32;
    let mut behind = 0u32;
    let mut staged = 0u32;
    let mut unstaged = 0u32;
    let mut untracked = 0u32;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            branch = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            upstream = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            let mut it = rest.split_whitespace();
            if let Some(a) = it.next() {
                ahead = a.trim_start_matches('+').parse().unwrap_or(0);
            }
            if let Some(b) = it.next() {
                behind = b.trim_start_matches('-').parse().unwrap_or(0);
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let fields: Vec<&str> = line.splitn(3, ' ').collect();
            if fields.len() >= 2 {
                let xy = fields[1].as_bytes();
                if xy.first().is_some_and(|&c| c != b'.') {
                    staged += 1;
                }
                if xy.get(1).is_some_and(|&c| c != b'.') {
                    unstaged += 1;
                }
            }
        } else if line.starts_with("? ") {
            untracked += 1;
        }
    }
    StatusSnapshot {
        branch,
        upstream,
        ahead,
        behind,
        staged,
        unstaged,
        untracked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_clean() {
        let raw = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n";
        let s = parse_status(raw);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.ahead, 0);
        assert_eq!(s.behind, 0);
        assert!(!s.dirty());
    }

    #[test]
    fn parse_status_dirty() {
        let raw = "# branch.head main\n# branch.ab +2 -1\n1 M. N... 100644 100644 100644 aaa bbb file1\n? unknown.txt\n";
        let s = parse_status(raw);
        assert_eq!(s.ahead, 2);
        assert_eq!(s.behind, 1);
        assert_eq!(s.staged, 1);
        assert_eq!(s.untracked, 1);
        assert!(s.dirty());
    }
}

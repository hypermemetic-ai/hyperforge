//! `build::release` — version bump logic.
//!
//! [`bump_cargo_toml`] is pure string manipulation. HYPERFORGE-44: [`plan`]
//! is what `build.release` and `build.release_all` may do, checked against
//! the checkout with nothing written; [`commit_and_tag`] does it.

use std::path::Path;

use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum BumpError {
    #[error("unknown bump kind: {0}")]
    UnknownKind(String),
    #[error("version is not semver-shaped: {0}")]
    NotSemver(String),
    #[error("manifest missing [package].version")]
    MissingVersion,
    #[error("io error: {0}")]
    Io(String),
    #[error("toml edit error: {0}")]
    Toml(String),
}

impl BumpError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownKind(_) => "unknown_bump_kind",
            Self::NotSemver(_) => "not_semver",
            Self::MissingVersion => "missing_version",
            Self::Io(_) => "io",
            Self::Toml(_) => "manifest_parse_error",
        }
    }
}

/// Parse a `MAJOR.MINOR.PATCH[-pre]` triple. Ignores any build/pre
/// suffix on the emitted new version — the bump resets the suffix.
fn parse_semver(v: &str) -> Result<(u64, u64, u64), BumpError> {
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err(BumpError::NotSemver(v.to_string()));
    }
    let parse = |s: &str| s.parse::<u64>().map_err(|_| BumpError::NotSemver(v.to_string()));
    Ok((parse(parts[0])?, parse(parts[1])?, parse(parts[2])?))
}

#[must_use]
pub fn apply_bump(current: (u64, u64, u64), kind: &str) -> Option<(u64, u64, u64)> {
    match kind {
        "major" => Some((current.0 + 1, 0, 0)),
        "minor" => Some((current.0, current.1 + 1, 0)),
        "patch" => Some((current.0, current.1, current.2 + 1)),
        _ => None,
    }
}

/// Edit the `version` in `[package]` of a Cargo.toml. Preserves
/// formatting via `toml_edit`. Returns `(old, new, new_text)`.
pub fn bump_cargo_toml(
    text: &str,
    kind_or_target: &str,
) -> Result<(String, String, String), BumpError> {
    let mut doc: toml_edit::DocumentMut = text.parse()
        .map_err(|e: toml_edit::TomlError| BumpError::Toml(e.to_string()))?;
    let pkg = doc.get_mut("package")
        .and_then(|t| t.as_table_mut())
        .ok_or(BumpError::MissingVersion)?;
    let old = pkg.get("version")
        .and_then(|v| v.as_str())
        .ok_or(BumpError::MissingVersion)?
        .to_string();
    let new = if ["major", "minor", "patch"].contains(&kind_or_target) {
        let parsed = parse_semver(&old)?;
        let nb = apply_bump(parsed, kind_or_target)
            .ok_or_else(|| BumpError::UnknownKind(kind_or_target.to_string()))?;
        format!("{}.{}.{}", nb.0, nb.1, nb.2)
    } else {
        // Treat as exact target. Validate shape.
        parse_semver(kind_or_target)?;
        kind_or_target.to_string()
    };
    pkg["version"] = toml_edit::value(new.clone());
    Ok((old, new, doc.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_patch_works() {
        let raw = "[package]\nname = \"foo\"\nversion = \"0.1.2\"\n";
        let (old, new, out) = bump_cargo_toml(raw, "patch").unwrap();
        assert_eq!(old, "0.1.2");
        assert_eq!(new, "0.1.3");
        assert!(out.contains("version = \"0.1.3\""));
    }

    #[test]
    fn bump_minor_resets_patch() {
        let raw = "[package]\nversion = \"1.4.7\"\n";
        let (_, new, _) = bump_cargo_toml(raw, "minor").unwrap();
        assert_eq!(new, "1.5.0");
    }

    #[test]
    fn bump_exact_target() {
        let raw = "[package]\nversion = \"0.1.0\"\n";
        let (_, new, _) = bump_cargo_toml(raw, "2.0.0").unwrap();
        assert_eq!(new, "2.0.0");
    }

    #[test]
    fn rejects_non_semver() {
        let raw = "[package]\nversion = \"not-a-version\"\n";
        assert!(bump_cargo_toml(raw, "patch").is_err());
    }
}

/// HYPERFORGE-44: why a release did not go ahead. `code` is the wire's.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// One release, worked out and not yet done.
#[derive(Debug, Clone)]
pub struct Plan {
    pub old: String,
    pub new: String,
    pub tag: String,
    pub branch: String,
    raw: String,
    new_text: String,
}

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    crate::v5::ops::git::run_in(dir, args).ok().map(|s| s.trim().to_string())
}

/// Work out a release, refusing a checkout that would release more than the
/// bump: a dirty tree, a detached HEAD, a branch that is not `origin`'s,
/// commits on either side of it, or a tag already taken. Fetches `origin`;
/// writes nothing else.
pub fn plan(dir: &Path, kind: &str) -> Result<Plan, Refusal> {
    use crate::v5::ops::git;
    let raw = std::fs::read_to_string(dir.join("Cargo.toml"))
        .map_err(|e| Refusal::new("manifest_not_found", format!("{}: {e}", dir.display())))?;
    let (old, new, new_text) = bump_cargo_toml(&raw, kind).map_err(|e| Refusal::new(e.code(), e.to_string()))?;
    git::fetch(dir, Some("origin")).map_err(|e| Refusal::new("fetch_failed", e.to_string()))?;
    let status = git::status(dir).map_err(|e| Refusal::new(e.code(), e.to_string()))?;
    if status.dirty() {
        return Err(Refusal::new("dirty_tree", format!("{} has uncommitted changes", dir.display())));
    }
    let branch = status
        .branch
        .filter(|b| b != "HEAD")
        .ok_or_else(|| Refusal::new("detached_head", format!("{} is on no branch", dir.display())))?;
    let remote = format!("refs/remotes/origin/{branch}");
    if git_stdout(dir, &["rev-parse", "-q", "--verify", &remote]).is_none() {
        return Err(Refusal::new("branch_not_on_origin", format!("origin has no {branch}")));
    }
    let counts = git_stdout(dir, &["rev-list", "--left-right", "--count", &format!("{remote}...HEAD")])
        .ok_or_else(|| Refusal::new("fetch_failed", "could not compare with origin"))?;
    let mut sides = counts.split_whitespace().map(|n| n.parse::<u32>().unwrap_or(u32::MAX));
    let (behind, ahead) = (sides.next().unwrap_or(u32::MAX), sides.next().unwrap_or(u32::MAX));
    if ahead > 0 {
        return Err(Refusal::new(
            "unpushed_commits",
            format!("{branch} has {ahead} commit(s) origin lacks: a release would push them too"),
        ));
    }
    if behind > 0 {
        return Err(Refusal::new("behind_origin", format!("{branch} is {behind} commit(s) behind origin")));
    }
    let tag = format!("v{new}");
    if git_stdout(dir, &["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")]).is_some() {
        return Err(Refusal::new("tag_exists", format!("{tag} already exists")));
    }
    Ok(Plan { old, new, tag, branch, raw, new_text })
}

/// Write the bump, commit it and tag it, annotated. A failed commit puts the
/// manifest back; a failed tag leaves the commit and says so.
pub fn commit_and_tag(dir: &Path, plan: &Plan) -> Result<(), Refusal> {
    use crate::v5::ops::git;
    let cargo = dir.join("Cargo.toml");
    std::fs::write(&cargo, &plan.new_text).map_err(|e| Refusal::new("io", e.to_string()))?;
    let committed = git::add(dir, &["Cargo.toml"])
        .and_then(|()| git::commit(dir, &format!("chore: bump version to {}", plan.new)));
    if let Err(e) = committed {
        let _ = std::fs::write(&cargo, &plan.raw);
        return Err(Refusal::new("commit_failed", e.to_string()));
    }
    git::tag_annotated(dir, &plan.tag, &format!("Release {}", plan.tag)).map_err(|e| {
        Refusal::new("tag_failed", format!("the bump is committed locally, {} is not tagged: {e}", plan.new))
    })
}

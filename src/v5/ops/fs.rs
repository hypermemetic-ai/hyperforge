//! `ops::fs` — filesystem interactions outside the daemon's config dir
//! (V5LIFECYCLE-9). Currently just `.hyperforge/config.toml` read/write.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v5::config::{OrgName, ProviderKind};

/// On-disk shape of `<repo>/.hyperforge/config.toml`. Keys it does not
/// name (`[ci]`, `[publish]`, ...) are left to their own readers. A file
/// without `repo_name` takes the checkout directory's name.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HyperforgeRepoConfig {
    #[serde(default)]
    pub repo_name: String,
    pub org: OrgName,
    #[serde(default)]
    pub forges: Vec<ProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Path layout: `<dir>/.hyperforge/config.toml`.
fn config_path(dir: &Path) -> std::path::PathBuf {
    dir.join(".hyperforge").join("config.toml")
}

/// Read the `.hyperforge/config.toml` in `dir`. `Ok(None)` when absent;
/// `Err` on malformed TOML.
pub fn read_hyperforge_config(dir: &Path) -> Result<Option<HyperforgeRepoConfig>, InitError> {
    let path = config_path(dir);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(InitError::Io(e.to_string())),
    };
    let mut cfg: HyperforgeRepoConfig =
        toml::from_str(&raw).map_err(|e| InitError::InvalidToml(e.to_string()))?;
    if cfg.repo_name.is_empty() {
        cfg.repo_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| InitError::InvalidToml(format!("{}: no repo_name and no directory name", path.display())))?;
    }
    Ok(Some(cfg))
}

/// Write a `.hyperforge/config.toml` atomically. Fails if a file
/// exists at the target and `force` is false.
pub fn write_hyperforge_config(
    dir: &Path,
    cfg: &HyperforgeRepoConfig,
    force: bool,
) -> Result<std::path::PathBuf, InitError> {
    if !dir.is_dir() {
        return Err(InitError::TargetNotDir(dir.display().to_string()));
    }
    let path = config_path(dir);
    if path.exists() && !force {
        return Err(InitError::AlreadyExists(path.display().to_string()));
    }
    let parent = path.parent().ok_or_else(|| InitError::Io("no parent".into()))?;
    std::fs::create_dir_all(parent).map_err(|e| InitError::Io(e.to_string()))?;
    let body = toml::to_string_pretty(cfg).map_err(|e| InitError::InvalidToml(e.to_string()))?;
    let tmp = parent.join(".hyperforge-config.toml.tmp");
    std::fs::write(&tmp, body).map_err(|e| InitError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| InitError::Io(e.to_string()))?;
    Ok(path)
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("target path is not a directory: {0}")]
    TargetNotDir(String),
    #[error("already exists at {0} — pass force=true to overwrite")]
    AlreadyExists(String),
    #[error("invalid TOML: {0}")]
    InvalidToml(String),
    #[error("io: {0}")]
    Io(String),
}

impl InitError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::TargetNotDir(_) => "not_a_directory",
            Self::AlreadyExists(_) => "already_exists",
            Self::InvalidToml(_) => "invalid_toml",
            Self::Io(_) => "io_error",
        }
    }
}

#[cfg(test)]
mod repo_name_default {
    use super::read_hyperforge_config;

    #[test]
    fn a_config_without_repo_name_takes_the_directory_name() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("plexus-core");
        std::fs::create_dir_all(dir.join(".hyperforge")).unwrap();
        std::fs::write(
            dir.join(".hyperforge/config.toml"),
            "org = \"hypermemetic-ai\"\nforges = [\"github\"]\n\n[ci]\nskip_validate = false\n",
        ).unwrap();
        let cfg = read_hyperforge_config(&dir).unwrap().unwrap();
        assert_eq!(cfg.repo_name, "plexus-core");
        assert_eq!(cfg.org.as_str(), "hypermemetic-ai");
    }

    #[test]
    fn a_named_config_keeps_its_name() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("checkout-dir");
        std::fs::create_dir_all(dir.join(".hyperforge")).unwrap();
        std::fs::write(dir.join(".hyperforge/config.toml"), "repo_name = \"menger\"\norg = \"hypermemetic-ai\"\n").unwrap();
        assert_eq!(read_hyperforge_config(&dir).unwrap().unwrap().repo_name, "menger");
    }
}

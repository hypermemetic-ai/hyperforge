//! The registered repos — `<config_dir>/ci.yaml` (HYPERFORGE-22).
//!
//! Only the repo paths are kept; each repo's pipelines are read from its
//! own `.hyperforge/pipelines.toml` at every call. Writes are atomic
//! (temp + rename), as the rest of v5's config is.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::spec::{self, Declared};

/// The file under the config dir that lists registered repos.
pub const REGISTRY_FILE: &str = "ci.yaml";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    #[serde(default)]
    repos: Vec<PathBuf>,
}

fn registry_path(config_dir: &Path) -> PathBuf {
    config_dir.join(REGISTRY_FILE)
}

/// The registered repo paths, in the order they were added.
pub fn repos(config_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let path = registry_path(config_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_yaml::from_str::<RegistryFile>(&text)
            .map(|f| f.repos)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn save(config_dir: &Path, repos: Vec<PathBuf>) -> Result<(), String> {
    let path = registry_path(config_dir);
    let text = serde_yaml::to_string(&RegistryFile { repos }).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(config_dir).map_err(|e| format!("{}: {e}", config_dir.display()))?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Expand `~/` and make the path absolute and canonical.
pub fn normalize(path: &str) -> Result<PathBuf, String> {
    let expanded = path.strip_prefix("~/").map_or_else(
        || PathBuf::from(path),
        |rest| dirs::home_dir().unwrap_or_default().join(rest),
    );
    std::fs::canonicalize(&expanded).map_err(|e| format!("{}: {e}", expanded.display()))
}

/// Every pipeline of every registered repo. A repo whose file no longer
/// reads is reported in the second list, never silently dropped.
pub fn declared(config_dir: &Path) -> Result<(Vec<Declared>, Vec<String>), String> {
    let mut out = Vec::new();
    let mut broken = Vec::new();
    for repo in repos(config_dir)? {
        match spec::load(&repo) {
            Ok(specs) => out.extend(specs.into_iter().map(|spec| Declared {
                repo: repo.clone(),
                spec,
            })),
            Err(e) => broken.push(e),
        }
    }
    Ok((out, broken))
}

/// The pipeline registered under `name`, read fresh from its repo. `-` and
/// `_` are one letter here ([`spec::same_name`]); an exact match wins.
pub fn find(config_dir: &Path, name: &str) -> Option<Declared> {
    let all = declared(config_dir).ok()?.0;
    let exact = all.iter().position(|d| d.spec.name == name);
    let at = exact.or_else(|| all.iter().position(|d| spec::same_name(&d.spec.name, name)))?;
    all.into_iter().nth(at)
}

/// Register `path`'s pipelines. Refused when its file does not parse, or
/// when one of its names is already registered from another repo.
pub fn add(config_dir: &Path, path: &str) -> Result<(PathBuf, Vec<String>), String> {
    let repo = normalize(path)?;
    let specs = spec::load(&repo)?;
    let mut repos = repos(config_dir)?;
    for other in repos.iter().filter(|r| **r != repo) {
        let Ok(theirs) = spec::load(other) else { continue };
        for p in &specs {
            if theirs.iter().any(|t| spec::same_name(&t.name, &p.name)) {
                return Err(format!(
                    "pipeline `{}` is already registered from {}; {} declares it too",
                    p.name,
                    other.display(),
                    repo.display()
                ));
            }
        }
    }
    if !repos.contains(&repo) {
        repos.push(repo.clone());
        save(config_dir, repos)?;
    }
    Ok((repo, specs.into_iter().map(|p| p.name).collect()))
}

/// Unregister `path`. Answers the pipelines it declared (as far as its file
/// still reads). Refused when it is not registered.
pub fn remove(config_dir: &Path, path: &str) -> Result<(PathBuf, Vec<String>), String> {
    let repo = normalize(path).unwrap_or_else(|_| PathBuf::from(path));
    let mut repos = repos(config_dir)?;
    let before = repos.len();
    repos.retain(|r| *r != repo);
    if repos.len() == before {
        return Err(format!("{} is not registered", repo.display()));
    }
    save(config_dir, repos)?;
    let names = spec::load(&repo)
        .map(|s| s.into_iter().map(|p| p.name).collect())
        .unwrap_or_default();
    Ok((repo, names))
}

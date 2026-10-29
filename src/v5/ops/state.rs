//! `ops::state` — shared yaml I/O, lookups, atomic mutation (V5LIFECYCLE-2).
//!
//! For now this is a re-export facade over `crate::v5::config`. The
//! implementations stay in config.rs (which also owns the type
//! definitions); everything a service needs to touch state goes through
//! one of the names here. The V5LIFECYCLE-11 checkpoint's grep
//! invariant requires that no service outside `ops/` calls yaml I/O
//! directly — `config.rs` is exempt because it's the state module's
//! implementation detail, not a service.

pub use crate::v5::config::{
    load_all, load_global, load_orgs, load_workspaces, save_global, save_org, save_workspace,
};

use crate::v5::config::{OrgConfig, OrgRepo};

/// Find a repo by name within an org.
#[must_use]
pub fn find_repo<'a>(org: &'a OrgConfig, name: &str) -> Option<&'a OrgRepo> {
    org.repos.iter().find(|r| r.name.as_str() == name)
}

/// Mutable variant.
#[must_use]
pub fn find_repo_mut<'a>(org: &'a mut OrgConfig, name: &str) -> Option<&'a mut OrgRepo> {
    org.repos.iter_mut().find(|r| r.name.as_str() == name)
}

/// Load one workspace yaml by name. `Ok(None)` when the file is absent
/// (not an error). Any other I/O or parse failure propagates.
pub fn load_workspace(
    config_dir: &std::path::Path,
    name: &str,
) -> Result<Option<crate::v5::config::WorkspaceConfig>, crate::v5::config::ConfigError> {
    let ws_dir = config_dir.join("workspaces");
    if !ws_dir.is_dir() {
        return Ok(None);
    }
    let all = load_workspaces(&ws_dir)?;
    Ok(all.into_iter().find(|(_, v)| v.name.as_str() == name).map(|(_, v)| v))
}

/// Delete `orgs/<name>.yaml`. Idempotent: returns `Ok(())` if the
/// file was already absent.
pub fn delete_org_file(
    config_dir: &std::path::Path,
    name: &str,
) -> Result<(), crate::v5::config::ConfigError> {
    let path = config_dir.join("orgs").join(format!("{name}.yaml"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(crate::v5::config::ConfigError::Io {
            file: path.display().to_string(),
            source: e,
        }),
    }
}

/// Delete `workspaces/<name>.yaml`. Idempotent.
pub fn delete_workspace_file(
    config_dir: &std::path::Path,
    name: &str,
) -> Result<(), crate::v5::config::ConfigError> {
    let path = config_dir.join("workspaces").join(format!("{name}.yaml"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(crate::v5::config::ConfigError::Io {
            file: path.display().to_string(),
            source: e,
        }),
    }
}

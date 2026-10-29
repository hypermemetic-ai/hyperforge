//! Org-level configuration (~/.config/hyperforge/orgs/{org}.toml)
//!
//! Stores org-wide defaults like SSH keys per forge. Per-repo config
//! can override these, but this provides a sensible default so every
//! `repo init` doesn't need `--ssh-keys`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Org-level configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrgConfig {
    /// SSH key paths per forge (`forge_name` -> `key_path`)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub ssh: HashMap<String, String>,

    /// Workspace path for this org's repos
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
}

/// Typed record of SSH key paths by forge.
///
/// This is the RPC-parameter shape for `orgs_add` and `orgs_update`. Each
/// field corresponds to a known forge. The closed set of forges is
/// expressed by the struct's field set — unknown forges are rejected at
/// the CLI parse layer (Synapse), before the call reaches the service. To
/// add support for a new forge, add a field here.
///
/// `JsonSchema` is required so Synapse can emit per-forge CLI flags
/// (e.g. `--ssh.github <path>`) instead of a single JSON-blob flag.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct OrgSshKeys {
    /// Path to the GitHub SSH private key for this org
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,

    /// Path to the Codeberg SSH private key for this org
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codeberg: Option<String>,

    /// Path to the GitLab SSH private key for this org
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gitlab: Option<String>,
}

impl OrgSshKeys {
    /// True if every field is None.
    pub fn is_empty(&self) -> bool {
        self.github.is_none() && self.codeberg.is_none() && self.gitlab.is_none()
    }

    /// Yield the set fields as (forge_name, key_path) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("github", self.github.as_deref()),
            ("codeberg", self.codeberg.as_deref()),
            ("gitlab", self.gitlab.as_deref()),
        ]
        .into_iter()
        .filter_map(|(forge, val)| val.map(|v| (forge, v)))
    }

    /// Convert into a `HashMap<String, String>` of the set fields.
    pub fn into_map(self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if let Some(v) = self.github {
            out.insert("github".to_string(), v);
        }
        if let Some(v) = self.codeberg {
            out.insert("codeberg".to_string(), v);
        }
        if let Some(v) = self.gitlab {
            out.insert("gitlab".to_string(), v);
        }
        out
    }
}

impl OrgConfig {
    /// Path to the org config file: ~/.config/hyperforge/orgs/{org}.toml
    pub fn config_path(config_dir: &Path, org: &str) -> PathBuf {
        config_dir.join("orgs").join(format!("{org}.toml"))
    }

    /// Load org config from disk. Returns default if file doesn't exist.
    pub fn load(config_dir: &Path, org: &str) -> Self {
        let path = Self::config_path(config_dir, org);
        match std::fs::read_to_string(&path) {
            Ok(content) => toml::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save org config to disk.
    pub fn save(&self, config_dir: &Path, org: &str) -> Result<(), String> {
        let path = Self::config_path(config_dir, org);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create org config dir: {e}"))?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize org config: {e}"))?;
        std::fs::write(&path, content)
            .map_err(|e| format!("Failed to write org config: {e}"))?;
        Ok(())
    }

    /// Check if any SSH keys are configured
    pub fn has_ssh_keys(&self) -> bool {
        !self.ssh.is_empty()
    }

    /// Get SSH key for a specific forge
    pub fn ssh_key_for_forge(&self, forge: &str) -> Option<&str> {
        self.ssh.get(forge).map(std::string::String::as_str)
    }

    /// Directory for generated SSH keys: ~/.config/hyperforge/orgs/{org}/keys/
    pub fn keys_dir(config_dir: &Path, org: &str) -> PathBuf {
        config_dir.join("orgs").join(org).join("keys")
    }

    /// Path to a generated SSH key for an org/forge: keys/{forge}_ed25519
    pub fn ssh_key_path(config_dir: &Path, org: &str, forge: &str) -> PathBuf {
        Self::keys_dir(config_dir, org).join(format!("{forge}_ed25519"))
    }

    /// Generate an ed25519 SSH keypair for an org/forge combination.
    /// Returns the path to the private key. Idempotent — reuses existing keys.
    pub fn generate_ssh_key(config_dir: &Path, org: &str, forge: &str) -> Result<PathBuf, String> {
        let key_path = Self::ssh_key_path(config_dir, org, forge);

        // Idempotent: if key already exists, return it
        if key_path.exists() {
            return Ok(key_path);
        }

        let keys_dir = Self::keys_dir(config_dir, org);
        std::fs::create_dir_all(&keys_dir)
            .map_err(|e| format!("Failed to create keys dir {}: {}", keys_dir.display(), e))?;

        let comment = format!("hyperforge:{org}@{forge}");
        let output = std::process::Command::new("ssh-keygen")
            .args([
                "-t", "ed25519",
                "-f", &key_path.to_string_lossy(),
                "-N", "",
                "-C", &comment,
            ])
            .output()
            .map_err(|e| format!("Failed to run ssh-keygen: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("ssh-keygen failed: {}", stderr.trim()));
        }

        Ok(key_path)
    }

    /// Read the public key contents for an org/forge SSH key.
    pub fn read_public_key(config_dir: &Path, org: &str, forge: &str) -> Result<String, String> {
        let key_path = Self::ssh_key_path(config_dir, org, forge);
        // ssh-keygen creates {name}.pub, so for github_ed25519 it's github_ed25519.pub
        let pub_path = PathBuf::from(format!("{}.pub", key_path.display()));
        std::fs::read_to_string(&pub_path)
            .map(|s| s.trim().to_string())
            .map_err(|e| format!("Failed to read public key {}: {}", pub_path.display(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_load_missing_returns_default() {
        let tmp = TempDir::new().unwrap();
        let config = OrgConfig::load(tmp.path(), "nonexistent");
        assert!(config.ssh.is_empty());
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let mut config = OrgConfig::default();
        config.ssh.insert("github".to_string(), "~/.ssh/gh_key".to_string());
        config.ssh.insert("codeberg".to_string(), "~/.ssh/cb_key".to_string());

        config.save(tmp.path(), "myorg").unwrap();
        let loaded = OrgConfig::load(tmp.path(), "myorg");

        assert_eq!(loaded.ssh.get("github").unwrap(), "~/.ssh/gh_key");
        assert_eq!(loaded.ssh.get("codeberg").unwrap(), "~/.ssh/cb_key");
    }

    #[test]
    fn test_has_ssh_keys() {
        let empty = OrgConfig::default();
        assert!(!empty.has_ssh_keys());

        let mut with_keys = OrgConfig::default();
        with_keys.ssh.insert("github".to_string(), "~/.ssh/key".to_string());
        assert!(with_keys.has_ssh_keys());
    }

    #[test]
    fn test_workspace_path_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config = OrgConfig {
            workspace_path: Some("/home/user/workspace".to_string()),
            ..OrgConfig::default()
        };
        config.save(tmp.path(), "myorg").unwrap();

        let loaded = OrgConfig::load(tmp.path(), "myorg");
        assert_eq!(loaded.workspace_path.as_deref(), Some("/home/user/workspace"));
    }

    #[test]
    fn test_workspace_path_absent_by_default() {
        let tmp = TempDir::new().unwrap();
        let config = OrgConfig::load(tmp.path(), "nonexistent");
        assert!(config.workspace_path.is_none());
    }

    #[test]
    fn test_generate_ssh_key() {
        let tmp = TempDir::new().unwrap();
        let key_path = OrgConfig::generate_ssh_key(tmp.path(), "testorg", "github").unwrap();

        assert!(key_path.exists(), "Private key should exist");
        let pub_path = PathBuf::from(format!("{}.pub", key_path.display()));
        assert!(pub_path.exists(), "Public key should exist");

        // Verify the key path is where we expect
        let expected = OrgConfig::ssh_key_path(tmp.path(), "testorg", "github");
        assert_eq!(key_path, expected);
    }

    #[test]
    fn test_generate_ssh_key_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path1 = OrgConfig::generate_ssh_key(tmp.path(), "testorg", "github").unwrap();
        let content1 = std::fs::read_to_string(&path1).unwrap();

        let path2 = OrgConfig::generate_ssh_key(tmp.path(), "testorg", "github").unwrap();
        let content2 = std::fs::read_to_string(&path2).unwrap();

        assert_eq!(path1, path2);
        assert_eq!(content1, content2, "Key should not be regenerated");
    }

    #[test]
    fn test_read_public_key() {
        let tmp = TempDir::new().unwrap();
        OrgConfig::generate_ssh_key(tmp.path(), "testorg", "github").unwrap();

        let pubkey = OrgConfig::read_public_key(tmp.path(), "testorg", "github").unwrap();
        assert!(pubkey.starts_with("ssh-ed25519 "), "Should be an ed25519 public key");
        assert!(pubkey.contains("hyperforge:testorg@github"), "Should contain the comment");
    }

    #[test]
    fn test_read_public_key_missing() {
        let tmp = TempDir::new().unwrap();
        let result = OrgConfig::read_public_key(tmp.path(), "testorg", "github");
        assert!(result.is_err());
    }
}

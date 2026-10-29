//! Auth plugin types

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Hierarchical secret path: forge/org/key or registry/token
///
/// Examples:
/// - "github/alice/token"
/// - "codeberg/acme-corp/token"
/// - "cargo/token"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SecretPath {
    pub path: String,
}

impl SecretPath {
    /// Parse from string
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
        }
    }

    /// Build from components (forge, org, key)
    pub fn from_parts(forge: &str, org: &str, key: &str) -> Self {
        Self {
            path: format!("{forge}/{org}/{key}"),
        }
    }

    /// Build from registry token path
    pub fn registry(registry: &str) -> Self {
        Self {
            path: format!("{registry}/token"),
        }
    }

    /// Get the path as a string
    pub fn as_str(&self) -> &str {
        &self.path
    }

    /// Parse segments
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('/').collect()
    }
}

impl From<String> for SecretPath {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for SecretPath {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl std::fmt::Display for SecretPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path)
    }
}

/// A secret value with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Secret {
    pub path: SecretPath,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Secret {
    pub fn new(path: impl Into<SecretPath>, value: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            path: path.into(),
            value: value.into(),
            created_at: Some(now),
            updated_at: Some(now),
        }
    }

    pub fn with_timestamps(
        path: impl Into<SecretPath>,
        value: impl Into<String>,
        created_at: Option<DateTime<Utc>>,
        updated_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            path: path.into(),
            value: value.into(),
            created_at,
            updated_at,
        }
    }
}

/// Secret metadata without the value (for listing)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretInfo {
    pub path: SecretPath,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl From<&Secret> for SecretInfo {
    fn from(secret: &Secret) -> Self {
        Self {
            path: secret.path.clone(),
            created_at: secret.created_at,
            updated_at: secret.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_path_from_parts() {
        let path = SecretPath::from_parts("github", "alice", "token");
        assert_eq!(path.as_str(), "github/alice/token");
    }

    #[test]
    fn test_secret_path_registry() {
        let path = SecretPath::registry("cargo");
        assert_eq!(path.as_str(), "cargo/token");
    }

    #[test]
    fn test_secret_path_segments() {
        let path = SecretPath::from_parts("github", "alice", "token");
        assert_eq!(path.segments(), vec!["github", "alice", "token"]);
    }

    #[test]
    fn test_secret_new() {
        let secret = Secret::new("github/alice/token", "ghp_xxx");
        assert_eq!(secret.path.as_str(), "github/alice/token");
        assert_eq!(secret.value, "ghp_xxx");
        assert!(secret.created_at.is_some());
        assert!(secret.updated_at.is_some());
    }

    #[test]
    fn test_secret_info_from_secret() {
        let secret = Secret::new("github/alice/token", "ghp_xxx");
        let info: SecretInfo = (&secret).into();
        assert_eq!(info.path, secret.path);
        assert_eq!(info.created_at, secret.created_at);
    }
}

//! YAML-based secret storage

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use thiserror::Error;

use super::types::{Secret, SecretInfo, SecretPath};

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Secret not found: {path}")]
    NotFound { path: String },

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("YAML error: {0}")]
    YamlError(#[from] serde_yaml::Error),

    #[error("Lock poisoned: {0}")]
    LockPoisoned(String),
}

pub type StorageResult<T> = Result<T, StorageError>;

/// YAML storage format
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StorageYaml {
    #[serde(default)]
    secrets: HashMap<String, SecretEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SecretEntry {
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<DateTime<Utc>>,
}

/// YAML-based secret storage
#[derive(Clone)]
pub struct YamlStorage {
    /// Path to secrets.yaml
    file_path: PathBuf,
    /// In-memory cache of secrets
    secrets: Arc<RwLock<HashMap<String, SecretEntry>>>,
}

impl YamlStorage {
    /// Create a new YAML storage at the given path
    pub fn new(file_path: PathBuf) -> Self {
        Self {
            file_path,
            secrets: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create storage at default location (~/.config/hyperforge/secrets.yaml)
    pub fn default_location() -> StorageResult<Self> {
        let config_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("hyperforge");

        let file_path = config_dir.join("secrets.yaml");

        Ok(Self::new(file_path))
    }

    /// Load secrets from YAML file
    pub async fn load(&self) -> StorageResult<()> {
        if !self.file_path.exists() {
            // No file yet, start with empty state
            return Ok(());
        }

        let content = tokio::fs::read_to_string(&self.file_path).await?;
        let storage: StorageYaml = serde_yaml::from_str(&content)?;

        let mut secrets = self.secrets.write().map_err(|e| {
            StorageError::LockPoisoned(e.to_string())
        })?;

        *secrets = storage.secrets;

        Ok(())
    }

    /// Save secrets to YAML file
    pub async fn save(&self) -> StorageResult<()> {
        // Clone data while holding lock, then release before async operations
        let secrets_clone = {
            let secrets = self.secrets.read().map_err(|e| {
                StorageError::LockPoisoned(e.to_string())
            })?;
            secrets.clone()
        };

        let storage = StorageYaml {
            secrets: secrets_clone,
        };

        let yaml = serde_yaml::to_string(&storage)?;

        // Ensure parent directory exists
        if let Some(parent) = self.file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&self.file_path, yaml).await?;

        Ok(())
    }

    /// Get a secret by path
    pub fn get(&self, path: &SecretPath) -> StorageResult<Secret> {
        let secrets = self.secrets.read().map_err(|e| {
            StorageError::LockPoisoned(e.to_string())
        })?;

        let entry = secrets.get(path.as_str()).ok_or_else(|| {
            StorageError::NotFound {
                path: path.to_string(),
            }
        })?;

        Ok(Secret::with_timestamps(
            path.clone(),
            entry.value.clone(),
            entry.created_at,
            entry.updated_at,
        ))
    }

    /// Set a secret
    pub async fn set(&self, secret: Secret) -> StorageResult<()> {
        let now = Utc::now();

        let entry = SecretEntry {
            value: secret.value,
            created_at: secret.created_at.or(Some(now)),
            updated_at: Some(now),
        };

        {
            let mut secrets = self.secrets.write().map_err(|e| {
                StorageError::LockPoisoned(e.to_string())
            })?;

            secrets.insert(secret.path.to_string(), entry);
        }

        // Save to disk
        self.save().await?;

        Ok(())
    }

    /// Delete a secret
    pub async fn delete(&self, path: &SecretPath) -> StorageResult<()> {
        {
            let mut secrets = self.secrets.write().map_err(|e| {
                StorageError::LockPoisoned(e.to_string())
            })?;

            secrets.remove(path.as_str()).ok_or_else(|| {
                StorageError::NotFound {
                    path: path.to_string(),
                }
            })?;
        }

        // Save to disk
        self.save().await?;

        Ok(())
    }

    /// List secrets matching a prefix
    pub fn list(&self, prefix: &str) -> StorageResult<Vec<SecretInfo>> {
        let secrets = self.secrets.read().map_err(|e| {
            StorageError::LockPoisoned(e.to_string())
        })?;

        let mut result = Vec::new();

        for (path_str, entry) in secrets.iter() {
            if path_str.starts_with(prefix) {
                result.push(SecretInfo {
                    path: SecretPath::new(path_str.clone()),
                    created_at: entry.created_at,
                    updated_at: entry.updated_at,
                });
            }
        }

        Ok(result)
    }

    /// Check if a secret exists
    pub fn exists(&self, path: &SecretPath) -> StorageResult<bool> {
        let secrets = self.secrets.read().map_err(|e| {
            StorageError::LockPoisoned(e.to_string())
        })?;

        Ok(secrets.contains_key(path.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_storage_set_and_get() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("secrets.yaml");
        let storage = YamlStorage::new(file_path.clone());

        // Set a secret
        let secret = Secret::new("github/alice/token", "ghp_xxx");
        storage.set(secret.clone()).await.unwrap();

        // Get it back
        let retrieved = storage.get(&secret.path).unwrap();
        assert_eq!(retrieved.value, "ghp_xxx");
        assert_eq!(retrieved.path, secret.path);

        // File should exist
        assert!(file_path.exists());
    }

    #[tokio::test]
    async fn test_storage_persistence() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("secrets.yaml");

        // Create storage, set secret, drop
        {
            let storage = YamlStorage::new(file_path.clone());
            let secret = Secret::new("github/alice/token", "ghp_xxx");
            storage.set(secret).await.unwrap();
        }

        // Create new storage, load from file
        let storage2 = YamlStorage::new(file_path);
        storage2.load().await.unwrap();

        let retrieved = storage2.get(&SecretPath::new("github/alice/token")).unwrap();
        assert_eq!(retrieved.value, "ghp_xxx");
    }

    #[tokio::test]
    async fn test_storage_delete() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("secrets.yaml");
        let storage = YamlStorage::new(file_path);

        // Set a secret
        let secret = Secret::new("github/alice/token", "ghp_xxx");
        storage.set(secret.clone()).await.unwrap();

        // Delete it
        storage.delete(&secret.path).await.unwrap();

        // Should not exist
        assert!(!storage.exists(&secret.path).unwrap());
    }

    #[tokio::test]
    async fn test_storage_list() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("secrets.yaml");
        let storage = YamlStorage::new(file_path);

        // Set multiple secrets
        storage.set(Secret::new("github/alice/token", "ghp_xxx")).await.unwrap();
        storage.set(Secret::new("github/bob/token", "ghp_yyy")).await.unwrap();
        storage.set(Secret::new("codeberg/alice/token", "cb_zzz")).await.unwrap();

        // List github secrets
        let github_secrets = storage.list("github/").unwrap();
        assert_eq!(github_secrets.len(), 2);

        // List all alice secrets
        let alice_secrets = storage.list("").unwrap();
        assert_eq!(alice_secrets.len(), 3);
    }
}

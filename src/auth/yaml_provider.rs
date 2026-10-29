//! YAML-file auth provider
//!
//! Reads secrets directly from ~/.config/hyperforge/secrets.yaml.

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

use super::AuthProvider;

/// A single secret entry in the YAML file
#[derive(Debug, Deserialize)]
struct SecretEntry {
    value: String,
}

/// Top-level secrets file structure
#[derive(Debug, Deserialize)]
struct SecretsFile {
    secrets: HashMap<String, SecretEntry>,
}

/// Auth provider that reads secrets directly from YAML on disk
pub struct YamlAuthProvider {
    secrets_path: PathBuf,
}

impl YamlAuthProvider {
    /// Create a new YAML auth provider using the default secrets path
    pub fn new() -> anyhow::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
        Ok(Self {
            secrets_path: home.join(".config/hyperforge/secrets.yaml"),
        })
    }

    /// Create with a custom secrets file path
    pub const fn with_path(path: PathBuf) -> Self {
        Self {
            secrets_path: path,
        }
    }
}

#[async_trait]
impl AuthProvider for YamlAuthProvider {
    async fn get_secret(&self, key: &str) -> anyhow::Result<Option<String>> {
        let content = match std::fs::read_to_string(&self.secrets_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(anyhow::anyhow!(
                    "Secrets file not found at {}. Run the secrets service to configure tokens.",
                    self.secrets_path.display()
                ));
            }
            Err(e) => return Err(e.into()),
        };

        let file: SecretsFile = serde_yaml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", self.secrets_path.display(), e))?;

        Ok(file.secrets.get(key).map(|entry| entry.value.clone()))
    }
}

//! Embedded secret store — resolves `secrets://<path>` references.
//!
//! V5CORE-4 pins the `SecretResolver` capability: one fallible lookup
//! keyed on a `SecretRef`. v1 backend is a YAML file at
//! `$HF_CONFIG/secrets.yaml`; future backends (OS keyring, remote KMS)
//! slot in without touching callers.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Typed `secrets://<path>` reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    /// Validate and wrap a raw string. Rejects anything that isn't
    /// `secrets://` followed by a non-empty path portion.
    pub fn parse(s: &str) -> Result<Self, SecretError> {
        let path = s
            .strip_prefix("secrets://")
            .ok_or_else(|| SecretError::InvalidRef(s.to_string()))?;
        if path.is_empty() {
            return Err(SecretError::InvalidRef(s.to_string()));
        }
        Ok(Self(s.to_string()))
    }

    /// The `<path>` portion after the `secrets://` prefix.
    #[must_use]
    pub fn path(&self) -> &str {
        self.0.strip_prefix("secrets://").unwrap_or(&self.0)
    }

    /// The full `secrets://<path>` string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Errors emitted by the secret store.
#[derive(Debug, Error)]
pub enum SecretError {
    /// Reference doesn't match `secrets://<path>` (rejected before any I/O).
    #[error("invalid secret reference: {0}")]
    InvalidRef(String),
    /// Reference is valid but nothing is stored under that path.
    #[error("secret not found: {0}")]
    NotFound(String),
    /// The backing YAML file exists but can't be parsed.
    #[error("corrupted secret store {file}: {reason}")]
    Corrupt { file: String, reason: String },
    /// A key is present but its value isn't a string.
    #[error("non-string value for key '{0}' in {1}")]
    BadValue(String, String),
    /// Low-level I/O error reading the file.
    #[error("I/O error reading {file}: {source}")]
    Io {
        file: String,
        #[source]
        source: std::io::Error,
    },
}

impl SecretError {
    /// Wire-side error discriminator (`snake_case`).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidRef(_) => "invalid_ref",
            Self::NotFound(_) => "not_found",
            Self::Corrupt { .. } => "corrupt_store",
            Self::BadValue(..) => "bad_value",
            Self::Io { .. } => "io_error",
        }
    }
}

/// Capability: turn a `SecretRef` into a plaintext secret.
///
/// Implementers SHOULD be safe to share across threads and SHOULD NOT
/// log the returned plaintext. Callers MUST NOT surface resolved values
/// through any response type listed in `CONTRACTS §types` (see the
/// secret-redaction rule).
pub trait SecretResolver: Send + Sync {
    /// Resolve the given reference or return a typed error.
    fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError>;
}

/// YAML-backed implementation (v1). Reads `<config_dir>/secrets.yaml`
/// on each call — secrets are not cached, so edits are picked up
/// without restarting the daemon.
#[derive(Debug, Clone)]
pub struct YamlSecretStore {
    file: PathBuf,
}

impl YamlSecretStore {
    /// Construct a YAML-backed store rooted at the given config dir.
    #[must_use]
    pub fn new(config_dir: &Path) -> Self {
        Self {
            file: config_dir.join("secrets.yaml"),
        }
    }

    fn file_display(&self) -> String {
        // Always surface the basename + full path so error messages
        // satisfy the V5CORE-4 "error message names secrets.yaml"
        // invariant.
        self.file.display().to_string()
    }
}

impl YamlSecretStore {
    /// V5PARITY-7: write a secret. Atomic via tempfile + rename.
    pub fn put_secret(&self, reference: &SecretRef, value: &str) -> Result<(), SecretError> {
        let mut map = self.load_map()?;
        map.insert(
            serde_yaml::Value::String(reference.path().to_string()),
            serde_yaml::Value::String(value.to_string()),
        );
        self.save_map(map)
    }

    /// V5PARITY-7: delete a secret. Idempotent on missing.
    pub fn delete_secret(&self, reference: &SecretRef) -> Result<bool, SecretError> {
        let mut map = self.load_map()?;
        let existed = map
            .remove(&serde_yaml::Value::String(reference.path().to_string()))
            .is_some();
        self.save_map(map)?;
        Ok(existed)
    }

    /// V5PARITY-7: list every reference key. Values are NEVER returned.
    pub fn list_refs(&self) -> Result<Vec<SecretRef>, SecretError> {
        let map = self.load_map()?;
        let mut out = Vec::new();
        for (k, _) in map {
            if let serde_yaml::Value::String(s) = k {
                let full = format!("secrets://{s}");
                if let Ok(r) = SecretRef::parse(&full) {
                    out.push(r);
                }
            }
        }
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }

    fn load_map(&self) -> Result<serde_yaml::Mapping, SecretError> {
        let contents = match fs::read_to_string(&self.file) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(serde_yaml::Mapping::new()),
            Err(e) => return Err(SecretError::Io { file: self.file_display(), source: e }),
        };
        if contents.trim().is_empty() {
            return Ok(serde_yaml::Mapping::new());
        }
        let parsed: serde_yaml::Value = serde_yaml::from_str(&contents).map_err(|e| {
            SecretError::Corrupt {
                file: format!("secrets.yaml ({})", self.file_display()),
                reason: e.to_string(),
            }
        })?;
        match parsed {
            serde_yaml::Value::Mapping(m) => Ok(m),
            serde_yaml::Value::Null => Ok(serde_yaml::Mapping::new()),
            _ => Err(SecretError::Corrupt {
                file: format!("secrets.yaml ({})", self.file_display()),
                reason: "top-level value is not a mapping".to_string(),
            }),
        }
    }

    fn save_map(&self, map: serde_yaml::Mapping) -> Result<(), SecretError> {
        let body = serde_yaml::to_string(&serde_yaml::Value::Mapping(map))
            .map_err(|e| SecretError::Corrupt {
                file: self.file_display(),
                reason: e.to_string(),
            })?;
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent).map_err(|e| SecretError::Io {
                file: parent.display().to_string(),
                source: e,
            })?;
        }
        let tmp = self.file.with_extension("yaml.tmp");
        write_private(&tmp, body.as_bytes()).map_err(|e| SecretError::Io {
            file: tmp.display().to_string(),
            source: e,
        })?;
        fs::rename(&tmp, &self.file).map_err(|e| SecretError::Io {
            file: self.file.display().to_string(),
            source: e,
        })?;
        Ok(())
    }
}

impl SecretResolver for YamlSecretStore {
    fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        // Load the file. Missing → empty store (all lookups not-found).
        let contents = match fs::read_to_string(&self.file) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SecretError::NotFound(reference.as_str().to_string()));
            }
            Err(e) => {
                return Err(SecretError::Io {
                    file: self.file_display(),
                    source: e,
                });
            }
        };

        // Parse YAML. Use serde_yaml::Value first so we can diagnose
        // unexpected value shapes precisely.
        let parsed: serde_yaml::Value = serde_yaml::from_str(&contents).map_err(|e| {
            SecretError::Corrupt {
                file: format!("secrets.yaml ({})", self.file_display()),
                reason: e.to_string(),
            }
        })?;

        let map = match parsed {
            serde_yaml::Value::Mapping(m) => m,
            serde_yaml::Value::Null => {
                // Empty document → not-found.
                return Err(SecretError::NotFound(reference.as_str().to_string()));
            }
            _ => {
                return Err(SecretError::Corrupt {
                    file: format!("secrets.yaml ({})", self.file_display()),
                    reason: "top-level value is not a mapping".to_string(),
                });
            }
        };

        let key = reference.path();
        let key_yaml = serde_yaml::Value::String(key.to_string());
        match map.get(&key_yaml) {
            None => Err(SecretError::NotFound(reference.as_str().to_string())),
            Some(serde_yaml::Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(SecretError::BadValue(
                key.to_string(),
                format!("secrets.yaml ({})", self.file_display()),
            )),
        }
    }
}

/// Write `bytes` to `path`, readable by its owner alone. The file replaces
/// the secrets file by rename, so its mode is the secrets file's mode.
/// `set_permissions` covers a stale tmp left by a crash at a wider mode,
/// which `mode` on open would not change.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::write(path, bytes)
}

#[cfg(all(test, unix))]
mod private_file_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_write_leaves_the_file_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.yaml");
        fs::write(&path, "secrets: {}\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        // A crash's stale tmp, at the wider mode, must not pass its mode on.
        let tmp = path.with_extension("yaml.tmp");
        fs::write(&tmp, "stale").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();

        let store = YamlSecretStore::new(dir.path());
        store
            .put_secret(&SecretRef::parse("secrets://npm/token").unwrap(), "value")
            .unwrap();

        assert_eq!(mode(&path), 0o600);
    }
}

//! `SecretsService` — V5PARITY-7. Wire surface for the secret store.
//!
//! Child activation on `HyperforgeService`. Methods write/list/delete
//! `secrets://...` refs in `$HF_CONFIG/secrets.yaml`. Resolved values
//! NEVER appear in any event payload — D9 secret-redaction rule.

use std::path::PathBuf;

use async_stream::stream;
use futures::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v5::secrets::{SecretRef, YamlSecretStore};

/// Per-service event surface.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SecretsEvent {
    /// `secrets.set` ack. `value_length` is the masked indicator —
    /// the value itself is never serialized.
    SecretSet {
        key: String,
        value_length: u32,
    },
    /// One ref from `secrets.list_refs`.
    SecretRef {
        key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        type_hint: Option<String>,
    },
    /// `secrets.delete` ack. `existed: true` means the ref was present
    /// before the delete; `false` means the call was a no-op.
    SecretDeleted {
        key: String,
        existed: bool,
    },
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
    },
}

#[derive(Clone)]
pub struct SecretsService {
    config_dir: PathBuf,
}

impl SecretsService {
    #[must_use]
    pub const fn new(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }
}

#[plexus_macros::activation(
    trait = "SecretsApi",
    methods = marked,
    namespace = "secrets",
    description = "v5 secret store wire surface (V5PARITY-7)",
    crate_path = "plexus_core"
)]
impl SecretsService {
    /// Write or replace a secret. Returns the key + masked length.
    #[plexus_macros::method(params(
        key = "secrets://<path> reference",
        value = "Plaintext to store (masked in events)"
    ))]
    pub async fn set(
        &self,
        key: String,
        value: String,
    ) -> impl Stream<Item = SecretsEvent> + Send + 'static {
        let store = YamlSecretStore::new(&self.config_dir);
        stream! {
            let parsed = match SecretRef::parse(&key) {
                Ok(r) => r,
                Err(e) => {
                    yield SecretsEvent::Error {
                        code: Some(e.code().into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
            match store.put_secret(&parsed, &value) {
                Ok(()) => yield SecretsEvent::SecretSet {
                    key: parsed.as_str().to_string(),
                    value_length: len,
                },
                Err(e) => yield SecretsEvent::Error {
                    code: Some(e.code().into()),
                    message: e.to_string(),
                },
            }
        }
    }

    /// Stream every stored ref (keys only — values never leave disk).
    #[plexus_macros::method]
    pub async fn list_refs(&self) -> impl Stream<Item = SecretsEvent> + Send + 'static {
        let store = YamlSecretStore::new(&self.config_dir);
        stream! {
            match store.list_refs() {
                Ok(refs) => {
                    for r in refs {
                        let key = r.as_str().to_string();
                        // Naive type hint from key shape: `*/token` → token, `*/ssh_key` → ssh_key, else None.
                        let type_hint = if key.ends_with("/token") {
                            Some("token".to_string())
                        } else if key.ends_with("/ssh_key") {
                            Some("ssh_key".to_string())
                        } else { None };
                        yield SecretsEvent::SecretRef { key, type_hint };
                    }
                }
                Err(e) => yield SecretsEvent::Error {
                    code: Some(e.code().into()),
                    message: e.to_string(),
                },
            }
        }
    }

    /// Remove a secret. Idempotent — `existed: false` if it wasn't there.
    #[plexus_macros::method(params(key = "secrets://<path> reference"))]
    pub async fn delete(
        &self,
        key: String,
    ) -> impl Stream<Item = SecretsEvent> + Send + 'static {
        let store = YamlSecretStore::new(&self.config_dir);
        stream! {
            let parsed = match SecretRef::parse(&key) {
                Ok(r) => r,
                Err(e) => {
                    yield SecretsEvent::Error {
                        code: Some(e.code().into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            match store.delete_secret(&parsed) {
                Ok(existed) => yield SecretsEvent::SecretDeleted {
                    key: parsed.as_str().to_string(),
                    existed,
                },
                Err(e) => yield SecretsEvent::Error {
                    code: Some(e.code().into()),
                    message: e.to_string(),
                },
            }
        }
    }
}

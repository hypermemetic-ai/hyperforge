//! `HyperforgeHub` (v5) — root activation for the v5 rewrite.
//!
//! V5CORE-2 scaffolded the hub with a placeholder `status` returning
//! only `version`. V5CORE-5 pins the full `StatusEvent` shape
//! (`version` + `config_dir`). V5CORE-6/7/8 attach child stubs,
//! V5CORE-4 adds `resolve_secret`.
//!
//! plexus-macros 0.5 rejects activations with zero `#[method]`
//! functions, so `status` ships from V5CORE-2 onwards.

use std::path::PathBuf;
use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::v5::ci::{CiHub, CiStore};
use crate::v5::orgs::OrgsHub;
use crate::v5::repos::ReposHub;
use crate::v5::secrets::{SecretRef, SecretResolver, YamlSecretStore};
use crate::v5::workspaces::WorkspacesHub;

/// Events emitted by the v5 root hub.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HyperforgeV5Event {
    /// Daemon self-report. `version` is the crate version; `config_dir`
    /// is the absolute, expanded config directory in use (V5CORE-5).
    /// V5PARITY-26: `onboarding_hint` is populated only when the
    /// config dir is empty (no orgs, no workspaces) so first-time
    /// users see a pointer to `begin` without having to know it exists.
    Status {
        version: String,
        config_dir: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        onboarding_hint: Option<String>,
    },
    /// Secret-resolve success (V5CORE-4). Carries the plaintext value
    /// under `.value`. Only emitted by `resolve_secret`; the redaction
    /// rule from CONTRACTS §types prohibits every other method from
    /// including resolved values.
    SecretResolved { value: String },
    /// Generic error event.
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
    },
    /// V5PARITY-7: per-credential probe result from `auth_check`.
    /// `valid: true` = the cred resolves AND the corresponding adapter
    /// can reach a known endpoint with it. `false` = either the secret
    /// is missing OR the API call fails. `message` is the diagnostic.
    AuthCheckResult {
        org: String,
        key: String,
        provider: String,
        valid: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// V5PARITY-7: a credential the org expects given its forge config.
    /// Tells callers what `secrets.set` calls would unblock the org.
    AuthRequirement {
        org: String,
        provider: String,
        key: String,
        cred_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        present: Option<bool>,
    },
    // V5PARITY-8: CLI events.
    ReloadDone {
        orgs: u32,
        workspaces: u32,
        secrets_refs: u32,
    },
    ConfigShow {
        provider_map: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        default_workspace: Option<String>,
    },
    SshKeySet {
        org: String,
        forge: String,
        path: String,
    },
    SshKeyShow {
        org: String,
        forge: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    BeginNextStep {
        action: String,
        message: String,
    },
    // V5PARITY-23 auth-discovery events.
    /// Static (no-org) scope/permission requirements for a provider.
    /// Emitted by `auth_requirements_for`.
    AuthRequirementsFor {
        provider: String,
        required_scopes: Vec<String>,
        recommended_scopes: Vec<String>,
        optional_scopes: Vec<String>,
    },
    /// External CLI auth detected on the host. Emitted by
    /// `auth_detect_external`. Token values NEVER appear in this
    /// payload; only metadata.
    ExternalAuthDetected {
        provider: String,
        source: String, // "gh-cli" | "glab-cli" | etc.
        logged_in: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        scopes: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        accessible_orgs: Option<Vec<String>>,
    },
}

/// Root activation for hyperforge v5.
#[derive(Clone)]
pub struct HyperforgeHub {
    state: Arc<HubState>,
}

/// Shared read-only state the root hub threads into methods.
#[derive(Debug)]
pub struct HubState {
    /// Absolute, expanded config directory.
    pub config_dir: PathBuf,
    /// HYPERFORGE-22: pipeline runs, kept under the state dir. One store
    /// per hub, so running stages outlive the call that started them.
    pub ci: Arc<CiStore>,
}

impl HyperforgeHub {
    /// Construct a hub rooted at the given config directory. Pipeline runs
    /// are kept under `<config_dir>/state`; the daemon passes the real state
    /// dir through [`HyperforgeHub::with_state_dir`].
    #[must_use]
    pub fn new(config_dir: PathBuf) -> Self {
        let state_dir = config_dir.join("state");
        Self::with_state_dir(config_dir, &state_dir)
    }

    /// Construct a hub with an explicit state dir (HYPERFORGE-22: where
    /// pipeline runs, attempts, outputs and logs are kept). Opening it fails
    /// every stage a previous process left running.
    #[must_use]
    pub fn with_state_dir(config_dir: PathBuf, state_dir: &std::path::Path) -> Self {
        let ci = Arc::new(CiStore::open(state_dir));
        Self {
            state: Arc::new(HubState { config_dir, ci }),
        }
    }
}

/// Hyperforge v5 root — minimal scaffold.
#[plexus_macros::activation(
    namespace = "hyperforge",
    description = "Hyperforge v5 root",
    crate_path = "plexus_core"
)]
impl HyperforgeHub {
    /// Orgs namespace — CRUD + credentials. Methods attached by V5ORGS.
    #[plexus_macros::child]
    fn orgs(&self) -> OrgsHub {
        OrgsHub::new(self.state.config_dir.clone())
    }

    /// Repos namespace — CRUD + `ForgePort`. Methods attached by V5REPOS.
    #[plexus_macros::child]
    fn repos(&self) -> ReposHub {
        ReposHub::with_config_dir(self.state.config_dir.clone())
    }

    /// Workspaces namespace — CRUD + reconcile + sync. Methods attached by V5WS.
    #[plexus_macros::child]
    fn workspaces(&self) -> WorkspacesHub {
        WorkspacesHub::new(self.state.config_dir.clone())
    }

    /// Secrets namespace — V5PARITY-7. RPC surface over the YAML store.
    #[plexus_macros::child]
    fn secrets(&self) -> crate::v5::secrets_hub::SecretsHub {
        crate::v5::secrets_hub::SecretsHub::new(self.state.config_dir.clone())
    }

    /// Build namespace — V5PARITY-9/10/11. Manifest, release, dist, exec.
    #[plexus_macros::child]
    fn build(&self) -> crate::v5::build_hub::BuildHub {
        crate::v5::build_hub::BuildHub::new(self.state.config_dir.clone())
    }

    /// CI namespace — HYPERFORGE-22. Pipelines declared in repos; each
    /// registered pipeline is a dynamic child of `ci`.
    #[plexus_macros::child]
    fn ci(&self) -> CiHub {
        CiHub::new(self.state.config_dir.clone(), self.state.ci.clone())
    }

    /// Return daemon version and config directory.
    #[plexus_macros::method]
    pub async fn status(&self) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        let version = env!("CARGO_PKG_VERSION").to_string();
        let config_dir_path = self.state.config_dir.clone();
        let config_dir = config_dir_path.display().to_string();
        stream! {
            // V5PARITY-26: empty-config detection — no orgs/, no workspaces/.
            let onboarding_hint = if config_is_empty(&config_dir_path) {
                Some(
                    "Run `hyperforge-v5 onboard` for guided setup, or call `begin` for next-step hints."
                        .to_string(),
                )
            } else {
                None
            };
            yield HyperforgeV5Event::Status { version, config_dir, onboarding_hint };
        }
    }

    /// Resolve a `secrets://<path>` reference through the embedded
    /// secret store and emit the plaintext value.
    ///
    /// This method exists to give tests a wire surface for the
    /// `SecretResolver` capability (V5CORE-4 acceptance #1). Production
    /// callers use the trait directly; no other wire method emits
    /// resolved secrets (redaction rule from CONTRACTS §types).
    #[plexus_macros::method(params(
        secret_ref = "secrets:// reference to resolve"
    ))]
    pub async fn resolve_secret(
        &self,
        secret_ref: String,
    ) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        let store = YamlSecretStore::new(&self.state.config_dir);
        stream! {
            let parsed = match SecretRef::parse(&secret_ref) {
                Ok(r) => r,
                Err(e) => {
                    yield HyperforgeV5Event::Error {
                        code: Some(e.code().to_string()),
                        message: format!("{secret_ref}: {e}"),
                    };
                    return;
                }
            };
            match store.resolve(&parsed) {
                Ok(value) => yield HyperforgeV5Event::SecretResolved { value },
                Err(e) => yield HyperforgeV5Event::Error {
                    code: Some(e.code().to_string()),
                    message: e.to_string(),
                },
            }
        }
    }

    /// V5PARITY-7: probe whether each org's credentials actually work.
    /// For tokens: resolves the secret, calls
    /// `adapter.repo_exists("__nonexistent__")` against the configured
    /// provider — auth-protected endpoint, so a non-`auth` error means
    /// the cred is fine.
    #[plexus_macros::method(params(
        org = "Optional org filter; default = all configured orgs"
    ))]
    pub async fn auth_check(
        &self,
        org: Option<String>,
    ) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => {
                    yield HyperforgeV5Event::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let resolver = YamlSecretStore::new(&config_dir);
            let probe_repo_ref = crate::v5::config::RepoRef {
                org: crate::v5::config::OrgName::from("__hyperforge_probe__"),
                name: crate::v5::config::RepoName::from("__hyperforge_probe__"),
            };
            for (org_name, org_cfg) in loaded.orgs.iter() {
                if let Some(filter) = org.as_deref() {
                    if filter != org_name.as_str() { continue; }
                }
                let provider = org_cfg.forge.provider;
                let provider_str = match provider {
                    crate::v5::config::ProviderKind::Github => "github",
                    crate::v5::config::ProviderKind::Codeberg => "codeberg",
                    crate::v5::config::ProviderKind::Gitlab => "gitlab",
                };
                for cred in &org_cfg.forge.credentials {
                    if !matches!(cred.cred_type, crate::v5::config::CredentialType::Token) { continue; }
                    let probe_url = match provider {
                        crate::v5::config::ProviderKind::Github => "https://github.com/__probe__/__probe__.git",
                        crate::v5::config::ProviderKind::Codeberg => "https://codeberg.org/__probe__/__probe__.git",
                        crate::v5::config::ProviderKind::Gitlab => "https://gitlab.com/__probe__/__probe__.git",
                    };
                    let probe_remote = crate::v5::config::Remote {
                        url: crate::v5::config::RemoteUrl::from(probe_url),
                        provider: Some(provider),
                    };
                    let result = crate::v5::ops::repo::exists_on_forge(
                        &probe_remote,
                        &probe_repo_ref,
                        &loaded.global.provider_map,
                        &resolver,
                        Some(cred.key.as_str()),
                        Some(crate::v5::ops::repo::default_token_ref_for(org_cfg)),
                    ).await;
                    let (valid, msg) = match result {
                        Ok(_) => (true, None),
                        Err(e) if matches!(e.class, crate::v5::adapters::ForgeErrorClass::Auth) => {
                            (false, Some(e.message))
                        }
                        // not_found / network / rate_limited / etc — credential
                        // worked enough to reach the API; mark valid.
                        Err(_) => (true, None),
                    };
                    yield HyperforgeV5Event::AuthCheckResult {
                        org: org_name.as_str().to_string(),
                        key: cred.key.clone(),
                        provider: provider_str.to_string(),
                        valid,
                        message: msg,
                    };
                }
            }
        }
    }

    /// V5PARITY-7: report which credentials each org needs.
    /// One `auth_requirement` event per (org, provider, expected key).
    #[plexus_macros::method(params(
        org = "Optional org filter; default = all configured orgs"
    ))]
    pub async fn auth_requirements(
        &self,
        org: Option<String>,
    ) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => {
                    yield HyperforgeV5Event::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let resolver = YamlSecretStore::new(&config_dir);
            for (org_name, org_cfg) in loaded.orgs.iter() {
                if let Some(filter) = org.as_deref() {
                    if filter != org_name.as_str() { continue; }
                }
                let provider_str = match org_cfg.forge.provider {
                    crate::v5::config::ProviderKind::Github => "github",
                    crate::v5::config::ProviderKind::Codeberg => "codeberg",
                    crate::v5::config::ProviderKind::Gitlab => "gitlab",
                };
                for cred in &org_cfg.forge.credentials {
                    let cred_type = match cred.cred_type {
                        crate::v5::config::CredentialType::Token => "token",
                        crate::v5::config::CredentialType::SshKey => "ssh_key",
                    };
                    // present == Some(true/false) when the ref is
                    // resolvable; None when we couldn't even check.
                    let present = if let Ok(parsed) = SecretRef::parse(&cred.key) {
                        Some(matches!(resolver.resolve(&parsed), Ok(v) if !v.is_empty()))
                    } else {
                        None
                    };
                    yield HyperforgeV5Event::AuthRequirement {
                        org: org_name.as_str().to_string(),
                        provider: provider_str.to_string(),
                        key: cred.key.clone(),
                        cred_type: cred_type.to_string(),
                        present,
                    };
                }
            }
        }
    }

    // ==================================================================
    // V5PARITY-8: CLI ergonomics — root methods.
    // ==================================================================

    /// Re-read all yaml from disk. v5 currently re-reads per-call so
    /// this is a no-op invalidator; future caching makes it load-bearing.
    #[plexus_macros::method]
    pub async fn reload(&self) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => {
                    yield HyperforgeV5Event::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let store = YamlSecretStore::new(&config_dir);
            let secrets_refs = store.list_refs().map(|v| v.len() as u32).unwrap_or(0);
            yield HyperforgeV5Event::ReloadDone {
                orgs: loaded.orgs.len() as u32,
                workspaces: loaded.workspaces.len() as u32,
                secrets_refs,
            };
        }
    }

    /// Show resolved global config: provider_map + default_workspace.
    #[plexus_macros::method]
    pub async fn config_show(&self) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l,
                Err(e) => {
                    yield HyperforgeV5Event::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let mut pm = serde_json::Map::new();
            for (k, v) in &loaded.global.provider_map {
                let val = match v {
                    crate::v5::config::ProviderKind::Github => "github",
                    crate::v5::config::ProviderKind::Codeberg => "codeberg",
                    crate::v5::config::ProviderKind::Gitlab => "gitlab",
                };
                pm.insert(k.as_str().to_string(), serde_json::Value::String(val.into()));
            }
            yield HyperforgeV5Event::ConfigShow {
                provider_map: serde_json::Value::Object(pm),
                default_workspace: loaded.global.default_workspace.as_ref().map(|w| w.as_str().to_string()),
            };
        }
    }

    /// Set an SSH-key credential on an org's forge block.
    /// Convenience wrapper over `orgs.set_credential` with SSH-specific
    /// shape; the underlying storage is identical.
    #[plexus_macros::method(params(
        org = "Org name",
        forge = "Provider name (github | codeberg | gitlab)",
        key = "Filesystem path to the SSH private key (~ expanded)"
    ))]
    pub async fn config_set_ssh_key(
        &self,
        org: String,
        forge: String,
        key: String,
    ) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            if org.is_empty() || forge.is_empty() || key.is_empty() {
                yield HyperforgeV5Event::Error {
                    code: Some("validation".into()),
                    message: "missing required parameter".into(),
                };
                return;
            }
            // Expand ~/.
            let expanded = if let Some(rest) = key.strip_prefix("~/") {
                if let Some(home) = std::env::var_os("HOME") {
                    std::path::PathBuf::from(home).join(rest).display().to_string()
                } else { key.clone() }
            } else { key.clone() };
            // Load + mutate the org yaml directly (cred shape: ssh_key).
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l, Err(e) => {
                    yield HyperforgeV5Event::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let Some(existing) = loaded.orgs.get(&crate::v5::config::OrgName::from(org.as_str())) else {
                yield HyperforgeV5Event::Error {
                    code: Some("not_found".into()),
                    message: format!("org '{org}' not found"),
                };
                return;
            };
            let mut updated = existing.clone();
            // Drop any existing ssh_key cred (one per forge convention),
            // then add a fresh one.
            updated.forge.credentials.retain(|c| !matches!(c.cred_type, crate::v5::config::CredentialType::SshKey));
            updated.forge.credentials.push(crate::v5::config::CredentialEntry {
                key: expanded.clone(),
                cred_type: crate::v5::config::CredentialType::SshKey,
            });
            let orgs_dir = config_dir.join("orgs");
            if let Err(e) = crate::v5::ops::state::save_org(&orgs_dir, &updated) {
                yield HyperforgeV5Event::Error {
                    code: Some("config_error".into()),
                    message: e.to_string(),
                };
                return;
            }
            yield HyperforgeV5Event::SshKeySet { org, forge, path: expanded };
        }
    }

    /// Read SSH key path(s) configured on an org. Never reveals file CONTENT.
    #[plexus_macros::method(params(
        org = "Org name",
        forge = "Optional forge filter"
    ))]
    pub async fn config_show_ssh_key(
        &self,
        org: String,
        forge: Option<String>,
    ) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            let loaded = match crate::v5::ops::state::load_all(&config_dir) {
                Ok(l) => l, Err(e) => {
                    yield HyperforgeV5Event::Error {
                        code: Some("config_error".into()),
                        message: e.to_string(),
                    };
                    return;
                }
            };
            let Some(existing) = loaded.orgs.get(&crate::v5::config::OrgName::from(org.as_str())) else {
                yield HyperforgeV5Event::Error {
                    code: Some("not_found".into()),
                    message: format!("org '{org}' not found"),
                };
                return;
            };
            let provider_str = match existing.forge.provider {
                crate::v5::config::ProviderKind::Github => "github",
                crate::v5::config::ProviderKind::Codeberg => "codeberg",
                crate::v5::config::ProviderKind::Gitlab => "gitlab",
            };
            // Apply optional --forge filter (we only have one forge per org for now,
            // but the parameter is here for v4-shape compatibility).
            if let Some(f) = forge.as_deref() {
                if f != provider_str {
                    yield HyperforgeV5Event::SshKeyShow {
                        org: org.clone(),
                        forge: f.to_string(),
                        path: None,
                    };
                    return;
                }
            }
            let path = existing.forge.credentials.iter()
                .find(|c| matches!(c.cred_type, crate::v5::config::CredentialType::SshKey))
                .map(|c| c.key.clone());
            yield HyperforgeV5Event::SshKeyShow {
                org,
                forge: provider_str.to_string(),
                path,
            };
        }
    }

    /// Guided onboarding entry point. Idempotent.
    /// Creates `$HF_CONFIG/config.yaml` with the default provider_map
    /// if missing; then emits a list of next-step suggestions.
    #[plexus_macros::method]
    pub async fn begin(&self) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        let config_dir = self.state.config_dir.clone();
        stream! {
            // Ensure config.yaml exists with the default provider_map.
            let cfg_path = config_dir.join("config.yaml");
            let existed = cfg_path.exists();
            if !existed {
                if let Err(e) = std::fs::create_dir_all(&config_dir) {
                    yield HyperforgeV5Event::Error {
                        code: Some("io_error".into()),
                        message: format!("create config dir: {e}"),
                    };
                    return;
                }
                let default_yaml = "provider_map:\n  github.com: github\n  codeberg.org: codeberg\n  gitlab.com: gitlab\n";
                if let Err(e) = std::fs::write(&cfg_path, default_yaml) {
                    yield HyperforgeV5Event::Error {
                        code: Some("io_error".into()),
                        message: format!("write {}: {e}", cfg_path.display()),
                    };
                    return;
                }
            }
            yield HyperforgeV5Event::BeginNextStep {
                action: "orgs.create".into(),
                message: "Register an org: `orgs create --name <org> --provider github`".into(),
            };
            yield HyperforgeV5Event::BeginNextStep {
                action: "secrets.set".into(),
                message: "Add a token: `secrets set --key secrets://github/<org>/token --value <gh-token>`".into(),
            };
            yield HyperforgeV5Event::BeginNextStep {
                action: "orgs.set_credential".into(),
                message: "Wire it: `orgs set_credential --name <org> --key secrets://github/<org>/token --type token`".into(),
            };
            yield HyperforgeV5Event::BeginNextStep {
                action: "repos.import".into(),
                message: "Pull in your repos: `repos import --org <org> --forge github`".into(),
            };
        }
    }

    // ==================================================================
    // V5PARITY-23: auth discovery (no org context).
    // ==================================================================

    /// Static scope/permission requirements per provider for full
    /// hyperforge functionality. Independent of any configured org.
    #[plexus_macros::method(params(provider = "github | codeberg | gitlab"))]
    pub async fn auth_requirements_for(
        &self,
        provider: String,
    ) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        stream! {
            let lower = provider.to_lowercase();
            let (required, recommended, optional): (&[&str], &[&str], &[&str]) = match lower.as_str() {
                "github"   => (&["repo", "read:org"], &["delete_repo"], &["gist"]),
                "codeberg" => (&["repository", "read:user"], &[], &[]),
                "gitlab"   => (&["api", "read_repository"], &["write_repository"], &[]),
                _ => {
                    yield HyperforgeV5Event::Error {
                        code: Some("validation".into()),
                        message: format!("unknown provider '{provider}'"),
                    };
                    return;
                }
            };
            yield HyperforgeV5Event::AuthRequirementsFor {
                provider: lower,
                required_scopes: required.iter().map(|&s| s.to_string()).collect(),
                recommended_scopes: recommended.iter().map(|&s| s.to_string()).collect(),
                optional_scopes: optional.iter().map(|&s| s.to_string()).collect(),
            };
        }
    }

    /// Probe for known external auth sources on the daemon host.
    /// Without `--provider`, probes all three. Token values are NEVER
    /// included in the emitted events.
    #[plexus_macros::method(params(provider = "Optional filter: github | codeberg | gitlab"))]
    pub async fn auth_detect_external(
        &self,
        provider: Option<String>,
    ) -> impl Stream<Item = HyperforgeV5Event> + Send + 'static {
        stream! {
            use crate::v5::ops::external_auth::{
                detect_status, list_accessible_orgs, ExternalAuthProvider,
            };
            let providers: Vec<ExternalAuthProvider> = match provider.as_deref() {
                None => vec![
                    ExternalAuthProvider::Github,
                    ExternalAuthProvider::Codeberg,
                    ExternalAuthProvider::Gitlab,
                ],
                Some("github") => vec![ExternalAuthProvider::Github],
                Some("codeberg") => vec![ExternalAuthProvider::Codeberg],
                Some("gitlab") => vec![ExternalAuthProvider::Gitlab],
                Some(other) => {
                    yield HyperforgeV5Event::Error {
                        code: Some("validation".into()),
                        message: format!("unknown provider '{other}'"),
                    };
                    return;
                }
            };
            for p in providers {
                let Some(status) = detect_status(p) else { continue };
                // For logged-in providers, list orgs. Failures here are
                // non-fatal — emit the status without orgs.
                let accessible_orgs = if status.logged_in {
                    list_accessible_orgs(p).ok()
                } else {
                    None
                };
                yield HyperforgeV5Event::ExternalAuthDetected {
                    provider: p.to_string(),
                    source: format!("{}-cli", p.cli()),
                    logged_in: status.logged_in,
                    username: status.username,
                    host: status.host,
                    scopes: status.scopes,
                    accessible_orgs,
                };
            }
        }
    }
}

/// V5PARITY-26: a config dir is "empty" when neither `orgs/` nor
/// `workspaces/` contains any *.yaml files. We don't check `secrets.yaml`
/// because the secret store is permissioned-but-content-shared with v4
/// in the historical layout — finding it doesn't imply v5 setup.
fn config_is_empty(config_dir: &std::path::Path) -> bool {
    fn dir_has_yaml(p: &std::path::Path) -> bool {
        std::fs::read_dir(p)
            .ok()
            .map(|rd| rd.flatten().any(|e| {
                e.path().extension().is_some_and(|ext| ext == "yaml")
            }))
            .unwrap_or(false)
    }
    !dir_has_yaml(&config_dir.join("orgs"))
        && !dir_has_yaml(&config_dir.join("workspaces"))
}

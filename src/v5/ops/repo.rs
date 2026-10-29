//! `ops::repo` — repo-level operations, single source of truth for
//! every service that touches a repo (V5LIFECYCLE-3, -4, -5).
//!
//! - `derive_provider`, `compute_drift` — relocated from `repos.rs`
//! - `sync_one` — pure sync inner loop; both `ReposService::sync` and
//!   `WorkspacesService::sync` iterate its output.
//! - `exists_on_forge`, `create_on_forge`, `delete_on_forge`,
//!   `privatize_on_forge` — the forge-call wrappers. Services never call
//!   adapter lifecycle methods directly after this module.
//! - `dismiss`, `purge` — pure state mutations (V5LIFECYCLE-5).

use std::collections::{BTreeMap, BTreeSet};

use crate::v5::adapters::{
    self, extract_host, for_provider, DriftFieldKind, ForgeAuth, ForgeErrorClass, ForgeMetadata,
    ForgePortError, MetadataFields, ProviderVisibility,
};
use crate::v5::config::{
    CredentialType, DomainName, OrgConfig, OrgRepo, ProviderKind, Remote, RepoMetadataLocal,
    RepoName, RepoRef,
};
use crate::v5::secrets::SecretResolver;

// ---------------------------------------------------------------------
// Provider derivation (relocated from repos.rs; still pub(crate)-ish —
// we expose it as `pub` so the services can call it through the ops
// namespace and no caller re-introduces a duplicate).
// ---------------------------------------------------------------------

/// Map a `Remote` to its `ProviderKind` via either an explicit
/// `provider:` override on the remote or the global `provider_map`
/// domain lookup.
pub fn derive_provider(
    remote: &Remote,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
) -> Result<ProviderKind, String> {
    if let Some(p) = remote.provider {
        return Ok(p);
    }
    let host = extract_host(remote.url.as_str())
        .ok_or_else(|| format!("cannot extract host from url '{}'", remote.url))?;
    provider_map
        .get(&DomainName::from(host.as_str()))
        .copied()
        .ok_or_else(|| {
            format!(
                "derivation failed for url '{}': host '{}' not in provider_map and no override",
                remote.url, host
            )
        })
}

// ---------------------------------------------------------------------
// Drift computation (relocated from repos.rs).
// ---------------------------------------------------------------------

/// A single metadata field that disagrees between local declared state
/// and the forge's current state.
#[derive(Debug, Clone)]
pub struct DriftField {
    pub field: String,
    pub local: serde_json::Value,
    pub remote: serde_json::Value,
}

/// Compute drift from local `RepoMetadataLocal` (a declared subset)
/// against the forge's `ForgeMetadata` (always all four fields).
/// Only fields the local side declares participate in drift.
#[must_use]
pub fn compute_drift(local: &Option<RepoMetadataLocal>, remote: &ForgeMetadata) -> Vec<DriftField> {
    let Some(local) = local else {
        return vec![];
    };
    let mut out = Vec::new();
    if let Some(ref lv) = local.default_branch {
        if *lv != remote.default_branch {
            out.push(DriftField {
                field: "default_branch".into(),
                local: serde_json::Value::String(lv.clone()),
                remote: serde_json::Value::String(remote.default_branch.clone()),
            });
        }
    }
    if let Some(ref lv) = local.description {
        if *lv != remote.description {
            out.push(DriftField {
                field: "description".into(),
                local: serde_json::Value::String(lv.clone()),
                remote: serde_json::Value::String(remote.description.clone()),
            });
        }
    }
    if let Some(lv) = local.archived {
        if lv != remote.archived {
            out.push(DriftField {
                field: "archived".into(),
                local: serde_json::Value::Bool(lv),
                remote: serde_json::Value::Bool(remote.archived),
            });
        }
    }
    if let Some(ref lv) = local.visibility {
        if *lv != remote.visibility {
            out.push(DriftField {
                field: "visibility".into(),
                local: serde_json::Value::String(lv.clone()),
                remote: serde_json::Value::String(remote.visibility.clone()),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------
// Forge-call wrappers (V5LIFECYCLE-4).
// ---------------------------------------------------------------------

/// Token credential-ref lookup for an org (first cred of type Token,
/// if any).
pub fn token_ref_for(org: &OrgConfig) -> Option<&str> {
    org.forge
        .credentials
        .iter()
        .find(|c| matches!(c.cred_type, CredentialType::Token))
        .map(|c| c.key.as_str())
}

/// V5PARITY-24: provider-default secret path for an org. The convention
/// is `secrets://<provider>/_default/token`. Always returns a path; the
/// secret may or may not exist — that's a runtime resolution concern.
#[must_use]
pub fn default_token_ref_for(org: &OrgConfig) -> String {
    let provider = match org.forge.provider {
        ProviderKind::Github => "github",
        ProviderKind::Codeberg => "codeberg",
        ProviderKind::Gitlab => "gitlab",
    };
    format!("secrets://{provider}/_default/token")
}

/// V5PARITY-34: filter remotes by the per-repo `forges` scope.
///
/// When `repo.forges` is `None`, every remote participates (legacy
/// behavior). When it's `Some(list)`, only remotes whose derived
/// provider is in `list` are returned. Empty `Some([])` means
/// "the repo is currently scoped to no forges" — no remotes
/// participate; callers emit a `forge_excluded` signal.
#[must_use]
pub fn filter_remotes_by_forges<'a>(
    repo: &'a OrgRepo,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
) -> Vec<&'a Remote> {
    let Some(scope) = repo.forges.as_ref() else {
        return repo.remotes.iter().collect();
    };
    repo.remotes
        .iter()
        .filter(|r| match derive_provider(r, provider_map) {
            Ok(p) => scope.contains(&p),
            Err(_) => false,
        })
        .collect()
}

/// V5PARITY-34: canonical remote that's in scope. Mirrors
/// `OrgRepo::canonical_remote()` (== `remotes.first()`) but applies
/// the `forges` filter. Returns `None` when the repo has no remotes
/// OR when every remote was excluded by the per-repo scope.
#[must_use]
pub fn canonical_remote_in_scope<'a>(
    repo: &'a OrgRepo,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
) -> Option<&'a Remote> {
    filter_remotes_by_forges(repo, provider_map).into_iter().next()
}

/// V5PARITY-34: did the per-repo `forges` scope exclude every remote?
/// Used by routing methods to emit `forge_excluded` rather than a
/// generic `no_remotes` error.
#[must_use]
pub fn all_remotes_excluded(repo: &OrgRepo, provider_map: &BTreeMap<DomainName, ProviderKind>) -> bool {
    !repo.remotes.is_empty() && filter_remotes_by_forges(repo, provider_map).is_empty()
}

/// Does this remote exist on its forge?
pub async fn exists_on_forge(
    remote: &Remote,
    repo_ref: &RepoRef,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
    resolver: &dyn SecretResolver,
    token_ref: Option<&str>,
    fallback_token_ref: Option<String>,
) -> Result<bool, ForgePortError> {
    let provider = derive_provider(remote, provider_map).map_err(|e| {
        ForgePortError::new(ForgeErrorClass::Network, e)
    })?;
    let adapter = for_provider(provider);
    let auth = ForgeAuth { token_ref, fallback_token_ref, resolver };
    adapter.repo_exists(remote, repo_ref, &auth).await
}

/// Create a repo on its forge.
pub async fn create_on_forge(
    remote: &Remote,
    repo_ref: &RepoRef,
    visibility: ProviderVisibility,
    description: &str,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
    resolver: &dyn SecretResolver,
    token_ref: Option<&str>,
    fallback_token_ref: Option<String>,
) -> Result<(), ForgePortError> {
    let provider = derive_provider(remote, provider_map).map_err(|e| {
        ForgePortError::new(ForgeErrorClass::Network, e)
    })?;
    let adapter = for_provider(provider);
    let auth = ForgeAuth { token_ref, fallback_token_ref, resolver };
    adapter.create_repo(remote, repo_ref, visibility, description, &auth).await
}

/// Delete a repo on its forge.
pub async fn delete_on_forge(
    remote: &Remote,
    repo_ref: &RepoRef,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
    resolver: &dyn SecretResolver,
    token_ref: Option<&str>,
    fallback_token_ref: Option<String>,
) -> Result<(), ForgePortError> {
    let provider = derive_provider(remote, provider_map).map_err(|e| {
        ForgePortError::new(ForgeErrorClass::Network, e)
    })?;
    let adapter = for_provider(provider);
    let auth = ForgeAuth { token_ref, fallback_token_ref, resolver };
    adapter.delete_repo(remote, repo_ref, &auth).await
}

/// V5PARITY-36: pull-clone a repo from `source_url` into the
/// destination forge under `dest_repo_ref`. Codeberg implements via
/// its migrate API; github/gitlab return Unimplemented (D10 ForgePort
/// surface).
pub async fn migrate_on_forge(
    dest_provider: ProviderKind,
    source_url: &str,
    dest_repo_ref: &RepoRef,
    options: &crate::v5::adapters::MigrateOptions,
    source_auth: Option<&str>,
    resolver: &dyn SecretResolver,
    token_ref: Option<&str>,
    fallback_token_ref: Option<String>,
) -> Result<crate::v5::adapters::RemoteRepo, ForgePortError> {
    let adapter = for_provider(dest_provider);
    let auth = ForgeAuth { token_ref, fallback_token_ref, resolver };
    adapter.migrate_from(source_url, dest_repo_ref, options, source_auth, &auth).await
}

/// V5PARITY-6: rename a repo on the forge.
pub async fn rename_on_forge(
    remote: &Remote,
    repo_ref: &RepoRef,
    new_name: &str,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
    resolver: &dyn SecretResolver,
    token_ref: Option<&str>,
    fallback_token_ref: Option<String>,
) -> Result<(), ForgePortError> {
    let provider = derive_provider(remote, provider_map).map_err(|e| {
        ForgePortError::new(ForgeErrorClass::Network, e)
    })?;
    let adapter = for_provider(provider);
    let auth = ForgeAuth { token_ref, fallback_token_ref, resolver };
    adapter.rename_repo(remote, repo_ref, new_name, &auth).await
}

/// V5PARITY-2: list repos on a forge for an org. `provider` is
/// supplied explicitly because there's no per-repo `Remote` yet at
/// import time.
pub async fn list_on_forge(
    provider: ProviderKind,
    org: &crate::v5::config::OrgName,
    resolver: &dyn SecretResolver,
    token_ref: Option<&str>,
    fallback_token_ref: Option<String>,
) -> Result<Vec<crate::v5::adapters::RemoteRepo>, ForgePortError> {
    let adapter = for_provider(provider);
    let auth = ForgeAuth { token_ref, fallback_token_ref, resolver };
    adapter.list_repos(org, &auth).await
}

/// Generic metadata write on the forge. Used by `repos.push`.
pub async fn write_metadata_on_forge(
    remote: &Remote,
    repo_ref: &RepoRef,
    fields: &MetadataFields,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
    resolver: &dyn SecretResolver,
    token_ref: Option<&str>,
    fallback_token_ref: Option<String>,
) -> Result<MetadataFields, ForgePortError> {
    let provider = derive_provider(remote, provider_map).map_err(|e| {
        ForgePortError::new(ForgeErrorClass::Network, e)
    })?;
    let adapter = for_provider(provider);
    let auth = ForgeAuth { token_ref, fallback_token_ref, resolver };
    adapter.write_metadata(remote, repo_ref, fields, &auth).await
}

/// Privatize a repo on its forge via `write_metadata` with only the
/// `visibility` field set to `private`. Used by soft-delete
/// (V5LIFECYCLE-6). Does not touch any other metadata field.
pub async fn privatize_on_forge(
    remote: &Remote,
    repo_ref: &RepoRef,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
    resolver: &dyn SecretResolver,
    token_ref: Option<&str>,
    fallback_token_ref: Option<String>,
) -> Result<(), ForgePortError> {
    let provider = derive_provider(remote, provider_map).map_err(|e| {
        ForgePortError::new(ForgeErrorClass::Network, e)
    })?;
    let adapter = for_provider(provider);
    let auth = ForgeAuth { token_ref, fallback_token_ref, resolver };
    let mut fields: MetadataFields = std::collections::BTreeMap::new();
    fields.insert(
        DriftFieldKind::Visibility,
        serde_json::Value::String("private".into()),
    );
    adapter
        .write_metadata(remote, repo_ref, &fields, &auth)
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------
// Sync — single source of truth, called by both services (V5LIFECYCLE-3).
// ---------------------------------------------------------------------

/// One per-remote sync outcome.
#[derive(Debug, Clone)]
pub struct SyncOutcomeEntry {
    pub remote: Remote,
    pub provider: Option<ProviderKind>,
    pub status: SyncStatus,
    pub drift: Vec<DriftField>,
    pub metadata: Option<ForgeMetadata>,
    pub error_class: Option<ForgeErrorClass>,
    pub error_message: Option<String>,
}

/// `SyncStatus` reuses the CONTRACTS §types enum variants as lowercase
/// strings on the wire; here we use a bounded enum internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    InSync,
    Drifted,
    Errored,
}

impl SyncStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InSync => "in_sync",
            Self::Drifted => "drifted",
            Self::Errored => "errored",
        }
    }
}

/// Run the sync inner loop on a single repo. Per-remote result vec;
/// callers aggregate at the level they care about (per-remote events
/// for `repos.sync`, per-repo collapsed events for `workspaces.sync`).
pub async fn sync_one(
    repo: &OrgRepo,
    org: &OrgConfig,
    provider_map: &BTreeMap<DomainName, ProviderKind>,
    resolver: &dyn SecretResolver,
    remote_filter: Option<&str>,
) -> Vec<SyncOutcomeEntry> {
    let tref = token_ref_for(org);
    let fallback = Some(default_token_ref_for(org));
    let repo_ref = RepoRef {
        org: org.name.clone(),
        name: repo.name.clone(),
    };
    // V5PARITY-34: scope by per-repo `forges` first, then narrow by
    // explicit URL filter if the caller asked for one.
    let scoped: Vec<&Remote> = filter_remotes_by_forges(repo, provider_map);
    let filtered: Vec<&Remote> = if let Some(f) = remote_filter.filter(|s| !s.is_empty()) {
        scoped.into_iter().filter(|r| r.url.as_str() == f).collect()
    } else {
        scoped
    };
    let mut out = Vec::new();
    for r in filtered {
        let provider = match derive_provider(r, provider_map) {
            Ok(p) => Some(p),
            Err(e) => {
                out.push(SyncOutcomeEntry {
                    remote: r.clone(),
                    provider: None,
                    status: SyncStatus::Errored,
                    drift: vec![],
                    metadata: None,
                    error_class: Some(ForgeErrorClass::Network),
                    error_message: Some(e),
                });
                continue;
            }
        };
        let adapter = for_provider(provider.unwrap());
        let auth = ForgeAuth {
            token_ref: tref,
            fallback_token_ref: fallback.clone(),
            resolver,
        };
        match adapter.read_metadata(r, &repo_ref, &auth).await {
            Ok(meta) => {
                let drift = compute_drift(&repo.metadata, &meta);
                let status = if drift.is_empty() {
                    SyncStatus::InSync
                } else {
                    SyncStatus::Drifted
                };
                out.push(SyncOutcomeEntry {
                    remote: r.clone(),
                    provider,
                    status,
                    drift,
                    metadata: Some(meta),
                    error_class: None,
                    error_message: None,
                });
            }
            Err(e) => {
                out.push(SyncOutcomeEntry {
                    remote: r.clone(),
                    provider,
                    status: SyncStatus::Errored,
                    drift: vec![],
                    metadata: None,
                    error_class: Some(e.class),
                    error_message: Some(e.message),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// Lifecycle state mutations (V5LIFECYCLE-5).
// ---------------------------------------------------------------------

/// Set the repo's lifecycle to `dismissed` and union `privatized` into
/// its `privatized_on` set. Doesn't write disk — caller follows up
/// with `ops::state::save_org`.
pub fn dismiss(repo: &mut OrgRepo, privatized: BTreeSet<ProviderKind>) {
    let md = repo.metadata.get_or_insert_with(RepoMetadataLocal::default);
    md.lifecycle = crate::v5::config::RepoLifecycle::Dismissed;
    md.privatized_on.extend(privatized);
}

/// Remove the repo entry from the org (after purge guards). Errors:
/// `NotDismissed` when lifecycle != dismissed; `Protected` when
/// protected == true.
pub fn purge(org: &mut OrgConfig, name: &RepoName) -> Result<(), PurgeError> {
    let idx = org
        .repos
        .iter()
        .position(|r| r.name == *name)
        .ok_or(PurgeError::NotFound)?;
    let repo = &org.repos[idx];
    let md = repo.metadata.as_ref();
    let protected = md.is_some_and(|m| m.protected);
    if protected {
        return Err(PurgeError::Protected);
    }
    let lifecycle = md.map_or(crate::v5::config::RepoLifecycle::Active, |m| m.lifecycle);
    if lifecycle != crate::v5::config::RepoLifecycle::Dismissed {
        return Err(PurgeError::NotDismissed);
    }
    org.repos.remove(idx);
    Ok(())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PurgeError {
    #[error("repo not found in org")]
    NotFound,
    #[error("repo is protected")]
    Protected,
    #[error("repo lifecycle is not dismissed (run repos.delete first)")]
    NotDismissed,
}

// Suppress unused imports we expose for service consumers.
#[allow(unused_imports)]
use adapters as _adapters_anchor;
